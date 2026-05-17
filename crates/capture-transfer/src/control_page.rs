use std::{
    os::fd::OwnedFd,
    sync::atomic::{AtomicU64, Ordering},
};

use thiserror::Error;

use crate::{
    error::{CaptureTransferError, Result as CaptureResult},
    shm::SharedMemorySegment,
};

pub const CONTROL_PAGE_ALIGNMENT: usize = 128;
pub const EMPTY_LATEST_INDEX: u64 = u64::MAX;
pub const VIDEO_TRACK_CONTROL_MAGIC: u64 = u64::from_le_bytes(*b"JSVTRK01");
pub const VIDEO_TRACK_CONTROL_VERSION: u64 = 1;
const VIDEO_RING_PUBLICATION_SEQUENCE_LEN: usize = std::mem::size_of::<u64>();
const _: () = assert!(std::mem::offset_of!(VideoRingEntry, publication_sequence) == 0);

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoTrackControlHeader {
    pub magic: u64,
    pub version: u64,
    pub header_len: u64,
    pub entry_len: u64,
    pub capacity: u64,
    pub index_mask: u64,
    pub producer_cursor: u64,
    pub latest_sequence: u64,
    pub latest_index: u64,
    pub len: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoTrackControlHeaderField {
    ProducerCursor,
    LatestSequence,
    LatestIndex,
    Len,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoRingEntry {
    pub publication_sequence: u64,
    pub producer_cursor: u64,
    pub sequence: u64,
    pub frame_key: u64,
    pub pool_id: u64,
    pub slot_id: u64,
    pub slot_generation: u64,
    pub payload_offset: u64,
    pub payload_len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingVideoRingEntry {
    pub sequence: u64,
    pub frame_key: u64,
    pub pool_id: u64,
    pub slot_id: u64,
    pub slot_generation: u64,
    pub payload_offset: u64,
    pub payload_len: u64,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoTrackControlLayout {
    pub header_offset: usize,
    pub entries_offset: usize,
    pub entry_len: usize,
    pub capacity: usize,
    pub byte_len: usize,
}

impl VideoTrackControlLayout {
    #[must_use]
    pub fn for_capacity(capacity: usize) -> Self {
        let entries_offset = align_up(std::mem::size_of::<VideoTrackControlHeader>(), CONTROL_PAGE_ALIGNMENT);
        let entry_len = std::mem::size_of::<VideoRingEntry>();
        let entries_len = capacity
            .checked_mul(entry_len)
            .expect("video control page entry byte length overflow");
        let byte_len = entries_offset
            .checked_add(entries_len)
            .expect("video control page byte length overflow");
        Self {
            header_offset: 0,
            entries_offset,
            entry_len,
            capacity,
            byte_len,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoTrackControlSnapshot {
    pub header: VideoTrackControlHeader,
    pub entries: Vec<VideoRingEntry>,
}

#[derive(Debug)]
pub struct VideoTrackControlPage {
    layout: VideoTrackControlLayout,
    storage: SharedMemorySegment,
}

impl VideoTrackControlPage {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = ring_capacity_for(capacity);
        let layout = VideoTrackControlLayout::for_capacity(capacity);
        let storage = SharedMemorySegment::new(layout.byte_len).expect("control page mapped storage allocation failed");
        let page = Self { layout, storage };
        page.store_initial_header(VideoTrackControlHeader {
            magic: VIDEO_TRACK_CONTROL_MAGIC,
            version: VIDEO_TRACK_CONTROL_VERSION,
            header_len: std::mem::size_of::<VideoTrackControlHeader>() as u64,
            entry_len: std::mem::size_of::<VideoRingEntry>() as u64,
            capacity: capacity as u64,
            index_mask: (capacity - 1) as u64,
            producer_cursor: 0,
            latest_sequence: 0,
            latest_index: EMPTY_LATEST_INDEX,
            len: 0,
        });
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
        let mut page = Self {
            layout: VideoTrackControlLayout::for_capacity(1),
            storage,
        };
        let initial_header: VideoTrackControlHeader = page.read_at(0);
        let capacity = validate_control_header(&initial_header, map_len)?;
        page.layout = VideoTrackControlLayout::for_capacity(capacity);
        page.validate_header()?;
        Ok(page)
    }

    pub fn validate_header(&self) -> CaptureResult<VideoTrackControlHeader> {
        let header = self.header();
        validate_control_header(&header, self.storage.len())?;
        Ok(header)
    }

    pub fn shadow_read_entry_for_cursor(&self, cursor: u64) -> CaptureResult<VideoRingEntry> {
        self.validate_header()?;
        self.read_entry_for_cursor(cursor)
            .map_err(|source| CaptureTransferError::SharedMemory {
                operation: "read-control-page",
                message: source.to_string(),
            })
    }

    #[cfg(test)]
    fn raw_entry_for_test(&self, index: usize) -> VideoRingEntry {
        self.entry(index)
    }

    #[cfg(test)]
    fn set_entry_publication_sequence_for_test(&self, index: usize, publication_sequence: u64) {
        self.store_entry_publication_sequence(index, publication_sequence, Ordering::Release);
    }

    #[cfg(test)]
    fn header_field_offset_for_test(&self, field: VideoTrackControlHeaderField) -> usize {
        self.header_field_offset(field)
    }

    #[cfg(test)]
    fn entry_publication_sequence_offset_for_test(&self, index: usize) -> usize {
        self.entry_publication_sequence_offset(index)
    }

    #[cfg(test)]
    fn load_header_producer_cursor_for_test(&self, ordering: Ordering) -> u64 {
        self.load_header_producer_cursor(ordering)
    }

    #[cfg(test)]
    fn store_header_producer_cursor_for_test(&self, value: u64, ordering: Ordering) {
        self.store_header_producer_cursor(value, ordering);
    }

    #[cfg(test)]
    fn load_entry_publication_sequence_for_test(&self, index: usize, ordering: Ordering) -> u64 {
        self.load_entry_publication_sequence(index, ordering)
    }

    #[cfg(test)]
    fn store_entry_publication_sequence_for_test(&self, index: usize, value: u64, ordering: Ordering) {
        self.store_entry_publication_sequence(index, value, ordering);
    }

    fn header(&self) -> VideoTrackControlHeader {
        VideoTrackControlHeader {
            magic: self.read_header_u64(std::mem::offset_of!(VideoTrackControlHeader, magic)),
            version: self.read_header_u64(std::mem::offset_of!(VideoTrackControlHeader, version)),
            header_len: self.read_header_u64(std::mem::offset_of!(VideoTrackControlHeader, header_len)),
            entry_len: self.read_header_u64(std::mem::offset_of!(VideoTrackControlHeader, entry_len)),
            capacity: self.read_header_u64(std::mem::offset_of!(VideoTrackControlHeader, capacity)),
            index_mask: self.read_header_u64(std::mem::offset_of!(VideoTrackControlHeader, index_mask)),
            producer_cursor: self.load_header_producer_cursor(Ordering::Acquire),
            latest_sequence: self.load_header_latest_sequence(Ordering::Acquire),
            latest_index: self.load_header_latest_index(Ordering::Acquire),
            len: self.load_header_len(Ordering::Acquire),
        }
    }

    fn store_initial_header(&self, header: VideoTrackControlHeader) {
        debug_assert_eq!(header.producer_cursor, 0);
        debug_assert_eq!(header.latest_sequence, 0);
        debug_assert_eq!(header.latest_index, EMPTY_LATEST_INDEX);
        debug_assert_eq!(header.len, 0);
        // This is the one whole-header write, performed before the page can be
        // shared with any reader. After init, hot fields use atomic helpers.
        self.write_at(self.layout.header_offset, header);
    }

    fn entry(&self, index: usize) -> VideoRingEntry {
        assert!(index < self.layout.capacity);
        // Snapshot/test reads are lossy observations. Authoritative cursor reads
        // use `read_entry_for_cursor`, which performs the second sequence load.
        // TODO(cross-process): do not use this as an authoritative reader after
        // fd passing; it does not re-load the sequence after descriptor copy.
        let publication_sequence = self.load_entry_publication_sequence(index, Ordering::Acquire);
        self.read_entry_descriptor(index, publication_sequence)
    }

    fn read_entry_descriptor(&self, index: usize, publication_sequence: u64) -> VideoRingEntry {
        assert!(index < self.layout.capacity);
        let mut entry = VideoRingEntry {
            publication_sequence,
            ..VideoRingEntry::default()
        };
        let descriptor_offset = self.entry_offset(index) + VIDEO_RING_PUBLICATION_SEQUENCE_LEN;
        let descriptor_len = self.layout.entry_len - VIDEO_RING_PUBLICATION_SEQUENCE_LEN;
        let bytes = self.storage.slice_at(descriptor_offset, descriptor_len);
        // SAFETY: `publication_sequence` is field 0. This copies the rest of
        // the repr(C) descriptor into a local value without reading the atomic
        // sequence as plain bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                std::ptr::addr_of_mut!(entry).cast::<u8>().add(VIDEO_RING_PUBLICATION_SEQUENCE_LEN),
                bytes.len(),
            );
        }
        entry
    }

    fn store_entry_descriptor(&self, index: usize, entry: VideoRingEntry) {
        assert!(index < self.layout.capacity);
        let descriptor_offset = self.entry_offset(index) + VIDEO_RING_PUBLICATION_SEQUENCE_LEN;
        let descriptor_len = self.layout.entry_len - VIDEO_RING_PUBLICATION_SEQUENCE_LEN;
        self.storage.with_slice_at_mut(descriptor_offset, descriptor_len, |bytes| {
            // SAFETY: `publication_sequence` is field 0. This copies the rest
            // of the repr(C) descriptor without touching the atomic sequence.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    std::ptr::addr_of!(entry).cast::<u8>().add(VIDEO_RING_PUBLICATION_SEQUENCE_LEN),
                    bytes.as_mut_ptr(),
                    bytes.len(),
                );
            }
        });
    }

    fn entry_publication_sequence(&self, index: usize) -> u64 {
        self.load_entry_publication_sequence(index, Ordering::Acquire)
    }

    fn load_entry_publication_sequence(&self, index: usize, ordering: Ordering) -> u64 {
        assert!(index < self.layout.capacity);
        self.load_u64_atomic(self.entry_publication_sequence_offset(index), ordering)
    }

    fn store_entry_publication_sequence(&self, index: usize, publication_sequence: u64, ordering: Ordering) {
        assert!(index < self.layout.capacity);
        self.store_u64_atomic(self.entry_publication_sequence_offset(index), publication_sequence, ordering);
    }

    fn entry_publication_sequence_offset(&self, index: usize) -> usize {
        // `publication_sequence` is field 0 of the repr(C) entry. Keep this
        // offset explicit because future atomics will load this field directly.
        self.entry_offset(index)
    }

    fn load_header_producer_cursor(&self, ordering: Ordering) -> u64 {
        self.load_u64_atomic(self.header_field_offset(VideoTrackControlHeaderField::ProducerCursor), ordering)
    }

    fn store_header_producer_cursor(&self, value: u64, ordering: Ordering) {
        self.store_u64_atomic(
            self.header_field_offset(VideoTrackControlHeaderField::ProducerCursor),
            value,
            ordering,
        );
    }

    fn load_header_latest_sequence(&self, ordering: Ordering) -> u64 {
        self.load_u64_atomic(self.header_field_offset(VideoTrackControlHeaderField::LatestSequence), ordering)
    }

    fn store_header_latest_sequence(&self, value: u64, ordering: Ordering) {
        self.store_u64_atomic(
            self.header_field_offset(VideoTrackControlHeaderField::LatestSequence),
            value,
            ordering,
        );
    }

    fn load_header_latest_index(&self, ordering: Ordering) -> u64 {
        self.load_u64_atomic(self.header_field_offset(VideoTrackControlHeaderField::LatestIndex), ordering)
    }

    fn store_header_latest_index(&self, value: u64, ordering: Ordering) {
        self.store_u64_atomic(self.header_field_offset(VideoTrackControlHeaderField::LatestIndex), value, ordering);
    }

    fn load_header_len(&self, ordering: Ordering) -> u64 {
        self.load_u64_atomic(self.header_field_offset(VideoTrackControlHeaderField::Len), ordering)
    }

    fn store_header_len(&self, value: u64, ordering: Ordering) {
        self.store_u64_atomic(self.header_field_offset(VideoTrackControlHeaderField::Len), value, ordering);
    }

    fn header_field_offset(&self, field: VideoTrackControlHeaderField) -> usize {
        let field_offset = match field {
            VideoTrackControlHeaderField::ProducerCursor => std::mem::offset_of!(VideoTrackControlHeader, producer_cursor),
            VideoTrackControlHeaderField::LatestSequence => std::mem::offset_of!(VideoTrackControlHeader, latest_sequence),
            VideoTrackControlHeaderField::LatestIndex => std::mem::offset_of!(VideoTrackControlHeader, latest_index),
            VideoTrackControlHeaderField::Len => std::mem::offset_of!(VideoTrackControlHeader, len),
        };
        self.layout.header_offset + field_offset
    }

    fn read_header_u64(&self, field_offset: usize) -> u64 {
        self.read_at(self.layout.header_offset + field_offset)
    }

    fn entry_offset(&self, index: usize) -> usize {
        self.layout.entries_offset + index * self.layout.entry_len
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

    pub fn push(&mut self, entry: PendingVideoRingEntry) {
        let mut header = self.header();
        // A u64 producer cursor is effectively inexhaustible for display-rate
        // capture, but saturation would make ring-slot selection ambiguous.
        debug_assert!(header.producer_cursor < u64::MAX);
        header.producer_cursor = header.producer_cursor.saturating_add(1);
        let index = ((header.producer_cursor - 1) & header.index_mask) as usize;
        let ring_entry = VideoRingEntry {
            publication_sequence: 0,
            producer_cursor: header.producer_cursor,
            sequence: entry.sequence,
            frame_key: entry.frame_key,
            pool_id: entry.pool_id,
            slot_id: entry.slot_id,
            slot_generation: entry.slot_generation,
            payload_offset: entry.payload_offset,
            payload_len: entry.payload_len,
        };
        // Descriptor fields are still plain copied values. The surrounding
        // publication sequence stores are the release/acquire boundary that a
        // future cross-process reader must preserve.
        self.store_entry_publication_sequence(index, 0, Ordering::Relaxed);
        self.store_entry_descriptor(index, ring_entry);
        self.store_entry_publication_sequence(index, header.producer_cursor, Ordering::Release);
        let len = (header.len + 1).min(header.capacity);
        self.store_header_latest_sequence(entry.sequence, Ordering::Relaxed);
        self.store_header_latest_index(index as u64, Ordering::Relaxed);
        self.store_header_len(len, Ordering::Release);
        self.store_header_producer_cursor(header.producer_cursor, Ordering::Release);
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

        let header = self.header();
        let index = ((cursor - 1) & header.index_mask) as usize;
        let first_sequence = self.entry_publication_sequence(index);
        let entry = self.read_entry_descriptor(index, first_sequence);
        let second_sequence = self.entry_publication_sequence(index);
        if first_sequence != cursor || second_sequence != cursor {
            return Err(VideoRingReadError::SlotSequenceMismatch {
                requested_cursor: cursor,
                first_sequence,
                second_sequence,
            });
        }
        Ok(entry)
    }

    pub fn read_latest_lossy_entry(&self) -> Result<Option<VideoRingEntry>, VideoRingReadError> {
        let Some(cursor) = self.latest_cursor() else {
            return Ok(None);
        };
        self.read_entry_for_cursor(cursor).map(Some)
    }

    #[must_use]
    pub fn latest_cursor(&self) -> Option<u64> {
        let producer_cursor = self.load_header_producer_cursor(Ordering::Acquire);
        (producer_cursor > 0).then_some(producer_cursor)
    }

    #[must_use]
    pub fn oldest_live_cursor(&self) -> Option<u64> {
        let producer_cursor = self.load_header_producer_cursor(Ordering::Acquire);
        let len = self.load_header_len(Ordering::Acquire);
        if producer_cursor == 0 || len == 0 {
            return None;
        }
        debug_assert!(producer_cursor >= len);
        Some(producer_cursor - len + 1)
    }

    #[must_use]
    pub fn cursor_lapped(&self, expected_cursor: u64) -> bool {
        self.oldest_live_cursor().is_some_and(|oldest| expected_cursor < oldest)
    }

    #[must_use]
    pub fn ring_snapshot(&self) -> Vec<VideoRingEntry> {
        let header = self.header();
        let len = header.len as usize;
        let start = if len == self.layout.capacity {
            (header.producer_cursor & header.index_mask) as usize
        } else {
            0
        };
        (0..len)
            .map(|offset| {
                let index = (start + offset) & (header.index_mask as usize);
                self.entry(index)
            })
            .collect()
    }

    #[must_use]
    pub fn snapshot(&self) -> VideoTrackControlSnapshot {
        VideoTrackControlSnapshot {
            header: self.header(),
            entries: self.ring_snapshot(),
        }
    }
}

fn ring_capacity_for(requested: usize) -> usize {
    requested.max(1).next_power_of_two()
}

fn validate_control_header(header: &VideoTrackControlHeader, map_len: usize) -> CaptureResult<usize> {
    if header.magic != VIDEO_TRACK_CONTROL_MAGIC {
        return Err(invalid_control_page("invalid magic"));
    }
    if header.version != VIDEO_TRACK_CONTROL_VERSION {
        return Err(invalid_control_page("invalid version"));
    }
    if header.header_len != std::mem::size_of::<VideoTrackControlHeader>() as u64 {
        return Err(invalid_control_page("invalid header length"));
    }
    if header.entry_len != std::mem::size_of::<VideoRingEntry>() as u64 {
        return Err(invalid_control_page("invalid entry length"));
    }
    let capacity = usize::try_from(header.capacity).map_err(|_| invalid_control_page("capacity does not fit usize"))?;
    if capacity == 0 || !capacity.is_power_of_two() {
        return Err(invalid_control_page("capacity must be a non-zero power of two"));
    }
    if header.index_mask != header.capacity - 1 {
        return Err(invalid_control_page("index mask does not match capacity"));
    }
    let layout = VideoTrackControlLayout::for_capacity(capacity);
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

const fn align_up(value: usize, alignment: usize) -> usize {
    debug_assert!(alignment.is_power_of_two());
    (value + alignment - 1) & !(alignment - 1)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::{
        CONTROL_PAGE_ALIGNMENT, EMPTY_LATEST_INDEX, PendingVideoRingEntry, VideoRingEntry, VideoRingReadError, VideoTrackControlHeader,
        VideoTrackControlPage,
    };

    fn pending(sequence: u64, frame_key: u64) -> PendingVideoRingEntry {
        PendingVideoRingEntry {
            sequence,
            frame_key,
            pool_id: 7,
            slot_id: sequence,
            slot_generation: 3,
            payload_offset: sequence * 64,
            payload_len: 4,
        }
    }

    #[test]
    fn control_page_layout_aligns_entries_after_header() {
        let page = VideoTrackControlPage::new(3);

        let layout = page.layout();
        assert_eq!(layout.header_offset, 0);
        assert_eq!(layout.entries_offset % CONTROL_PAGE_ALIGNMENT, 0);
        assert!(layout.entries_offset >= std::mem::size_of::<VideoTrackControlHeader>());
        assert_eq!(layout.entry_len, std::mem::size_of::<VideoRingEntry>());
        assert_eq!(layout.capacity, 4);
        assert_eq!(layout.byte_len, layout.entries_offset + layout.capacity * layout.entry_len);
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
    fn hot_control_fields_are_aligned_for_atomic_u64_access() {
        let page = VideoTrackControlPage::new(2);

        assert_eq!(
            page.header_field_offset_for_test(super::VideoTrackControlHeaderField::ProducerCursor)
                % std::mem::align_of::<std::sync::atomic::AtomicU64>(),
            0
        );
        assert_eq!(
            page.header_field_offset_for_test(super::VideoTrackControlHeaderField::LatestSequence)
                % std::mem::align_of::<std::sync::atomic::AtomicU64>(),
            0
        );
        assert_eq!(
            page.header_field_offset_for_test(super::VideoTrackControlHeaderField::LatestIndex)
                % std::mem::align_of::<std::sync::atomic::AtomicU64>(),
            0
        );
        assert_eq!(
            page.header_field_offset_for_test(super::VideoTrackControlHeaderField::Len)
                % std::mem::align_of::<std::sync::atomic::AtomicU64>(),
            0
        );
        assert_eq!(
            page.entry_publication_sequence_offset_for_test(0) % std::mem::align_of::<std::sync::atomic::AtomicU64>(),
            0
        );
    }

    #[test]
    fn atomic_header_producer_cursor_roundtrips_through_mapped_page() {
        let page = VideoTrackControlPage::new(2);

        page.store_header_producer_cursor_for_test(17, Ordering::Release);

        assert_eq!(page.load_header_producer_cursor_for_test(Ordering::Acquire), 17);
        assert_eq!(page.snapshot().header.producer_cursor, 17);
    }

    #[test]
    fn atomic_entry_publication_sequence_roundtrips_through_mapped_page() {
        let page = VideoTrackControlPage::new(2);

        page.store_entry_publication_sequence_for_test(0, 23, Ordering::Release);

        assert_eq!(page.load_entry_publication_sequence_for_test(0, Ordering::Acquire), 23);
        assert_eq!(page.raw_entry_for_test(0).publication_sequence, 23);
    }

    #[test]
    fn read_only_control_page_mapping_validates_header_from_fd() {
        let page = VideoTrackControlPage::new(3);

        let mapped = VideoTrackControlPage::map_read_only(page.try_clone_fd().unwrap(), page.mapped_len()).unwrap();

        let header = mapped.validate_header().unwrap();
        assert_eq!(header.magic, super::VIDEO_TRACK_CONTROL_MAGIC);
        assert_eq!(header.version, super::VIDEO_TRACK_CONTROL_VERSION);
        assert_eq!(header.capacity, 4);
        assert_eq!(mapped.mapped_len(), page.mapped_len());
    }

    #[test]
    fn read_only_control_page_shadow_reads_published_entry_from_fd() {
        let mut page = VideoTrackControlPage::new(2);
        page.push(pending(10, 100));

        let mapped = VideoTrackControlPage::map_read_only(page.try_clone_fd().unwrap(), page.mapped_len()).unwrap();
        let entry = mapped.shadow_read_entry_for_cursor(1).unwrap();

        assert_eq!(entry.publication_sequence, 1);
        assert_eq!(entry.producer_cursor, 1);
        assert_eq!(entry.sequence, 10);
        assert_eq!(entry.frame_key, 100);
        assert_eq!(entry.pool_id, 7);
        assert_eq!(entry.slot_id, 10);
        assert_eq!(entry.payload_offset, 640);
        assert_eq!(entry.payload_len, 4);
    }

    #[test]
    fn control_page_mapped_entries_start_zeroed() {
        let page = VideoTrackControlPage::new(2);

        assert_eq!(page.raw_entry_for_test(0), VideoRingEntry::default());
        assert_eq!(page.raw_entry_for_test(1), VideoRingEntry::default());
    }

    #[test]
    fn latest_cursor_returns_none_on_empty_page() {
        let page = VideoTrackControlPage::new(2);

        assert_eq!(page.latest_cursor(), None);
    }

    #[test]
    fn empty_track_control_page_uses_fixed_empty_sentinels() {
        let page = VideoTrackControlPage::new(0);

        let snapshot = page.snapshot();
        assert_ne!(snapshot.header.magic, 0);
        assert_ne!(snapshot.header.version, 0);
        assert_eq!(
            snapshot.header.header_len,
            std::mem::size_of::<super::VideoTrackControlHeader>() as u64
        );
        assert_eq!(snapshot.header.entry_len, std::mem::size_of::<super::VideoRingEntry>() as u64);
        assert_eq!(snapshot.header.capacity, 1);
        assert_eq!(snapshot.header.index_mask, 0);
        assert_eq!(snapshot.header.producer_cursor, 0);
        assert_eq!(snapshot.header.latest_sequence, 0);
        assert_eq!(snapshot.header.latest_index, EMPTY_LATEST_INDEX);
        assert_eq!(snapshot.header.len, 0);
        assert_eq!(page.read_latest_lossy_entry(), Ok(None));
        assert!(snapshot.entries.is_empty());
    }

    #[test]
    fn track_control_page_rounds_capacity_to_power_of_two() {
        let page = VideoTrackControlPage::new(3);

        let snapshot = page.snapshot();
        assert_eq!(snapshot.header.capacity, 4);
        assert_eq!(snapshot.header.index_mask, 3);
    }

    #[test]
    fn track_control_page_wraps_entries_in_oldest_to_newest_order() {
        let mut page = VideoTrackControlPage::new(2);

        page.push(pending(10, 100));
        page.push(pending(11, 101));
        page.push(pending(12, 102));

        let snapshot = page.snapshot();
        assert_eq!(snapshot.header.capacity, 2);
        assert_eq!(snapshot.header.producer_cursor, 3);
        assert_eq!(snapshot.header.latest_sequence, 12);
        assert_eq!(snapshot.header.latest_index, 0);
        assert_eq!(snapshot.header.len, 2);
        assert_eq!(
            snapshot
                .entries
                .iter()
                .map(|entry| { (entry.publication_sequence, entry.producer_cursor, entry.sequence, entry.frame_key,) })
                .collect::<Vec<_>>(),
            vec![(2, 2, 11, 101), (3, 3, 12, 102)]
        );
        let latest = page.read_entry_for_cursor(3).unwrap();
        assert_eq!((latest.producer_cursor, latest.sequence), (3, 12));
    }

    #[test]
    fn track_control_page_reports_latest_lossy_lap_state() {
        let mut page = VideoTrackControlPage::new(3);

        assert_eq!(page.oldest_live_cursor(), None);
        assert_eq!(page.latest_cursor(), None);
        assert!(!page.cursor_lapped(1));

        page.push(pending(10, 100));
        page.push(pending(11, 101));

        assert_eq!(page.oldest_live_cursor(), Some(1));
        assert_eq!(page.latest_cursor(), Some(2));
        assert!(!page.cursor_lapped(1));
        assert!(!page.cursor_lapped(2));

        page.push(pending(12, 102));
        page.push(pending(13, 103));
        page.push(pending(14, 104));

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

        page.push(pending(10, 100));

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

        page.push(pending(10, 100));
        page.push(pending(11, 101));

        let entry = page.read_entry_for_cursor(2).unwrap();
        assert_eq!(entry.publication_sequence, 2);
        assert_eq!(entry.producer_cursor, 2);
        assert_eq!(entry.sequence, 11);
        assert_eq!(entry.frame_key, 101);
        assert_eq!(entry.pool_id, 7);
        assert_eq!(entry.slot_id, 11);
        assert_eq!(entry.slot_generation, 3);
        assert_eq!(entry.payload_offset, 704);
        assert_eq!(entry.payload_len, 4);
    }

    #[test]
    fn read_entry_for_cursor_reports_lapped_cursors_after_wraparound() {
        let mut page = VideoTrackControlPage::new(2);

        page.push(pending(10, 100));
        page.push(pending(11, 101));
        page.push(pending(12, 102));

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

        page.push(pending(10, 100));
        page.set_entry_publication_sequence_for_test(0, 99);

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

        page.push(pending(10, 100));
        page.push(pending(11, 101));
        page.push(pending(12, 102));

        let latest = page.read_latest_lossy_entry().unwrap().unwrap();
        assert_eq!(latest.publication_sequence, 3);
        assert_eq!(latest.producer_cursor, 3);
        assert_eq!(latest.sequence, 12);
        assert_eq!(latest.frame_key, 102);
    }

    #[test]
    fn read_latest_lossy_entry_preserves_slot_sequence_mismatch() {
        let mut page = VideoTrackControlPage::new(2);

        page.push(pending(10, 100));
        page.set_entry_publication_sequence_for_test(0, 0);

        assert_eq!(
            page.read_latest_lossy_entry(),
            Err(VideoRingReadError::SlotSequenceMismatch {
                requested_cursor: 1,
                first_sequence: 0,
                second_sequence: 0,
            })
        );
    }
}
