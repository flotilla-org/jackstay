use std::{
    collections::{BTreeMap, BTreeSet},
    os::fd::OwnedFd,
    sync::Arc,
};

use crate::{
    error::{CaptureTransferError, Result},
    model::{ClockDomain, ColorSpace, DamageKind, FrameSyncKind, PixelFormat, TrackId},
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
    pub clock_domain: ClockDomain,
    pub color_space: ColorSpace,
    pub sync_kind: FrameSyncKind,
    pub damage_kind: DamageKind,
    pub damage_base_sequence: u64,
    pub dropped_before_publish: u64,
    pub producer_drop_count: u64,
    pub evicted_count: u64,
    pub consumer_skipped_count: u64,
}

#[derive(Debug, Clone)]
pub struct AcquiredVideoFrame {
    pub desc: VideoFrameDesc,
    consumer_id: ConsumerId,
    track_id: TrackId,
    frame_key: u64,
    segment: Arc<SharedMemorySegment>,
}

impl AcquiredVideoFrame {
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.segment.as_slice()
    }

    pub fn try_clone_fd(&self) -> Result<OwnedFd> {
        self.segment.try_clone_fd()
    }
}

#[derive(Debug)]
struct StoredFrame {
    key: u64,
    desc: VideoFrameDesc,
    segment: Arc<SharedMemorySegment>,
    pinned_by: BTreeSet<ConsumerId>,
}

#[derive(Debug)]
pub struct VideoSlotManager {
    capacity_per_track: usize,
    next_frame_key: u64,
    frames_by_track: BTreeMap<TrackId, Vec<StoredFrame>>,
    evicted_by_track: BTreeMap<TrackId, u64>,
    last_acquired_by_consumer: BTreeMap<(ConsumerId, TrackId), u64>,
    skipped_by_consumer: BTreeMap<(ConsumerId, TrackId), u64>,
}

impl VideoSlotManager {
    #[must_use]
    pub fn new(capacity_per_track: usize) -> Self {
        Self {
            capacity_per_track: capacity_per_track.max(1),
            next_frame_key: 1,
            frames_by_track: BTreeMap::new(),
            evicted_by_track: BTreeMap::new(),
            last_acquired_by_consumer: BTreeMap::new(),
            skipped_by_consumer: BTreeMap::new(),
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
            segment: Arc::new(segment),
            pinned_by: BTreeSet::new(),
        });
        let evicted = Self::prune_unpinned(frames, self.capacity_per_track);
        *self.evicted_by_track.entry(track_id).or_default() += evicted;
        Ok(())
    }

    pub fn acquire_latest(&mut self, consumer_id: ConsumerId, track_id: TrackId) -> Result<AcquiredVideoFrame> {
        let frame = self
            .frames_by_track
            .get_mut(&track_id)
            .and_then(|frames| frames.last_mut())
            .ok_or(CaptureTransferError::UnknownTrack { track_id })?;
        frame.pinned_by.insert(consumer_id);
        let consumer_key = (consumer_id, track_id);
        let sequence = frame.desc.sequence;
        if let Some(previous) = self.last_acquired_by_consumer.insert(consumer_key, sequence)
            && sequence > previous.saturating_add(1)
        {
            *self.skipped_by_consumer.entry(consumer_key).or_default() += sequence - previous - 1;
        }
        let mut desc = frame.desc.clone();
        desc.evicted_count = self.evicted_by_track.get(&track_id).copied().unwrap_or(0);
        desc.consumer_skipped_count = self.skipped_by_consumer.get(&consumer_key).copied().unwrap_or(0);
        Ok(AcquiredVideoFrame {
            desc,
            consumer_id,
            track_id,
            frame_key: frame.key,
            segment: Arc::clone(&frame.segment),
        })
    }

    pub fn release(&mut self, frame: AcquiredVideoFrame) {
        if let Some(frames) = self.frames_by_track.get_mut(&frame.track_id) {
            if let Some(stored) = frames.iter_mut().find(|stored| stored.key == frame.frame_key) {
                stored.pinned_by.remove(&frame.consumer_id);
            }
            let evicted = Self::prune_unpinned(frames, self.capacity_per_track);
            *self.evicted_by_track.entry(frame.track_id).or_default() += evicted;
        }
    }

    pub fn disconnect_consumer(&mut self, consumer_id: ConsumerId) {
        for (track_id, frames) in &mut self.frames_by_track {
            for frame in frames.iter_mut() {
                frame.pinned_by.remove(&consumer_id);
            }
            let evicted = Self::prune_unpinned(frames, self.capacity_per_track);
            *self.evicted_by_track.entry(*track_id).or_default() += evicted;
        }
        self.last_acquired_by_consumer
            .retain(|(stored_consumer_id, _), _| *stored_consumer_id != consumer_id);
        self.skipped_by_consumer
            .retain(|(stored_consumer_id, _), _| *stored_consumer_id != consumer_id);
    }

    #[must_use]
    pub fn pinned_frame_count(&self) -> usize {
        self.frames_by_track
            .values()
            .flatten()
            .filter(|frame| !frame.pinned_by.is_empty())
            .count()
    }

    fn prune_unpinned(frames: &mut Vec<StoredFrame>, capacity: usize) -> u64 {
        let mut evicted = 0;
        while frames.len() > capacity {
            if let Some(index) = frames.iter().position(|frame| frame.pinned_by.is_empty()) {
                frames.remove(index);
                evicted += 1;
            } else {
                break;
            }
        }
        evicted
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        model::{ClockDomain, ColorSpace, DamageKind, FrameSyncKind, PixelFormat, TrackId},
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
            clock_domain: ClockDomain::MediaTime,
            color_space: ColorSpace::Unknown,
            sync_kind: FrameSyncKind::CpuCopyComplete,
            damage_kind: DamageKind::FullFrame,
            damage_base_sequence: sequence,
            dropped_before_publish: 0,
            producer_drop_count: 0,
            evicted_count: 0,
            consumer_skipped_count: 0,
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
    fn acquiring_latest_preserves_frame_metadata() {
        let mut slots = VideoSlotManager::new(2);
        let track = TrackId::new(1);
        let mut desc = frame_desc(7);
        desc.damage_base_sequence = 3;
        desc.producer_drop_count = 2;

        slots.publish(track, desc.clone(), &[1, 2, 3, 4]).unwrap();

        let frame = slots.acquire_latest(ConsumerId::new(7), track).unwrap();
        assert_eq!(frame.desc, desc);
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

    #[test]
    fn acquired_frame_can_clone_readable_fd() {
        use std::{fs::File, io::Read};

        let mut slots = VideoSlotManager::new(2);
        let track = TrackId::new(1);

        slots.publish(track, frame_desc(1), &[9, 8, 7]).unwrap();
        let frame = slots.acquire_latest(ConsumerId::new(7), track).unwrap();

        let mut file = File::from(frame.try_clone_fd().unwrap());
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();

        assert_eq!(bytes, &[9, 8, 7]);
    }

    #[test]
    fn cloned_fd_survives_release_and_prune() {
        use std::{fs::File, io::Read};

        let mut slots = VideoSlotManager::new(1);
        let track = TrackId::new(1);

        slots.publish(track, frame_desc(1), &[9, 8, 7]).unwrap();
        let frame = slots.acquire_latest(ConsumerId::new(7), track).unwrap();
        let mut file = File::from(frame.try_clone_fd().unwrap());
        slots.release(frame);

        slots.publish(track, frame_desc(2), &[1, 2, 3]).unwrap();
        assert_eq!(slots.pinned_frame_count(), 0);

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, &[9, 8, 7]);
    }
}
