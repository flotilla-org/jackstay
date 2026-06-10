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

use std::os::fd::OwnedFd;

use crate::{
    control_page::{PendingVideoRingEntry, VideoTrackControlPage},
    error::Result,
    model::{ClockDomain, ColorSpace, DamageKind, FrameSyncKind, PayloadKind, PixelFormat},
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

/// Platform seam for the native handle path.
///
/// Implementations own the process-local surface and fence objects; everything
/// that crosses a process boundary is either a plain id carried in ring slots
/// (`pool_id`, `slot_id`, `fence_value`) or an opaque blob carried by the
/// setup channel (the serialized sync handle, surface pool registration).
pub trait NativeFrameBackend {
    /// A frame as captured from the platform source (e.g. an SCK sample).
    type CapturedFrame;
    /// A pool of fixed-size surfaces (e.g. IOSurfaces) frames are staged into.
    type SurfacePool;
    /// The stream's timeline fence (e.g. MTLSharedEvent).
    type Fence;

    /// The payload kind frames staged by this backend publish as.
    fn payload_kind(&self) -> PayloadKind;

    /// Allocate a pool of `slot_count` surfaces matching `params`. Pool ids
    /// are unique forever, so replacement needs no generation.
    fn allocate_surface_pool(&mut self, params: &NativeStreamParams, slot_count: u32) -> Result<Self::SurfacePool>;

    /// The transferable id consumers use to attach this pool.
    fn pool_id(&self, pool: &Self::SurfacePool) -> u64;

    /// Bind a captured frame to a pool surface and return the slot id that
    /// now holds it. For wrap-free producers this is where surface contents
    /// become the frame (zero-copy backends may adopt the captured surface
    /// directly).
    fn stage_frame(&mut self, pool: &mut Self::SurfacePool, frame: &Self::CapturedFrame) -> Result<u32>;

    /// Create the stream's timeline fence.
    fn create_fence(&mut self) -> Result<Self::Fence>;

    /// The transferable id consumers use to name this fence; carried once in
    /// the stream config, paired with per-frame `fence_value`s.
    fn fence_id(&self, fence: &Self::Fence) -> u64;

    /// Signal the timeline to `value` once the staged frame's pixels are
    /// complete. Real backends signal from capture/GPU completion; the value
    /// must be monotonic.
    fn signal_fence(&mut self, fence: &mut Self::Fence, value: u64) -> Result<()>;

    /// Serialize the fence handle for setup-channel transfer (on macOS an
    /// NSXPCCoder-encoded MTLSharedEventHandle; opaque bytes at this layer).
    fn serialize_sync_handle(&self, fence: &Self::Fence) -> Result<Vec<u8>>;
}

/// Publishes native frames from a [`NativeFrameBackend`] through a jackstay
/// broadcast ring. This is the thin tracer spine: it owns the control page,
/// one surface pool, and the stream fence; integration with track/session
/// management arrives in later slices.
#[derive(Debug)]
pub struct NativeTrackProducer<B: NativeFrameBackend> {
    backend: B,
    params: NativeStreamParams,
    page: VideoTrackControlPage,
    pool: B::SurfacePool,
    fence: B::Fence,
    sequence: u64,
    fence_value: u64,
}

impl<B: NativeFrameBackend> NativeTrackProducer<B> {
    pub fn new(mut backend: B, params: NativeStreamParams, slot_count: u32) -> Result<Self> {
        let pool = backend.allocate_surface_pool(&params, slot_count)?;
        let fence = backend.create_fence()?;
        // The ring rounds `slot_count` up to the next power of two, so its lap
        // threshold can exceed the surface-pool size (e.g. slot_count = 3 gives
        // a 4-entry ring over 3 surfaces). The seqlock identity is independent
        // of slot indices, so this is correct for staleness detection. But a
        // backend that lets consumers pin a surface by `slot_id` must assume a
        // surface can be recycled after `slot_count` frames, not after a full
        // ring lap. Real backends (#84) should either size the pool to the
        // rounded-up ring capacity or honor the tighter recycle bound.
        let page = VideoTrackControlPage::new(slot_count as usize);
        Ok(Self {
            backend,
            params,
            page,
            pool,
            fence,
            sequence: 0,
            fence_value: 0,
        })
    }

    /// Stage `frame`, signal the fence, and publish the descriptor. Returns
    /// the publish cursor.
    pub fn publish(&mut self, frame: &B::CapturedFrame, timestamp_ns: u64) -> Result<u64> {
        let slot_id = self.backend.stage_frame(&mut self.pool, frame)?;
        // Saturating + debug_assert, matching the producer-cursor overflow guard
        // in VideoTrackControlPage::push. u64 will not overflow at any real frame
        // rate; the guard keeps the two hot paths consistent.
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
            dropped_before_publish: 0,
            producer_drop_count: 0,
            payload_kind: self.backend.payload_kind() as u32,
            modifier: self.params.modifier,
            fence_id: self.backend.fence_id(&self.fence),
            fence_value: self.fence_value,
            flags: 0,
        });
        Ok(cursor)
    }

    /// The control page fd a consumer maps to read descriptors.
    pub fn control_page_fd(&self) -> Result<OwnedFd> {
        self.page.try_clone_fd()
    }

    #[must_use]
    pub fn control_page_len(&self) -> usize {
        self.page.mapped_len()
    }

    /// The setup-channel blob a consumer deserializes into its fence handle.
    pub fn serialized_sync_handle(&self) -> Result<Vec<u8>> {
        self.backend.serialize_sync_handle(&self.fence)
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
/// introduction.
pub mod fake {
    use std::{
        collections::BTreeMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
    };

    use super::{NativeFrameBackend, NativeStreamParams};
    use crate::{
        error::{CaptureTransferError, Result},
        model::PayloadKind,
    };

    #[derive(Debug, Default)]
    struct RegistryInner {
        next_pool_id: u64,
        next_fence_id: u64,
        pools: BTreeMap<u64, Arc<Mutex<Vec<Vec<u8>>>>>,
        fences: BTreeMap<u64, Arc<AtomicU64>>,
    }

    /// The shared "OS" namespace surfaces and fences live in.
    #[derive(Debug, Clone, Default)]
    pub struct FakeSurfaceRegistry {
        inner: Arc<Mutex<RegistryInner>>,
    }

    impl FakeSurfaceRegistry {
        /// Consumer-side surface resolution (stands in for pool attach +
        /// surface lookup).
        #[must_use]
        pub fn surface_bytes(&self, pool_id: u64, slot_id: u32) -> Option<Vec<u8>> {
            let inner = self.inner.lock().expect("fake surface registry poisoned");
            let pool = inner.pools.get(&pool_id)?;
            let slots = pool.lock().expect("fake surface pool poisoned");
            slots.get(slot_id as usize).cloned()
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
        slots: Arc<Mutex<Vec<Vec<u8>>>>,
        slot_count: u32,
        next_slot: u32,
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
            let slots = Arc::new(Mutex::new(vec![Vec::new(); slot_count as usize]));
            inner.pools.insert(pool_id, Arc::clone(&slots));
            Ok(FakeSurfacePool {
                pool_id,
                slots,
                slot_count,
                next_slot: 0,
            })
        }

        fn pool_id(&self, pool: &FakeSurfacePool) -> u64 {
            pool.pool_id
        }

        fn stage_frame(&mut self, pool: &mut FakeSurfacePool, frame: &FakeCapturedFrame) -> Result<u32> {
            let slot_id = pool.next_slot;
            pool.next_slot = (pool.next_slot + 1) % pool.slot_count;
            let mut slots = pool.slots.lock().expect("fake surface pool poisoned");
            slots[slot_id as usize] = frame.bytes.clone();
            Ok(slot_id)
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

        fn serialize_sync_handle(&self, fence: &FakeFence) -> Result<Vec<u8>> {
            Ok(fence.fence_id.to_le_bytes().to_vec())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        NativeStreamParams, NativeTrackProducer,
        fake::{FakeCapturedFrame, FakeNativeBackend, FakeSurfaceRegistry},
    };
    use crate::{
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

    fn producer(registry: &FakeSurfaceRegistry, slot_count: u32) -> NativeTrackProducer<FakeNativeBackend> {
        NativeTrackProducer::new(FakeNativeBackend::new(registry.clone()), params(), slot_count).unwrap()
    }

    /// The consumer half of the tracer: map the ring read-only over an fd the
    /// way a remote process would after a setup-channel introduction.
    fn map_consumer_page(producer: &NativeTrackProducer<FakeNativeBackend>) -> VideoTrackControlPage {
        VideoTrackControlPage::map_read_only(producer.control_page_fd().unwrap(), producer.control_page_len()).unwrap()
    }

    #[test]
    fn fake_backend_native_frame_round_trips_producer_ring_consumer() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry, 4);

        let cursor = producer.publish(&FakeCapturedFrame { bytes: vec![9, 8, 7, 6] }, 1_000).unwrap();

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

        // Sync: the setup-channel blob names the same fence the ring does,
        // and the timeline has reached the frame's value.
        let handle = producer.serialized_sync_handle().unwrap();
        let fence_id = u64::from_le_bytes(handle.as_slice().try_into().unwrap());
        assert_eq!(fence_id, entry.fence_id);
        assert!(registry.fence_value(entry.fence_id).unwrap() >= entry.fence_value);

        // Payload: resolve (pool_id, slot_id) through the shared namespace.
        let bytes = registry.surface_bytes(entry.pool_id, entry.slot_id).unwrap();
        assert_eq!(bytes, vec![9, 8, 7, 6]);
    }

    #[test]
    fn native_frames_reuse_pool_slots_with_monotonic_fence_values() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry, 2);

        for (index, payload) in [vec![1], vec![2], vec![3]].into_iter().enumerate() {
            let cursor = producer.publish(&FakeCapturedFrame { bytes: payload }, index as u64).unwrap();
            assert_eq!(cursor, index as u64 + 1);
        }

        let page = map_consumer_page(&producer);
        let second = page.read_entry_for_cursor(2).unwrap();
        let third = page.read_entry_for_cursor(3).unwrap();
        // Slot ids wrap around the pool; fence values and cursors do not.
        assert_eq!(second.slot_id, 1);
        assert_eq!(third.slot_id, 0);
        assert_eq!(second.fence_value, 2);
        assert_eq!(third.fence_value, 3);
        assert_eq!(second.config_generation, third.config_generation);
        assert_eq!(registry.surface_bytes(third.pool_id, third.slot_id).unwrap(), vec![3]);
    }

    #[test]
    fn lapped_native_consumer_detects_lap_and_resyncs_to_latest() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry, 2);

        let first_cursor = producer.publish(&FakeCapturedFrame { bytes: vec![1] }, 1).unwrap();
        producer.publish(&FakeCapturedFrame { bytes: vec![2] }, 2).unwrap();
        producer.publish(&FakeCapturedFrame { bytes: vec![3] }, 3).unwrap();

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
        let mut producer = producer(&registry, 2);
        let cursor = producer.publish(&FakeCapturedFrame { bytes: vec![1] }, 1).unwrap();

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
