//! CPU arena setup on an already selected/authorized Unix connection.
//! The host owns routing and authority. This protocol only admits one process
//! incarnation and transfers its initial/replacement maps; frames never use it.

use std::{
    io::{Read, Write},
    net::Shutdown,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixStream,
    },
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

const MAGIC: &[u8; 8] = b"JSCPU001";
const MAX_MESSAGE: usize = 16 * 1024;
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

fn read_message<T: DeserializeOwned>(stream: &mut UnixStream) -> Result<Option<T>, SocketError> {
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

fn write_message<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<(), SocketError> {
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
pub fn peer_pid(stream: &UnixStream) -> Result<u32, SocketError> {
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

struct ServerSession {
    producer: Arc<Mutex<ArenaProducer>>,
    incarnation: Option<IncarnationId>,
}

impl ServerSession {
    fn reply(&mut self, request: Request, pid: u32) -> Result<(Response, Vec<OwnedFd>), SocketError> {
        let mut producer = self.producer.lock().map_err(|_| SocketError::Protocol("producer mutex poisoned"))?;
        match request {
            Request::Attach { holding } => {
                if self.incarnation.is_some() {
                    return Err(SocketError::Protocol("connection already attached"));
                }
                let grant = producer.attach_process(holding, pid)?;
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
/// Set socket timeouts beforehand if the host requires bounded setup operations.
pub fn serve_cpu(mut stream: UnixStream, producer: Arc<Mutex<ArenaProducer>>) -> Result<(), SocketError> {
    // macOS accepted sockets can inherit O_NONBLOCK from the listener. This
    // synchronous setup loop must not turn an idle read into peer closure.
    stream.set_nonblocking(false)?;
    let pid = peer_pid(&stream)?;
    let mut session = ServerSession {
        producer,
        incarnation: None,
    };
    while let Some(request) = read_message(&mut stream)? {
        let (reply, fds) = match session.reply(request, pid) {
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
            crate::fdpass::send_fds(&stream, &fds.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>())?;
            // A fast recipient must not acknowledge mapping retirement while
            // sender FD copies still retain that storage. It imports only after
            // this marker, sent after those copies drop. No network I/O holds
            // the producer mutex, including when the recipient stops reading.
            drop(fds);
            stream.write_all(&[TRANSFER_FINISHED])?;
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct CpuSetupClient {
    stream: UnixStream,
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
    /// independent transport FD copies. Only this object may use the byte stream.
    /// The stream must use blocking I/O; read/write timeouts may be set by the host.
    pub unsafe fn from_stream(stream: UnixStream) -> Self {
        Self {
            stream,
            identity: None,
            failed: false,
        }
    }

    fn request(&mut self, request: Request) -> Result<(Response, Vec<OwnedFd>), SocketError> {
        if self.failed {
            return Err(SocketError::Protocol("connection failed"));
        }
        let result = (|| {
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
                crate::fdpass::recv_fds(&self.stream, count)?
            };
            if fds.len() != count {
                return Err(SocketError::Protocol("incorrect setup FD count"));
            }
            if count != 0 {
                for fd in &fds {
                    // SAFETY: fd is owned and live. Setup descriptors must not
                    // escape through a later exec of a consumer subprocess.
                    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                        return Err(std::io::Error::last_os_error().into());
                    }
                }
                let mut marker = [0];
                self.stream.read_exact(&mut marker)?;
                if marker != [TRANSFER_FINISHED] {
                    return Err(SocketError::Protocol("sender did not finish transfer"));
                }
            }
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
        let _ = self.stream.shutdown(Shutdown::Both);
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
            // the transfer marker proves the sender relinquished extra FD copies.
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
                    // attachment. Sender copies dropped before the marker.
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

    #[test]
    fn recipient_requires_transfer_completion_before_importing_fds() {
        let (mut server, stream) = UnixStream::pair().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let task = std::thread::spawn(move || {
            let mut producer = ArenaProducer::new(ArenaConfig {
                resource_capacity: 6,
                retained_history: 2,
                producer_reserve: 1,
                payload_capacity: 4,
                memory_budget: 1024 * 1024,
                max_incarnations: 1,
                drain_timeout: Duration::from_secs(5),
            })
            .unwrap();
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

    #[test]
    fn setup_rejects_oversized_messages_before_reading_the_body() {
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        receiver.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        sender.write_all(MAGIC).unwrap();
        sender.write_all(&((MAX_MESSAGE + 1) as u32).to_le_bytes()).unwrap();
        assert!(matches!(
            read_message::<Request>(&mut receiver),
            Err(SocketError::Protocol("message size exceeds limit"))
        ));
    }
}
