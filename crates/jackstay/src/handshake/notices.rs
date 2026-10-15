//! Tracks while attached. After admission the host announces added, removed
//! and updated tracks on the incarnation's tracks channel, as setup messages
//! framed like the handshake. The consumer may select an offered audio or
//! app-kind track late with a one-track `selection`; the host answers with
//! `admitted` and, for audio, the ring, preceded by the doorbell when this is
//! the incarnation's first playback track. Late selections take the next selection
//! positions, so doorbell hint bits stay stable.

use std::{
    collections::{HashSet, VecDeque},
    io,
    time::Duration,
};

use flotilla_cbor::Message;
use serde::{Deserialize, Serialize};

use super::{
    Admission, Admitted, Direction, Error, Handshake, Kind, LIMITS, Refusal, Rings, Selected, Selection, TIMEOUT, Track, claim, offerable,
    receive_rings, send_rings,
};
use crate::{
    audio::{self, Playback},
    local::{Bounded, Stream},
};

/// A change to the offer while attached.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Notice {
    Added {
        track: Track,
    },
    Removed {
        id: u64,
    },
    /// A new descriptor for the same track, for example after a device
    /// reopen. Selections survive it.
    Updated {
        track: Track,
    },
}

impl Message for Notice {
    const TYPES: &'static [&'static str] = &["added", "removed", "updated"];
}

/// Late tracks are audio and app kinds; the other core kinds have
/// channels fixed at admission.
fn late(kind: &Kind) -> bool {
    !matches!(kind, Kind::Video | Kind::Input | Kind::Affordances)
}

/// One message body, or None if none starts within `timeout` (zero polls).
/// A started message gets the setup bound.
fn receive(stream: &mut Stream, timeout: Duration) -> Result<Option<Vec<u8>>, Error> {
    let mut first = [0];
    // Bounded treats no time left as timed out before reading, so a zero
    // timeout becomes the shortest poll.
    let timeout = timeout.max(Duration::from_millis(1));
    let started = Bounded::new(&mut *stream, timeout)?.read_exact(&mut first);
    match started {
        Err(error) if error.kind() == io::ErrorKind::TimedOut => Ok(None),
        started => {
            started?;
            Ok(Some(Handshake::new(stream)?.read_body(&first)?))
        }
    }
}

/// A host's handle on one incarnation's tracks. Dropping it closes the
/// channel and the playback doorbell, and ends any recording writer's
/// admission.
pub struct Notices {
    pub(super) stream: Stream,
    pub(super) tracks: Vec<Track>,
    pub(super) playback: Vec<Playback>,
    /// Every ID offered to this incarnation; none is reused.
    pub(super) offered: HashSet<u64>,
    pub(super) admitted: HashSet<u64>,
    pub(super) positions: usize,
    pub(super) bell: Option<audio::Bell>,
    pub(super) leases: Vec<audio::Lease>,
}

impl Notices {
    fn send(&mut self, notice: &Notice) -> Result<(), Error> {
        Handshake::new(&mut self.stream)?.write(notice)
    }

    fn find(&self, id: u64) -> Result<usize, Error> {
        let index = self.tracks.iter().position(|t| t.id == id);
        index
            .filter(|i| late(&self.tracks[*i].kind))
            .ok_or(Error::Protocol("unknown track"))
    }

    /// Offer an audio or app-kind track with a new ID. Audio needs its ring.
    pub fn add(&mut self, track: Track, ring: Option<Playback>) -> Result<(), Error> {
        let ring = Vec::from_iter(ring);
        if !late(&track.kind) || track.kind.audio() == ring.is_empty() || !offerable(&track, &ring) || !self.offered.insert(track.id) {
            return Err(Error::Protocol("invalid track"));
        }
        self.playback.extend(ring);
        self.tracks.push(track.clone());
        self.send(&Notice::Added { track })
    }

    /// Replace a track's label or descriptor. An audio descriptor must still
    /// fit its ring: reconfigure the writer, which marks the ring.
    pub fn update(&mut self, track: Track) -> Result<(), Error> {
        let index = self.find(track.id)?;
        if self.tracks[index].kind != track.kind || !offerable(&track, &self.playback) {
            return Err(Error::Protocol("invalid track"));
        }
        self.tracks[index] = track.clone();
        self.send(&Notice::Updated { track })
    }

    /// Withdraw a track. Consumers keep their mapping until they drop it.
    pub fn remove(&mut self, id: u64) -> Result<(), Error> {
        self.tracks.remove(self.find(id)?);
        self.playback.retain(|p| p.id() != id);
        self.admitted.remove(&id);
        self.leases.retain(|lease| lease.id() != id);
        self.send(&Notice::Removed { id })
    }

    /// Answer one late selection if it starts within `timeout`; false if none
    /// did. Fails once the consumer has gone.
    pub fn serve(&mut self, timeout: Duration) -> Result<bool, Error> {
        let Some(body) = receive(&mut self.stream, timeout)? else {
            return Ok(false);
        };
        let selection: Selection = flotilla_cbor::decode_message(&body, &LIMITS)?;
        let [Selected { id, write, .. }] = selection.tracks[..] else {
            return Err(Error::Protocol("invalid selection"));
        };
        let refusal = match self.tracks.iter().find(|t| t.id == id) {
            None => Some(Refusal::Unknown),
            Some(t) if write && t.direction == Direction::FromSource => Some(Refusal::NotWritable),
            Some(t) if !late(&t.kind) || self.admitted.contains(&id) => Some(Refusal::Unavailable),
            Some(t) if t.kind == Kind::Recording => claim(id, write, &self.playback, &mut self.leases),
            Some(_) => None,
        };
        // Any failure from here leaves the sides' positions apart; it ends
        // the incarnation, as every channel error does.
        let position = self.positions;
        self.positions += 1;
        let mut handshake = Handshake::new(&mut self.stream)?;
        handshake.write(&Admitted {
            tracks: vec![Admission { id, refusal }],
        })?;
        if refusal.is_some() {
            return Ok(true);
        }
        self.admitted.insert(id);
        let rings: Vec<_> = self.playback.iter().filter(|p| p.id() == id).map(|ring| (position, ring)).collect();
        send_rings(&mut handshake, &mut self.bell, &rings)?;
        Ok(true)
    }
}

/// The host's answer to a late selection.
pub struct Late {
    pub admission: Admission,
    /// The doorbell hint bit, mod 64.
    pub position: usize,
    pub playback: Option<audio::PlaybackRing>,
    pub recording: Option<audio::RecordingWriter>,
    /// Present when this is the incarnation's first playback track.
    pub doorbell: Option<audio::Doorbell>,
}

/// A consumer's view of its incarnation's tracks while attached.
pub struct Tracks {
    pub(super) stream: Stream,
    pub(super) tracks: Vec<Track>,
    pub(super) positions: usize,
    pub(super) doorbell: bool,
    pub(super) pending: VecDeque<Notice>,
}

impl Tracks {
    /// The offer as amended by the notices received so far.
    #[must_use]
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn apply(&mut self, notice: Notice) -> Result<(), Error> {
        let index = |id| self.tracks.iter().position(|t: &Track| t.id == id);
        match &notice {
            Notice::Added { track } if index(track.id).is_none() => self.tracks.push(track.clone()),
            Notice::Updated { track } if let Some(i) = index(track.id) => self.tracks[i] = track.clone(),
            Notice::Removed { id } if let Some(i) = index(*id) => drop(self.tracks.remove(i)),
            _ => return Err(Error::Protocol("notice does not match the offer")),
        }
        self.pending.push_back(notice);
        Ok(())
    }

    /// The next notice, or None if none arrives within `timeout` (zero polls).
    pub fn next(&mut self, timeout: Duration) -> Result<Option<Notice>, Error> {
        if self.pending.is_empty()
            && let Some(body) = receive(&mut self.stream, timeout)?
        {
            self.apply(flotilla_cbor::decode_message(&body, &LIMITS)?)?;
        }
        Ok(self.pending.pop_front())
    }

    /// Select one offered track without disturbing other selections; an
    /// audio track's ring is mapped now. The host must be serving.
    /// Notices that arrive first stay queued for [`Tracks::next`].
    pub fn select(&mut self, selected: Selected) -> Result<Late, Error> {
        Handshake::new(&mut self.stream)?.write(&Selection { tracks: vec![selected] })?;
        let position = self.positions;
        self.positions += 1;
        let admitted: Admitted = loop {
            let body = receive(&mut self.stream, TIMEOUT)?.ok_or(io::Error::from(io::ErrorKind::TimedOut))?;
            if flotilla_cbor::message_type(&body, &LIMITS, &["added", "removed", "updated", "admitted"])? == "admitted" {
                break flotilla_cbor::decode_message(&body, &LIMITS)?;
            }
            self.apply(flotilla_cbor::decode_message(&body, &LIMITS)?)?;
        };
        let [admission] = admitted.tracks[..] else {
            return Err(Error::Protocol("admission does not answer the selection"));
        };
        if admission.id != selected.id {
            return Err(Error::Protocol("admission does not answer the selection"));
        }
        let track = self.tracks.iter().filter(|t| t.id == selected.id && t.kind.audio());
        let tracks: Vec<_> = track.filter(|_| admission.refusal.is_none()).collect();
        let Rings {
            doorbell,
            mut playback,
            mut recording,
        } = receive_rings(&mut Handshake::new(&mut self.stream)?, self.doorbell, &tracks)?;
        self.doorbell |= doorbell.is_some();
        Ok(Late {
            admission,
            position,
            playback: playback.pop(),
            recording: recording.pop(),
            doorbell,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_that_do_not_match_the_offer_fail_closed() {
        #[cfg(unix)]
        let (stream, _host) = Stream::pair().unwrap();
        #[cfg(windows)]
        let (stream, _host) = crate::local::pipe_pair().unwrap();
        let track = Track {
            id: 1,
            kind: Kind::Other("org.example.cue".into()),
            direction: Direction::FromSource,
            label: "cue".into(),
            descriptor: Default::default(),
        };
        let mut tracks = Tracks {
            stream,
            tracks: vec![track.clone()],
            positions: 0,
            doorbell: false,
            pending: VecDeque::new(),
        };
        assert!(tracks.apply(Notice::Added { track }).is_err());
        assert!(tracks.apply(Notice::Removed { id: 2 }).is_err());
        assert!(tracks.pending.is_empty() && tracks.tracks.len() == 1);
    }
}
