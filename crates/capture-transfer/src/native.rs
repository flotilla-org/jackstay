//! The native handle path (ADR-0004): frames are OS-native surfaces published
//! by reference, never by copying pixels.
//!
//! A native frame in the broadcast ring is just a descriptor: `(pool_id,
//! slot_id)` names a surface in a pool that was introduced over the setup
//! channel, and `fence_value` is the point on the stream's timeline fence the
//! consumer must wait for before sampling. Handles and fds never enter ring
//! slots. [`NativeFrameBackend`] is the platform seam behind that contract;
//! the macOS IOSurface/MTLSharedEvent implementation arrives in a later slice
//! (#84), and [`fake`] provides an OS-free implementation for tests.
//!
//! Surface reuse is *prevented*, not detected: the neutral core excludes
//! slots still named by live ring entries, then asks the backend/reuse policy
//! to claim one of the remaining candidates. macOS can answer with IOSurface
//! in-use state; Linux answers with explicit lease/release state and native
//! release synchronization.

pub mod attach;
pub mod lease;
#[cfg(all(target_os = "linux", any(feature = "backend-linux", test)))]
pub mod linux;
#[cfg(all(target_os = "macos", feature = "backend-macos"))]
pub mod macos;

use std::{collections::VecDeque, os::fd::OwnedFd};

use crate::{
    control_page::{PendingVideoRingEntry, VideoTrackControlPage},
    error::{CaptureTransferError, Result},
    model::{ClockDomain, ColorSpace, DamageKind, FrameSyncKind, PayloadKind, PixelFormat},
    native::attach::{AttachGrant, AttachPool},
};

/// Stream-level parameters for a native track. These land in the ring's
/// config generation; a change (resize, format switch) means a new pool and a
/// new generation, exactly like swapchain recreation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeStreamParams {
    pub width: u32,
    pub height: u32,
    pub pixel_format: PixelFormat,
    pub color_space: ColorSpace,
    pub clock_domain: ClockDomain,
    /// Format modifier (e.g. dmabuf modifier); 0 / linear for IOSurface.
    pub modifier: u64,
}

/// What the producer does when no surface is eligible for staging: every
/// slot is either held by a consumer or still named by a live ring entry.
/// Never a silent overwrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PoolExhaustionPolicy {
    /// Drop the frame and account for it: the producer's drop counters
    /// advance and the next published frame carries the gap in
    /// `dropped_before_publish`. The right default for latest-wins capture,
    /// where blocking the producer is the worst outcome.
    #[default]
    DropFrame,
    /// Fail the publish with [`CaptureTransferError::SurfacePoolExhausted`]
    /// without consuming the frame's sequence number; the caller may retry
    /// the same frame (its stall policy).
    Fail,
}

/// The result of a [`NativeTrackProducer::publish`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    Published {
        cursor: u64,
    },
    /// The frame was dropped under [`PoolExhaustionPolicy::DropFrame`]; the
    /// gap is carried by the next published frame's `dropped_before_publish`.
    Dropped,
}

/// A pool slot that has left the ring's live window and may be reusable if
/// the backend's native lifetime source agrees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotReuseCandidate {
    pub slot_id: u32,
    pub last_cursor: u64,
}

/// Backend answer to a reusable-slot claim attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotClaim {
    Ready { slot_id: u32 },
    WouldBlock,
}

impl PublishOutcome {
    #[must_use]
    pub const fn cursor(self) -> Option<u64> {
        match self {
            Self::Published { cursor } => Some(cursor),
            Self::Dropped => None,
        }
    }
}

/// Platform seam for the native handle path.
///
/// Implementations own the process-local surface and fence objects; everything
/// that crosses a process boundary is either a plain id carried in ring slots
/// (`pool_id`, `slot_id`, `fence_value`) or a transferable handle carried by
/// the setup channel. Handles are typed, not byte blobs: real platform
/// handles refuse byte serialization (an IOSurface or `MTLSharedEventHandle`
/// crosses only a live XPC connection as an object; a dmabuf is an fd that
/// crosses only via SCM_RIGHTS), so only the platform transport knows how to
/// move them — the protocol just says *what* moves and *when*.
///
/// The neutral core filters by ring liveness. Backend-specific reuse policy
/// then chooses from those candidates, because the native source of truth is
/// platform-specific: IOSurface in-use state on macOS, explicit release
/// timelines on Linux.
pub trait NativeFrameBackend {
    /// A frame as captured from the platform source (e.g. an SCK sample).
    type CapturedFrame;
    /// A pool of fixed-size surfaces (e.g. IOSurfaces) frames are staged into.
    type SurfacePool;
    /// The stream's timeline fence (e.g. MTLSharedEvent).
    type Fence;
    /// One pool surface as the setup-channel transport transfers it (a
    /// retained IOSurface on macOS, a dmabuf fd on Linux, bytes in the fake).
    type SurfaceHandle;
    /// The fence as the setup-channel transport transfers it (an
    /// `MTLSharedEventHandle` on macOS, a syncobj fd on Linux, bytes in the
    /// fake).
    type SyncHandle;

    /// The payload kind frames staged by this backend publish as.
    fn payload_kind(&self) -> PayloadKind;

    /// Allocate a pool of `slot_count` surfaces matching `params`. Pool ids
    /// are unique forever, so replacement needs no generation.
    fn allocate_surface_pool(&mut self, params: &NativeStreamParams, slot_count: u32) -> Result<Self::SurfacePool>;

    /// The transferable id consumers use to attach this pool.
    fn pool_id(&self, pool: &Self::SurfacePool) -> u64;

    /// Claim one reusable staging slot from the ring-eligible candidates.
    /// Returning [`SlotClaim::WouldBlock`] means no candidate is currently
    /// safe to reuse under the backend's native lifetime rules.
    fn claim_reusable_slot(&mut self, pool: &mut Self::SurfacePool, candidates: &[SlotReuseCandidate]) -> Result<SlotClaim>;

    /// Bind a captured frame to the given pool surface. The slot was chosen
    /// by the caller and is guaranteed unheld and outside the ring's live
    /// window. Zero-copy backends may adopt the captured surface directly.
    fn stage_frame(&mut self, pool: &mut Self::SurfacePool, slot_id: u32, frame: &Self::CapturedFrame) -> Result<()>;

    /// For negotiated native pools where the capture API delivers a concrete
    /// pool buffer, return that buffer's slot. Backends that copy or stage
    /// into producer-owned surfaces leave this as `None`.
    fn frame_slot_hint(&self, _frame: &Self::CapturedFrame) -> Option<u32> {
        None
    }

    /// Export each pool surface for setup-channel transfer, one entry per
    /// slot, indexed by `slot_id`.
    fn export_surface_handles(&self, pool: &Self::SurfacePool) -> Result<Vec<Self::SurfaceHandle>>;

    /// Create the stream's timeline fence.
    fn create_fence(&mut self) -> Result<Self::Fence>;

    /// The transferable id consumers use to name this fence; carried once in
    /// the stream config, paired with per-frame `fence_value`s.
    fn fence_id(&self, fence: &Self::Fence) -> u64;

    /// Signal the timeline to `value` once the staged frame's pixels are
    /// complete. Real backends signal from capture/GPU completion; the value
    /// must be monotonic.
    fn signal_fence(&mut self, fence: &mut Self::Fence, value: u64) -> Result<()>;

    /// Export the fence for setup-channel transfer.
    fn export_sync_handle(&self, fence: &Self::Fence) -> Result<Self::SyncHandle>;
}

/// Publishes native frames from a [`NativeFrameBackend`] through a jackstay
/// broadcast ring, gating surface reuse on the backend's native reuse policy
/// and the ring's live window. Owns the control page, one surface pool, and the stream
/// fence; integration with track/session management arrives in later slices.
#[derive(Debug)]
pub struct NativeTrackProducer<B: NativeFrameBackend> {
    backend: B,
    params: NativeStreamParams,
    page: VideoTrackControlPage,
    pool: B::SurfacePool,
    retired_pools: VecDeque<B::SurfacePool>,
    pool_slot_count: u32,
    fence: B::Fence,
    exhaustion_policy: PoolExhaustionPolicy,
    /// Cursor each pool slot was last published at (0 = never published;
    /// real cursors start at 1, so 0 is unambiguous); a slot is reachable
    /// while its cursor is inside the ring's live window.
    slot_cursors: Vec<u64>,
    next_slot_hint: u32,
    last_cursor: u64,
    sequence: u64,
    fence_value: u64,
    pending_dropped: u32,
    producer_drop_count: u64,
}

impl<B: NativeFrameBackend> NativeTrackProducer<B> {
    /// `ring_capacity` is rounded up to a power of two by the control page;
    /// `pool_slot_count` must exceed the *rounded* capacity so at least one
    /// surface is always outside the ring's live window — otherwise live
    /// entries alone could pin every surface and stall the stream with no
    /// consumer involvement.
    pub fn new(
        mut backend: B,
        params: NativeStreamParams,
        ring_capacity: usize,
        pool_slot_count: u32,
        exhaustion_policy: PoolExhaustionPolicy,
    ) -> Result<Self> {
        let pool = backend.allocate_surface_pool(&params, pool_slot_count)?;
        let fence = backend.create_fence()?;
        Self::from_allocated_parts(backend, params, ring_capacity, pool_slot_count, pool, fence, exhaustion_policy)
    }

    /// Build a producer around a native pool and fence that were discovered or
    /// allocated before the generic producer exists. This is the shape needed
    /// by negotiated backends such as PipeWire, where stream buffers arrive
    /// from the compositor rather than from `allocate_surface_pool`.
    pub fn from_allocated_parts(
        backend: B,
        params: NativeStreamParams,
        ring_capacity: usize,
        pool_slot_count: u32,
        pool: B::SurfacePool,
        fence: B::Fence,
        exhaustion_policy: PoolExhaustionPolicy,
    ) -> Result<Self> {
        let page = VideoTrackControlPage::new(ring_capacity);
        let rounded_capacity = page.layout().slot_capacity;
        if pool_slot_count as usize <= rounded_capacity {
            return Err(CaptureTransferError::NativeBackend {
                operation: "create-native-producer",
                message: format!("surface pool ({pool_slot_count} slots) must exceed the rounded ring capacity ({rounded_capacity})"),
            });
        }
        Ok(Self {
            backend,
            params,
            page,
            pool,
            retired_pools: VecDeque::new(),
            pool_slot_count,
            fence,
            exhaustion_policy,
            slot_cursors: vec![0; pool_slot_count as usize],
            next_slot_hint: 0,
            last_cursor: 0,
            sequence: 0,
            fence_value: 0,
            pending_dropped: 0,
            producer_drop_count: 0,
        })
    }

    /// Replace the active native pool for a stream reconfiguration while
    /// preserving the ring, sequence, cursor, drop accounting, and fence.
    ///
    /// Existing consumers still need a transport-level `POOL_ADDED` grant for
    /// the new pool before they can sample frames from it. This method is the
    /// producer-side swap primitive that makes those events truthful.
    pub fn reconfigure(&mut self, params: NativeStreamParams, pool_slot_count: u32) -> Result<()> {
        let pool = self.backend.allocate_surface_pool(&params, pool_slot_count)?;
        self.replace_pool(params, pool_slot_count, pool)
    }

    /// Replace the active pool with one already allocated by the capture API.
    /// Negotiated backends such as PipeWire use this when the compositor, not
    /// porthole, owns the buffer allocation.
    pub fn replace_allocated_pool(&mut self, params: NativeStreamParams, pool_slot_count: u32, pool: B::SurfacePool) -> Result<()> {
        self.replace_pool(params, pool_slot_count, pool)
    }

    fn replace_pool(&mut self, params: NativeStreamParams, pool_slot_count: u32, pool: B::SurfacePool) -> Result<()> {
        let rounded_capacity = self.page.layout().slot_capacity;
        if pool_slot_count as usize <= rounded_capacity {
            return Err(CaptureTransferError::NativeBackend {
                operation: "reconfigure-native-producer",
                message: format!("surface pool ({pool_slot_count} slots) must exceed the rounded ring capacity ({rounded_capacity})"),
            });
        }
        let retired = std::mem::replace(&mut self.pool, pool);
        self.retired_pools.push_back(retired);
        while self.retired_pools.len() > 2 {
            self.retired_pools.pop_front();
        }
        self.params = params;
        self.pool_slot_count = pool_slot_count;
        self.slot_cursors = vec![0; pool_slot_count as usize];
        self.next_slot_hint = 0;
        Ok(())
    }

    /// Stage `frame` into an eligible surface, signal the fence, and publish
    /// the descriptor. When the pool is exhausted the configured
    /// [`PoolExhaustionPolicy`] applies.
    pub fn publish(&mut self, frame: &B::CapturedFrame, timestamp_ns: u64) -> Result<PublishOutcome> {
        let Some(slot_id) = self.claim_slot(frame)? else {
            return match self.exhaustion_policy {
                PoolExhaustionPolicy::DropFrame => {
                    // The captured frame existed: its sequence number is
                    // consumed so the gap stays visible to consumers.
                    self.sequence = self.sequence.saturating_add(1);
                    debug_assert!(self.sequence < u64::MAX);
                    self.pending_dropped = self.pending_dropped.saturating_add(1);
                    self.producer_drop_count = self.producer_drop_count.saturating_add(1);
                    Ok(PublishOutcome::Dropped)
                }
                // Nothing is consumed: the caller may retry the same frame.
                PoolExhaustionPolicy::Fail => Err(CaptureTransferError::SurfacePoolExhausted {
                    slot_count: self.pool_slot_count,
                }),
            };
        };

        self.backend.stage_frame(&mut self.pool, slot_id, frame)?;
        // Saturating + debug_assert, matching the producer-cursor overflow
        // guard in VideoTrackControlPage::push. u64 will not overflow at any
        // real frame rate; the guard keeps the hot paths consistent.
        self.sequence = self.sequence.saturating_add(1);
        debug_assert!(self.sequence < u64::MAX);
        self.fence_value = self.fence_value.saturating_add(1);
        debug_assert!(self.fence_value < u64::MAX);
        // The tracer signals inline before publishing. A real backend signals
        // from GPU/capture completion; the descriptor may then be observed
        // before the fence reaches its value, which is exactly what the
        // consumer-side wait is for.
        self.backend.signal_fence(&mut self.fence, self.fence_value)?;
        let cursor = self.page.push(PendingVideoRingEntry {
            sequence: self.sequence,
            timestamp_ns,
            width: self.params.width,
            height: self.params.height,
            // Native payloads have no in-band bytes; the surface defines its
            // own row layout.
            stride: 0,
            pixel_format: self.params.pixel_format as u32,
            pool_id: self.backend.pool_id(&self.pool),
            slot_id,
            payload_offset: 0,
            payload_len: 0,
            clock_domain: self.params.clock_domain as u32,
            color_space: self.params.color_space as u32,
            sync_kind: FrameSyncKind::NativeTimeline as u32,
            damage_kind: DamageKind::FullFrame as u32,
            damage_base_sequence: self.sequence,
            dropped_before_publish: self.pending_dropped,
            producer_drop_count: self.producer_drop_count,
            payload_kind: self.backend.payload_kind() as u32,
            modifier: self.params.modifier,
            fence_id: self.backend.fence_id(&self.fence),
            fence_value: self.fence_value,
            flags: 0,
        });
        self.pending_dropped = 0;
        self.last_cursor = cursor;
        self.slot_cursors[slot_id as usize] = cursor;
        self.next_slot_hint = (slot_id + 1) % self.pool_slot_count;
        Ok(PublishOutcome::Published { cursor })
    }

    /// Pick the next eligible surface. The core filters out slots still
    /// named by live ring entries, then delegates native lifetime checks to
    /// the backend/reuse policy. Returns `None` when the pool is exhausted.
    fn claim_slot(&mut self, frame: &B::CapturedFrame) -> Result<Option<u32>> {
        let slot_hint = self.backend.frame_slot_hint(frame);
        let ring_capacity = self.page.layout().slot_capacity as u64;
        let live_len = self.last_cursor.min(ring_capacity);
        let oldest_live_cursor = self.last_cursor - live_len + 1;
        let mut candidates = Vec::with_capacity(self.pool_slot_count as usize);
        for offset in 0..self.pool_slot_count {
            let slot_id = (self.next_slot_hint + offset) % self.pool_slot_count;
            if slot_hint.is_some_and(|hint| hint != slot_id) {
                continue;
            }
            let published_at = self.slot_cursors[slot_id as usize];
            let reachable = published_at != 0 && published_at >= oldest_live_cursor;
            if reachable {
                continue;
            }
            candidates.push(SlotReuseCandidate {
                slot_id,
                last_cursor: published_at,
            });
        }
        match self.backend.claim_reusable_slot(&mut self.pool, &candidates)? {
            SlotClaim::Ready { slot_id } => Ok(Some(slot_id)),
            SlotClaim::WouldBlock => Ok(None),
        }
    }

    /// Register `consumer_id` and assemble everything a consumer needs,
    /// transferred exactly once at attach: the ring mapping, the pool's
    /// surface handles, and the serialized sync handle. Steady state after
    /// this is shared-memory only.
    pub fn grant_attach(&mut self, consumer_id: u64) -> Result<AttachGrant<B::SurfaceHandle, B::SyncHandle>> {
        let consumer_slot = self.page.register_consumer_cursor(consumer_id)? as u64;
        Ok(AttachGrant {
            consumer_id,
            consumer_slot,
            ring_fd: self.page.try_clone_fd()?,
            ring_map_len: self.page.mapped_len() as u64,
            pool_id: self.backend.pool_id(&self.pool),
            pool_slot_count: self.pool_slot_count,
            surface_handles: self.backend.export_surface_handles(&self.pool)?,
            fence_id: self.backend.fence_id(&self.fence),
            sync_handle: self.backend.export_sync_handle(&self.fence)?,
        })
    }

    /// Export the currently active pool without registering a new consumer or
    /// re-sending the ring/fence grant. Existing attaches use this after a
    /// reconfiguration event.
    pub fn export_current_pool(&self) -> Result<AttachPool<B::SurfaceHandle>> {
        self.export_pool(self.backend.pool_id(&self.pool))
    }

    /// Export the active or recently retired pool named by a ring entry.
    pub fn export_pool(&self, pool_id: u64) -> Result<AttachPool<B::SurfaceHandle>> {
        let pool = if self.backend.pool_id(&self.pool) == pool_id {
            Some(&self.pool)
        } else {
            self.retired_pools.iter().find(|pool| self.backend.pool_id(pool) == pool_id)
        }
        .ok_or_else(|| CaptureTransferError::NativeBackend {
            operation: "export-native-pool",
            message: format!("pool id {pool_id} is not active or recently retired"),
        })?;
        let surface_handles = self.backend.export_surface_handles(pool)?;
        Ok(AttachPool {
            pool_id,
            pool_slot_count: surface_handles.len() as u32,
            surface_handles,
        })
    }

    #[must_use]
    pub fn params(&self) -> &NativeStreamParams {
        &self.params
    }

    /// The control page fd a consumer maps to read descriptors.
    pub fn control_page_fd(&self) -> Result<OwnedFd> {
        self.page.try_clone_fd()
    }

    #[must_use]
    pub fn control_page_len(&self) -> usize {
        self.page.mapped_len()
    }

    /// The fence handle a consumer resolves on its side of the setup channel.
    pub fn export_sync_handle(&self) -> Result<B::SyncHandle> {
        self.backend.export_sync_handle(&self.fence)
    }

    #[cfg(all(target_os = "linux", any(feature = "backend-linux", test)))]
    pub(crate) fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    #[must_use]
    pub fn control_page(&self) -> &VideoTrackControlPage {
        &self.page
    }
}

/// An OS-free [`NativeFrameBackend`] for tests. [`fake::FakeSurfaceRegistry`]
/// stands in for the kernel's surface and fence namespaces: the producer-side
/// backend registers objects there, and a "remote" consumer resolves the ids
/// it read from ring slots against the same registry, the way a real consumer
/// resolves an IOSurface or waits a shared event after a setup-channel
/// introduction. Consumer-side surface holds (`IOSurfaceIncrementUseCount`)
/// are modelled by [`fake::FakeSurfaceRegistry::hold_surface`].
pub mod fake {
    use std::{
        collections::BTreeMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
    };

    use super::{NativeFrameBackend, NativeStreamParams, SlotClaim, SlotReuseCandidate};
    use crate::{
        error::{CaptureTransferError, Result},
        model::PayloadKind,
    };

    #[derive(Debug, Default)]
    struct FakePool {
        slots: Vec<Vec<u8>>,
        use_counts: Vec<u32>,
    }

    #[derive(Debug, Default)]
    struct RegistryInner {
        next_pool_id: u64,
        next_fence_id: u64,
        pools: BTreeMap<u64, Arc<Mutex<FakePool>>>,
        fences: BTreeMap<u64, Arc<AtomicU64>>,
    }

    /// The shared "OS" namespace surfaces and fences live in.
    #[derive(Debug, Clone, Default)]
    pub struct FakeSurfaceRegistry {
        inner: Arc<Mutex<RegistryInner>>,
    }

    impl FakeSurfaceRegistry {
        fn pool(&self, pool_id: u64) -> Option<Arc<Mutex<FakePool>>> {
            let inner = self.inner.lock().expect("fake surface registry poisoned");
            inner.pools.get(&pool_id).cloned()
        }

        /// Consumer-side surface resolution (stands in for pool attach +
        /// surface lookup).
        #[must_use]
        pub fn surface_bytes(&self, pool_id: u64, slot_id: u32) -> Option<Vec<u8>> {
            let pool = self.pool(pool_id)?;
            let pool = pool.lock().expect("fake surface pool poisoned");
            pool.slots.get(slot_id as usize).cloned()
        }

        /// Consumer-side hold (stands in for `IOSurfaceIncrementUseCount`).
        pub fn hold_surface(&self, pool_id: u64, slot_id: u32) {
            let pool = self.pool(pool_id).expect("hold_surface on unknown pool");
            let mut pool = pool.lock().expect("fake surface pool poisoned");
            pool.use_counts[slot_id as usize] += 1;
        }

        /// Consumer-side release (stands in for `IOSurfaceDecrementUseCount`).
        pub fn release_surface(&self, pool_id: u64, slot_id: u32) {
            let pool = self.pool(pool_id).expect("release_surface on unknown pool");
            let mut pool = pool.lock().expect("fake surface pool poisoned");
            let count = &mut pool.use_counts[slot_id as usize];
            *count = count.checked_sub(1).expect("release_surface without matching hold");
        }

        /// Consumer-side fence wait peek: the timeline's current value.
        #[must_use]
        pub fn fence_value(&self, fence_id: u64) -> Option<u64> {
            let inner = self.inner.lock().expect("fake surface registry poisoned");
            inner.fences.get(&fence_id).map(|value| value.load(Ordering::Acquire))
        }
    }

    #[derive(Debug)]
    pub struct FakeNativeBackend {
        registry: FakeSurfaceRegistry,
    }

    impl FakeNativeBackend {
        #[must_use]
        pub fn new(registry: FakeSurfaceRegistry) -> Self {
            Self { registry }
        }
    }

    /// A captured frame is just bytes here; a real backend's captured frame
    /// is a platform sample owning a surface.
    #[derive(Debug, Clone)]
    pub struct FakeCapturedFrame {
        pub bytes: Vec<u8>,
    }

    #[derive(Debug)]
    pub struct FakeSurfacePool {
        pool_id: u64,
        inner: Arc<Mutex<FakePool>>,
    }

    #[derive(Debug)]
    pub struct FakeFence {
        fence_id: u64,
        value: Arc<AtomicU64>,
    }

    impl NativeFrameBackend for FakeNativeBackend {
        type CapturedFrame = FakeCapturedFrame;
        type SurfacePool = FakeSurfacePool;
        type Fence = FakeFence;
        // The fake's handles really are bytes: its "OS namespace" is the
        // shared registry, so a handle only needs to name (pool, slot) / a
        // fence id within it.
        type SurfaceHandle = Vec<u8>;
        type SyncHandle = Vec<u8>;

        fn payload_kind(&self) -> PayloadKind {
            // The fake mimics the macOS backend: surface ids resolve through
            // a shared namespace the way IOSurfaces do.
            PayloadKind::IoSurface
        }

        fn allocate_surface_pool(&mut self, _params: &NativeStreamParams, slot_count: u32) -> Result<FakeSurfacePool> {
            if slot_count == 0 {
                return Err(CaptureTransferError::NativeBackend {
                    operation: "allocate-surface-pool",
                    message: "slot count must be non-zero".to_string(),
                });
            }
            let mut inner = self.registry.inner.lock().expect("fake surface registry poisoned");
            inner.next_pool_id += 1;
            let pool_id = inner.next_pool_id;
            let pool = Arc::new(Mutex::new(FakePool {
                slots: vec![Vec::new(); slot_count as usize],
                use_counts: vec![0; slot_count as usize],
            }));
            inner.pools.insert(pool_id, Arc::clone(&pool));
            Ok(FakeSurfacePool { pool_id, inner: pool })
        }

        fn pool_id(&self, pool: &FakeSurfacePool) -> u64 {
            pool.pool_id
        }

        fn claim_reusable_slot(&mut self, pool: &mut FakeSurfacePool, candidates: &[SlotReuseCandidate]) -> Result<SlotClaim> {
            let pool = pool.inner.lock().expect("fake surface pool poisoned");
            Ok(candidates
                .iter()
                .find(|candidate| pool.use_counts[candidate.slot_id as usize] == 0)
                .map(|candidate| SlotClaim::Ready {
                    slot_id: candidate.slot_id,
                })
                .unwrap_or(SlotClaim::WouldBlock))
        }

        fn stage_frame(&mut self, pool: &mut FakeSurfacePool, slot_id: u32, frame: &FakeCapturedFrame) -> Result<()> {
            let mut pool = pool.inner.lock().expect("fake surface pool poisoned");
            pool.slots[slot_id as usize] = frame.bytes.clone();
            Ok(())
        }

        fn export_surface_handles(&self, pool: &FakeSurfacePool) -> Result<Vec<Vec<u8>>> {
            let inner = pool.inner.lock().expect("fake surface pool poisoned");
            Ok((0..inner.slots.len() as u32)
                .map(|slot_id| {
                    let mut handle = pool.pool_id.to_le_bytes().to_vec();
                    handle.extend_from_slice(&slot_id.to_le_bytes());
                    handle
                })
                .collect())
        }

        fn create_fence(&mut self) -> Result<FakeFence> {
            let mut inner = self.registry.inner.lock().expect("fake surface registry poisoned");
            inner.next_fence_id += 1;
            let fence_id = inner.next_fence_id;
            let value = Arc::new(AtomicU64::new(0));
            inner.fences.insert(fence_id, Arc::clone(&value));
            Ok(FakeFence { fence_id, value })
        }

        fn fence_id(&self, fence: &FakeFence) -> u64 {
            fence.fence_id
        }

        fn signal_fence(&mut self, fence: &mut FakeFence, value: u64) -> Result<()> {
            // Release: this publishes the new timeline value to consumers that
            // load it with Acquire. The swapped-out value is only used for the
            // monotonicity check below; nothing here depends on acquiring prior
            // writes, so Release is the honest ordering.
            let previous = fence.value.swap(value, Ordering::Release);
            if previous >= value {
                return Err(CaptureTransferError::NativeBackend {
                    operation: "signal-fence",
                    message: format!("timeline values must be monotonic: {previous} -> {value}"),
                });
            }
            Ok(())
        }

        fn export_sync_handle(&self, fence: &FakeFence) -> Result<Vec<u8>> {
            Ok(fence.fence_id.to_le_bytes().to_vec())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        NativeFrameBackend, NativeStreamParams, NativeTrackProducer, PoolExhaustionPolicy, PublishOutcome,
        fake::{FakeCapturedFrame, FakeNativeBackend, FakeSurfaceRegistry},
    };
    use crate::{
        CaptureTransferError,
        control_page::{VideoRingReadError, VideoTrackControlPage},
        model::{ClockDomain, ColorSpace, FrameSyncKind, PayloadKind, PixelFormat},
    };

    fn params() -> NativeStreamParams {
        NativeStreamParams {
            width: 640,
            height: 480,
            pixel_format: PixelFormat::Bgra8Unorm,
            color_space: ColorSpace::Srgb,
            clock_domain: ClockDomain::HostTime,
            modifier: 0,
        }
    }

    fn producer(
        registry: &FakeSurfaceRegistry,
        ring_capacity: usize,
        pool_slot_count: u32,
        policy: PoolExhaustionPolicy,
    ) -> NativeTrackProducer<FakeNativeBackend> {
        NativeTrackProducer::new(
            FakeNativeBackend::new(registry.clone()),
            params(),
            ring_capacity,
            pool_slot_count,
            policy,
        )
        .unwrap()
    }

    fn frame(bytes: &[u8]) -> FakeCapturedFrame {
        FakeCapturedFrame { bytes: bytes.to_vec() }
    }

    fn publish_cursor(producer: &mut NativeTrackProducer<FakeNativeBackend>, bytes: &[u8], timestamp_ns: u64) -> u64 {
        producer
            .publish(&frame(bytes), timestamp_ns)
            .unwrap()
            .cursor()
            .expect("publish expected to succeed")
    }

    /// The consumer half of the tracer: map the ring read-only over an fd the
    /// way a remote process would after a setup-channel introduction.
    fn map_consumer_page(producer: &NativeTrackProducer<FakeNativeBackend>) -> VideoTrackControlPage {
        VideoTrackControlPage::map_read_only(producer.control_page_fd().unwrap(), producer.control_page_len()).unwrap()
    }

    #[test]
    fn pool_must_exceed_rounded_ring_capacity() {
        let registry = FakeSurfaceRegistry::default();
        // Ring capacity 3 rounds to 4; a 4-slot pool could be fully pinned by
        // live entries alone.
        let error = NativeTrackProducer::new(
            FakeNativeBackend::new(registry.clone()),
            params(),
            3,
            4,
            PoolExhaustionPolicy::default(),
        )
        .unwrap_err();
        assert!(matches!(error, CaptureTransferError::NativeBackend { .. }));
        assert!(error.to_string().contains("must exceed the rounded ring capacity"));
    }

    #[test]
    fn producer_can_start_from_preallocated_native_parts() {
        let registry = FakeSurfaceRegistry::default();
        let mut backend = FakeNativeBackend::new(registry.clone());
        let params = params();
        let pool = backend.allocate_surface_pool(&params, 5).unwrap();
        let fence = backend.create_fence().unwrap();
        let mut producer =
            NativeTrackProducer::from_allocated_parts(backend, params, 4, 5, pool, fence, PoolExhaustionPolicy::default()).unwrap();

        let cursor = publish_cursor(&mut producer, &[7, 8, 9], 1_000);
        let page = map_consumer_page(&producer);
        let entry = page.read_entry_for_cursor(cursor).unwrap();

        assert_eq!(entry.cursor, 1);
        assert_eq!(entry.slot_id, 0);
        assert_eq!(registry.surface_bytes(entry.pool_id, entry.slot_id).unwrap(), vec![7, 8, 9]);
        assert!(registry.fence_value(entry.fence_id).unwrap() >= entry.fence_value);
    }

    #[test]
    fn fake_backend_native_frame_round_trips_producer_ring_consumer() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry, 4, 5, PoolExhaustionPolicy::default());

        let cursor = publish_cursor(&mut producer, &[9, 8, 7, 6], 1_000);

        let page = map_consumer_page(&producer);
        let entry = page.read_entry_for_cursor(cursor).unwrap();

        // Descriptor: native, desc-only, versioned config carries the stream values.
        assert_eq!(entry.cursor, 1);
        assert_eq!(entry.sequence, 1);
        assert_eq!(entry.timestamp_ns, 1_000);
        assert_eq!(entry.payload_kind, PayloadKind::IoSurface as u32);
        assert_eq!(entry.sync_kind, FrameSyncKind::NativeTimeline as u32);
        assert_eq!(entry.payload_offset, 0);
        assert_eq!(entry.payload_len, 0);
        assert_eq!(entry.config_generation, 1);
        assert_eq!((entry.width, entry.height), (640, 480));
        assert_eq!(entry.pixel_format, PixelFormat::Bgra8Unorm as u32);
        assert_eq!(entry.color_space, ColorSpace::Srgb as u32);

        // Sync: the setup-channel handle names the same fence the ring does,
        // and the timeline has reached the frame's value.
        let handle = producer.export_sync_handle().unwrap();
        let fence_id = u64::from_le_bytes(handle.as_slice().try_into().unwrap());
        assert_eq!(fence_id, entry.fence_id);
        assert!(registry.fence_value(entry.fence_id).unwrap() >= entry.fence_value);

        // Payload: resolve (pool_id, slot_id) through the shared namespace.
        let bytes = registry.surface_bytes(entry.pool_id, entry.slot_id).unwrap();
        assert_eq!(bytes, vec![9, 8, 7, 6]);
    }

    #[test]
    fn producer_reconfigure_switches_pool_and_config_without_resetting_stream_counters() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry, 2, 3, PoolExhaustionPolicy::default());

        let first_cursor = publish_cursor(&mut producer, &[1], 1_000);
        let first = producer.control_page().read_entry_for_cursor(first_cursor).unwrap();
        registry.hold_surface(first.pool_id, first.slot_id);

        let mut reconfigured = params();
        reconfigured.width = 800;
        reconfigured.height = 600;
        producer.reconfigure(reconfigured, 3).unwrap();

        let second_cursor = publish_cursor(&mut producer, &[2], 2_000);
        let second = producer.control_page().read_entry_for_cursor(second_cursor).unwrap();

        assert_eq!(second.cursor, first.cursor + 1);
        assert_eq!(second.sequence, first.sequence + 1);
        assert_eq!(second.fence_id, first.fence_id);
        assert_eq!(second.fence_value, first.fence_value + 1);
        assert_ne!(second.pool_id, first.pool_id);
        assert_eq!(second.slot_id, 0);
        assert_eq!(second.config_generation, first.config_generation + 1);
        assert_eq!((second.width, second.height), (800, 600));
        let retired_pool = producer.export_pool(first.pool_id).unwrap();
        assert_eq!(retired_pool.pool_id, first.pool_id);
        assert_eq!(retired_pool.surface_handles.len(), 3);
        assert_eq!(registry.surface_bytes(first.pool_id, first.slot_id).unwrap(), vec![1]);
        assert_eq!(registry.surface_bytes(second.pool_id, second.slot_id).unwrap(), vec![2]);

        registry.release_surface(first.pool_id, first.slot_id);
    }

    #[test]
    fn producer_reconfigure_rejects_pool_that_cannot_escape_the_live_ring() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry, 3, 5, PoolExhaustionPolicy::default());

        let error = producer.reconfigure(params(), 4).unwrap_err();

        assert!(matches!(error, CaptureTransferError::NativeBackend { .. }));
        assert!(error.to_string().contains("must exceed the rounded ring capacity"));
    }

    #[test]
    fn native_frames_rotate_pool_slots_with_monotonic_fence_values() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry, 2, 3, PoolExhaustionPolicy::default());

        for (index, payload) in [vec![1], vec![2], vec![3], vec![4]].into_iter().enumerate() {
            let cursor = publish_cursor(&mut producer, &payload, index as u64);
            assert_eq!(cursor, index as u64 + 1);
        }

        let page = map_consumer_page(&producer);
        let third = page.read_entry_for_cursor(3).unwrap();
        let fourth = page.read_entry_for_cursor(4).unwrap();
        // The pool rotates 0, 1, 2; by cursor 4 only cursors 3 and 4 are live
        // in the 2-entry ring, so slot 0 has left the live window and is
        // reused. Fence values and cursors never wrap.
        assert_eq!(third.slot_id, 2);
        assert_eq!(fourth.slot_id, 0);
        assert_eq!(third.fence_value, 3);
        assert_eq!(fourth.fence_value, 4);
        assert_eq!(third.config_generation, fourth.config_generation);
        assert_eq!(registry.surface_bytes(fourth.pool_id, fourth.slot_id).unwrap(), vec![4]);
    }

    #[test]
    fn held_surface_is_never_restaged_until_released() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry, 2, 4, PoolExhaustionPolicy::default());

        let first = publish_cursor(&mut producer, &[1], 1);
        let page = map_consumer_page(&producer);
        let entry = page.read_entry_for_cursor(first).unwrap();
        assert_eq!(entry.slot_id, 0);
        // The consumer acquires the surface (IOSurfaceIncrementUseCount
        // stand-in).
        registry.hold_surface(entry.pool_id, entry.slot_id);

        // Publish well past both the live window and the pool rotation; the
        // held slot 0 must never be staged into again.
        for sequence in 2..=8 {
            let cursor = publish_cursor(&mut producer, &[sequence as u8], sequence);
            let published = producer.control_page().read_entry_for_cursor(cursor).unwrap();
            assert_ne!(published.slot_id, 0, "held surface restaged at cursor {cursor}");
        }
        assert_eq!(registry.surface_bytes(entry.pool_id, 0).unwrap(), vec![1]);

        // Released, the slot rejoins the rotation.
        registry.release_surface(entry.pool_id, entry.slot_id);
        let mut reused = false;
        for sequence in 9..=12 {
            let cursor = publish_cursor(&mut producer, &[sequence as u8], sequence);
            let published = producer.control_page().read_entry_for_cursor(cursor).unwrap();
            reused |= published.slot_id == 0;
        }
        assert!(reused, "released surface never rejoined the rotation");
    }

    #[test]
    fn exhausted_pool_drops_frames_and_accounts_the_gap() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry, 2, 3, PoolExhaustionPolicy::DropFrame);

        let pool_id = {
            let cursor = publish_cursor(&mut producer, &[1], 1);
            producer.control_page().read_entry_for_cursor(cursor).unwrap().pool_id
        };
        // Consumers hold every surface.
        for slot_id in 0..3 {
            registry.hold_surface(pool_id, slot_id);
        }

        assert_eq!(producer.publish(&frame(&[2]), 2).unwrap(), PublishOutcome::Dropped);
        assert_eq!(producer.publish(&frame(&[3]), 3).unwrap(), PublishOutcome::Dropped);

        // Releases make surfaces eligible again; the next publish carries the
        // gap. The cursor only advances on publishes while the sequence also
        // counts the dropped frames, so the gap stays visible to consumers.
        registry.release_surface(pool_id, 1);
        registry.release_surface(pool_id, 2);
        let cursor = producer.publish(&frame(&[4]), 4).unwrap().cursor().unwrap();
        let entry = producer.control_page().read_entry_for_cursor(cursor).unwrap();
        assert_eq!(cursor, 2);
        assert_eq!(entry.sequence, 4);
        assert_eq!(entry.dropped_before_publish, 2);
        assert_eq!(entry.producer_drop_count, 2);

        // The gap is reported once, not re-reported.
        let cursor = publish_cursor(&mut producer, &[5], 5);
        let entry = producer.control_page().read_entry_for_cursor(cursor).unwrap();
        assert_eq!(entry.dropped_before_publish, 0);
        assert_eq!(entry.producer_drop_count, 2);
    }

    #[test]
    fn exhausted_pool_fails_without_consuming_the_frame_under_fail_policy() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry, 2, 3, PoolExhaustionPolicy::Fail);

        let pool_id = {
            let cursor = publish_cursor(&mut producer, &[1], 1);
            producer.control_page().read_entry_for_cursor(cursor).unwrap().pool_id
        };
        for slot_id in 0..3 {
            registry.hold_surface(pool_id, slot_id);
        }

        let error = producer.publish(&frame(&[2]), 2).unwrap_err();
        assert_eq!(error, CaptureTransferError::SurfacePoolExhausted { slot_count: 3 });

        // Nothing was consumed: after a release the same frame publishes with
        // no gap recorded.
        registry.release_surface(pool_id, 1);
        let cursor = producer.publish(&frame(&[2]), 2).unwrap().cursor().unwrap();
        let entry = producer.control_page().read_entry_for_cursor(cursor).unwrap();
        assert_eq!(entry.sequence, 2);
        assert_eq!(entry.dropped_before_publish, 0);
        assert_eq!(entry.producer_drop_count, 0);
    }

    #[test]
    fn lapped_native_consumer_detects_lap_and_resyncs_to_latest() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry, 2, 3, PoolExhaustionPolicy::default());

        let first_cursor = publish_cursor(&mut producer, &[1], 1);
        publish_cursor(&mut producer, &[2], 2);
        publish_cursor(&mut producer, &[3], 3);

        let page = map_consumer_page(&producer);
        assert_eq!(
            page.read_entry_for_cursor(first_cursor),
            Err(VideoRingReadError::Lapped {
                requested_cursor: 1,
                oldest_live_cursor: 2,
                latest_cursor: 3,
            })
        );

        // Resync to latest and sample the newest surface.
        let latest = page.read_latest_lossy_entry().unwrap().unwrap();
        assert_eq!(latest.cursor, 3);
        assert!(registry.fence_value(latest.fence_id).unwrap() >= latest.fence_value);
        assert_eq!(registry.surface_bytes(latest.pool_id, latest.slot_id).unwrap(), vec![3]);
    }

    #[test]
    fn native_consumer_double_read_rejects_torn_slot() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry, 2, 3, PoolExhaustionPolicy::default());
        let cursor = publish_cursor(&mut producer, &[1], 1);

        // Simulate a producer mid-rewrite of the slot under the reader.
        producer.control_page().set_slot_publication_sequence_for_test(0, 0);

        let page = map_consumer_page(&producer);
        assert_eq!(
            page.read_entry_for_cursor(cursor),
            Err(VideoRingReadError::SlotSequenceMismatch {
                requested_cursor: 1,
                first_sequence: 0,
                second_sequence: 0,
            })
        );
    }
}
