//! Handshake v3: track offer, selection and admission on one host-selected
//! connection, before CPU setup continues on the same stream.
//!
//! The source host offers its publication's tracks. The consumer selects a
//! subset, each optional or required, and may request to write a track that
//! flows toward the source. The host replies with what it admitted, in
//! selection order, and then passes one channel per admitted input or
//! affordances track: a socketpair end with SCM_RIGHTS on Unix, or a private
//! pipe end duplicated into the verified peer on Windows. When playback tracks
//! are admitted it then passes their doorbell and read-only rings in one
//! transfer (see [`crate::audio`]). A required refusal closes setup on both
//! sides; optional refusals are reported.
//!
//! An admitted video track is carried by this same connection, which stays
//! the CPU setup stream so `serve_cpu` sees the consumer's kernel identity.
//! This module creates no listener, discovers no source and grants no
//! authority: the host selects the source and supplies its input target.
#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};
#[cfg(windows)]
use std::os::windows::io::OwnedHandle;
use std::{collections::HashSet, io, time::Duration};

pub use flotilla_cbor::Bytes;
use flotilla_cbor::{Framing, HEADER_LEN, Limits, Message};
use serde::{Deserialize, Serialize};

use crate::{
    affordances,
    audio::{self, Playback},
    input::{
        self, Mode, Target,
        transport::{Client, ConnectError, Server},
    },
    local::{Bounded, Stream},
};

/// The frame magic of every setup message on a source connection: this
/// handshake, then CPU setup.
pub const MAGIC: [u8; 8] = *b"JSSETUP3";
/// 64 KiB per message, 4 KiB per opaque byte string such as a descriptor.
pub const LIMITS: Limits = Limits {
    max_message: 64 * 1024,
    max_byte_string: 4 * 1024,
    ..flotilla_cbor::DEFAULT_LIMITS
};
pub(crate) const FRAMING: Framing = Framing::new(MAGIC, LIMITS);
const TIMEOUT: Duration = Duration::from_secs(5);

/// A track kind: a core kind, or an app kind named by reverse domain (for
/// example `org.example.cue`) whose descriptor Jackstay treats as opaque.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", from = "String")]
pub enum Kind {
    Video,
    Input,
    Affordances,
    /// Playback audio, described by an [`audio::Descriptor`].
    Playback,
    /// Any other kind. A consumer skips kinds it does not know.
    Other(String),
}

impl From<String> for Kind {
    fn from(kind: String) -> Self {
        match kind.as_str() {
            "video" => Self::Video,
            "input" => Self::Input,
            "affordances" => Self::Affordances,
            "playback" => Self::Playback,
            _ => Self::Other(kind),
        }
    }
}

impl From<Kind> for String {
    fn from(kind: Kind) -> Self {
        kind.as_str().to_owned()
    }
}

impl Kind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Video => "video",
            Self::Input => "input",
            Self::Affordances => "affordances",
            Self::Playback => "playback",
            Self::Other(kind) => kind,
        }
    }

    /// Whether a host may offer this kind: a core kind, or a reverse-domain
    /// name of at least two dot-separated lowercase labels.
    fn offerable(&self) -> bool {
        let Self::Other(kind) = self else { return true };
        kind.contains('.')
            && kind
                .split('.')
                .all(|label| !label.is_empty() && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
    }
}

/// Which way a track's data flows, seen from the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    FromSource,
    ToSource,
    Both,
}

/// One offered track. IDs are stable and never reused within a publication.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    pub id: u64,
    pub kind: Kind,
    pub direction: Direction,
    pub label: String,
    /// Empty for core kinds other than playback.
    #[serde(default)]
    pub descriptor: Bytes,
}

/// The source's description of its publication's tracks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "offer")]
pub struct Offer {
    /// The source label: the program's identity.
    pub source: String,
    pub tracks: Vec<Track>,
}

impl Message for Offer {
    const TYPES: &'static [&'static str] = &["offer"];
}

/// One selected track. `write` requests writing a track that flows toward
/// the source; selecting input always requests it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selected {
    pub id: u64,
    pub required: bool,
    #[serde(default)]
    pub write: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename = "selection")]
struct Selection {
    tracks: Vec<Selected>,
}

impl Message for Selection {
    const TYPES: &'static [&'static str] = &["selection"];
}

/// Why the host did not admit a selected track.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Refusal {
    /// The offer has no track with this ID.
    Unknown,
    /// The host withholds this track from this consumer, for example input
    /// without an authorized target, or input selected without writing.
    Unavailable,
    /// Writing was requested on a track that flows only from the source.
    NotWritable,
}

/// The host's answer to one selected track, in selection order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admission {
    pub id: u64,
    /// None when admitted.
    pub refusal: Option<Refusal>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename = "admitted")]
struct Admitted {
    tracks: Vec<Admission>,
}

impl Message for Admitted {
    const TYPES: &'static [&'static str] = &["admitted"];
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("handshake I/O: {0}")]
    Io(#[from] io::Error),
    #[error("handshake descriptor transfer: {0}")]
    Transfer(#[from] crate::CaptureTransferError),
    #[error("handshake message: {0}")]
    Message(#[from] flotilla_cbor::Error),
    #[error("invalid handshake: {0}")]
    Protocol(&'static str),
    #[error("required track {id} refused: {refusal:?}")]
    Refused { id: u64, refusal: Refusal },
    #[error("input admission: {0:?}")]
    Input(input::Error),
    #[error("playback: {0}")]
    Audio(#[from] audio::Error),
}

pub struct Accepted {
    /// Continue with serve_cpu on this original, blocking connection when a
    /// video track was admitted.
    pub media: Stream,
    pub admissions: Vec<Admission>,
    /// Retain this independently of media; dropping schedules target cleanup.
    pub input: Option<Server>,
    pub affordances: Option<affordances::Producer>,
    /// Retain for the incarnation's lifetime when playback was admitted.
    pub doorbell: Option<audio::Bell>,
}

/// Run the host side on a worker. Supplying a target authorizes input on that
/// target for this peer; None refuses any input track. Every offered playback
/// track needs its ring in `playback`. One offer track per
/// video, input and affordances kind at most: one incarnation carries one
/// media stream. On any failure, including a required
/// refusal, all owned channels close; keep pumping target cleanup. The whole
/// handshake is bounded to five seconds.
pub fn accept(stream: Stream, offer: &Offer, target: Option<Target>, playback: &[Playback]) -> Result<Accepted, Error> {
    let mut ids = HashSet::new();
    let mut single_kinds = HashSet::new();
    let ring = |id| playback.iter().find(|p| p.id() == id);
    if offer.tracks.iter().filter(|t| t.kind == Kind::Playback).count() > audio::MAX_TRACKS {
        return Err(Error::Protocol("invalid offer"));
    }
    for track in &offer.tracks {
        let single = matches!(track.kind, Kind::Video | Kind::Input | Kind::Affordances);
        let fits = || {
            let descriptor = audio::Descriptor::decode(&track.descriptor);
            ring(track.id).is_some_and(|ring| descriptor.is_ok_and(|descriptor| ring.fits(&descriptor)))
        };
        let unplayable = track.kind == Kind::Playback && (track.direction != Direction::FromSource || !fits());
        if !ids.insert(track.id) || !track.kind.offerable() || unplayable || (single && !single_kinds.insert(track.kind.clone())) {
            return Err(Error::Protocol("invalid offer"));
        }
    }
    let mut handshake = Handshake::new(stream)?;
    handshake.write(offer)?;
    let selection: Selection = handshake.read()?;
    let mut selected = HashSet::new();
    if selection.tracks.len() > offer.tracks.len() || !selection.tracks.iter().all(|s| selected.insert(s.id)) {
        return Err(Error::Protocol("invalid selection"));
    }
    let admissions: Vec<Admission> = selection
        .tracks
        .iter()
        .map(|s| Admission {
            id: s.id,
            refusal: match offer.tracks.iter().find(|t| t.id == s.id) {
                None => Some(Refusal::Unknown),
                Some(t) if s.write && t.direction == Direction::FromSource => Some(Refusal::NotWritable),
                Some(t) if t.kind == Kind::Input && (!s.write || target.is_none()) => Some(Refusal::Unavailable),
                Some(_) => None,
            },
        })
        .collect();
    let refused = selection
        .tracks
        .iter()
        .zip(&admissions)
        .find(|(s, a)| s.required && a.refusal.is_some());
    let mut input = None;
    let mut affordances = None;
    let mut peers = Vec::new();
    if refused.is_none() {
        for kind in channel_kinds(offer, &admissions) {
            let (host, peer) = channel_pair()?;
            if kind == Kind::Input {
                let target = target.clone().ok_or(Error::Protocol("input admitted without a target"))?;
                input = Some(Server::start(target, host)?);
            } else {
                affordances = Some(affordances::Producer::start(host)?);
            }
            peers.push(peer);
        }
    }
    handshake.write(&Admitted {
        tracks: admissions.clone(),
    })?;
    if let Some((_, admission)) = refused {
        return Err(Error::Refused {
            id: admission.id,
            refusal: admission.refusal.expect("refused"),
        });
    }
    for peer in peers {
        handshake.send_channel(peer)?;
    }
    let playing = playback_tracks(offer, &admissions);
    let mut doorbell = None;
    if !playing.is_empty() {
        let (bell, mut objects) = audio::Bell::new()?;
        // Register first, so every block after the transfer rings the bell.
        for (position, id) in &playing {
            let ring = ring(*id).expect("validated offer");
            ring.register(&bell, *position);
            objects.push(ring.read_only_fd()?);
        }
        handshake.send_objects(objects)?;
        doorbell = Some(bell);
    }
    Ok(Accepted {
        media: handshake.finish()?,
        admissions,
        input,
        affordances,
        doorbell,
    })
}

/// Selection positions and IDs of the admitted playback tracks. Both sides
/// derive the ring transfer order from this.
fn playback_tracks(offer: &Offer, admissions: &[Admission]) -> Vec<(usize, u64)> {
    let playback = |id| offer.tracks.iter().any(|t| t.id == id && t.kind == Kind::Playback);
    admissions
        .iter()
        .enumerate()
        .filter(|(_, a)| a.refusal.is_none() && playback(a.id))
        .map(|(position, a)| (position, a.id))
        .collect()
}

/// The kinds of the admitted tracks that each carry one channel, in selection
/// order. Both sides derive the transfer order from this.
fn channel_kinds(offer: &Offer, admissions: &[Admission]) -> Vec<Kind> {
    admissions
        .iter()
        .filter(|a| a.refusal.is_none())
        .filter_map(|a| offer.tracks.iter().find(|t| t.id == a.id))
        .map(|t| t.kind.clone())
        .filter(|kind| matches!(kind, Kind::Input | Kind::Affordances))
        .collect()
}

/// A received offer awaiting the consumer's selection. The five-second bound
/// started at [`connect`] and covers the selection too.
pub struct Offered {
    handshake: Handshake,
    offer: Offer,
}

pub struct Connected {
    /// Continue with CpuSetupClient on this original, blocking connection.
    pub media: Stream,
    /// The host's answers, in selection order.
    pub admissions: Vec<Admission>,
    pub input: Option<Client>,
    /// A clean input admission rejection for an optional input track.
    pub input_error: Option<input::Error>,
    pub affordances: Option<affordances::Host>,
    /// The admitted playback rings, in selection order.
    pub playback: Vec<audio::PlaybackRing>,
    /// Present when playback was admitted.
    pub doorbell: Option<audio::Doorbell>,
}

/// Read the offer of the selected source. Call off the GUI/input thread.
pub fn connect(stream: Stream) -> Result<Offered, Error> {
    let mut handshake = Handshake::new(stream)?;
    let offer: Offer = handshake.read()?;
    let mut ids = HashSet::new();
    if !offer.tracks.iter().all(|t| ids.insert(t.id)) {
        return Err(Error::Protocol("duplicate track ID"));
    }
    Ok(Offered { handshake, offer })
}

impl Offered {
    pub fn offer(&self) -> &Offer {
        &self.offer
    }

    /// Select tracks. `mode` is the input mode requested when an input track
    /// is admitted. Optional refusals, and a clean input admission rejection
    /// for optional input, are reported in the result; required ones fail the
    /// whole setup. Input admission adds its own five-second bound.
    pub fn select(mut self, selection: &[Selected], mode: Mode) -> Result<Connected, Error> {
        let mut ids = HashSet::new();
        if selection.len() > self.offer.tracks.len() || !selection.iter().all(|s| ids.insert(s.id)) {
            return Err(Error::Protocol("invalid selection"));
        }
        self.handshake.write(&Selection {
            tracks: selection.to_vec(),
        })?;
        let admitted: Admitted = self.handshake.read()?;
        if admitted.tracks.len() != selection.len() || admitted.tracks.iter().zip(selection).any(|(a, s)| a.id != s.id) {
            return Err(Error::Protocol("admission does not answer the selection"));
        }
        if let Some((_, a)) = selection
            .iter()
            .zip(&admitted.tracks)
            .find(|(s, a)| s.required && a.refusal.is_some())
        {
            return Err(Error::Refused {
                id: a.id,
                refusal: a.refusal.expect("refused"),
            });
        }
        let mut input = None;
        let mut affordances = None;
        let mut input_required = false;
        for kind in channel_kinds(&self.offer, &admitted.tracks) {
            let channel = self.handshake.receive_channel()?;
            if kind == Kind::Input {
                input_required = selection
                    .iter()
                    .any(|s| s.required && self.offer.tracks.iter().any(|t| t.id == s.id && t.kind == Kind::Input));
                input = Some(channel);
            } else {
                affordances = Some(affordances::Host::start(channel)?);
            }
        }
        let playing = playback_tracks(&self.offer, &admitted.tracks);
        let mut playback = Vec::new();
        let mut doorbell = None;
        if !playing.is_empty() {
            let mut objects = self.handshake.receive_objects(playing.len() + 2)?.into_iter();
            let (Some(page), Some(wake)) = (objects.next(), objects.next()) else {
                return Err(Error::Protocol("missing doorbell"));
            };
            doorbell = Some(audio::Doorbell::map(page, wake)?);
            for ((_, id), fd) in playing.into_iter().zip(objects) {
                let track = self.offer.tracks.iter().find(|t| t.id == id).expect("offered");
                let descriptor = audio::Descriptor::decode(&track.descriptor)?;
                playback.push(audio::PlaybackRing::map(id, descriptor, fd)?);
            }
        }
        let media = self.handshake.finish()?;
        let (input, input_error) = match input.map(|stream| Client::connect(stream, mode)) {
            None => (None, None),
            Some(Ok(client)) => (Some(client), None),
            Some(Err(ConnectError::Admission(error))) if input_required => return Err(Error::Input(error)),
            Some(Err(ConnectError::Admission(error))) => (None, Some(error)),
            Some(Err(ConnectError::Transport(error))) => return Err(error.into()),
        };
        Ok(Connected {
            media,
            admissions: admitted.tracks,
            input,
            input_error,
            affordances,
            playback,
            doorbell,
        })
    }
}

#[cfg(unix)]
fn channel_pair() -> io::Result<(Stream, Stream)> {
    Stream::pair()
}

#[cfg(windows)]
fn channel_pair() -> io::Result<(Stream, Stream)> {
    crate::local::pipe_pair()
}

// Exact reads under an absolute deadline (crate::local::Bounded), not a
// per-byte timeout a trickling peer can extend. No buffered reader may eat
// media bytes or the ancillary-data byte belonging to a channel.
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
    fn read<M: Message>(&mut self) -> Result<M, Error> {
        let mut header = [0; HEADER_LEN];
        self.exchange.read_exact(&mut header)?;
        let mut body = vec![0; FRAMING.body_len(&header)?];
        self.exchange.read_exact(&mut body)?;
        Ok(flotilla_cbor::decode_message(&body, &LIMITS)?)
    }
    fn write<M: Message>(&mut self, message: &M) -> Result<(), Error> {
        Ok(self.exchange.write_all(&FRAMING.encode(message)?)?)
    }
    /// The media stream, blocking again with its previous timeouts.
    fn finish(self) -> io::Result<Stream> {
        self.exchange.finish()
    }
}

#[cfg(unix)]
impl Handshake {
    fn send_channel(&mut self, peer: Stream) -> Result<(), Error> {
        self.send_objects(vec![peer.into()])
    }
    fn receive_channel(&mut self) -> Result<Stream, Error> {
        let stream = Stream::from(self.receive_objects(1)?.remove(0));
        if stream.peer_addr().is_err() {
            return Err(Error::Protocol("channel descriptor is not a connected Unix socket"));
        }
        Ok(stream)
    }
    fn send_objects(&mut self, objects: Vec<OwnedFd>) -> Result<(), Error> {
        self.exchange.writable()?;
        let fds: Vec<_> = objects.iter().map(AsRawFd::as_raw_fd).collect();
        crate::fdpass::send_fds(self.exchange.stream(), &fds)?;
        // Retain the sending copies until the peer has installed them.
        // Parallel macOS tests otherwise intermittently see channel EOF.
        let mut receipt = [0];
        self.exchange.read_exact(&mut receipt)?;
        if receipt != [1] {
            return Err(Error::Protocol("channel descriptor not received"));
        }
        // These copies must not keep a channel alive after peer loss.
        drop(objects);
        Ok(())
    }
    fn receive_objects(&mut self, count: usize) -> Result<Vec<OwnedFd>, Error> {
        self.exchange.readable()?;
        let fds = crate::fdpass::recv_fds(self.exchange.stream(), count)?;
        for fd in &fds {
            // SAFETY: this live received FD must not leak through exec.
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error().into());
            }
        }
        if fds.len() != count {
            return Err(Error::Protocol("missing descriptors"));
        }
        self.exchange.write_all(&[1])?;
        Ok(fds)
    }
}

// Handle transfer is one write and one acknowledgement, both bounded by the
// time left: ready() sets the pipe's timeouts to it.
#[cfg(windows)]
impl Handshake {
    fn send_channel(&mut self, peer: Stream) -> Result<(), Error> {
        self.send_objects(vec![peer.into_handle()?])
    }
    fn receive_channel(&mut self) -> Result<Stream, Error> {
        let handle = self.receive_objects(1)?.remove(0);
        // SAFETY: the host this stream connected to duplicated its private pipe
        // end into this process for this receiver alone.
        let stream = unsafe { crate::local::PipeStream::from_owned_handle(handle, false) }
            .map_err(|_| Error::Protocol("channel handle is not a connected pipe"))?;
        if !stream.is_alive() {
            return Err(Error::Protocol("channel handle is not a connected pipe"));
        }
        Ok(stream)
    }
    /// Read-only rings are already restricted, so every handle keeps its access.
    fn send_objects(&mut self, objects: Vec<OwnedHandle>) -> Result<(), Error> {
        self.exchange.writable()?;
        // Duplication closes these copies before the peer learns the values,
        // so they cannot keep a channel alive after peer loss; the peer
        // acknowledges adopting them within the same deadline.
        let objects = objects.into_iter().map(|object| (object, crate::local::Access::Same)).collect();
        crate::local::send_handles(self.exchange.stream_mut(), objects)?;
        Ok(())
    }
    fn receive_objects(&mut self, count: usize) -> Result<Vec<OwnedHandle>, Error> {
        self.exchange.readable()?;
        let handles = crate::local::receive_handles(self.exchange.stream_mut(), count)?;
        if handles.len() != count {
            return Err(Error::Protocol("missing handles"));
        }
        Ok(handles)
    }
}
