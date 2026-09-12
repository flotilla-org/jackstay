use std::{
    mem::ManuallyDrop,
    os::fd::OwnedFd,
    sync::{Arc, atomic::Ordering::SeqCst},
};

use super::{
    ACTIVE, AdmissionError, ArenaConsumer, ArenaError, ArenaProducer, CLAIM_SLOT_LEN, CONFIGURATION, ClaimMap, ConsumerResources,
    FIRST_CURSOR, HEADER_LEN, IncarnationId, OFFERED_GENERATION, RECONFIGURATION_EPOCH, ResourceLayout, ResourceMap, TERMINAL, wait,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconfigurationStatus {
    Ready { generation: u64 },
    PausedCapacity { requested: u64, available: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigurationInstall {
    Installed,
    Stale,
}

/// A setup offer for an already admitted consumer. It grants no new holding
/// credit and cannot be used to create a consumer incarnation.
pub struct ConfigurationGrant {
    fd: Option<OwnedFd>,
    layout: ResourceLayout,
    generation: u64,
    arena_scope: [u8; 16],
    claims: Arc<ClaimMap>,
    mapping_slot: usize,
    consumed: bool,
}

impl ArenaProducer {
    /// Replace inline CPU storage while preserving the holding reservation,
    /// history length, and producer reserve. The next publication supplies its
    /// complete size/format descriptor. Allocation is budgeted before creation.
    pub fn reconfigure_cpu(&mut self, payload_capacity: usize) -> Result<ReconfigurationStatus, ArenaError> {
        if self.native_resources {
            return Err(ArenaError::Configuration(
                "CPU reconfiguration cannot replace an external native pool",
            ));
        }
        if self.control.word(TERMINAL).load(SeqCst) != 0 {
            return Err(ArenaError::Closed);
        }
        if self.pending_layout.is_some() {
            return Err(AdmissionError::ReconfigurationPending.into());
        }
        let old = self.resources.as_ref().expect("installed allocation");
        let layout = ResourceLayout::new(old.layout.resources, old.layout.history, payload_capacity)?;
        let old_id = self.admission.current_allocation().expect("installed allocation is charged");
        self.admission.begin_reconfiguration(layout.len as u64)?;
        self.control.word(CONFIGURATION).store(0, SeqCst);
        for index in 0..old.layout.resources {
            let state = old.state(index).load(SeqCst);
            old.state(index).store(state & !1, SeqCst);
        }
        self.retired
            .push((old_id, self.resources.take().expect("retired current allocation")));
        self.pending_layout = Some(layout);
        self.control.word(FIRST_CURSOR).store(self.cursor + 1, SeqCst);
        self.signal_reconfiguration()?;
        self.advance_reconfiguration()
    }

    /// Retry a capacity-paused transition after old-resource cleanup or a
    /// failed allocation. Publication remains paused until installation.
    pub fn advance_reconfiguration(&mut self) -> Result<ReconfigurationStatus, ArenaError> {
        if self.control.word(TERMINAL).load(SeqCst) != 0 {
            return Err(ArenaError::Closed);
        }
        self.poll_cleanup()?;
        let Some(layout) = self.pending_layout else {
            return Ok(ReconfigurationStatus::Ready {
                generation: self.control.word(CONFIGURATION).load(SeqCst),
            });
        };
        let allocation = match self.admission.reserve_reconfiguration() {
            Ok(allocation) => allocation,
            Err(AdmissionError::InsufficientMemory { requested, available }) => {
                return Ok(ReconfigurationStatus::PausedCapacity { requested, available });
            }
            Err(error) => return Err(error.into()),
        };
        let map = match ResourceMap::new(layout, self.control.scope, allocation.0) {
            Ok(map) => map,
            Err(error) => {
                self.admission.abandon_reconfiguration_allocation()?;
                return Err(error);
            }
        };
        self.admission.install_reconfiguration(map.storage.len() as u64)?;
        self.resources = Some(Arc::new(map));
        self.pending_layout = None;
        self.next_slot = 0;
        self.control.word(CONFIGURATION).store(allocation.0, SeqCst);
        self.signal_reconfiguration()?;
        Ok(ReconfigurationStatus::Ready { generation: allocation.0 })
    }

    fn signal_reconfiguration(&self) -> Result<(), ArenaError> {
        self.control.word(RECONFIGURATION_EPOCH).fetch_add(1, SeqCst);
        for claims in self.claims.values() {
            claims.signal(wait::RECONFIGURATION)?;
        }
        Ok(())
    }

    pub fn configuration_offer(&mut self, incarnation: IncarnationId) -> Result<Option<ConfigurationGrant>, ArenaError> {
        let claims = self.claims.get(&incarnation).ok_or(AdmissionError::UnknownIncarnation)?;
        if claims.word(ACTIVE).load(SeqCst) == 0 {
            return Err(ArenaError::Closed);
        }
        let Some(resources) = &self.resources else { return Ok(None) };
        if claims.word(OFFERED_GENERATION).load(SeqCst) != 0 {
            return Ok(None);
        }
        let fd = resources.storage.try_clone_fd()?;
        let mapping_slot = (0..claims.frames + 2)
            .find(|index| {
                claims
                    .mapping_slot(*index)
                    .compare_exchange(0, resources.generation, SeqCst, SeqCst)
                    .is_ok()
            })
            .ok_or(ArenaError::Configuration("configuration mapping capacity exhausted"))?;
        claims.word(OFFERED_GENERATION).store(resources.generation, SeqCst);
        let grant = ConfigurationGrant {
            fd: Some(fd),
            layout: resources.layout,
            generation: resources.generation,
            arena_scope: self.control.scope,
            claims: Arc::clone(claims),
            mapping_slot,
            consumed: false,
        };
        if claims.word(ACTIVE).load(SeqCst) == 0 {
            return Err(ArenaError::Closed);
        }
        Ok(Some(grant))
    }

    pub(super) fn collect_retired_allocations(&mut self) {
        let mut index = 0;
        while index < self.retired.len() {
            let (id, map) = &self.retired[index];
            let retained = self.claims.values().any(|claims| {
                (0..claims.frames + 2).any(|slot| claims.mapping_slot(slot).load(SeqCst) == map.generation)
                    || (0..map.layout.resources).any(|slot| {
                        let cursor = map.state(slot).load(SeqCst) >> 1;
                        cursor != 0 && claims.contains(cursor)
                    })
            });
            if retained {
                index += 1;
            } else {
                let id = *id;
                // Destroy the producer mapping before returning its bytes to
                // admission. All recipient mappings and claims were checked.
                drop(self.retired.swap_remove(index));
                self.admission
                    .complete_allocation_cleanup(id)
                    .expect("retired allocation is charged");
            }
        }
    }
}

impl ArenaConsumer {
    pub fn install_configuration(&mut self, mut grant: ConfigurationGrant) -> Result<ConfigurationInstall, ArenaError> {
        if grant.arena_scope != self.control.scope || grant.claims.scope != self.lifetime.claims.scope {
            return Err(ArenaError::Mapping("configuration grant belongs to another arena or incarnation"));
        }
        if self.is_closed() {
            return Err(ArenaError::Closed);
        }
        if grant.generation != self.control.word(CONFIGURATION).load(SeqCst) {
            return Ok(ConfigurationInstall::Stale);
        }
        let map = ResourceMap::map(
            grant.fd.take().expect("single-use offer"),
            grant.layout,
            grant.arena_scope,
            grant.generation,
        )?;
        self.resources = Some(Arc::new(ConsumerResources {
            map: ManuallyDrop::new(map),
            mapping_slot: grant.mapping_slot,
            lifetime: Arc::clone(&self.lifetime),
        }));
        grant.consumed = true;
        grant.claims.word(OFFERED_GENERATION).store(0, SeqCst);
        Ok(ConfigurationInstall::Installed)
    }

    /// Relinquish unleased configuration storage while retaining the control
    /// mapping, wait channel, and any previously returned frame leases.
    pub fn relinquish_configuration(&mut self) {
        self.resources = None;
    }
}

impl ClaimMap {
    pub(super) fn mapping_slot(&self, index: usize) -> &std::sync::atomic::AtomicU64 {
        assert!(index < self.frames + 2);
        self.word(HEADER_LEN + self.frames * (CLAIM_SLOT_LEN + 8) + index * 8)
    }

    pub(super) fn has_mappings(&self) -> bool {
        (0..self.frames + 2).any(|index| self.mapping_slot(index).load(SeqCst) != 0)
    }
}

impl Drop for ConsumerResources {
    fn drop(&mut self) {
        // SAFETY: the last Arc owns this map exclusively. Unmap before the
        // acknowledgement permits the producer to reclaim its allocation bytes.
        unsafe { ManuallyDrop::drop(&mut self.map) };
        self.lifetime.claims.mapping_slot(self.mapping_slot).store(0, SeqCst);
        let _ = self.lifetime.claims.release_wake.signal();
    }
}

impl Drop for ConfigurationGrant {
    fn drop(&mut self) {
        if !self.consumed {
            drop(self.fd.take());
            self.claims.mapping_slot(self.mapping_slot).store(0, SeqCst);
            self.claims.word(OFFERED_GENERATION).store(0, SeqCst);
            let _ = self.claims.release_wake.signal();
        }
    }
}
