//! The Windows [`NativeFrameBackend`]: a Jackstay-owned pool of shared BGRA8
//! `ID3D11Texture2D`s and `ID3D11Fence` timelines (#28, decided in #22).
//!
//! Staging mirrors the macOS blit + signal: `stage_frame` queues a
//! `CopySubresourceRegion` from the captured texture into the pool slot on the
//! device's immediate context, and `signal_fence` queues the fence signal
//! behind it and flushes. The descriptor can therefore never name a fence value
//! that no submitted copy will reach. The captured texture is only read by that
//! queued copy, so a capture source may return its buffer at once.
//!
//! Every pool texture is created `SHARED | SHARED_NTHANDLE`; its NT handle is
//! the [`SharedTextureHandle`]. The producer fence is a shared `ID3D11Fence`
//! ([`SharedFenceHandle`]). Handles cross a process boundary only by
//! duplication into a verified peer over the setup channel ([`setup`]); they
//! are never named and never sent as bytes a third process could use.
//!
//! Consumers import handles on a device created on the grant's adapter
//! ([`D3d11Device::open_texture`], [`D3d11Device::open_fence`]), GPU-wait on
//! the frame's fence value ([`D3d11Fence::gpu_wait`]) and register their own
//! shared fence for deferred release; its `SetEventOnCompletion` wakes the
//! producer ([`D3d11Fence`] implements [`ReleaseTimeline`]).
//!
//! [`ReleaseTimeline`]: crate::acquisition::arena::ReleaseTimeline

pub mod capture;
pub mod setup;
mod timeline;

use std::{
    fmt,
    os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use ::windows::{
    Win32::{
        Foundation::{GENERIC_ALL, HANDLE, HMODULE, LUID},
        Graphics::{
            Direct3D::{
                D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_11_1,
            },
            Direct3D11::{
                D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_FENCE_FLAG_SHARED, D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_RESOURCE_MISC_SHARED,
                D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
                D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11Device1, ID3D11Device5, ID3D11DeviceContext,
                ID3D11DeviceContext4, ID3D11Fence, ID3D11Texture2D,
            },
            Dxgi::{
                Common::{DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC},
                CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_WAS_STILL_DRAWING, DXGI_SHARED_RESOURCE_READ,
                DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIResource1,
            },
        },
        System::Threading::{CreateEventW, WaitForSingleObject},
    },
    core::{Interface, PCWSTR},
};
use serde::{Deserialize, Serialize};

use crate::{
    error::{CaptureTransferError, Result},
    model::{PayloadKind, PixelFormat},
    native::{NativeFrameBackend, NativeStreamParams, SlotClaim, SlotReuseCandidate},
};

/// Largest texture dimension every D3D11 feature level 11 device accepts.
pub const MAX_TEXTURE_DIMENSION: u32 = 16384;
/// D3D11 NT handles, duplicated at most once per slot per grant, go through
/// the setup channel's bounded handle transfer.
pub const MAX_POOL_SLOTS: u32 = 15;

fn failure(operation: &'static str, error: impl fmt::Display) -> CaptureTransferError {
    CaptureTransferError::NativeBackend {
        operation,
        message: error.to_string(),
    }
}

/// Ids unique across producer restarts: PID in the high bits, a process-local
/// counter in the low bits (the macOS backend's scheme).
fn next_unique_id(counter: &AtomicU64) -> u64 {
    (u64::from(std::process::id()) << 32) | counter.fetch_add(1, Ordering::Relaxed)
}

static NEXT_POOL_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_FENCE_ID: AtomicU64 = AtomicU64::new(1);

/// A DXGI adapter's locally unique identifier, as `(HighPart << 32) | LowPart`.
/// LUIDs are valid only until reboot, and one GPU can appear under more than
/// one LUID (an indirect display adapter reports its render GPU's
/// description); compare LUIDs, never names.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AdapterLuid(pub u64);

impl AdapterLuid {
    #[must_use]
    pub fn from_luid(luid: LUID) -> Self {
        Self((u64::from(luid.HighPart as u32) << 32) | u64::from(luid.LowPart))
    }

    #[must_use]
    pub fn to_luid(self) -> LUID {
        LUID {
            LowPart: self.0 as u32,
            HighPart: (self.0 >> 32) as i32,
        }
    }
}

impl fmt::Debug for AdapterLuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AdapterLuid({self})")
    }
}

impl fmt::Display for AdapterLuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:08x}:{:08x}", self.0 >> 32, self.0 as u32)
    }
}

/// What a device reports about its adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterInfo {
    pub luid: AdapterLuid,
    pub description: String,
    pub vendor_id: u32,
    pub device_id: u32,
    /// WARP / the Basic Render Driver.
    pub software: bool,
}

fn adapter_info(adapter: &IDXGIAdapter1) -> Result<AdapterInfo> {
    // SAFETY: GetDesc1 fills a plain structure from a live adapter.
    let desc = unsafe { adapter.GetDesc1() }.map_err(|error| failure("adapter-desc", error))?;
    let end = desc
        .Description
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(desc.Description.len());
    Ok(AdapterInfo {
        luid: AdapterLuid::from_luid(desc.AdapterLuid),
        description: String::from_utf16_lossy(&desc.Description[..end]),
        vendor_id: desc.VendorId,
        device_id: desc.DeviceId,
        software: desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
    })
}

/// Every adapter DXGI enumerates for this session, in its order: the first
/// is the default a null-adapter device uses.
pub fn adapters() -> Result<Vec<AdapterInfo>> {
    enumerate_adapters()?.iter().map(adapter_info).collect()
}

fn enumerate_adapters() -> Result<Vec<IDXGIAdapter1>> {
    // SAFETY: plain factory creation; enumeration stops at the first error
    // (DXGI_ERROR_NOT_FOUND after the last adapter).
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(|error| failure("dxgi-factory", error))?;
        let mut adapters = Vec::new();
        while let Ok(adapter) = factory.EnumAdapters1(adapters.len() as u32) {
            adapters.push(adapter);
        }
        Ok(adapters)
    }
}

/// Which adapter a [`D3d11Device`] is created on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdapterSelection {
    /// The system default (the first DXGI adapter).
    #[default]
    Default,
    /// Exactly this adapter; creation fails if it is not present.
    Luid(AdapterLuid),
    /// The software rasterizer, for tests and headless runners.
    Warp,
}

/// A D3D11 device with its immediate context. Device methods are free-threaded;
/// the immediate context is used only under this value's lock, so the device
/// can be shared between a capture callback, a producer and a setup server.
pub struct D3d11Device {
    device: ID3D11Device5,
    context: Mutex<ID3D11DeviceContext4>,
    adapter: AdapterInfo,
}

// SAFETY: D3D11 devices (and their device-child objects) are free-threaded
// unless created with D3D11_CREATE_DEVICE_SINGLETHREADED, which this module
// never uses. The immediate context is not; it is only reached through the
// mutex.
unsafe impl Send for D3d11Device {}
unsafe impl Sync for D3d11Device {}

impl fmt::Debug for D3d11Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("D3d11Device")
            .field("adapter", &self.adapter)
            .finish_non_exhaustive()
    }
}

impl D3d11Device {
    /// Create a hardware (or WARP) device with BGRA support at feature level
    /// 11.0 or later.
    pub fn new(selection: AdapterSelection) -> Result<Self> {
        let adapter: Option<IDXGIAdapter> = match selection {
            AdapterSelection::Default | AdapterSelection::Warp => None,
            AdapterSelection::Luid(luid) => {
                let adapters = enumerate_adapters()?;
                let mut found = None;
                for adapter in adapters {
                    if adapter_info(&adapter)?.luid == luid {
                        found = Some(adapter.cast().map_err(|error| failure("adapter-cast", error))?);
                        break;
                    }
                }
                Some(found.ok_or_else(|| failure("select-adapter", format!("no DXGI adapter has LUID {luid}")))?)
            }
        };
        let driver = match (&adapter, selection) {
            (Some(_), _) => D3D_DRIVER_TYPE_UNKNOWN,
            (None, AdapterSelection::Warp) => D3D_DRIVER_TYPE_WARP,
            (None, _) => D3D_DRIVER_TYPE_HARDWARE,
        };
        let mut device = None;
        let mut context = None;
        let mut level = D3D_FEATURE_LEVEL::default();
        // SAFETY: every out pointer is a live local; the adapter, if any, is live.
        unsafe {
            D3D11CreateDevice(
                adapter.as_ref(),
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
        }
        .map_err(|error| failure("create-device", error))?;
        let device = device.ok_or_else(|| failure("create-device", "no device returned"))?;
        let context = context.ok_or_else(|| failure("create-device", "no immediate context returned"))?;
        Self::from_parts(device, context)
    }

    /// Adopt a device the caller created (for example a renderer's own
    /// device). It must not be single-threaded, and the caller must not use its
    /// immediate context concurrently with this value.
    pub fn from_device(device: ID3D11Device) -> Result<Self> {
        // SAFETY: GetImmediateContext returns an owned reference.
        let context = unsafe { device.GetImmediateContext() }.map_err(|error| failure("immediate-context", error))?;
        Self::from_parts(device, context)
    }

    fn from_parts(device: ID3D11Device, context: ID3D11DeviceContext) -> Result<Self> {
        let device: ID3D11Device5 = device
            .cast()
            .map_err(|error| failure("device5", format!("ID3D11Device5 unavailable (Windows 10 1703+ runtime): {error}")))?;
        let context: ID3D11DeviceContext4 = context
            .cast()
            .map_err(|error| failure("context4", format!("ID3D11DeviceContext4 unavailable: {error}")))?;
        // SAFETY: plain COM queries on a live device.
        let adapter = unsafe {
            let dxgi: IDXGIDevice = device.cast().map_err(|error| failure("dxgi-device", error))?;
            let adapter: IDXGIAdapter1 = dxgi
                .GetAdapter()
                .and_then(|adapter| adapter.cast())
                .map_err(|error| failure("dxgi-adapter", error))?;
            adapter_info(&adapter)?
        };
        Ok(Self {
            device,
            context: Mutex::new(context),
            adapter,
        })
    }

    #[must_use]
    pub fn adapter(&self) -> &AdapterInfo {
        &self.adapter
    }

    #[must_use]
    pub fn luid(&self) -> AdapterLuid {
        self.adapter.luid
    }

    /// The device, for callers importing into their own rendering. Do not use
    /// its immediate context outside [`Self::context`].
    #[must_use]
    pub fn raw(&self) -> &ID3D11Device5 {
        &self.device
    }

    /// Exclusive use of the immediate context.
    pub fn context(&self) -> MutexGuard<'_, ID3D11DeviceContext4> {
        self.context.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `Some(reason)` once the device has been removed or reset (for example
    /// when an RDP reconnect replaces the session's display adapter).
    #[must_use]
    pub fn removed_reason(&self) -> Option<::windows::core::Error> {
        // SAFETY: plain query on a live device.
        unsafe { self.device.GetDeviceRemovedReason() }.err()
    }

    /// Whether this device can create, share and reopen a fence. Devices that
    /// cannot are reported and must publish through the CPU path instead.
    pub fn probe_shared_fences(&self) -> Result<()> {
        let fence = self.create_shared_fence()?;
        let handle = fence.export_handle()?;
        self.open_fence(&handle)?;
        Ok(())
    }

    /// A new shared fence at value 0: a consumer's release timeline, or a
    /// test gate.
    pub fn create_shared_fence(&self) -> Result<D3d11Fence> {
        let mut fence: Option<ID3D11Fence> = None;
        // SAFETY: the out pointer is a live local.
        unsafe { self.device.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence) }.map_err(|error| failure("create-fence", error))?;
        Ok(D3d11Fence::new(fence.ok_or_else(|| failure("create-fence", "no fence returned"))?))
    }

    /// Import a fence handle received from the setup channel. The handle may
    /// be closed afterwards; the opened fence keeps the object alive.
    pub fn open_fence(&self, handle: &SharedFenceHandle) -> Result<D3d11Fence> {
        let mut fence: Option<ID3D11Fence> = None;
        // SAFETY: the handle is a live NT handle this value owns.
        unsafe { self.device.OpenSharedFence(raw(handle.as_handle()), &mut fence) }.map_err(|error| failure("open-fence", error))?;
        Ok(D3d11Fence::new(fence.ok_or_else(|| failure("open-fence", "no fence returned"))?))
    }

    /// Import a pool texture handle. Open once per slot per pool and cache;
    /// the texture outlives the handle.
    pub fn open_texture(&self, handle: &SharedTextureHandle) -> Result<ID3D11Texture2D> {
        let device: ID3D11Device1 = self.device.cast().map_err(|error| failure("device1", error))?;
        // SAFETY: the handle is a live NT handle this value owns.
        unsafe { device.OpenSharedResource1(raw(handle.as_handle())) }.map_err(|error| failure("open-texture", error))
    }

    /// A texture holding `pixels` (tightly packed, 4 bytes per pixel), for
    /// synthetic sources and tests.
    pub fn upload(&self, width: u32, height: u32, format: PixelFormat, pixels: &[u8]) -> Result<ID3D11Texture2D> {
        let expected = texture_bytes(width, height)?;
        if pixels.len() as u64 != expected {
            return Err(failure("upload", format!("{} bytes for a {width}x{height} texture", pixels.len())));
        }
        let desc = texture_desc(width, height, dxgi_format(format)?, false);
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: width * 4,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        // SAFETY: the description, initial data (sized above) and out pointer are live.
        unsafe { self.device.CreateTexture2D(&desc, Some(&data), Some(&mut texture)) }.map_err(|error| failure("upload", error))?;
        texture.ok_or_else(|| failure("upload", "no texture returned"))
    }

    /// Queue a readback of `texture` behind GPU waits, then optionally signal
    /// `release` once the copy has executed. Nothing blocks on the CPU: the
    /// GPU waits order the copy after the producer's write (and any test gate).
    /// Finish with [`PendingReadback::finish`].
    pub fn submit_readback(
        &self,
        texture: &ID3D11Texture2D,
        waits: &[(&D3d11Fence, u64)],
        release: Option<(&D3d11Fence, u64)>,
    ) -> Result<PendingReadback> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: GetDesc fills a local structure.
        unsafe { texture.GetDesc(&mut desc) };
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..desc
        };
        let mut staging = None;
        // SAFETY: the description and out pointer are live locals.
        unsafe { self.device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }.map_err(|error| failure("readback", error))?;
        let staging = staging.ok_or_else(|| failure("readback", "no staging texture returned"))?;
        let context = self.context();
        // SAFETY: all resources belong to this device; the context is locked.
        unsafe {
            for (fence, value) in waits {
                context
                    .Wait(&fence.fence, *value)
                    .map_err(|error| failure("readback-wait", error))?;
            }
            context.CopyResource(&staging, texture);
            if let Some((fence, value)) = release {
                context
                    .Signal(&fence.fence, value)
                    .map_err(|error| failure("readback-signal", error))?;
            }
            context.Flush();
        }
        Ok(PendingReadback {
            staging,
            width: desc.Width,
            height: desc.Height,
        })
    }

    /// [`Self::submit_readback`] and wait for it.
    pub fn read_pixels(&self, texture: &ID3D11Texture2D, waits: &[(&D3d11Fence, u64)], timeout: Duration) -> Result<Vec<u8>> {
        self.submit_readback(texture, waits, None)?.finish(self, timeout)
    }
}

/// A submitted readback; see [`D3d11Device::submit_readback`].
pub struct PendingReadback {
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
}

impl PendingReadback {
    /// Tightly packed pixels, once the GPU has executed the copy. Polls
    /// without holding the context lock between attempts.
    pub fn finish(self, device: &D3d11Device, timeout: Duration) -> Result<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let context = device.context();
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                // SAFETY: the staging texture belongs to this device; the
                // context is locked; DO_NOT_WAIT returns WAS_STILL_DRAWING
                // instead of blocking on queued GPU waits.
                match unsafe {
                    context.Map(
                        &self.staging,
                        0,
                        D3D11_MAP_READ,
                        D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                        Some(&mut mapped),
                    )
                } {
                    Ok(()) => {
                        let row = self.width as usize * 4;
                        let mut pixels = vec![0; row * self.height as usize];
                        for (y, target) in pixels.chunks_exact_mut(row).enumerate() {
                            // SAFETY: the mapping holds `height` rows of `RowPitch >= row` bytes.
                            let source =
                                unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize), row) };
                            target.copy_from_slice(source);
                        }
                        // SAFETY: mapped above on this locked context.
                        unsafe { context.Unmap(&self.staging, 0) };
                        return Ok(pixels);
                    }
                    Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => {}
                    Err(error) => return Err(failure("readback-map", error)),
                }
            }
            if Instant::now() >= deadline {
                return Err(failure("readback-map", "GPU readback did not complete before the timeout"));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

fn raw(handle: BorrowedHandle<'_>) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}

/// Adopt a HANDLE returned by a `CreateSharedHandle` call.
fn owned(handle: HANDLE) -> OwnedHandle {
    // SAFETY: the caller passes a fresh handle it owns exclusively.
    unsafe { OwnedHandle::from_raw_handle(handle.0) }
}

/// The NT handle of one pool texture: the Windows `SurfaceHandle`. It can be
/// duplicated into a verified peer ([`crate::local::send_handles`]) and
/// imported with [`D3d11Device::open_texture`].
#[derive(Debug)]
pub struct SharedTextureHandle(OwnedHandle);

/// The NT handle of a shared `ID3D11Fence`: the Windows `SyncHandle`.
#[derive(Debug)]
pub struct SharedFenceHandle(OwnedHandle);

macro_rules! shared_handle {
    ($name:ident) => {
        impl $name {
            /// Adopt a handle received from a trusted setup channel. Import
            /// validates the object type; a wrong handle fails there.
            #[must_use]
            pub fn from_owned(handle: OwnedHandle) -> Self {
                Self(handle)
            }

            #[must_use]
            pub fn into_owned(self) -> OwnedHandle {
                self.0
            }

            pub fn try_clone(&self) -> Result<Self> {
                self.0
                    .try_clone()
                    .map(Self)
                    .map_err(|error| failure("duplicate-shared-handle", error))
            }
        }

        impl AsHandle for $name {
            fn as_handle(&self) -> BorrowedHandle<'_> {
                self.0.as_handle()
            }
        }
    };
}
shared_handle!(SharedTextureHandle);
shared_handle!(SharedFenceHandle);

/// The completed value a fence reports once its device is gone: removed, or
/// its process exited. See [`D3d11Fence::is_abandoned`].
pub const ABANDONED_FENCE_VALUE: u64 = u64::MAX;

/// An `ID3D11Fence` timeline: the producer's readiness fence, a consumer's
/// release fence, or either one opened from its shared handle.
///
/// As a [`ReleaseTimeline`](crate::acquisition::arena::ReleaseTimeline), its
/// notifications come from `SetEventOnCompletion` on an event waited by the
/// system thread pool, so the producer wakes without polling.
pub struct D3d11Fence {
    fence: ID3D11Fence,
    notifier: timeline::Notifier,
}

// SAFETY: ID3D11Fence is a device child, free-threaded like its device;
// GetCompletedValue and SetEventOnCompletion need no context.
unsafe impl Send for D3d11Fence {}
unsafe impl Sync for D3d11Fence {}

impl fmt::Debug for D3d11Fence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("D3d11Fence")
            .field("completed", &self.completed_value())
            .finish_non_exhaustive()
    }
}

impl D3d11Fence {
    fn new(fence: ID3D11Fence) -> Self {
        Self {
            notifier: timeline::Notifier::new(fence.clone()),
            fence,
        }
    }

    /// Adopt a fence the caller created on its own device, for example a
    /// renderer's shared release fence. Registration as a release timeline
    /// needs one created with `D3D11_FENCE_FLAG_SHARED`.
    #[must_use]
    pub fn from_raw(fence: ID3D11Fence) -> Self {
        Self::new(fence)
    }

    #[must_use]
    pub fn raw(&self) -> &ID3D11Fence {
        &self.fence
    }

    /// The completed value. A removed device reports `u64::MAX`: no further
    /// GPU work on it can run.
    #[must_use]
    pub fn completed_value(&self) -> u64 {
        // SAFETY: plain query on a live fence.
        unsafe { self.fence.GetCompletedValue() }
    }

    /// The fence's device is gone: removed, or its process exited (observed on
    /// Beaufort, #28). Its value then reads `u64::MAX` and satisfies every
    /// wait, although the work it covered never ran: a consumer must discard
    /// frames whose readiness fence is abandoned, and treat the producer as
    /// lost.
    #[must_use]
    pub fn is_abandoned(&self) -> bool {
        self.completed_value() == ABANDONED_FENCE_VALUE
    }

    /// Block until `value` completes; false on timeout. For tests and
    /// diagnostics: renderers GPU-wait with [`Self::gpu_wait`].
    #[must_use]
    pub fn wait_cpu(&self, value: u64, timeout: Duration) -> bool {
        if self.completed_value() >= value {
            return true;
        }
        // SAFETY: a fresh unnamed event, owned and closed below.
        let Ok(event) = (unsafe { CreateEventW(None, true, false, PCWSTR::null()) }) else {
            return false;
        };
        let event = owned(event);
        // SAFETY: the event is live until the wait below returns.
        if unsafe { self.fence.SetEventOnCompletion(value, raw(event.as_handle())) }.is_err() {
            return false;
        }
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        // SAFETY: waiting on a live event.
        unsafe { WaitForSingleObject(raw(event.as_handle()), millis) }.0 == 0 || self.completed_value() >= value
    }

    /// Queue a GPU wait for `value` on `device`'s immediate context: later work
    /// on that context runs only after it. The fence must be open on `device`.
    pub fn gpu_wait(&self, device: &D3d11Device, value: u64) -> Result<()> {
        // SAFETY: the fence was opened on this device; the context is locked.
        unsafe { device.context().Wait(&self.fence, value) }.map_err(|error| failure("fence-gpu-wait", error))
    }

    /// Queue a signal of `value` after all work already queued on `device`'s
    /// immediate context, and flush so another device can observe it.
    pub fn signal_gpu(&self, device: &D3d11Device, value: u64) -> Result<()> {
        let context = device.context();
        // SAFETY: the fence belongs to this device; the context is locked.
        unsafe {
            context.Signal(&self.fence, value).map_err(|error| failure("fence-signal", error))?;
            context.Flush();
        }
        Ok(())
    }

    /// Duplicate a shared NT handle for transfer or reopening.
    pub fn export_handle(&self) -> Result<SharedFenceHandle> {
        // SAFETY: the fence was created shared; GENERIC_ALL is the only access
        // ID3D11Fence::CreateSharedHandle supports.
        let handle = unsafe { self.fence.CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null()) }
            .map_err(|error| failure("export-fence", error))?;
        Ok(SharedFenceHandle(owned(handle)))
    }
}

/// A captured frame: a texture on the backend's device and the region of it
/// to publish. A capture source can return the texture to its pool as soon as
/// `publish` returns: the queued copy is the texture's last use.
pub struct D3d11CapturedFrame {
    texture: ID3D11Texture2D,
    left: u32,
    top: u32,
    width: u32,
    height: u32,
}

impl fmt::Debug for D3d11CapturedFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("D3d11CapturedFrame")
            .field("left", &self.left)
            .field("top", &self.top)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl D3d11CapturedFrame {
    /// The whole texture.
    #[must_use]
    pub fn new(texture: ID3D11Texture2D) -> Self {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: GetDesc fills a local structure.
        unsafe { texture.GetDesc(&mut desc) };
        Self {
            texture,
            left: 0,
            top: 0,
            width: desc.Width,
            height: desc.Height,
        }
    }

    /// A `width` x `height` region at (`left`, `top`), for example a window's
    /// client area inside its captured frame. Must lie inside the texture.
    pub fn cropped(texture: ID3D11Texture2D, left: u32, top: u32, width: u32, height: u32) -> Result<Self> {
        let whole = Self::new(texture);
        let fits = width > 0
            && height > 0
            && left.checked_add(width).is_some_and(|right| right <= whole.width)
            && top.checked_add(height).is_some_and(|bottom| bottom <= whole.height);
        if !fits {
            return Err(failure(
                "crop-frame",
                format!(
                    "{width}x{height} at ({left}, {top}) lies outside the {}x{} texture",
                    whole.width, whole.height
                ),
            ));
        }
        Ok(Self {
            left,
            top,
            width,
            height,
            ..whole
        })
    }

    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// The producer's pool: `slot_count` shared textures, allocated per
/// configuration generation.
pub struct D3d11SurfacePool {
    pool_id: u64,
    textures: Vec<ID3D11Texture2D>,
    handles: Vec<SharedTextureHandle>,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    charged_bytes: u64,
}

// SAFETY: textures are free-threaded device children; the pool is only used
// through the backend, whose context use is serialized.
unsafe impl Send for D3d11SurfacePool {}

impl fmt::Debug for D3d11SurfacePool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("D3d11SurfacePool")
            .field("pool_id", &self.pool_id)
            .field("slots", &self.textures.len())
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl D3d11SurfacePool {
    /// The producer-side texture of a slot (for example a scaling pass that
    /// renders into the slot instead of copying).
    #[must_use]
    pub fn texture(&self, slot_id: u32) -> Option<&ID3D11Texture2D> {
        self.textures.get(slot_id as usize)
    }
}

/// The producer's timeline and its shared handle.
pub struct D3d11ProducerFence {
    fence: Arc<D3d11Fence>,
    fence_id: u64,
}

impl fmt::Debug for D3d11ProducerFence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("D3d11ProducerFence")
            .field("fence_id", &self.fence_id)
            .finish_non_exhaustive()
    }
}

/// The Windows backend. See the module docs for the staging model.
#[derive(Debug)]
pub struct D3d11FrameBackend {
    device: Arc<D3d11Device>,
    pending: bool,
}

impl D3d11FrameBackend {
    /// Stage on `device`, which must support shared fences
    /// ([`D3d11Device::probe_shared_fences`]); otherwise publish through the
    /// CPU arena.
    #[must_use]
    pub fn new(device: Arc<D3d11Device>) -> Self {
        Self { device, pending: false }
    }

    #[must_use]
    pub fn device(&self) -> &Arc<D3d11Device> {
        &self.device
    }
}

fn dxgi_format(format: PixelFormat) -> Result<DXGI_FORMAT> {
    match format {
        PixelFormat::Bgra8Unorm => Ok(DXGI_FORMAT_B8G8R8A8_UNORM),
        PixelFormat::Rgba8Unorm => Ok(DXGI_FORMAT_R8G8B8A8_UNORM),
        PixelFormat::Unknown => Err(failure("format", "unknown pixel format has no DXGI mapping")),
    }
}

fn texture_desc(width: u32, height: u32, format: DXGI_FORMAT, shared: bool) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: if shared {
            (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32
        } else {
            0
        },
    }
}

fn texture_bytes(width: u32, height: u32) -> Result<u64> {
    if width == 0 || height == 0 || width > MAX_TEXTURE_DIMENSION || height > MAX_TEXTURE_DIMENSION {
        return Err(failure(
            "texture-size",
            format!("{width}x{height} is outside 1..={MAX_TEXTURE_DIMENSION} per dimension"),
        ));
    }
    Ok(u64::from(width) * u64::from(height) * 4)
}

/// D3D11 does not report a texture's allocation size. Charge a conservative
/// bound for tiled layouts: both dimensions rounded up to 64 texels, 4 bytes
/// per texel, rounded up to the 64 KiB allocation granularity, per slot.
fn slot_charge(width: u32, height: u32) -> Result<u64> {
    texture_bytes(width, height)?;
    let round = |value: u64, to: u64| value.div_ceil(to) * to;
    Ok(round(round(u64::from(width), 64) * round(u64::from(height), 64) * 4, 64 * 1024))
}

impl NativeFrameBackend for D3d11FrameBackend {
    type CapturedFrame = D3d11CapturedFrame;
    type SurfacePool = D3d11SurfacePool;
    type Fence = D3d11ProducerFence;
    type SurfaceHandle = SharedTextureHandle;
    type SyncHandle = SharedFenceHandle;

    fn payload_kind(&self) -> PayloadKind {
        PayloadKind::D3dSharedResource
    }

    fn allocate_surface_pool(&mut self, params: &NativeStreamParams, slot_count: u32) -> Result<D3d11SurfacePool> {
        super::arena::ArenaNativeBackend::allocate_surface_pool_bounded(self, params, slot_count, u64::MAX)
    }

    fn pool_id(&self, pool: &D3d11SurfacePool) -> u64 {
        pool.pool_id
    }

    /// The arena's claim and release accounting (including consumer release
    /// fences) and the producer's own completed value already exclude slots in
    /// use. D3D11 has no cross-process in-use query to add.
    fn claim_reusable_slot(&mut self, _pool: &mut D3d11SurfacePool, candidates: &[SlotReuseCandidate]) -> Result<SlotClaim> {
        Ok(candidates.first().map_or(SlotClaim::WouldBlock, |candidate| SlotClaim::Ready {
            slot_id: candidate.slot_id,
        }))
    }

    fn stage_frame(&mut self, pool: &mut D3d11SurfacePool, slot_id: u32, frame: &D3d11CapturedFrame) -> Result<()> {
        if self.pending {
            return Err(failure("stage-frame", "previous staged frame was never signalled"));
        }
        if (frame.width, frame.height) != (pool.width, pool.height) {
            return Err(failure(
                "stage-frame",
                format!(
                    "captured {}x{} into a {}x{} pool; a resize is a new pool",
                    frame.width, frame.height, pool.width, pool.height
                ),
            ));
        }
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: GetDesc fills a local structure.
        unsafe { frame.texture.GetDesc(&mut desc) };
        if desc.Format != pool.format {
            return Err(failure(
                "stage-frame",
                format!("captured pixel format {:?} differs from the pool's {:?}", desc.Format, pool.format),
            ));
        }
        let target = pool
            .textures
            .get(slot_id as usize)
            .ok_or_else(|| failure("stage-frame", "slot outside the pool"))?;
        let region = D3D11_BOX {
            left: frame.left,
            top: frame.top,
            front: 0,
            right: frame.left + frame.width,
            bottom: frame.top + frame.height,
            back: 1,
        };
        // SAFETY: both textures belong to this device (a foreign texture fails
        // the copy in the runtime, not memory safety); the context is locked.
        unsafe {
            self.device
                .context()
                .CopySubresourceRegion(target, 0, 0, 0, 0, &frame.texture, 0, Some(&region));
        }
        self.pending = true;
        Ok(())
    }

    fn export_surface_handles(&self, pool: &D3d11SurfacePool) -> Result<Vec<SharedTextureHandle>> {
        pool.handles.iter().map(SharedTextureHandle::try_clone).collect()
    }

    fn create_fence(&mut self) -> Result<D3d11ProducerFence> {
        Ok(D3d11ProducerFence {
            fence: Arc::new(self.device.create_shared_fence()?),
            fence_id: next_unique_id(&NEXT_FENCE_ID),
        })
    }

    fn fence_id(&self, fence: &D3d11ProducerFence) -> u64 {
        fence.fence_id
    }

    fn signal_fence(&mut self, fence: &mut D3d11ProducerFence, value: u64) -> Result<()> {
        if !std::mem::take(&mut self.pending) {
            return Err(failure("signal-fence", "no staged frame to signal"));
        }
        fence.fence.signal_gpu(&self.device, value)
    }

    fn export_sync_handle(&self, fence: &D3d11ProducerFence) -> Result<SharedFenceHandle> {
        fence.fence.export_handle()
    }
}

impl super::arena::ArenaNativeBackend for D3d11FrameBackend {
    fn pool_allocation_upper_bound(&self, params: &NativeStreamParams, slot_count: u32) -> Result<u64> {
        dxgi_format(params.pixel_format)?;
        if slot_count == 0 || slot_count > MAX_POOL_SLOTS {
            return Err(failure(
                "native-pool-preflight",
                format!("{slot_count} slots is outside 1..={MAX_POOL_SLOTS}"),
            ));
        }
        Ok(slot_charge(params.width, params.height)? * u64::from(slot_count))
    }

    fn allocate_surface_pool_bounded(
        &mut self,
        params: &NativeStreamParams,
        slot_count: u32,
        reserved_bytes: u64,
    ) -> Result<D3d11SurfacePool> {
        let charged_bytes = self.pool_allocation_upper_bound(params, slot_count)?;
        if charged_bytes > reserved_bytes {
            return Err(failure(
                "allocate-surface-pool",
                format!("pool needs {charged_bytes} bytes but {reserved_bytes} were reserved"),
            ));
        }
        let format = dxgi_format(params.pixel_format)?;
        let desc = texture_desc(params.width, params.height, format, true);
        let mut textures = Vec::with_capacity(slot_count as usize);
        let mut handles = Vec::with_capacity(slot_count as usize);
        for _ in 0..slot_count {
            let mut texture = None;
            // SAFETY: the description and out pointer are live locals.
            unsafe { self.device.raw().CreateTexture2D(&desc, None, Some(&mut texture)) }
                .map_err(|error| failure("allocate-surface-pool", error))?;
            let texture = texture.ok_or_else(|| failure("allocate-surface-pool", "no texture returned"))?;
            let resource: IDXGIResource1 = texture.cast().map_err(|error| failure("allocate-surface-pool", error))?;
            // SAFETY: the texture was created SHARED_NTHANDLE; the handle is
            // unnamed and adopted at once.
            let handle =
                unsafe { resource.CreateSharedHandle(None, (DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE).0, PCWSTR::null()) }
                    .map_err(|error| failure("share-surface", error))?;
            handles.push(SharedTextureHandle(owned(handle)));
            textures.push(texture);
        }
        Ok(D3d11SurfacePool {
            pool_id: next_unique_id(&NEXT_POOL_ID),
            textures,
            handles,
            width: params.width,
            height: params.height,
            format,
            charged_bytes,
        })
    }

    fn allocated_pool_bytes(&self, pool: &D3d11SurfacePool) -> Result<u64> {
        Ok(pool.charged_bytes)
    }

    fn completed_producer_value(&self, fence: &D3d11ProducerFence) -> Result<u64> {
        Ok(fence.fence.completed_value())
    }

    fn producer_completion_timeline(&self, fence: &D3d11ProducerFence) -> Result<Arc<dyn crate::acquisition::arena::ReleaseTimeline>> {
        Ok(fence.fence.clone())
    }
}
