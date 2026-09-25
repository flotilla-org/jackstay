//! Windows.Graphics.Capture sessions publishing into a Jackstay arena (#28,
//! placement decided in #23).
//!
//! The host (Porthole) authorizes and selects a window, creates its
//! `GraphicsCaptureItem` and hands it over as a [`CaptureTarget`] with a
//! [`CapturePolicy`]. Jackstay owns everything after that: its own D3D11
//! device, a free-threaded frame pool of two buffers, the session, the
//! immediate copy (cropped to the client area) into its shared pool, fences
//! and adapter identity. Each WGC buffer is copied and returned at once; it is
//! never leased to a consumer.
//!
//! Lifecycle:
//! - Resize: a new content size recreates the frame pool; a new output size
//!   reconfigures the arena (a new pool generation that consumers install;
//!   frames they hold keep their old pool).
//! - `GraphicsCaptureItem.Closed`: terminal. Publication stops; nothing
//!   restarts it.
//! - Lock or RDP disconnect: [`CaptureState::Paused`]; frames are dropped and
//!   capture resumes when the desktop returns. Not a failure.
//! - Device loss (for example after an RDP reconnect): a new device, session
//!   and arena. [`CaptureStatus::epoch`] advances and the new
//!   [`Publication`] carries the new adapter LUID; consumers attach again.
//! - A device that cannot share fences publishes through the CPU arena
//!   instead, reported as [`PublicationMode::Cpu`].
//!
//! Frame-pool events run on system thread-pool threads. All device context
//! use happens under one lock per capture, and teardown closes WGC objects
//! outside that lock.

mod desktop;
mod scale;
mod window;

use std::{
    fmt,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use ::windows::{
    Foundation::{TimeSpan, TypedEventHandler},
    Graphics::{
        Capture::{
            Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind, GraphicsCaptureItem,
            GraphicsCaptureSession,
        },
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus,
    Win32::{
        Foundation::HWND,
        Graphics::{
            Direct3D11::{
                D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
                ID3D11Texture2D,
            },
            Dxgi::IDXGIDevice,
        },
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
        },
    },
    core::{IInspectable, Interface},
};
pub use desktop::{DesktopMonitor, DesktopUnavailable, SessionDesktop, SessionState};

use super::{AdapterInfo, AdapterSelection, D3d11CapturedFrame, D3d11Device, D3d11FrameBackend, failure, setup::D3d11Producer};
use crate::{
    acquisition::arena::{ArenaConfig, ArenaError, ArenaProducer, FrameDescriptor, PublishOutcome, ReconfigurationStatus},
    error::Result,
    model::{ClockDomain, ColorSpace, DamageKind, FrameSyncKind, PayloadKind, PixelFormat},
    native::{NativeStreamParams, arena::NativeArenaProducer},
};

/// Device recovery retries (at the watch interval) before the capture fails.
const RECOVERY_ATTEMPTS: u32 = 300;

/// Whether this system has Windows.Graphics.Capture.
#[must_use]
pub fn is_supported() -> bool {
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

/// The capture item for a window (`IGraphicsCaptureItemInterop::CreateForWindow`).
/// The host selects and authorizes the window; this only creates the item.
pub fn capture_item_for_window(window: HWND) -> Result<GraphicsCaptureItem> {
    let interop =
        ::windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().map_err(|error| failure("capture-item", error))?;
    // SAFETY: CreateForWindow validates the window handle.
    unsafe { interop.CreateForWindow(window) }.map_err(|error| failure("capture-item", error))
}

/// What to capture: the host's item, and the window it was created for when
/// the published image should be cropped to that window's client area.
#[derive(Clone)]
pub struct CaptureTarget {
    item: GraphicsCaptureItem,
    window: Option<HWND>,
}

impl fmt::Debug for CaptureTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaptureTarget")
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

impl CaptureTarget {
    /// Publish the whole captured content.
    #[must_use]
    pub fn item(item: GraphicsCaptureItem) -> Self {
        Self { item, window: None }
    }

    /// Publish `window`'s client area. `item` must have been created for it.
    #[must_use]
    pub fn window_client_area(item: GraphicsCaptureItem, window: HWND) -> Self {
        Self {
            item,
            window: Some(window),
        }
    }
}

/// The yellow capture border WGC draws around a captured window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BorderPolicy {
    /// Leave the system default (a border).
    Show,
    /// Ask for borderless capture; keep capturing with a border if refused.
    #[default]
    PreferHidden,
    /// Ask for borderless capture; fail to start if it is refused.
    RequireHidden,
}

/// The published image size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputSize {
    /// The captured region's own size (a resize reconfigures the arena).
    #[default]
    Source,
    /// Scale down, preserving aspect, to fit within this size; never scale up.
    Fit { max_width: u32, max_height: u32 },
    /// Always this size: the region is scaled to fit, preserving aspect,
    /// centred on black. Resizes do not reconfigure the arena.
    Fixed { width: u32, height: u32 },
}

/// How to publish frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PublicationPreference {
    /// Shared D3D11 textures and fences, or CPU publication (reported) if
    /// the device cannot share fences.
    #[default]
    D3d11,
    /// CPU publication even where fences work.
    Cpu,
}

/// Arena sizing for a capture's publications.
#[derive(Debug, Clone, Copy)]
pub struct CaptureArena {
    /// Pool slots (at most [`super::MAX_POOL_SLOTS`] for D3D11).
    pub resource_capacity: u32,
    pub retained_history: u32,
    pub producer_reserve: u32,
    pub memory_budget: u64,
    pub max_incarnations: u32,
    pub drain_timeout: Duration,
}

impl Default for CaptureArena {
    fn default() -> Self {
        Self {
            resource_capacity: 6,
            retained_history: 2,
            producer_reserve: 1,
            memory_budget: 768 * 1024 * 1024,
            max_incarnations: 8,
            drain_timeout: Duration::from_secs(5),
        }
    }
}

/// Host policy for a capture (#23): cursor, border acceptability, minimum
/// update interval and output size, plus the device and arena to use.
#[derive(Debug, Clone)]
pub struct CapturePolicy {
    pub cursor: bool,
    pub border: BorderPolicy,
    /// Frames are delivered at most this often (Windows 11 24H2+; reported
    /// in the status notes where unsupported).
    pub min_update_interval: Option<Duration>,
    pub output_size: OutputSize,
    /// The device's adapter. After device loss, `Default` picks the new
    /// default; a named LUID that no longer exists fails the capture.
    pub adapter: AdapterSelection,
    pub publication: PublicationPreference,
    pub arena: CaptureArena,
    /// Desktop availability; `None` uses [`SessionDesktop`].
    pub desktop: Option<Arc<dyn DesktopMonitor>>,
    /// How often lock/disconnect and device removal are checked.
    pub watch_interval: Duration,
}

impl Default for CapturePolicy {
    fn default() -> Self {
        Self {
            cursor: true,
            border: BorderPolicy::default(),
            min_update_interval: None,
            output_size: OutputSize::default(),
            adapter: AdapterSelection::default(),
            publication: PublicationPreference::default(),
            arena: CaptureArena::default(),
            desktop: None,
            watch_interval: Duration::from_millis(100),
        }
    }
}

/// Where frames are published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationMode {
    D3d11 {
        adapter: AdapterInfo,
    },
    /// CPU publication, and why (fences unavailable, or requested).
    Cpu {
        adapter: AdapterInfo,
        reason: String,
    },
}

/// The arena a capture currently publishes into. Serve it with
/// [`super::setup::serve_d3d11`] or [`crate::acquisition::socket::serve_cpu`].
#[derive(Clone)]
pub enum Publication {
    D3d11(D3d11Producer),
    Cpu(Arc<Mutex<ArenaProducer>>),
}

impl fmt::Debug for Publication {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::D3d11(_) => "Publication::D3d11",
            Self::Cpu(_) => "Publication::Cpu",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureState {
    Running,
    /// The desktop cannot be seen; frames are dropped until it returns.
    Paused(DesktopUnavailable),
    /// The device was lost; a new one will be created (after the desktop
    /// returns, if it is also unavailable).
    Recovering {
        reason: String,
    },
    /// The item closed (its window went away). Terminal.
    Closed,
    /// The host stopped the capture. Terminal.
    Stopped,
    /// Terminal failure.
    Failed(String),
}

impl CaptureState {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Closed | Self::Stopped | Self::Failed(_))
    }
}

#[derive(Debug, Clone)]
pub struct CaptureStatus {
    pub state: CaptureState,
    /// Advances when device loss replaces the publication.
    pub epoch: u64,
    pub mode: PublicationMode,
    /// The current published size.
    pub size: (u32, u32),
    pub frames_published: u64,
    /// Frames not published: pool exhaustion, a pending reconfiguration, a
    /// paused desktop or an empty client area.
    pub frames_dropped: u64,
    /// Whether WGC draws its border (None until known).
    pub border_shown: Option<bool>,
    /// Policy settings the system could not apply.
    pub notes: Vec<String>,
    /// Increments on every status change; see [`WgcCapture::wait_for_change`].
    pub revision: u64,
}

struct Cpu {
    producer: Arc<Mutex<ArenaProducer>>,
    size: (u32, u32),
    pending: Option<(u32, u32)>,
    staging: Option<(u32, u32, ID3D11Texture2D)>,
    sequence: u64,
    dropped: u64,
    pending_drops: u32,
}

enum Publisher {
    D3d11 {
        producer: D3d11Producer,
        pending: Option<(u32, u32)>,
    },
    Cpu(Box<Cpu>),
}

fn stream_params(size: (u32, u32)) -> NativeStreamParams {
    NativeStreamParams {
        width: size.0,
        height: size.1,
        pixel_format: PixelFormat::Bgra8Unorm,
        color_space: ColorSpace::Srgb,
        clock_domain: ClockDomain::HostTime,
        modifier: 0,
    }
}

enum Staged {
    Published,
    Dropped,
}

impl Publisher {
    fn new(device: &Arc<D3d11Device>, policy: &CapturePolicy, size: (u32, u32)) -> Result<(Self, PublicationMode)> {
        let arena = policy.arena;
        let config = |payload_capacity| ArenaConfig {
            resource_capacity: arena.resource_capacity,
            retained_history: arena.retained_history,
            producer_reserve: arena.producer_reserve,
            payload_capacity,
            memory_budget: arena.memory_budget,
            max_incarnations: arena.max_incarnations,
            drain_timeout: arena.drain_timeout,
        };
        let cpu_reason = match policy.publication {
            PublicationPreference::Cpu => Some("CPU publication requested".to_owned()),
            PublicationPreference::D3d11 => device
                .probe_shared_fences()
                .err()
                .map(|error| format!("shared ID3D11Fence unavailable on adapter {}: {error}", device.luid())),
        };
        if let Some(reason) = cpu_reason {
            let producer = ArenaProducer::new(config(size.0 as usize * size.1 as usize * 4)).map_err(arena_failure)?;
            return Ok((
                Self::Cpu(Box::new(Cpu {
                    producer: Arc::new(Mutex::new(producer)),
                    size,
                    pending: None,
                    staging: None,
                    sequence: 0,
                    dropped: 0,
                    pending_drops: 0,
                })),
                PublicationMode::Cpu {
                    adapter: device.adapter().clone(),
                    reason,
                },
            ));
        }
        let producer =
            NativeArenaProducer::new(D3d11FrameBackend::new(Arc::clone(device)), stream_params(size), config(0)).map_err(arena_failure)?;
        Ok((
            Self::D3d11 {
                producer: Arc::new(Mutex::new(producer)),
                pending: None,
            },
            PublicationMode::D3d11 {
                adapter: device.adapter().clone(),
            },
        ))
    }

    fn publication(&self) -> Publication {
        match self {
            Self::D3d11 { producer, .. } => Publication::D3d11(Arc::clone(producer)),
            Self::Cpu(cpu) => Publication::Cpu(Arc::clone(&cpu.producer)),
        }
    }

    fn size(&self) -> (u32, u32) {
        match self {
            Self::D3d11 { producer, .. } => {
                let producer = lock(producer);
                (producer.params().width, producer.params().height)
            }
            Self::Cpu(cpu) => cpu.size,
        }
    }

    fn stop(&self) {
        match self {
            Self::D3d11 { producer, .. } => lock(producer).stop(),
            Self::Cpu(cpu) => lock(&cpu.producer).stop(),
        }
    }

    /// Bring the arena to `size`; true once frames of that size can publish.
    fn ready_for(&mut self, size: (u32, u32)) -> std::result::Result<bool, ArenaError> {
        match self {
            Self::D3d11 { producer, pending } => {
                let mut producer = lock(producer);
                loop {
                    // A replacement paused for capacity completes before a
                    // newer size can be proposed.
                    if pending.is_some() {
                        if !matches!(producer.advance_reconfiguration()?, ReconfigurationStatus::Ready { .. }) {
                            return Ok(false);
                        }
                        *pending = None;
                    }
                    if (producer.params().width, producer.params().height) == size {
                        return Ok(true);
                    }
                    if !matches!(producer.reconfigure(stream_params(size))?, ReconfigurationStatus::Ready { .. }) {
                        *pending = Some(size);
                        return Ok(false);
                    }
                }
            }
            Self::Cpu(cpu) => {
                let mut producer = lock(&cpu.producer);
                loop {
                    if let Some(target) = cpu.pending {
                        if !matches!(producer.advance_reconfiguration()?, ReconfigurationStatus::Ready { .. }) {
                            return Ok(false);
                        }
                        cpu.size = target;
                        cpu.pending = None;
                    }
                    if cpu.size == size {
                        return Ok(true);
                    }
                    match producer.reconfigure_cpu(size.0 as usize * size.1 as usize * 4)? {
                        ReconfigurationStatus::Ready { .. } => cpu.size = size,
                        _ => {
                            cpu.pending = Some(size);
                            return Ok(false);
                        }
                    }
                }
            }
        }
    }

    fn publish(&mut self, device: &D3d11Device, frame: &D3d11CapturedFrame, timestamp_ns: u64) -> std::result::Result<Staged, ArenaError> {
        if !self.ready_for(frame.size())? {
            if let Self::Cpu(cpu) = self {
                cpu.dropped += 1;
                cpu.pending_drops = cpu.pending_drops.saturating_add(1);
            }
            return Ok(Staged::Dropped);
        }
        match self {
            Self::D3d11 { producer, .. } => Ok(match lock(producer).publish(frame, timestamp_ns)? {
                PublishOutcome::Published { .. } => Staged::Published,
                PublishOutcome::Dropped => Staged::Dropped,
            }),
            Self::Cpu(cpu) => cpu.publish(device, frame, timestamp_ns),
        }
    }
}

impl Cpu {
    fn publish(&mut self, device: &D3d11Device, frame: &D3d11CapturedFrame, timestamp_ns: u64) -> std::result::Result<Staged, ArenaError> {
        let (width, height) = frame.size();
        if self.staging.as_ref().is_none_or(|(w, h, _)| (*w, *h) != (width, height)) {
            let desc = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..super::texture_desc(width, height, super::dxgi_format(PixelFormat::Bgra8Unorm)?, false)
            };
            let mut staging = None;
            // SAFETY: description and out pointer are live locals.
            unsafe { device.raw().CreateTexture2D(&desc, None, Some(&mut staging)) }.map_err(|error| failure("cpu-staging", error))?;
            self.staging = Some((width, height, staging.ok_or_else(|| failure("cpu-staging", "no texture"))?));
        }
        let staging = &self.staging.as_ref().expect("created above").2;
        let row = width as usize * 4;
        let mut pixels = vec![0; row * height as usize];
        {
            let context = device.context();
            let region = D3D11_BOX {
                left: frame.left,
                top: frame.top,
                front: 0,
                right: frame.left + width,
                bottom: frame.top + height,
                back: 1,
            };
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            // SAFETY: both textures belong to this device; the context is
            // locked; the blocking map returns once the copy has executed.
            unsafe {
                context.CopySubresourceRegion(staging, 0, 0, 0, 0, &frame.texture, 0, Some(&region));
                context
                    .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .map_err(|error| failure("cpu-map", error))?;
                for (y, target) in pixels.chunks_exact_mut(row).enumerate() {
                    target.copy_from_slice(std::slice::from_raw_parts(
                        mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize),
                        row,
                    ));
                }
                context.Unmap(staging, 0);
            }
        }
        self.sequence += 1;
        let descriptor = FrameDescriptor {
            sequence: self.sequence,
            timestamp_ns,
            width,
            height,
            stride: width * 4,
            pixel_format: PixelFormat::Bgra8Unorm as u32,
            color_space: ColorSpace::Srgb as u32,
            clock_domain: ClockDomain::HostTime as u32,
            payload_kind: PayloadKind::CpuShm as u32,
            sync_kind: FrameSyncKind::CpuCopyComplete as u32,
            damage_kind: DamageKind::FullFrame as u32,
            damage_base_sequence: self.sequence,
            dropped_before_publish: self.pending_drops,
            producer_drop_count: self.dropped,
            ..FrameDescriptor::default()
        };
        Ok(match lock(&self.producer).publish(descriptor, &pixels)? {
            PublishOutcome::Published { .. } => {
                self.pending_drops = 0;
                Staged::Published
            }
            PublishOutcome::Dropped => {
                self.dropped += 1;
                self.pending_drops = self.pending_drops.saturating_add(1);
                Staged::Dropped
            }
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn arena_failure(error: ArenaError) -> crate::CaptureTransferError {
    match error {
        ArenaError::Storage(error) => error,
        error => failure("capture-arena", error),
    }
}

/// One device, frame pool, session and publication.
struct Engine {
    id: u64,
    device: Arc<D3d11Device>,
    winrt_device: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_token: i64,
    content_size: SizeInt32,
    publisher: Publisher,
    scaler: Option<scale::Scaler>,
}

// SAFETY: WinRT capture objects are agile; D3D11 objects are free-threaded
// and their context use is serialized by the capture lock (see D3d11Device).
unsafe impl Send for Engine {}

impl Engine {
    fn close(self) {
        let _ = self.pool.RemoveFrameArrived(self.frame_token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
        self.publisher.stop();
    }
}

struct Inner {
    target: CaptureTarget,
    policy: CapturePolicy,
    engine: Option<Engine>,
    publication: Option<Publication>,
    status: CaptureStatus,
    closed_token: Option<i64>,
    lost: Option<String>,
    recovery_attempts: u32,
}

// SAFETY: as Engine; the item is agile and the HWND is only passed to Win32.
unsafe impl Send for Inner {}

struct Shared {
    inner: Mutex<Inner>,
    changed: Condvar,
    stop: AtomicBool,
    closed: AtomicBool,
    next_engine: AtomicU64,
}

impl Shared {
    fn update(&self, inner: &mut Inner, change: impl FnOnce(&mut CaptureStatus)) {
        change(&mut inner.status);
        inner.status.revision += 1;
        self.changed.notify_all();
    }
}

/// A running capture; see the module docs. Dropping it stops the capture.
pub struct WgcCapture {
    shared: Arc<Shared>,
    watcher: Option<JoinHandle<()>>,
}

impl fmt::Debug for WgcCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WgcCapture").field("status", &self.status()).finish_non_exhaustive()
    }
}

fn timestamp_ns(frame: &Direct3D11CaptureFrame) -> u64 {
    // SystemRelativeTime counts 100 ns QPC-derived ticks.
    frame
        .SystemRelativeTime()
        .map_or(0, |time: TimeSpan| u64::try_from(time.Duration).unwrap_or(0).saturating_mul(100))
}

fn frame_texture(frame: &Direct3D11CaptureFrame) -> ::windows::core::Result<ID3D11Texture2D> {
    let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
    // SAFETY: the frame's surface is a live DXGI surface.
    unsafe { access.GetInterface() }
}

/// The region to publish and the output size and placement for it.
fn plan(policy: OutputSize, region: (u32, u32, u32, u32)) -> ((u32, u32), Option<scale::Placement>) {
    let (width, height) = (region.2, region.3);
    let fit = |max_width: u32, max_height: u32| {
        let scale = (f64::from(max_width) / f64::from(width)).min(f64::from(max_height) / f64::from(height));
        (
            ((f64::from(width) * scale).round() as u32).clamp(1, max_width),
            ((f64::from(height) * scale).round() as u32).clamp(1, max_height),
        )
    };
    match policy {
        OutputSize::Source => ((width, height), None),
        OutputSize::Fit { max_width, max_height } => {
            if width <= max_width && height <= max_height {
                ((width, height), None)
            } else {
                let size = fit(max_width.max(1), max_height.max(1));
                (
                    size,
                    Some(scale::Placement {
                        left: 0.0,
                        top: 0.0,
                        width: size.0 as f32,
                        height: size.1 as f32,
                    }),
                )
            }
        }
        OutputSize::Fixed {
            width: out_width,
            height: out_height,
        } => {
            let out = (out_width.max(1), out_height.max(1));
            if (width, height) == out {
                return (out, None);
            }
            let inner = fit(out.0, out.1);
            (
                out,
                Some(scale::Placement {
                    left: ((out.0 - inner.0) / 2) as f32,
                    top: ((out.1 - inner.1) / 2) as f32,
                    width: inner.0 as f32,
                    height: inner.1 as f32,
                }),
            )
        }
    }
}

impl WgcCapture {
    /// Start capturing. Fails if WGC is unsupported, the device cannot be
    /// created, or [`BorderPolicy::RequireHidden`] cannot be met.
    pub fn start(target: CaptureTarget, policy: CapturePolicy) -> Result<Self> {
        if !is_supported() {
            return Err(failure("capture-start", "Windows.Graphics.Capture is not supported"));
        }
        if policy.arena.resource_capacity > super::MAX_POOL_SLOTS {
            return Err(failure("capture-start", "resource capacity exceeds the D3D11 pool limit"));
        }
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                target,
                policy,
                engine: None,
                publication: None,
                status: CaptureStatus {
                    state: CaptureState::Running,
                    epoch: 0,
                    mode: PublicationMode::Cpu {
                        adapter: AdapterInfo {
                            luid: super::AdapterLuid(0),
                            description: String::new(),
                            vendor_id: 0,
                            device_id: 0,
                            software: false,
                        },
                        reason: "not started".to_owned(),
                    },
                    size: (0, 0),
                    frames_published: 0,
                    frames_dropped: 0,
                    border_shown: None,
                    notes: Vec::new(),
                    revision: 0,
                },
                closed_token: None,
                lost: None,
                recovery_attempts: 0,
            }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            next_engine: AtomicU64::new(1),
        });
        {
            let mut inner = lock(&shared.inner);
            let weak = Arc::downgrade(&shared);
            let token = inner
                .target
                .item
                .Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                    if let Some(shared) = weak.upgrade() {
                        shared.closed.store(true, Ordering::SeqCst);
                        shared.changed.notify_all();
                    }
                    Ok(())
                }))
                .map_err(|error| failure("capture-closed-event", error))?;
            inner.closed_token = Some(token);
            let engine = Self::create_engine(&shared, &mut inner)?;
            Self::install(&shared, &mut inner, engine);
        }
        let watcher = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("jackstay-wgc-watch".to_owned())
                .spawn(move || watch(&shared))
                .map_err(|error| failure("capture-watch", error))?
        };
        Ok(Self {
            shared,
            watcher: Some(watcher),
        })
    }

    fn create_engine(shared: &Arc<Shared>, inner: &mut Inner) -> Result<Engine> {
        let policy = &inner.policy;
        let device = Arc::new(D3d11Device::new(policy.adapter)?);
        let dxgi: IDXGIDevice = device.raw().cast().map_err(|error| failure("capture-device", error))?;
        // SAFETY: plain interop wrapping of a live DXGI device.
        let winrt_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .and_then(|device| device.cast())
            .map_err(|error| failure("capture-device", error))?;
        let item = &inner.target.item;
        let content_size = item.Size().map_err(|error| failure("capture-item-size", error))?;
        let pool =
            Direct3D11CaptureFramePool::CreateFreeThreaded(&winrt_device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, content_size)
                .map_err(|error| failure("capture-frame-pool", error))?;
        let session = pool.CreateCaptureSession(item).map_err(|error| failure("capture-session", error))?;
        let mut notes = Vec::new();
        if let Err(error) = session.SetIsCursorCaptureEnabled(policy.cursor) {
            notes.push(format!("cursor capture setting unavailable: {error}"));
        }
        let border_shown = match policy.border {
            BorderPolicy::Show => true,
            BorderPolicy::PreferHidden | BorderPolicy::RequireHidden => {
                let allowed = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)
                    .and_then(|operation| operation.join())
                    .is_ok_and(|status| status == AppCapabilityAccessStatus::Allowed);
                let hidden = allowed && session.SetIsBorderRequired(false).is_ok();
                if !hidden {
                    if policy.border == BorderPolicy::RequireHidden {
                        let _ = session.Close();
                        let _ = pool.Close();
                        return Err(failure("capture-border", "borderless capture was refused"));
                    }
                    notes.push("borderless capture unavailable; the border is shown".to_owned());
                }
                !hidden
            }
        };
        if let Some(interval) = policy.min_update_interval {
            let ticks = i64::try_from(interval.as_nanos() / 100).unwrap_or(i64::MAX);
            if let Err(error) = session.SetMinUpdateInterval(TimeSpan { Duration: ticks }) {
                notes.push(format!("minimum update interval unavailable: {error}"));
            }
        }
        let region = window::client_region(
            inner.target.window,
            content_size.Width.max(1) as u32,
            content_size.Height.max(1) as u32,
        );
        let (size, _) = plan(policy.output_size, region);
        let (publisher, mode) = Publisher::new(&device, policy, size)?;
        let id = shared.next_engine.fetch_add(1, Ordering::Relaxed);
        let weak: Weak<Shared> = Arc::downgrade(shared);
        let frame_token = pool
            .FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
                move |pool, _| {
                    let Some(pool) = pool.as_ref() else {
                        return Ok(());
                    };
                    // Drain to the newest frame; older buffers go straight back.
                    let mut latest = None;
                    while let Ok(frame) = pool.TryGetNextFrame() {
                        if let Some(previous) = latest.replace(frame) {
                            let _ = Direct3D11CaptureFrame::Close(&previous);
                        }
                    }
                    if let (Some(frame), Some(shared)) = (latest, weak.upgrade()) {
                        on_frame(&shared, id, &frame);
                        let _ = frame.Close();
                    }
                    Ok(())
                },
            ))
            .map_err(|error| failure("capture-frame-event", error))?;
        session.StartCapture().map_err(|error| failure("capture-start", error))?;
        inner.status.notes = notes;
        inner.status.border_shown = Some(border_shown);
        inner.status.mode = mode;
        inner.status.size = size;
        Ok(Engine {
            id,
            device,
            winrt_device,
            pool,
            session,
            frame_token,
            content_size,
            publisher,
            scaler: None,
        })
    }

    fn install(shared: &Shared, inner: &mut Inner, engine: Engine) {
        inner.publication = Some(engine.publisher.publication());
        inner.engine = Some(engine);
        shared.update(inner, |status| {
            status.epoch += 1;
            status.state = CaptureState::Running;
        });
    }

    #[must_use]
    pub fn status(&self) -> CaptureStatus {
        lock(&self.shared.inner).status.clone()
    }

    /// The current publication and its epoch. After device loss a new epoch
    /// replaces it; the old one is stopped and drains as consumers leave.
    #[must_use]
    pub fn publication(&self) -> Option<(u64, Publication)> {
        let inner = lock(&self.shared.inner);
        inner.publication.clone().map(|publication| (inner.status.epoch, publication))
    }

    /// Wait until the status revision passes `revision`, or the timeout.
    #[must_use]
    pub fn wait_for_change(&self, revision: u64, timeout: Duration) -> CaptureStatus {
        let inner = lock(&self.shared.inner);
        let (inner, _) = self
            .shared
            .changed
            .wait_timeout_while(inner, timeout, |inner| inner.status.revision <= revision)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.status.clone()
    }

    /// Treat the device as removed now. Exercises the recovery a host sees
    /// after a real removal (for example an RDP reconnect), without removing
    /// the device.
    pub fn simulate_device_loss(&self, reason: &str) {
        lock(&self.shared.inner).lost = Some(reason.to_owned());
        self.shared.changed.notify_all();
    }

    /// Stop capturing and publication. Consumers keep held frames; hosts
    /// drain each publication with its `poll_shutdown_ready`.
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.changed.notify_all();
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        let (engine, token, item) = {
            let mut inner = lock(&self.shared.inner);
            let engine = inner.engine.take();
            if let Some(engine) = &engine {
                engine.publisher.stop();
            }
            let token = inner.closed_token.take();
            if !inner.status.state.is_terminal() {
                self.shared.update(&mut inner, |status| status.state = CaptureState::Stopped);
            }
            (engine, token, inner.target.item.clone())
        };
        if let Some(token) = token {
            let _ = item.RemoveClosed(token);
        }
        if let Some(engine) = engine {
            engine.close();
        }
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn on_frame(shared: &Shared, engine_id: u64, frame: &Direct3D11CaptureFrame) {
    let mut guard = lock(&shared.inner);
    let inner = &mut *guard;
    if inner.status.state != CaptureState::Running || inner.lost.is_some() {
        if inner.engine.as_ref().is_some_and(|engine| engine.id == engine_id) {
            shared.update(inner, |status| status.frames_dropped += 1);
        }
        return;
    }
    let Some(engine) = inner.engine.as_mut().filter(|engine| engine.id == engine_id) else {
        return;
    };
    let result = (|| -> Result<Staged> {
        let content = frame.ContentSize().map_err(|error| failure("capture-frame", error))?;
        let texture = frame_texture(frame).map_err(|error| failure("capture-frame", error))?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: GetDesc fills a local structure.
        unsafe { texture.GetDesc(&mut desc) };
        if content.Width != engine.content_size.Width || content.Height != engine.content_size.Height {
            // The next frames arrive at the new size; this one still fits
            // its old buffer.
            engine
                .pool
                .Recreate(&engine.winrt_device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, content)
                .map_err(|error| failure("capture-frame-pool", error))?;
            engine.content_size = content;
        }
        let visible = (
            (content.Width.max(0) as u32).min(desc.Width),
            (content.Height.max(0) as u32).min(desc.Height),
        );
        if visible.0 == 0 || visible.1 == 0 {
            return Ok(Staged::Dropped);
        }
        let region = window::client_region(inner.target.window, visible.0, visible.1);
        if region.2 == 0 || region.3 == 0 {
            return Ok(Staged::Dropped);
        }
        let (size, placement) = plan(inner.policy.output_size, region);
        let source = match placement {
            None => D3d11CapturedFrame::cropped(texture, region.0, region.1, region.2, region.3)?,
            Some(placement) => {
                if engine.scaler.is_none() {
                    engine.scaler = Some(scale::Scaler::new(&engine.device)?);
                }
                let scaler = engine.scaler.as_mut().expect("created above");
                D3d11CapturedFrame::new(scaler.render(&engine.device, &texture, region, size, placement)?)
            }
        };
        engine
            .publisher
            .publish(&engine.device, &source, timestamp_ns(frame))
            .map_err(arena_failure)
    })();
    match result {
        Ok(Staged::Published) => {
            let size = engine.publisher.size();
            shared.update(inner, |status| {
                status.frames_published += 1;
                status.size = size;
            });
        }
        Ok(Staged::Dropped) => shared.update(inner, |status| status.frames_dropped += 1),
        Err(error) => {
            if let Some(reason) = engine.device.removed_reason() {
                inner.lost = Some(format!("device removed: {reason}"));
                shared.changed.notify_all();
            } else {
                let message = error.to_string();
                engine.publisher.stop();
                shared.update(inner, |status| status.state = CaptureState::Failed(message));
                shared.changed.notify_all();
            }
        }
    }
}

/// Lock/disconnect, device removal, recovery and item closure.
fn watch(shared: &Arc<Shared>) {
    let (desktop, interval) = {
        let inner = lock(&shared.inner);
        (
            inner.policy.desktop.clone().unwrap_or_else(|| Arc::new(SessionDesktop)),
            inner.policy.watch_interval,
        )
    };
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let unavailable = desktop.unavailable();
        let mut retired = None;
        {
            let mut guard = lock(&shared.inner);
            let inner = &mut *guard;
            if inner.status.state.is_terminal() {
                retired = inner.engine.take();
                drop(guard);
                if let Some(engine) = retired {
                    engine.close();
                }
                return;
            }
            // WGC's Closed event was not observed for a destroyed window on
            // Beaufort (#28), so a window target is also checked directly.
            let window_gone = inner
                .target
                .window
                // SAFETY: IsWindow accepts any value, including stale handles.
                .is_some_and(|window| !unsafe { ::windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(window)) }.as_bool());
            if shared.closed.load(Ordering::SeqCst) || window_gone {
                let source = if shared.closed.load(Ordering::SeqCst) {
                    "item Closed event"
                } else {
                    "window destroyed (IsWindow)"
                };
                retired = inner.engine.take();
                // Consumers see the terminal state no later than the status.
                if let Some(engine) = &retired {
                    engine.publisher.stop();
                }
                shared.update(inner, |status| {
                    status.state = CaptureState::Closed;
                    status.notes.push(format!("closed: {source}"));
                });
            } else {
                if inner.lost.is_none()
                    && let Some(reason) = inner.engine.as_ref().and_then(|engine| engine.device.removed_reason())
                {
                    inner.lost = Some(format!("device removed: {reason}"));
                }
                match (&inner.lost, unavailable) {
                    (_, Some(reason)) => {
                        if inner.status.state != CaptureState::Paused(reason) {
                            shared.update(inner, |status| status.state = CaptureState::Paused(reason));
                        }
                    }
                    (Some(reason), None) if !matches!(inner.status.state, CaptureState::Recovering { .. }) => {
                        let reason = reason.clone();
                        shared.update(inner, |status| status.state = CaptureState::Recovering { reason });
                        retired = inner.engine.take();
                    }
                    (Some(_), None) => {}
                    (None, None) => {
                        if inner.status.state != CaptureState::Running {
                            shared.update(inner, |status| status.state = CaptureState::Running);
                        }
                    }
                }
            }
        }
        // WGC objects close outside the capture lock: an in-flight frame
        // callback may be waiting for it.
        if let Some(engine) = retired {
            engine.close();
        }
        {
            let mut guard = lock(&shared.inner);
            let inner = &mut *guard;
            if matches!(inner.status.state, CaptureState::Recovering { .. }) && desktop.unavailable().is_none() {
                match WgcCapture::create_engine(shared, inner) {
                    Ok(engine) => {
                        inner.lost = None;
                        inner.recovery_attempts = 0;
                        WgcCapture::install(shared, inner, engine);
                    }
                    // Adapters can be in flux just after a reconnect: retry
                    // for a while before giving up.
                    Err(error) if inner.recovery_attempts < RECOVERY_ATTEMPTS => {
                        inner.recovery_attempts += 1;
                        let reason = format!("device recovery attempt {} failed: {error}", inner.recovery_attempts);
                        shared.update(inner, |status| status.state = CaptureState::Recovering { reason });
                    }
                    Err(error) => {
                        let message = format!("device recovery failed: {error}");
                        shared.update(inner, |status| status.state = CaptureState::Failed(message));
                    }
                }
            }
            if inner.status.state.is_terminal() {
                continue;
            }
            let _ = shared
                .changed
                .wait_timeout_while(guard, interval, |inner| {
                    // Wake early for a new loss; retry recovery at the interval.
                    !shared.stop.load(Ordering::SeqCst)
                        && !shared.closed.load(Ordering::SeqCst)
                        && (inner.lost.is_none() || matches!(inner.status.state, CaptureState::Recovering { .. }))
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}
