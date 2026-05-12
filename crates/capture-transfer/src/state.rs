use std::collections::BTreeMap;

use crate::{
    error::{CaptureTransferError, Result},
    model::{SourceDesc, SourceId, TrackDesc, TrackId},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRegistration {
    pub source_id: SourceId,
    pub desc: SourceDesc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRegistration {
    pub track_id: TrackId,
    pub source_id: SourceId,
    pub desc: TrackDesc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    kind: EventKind,
    source_id: Option<SourceId>,
    track_id: Option<TrackId>,
    source_desc: Option<SourceDesc>,
    track_desc: Option<TrackDesc>,
}

impl Event {
    #[must_use]
    pub fn source_registered(source_id: SourceId, desc: SourceDesc) -> Self {
        Self {
            kind: EventKind::SourceRegistered,
            source_id: Some(source_id),
            track_id: None,
            source_desc: Some(desc),
            track_desc: None,
        }
    }

    #[must_use]
    pub fn source_updated(source_id: SourceId, desc: SourceDesc) -> Self {
        Self {
            kind: EventKind::SourceUpdated,
            source_id: Some(source_id),
            track_id: None,
            source_desc: Some(desc),
            track_desc: None,
        }
    }

    #[must_use]
    pub fn source_unregistered(source_id: SourceId) -> Self {
        Self {
            kind: EventKind::SourceUnregistered,
            source_id: Some(source_id),
            track_id: None,
            source_desc: None,
            track_desc: None,
        }
    }

    #[must_use]
    pub fn track_registered(track_id: TrackId, source_id: SourceId, desc: TrackDesc) -> Self {
        Self {
            kind: EventKind::TrackRegistered,
            source_id: Some(source_id),
            track_id: Some(track_id),
            source_desc: None,
            track_desc: Some(desc),
        }
    }

    #[must_use]
    pub fn track_updated(track_id: TrackId, source_id: SourceId, desc: TrackDesc) -> Self {
        Self {
            kind: EventKind::TrackUpdated,
            source_id: Some(source_id),
            track_id: Some(track_id),
            source_desc: None,
            track_desc: Some(desc),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> EventKind {
        self.kind
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    ProducerStarted,
    SourceRegistered,
    SourceUpdated,
    TrackRegistered,
    TrackUpdated,
    SourceUnregistered,
    ProducerStopped,
}

#[derive(Debug, Default)]
pub struct SessionState {
    next_source_id: u64,
    next_track_id: u64,
    sources: BTreeMap<SourceId, SourceRegistration>,
    tracks: BTreeMap<TrackId, TrackRegistration>,
    events: Vec<Event>,
}

impl SessionState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_source_id: 1,
            next_track_id: 1,
            sources: BTreeMap::new(),
            tracks: BTreeMap::new(),
            events: Vec::new(),
        }
    }

    pub fn register_source(&mut self, desc: SourceDesc) -> Result<SourceId> {
        let source_id = SourceId::new(self.next_source_id);
        self.next_source_id += 1;
        self.sources.insert(
            source_id,
            SourceRegistration {
                source_id,
                desc: desc.clone(),
            },
        );
        self.events.push(Event::source_registered(source_id, desc));
        Ok(source_id)
    }

    pub fn update_source(&mut self, source_id: SourceId, desc: SourceDesc) -> Result<()> {
        let registration = self
            .sources
            .get_mut(&source_id)
            .ok_or(CaptureTransferError::UnknownSource { source_id })?;
        registration.desc = desc.clone();
        self.events.push(Event::source_updated(source_id, desc));
        Ok(())
    }

    pub fn unregister_source(&mut self, source_id: SourceId) -> Result<()> {
        self.sources
            .remove(&source_id)
            .ok_or(CaptureTransferError::UnknownSource { source_id })?;
        self.tracks.retain(|_, registration| registration.source_id != source_id);
        self.events.push(Event::source_unregistered(source_id));
        Ok(())
    }

    pub fn register_track(&mut self, source_id: SourceId, desc: TrackDesc) -> Result<TrackId> {
        self.source(source_id)?;

        let track_id = TrackId::new(self.next_track_id);
        self.next_track_id += 1;
        self.tracks.insert(
            track_id,
            TrackRegistration {
                track_id,
                source_id,
                desc: desc.clone(),
            },
        );
        self.events.push(Event::track_registered(track_id, source_id, desc));
        Ok(track_id)
    }

    pub fn update_track(&mut self, track_id: TrackId, desc: TrackDesc) -> Result<()> {
        let registration = self
            .tracks
            .get_mut(&track_id)
            .ok_or(CaptureTransferError::UnknownTrack { track_id })?;
        registration.desc = desc.clone();
        self.events.push(Event::track_updated(track_id, registration.source_id, desc));
        Ok(())
    }

    pub fn source(&self, source_id: SourceId) -> Result<&SourceRegistration> {
        self.sources
            .get(&source_id)
            .ok_or(CaptureTransferError::UnknownSource { source_id })
    }

    pub fn track(&self, track_id: TrackId) -> Result<&TrackRegistration> {
        self.tracks.get(&track_id).ok_or(CaptureTransferError::UnknownTrack { track_id })
    }

    #[must_use]
    pub fn replay_events(&self) -> Vec<Event> {
        self.events.clone()
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        error::CaptureTransferError,
        model::{PixelFormat, SourceDesc, SourceId, SourceKind, TrackDesc, TrackType, VideoTrackDesc},
        state::{Event, EventKind, SessionState},
    };

    fn window_source(label: &str) -> SourceDesc {
        SourceDesc {
            kind: SourceKind::Window,
            label: label.to_string(),
        }
    }

    fn bgra_track(width: u32, height: u32) -> TrackDesc {
        TrackDesc::Video(VideoTrackDesc {
            width,
            height,
            pixel_format: PixelFormat::Bgra8Unorm,
        })
    }

    #[test]
    fn registering_sources_allocates_stable_nonzero_ids() {
        let mut session = SessionState::new();

        let first = session.register_source(window_source("Terminal")).unwrap();
        let second = session.register_source(window_source("Preview")).unwrap();

        assert_eq!(first, SourceId::new(1));
        assert_eq!(second, SourceId::new(2));
        assert_ne!(first, second);
    }

    #[test]
    fn registering_video_track_requires_existing_source() {
        let mut session = SessionState::new();

        let err = session.register_track(SourceId::new(404), bgra_track(640, 480)).unwrap_err();

        assert_eq!(
            err,
            CaptureTransferError::UnknownSource {
                source_id: SourceId::new(404)
            }
        );
    }

    #[test]
    fn registering_video_track_records_source_relationship() {
        let mut session = SessionState::new();
        let source = session.register_source(window_source("Terminal")).unwrap();

        let track = session.register_track(source, bgra_track(800, 600)).unwrap();
        let registration = session.track(track).unwrap();

        assert_eq!(registration.source_id, source);
        assert_eq!(registration.desc.track_type(), TrackType::Video);
    }

    #[test]
    fn updating_source_and_track_emit_replayable_events() {
        let mut session = SessionState::new();
        let source = session.register_source(window_source("Terminal")).unwrap();
        let track = session.register_track(source, bgra_track(800, 600)).unwrap();

        session.update_source(source, window_source("Terminal - vim")).unwrap();
        session.update_track(track, bgra_track(1024, 768)).unwrap();

        let events = session.replay_events();

        assert_eq!(
            events,
            vec![
                Event::source_registered(source, window_source("Terminal")),
                Event::track_registered(track, source, bgra_track(800, 600)),
                Event::source_updated(source, window_source("Terminal - vim")),
                Event::track_updated(track, source, bgra_track(1024, 768)),
            ]
        );
    }

    #[test]
    fn unregistering_source_removes_attached_tracks_and_emits_terminal_event() {
        let mut session = SessionState::new();
        let source = session.register_source(window_source("Terminal")).unwrap();
        let track = session.register_track(source, bgra_track(800, 600)).unwrap();

        session.unregister_source(source).unwrap();

        assert!(session.source(source).is_err());
        assert!(session.track(track).is_err());
        assert_eq!(session.replay_events().last().map(Event::kind), Some(EventKind::SourceUnregistered));
    }
}
