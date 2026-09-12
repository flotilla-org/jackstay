//! Shared CPU/native acquisition ownership.
//!
//! Admission reserves worst-case distinct resource holdings. Publication and
//! claim slots will consume these reservations; sharing one frame does not
//! justify promising the same capacity to another incarnation.

use std::collections::BTreeMap;

use thiserror::Error;

#[cfg(unix)]
pub mod arena;

mod allocation;
pub use allocation::AllocationId;

/// One admitted lifetime, scoped to its producer's admission book. A name or
/// authorization identity is deliberately not part of this identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct IncarnationId(u64);

#[derive(Debug, Clone, Copy)]
pub struct AdmissionLimits {
    /// Usable resource slots in the current allocation.
    pub resource_capacity: u32,
    pub retained_history: u32,
    pub producer_reserve: u32,
    /// Bytes already allocated for the initial resource generation.
    pub allocated_bytes: u64,
    /// Persistent producer control data, independent of resource generations.
    pub fixed_bytes: u64,
    /// Total allocation budget, including consumer claim mappings.
    pub memory_budget: u64,
    /// Includes closing incarnations until their cleanup has completed.
    pub max_incarnations: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct HoldingRequest {
    pub frames: u32,
    /// The library-computed allocation size of the claim mapping, including
    /// headers and OS allocation rounding. Not a value supplied by the peer.
    pub claim_bytes: u64,
}

/// A grant is an admission record, not a frame lease. Dropping this value does
/// not release capacity; cleanup of the incarnation owns that transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldingReservation {
    incarnation: IncarnationId,
    frames: u32,
    claim_bytes: u64,
}

impl HoldingReservation {
    #[must_use]
    pub fn incarnation(self) -> IncarnationId {
        self.incarnation
    }

    #[must_use]
    pub fn frames(self) -> u32 {
        self.frames
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AdmissionError {
    #[error("invalid admission limits: {0}")]
    InvalidLimits(&'static str),
    #[error("a holding request needs nonzero frames and claim allocation bytes")]
    InvalidRequest,
    #[error("requested {requested} holding slots, but only {available} can be reserved")]
    InsufficientResources { requested: u32, available: u32 },
    #[error("requested {requested} allocation bytes, but only {available} remain")]
    InsufficientMemory { requested: u64, available: u64 },
    #[error("the incarnation table is full, including incarnations still draining")]
    IncarnationCapacity,
    #[error("incarnation identities exhausted")]
    IdsExhausted,
    #[error("incarnation is unknown or its cleanup already completed")]
    UnknownIncarnation,
    #[error("an active incarnation cannot complete cleanup")]
    StillActive,
    #[error("a configuration replacement is pending; admission is paused")]
    ReconfigurationPending,
    #[error("there is no pending configuration replacement")]
    NoReconfiguration,
    #[error("replacement allocation has not been reserved")]
    AllocationNotReserved,
    #[error("allocation is unknown or its cleanup already completed")]
    UnknownAllocation,
    #[error("the current or pending allocation cannot complete cleanup")]
    AllocationInUse,
    #[error("actual allocation size must be nonzero and no greater than its reserved upper bound")]
    InvalidAllocationSize,
}

#[derive(Debug)]
struct IncarnationRecord {
    reservation: HoldingReservation,
    closing: bool,
}

/// The producer serializes admission and reclamation through this book. It
/// contains no authorization policy and requires no per-frame broker request.
#[derive(Debug)]
pub struct AdmissionBook {
    limits: AdmissionLimits,
    next_incarnation: u64,
    reservations: BTreeMap<IncarnationId, IncarnationRecord>,
    reserved_frames: u32,
    committed_bytes: u64,
    allocations: BTreeMap<AllocationId, u64>,
    current_allocation: Option<AllocationId>,
    next_allocation: u64,
    pending_allocation: Option<allocation::PendingAllocation>,
}

impl AdmissionBook {
    pub fn new(limits: AdmissionLimits) -> Result<Self, AdmissionError> {
        let protected = limits
            .retained_history
            .checked_add(limits.producer_reserve)
            .ok_or(AdmissionError::InvalidLimits("history and producer reserve overflow"))?;
        if limits.producer_reserve == 0 || protected > limits.resource_capacity {
            return Err(AdmissionError::InvalidLimits(
                "resource capacity must cover history and a nonzero producer reserve",
            ));
        }
        let committed_bytes = limits
            .allocated_bytes
            .checked_add(limits.fixed_bytes)
            .ok_or(AdmissionError::InvalidLimits("initial allocation byte total overflows"))?;
        if committed_bytes > limits.memory_budget {
            return Err(AdmissionError::InvalidLimits("existing allocation exceeds the memory budget"));
        }
        if limits.max_incarnations == 0 {
            return Err(AdmissionError::InvalidLimits("incarnation capacity must be nonzero"));
        }
        Ok(Self {
            limits,
            next_incarnation: 1,
            reservations: BTreeMap::new(),
            reserved_frames: 0,
            committed_bytes,
            allocations: BTreeMap::from([(AllocationId(1), limits.allocated_bytes)]),
            current_allocation: Some(AllocationId(1)),
            next_allocation: 2,
            pending_allocation: None,
        })
    }

    /// Reserve before allocating/transferring the claim mapping. Every failure
    /// leaves existing grants and accounting unchanged. This returns an error
    /// with the smaller available resource count rather than silently shrinking
    /// the requested reservation.
    pub fn admit(&mut self, request: HoldingRequest) -> Result<HoldingReservation, AdmissionError> {
        if self.pending_allocation.is_some() {
            return Err(AdmissionError::ReconfigurationPending);
        }
        if request.frames == 0 || request.claim_bytes == 0 {
            return Err(AdmissionError::InvalidRequest);
        }
        let available = self.available_frames();
        if request.frames > available {
            return Err(AdmissionError::InsufficientResources {
                requested: request.frames,
                available,
            });
        }
        if self.reservations.len() as u64 >= u64::from(self.limits.max_incarnations) {
            return Err(AdmissionError::IncarnationCapacity);
        }
        let available = self.limits.memory_budget - self.committed_bytes;
        if request.claim_bytes > available {
            return Err(AdmissionError::InsufficientMemory {
                requested: request.claim_bytes,
                available,
            });
        }
        let next = self.next_incarnation.checked_add(1).ok_or(AdmissionError::IdsExhausted)?;
        let reservation = HoldingReservation {
            incarnation: IncarnationId(self.next_incarnation),
            frames: request.frames,
            claim_bytes: request.claim_bytes,
        };
        self.next_incarnation = next;
        self.reserved_frames += request.frames;
        self.committed_bytes += request.claim_bytes;
        self.reservations.insert(
            reservation.incarnation,
            IncarnationRecord {
                reservation,
                closing: false,
            },
        );
        Ok(reservation)
    }

    /// Record closure without reclaiming anything. The arena must first close
    /// the shared acquisition gate; connection loss alone proves neither
    /// process termination nor completion of reads or GPU commands.
    pub fn close(&mut self, incarnation: IncarnationId) -> Result<(), AdmissionError> {
        let record = self.reservations.get_mut(&incarnation).ok_or(AdmissionError::UnknownIncarnation)?;
        record.closing = true;
        Ok(())
    }

    /// Acknowledge reclamation by the arena's cleanup owner. Call only after
    /// that owner has established completion of all CPU/GPU use and the end of
    /// claim-page access, and has reclaimed the incarnation's mapping. This
    /// accounting operation supplies no such proof itself; it is never an EOF
    /// handler or a timeout-based release.
    pub fn complete_cleanup(&mut self, incarnation: IncarnationId) -> Result<(), AdmissionError> {
        let record = self.reservations.get(&incarnation).ok_or(AdmissionError::UnknownIncarnation)?;
        if !record.closing {
            return Err(AdmissionError::StillActive);
        }
        let reservation = self
            .reservations
            .remove(&incarnation)
            .expect("record was checked above")
            .reservation;
        self.reserved_frames -= reservation.frames;
        self.committed_bytes -= reservation.claim_bytes;
        Ok(())
    }

    /// Resource availability only; memory and incarnation capacity are also
    /// checked transactionally by `admit`.
    #[must_use]
    pub fn available_frames(&self) -> u32 {
        self.limits.resource_capacity - self.limits.retained_history - self.limits.producer_reserve - self.reserved_frames
    }
}
