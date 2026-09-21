//! One host-authorized source connection, separate media and input transports.
//!
//! The original stream remains the media setup stream: moving CPU setup to a
//! socketpair would lose the original peer's kernel identity. Only the optional
//! input channel is transferred. The host selects and authorizes both resources;
//! this module creates no listener, discovers no source and grants no authority.
use std::{
    io::{self, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    thread,
    time::{Duration, Instant},
};

use crate::input::{
    self, Mode, Target,
    transport::{Client, ConnectError, Server},
};

const MAGIC: &[u8; 8] = b"JSBOOT01";
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
pub enum InputRequest {
    None,
    Optional(Mode),
    Required(Mode),
}

pub struct Accepted {
    /// Continue with serve_cpu on this original, blocking connection.
    pub media: UnixStream,
    /// Retain this independently of media; dropping schedules target cleanup.
    pub input: Option<Server>,
}

pub struct Connected {
    /// Continue with CpuSetupClient on this original, blocking connection.
    pub media: UnixStream,
    pub input: Option<Client>,
    /// Clean admission rejection for Optional input; None for an observer.
    pub input_error: Option<input::Error>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("source bootstrap I/O: {0}")]
    Io(#[from] io::Error),
    #[error("source bootstrap descriptor transfer: {0}")]
    Transfer(#[from] crate::CaptureTransferError),
    #[error("invalid source bootstrap: {0}")]
    Protocol(&'static str),
    #[error("input admission: {0:?}")]
    Input(input::Error),
}

/// Negotiate one already selected source, on a worker, before media setup.
/// Supplying a target authorizes input on that target for this peer. None means
/// observation only. No input target is admitted for an observer request.
/// On any failure all owned channels close; keep pumping target cleanup.
/// The fixed-size bootstrap is bounded to five seconds. The returned media
/// stream is blocking; existing read/write timeouts are left unchanged.
pub fn accept(stream: UnixStream, target: Option<Target>) -> Result<Accepted, Error> {
    let mut handshake = Handshake::new(stream)?;
    let mut request = [0; 12];
    handshake.read(&mut request)?;
    if &request[..8] != MAGIC {
        return Err(Error::Protocol("unsupported version"));
    }
    let mode = u32::from_be_bytes(request[8..].try_into().unwrap());
    if !matches!(mode, 0 | 1 | 2 | 4) {
        return Err(Error::Protocol("invalid input mode"));
    }
    let input = if mode != 0 {
        target
            .map(|target| {
                let (host, peer) = UnixStream::pair()?;
                Ok::<_, io::Error>((Server::start(target, host)?, peer))
            })
            .transpose()?
    } else {
        None
    };
    let mut reply = [0; 12];
    reply[..8].copy_from_slice(MAGIC);
    reply[8..].copy_from_slice(&u32::from(input.is_some()).to_be_bytes());
    handshake.write(&reply)?;
    let server = if let Some((server, peer)) = input {
        handshake.ready(libc::POLLOUT)?;
        crate::fdpass::send_fd(&handshake.stream, peer.as_raw_fd())?;
        // Retain the sending copy until the peer has installed the descriptor.
        // Parallel macOS bootstrap tests otherwise intermittently see input EOF.
        let mut receipt = [0];
        handshake.read(&mut receipt)?;
        if receipt != [1] {
            return Err(Error::Protocol("input descriptor not received"));
        }
        // This copy must not keep the controller channel alive after peer loss.
        drop(peer);
        Some(server)
    } else {
        None
    };
    Ok(Accepted {
        media: handshake.finish()?,
        input: server,
    })
}

/// Negotiate the selected source before handing its stream to media setup.
/// Call off the GUI/input thread: bootstrap takes at most five seconds, followed
/// by the existing input admission's five-second bound when requested.
/// Optional input preserves media on a clean admission rejection, exposing its
/// reason. Protocol/transport errors and Required rejection fail the whole setup.
/// The caller separately attaches media. A later media failure does not close
/// input automatically; close/poll it if the host abandons the association.
pub fn connect(stream: UnixStream, request: InputRequest) -> Result<Connected, Error> {
    let mut handshake = Handshake::new(stream)?;
    let mode = match request {
        InputRequest::None => None,
        InputRequest::Optional(mode) | InputRequest::Required(mode) => Some(mode),
    };
    let mut message = [0; 12];
    message[..8].copy_from_slice(MAGIC);
    message[8..].copy_from_slice(&mode.map_or(0, Mode::bit).to_be_bytes());
    handshake.write(&message)?;
    handshake.read(&mut message)?;
    if &message[..8] != MAGIC {
        return Err(Error::Protocol("unsupported version"));
    }
    let available = u32::from_be_bytes(message[8..].try_into().unwrap());
    let mut input = None;
    let mut input_error = None;
    match (mode, available) {
        (None, 0) => {}
        (Some(_), 0) => input_error = Some(input::Error::Unsupported),
        (Some(mode), 1) => {
            handshake.ready(libc::POLLIN)?;
            let fd = crate::fdpass::recv_fd(&handshake.stream)?;
            // SAFETY: this live received FD must not leak through exec.
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error().into());
            }
            let stream = UnixStream::from(fd);
            if stream.peer_addr().is_err() {
                return Err(Error::Protocol("input descriptor is not a connected Unix socket"));
            }
            handshake.write(&[1])?;
            match Client::connect(stream, mode) {
                Ok(client) => input = Some(client),
                Err(ConnectError::Admission(error)) => input_error = Some(error),
                Err(ConnectError::Transport(error)) => return Err(error.into()),
            }
        }
        _ => return Err(Error::Protocol("unexpected input offer")),
    }
    if let Some(error) = input_error {
        if matches!(request, InputRequest::Required(_)) {
            return Err(Error::Input(error));
        }
    }
    Ok(Connected {
        media: handshake.finish()?,
        input,
        input_error,
    })
}

// A fixed-size preface with an absolute deadline, not a per-byte timeout that a
// stalled or trickling peer can extend. No buffered reader may eat media bytes
// or the ancillary-data byte belonging to the input channel.
struct Handshake {
    stream: UnixStream,
    deadline: Instant,
}
impl Handshake {
    fn new(stream: UnixStream) -> io::Result<Self> {
        crate::socket_options::suppress_sigpipe(&stream)?;
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            deadline: Instant::now() + TIMEOUT,
        })
    }
    fn ready(&self, events: i16) -> io::Result<()> {
        loop {
            let remaining = self
                .deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
            let mut fd = libc::pollfd {
                fd: self.stream.as_raw_fd(),
                events,
                revents: 0,
            };
            // SAFETY: one initialized pollfd; the stream owns its live descriptor.
            let result = unsafe { libc::poll(&mut fd, 1, remaining.as_millis().max(1) as i32) };
            if result > 0 {
                return Ok(());
            }
            if result == 0 {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    fn read(&mut self, mut bytes: &mut [u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            self.ready(libc::POLLIN)?;
            match self.stream.read(bytes) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(count) => bytes = &mut bytes[count..],
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => thread::yield_now(),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            self.ready(libc::POLLOUT)?;
            match self.stream.write(bytes) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => bytes = &bytes[count..],
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => thread::yield_now(),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    fn finish(self) -> io::Result<UnixStream> {
        self.stream.set_nonblocking(false)?;
        Ok(self.stream)
    }
}
