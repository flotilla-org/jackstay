use std::{
    mem::ManuallyDrop,
    os::fd::OwnedFd,
    sync::{Arc, atomic::Ordering::SeqCst},
};

use serde::{Deserialize, Serialize};

use super::{
    ACTIVE, AdmissionError, AllocationId, ArenaConsumer, ArenaError, ArenaProducer, CLAIM_SLOT_LEN, CONFIGURATION, ClaimMap,
    ConsumerResources, FIRST_CURSOR, HEADER_LEN, IncarnationId, OFFERED_GENERATION, RECONFIGURATION_EPOCH, ResourceAttachment,
    ResourceLayout, ResourceMap, TERMINAL, VERSION, wait,
};

pub(crate) trait RetiredResource: Send {
    /// Actual producer completion, independently of the consumer claims.
    fn ready(&self) -> Result<bool, ArenaError>;
}

pub(super) struct RetiredAllocation {
    id: AllocationId,
    map: Arc<ResourceMap>,
    native: Option<Box<dyn RetiredResource>>,
}

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

/// Setup metadata accompanying one replacement resource FD. Control, claim,
/// and notification mappings remain those of the existing incarnation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigurationDescriptor {
    pub version: u64,
    pub generation: u64,
    pub incarnation: u64,
    pub arena_scope: [u8; 16],
    pub claim_scope: [u8; 16],
    pub recipient_pid: u32,
    pub mapping_slot: u32,
    pub resources: u32,
    pub history: u32,
    pub payload_capacity: u64,
    pub resource_map_len: u64,
}

impl ConfigurationGrant {
    /// Export only to the incarnation's already monitored process. The host
    /// must transfer this single-use offer to that process and relinquish its
    /// setup FD. Dropping an exported FD is not recipient retirement proof.
    pub fn into_parts(mut self) -> Result<(ConfigurationDescriptor, OwnedFd), ArenaError> {
        if self.claims.recipient_pid == 0 {
            return Err(ArenaError::Configuration("replacement export requires a process-bound incarnation"));
        }
        let descriptor = ConfigurationDescriptor {
            version: VERSION,
            generation: self.generation,
            incarnation: self.claims.incarnation.0,
            arena_scope: self.arena_scope,
            claim_scope: self.claims.scope,
            recipient_pid: self.claims.recipient_pid,
            mapping_slot: self.mapping_slot as u32,
            resources: self.layout.resources as u32,
            history: self.layout.history as u32,
            payload_capacity: self.layout.payload_capacity as u64,
            resource_map_len: self.layout.len as u64,
        };
        let fd = self.fd.take().expect("single-use offer");
        self.consumed = true;
        Ok((descriptor, fd))
    }

    /// Import a replacement for an already mapped consumer incarnation.
    ///
    /// # Safety
    /// The sender must be that arena's conforming sole producer and this
    /// process the sole recipient of this single-use offer. Do not replay,
    /// forward, or fork it. The sender must keep the allocation charged and
    /// initialized until this recipient's mapping/lease retirement or verified
    /// process exit. Header checks cannot prove another process obeys that
    /// lifetime protocol. Drop all other copies of the received resource FD.
    pub unsafe fn from_parts(consumer: &ArenaConsumer, descriptor: ConfigurationDescriptor, fd: OwnedFd) -> Result<Self, ArenaError> {
        let claims = &consumer.lifetime.claims;
        if descriptor.version != VERSION
            || descriptor.generation == 0
            || descriptor.arena_scope != consumer.control.scope
            || descriptor.claim_scope != claims.scope
            || descriptor.incarnation != claims.incarnation.0
            || descriptor.recipient_pid != std::process::id()
            || descriptor.recipient_pid != claims.recipient_pid
        {
            return Err(ArenaError::Mapping(
                "replacement grant belongs to another arena, incarnation, or process",
            ));
        }
        let mapping_slot = descriptor.mapping_slot as usize;
        if mapping_slot >= claims.frames + 2
            || claims.mapping_slot(mapping_slot).load(SeqCst) != descriptor.generation
            || claims.word(OFFERED_GENERATION).load(SeqCst) != descriptor.generation
        {
            return Err(ArenaError::Mapping("replacement grant disagrees with outstanding offer"));
        }
        let payload_capacity =
            usize::try_from(descriptor.payload_capacity).map_err(|_| ArenaError::Mapping("replacement payload capacity overflows"))?;
        let layout = ResourceLayout::new(descriptor.resources as usize, descriptor.history as usize, payload_capacity)?;
        if layout.history != consumer.control.history || layout.len as u64 != descriptor.resource_map_len {
            return Err(ArenaError::Mapping("replacement mapping layout disagrees with setup"));
        }
        Ok(Self {
            fd: Some(fd),
            layout,
            generation: descriptor.generation,
            arena_scope: descriptor.arena_scope,
            claims: Arc::clone(claims),
            mapping_slot,
            consumed: false,
        })
    }
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
        self.begin_reconfiguration(payload_capacity, 0, || None)?;
        self.advance_reconfiguration()
    }

    pub(crate) fn begin_native_reconfiguration(
        &mut self,
        external_bytes: u64,
        retire: impl FnOnce() -> Box<dyn RetiredResource>,
    ) -> Result<(), ArenaError> {
        if !self.native_resources {
            return Err(ArenaError::Configuration("native replacement requires an external pool"));
        }
        self.begin_reconfiguration(0, external_bytes, || Some(retire()))
    }

    fn begin_reconfiguration(
        &mut self,
        payload_capacity: usize,
        external_bytes: u64,
        retire: impl FnOnce() -> Option<Box<dyn RetiredResource>>,
    ) -> Result<(), ArenaError> {
        if self.control.word(TERMINAL).load(SeqCst) != 0 {
            return Err(ArenaError::Closed);
        }
        if self.pending_layout.is_some() {
            return Err(AdmissionError::ReconfigurationPending.into());
        }
        let old = self.resources.as_ref().expect("installed allocation");
        let layout = ResourceLayout::new(old.layout.resources, old.layout.history, payload_capacity)?;
        let old_id = self.admission.current_allocation().expect("installed allocation is charged");
        let bytes = (layout.len as u64)
            .checked_add(external_bytes)
            .ok_or(ArenaError::Configuration("replacement allocation byte total overflow"))?;
        self.admission.begin_reconfiguration(bytes)?;
        self.control.word(CONFIGURATION).store(0, SeqCst);
        for index in 0..old.layout.resources {
            let state = old.state(index).load(SeqCst);
            old.state(index).store(state & !1, SeqCst);
        }
        self.retired.push(RetiredAllocation {
            id: old_id,
            map: self.resources.take().expect("retired current allocation"),
            native: retire(),
        });
        self.pending_layout = Some(layout);
        self.control.word(FIRST_CURSOR).store(self.cursor + 1, SeqCst);
        self.signal_reconfiguration()
    }

    /// Retry a capacity-paused transition after old-resource cleanup or a
    /// failed allocation. Publication remains paused until installation.
    pub fn advance_reconfiguration(&mut self) -> Result<ReconfigurationStatus, ArenaError> {
        if self.native_resources {
            return Err(ArenaError::Configuration("native replacement requires its pool allocator"));
        }
        let (status, installed) = self.advance_with_allocation(|| Ok(((), 0)))?;
        if installed.is_some() {
            self.signal_reconfiguration()?;
        }
        Ok(status)
    }

    // Reserve maps and backing storage together. Return the external owner to
    // the caller before it notifies consumers about the installed generation.
    pub(crate) fn advance_with_allocation<T>(
        &mut self,
        allocate: impl FnOnce() -> Result<(T, u64), ArenaError>,
    ) -> Result<(ReconfigurationStatus, Option<T>), ArenaError> {
        if self.control.word(TERMINAL).load(SeqCst) != 0 {
            return Err(ArenaError::Closed);
        }
        self.poll_cleanup()?;
        let Some(layout) = self.pending_layout else {
            return Ok((
                ReconfigurationStatus::Ready {
                    generation: self.control.word(CONFIGURATION).load(SeqCst),
                },
                None,
            ));
        };
        let allocation = match self.admission.reserve_reconfiguration() {
            Ok(allocation) => allocation,
            Err(AdmissionError::InsufficientMemory { requested, available }) => {
                return Ok((ReconfigurationStatus::PausedCapacity { requested, available }, None));
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
        let (external, external_bytes) = match allocate() {
            Ok(allocation) => allocation,
            Err(error) => {
                drop(map);
                self.admission.abandon_reconfiguration_allocation()?;
                return Err(error);
            }
        };
        let installed = (map.storage.len() as u64)
            .checked_add(external_bytes)
            .ok_or(AdmissionError::InvalidAllocationSize)
            .and_then(|bytes| self.admission.install_reconfiguration(bytes));
        if let Err(error) = installed {
            drop(external);
            drop(map);
            self.admission.abandon_reconfiguration_allocation()?;
            return Err(error.into());
        }
        self.resources = Some(Arc::new(map));
        self.pending_layout = None;
        self.next_slot = 0;
        self.control.word(CONFIGURATION).store(allocation.0, SeqCst);
        Ok((ReconfigurationStatus::Ready { generation: allocation.0 }, Some(external)))
    }

    pub(crate) fn signal_reconfiguration(&self) -> Result<(), ArenaError> {
        if self
            .control
            .word(RECONFIGURATION_EPOCH)
            .fetch_update(SeqCst, SeqCst, |epoch| epoch.checked_add(1))
            .is_err()
        {
            // Epochs participate in the wait predicate. Reusing an old value
            // could hide a transition; close acquisition without revoking any
            // already acquired storage or discarding its retirement owner.
            self.control.word(TERMINAL).store(1, SeqCst);
            for claims in self.claims.values() {
                claims.close();
            }
            return Err(ArenaError::GenerationsExhausted);
        }
        for claims in self.claims.values() {
            claims.signal(wait::RECONFIGURATION)?;
        }
        Ok(())
    }

    /// Install this producer's current CPU allocation on an already admitted
    /// local consumer. No admission or frame ownership changes are implied.
    pub fn configure_consumer(&mut self, consumer: &mut ArenaConsumer) -> Result<Option<ConfigurationInstall>, ArenaError> {
        if self.control.scope != consumer.control.scope
            || self
                .claims
                .get(&consumer.incarnation())
                .is_none_or(|claims| claims.scope != consumer.lifetime.claims.scope)
        {
            return Err(ArenaError::Configuration("consumer belongs to another producer"));
        }
        if consumer.is_closed() {
            return Err(ArenaError::Closed);
        }
        if consumer.is_configured() {
            return Ok(None);
        }
        self.configuration_offer(consumer.incarnation())?
            .map(|offer| consumer.install_configuration(offer))
            .transpose()
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

    pub(super) fn collect_retired_allocations(&mut self) -> Result<(), ArenaError> {
        let mut index = 0;
        while index < self.retired.len() {
            let RetiredAllocation { id, map, native } = &self.retired[index];
            let retained = self.claims.values().any(|claims| {
                (0..claims.frames + 2).any(|slot| claims.mapping_slot(slot).load(SeqCst) == map.generation)
                    || (0..map.layout.resources).any(|slot| {
                        let cursor = map.state(slot).load(SeqCst) >> 1;
                        cursor != 0 && claims.contains(cursor)
                    })
            });
            if retained || !native.as_ref().map(|resource| resource.ready()).transpose()?.unwrap_or(true) {
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
        Ok(())
    }
}

impl ArenaConsumer {
    pub fn install_configuration(&mut self, grant: ConfigurationGrant) -> Result<ConfigurationInstall, ArenaError> {
        self.install_with_resources(grant, None)
    }

    pub(crate) fn install_native_configuration(
        &mut self,
        grant: ConfigurationGrant,
        slots: usize,
        resources: Box<dyn ResourceAttachment>,
    ) -> Result<ConfigurationInstall, ArenaError> {
        if grant.layout.payload_capacity != 0 || grant.layout.resources != slots {
            return Err(ArenaError::Mapping("native replacement handles disagree with resource layout"));
        }
        self.install_with_resources(grant, Some(resources))
    }

    fn install_with_resources(
        &mut self,
        mut grant: ConfigurationGrant,
        attachment: Option<Box<dyn ResourceAttachment>>,
    ) -> Result<ConfigurationInstall, ArenaError> {
        if grant.arena_scope != self.control.scope || grant.claims.scope != self.lifetime.claims.scope {
            return Err(ArenaError::Mapping("configuration grant belongs to another arena or incarnation"));
        }
        if self.is_closed() {
            return Err(ArenaError::Closed);
        }
        let map = ResourceMap::map(
            grant.fd.take().expect("single-use offer"),
            grant.layout,
            grant.arena_scope,
            grant.generation,
        )?;
        // Staleness is a normal retry only for a valid resource mapping. Do not
        // hide contradictory available metadata behind the stale outcome.
        if grant.generation != self.control.word(CONFIGURATION).load(SeqCst) {
            drop(map);
            return Ok(ConfigurationInstall::Stale);
        }
        self.resources = Some(Arc::new(ConsumerResources {
            map: ManuallyDrop::new(map),
            mapping_slot: grant.mapping_slot,
            claims: Arc::clone(&self.lifetime.claims),
            attachment,
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
        // Imported surfaces and readiness handles must also be destroyed before
        // the acknowledgement permits allocation-byte reclamation.
        drop(self.attachment.take());
        // SAFETY: the last Arc owns this map exclusively. Unmap before the
        // acknowledgement permits the producer to reclaim its allocation bytes.
        unsafe { ManuallyDrop::drop(&mut self.map) };
        self.claims.mapping_slot(self.mapping_slot).store(0, SeqCst);
        let _ = self.claims.release_wake.signal();
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::acquisition::arena::{AcquireOutcome, ArenaConfig, Cancellation, FrameDescriptor, WaitInterest, WaitOutcome};

    #[test]
    fn exhausted_reconfiguration_epochs_close_waiters_without_revoking_frames() {
        // A replacement signals both retirement and installation. Exercise
        // exhaustion at each boundary, without executing 2^64 transitions.
        for epoch in [u64::MAX, u64::MAX - 1] {
            let mut producer = ArenaProducer::new(ArenaConfig {
                resource_capacity: 6,
                retained_history: 2,
                producer_reserve: 1,
                max_incarnations: 1,
                payload_capacity: 4,
                memory_budget: 1024 * 1024,
                drain_timeout: Duration::from_secs(5),
            })
            .unwrap();
            let mut consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
            producer.publish(FrameDescriptor::default(), b"held").unwrap();
            let AcquireOutcome::Frame(held) = consumer.acquire_latest(0).unwrap() else {
                panic!("missing frame")
            };
            producer.control.word(RECONFIGURATION_EPOCH).store(epoch, SeqCst);
            let observed = consumer.events();
            assert!(matches!(producer.reconfigure_cpu(8), Err(ArenaError::GenerationsExhausted)));
            assert_eq!(consumer.events().reconfiguration_epoch, u64::MAX, "epoch wrapped");
            assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Closed));
            assert!(matches!(producer.attach(1), Err(ArenaError::Closed)));
            assert!(matches!(
                producer.publish(FrameDescriptor::default(), b"late"),
                Err(ArenaError::Closed)
            ));
            assert!(
                matches!(consumer.wait(observed, WaitInterest::ALL, &Cancellation::new().unwrap(), Some(Duration::from_secs(1))).unwrap(),
                WaitOutcome::Changed(events) if events.closed)
            );
            assert_eq!(held.bytes(), b"held");
            drop(consumer);
            assert!(!producer.poll_shutdown_ready().unwrap());
            drop(held);
            assert!(producer.poll_shutdown_ready().unwrap());
        }
    }
}
