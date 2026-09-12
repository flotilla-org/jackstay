//! Allocation accounting only. The arena supplies the mapping/lease/completion
//! proofs before acknowledging cleanup; this book cannot establish them.
use super::{AdmissionBook, AdmissionError};

/// Identifies one resource allocation in its producer's admission book. Never
/// reuse an old identity for a replacement, even when its geometry is equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AllocationId(pub(super) u64);

#[derive(Debug)]
pub(super) struct PendingAllocation {
    upper_bound: u64,
    reserved: Option<AllocationId>,
}

impl AdmissionBook {
    #[must_use]
    pub fn current_allocation(&self) -> Option<AllocationId> {
        self.current_allocation
    }

    /// Includes fixed control bytes, claim mappings, all unreclaimed resource
    /// generations, and any reserved replacement allocation upper bound.
    #[must_use]
    pub fn committed_bytes(&self) -> u64 {
        self.committed_bytes
    }

    /// Start a producer-serialized transition before retiring publication.
    /// Resource count/history/reserve remain unchanged. Admission stays closed
    /// through allocation and installation. No bytes are released here.
    pub fn begin_reconfiguration(&mut self, upper_bound: u64) -> Result<(), AdmissionError> {
        if self.pending_allocation.is_some() {
            return Err(AdmissionError::ReconfigurationPending);
        }
        if upper_bound == 0 {
            return Err(AdmissionError::InvalidAllocationSize);
        }
        // Reject a proposal that cannot fit even after all old resource
        // generations are reclaimed. Existing claim reservations stay covered.
        let resource_bytes: u64 = self.allocations.values().sum();
        let nonresource_bytes = self.committed_bytes - resource_bytes;
        let available = self.limits.memory_budget - nonresource_bytes;
        if upper_bound > available {
            return Err(AdmissionError::InsufficientMemory {
                requested: upper_bound,
                available,
            });
        }
        self.next_allocation.checked_add(1).ok_or(AdmissionError::IdsExhausted)?;
        self.pending_allocation = Some(PendingAllocation {
            upper_bound,
            reserved: None,
        });
        self.current_allocation = None;
        Ok(())
    }

    /// Reserve before allocating. Insufficient overlap capacity leaves the
    /// proposal pending and admission paused, allowing proven old-allocation
    /// cleanup to make space. Retrying an existing reservation is idempotent.
    pub fn reserve_reconfiguration(&mut self) -> Result<AllocationId, AdmissionError> {
        let pending = self.pending_allocation.as_mut().ok_or(AdmissionError::NoReconfiguration)?;
        if let Some(id) = pending.reserved {
            return Ok(id);
        }
        let available = self.limits.memory_budget - self.committed_bytes;
        if pending.upper_bound > available {
            return Err(AdmissionError::InsufficientMemory {
                requested: pending.upper_bound,
                available,
            });
        }
        let next = self.next_allocation.checked_add(1).ok_or(AdmissionError::IdsExhausted)?;
        let id = AllocationId(self.next_allocation);
        self.next_allocation = next;
        self.committed_bytes += pending.upper_bound;
        self.allocations.insert(id, pending.upper_bound);
        pending.reserved = Some(id);
        Ok(id)
    }

    /// Install only after allocation and setup succeeded. The allocator must
    /// obey the reserved upper bound during allocation, not merely compare its
    /// final size here. Any unused portion of that bound becomes available.
    pub fn install_reconfiguration(&mut self, actual_bytes: u64) -> Result<AllocationId, AdmissionError> {
        let pending = self.pending_allocation.as_ref().ok_or(AdmissionError::NoReconfiguration)?;
        let id = pending.reserved.ok_or(AdmissionError::AllocationNotReserved)?;
        if actual_bytes == 0 || actual_bytes > pending.upper_bound {
            return Err(AdmissionError::InvalidAllocationSize);
        }
        self.committed_bytes -= pending.upper_bound - actual_bytes;
        self.allocations.insert(id, actual_bytes);
        self.current_allocation = Some(id);
        self.pending_allocation = None;
        Ok(id)
    }

    /// Return a failed setup allocation's reservation only after the arena has
    /// destroyed its allocation and every partial mapping/handle. The proposal
    /// stays pending, so another admission cannot consume its retry capacity.
    pub fn abandon_reconfiguration_allocation(&mut self) -> Result<(), AdmissionError> {
        let pending = self.pending_allocation.as_mut().ok_or(AdmissionError::NoReconfiguration)?;
        let id = pending.reserved.take().ok_or(AdmissionError::AllocationNotReserved)?;
        self.committed_bytes -= self.allocations.remove(&id).expect("reserved allocation is charged");
        Ok(())
    }

    /// Acknowledge actual destruction of a retired allocation. The arena must
    /// first prove all mappings, setup offers, leases, and asynchronous uses
    /// have retired. Neither installation nor timeout supplies that proof.
    pub fn complete_allocation_cleanup(&mut self, id: AllocationId) -> Result<(), AdmissionError> {
        if self.current_allocation == Some(id) || self.pending_allocation.as_ref().and_then(|pending| pending.reserved) == Some(id) {
            return Err(AdmissionError::AllocationInUse);
        }
        let bytes = self.allocations.remove(&id).ok_or(AdmissionError::UnknownAllocation)?;
        self.committed_bytes -= bytes;
        Ok(())
    }
}
