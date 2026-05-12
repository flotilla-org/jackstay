use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use crate::{
    error::{CaptureTransferError, Result},
    model::{PixelFormat, TrackId},
    shm::SharedMemorySegment,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConsumerId(u64);

impl ConsumerId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrameDesc {
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: PixelFormat,
}

#[derive(Debug, Clone)]
pub struct AcquiredVideoFrame {
    pub desc: VideoFrameDesc,
    consumer_id: ConsumerId,
    track_id: TrackId,
    frame_key: u64,
    segment: Rc<SharedMemorySegment>,
}

impl AcquiredVideoFrame {
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.segment.as_slice()
    }
}

#[derive(Debug)]
struct StoredFrame {
    key: u64,
    desc: VideoFrameDesc,
    segment: Rc<SharedMemorySegment>,
    pinned_by: BTreeSet<ConsumerId>,
}

#[derive(Debug)]
pub struct VideoSlotManager {
    capacity_per_track: usize,
    next_frame_key: u64,
    frames_by_track: BTreeMap<TrackId, Vec<StoredFrame>>,
}

impl VideoSlotManager {
    #[must_use]
    pub fn new(capacity_per_track: usize) -> Self {
        Self {
            capacity_per_track: capacity_per_track.max(1),
            next_frame_key: 1,
            frames_by_track: BTreeMap::new(),
        }
    }

    pub fn publish(&mut self, track_id: TrackId, desc: VideoFrameDesc, pixels: &[u8]) -> Result<()> {
        let key = self.next_frame_key;
        self.next_frame_key += 1;
        let mut segment = SharedMemorySegment::new(pixels.len())?;
        segment.as_mut_slice().copy_from_slice(pixels);
        let frames = self.frames_by_track.entry(track_id).or_default();
        frames.push(StoredFrame {
            key,
            desc,
            segment: Rc::new(segment),
            pinned_by: BTreeSet::new(),
        });
        Self::prune_unpinned(frames, self.capacity_per_track);
        Ok(())
    }

    pub fn acquire_latest(&mut self, consumer_id: ConsumerId, track_id: TrackId) -> Result<AcquiredVideoFrame> {
        let frame = self
            .frames_by_track
            .get_mut(&track_id)
            .and_then(|frames| frames.last_mut())
            .ok_or(CaptureTransferError::UnknownTrack { track_id })?;
        frame.pinned_by.insert(consumer_id);
        Ok(AcquiredVideoFrame {
            desc: frame.desc.clone(),
            consumer_id,
            track_id,
            frame_key: frame.key,
            segment: Rc::clone(&frame.segment),
        })
    }

    pub fn release(&mut self, frame: AcquiredVideoFrame) {
        if let Some(frames) = self.frames_by_track.get_mut(&frame.track_id) {
            if let Some(stored) = frames.iter_mut().find(|stored| stored.key == frame.frame_key) {
                stored.pinned_by.remove(&frame.consumer_id);
            }
            Self::prune_unpinned(frames, self.capacity_per_track);
        }
    }

    pub fn disconnect_consumer(&mut self, consumer_id: ConsumerId) {
        for frames in self.frames_by_track.values_mut() {
            for frame in frames.iter_mut() {
                frame.pinned_by.remove(&consumer_id);
            }
            Self::prune_unpinned(frames, self.capacity_per_track);
        }
    }

    #[must_use]
    pub fn pinned_frame_count(&self) -> usize {
        self.frames_by_track
            .values()
            .flatten()
            .filter(|frame| !frame.pinned_by.is_empty())
            .count()
    }

    fn prune_unpinned(frames: &mut Vec<StoredFrame>, capacity: usize) {
        while frames.len() > capacity {
            if let Some(index) = frames.iter().position(|frame| frame.pinned_by.is_empty()) {
                frames.remove(index);
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        model::{PixelFormat, TrackId},
        video::{ConsumerId, VideoFrameDesc, VideoSlotManager},
    };

    fn frame_desc(sequence: u64) -> VideoFrameDesc {
        VideoFrameDesc {
            sequence,
            timestamp_ns: sequence * 1_000,
            width: 2,
            height: 1,
            stride: 8,
            pixel_format: PixelFormat::Bgra8Unorm,
        }
    }

    #[test]
    fn acquiring_latest_returns_published_pixels() {
        let mut slots = VideoSlotManager::new(2);
        let track = TrackId::new(1);

        slots.publish(track, frame_desc(1), &[1, 2, 3, 4]).unwrap();

        let frame = slots.acquire_latest(ConsumerId::new(7), track).unwrap();
        assert_eq!(frame.desc.sequence, 1);
        assert_eq!(frame.bytes(), &[1, 2, 3, 4]);
    }

    #[test]
    fn acquire_latest_skips_stale_frames() {
        let mut slots = VideoSlotManager::new(2);
        let track = TrackId::new(1);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        slots.publish(track, frame_desc(2), &[2]).unwrap();
        slots.publish(track, frame_desc(3), &[3]).unwrap();

        let frame = slots.acquire_latest(ConsumerId::new(7), track).unwrap();
        assert_eq!(frame.desc.sequence, 3);
        assert_eq!(frame.bytes(), &[3]);
    }

    #[test]
    fn acquired_frame_remains_readable_after_newer_publish() {
        let mut slots = VideoSlotManager::new(2);
        let track = TrackId::new(1);
        let consumer = ConsumerId::new(7);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        let old = slots.acquire_latest(consumer, track).unwrap();

        slots.publish(track, frame_desc(2), &[2]).unwrap();
        let latest = slots.acquire_latest(consumer, track).unwrap();

        assert_eq!(old.bytes(), &[1]);
        assert_eq!(latest.bytes(), &[2]);
        assert_eq!(slots.pinned_frame_count(), 2);

        slots.release(old);
        assert_eq!(slots.pinned_frame_count(), 1);
    }

    #[test]
    fn disconnecting_consumer_releases_its_pins() {
        let mut slots = VideoSlotManager::new(2);
        let track = TrackId::new(1);
        let consumer = ConsumerId::new(7);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        let _frame = slots.acquire_latest(consumer, track).unwrap();
        assert_eq!(slots.pinned_frame_count(), 1);

        slots.disconnect_consumer(consumer);

        assert_eq!(slots.pinned_frame_count(), 0);
    }
}
