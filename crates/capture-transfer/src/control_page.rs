use std::{
    os::fd::OwnedFd,
    sync::atomic::{AtomicU64, Ordering, fence},
};

use thiserror::Error;

use crate::{
    error::{CaptureTransferError, Result as CaptureResult},
    shm::SharedMemorySegment,
};

// The wire layout implemented here is specified by include/jackstay_ring.h.
// Every struct below mirrors that header exactly; the const block after the
// struct definitions static-asserts each documented offset and size.
//
// Records are one 128-byte cacheline each (Apple silicon line size) so no two
// records false-share; the header's hot cursors live alone on the second line.

pub const CONTROL_PAGE_ALIGNMENT: usize = 128;
pub const VIDEO_TRACK_CONTROL_MAGIC: u64 = u64::from_le_bytes(*b"JSFRING1");
pub const VIDEO_TRACK_CONTROL_VERSION: u32 = 3;
pub const CONFIG_RING_CAPACITY: usize = 4;
// config_index_mask derives the ring mask as CONFIG_RING_CAPACITY - 1, which is
// only a valid index mask when the capacity is a power of two.
const _: () = assert!(CONFIG_RING_CAPACITY.is_power_of_two());
const HEADER_LEN: usize = 256;
const SEQLOCK_WORD_LEN: usize = std::mem::size_of::<u64>();

/// jackstay_ring_header. Line 0 is written once at creation and read-only
/// afterwards; line 1 holds the only mutable cross-process words.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoTrackControlHeader {
    pub magic: u64,
    pub layout_version: u32,
    pub header_len: u32,
    pub slot_len: u32,
    pub slot_capacity: u32,
    pub config_len: u32,
    pub config_capacity: u32,
    pub consumer_len: u32,
    pub consumer_capacity: u32,
    pub slots_offset: u32,
    pub configs_offset: u32,
    pub consumers_offset: u32,
    pub reserved0: [u8; 76],
    /// Count of published frames; 0 = empty. Ring occupancy derives from it:
    /// len = min(cursor, slot_capacity), latest index = (cursor-1) & mask,
    /// oldest live cursor = cursor - len + 1.
    pub producer_cursor: u64,
    /// Latest stream config generation; 0 = none published yet.
    pub config_cursor: u64,
    pub reserved1: [u8; 112],
}

/// jackstay_frame_slot: one published frame. The payload is not here; cpu-shm
/// payloads live in a pool segment attached via the setup channel, native
/// payloads are the pool slot itself (an IOSurface/dmabuf registered via the
/// setup channel) and payload_offset/len are zero.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameSlot {
    /// Seqlock word: 0 while mid-write, else the producer cursor value at
    /// publish, which makes it invalidation flag, version, and identity in
    /// one compare. Must remain field 0: it is accessed atomically by offset
    /// while the rest of the slot is copied as plain bytes.
    pub publication_sequence: u64,
    /// Producer frame count. Unlike the cursor it advances on dropped frames,
    /// so capture gaps stay visible.
    pub sequence: u64,
    pub timestamp_ns: u64,
    /// Unique forever, never reused, so stale pools need no generation.
    pub pool_id: u64,
    pub payload_offset: u64,
    pub payload_len: u64,
    /// Timeline value to wait for on the config's fence before sampling a
    /// native payload.
    pub fence_value: u64,
    pub damage_base_sequence: u64,
    pub producer_drop_count: u64,
    pub slot_id: u32,
    /// Names a StreamConfig generation (truncated; generations are asserted
    /// to stay within u32).
    pub config_generation: u32,
    pub payload_kind: u32,
    pub damage_kind: u32,
    pub dropped_before_publish: u32,
    pub flags: u32,
    pub reserved: [u8; 32],
}

/// jackstay_stream_config: per-stream values that change only on reconfigure.
/// A resize is not a special frame; it is a new generation (and, when
/// dimensions grow, a new pool) that subsequent frames reference. The ring
/// holds the last CONFIG_RING_CAPACITY generations at index
/// generation & (capacity-1).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfig {
    /// Seqlock word: 0 while mid-write, else the generation. Field 0, same
    /// discipline as FrameSlot::publication_sequence.
    pub config_generation: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: u32,
    pub color_space: u32,
    pub clock_domain: u32,
    pub sync_kind: u32,
    pub reserved0: u32,
    pub modifier: u64,
    /// The stream's timeline fence, as registered via the setup channel.
    pub fence_id: u64,
    pub reserved1: [u8; 72],
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            config_generation: 0,
            width: 0,
            height: 0,
            stride: 0,
            pixel_format: 0,
            color_space: 0,
            clock_domain: 0,
            sync_kind: 0,
            reserved0: 0,
            modifier: 0,
            fence_id: 0,
            reserved1: [0; 72],
        }
    }
}

/// jackstay_consumer_slot: one cacheline per consumer so consumers never
/// share a line. The producer allocates and frees slots (consumer_id == 0
/// means free); a registered consumer stores only into its own slot. Every
/// field is observational: stores are individually atomic but the slot is
/// never read as one consistent snapshot.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoConsumerCursorEntry {
    pub consumer_id: u64,
    /// Frames at or below this cursor are no longer being sampled.
    pub release_cursor: u64,
    pub last_acquired_cursor: u64,
    pub last_acquired_sequence: u64,
    pub acquired_count: u64,
    pub skipped_count: u64,
    pub reserved: [u8; 80],
}

impl Default for VideoConsumerCursorEntry {
    fn default() -> Self {
        Self {
            consumer_id: 0,
            release_cursor: 0,
            last_acquired_cursor: 0,
            last_acquired_sequence: 0,
            acquired_count: 0,
            skipped_count: 0,
            reserved: [0; 80],
        }
    }
}

// Offsets and sizes are the contract from include/jackstay_ring.h.
const _: () = {
    assert!(std::mem::size_of::<VideoTrackControlHeader>() == 256);
    assert!(std::mem::offset_of!(VideoTrackControlHeader, magic) == 0);
    assert!(std::mem::offset_of!(VideoTrackControlHeader, layout_version) == 8);
    assert!(std::mem::offset_of!(VideoTrackControlHeader, header_len) == 12);
    assert!(std::mem::offset_of!(VideoTrackControlHeader, slot_len) == 16);
    assert!(std::mem::offset_of!(VideoTrackControlHeader, slot_capacity) == 20);
    assert!(std::mem::offset_of!(VideoTrackControlHeader, config_len) == 24);
    assert!(std::mem::offset_of!(VideoTrackControlHeader, config_capacity) == 28);
    assert!(std::mem::offset_of!(VideoTrackControlHeader, consumer_len) == 32);
    assert!(std::mem::offset_of!(VideoTrackControlHeader, consumer_capacity) == 36);
    assert!(std::mem::offset_of!(VideoTrackControlHeader, slots_offset) == 40);
    assert!(std::mem::offset_of!(VideoTrackControlHeader, configs_offset) == 44);
    assert!(std::mem::offset_of!(VideoTrackControlHeader, consumers_offset) == 48);
    assert!(std::mem::offset_of!(VideoTrackControlHeader, producer_cursor) == 128);
    assert!(std::mem::offset_of!(VideoTrackControlHeader, config_cursor) == 136);

    assert!(std::mem::size_of::<FrameSlot>() == 128);
    assert!(std::mem::offset_of!(FrameSlot, publication_sequence) == 0);
    assert!(std::mem::offset_of!(FrameSlot, sequence) == 8);
    assert!(std::mem::offset_of!(FrameSlot, timestamp_ns) == 16);
    assert!(std::mem::offset_of!(FrameSlot, pool_id) == 24);
    assert!(std::mem::offset_of!(FrameSlot, payload_offset) == 32);
    assert!(std::mem::offset_of!(FrameSlot, payload_len) == 40);
    assert!(std::mem::offset_of!(FrameSlot, fence_value) == 48);
    assert!(std::mem::offset_of!(FrameSlot, damage_base_sequence) == 56);
    assert!(std::mem::offset_of!(FrameSlot, producer_drop_count) == 64);
    assert!(std::mem::offset_of!(FrameSlot, slot_id) == 72);
    assert!(std::mem::offset_of!(FrameSlot, config_generation) == 76);
    assert!(std::mem::offset_of!(FrameSlot, payload_kind) == 80);
    assert!(std::mem::offset_of!(FrameSlot, damage_kind) == 84);
    assert!(std::mem::offset_of!(FrameSlot, dropped_before_publish) == 88);
    assert!(std::mem::offset_of!(FrameSlot, flags) == 92);
    assert!(std::mem::offset_of!(FrameSlot, reserved) == 96);

    assert!(std::mem::size_of::<StreamConfig>() == 128);
    assert!(std::mem::offset_of!(StreamConfig, config_generation) == 0);
    assert!(std::mem::offset_of!(StreamConfig, width) == 8);
    assert!(std::mem::offset_of!(StreamConfig, height) == 12);
    assert!(std::mem::offset_of!(StreamConfig, stride) == 16);
    assert!(std::mem::offset_of!(StreamConfig, pixel_format) == 20);
    assert!(std::mem::offset_of!(StreamConfig, color_space) == 24);
    assert!(std::mem::offset_of!(StreamConfig, clock_domain) == 28);
    assert!(std::mem::offset_of!(StreamConfig, sync_kind) == 32);
    assert!(std::mem::offset_of!(StreamConfig, modifier) == 40);
    assert!(std::mem::offset_of!(StreamConfig, fence_id) == 48);
    assert!(std::mem::offset_of!(StreamConfig, reserved1) == 56);

    assert!(std::mem::size_of::<VideoConsumerCursorEntry>() == 128);
    assert!(std::mem::offset_of!(VideoConsumerCursorEntry, consumer_id) == 0);
    assert!(std::mem::offset_of!(VideoConsumerCursorEntry, release_cursor) == 8);
    assert!(std::mem::offset_of!(VideoConsumerCursorEntry, last_acquired_cursor) == 16);
    assert!(std::mem::offset_of!(VideoConsumerCursorEntry, last_acquired_sequence) == 24);
    assert!(std::mem::offset_of!(VideoConsumerCursorEntry, acquired_count) == 32);
    assert!(std::mem::offset_of!(VideoConsumerCursorEntry, skipped_count) == 40);
};

/// A frame as a producer submits it: the per-frame slot values plus the
/// stream-level values. The page splits them, deduplicating unchanged stream
/// values into the current config generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingVideoRingEntry {
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: u32,
    pub pool_id: u64,
    pub slot_id: u32,
    pub payload_offset: u64,
    pub payload_len: u64,
    pub clock_domain: u32,
    pub color_space: u32,
    pub sync_kind: u32,
    pub damage_kind: u32,
    pub damage_base_sequence: u64,
    pub dropped_before_publish: u32,
    pub producer_drop_count: u64,
    pub payload_kind: u32,
    pub modifier: u64,
    pub fence_id: u64,
    pub fence_value: u64,
    pub flags: u32,
}

/// A validated read: the frame slot merged with the stream config it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoRingEntry {
    pub cursor: u64,
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub config_generation: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: u32,
    pub pool_id: u64,
    pub slot_id: u32,
    pub payload_offset: u64,
    pub payload_len: u64,
    pub clock_domain: u32,
    pub color_space: u32,
    pub sync_kind: u32,
    pub damage_kind: u32,
    pub damage_base_sequence: u64,
    pub dropped_before_publish: u32,
    pub producer_drop_count: u64,
    pub payload_kind: u32,
    pub modifier: u64,
    pub fence_id: u64,
    pub fence_value: u64,
    pub flags: u32,
}

impl VideoRingEntry {
    fn from_parts(cursor: u64, slot: &FrameSlot, config: &StreamConfig) -> Self {
        Self {
            cursor,
            sequence: slot.sequence,
            timestamp_ns: slot.timestamp_ns,
            config_generation: config.config_generation,
            width: config.width,
            height: config.height,
            stride: config.stride,
            pixel_format: config.pixel_format,
            pool_id: slot.pool_id,
            slot_id: slot.slot_id,
            payload_offset: slot.payload_offset,
            payload_len: slot.payload_len,
            clock_domain: config.clock_domain,
            color_space: config.color_space,
            sync_kind: config.sync_kind,
            damage_kind: slot.damage_kind,
            damage_base_sequence: slot.damage_base_sequence,
            dropped_before_publish: slot.dropped_before_publish,
            producer_drop_count: slot.producer_drop_count,
            payload_kind: slot.payload_kind,
            modifier: config.modifier,
            fence_id: config.fence_id,
            fence_value: slot.fence_value,
            flags: slot.flags,
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum VideoRingReadError {
    #[error("video ring is empty")]
    Empty,
    #[error("video ring cursor {requested_cursor} is not published yet; latest cursor is {latest_cursor}")]
    NotPublished { requested_cursor: u64, latest_cursor: u64 },
    #[error(
        "video ring cursor {requested_cursor} has been lapped; oldest live cursor is {oldest_live_cursor}, latest cursor is {latest_cursor}"
    )]
    Lapped {
        requested_cursor: u64,
        oldest_live_cursor: u64,
        latest_cursor: u64,
    },
    #[error("video ring cursor {requested_cursor} slot sequence mismatch: first read {first_sequence}, second read {second_sequence}")]
    SlotSequenceMismatch {
        requested_cursor: u64,
        first_sequence: u64,
        second_sequence: u64,
    },
    #[error(
        "video ring cursor {requested_cursor} names stream config generation {requested_generation} which is no longer live: first read {first_generation}, second read {second_generation}"
    )]
    ConfigOverwritten {
        requested_cursor: u64,
        requested_generation: u64,
        first_generation: u64,
        second_generation: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoTrackControlLayout {
    pub header_offset: usize,
    pub slots_offset: usize,
    pub slot_len: usize,
    pub slot_capacity: usize,
    pub configs_offset: usize,
    pub config_len: usize,
    pub config_capacity: usize,
    pub consumers_offset: usize,
    pub consumer_len: usize,
    pub consumer_capacity: usize,
    pub byte_len: usize,
}

impl VideoTrackControlLayout {
    #[must_use]
    pub fn for_capacity(capacity: usize) -> Self {
        let slot_capacity = ring_capacity_for(capacity);
        let slot_len = std::mem::size_of::<FrameSlot>();
        let slots_offset = HEADER_LEN;
        let slots_len = slot_capacity
            .checked_mul(slot_len)
            .expect("video control page slot byte length overflow");
        let configs_offset = slots_offset
            .checked_add(slots_len)
            .expect("video control page slot byte length overflow");
        let config_len = std::mem::size_of::<StreamConfig>();
        let configs_len = CONFIG_RING_CAPACITY * config_len;
        let consumers_offset = configs_offset
            .checked_add(configs_len)
            .expect("video control page config byte length overflow");
        let consumer_len = std::mem::size_of::<VideoConsumerCursorEntry>();
        let consumer_capacity = slot_capacity;
        let consumers_len = consumer_capacity
            .checked_mul(consumer_len)
            .expect("video control page consumer byte length overflow");
        let byte_len = consumers_offset
            .checked_add(consumers_len)
            .expect("video control page byte length overflow");
        Self {
            header_offset: 0,
            slots_offset,
            slot_len,
            slot_capacity,
            configs_offset,
            config_len,
            config_capacity: CONFIG_RING_CAPACITY,
            consumers_offset,
            consumer_len,
            consumer_capacity,
            byte_len,
        }
    }

    const fn slot_index_mask(&self) -> u64 {
        (self.slot_capacity - 1) as u64
    }

    const fn config_index_mask(&self) -> u64 {
        (self.config_capacity - 1) as u64
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoTrackControlSnapshot {
    pub header: VideoTrackControlHeader,
    pub entries: Vec<VideoRingEntry>,
    pub configs: Vec<StreamConfig>,
    pub consumer_entries: Vec<VideoConsumerCursorEntry>,
}

#[derive(Debug)]
pub struct VideoTrackControlPage {
    layout: VideoTrackControlLayout,
    storage: SharedMemorySegment,
    /// Producer-side cache of the live config (stored with generation 0 so it
    /// compares against candidate bodies directly). Only the creating
    /// producer pushes, so a mapped page starts cold and republishes once.
    producer_config: Option<(u64, StreamConfig)>,
}

impl VideoTrackControlPage {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let layout = VideoTrackControlLayout::for_capacity(capacity);
        let storage = SharedMemorySegment::new(layout.byte_len).expect("control page mapped storage allocation failed");
        let page = Self {
            layout,
            storage,
            producer_config: None,
        };
        page.store_initial_header();
        page
    }

    #[must_use]
    pub fn layout(&self) -> VideoTrackControlLayout {
        self.layout
    }

    #[must_use]
    pub fn mapped_len(&self) -> usize {
        self.storage.len()
    }

    pub fn try_clone_fd(&self) -> CaptureResult<OwnedFd> {
        self.storage.try_clone_fd()
    }

    pub fn map_read_only(fd: OwnedFd, map_len: usize) -> CaptureResult<Self> {
        let storage = SharedMemorySegment::map_read_only(fd, map_len)?;
        Self::from_mapped_storage(storage, map_len)
    }

    pub fn map_read_write(fd: OwnedFd, map_len: usize) -> CaptureResult<Self> {
        let storage = SharedMemorySegment::map_read_write(fd, map_len)?;
        Self::from_mapped_storage(storage, map_len)
    }

    fn from_mapped_storage(storage: SharedMemorySegment, map_len: usize) -> CaptureResult<Self> {
        if map_len < HEADER_LEN {
            return Err(invalid_control_page("mapped length is smaller than the ring header"));
        }
        let mut page = Self {
            layout: VideoTrackControlLayout::for_capacity(1),
            storage,
            producer_config: None,
        };
        let initial_header = page.header();
        let capacity = validate_control_header(&initial_header, map_len)?;
        page.layout = VideoTrackControlLayout::for_capacity(capacity);
        // Re-read after choosing the final layout so a racing or corrupt header
        // is caught against the mapping shape callers will use.
        page.validate_header()?;
        Ok(page)
    }

    pub fn validate_header(&self) -> CaptureResult<VideoTrackControlHeader> {
        let header = self.header();
        validate_control_header(&header, self.storage.len())?;
        Ok(header)
    }

    pub fn shadow_read_entry_for_cursor(&self, cursor: u64) -> CaptureResult<VideoRingEntry> {
        // This shadow path is diagnostic while socket metadata remains
        // authoritative, so validate on each read and fail closed.
        self.validate_header()?;
        self.read_entry_for_cursor(cursor)
            .map_err(|source| CaptureTransferError::SharedMemory {
                operation: "read-control-page",
                message: source.to_string(),
            })
    }

    fn header(&self) -> VideoTrackControlHeader {
        VideoTrackControlHeader {
            magic: self.read_at(std::mem::offset_of!(VideoTrackControlHeader, magic)),
            layout_version: self.read_at(std::mem::offset_of!(VideoTrackControlHeader, layout_version)),
            header_len: self.read_at(std::mem::offset_of!(VideoTrackControlHeader, header_len)),
            slot_len: self.read_at(std::mem::offset_of!(VideoTrackControlHeader, slot_len)),
            slot_capacity: self.read_at(std::mem::offset_of!(VideoTrackControlHeader, slot_capacity)),
            config_len: self.read_at(std::mem::offset_of!(VideoTrackControlHeader, config_len)),
            config_capacity: self.read_at(std::mem::offset_of!(VideoTrackControlHeader, config_capacity)),
            consumer_len: self.read_at(std::mem::offset_of!(VideoTrackControlHeader, consumer_len)),
            consumer_capacity: self.read_at(std::mem::offset_of!(VideoTrackControlHeader, consumer_capacity)),
            slots_offset: self.read_at(std::mem::offset_of!(VideoTrackControlHeader, slots_offset)),
            configs_offset: self.read_at(std::mem::offset_of!(VideoTrackControlHeader, configs_offset)),
            consumers_offset: self.read_at(std::mem::offset_of!(VideoTrackControlHeader, consumers_offset)),
            reserved0: [0; 76],
            producer_cursor: self.load_header_producer_cursor(Ordering::Acquire),
            config_cursor: self.load_header_config_cursor(Ordering::Acquire),
            reserved1: [0; 112],
        }
    }

    fn store_initial_header(&self) {
        let header = VideoTrackControlHeader {
            magic: VIDEO_TRACK_CONTROL_MAGIC,
            layout_version: VIDEO_TRACK_CONTROL_VERSION,
            header_len: HEADER_LEN as u32,
            slot_len: self.layout.slot_len as u32,
            slot_capacity: self.layout.slot_capacity as u32,
            config_len: self.layout.config_len as u32,
            config_capacity: self.layout.config_capacity as u32,
            consumer_len: self.layout.consumer_len as u32,
            consumer_capacity: self.layout.consumer_capacity as u32,
            slots_offset: self.layout.slots_offset as u32,
            configs_offset: self.layout.configs_offset as u32,
            consumers_offset: self.layout.consumers_offset as u32,
            reserved0: [0; 76],
            producer_cursor: 0,
            config_cursor: 0,
            reserved1: [0; 112],
        };
        // This is the one whole-header write, performed before the page can be
        // shared with any reader. After init, hot fields use atomic helpers.
        self.write_at(self.layout.header_offset, header);
    }

    pub fn push(&mut self, entry: PendingVideoRingEntry) -> u64 {
        let config_generation = self.ensure_stream_config(&entry);
        // A u64 producer cursor is effectively inexhaustible for display-rate
        // capture, but saturation would make ring-slot selection ambiguous.
        let cursor = self.load_header_producer_cursor(Ordering::Relaxed).saturating_add(1);
        debug_assert!(cursor < u64::MAX);
        let index = ((cursor - 1) & self.layout.slot_index_mask()) as usize;
        let slot = FrameSlot {
            publication_sequence: 0,
            sequence: entry.sequence,
            timestamp_ns: entry.timestamp_ns,
            pool_id: entry.pool_id,
            payload_offset: entry.payload_offset,
            payload_len: entry.payload_len,
            fence_value: entry.fence_value,
            damage_base_sequence: entry.damage_base_sequence,
            producer_drop_count: entry.producer_drop_count,
            slot_id: entry.slot_id,
            config_generation: config_generation as u32,
            payload_kind: entry.payload_kind,
            damage_kind: entry.damage_kind,
            dropped_before_publish: entry.dropped_before_publish,
            flags: entry.flags,
            reserved: [0; 32],
        };
        // Seqlock writer. The release fence keeps the invalidating zero ahead
        // of the descriptor's plain stores; without it ARM may drain the new
        // descriptor bytes first and a lagging reader of the old cursor would
        // double-read clean over torn data.
        self.store_slot_publication_sequence(index, 0, Ordering::Relaxed);
        fence(Ordering::Release);
        self.store_slot_descriptor(index, slot);
        self.store_slot_publication_sequence(index, cursor, Ordering::Release);
        self.store_header_producer_cursor(cursor, Ordering::Release);
        cursor
    }

    fn ensure_stream_config(&mut self, entry: &PendingVideoRingEntry) -> u64 {
        let body = StreamConfig {
            config_generation: 0,
            width: entry.width,
            height: entry.height,
            stride: entry.stride,
            pixel_format: entry.pixel_format,
            color_space: entry.color_space,
            clock_domain: entry.clock_domain,
            sync_kind: entry.sync_kind,
            reserved0: 0,
            modifier: entry.modifier,
            fence_id: entry.fence_id,
            reserved1: [0; 72],
        };
        if let Some((generation, cached)) = &self.producer_config
            && *cached == body
        {
            return *generation;
        }
        let generation = self.load_header_config_cursor(Ordering::Relaxed) + 1;
        // The frame slot carries the generation truncated to u32; reconfigures
        // are rare enough (a per-event resize storm would take years) that the
        // truncation is asserted rather than handled.
        debug_assert!(generation <= u64::from(u32::MAX));
        let index = (generation & self.layout.config_index_mask()) as usize;
        // Same seqlock writer shape as push().
        self.store_config_generation(index, 0, Ordering::Relaxed);
        fence(Ordering::Release);
        self.store_config_descriptor(index, body);
        self.store_config_generation(index, generation, Ordering::Release);
        self.store_header_config_cursor(generation, Ordering::Release);
        self.producer_config = Some((generation, body));
        generation
    }

    pub fn read_entry_for_cursor(&self, cursor: u64) -> Result<VideoRingEntry, VideoRingReadError> {
        let Some(latest_cursor) = self.latest_cursor() else {
            return Err(VideoRingReadError::Empty);
        };
        if cursor > latest_cursor {
            return Err(VideoRingReadError::NotPublished {
                requested_cursor: cursor,
                latest_cursor,
            });
        }
        let oldest_live_cursor = self.oldest_live_cursor().expect("latest cursor implies oldest cursor");
        if cursor < oldest_live_cursor {
            return Err(VideoRingReadError::Lapped {
                requested_cursor: cursor,
                oldest_live_cursor,
                latest_cursor,
            });
        }

        let index = ((cursor - 1) & self.layout.slot_index_mask()) as usize;
        let slot = self.read_slot_seqlock(index, cursor)?;
        let generation = u64::from(slot.config_generation);
        let config = self
            .read_config_seqlock(generation)
            .map_err(|(first, second)| VideoRingReadError::ConfigOverwritten {
                requested_cursor: cursor,
                requested_generation: generation,
                first_generation: first,
                second_generation: second,
            })?;
        Ok(VideoRingEntry::from_parts(cursor, &slot, &config))
    }

    pub fn read_latest_lossy_entry(&self) -> Result<Option<VideoRingEntry>, VideoRingReadError> {
        let Some(cursor) = self.latest_cursor() else {
            return Ok(None);
        };
        self.read_entry_for_cursor(cursor).map(Some)
    }

    fn read_slot_seqlock(&self, index: usize, expected_cursor: u64) -> Result<FrameSlot, VideoRingReadError> {
        let first_sequence = self.load_slot_publication_sequence(index, Ordering::Acquire);
        let slot = self.read_slot_descriptor(index, first_sequence);
        // Order the descriptor's plain loads before the validating re-read; an
        // acquire load alone does not keep earlier loads from completing late.
        fence(Ordering::Acquire);
        let second_sequence = self.load_slot_publication_sequence(index, Ordering::Relaxed);
        if first_sequence != expected_cursor || second_sequence != expected_cursor {
            return Err(VideoRingReadError::SlotSequenceMismatch {
                requested_cursor: expected_cursor,
                first_sequence,
                second_sequence,
            });
        }
        Ok(slot)
    }

    fn read_config_seqlock(&self, generation: u64) -> Result<StreamConfig, (u64, u64)> {
        if generation == 0 {
            return Err((0, 0));
        }
        let index = (generation & self.layout.config_index_mask()) as usize;
        let first = self.load_config_generation(index, Ordering::Acquire);
        let mut config = self.read_config_descriptor(index);
        // Same load-ordering fence as the frame slot reader.
        fence(Ordering::Acquire);
        let second = self.load_config_generation(index, Ordering::Relaxed);
        if first != generation || second != generation {
            return Err((first, second));
        }
        config.config_generation = generation;
        Ok(config)
    }

    fn read_slot_descriptor(&self, index: usize, publication_sequence: u64) -> FrameSlot {
        assert!(index < self.layout.slot_capacity);
        let mut slot = FrameSlot {
            publication_sequence,
            ..FrameSlot::default()
        };
        let descriptor_offset = self.slot_offset(index) + SEQLOCK_WORD_LEN;
        let descriptor_len = self.layout.slot_len - SEQLOCK_WORD_LEN;
        let bytes = self.storage.slice_at(descriptor_offset, descriptor_len);
        // SAFETY: `publication_sequence` is field 0. This copies the rest of
        // the repr(C) slot into a local value without reading the atomic
        // sequence as plain bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                std::ptr::addr_of_mut!(slot).cast::<u8>().add(SEQLOCK_WORD_LEN),
                bytes.len(),
            );
        }
        slot
    }

    fn store_slot_descriptor(&self, index: usize, slot: FrameSlot) {
        assert!(index < self.layout.slot_capacity);
        let descriptor_offset = self.slot_offset(index) + SEQLOCK_WORD_LEN;
        let descriptor_len = self.layout.slot_len - SEQLOCK_WORD_LEN;
        self.storage.with_slice_at_mut(descriptor_offset, descriptor_len, |bytes| {
            // SAFETY: `publication_sequence` is field 0. This copies the rest
            // of the repr(C) slot without touching the atomic sequence.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    std::ptr::addr_of!(slot).cast::<u8>().add(SEQLOCK_WORD_LEN),
                    bytes.as_mut_ptr(),
                    bytes.len(),
                );
            }
        });
    }

    fn read_config_descriptor(&self, index: usize) -> StreamConfig {
        assert!(index < self.layout.config_capacity);
        let mut config = StreamConfig::default();
        let descriptor_offset = self.config_offset(index) + SEQLOCK_WORD_LEN;
        let descriptor_len = self.layout.config_len - SEQLOCK_WORD_LEN;
        let bytes = self.storage.slice_at(descriptor_offset, descriptor_len);
        // SAFETY: `config_generation` is field 0. This copies the rest of the
        // repr(C) config without reading the atomic generation as plain bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                std::ptr::addr_of_mut!(config).cast::<u8>().add(SEQLOCK_WORD_LEN),
                bytes.len(),
            );
        }
        config
    }

    fn store_config_descriptor(&self, index: usize, config: StreamConfig) {
        assert!(index < self.layout.config_capacity);
        let descriptor_offset = self.config_offset(index) + SEQLOCK_WORD_LEN;
        let descriptor_len = self.layout.config_len - SEQLOCK_WORD_LEN;
        self.storage.with_slice_at_mut(descriptor_offset, descriptor_len, |bytes| {
            // SAFETY: `config_generation` is field 0. This copies the rest of
            // the repr(C) config without touching the atomic generation.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    std::ptr::addr_of!(config).cast::<u8>().add(SEQLOCK_WORD_LEN),
                    bytes.as_mut_ptr(),
                    bytes.len(),
                );
            }
        });
    }

    fn load_slot_publication_sequence(&self, index: usize, ordering: Ordering) -> u64 {
        assert!(index < self.layout.slot_capacity);
        self.load_u64_atomic(self.slot_offset(index), ordering)
    }

    fn store_slot_publication_sequence(&self, index: usize, publication_sequence: u64, ordering: Ordering) {
        assert!(index < self.layout.slot_capacity);
        self.store_u64_atomic(self.slot_offset(index), publication_sequence, ordering);
    }

    fn load_config_generation(&self, index: usize, ordering: Ordering) -> u64 {
        assert!(index < self.layout.config_capacity);
        self.load_u64_atomic(self.config_offset(index), ordering)
    }

    fn store_config_generation(&self, index: usize, generation: u64, ordering: Ordering) {
        assert!(index < self.layout.config_capacity);
        self.store_u64_atomic(self.config_offset(index), generation, ordering);
    }

    fn load_header_producer_cursor(&self, ordering: Ordering) -> u64 {
        self.load_u64_atomic(
            self.layout.header_offset + std::mem::offset_of!(VideoTrackControlHeader, producer_cursor),
            ordering,
        )
    }

    fn store_header_producer_cursor(&self, value: u64, ordering: Ordering) {
        self.store_u64_atomic(
            self.layout.header_offset + std::mem::offset_of!(VideoTrackControlHeader, producer_cursor),
            value,
            ordering,
        );
    }

    fn load_header_config_cursor(&self, ordering: Ordering) -> u64 {
        self.load_u64_atomic(
            self.layout.header_offset + std::mem::offset_of!(VideoTrackControlHeader, config_cursor),
            ordering,
        )
    }

    fn store_header_config_cursor(&self, value: u64, ordering: Ordering) {
        self.store_u64_atomic(
            self.layout.header_offset + std::mem::offset_of!(VideoTrackControlHeader, config_cursor),
            value,
            ordering,
        );
    }

    fn slot_offset(&self, index: usize) -> usize {
        self.layout.slots_offset + index * self.layout.slot_len
    }

    fn config_offset(&self, index: usize) -> usize {
        self.layout.configs_offset + index * self.layout.config_len
    }

    fn consumer_offset(&self, index: usize) -> usize {
        assert!(index < self.layout.consumer_capacity);
        self.layout.consumers_offset + index * self.layout.consumer_len
    }

    fn load_u64_atomic(&self, offset: usize, ordering: Ordering) -> u64 {
        assert_eq!(offset % std::mem::align_of::<AtomicU64>(), 0);
        let bytes = self.storage.slice_at(offset, std::mem::size_of::<AtomicU64>());
        // SAFETY: callers supply an AtomicU64-aligned offset inside the live
        // mapping. The storage is a plain mapped page whose hot u64 fields are
        // exclusively accessed through these atomic helpers after init.
        unsafe { (&*bytes.as_ptr().cast::<AtomicU64>()).load(ordering) }
    }

    fn store_u64_atomic(&self, offset: usize, value: u64, ordering: Ordering) {
        assert_eq!(offset % std::mem::align_of::<AtomicU64>(), 0);
        self.storage.with_slice_at_mut(offset, std::mem::size_of::<AtomicU64>(), |bytes| {
            // SAFETY: callers supply an AtomicU64-aligned offset inside the live
            // mapping. The storage is a plain mapped page whose hot u64 fields
            // are exclusively accessed through these atomic helpers after init.
            unsafe {
                (&*bytes.as_mut_ptr().cast::<AtomicU64>()).store(value, ordering);
            }
        });
    }

    fn read_at<T: Copy>(&self, offset: usize) -> T {
        let bytes = self.storage.slice_at(offset, std::mem::size_of::<T>());
        // SAFETY: the page layout writes only plain repr(C) integer structs at
        // these offsets, and the slice length was checked against T's size.
        unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) }
    }

    fn write_at<T: Copy>(&self, offset: usize, value: T) {
        self.storage.with_slice_at_mut(offset, std::mem::size_of::<T>(), |bytes| {
            // SAFETY: the destination slice is exactly T-sized and lives inside
            // the mapped page. T is Copy and contains no references or drop glue.
            unsafe {
                std::ptr::copy_nonoverlapping(std::ptr::addr_of!(value).cast::<u8>(), bytes.as_mut_ptr(), bytes.len());
            }
        });
    }

    pub fn register_consumer_cursor(&self, consumer_id: u64) -> CaptureResult<usize> {
        // This is a producer-side slot allocator called through VideoSlotManager's
        // mutable track state. It is not a cross-process concurrent allocator; mapped
        // consumers are only allowed to write into their assigned slot.
        if consumer_id == 0 {
            return Err(invalid_control_page("consumer id must be non-zero"));
        }
        for index in 0..self.layout.consumer_capacity {
            if self.consumer_cursor_entry(index).consumer_id == consumer_id {
                return Ok(index);
            }
        }
        for index in 0..self.layout.consumer_capacity {
            if self.consumer_cursor_entry(index).consumer_id == 0 {
                self.write_at(
                    self.consumer_offset(index),
                    VideoConsumerCursorEntry {
                        consumer_id,
                        ..VideoConsumerCursorEntry::default()
                    },
                );
                return Ok(index);
            }
        }
        Err(invalid_control_page("consumer cursor table is full"))
    }

    pub fn unregister_consumer_cursor(&self, consumer_id: u64) {
        for index in 0..self.layout.consumer_capacity {
            if self.consumer_cursor_entry(index).consumer_id == consumer_id {
                self.write_at(self.consumer_offset(index), VideoConsumerCursorEntry::default());
                return;
            }
        }
    }

    #[must_use]
    pub fn consumer_cursor_entry(&self, index: usize) -> VideoConsumerCursorEntry {
        self.read_at(self.consumer_offset(index))
    }

    pub fn store_consumer_release_cursor(&self, index: usize, consumer_id: u64, release_cursor: u64) -> CaptureResult<()> {
        let entry = self.consumer_cursor_entry(index);
        // The read-check-store sequence is not one atomic op, but only the
        // producer reallocates a consumer slot (see register_consumer_cursor),
        // and a single consumer never races itself, so no other writer can
        // change ownership between the check and the store. No CAS is needed.
        if entry.consumer_id != consumer_id {
            return Err(invalid_control_page("consumer cursor slot does not match consumer id"));
        }
        self.store_u64_atomic(
            self.consumer_offset(index) + std::mem::offset_of!(VideoConsumerCursorEntry, release_cursor),
            release_cursor,
            Ordering::Release,
        );
        Ok(())
    }

    pub fn store_consumer_acquire_cursor(
        &self,
        index: usize,
        consumer_id: u64,
        last_acquired_cursor: u64,
        last_acquired_sequence: u64,
        skipped_count: u64,
        acquired_count: u64,
    ) -> CaptureResult<()> {
        let entry = self.consumer_cursor_entry(index);
        if entry.consumer_id != consumer_id {
            return Err(invalid_control_page("consumer cursor slot does not match consumer id"));
        }
        // These fields are observational in this slice. They are stored independently
        // so acquire updates cannot clobber the consumer-owned release cursor; readers
        // must not treat the four acquire fields as one atomic snapshot.
        let base = self.consumer_offset(index);
        self.store_u64_atomic(
            base + std::mem::offset_of!(VideoConsumerCursorEntry, last_acquired_cursor),
            last_acquired_cursor,
            Ordering::Release,
        );
        self.store_u64_atomic(
            base + std::mem::offset_of!(VideoConsumerCursorEntry, last_acquired_sequence),
            last_acquired_sequence,
            Ordering::Release,
        );
        self.store_u64_atomic(
            base + std::mem::offset_of!(VideoConsumerCursorEntry, skipped_count),
            skipped_count,
            Ordering::Release,
        );
        self.store_u64_atomic(
            base + std::mem::offset_of!(VideoConsumerCursorEntry, acquired_count),
            acquired_count,
            Ordering::Release,
        );
        Ok(())
    }

    #[must_use]
    pub fn latest_cursor(&self) -> Option<u64> {
        let producer_cursor = self.load_header_producer_cursor(Ordering::Acquire);
        (producer_cursor > 0).then_some(producer_cursor)
    }

    #[must_use]
    pub fn oldest_live_cursor(&self) -> Option<u64> {
        let producer_cursor = self.load_header_producer_cursor(Ordering::Acquire);
        if producer_cursor == 0 {
            return None;
        }
        let len = producer_cursor.min(self.layout.slot_capacity as u64);
        Some(producer_cursor - len + 1)
    }

    #[must_use]
    pub fn cursor_lapped(&self, expected_cursor: u64) -> bool {
        self.oldest_live_cursor().is_some_and(|oldest| expected_cursor < oldest)
    }

    #[must_use]
    pub fn ring_snapshot(&self) -> Vec<VideoRingEntry> {
        let (Some(oldest), Some(latest)) = (self.oldest_live_cursor(), self.latest_cursor()) else {
            return Vec::new();
        };
        // Lossy by design: entries with overwritten configs or torn seqlock
        // reads (ConfigOverwritten / SlotSequenceMismatch / Lapped) are omitted
        // rather than failing the call. A diagnostic snapshot must not fault.
        (oldest..=latest)
            .filter_map(|cursor| self.read_entry_for_cursor(cursor).ok())
            .collect()
    }

    #[must_use]
    pub fn config_snapshot(&self) -> Vec<StreamConfig> {
        let latest = self.load_header_config_cursor(Ordering::Acquire);
        if latest == 0 {
            return Vec::new();
        }
        let live = latest.min(self.layout.config_capacity as u64);
        let oldest = latest - live + 1;
        (oldest..=latest)
            .filter_map(|generation| self.read_config_seqlock(generation).ok())
            .collect()
    }

    #[must_use]
    pub fn consumer_cursor_snapshot(&self) -> Vec<VideoConsumerCursorEntry> {
        (0..self.layout.consumer_capacity)
            .map(|index| self.consumer_cursor_entry(index))
            .collect()
    }

    #[must_use]
    pub fn snapshot(&self) -> VideoTrackControlSnapshot {
        VideoTrackControlSnapshot {
            header: self.header(),
            entries: self.ring_snapshot(),
            configs: self.config_snapshot(),
            consumer_entries: self.consumer_cursor_snapshot(),
        }
    }

    #[cfg(test)]
    fn raw_slot_for_test(&self, index: usize) -> FrameSlot {
        let publication_sequence = self.load_slot_publication_sequence(index, Ordering::Acquire);
        self.read_slot_descriptor(index, publication_sequence)
    }

    #[cfg(test)]
    pub(crate) fn set_slot_publication_sequence_for_test(&self, index: usize, publication_sequence: u64) {
        self.store_slot_publication_sequence(index, publication_sequence, Ordering::Release);
    }

    #[cfg(test)]
    fn hot_field_offsets_for_test(&self) -> Vec<usize> {
        let header = self.layout.header_offset;
        vec![
            header + std::mem::offset_of!(VideoTrackControlHeader, producer_cursor),
            header + std::mem::offset_of!(VideoTrackControlHeader, config_cursor),
            self.slot_offset(0),
            self.config_offset(0),
            self.consumer_offset(0),
        ]
    }
}

fn ring_capacity_for(requested: usize) -> usize {
    requested.max(1).next_power_of_two()
}

fn validate_control_header(header: &VideoTrackControlHeader, map_len: usize) -> CaptureResult<usize> {
    if header.magic != VIDEO_TRACK_CONTROL_MAGIC {
        return Err(invalid_control_page("invalid magic"));
    }
    if header.layout_version != VIDEO_TRACK_CONTROL_VERSION {
        return Err(invalid_control_page("invalid layout version"));
    }
    if header.header_len != HEADER_LEN as u32 {
        return Err(invalid_control_page("invalid header length"));
    }
    if header.slot_len != std::mem::size_of::<FrameSlot>() as u32 {
        return Err(invalid_control_page("invalid frame slot length"));
    }
    if header.config_len != std::mem::size_of::<StreamConfig>() as u32 {
        return Err(invalid_control_page("invalid stream config length"));
    }
    if header.consumer_len != std::mem::size_of::<VideoConsumerCursorEntry>() as u32 {
        return Err(invalid_control_page("invalid consumer slot length"));
    }
    let capacity = header.slot_capacity as usize;
    if capacity == 0 || !capacity.is_power_of_two() {
        return Err(invalid_control_page("slot capacity must be a non-zero power of two"));
    }
    if header.config_capacity as usize != CONFIG_RING_CAPACITY {
        return Err(invalid_control_page("config capacity does not match layout"));
    }
    if header.consumer_capacity != header.slot_capacity {
        return Err(invalid_control_page("consumer capacity must match ring capacity"));
    }
    let layout = VideoTrackControlLayout::for_capacity(capacity);
    if header.slots_offset as usize != layout.slots_offset
        || header.configs_offset as usize != layout.configs_offset
        || header.consumers_offset as usize != layout.consumers_offset
    {
        return Err(invalid_control_page("region offsets do not match layout"));
    }
    if layout.byte_len > map_len {
        return Err(invalid_control_page("mapped length is smaller than declared layout"));
    }
    Ok(capacity)
}

fn invalid_control_page(message: impl Into<String>) -> CaptureTransferError {
    CaptureTransferError::SharedMemory {
        operation: "validate-control-page",
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CONFIG_RING_CAPACITY, CONTROL_PAGE_ALIGNMENT, FrameSlot, PendingVideoRingEntry, VideoRingReadError, VideoTrackControlPage,
    };
    use crate::model::{ClockDomain, ColorSpace, DamageKind, FrameSyncKind, PayloadKind, PixelFormat};

    fn pending(sequence: u64) -> PendingVideoRingEntry {
        PendingVideoRingEntry {
            sequence,
            timestamp_ns: 0,
            width: 0,
            height: 0,
            stride: 0,
            pixel_format: 0,
            pool_id: 7,
            slot_id: sequence as u32,
            payload_offset: sequence * 64,
            payload_len: 4,
            clock_domain: 0,
            color_space: 0,
            sync_kind: 0,
            damage_kind: 0,
            damage_base_sequence: 0,
            dropped_before_publish: 0,
            producer_drop_count: 0,
            payload_kind: 0,
            modifier: 0,
            fence_id: 0,
            fence_value: 0,
            flags: 0,
        }
    }

    fn full_pending(sequence: u64) -> PendingVideoRingEntry {
        PendingVideoRingEntry {
            sequence,
            timestamp_ns: 123_456_789,
            width: 1920,
            height: 1080,
            stride: 7680,
            pixel_format: PixelFormat::Rgba8Unorm as u32,
            pool_id: 7,
            slot_id: sequence as u32,
            payload_offset: sequence * 64,
            payload_len: 4,
            clock_domain: ClockDomain::MediaTime as u32,
            color_space: ColorSpace::Srgb as u32,
            sync_kind: FrameSyncKind::CpuCopyComplete as u32,
            damage_kind: DamageKind::InlineRects as u32,
            damage_base_sequence: sequence.saturating_sub(1),
            dropped_before_publish: 5,
            producer_drop_count: 8,
            payload_kind: PayloadKind::IoSurface as u32,
            modifier: 42,
            fence_id: 9,
            fence_value: 100,
            flags: 1,
        }
    }

    #[test]
    fn control_page_layout_packs_cacheline_records() {
        let page = VideoTrackControlPage::new(3);

        let layout = page.layout();
        assert_eq!(layout.header_offset, 0);
        assert_eq!(layout.slots_offset, 256);
        assert_eq!(layout.slot_len, 128);
        assert_eq!(layout.slot_capacity, 4);
        assert_eq!(layout.config_len, 128);
        assert_eq!(layout.config_capacity, CONFIG_RING_CAPACITY);
        assert_eq!(layout.consumer_len, 128);
        assert_eq!(layout.consumer_capacity, 4);
        assert_eq!(layout.slots_offset % CONTROL_PAGE_ALIGNMENT, 0);
        assert_eq!(layout.configs_offset % CONTROL_PAGE_ALIGNMENT, 0);
        assert_eq!(layout.consumers_offset % CONTROL_PAGE_ALIGNMENT, 0);
        assert_eq!(layout.configs_offset, layout.slots_offset + 4 * 128);
        assert_eq!(layout.consumers_offset, layout.configs_offset + CONFIG_RING_CAPACITY * 128);
        assert_eq!(layout.byte_len, layout.consumers_offset + 4 * 128);
        assert_eq!(page.mapped_len(), layout.byte_len);
    }

    #[test]
    fn control_page_layout_is_deterministic_for_capacity() {
        assert_eq!(
            super::VideoTrackControlLayout::for_capacity(4),
            super::VideoTrackControlLayout::for_capacity(4)
        );
    }

    #[test]
    fn hot_words_never_share_a_cacheline_between_records() {
        let page = VideoTrackControlPage::new(2);

        for offset in page.hot_field_offsets_for_test() {
            assert_eq!(offset % std::mem::align_of::<std::sync::atomic::AtomicU64>(), 0);
        }
        // Seqlock words sit at the start of 128-byte records, so consecutive
        // records' atomics are always a full line apart.
        assert_eq!(page.layout().slot_len % CONTROL_PAGE_ALIGNMENT, 0);
        assert_eq!(page.layout().consumer_len % CONTROL_PAGE_ALIGNMENT, 0);
    }

    #[test]
    fn push_returns_monotonic_cursors() {
        let mut page = VideoTrackControlPage::new(2);

        assert_eq!(page.push(pending(10)), 1);
        assert_eq!(page.push(pending(11)), 2);
        assert_eq!(page.push(pending(12)), 3);
    }

    #[test]
    fn control_page_mapped_slots_start_zeroed() {
        let page = VideoTrackControlPage::new(2);

        assert_eq!(page.raw_slot_for_test(0), FrameSlot::default());
        assert_eq!(page.raw_slot_for_test(1), FrameSlot::default());
    }

    #[test]
    fn latest_cursor_returns_none_on_empty_page() {
        let page = VideoTrackControlPage::new(2);

        assert_eq!(page.latest_cursor(), None);
        assert_eq!(page.read_latest_lossy_entry(), Ok(None));
    }

    #[test]
    fn empty_track_control_page_writes_geometry_header() {
        let page = VideoTrackControlPage::new(0);

        let header = page.snapshot().header;
        assert_eq!(header.magic, super::VIDEO_TRACK_CONTROL_MAGIC);
        assert_eq!(header.layout_version, super::VIDEO_TRACK_CONTROL_VERSION);
        assert_eq!(header.header_len, 256);
        assert_eq!(header.slot_len, 128);
        assert_eq!(header.slot_capacity, 1);
        assert_eq!(header.config_capacity, CONFIG_RING_CAPACITY as u32);
        assert_eq!(header.consumer_capacity, 1);
        assert_eq!(header.producer_cursor, 0);
        assert_eq!(header.config_cursor, 0);
    }

    #[test]
    fn track_control_page_rounds_capacity_to_power_of_two() {
        let page = VideoTrackControlPage::new(3);

        assert_eq!(page.snapshot().header.slot_capacity, 4);
    }

    #[test]
    fn track_control_page_wraps_entries_in_oldest_to_newest_order() {
        let mut page = VideoTrackControlPage::new(2);

        page.push(pending(10));
        page.push(pending(11));
        page.push(pending(12));

        let snapshot = page.snapshot();
        assert_eq!(snapshot.header.slot_capacity, 2);
        assert_eq!(snapshot.header.producer_cursor, 3);
        assert_eq!(
            snapshot
                .entries
                .iter()
                .map(|entry| (entry.cursor, entry.sequence))
                .collect::<Vec<_>>(),
            vec![(2, 11), (3, 12)]
        );
        let latest = page.read_entry_for_cursor(3).unwrap();
        assert_eq!((latest.cursor, latest.sequence), (3, 12));
    }

    #[test]
    fn track_control_page_reports_latest_lossy_lap_state() {
        let mut page = VideoTrackControlPage::new(3);

        assert_eq!(page.oldest_live_cursor(), None);
        assert_eq!(page.latest_cursor(), None);
        assert!(!page.cursor_lapped(1));

        page.push(pending(10));
        page.push(pending(11));

        assert_eq!(page.oldest_live_cursor(), Some(1));
        assert_eq!(page.latest_cursor(), Some(2));
        assert!(!page.cursor_lapped(1));
        assert!(!page.cursor_lapped(2));

        page.push(pending(12));
        page.push(pending(13));
        page.push(pending(14));

        assert_eq!(page.oldest_live_cursor(), Some(2));
        assert_eq!(page.latest_cursor(), Some(5));
        assert!(page.cursor_lapped(1));
        assert!(!page.cursor_lapped(2));
        assert!(!page.cursor_lapped(5));
    }

    #[test]
    fn read_entry_for_cursor_reports_empty_and_future_cursors() {
        let mut page = VideoTrackControlPage::new(2);

        assert_eq!(page.read_entry_for_cursor(1), Err(VideoRingReadError::Empty));

        page.push(pending(10));

        assert_eq!(
            page.read_entry_for_cursor(0),
            Err(VideoRingReadError::Lapped {
                requested_cursor: 0,
                oldest_live_cursor: 1,
                latest_cursor: 1,
            })
        );
        assert_eq!(
            page.read_entry_for_cursor(2),
            Err(VideoRingReadError::NotPublished {
                requested_cursor: 2,
                latest_cursor: 1,
            })
        );
    }

    #[test]
    fn read_entry_for_cursor_returns_requested_entry() {
        let mut page = VideoTrackControlPage::new(4);

        page.push(pending(10));
        page.push(pending(11));

        let entry = page.read_entry_for_cursor(2).unwrap();
        assert_eq!(entry.cursor, 2);
        assert_eq!(entry.sequence, 11);
        assert_eq!(entry.pool_id, 7);
        assert_eq!(entry.slot_id, 11);
        assert_eq!(entry.payload_offset, 704);
        assert_eq!(entry.payload_len, 4);
        assert_eq!(entry.config_generation, 1);
    }

    #[test]
    fn read_entry_for_cursor_merges_full_descriptor_and_config() {
        let mut page = VideoTrackControlPage::new(2);
        let pending = full_pending(10);
        page.push(pending.clone());

        let entry = page.read_entry_for_cursor(1).unwrap();
        assert_eq!(entry.cursor, 1);
        assert_eq!(entry.config_generation, 1);
        assert_eq!(entry.sequence, pending.sequence);
        assert_eq!(entry.timestamp_ns, pending.timestamp_ns);
        assert_eq!(entry.width, pending.width);
        assert_eq!(entry.height, pending.height);
        assert_eq!(entry.stride, pending.stride);
        assert_eq!(entry.pixel_format, pending.pixel_format);
        assert_eq!(entry.pool_id, pending.pool_id);
        assert_eq!(entry.slot_id, pending.slot_id);
        assert_eq!(entry.payload_offset, pending.payload_offset);
        assert_eq!(entry.payload_len, pending.payload_len);
        assert_eq!(entry.clock_domain, pending.clock_domain);
        assert_eq!(entry.color_space, pending.color_space);
        assert_eq!(entry.sync_kind, pending.sync_kind);
        assert_eq!(entry.damage_kind, pending.damage_kind);
        assert_eq!(entry.damage_base_sequence, pending.damage_base_sequence);
        assert_eq!(entry.dropped_before_publish, pending.dropped_before_publish);
        assert_eq!(entry.producer_drop_count, pending.producer_drop_count);
        assert_eq!(entry.payload_kind, pending.payload_kind);
        assert_eq!(entry.modifier, pending.modifier);
        assert_eq!(entry.fence_id, pending.fence_id);
        assert_eq!(entry.fence_value, pending.fence_value);
        assert_eq!(entry.flags, pending.flags);
    }

    #[test]
    fn read_entry_for_cursor_reports_lapped_cursors_after_wraparound() {
        let mut page = VideoTrackControlPage::new(2);

        page.push(pending(10));
        page.push(pending(11));
        page.push(pending(12));

        assert_eq!(
            page.read_entry_for_cursor(1),
            Err(VideoRingReadError::Lapped {
                requested_cursor: 1,
                oldest_live_cursor: 2,
                latest_cursor: 3,
            })
        );
    }

    #[test]
    fn read_entry_for_cursor_reports_slot_sequence_mismatch() {
        let mut page = VideoTrackControlPage::new(2);

        page.push(pending(10));
        page.set_slot_publication_sequence_for_test(0, 99);

        assert_eq!(
            page.read_entry_for_cursor(1),
            Err(VideoRingReadError::SlotSequenceMismatch {
                requested_cursor: 1,
                first_sequence: 99,
                second_sequence: 99,
            })
        );
    }

    #[test]
    fn read_latest_lossy_entry_returns_newest_entry_after_wraparound() {
        let mut page = VideoTrackControlPage::new(2);

        assert_eq!(page.read_latest_lossy_entry(), Ok(None));

        page.push(pending(10));
        page.push(pending(11));
        page.push(pending(12));

        let latest = page.read_latest_lossy_entry().unwrap().unwrap();
        assert_eq!(latest.cursor, 3);
        assert_eq!(latest.sequence, 12);
    }

    #[test]
    fn unchanged_stream_values_reuse_one_config_generation() {
        let mut page = VideoTrackControlPage::new(4);

        page.push(full_pending(10));
        page.push(full_pending(11));
        page.push(full_pending(12));

        let snapshot = page.snapshot();
        assert_eq!(snapshot.header.config_cursor, 1);
        assert_eq!(snapshot.configs.len(), 1);
        assert_eq!(snapshot.configs[0].config_generation, 1);
        assert_eq!(snapshot.configs[0].width, 1920);
        assert!(snapshot.entries.iter().all(|entry| entry.config_generation == 1));
    }

    #[test]
    fn changed_stream_values_publish_new_config_generation() {
        let mut page = VideoTrackControlPage::new(4);

        page.push(full_pending(10));
        let mut resized = full_pending(11);
        resized.width = 2560;
        resized.stride = 10240;
        page.push(resized);

        let first = page.read_entry_for_cursor(1).unwrap();
        let second = page.read_entry_for_cursor(2).unwrap();
        assert_eq!(first.config_generation, 1);
        assert_eq!((first.width, first.stride), (1920, 7680));
        assert_eq!(second.config_generation, 2);
        assert_eq!((second.width, second.stride), (2560, 10240));
        assert_eq!(page.snapshot().header.config_cursor, 2);
    }

    #[test]
    fn frames_older_than_the_config_ring_report_config_overwritten() {
        // Capacity 8 ring keeps old frames live while more than
        // CONFIG_RING_CAPACITY reconfigures overwrite generation 1.
        let mut page = VideoTrackControlPage::new(8);

        page.push(full_pending(10));
        for round in 0..CONFIG_RING_CAPACITY as u32 {
            let mut resized = full_pending(11 + u64::from(round));
            resized.width = 2560 + round;
            page.push(resized);
        }

        let error = page.read_entry_for_cursor(1).unwrap_err();
        assert_eq!(
            error,
            VideoRingReadError::ConfigOverwritten {
                requested_cursor: 1,
                requested_generation: 1,
                first_generation: 5,
                second_generation: 5,
            }
        );
        // The newest frame still reads cleanly.
        let latest = page.read_latest_lossy_entry().unwrap().unwrap();
        assert_eq!(latest.config_generation, 5);
    }

    #[test]
    fn consumer_cursor_slot_registers_and_roundtrips_release_cursor() {
        let page = VideoTrackControlPage::new(2);

        let slot = page.register_consumer_cursor(7).unwrap();
        page.store_consumer_release_cursor(slot, 7, 3).unwrap();

        let entry = page.consumer_cursor_entry(slot);
        assert_eq!(slot, 0);
        assert_eq!(entry.consumer_id, 7);
        assert_eq!(entry.release_cursor, 3);
    }

    #[test]
    fn consumer_cursor_slot_reuses_existing_consumer_slot() {
        let page = VideoTrackControlPage::new(2);

        let first = page.register_consumer_cursor(7).unwrap();
        let second = page.register_consumer_cursor(7).unwrap();

        assert_eq!(first, second);
        assert_eq!(page.consumer_cursor_entry(first).consumer_id, 7);
    }

    #[test]
    fn consumer_cursor_slot_unregisters_and_zeroes_consumer() {
        let page = VideoTrackControlPage::new(2);
        let slot = page.register_consumer_cursor(7).unwrap();
        page.store_consumer_release_cursor(slot, 7, 3).unwrap();

        page.unregister_consumer_cursor(7);

        let entry = page.consumer_cursor_entry(slot);
        assert_eq!(entry.consumer_id, 0);
        assert_eq!(entry.release_cursor, 0);

        let reused = page.register_consumer_cursor(8).unwrap();
        assert_eq!(reused, slot);
        assert_eq!(page.consumer_cursor_entry(reused).release_cursor, 0);
    }

    #[test]
    fn consumer_release_update_rejects_slot_consumer_mismatch() {
        let page = VideoTrackControlPage::new(2);
        let slot = page.register_consumer_cursor(7).unwrap();

        let error = page.store_consumer_release_cursor(slot, 8, 3).unwrap_err();

        assert!(error.to_string().contains("consumer cursor slot does not match consumer id"));
    }

    #[test]
    fn consumer_acquire_update_rejects_slot_consumer_mismatch() {
        let page = VideoTrackControlPage::new(2);
        let slot = page.register_consumer_cursor(7).unwrap();

        let error = page.store_consumer_acquire_cursor(slot, 8, 1, 1, 0, 1).unwrap_err();

        assert!(error.to_string().contains("consumer cursor slot does not match consumer id"));
    }

    #[test]
    fn consumer_acquire_update_preserves_release_cursor() {
        let page = VideoTrackControlPage::new(2);
        let slot = page.register_consumer_cursor(7).unwrap();
        page.store_consumer_release_cursor(slot, 7, 9).unwrap();

        page.store_consumer_acquire_cursor(slot, 7, 10, 20, 1, 2).unwrap();

        let entry = page.consumer_cursor_entry(slot);
        assert_eq!(entry.last_acquired_cursor, 10);
        assert_eq!(entry.last_acquired_sequence, 20);
        assert_eq!(entry.skipped_count, 1);
        assert_eq!(entry.acquired_count, 2);
        assert_eq!(entry.release_cursor, 9);
    }

    #[test]
    fn read_only_control_page_mapping_validates_header_from_fd() {
        let page = VideoTrackControlPage::new(3);

        let mapped = VideoTrackControlPage::map_read_only(page.try_clone_fd().unwrap(), page.mapped_len()).unwrap();

        let header = mapped.validate_header().unwrap();
        assert_eq!(header.magic, super::VIDEO_TRACK_CONTROL_MAGIC);
        assert_eq!(header.layout_version, super::VIDEO_TRACK_CONTROL_VERSION);
        assert_eq!(header.slot_capacity, 4);
        assert_eq!(mapped.mapped_len(), page.mapped_len());
    }

    #[test]
    fn read_only_control_page_shadow_reads_published_entry_from_fd() {
        let mut page = VideoTrackControlPage::new(2);
        page.push(pending(10));

        let mapped = VideoTrackControlPage::map_read_only(page.try_clone_fd().unwrap(), page.mapped_len()).unwrap();
        let entry = mapped.shadow_read_entry_for_cursor(1).unwrap();

        assert_eq!(entry.cursor, 1);
        assert_eq!(entry.sequence, 10);
        assert_eq!(entry.pool_id, 7);
        assert_eq!(entry.slot_id, 10);
        assert_eq!(entry.payload_offset, 640);
        assert_eq!(entry.payload_len, 4);
    }

    #[test]
    fn read_only_control_page_shadow_reads_full_descriptor_from_fd() {
        let mut page = VideoTrackControlPage::new(2);
        let pending = full_pending(10);
        page.push(pending.clone());

        let mapped = VideoTrackControlPage::map_read_only(page.try_clone_fd().unwrap(), page.mapped_len()).unwrap();
        let entry = mapped.shadow_read_entry_for_cursor(1).unwrap();
        let direct = page.read_entry_for_cursor(1).unwrap();

        assert_eq!(entry, direct);
        assert_eq!(entry.width, pending.width);
        assert_eq!(entry.modifier, pending.modifier);
        assert_eq!(entry.fence_id, pending.fence_id);
        assert_eq!(entry.fence_value, pending.fence_value);
    }
}
