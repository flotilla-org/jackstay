//! CPU arena setup on an already selected/authorized local connection: a
//! connected Unix stream, or a named pipe on Windows ([`crate::local`]).
//! The host owns routing and authority. This protocol only admits one process
//! incarnation and transfers its initial/replacement maps; frames never use it.

#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};
#[cfg(windows)]
use std::os::windows::io::{AsHandle, OwnedHandle as OwnedFd};
use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

use super::{
    IncarnationId,
    arena::{
        ArenaConsumer, ArenaError, ArenaProducer, ConfigurationDescriptor, ConfigurationGrant, ConfigurationInstall, ConsumerGrant,
        GrantDescriptor,
    },
};
use crate::local::Stream;

const MAGIC: &[u8; 8] = b"JSCPU001";
const MAX_MESSAGE: usize = 16 * 1024;
#[cfg(unix)]
const TRANSFER_FINISHED: u8 = 1;

#[derive(Debug, Error)]
pub enum SocketError {
    #[error("CPU setup I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("CPU setup JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Arena(#[from] ArenaError),
    #[error(transparent)]
    Transfer(#[from] crate::CaptureTransferError),
    #[error("invalid CPU setup protocol: {0}")]
    Protocol(&'static str),
    #[error("CPU setup request rejected: {0}")]
    Rejected(String),
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Attach { holding: u32 },
    Configuration,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Response {
    Attached { descriptor: GrantDescriptor },
    Configuration { descriptor: ConfigurationDescriptor },
    Empty,
    Rejected { message: String },
}

fn read_message<T: DeserializeOwned>(stream: &mut Stream) -> Result<Option<T>, SocketError> {
    let mut header = [0; 12];
    loop {
        match stream.read(&mut header[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    stream.read_exact(&mut header[1..])?;
    if &header[..8] != MAGIC {
        return Err(SocketError::Protocol("unsupported version"));
    }
    let len = u32::from_le_bytes(header[8..].try_into().expect("four bytes")) as usize;
    if len == 0 || len > MAX_MESSAGE {
        return Err(SocketError::Protocol("message size exceeds limit"));
    }
    let mut bytes = vec![0; len];
    // Exact reads cannot consume the following ancillary byte before recvmsg.
    stream.read_exact(&mut bytes)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn write_message<T: Serialize>(stream: &mut Stream, value: &T) -> Result<(), SocketError> {
    // Setup accepts host-owned streams; apply this at the fallible write boundary
    // rather than making the infallible CpuSetupClient constructor fallible.
    #[cfg(unix)]
    crate::socket_options::suppress_sigpipe(stream)?;
    let bytes = serde_json::to_vec(value)?;
    if bytes.is_empty() || bytes.len() > MAX_MESSAGE {
        return Err(SocketError::Protocol("message size exceeds limit"));
    }
    stream.write_all(MAGIC)?;
    stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}

/// Obtain the peer's kernel identity, never a PID supplied in request metadata.
#[cfg(unix)]
pub fn peer_pid(stream: &std::os::unix::net::UnixStream) -> Result<u32, SocketError> {
    #[cfg(target_os = "macos")]
    let (level, option, mut credentials) = (libc::SOL_LOCAL, libc::LOCAL_PEERPID, 0 as libc::pid_t);
    #[cfg(target_os = "linux")]
    let (level, option, mut credentials) = (libc::SOL_SOCKET, libc::SO_PEERCRED, libc::ucred { pid: 0, uid: 0, gid: 0 });
    let mut len = std::mem::size_of_val(&credentials) as libc::socklen_t;
    // SAFETY: the platform option matches the live output object's layout.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            level,
            option,
            std::ptr::addr_of_mut!(credentials).cast(),
            &mut len,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if len as usize != std::mem::size_of_val(&credentials) {
        return Err(SocketError::Protocol("unexpected peer credential size"));
    }
    #[cfg(target_os = "macos")]
    let pid = credentials;
    #[cfg(target_os = "linux")]
    let pid = credentials.pid;
    u32::try_from(pid)
        .ok()
        .filter(|pid| *pid != 0)
        .ok_or(SocketError::Protocol("peer has no process identity"))
}

/// The admitted recipient: the kernel-reported peer PID on Unix, or on Windows
/// the peer's process handle, opened while its connection held the pipe.
#[cfg(unix)]
type Peer = u32;
#[cfg(windows)]
type Peer = Arc<OwnedFd>;

#[cfg(unix)]
fn peer(stream: &Stream) -> Result<Peer, SocketError> {
    peer_pid(stream)
}

#[cfg(windows)]
fn peer(stream: &Stream) -> Result<Peer, SocketError> {
    Ok(stream.peer_process()?)
}

struct ServerSession {
    producer: Arc<Mutex<ArenaProducer>>,
    incarnation: Option<IncarnationId>,
}

impl ServerSession {
    fn reply(&mut self, request: Request, peer: &Peer) -> Result<(Response, Vec<OwnedFd>), SocketError> {
        let mut producer = self.producer.lock().map_err(|_| SocketError::Protocol("producer mutex poisoned"))?;
        match request {
            Request::Attach { holding } => {
                if self.incarnation.is_some() {
                    return Err(SocketError::Protocol("connection already attached"));
                }
                #[cfg(unix)]
                let grant = producer.attach_process(holding, *peer)?;
                #[cfg(windows)]
                let grant = producer.attach_process_handle(holding, peer.as_handle())?;
                self.incarnation = Some(grant.incarnation());
                let (descriptor, fds) = grant.into_parts()?;
                Ok((Response::Attached { descriptor }, fds.into()))
            }
            Request::Configuration => {
                let incarnation = self.incarnation.ok_or(SocketError::Protocol("configuration requires attachment"))?;
                match producer.configuration_offer(incarnation)? {
                    Some(grant) => {
                        let (descriptor, fd) = grant.into_parts()?;
                        Ok((Response::Configuration { descriptor }, vec![fd]))
                    }
                    None => Ok((Response::Empty, vec![])),
                }
            }
        }
    }
}

impl Drop for ServerSession {
    fn drop(&mut self) {
        if let Some(incarnation) = self.incarnation
            && let Ok(mut producer) = self.producer.lock()
        {
            // EOF is closure, not proof that this process or its mappings died.
            let _ = producer.close(incarnation);
        }
    }
}

/// Serve setup for one host-selected CPU producer until connection EOF/error.
/// Call only after the host's routing/authorization handshake. The host continues
/// polling producer cleanup while idle and retains its teardown owner afterwards.
/// Set stream timeouts beforehand if the host requires bounded setup operations.
///
/// Admission binds to the peer the kernel reports for this connection: its PID
/// on Unix, or on Windows its process handle (kept from
/// [`crate::local::Listener::accept`], else opened from the pipe's client PID
/// while the pipe is connected). Grants are duplicated into that same process.
pub fn serve_cpu(mut stream: Stream, producer: Arc<Mutex<ArenaProducer>>) -> Result<(), SocketError> {
    // macOS accepted sockets can inherit O_NONBLOCK from the listener. This
    // synchronous setup loop must not turn an idle read into peer closure.
    stream.set_nonblocking(false)?;
    let peer = peer(&stream)?;
    let mut session = ServerSession {
        producer,
        incarnation: None,
    };
    while let Some(request) = read_message(&mut stream)? {
        let (reply, fds) = match session.reply(request, &peer) {
            Ok(reply) => reply,
            Err(error) => (
                Response::Rejected {
                    message: error.to_string(),
                },
                vec![],
            ),
        };
        write_message(&mut stream, &reply)?;
        if !fds.is_empty() {
            send_objects(&mut stream, fds)?;
        }
    }
    Ok(())
}

// No network I/O holds the producer mutex, including when the recipient stops
// reading. A fast recipient must not acknowledge mapping retirement while
// sender copies still retain that storage, so it imports only after they drop.
#[cfg(unix)]
fn send_objects(stream: &mut Stream, fds: Vec<OwnedFd>) -> Result<(), SocketError> {
    crate::fdpass::send_fds(stream, &fds.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>())?;
    // The recipient waits for this marker, sent after these copies drop.
    drop(fds);
    stream.write_all(&[TRANSFER_FINISHED])?;
    Ok(())
}

/// Windows: duplicate into the verified peer with the least access its import
/// needs (docs/design/acquisition-process-cleanup.md). Duplication closes these
/// copies before the peer learns any value, and the peer acknowledges receipt.
#[cfg(windows)]
fn send_objects(stream: &mut Stream, handles: Vec<OwnedFd>) -> Result<(), SocketError> {
    use windows_sys::Win32::{
        Storage::FileSystem::SYNCHRONIZE,
        System::{
            Memory::{FILE_MAP_READ, FILE_MAP_WRITE},
            Threading::EVENT_MODIFY_STATE,
        },
    };

    use crate::local::Access::Rights;
    // The order is fixed by ConsumerGrant::into_parts (see GrantDescriptor):
    // control, resources, claims, consumer event, producer event. A
    // configuration offer (ConfigurationGrant::into_parts) carries one resource
    // section. Keep this list in step with those; import maps each object with
    // exactly this access (docs/design/acquisition-process-cleanup.md).
    let access: &[u32] = match handles.len() {
        5 => &[
            FILE_MAP_READ,
            FILE_MAP_READ,
            FILE_MAP_READ | FILE_MAP_WRITE,
            SYNCHRONIZE | EVENT_MODIFY_STATE,
            EVENT_MODIFY_STATE,
        ],
        1 => &[FILE_MAP_READ],
        _ => return Err(SocketError::Protocol("unexpected setup handle count")),
    };
    let objects = handles
        .into_iter()
        .zip(access)
        .map(|(handle, access)| (handle, Rights(*access)))
        .collect();
    crate::local::send_handles(stream, objects)?;
    Ok(())
}

#[cfg(unix)]
fn receive_objects(stream: &mut Stream, count: usize) -> Result<Vec<OwnedFd>, SocketError> {
    let fds = crate::fdpass::recv_fds(stream, count)?;
    if fds.len() != count {
        return Err(SocketError::Protocol("incorrect setup FD count"));
    }
    for fd in &fds {
        // SAFETY: fd is owned and live. Setup descriptors must not
        // escape through a later exec of a consumer subprocess.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    let mut marker = [0];
    stream.read_exact(&mut marker)?;
    if marker != [TRANSFER_FINISHED] {
        return Err(SocketError::Protocol("sender did not finish transfer"));
    }
    Ok(fds)
}

/// Windows: the server duplicated these non-inheritable handles into this
/// process and closed its own copies before sending their values.
#[cfg(windows)]
fn receive_objects(stream: &mut Stream, count: usize) -> Result<Vec<OwnedFd>, SocketError> {
    Ok(crate::local::receive_handles(stream, count)?)
}

#[derive(Debug)]
pub struct CpuSetupClient {
    stream: Stream,
    identity: Option<(IncarnationId, [u8; 16])>,
    failed: bool,
}

impl CpuSetupClient {
    /// Use a connection whose host routing/authorization handshake has finished.
    ///
    /// # Safety
    /// The peer must be the conforming sole producer of the selected arena.
    /// This process must be the socket's original peer and the sole recipient
    /// of its grants. Do not fork, forward or replay grants/mappings, or retain
    /// independent transport FD copies. The library's private shutdown_handle
    /// duplicate is an exception: it only interrupts I/O, never reads/writes or
    /// transfers grants. Only this object may use the byte stream.
    /// The stream must use blocking I/O; read/write timeouts may be set by the host.
    /// On Windows the peer must be a server verified by [`crate::local::connect`]
    /// (or an equally trusted pipe peer): transferred handle values are adopted.
    pub unsafe fn from_stream(stream: Stream) -> Self {
        Self {
            stream,
            identity: None,
            failed: false,
        }
    }

    // The C connection keeps this private handle solely to interrupt I/O.
    // It never reads, writes, exports, or receives grants.
    pub(crate) fn shutdown_handle(&self) -> std::io::Result<crate::local::ShutdownHandle> {
        crate::local::shutdown_handle(&self.stream)
    }

    /// Whether the producer still holds its end of the setup connection. Never
    /// consumes setup bytes. A failed or shut-down connection is not alive.
    /// Producer exit or closure ends the connection; the arena separately
    /// reports acquisition closure.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        !self.failed && crate::local::is_alive(&self.stream)
    }

    fn request(&mut self, request: Request) -> Result<(Response, Vec<OwnedFd>), SocketError> {
        if self.failed {
            return Err(SocketError::Protocol("connection failed"));
        }
        let result = (|| -> Result<_, SocketError> {
            write_message(&mut self.stream, &request)?;
            let reply: Response = read_message(&mut self.stream)?.ok_or(SocketError::Protocol("missing reply"))?;
            let count = match &reply {
                Response::Attached { .. } => 5,
                Response::Configuration { .. } => 1,
                Response::Empty | Response::Rejected { .. } => 0,
            };
            let fds = if count == 0 {
                vec![]
            } else {
                receive_objects(&mut self.stream, count)?
            };
            Ok((reply, fds))
        })();
        if result.is_err() {
            self.fail();
        }
        match result? {
            (Response::Rejected { message }, _) => Err(SocketError::Rejected(message)),
            reply => Ok(reply),
        }
    }

    fn fail(&mut self) {
        self.failed = true;
        #[cfg(unix)]
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        #[cfg(windows)]
        self.stream.shutdown();
    }

    pub fn attach(&mut self, holding: u32) -> Result<ArenaConsumer, SocketError> {
        if self.identity.is_some() {
            return Err(SocketError::Protocol("connection already attached"));
        }
        let (reply, fds) = self.request(Request::Attach { holding })?;
        let result = (|| {
            let Response::Attached { descriptor } = reply else {
                return Err(SocketError::Protocol("unexpected attach reply"));
            };
            if descriptor.payload_capacity == 0 {
                return Err(SocketError::Protocol("CPU setup cannot import native resources"));
            }
            let fds = fds
                .try_into()
                .map_err(|_| SocketError::Protocol("initial setup requires five FDs"))?;
            // SAFETY: from_stream's sole-producer/process contract applies, and
            // the transfer protocol proves the sender relinquished its copies.
            let grant = unsafe { ConsumerGrant::from_parts(descriptor, fds) }?;
            Ok(ArenaConsumer::from_grant(grant)?)
        })();
        match result {
            Ok(consumer) => {
                self.identity = Some((consumer.incarnation(), consumer.claim_scope()));
                Ok(consumer)
            }
            Err(error) => {
                self.fail();
                Err(error)
            }
        }
    }

    pub fn install_configuration(&mut self, consumer: &mut ArenaConsumer) -> Result<Option<ConfigurationInstall>, SocketError> {
        if self.identity != Some((consumer.incarnation(), consumer.claim_scope())) {
            return Err(SocketError::Protocol("consumer belongs to another connection"));
        }
        if consumer.is_configured() {
            return Ok(None);
        }
        let (reply, fds) = self.request(Request::Configuration)?;
        let result = (|| {
            match reply {
                Response::Empty => Ok(None),
                Response::Configuration { descriptor } => {
                    if descriptor.payload_capacity == 0 || fds.len() != 1 {
                        return Err(SocketError::Protocol("invalid CPU configuration resources"));
                    }
                    let fd = fds.into_iter().next().expect("one FD");
                    // SAFETY: same sole producer, process and incarnation as
                    // attachment. Sender copies dropped before the transfer finished.
                    let grant = unsafe { ConfigurationGrant::from_parts(consumer, descriptor, fd) }?;
                    Ok(Some(consumer.install_configuration(grant)?))
                }
                _ => Err(SocketError::Protocol("unexpected configuration reply")),
            }
        })();
        if result.is_err() {
            self.fail();
        }
        result
    }
}

impl Drop for CpuSetupClient {
    fn drop(&mut self) {
        self.fail();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::acquisition::arena::ArenaConfig;

    #[cfg(unix)]
    fn pair() -> (Stream, Stream) {
        Stream::pair().unwrap()
    }

    #[cfg(windows)]
    fn pair() -> (Stream, Stream) {
        crate::local::pipe_pair().unwrap()
    }

    fn test_producer() -> ArenaProducer {
        ArenaProducer::new(ArenaConfig {
            resource_capacity: 6,
            retained_history: 2,
            producer_reserve: 1,
            payload_capacity: 4,
            memory_budget: 1024 * 1024,
            max_incarnations: 1,
            drain_timeout: Duration::from_secs(5),
        })
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn recipient_requires_transfer_completion_before_importing_fds() {
        use std::net::Shutdown;
        let (mut server, stream) = pair();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let task = std::thread::spawn(move || {
            let mut producer = test_producer();
            assert!(matches!(
                read_message::<Request>(&mut server).unwrap(),
                Some(Request::Attach { holding: 1 })
            ));
            let grant = producer.attach_process(1, peer_pid(&server).unwrap()).unwrap();
            let (descriptor, fds) = grant.into_parts().unwrap();
            write_message(&mut server, &Response::Attached { descriptor }).unwrap();
            crate::fdpass::send_fds(&server, &fds.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>()).unwrap();
            // An interrupted transfer has sent the FDs but not proved sender
            // disposal. It must not produce a consumer that can acknowledge
            // mapping retirement. EOF makes this deterministic, without sleeps.
            server.shutdown(Shutdown::Write).unwrap();
            let mut byte = [0];
            assert_eq!(server.read(&mut byte).unwrap(), 0);
            drop(fds);
        });
        // SAFETY: the sole producer is the test arena above. The fault is a
        // truncated transport, not a violation of its shared-memory protocol.
        let mut client = unsafe { CpuSetupClient::from_stream(stream) };
        assert!(matches!(client.attach(1), Err(SocketError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof));
        drop(client);
        task.join().unwrap();
    }

    /// Windows: a reply whose handle transfer never completes fails attachment
    /// instead of producing a consumer.
    #[cfg(windows)]
    #[test]
    fn recipient_fails_when_the_handle_transfer_is_truncated() {
        let (mut server, stream) = pair();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let task = std::thread::spawn(move || {
            let mut producer = test_producer();
            assert!(matches!(
                read_message::<Request>(&mut server).unwrap(),
                Some(Request::Attach { holding: 1 })
            ));
            let grant = producer.attach_process(1, std::process::id()).unwrap();
            let (descriptor, handles) = grant.into_parts().unwrap();
            write_message(&mut server, &Response::Attached { descriptor }).unwrap();
            // Only the start of the handle message, then closure.
            server.write_all(&5_u32.to_le_bytes()).unwrap();
            drop(server);
            drop(handles);
        });
        // SAFETY: the sole producer is the test arena above.
        let mut client = unsafe { CpuSetupClient::from_stream(stream) };
        assert!(matches!(client.attach(1), Err(SocketError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof));
        assert!(!client.is_alive());
        task.join().unwrap();
    }

    #[test]
    fn liveness_follows_the_producer_end_without_consuming_setup_bytes() {
        let (server, stream) = pair();
        // SAFETY: no grants are exchanged; only liveness is observed.
        let client = unsafe { CpuSetupClient::from_stream(stream) };
        assert!(client.is_alive());
        drop(server);
        assert!(!client.is_alive());
    }

    #[test]
    fn setup_rejects_oversized_messages_before_reading_the_body() {
        let (mut sender, mut receiver) = pair();
        receiver.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        sender.write_all(MAGIC).unwrap();
        sender.write_all(&((MAX_MESSAGE + 1) as u32).to_le_bytes()).unwrap();
        assert!(matches!(
            read_message::<Request>(&mut receiver),
            Err(SocketError::Protocol("message size exceeds limit"))
        ));
    }
}
