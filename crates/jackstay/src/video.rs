#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(windows)]
use std::os::windows::io::OwnedHandle as OwnedFd;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use crate::{
    control_page::{PendingVideoRingEntry, VideoRingEntry, VideoRingReadError, VideoTrackControlPage, VideoTrackControlSnapshot},
    error::{CaptureTransferError, Result},
    model::{ClockDomain, ColorSpace, DamageKind, FrameSyncKind, PayloadKind, PixelFormat, TrackId},
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
    /// Pool ids are unique forever (never reused), so a pool reference needs
    /// no generation to detect staleness.
    pub pool_id: u64,
    pub slot_id: u32,
    pub payload_offset: u64,
    pub payload_len: u64,
    pub payload_map_len: u64,
    pub clock_domain: ClockDomain,
    pub color_space: ColorSpace,
    pub sync_kind: FrameSyncKind,
    pub damage_kind: DamageKind,
    pub damage_base_sequence: u64,
    pub dropped_before_publish: u32,
    pub producer_drop_count: u64,
    pub evicted_count: u64,
    pub consumer_skipped_count: u64,
    /// Payload kind. `CpuShm` (default) keeps pixels in the shm slot; a native
    /// kind means there is no in-band payload and the frame is the surface-pool
    /// slot above plus the `(fence_id, fence_value)` fence below.
    pub payload_kind: PayloadKind,
    /// Format modifier (e.g. dmabuf modifier). 0 / linear for IOSurface.
    pub modifier: u64,
    /// Fence/timeline identity for native frames (the producer's shared event).
    pub fence_id: u64,
    /// Monotonic value the consumer must wait for on `fence_id` before sampling.
    pub fence_value: u64,
    /// Reserved descriptor flags; 0 today.
    pub flags: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuPoolRegistration {
    pub track_id: TrackId,
    pub pool_id: u64,
    pub payload_map_len: u64,
    pub slot_stride: u64,
    pub slot_count: u64,
}

#[derive(Debug)]
pub struct VideoControlPageRegistration {
    pub track_id: TrackId,
    pub consumer_id: ConsumerId,
    pub consumer_slot: u64,
    pub map_len: u64,
    pub fd: OwnedFd,
}

#[derive(Debug, Clone)]
pub struct AcquiredVideoFrame {
    pub desc: VideoFrameDesc,
    consumer_id: ConsumerId,
    track_id: TrackId,
    producer_cursor: u64,
    segment: Arc<SharedMemorySegment>,
    pool_registration: Option<CpuPoolRegistration>,
}

#[derive(Debug)]
pub struct ClaimedVideoSlot {
    desc: VideoFrameDesc,
    track_id: TrackId,
    segment: Arc<SharedMemorySegment>,
    slot_index: usize,
    slot_stride: usize,
    slot_count: usize,
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

    #[must_use]
    pub const fn producer_cursor(&self) -> u64 {
        self.producer_cursor
    }

    pub fn try_clone_fd(&self) -> Result<OwnedFd> {
        self.segment.try_clone_fd()
    }

    #[must_use]
    pub fn cpu_pool_registration(&self) -> Option<CpuPoolRegistration> {
        self.pool_registration.clone()
    }
}

#[derive(Debug)]
struct StoredFrame {
    /// The publish cursor returned by the ring; doubles as the in-process
    /// frame identity for pinning since cursors are never reused.
    cursor: u64,
    desc: VideoFrameDesc,
    segment: Arc<SharedMemorySegment>,
    pool_id: Option<u64>,
    slot_index: Option<usize>,
    pool_registration: Option<CpuPoolRegistration>,
    pinned_by: BTreeSet<ConsumerId>,
}

#[derive(Debug)]
struct TrackRingControl {
    page: VideoTrackControlPage,
    consumer_slots: BTreeMap<ConsumerId, usize>,
}

impl TrackRingControl {
    fn new(capacity: usize) -> Self {
        Self {
            page: VideoTrackControlPage::new(capacity),
            consumer_slots: BTreeMap::new(),
        }
    }

    fn push(&mut self, entry: PendingVideoRingEntry) -> u64 {
        self.page.push(entry)
    }

    fn latest(&self) -> std::result::Result<Option<VideoRingEntry>, VideoRingReadError> {
        self.page.read_latest_lossy_entry()
    }

    fn entry_for_cursor(&self, cursor: u64) -> std::result::Result<VideoRingEntry, VideoRingReadError> {
        self.page.read_entry_for_cursor(cursor)
    }

    fn ring_snapshot(&self) -> Vec<VideoRingEntry> {
        self.page.ring_snapshot()
    }

    fn retained_cursor_bounds(&self) -> Option<(u64, u64)> {
        let entries = self.ring_snapshot();
        let oldest = entries.first()?.cursor;
        let latest = entries.last()?.cursor;
        Some((oldest, latest))
    }

    fn snapshot(&self) -> TrackRingControlSnapshot {
        let page = self.page.snapshot();
        TrackRingControlSnapshot {
            capacity: page.header.slot_capacity as usize,
            producer_cursor: page.header.producer_cursor,
            latest_sequence: page.entries.last().map(|entry| entry.sequence),
            entries: page.entries,
        }
    }

    fn control_page_snapshot(&self) -> VideoTrackControlSnapshot {
        self.page.snapshot()
    }

    fn control_page_registration(&mut self, track_id: TrackId, consumer_id: ConsumerId) -> Result<VideoControlPageRegistration> {
        let consumer_slot = self.register_consumer(consumer_id)? as u64;
        Ok(VideoControlPageRegistration {
            track_id,
            consumer_id,
            consumer_slot,
            map_len: self.page.mapped_len() as u64,
            fd: self.page.try_clone_fd()?,
        })
    }

    fn register_consumer(&mut self, consumer_id: ConsumerId) -> Result<usize> {
        if let Some(slot) = self.consumer_slots.get(&consumer_id).copied() {
            return Ok(slot);
        }
        let slot = self.page.register_consumer_cursor(consumer_id.get())?;
        self.consumer_slots.insert(consumer_id, slot);
        Ok(slot)
    }

    fn store_consumer_acquire(&mut self, consumer_id: ConsumerId, slot: usize, snapshot: &ConsumerRingCursorSnapshot) -> Result<()> {
        self.page.store_consumer_acquire_cursor(
            slot,
            consumer_id.get(),
            snapshot
                .last_acquired_cursor
                .expect("consumer acquire snapshot has a cursor after acquire"),
            snapshot
                .last_acquired_sequence
                .expect("consumer acquire snapshot has a sequence after acquire"),
            snapshot.skipped_count,
            snapshot.acquired_count,
        )
    }

    fn store_consumer_release(&mut self, consumer_id: ConsumerId, release_cursor: u64) -> Result<()> {
        let slot = self.register_consumer(consumer_id)?;
        self.page.store_consumer_release_cursor(slot, consumer_id.get(), release_cursor)
    }

    fn unregister_consumer(&mut self, consumer_id: ConsumerId) {
        self.consumer_slots.remove(&consumer_id);
        self.page.unregister_consumer_cursor(consumer_id.get());
    }
}

#[derive(Debug, Clone, Default)]
struct ConsumerRingCursor {
    last_acquired_cursor: Option<u64>,
    last_acquired_sequence: Option<u64>,
    // Tracked for the future shared control page; frame pinning still gates
    // actual slot reuse in this in-process prototype.
    release_cursor: u64,
    skipped_count: u64,
    acquired_count: u64,
}

impl ConsumerRingCursor {
    fn acquire(&mut self, entry: &VideoRingEntry) {
        if let Some(previous) = self.last_acquired_cursor
            && entry.cursor > previous.saturating_add(1)
        {
            self.skipped_count = self.skipped_count.saturating_add(entry.cursor - previous - 1);
        }
        self.last_acquired_cursor = Some(entry.cursor);
        self.last_acquired_sequence = Some(entry.sequence);
        self.acquired_count = self.acquired_count.saturating_add(1);
    }

    fn release(&mut self, producer_cursor: u64) {
        self.release_cursor = self.release_cursor.max(producer_cursor);
    }

    fn snapshot(&self) -> ConsumerRingCursorSnapshot {
        ConsumerRingCursorSnapshot {
            last_acquired_cursor: self.last_acquired_cursor,
            last_acquired_sequence: self.last_acquired_sequence,
            release_cursor: self.release_cursor,
            skipped_count: self.skipped_count,
            acquired_count: self.acquired_count,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRingControlSnapshot {
    pub capacity: usize,
    pub producer_cursor: u64,
    pub latest_sequence: Option<u64>,
    pub entries: Vec<VideoRingEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerRingCursorSnapshot {
    pub last_acquired_cursor: Option<u64>,
    pub last_acquired_sequence: Option<u64>,
    pub release_cursor: u64,
    pub skipped_count: u64,
    pub acquired_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoStorageMode {
    ImmutablePerFrame,
    ReusablePool,
}

#[derive(Debug)]
pub enum OrderedVideoAcquire {
    Frame(AcquiredVideoFrame),
    Lapped {
        after_producer_cursor: u64,
        oldest_available_cursor: u64,
        latest_available_cursor: u64,
        skipped_count: u64,
    },
    Empty,
}

#[derive(Debug)]
struct TrackPool {
    pool_id: u64,
    segment: Arc<SharedMemorySegment>,
    slot_stride: usize,
    slot_count: usize,
    next_slot: usize,
}

#[derive(Debug)]
struct PublishedPayload {
    desc: VideoFrameDesc,
    segment: Arc<SharedMemorySegment>,
    pool_id: Option<u64>,
    slot_index: Option<usize>,
    pool_registration: Option<CpuPoolRegistration>,
}

#[derive(Debug)]
pub struct VideoSlotManager {
    capacity_per_track: usize,
    storage_mode: VideoStorageMode,
    next_pool_id: u64,
    frames_by_track: BTreeMap<TrackId, Vec<StoredFrame>>,
    controls_by_track: BTreeMap<TrackId, TrackRingControl>,
    pools_by_track: BTreeMap<TrackId, TrackPool>,
    pending_claims_by_track: BTreeMap<TrackId, BTreeSet<(u64, usize)>>,
    evicted_by_track: BTreeMap<TrackId, u64>,
    consumer_cursors: BTreeMap<(ConsumerId, TrackId), ConsumerRingCursor>,
}

impl VideoSlotManager {
    #[must_use]
    pub fn new(capacity_per_track: usize) -> Self {
        Self {
            capacity_per_track: capacity_per_track.max(1),
            storage_mode: VideoStorageMode::ImmutablePerFrame,
            next_pool_id: 1,
            frames_by_track: BTreeMap::new(),
            controls_by_track: BTreeMap::new(),
            pools_by_track: BTreeMap::new(),
            pending_claims_by_track: BTreeMap::new(),
            evicted_by_track: BTreeMap::new(),
            consumer_cursors: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn new_reusable_pool(capacity_per_track: usize) -> Self {
        let mut manager = Self::new(capacity_per_track);
        manager.storage_mode = VideoStorageMode::ReusablePool;
        manager
    }

    pub fn publish(&mut self, track_id: TrackId, desc: VideoFrameDesc, pixels: &[u8]) -> Result<()> {
        let payload = match self.storage_mode {
            VideoStorageMode::ImmutablePerFrame => self.publish_immutable_frame(desc, pixels)?,
            VideoStorageMode::ReusablePool => {
                let mut claim = self.claim_video_slot(track_id, desc, pixels.len())?;
                claim.copy_from_slice(pixels);
                self.commit_claimed_slot(claim)?
            }
        };
        self.store_published_payload(track_id, payload);
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
            .insert((pool.pool_id, slot_index));

        desc.payload_offset = offset as u64;
        desc.payload_len = len as u64;
        desc.payload_map_len = pool.segment.len() as u64;
        desc.pool_id = pool.pool_id;
        desc.slot_id = slot_index as u32;

        Ok(ClaimedVideoSlot {
            desc,
            track_id,
            segment: Arc::clone(&pool.segment),
            slot_index,
            slot_stride: pool.slot_stride,
            slot_count: pool.slot_count,
        })
    }

    pub fn commit_video_slot(&mut self, claim: ClaimedVideoSlot) -> Result<()> {
        let track_id = claim.track_id;
        let payload = self.commit_claimed_slot(claim)?;
        self.store_published_payload(track_id, payload);
        Ok(())
    }

    fn commit_claimed_slot(&mut self, claim: ClaimedVideoSlot) -> Result<PublishedPayload> {
        let pool_id = claim.desc.pool_id;
        if let Some(pending_claims) = self.pending_claims_by_track.get_mut(&claim.track_id) {
            pending_claims.remove(&(pool_id, claim.slot_index));
        }
        if let Some(frames) = self.frames_by_track.get_mut(&claim.track_id) {
            frames.retain(|frame| {
                frame.pool_id != Some(pool_id) || frame.slot_index != Some(claim.slot_index) || !frame.pinned_by.is_empty()
            });
        }
        let pool_registration = CpuPoolRegistration {
            track_id: claim.track_id,
            pool_id,
            payload_map_len: claim.desc.payload_map_len,
            slot_stride: claim.slot_stride as u64,
            slot_count: claim.slot_count as u64,
        };
        Ok(PublishedPayload {
            desc: claim.desc,
            pool_registration: Some(pool_registration),
            segment: claim.segment,
            pool_id: Some(pool_id),
            slot_index: Some(claim.slot_index),
        })
    }

    fn store_published_payload(&mut self, track_id: TrackId, payload: PublishedPayload) {
        let ring_entry = PendingVideoRingEntry {
            sequence: payload.desc.sequence,
            timestamp_ns: payload.desc.timestamp_ns,
            width: payload.desc.width,
            height: payload.desc.height,
            stride: payload.desc.stride,
            pixel_format: payload.desc.pixel_format as u32,
            pool_id: payload.desc.pool_id,
            slot_id: payload.desc.slot_id,
            payload_offset: payload.desc.payload_offset,
            payload_len: payload.desc.payload_len,
            clock_domain: payload.desc.clock_domain as u32,
            color_space: payload.desc.color_space as u32,
            sync_kind: payload.desc.sync_kind as u32,
            damage_kind: payload.desc.damage_kind as u32,
            damage_base_sequence: payload.desc.damage_base_sequence,
            dropped_before_publish: payload.desc.dropped_before_publish,
            producer_drop_count: payload.desc.producer_drop_count,
            payload_kind: payload.desc.payload_kind as u32,
            modifier: payload.desc.modifier,
            fence_id: payload.desc.fence_id,
            fence_value: payload.desc.fence_value,
            flags: payload.desc.flags,
        };
        let cursor = self
            .controls_by_track
            .entry(track_id)
            .or_insert_with(|| TrackRingControl::new(self.capacity_per_track))
            .push(ring_entry);
        let frames = self.frames_by_track.entry(track_id).or_default();
        frames.push(StoredFrame {
            cursor,
            desc: payload.desc,
            segment: payload.segment,
            pool_id: payload.pool_id,
            slot_index: payload.slot_index,
            pool_registration: payload.pool_registration,
            pinned_by: BTreeSet::new(),
        });
        let evicted = Self::prune_unpinned(frames, self.capacity_per_track);
        *self.evicted_by_track.entry(track_id).or_default() += evicted;
    }

    pub fn acquire_latest(&mut self, consumer_id: ConsumerId, track_id: TrackId) -> Result<AcquiredVideoFrame> {
        let control = self
            .controls_by_track
            .get(&track_id)
            .ok_or(CaptureTransferError::UnknownTrack { track_id })?;
        let entry = control
            .latest()
            .map_err(|source| CaptureTransferError::VideoControlRingRead { track_id, source })?
            .ok_or(CaptureTransferError::UnknownTrack { track_id })?;
        self.acquire_ring_entry(consumer_id, track_id, entry)
    }

    pub fn acquire_cursor(&mut self, consumer_id: ConsumerId, track_id: TrackId, producer_cursor: u64) -> Result<AcquiredVideoFrame> {
        let control = self
            .controls_by_track
            .get(&track_id)
            .ok_or(CaptureTransferError::UnknownTrack { track_id })?;
        let entry = control
            .entry_for_cursor(producer_cursor)
            .map_err(|source| CaptureTransferError::VideoControlRingRead { track_id, source })?;
        self.acquire_ring_entry(consumer_id, track_id, entry)
    }

    pub fn acquire_next_after(
        &mut self,
        consumer_id: ConsumerId,
        track_id: TrackId,
        after_producer_cursor: u64,
    ) -> Result<OrderedVideoAcquire> {
        let control = self
            .controls_by_track
            .get(&track_id)
            .ok_or(CaptureTransferError::UnknownTrack { track_id })?;
        let Some((oldest_available_cursor, latest_available_cursor)) = control.retained_cursor_bounds() else {
            return Ok(OrderedVideoAcquire::Empty);
        };
        let target_cursor = if after_producer_cursor == 0 {
            oldest_available_cursor
        } else {
            after_producer_cursor.saturating_add(1)
        };

        if target_cursor < oldest_available_cursor {
            return Ok(OrderedVideoAcquire::Lapped {
                after_producer_cursor,
                oldest_available_cursor,
                latest_available_cursor,
                skipped_count: oldest_available_cursor.saturating_sub(target_cursor),
            });
        }
        if target_cursor > latest_available_cursor {
            return Ok(OrderedVideoAcquire::Empty);
        }

        let entry = control
            .entry_for_cursor(target_cursor)
            .map_err(|source| CaptureTransferError::VideoControlRingRead { track_id, source })?;
        self.acquire_ring_entry(consumer_id, track_id, entry)
            .map(OrderedVideoAcquire::Frame)
    }

    fn acquire_ring_entry(&mut self, consumer_id: ConsumerId, track_id: TrackId, entry: VideoRingEntry) -> Result<AcquiredVideoFrame> {
        let consumer_slot = self
            .controls_by_track
            .get_mut(&track_id)
            .ok_or(CaptureTransferError::UnknownTrack { track_id })?
            .register_consumer(consumer_id)?;
        let (desc, segment, pool_registration) = {
            let frame = self
                .frames_by_track
                .get_mut(&track_id)
                .and_then(|frames| frames.iter_mut().find(|frame| frame.cursor == entry.cursor))
                .ok_or(CaptureTransferError::UnknownTrack { track_id })?;
            frame.pinned_by.insert(consumer_id);
            (frame.desc.clone(), Arc::clone(&frame.segment), frame.pool_registration.clone())
        };
        let consumer_key = (consumer_id, track_id);
        let cursor = self.consumer_cursors.entry(consumer_key).or_default();
        cursor.acquire(&entry);
        let cursor_snapshot = cursor.snapshot();
        if let Some(control) = self.controls_by_track.get_mut(&track_id) {
            if let Err(error) = control.store_consumer_acquire(consumer_id, consumer_slot, &cursor_snapshot) {
                if let Some(frames) = self.frames_by_track.get_mut(&track_id)
                    && let Some(stored) = frames.iter_mut().find(|stored| stored.cursor == entry.cursor)
                {
                    stored.pinned_by.remove(&consumer_id);
                }
                return Err(error);
            }
        }
        let mut desc = desc;
        desc.evicted_count = self.evicted_by_track.get(&track_id).copied().unwrap_or(0);
        desc.consumer_skipped_count = cursor.skipped_count;
        Ok(AcquiredVideoFrame {
            desc,
            consumer_id,
            track_id,
            producer_cursor: entry.cursor,
            segment,
            pool_registration,
        })
    }

    pub fn release(&mut self, frame: AcquiredVideoFrame) {
        let mut release_cursor = None;
        if let Some(cursor) = self.consumer_cursors.get_mut(&(frame.consumer_id, frame.track_id)) {
            cursor.release(frame.producer_cursor);
            release_cursor = Some(cursor.release_cursor);
        }
        if let Some(release_cursor) = release_cursor
            && let Some(control) = self.controls_by_track.get_mut(&frame.track_id)
        {
            let _ = control.store_consumer_release(frame.consumer_id, release_cursor);
        }
        if let Some(frames) = self.frames_by_track.get_mut(&frame.track_id) {
            if let Some(stored) = frames.iter_mut().find(|stored| stored.cursor == frame.producer_cursor) {
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
            if let Some(control) = self.controls_by_track.get_mut(track_id) {
                control.unregister_consumer(consumer_id);
            }
        }
        self.consumer_cursors
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
    pub fn debug_ring_snapshot(&self, track_id: TrackId) -> Vec<VideoRingEntry> {
        self.controls_by_track
            .get(&track_id)
            .map_or_else(Vec::new, TrackRingControl::ring_snapshot)
    }

    #[must_use]
    pub fn debug_control_snapshot(&self, track_id: TrackId) -> Option<TrackRingControlSnapshot> {
        self.controls_by_track.get(&track_id).map(TrackRingControl::snapshot)
    }

    #[must_use]
    pub fn debug_control_page_snapshot(&self, track_id: TrackId) -> Option<VideoTrackControlSnapshot> {
        self.controls_by_track.get(&track_id).map(TrackRingControl::control_page_snapshot)
    }

    pub fn control_page_registration(&mut self, track_id: TrackId, consumer_id: ConsumerId) -> Result<VideoControlPageRegistration> {
        self.controls_by_track
            .get_mut(&track_id)
            .ok_or(CaptureTransferError::UnknownTrack { track_id })?
            .control_page_registration(track_id, consumer_id)
    }

    #[must_use]
    pub fn debug_consumer_cursor_snapshot(&self, consumer_id: ConsumerId, track_id: TrackId) -> Option<ConsumerRingCursorSnapshot> {
        self.consumer_cursors
            .get(&(consumer_id, track_id))
            .map(ConsumerRingCursor::snapshot)
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
        desc.payload_offset = 0;
        desc.payload_len = pixels.len() as u64;
        desc.payload_map_len = pixels.len() as u64;
        Ok(PublishedPayload {
            desc,
            segment: Arc::new(segment),
            pool_id: None,
            slot_index: None,
            pool_registration: None,
        })
    }

    fn replace_pool(&mut self, track_id: TrackId, slot_stride: usize) -> Result<()> {
        let pool_id = self.next_pool_id;
        self.next_pool_id += 1;
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
                segment: Arc::new(segment),
                slot_stride,
                slot_count: self.capacity_per_track,
                next_slot: 0,
            },
        );
        // Outstanding claims for the old pool still own their mmap Arc, but
        // clearing the pending set makes later commits unpublishable no-ops.
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
                frames
                    .iter()
                    .any(|frame| frame.pool_id == Some(pool.pool_id) && frame.slot_index == Some(slot_index) && !frame.pinned_by.is_empty())
            });
            let claimed = pending_claims.is_some_and(|claims| claims.contains(&(pool.pool_id, slot_index)));
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
        CaptureTransferError,
        control_page::VideoRingReadError,
        model::{ClockDomain, ColorSpace, DamageKind, FrameSyncKind, PayloadKind, PixelFormat, TrackId},
        shm::SharedMemorySegment,
        video::{ConsumerId, OrderedVideoAcquire, VideoFrameDesc, VideoSlotManager},
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
            payload_kind: PayloadKind::CpuShm,
            modifier: 0,
            fence_id: 0,
            fence_value: 0,
            flags: 0,
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
    fn acquire_cursor_returns_requested_live_frame() {
        let mut slots = VideoSlotManager::new(3);
        let track = TrackId::new(1);
        let consumer = ConsumerId::new(7);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        slots.publish(track, frame_desc(2), &[2]).unwrap();
        slots.publish(track, frame_desc(3), &[3]).unwrap();

        let frame = slots.acquire_cursor(consumer, track, 2).unwrap();
        assert_eq!(frame.desc.sequence, 2);
        assert_eq!(frame.producer_cursor(), 2);
        assert_eq!(frame.bytes(), &[2]);
        assert_eq!(slots.pinned_frame_count(), 1);
        let cursor = slots.debug_consumer_cursor_snapshot(consumer, track).unwrap();
        assert_eq!(cursor.last_acquired_cursor, Some(2));
        assert_eq!(cursor.last_acquired_sequence, Some(2));
    }

    #[test]
    fn acquire_cursor_reports_lapped_cursor_after_wraparound() {
        let mut slots = VideoSlotManager::new(2);
        let track = TrackId::new(1);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        slots.publish(track, frame_desc(2), &[2]).unwrap();
        slots.publish(track, frame_desc(3), &[3]).unwrap();

        let error = slots.acquire_cursor(ConsumerId::new(7), track, 1).unwrap_err();
        assert!(matches!(
            error,
            CaptureTransferError::VideoControlRingRead {
                source: VideoRingReadError::Lapped { .. },
                ..
            }
        ));
    }

    #[test]
    fn acquire_next_after_zero_returns_oldest_retained_frame() {
        let mut slots = VideoSlotManager::new_reusable_pool(3);
        let track = TrackId::new(1);
        let consumer = ConsumerId::new(7);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        slots.publish(track, frame_desc(2), &[2]).unwrap();

        let frame = match slots.acquire_next_after(consumer, track, 0).unwrap() {
            OrderedVideoAcquire::Frame(frame) => frame,
            other => panic!("expected frame, got {other:?}"),
        };

        assert_eq!(frame.producer_cursor(), 1);
        assert_eq!(frame.desc.sequence, 1);
        assert_eq!(frame.bytes(), &[1]);
        slots.release(frame);
    }

    #[test]
    fn acquire_next_after_retained_cursor_returns_next_frame() {
        let mut slots = VideoSlotManager::new_reusable_pool(3);
        let track = TrackId::new(1);
        let consumer = ConsumerId::new(7);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        slots.publish(track, frame_desc(2), &[2]).unwrap();
        slots.publish(track, frame_desc(3), &[3]).unwrap();

        let frame = match slots.acquire_next_after(consumer, track, 1).unwrap() {
            OrderedVideoAcquire::Frame(frame) => frame,
            other => panic!("expected frame, got {other:?}"),
        };

        assert_eq!(frame.producer_cursor(), 2);
        assert_eq!(frame.desc.sequence, 2);
        assert_eq!(frame.bytes(), &[2]);
        slots.release(frame);
    }

    #[test]
    fn acquire_next_after_latest_cursor_returns_empty() {
        let mut slots = VideoSlotManager::new_reusable_pool(3);
        let track = TrackId::new(1);

        slots.publish(track, frame_desc(1), &[1]).unwrap();

        let result = slots.acquire_next_after(ConsumerId::new(7), track, 1).unwrap();

        assert!(matches!(result, OrderedVideoAcquire::Empty));
    }

    #[test]
    fn acquire_next_after_reports_lapped_cursor() {
        let mut slots = VideoSlotManager::new_reusable_pool(2);
        let track = TrackId::new(1);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        slots.publish(track, frame_desc(2), &[2]).unwrap();
        slots.publish(track, frame_desc(3), &[3]).unwrap();
        slots.publish(track, frame_desc(4), &[4]).unwrap();

        let result = slots.acquire_next_after(ConsumerId::new(7), track, 1).unwrap();

        assert!(matches!(
            result,
            OrderedVideoAcquire::Lapped {
                after_producer_cursor: 1,
                oldest_available_cursor: 3,
                latest_available_cursor: 4,
                skipped_count: 1,
            }
        ));
    }

    #[test]
    fn acquire_next_after_does_not_change_latest_semantics() {
        let mut slots = VideoSlotManager::new_reusable_pool(2);
        let track = TrackId::new(1);
        let consumer = ConsumerId::new(7);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        slots.publish(track, frame_desc(2), &[2]).unwrap();

        let latest = slots.acquire_latest(consumer, track).unwrap();

        assert_eq!(latest.producer_cursor(), 2);
        assert_eq!(latest.desc.sequence, 2);
        slots.release(latest);
    }

    #[test]
    fn acquire_next_after_release_updates_consumer_release_cursor() {
        let mut slots = VideoSlotManager::new_reusable_pool(2);
        let track = TrackId::new(1);
        let consumer = ConsumerId::new(7);

        slots.publish(track, frame_desc(1), &[1]).unwrap();

        let frame = match slots.acquire_next_after(consumer, track, 0).unwrap() {
            OrderedVideoAcquire::Frame(frame) => frame,
            other => panic!("expected frame, got {other:?}"),
        };
        slots.release(frame);

        let cursor = slots.debug_consumer_cursor_snapshot(consumer, track).unwrap();
        assert_eq!(cursor.release_cursor, 1);
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
        slots.release(first);

        slots.publish(track, frame_desc(2), &[5, 6, 7, 8]).unwrap();
        let second = slots.acquire_latest(consumer, track).unwrap();
        assert_ne!(second.desc.pool_id, 0);
        assert_eq!(second.desc.slot_id, 1);
    }

    #[test]
    fn reusable_pool_acquired_frames_expose_pool_registration_metadata() {
        let mut slots = VideoSlotManager::new_reusable_pool(3);
        let track = TrackId::new(1);
        let consumer = ConsumerId::new(7);

        slots.publish(track, frame_desc(1), &[1, 2, 3, 4]).unwrap();
        let first = slots.acquire_latest(consumer, track).unwrap();
        let first_pool = first.cpu_pool_registration().unwrap();
        assert_eq!(first_pool.track_id, track);
        assert_eq!(first_pool.pool_id, first.desc.pool_id);
        assert_eq!(first_pool.payload_map_len, first.desc.payload_map_len);
        assert_eq!(first_pool.slot_stride, 64);
        assert_eq!(first_pool.slot_count, 3);
        slots.release(first);

        slots.publish(track, frame_desc(2), &[5, 6, 7, 8]).unwrap();
        let second = slots.acquire_latest(consumer, track).unwrap();
        let second_pool = second.cpu_pool_registration().unwrap();
        assert_eq!(second_pool, first_pool);
        assert_ne!(second.desc.payload_offset, 0);
        assert_eq!(second.desc.payload_offset, first_pool.slot_stride);
    }

    #[test]
    fn immutable_acquired_frames_do_not_expose_pool_registration_metadata() {
        let mut slots = VideoSlotManager::new(3);
        let track = TrackId::new(1);
        let consumer = ConsumerId::new(7);

        slots.publish(track, frame_desc(1), &[1, 2, 3, 4]).unwrap();
        let frame = slots.acquire_latest(consumer, track).unwrap();

        assert_eq!(frame.cpu_pool_registration(), None);
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
    fn ring_control_snapshot_tracks_producer_cursor_and_latest_sequence() {
        let mut slots = VideoSlotManager::new_reusable_pool(2);
        let track = TrackId::new(1);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        slots.publish(track, frame_desc(2), &[2]).unwrap();
        slots.publish(track, frame_desc(3), &[3]).unwrap();

        let control = slots.debug_control_snapshot(track).unwrap();
        assert_eq!(control.capacity, 2);
        assert_eq!(control.producer_cursor, 3);
        assert_eq!(control.latest_sequence, Some(3));
        assert_eq!(
            control
                .entries
                .iter()
                .map(|entry| (entry.cursor, entry.sequence))
                .collect::<Vec<_>>(),
            vec![(2, 2), (3, 3)]
        );
    }

    #[test]
    fn control_page_snapshot_tracks_ring_header_after_wraparound() {
        let mut slots = VideoSlotManager::new_reusable_pool(2);
        let track = TrackId::new(1);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        slots.publish(track, frame_desc(2), &[2]).unwrap();
        slots.publish(track, frame_desc(3), &[3]).unwrap();

        let page = slots.debug_control_page_snapshot(track).unwrap();
        assert_eq!(page.header.slot_capacity, 2);
        assert_eq!(page.header.producer_cursor, 3);
        assert_eq!(
            page.entries.iter().map(|entry| (entry.cursor, entry.sequence)).collect::<Vec<_>>(),
            vec![(2, 2), (3, 3)]
        );
    }

    #[test]
    fn publishing_frame_writes_full_descriptor_to_control_page() {
        let mut slots = VideoSlotManager::new(2);
        let track = TrackId::new(1);
        let mut desc = frame_desc(9);
        desc.timestamp_ns = 987_654_321;
        desc.width = 640;
        desc.height = 480;
        desc.stride = 2_560;
        desc.pixel_format = PixelFormat::Rgba8Unorm;
        desc.clock_domain = ClockDomain::HostTime;
        desc.color_space = ColorSpace::Srgb;
        desc.sync_kind = FrameSyncKind::SckSampleReady;
        desc.damage_kind = DamageKind::InlineRects;
        desc.damage_base_sequence = 7;
        desc.dropped_before_publish = 3;
        desc.producer_drop_count = 4;

        slots.publish(track, desc, &[1, 2, 3, 4]).unwrap();

        let page = slots.debug_control_page_snapshot(track).unwrap();
        let entry = page.entries.last().unwrap();
        assert_eq!(entry.sequence, 9);
        assert_eq!(entry.timestamp_ns, 987_654_321);
        assert_eq!(entry.width, 640);
        assert_eq!(entry.height, 480);
        assert_eq!(entry.stride, 2_560);
        assert_eq!(entry.pixel_format, PixelFormat::Rgba8Unorm as u32);
        assert_eq!(entry.clock_domain, ClockDomain::HostTime as u32);
        assert_eq!(entry.color_space, ColorSpace::Srgb as u32);
        assert_eq!(entry.sync_kind, FrameSyncKind::SckSampleReady as u32);
        assert_eq!(entry.damage_kind, DamageKind::InlineRects as u32);
        assert_eq!(entry.damage_base_sequence, 7);
        assert_eq!(entry.dropped_before_publish, 3);
        assert_eq!(entry.producer_drop_count, 4);
    }

    #[test]
    fn slow_consumer_skip_count_uses_ring_cursor_after_wraparound() {
        let mut slots = VideoSlotManager::new_reusable_pool(2);
        let track = TrackId::new(1);
        let consumer = ConsumerId::new(7);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        let first = slots.acquire_latest(consumer, track).unwrap();
        assert_eq!(first.desc.consumer_skipped_count, 0);
        slots.release(first);

        slots.publish(track, frame_desc(2), &[2]).unwrap();
        slots.publish(track, frame_desc(3), &[3]).unwrap();
        slots.publish(track, frame_desc(4), &[4]).unwrap();

        let latest = slots.acquire_latest(consumer, track).unwrap();
        assert_eq!(latest.desc.sequence, 4);
        assert_eq!(latest.desc.consumer_skipped_count, 2);

        let cursor = slots.debug_consumer_cursor_snapshot(consumer, track).unwrap();
        assert_eq!(cursor.last_acquired_cursor, Some(4));
        assert_eq!(cursor.skipped_count, 2);
        assert_eq!(cursor.acquired_count, 2);
    }

    #[test]
    fn releasing_frame_advances_consumer_release_cursor() {
        let mut slots = VideoSlotManager::new_reusable_pool(2);
        let track = TrackId::new(1);
        let consumer = ConsumerId::new(7);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        let frame = slots.acquire_latest(consumer, track).unwrap();

        let cursor = slots.debug_consumer_cursor_snapshot(consumer, track).unwrap();
        assert_eq!(cursor.last_acquired_cursor, Some(1));
        assert_eq!(cursor.release_cursor, 0);

        slots.release(frame);

        let cursor = slots.debug_consumer_cursor_snapshot(consumer, track).unwrap();
        assert_eq!(cursor.last_acquired_cursor, Some(1));
        assert_eq!(cursor.release_cursor, 1);
    }

    #[test]
    fn consumer_cursor_control_page_entry_tracks_acquire_and_release() {
        let mut slots = VideoSlotManager::new_reusable_pool(2);
        let track = TrackId::new(1);
        let consumer = ConsumerId::new(7);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        let frame = slots.acquire_latest(consumer, track).unwrap();

        let page = slots.debug_control_page_snapshot(track).unwrap();
        let entry = page
            .consumer_entries
            .iter()
            .find(|entry| entry.consumer_id == consumer.get())
            .unwrap();
        assert_eq!(entry.last_acquired_cursor, 1);
        assert_eq!(entry.last_acquired_sequence, 1);
        assert_eq!(entry.acquired_count, 1);
        assert_eq!(entry.release_cursor, 0);

        slots.release(frame);

        let page = slots.debug_control_page_snapshot(track).unwrap();
        let entry = page
            .consumer_entries
            .iter()
            .find(|entry| entry.consumer_id == consumer.get())
            .unwrap();
        assert_eq!(entry.release_cursor, 1);
    }

    #[test]
    fn disconnecting_consumer_clears_control_page_cursor_slot() {
        let mut slots = VideoSlotManager::new_reusable_pool(2);
        let track = TrackId::new(1);
        let consumer = ConsumerId::new(7);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        let _frame = slots.acquire_latest(consumer, track).unwrap();
        assert!(
            slots
                .debug_control_page_snapshot(track)
                .unwrap()
                .consumer_entries
                .iter()
                .any(|entry| entry.consumer_id == consumer.get())
        );

        slots.disconnect_consumer(consumer);

        assert!(
            slots
                .debug_control_page_snapshot(track)
                .unwrap()
                .consumer_entries
                .iter()
                .all(|entry| entry.consumer_id != consumer.get())
        );
    }

    #[test]
    fn consumers_track_independent_ring_cursors_on_same_track() {
        let mut slots = VideoSlotManager::new_reusable_pool(2);
        let track = TrackId::new(1);
        let fast = ConsumerId::new(7);
        let slow = ConsumerId::new(8);

        slots.publish(track, frame_desc(1), &[1]).unwrap();
        let fast_first = slots.acquire_latest(fast, track).unwrap();
        let slow_first = slots.acquire_latest(slow, track).unwrap();
        slots.release(fast_first);

        slots.publish(track, frame_desc(2), &[2]).unwrap();
        let fast_second = slots.acquire_latest(fast, track).unwrap();
        slots.release(fast_second);

        slots.publish(track, frame_desc(3), &[3]).unwrap();
        slots.publish(track, frame_desc(4), &[4]).unwrap();

        let fast_latest = slots.acquire_latest(fast, track).unwrap();
        let slow_latest = slots.acquire_latest(slow, track).unwrap();

        assert_eq!(fast_latest.desc.consumer_skipped_count, 1);
        assert_eq!(slow_latest.desc.consumer_skipped_count, 2);

        slots.release(fast_latest);
        slots.release(slow_first);

        let fast_cursor = slots.debug_consumer_cursor_snapshot(fast, track).unwrap();
        assert_eq!(fast_cursor.last_acquired_cursor, Some(4));
        assert_eq!(fast_cursor.release_cursor, 4);
        assert_eq!(fast_cursor.skipped_count, 1);
        assert_eq!(fast_cursor.acquired_count, 3);

        let slow_cursor = slots.debug_consumer_cursor_snapshot(slow, track).unwrap();
        assert_eq!(slow_cursor.last_acquired_cursor, Some(4));
        assert_eq!(slow_cursor.release_cursor, 1);
        assert_eq!(slow_cursor.skipped_count, 2);
        assert_eq!(slow_cursor.acquired_count, 2);
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

    // Anonymous shm objects are mmap-only on macOS (read()/seek() fail with
    // ENXIO), so cloned fds are exercised the way consumers use them: mapped.

    #[test]
    fn acquired_frame_can_clone_mappable_fd() {
        let mut slots = VideoSlotManager::new(2);
        let track = TrackId::new(1);

        slots.publish(track, frame_desc(1), &[9, 8, 7]).unwrap();
        let frame = slots.acquire_latest(ConsumerId::new(7), track).unwrap();

        let mapping = SharedMemorySegment::map_read_only(frame.try_clone_fd().unwrap(), frame.desc.payload_map_len as usize).unwrap();

        assert_eq!(
            mapping.slice_at(frame.desc.payload_offset as usize, frame.desc.payload_len as usize),
            &[9, 8, 7]
        );
    }

    #[test]
    fn cloned_fd_survives_release_and_prune() {
        let mut slots = VideoSlotManager::new(1);
        let track = TrackId::new(1);

        slots.publish(track, frame_desc(1), &[9, 8, 7]).unwrap();
        let frame = slots.acquire_latest(ConsumerId::new(7), track).unwrap();
        let fd = frame.try_clone_fd().unwrap();
        let map_len = frame.desc.payload_map_len as usize;
        let payload_offset = frame.desc.payload_offset as usize;
        let payload_len = frame.desc.payload_len as usize;
        slots.release(frame);

        slots.publish(track, frame_desc(2), &[1, 2, 3]).unwrap();
        assert_eq!(slots.pinned_frame_count(), 0);

        let mapping = SharedMemorySegment::map_read_only(fd, map_len).unwrap();
        assert_eq!(mapping.slice_at(payload_offset, payload_len), &[9, 8, 7]);
    }
}
