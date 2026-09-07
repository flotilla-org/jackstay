//! The real macOS [`NativeFrameBackend`]: IOSurface pools, Metal blit
//! staging, and `MTLSharedEvent` timeline fences (#84, patterns validated by
//! spike #81).
//!
//! Staging and signalling are one GPU submission: `stage_frame` encodes the
//! blit from the captured surface into the pool slot and `signal_fence`
//! encodes the timeline signal on the *same* command buffer, then commits.
//! The producer's publish ordering — submit GPU work, obtain fence value,
//! publish ring slot — is therefore structural: the descriptor can never name
//! a fence value no submitted work will reach.
//!
//! Handles ([`IoSurface`], [`SharedEventHandle`]) are process-local object
//! references. They cross a process boundary only over a live XPC connection
//! (ADR-0007); the XPC transport encodes them as objects, never as bytes.

pub mod xpc;

use std::{
    ffi::{CStr, c_char, c_void},
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    error::{CaptureTransferError, Result},
    model::{PayloadKind, PixelFormat},
    native::{NativeFrameBackend, NativeStreamParams, SlotClaim, SlotReuseCandidate},
};

mod ffi {
    use std::ffi::{c_char, c_void};

    unsafe extern "C" {
        pub fn porthole_native_string_free(message: *mut c_char);

        pub fn porthole_native_metal_create(out_metal: *mut *mut c_void) -> *mut c_char;
        pub fn porthole_native_metal_destroy(metal: *mut c_void);

        pub fn porthole_native_surface_create(
            width: u32,
            height: u32,
            fourcc: u32,
            bytes_per_element: u32,
            out_surface: *mut *mut c_void,
        ) -> *mut c_char;
        pub fn porthole_native_surface_retain(surface: *mut c_void);
        pub fn porthole_native_surface_release(surface: *mut c_void);
        pub fn porthole_native_surface_hold(surface: *mut c_void);
        pub fn porthole_native_surface_unhold(surface: *mut c_void);
        pub fn porthole_native_surface_in_use(surface: *mut c_void) -> i32;
        pub fn porthole_native_surface_width(surface: *mut c_void) -> u32;
        pub fn porthole_native_surface_height(surface: *mut c_void) -> u32;
        pub fn porthole_native_surface_write(surface: *mut c_void, pixels: *const u8, len: usize) -> *mut c_char;
        pub fn porthole_native_surface_read(surface: *mut c_void, pixels: *mut u8, len: usize) -> *mut c_char;

        pub fn porthole_native_pool_create(
            metal: *mut c_void,
            width: u32,
            height: u32,
            fourcc: u32,
            bytes_per_element: u32,
            mtl_pixel_format: u64,
            slot_count: u32,
            out_pool: *mut *mut c_void,
        ) -> *mut c_char;
        pub fn porthole_native_pool_destroy(pool: *mut c_void);
        pub fn porthole_native_pool_surface_in_use(pool: *mut c_void, slot_id: u32) -> i32;
        pub fn porthole_native_pool_copy_surface(pool: *mut c_void, slot_id: u32) -> *mut c_void;

        pub fn porthole_native_event_create(metal: *mut c_void, out_event: *mut *mut c_void) -> *mut c_char;
        pub fn porthole_native_event_destroy(event: *mut c_void);
        pub fn porthole_native_event_copy_handle(event: *mut c_void) -> *mut c_void;
        pub fn porthole_native_object_release(object: *mut c_void);
        pub fn porthole_native_event_from_handle(metal: *mut c_void, handle: *mut c_void, out_event: *mut *mut c_void) -> *mut c_char;
        pub fn porthole_native_event_signaled_value(event: *mut c_void) -> u64;
        pub fn porthole_native_event_wait(event: *mut c_void, value: u64, timeout_ms: u64) -> i32;

        pub fn porthole_native_stage_blit(
            metal: *mut c_void,
            pool: *mut c_void,
            slot_id: u32,
            src_surface: *mut c_void,
            out_stage: *mut *mut c_void,
        ) -> *mut c_char;
        pub fn porthole_native_stage_commit(stage: *mut c_void, event: *mut c_void, value: u64) -> *mut c_char;
        pub fn porthole_native_stage_destroy(stage: *mut c_void);

        pub fn porthole_native_consumer_sample(
            metal: *mut c_void,
            event: *mut c_void,
            fence_value: u64,
            src_surface: *mut c_void,
            width: u32,
            height: u32,
            out_pixels: *mut u8,
            out_len: usize,
        ) -> *mut c_char;
    }
}

/// Converts a shim error string into [`CaptureTransferError::NativeBackend`],
/// freeing the C allocation. `Ok(())` when the shim returned NULL.
fn check(operation: &'static str, error: *mut c_char) -> Result<()> {
    if error.is_null() {
        return Ok(());
    }
    let message = unsafe { CStr::from_ptr(error) }.to_string_lossy().into_owned();
    unsafe { ffi::porthole_native_string_free(error) };
    Err(CaptureTransferError::NativeBackend { operation, message })
}

/// (IOSurface fourcc, bytes per element, MTLPixelFormat raw value).
fn format_desc(format: PixelFormat) -> Result<(u32, u32, u64)> {
    match format {
        // 'BGRA' / MTLPixelFormatBGRA8Unorm
        PixelFormat::Bgra8Unorm => Ok((0x4247_5241, 4, 80)),
        // 'RGBA' / MTLPixelFormatRGBA8Unorm
        PixelFormat::Rgba8Unorm => Ok((0x5247_4241, 4, 70)),
        PixelFormat::Unknown => Err(CaptureTransferError::NativeBackend {
            operation: "format-desc",
            message: "unknown pixel format has no IOSurface mapping".to_string(),
        }),
    }
}

/// Ids unique across daemon restarts: pid in the high bits, a process-local
/// counter in the low bits. Pool and fence ids share the scheme.
fn next_unique_id(counter: &AtomicU64) -> u64 {
    let pid = std::process::id() as u64;
    (pid << 32) | counter.fetch_add(1, Ordering::Relaxed)
}

static NEXT_POOL_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_FENCE_ID: AtomicU64 = AtomicU64::new(1);

/// A Metal device + command queue. One per producer or consumer; cheap to
/// create, thread-safe to use.
#[derive(Debug)]
pub struct MetalContext {
    raw: NonNull<c_void>,
}

// Metal devices, queues, and shared events are documented thread-safe.
unsafe impl Send for MetalContext {}
unsafe impl Sync for MetalContext {}

impl MetalContext {
    pub fn new() -> Result<Self> {
        let mut raw: *mut c_void = std::ptr::null_mut();
        check("metal-create", unsafe { ffi::porthole_native_metal_create(&mut raw) })?;
        Ok(Self {
            raw: NonNull::new(raw).expect("shim returned NULL metal context without error"),
        })
    }
}

impl Drop for MetalContext {
    fn drop(&mut self) {
        unsafe { ffi::porthole_native_metal_destroy(self.raw.as_ptr()) };
    }
}

/// A retained IOSurface reference. This is the macOS `SurfaceHandle`: the XPC
/// transport encodes it as an object; in-process consumers use it directly.
#[derive(Debug)]
pub struct IoSurface {
    raw: NonNull<c_void>,
}

// IOSurface is a thread-safe CF type.
unsafe impl Send for IoSurface {}
unsafe impl Sync for IoSurface {}

impl IoSurface {
    /// Allocate a standalone surface (tests and synthetic sources; real
    /// captured surfaces arrive from SCK).
    pub fn allocate(width: u32, height: u32, format: PixelFormat) -> Result<Self> {
        let (fourcc, bytes_per_element, _) = format_desc(format)?;
        let mut raw: *mut c_void = std::ptr::null_mut();
        check("surface-create", unsafe {
            ffi::porthole_native_surface_create(width, height, fourcc, bytes_per_element, &mut raw)
        })?;
        Ok(Self {
            raw: NonNull::new(raw).expect("shim returned NULL surface without error"),
        })
    }

    /// Adopt a +1 retained IOSurfaceRef (e.g. from an XPC decode).
    /// Ownership transfers to the wrapper.
    ///
    /// # Safety
    /// `raw` must be a retained IOSurfaceRef the caller owns.
    #[must_use]
    pub unsafe fn from_retained(raw: NonNull<c_void>) -> Self {
        Self { raw }
    }

    /// Wrap a borrowed IOSurfaceRef, retaining it — e.g. an SCK callback's
    /// surface, which the sample buffer keeps alive only for the callback's
    /// duration.
    ///
    /// # Safety
    /// `raw` must be a valid IOSurfaceRef for the duration of this call.
    #[must_use]
    pub unsafe fn from_borrowed(raw: NonNull<c_void>) -> Self {
        unsafe { ffi::porthole_native_surface_retain(raw.as_ptr()) };
        Self { raw }
    }

    /// The raw IOSurfaceRef, borrowed (e.g. for the XPC transport to encode).
    #[must_use]
    pub fn as_raw(&self) -> *mut c_void {
        self.raw.as_ptr()
    }

    #[must_use]
    pub fn width(&self) -> u32 {
        unsafe { ffi::porthole_native_surface_width(self.raw.as_ptr()) }
    }

    #[must_use]
    pub fn height(&self) -> u32 {
        unsafe { ffi::porthole_native_surface_height(self.raw.as_ptr()) }
    }

    /// Whether any process is using the surface (`IOSurfaceIsInUse`).
    #[must_use]
    pub fn is_in_use(&self) -> bool {
        unsafe { ffi::porthole_native_surface_in_use(self.raw.as_ptr()) != 0 }
    }

    /// Consumer-side hold (`IOSurfaceIncrementUseCount`). Per the hold
    /// protocol in jackstay_ring.h, take the hold *before* re-validating the
    /// ring entry's liveness.
    pub fn hold(&self) {
        unsafe { ffi::porthole_native_surface_hold(self.raw.as_ptr()) };
    }

    /// Consumer-side release (`IOSurfaceDecrementUseCount`).
    pub fn release_hold(&self) {
        unsafe { ffi::porthole_native_surface_unhold(self.raw.as_ptr()) };
    }

    /// Write tightly-packed pixels (`width * height * bpe` bytes) into the
    /// surface, handling its row stride.
    pub fn write_pixels(&self, pixels: &[u8]) -> Result<()> {
        check("surface-write", unsafe {
            ffi::porthole_native_surface_write(self.raw.as_ptr(), pixels.as_ptr(), pixels.len())
        })
    }

    /// Read the surface into a tightly-packed buffer.
    pub fn read_pixels(&self, pixels: &mut [u8]) -> Result<()> {
        check("surface-read", unsafe {
            ffi::porthole_native_surface_read(self.raw.as_ptr(), pixels.as_mut_ptr(), pixels.len())
        })
    }
}

impl Clone for IoSurface {
    fn clone(&self) -> Self {
        unsafe { ffi::porthole_native_surface_retain(self.raw.as_ptr()) };
        Self { raw: self.raw }
    }
}

impl Drop for IoSurface {
    fn drop(&mut self) {
        unsafe { ffi::porthole_native_surface_release(self.raw.as_ptr()) };
    }
}

/// A retained `MTLSharedEventHandle`. This is the macOS `SyncHandle`: it
/// refuses byte serialization and crosses a process boundary only inside an
/// NSXPCCoder message (ADR-0007).
#[derive(Debug)]
pub struct SharedEventHandle {
    raw: NonNull<c_void>,
}

unsafe impl Send for SharedEventHandle {}
unsafe impl Sync for SharedEventHandle {}

impl SharedEventHandle {
    /// Adopt a +1 retained MTLSharedEventHandle (e.g. from an XPC decode).
    ///
    /// # Safety
    /// `raw` must be a retained MTLSharedEventHandle the caller owns.
    #[must_use]
    pub unsafe fn from_retained(raw: NonNull<c_void>) -> Self {
        Self { raw }
    }

    /// The raw handle object, borrowed (for the XPC transport to encode).
    #[must_use]
    pub fn as_raw(&self) -> *mut c_void {
        self.raw.as_ptr()
    }
}

impl Drop for SharedEventHandle {
    fn drop(&mut self) {
        unsafe { ffi::porthole_native_object_release(self.raw.as_ptr()) };
    }
}

/// A captured frame on the native path: a retained IOSurface from the
/// platform source (SCK hands these out per frame callback).
#[derive(Debug)]
pub struct MacosCapturedFrame {
    pub surface: IoSurface,
}

/// The producer-side pool: `slot_count` IOSurfaces with pre-wrapped Metal
/// textures, allocated once per stream config generation.
#[derive(Debug)]
pub struct MacosSurfacePool {
    raw: NonNull<c_void>,
    pool_id: u64,
    slot_count: u32,
}

unsafe impl Send for MacosSurfacePool {}

impl Drop for MacosSurfacePool {
    fn drop(&mut self) {
        unsafe { ffi::porthole_native_pool_destroy(self.raw.as_ptr()) };
    }
}

/// The stream's `MTLSharedEvent`.
#[derive(Debug)]
pub struct MacosFence {
    raw: NonNull<c_void>,
    fence_id: u64,
}

unsafe impl Send for MacosFence {}

impl Drop for MacosFence {
    fn drop(&mut self) {
        unsafe { ffi::porthole_native_event_destroy(self.raw.as_ptr()) };
    }
}

/// An encoded-but-uncommitted blit, between `stage_frame` and `signal_fence`.
#[derive(Debug)]
struct PendingStage {
    raw: NonNull<c_void>,
}

unsafe impl Send for PendingStage {}

impl Drop for PendingStage {
    fn drop(&mut self) {
        unsafe { ffi::porthole_native_stage_destroy(self.raw.as_ptr()) };
    }
}

/// The real macOS backend. See the module docs for the staging model.
#[derive(Debug)]
pub struct MacosFrameBackend {
    metal: MetalContext,
    pending: Option<PendingStage>,
}

impl MacosFrameBackend {
    pub fn new() -> Result<Self> {
        Ok(Self {
            metal: MetalContext::new()?,
            pending: None,
        })
    }

    #[must_use]
    pub fn metal(&self) -> &MetalContext {
        &self.metal
    }
}

impl NativeFrameBackend for MacosFrameBackend {
    type CapturedFrame = MacosCapturedFrame;
    type SurfacePool = MacosSurfacePool;
    type Fence = MacosFence;
    type SurfaceHandle = IoSurface;
    type SyncHandle = SharedEventHandle;

    fn payload_kind(&self) -> PayloadKind {
        PayloadKind::IoSurface
    }

    fn allocate_surface_pool(&mut self, params: &NativeStreamParams, slot_count: u32) -> Result<MacosSurfacePool> {
        if slot_count == 0 {
            return Err(CaptureTransferError::NativeBackend {
                operation: "allocate-surface-pool",
                message: "slot count must be non-zero".to_string(),
            });
        }
        let (fourcc, bytes_per_element, mtl_format) = format_desc(params.pixel_format)?;
        let mut raw: *mut c_void = std::ptr::null_mut();
        check("allocate-surface-pool", unsafe {
            ffi::porthole_native_pool_create(
                self.metal.raw.as_ptr(),
                params.width,
                params.height,
                fourcc,
                bytes_per_element,
                mtl_format,
                slot_count,
                &mut raw,
            )
        })?;
        Ok(MacosSurfacePool {
            raw: NonNull::new(raw).expect("shim returned NULL pool without error"),
            pool_id: next_unique_id(&NEXT_POOL_ID),
            slot_count,
        })
    }

    fn pool_id(&self, pool: &MacosSurfacePool) -> u64 {
        pool.pool_id
    }

    fn claim_reusable_slot(&mut self, pool: &mut MacosSurfacePool, candidates: &[SlotReuseCandidate]) -> Result<SlotClaim> {
        for candidate in candidates {
            // IOSurfaceIsInUse is the cross-process signal (a global bool,
            // not a count): any consumer hold, and any still-in-flight GPU
            // work that references the surface, keeps it true — both
            // correctly block reuse.
            let in_use = unsafe { ffi::porthole_native_pool_surface_in_use(pool.raw.as_ptr(), candidate.slot_id) } != 0;
            if !in_use {
                return Ok(SlotClaim::Ready {
                    slot_id: candidate.slot_id,
                });
            }
        }
        Ok(SlotClaim::WouldBlock)
    }

    fn stage_frame(&mut self, pool: &mut MacosSurfacePool, slot_id: u32, frame: &MacosCapturedFrame) -> Result<()> {
        if self.pending.is_some() {
            return Err(CaptureTransferError::NativeBackend {
                operation: "stage-frame",
                message: "previous staged frame was never signalled".to_string(),
            });
        }
        let mut raw: *mut c_void = std::ptr::null_mut();
        check("stage-frame", unsafe {
            ffi::porthole_native_stage_blit(
                self.metal.raw.as_ptr(),
                pool.raw.as_ptr(),
                slot_id,
                frame.surface.as_raw(),
                &mut raw,
            )
        })?;
        self.pending = Some(PendingStage {
            raw: NonNull::new(raw).expect("shim returned NULL stage without error"),
        });
        Ok(())
    }

    fn export_surface_handles(&self, pool: &MacosSurfacePool) -> Result<Vec<IoSurface>> {
        Ok((0..pool.slot_count)
            .map(|slot_id| {
                let raw = unsafe { ffi::porthole_native_pool_copy_surface(pool.raw.as_ptr(), slot_id) };
                unsafe { IoSurface::from_retained(NonNull::new(raw).expect("pool slot surface is never NULL")) }
            })
            .collect())
    }

    fn create_fence(&mut self) -> Result<MacosFence> {
        let mut raw: *mut c_void = std::ptr::null_mut();
        check("create-fence", unsafe {
            ffi::porthole_native_event_create(self.metal.raw.as_ptr(), &mut raw)
        })?;
        Ok(MacosFence {
            raw: NonNull::new(raw).expect("shim returned NULL event without error"),
            fence_id: next_unique_id(&NEXT_FENCE_ID),
        })
    }

    fn fence_id(&self, fence: &MacosFence) -> u64 {
        fence.fence_id
    }

    fn signal_fence(&mut self, fence: &mut MacosFence, value: u64) -> Result<()> {
        let Some(stage) = self.pending.take() else {
            return Err(CaptureTransferError::NativeBackend {
                operation: "signal-fence",
                message: "no staged frame to signal".to_string(),
            });
        };
        // porthole_native_stage_commit takes ownership via __bridge_transfer;
        // forget the PendingStage so its Drop does not also call
        // porthole_native_stage_destroy (which would double-free).
        let raw = stage.raw;
        std::mem::forget(stage);
        check("signal-fence", unsafe {
            ffi::porthole_native_stage_commit(raw.as_ptr(), fence.raw.as_ptr(), value)
        })
    }

    fn export_sync_handle(&self, fence: &MacosFence) -> Result<SharedEventHandle> {
        let raw = unsafe { ffi::porthole_native_event_copy_handle(fence.raw.as_ptr()) };
        Ok(unsafe { SharedEventHandle::from_retained(NonNull::new(raw).expect("newSharedEventHandle is never NULL")) })
    }
}

/// The consumer's side of the fence: resolve the transferred handle against
/// a local Metal device and wait on per-frame timeline values.
#[derive(Debug)]
pub struct ConsumerFence {
    raw: NonNull<c_void>,
}

unsafe impl Send for ConsumerFence {}
unsafe impl Sync for ConsumerFence {}

impl ConsumerFence {
    pub fn from_handle(metal: &MetalContext, handle: &SharedEventHandle) -> Result<Self> {
        let mut raw: *mut c_void = std::ptr::null_mut();
        check("fence-from-handle", unsafe {
            ffi::porthole_native_event_from_handle(metal.raw.as_ptr(), handle.as_raw(), &mut raw)
        })?;
        Ok(Self {
            raw: NonNull::new(raw).expect("shim returned NULL event without error"),
        })
    }

    /// Block until the timeline reaches `value`; false on timeout. This is a
    /// CPU wait, for tests and diagnostics — a real viewer GPU-waits on the
    /// fence by encoding the wait on its present command buffer (see
    /// [`Self::sample_offscreen`] and the C viewer's Metal path) so sampling
    /// is gated GPU-side without blocking a thread.
    #[must_use]
    pub fn wait(&self, value: u64, timeout_ms: u64) -> bool {
        unsafe { ffi::porthole_native_event_wait(self.raw.as_ptr(), value, timeout_ms) != 0 }
    }

    #[must_use]
    pub fn signaled_value(&self) -> u64 {
        unsafe { ffi::porthole_native_event_signaled_value(self.raw.as_ptr()) }
    }

    /// The consumer present path, rendered to an offscreen readback buffer so
    /// it is testable without a display: latch `surface` into a texture on
    /// `metal`, GPU-wait for `fence_value` on this fence, blit, and read back
    /// `width * height * 4` BGRA8 bytes. No CPU fence wait — the GPU-encoded
    /// wait is what orders the sample after the producer's staging blit.
    ///
    /// `metal` must be the same device this fence was resolved against in
    /// [`Self::from_handle`]; the event belongs to that device.
    pub fn sample_offscreen(
        &self,
        metal: &MetalContext,
        surface: &IoSurface,
        fence_value: u64,
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>> {
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        check("consumer-sample", unsafe {
            ffi::porthole_native_consumer_sample(
                metal.raw.as_ptr(),
                self.raw.as_ptr(),
                fence_value,
                surface.as_raw(),
                width,
                height,
                pixels.as_mut_ptr(),
                pixels.len(),
            )
        })?;
        Ok(pixels)
    }
}

impl Drop for ConsumerFence {
    fn drop(&mut self) {
        unsafe { ffi::porthole_native_event_destroy(self.raw.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::{ConsumerFence, IoSurface, MacosCapturedFrame, MacosFrameBackend, MetalContext};
    use crate::{
        model::{ClockDomain, ColorSpace, PixelFormat},
        native::{NativeStreamParams, NativeTrackProducer, PoolExhaustionPolicy},
    };

    const WIDTH: u32 = 64;
    const HEIGHT: u32 = 48;

    fn params() -> NativeStreamParams {
        NativeStreamParams {
            width: WIDTH,
            height: HEIGHT,
            pixel_format: PixelFormat::Bgra8Unorm,
            color_space: ColorSpace::Srgb,
            clock_domain: ClockDomain::HostTime,
            modifier: 0,
        }
    }

    fn producer() -> NativeTrackProducer<MacosFrameBackend> {
        let backend = MacosFrameBackend::new().expect("Metal device required for backend tests");
        NativeTrackProducer::new(backend, params(), 2, 3, PoolExhaustionPolicy::Fail).unwrap()
    }

    fn gradient(seed: u8) -> Vec<u8> {
        (0..WIDTH as usize * HEIGHT as usize * 4)
            .map(|i| (i as u8).wrapping_add(seed))
            .collect()
    }

    fn captured(seed: u8) -> MacosCapturedFrame {
        let surface = IoSurface::allocate(WIDTH, HEIGHT, PixelFormat::Bgra8Unorm).unwrap();
        surface.write_pixels(&gradient(seed)).unwrap();
        MacosCapturedFrame { surface }
    }

    #[test]
    fn blit_round_trip_publishes_pixels_and_signals_fence() {
        let mut producer = producer();
        let grant = producer.grant_attach(1).unwrap();

        let cursor = producer.publish(&captured(7), 1).unwrap().cursor().unwrap();
        let entry = producer.control_page().read_latest_lossy_entry().unwrap().unwrap();
        assert_eq!(entry.cursor, cursor);
        assert_eq!(entry.pool_id, grant.pool_id);

        // Consumer side: resolve the fence handle, wait for the frame's
        // value, then read the slot surface the grant transferred.
        let metal = MetalContext::new().unwrap();
        let fence = ConsumerFence::from_handle(&metal, &grant.sync_handle).unwrap();
        assert!(fence.wait(entry.fence_value, 5_000), "fence not signalled within 5s");

        let surface = &grant.surface_handles[entry.slot_id as usize];
        let mut pixels = vec![0u8; WIDTH as usize * HEIGHT as usize * 4];
        surface.read_pixels(&mut pixels).unwrap();
        assert_eq!(pixels, gradient(7), "blitted pixels do not match the captured frame");
    }

    #[test]
    fn consumer_gpu_sample_is_zero_copy_and_fence_ordered() {
        let mut producer = producer();
        let grant = producer.grant_attach(1).unwrap();

        // The producer signals the fence on the same command buffer as its
        // staging blit (#84), so the timeline only reaches fence_value once
        // the pool surface actually holds these pixels.
        producer.publish(&captured(9), 1).unwrap();
        let entry = producer.control_page().read_latest_lossy_entry().unwrap().unwrap();

        // A consumer with its OWN device, as a real viewer has. Resolve the
        // transferred handle and GPU-sample the slot surface with NO CPU
        // fence wait: the GPU-encoded encodeWaitForEvent is the only thing
        // ordering the sample after the producer's blit. Correct pixels prove
        // both the zero-copy IOSurface latch and the GPU ordering.
        let metal = MetalContext::new().unwrap();
        let fence = ConsumerFence::from_handle(&metal, &grant.sync_handle).unwrap();
        let surface = &grant.surface_handles[entry.slot_id as usize];
        let sampled = fence
            .sample_offscreen(&metal, surface, entry.fence_value, entry.width, entry.height)
            .unwrap();
        assert_eq!(sampled, gradient(9), "GPU-sampled pixels do not match the published frame");
    }

    #[test]
    fn frames_rotate_pool_slots_and_advance_the_timeline() {
        let mut producer = producer();
        let grant = producer.grant_attach(1).unwrap();
        let metal = MetalContext::new().unwrap();
        let fence = ConsumerFence::from_handle(&metal, &grant.sync_handle).unwrap();

        for sequence in 1..=4u64 {
            producer.publish(&captured(sequence as u8), sequence).unwrap();
            let entry = producer.control_page().read_latest_lossy_entry().unwrap().unwrap();
            assert_eq!(entry.sequence, sequence);
            assert!(fence.wait(entry.fence_value, 5_000), "frame {sequence} fence not signalled");
            let surface = &grant.surface_handles[entry.slot_id as usize];
            let mut pixels = vec![0u8; WIDTH as usize * HEIGHT as usize * 4];
            surface.read_pixels(&mut pixels).unwrap();
            assert_eq!(pixels, gradient(sequence as u8), "frame {sequence} pixels wrong");
        }
    }

    #[test]
    fn consumer_hold_is_visible_through_the_os_use_count() {
        let mut producer = producer();
        let grant = producer.grant_attach(1).unwrap();
        producer.publish(&captured(1), 1).unwrap();
        let entry = producer.control_page().read_latest_lossy_entry().unwrap().unwrap();

        let surface = &grant.surface_handles[entry.slot_id as usize];
        surface.hold();
        assert!(surface.is_in_use(), "hold not visible via IOSurfaceIsInUse");
        surface.release_hold();
    }

    #[test]
    fn mismatched_capture_dimensions_fail_staging() {
        let mut producer = producer();
        let small = IoSurface::allocate(16, 16, PixelFormat::Bgra8Unorm).unwrap();
        let error = producer.publish(&MacosCapturedFrame { surface: small }, 1).unwrap_err();
        assert!(error.to_string().contains("a resize is a new pool"), "unexpected error: {error}");
    }

    #[test]
    fn mismatched_capture_pixel_format_fails_staging() {
        let mut producer = producer();
        let rgba = IoSurface::allocate(WIDTH, HEIGHT, PixelFormat::Rgba8Unorm).unwrap();
        let error = producer.publish(&MacosCapturedFrame { surface: rgba }, 1).unwrap_err();
        assert!(error.to_string().contains("pixel format"), "unexpected error: {error}");
    }

    #[test]
    fn sync_handle_round_trips_in_process() {
        let mut backend = MacosFrameBackend::new().unwrap();
        let fence = {
            use crate::native::NativeFrameBackend;
            backend.create_fence().unwrap()
        };
        use crate::native::NativeFrameBackend;
        let handle = backend.export_sync_handle(&fence).unwrap();
        let metal = MetalContext::new().unwrap();
        let consumer = ConsumerFence::from_handle(&metal, &handle).unwrap();
        assert_eq!(consumer.signaled_value(), 0);
    }
}
