//! Tracks while attached. After admission the host announces added, removed
//! and updated tracks on the incarnation's tracks channel, as setup messages
//! framed like the handshake. The consumer may select an offered playback or
//! app-kind track late with a one-track `selection`; the host answers with
//! `admitted` and, for playback, the read-only ring, preceded by the doorbell
//! when the incarnation had none. Late selections take the next selection
//! positions, so doorbell hint bits stay stable.

use std::{
    collections::{HashSet, VecDeque},
    io,
    time::Duration,
};

use flotilla_cbor::Message;
use serde::{Deserialize, Serialize};

use super::{Admission, Admitted, Direction, Error, Handshake, Kind, LIMITS, Refusal, Selected, Selection, TIMEOUT, Track, offerable};
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

/// Late tracks are playback and app kinds; the other core kinds have
/// channels fixed at admission.
fn late(kind: &Kind) -> bool {
    !matches!(kind, Kind::Video | Kind::Input | Kind::Affordances)
}

/// One message body, or None if none starts within `timeout` (zero polls).
/// A started message gets the setup bound.
fn receive(stream: &mut Stream, timeout: Duration) -> Result<Option<Vec<u8>>, Error> {
    let mut first = [0];
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
/// channel and the playback doorbell.
pub struct Notices {
    pub(super) stream: Stream,
    pub(super) tracks: Vec<Track>,
    pub(super) playback: Vec<Playback>,
    /// Every ID offered to this incarnation; none is reused.
    pub(super) offered: HashSet<u64>,
    pub(super) admitted: HashSet<u64>,
    pub(super) positions: usize,
    pub(super) bell: Option<audio::Bell>,
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

    /// Offer a playback or app-kind track with a new ID. Playback needs its ring.
    pub fn add(&mut self, track: Track, ring: Option<Playback>) -> Result<(), Error> {
        let ring = Vec::from_iter(ring);
        if !late(&track.kind) || !offerable(&track, &ring) || !self.offered.insert(track.id) {
            return Err(Error::Protocol("invalid track"));
        }
        self.playback.extend(ring);
        self.tracks.push(track.clone());
        self.send(&Notice::Added { track })
    }

    /// Replace a track's label or descriptor. A playback descriptor must still
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
            Some(_) => None,
        };
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
        if let Some(ring) = self.playback.iter().find(|p| p.id() == id) {
            let mut objects = Vec::new();
            if self.bell.is_none() {
                let (bell, bell_objects) = audio::Bell::new()?;
                (self.bell, objects) = (Some(bell), bell_objects);
            }
            // Register first, as at admission.
            ring.register(self.bell.as_ref().expect("created"), position);
            objects.push(ring.read_only_fd()?);
            handshake.send_objects(objects)?;
        }
        Ok(true)
    }
}

/// The host's answer to a late selection.
pub struct Late {
    pub admission: Admission,
    /// The doorbell hint bit, mod 64.
    pub position: usize,
    pub playback: Option<audio::PlaybackRing>,
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

    /// Select one offered track without disturbing other selections; a
    /// playback track's ring is mapped now. The host must be serving.
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
        let mut late = Late {
            admission,
            position,
            playback: None,
            doorbell: None,
        };
        let track = self.tracks.iter().find(|t| t.id == selected.id && t.kind == Kind::Playback);
        if admission.refusal.is_none()
            && let Some(track) = track
        {
            let descriptor = audio::Descriptor::decode(&track.descriptor)?;
            let count = if self.doorbell { 1 } else { 3 };
            let mut objects = Handshake::new(&mut self.stream)?.receive_objects(count)?.into_iter();
            if !self.doorbell {
                let (Some(page), Some(wake)) = (objects.next(), objects.next()) else {
                    return Err(Error::Protocol("missing doorbell"));
                };
                late.doorbell = Some(audio::Doorbell::map(page, wake)?);
                self.doorbell = true;
            }
            let ring = objects.next().ok_or(Error::Protocol("missing ring"))?;
            late.playback = Some(audio::PlaybackRing::map(selected.id, descriptor, ring)?);
        }
        Ok(late)
    }
}
