//! Persistent control is separate from replaceable resource storage. Only
//! immutable headers are copied plainly; mutable shared words are atomic and
//! descriptors/payloads are read only after the parent module validates a claim.
use std::{
    cell::UnsafeCell,
    mem::{align_of, size_of},
    os::fd::OwnedFd,
    sync::atomic::AtomicU64,
};

use super::{
    ArenaError, FrameDescriptor, HEADER_LEN, LATEST, RECONFIGURATION_EPOCH, ResourceRecord, TERMINAL, VERSION, checked_add, checked_mul,
    page_rounded,
};
use crate::shm::SharedMemorySegment;
const RESOURCE_MAGIC: u64 = u64::from_le_bytes(*b"JSRES001");
const CONTROL_MAGIC: u64 = u64::from_le_bytes(*b"JSCTL001");

#[repr(C)]
#[derive(Clone, Copy)]
struct ResourceHeader {
    magic: u64,
    version: u64,
    resources: u64,
    history: u64,
    payload_capacity: u64,
    map_len: u64,
    records_offset: u64,
    payload_offset: u64,
    scope: [u8; 16],
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ResourceLayout {
    pub(super) resources: usize,
    pub(super) history: usize,
    pub(super) payload_capacity: usize,
    pub(super) records: usize,
    pub(super) payload: usize,
    pub(super) len: usize,
}

impl ResourceLayout {
    pub(super) fn new(resources: usize, history: usize, payload_capacity: usize) -> Result<Self, ArenaError> {
        if history == 0 || resources <= history {
            return Err(ArenaError::Configuration("resources must exceed a nonzero history"));
        }
        let records = HEADER_LEN;
        let payload = checked_add(records, checked_mul(resources, size_of::<ResourceRecord>())?)?;
        let len = page_rounded(checked_add(payload, checked_mul(resources, payload_capacity)?)?)?;
        Ok(Self {
            resources,
            history,
            payload_capacity,
            records,
            payload,
            len,
        })
    }

    pub(super) fn record_offset(self, index: usize) -> usize {
        assert!(index < self.resources);
        self.records + index * size_of::<ResourceRecord>()
    }

    pub(super) fn payload_offset(self, index: usize) -> usize {
        assert!(index < self.resources);
        self.payload + index * self.payload_capacity
    }
}

#[derive(Debug)]
pub(super) struct ResourceMap {
    pub(super) storage: SharedMemorySegment,
    pub(super) layout: ResourceLayout,
}

impl ResourceMap {
    pub(super) fn new(layout: ResourceLayout, scope: [u8; 16]) -> Result<Self, ArenaError> {
        let storage = SharedMemorySegment::new(layout.len)?;
        let header = ResourceHeader {
            magic: RESOURCE_MAGIC,
            version: VERSION,
            resources: layout.resources as u64,
            history: layout.history as u64,
            payload_capacity: layout.payload_capacity as u64,
            map_len: layout.len as u64,
            records_offset: layout.records as u64,
            payload_offset: layout.payload as u64,
            scope,
        };
        // SAFETY: fresh unpublished writable mapping; all typed objects are
        // aligned, bounded by ResourceLayout, and initialized before any fd transfer.
        unsafe {
            storage.as_ptr().cast_mut().cast::<ResourceHeader>().write(header);
            for index in 0..layout.resources {
                storage
                    .as_ptr()
                    .add(layout.record_offset(index))
                    .cast_mut()
                    .cast::<ResourceRecord>()
                    .write(ResourceRecord {
                        state: AtomicU64::new(0),
                        descriptor: UnsafeCell::new(FrameDescriptor::default()),
                    });
            }
        }
        Ok(Self { storage, layout })
    }

    pub(super) fn map(fd: OwnedFd, expected: ResourceLayout, scope: [u8; 16]) -> Result<Self, ArenaError> {
        let storage = SharedMemorySegment::map_read_only(fd, expected.len)?;
        // SAFETY: the opaque grant names an initialized arena. ResourceHeader bytes
        // are immutable after creation; no mutable atomic fields are copied.
        let header = unsafe { storage.as_ptr().cast::<ResourceHeader>().read() };
        if header.magic != RESOURCE_MAGIC
            || header.scope != scope
            || header.version != VERSION
            || header.map_len != expected.len as u64
            || header.resources != expected.resources as u64
            || header.history != expected.history as u64
            || header.payload_capacity != expected.payload_capacity as u64
            || header.records_offset != expected.records as u64
            || header.payload_offset != expected.payload as u64
        {
            return Err(ArenaError::Mapping("arena header disagrees with grant"));
        }
        Ok(Self { storage, layout: expected })
    }

    fn word(&self, offset: usize) -> &AtomicU64 {
        assert!(offset % align_of::<AtomicU64>() == 0 && offset + 8 <= self.layout.len);
        // SAFETY: every call names a initialized atomic word, never plain
        // descriptor storage. No reference spans other mutable map contents.
        unsafe { &*self.storage.as_ptr().add(offset).cast::<AtomicU64>() }
    }

    pub(super) fn state(&self, index: usize) -> &AtomicU64 {
        self.word(self.layout.record_offset(index))
    }

    pub(super) fn descriptor_ptr(&self, index: usize) -> *mut FrameDescriptor {
        // SAFETY: ResourceLayout bounds and aligns the record. Taking a raw field
        // address does not read or borrow the concurrently protected contents.
        unsafe {
            let record = self.storage.as_ptr().add(self.layout.record_offset(index)).cast::<ResourceRecord>();
            UnsafeCell::raw_get(std::ptr::addr_of!((*record).descriptor))
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ControlHeader {
    magic: u64,
    version: u64,
    history: u64,
    map_len: u64,
    scope: [u8; 16],
}

const _: () = {
    assert!(size_of::<ControlHeader>() <= LATEST);
    assert!(size_of::<ResourceHeader>() <= HEADER_LEN);
};

#[derive(Debug)]
pub(super) struct ControlMap {
    pub(super) storage: SharedMemorySegment,
    pub(super) history: usize,
    pub(super) scope: [u8; 16],
}

impl ControlMap {
    pub(super) fn allocation_len(history: usize) -> Result<usize, ArenaError> {
        if history == 0 {
            return Err(ArenaError::Configuration("history must be nonzero"));
        }
        page_rounded(checked_add(HEADER_LEN, checked_mul(history, size_of::<AtomicU64>())?)?)
    }

    pub(super) fn new(history: usize) -> Result<Self, ArenaError> {
        let storage = SharedMemorySegment::new(Self::allocation_len(history)?)?;
        let scope = super::random_scope()?;
        // SAFETY: fresh unpublished map, with bounded aligned scalar objects.
        unsafe {
            storage.as_ptr().cast_mut().cast::<ControlHeader>().write(ControlHeader {
                magic: CONTROL_MAGIC,
                version: VERSION,
                history: history as u64,
                map_len: storage.len() as u64,
                scope,
            });
            for offset in [LATEST, TERMINAL, RECONFIGURATION_EPOCH] {
                storage.as_ptr().add(offset).cast_mut().cast::<AtomicU64>().write(AtomicU64::new(0));
            }
            for index in 0..history {
                storage
                    .as_ptr()
                    .add(HEADER_LEN + index * 8)
                    .cast_mut()
                    .cast::<AtomicU64>()
                    .write(AtomicU64::new(0));
            }
        }
        Ok(Self { storage, history, scope })
    }

    pub(super) fn map(fd: OwnedFd, history: usize, scope: [u8; 16]) -> Result<Self, ArenaError> {
        let storage = SharedMemorySegment::map_read_only(fd, Self::allocation_len(history)?)?;
        // SAFETY: the grant owns a conforming initialized map; header is immutable.
        let header = unsafe { storage.as_ptr().cast::<ControlHeader>().read() };
        if header.magic != CONTROL_MAGIC
            || header.version != VERSION
            || header.scope != scope
            || header.history != history as u64
            || header.map_len != storage.len() as u64
        {
            return Err(ArenaError::Mapping("control header disagrees with grant"));
        }
        Ok(Self { storage, history, scope })
    }

    pub(super) fn word(&self, offset: usize) -> &AtomicU64 {
        assert!(offset % align_of::<AtomicU64>() == 0 && offset + 8 <= self.storage.len());
        // SAFETY: callers name initialized atomic words, never header bytes.
        unsafe { &*self.storage.as_ptr().add(offset).cast::<AtomicU64>() }
    }

    pub(super) fn ring(&self, cursor: u64) -> &AtomicU64 {
        assert!(cursor != 0);
        self.word(HEADER_LEN + ((cursor - 1) % self.history as u64) as usize * 8)
    }
}
