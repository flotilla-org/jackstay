use thiserror::Error;

pub const EMPTY_LATEST_INDEX: u64 = u64::MAX;
pub const VIDEO_TRACK_CONTROL_MAGIC: u64 = u64::from_le_bytes(*b"JSVTRK01");
pub const VIDEO_TRACK_CONTROL_VERSION: u64 = 1;

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

#[repr(C)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoTrackControlSnapshot {
    pub header: VideoTrackControlHeader,
    pub entries: Vec<VideoRingEntry>,
}

#[derive(Debug)]
pub struct VideoTrackControlPage {
    header: VideoTrackControlHeader,
    entries: Vec<VideoRingEntry>,
}

impl VideoTrackControlPage {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = ring_capacity_for(capacity);
        Self {
            header: VideoTrackControlHeader {
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
            },
            entries: vec![VideoRingEntry::default(); capacity],
        }
    }

    pub fn push(&mut self, entry: PendingVideoRingEntry) {
        // A u64 producer cursor is effectively inexhaustible for display-rate
        // capture, but saturation would make ring-slot selection ambiguous.
        debug_assert!(self.header.producer_cursor < u64::MAX);
        self.header.producer_cursor = self.header.producer_cursor.saturating_add(1);
        let index = ((self.header.producer_cursor - 1) & self.header.index_mask) as usize;
        self.entries[index] = VideoRingEntry {
            publication_sequence: 0,
            producer_cursor: self.header.producer_cursor,
            sequence: entry.sequence,
            frame_key: entry.frame_key,
            pool_id: entry.pool_id,
            slot_id: entry.slot_id,
            slot_generation: entry.slot_generation,
            payload_offset: entry.payload_offset,
            payload_len: entry.payload_len,
        };
        self.entries[index].publication_sequence = self.header.producer_cursor;
        self.header.latest_sequence = entry.sequence;
        self.header.latest_index = index as u64;
        self.header.len = (self.header.len + 1).min(self.header.capacity);
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

        let index = ((cursor - 1) & self.header.index_mask) as usize;
        let first_sequence = self.entries[index].publication_sequence;
        let entry = self.entries[index].clone();
        let second_sequence = self.entries[index].publication_sequence;
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
        (self.header.len > 0).then_some(self.header.producer_cursor)
    }

    #[must_use]
    pub fn oldest_live_cursor(&self) -> Option<u64> {
        if self.header.len == 0 {
            return None;
        }
        debug_assert!(self.header.producer_cursor >= self.header.len);
        Some(self.header.producer_cursor - self.header.len + 1)
    }

    #[must_use]
    pub fn cursor_lapped(&self, expected_cursor: u64) -> bool {
        self.oldest_live_cursor().is_some_and(|oldest| expected_cursor < oldest)
    }

    #[must_use]
    pub fn ring_snapshot(&self) -> Vec<VideoRingEntry> {
        let len = self.header.len as usize;
        let start = if len == self.entries.len() {
            (self.header.producer_cursor & self.header.index_mask) as usize
        } else {
            0
        };
        (0..len)
            .map(|offset| {
                let index = (start + offset) & self.header.index_mask as usize;
                self.entries[index].clone()
            })
            .collect()
    }

    #[must_use]
    pub fn snapshot(&self) -> VideoTrackControlSnapshot {
        VideoTrackControlSnapshot {
            header: self.header,
            entries: self.ring_snapshot(),
        }
    }
}

fn ring_capacity_for(requested: usize) -> usize {
    requested.max(1).next_power_of_two()
}

#[cfg(test)]
mod tests {
    use super::{EMPTY_LATEST_INDEX, PendingVideoRingEntry, VideoRingReadError, VideoTrackControlPage};

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
        page.entries[0].publication_sequence = 99;

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
        page.entries[0].publication_sequence = 0;

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
