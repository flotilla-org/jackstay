//! Named-pipe Local Endpoints (Wheelhouse ADR 0011).
//!
//! Servers create byte-mode pipes with an explicit protected DACL (SYSTEM and
//! the current user, or the logon SID for a session-bound endpoint), reject
//! remote clients, and create their first instance with
//! `FILE_FLAG_FIRST_PIPE_INSTANCE`, so an existing pipe of that name is an
//! error rather than a silent takeover. Clients connect at identification
//! impersonation level (the server cannot act as them) and check the server
//! process's owner before sending anything. Accepting opens the client process
//! once, while its connection holds the pipe, and keeps that handle: admission
//! watches it and handle transfer duplicates into it, so no PID is resolved
//! again later.
//!
//! Every handle is overlapped. Each read or write waits on its own completion
//! and on a shutdown event, which gives cancellation, timeouts and a
//! nonblocking mode without deprecated `PIPE_NOWAIT` semantics.

use std::{
    cell::Cell,
    io::{self, Read, Write},
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle, RawHandle},
    ptr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{
        DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_IO_INCOMPLETE,
        ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_OPERATION_ABORTED, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED,
        GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::{
        Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_GROUPS, TOKEN_QUERY, TOKEN_USER, TokenGroups, TokenSessionId,
        TokenUser,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_TYPE_PIPE, GetFileType, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
        ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
    },
    System::{
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeClientSessionId, GetNamedPipeServerProcessId,
            GetNamedPipeServerSessionId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
            PIPE_WAIT, PeekNamedPipe, WaitNamedPipeW,
        },
        SystemServices::SE_GROUP_LOGON_ID,
        Threading::{
            CreateEventW, GetCurrentProcess, INFINITE, OpenProcess, OpenProcessToken, PROCESS_DUP_HANDLE,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
        },
    },
};

use super::{CONNECT_TIMEOUT, Connection, Endpoint, Error, PeerIdentity, Scope};

/// Per-direction pipe buffer quota. Writers beyond it complete as the peer reads.
const BUFFER: u32 = 64 * 1024;
/// A handle transfer carries at most this many handles.
const MAX_HANDLES: usize = 16;
const HANDLES_RECEIVED: u8 = 1;
/// How long dropping a stream lets a background write finish before cancelling.
const DROP_FLUSH: Duration = Duration::from_secs(1);

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn check(result: i32) -> io::Result<()> {
    if result == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn is_error(error: &io::Error, code: u32) -> bool {
    error.raw_os_error() == Some(code as i32)
}

fn event() -> io::Result<OwnedHandle> {
    // SAFETY: null attributes give a non-inheritable handle; an unnamed,
    // manual-reset, initially clear event.
    let raw = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a fresh handle owned by nothing else.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

fn signaled(handle: &OwnedHandle) -> bool {
    // SAFETY: the owned handle is live for the call; zero timeout polls.
    unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) == WAIT_OBJECT_0 }
}

/// Wait timeout in whole milliseconds, rounded up; INFINITE without a limit.
fn milliseconds(limit: Option<Duration>) -> u32 {
    limit.map_or(INFINITE, |limit| {
        limit
            .as_millis()
            .saturating_add(u128::from(limit.subsec_nanos() % 1_000_000 != 0))
            .min(u128::from(INFINITE - 1)) as u32
    })
}

fn random_hex() -> io::Result<String> {
    use windows_sys::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
    let mut bytes = [0_u8; 16];
    // SAFETY: the buffer is writable for its length; no algorithm handle is needed.
    let status = unsafe {
        BCryptGenRandom(
            ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(io::Error::other(format!("BCryptGenRandom failed with NTSTATUS {status:#x}")));
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// The token facts endpoints are built and checked from.
#[derive(Debug, Clone)]
pub(super) struct Identity {
    user: String,
    logon: Option<String>,
    session: u32,
}

fn token_information(token: &OwnedHandle, class: i32) -> io::Result<Vec<u64>> {
    let mut len = 0;
    // SAFETY: a null buffer with zero length queries the required size.
    unsafe { GetTokenInformation(token.as_raw_handle(), class, ptr::null_mut(), 0, &mut len) };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    // u64 storage aligns the SID pointers and structures the token returns.
    let mut buffer = vec![0_u64; (len as usize).div_ceil(size_of::<u64>())];
    // SAFETY: buffer is writable for at least len bytes.
    check(unsafe { GetTokenInformation(token.as_raw_handle(), class, buffer.as_mut_ptr().cast(), len, &mut len) })?;
    Ok(buffer)
}

fn sid_string(sid: *mut std::ffi::c_void) -> io::Result<String> {
    let mut text = ptr::null_mut();
    // SAFETY: sid points into a live token-information buffer; the returned
    // string is LocalAlloc'd and freed below.
    check(unsafe { ConvertSidToStringSidW(sid, &mut text) })?;
    // SAFETY: text is a NUL-terminated UTF-16 string from the call above.
    let len = (0..).take_while(|&index| unsafe { *text.add(index) } != 0).count();
    // SAFETY: len counted the initialized code units before the terminator.
    let value = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    // SAFETY: text was allocated by ConvertSidToStringSidW.
    unsafe { LocalFree(text.cast()) };
    Ok(value)
}

fn identity_of(process: HANDLE) -> io::Result<Identity> {
    let mut raw = ptr::null_mut();
    // SAFETY: process is a live process handle (or the current-process pseudo
    // handle) with query access; the token handle is closed by OwnedHandle.
    check(unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut raw) })?;
    // SAFETY: OpenProcessToken succeeded, so raw is a fresh owned handle.
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let user = token_information(&token, TokenUser)?;
    // SAFETY: TokenUser fills a TOKEN_USER at the start of the aligned buffer.
    let user = sid_string(unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid })?;
    let groups = token_information(&token, TokenGroups)?;
    let groups = groups.as_ptr().cast::<TOKEN_GROUPS>();
    // SAFETY: TokenGroups fills a TOKEN_GROUPS whose GroupCount entries follow
    // in the same buffer.
    let logon = unsafe {
        let entries = std::slice::from_raw_parts((*groups).Groups.as_ptr(), (*groups).GroupCount as usize);
        entries
            .iter()
            .find(|group| group.Attributes & (SE_GROUP_LOGON_ID as u32) == SE_GROUP_LOGON_ID as u32)
            .map(|group| sid_string(group.Sid))
            .transpose()?
    };
    let session = token_information(&token, TokenSessionId)?;
    Ok(Identity {
        user,
        logon,
        session: session[0] as u32,
    })
}

pub(super) fn current() -> io::Result<Identity> {
    // SAFETY: the current-process pseudo handle is always valid.
    identity_of(unsafe { GetCurrentProcess() })
}

/// What a client requires of the server before trusting it.
#[derive(Debug, Clone)]
pub(crate) struct ServerPolicy {
    /// The owner the server process must run as (a SID string).
    pub(crate) user: String,
    /// The Windows session the server must be in, for a session-bound endpoint.
    pub(crate) session: Option<u32>,
}

impl ServerPolicy {
    pub(crate) fn current(scope: Scope) -> io::Result<Self> {
        let me = current()?;
        Ok(Self {
            user: me.user,
            session: (scope == Scope::Session).then_some(me.session),
        })
    }

    pub(crate) fn check(&self, server: &PeerIdentity) -> Result<(), Error> {
        if server.user != self.user {
            return Err(Error::UntrustedServer(format!(
                "server process {} runs as {}, not {}",
                server.pid, server.user, self.user
            )));
        }
        if let Some(session) = self.session
            && server.session != Some(session)
        {
            return Err(Error::UntrustedServer(format!(
                "server process {} is in session {:?}, not {session}",
                server.pid, server.session
            )));
        }
        Ok(())
    }
}

/// What a listener requires of an accepted client.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PeerPolicy {
    /// Refuse peers outside this Windows session (session-bound endpoints).
    pub(crate) session: Option<u32>,
}

impl PeerPolicy {
    pub(crate) fn check(&self, peer: &PeerIdentity) -> Result<(), Error> {
        match self.session {
            Some(session) if peer.session != Some(session) => Err(Error::RefusedPeer(format!(
                "peer process {} is in session {:?}; this endpoint is bound to session {session}",
                peer.pid, peer.session
            ))),
            _ => Ok(()),
        }
    }
}

pub(super) fn render(endpoint: &Endpoint) -> io::Result<String> {
    render_for(endpoint, &current()?)
}

fn render_for(endpoint: &Endpoint, me: &Identity) -> io::Result<String> {
    Ok(match endpoint.scope {
        Scope::User => format!(r"\\.\pipe\jackstay.{}.{}", me.user, endpoint.name),
        Scope::Session => format!(r"\\.\pipe\jackstay.{}.s{}.{}", me.user, me.session, endpoint.name),
    })
}

// ---------------------------------------------------------------------------
// Pipe creation
// ---------------------------------------------------------------------------

struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is an immutable LocalAlloc'd block after construction;
// CreateNamedPipeW only reads it.
unsafe impl Send for SecurityDescriptor {}
// SAFETY: as above; shared reads only.
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    /// Protected DACL: SYSTEM plus the user, or plus the logon SID when bound
    /// to this logon session. No inherited ACEs; nobody else is granted access.
    fn new(scope: Scope, me: &Identity) -> io::Result<Self> {
        let trustee = match scope {
            Scope::User => me.user.as_str(),
            Scope::Session => me
                .logon
                .as_deref()
                .ok_or_else(|| io::Error::other("the process token has no logon SID"))?,
        };
        let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{trustee})"));
        let mut descriptor = ptr::null_mut();
        // SAFETY: sddl is NUL-terminated; the descriptor is freed in Drop.
        check(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, ptr::null_mut())
        })?;
        Ok(Self(descriptor))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.0) };
    }
}

fn create_instance(name: &[u16], descriptor: &SecurityDescriptor, first: bool, max_instances: u32) -> io::Result<OwnedHandle> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let first = if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 };
    // SAFETY: name is NUL-terminated and attributes/descriptor outlive the call.
    let raw = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | first,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            max_instances,
            BUFFER,
            BUFFER,
            0,
            &attributes,
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a fresh pipe handle owned by nothing else.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// Wait for one client on a fresh instance. Ok(false): the wait was cancelled.
fn connect_instance(instance: &OwnedHandle, event: &OwnedHandle, cancel: Option<&OwnedHandle>) -> io::Result<bool> {
    let mut overlapped = OVERLAPPED {
        hEvent: event.as_raw_handle(),
        ..Default::default()
    };
    // SAFETY: instance is a live overlapped pipe handle; overlapped outlives the
    // operation because every path below waits for its completion.
    if unsafe { ConnectNamedPipe(instance.as_raw_handle(), &mut overlapped) } != 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if is_error(&error, ERROR_PIPE_CONNECTED) {
        return Ok(true);
    }
    if !is_error(&error, ERROR_IO_PENDING) {
        return Err(error);
    }
    let handles: Vec<HANDLE> = std::iter::once(event.as_raw_handle())
        .chain(cancel.map(|c| c.as_raw_handle()))
        .collect();
    // SAFETY: handles are live for the call and the count matches.
    let waited = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE) };
    if waited != WAIT_OBJECT_0 {
        // SAFETY: cancel our own operation, then settle it before returning.
        unsafe { CancelIoEx(instance.as_raw_handle(), &overlapped) };
    }
    let mut transferred = 0;
    // SAFETY: waits for the (possibly cancelled) operation to settle.
    let settled = unsafe { GetOverlappedResult(instance.as_raw_handle(), &overlapped, &mut transferred, 1) };
    if settled != 0 {
        // Connected, even if cancellation woke us in the same instant: hand the
        // client out rather than dropping a connection it already sees as open.
        return Ok(true);
    }
    if waited != WAIT_OBJECT_0 {
        return Ok(false);
    }
    Err(io::Error::last_os_error())
}

fn open_process(pid: u32, access: u32) -> io::Result<OwnedHandle> {
    // SAFETY: scalar arguments; success returns a fresh non-inheritable handle.
    let raw = unsafe { OpenProcess(access, 0, pid) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a fresh process handle owned by nothing else.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// Rights kept on an accepted peer: watch its exit, query it, and duplicate
/// setup handles into it.
const PEER_ACCESS: u32 = PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_DUP_HANDLE;

pub(crate) struct PipeListener {
    name: Vec<u16>,
    descriptor: SecurityDescriptor,
    pending: Mutex<Option<OwnedHandle>>,
    connected: OwnedHandle,
    cancel: OwnedHandle,
    policy: PeerPolicy,
}

impl PipeListener {
    pub(crate) fn bind(endpoint: &Endpoint) -> Result<Self, Error> {
        let me = current()?;
        let name = wide(&render_for(endpoint, &me)?);
        let descriptor = SecurityDescriptor::new(endpoint.scope, &me)?;
        let first = create_instance(&name, &descriptor, true, PIPE_UNLIMITED_INSTANCES).map_err(|error| {
            // FILE_FLAG_FIRST_PIPE_INSTANCE: any existing pipe of this name,
            // ours or not, denies creation. Never share or take it over.
            if is_error(&error, ERROR_ACCESS_DENIED) || is_error(&error, ERROR_PIPE_BUSY) {
                Error::InUse
            } else {
                Error::Io(error)
            }
        })?;
        Ok(Self {
            name,
            descriptor,
            pending: Mutex::new(Some(first)),
            connected: event()?,
            cancel: event()?,
            policy: PeerPolicy {
                session: (endpoint.scope == Scope::Session).then_some(me.session),
            },
        })
    }

    pub(crate) fn accept(&self) -> Result<Connection, Error> {
        let mut pending = self.pending.lock().map_err(|_| io::Error::other("listener mutex poisoned"))?;
        if signaled(&self.cancel) {
            return Err(Error::Cancelled);
        }
        let instance = match pending.take() {
            Some(instance) => instance,
            None => create_instance(&self.name, &self.descriptor, false, PIPE_UNLIMITED_INSTANCES)?,
        };
        match connect_instance(&instance, &self.connected, Some(&self.cancel)) {
            Ok(true) => {}
            Ok(false) => {
                *pending = Some(instance);
                return Err(Error::Cancelled);
            }
            // For example a client that connected and left before we noticed
            // (ERROR_NO_DATA): this instance is spent. Keep the address served.
            Err(error) => {
                *pending = create_instance(&self.name, &self.descriptor, false, PIPE_UNLIMITED_INSTANCES).ok();
                return Err(Error::Io(error));
            }
        }
        // Keep the address served before handing this instance out.
        *pending = create_instance(&self.name, &self.descriptor, false, PIPE_UNLIMITED_INSTANCES).ok();
        drop(pending);
        let mut stream = PipeStream::new(instance, true)?;
        let (peer, process) = identify_client(&stream).map_err(|error| Error::RefusedPeer(format!("cannot identify peer: {error}")))?;
        self.policy.check(&peer)?;
        stream.peer_process = Some(Arc::new(process));
        Ok(Connection { stream, peer })
    }

    pub(crate) fn cancel(&self) {
        // SAFETY: the owned event is live.
        unsafe { SetEvent(self.cancel.as_raw_handle()) };
    }
}

fn identify_client(stream: &PipeStream) -> io::Result<(PeerIdentity, OwnedHandle)> {
    let pid = stream.peer_pid()?;
    let process = open_process(pid, PEER_ACCESS)?;
    let identity = identity_of(process.as_raw_handle())?;
    // The connection still holding its end (and naming the same process) means
    // the handle opened above is the connected peer, not a reused PID.
    if !stream.is_alive() || stream.peer_pid()? != pid {
        return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "peer left during identification"));
    }
    Ok((
        PeerIdentity {
            pid,
            user: identity.user,
            session: Some(stream.peer_session()?),
        },
        process,
    ))
}

pub(crate) fn connect(endpoint: &Endpoint, policy: &ServerPolicy) -> Result<Connection, Error> {
    connect_name(&render(endpoint)?, policy)
}

/// Open a pipe by its rendered name and verify its server against `policy`.
pub(crate) fn connect_name(name: &str, policy: &ServerPolicy) -> Result<Connection, Error> {
    let name = wide(name);
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let handle = loop {
        // SAFETY: name is NUL-terminated. Identification level lets the server
        // check who connected without being able to impersonate this client.
        let raw = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                ptr::null_mut(),
            )
        };
        if raw != INVALID_HANDLE_VALUE {
            // SAFETY: raw is a fresh handle owned by nothing else.
            break unsafe { OwnedHandle::from_raw_handle(raw) };
        }
        let error = io::Error::last_os_error();
        if !is_error(&error, ERROR_PIPE_BUSY) {
            return Err(error.into());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::from(io::ErrorKind::TimedOut).into());
        }
        // SAFETY: name is NUL-terminated. Failure just retries until the deadline.
        unsafe { WaitNamedPipeW(name.as_ptr(), milliseconds(Some(remaining)).max(1)) };
    };
    let stream = PipeStream::new(handle, false)?;
    let server = identify_server(&stream)?;
    policy.check(&server)?;
    Ok(Connection { stream, peer: server })
}

fn identify_server(stream: &PipeStream) -> io::Result<PeerIdentity> {
    let pid = stream.peer_pid()?;
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let identity = identity_of(process.as_raw_handle())?;
    if !stream.is_alive() || stream.peer_pid()? != pid {
        return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "server left during verification"));
    }
    Ok(PeerIdentity {
        pid,
        user: identity.user,
        session: Some(stream.peer_session()?),
    })
}

/// Two connected ends of a fresh private pipe in this process, like a socket
/// pair. The name is random, single-instance and protected like a user-scoped
/// endpoint; the client end is verified to be this process's own connection.
pub fn pipe_pair() -> io::Result<(PipeStream, PipeStream)> {
    let me = current()?;
    let name = wide(&format!(r"\\.\pipe\jackstay.pair.{}.{}", std::process::id(), random_hex()?));
    let descriptor = SecurityDescriptor::new(Scope::User, &me)?;
    let server = create_instance(&name, &descriptor, true, 1)?;
    // SAFETY: name is NUL-terminated; see connect_name.
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a fresh handle owned by nothing else.
    let client = unsafe { OwnedHandle::from_raw_handle(raw) };
    if !connect_instance(&server, &event()?, None)? {
        return Err(io::Error::other("pipe pair connection was cancelled"));
    }
    let server = PipeStream::new(server, true)?;
    if server.peer_pid()? != std::process::id() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "another process took the private pipe",
        ));
    }
    Ok((server, PipeStream::new(client, false)?))
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

struct PendingWrite {
    overlapped: OVERLAPPED,
    _data: Vec<u8>,
}

/// One end of a connected named pipe, used like a `UnixStream`: blocking by
/// default with optional timeouts, or nonblocking. Reads report EOF (`Ok(0)`)
/// once the peer has closed and its data is drained, or after local shutdown.
pub struct PipeStream {
    handle: OwnedHandle,
    server: bool,
    read_event: OwnedHandle,
    write_event: OwnedHandle,
    shutdown: Arc<OwnedHandle>,
    peer_process: Option<Arc<OwnedHandle>>,
    read_timeout: Cell<Option<Duration>>,
    write_timeout: Cell<Option<Duration>>,
    nonblocking: Cell<bool>,
    // A nonblocking write the pipe could not buffer at once. Boxed so its
    // OVERLAPPED and data stay put until the kernel completes it.
    pending: Option<Box<PendingWrite>>,
}

// SAFETY: every field is an owned kernel handle or plain data; the pending
// write's raw event pointer names this stream's own write event.
unsafe impl Send for PipeStream {}

impl std::fmt::Debug for PipeStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeStream")
            .field("handle", &self.handle)
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

/// Interrupts I/O on one stream: current and later reads see EOF, writes fail.
#[derive(Clone)]
pub struct PipeShutdown(Arc<OwnedHandle>);

impl PipeShutdown {
    pub fn shutdown(&self) {
        // SAFETY: the owned event is live.
        unsafe { SetEvent(self.0.as_raw_handle()) };
    }
}

fn read_error(error: io::Error) -> io::Result<usize> {
    if [ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED, ERROR_NO_DATA, ERROR_OPERATION_ABORTED]
        .iter()
        .any(|code| is_error(&error, *code))
    {
        Ok(0)
    } else {
        Err(error)
    }
}

fn write_error(error: io::Error) -> io::Error {
    if [ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED, ERROR_NO_DATA]
        .iter()
        .any(|code| is_error(&error, *code))
    {
        io::Error::new(io::ErrorKind::BrokenPipe, error)
    } else {
        error
    }
}

impl PipeStream {
    fn new(handle: OwnedHandle, server: bool) -> io::Result<Self> {
        Ok(Self {
            handle,
            server,
            read_event: event()?,
            write_event: event()?,
            shutdown: Arc::new(event()?),
            peer_process: None,
            read_timeout: Cell::new(None),
            write_timeout: Cell::new(None),
            nonblocking: Cell::new(false),
            pending: None,
        })
    }

    /// Adopt a pipe handle received from a trusted peer (see [`receive_handles`]).
    ///
    /// # Safety
    /// `handle` must be an exclusively owned, overlapped named-pipe handle that
    /// no other code uses. `server` states which end it is.
    pub unsafe fn from_owned_handle(handle: OwnedHandle, server: bool) -> io::Result<Self> {
        // SAFETY: the caller supplies a live owned handle.
        if unsafe { GetFileType(handle.as_raw_handle()) } != FILE_TYPE_PIPE {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "handle is not a pipe"));
        }
        Self::new(handle, server)
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        if timeout == Some(Duration::ZERO) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "zero timeout"));
        }
        self.read_timeout.set(timeout);
        Ok(())
    }

    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        if timeout == Some(Duration::ZERO) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "zero timeout"));
        }
        self.write_timeout.set(timeout);
        Ok(())
    }

    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(self.read_timeout.get())
    }

    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(self.write_timeout.get())
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.nonblocking.set(nonblocking);
        Ok(())
    }

    /// Give up this end's pipe handle, for example to duplicate it into the
    /// peer. A background write is completed first.
    pub fn into_handle(mut self) -> io::Result<OwnedHandle> {
        self.settle_pending(true)?;
        self.handle.try_clone()
    }

    /// Interrupt all I/O on this stream permanently, as `shutdown(Both)`.
    pub fn shutdown(&self) {
        self.shutdown_handle().shutdown();
    }

    #[must_use]
    pub fn shutdown_handle(&self) -> PipeShutdown {
        PipeShutdown(Arc::clone(&self.shutdown))
    }

    fn is_shut(&self) -> bool {
        signaled(&self.shutdown)
    }

    /// The peer still holds its end (buffered data counts as alive).
    #[must_use]
    pub fn is_alive(&self) -> bool {
        let mut available = 0;
        // SAFETY: live pipe handle; only the available count is requested.
        !self.is_shut()
            && unsafe {
                PeekNamedPipe(
                    self.handle.as_raw_handle(),
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    &mut available,
                    ptr::null_mut(),
                )
            } != 0
    }

    /// The connected peer's process ID, as the pipe reports it.
    pub fn peer_pid(&self) -> io::Result<u32> {
        let mut pid = 0;
        // SAFETY: live pipe handle and writable output.
        check(unsafe {
            if self.server {
                GetNamedPipeClientProcessId(self.handle.as_raw_handle(), &mut pid)
            } else {
                GetNamedPipeServerProcessId(self.handle.as_raw_handle(), &mut pid)
            }
        })?;
        Ok(pid)
    }

    /// The connected peer's Windows session ID.
    pub fn peer_session(&self) -> io::Result<u32> {
        let mut session = 0;
        // SAFETY: live pipe handle and writable output.
        check(unsafe {
            if self.server {
                GetNamedPipeClientSessionId(self.handle.as_raw_handle(), &mut session)
            } else {
                GetNamedPipeServerSessionId(self.handle.as_raw_handle(), &mut session)
            }
        })?;
        Ok(session)
    }

    /// The peer's process, with rights to watch it and duplicate handles into
    /// it. An accepted connection reuses the handle opened at accept. Otherwise
    /// (a pipe pair, or a client) it is opened now from the pipe's PID, then the
    /// connection is re-checked so the handle names the connected process.
    pub fn peer_process(&self) -> io::Result<Arc<OwnedHandle>> {
        if let Some(process) = &self.peer_process {
            return Ok(Arc::clone(process));
        }
        let pid = self.peer_pid()?;
        let process = open_process(pid, PEER_ACCESS)?;
        if !self.is_alive() || self.peer_pid()? != pid {
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "peer left during identification"));
        }
        Ok(Arc::new(process))
    }

    /// Start one overlapped transfer and wait for it, the shutdown event, or the
    /// timeout. Every path settles the operation before its buffer goes away.
    fn transfer(&self, write: bool, data: *mut u8, len: u32, timeout: Option<Duration>) -> io::Result<usize> {
        let event = if write { &self.write_event } else { &self.read_event };
        let handle = self.handle.as_raw_handle();
        let mut overlapped = OVERLAPPED {
            hEvent: event.as_raw_handle(),
            ..Default::default()
        };
        // SAFETY: data is valid for len bytes until this function returns, and
        // the operation is complete or cancelled-and-settled by then.
        let started = unsafe {
            if write {
                WriteFile(handle, data, len, ptr::null_mut(), &mut overlapped)
            } else {
                ReadFile(handle, data, len, ptr::null_mut(), &mut overlapped)
            }
        };
        if started == 0 {
            let error = io::Error::last_os_error();
            if !is_error(&error, ERROR_IO_PENDING) {
                return Err(error);
            }
            let handles = [event.as_raw_handle(), self.shutdown.as_raw_handle()];
            // SAFETY: both handles are live and the count matches.
            let waited = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, milliseconds(timeout)) };
            if waited != WAIT_OBJECT_0 {
                // SAFETY: cancel this operation, then wait for it to settle.
                unsafe { CancelIoEx(handle, &overlapped) };
                let mut transferred = 0;
                // SAFETY: as above; bWait blocks only until cancellation lands.
                let settled = unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 1) };
                if settled != 0 && transferred > 0 {
                    return Ok(transferred as usize);
                }
                return Err(match waited {
                    WAIT_TIMEOUT => io::Error::from(io::ErrorKind::TimedOut),
                    _ if self.is_shut() => io::Error::from_raw_os_error(ERROR_OPERATION_ABORTED as i32),
                    _ => io::Error::last_os_error(),
                });
            }
        }
        let mut transferred = 0;
        // SAFETY: the operation has completed; this only collects its result.
        check(unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 0) })?;
        Ok(transferred as usize)
    }

    /// Collect a background write. Ok(false): still running (only when not waiting).
    fn settle_pending(&mut self, wait: bool) -> io::Result<bool> {
        let Some(pending) = &self.pending else {
            return Ok(true);
        };
        let handle = self.handle.as_raw_handle();
        if wait {
            let handles = [self.write_event.as_raw_handle(), self.shutdown.as_raw_handle()];
            // SAFETY: both handles are live and the count matches.
            let waited = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, milliseconds(self.write_timeout.get())) };
            if waited != WAIT_OBJECT_0 {
                // SAFETY: cancel the background write and let it settle.
                unsafe { CancelIoEx(handle, &pending.overlapped) };
                let mut transferred = 0;
                // SAFETY: waits for the cancelled operation to settle.
                unsafe { GetOverlappedResult(handle, &pending.overlapped, &mut transferred, 1) };
                self.pending = None;
                return Err(if waited == WAIT_TIMEOUT {
                    io::ErrorKind::TimedOut.into()
                } else {
                    io::ErrorKind::BrokenPipe.into()
                });
            }
        }
        let mut transferred = 0;
        // SAFETY: bWait is false: this polls the boxed operation's state.
        if unsafe { GetOverlappedResult(handle, &pending.overlapped, &mut transferred, 0) } == 0 {
            let error = io::Error::last_os_error();
            if is_error(&error, ERROR_IO_INCOMPLETE) {
                return Ok(false);
            }
            self.pending = None;
            return Err(write_error(error));
        }
        self.pending = None;
        Ok(true)
    }

    fn write_nonblocking(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if !self.settle_pending(false)? {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let len = bytes.len().min(BUFFER as usize);
        let mut pending = Box::new(PendingWrite {
            overlapped: OVERLAPPED {
                hEvent: self.write_event.as_raw_handle(),
                ..Default::default()
            },
            _data: bytes[..len].to_vec(),
        });
        let handle = self.handle.as_raw_handle();
        // SAFETY: the boxed buffer and OVERLAPPED stay at fixed addresses until
        // settle_pending or Drop observes completion.
        let started = unsafe { WriteFile(handle, pending._data.as_ptr(), len as u32, ptr::null_mut(), &mut pending.overlapped) };
        if started == 0 {
            let error = io::Error::last_os_error();
            if !is_error(&error, ERROR_IO_PENDING) {
                return Err(write_error(error));
            }
            // Accepted: the kernel owns these bytes and completes them in order.
            self.pending = Some(pending);
            return Ok(len);
        }
        let mut transferred = 0;
        // SAFETY: completed synchronously; this collects its result.
        check(unsafe { GetOverlappedResult(handle, &pending.overlapped, &mut transferred, 0) }).map_err(write_error)?;
        Ok(transferred as usize)
    }
}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.is_shut() {
            return Ok(0);
        }
        let mut len = buf.len().min(u32::MAX as usize) as u32;
        if self.nonblocking.get() {
            let mut available = 0;
            // SAFETY: live pipe handle; only the available count is requested.
            let peeked = unsafe {
                PeekNamedPipe(
                    self.handle.as_raw_handle(),
                    ptr::null_mut(),
                    0,
                    ptr::null_mut(),
                    &mut available,
                    ptr::null_mut(),
                )
            };
            if peeked == 0 {
                return read_error(io::Error::last_os_error());
            }
            if available == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            len = len.min(available);
        }
        match self.transfer(false, buf.as_mut_ptr(), len, self.read_timeout.get()) {
            Ok(count) => Ok(count),
            Err(error) => read_error(error),
        }
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.is_shut() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if self.nonblocking.get() {
            return self.write_nonblocking(buf);
        }
        self.settle_pending(true)?;
        let len = buf.len().min(u32::MAX as usize) as u32;
        self.transfer(true, buf.as_ptr().cast_mut(), len, self.write_timeout.get())
            .map_err(|error| {
                if self.is_shut() {
                    io::ErrorKind::BrokenPipe.into()
                } else {
                    write_error(error)
                }
            })
    }

    /// Nonblocking: `WouldBlock` while an accepted write is still in flight.
    fn flush(&mut self) -> io::Result<()> {
        if self.settle_pending(!self.nonblocking.get())? {
            Ok(())
        } else {
            Err(io::ErrorKind::WouldBlock.into())
        }
    }
}

impl Drop for PipeStream {
    fn drop(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let handle = self.handle.as_raw_handle();
        // Give accepted bytes a bounded chance to reach a live reader; closing
        // the handle would otherwise cancel them.
        let handles = [self.write_event.as_raw_handle(), self.shutdown.as_raw_handle()];
        // SAFETY: both handles are live and the count matches.
        let waited = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, milliseconds(Some(DROP_FLUSH))) };
        if waited != WAIT_OBJECT_0 {
            // SAFETY: cancel the operation whose buffer is about to be freed.
            unsafe { CancelIoEx(handle, &pending.overlapped) };
        }
        let mut transferred = 0;
        // SAFETY: waits until the kernel no longer references the box.
        unsafe { GetOverlappedResult(handle, &pending.overlapped, &mut transferred, 1) };
    }
}

// ---------------------------------------------------------------------------
// Handle transfer
// ---------------------------------------------------------------------------

/// Access for a duplicated handle in the peer.
#[derive(Debug, Clone, Copy)]
pub enum Access {
    /// The access this process holds.
    Same,
    /// Exactly these rights (never more than the source handle allows).
    Rights(u32),
}

/// Duplicate owned handles into the stream's verified peer process and send the
/// resulting values. The sender's own copies are closed during duplication,
/// before the peer can learn any value, so the peer's copies are the only
/// ones left. Waits for the peer to acknowledge adopting them.
///
/// If sending the values fails, the duplicates are closed again in the peer,
/// which cannot have seen them. A missing acknowledgement is an error but the
/// duplicates stay with the peer, which may already own them; they die with it.
pub fn send_handles(stream: &mut PipeStream, objects: Vec<(OwnedHandle, Access)>) -> io::Result<()> {
    if objects.is_empty() || objects.len() > MAX_HANDLES {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid handle transfer count"));
    }
    let process = stream.peer_process()?;
    let mut values = Vec::with_capacity(objects.len());
    let mut failure = None;
    for (object, access) in objects {
        if failure.is_some() {
            continue; // dropping closes the remaining local copies
        }
        let (rights, options) = match access {
            Access::Same => (0, DUPLICATE_SAME_ACCESS | DUPLICATE_CLOSE_SOURCE),
            Access::Rights(rights) => (rights, DUPLICATE_CLOSE_SOURCE),
        };
        let mut duplicate = ptr::null_mut();
        // SAFETY: the source handle is owned and live; DUPLICATE_CLOSE_SOURCE
        // closes it whatever the outcome, so ownership passes to the call.
        let duplicated = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                object.into_raw_handle(),
                process.as_raw_handle(),
                &mut duplicate,
                rights,
                0,
                options,
            )
        };
        if duplicated == 0 {
            failure = Some(io::Error::last_os_error());
        } else {
            values.push(duplicate as usize as u64);
        }
    }
    let result = match failure {
        Some(error) => Err(error),
        None => {
            let mut message = Vec::with_capacity(4 + values.len() * 8);
            message.extend_from_slice(&(values.len() as u32).to_le_bytes());
            for value in &values {
                message.extend_from_slice(&value.to_le_bytes());
            }
            stream.write_all(&message).and_then(|()| stream.flush())
        }
    };
    if let Err(error) = result {
        for value in values {
            // SAFETY: the peer never received these values; closing them in the
            // peer (DUPLICATE_CLOSE_SOURCE with no target) removes our grant.
            unsafe {
                DuplicateHandle(
                    process.as_raw_handle(),
                    value as usize as RawHandle,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    0,
                    0,
                    DUPLICATE_CLOSE_SOURCE,
                )
            };
        }
        return Err(error);
    }
    let mut receipt = [0];
    stream.read_exact(&mut receipt)?;
    if receipt != [HANDLES_RECEIVED] {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "peer did not acknowledge handles"));
    }
    Ok(())
}

/// Adopt exactly `count` handles a trusted peer duplicated into this process
/// with [`send_handles`], and acknowledge them.
///
/// Only use this on a stream whose peer was verified (a [`super::connect`]ed
/// server, or an accepted client) and which follows this protocol: the values
/// name handles in this process that nothing else owns. A wrong count still
/// adopts and closes what was sent, and fails.
pub fn receive_handles(stream: &mut PipeStream, count: usize) -> io::Result<Vec<OwnedHandle>> {
    let mut header = [0; 4];
    stream.read_exact(&mut header)?;
    let sent = u32::from_le_bytes(header) as usize;
    if sent == 0 || sent > MAX_HANDLES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid handle transfer count"));
    }
    let mut values = vec![0; sent * 8];
    stream.read_exact(&mut values)?;
    let mut handles = Vec::with_capacity(sent);
    for value in values.chunks_exact(8) {
        let value = u64::from_le_bytes(value.try_into().expect("eight bytes"));
        if value == 0
            || handles
                .iter()
                .any(|handle: &OwnedHandle| handle.as_raw_handle() as usize as u64 == value)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid transferred handle value"));
        }
        // SAFETY: the verified peer duplicated this handle into this process
        // for this receiver alone (see the function contract).
        handles.push(unsafe { OwnedHandle::from_raw_handle(value as usize as RawHandle) });
    }
    stream.write_all(&[HANDLES_RECEIVED])?;
    stream.flush()?;
    if sent != count {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected transferred handle count"));
    }
    Ok(handles)
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;
    use crate::local::{Listener, Transport};

    fn unique(prefix: &str) -> Endpoint {
        Endpoint::new(
            Scope::User,
            &format!("{prefix}-{}-{}", std::process::id(), &random_hex().unwrap()[..8]),
            Transport::LocalStream,
        )
        .unwrap()
    }

    #[test]
    fn rendering_is_direct_and_scoped_by_user_and_session() {
        let me = current().unwrap();
        let user = render_for(&Endpoint::new(Scope::User, "cpu", Transport::LocalStream).unwrap(), &me).unwrap();
        assert_eq!(user, format!(r"\\.\pipe\jackstay.{}.cpu", me.user));
        let session = render_for(&Endpoint::new(Scope::Session, "cpu", Transport::LocalStream).unwrap(), &me).unwrap();
        assert_eq!(session, format!(r"\\.\pipe\jackstay.{}.s{}.cpu", me.user, me.session));
    }

    #[test]
    fn accept_and_connect_report_each_others_verified_identity() {
        let endpoint = unique("identity");
        let listener = Listener::bind(&endpoint).unwrap();
        let client = thread::spawn(move || super::super::connect(&endpoint).unwrap());
        let accepted = listener.accept().unwrap();
        let connected = client.join().unwrap();
        let me = current().unwrap();
        for peer in [accepted.peer(), connected.peer()] {
            assert_eq!(peer.pid, std::process::id());
            assert_eq!(peer.user, me.user);
            assert_eq!(peer.session, Some(me.session));
        }
    }

    #[test]
    fn a_pipe_name_already_taken_is_refused_rather_than_shared() {
        let endpoint = unique("first-instance");
        let _owner = Listener::bind(&endpoint).unwrap();
        assert!(matches!(Listener::bind(&endpoint), Err(Error::InUse)));
        // Another user's (or any) pre-existing pipe of the rendered name: create
        // it directly with a default DACL, as a squatter would.
        let squatted = unique("squatted");
        let name = wide(&render(&squatted).unwrap());
        // SAFETY: name is NUL-terminated; null attributes give the default DACL.
        let raw = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER,
                BUFFER,
                0,
                ptr::null(),
            )
        };
        assert_ne!(raw, INVALID_HANDLE_VALUE);
        // SAFETY: fresh handle owned by this test.
        let _squatter = unsafe { OwnedHandle::from_raw_handle(raw) };
        assert!(matches!(Listener::bind(&squatted), Err(Error::InUse)));
    }

    #[test]
    fn a_client_refuses_a_server_not_owned_by_the_expected_user() {
        // Real server, injected expectation: the check compares the server's
        // actual token owner against the policy, so SYSTEM must not match us.
        let endpoint = unique("owner");
        let listener = Listener::bind(&endpoint).unwrap();
        let name = render(&endpoint).unwrap();
        let accepting = thread::spawn(move || {
            let _ = listener.accept();
        });
        let policy = ServerPolicy {
            user: "S-1-5-18".into(),
            session: None,
        };
        assert!(matches!(connect_name(&name, &policy), Err(Error::UntrustedServer(_))));
        accepting.join().unwrap();
    }

    #[test]
    fn a_client_refuses_a_real_server_owned_by_another_account() {
        // The RPC endpoint mapper's pipe is served by a service account on every
        // Windows installation, never by the test user.
        let policy = ServerPolicy::current(Scope::User).unwrap();
        match connect_name(r"\\.\pipe\epmapper", &policy) {
            Err(Error::UntrustedServer(message)) => assert!(message.contains("runs as"), "{message}"),
            // Some hardened hosts deny opening the service process itself.
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied => {}
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_session_bound_client_refuses_a_server_in_another_session() {
        let me = current().unwrap();
        let policy = ServerPolicy {
            user: me.user.clone(),
            session: Some(me.session),
        };
        let other_session = PeerIdentity {
            pid: 4242,
            user: me.user.clone(),
            session: Some(me.session + 1),
        };
        assert!(matches!(policy.check(&other_session), Err(Error::UntrustedServer(_))));
    }

    #[test]
    fn a_session_bound_endpoint_refuses_a_peer_from_another_session() {
        // Creating a second Windows session needs another logon; inject the
        // accepted identity instead. The same check runs on every accept.
        let me = current().unwrap();
        let policy = PeerPolicy { session: Some(me.session) };
        let session0 = PeerIdentity {
            pid: 4242,
            user: me.user.clone(),
            session: Some(0),
        };
        let desktop = PeerIdentity {
            session: Some(me.session),
            ..session0.clone()
        };
        if me.session != 0 {
            assert!(matches!(policy.check(&session0), Err(Error::RefusedPeer(_))));
        }
        assert!(policy.check(&desktop).is_ok());
        assert!(PeerPolicy { session: None }.check(&session0).is_ok());
    }

    #[test]
    fn a_session_bound_endpoint_serves_its_own_session() {
        let endpoint = Endpoint::new(
            Scope::Session,
            &format!("session-{}-{}", std::process::id(), &random_hex().unwrap()[..8]),
            Transport::LocalStream,
        )
        .unwrap();
        let listener = Listener::bind(&endpoint).unwrap();
        // The client keeps its end until accept has identified it.
        let client = thread::spawn(move || super::super::connect(&endpoint));
        let accepted = listener.accept().unwrap();
        assert_eq!(accepted.peer().session, Some(current().unwrap().session));
        let connected = client.join().unwrap().unwrap();
        assert_eq!(connected.peer().pid, std::process::id());
    }

    #[test]
    fn endpoints_reject_remote_clients() {
        // PIPE_REJECT_REMOTE_CLIENTS: the SMB path to this same machine is a
        // remote client and is refused, unlike the local \\.\ path.
        let endpoint = unique("remote");
        let listener = Listener::bind(&endpoint).unwrap();
        let local = render(&endpoint).unwrap();
        let remote = local.replacen(r"\\.\", r"\\127.0.0.1\", 1);
        let policy = ServerPolicy::current(Scope::User).unwrap();
        let accepting = thread::spawn(move || listener.accept().map(|connection| connection.peer().pid));
        let refused = connect_name(&remote, &policy);
        // Control: the same protected DACL without PIPE_REJECT_REMOTE_CLIENTS.
        // Where the loopback SMB path works at all (it needs the Server
        // service), it reaches that pipe, so the refusal above is the flag's.
        let control_local = format!(r"\\.\pipe\jackstay-control-{}", random_hex().unwrap());
        let descriptor = SecurityDescriptor::new(Scope::User, &current().unwrap()).unwrap();
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        // SAFETY: NUL-terminated name; attributes outlive the call.
        let raw = unsafe {
            CreateNamedPipeW(
                wide(&control_local).as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                1,
                BUFFER,
                BUFFER,
                0,
                &attributes,
            )
        };
        assert_ne!(raw, INVALID_HANDLE_VALUE);
        // SAFETY: fresh handle owned by this test.
        let _control = unsafe { OwnedHandle::from_raw_handle(raw) };
        let control_remote = control_local.replacen(r"\\.\", r"\\127.0.0.1\", 1);
        // SAFETY: NUL-terminated name; the handle is closed below if opened.
        let raw = unsafe {
            CreateFileW(
                wide(&control_remote).as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            eprintln!(
                "loopback SMB unavailable ({}); only the refusal is checked",
                io::Error::last_os_error()
            );
        } else {
            // SAFETY: fresh handle owned by this test.
            drop(unsafe { OwnedHandle::from_raw_handle(raw) });
        }
        match refused {
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied || raw == INVALID_HANDLE_VALUE => {}
            other => panic!("remote client was not refused: {other:?}"),
        }
        // The local path still reaches the same listener.
        let _held = connect_name(&local, &policy).unwrap();
        assert_eq!(accepting.join().unwrap().unwrap(), std::process::id());
    }

    #[test]
    fn cancel_wakes_a_blocked_accept() {
        let listener = Arc::new(Listener::bind(&unique("cancel")).unwrap());
        let waiting = {
            let listener = Arc::clone(&listener);
            thread::spawn(move || listener.accept())
        };
        thread::sleep(Duration::from_millis(50));
        listener.cancel();
        assert!(matches!(waiting.join().unwrap(), Err(Error::Cancelled)));
    }

    #[test]
    fn streams_time_out_shut_down_and_report_peer_closure() {
        let (mut server, mut client) = pipe_pair().unwrap();
        assert!(server.is_alive() && client.is_alive());
        server.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let mut byte = [0];
        assert_eq!(server.read(&mut byte).unwrap_err().kind(), io::ErrorKind::TimedOut);
        server.set_nonblocking(true).unwrap();
        assert_eq!(server.read(&mut byte).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        client.write_all(b"xy").unwrap();
        drop(client);
        // Data written before closure is still delivered, then EOF.
        let mut data = [0; 2];
        server.set_nonblocking(false).unwrap();
        server.read_exact(&mut data).unwrap();
        assert_eq!(&data, b"xy");
        assert_eq!(server.read(&mut byte).unwrap(), 0);
        assert!(!server.is_alive());

        let (mut server, _client) = pipe_pair().unwrap();
        let shutdown = server.shutdown_handle();
        let reader = thread::spawn(move || server.read(&mut [0]).unwrap());
        thread::sleep(Duration::from_millis(50));
        shutdown.shutdown();
        assert_eq!(reader.join().unwrap(), 0);
    }

    #[test]
    fn nonblocking_writes_beyond_the_buffer_complete_in_order() {
        let (mut server, mut client) = pipe_pair().unwrap();
        server.set_nonblocking(true).unwrap();
        let payload: Vec<u8> = (0..(BUFFER as usize * 3)).map(|index| index as u8).collect();
        let reader = thread::spawn(move || {
            let mut received = vec![0; BUFFER as usize * 3];
            client.read_exact(&mut received).unwrap();
            received
        });
        let mut offset = 0;
        while offset < payload.len() {
            match server.write(&payload[offset..]) {
                Ok(count) => offset += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(1)),
                Err(error) => panic!("{error}"),
            }
        }
        while let Err(error) = server.flush() {
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(reader.join().unwrap(), payload);
    }

    #[test]
    fn handles_move_into_the_peer_with_the_requested_access() {
        use windows_sys::Win32::System::Threading::EVENT_MODIFY_STATE;
        let (mut server, mut client) = pipe_pair().unwrap();
        let receiving = thread::spawn(move || receive_handles(&mut client, 2).unwrap());
        send_handles(
            &mut server,
            vec![
                (event().unwrap(), Access::Same),
                (event().unwrap(), Access::Rights(EVENT_MODIFY_STATE)),
            ],
        )
        .unwrap();
        let received = receiving.join().unwrap();
        // SAFETY: owned live event handles.
        assert_ne!(unsafe { SetEvent(received[1].as_raw_handle()) }, 0);
        // The restricted duplicate cannot be waited on.
        // SAFETY: as above.
        assert_eq!(
            unsafe { WaitForSingleObject(received[1].as_raw_handle(), 0) },
            windows_sys::Win32::Foundation::WAIT_FAILED
        );
        // SAFETY: as above.
        assert_eq!(unsafe { WaitForSingleObject(received[0].as_raw_handle(), 0) }, WAIT_TIMEOUT);
    }
}
