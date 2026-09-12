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
    io::Read,
    mem::{ManuallyDrop, align_of, size_of},
    os::fd::OwnedFd,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering::SeqCst},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{AdmissionBook, AdmissionError, AdmissionLimits, AllocationId, HoldingRequest, IncarnationId};
use crate::{CaptureTransferError, shm::SharedMemorySegment};

mod reconfiguration;
use reconfiguration::RetiredAllocation;
pub(crate) use reconfiguration::RetiredResource;
pub use reconfiguration::{ConfigurationDescriptor, ConfigurationGrant, ConfigurationInstall, ReconfigurationStatus};

mod mapping;
use mapping::{ControlMap, ResourceLayout, ResourceMap};

mod wait;
pub use wait::{Cancellation, WaitEvents, WaitInterest, WaitOutcome};
mod retirement;
pub use retirement::ConsumerReleaseTimeline;
mod cleanup;
mod process;
pub use cleanup::{CleanupFailure, RejectedDeferredRelease, ReleaseNotification, ReleaseTimeline, ReleaseTimelineRegistration};

const CLAIM_MAGIC: u64 = u64::from_le_bytes(*b"JSCLM001");
const VERSION: u64 = 7;
const HEADER_LEN: usize = 256;
const LATEST: usize = 128;
const TERMINAL: usize = 136;
const RECONFIGURATION_EPOCH: usize = 144;
const CONFIGURATION: usize = 152;
const FIRST_CURSOR: usize = 160;
const ACTIVE: usize = 128;
const QUIESCENT: usize = 136;
const CAPACITY_EPOCH: usize = 144;
const WAIT_INTEREST: usize = 152;
const OFFERED_GENERATION: usize = 160;
const MAX_GENERATION: u64 = u64::MAX >> 1;
const CLAIM_SLOT_LEN: usize = 32;
const RELEASE_ID: usize = 8;
const RELEASE_VALUE: usize = 16;
const RELEASE_PENDING: usize = 24;

#[derive(Debug, Clone, Copy)]
pub struct ArenaConfig {
    pub resource_capacity: u32,
    pub retained_history: u32,
    pub producer_reserve: u32,
    /// Inline CPU storage per resource. Native-only arenas may use zero.
    pub payload_capacity: usize,
    pub memory_budget: u64,
    pub max_incarnations: u32,
    /// Time allowed to drain after closure is observed. Expiry reports recovery
    /// failure; it never permits reuse. Active consumers may keep their leases.
    pub drain_timeout: Duration,
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
    /// Installed allocation generation, stamped by the arena at publication.
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
struct ClaimHeader {
    magic: u64,
    version: u64,
    incarnation: u64,
    frames: u64,
    map_len: u64,
    scope: [u8; 16],
    recipient_pid: u64,
}

const _: () = {
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
    #[error("incarnation recovery required: {reason}")]
    RecoveryRequired { reason: String },
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
    Reconfiguration,
    Closed,
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

#[derive(Debug)]
struct ClaimMap {
    storage: SharedMemorySegment,
    incarnation: IncarnationId,
    frames: usize,
    scope: [u8; 16],
    recipient_pid: u32,
    wake: Arc<wait::Wake>,
    release_wake: Arc<wait::Wake>,
}

fn random_scope() -> Result<[u8; 16], ArenaError> {
    let mut scope = [0; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut scope))
        .map_err(|error| CaptureTransferError::SharedMemory {
            operation: "acquisition-scope",
            message: error.to_string(),
        })?;
    Ok(scope)
}

impl ClaimMap {
    fn allocation_len(frames: u32) -> Result<usize, ArenaError> {
        // Each holding slot has claim/release words and one bounded timeline
        // registration entry. All words are initialized before fd transfer.
        page_rounded(checked_add(
            checked_add(HEADER_LEN, checked_mul(frames as usize, CLAIM_SLOT_LEN + 8)?)?,
            checked_mul(frames as usize + 2, 8)?,
        )?)
    }

    fn new(
        incarnation: IncarnationId,
        frames: u32,
        len: usize,
        wake: Arc<wait::Wake>,
        release_wake: Arc<wait::Wake>,
        recipient_pid: u32,
    ) -> Result<Self, ArenaError> {
        let storage = SharedMemorySegment::new(len)?;
        let scope = random_scope()?;
        // SAFETY: initialized while the new writable map is exclusively owned.
        unsafe {
            storage.as_ptr().cast_mut().cast::<ClaimHeader>().write(ClaimHeader {
                magic: CLAIM_MAGIC,
                version: VERSION,
                incarnation: incarnation.0,
                frames: frames as u64,
                map_len: len as u64,
                scope,
                recipient_pid: u64::from(recipient_pid),
            });
            for (offset, value) in [
                (ACTIVE, 1),
                (QUIESCENT, 0),
                (CAPACITY_EPOCH, 0),
                (WAIT_INTEREST, 0),
                (OFFERED_GENERATION, 0),
            ] {
                storage
                    .as_ptr()
                    .add(offset)
                    .cast_mut()
                    .cast::<AtomicU64>()
                    .write(AtomicU64::new(value));
            }
            for index in 0..frames as usize * (CLAIM_SLOT_LEN / 8 + 1) + frames as usize + 2 {
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
            scope,
            recipient_pid,
            wake,
            release_wake,
        })
    }

    fn map(
        fd: OwnedFd,
        incarnation: IncarnationId,
        frames: usize,
        len: usize,
        wake: Arc<wait::Wake>,
        release_wake: Arc<wait::Wake>,
        recipient_pid: u32,
    ) -> Result<Self, ArenaError> {
        let storage = SharedMemorySegment::map_read_write(fd, len)?;
        // SAFETY: grant owns this initialized incarnation's map; header never changes.
        let header = unsafe { storage.as_ptr().cast::<ClaimHeader>().read() };
        if header.magic != CLAIM_MAGIC
            || header.version != VERSION
            || header.incarnation != incarnation.0
            || header.frames != frames as u64
            || header.map_len != len as u64
            || header.recipient_pid != u64::from(recipient_pid)
        {
            return Err(ArenaError::Mapping("claim header disagrees with grant"));
        }
        Ok(Self {
            storage,
            incarnation,
            frames,
            scope: header.scope,
            recipient_pid,
            wake,
            release_wake,
        })
    }

    fn word(&self, offset: usize) -> &AtomicU64 {
        assert!(offset % align_of::<AtomicU64>() == 0 && offset + 8 <= self.storage.len());
        // SAFETY: all callers name initialized atomic words in the claim map.
        unsafe { &*self.storage.as_ptr().add(offset).cast::<AtomicU64>() }
    }

    fn slot(&self, index: usize) -> &AtomicU64 {
        assert!(index < self.frames);
        self.word(HEADER_LEN + index * CLAIM_SLOT_LEN)
    }

    fn contains(&self, generation: u64) -> bool {
        (0..self.frames).any(|index| self.slot(index).load(SeqCst) == generation)
    }

    fn close(&self) {
        if self.word(ACTIVE).swap(0, SeqCst) != 0 {
            let _ = self.signal(wait::CLOSED);
            let _ = self.release_wake.signal();
        }
    }

    fn acknowledge_quiescent(&self) {
        self.word(QUIESCENT).store(1, SeqCst);
        let _ = self.release_wake.signal();
    }
}

/// An opaque, single-use grant for one incarnation. Dropping an unconsumed
/// grant abandons that admission safely; mapped clients own their own lifetime.
pub struct ConsumerGrant {
    control_fd: Option<OwnedFd>,
    arena_scope: [u8; 16],
    resource_fd: Option<OwnedFd>,
    claim_fd: Option<OwnedFd>,
    reader_fd: Option<OwnedFd>,
    layout: ResourceLayout,
    generation: u64,
    drain_timeout: Duration,
    mapping_slot: usize,
    claims: Arc<ClaimMap>,
    consumed: bool,
}

/// Export-only grant for a monitored process. Keeping this separate prevents a
/// safe local reader from outliving a different process's reclamation proof.
/// Import still uses ConsumerGrant::from_parts with its process-lifetime contract.
pub struct RemoteConsumerGrant(ConsumerGrant);

impl RemoteConsumerGrant {
    #[must_use]
    pub fn incarnation(&self) -> IncarnationId {
        self.0.incarnation()
    }

    pub fn into_parts(self) -> Result<(GrantDescriptor, [OwnedFd; 5]), ArenaError> {
        self.0.into_parts()
    }
}

/// Setup descriptor. FDs: control, resources, claims, notification reader,
/// notification writer, in that order. Never resend a consumed grant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantDescriptor {
    pub version: u64,
    pub generation: u64,
    pub drain_timeout: Duration,
    pub mapping_slot: u32,
    pub incarnation: u64,
    pub resources: u32,
    pub history: u32,
    pub payload_capacity: u64,
    pub resource_map_len: u64,
    pub control_map_len: u64,
    pub arena_scope: [u8; 16],
    pub claim_scope: [u8; 16],
    pub holding: u32,
    pub claim_map_len: u64,
    /// Zero for an unbound grant; otherwise the admitted recipient process.
    pub recipient_pid: u32,
}

impl ConsumerGrant {
    #[must_use]
    pub fn incarnation(&self) -> IncarnationId {
        self.claims.incarnation
    }

    fn into_parts(mut self) -> Result<(GrantDescriptor, [OwnedFd; 5]), ArenaError> {
        let writer_fd = self.claims.wake.fd()?;
        self.consumed = true;
        let descriptor = GrantDescriptor {
            version: VERSION,
            generation: self.generation,
            drain_timeout: self.drain_timeout,
            mapping_slot: self.mapping_slot as u32,
            incarnation: self.claims.incarnation.0,
            resources: self.layout.resources as u32,
            history: self.layout.history as u32,
            payload_capacity: self.layout.payload_capacity as u64,
            resource_map_len: self.layout.len as u64,
            control_map_len: ControlMap::allocation_len(self.layout.history)? as u64,
            arena_scope: self.arena_scope,
            claim_scope: self.claims.scope,
            holding: self.claims.frames as u32,
            claim_map_len: self.claims.storage.len() as u64,
            recipient_pid: self.claims.recipient_pid,
        };
        Ok((
            descriptor,
            [
                self.control_fd.take().expect("single-use grant"),
                self.resource_fd.take().expect("single-use grant"),
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
    /// or pass that grant on after mapping it. A process-bound grant belongs
    /// only to the lifetime admitted by the sender, never a reused PID.
    /// Length/header checks cannot prove
    /// another process follows a shared-memory lifetime protocol.
    pub unsafe fn from_parts(descriptor: GrantDescriptor, fds: [OwnedFd; 5]) -> Result<Self, ArenaError> {
        let [control_fd, resource_fd, claim_fd, reader_fd, writer_fd] = fds;
        if descriptor.drain_timeout.is_zero() || std::time::Instant::now().checked_add(descriptor.drain_timeout).is_none() {
            return Err(ArenaError::Mapping("invalid consumer drain interval"));
        }
        if descriptor.recipient_pid != std::process::id() {
            return Err(ArenaError::Mapping("grant belongs to a different recipient process"));
        }
        if descriptor.version != VERSION || descriptor.generation == 0 || descriptor.incarnation == 0 || descriptor.holding == 0 {
            return Err(ArenaError::Mapping("invalid grant version or incarnation reservation"));
        }
        let payload_capacity =
            usize::try_from(descriptor.payload_capacity).map_err(|_| ArenaError::Mapping("payload capacity overflow"))?;
        let layout = ResourceLayout::new(descriptor.resources as usize, descriptor.history as usize, payload_capacity)?;
        let claim_len = ClaimMap::allocation_len(descriptor.holding)?;
        if descriptor.resource_map_len != layout.len as u64
            || descriptor.claim_map_len != claim_len as u64
            || descriptor.control_map_len != ControlMap::allocation_len(layout.history)? as u64
        {
            return Err(ArenaError::Mapping("grant mapping sizes disagree with layout"));
        }
        // Keep the mapped owner solely for abandonment acknowledgement until
        // from_grant transfers the lifetime to a ConsumerLifetime.
        let claims = Arc::new(ClaimMap::map(
            claim_fd.try_clone().map_err(|error| CaptureTransferError::SharedMemory {
                operation: "clone-claim-fd",
                message: error.to_string(),
            })?,
            IncarnationId(descriptor.incarnation),
            descriptor.holding as usize,
            claim_len,
            Arc::new(wait::Wake::from_fd(writer_fd)?),
            Arc::new(wait::Wake::from_fd(reader_fd.try_clone()?)?),
            descriptor.recipient_pid,
        )?);
        if descriptor.mapping_slot as usize >= claims.frames + 2
            || claims.mapping_slot(descriptor.mapping_slot as usize).load(SeqCst) != descriptor.generation
            || claims.word(OFFERED_GENERATION).load(SeqCst) != descriptor.generation
        {
            return Err(ArenaError::Mapping("initial configuration offer disagrees with claim page"));
        }
        if claims.scope != descriptor.claim_scope {
            return Err(ArenaError::Mapping("claim scope disagrees with grant"));
        }
        Ok(Self {
            control_fd: Some(control_fd),
            arena_scope: descriptor.arena_scope,
            resource_fd: Some(resource_fd),
            claim_fd: Some(claim_fd),
            reader_fd: Some(reader_fd),
            layout,
            generation: descriptor.generation,
            drain_timeout: descriptor.drain_timeout,
            mapping_slot: descriptor.mapping_slot as usize,
            claims,
            consumed: false,
        })
    }
}

impl Drop for ConsumerGrant {
    fn drop(&mut self) {
        if !self.consumed {
            drop(self.resource_fd.take());
            self.claims.mapping_slot(self.mapping_slot).store(0, SeqCst);
            self.claims.word(OFFERED_GENERATION).store(0, SeqCst);
            self.claims.close();
            self.claims.acknowledge_quiescent();
        }
    }
}

/// Sole publisher and cleanup owner. `&mut self` serializes admission,
/// resource retirement, and publication; consumer operations require no call
/// into this object and run through separate shared mappings.
pub struct ArenaProducer {
    resources: Option<Arc<ResourceMap>>,
    retired: Vec<RetiredAllocation>,
    pending_layout: Option<ResourceLayout>,
    control: Arc<ControlMap>,
    admission: AdmissionBook,
    claims: BTreeMap<IncarnationId, Arc<ClaimMap>>,
    cursor: u64,
    next_slot: usize,
    cleanup: cleanup::CleanupRegistry,
    native_resources: bool,
    drain_timeout: Duration,
}

impl ArenaProducer {
    /// Stop publication/admission and close acquisition. Existing leases and
    /// mappings must still drain; this is not permission to reclaim their bytes.
    pub fn stop(&mut self) {
        self.control.word(TERMINAL).store(1, SeqCst);
        for claims in self.claims.values() {
            claims.close();
        }
    }
    pub fn new(config: ArenaConfig) -> Result<Self, ArenaError> {
        Self::create(config, 0, false)
    }

    pub(crate) fn with_external_allocation(config: ArenaConfig, external_bytes: u64) -> Result<Self, ArenaError> {
        Self::create(config, external_bytes, true)
    }

    fn create(config: ArenaConfig, external_bytes: u64, native_resources: bool) -> Result<Self, ArenaError> {
        let (layout, admission) = Self::prepare(config, external_bytes)?;
        let control = Arc::new(ControlMap::new(layout.history)?);
        let resources = Arc::new(ResourceMap::new(layout, control.scope, 1)?);
        Ok(Self {
            resources: Some(resources),
            retired: Vec::new(),
            pending_layout: None,
            control,
            admission,
            claims: BTreeMap::new(),
            cursor: 0,
            next_slot: 0,
            cleanup: cleanup::CleanupRegistry::default(),
            native_resources,
            drain_timeout: config.drain_timeout,
        })
    }

    pub(crate) fn validate_external_allocation(config: ArenaConfig, external_bytes: u64) -> Result<(), ArenaError> {
        Self::prepare(config, external_bytes).map(|_| ())
    }

    fn prepare(config: ArenaConfig, external_bytes: u64) -> Result<(ResourceLayout, AdmissionBook), ArenaError> {
        if config.drain_timeout.is_zero() || std::time::Instant::now().checked_add(config.drain_timeout).is_none() {
            return Err(ArenaError::Configuration("drain interval must be positive and representable"));
        }
        let layout = ResourceLayout::new(
            config.resource_capacity as usize,
            config.retained_history as usize,
            config.payload_capacity,
        )?;
        let control_len = ControlMap::allocation_len(layout.history)?;
        let admission = AdmissionBook::new(AdmissionLimits {
            resource_capacity: config.resource_capacity,
            retained_history: config.retained_history,
            producer_reserve: config.producer_reserve,
            allocated_bytes: (layout.len as u64)
                .checked_add(external_bytes)
                .ok_or(ArenaError::Configuration("allocation byte total overflow"))?,
            fixed_bytes: control_len as u64,
            memory_budget: config.memory_budget,
            max_incarnations: config.max_incarnations,
        })?;
        Ok((layout, admission))
    }

    /// Poll shutdown without inferring retirement from closure or timeout.
    /// True means all admitted mappings, leases and retired allocations have
    /// drained. The owner can then destroy this arena's remaining allocation.
    pub fn poll_shutdown_ready(&mut self) -> Result<bool, ArenaError> {
        self.poll_cleanup()?;
        Ok(self.control.word(TERMINAL).load(SeqCst) != 0 && self.claims.is_empty() && self.retired.is_empty())
    }

    pub fn attach(&mut self, holding: u32) -> Result<ConsumerGrant, ArenaError> {
        self.attach_with_recipient(holding, 0, None)
    }

    /// Bind cleanup to the current kernel lifetime for this PID before any
    /// mapping escapes. The host selects/authorizes the recipient and must send
    /// this grant only to that process. A remote grant cannot be mapped locally
    /// through the safe in-process consumer constructor.
    pub fn attach_process(&mut self, holding: u32, pid: u32) -> Result<RemoteConsumerGrant, ArenaError> {
        let process = Arc::new(process::ProcessWatch::new(pid)?);
        self.attach_with_recipient(holding, pid, Some(process)).map(RemoteConsumerGrant)
    }

    fn attach_with_recipient(
        &mut self,
        holding: u32,
        recipient_pid: u32,
        process: Option<Arc<process::ProcessWatch>>,
    ) -> Result<ConsumerGrant, ArenaError> {
        if self.control.word(TERMINAL).load(SeqCst) != 0 {
            return Err(ArenaError::Closed);
        }
        self.poll_cleanup()?;
        let len = ClaimMap::allocation_len(holding)?;
        let reservation = self.admission.admit(HoldingRequest {
            frames: holding,
            claim_bytes: len as u64,
        })?;
        let incarnation = reservation.incarnation();
        let result = (|| {
            let (wake, receiver) = wait::channel()?;
            let release_wake = Arc::new(wait::Wake::from_fd(receiver.fd()?)?);
            let claims = Arc::new(ClaimMap::new(incarnation, holding, len, wake, release_wake, recipient_pid)?);
            let control_fd = self.control.storage.try_clone_fd()?;
            let resources = self.resources.as_ref().expect("admission requires installed allocation");
            let resource_fd = resources.storage.try_clone_fd()?;
            let claim_fd = claims.storage.try_clone_fd()?;
            claims.mapping_slot(0).store(resources.generation, SeqCst);
            claims.word(OFFERED_GENERATION).store(resources.generation, SeqCst);
            let grant = ConsumerGrant {
                control_fd: Some(control_fd),
                arena_scope: self.control.scope,
                resource_fd: Some(resource_fd),
                claim_fd: Some(claim_fd),
                reader_fd: Some(receiver.into_fd()),
                layout: resources.layout,
                generation: resources.generation,
                drain_timeout: self.drain_timeout,
                mapping_slot: 0,
                claims,
                consumed: false,
            };
            self.cleanup
                .track(Arc::clone(&grant.claims), self.native_resources, process, self.drain_timeout)?;
            Ok(grant)
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

    pub fn publish(&mut self, descriptor: FrameDescriptor, bytes: &[u8]) -> Result<PublishOutcome, ArenaError> {
        self.publish_prepared(bytes, |_, _| Ok(Some(descriptor)))
    }

    /// Prepare an external resource only after it is retired and unclaimed.
    /// The callback may reject a target whose backend work has not completed.
    /// Published metadata must name producer-readiness synchronization for any
    /// writes that remain asynchronous when the callback returns.
    pub(crate) fn publish_resource(
        &mut self,
        prepare: impl FnMut(u32, u64) -> Result<Option<FrameDescriptor>, ArenaError>,
    ) -> Result<PublishOutcome, ArenaError> {
        if self
            .resources
            .as_ref()
            .is_some_and(|resources| resources.layout.payload_capacity != 0)
        {
            return Err(ArenaError::Configuration("native resources require no inline CPU storage"));
        }
        self.publish_prepared(&[], prepare)
    }

    fn publish_prepared(
        &mut self,
        bytes: &[u8],
        mut prepare: impl FnMut(u32, u64) -> Result<Option<FrameDescriptor>, ArenaError>,
    ) -> Result<PublishOutcome, ArenaError> {
        if self.control.word(TERMINAL).load(SeqCst) != 0 {
            return Err(ArenaError::Closed);
        }
        self.poll_cleanup()?;
        let Some(resources) = self.resources.as_ref() else {
            return Ok(PublishOutcome::Dropped);
        };
        if bytes.len() > resources.layout.payload_capacity {
            return Err(ArenaError::PayloadTooLarge);
        }
        if self.cursor == MAX_GENERATION {
            self.control.word(TERMINAL).store(1, SeqCst);
            for claims in self.claims.values() {
                claims.close();
            }
            return Err(ArenaError::GenerationsExhausted);
        }
        let oldest = self.cursor.saturating_sub(resources.layout.history as u64 - 1).max(1);
        for offset in 0..resources.layout.resources {
            let index = (self.next_slot + offset) % resources.layout.resources;
            let state = resources.state(index).load(SeqCst);
            let generation = state >> 1;
            if generation != 0 && generation >= oldest {
                continue;
            }
            // Retire BEFORE scanning claims. Repeated attempts leave the
            // generation retired, so no new successful acquisitions can pin it.
            resources.state(index).store(generation << 1, SeqCst);
            if generation != 0 && self.claims.values().any(|claims| claims.contains(generation)) {
                continue;
            }
            let Some(mut descriptor) = prepare(index as u32, generation)? else {
                continue;
            };
            let cursor = self.cursor + 1;
            let payload_offset = resources.layout.payload_offset(index);
            descriptor.cursor = cursor;
            descriptor.config_generation = resources.generation;
            descriptor.payload_offset = payload_offset as u64;
            descriptor.payload_len = bytes.len() as u64;
            // SAFETY: resource is outside history, retired, and unclaimed.
            // SC retirement/scan excludes successful concurrent acquisition.
            // This producer owns the only write API; descriptors are copied
            // by readers only after validating a claim for this generation.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    resources.storage.as_ptr().add(payload_offset).cast_mut(),
                    bytes.len(),
                );
                resources.descriptor_ptr(index).write(descriptor);
            }
            resources.state(index).store((cursor << 1) | 1, SeqCst);
            self.control.ring(cursor).store(index as u64 + 1, SeqCst);
            self.control.word(LATEST).store(cursor, SeqCst);
            self.cursor = cursor;
            self.next_slot = (index + 1) % resources.layout.resources;
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
            if claims.word(QUIESCENT).load(SeqCst) == 0
                || claims.has_mappings()
                || (0..claims.frames).any(|index| claims.slot(index).load(SeqCst) != 0)
            {
                return true;
            }
            // QUIESCENT proves no more consumer-side claim-map access. Pending
            // GPU claims can outlive that acknowledgement, hence the separate
            // empty-claim check above. Process-exit cleanup needs OS proof.
            self.admission.close(*incarnation).expect("tracked incarnation");
            self.admission.complete_cleanup(*incarnation).expect("closed incarnation");
            self.cleanup.remove(*incarnation);
            false
        });
    }
}

impl Drop for ArenaProducer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Debug)]
struct ConsumerLifetime {
    claims: Arc<ClaimMap>,
    retirement: retirement::Retirement,
}

#[derive(Debug)]
struct ConsumerResources {
    map: ManuallyDrop<ResourceMap>,
    mapping_slot: usize,
    claims: Arc<ClaimMap>,
    attachment: Option<Box<dyn ResourceAttachment>>,
}

/// Backend resources share the mapping owner's lifetime and retirement
/// acknowledgement. No independent native lease table or release path.
pub(crate) trait ResourceAttachment: std::fmt::Debug + Send + Sync + 'static {
    fn as_any(&self) -> &dyn std::any::Any;
    fn validate_descriptor(&self, descriptor: &FrameDescriptor) -> Result<(), ArenaError>;
}

impl Drop for ConsumerLifetime {
    fn drop(&mut self) {
        self.retirement.close();
    }
}

#[derive(Debug)]
pub struct ArenaConsumer {
    control: Arc<ControlMap>,
    lifetime: Arc<ConsumerLifetime>,
    resources: Option<Arc<ConsumerResources>>,
    receiver: wait::Receiver,
}

impl ArenaConsumer {
    pub fn from_grant(grant: ConsumerGrant) -> Result<Self, ArenaError> {
        Self::from_grant_with_resources(grant, None)
    }

    pub(crate) fn from_native_grant(
        grant: ConsumerGrant,
        slots: usize,
        resources: Box<dyn ResourceAttachment>,
    ) -> Result<Self, ArenaError> {
        if grant.layout.payload_capacity != 0 || grant.layout.resources != slots {
            return Err(ArenaError::Mapping("native resource setup disagrees with current mapping"));
        }
        Self::from_grant_with_resources(grant, Some(resources))
    }

    fn from_grant_with_resources(mut grant: ConsumerGrant, attachment: Option<Box<dyn ResourceAttachment>>) -> Result<Self, ArenaError> {
        let control = ControlMap::map(
            grant.control_fd.take().expect("single-use grant"),
            grant.layout.history,
            grant.arena_scope,
        )?;
        let map = ResourceMap::map(
            grant.resource_fd.take().expect("single-use grant"),
            grant.layout,
            control.scope,
            grant.generation,
        )?;
        let claims = ClaimMap::map(
            grant.claim_fd.take().expect("single-use grant"),
            grant.claims.incarnation,
            grant.claims.frames,
            grant.claims.storage.len(),
            Arc::clone(&grant.claims.wake),
            Arc::clone(&grant.claims.release_wake),
            grant.claims.recipient_pid,
        )?;
        let receiver = wait::Receiver::from_fd(grant.reader_fd.take().expect("single-use grant"))?;
        let claims = Arc::new(claims);
        let retirement = retirement::Retirement::new(Arc::clone(&claims), grant.drain_timeout)?;
        let lifetime = Arc::new(ConsumerLifetime { claims, retirement });
        grant.consumed = true;
        grant.claims.word(OFFERED_GENERATION).store(0, SeqCst);
        Ok(Self {
            receiver,
            control: Arc::new(control),
            resources: Some(Arc::new(ConsumerResources {
                map: ManuallyDrop::new(map),
                mapping_slot: grant.mapping_slot,
                claims: Arc::clone(&lifetime.claims),
                attachment,
            })),
            lifetime,
        })
    }

    #[must_use]
    pub fn incarnation(&self) -> IncarnationId {
        self.lifetime.claims.incarnation
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) fn claim_scope(&self) -> [u8; 16] {
        self.lifetime.claims.scope
    }

    pub fn acquire_latest(&self, after: u64) -> Result<AcquireOutcome, ArenaError> {
        let cursor = self.control.word(LATEST).load(SeqCst);
        if self.is_closed() {
            return Ok(AcquireOutcome::Closed);
        }
        if !self.is_configured() {
            return Ok(AcquireOutcome::Reconfiguration);
        }
        if cursor == 0 || cursor <= after || cursor < self.control.word(FIRST_CURSOR).load(SeqCst) {
            return Ok(AcquireOutcome::Empty);
        }
        self.acquire(cursor)
    }

    /// Ordered retained delivery. `after == 0` starts at the oldest advertised
    /// frame. A gap names published cursors, not pre-publication drops. The
    /// caller may acknowledge a gap by continuing after its `last` cursor.
    pub fn acquire_next(&self, after: u64) -> Result<AcquireOutcome, ArenaError> {
        let latest = self.control.word(LATEST).load(SeqCst);
        if self.is_closed() {
            return Ok(AcquireOutcome::Closed);
        }
        if !self.is_configured() {
            return Ok(AcquireOutcome::Reconfiguration);
        }
        if latest == 0 || after >= latest {
            return Ok(AcquireOutcome::Empty);
        }
        let oldest = latest
            .saturating_sub(self.control.history as u64 - 1)
            .max(self.control.word(FIRST_CURSOR).load(SeqCst));
        let cursor = if after == 0 { oldest } else { after + 1 };
        if cursor < oldest {
            return Ok(AcquireOutcome::Gap {
                first: cursor,
                last: oldest - 1,
            });
        }
        if cursor > latest {
            return Ok(AcquireOutcome::Empty);
        }
        self.acquire(cursor)
    }

    /// Try precisely this cursor. Retirement or a concurrent ring wrap is a
    /// miss and never silently substitutes a more recent frame.
    pub fn acquire_exact(&self, cursor: u64) -> Result<AcquireOutcome, ArenaError> {
        if cursor == 0 {
            return Err(ArenaError::Configuration("frame cursors start at one"));
        }
        let latest = self.control.word(LATEST).load(SeqCst);
        if self.is_closed() {
            return Ok(AcquireOutcome::Closed);
        }
        if !self.is_configured() {
            return Ok(AcquireOutcome::Reconfiguration);
        }
        if cursor > latest {
            return Ok(AcquireOutcome::Empty);
        }
        let oldest = latest
            .saturating_sub(self.control.history as u64 - 1)
            .max(self.control.word(FIRST_CURSOR).load(SeqCst));
        if cursor < oldest {
            return Ok(AcquireOutcome::Miss { cursor });
        }
        self.acquire(cursor)
    }

    pub(crate) fn is_configured(&self) -> bool {
        self.resources
            .as_ref()
            .is_some_and(|resources| resources.map.generation == self.control.word(CONFIGURATION).load(SeqCst))
    }

    fn is_closed(&self) -> bool {
        self.lifetime.claims.word(ACTIVE).load(SeqCst) == 0 || self.control.word(TERMINAL).load(SeqCst) != 0
    }

    fn acquire(&self, cursor: u64) -> Result<AcquireOutcome, ArenaError> {
        let resources = self.resources.as_ref().expect("configuration checked");
        let map = &resources.map;
        let advertised = self.control.ring(cursor).load(SeqCst);
        if advertised == 0 || advertised > map.layout.resources as u64 {
            return Err(ArenaError::Mapping("advertised resource index is invalid"));
        }
        let index = advertised as usize - 1;
        #[cfg(test)]
        concurrency_tests::run_hook(concurrency_tests::Phase::Selected);
        let Some(claim_slot) = (0..self.lifetime.claims.frames)
            .find(|slot| self.lifetime.claims.slot(*slot).compare_exchange(0, cursor, SeqCst, SeqCst).is_ok())
        else {
            return Ok(AcquireOutcome::HoldingLimit);
        };
        let claim = Claim {
            owner: Some(Arc::clone(resources)),
            lifetime: Arc::clone(&self.lifetime),
            slot: claim_slot,
            release_on_drop: true,
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
        if !self.is_configured() {
            return Ok(AcquireOutcome::Reconfiguration);
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
        if let Some(attachment) = &resources.attachment {
            attachment.validate_descriptor(&descriptor)?;
        }
        Ok(AcquireOutcome::Frame(FrameLease { descriptor, claim }))
    }
}

impl Drop for ArenaConsumer {
    fn drop(&mut self) {
        self.lifetime.retirement.begin_drain();
    }
}

#[derive(Debug)]
struct Claim {
    owner: Option<Arc<ConsumerResources>>,
    lifetime: Arc<ConsumerLifetime>,
    slot: usize,
    release_on_drop: bool,
}

impl Drop for Claim {
    fn drop(&mut self) {
        if self.release_on_drop {
            self.lifetime.claims.return_credit(self.slot);
        }
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
    pub(crate) fn retained_resources(&self) -> Option<&dyn ResourceAttachment> {
        self.claim.owner.as_ref().expect("live frame owns storage").attachment.as_deref()
    }
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
                self.claim
                    .owner
                    .as_ref()
                    .expect("live frame owns storage")
                    .map
                    .storage
                    .as_ptr()
                    .add(self.descriptor.payload_offset as usize),
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
            drain_timeout: std::time::Duration::from_secs(5),
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
