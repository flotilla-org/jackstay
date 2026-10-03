//! One host-authorized source connection, separate media and input transports.
//!
//! The original stream remains the media setup stream: moving CPU setup to a
//! socketpair would lose the original peer's kernel identity. Only the optional
//! input channel is transferred: a socketpair end passed with SCM_RIGHTS on
//! Unix, or a private pipe-pair end duplicated into the verified peer process
//! on Windows. The host selects and authorizes both resources; this module
//! creates no listener, discovers no source and grants no authority.
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::{io, time::Duration};

use crate::{
    input::{
        self, Mode, Target,
        transport::{Client, ConnectError, Server},
    },
    local::{Bounded, Stream},
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
    pub media: Stream,
    /// Retain this independently of media; dropping schedules target cleanup.
    pub input: Option<Server>,
}

pub struct Connected {
    /// Continue with CpuSetupClient on this original, blocking connection.
    pub media: Stream,
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
pub fn accept(stream: Stream, target: Option<Target>) -> Result<Accepted, Error> {
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
                let (host, peer) = channel_pair()?;
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
        handshake.send_input(peer)?;
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
pub fn connect(stream: Stream, request: InputRequest) -> Result<Connected, Error> {
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
            let stream = handshake.receive_input()?;
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

#[cfg(unix)]
fn channel_pair() -> io::Result<(Stream, Stream)> {
    Stream::pair()
}

#[cfg(windows)]
fn channel_pair() -> io::Result<(Stream, Stream)> {
    crate::local::pipe_pair()
}

// A fixed-size preface with an absolute deadline, not a per-byte timeout that a
// stalled or trickling peer can extend (crate::local::Bounded). No buffered
// reader may eat media bytes or the ancillary-data byte belonging to the input
// channel.
struct Handshake {
    exchange: Bounded<Stream>,
}

impl Handshake {
    fn new(stream: Stream) -> io::Result<Self> {
        #[cfg(unix)]
        crate::socket_options::suppress_sigpipe(&stream)?;
        Ok(Self {
            exchange: Bounded::new(stream, TIMEOUT)?,
        })
    }
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<()> {
        self.exchange.read_exact(bytes)
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.exchange.write_all(bytes)
    }
    /// The media stream, blocking again with its previous timeouts.
    fn finish(self) -> io::Result<Stream> {
        self.exchange.finish()
    }
}

#[cfg(unix)]
impl Handshake {
    fn send_input(&mut self, peer: Stream) -> Result<(), Error> {
        self.exchange.writable()?;
        crate::fdpass::send_fd(self.exchange.stream(), peer.as_raw_fd())?;
        // Retain the sending copy until the peer has installed the descriptor.
        // Parallel macOS bootstrap tests otherwise intermittently see input EOF.
        let mut receipt = [0];
        self.read(&mut receipt)?;
        if receipt != [1] {
            return Err(Error::Protocol("input descriptor not received"));
        }
        // This copy must not keep the controller channel alive after peer loss.
        drop(peer);
        Ok(())
    }
    fn receive_input(&mut self) -> Result<Stream, Error> {
        self.exchange.readable()?;
        let fd = crate::fdpass::recv_fd(self.exchange.stream())?;
        // SAFETY: this live received FD must not leak through exec.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let stream = Stream::from(fd);
        if stream.peer_addr().is_err() {
            return Err(Error::Protocol("input descriptor is not a connected Unix socket"));
        }
        self.write(&[1])?;
        Ok(stream)
    }
}

// Handle transfer is one write and one acknowledgement, both bounded by the
// time left: ready() sets the pipe's timeouts to it.
#[cfg(windows)]
impl Handshake {
    fn send_input(&mut self, peer: Stream) -> Result<(), Error> {
        self.exchange.writable()?;
        // Duplication closes this copy before the peer learns the value, so it
        // cannot keep the controller channel alive after peer loss; the peer
        // acknowledges adopting it within the same deadline.
        crate::local::send_handles(self.exchange.stream_mut(), vec![(peer.into_handle()?, crate::local::Access::Same)])?;
        Ok(())
    }
    fn receive_input(&mut self) -> Result<Stream, Error> {
        self.exchange.readable()?;
        let handle = crate::local::receive_handles(self.exchange.stream_mut(), 1)?.remove(0);
        // SAFETY: the host this stream connected to duplicated its private pipe
        // end into this process for this receiver alone.
        let stream = unsafe { crate::local::PipeStream::from_owned_handle(handle, false) }
            .map_err(|_| Error::Protocol("input handle is not a connected pipe"))?;
        if !stream.is_alive() {
            return Err(Error::Protocol("input handle is not a connected pipe"));
        }
        Ok(stream)
    }
}

/// Channel identities, independent of semantic domains.
pub const INPUT: u32 = 1;
pub const AFFORDANCES: u32 = 2;
const MAGIC_V2: &[u8; 8] = b"JSBOOT02";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelRequest {
    None,
    Optional,
    Required,
}
impl ChannelRequest {
    fn code(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Optional => 1,
            Self::Required => 2,
        }
    }
}
pub struct AcceptedV2 {
    pub media: Stream,
    pub input: Option<Server>,
    pub affordances: Option<crate::affordances::Producer>,
}
pub struct ConnectedV2 {
    pub media: Stream,
    pub input: Option<Client>,
    pub input_error: Option<input::Error>,
    pub affordances: Option<crate::affordances::Host>,
    pub affordances_refused: bool,
}
/// Explicit v2; never retry or downgrade a consumed stream. The 24-byte
/// request is magic[8], input mode, affordances request (0/1/2), affordances
/// version, reserved zero. The 48-byte reply is magic[8], channel mask, input
/// version, affordances version, reserved zero, then two 12-byte NUL-padded
/// UTF-8 channel names (`input`, `affordances`). Integers are u32 BE. Descriptors follow
/// in INPUT then AFFORDANCES order, with receipt within the five-second bound.
pub fn accept_v2(stream: Stream, target: Option<Target>, affordances: bool) -> Result<AcceptedV2, Error> {
    let mut h = Handshake::new(stream)?;
    let mut r = [0u8; 24];
    h.read(&mut r)?;
    let word = |i| u32::from_be_bytes(r[i..i + 4].try_into().unwrap());
    if &r[..8] != MAGIC_V2 || !matches!(word(8), 0 | 1 | 2 | 4) || word(12) > 2 || word(20) != 0 {
        return Err(Error::Protocol("invalid v2 request"));
    }
    let input = if word(8) != 0 {
        target
            .map(|t| {
                let (a, b) = channel_pair()?;
                Ok::<_, io::Error>((Server::start(t, a)?, b))
            })
            .transpose()?
    } else {
        None
    };
    let aff = if word(12) != 0 && word(16) == 1 && affordances {
        let (a, b) = channel_pair()?;
        Some((crate::affordances::Producer::start(a)?, b))
    } else {
        None
    };
    let mask = (if input.is_some() { INPUT } else { 0 }) | (if aff.is_some() { AFFORDANCES } else { 0 });
    let mut reply = [0u8; 48];
    reply[..8].copy_from_slice(MAGIC_V2);
    reply[8..12].copy_from_slice(&mask.to_be_bytes());
    reply[12..16].copy_from_slice(&u32::from(input.is_some()).to_be_bytes());
    reply[16..20].copy_from_slice(&u32::from(aff.is_some()).to_be_bytes());
    reply[24..36].copy_from_slice(b"input\0\0\0\0\0\0\0");
    reply[36..48].copy_from_slice(b"affordances\0");
    h.write(&reply)?;
    let input = if let Some((s, p)) = input {
        h.send_input(p)?;
        Some(s)
    } else {
        None
    };
    let affordances = if let Some((s, p)) = aff {
        h.send_input(p)?;
        Some(s)
    } else {
        None
    };
    Ok(AcceptedV2 {
        media: h.finish()?,
        input,
        affordances,
    })
}
/// Optional clean refusals preserve media and other channels; malformed offers,
/// uncertain transfers and required refusals close all newly owned channels.
pub fn connect_v2(stream: Stream, input_request: InputRequest, request: ChannelRequest) -> Result<ConnectedV2, Error> {
    let mut h = Handshake::new(stream)?;
    let mode = match input_request {
        InputRequest::None => None,
        InputRequest::Optional(m) | InputRequest::Required(m) => Some(m),
    };
    let mut r = [0u8; 24];
    r[..8].copy_from_slice(MAGIC_V2);
    r[8..12].copy_from_slice(&mode.map_or(0, Mode::bit).to_be_bytes());
    r[12..16].copy_from_slice(&request.code().to_be_bytes());
    r[16..20].copy_from_slice(&1u32.to_be_bytes());
    h.write(&r)?;
    let mut r = [0u8; 48];
    h.read(&mut r)?;
    let word = |i| u32::from_be_bytes(r[i..i + 4].try_into().unwrap());
    let mask = word(8);
    if &r[..8] != MAGIC_V2
        || &r[24..36] != b"input\0\0\0\0\0\0\0"
        || &r[36..48] != b"affordances\0"
        || mask & !(INPUT | AFFORDANCES) != 0
        || word(12) != u32::from(mask & INPUT != 0)
        || word(16) != u32::from(mask & AFFORDANCES != 0)
        || word(20) != 0
        || (mode.is_none() && mask & INPUT != 0)
        || (request == ChannelRequest::None && mask & AFFORDANCES != 0)
    {
        return Err(Error::Protocol("invalid v2 offer"));
    }
    let input_stream = if mask & INPUT != 0 { Some(h.receive_input()?) } else { None };
    let affordances = if mask & AFFORDANCES != 0 {
        Some(crate::affordances::Host::start(h.receive_input()?)?)
    } else {
        None
    };
    let media = h.finish()?;
    let mut input = None;
    let mut input_error = None;
    if let Some(mode) = mode {
        if let Some(stream) = input_stream {
            match Client::connect(stream, mode) {
                Ok(c) => input = Some(c),
                Err(ConnectError::Admission(e)) => input_error = Some(e),
                Err(ConnectError::Transport(e)) => return Err(e.into()),
            }
        } else {
            input_error = Some(input::Error::Unsupported)
        }
    }
    if matches!(input_request, InputRequest::Required(_)) {
        if let Some(e) = input_error {
            return Err(Error::Input(e));
        }
    }
    let affordances_refused = request != ChannelRequest::None && affordances.is_none();
    if affordances_refused && request == ChannelRequest::Required {
        return Err(Error::Protocol("required affordances refused"));
    }
    Ok(ConnectedV2 {
        media,
        input,
        input_error,
        affordances,
        affordances_refused,
    })
}
