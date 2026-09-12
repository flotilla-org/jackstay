//! Shared publication and storage claims. A ring position advertises a resource;
//! only claim publication followed by generation validation acquires it.
//!
//! The SC ordering argument is in docs/design/acquisition-ownership.md. Plain
//! descriptors and CPU bytes are read only under a validated claim. Native
//! resources use the same descriptor/claim lifetime; their readiness and release
//! completion must additionally be satisfied by the native backend.

use std::{
    cell::UnsafeCell,
    collections::BTreeMap,
    mem::{align_of, size_of},
    os::fd::OwnedFd,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering::SeqCst},
    },
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{AdmissionBook, AdmissionError, AdmissionLimits, HoldingRequest, IncarnationId};
use crate::{CaptureTransferError, shm::SharedMemorySegment};

mod wait;
pub use wait::{Cancellation, WaitEvents, WaitInterest, WaitOutcome};

const MAGIC: u64 = u64::from_le_bytes(*b"JSACQ001");
const CLAIM_MAGIC: u64 = u64::from_le_bytes(*b"JSCLM001");
const VERSION: u64 = 2;
const HEADER_LEN: usize = 256;
const LATEST: usize = 128;
const TERMINAL: usize = 136;
const RECONFIGURATION_EPOCH: usize = 144;
const ACTIVE: usize = 128;
const QUIESCENT: usize = 136;
const CAPACITY_EPOCH: usize = 144;
const WAIT_INTEREST: usize = 152;
const MAX_GENERATION: u64 = u64::MAX >> 1;

#[derive(Debug, Clone, Copy)]
pub struct ArenaConfig {
    pub resource_capacity: u32,
    pub retained_history: u32,
    pub producer_reserve: u32,
    /// Inline CPU storage per resource. Native-only arenas may use zero.
    pub payload_capacity: usize,
    pub memory_budget: u64,
    pub max_incarnations: u32,
}

/// Complete, immutable metadata copied only after acquiring the resource.
/// Contains no pointer to the publication/config rings. Integer fields keep
/// unknown enum values representable for validation at the consumer API.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameDescriptor {
    pub cursor: u64,
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub config_generation: u64,
    pub pool_id: u64,
    pub payload_offset: u64,
    pub payload_len: u64,
    pub modifier: u64,
    pub fence_id: u64,
    pub fence_value: u64,
    pub damage_base_sequence: u64,
    pub producer_drop_count: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: u32,
    pub slot_id: u32,
    pub clock_domain: u32,
    pub color_space: u32,
    pub sync_kind: u32,
    pub payload_kind: u32,
    pub damage_kind: u32,
    pub dropped_before_publish: u32,
    pub flags: u32,
}

#[repr(C, align(128))]
struct ResourceRecord {
    state: AtomicU64,
    descriptor: UnsafeCell<FrameDescriptor>,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Header {
    magic: u64,
    version: u64,
    resources: u64,
    history: u64,
    payload_capacity: u64,
    map_len: u64,
    records_offset: u64,
    payload_offset: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ClaimHeader {
    magic: u64,
    version: u64,
    incarnation: u64,
    frames: u64,
    map_len: u64,
}

const _: () = {
    assert!(size_of::<Header>() <= LATEST);
    assert!(size_of::<ClaimHeader>() <= ACTIVE);
    assert!(align_of::<ResourceRecord>() == 128);
    assert!(size_of::<ResourceRecord>() == 256);
    assert!(size_of::<FrameDescriptor>() == 144);
};

#[derive(Debug, Error)]
pub enum ArenaError {
    #[error("acquisition notification failed: {0}")]
    Notification(#[from] std::io::Error),
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    #[error(transparent)]
    Storage(#[from] CaptureTransferError),
    #[error("invalid arena configuration: {0}")]
    Configuration(&'static str),
    #[error("invalid acquisition mapping: {0}")]
    Mapping(&'static str),
    #[error("frame generations exhausted")]
    GenerationsExhausted,
    #[error("the acquisition arena is closed")]
    Closed,
    #[error("payload exceeds resource capacity")]
    PayloadTooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    Published { cursor: u64 },
    Dropped,
}

#[derive(Debug)]
pub enum AcquireOutcome {
    Frame(FrameLease),
    Empty,
    Miss { cursor: u64 },
    Gap { first: u64, last: u64 },
    HoldingLimit,
    Closed,
}

#[derive(Debug, Clone, Copy)]
struct Layout {
    resources: usize,
    history: usize,
    payload_capacity: usize,
    records: usize,
    payload: usize,
    len: usize,
}

fn checked_add(a: usize, b: usize) -> Result<usize, ArenaError> {
    a.checked_add(b).ok_or(ArenaError::Configuration("mapping size overflow"))
}

fn checked_mul(a: usize, b: usize) -> Result<usize, ArenaError> {
    a.checked_mul(b).ok_or(ArenaError::Configuration("mapping size overflow"))
}

fn rounded(value: usize, alignment: usize) -> Result<usize, ArenaError> {
    Ok(checked_add(value, alignment - 1)? / alignment * alignment)
}

fn page_rounded(value: usize) -> Result<usize, ArenaError> {
    // SAFETY: sysconf has no pointer arguments and does not mutate Rust memory.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return Err(ArenaError::Configuration("cannot determine OS page size"));
    }
    rounded(value, page as usize)
}

impl Layout {
    fn new(resources: usize, history: usize, payload_capacity: usize) -> Result<Self, ArenaError> {
        if history == 0 || resources <= history {
            return Err(ArenaError::Configuration("resources must exceed a nonzero history"));
        }
        let records = rounded(checked_add(HEADER_LEN, checked_mul(history, size_of::<AtomicU64>())?)?, 128)?;
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

    fn record_offset(self, index: usize) -> usize {
        assert!(index < self.resources);
        self.records + index * size_of::<ResourceRecord>()
    }

    fn payload_offset(self, index: usize) -> usize {
        assert!(index < self.resources);
        self.payload + index * self.payload_capacity
    }

    fn ring_offset(self, cursor: u64) -> usize {
        HEADER_LEN + (((cursor - 1) % self.history as u64) as usize) * size_of::<AtomicU64>()
    }
}

#[derive(Debug)]
struct ArenaMap {
    storage: SharedMemorySegment,
    layout: Layout,
}

impl ArenaMap {
    fn new(layout: Layout) -> Result<Self, ArenaError> {
        let storage = SharedMemorySegment::new(layout.len)?;
        let header = Header {
            magic: MAGIC,
            version: VERSION,
            resources: layout.resources as u64,
            history: layout.history as u64,
            payload_capacity: layout.payload_capacity as u64,
            map_len: layout.len as u64,
            records_offset: layout.records as u64,
            payload_offset: layout.payload as u64,
        };
        // SAFETY: fresh unpublished writable mapping; all typed objects are
        // aligned, bounded by Layout, and initialized before any fd transfer.
        unsafe {
            storage.as_ptr().cast_mut().cast::<Header>().write(header);
            for offset in [LATEST, TERMINAL, RECONFIGURATION_EPOCH] {
                storage.as_ptr().add(offset).cast_mut().cast::<AtomicU64>().write(AtomicU64::new(0));
            }
            for index in 0..layout.history {
                storage
                    .as_ptr()
                    .add(HEADER_LEN + index * 8)
                    .cast_mut()
                    .cast::<AtomicU64>()
                    .write(AtomicU64::new(0));
            }
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

    fn map(fd: OwnedFd, expected: Layout) -> Result<Self, ArenaError> {
        let storage = SharedMemorySegment::map_read_only(fd, expected.len)?;
        // SAFETY: the opaque grant names an initialized arena. Header bytes
        // are immutable after creation; no mutable atomic fields are copied.
        let header = unsafe { storage.as_ptr().cast::<Header>().read() };
        if header.magic != MAGIC
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

    fn state(&self, index: usize) -> &AtomicU64 {
        self.word(self.layout.record_offset(index))
    }

    fn descriptor_ptr(&self, index: usize) -> *mut FrameDescriptor {
        // SAFETY: Layout bounds and aligns the record. Taking a raw field
        // address does not read or borrow the concurrently protected contents.
        unsafe {
            let record = self.storage.as_ptr().add(self.layout.record_offset(index)).cast::<ResourceRecord>();
            UnsafeCell::raw_get(std::ptr::addr_of!((*record).descriptor))
        }
    }
}

#[derive(Debug)]
struct ClaimMap {
    storage: SharedMemorySegment,
    incarnation: IncarnationId,
    frames: usize,
    wake: Arc<wait::Wake>,
}

impl ClaimMap {
    fn allocation_len(frames: u32) -> Result<usize, ArenaError> {
        page_rounded(checked_add(HEADER_LEN, checked_mul(frames as usize, 8)?)?)
    }

    fn new(incarnation: IncarnationId, frames: u32, len: usize, wake: Arc<wait::Wake>) -> Result<Self, ArenaError> {
        let storage = SharedMemorySegment::new(len)?;
        // SAFETY: initialized while the new writable map is exclusively owned.
        unsafe {
            storage.as_ptr().cast_mut().cast::<ClaimHeader>().write(ClaimHeader {
                magic: CLAIM_MAGIC,
                version: VERSION,
                incarnation: incarnation.0,
                frames: frames as u64,
                map_len: len as u64,
            });
            for (offset, value) in [(ACTIVE, 1), (QUIESCENT, 0), (CAPACITY_EPOCH, 0), (WAIT_INTEREST, 0)] {
                storage
                    .as_ptr()
                    .add(offset)
                    .cast_mut()
                    .cast::<AtomicU64>()
                    .write(AtomicU64::new(value));
            }
            for index in 0..frames as usize {
                storage
                    .as_ptr()
                    .add(HEADER_LEN + index * 8)
                    .cast_mut()
                    .cast::<AtomicU64>()
                    .write(AtomicU64::new(0));
            }
        }
        Ok(Self {
            storage,
            incarnation,
            frames: frames as usize,
            wake,
        })
    }

    fn map(fd: OwnedFd, incarnation: IncarnationId, frames: usize, len: usize, wake: Arc<wait::Wake>) -> Result<Self, ArenaError> {
        let storage = SharedMemorySegment::map_read_write(fd, len)?;
        // SAFETY: grant owns this initialized incarnation's map; header never changes.
        let header = unsafe { storage.as_ptr().cast::<ClaimHeader>().read() };
        if header.magic != CLAIM_MAGIC
            || header.version != VERSION
            || header.incarnation != incarnation.0
            || header.frames != frames as u64
            || header.map_len != len as u64
        {
            return Err(ArenaError::Mapping("claim header disagrees with grant"));
        }
        Ok(Self {
            storage,
            incarnation,
            frames,
            wake,
        })
    }

    fn word(&self, offset: usize) -> &AtomicU64 {
        assert!(offset % align_of::<AtomicU64>() == 0 && offset + 8 <= self.storage.len());
        // SAFETY: all callers name initialized atomic words in the claim map.
        unsafe { &*self.storage.as_ptr().add(offset).cast::<AtomicU64>() }
    }

    fn slot(&self, index: usize) -> &AtomicU64 {
        assert!(index < self.frames);
        self.word(HEADER_LEN + index * 8)
    }

    fn contains(&self, generation: u64) -> bool {
        (0..self.frames).any(|index| self.slot(index).load(SeqCst) == generation)
    }

    fn close(&self) {
        self.word(ACTIVE).store(0, SeqCst);
        let _ = self.signal(wait::CLOSED);
    }
}

/// An opaque, single-use grant for one incarnation. Dropping an unconsumed
/// grant abandons that admission safely; mapped clients own their own lifetime.
pub struct ConsumerGrant {
    arena_fd: Option<OwnedFd>,
    claim_fd: Option<OwnedFd>,
    reader_fd: Option<OwnedFd>,
    layout: Layout,
    claims: Arc<ClaimMap>,
    consumed: bool,
}

/// Setup-channel descriptor. Accompanying FDs: arena, claim mapping, notification
/// reader, notification writer, in that order. Never resend a consumed grant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantDescriptor {
    pub version: u64,
    pub incarnation: u64,
    pub resources: u32,
    pub history: u32,
    pub payload_capacity: u64,
    pub arena_map_len: u64,
    pub holding: u32,
    pub claim_map_len: u64,
}

impl ConsumerGrant {
    pub fn into_parts(mut self) -> Result<(GrantDescriptor, [OwnedFd; 4]), ArenaError> {
        let writer_fd = self.claims.wake.fd()?;
        self.consumed = true;
        let descriptor = GrantDescriptor {
            version: VERSION,
            incarnation: self.claims.incarnation.0,
            resources: self.layout.resources as u32,
            history: self.layout.history as u32,
            payload_capacity: self.layout.payload_capacity as u64,
            arena_map_len: self.layout.len as u64,
            holding: self.claims.frames as u32,
            claim_map_len: self.claims.storage.len() as u64,
        };
        Ok((
            descriptor,
            [
                self.arena_fd.take().expect("single-use grant"),
                self.claim_fd.take().expect("single-use grant"),
                self.reader_fd.take().expect("single-use grant"),
                writer_fd,
            ],
        ))
    }

    /// Reconstruct a single-use grant received over an authorized setup channel.
    ///
    /// # Safety
    /// The sender must be a conforming, sole producer of this arena, obeying the
    /// immutable-header and claim-before-reuse protocol. This process must be
    /// the sole recipient of this incarnation's claim grant. It must not fork
    /// or pass that grant on after mapping it. Length/header checks cannot prove
    /// another process follows a shared-memory lifetime protocol.
    pub unsafe fn from_parts(descriptor: GrantDescriptor, fds: [OwnedFd; 4]) -> Result<Self, ArenaError> {
        let [arena_fd, claim_fd, reader_fd, writer_fd] = fds;
        if descriptor.version != VERSION || descriptor.incarnation == 0 || descriptor.holding == 0 {
            return Err(ArenaError::Mapping("invalid grant version or incarnation reservation"));
        }
        let payload_capacity =
            usize::try_from(descriptor.payload_capacity).map_err(|_| ArenaError::Mapping("payload capacity overflow"))?;
        let layout = Layout::new(descriptor.resources as usize, descriptor.history as usize, payload_capacity)?;
        let claim_len = ClaimMap::allocation_len(descriptor.holding)?;
        if descriptor.arena_map_len != layout.len as u64 || descriptor.claim_map_len != claim_len as u64 {
            return Err(ArenaError::Mapping("grant mapping sizes disagree with layout"));
        }
        // Keep the mapped owner solely for abandonment acknowledgement until
        // from_grant transfers the lifetime to a ConsumerInner.
        let claims = Arc::new(ClaimMap::map(
            claim_fd.try_clone().map_err(|error| CaptureTransferError::SharedMemory {
                operation: "clone-claim-fd",
                message: error.to_string(),
            })?,
            IncarnationId(descriptor.incarnation),
            descriptor.holding as usize,
            claim_len,
            Arc::new(wait::Wake::from_fd(writer_fd)?),
        )?);
        Ok(Self {
            arena_fd: Some(arena_fd),
            claim_fd: Some(claim_fd),
            reader_fd: Some(reader_fd),
            layout,
            claims,
            consumed: false,
        })
    }
}

impl Drop for ConsumerGrant {
    fn drop(&mut self) {
        if !self.consumed {
            self.claims.close();
            self.claims.word(QUIESCENT).store(1, SeqCst);
        }
    }
}

/// Sole publisher and cleanup owner. `&mut self` serializes admission,
/// resource retirement, and publication; consumer operations require no call
/// into this object and run through separate shared mappings.
pub struct ArenaProducer {
    map: Arc<ArenaMap>,
    admission: AdmissionBook,
    claims: BTreeMap<IncarnationId, Arc<ClaimMap>>,
    cursor: u64,
    next_slot: usize,
}

impl ArenaProducer {
    pub fn new(config: ArenaConfig) -> Result<Self, ArenaError> {
        let layout = Layout::new(
            config.resource_capacity as usize,
            config.retained_history as usize,
            config.payload_capacity,
        )?;
        let admission = AdmissionBook::new(AdmissionLimits {
            resource_capacity: config.resource_capacity,
            retained_history: config.retained_history,
            producer_reserve: config.producer_reserve,
            allocated_bytes: layout.len as u64,
            memory_budget: config.memory_budget,
            max_incarnations: config.max_incarnations,
        })?;
        Ok(Self {
            map: Arc::new(ArenaMap::new(layout)?),
            admission,
            claims: BTreeMap::new(),
            cursor: 0,
            next_slot: 0,
        })
    }

    pub fn attach(&mut self, holding: u32) -> Result<ConsumerGrant, ArenaError> {
        if self.map.word(TERMINAL).load(SeqCst) != 0 {
            return Err(ArenaError::Closed);
        }
        self.collect_quiescent();
        let len = ClaimMap::allocation_len(holding)?;
        let reservation = self.admission.admit(HoldingRequest {
            frames: holding,
            claim_bytes: len as u64,
        })?;
        let incarnation = reservation.incarnation();
        let result = (|| {
            let (wake, receiver) = wait::channel()?;
            let claims = Arc::new(ClaimMap::new(incarnation, holding, len, wake)?);
            let arena_fd = self.map.storage.try_clone_fd()?;
            let claim_fd = claims.storage.try_clone_fd()?;
            Ok(ConsumerGrant {
                arena_fd: Some(arena_fd),
                claim_fd: Some(claim_fd),
                reader_fd: Some(receiver.into_fd()),
                layout: self.map.layout,
                claims,
                consumed: false,
            })
        })();
        match result {
            Ok(grant) => {
                self.claims.insert(incarnation, Arc::clone(&grant.claims));
                Ok(grant)
            }
            Err(error) => {
                // No mapping has escaped. Failed allocation/FD setup drops all
                // temporary owners before rolling back the reservation.
                self.admission.close(incarnation)?;
                self.admission.complete_cleanup(incarnation)?;
                Err(error)
            }
        }
    }

    pub fn publish(&mut self, mut descriptor: FrameDescriptor, bytes: &[u8]) -> Result<PublishOutcome, ArenaError> {
        if bytes.len() > self.map.layout.payload_capacity {
            return Err(ArenaError::PayloadTooLarge);
        }
        if self.cursor == MAX_GENERATION {
            self.map.word(TERMINAL).store(1, SeqCst);
            for claims in self.claims.values() {
                claims.close();
            }
            return Err(ArenaError::GenerationsExhausted);
        }
        self.collect_quiescent();
        let oldest = self.cursor.saturating_sub(self.map.layout.history as u64 - 1).max(1);
        for offset in 0..self.map.layout.resources {
            let index = (self.next_slot + offset) % self.map.layout.resources;
            let state = self.map.state(index).load(SeqCst);
            let generation = state >> 1;
            if generation != 0 && generation >= oldest {
                continue;
            }
            // Retire BEFORE scanning claims. Repeated attempts leave the
            // generation retired, so no new successful acquisitions can pin it.
            self.map.state(index).store(generation << 1, SeqCst);
            if generation != 0 && self.claims.values().any(|claims| claims.contains(generation)) {
                continue;
            }
            let cursor = self.cursor + 1;
            let payload_offset = self.map.layout.payload_offset(index);
            descriptor.cursor = cursor;
            descriptor.payload_offset = payload_offset as u64;
            descriptor.payload_len = bytes.len() as u64;
            // SAFETY: resource is outside history, retired, and unclaimed.
            // SC retirement/scan excludes successful concurrent acquisition.
            // This producer owns the only write API; descriptors are copied
            // by readers only after validating a claim for this generation.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    self.map.storage.as_ptr().add(payload_offset).cast_mut(),
                    bytes.len(),
                );
                self.map.descriptor_ptr(index).write(descriptor);
            }
            self.map.state(index).store((cursor << 1) | 1, SeqCst);
            self.map.word(self.map.layout.ring_offset(cursor)).store(index as u64 + 1, SeqCst);
            self.map.word(LATEST).store(cursor, SeqCst);
            self.cursor = cursor;
            self.next_slot = (index + 1) % self.map.layout.resources;
            for claims in self.claims.values() {
                claims.signal(wait::DATA)?;
            }
            return Ok(PublishOutcome::Published { cursor });
        }
        Ok(PublishOutcome::Dropped)
    }

    /// Connection loss closes acquisition but leaves outstanding resources and
    /// mapping capacity charged. A running process can still own old leases.
    pub fn close(&mut self, incarnation: IncarnationId) -> Result<(), ArenaError> {
        let claims = self.claims.get(&incarnation).ok_or(AdmissionError::UnknownIncarnation)?;
        claims.close();
        self.admission.close(incarnation)?;
        Ok(())
    }

    fn collect_quiescent(&mut self) {
        self.claims.retain(|incarnation, claims| {
            if claims.word(QUIESCENT).load(SeqCst) == 0 {
                return true;
            }
            // QUIESCENT is written by the last library owner only after all
            // its claims and in-progress methods are gone. Process-exit cleanup
            // requires separate OS proof and does not manufacture this flag.
            self.admission.close(*incarnation).expect("tracked incarnation");
            self.admission.complete_cleanup(*incarnation).expect("closed incarnation");
            false
        });
    }
}

impl Drop for ArenaProducer {
    fn drop(&mut self) {
        self.map.word(TERMINAL).store(1, SeqCst);
        for claims in self.claims.values() {
            claims.close();
        }
    }
}

#[derive(Debug)]
struct ConsumerInner {
    map: Arc<ArenaMap>,
    claims: ClaimMap,
}

impl Drop for ConsumerInner {
    fn drop(&mut self) {
        self.claims.close();
        // Every frame keeps this owner alive; all claim clears precede this
        // final store, and no method can still have a reference to the owner.
        self.claims.word(QUIESCENT).store(1, SeqCst);
    }
}

#[derive(Debug)]
pub struct ArenaConsumer {
    inner: Arc<ConsumerInner>,
    receiver: wait::Receiver,
}

impl ArenaConsumer {
    pub fn from_grant(mut grant: ConsumerGrant) -> Result<Self, ArenaError> {
        let map = ArenaMap::map(grant.arena_fd.take().expect("single-use grant"), grant.layout)?;
        let claims = ClaimMap::map(
            grant.claim_fd.take().expect("single-use grant"),
            grant.claims.incarnation,
            grant.claims.frames,
            grant.claims.storage.len(),
            Arc::clone(&grant.claims.wake),
        )?;
        let receiver = wait::Receiver::from_fd(grant.reader_fd.take().expect("single-use grant"))?;
        grant.consumed = true;
        Ok(Self {
            receiver,
            inner: Arc::new(ConsumerInner {
                map: Arc::new(map),
                claims,
            }),
        })
    }

    #[must_use]
    pub fn incarnation(&self) -> IncarnationId {
        self.inner.claims.incarnation
    }

    pub fn acquire_latest(&self, after: u64) -> Result<AcquireOutcome, ArenaError> {
        let cursor = self.inner.map.word(LATEST).load(SeqCst);
        if self.is_closed() {
            return Ok(AcquireOutcome::Closed);
        }
        if cursor == 0 || cursor <= after {
            return Ok(AcquireOutcome::Empty);
        }
        self.acquire(cursor)
    }

    /// Ordered retained delivery. `after == 0` starts at the oldest advertised
    /// frame. A gap names published cursors, not pre-publication drops. The
    /// caller may acknowledge a gap by continuing after its `last` cursor.
    pub fn acquire_next(&self, after: u64) -> Result<AcquireOutcome, ArenaError> {
        let latest = self.inner.map.word(LATEST).load(SeqCst);
        if self.is_closed() {
            return Ok(AcquireOutcome::Closed);
        }
        if latest == 0 || after >= latest {
            return Ok(AcquireOutcome::Empty);
        }
        let oldest = latest.saturating_sub(self.inner.map.layout.history as u64 - 1).max(1);
        let cursor = if after == 0 { oldest } else { after + 1 };
        if cursor < oldest {
            return Ok(AcquireOutcome::Gap {
                first: cursor,
                last: oldest - 1,
            });
        }
        self.acquire(cursor)
    }

    /// Try precisely this cursor. Retirement or a concurrent ring wrap is a
    /// miss and never silently substitutes a more recent frame.
    pub fn acquire_exact(&self, cursor: u64) -> Result<AcquireOutcome, ArenaError> {
        if cursor == 0 {
            return Err(ArenaError::Configuration("frame cursors start at one"));
        }
        let latest = self.inner.map.word(LATEST).load(SeqCst);
        if self.is_closed() {
            return Ok(AcquireOutcome::Closed);
        }
        if cursor > latest {
            return Ok(AcquireOutcome::Empty);
        }
        let oldest = latest.saturating_sub(self.inner.map.layout.history as u64 - 1).max(1);
        if cursor < oldest {
            return Ok(AcquireOutcome::Miss { cursor });
        }
        self.acquire(cursor)
    }

    fn is_closed(&self) -> bool {
        self.inner.claims.word(ACTIVE).load(SeqCst) == 0 || self.inner.map.word(TERMINAL).load(SeqCst) != 0
    }

    fn acquire(&self, cursor: u64) -> Result<AcquireOutcome, ArenaError> {
        let map = &self.inner.map;
        let advertised = map.word(map.layout.ring_offset(cursor)).load(SeqCst);
        if advertised == 0 || advertised > map.layout.resources as u64 {
            return Err(ArenaError::Mapping("advertised resource index is invalid"));
        }
        let index = advertised as usize - 1;
        #[cfg(test)]
        concurrency_tests::run_hook(concurrency_tests::Phase::Selected);
        let Some(claim_slot) =
            (0..self.inner.claims.frames).find(|slot| self.inner.claims.slot(*slot).compare_exchange(0, cursor, SeqCst, SeqCst).is_ok())
        else {
            return Ok(AcquireOutcome::HoldingLimit);
        };
        let claim = Claim {
            owner: Arc::clone(&self.inner),
            slot: claim_slot,
        };
        #[cfg(test)]
        concurrency_tests::run_hook(concurrency_tests::Phase::Claimed);
        // Successful generation validation linearizes acquisition, conditional
        // on the subsequent incarnation/terminal check. Never read descriptor
        // bytes speculatively before this point.
        if map.state(index).load(SeqCst) != (cursor << 1) | 1 {
            return Ok(AcquireOutcome::Miss { cursor });
        }
        #[cfg(test)]
        concurrency_tests::run_hook(concurrency_tests::Phase::Validated);
        if self.is_closed() {
            return Ok(AcquireOutcome::Closed);
        }
        // SAFETY: claim publication and generation validation precede the copy.
        // A retiring producer must see the held claim before writing this slot.
        let descriptor = unsafe { map.descriptor_ptr(index).read() };
        if descriptor.cursor != cursor
            || descriptor.payload_offset != map.layout.payload_offset(index) as u64
            || descriptor.payload_len > map.layout.payload_capacity as u64
        {
            return Err(ArenaError::Mapping("leased descriptor contradicts resource identity or bounds"));
        }
        Ok(AcquireOutcome::Frame(FrameLease { descriptor, claim }))
    }
}

impl Drop for ArenaConsumer {
    fn drop(&mut self) {
        self.inner.claims.close();
    }
}

#[derive(Debug)]
struct Claim {
    owner: Arc<ConsumerInner>,
    slot: usize,
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.owner.claims.slot(self.slot).store(0, SeqCst);
        if self.owner.claims.word(CAPACITY_EPOCH).fetch_add(1, SeqCst) == u64::MAX {
            self.owner.claims.close();
        }
        let _ = self.owner.claims.signal(wait::CAPACITY);
    }
}

/// A CPU lease releases on drop, after all borrowed byte slices have ended.
/// Native APIs must retain this object until their completion evidence permits
/// release; dropping it when GPU commands are merely submitted is not valid.
#[derive(Debug)]
pub struct FrameLease {
    descriptor: FrameDescriptor,
    claim: Claim,
}

impl FrameLease {
    #[must_use]
    pub fn cursor(&self) -> u64 {
        self.descriptor.cursor
    }

    #[must_use]
    pub fn descriptor(&self) -> &FrameDescriptor {
        &self.descriptor
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: validated bounds, live mapping, and this lease's claim prevent
        // producer writes throughout the returned slice's borrow lifetime.
        unsafe {
            std::slice::from_raw_parts(
                self.claim.owner.map.storage.as_ptr().add(self.descriptor.payload_offset as usize),
                self.descriptor.payload_len as usize,
            )
        }
    }
}

#[cfg(test)]
mod concurrency_tests {
    use std::{cell::RefCell, rc::Rc};

    use super::*;

    // Scheduler injection only: tests still acquire and publish through the
    // public arena interface. No production hook or configurable bypass exists.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum Phase {
        Selected,
        Claimed,
        Validated,
        BeforeSleep,
    }

    type Hook = (Phase, Box<dyn FnOnce()>);
    thread_local! { static HOOK: RefCell<Option<Hook>> = RefCell::new(None); }

    struct HookReset;
    impl Drop for HookReset {
        fn drop(&mut self) {
            HOOK.with(|hook| {
                hook.borrow_mut().take();
            });
        }
    }

    fn schedule(phase: Phase, callback: impl FnOnce() + 'static) -> HookReset {
        HOOK.with(|hook| *hook.borrow_mut() = Some((phase, Box::new(callback))));
        HookReset
    }

    pub(super) fn run_hook(phase: Phase) {
        let callback = HOOK.with(|hook| {
            let mut hook = hook.borrow_mut();
            if hook.as_ref().is_some_and(|(scheduled, _)| *scheduled == phase) {
                hook.take()
            } else {
                None
            }
        });
        if let Some((_, callback)) = callback {
            callback();
        }
    }

    fn config() -> ArenaConfig {
        ArenaConfig {
            resource_capacity: 5,
            retained_history: 2,
            producer_reserve: 1,
            payload_capacity: 4,
            memory_budget: 1024 * 1024,
            max_incarnations: 2,
        }
    }

    #[test]
    fn publication_between_selection_claim_and_validation_never_reads_reused_bytes() {
        for phase in [Phase::Selected, Phase::Claimed, Phase::Validated] {
            let producer = Rc::new(RefCell::new(ArenaProducer::new(config()).unwrap()));
            let consumer = ArenaConsumer::from_grant(producer.borrow_mut().attach(1).unwrap()).unwrap();
            producer.borrow_mut().publish(FrameDescriptor::default(), b"abcd").unwrap();
            let publishing = Rc::clone(&producer);
            let _reset = schedule(phase, move || {
                for sequence in 2..=20 {
                    assert!(matches!(
                        publishing
                            .borrow_mut()
                            .publish(
                                FrameDescriptor {
                                    sequence,
                                    ..FrameDescriptor::default()
                                },
                                b"wxyz"
                            )
                            .unwrap(),
                        PublishOutcome::Published { .. }
                    ));
                }
            });
            match (phase, consumer.acquire_latest(0).unwrap()) {
                (Phase::Selected | Phase::Claimed, AcquireOutcome::Miss { cursor: 1 }) => {}
                (Phase::Validated, AcquireOutcome::Frame(frame)) => assert_eq!(frame.bytes(), b"abcd"),
                (_, result) => panic!("unexpected raced acquisition: {result:?}"),
            }
            // Failed provisional claims and finished leases both return credit.
            let AcquireOutcome::Frame(latest) = consumer.acquire_latest(1).unwrap() else {
                panic!("claim credit leaked")
            };
            assert_eq!(latest.descriptor().sequence, 20);
            assert_eq!(latest.bytes(), b"wxyz");
        }
    }

    #[test]
    fn closing_after_claim_publication_prevents_success_without_revoking_an_old_lease() {
        let producer = Rc::new(RefCell::new(ArenaProducer::new(config()).unwrap()));
        let consumer = ArenaConsumer::from_grant(producer.borrow_mut().attach(2).unwrap()).unwrap();
        producer.borrow_mut().publish(FrameDescriptor::default(), b"abcd").unwrap();
        let AcquireOutcome::Frame(held) = consumer.acquire_latest(0).unwrap() else {
            panic!("missing first lease")
        };
        let closing = Rc::clone(&producer);
        let incarnation = consumer.incarnation();
        let _reset = schedule(Phase::Claimed, move || {
            closing.borrow_mut().close(incarnation).unwrap();
        });
        assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Closed));
        for _ in 0..20 {
            producer.borrow_mut().publish(FrameDescriptor::default(), b"wxyz").unwrap();
        }
        assert_eq!(held.bytes(), b"abcd");
    }

    #[test]
    fn data_release_closure_and_cancellation_cannot_be_lost_after_the_final_wait_recheck() {
        use std::time::Duration;
        for event in 0..4 {
            let producer = Rc::new(RefCell::new(ArenaProducer::new(config()).unwrap()));
            let mut consumer = ArenaConsumer::from_grant(producer.borrow_mut().attach(1).unwrap()).unwrap();
            producer.borrow_mut().publish(FrameDescriptor::default(), b"abcd").unwrap();
            let AcquireOutcome::Frame(held) = consumer.acquire_latest(0).unwrap() else {
                panic!("no frame")
            };
            let observed = consumer.events();
            let cancel = Cancellation::new().unwrap();
            let cancelling = cancel.clone();
            let changing = Rc::clone(&producer);
            let incarnation = consumer.incarnation();
            let _reset = schedule(Phase::BeforeSleep, move || {
                match event {
                    0 => {
                        changing.borrow_mut().publish(FrameDescriptor::default(), b"wxyz").unwrap();
                    }
                    1 => {}
                    2 => {
                        changing.borrow_mut().close(incarnation).unwrap();
                    }
                    3 => {
                        cancelling.cancel().unwrap();
                    }
                    _ => unreachable!(),
                }
                drop(held);
            });
            let interest = match event {
                0 => WaitInterest::DATA,
                1 => WaitInterest::CAPACITY,
                _ => WaitInterest::ALL,
            };
            let outcome = consumer.wait(observed, interest, &cancel, Some(Duration::from_secs(2))).unwrap();
            match (event, outcome) {
                (0, WaitOutcome::Changed(events)) => assert_eq!(events.data_cursor, 2),
                (1, WaitOutcome::Changed(events)) => assert_eq!(events.capacity_epoch, 1),
                (2, WaitOutcome::Changed(events)) => assert!(events.closed),
                (3, WaitOutcome::Cancelled) => {}
                (_, result) => panic!("lost wakeup: {result:?}"),
            }
        }
    }
}
