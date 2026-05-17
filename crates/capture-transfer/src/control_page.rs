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
            publication_sequence: self.header.producer_cursor,
            producer_cursor: self.header.producer_cursor,
            sequence: entry.sequence,
            frame_key: entry.frame_key,
            pool_id: entry.pool_id,
            slot_id: entry.slot_id,
            slot_generation: entry.slot_generation,
            payload_offset: entry.payload_offset,
            payload_len: entry.payload_len,
        };
        self.header.latest_sequence = entry.sequence;
        self.header.latest_index = index as u64;
        self.header.len = (self.header.len + 1).min(self.header.capacity);
    }

    #[must_use]
    pub fn latest(&self) -> Option<&VideoRingEntry> {
        if self.header.latest_index == EMPTY_LATEST_INDEX {
            return None;
        }
        let index = usize::try_from(self.header.latest_index).ok()?;
        self.entries.get(index)
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
    use super::{EMPTY_LATEST_INDEX, PendingVideoRingEntry, VideoTrackControlPage};

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
        assert_eq!(page.latest(), None);
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
        assert_eq!(page.latest().map(|entry| (entry.producer_cursor, entry.sequence)), Some((3, 12)));
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
}
