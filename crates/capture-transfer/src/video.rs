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
    pub pool_id: u64,
    pub slot_id: u64,
    pub slot_generation: u64,
    pub payload_offset: u64,
    pub payload_len: u64,
    pub payload_map_len: u64,
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

#[derive(Debug)]
pub struct ClaimedVideoSlot {
    desc: VideoFrameDesc,
    track_id: TrackId,
    segment: Arc<SharedMemorySegment>,
    pool_generation: u64,
    slot_index: usize,
}

impl ClaimedVideoSlot {
    #[must_use]
    pub fn desc(&self) -> &VideoFrameDesc {
        &self.desc
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.segment
            .slice_at(self.desc.payload_offset as usize, self.desc.payload_len as usize)
    }

    pub fn with_bytes_mut<R>(&mut self, f: impl FnOnce(&mut [u8]) -> R) -> R {
        self.segment
            .with_slice_at_mut(self.desc.payload_offset as usize, self.desc.payload_len as usize, f)
    }

    pub fn copy_from_slice(&mut self, pixels: &[u8]) {
        assert_eq!(pixels.len(), self.desc.payload_len as usize);
        self.with_bytes_mut(|bytes| bytes.copy_from_slice(pixels));
    }
}

impl AcquiredVideoFrame {
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.segment
            .slice_at(self.desc.payload_offset as usize, self.desc.payload_len as usize)
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
    pool_generation: Option<u64>,
    slot_index: Option<usize>,
    pinned_by: BTreeSet<ConsumerId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameRingEntry {
    pub sequence: u64,
    pub frame_key: u64,
    pub pool_id: u64,
    pub slot_id: u64,
    pub slot_generation: u64,
    pub payload_offset: u64,
    pub payload_len: u64,
}

#[derive(Debug)]
struct TrackFrameRing {
    entries: Vec<Option<FrameRingEntry>>,
    next_index: usize,
    len: usize,
}

impl TrackFrameRing {
    fn new(capacity: usize) -> Self {
        Self {
            entries: vec![None; capacity.max(1)],
            next_index: 0,
            len: 0,
        }
    }

    fn push(&mut self, entry: FrameRingEntry) {
        self.entries[self.next_index] = Some(entry);
        self.next_index = (self.next_index + 1) % self.entries.len();
        self.len = (self.len + 1).min(self.entries.len());
    }

    fn latest(&self) -> Option<&FrameRingEntry> {
        if self.len == 0 {
            return None;
        }
        let index = if self.next_index == 0 {
            self.entries.len() - 1
        } else {
            self.next_index - 1
        };
        self.entries[index].as_ref()
    }

    fn snapshot(&self) -> Vec<FrameRingEntry> {
        let start = if self.len == self.entries.len() { self.next_index } else { 0 };
        (0..self.len)
            .filter_map(|offset| {
                let index = (start + offset) % self.entries.len();
                self.entries[index].clone()
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoStorageMode {
    ImmutablePerFrame,
    ReusablePool,
}

#[derive(Debug)]
struct TrackPool {
    pool_id: u64,
    generation: u64,
    segment: Arc<SharedMemorySegment>,
    slot_stride: usize,
    slot_count: usize,
    next_slot: usize,
}

#[derive(Debug)]
struct PublishedPayload {
    desc: VideoFrameDesc,
    segment: Arc<SharedMemorySegment>,
    pool_generation: Option<u64>,
    slot_index: Option<usize>,
}

#[derive(Debug)]
pub struct VideoSlotManager {
    capacity_per_track: usize,
    storage_mode: VideoStorageMode,
    next_frame_key: u64,
    next_pool_id: u64,
    next_pool_generation: u64,
    frames_by_track: BTreeMap<TrackId, Vec<StoredFrame>>,
    rings_by_track: BTreeMap<TrackId, TrackFrameRing>,
    pools_by_track: BTreeMap<TrackId, TrackPool>,
    pending_claims_by_track: BTreeMap<TrackId, BTreeSet<(u64, usize)>>,
    evicted_by_track: BTreeMap<TrackId, u64>,
    last_acquired_by_consumer: BTreeMap<(ConsumerId, TrackId), u64>,
    skipped_by_consumer: BTreeMap<(ConsumerId, TrackId), u64>,
}

impl VideoSlotManager {
    #[must_use]
    pub fn new(capacity_per_track: usize) -> Self {
        Self {
            capacity_per_track: capacity_per_track.max(1),
            storage_mode: VideoStorageMode::ImmutablePerFrame,
            next_frame_key: 1,
            next_pool_id: 1,
            next_pool_generation: 1,
            frames_by_track: BTreeMap::new(),
            rings_by_track: BTreeMap::new(),
            pools_by_track: BTreeMap::new(),
            pending_claims_by_track: BTreeMap::new(),
            evicted_by_track: BTreeMap::new(),
            last_acquired_by_consumer: BTreeMap::new(),
            skipped_by_consumer: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn new_reusable_pool(capacity_per_track: usize) -> Self {
        let mut manager = Self::new(capacity_per_track);
        manager.storage_mode = VideoStorageMode::ReusablePool;
        manager
    }

    pub fn publish(&mut self, track_id: TrackId, desc: VideoFrameDesc, pixels: &[u8]) -> Result<()> {
        let key = self.next_frame_key;
        self.next_frame_key += 1;
        let payload = match self.storage_mode {
            VideoStorageMode::ImmutablePerFrame => self.publish_immutable_frame(desc, pixels)?,
            VideoStorageMode::ReusablePool => {
                let mut claim = self.claim_video_slot(track_id, desc, pixels.len())?;
                claim.copy_from_slice(pixels);
                self.commit_video_slot_with_key(key, claim)?
            }
        };
        self.store_published_payload(track_id, key, payload);
        Ok(())
    }

    pub fn claim_video_slot(&mut self, track_id: TrackId, mut desc: VideoFrameDesc, len: usize) -> Result<ClaimedVideoSlot> {
        if self.storage_mode != VideoStorageMode::ReusablePool {
            return Err(CaptureTransferError::SharedMemory {
                operation: "claim-video-slot",
                message: "video slot claiming requires reusable-pool storage".to_string(),
            });
        }
        let slot_count = self.capacity_per_track;
        let required_stride = aligned_slot_stride(len);
        let needs_new_pool = self
            .pools_by_track
            .get(&track_id)
            .is_none_or(|pool| pool.slot_stride < required_stride || pool.slot_count != slot_count);
        if needs_new_pool {
            self.replace_pool(track_id, required_stride)?;
        }

        let mut slot_index = self.find_free_slot(track_id);
        if slot_index.is_none() {
            self.replace_pool(track_id, required_stride)?;
            slot_index = Some(0);
        }
        let slot_index = slot_index.expect("replacement pool must have at least one slot");
        let pool = self.pools_by_track.get_mut(&track_id).expect("pool exists after creation");
        let offset = pool.slot_stride * slot_index;
        pool.next_slot = (slot_index + 1) % pool.slot_count;
        self.pending_claims_by_track
            .entry(track_id)
            .or_default()
            .insert((pool.generation, slot_index));

        desc.payload_offset = offset as u64;
        desc.payload_len = len as u64;
        desc.payload_map_len = pool.segment.len() as u64;
        desc.pool_id = pool.pool_id;
        desc.slot_id = slot_index as u64;
        desc.slot_generation = pool.generation;

        Ok(ClaimedVideoSlot {
            desc,
            track_id,
            segment: Arc::clone(&pool.segment),
            pool_generation: pool.generation,
            slot_index,
        })
    }

    pub fn commit_video_slot(&mut self, claim: ClaimedVideoSlot) -> Result<()> {
        let key = self.next_frame_key;
        self.next_frame_key += 1;
        let track_id = claim.track_id;
        let payload = self.commit_video_slot_with_key(key, claim)?;
        self.store_published_payload(track_id, key, payload);
        Ok(())
    }

    fn commit_video_slot_with_key(&mut self, _key: u64, claim: ClaimedVideoSlot) -> Result<PublishedPayload> {
        // The frame key is assigned by the caller and stored with the returned
        // payload. It is accepted here to keep this helper paired with the
        // publish path until ring headers carry their own key fields.
        if let Some(pending_claims) = self.pending_claims_by_track.get_mut(&claim.track_id) {
            pending_claims.remove(&(claim.pool_generation, claim.slot_index));
        }
        if let Some(frames) = self.frames_by_track.get_mut(&claim.track_id) {
            frames.retain(|frame| {
                frame.pool_generation != Some(claim.pool_generation)
                    || frame.slot_index != Some(claim.slot_index)
                    || !frame.pinned_by.is_empty()
            });
        }
        Ok(PublishedPayload {
            desc: claim.desc,
            segment: claim.segment,
            pool_generation: Some(claim.pool_generation),
            slot_index: Some(claim.slot_index),
        })
    }

    fn store_published_payload(&mut self, track_id: TrackId, key: u64, payload: PublishedPayload) {
        let ring_entry = FrameRingEntry {
            sequence: payload.desc.sequence,
            frame_key: key,
            pool_id: payload.desc.pool_id,
            slot_id: payload.desc.slot_id,
            slot_generation: payload.desc.slot_generation,
            payload_offset: payload.desc.payload_offset,
            payload_len: payload.desc.payload_len,
        };
        let frames = self.frames_by_track.entry(track_id).or_default();
        frames.push(StoredFrame {
            key,
            desc: payload.desc,
            segment: payload.segment,
            pool_generation: payload.pool_generation,
            slot_index: payload.slot_index,
            pinned_by: BTreeSet::new(),
        });
        self.rings_by_track
            .entry(track_id)
            .or_insert_with(|| TrackFrameRing::new(self.capacity_per_track))
            .push(ring_entry);
        let evicted = Self::prune_unpinned(frames, self.capacity_per_track);
        *self.evicted_by_track.entry(track_id).or_default() += evicted;
    }

    pub fn acquire_latest(&mut self, consumer_id: ConsumerId, track_id: TrackId) -> Result<AcquiredVideoFrame> {
        let frame_key = self
            .rings_by_track
            .get(&track_id)
            .and_then(TrackFrameRing::latest)
            .map(|entry| entry.frame_key)
            .ok_or(CaptureTransferError::UnknownTrack { track_id })?;
        let frame = self
            .frames_by_track
            .get_mut(&track_id)
            .and_then(|frames| frames.iter_mut().find(|frame| frame.key == frame_key))
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

    #[must_use]
    pub fn debug_ring_snapshot(&self, track_id: TrackId) -> Vec<FrameRingEntry> {
        self.rings_by_track.get(&track_id).map_or_else(Vec::new, TrackFrameRing::snapshot)
    }

    fn prune_unpinned(frames: &mut Vec<StoredFrame>, capacity: usize) -> u64 {
        let mut evicted = 0;
        while frames.len() > capacity {
            let newest_index = frames.len() - 1;
            if let Some(index) = frames.iter().take(newest_index).position(|frame| frame.pinned_by.is_empty()) {
                frames.remove(index);
                evicted += 1;
            } else {
                break;
            }
        }
        evicted
    }

    fn publish_immutable_frame(&self, mut desc: VideoFrameDesc, pixels: &[u8]) -> Result<PublishedPayload> {
        let mut segment = SharedMemorySegment::new(pixels.len())?;
        segment.as_mut_slice().copy_from_slice(pixels);
        desc.pool_id = 0;
        desc.slot_id = 0;
        desc.slot_generation = 0;
        desc.payload_offset = 0;
        desc.payload_len = pixels.len() as u64;
        desc.payload_map_len = pixels.len() as u64;
        Ok(PublishedPayload {
            desc,
            segment: Arc::new(segment),
            pool_generation: None,
            slot_index: None,
        })
    }

    fn replace_pool(&mut self, track_id: TrackId, slot_stride: usize) -> Result<()> {
        let pool_id = self.next_pool_id;
        self.next_pool_id += 1;
        let generation = self.next_pool_generation;
        self.next_pool_generation += 1;
        let len = slot_stride
            .checked_mul(self.capacity_per_track)
            .ok_or_else(|| CaptureTransferError::SharedMemory {
                operation: "pool-size",
                message: "pool size overflow".to_string(),
            })?;
        let segment = SharedMemorySegment::new(len)?;
        self.pools_by_track.insert(
            track_id,
            TrackPool {
                pool_id,
                generation,
                segment: Arc::new(segment),
                slot_stride,
                slot_count: self.capacity_per_track,
                next_slot: 0,
            },
        );
        // Outstanding claims for the old generation still own their mmap Arc,
        // but clearing the pending set makes later commits unpublishable no-ops.
        self.pending_claims_by_track.remove(&track_id);
        Ok(())
    }

    fn find_free_slot(&self, track_id: TrackId) -> Option<usize> {
        let pool = self.pools_by_track.get(&track_id)?;
        let frames = self.frames_by_track.get(&track_id);
        let pending_claims = self.pending_claims_by_track.get(&track_id);
        for attempt in 0..pool.slot_count {
            let slot_index = (pool.next_slot + attempt) % pool.slot_count;
            let pinned = frames.is_some_and(|frames| {
                frames.iter().any(|frame| {
                    frame.pool_generation == Some(pool.generation) && frame.slot_index == Some(slot_index) && !frame.pinned_by.is_empty()
                })
            });
            let claimed = pending_claims.is_some_and(|claims| claims.contains(&(pool.generation, slot_index)));
            if !pinned && !claimed {
                return Some(slot_index);
            }
        }
        None
    }
}

fn aligned_slot_stride(len: usize) -> usize {
    const ALIGNMENT: usize = 64;
    let len = len.max(1);
    len.div_ceil(ALIGNMENT) * ALIGNMENT
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
            pool_id: 0,
            slot_id: 0,
            slot_generation: 0,
            payload_offset: 0,
            payload_len: 0,
            payload_map_len: 0,
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
        assert_eq!(frame.desc.payload_len, 4);
        assert!(frame.desc.payload_offset + frame.desc.payload_len <= frame.desc.payload_map_len);
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
        desc.payload_len = 4;
        desc.payload_map_len = 4;
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
    fn latest_publish_survives_when_older_frames_are_pinned() {
        let mut slots = VideoSlotManager::new(1);
        let track = TrackId::new(1);
        let consumer = ConsumerId::new(7);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        let old = slots.acquire_latest(consumer, track).unwrap();

        slots.publish(track, frame_desc(2), &[2]).unwrap();
        let latest = slots.acquire_latest(consumer, track).unwrap();

        assert_eq!(old.bytes(), &[1]);
        assert_eq!(latest.desc.sequence, 2);
        assert_eq!(latest.bytes(), &[2]);
        assert_eq!(slots.pinned_frame_count(), 2);
    }

    #[test]
    fn unpinned_frames_reuse_track_pool_slots() {
        let mut slots = VideoSlotManager::new_reusable_pool(2);
        let track = TrackId::new(1);
        let consumer = ConsumerId::new(7);

        slots.publish(track, frame_desc(1), &[1, 2, 3, 4]).unwrap();
        let first = slots.acquire_latest(consumer, track).unwrap();
        let first_offset = first.desc.payload_offset;
        let map_len = first.desc.payload_map_len;
        slots.release(first);

        slots.publish(track, frame_desc(2), &[5, 6, 7, 8]).unwrap();
        let second = slots.acquire_latest(consumer, track).unwrap();

        assert_eq!(second.desc.payload_map_len, map_len);
        assert_ne!(second.desc.payload_offset, first_offset);
        assert_eq!(second.desc.payload_len, 4);
        assert_eq!(second.bytes(), &[5, 6, 7, 8]);
    }

    #[test]
    fn reusable_pool_frames_expose_slot_identity() {
        let mut slots = VideoSlotManager::new_reusable_pool(2);
        let track = TrackId::new(1);
        let consumer = ConsumerId::new(7);

        slots.publish(track, frame_desc(1), &[1, 2, 3, 4]).unwrap();
        let first = slots.acquire_latest(consumer, track).unwrap();
        assert_ne!(first.desc.pool_id, 0);
        assert_eq!(first.desc.slot_id, 0);
        assert_ne!(first.desc.slot_generation, 0);
        slots.release(first);

        slots.publish(track, frame_desc(2), &[5, 6, 7, 8]).unwrap();
        let second = slots.acquire_latest(consumer, track).unwrap();
        assert_ne!(second.desc.pool_id, 0);
        assert_eq!(second.desc.slot_id, 1);
        assert_ne!(second.desc.slot_generation, 0);
    }

    #[test]
    fn claimed_slot_can_be_filled_and_committed_without_source_slice() {
        let mut slots = VideoSlotManager::new_reusable_pool(2);
        let track = TrackId::new(1);
        let desc = frame_desc(1);

        let mut claim = slots.claim_video_slot(track, desc, 4).unwrap();
        claim.with_bytes_mut(|bytes| bytes.copy_from_slice(&[9, 8, 7, 6]));
        let slot_id = claim.desc().slot_id;
        slots.commit_video_slot(claim).unwrap();

        let frame = slots.acquire_latest(ConsumerId::new(7), track).unwrap();
        assert_eq!(frame.desc.sequence, 1);
        assert_eq!(frame.desc.slot_id, slot_id);
        assert_eq!(frame.bytes(), &[9, 8, 7, 6]);
    }

    #[test]
    fn uncommitted_claim_does_not_publish_frame() {
        let mut slots = VideoSlotManager::new_reusable_pool(2);
        let track = TrackId::new(1);

        let mut claim = slots.claim_video_slot(track, frame_desc(1), 4).unwrap();
        claim.with_bytes_mut(|bytes| bytes.copy_from_slice(&[1, 2, 3, 4]));
        drop(claim);

        assert!(slots.acquire_latest(ConsumerId::new(7), track).is_err());
        assert!(slots.debug_ring_snapshot(track).is_empty());
    }

    #[test]
    fn outstanding_claims_reserve_distinct_slots() {
        let mut slots = VideoSlotManager::new_reusable_pool(2);
        let track = TrackId::new(1);

        let first = slots.claim_video_slot(track, frame_desc(1), 4).unwrap();
        let second = slots.claim_video_slot(track, frame_desc(2), 4).unwrap();

        assert_ne!(first.desc().slot_id, second.desc().slot_id);
    }

    #[test]
    fn acquire_latest_resolves_latest_ring_entry() {
        let mut slots = VideoSlotManager::new_reusable_pool(2);
        let track = TrackId::new(1);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        slots.publish(track, frame_desc(2), &[2]).unwrap();
        slots.publish(track, frame_desc(3), &[3]).unwrap();

        let ring = slots.debug_ring_snapshot(track);
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.iter().map(|entry| entry.sequence).collect::<Vec<_>>(), vec![2, 3]);

        let latest = slots.acquire_latest(ConsumerId::new(7), track).unwrap();
        assert_eq!(latest.desc.sequence, 3);
        assert_eq!(latest.bytes(), &[3]);
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
        use std::{
            fs::File,
            io::{Read, Seek, SeekFrom},
        };

        let mut slots = VideoSlotManager::new(2);
        let track = TrackId::new(1);

        slots.publish(track, frame_desc(1), &[9, 8, 7]).unwrap();
        let frame = slots.acquire_latest(ConsumerId::new(7), track).unwrap();

        let mut file = File::from(frame.try_clone_fd().unwrap());
        let mut bytes = Vec::new();
        file.seek(SeekFrom::Start(frame.desc.payload_offset)).unwrap();
        file.take(frame.desc.payload_len).read_to_end(&mut bytes).unwrap();

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
