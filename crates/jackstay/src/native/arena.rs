//! Native staging backed by the common acquisition arena. Native reuse checks
//! supplement the shared claim; they do not replace acquisition ownership.

use std::sync::Arc;

use super::{NativeFrameBackend, NativeStreamParams, SlotClaim, SlotReuseCandidate};
use crate::{
    acquisition::{
        IncarnationId,
        arena::{
            ArenaConfig, ArenaConsumer, ArenaError, ArenaProducer, ConsumerGrant, FrameDescriptor, FrameLease, PublishOutcome,
            ReleaseTimeline, ReleaseTimelineRegistration, RemoteConsumerGrant, ResourceAttachment,
        },
    },
    model::{DamageKind, FrameSyncKind},
};

/// Extra facts required by the arena contract. An already allocated/imported
/// pool supplies its actual byte footprint; producer writes need completion
/// evidence independently of consumer release timelines.
pub trait ArenaNativeBackend: NativeFrameBackend {
    /// Backing-storage bytes to reserve before allocating a pool. Includes
    /// native row/allocation alignment; excludes the arena's separate maps.
    fn pool_allocation_upper_bound(&self, params: &NativeStreamParams, slot_count: u32) -> crate::Result<u64>;
    /// Allocate only if the native layout fits the already reserved bound.
    /// Revalidate before creating storage, including if native alignment has
    /// changed since preflight. Any partial allocation is destroyed on error.
    fn allocate_surface_pool_bounded(
        &mut self,
        params: &NativeStreamParams,
        slot_count: u32,
        reserved_bytes: u64,
    ) -> crate::Result<Self::SurfacePool>;
    fn allocated_pool_bytes(&self, pool: &Self::SurfacePool) -> crate::Result<u64>;
    fn completed_producer_value(&self, fence: &Self::Fence) -> crate::Result<u64>;
}

pub struct NativeArenaGrant<S, Y, G = ConsumerGrant> {
    pub consumer: G,
    pub pool_id: u64,
    pub surface_handles: Vec<S>,
    pub fence_id: u64,
    pub sync_handle: Y,
}

#[derive(Debug)]
struct RetainedNativeResources<S, Y> {
    pool_id: u64,
    surfaces: Vec<S>,
    fence_id: u64,
    sync_handle: Y,
}

impl<S, Y> ResourceAttachment for RetainedNativeResources<S, Y>
where
    S: std::fmt::Debug + Send + Sync + 'static,
    Y: std::fmt::Debug + Send + Sync + 'static,
{
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn validate_descriptor(&self, descriptor: &FrameDescriptor) -> Result<(), ArenaError> {
        if descriptor.pool_id != self.pool_id
            || descriptor.fence_id != self.fence_id
            || descriptor.slot_id as usize >= self.surfaces.len()
            || descriptor.sync_kind != FrameSyncKind::NativeTimeline as u32
        {
            return Err(ArenaError::Mapping(
                "native frame contradicts retained pool, slot, or readiness identity",
            ));
        }
        Ok(())
    }
}

impl<S, Y> NativeArenaGrant<S, Y>
where
    S: std::fmt::Debug + Send + Sync + 'static,
    Y: std::fmt::Debug + Send + Sync + 'static,
{
    /// Consume setup into the common consumer. The resource generation owns
    /// its native handles through all acquired and deferred frame lifetimes.
    pub fn into_consumer(self) -> Result<ArenaConsumer, ArenaError> {
        let mut consumer = ArenaConsumer::from_grant(self.consumer)?;
        let slots = self.surface_handles.len();
        consumer.retain_native_resources(
            slots,
            Box::new(RetainedNativeResources {
                pool_id: self.pool_id,
                surfaces: self.surface_handles,
                fence_id: self.fence_id,
                sync_handle: self.sync_handle,
            }),
        )?;
        Ok(consumer)
    }
}

/// Borrowed only while the acquired frame still owns its resource generation.
/// Satisfy the descriptor's readiness value before using the surface.
pub struct NativeFrameResources<'a, S, Y> {
    pub surface: &'a S,
    pub sync_handle: &'a Y,
}

impl FrameLease {
    pub fn native_resources<S, Y>(&self) -> Result<NativeFrameResources<'_, S, Y>, ArenaError>
    where
        S: std::fmt::Debug + Send + Sync + 'static,
        Y: std::fmt::Debug + Send + Sync + 'static,
    {
        let resources = self
            .retained_resources()
            .and_then(|resources| resources.as_any().downcast_ref::<RetainedNativeResources<S, Y>>())
            .ok_or(ArenaError::Mapping("frame has no retained resources for this native backend"))?;
        Ok(NativeFrameResources {
            surface: &resources.surfaces[self.descriptor().slot_id as usize],
            sync_handle: &resources.sync_handle,
        })
    }
}

pub struct NativeArenaProducer<B: ArenaNativeBackend> {
    arena: ArenaProducer,
    backend: B,
    pool: B::SurfacePool,
    fence: B::Fence,
    params: NativeStreamParams,
    submitted: Vec<u64>,
    sequence: u64,
    fence_value: u64,
    dropped: u64,
    pending_drops: u32,
    faulted: bool,
}

impl<B: ArenaNativeBackend> NativeArenaProducer<B> {
    /// Admit an existing native allocation. Pool byte accounting is checked
    /// before allocating arena metadata or admitting consumers. Negotiated
    /// pools can be supplied by their compositor; authority stays with the host.
    pub fn from_allocated_parts(
        backend: B,
        pool: B::SurfacePool,
        fence: B::Fence,
        params: NativeStreamParams,
        config: ArenaConfig,
    ) -> Result<Self, ArenaError> {
        if config.payload_capacity != 0 {
            return Err(ArenaError::Configuration("native arena has no inline payload"));
        }
        if backend.export_surface_handles(&pool)?.len() != config.resource_capacity as usize {
            return Err(ArenaError::Configuration("native pool capacity differs from arena capacity"));
        }
        let arena = ArenaProducer::with_external_allocation(config, backend.allocated_pool_bytes(&pool)?)?;
        Ok(Self {
            arena,
            backend,
            pool,
            fence,
            params,
            submitted: vec![0; config.resource_capacity as usize],
            sequence: 0,
            fence_value: 0,
            dropped: 0,
            pending_drops: 0,
            faulted: false,
        })
    }

    pub fn attach(&mut self, holding: u32) -> Result<NativeArenaGrant<B::SurfaceHandle, B::SyncHandle>, ArenaError> {
        let consumer = self.arena.attach(holding)?;
        self.grant(consumer)
    }

    pub fn attach_process(
        &mut self,
        holding: u32,
        pid: u32,
    ) -> Result<NativeArenaGrant<B::SurfaceHandle, B::SyncHandle, RemoteConsumerGrant>, ArenaError> {
        let consumer = self.arena.attach_process(holding, pid)?;
        self.grant(consumer)
    }

    fn grant<G>(&self, consumer: G) -> Result<NativeArenaGrant<B::SurfaceHandle, B::SyncHandle, G>, ArenaError> {
        Ok(NativeArenaGrant {
            consumer,
            pool_id: self.backend.pool_id(&self.pool),
            surface_handles: self.backend.export_surface_handles(&self.pool)?,
            fence_id: self.backend.fence_id(&self.fence),
            sync_handle: self.backend.export_sync_handle(&self.fence)?,
        })
    }

    pub fn publish(&mut self, frame: &B::CapturedFrame, timestamp_ns: u64) -> Result<PublishOutcome, ArenaError> {
        if self.faulted {
            return Err(ArenaError::Closed);
        }
        let result = self.publish_frame(frame, timestamp_ns);
        if result.is_err() {
            self.faulted = true;
            self.arena.stop();
        }
        result
    }

    fn publish_frame(&mut self, frame: &B::CapturedFrame, timestamp_ns: u64) -> Result<PublishOutcome, ArenaError> {
        self.sequence = self.sequence.checked_add(1).ok_or(ArenaError::GenerationsExhausted)?;
        let ready = self.backend.completed_producer_value(&self.fence)?;
        let hint = self.backend.frame_slot_hint(frame);
        let result = self.arena.publish_resource(|slot, previous_cursor| {
            if hint.is_some_and(|hint| hint != slot) || self.submitted[slot as usize] > ready {
                return Ok(None);
            }
            let candidate = SlotReuseCandidate {
                slot_id: slot,
                last_cursor: previous_cursor,
            };
            match self.backend.claim_reusable_slot(&mut self.pool, &[candidate])? {
                SlotClaim::WouldBlock => return Ok(None),
                SlotClaim::Ready { slot_id } if slot_id != slot => {
                    return Err(ArenaError::Mapping("backend selected an unclaimed native slot"));
                }
                SlotClaim::Ready { .. } => {}
            }
            self.backend.stage_frame(&mut self.pool, slot, frame)?;
            self.fence_value = self.fence_value.checked_add(1).ok_or(ArenaError::GenerationsExhausted)?;
            self.submitted[slot as usize] = self.fence_value;
            self.backend.signal_fence(&mut self.fence, self.fence_value)?;
            Ok(Some(FrameDescriptor {
                sequence: self.sequence,
                timestamp_ns,
                config_generation: 1,
                pool_id: self.backend.pool_id(&self.pool),
                slot_id: slot,
                width: self.params.width,
                height: self.params.height,
                pixel_format: self.params.pixel_format as u32,
                color_space: self.params.color_space as u32,
                clock_domain: self.params.clock_domain as u32,
                payload_kind: self.backend.payload_kind() as u32,
                modifier: self.params.modifier,
                sync_kind: FrameSyncKind::NativeTimeline as u32,
                fence_id: self.backend.fence_id(&self.fence),
                fence_value: self.fence_value,
                damage_kind: DamageKind::FullFrame as u32,
                damage_base_sequence: self.sequence,
                dropped_before_publish: self.pending_drops,
                producer_drop_count: self.dropped,
                ..FrameDescriptor::default()
            }))
        });
        match result {
            Ok(PublishOutcome::Published { .. }) => self.pending_drops = 0,
            Ok(PublishOutcome::Dropped) => {
                self.dropped = self.dropped.saturating_add(1);
                self.pending_drops = self.pending_drops.saturating_add(1);
            }
            // A failed native operation may leave backend work unresolved.
            // Do not try that storage again based merely on a later frame.
            Err(_) => {}
        }
        result
    }

    pub fn register_release_timeline(
        &mut self,
        incarnation: IncarnationId,
        timeline: Arc<dyn ReleaseTimeline>,
    ) -> Result<ReleaseTimelineRegistration, ArenaError> {
        self.arena.register_release_timeline(incarnation, timeline)
    }

    pub fn poll_cleanup(&mut self) -> Result<usize, ArenaError> {
        self.arena.poll_cleanup()
    }

    pub fn cleanup_failures(&self) -> Vec<crate::acquisition::arena::CleanupFailure> {
        self.arena.cleanup_failures()
    }

    pub fn retry_cleanup(&mut self, incarnation: IncarnationId) -> Result<(), ArenaError> {
        self.arena.retry_cleanup(incarnation)
    }

    pub fn close(&mut self, incarnation: IncarnationId) -> Result<(), ArenaError> {
        self.arena.close(incarnation)
    }
}
