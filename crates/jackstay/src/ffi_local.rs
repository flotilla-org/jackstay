//! C ownership boundary for Local Endpoints (Wheelhouse ADR 0011).
//!
//! A host binds a listener and accepts connections carrying the verified peer
//! identity; a client connects by logical address and the library verifies the
//! server. Either side then hands the connection to exactly one setup call
//! (CPU setup, source bootstrap or input), which consumes it. These are the
//! Windows setup entry points; the FD-taking calls remain for POSIX callers.

use std::{
    ffi::{CStr, c_char},
    ptr,
};

use crate::{
    ffi::*,
    local::{self, Connection, Endpoint, Error, Listener, PeerIdentity, Scope, Stream, Transport},
};

pub const FT_ENDPOINT_SCOPE_USER: u32 = 1;
pub const FT_ENDPOINT_SCOPE_SESSION: u32 = 2;
pub const FT_ENDPOINT_TRANSPORT_LOCAL_STREAM: u32 = 1;

/// A logical address: scope, name and transport kind.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtLocalEndpoint {
    pub scope: u32,
    pub transport: u32,
    pub name: *const c_char,
}

/// Kernel-reported identity of a connection's peer.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtPeerIdentity {
    pub pid: u32,
    pub session: u32,
    pub has_session: u32,
    pub reserved: u32,
    /// NUL-terminated: the user SID string on Windows, the decimal UID on Unix.
    pub user: [c_char; 192],
}

impl Default for FtPeerIdentity {
    fn default() -> Self {
        Self {
            pid: 0,
            session: 0,
            has_session: 0,
            reserved: 0,
            user: [0; 192],
        }
    }
}

pub struct FtLocalListener(Listener);

/// One connected stream and its verified peer, until a setup call consumes it.
pub struct FtLocalConnection {
    pub(crate) stream: Stream,
    pub(crate) peer: PeerIdentity,
}

impl From<Connection> for FtLocalConnection {
    fn from(connection: Connection) -> Self {
        let peer = connection.peer().clone();
        Self {
            stream: connection.into_stream(),
            peer,
        }
    }
}

fn status(error: &Error) -> FtStatus {
    match error {
        Error::InUse => FT_STATUS_ADDRESS_IN_USE,
        Error::UntrustedServer(_) | Error::RefusedPeer(_) => FT_STATUS_UNTRUSTED_PEER,
        Error::Cancelled => FT_STATUS_CANCELLED,
        Error::Io(error) if error.kind() == std::io::ErrorKind::TimedOut => FT_STATUS_TIMEOUT,
        Error::Io(error) if error.kind() == std::io::ErrorKind::InvalidInput => FT_STATUS_INVALID_ARGUMENT,
        Error::Io(_) => FT_STATUS_ERROR,
    }
}

/// # Safety
/// `endpoint` is null or points to a live descriptor whose name is null or a
/// NUL-terminated string valid for this call.
unsafe fn endpoint(endpoint: *const FtLocalEndpoint) -> Result<Endpoint, FtStatus> {
    // SAFETY: caller guarantees a live descriptor or null.
    let endpoint = unsafe { endpoint.as_ref() }.ok_or(FT_STATUS_INVALID_ARGUMENT)?;
    if endpoint.transport != FT_ENDPOINT_TRANSPORT_LOCAL_STREAM {
        return Err(FT_STATUS_UNSUPPORTED);
    }
    let scope = match endpoint.scope {
        FT_ENDPOINT_SCOPE_USER => Scope::User,
        FT_ENDPOINT_SCOPE_SESSION => Scope::Session,
        _ => return Err(FT_STATUS_INVALID_ARGUMENT),
    };
    if endpoint.name.is_null() {
        return Err(FT_STATUS_INVALID_ARGUMENT);
    }
    // SAFETY: non-null NUL-terminated name, live for this call.
    let name = unsafe { CStr::from_ptr(endpoint.name) }
        .to_str()
        .map_err(|_| FT_STATUS_INVALID_ARGUMENT)?;
    Endpoint::new(scope, name, Transport::LocalStream).map_err(|_| FT_STATUS_INVALID_ARGUMENT)
}

/// Take ownership of a connection handle after basic validation, nulling the
/// caller's pointer. None means the argument was invalid and nothing moved.
///
/// # Safety
/// `connection` is null or a writable pointer to null or a live, exclusively
/// owned connection handle.
pub(crate) unsafe fn take_connection(connection: *mut *mut FtLocalConnection) -> Option<Box<FtLocalConnection>> {
    // SAFETY: caller guarantees writable storage or null.
    let slot = unsafe { connection.as_mut() }?;
    if slot.is_null() {
        return None;
    }
    // SAFETY: this API allocated the box; ownership moves exactly once.
    Some(unsafe { Box::from_raw(std::mem::replace(slot, ptr::null_mut())) })
}

/// Hand a connection back to the caller in `slot`, e.g. after bootstrap.
///
/// # Safety
/// `slot` is writable and currently null.
pub(crate) unsafe fn restore_connection(slot: *mut *mut FtLocalConnection, connection: FtLocalConnection) {
    // SAFETY: caller guarantees writable storage.
    unsafe { *slot = Box::into_raw(Box::new(connection)) };
}

/// Render an endpoint's platform address (a socket path or `\\.\pipe\` name)
/// into `out`, NUL-terminated, for diagnostics. Binding and connecting render
/// it themselves.
///
/// # Safety
/// `endpoint` is live; `out` is writable for `len` bytes and does not alias it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_local_endpoint_render(endpoint_ptr: *const FtLocalEndpoint, out: *mut c_char, len: usize) -> FtStatus {
    if out.is_null() || len == 0 {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: caller supplies a live descriptor.
    let endpoint = match unsafe { endpoint(endpoint_ptr) } {
        Ok(endpoint) => endpoint,
        Err(status) => return status,
    };
    let Ok(rendered) = endpoint.render() else {
        return FT_STATUS_ERROR;
    };
    // SAFETY: caller supplies a writable buffer of len bytes.
    if unsafe { crate::daemon::copy_string_to_c_buffer(&rendered, out, len) } {
        FT_STATUS_OK
    } else {
        FT_STATUS_CAPACITY
    }
}

/// Create a Local Endpoint. ADDRESS_IN_USE: another listener (or, on Windows,
/// any pipe of that name) already holds it; it is never shared or taken over.
///
/// # Safety
/// `endpoint` is live; `out` is writable, disjoint and starts null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_local_listener_create(endpoint_ptr: *const FtLocalEndpoint, out: *mut *mut FtLocalListener) -> FtStatus {
    // SAFETY: caller supplies writable output storage.
    let Some(out) = (unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: caller supplies a live descriptor.
    let endpoint = match unsafe { endpoint(endpoint_ptr) } {
        Ok(endpoint) => endpoint,
        Err(status) => return status,
    };
    match Listener::bind(&endpoint) {
        Ok(listener) => {
            *out = Box::into_raw(Box::new(FtLocalListener(listener)));
            FT_STATUS_OK
        }
        Err(error) => status(&error),
    }
}

/// Block until one client connects. UNTRUSTED_PEER: the peer failed the
/// endpoint's policy (another Windows session on a session-bound endpoint) or
/// could not be identified; it was disconnected and the listener stays usable.
/// CANCELLED after `ft_local_listener_cancel`.
///
/// # Safety
/// `listener` is live; `out` is writable, disjoint and starts null. Accept and
/// cancel may overlap; destroy must not.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_local_listener_accept(listener: *const FtLocalListener, out: *mut *mut FtLocalConnection) -> FtStatus {
    // SAFETY: caller supplies a live listener and writable output.
    let (Some(listener), Some(out)) = (unsafe { listener.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    match listener.0.accept() {
        Ok(connection) => {
            *out = Box::into_raw(Box::new(connection.into()));
            FT_STATUS_OK
        }
        Err(error) => status(&error),
    }
}

/// Permanently wake and fail current and later accepts. Thread-safe.
///
/// # Safety
/// `listener` is null or live until this call returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_local_listener_cancel(listener: *const FtLocalListener) {
    // SAFETY: caller keeps the handle alive for this call.
    if let Some(listener) = unsafe { listener.as_ref() } {
        listener.0.cancel();
    }
}

/// Connect by logical address and verify the server before returning: it must
/// run as the current user and, for a session-bound endpoint, in the current
/// Windows session (UNTRUSTED_PEER otherwise). Waits up to five seconds for a
/// busy endpoint; run off GUI/input threads.
///
/// # Safety
/// `endpoint` is live; `out` is writable, disjoint and starts null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_local_connect(endpoint_ptr: *const FtLocalEndpoint, out: *mut *mut FtLocalConnection) -> FtStatus {
    // SAFETY: caller supplies writable output storage.
    let Some(out) = (unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: caller supplies a live descriptor.
    let endpoint = match unsafe { endpoint(endpoint_ptr) } {
        Ok(endpoint) => endpoint,
        Err(status) => return status,
    };
    match local::connect(&endpoint) {
        Ok(connection) => {
            *out = Box::into_raw(Box::new(connection.into()));
            FT_STATUS_OK
        }
        Err(error) => status(&error),
    }
}

/// Copy the connection's verified peer identity.
///
/// # Safety
/// `connection` is live; `out` is writable and disjoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_local_connection_peer(connection: *const FtLocalConnection, out: *mut FtPeerIdentity) -> FtStatus {
    // SAFETY: caller supplies live, disjoint pointers.
    let (Some(connection), Some(out)) = (unsafe { connection.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let mut identity = FtPeerIdentity {
        pid: connection.peer.pid,
        session: connection.peer.session.unwrap_or(0),
        has_session: u32::from(connection.peer.session.is_some()),
        ..Default::default()
    };
    // SAFETY: the fixed-size buffer is owned by `identity`.
    if !unsafe { crate::daemon::copy_string_to_c_buffer(&connection.peer.user, identity.user.as_mut_ptr(), identity.user.len()) } {
        return FT_STATUS_ERROR;
    }
    *out = identity;
    FT_STATUS_OK
}

/// OK while the peer holds its end; CLOSED once it has closed or failed.
/// Never consumes protocol bytes.
///
/// # Safety
/// `connection` is live and not concurrently consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_local_connection_alive(connection: *const FtLocalConnection) -> FtStatus {
    // SAFETY: caller supplies a live handle.
    let Some(connection) = (unsafe { connection.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if local::is_alive(&connection.stream) {
        FT_STATUS_OK
    } else {
        FT_STATUS_CLOSED
    }
}

/// An absolute deadline for one host exchange. Every read or write is bounded
/// by the time left, so a peer that trickles bytes cannot stretch the call
/// past `timeout_ms` (as in the bootstrap handshake).
struct Deadline(std::time::Instant);

impl Deadline {
    fn bound(&self, stream: &Stream) -> std::io::Result<()> {
        let remaining = self
            .0
            .checked_duration_since(std::time::Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::TimedOut))?;
        stream.set_read_timeout(Some(remaining))?;
        stream.set_write_timeout(Some(remaining))
    }
}

/// Run `exchange` within `timeout_ms` (nonzero) from now, then restore the
/// stream's blocking defaults for setup.
fn with_deadline(
    stream: &mut Stream,
    timeout_ms: u32,
    exchange: impl FnOnce(&mut Stream, &Deadline) -> std::io::Result<FtStatus>,
) -> FtStatus {
    let deadline = Deadline(std::time::Instant::now() + std::time::Duration::from_millis(u64::from(timeout_ms)));
    let result = exchange(stream, &deadline);
    let restored = stream.set_read_timeout(None).is_ok() && stream.set_write_timeout(None).is_ok();
    match result {
        Ok(_) if !restored => FT_STATUS_ERROR,
        Ok(status) => status,
        Err(error) if matches!(error.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => FT_STATUS_TIMEOUT,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ) =>
        {
            FT_STATUS_CLOSED
        }
        Err(_) => FT_STATUS_ERROR,
    }
}

/// Write all of `data` on an unconsumed connection: a host's own exchange
/// before handing the connection to a setup call, such as presenting a
/// host-issued attach token. Jackstay adds no framing. OK: every byte was
/// written; CLOSED: the peer is gone; TIMEOUT: `timeout_ms` elapsed. After any
/// failure the stream position is unknown: destroy the connection.
///
/// # Safety
/// `connection` is live, exclusively owned and not concurrently used;
/// `data` is readable for `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_local_connection_write(
    connection: *mut FtLocalConnection,
    data: *const u8,
    len: usize,
    timeout_ms: u32,
) -> FtStatus {
    // SAFETY: caller supplies a live, exclusively owned handle.
    let Some(connection) = (unsafe { connection.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if data.is_null() || len == 0 || timeout_ms == 0 {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: caller guarantees `len` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    with_deadline(&mut connection.stream, timeout_ms, |stream, deadline| {
        use std::io::Write;
        let mut rest = bytes;
        while !rest.is_empty() {
            deadline.bound(stream)?;
            match stream.write(rest) {
                Ok(0) => return Ok(FT_STATUS_CLOSED),
                Ok(count) => rest = &rest[count..],
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        deadline.bound(stream)?;
        stream.flush()?;
        Ok(FT_STATUS_OK)
    })
}

/// Read a host's reply up to and including the first `delimiter` byte, one
/// byte at a time, so no byte after it (such as the start of setup) is
/// consumed. `*out_len` counts the delimiter. CAPACITY: `capacity` bytes
/// arrived without it; CLOSED: the peer closed first; TIMEOUT: `timeout_ms`
/// elapsed. After any failure destroy the connection.
///
/// # Safety
/// `connection` is live, exclusively owned and not concurrently used; `out` is
/// writable for `capacity` bytes and `out_len` is writable; neither aliases.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_local_connection_read_until(
    connection: *mut FtLocalConnection,
    delimiter: u8,
    out: *mut u8,
    capacity: usize,
    out_len: *mut usize,
    timeout_ms: u32,
) -> FtStatus {
    // SAFETY: caller supplies live, disjoint pointers.
    let (Some(connection), Some(out_len)) = (unsafe { connection.as_mut() }, unsafe { out_len.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if out.is_null() || capacity == 0 || timeout_ms == 0 {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    *out_len = 0;
    // SAFETY: caller guarantees `capacity` writable bytes, disjoint from the rest.
    let buffer = unsafe { std::slice::from_raw_parts_mut(out, capacity) };
    let mut filled = 0;
    let status = with_deadline(&mut connection.stream, timeout_ms, |stream, deadline| {
        use std::io::Read;
        while filled < capacity {
            let mut byte = [0];
            deadline.bound(stream)?;
            match stream.read(&mut byte) {
                Ok(0) => return Ok(FT_STATUS_CLOSED),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
            buffer[filled] = byte[0];
            filled += 1;
            if byte[0] == delimiter {
                return Ok(FT_STATUS_OK);
            }
        }
        Ok(FT_STATUS_CAPACITY)
    });
    *out_len = filled;
    status
}

/// Close an unconsumed connection. Null input/handle is harmless.
///
/// # Safety
/// Pointer is writable and exclusively owns its handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_local_connection_destroy(connection: *mut *mut FtLocalConnection) {
    // SAFETY: caller supplies exclusive handle storage.
    drop(unsafe { take_connection(connection) });
}

/// Destroy a listener, releasing its address. Null input/handle is harmless.
///
/// # Safety
/// Pointer is writable and exclusively owns its handle; no accept or cancel
/// call may still be running.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_local_listener_destroy(listener: *mut *mut FtLocalListener) {
    // SAFETY: caller supplies exclusive handle storage.
    if let Some(listener) = unsafe { listener.as_mut() }
        && !listener.is_null()
    {
        // SAFETY: this API allocated the box; consumed once after clearing it.
        drop(unsafe { Box::from_raw(std::mem::replace(listener, ptr::null_mut())) });
    }
}
