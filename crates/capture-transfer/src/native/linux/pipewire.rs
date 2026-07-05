//! Narrow PipeWire runtime probe for the Linux dmabuf producer path.
//!
//! Full capture still comes from a portal-created PipeWire ScreenCast stream.
//! This module keeps the first integration boundary small: prove PipeWire can
//! initialize, expose a loop object, and agree on the SPA constants the dmabuf
//! producer will need for buffers, sync objects, modifiers, and formats.

use std::{
    collections::{BTreeMap, HashMap},
    ffi::CStr,
    io,
    mem::size_of,
    os::{
        fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd},
        raw::{c_char, c_void},
    },
    path::Path,
    ptr::NonNull,
    sync::{Arc, Mutex},
};

use crate::{
    error::{CaptureTransferError, Result},
    model::{PayloadKind, PixelFormat},
    native::{
        NativeFrameBackend, NativeStreamParams, NativeTrackProducer, PoolExhaustionPolicy, SlotClaim, SlotReuseCandidate,
        lease::{NativeLeaseBook, NativeLeaseIdentity, NativeLeaseRelease},
        linux::{
            LinuxDmabufPlaneHandle, LinuxNativeLeaseBackend, LinuxSurfaceHandle, LinuxSyncDescriptor, LinuxSyncHandle,
            dmabuf::FT_NATIVE_HANDLE_DMABUF,
            drm::{DrmDevice, DrmSyncobjTimeline, FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE},
            vulkan,
        },
    },
};

const fn fourcc_code(a: u8, b: u8, c: u8, d: u8) -> u32 {
    u32::from_le_bytes([a, b, c, d])
}

pub const DRM_FORMAT_RGBA8888: u32 = fourcc_code(b'R', b'A', b'2', b'4');
pub const DRM_FORMAT_BGRA8888: u32 = fourcc_code(b'B', b'A', b'2', b'4');
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct PortholeNativeLinuxPipewireProbe {
    struct_size: u32,
    can_init: u32,
    can_create_thread_loop: u32,
    spa_data_dmabuf: u32,
    spa_data_syncobj: u32,
    spa_meta_header: u32,
    spa_meta_video_damage: u32,
    spa_video_format_bgra: u32,
    spa_video_format_rgba: u32,
    spa_video_max_planes: u32,
    spa_format_video_modifier: u32,
    library_version: *const c_char,
}

const PIPEWIRE_MAX_PLANES: usize = 4;
const PIPEWIRE_MAX_MODIFIERS: usize = 64;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct PortholeNativeLinuxPipewirePlane {
    fd: i64,
    offset: u32,
    stride: i32,
    maxsize: u32,
    data_type: u32,
}

impl Default for PortholeNativeLinuxPipewirePlane {
    fn default() -> Self {
        Self {
            fd: -1,
            offset: 0,
            stride: 0,
            maxsize: 0,
            data_type: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct PortholeNativeLinuxPipewireBuffer {
    struct_size: u32,
    plane_count: u32,
    has_header: u32,
    header_flags: u32,
    header_pts: i64,
    header_sequence: u64,
    planes: [PortholeNativeLinuxPipewirePlane; PIPEWIRE_MAX_PLANES],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct PortholeNativeLinuxPipewireStreamDesc {
    struct_size: u32,
    remote_fd: i32,
    node_id: u32,
    object_serial: u64,
    modifier_count: u32,
    modifiers: [u64; PIPEWIRE_MAX_MODIFIERS],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct PortholeNativeLinuxPipewireStreamConfig {
    struct_size: u32,
    width: u32,
    height: u32,
    spa_format: u32,
    flags: u32,
    modifier: u64,
}

#[repr(C)]
struct PortholeNativeLinuxPipewireStream {
    _private: [u8; 0],
}

type PipeWireConfigChangedCallback = unsafe extern "C" fn(*mut c_void, *const PortholeNativeLinuxPipewireStreamConfig);
type PipeWireBufferAddedCallback = unsafe extern "C" fn(*mut c_void, u32, *const c_void);
type PipeWireBufferRemovedCallback = unsafe extern "C" fn(*mut c_void, u32);
type PipeWireFrameCallback = unsafe extern "C" fn(*mut c_void, u32, u64, *const c_void);

#[repr(C)]
#[derive(Clone, Copy)]
struct PortholeNativeLinuxPipewireStreamCallbacks {
    struct_size: u32,
    user_data: *mut c_void,
    config_changed: Option<PipeWireConfigChangedCallback>,
    buffer_added: Option<PipeWireBufferAddedCallback>,
    buffer_removed: Option<PipeWireBufferRemovedCallback>,
    frame_ready: Option<PipeWireFrameCallback>,
}

impl Default for PortholeNativeLinuxPipewireBuffer {
    fn default() -> Self {
        Self {
            struct_size: size_of::<Self>() as u32,
            plane_count: 0,
            has_header: 0,
            header_flags: 0,
            header_pts: 0,
            header_sequence: 0,
            planes: [PortholeNativeLinuxPipewirePlane::default(); PIPEWIRE_MAX_PLANES],
        }
    }
}

impl Default for PortholeNativeLinuxPipewireProbe {
    fn default() -> Self {
        Self {
            struct_size: size_of::<Self>() as u32,
            can_init: 0,
            can_create_thread_loop: 0,
            spa_data_dmabuf: 0,
            spa_data_syncobj: 0,
            spa_meta_header: 0,
            spa_meta_video_damage: 0,
            spa_video_format_bgra: 0,
            spa_video_format_rgba: 0,
            spa_video_max_planes: 0,
            spa_format_video_modifier: 0,
            library_version: std::ptr::null(),
        }
    }
}

mod ffi {
    use std::os::raw::c_void;

    use super::{PortholeNativeLinuxPipewireBuffer, PortholeNativeLinuxPipewireProbe, PortholeNativeLinuxPipewireStreamCallbacks};

    unsafe extern "C" {
        pub fn porthole_native_linux_pipewire_probe(out: *mut PortholeNativeLinuxPipewireProbe) -> i32;
        pub fn porthole_native_linux_pipewire_describe_buffer(buffer: *const c_void, out: *mut PortholeNativeLinuxPipewireBuffer) -> i32;
        pub fn porthole_native_linux_pipewire_stream_open(
            desc: *const super::PortholeNativeLinuxPipewireStreamDesc,
            callbacks: *const PortholeNativeLinuxPipewireStreamCallbacks,
            out: *mut *mut super::PortholeNativeLinuxPipewireStream,
        ) -> i32;
        pub fn porthole_native_linux_pipewire_stream_close(stream: *mut super::PortholeNativeLinuxPipewireStream);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeWireRuntimeProbe {
    pub can_init: bool,
    pub can_create_thread_loop: bool,
    pub spa_data_dmabuf: u32,
    pub spa_data_syncobj: u32,
    pub spa_meta_header: u32,
    pub spa_meta_video_damage: u32,
    pub spa_video_format_bgra: u32,
    pub spa_video_format_rgba: u32,
    pub spa_video_max_planes: u32,
    pub spa_format_video_modifier: u32,
    pub library_version: Option<String>,
}

impl PipeWireRuntimeProbe {
    pub fn probe() -> Result<Self> {
        let mut raw = PortholeNativeLinuxPipewireProbe::default();
        let errno = unsafe { ffi::porthole_native_linux_pipewire_probe(&mut raw) };
        if errno != 0 {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-pipewire-probe",
                message: io::Error::from_raw_os_error(errno).to_string(),
            });
        }
        let library_version = if raw.library_version.is_null() {
            None
        } else {
            Some(unsafe { CStr::from_ptr(raw.library_version) }.to_string_lossy().into_owned())
        };
        Ok(Self {
            can_init: raw.can_init != 0,
            can_create_thread_loop: raw.can_create_thread_loop != 0,
            spa_data_dmabuf: raw.spa_data_dmabuf,
            spa_data_syncobj: raw.spa_data_syncobj,
            spa_meta_header: raw.spa_meta_header,
            spa_meta_video_damage: raw.spa_meta_video_damage,
            spa_video_format_bgra: raw.spa_video_format_bgra,
            spa_video_format_rgba: raw.spa_video_format_rgba,
            spa_video_max_planes: raw.spa_video_max_planes,
            spa_format_video_modifier: raw.spa_format_video_modifier,
            library_version,
        })
    }

    #[must_use]
    pub fn supports_dmabuf_producer_primitives(&self) -> bool {
        self.can_init
            && self.can_create_thread_loop
            && self.spa_data_dmabuf != 0
            && self.spa_data_syncobj != 0
            && self.spa_video_max_planes >= 4
            && self.spa_format_video_modifier != 0
    }

    #[must_use]
    pub fn spa_format_for_pixel_format(&self, pixel_format: PixelFormat) -> Option<u32> {
        match pixel_format {
            PixelFormat::Bgra8Unorm => Some(self.spa_video_format_bgra),
            PixelFormat::Rgba8Unorm => Some(self.spa_video_format_rgba),
            PixelFormat::Unknown => None,
        }
    }

    #[must_use]
    pub fn pixel_format_for_spa_format(&self, spa_format: u32) -> Option<PixelFormat> {
        if spa_format == self.spa_video_format_bgra {
            Some(PixelFormat::Bgra8Unorm)
        } else if spa_format == self.spa_video_format_rgba {
            Some(PixelFormat::Rgba8Unorm)
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub struct PipeWireDmabufPlane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

#[derive(Debug)]
pub struct PipeWireDmabufFrame {
    pub width: u32,
    pub height: u32,
    pub spa_format: u32,
    pub modifier: u64,
    pub planes: Vec<PipeWireDmabufPlane>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeWireBufferPlaneDescriptor {
    pub fd: RawFd,
    pub offset: u32,
    pub stride: u32,
    pub maxsize: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeWireBufferDescriptor {
    pub planes: Vec<PipeWireBufferPlaneDescriptor>,
    pub header: Option<PipeWireBufferHeader>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeWireBufferHeader {
    pub flags: u32,
    pub pts: i64,
    pub sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeWireStreamConfig {
    pub width: u32,
    pub height: u32,
    pub spa_format: u32,
    pub flags: u32,
    pub modifier: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeWireStreamTarget {
    pub node_id: u32,
    pub object_serial: Option<u64>,
}

pub trait PipeWireStreamObserver: Send + 'static {
    fn config_changed(&mut self, _config: PipeWireStreamConfig) {}
    fn buffer_added(&mut self, slot_id: u32, descriptor: Result<PipeWireBufferDescriptor>);
    fn buffer_removed(&mut self, slot_id: u32);
    fn frame_ready(&mut self, slot_id: u32, stream_time_ns: u64, descriptor: Result<PipeWireBufferDescriptor>);
}

pub type SharedPipeWireNativeProducer = Arc<Mutex<NativeTrackProducer<PipeWireNativeBackend>>>;

#[derive(Debug, Clone)]
pub struct PipeWireNativeProducerHandle {
    inner: Arc<Mutex<PipeWireNativeProducerState>>,
}

#[derive(Debug)]
pub struct PipeWireNativeProducerObserver {
    inner: Arc<Mutex<PipeWireNativeProducerState>>,
}

#[derive(Debug)]
struct PipeWireNativeProducerState {
    backend: Option<PipeWireNativeBackend>,
    config: Option<PipeWireStreamConfig>,
    buffers: BTreeMap<u32, PipeWireDmabufFrame>,
    slot_map: BTreeMap<u32, u32>,
    producer: Option<SharedPipeWireNativeProducer>,
    reconfiguring: bool,
    ring_capacity: usize,
    exhaustion_policy: PoolExhaustionPolicy,
    last_error: Option<String>,
}

impl PipeWireNativeProducerObserver {
    #[must_use]
    pub fn new(
        backend: PipeWireNativeBackend,
        ring_capacity: usize,
        exhaustion_policy: PoolExhaustionPolicy,
    ) -> (Self, PipeWireNativeProducerHandle) {
        let inner = Arc::new(Mutex::new(PipeWireNativeProducerState {
            backend: Some(backend),
            config: None,
            buffers: BTreeMap::new(),
            slot_map: BTreeMap::new(),
            producer: None,
            reconfiguring: false,
            ring_capacity,
            exhaustion_policy,
            last_error: None,
        }));
        (Self { inner: Arc::clone(&inner) }, PipeWireNativeProducerHandle { inner })
    }

    fn record_error(state: &mut PipeWireNativeProducerState, error: impl std::fmt::Display) {
        state.last_error = Some(error.to_string());
    }

    fn drain_pending_pool(state: &mut PipeWireNativeProducerState) -> (Vec<PipeWireDmabufFrame>, BTreeMap<u32, u32>) {
        let pipewire_slots = state.buffers.keys().copied().collect::<Vec<_>>();
        let mut frames = Vec::with_capacity(pipewire_slots.len());
        let mut slot_map = BTreeMap::new();
        for (native_slot, pipewire_slot) in pipewire_slots.into_iter().enumerate() {
            frames.push(
                state
                    .buffers
                    .remove(&pipewire_slot)
                    .expect("PipeWire buffer key came from the pending buffer map"),
            );
            slot_map.insert(pipewire_slot, native_slot as u32);
        }
        (frames, slot_map)
    }

    fn params_for_config(backend: &PipeWireNativeBackend, config: PipeWireStreamConfig) -> Result<NativeStreamParams> {
        let pixel_format =
            backend
                .probe()
                .pixel_format_for_spa_format(config.spa_format)
                .ok_or_else(|| CaptureTransferError::NativeBackend {
                    operation: "linux-pipewire-build-producer",
                    message: format!("unsupported negotiated SPA format {}", config.spa_format),
                })?;
        Ok(NativeStreamParams {
            width: config.width,
            height: config.height,
            pixel_format,
            color_space: crate::model::ColorSpace::Srgb,
            clock_domain: crate::model::ClockDomain::MediaTime,
            modifier: config.modifier,
        })
    }

    fn try_build_producer(state: &mut PipeWireNativeProducerState) -> Result<Option<SharedPipeWireNativeProducer>> {
        if let Some(producer) = &state.producer {
            return Ok(Some(Arc::clone(producer)));
        }
        let Some(config) = state.config else {
            return Ok(None);
        };
        if state.buffers.is_empty() {
            return Ok(None);
        }
        let (frames, slot_map) = Self::drain_pending_pool(state);
        let Some(mut backend) = state.backend.take() else {
            return Ok(None);
        };
        let params = Self::params_for_config(&backend, config)?;
        let pool = backend.allocate_pool_from_frames(frames)?;
        let pool_slot_count = pool.slot_count();
        let fence = backend.create_fence()?;
        let producer = NativeTrackProducer::from_allocated_parts(
            backend,
            params,
            state.ring_capacity,
            pool_slot_count,
            pool,
            fence,
            state.exhaustion_policy,
        )?;
        let producer = Arc::new(Mutex::new(producer));
        state.slot_map = slot_map;
        state.producer = Some(Arc::clone(&producer));
        Ok(Some(producer))
    }

    fn try_reconfigure_producer(state: &mut PipeWireNativeProducerState) -> Result<Option<SharedPipeWireNativeProducer>> {
        if !state.reconfiguring {
            return Ok(None);
        }
        let Some(config) = state.config else {
            return Ok(None);
        };
        let Some(producer) = state.producer.as_ref().map(Arc::clone) else {
            return Ok(None);
        };
        if state.buffers.is_empty() {
            let params = {
                let mut producer = producer.lock().expect("pipewire native producer poisoned");
                Self::params_for_config(producer.backend_mut(), config)?
            };
            let current_params = producer.lock().expect("pipewire native producer poisoned").params().clone();
            if params == current_params {
                state.reconfiguring = false;
                return Ok(Some(producer));
            }
            return Ok(None);
        }
        let (frames, slot_map) = Self::drain_pending_pool(state);
        {
            let mut producer = producer.lock().expect("pipewire native producer poisoned");
            let params = Self::params_for_config(producer.backend_mut(), config)?;
            let pool = producer.backend_mut().allocate_pool_from_frames(frames)?;
            let pool_slot_count = pool.slot_count();
            producer.replace_allocated_pool(params, pool_slot_count, pool)?;
        }
        state.slot_map = slot_map;
        state.reconfiguring = false;
        Ok(Some(producer))
    }
}

impl PipeWireNativeProducerHandle {
    pub fn producer(&self) -> Option<SharedPipeWireNativeProducer> {
        self.inner
            .lock()
            .expect("pipewire native producer state poisoned")
            .producer
            .as_ref()
            .map(Arc::clone)
    }

    pub fn last_error(&self) -> Option<String> {
        self.inner
            .lock()
            .expect("pipewire native producer state poisoned")
            .last_error
            .clone()
    }
}

impl PipeWireStreamObserver for PipeWireNativeProducerObserver {
    fn config_changed(&mut self, config: PipeWireStreamConfig) {
        let mut state = self.inner.lock().expect("pipewire native producer state poisoned");
        state.config = Some(config);
        if state.producer.is_some() {
            state.buffers.clear();
            state.reconfiguring = true;
        }
    }

    fn buffer_added(&mut self, slot_id: u32, descriptor: Result<PipeWireBufferDescriptor>) {
        let mut state = self.inner.lock().expect("pipewire native producer state poisoned");
        let Some(config) = state.config else {
            Self::record_error(&mut state, "PipeWire buffer arrived before stream config");
            return;
        };
        match descriptor.and_then(|descriptor| descriptor.to_owned_frame(config.width, config.height, config.spa_format, config.modifier)) {
            Ok(frame) => {
                if state.producer.is_none() || state.reconfiguring {
                    state.buffers.insert(slot_id, frame);
                }
            }
            Err(error) => Self::record_error(&mut state, error),
        }
    }

    fn buffer_removed(&mut self, slot_id: u32) {
        let mut state = self.inner.lock().expect("pipewire native producer state poisoned");
        if state.producer.is_some() {
            state.reconfiguring = true;
        }
        if state.producer.is_none() || state.reconfiguring {
            state.buffers.remove(&slot_id);
            state.slot_map.remove(&slot_id);
        }
    }

    fn frame_ready(&mut self, slot_id: u32, stream_time_ns: u64, descriptor: Result<PipeWireBufferDescriptor>) {
        let mut state = self.inner.lock().expect("pipewire native producer state poisoned");
        if let Err(error) = descriptor {
            Self::record_error(&mut state, error);
            return;
        }
        if state.reconfiguring {
            match Self::try_reconfigure_producer(&mut state) {
                Ok(Some(_)) => {}
                Ok(None) => return,
                Err(error) => {
                    Self::record_error(&mut state, error);
                    return;
                }
            }
        }
        let producer = match Self::try_build_producer(&mut state) {
            Ok(Some(producer)) => producer,
            Ok(None) => return,
            Err(error) => {
                Self::record_error(&mut state, error);
                return;
            }
        };
        let Some(native_slot_id) = state.slot_map.get(&slot_id).copied() else {
            Self::record_error(&mut state, format!("PipeWire frame arrived for unknown slot {slot_id}"));
            return;
        };
        drop(state);
        if let Err(error) = producer
            .lock()
            .expect("pipewire native producer poisoned")
            .publish(&PipeWireNativeFrame { slot_id: native_slot_id }, stream_time_ns)
        {
            if let Ok(mut state) = self.inner.lock() {
                Self::record_error(&mut state, error);
            }
        }
    }
}

struct PipeWireStreamObserverState {
    observer: Mutex<Box<dyn PipeWireStreamObserver>>,
}

pub struct PipeWireStream {
    raw: NonNull<PortholeNativeLinuxPipewireStream>,
    _observer: Option<Box<PipeWireStreamObserverState>>,
}

// The stream handle owns an independent PipeWire thread loop and is not used
// through shared references; moving it between threads only changes which
// thread eventually drops/closes the handle.
unsafe impl Send for PipeWireStream {}

impl std::fmt::Debug for PipeWireStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeWireStream").finish_non_exhaustive()
    }
}

impl PipeWireStream {
    pub fn open_remote(remote: impl AsFd, target: PipeWireStreamTarget) -> Result<Self> {
        Self::open_remote_fd(remote.as_fd().as_raw_fd(), target, None)
    }

    pub fn open_remote_with_observer(
        remote: impl AsFd,
        target: PipeWireStreamTarget,
        observer: Box<dyn PipeWireStreamObserver>,
    ) -> Result<Self> {
        Self::open_remote_fd(remote.as_fd().as_raw_fd(), target, Some(observer))
    }

    fn open_remote_fd(remote_fd: RawFd, target: PipeWireStreamTarget, observer: Option<Box<dyn PipeWireStreamObserver>>) -> Result<Self> {
        let modifiers = supported_pipewire_modifiers();
        let desc = PortholeNativeLinuxPipewireStreamDesc {
            struct_size: size_of::<PortholeNativeLinuxPipewireStreamDesc>() as u32,
            remote_fd,
            node_id: target.node_id,
            object_serial: target.object_serial.unwrap_or(0),
            modifier_count: modifiers.len() as u32,
            modifiers: modifier_array(&modifiers),
        };
        let mut observer_state = observer.map(|observer| {
            Box::new(PipeWireStreamObserverState {
                observer: Mutex::new(observer),
            })
        });
        let callbacks = observer_state.as_mut().map(|state| PortholeNativeLinuxPipewireStreamCallbacks {
            struct_size: size_of::<PortholeNativeLinuxPipewireStreamCallbacks>() as u32,
            user_data: (&mut **state as *mut PipeWireStreamObserverState).cast(),
            config_changed: Some(pipewire_config_changed),
            buffer_added: Some(pipewire_buffer_added),
            buffer_removed: Some(pipewire_buffer_removed),
            frame_ready: Some(pipewire_frame_ready),
        });
        let mut raw = std::ptr::null_mut();
        let errno = unsafe {
            ffi::porthole_native_linux_pipewire_stream_open(
                &desc,
                callbacks.as_ref().map_or(std::ptr::null(), |callbacks| {
                    callbacks as *const PortholeNativeLinuxPipewireStreamCallbacks
                }),
                &mut raw,
            )
        };
        if errno != 0 {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-pipewire-stream-open",
                message: io::Error::from_raw_os_error(errno).to_string(),
            });
        }
        let raw = NonNull::new(raw).ok_or_else(|| CaptureTransferError::NativeBackend {
            operation: "linux-pipewire-stream-open",
            message: "PipeWire stream open returned a null handle".to_string(),
        })?;
        Ok(Self {
            raw,
            _observer: observer_state,
        })
    }
}

fn supported_pipewire_modifiers() -> Vec<u64> {
    let mut modifiers = vulkan::supported_format_modifiers(PixelFormat::Bgra8Unorm, PIPEWIRE_MAX_MODIFIERS).unwrap_or_else(|_| Vec::new());
    if !modifiers.contains(&DRM_FORMAT_MOD_LINEAR) {
        modifiers.push(DRM_FORMAT_MOD_LINEAR);
    }
    modifiers.truncate(PIPEWIRE_MAX_MODIFIERS);
    modifiers
}

fn modifier_array(modifiers: &[u64]) -> [u64; PIPEWIRE_MAX_MODIFIERS] {
    let mut out = [0; PIPEWIRE_MAX_MODIFIERS];
    let copy_len = modifiers.len().min(PIPEWIRE_MAX_MODIFIERS);
    out[..copy_len].copy_from_slice(&modifiers[..copy_len]);
    out
}

impl Drop for PipeWireStream {
    fn drop(&mut self) {
        unsafe { ffi::porthole_native_linux_pipewire_stream_close(self.raw.as_ptr()) };
    }
}

unsafe extern "C" fn pipewire_config_changed(user_data: *mut c_void, config: *const PortholeNativeLinuxPipewireStreamConfig) {
    if config.is_null() {
        return;
    }
    let config = unsafe { *config };
    if config.struct_size < size_of::<PortholeNativeLinuxPipewireStreamConfig>() as u32 {
        return;
    }
    with_pipewire_observer(user_data, |observer| {
        observer.config_changed(PipeWireStreamConfig {
            width: config.width,
            height: config.height,
            spa_format: config.spa_format,
            flags: config.flags,
            modifier: config.modifier,
        });
    });
}

unsafe extern "C" fn pipewire_buffer_added(user_data: *mut c_void, slot_id: u32, buffer: *const c_void) {
    with_pipewire_observer(user_data, |observer| {
        observer.buffer_added(slot_id, unsafe { PipeWireBufferDescriptor::describe_spa_buffer(buffer) });
    });
}

unsafe extern "C" fn pipewire_buffer_removed(user_data: *mut c_void, slot_id: u32) {
    with_pipewire_observer(user_data, |observer| {
        observer.buffer_removed(slot_id);
    });
}

unsafe extern "C" fn pipewire_frame_ready(user_data: *mut c_void, slot_id: u32, stream_time_ns: u64, buffer: *const c_void) {
    with_pipewire_observer(user_data, |observer| {
        observer.frame_ready(slot_id, stream_time_ns, unsafe {
            PipeWireBufferDescriptor::describe_spa_buffer(buffer)
        });
    });
}

fn with_pipewire_observer(user_data: *mut c_void, call: impl FnOnce(&mut dyn PipeWireStreamObserver)) {
    if user_data.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let state = unsafe { &*(user_data.cast::<PipeWireStreamObserverState>()) };
        if let Ok(mut observer) = state.observer.lock() {
            call(observer.as_mut());
        }
    }));
}

impl PipeWireBufferDescriptor {
    /// Describes a borrowed `struct spa_buffer *` delivered by PipeWire.
    ///
    /// The returned file descriptors are still owned by PipeWire. Call
    /// [`Self::to_owned_frame`] inside the callback if the frame descriptor must
    /// outlive the callback.
    ///
    /// # Safety
    ///
    /// `buffer` must be either null or a valid pointer to a live `struct
    /// spa_buffer` for the duration of this call.
    pub unsafe fn describe_spa_buffer(buffer: *const c_void) -> Result<Self> {
        let mut raw = PortholeNativeLinuxPipewireBuffer::default();
        let errno = unsafe { ffi::porthole_native_linux_pipewire_describe_buffer(buffer, &mut raw) };
        if errno != 0 {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-pipewire-describe-buffer",
                message: io::Error::from_raw_os_error(errno).to_string(),
            });
        }
        if raw.plane_count == 0 || raw.plane_count as usize > raw.planes.len() {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-pipewire-describe-buffer",
                message: format!("invalid plane count {}", raw.plane_count),
            });
        }

        let mut planes = Vec::with_capacity(raw.plane_count as usize);
        for plane in raw.planes.iter().take(raw.plane_count as usize) {
            if plane.fd < 0 || plane.fd > i32::MAX as i64 {
                return Err(CaptureTransferError::NativeBackend {
                    operation: "linux-pipewire-describe-buffer",
                    message: format!("invalid dmabuf fd {}", plane.fd),
                });
            }
            if plane.stride <= 0 {
                return Err(CaptureTransferError::NativeBackend {
                    operation: "linux-pipewire-describe-buffer",
                    message: format!("invalid dmabuf stride {}", plane.stride),
                });
            }
            planes.push(PipeWireBufferPlaneDescriptor {
                fd: plane.fd as RawFd,
                offset: plane.offset,
                stride: plane.stride as u32,
                maxsize: plane.maxsize,
            });
        }

        let header = (raw.has_header != 0).then_some(PipeWireBufferHeader {
            flags: raw.header_flags,
            pts: raw.header_pts,
            sequence: raw.header_sequence,
        });
        Ok(Self { planes, header })
    }

    pub fn to_owned_frame(&self, width: u32, height: u32, spa_format: u32, modifier: u64) -> Result<PipeWireDmabufFrame> {
        let mut planes = Vec::with_capacity(self.planes.len());
        for plane in &self.planes {
            planes.push(PipeWireDmabufPlane {
                fd: dup_fd(plane.fd, "linux-pipewire-dup-dmabuf")?,
                offset: plane.offset,
                stride: plane.stride,
            });
        }
        Ok(PipeWireDmabufFrame {
            width,
            height,
            spa_format,
            modifier,
            planes,
        })
    }
}

fn dup_fd(fd: RawFd, operation: &'static str) -> Result<OwnedFd> {
    let duped = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duped < 0 {
        return Err(CaptureTransferError::NativeBackend {
            operation,
            message: io::Error::last_os_error().to_string(),
        });
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duped) })
}

impl PipeWireDmabufFrame {
    pub fn into_surface_handle(self, probe: &PipeWireRuntimeProbe) -> Result<LinuxSurfaceHandle> {
        if self.width == 0 || self.height == 0 {
            return Err(CaptureTransferError::NativeBackend {
                operation: "pipewire-dmabuf-frame-descriptor",
                message: format!("invalid frame dimensions {}x{}", self.width, self.height),
            });
        }
        if self.planes.is_empty() || self.planes.len() > probe.spa_video_max_planes as usize {
            return Err(CaptureTransferError::NativeBackend {
                operation: "pipewire-dmabuf-frame-descriptor",
                message: format!(
                    "dmabuf plane count {} outside 1..={}",
                    self.planes.len(),
                    probe.spa_video_max_planes
                ),
            });
        }
        let Some(pixel_format) = probe.pixel_format_for_spa_format(self.spa_format) else {
            return Err(CaptureTransferError::NativeBackend {
                operation: "pipewire-dmabuf-frame-descriptor",
                message: format!("unsupported SPA video format {}", self.spa_format),
            });
        };

        let mut planes = Vec::with_capacity(self.planes.len());
        for plane in self.planes {
            if plane.stride == 0 {
                return Err(CaptureTransferError::NativeBackend {
                    operation: "pipewire-dmabuf-frame-descriptor",
                    message: "dmabuf plane stride must be non-zero".to_string(),
                });
            }
            planes.push(LinuxDmabufPlaneHandle {
                fd: plane.fd,
                offset: plane.offset,
                stride: plane.stride,
            });
        }
        Ok(LinuxSurfaceHandle {
            handle_kind: FT_NATIVE_HANDLE_DMABUF,
            width: self.width,
            height: self.height,
            pixel_format: pixel_format as u32,
            modifier: self.modifier,
            planes,
        })
    }
}

#[derive(Debug)]
pub struct PipeWireNativeBackend {
    drm: DrmDevice,
    probe: PipeWireRuntimeProbe,
    next_pool_id: u64,
    next_fence_id: u64,
    lease_book: NativeLeaseBook,
    release_syncs: HashMap<u64, DrmSyncobjTimeline>,
}

#[derive(Debug)]
pub struct PipeWireNativePool {
    pool_id: u64,
    surfaces: Vec<LinuxSurfaceHandle>,
    reuse_blocked: Vec<bool>,
}

#[derive(Debug)]
pub struct PipeWireNativeFrame {
    pub slot_id: u32,
}

#[derive(Debug)]
pub struct PipeWireNativeFence {
    fence_id: u64,
    timeline: DrmSyncobjTimeline,
}

impl PipeWireNativeBackend {
    pub fn open(drm_render_path: impl AsRef<Path>) -> Result<Self> {
        let probe = PipeWireRuntimeProbe::probe()?;
        if !probe.supports_dmabuf_producer_primitives() {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-pipewire-open",
                message: "PipeWire runtime does not expose required dmabuf producer primitives".to_string(),
            });
        }
        let drm = DrmDevice::open(drm_render_path)?;
        if !drm.supports_syncobj_timeline()? {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-pipewire-drm-syncobj-timeline-cap",
                message: "DRM device does not support syncobj timelines".to_string(),
            });
        }
        Ok(Self {
            drm,
            probe,
            next_pool_id: 1,
            next_fence_id: 1,
            lease_book: NativeLeaseBook::new(),
            release_syncs: HashMap::new(),
        })
    }

    pub fn allocate_pool_from_frames(&mut self, frames: Vec<PipeWireDmabufFrame>) -> Result<PipeWireNativePool> {
        if frames.is_empty() {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-pipewire-allocate-pool",
                message: "PipeWire pool must contain at least one dmabuf frame".to_string(),
            });
        }
        let pool_id = self.next_pool_id;
        self.next_pool_id = self.next_pool_id.saturating_add(1);
        let surfaces = frames
            .into_iter()
            .map(|frame| frame.into_surface_handle(&self.probe))
            .collect::<Result<Vec<_>>>()?;
        let surface_count = surfaces.len();
        Ok(PipeWireNativePool {
            pool_id,
            surfaces,
            reuse_blocked: vec![false; surface_count],
        })
    }

    #[must_use]
    pub fn probe(&self) -> &PipeWireRuntimeProbe {
        &self.probe
    }

    fn refresh_release_syncs(&mut self) -> Result<()> {
        for (release_sync_id, timeline) in &self.release_syncs {
            let reached_value = timeline.query(0)?;
            self.lease_book
                .complete_release_sync(*release_sync_id, reached_value)
                .map_err(CaptureTransferError::from)?;
        }
        Ok(())
    }
}

impl PipeWireNativePool {
    #[must_use]
    pub fn pool_id(&self) -> u64 {
        self.pool_id
    }

    #[must_use]
    pub fn slot_count(&self) -> u32 {
        self.surfaces.len() as u32
    }

    fn claim_reusable_slot(&self, candidates: &[SlotReuseCandidate]) -> SlotClaim {
        candidates
            .iter()
            .find(|candidate| !self.reuse_blocked[candidate.slot_id as usize])
            .map(|candidate| SlotClaim::Ready {
                slot_id: candidate.slot_id,
            })
            .unwrap_or(SlotClaim::WouldBlock)
    }

    fn set_reuse_blocked(&mut self, slot_id: u32, blocked: bool) -> Result<()> {
        let Some(slot) = self.reuse_blocked.get_mut(slot_id as usize) else {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-pipewire-set-reuse-blocked",
                message: format!("slot {slot_id} outside pool with {} slots", self.surfaces.len()),
            });
        };
        *slot = blocked;
        Ok(())
    }
}

impl NativeFrameBackend for PipeWireNativeBackend {
    type CapturedFrame = PipeWireNativeFrame;
    type SurfacePool = PipeWireNativePool;
    type Fence = PipeWireNativeFence;
    type SurfaceHandle = LinuxSurfaceHandle;
    type SyncHandle = LinuxSyncHandle;

    fn payload_kind(&self) -> PayloadKind {
        PayloadKind::DmaBuf
    }

    fn allocate_surface_pool(&mut self, _params: &NativeStreamParams, _slot_count: u32) -> Result<Self::SurfacePool> {
        Err(CaptureTransferError::NativeBackend {
            operation: "linux-pipewire-allocate-surface-pool",
            message: "PipeWire pools are negotiated by the compositor; use allocate_pool_from_frames".to_string(),
        })
    }

    fn pool_id(&self, pool: &Self::SurfacePool) -> u64 {
        pool.pool_id
    }

    fn claim_reusable_slot(&mut self, pool: &mut Self::SurfacePool, candidates: &[SlotReuseCandidate]) -> Result<SlotClaim> {
        self.refresh_release_syncs()?;
        for candidate in candidates {
            pool.set_reuse_blocked(
                candidate.slot_id,
                self.lease_book.slot_has_unresolved_leases(pool.pool_id, candidate.slot_id),
            )?;
        }
        Ok(pool.claim_reusable_slot(candidates))
    }

    fn stage_frame(&mut self, _pool: &mut Self::SurfacePool, slot_id: u32, frame: &Self::CapturedFrame) -> Result<()> {
        if slot_id != frame.slot_id {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-pipewire-stage-frame",
                message: format!("PipeWire delivered slot {}, but producer selected slot {slot_id}", frame.slot_id),
            });
        }
        Ok(())
    }

    fn frame_slot_hint(&self, frame: &Self::CapturedFrame) -> Option<u32> {
        Some(frame.slot_id)
    }

    fn export_surface_handles(&self, pool: &Self::SurfacePool) -> Result<Vec<Self::SurfaceHandle>> {
        pool.surfaces.iter().map(clone_linux_surface_handle).collect()
    }

    fn create_fence(&mut self) -> Result<Self::Fence> {
        let fence_id = self.next_fence_id;
        self.next_fence_id = self.next_fence_id.saturating_add(1);
        Ok(PipeWireNativeFence {
            fence_id,
            timeline: self.drm.create_syncobj_timeline()?,
        })
    }

    fn fence_id(&self, fence: &Self::Fence) -> u64 {
        fence.fence_id
    }

    fn signal_fence(&mut self, fence: &mut Self::Fence, value: u64) -> Result<()> {
        fence.timeline.signal(value)
    }

    fn export_sync_handle(&self, fence: &Self::Fence) -> Result<Self::SyncHandle> {
        fence.timeline.export_handle(fence.fence_id)
    }
}

impl LinuxNativeLeaseBackend for PipeWireNativeBackend {
    fn acquire_linux_lease(&mut self, identity: NativeLeaseIdentity) -> Result<u64> {
        Ok(self.lease_book.acquire(identity))
    }

    fn register_linux_release_sync(&mut self, sync: LinuxSyncDescriptor, fd: OwnedFd) -> Result<u64> {
        if sync.sync_kind != FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-pipewire-register-release-sync",
                message: format!("unsupported release sync kind {}", sync.sync_kind),
            });
        }
        let timeline = self.drm.import_syncobj_timeline_fd(fd.as_raw_fd())?;
        let release_sync_id = self.lease_book.register_release_sync();
        self.release_syncs.insert(release_sync_id, timeline);
        Ok(release_sync_id)
    }

    fn release_linux_lease(&mut self, lease_id: u64, release: NativeLeaseRelease) -> Result<()> {
        self.lease_book
            .release(lease_id, release)
            .map(|_| ())
            .map_err(CaptureTransferError::from)
    }
}

fn clone_linux_surface_handle(surface: &LinuxSurfaceHandle) -> Result<LinuxSurfaceHandle> {
    let mut planes = Vec::with_capacity(surface.planes.len());
    for plane in &surface.planes {
        planes.push(LinuxDmabufPlaneHandle {
            fd: plane
                .fd
                .as_fd()
                .try_clone_to_owned()
                .map_err(|error| CaptureTransferError::FdPassing {
                    operation: "clone-pipewire-dmabuf-plane",
                    message: error.to_string(),
                })?,
            offset: plane.offset,
            stride: plane.stride,
        });
    }
    Ok(LinuxSurfaceHandle {
        handle_kind: surface.handle_kind,
        width: surface.width,
        height: surface.height,
        pixel_format: surface.pixel_format,
        modifier: surface.modifier,
        planes,
    })
}

#[must_use]
pub fn drm_fourcc_for_pixel_format(pixel_format: PixelFormat) -> Option<u32> {
    match pixel_format {
        PixelFormat::Bgra8Unorm => Some(DRM_FORMAT_BGRA8888),
        PixelFormat::Rgba8Unorm => Some(DRM_FORMAT_RGBA8888),
        PixelFormat::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        os::fd::{AsRawFd, OwnedFd, RawFd},
        sync::{Arc, Mutex},
    };

    use super::{
        DRM_FORMAT_BGRA8888, DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_RGBA8888, PipeWireBufferDescriptor, PipeWireBufferPlaneDescriptor,
        PipeWireDmabufFrame, PipeWireDmabufPlane, PipeWireNativeBackend, PipeWireNativeFrame, PipeWireNativeProducerObserver,
        PipeWireRuntimeProbe, PipeWireStream, PipeWireStreamConfig, PipeWireStreamObserver, PipeWireStreamObserverState,
        PipeWireStreamTarget, PortholeNativeLinuxPipewireStreamConfig, drm_fourcc_for_pixel_format, pipewire_buffer_added,
        pipewire_buffer_removed, pipewire_config_changed, pipewire_frame_ready, supported_pipewire_modifiers,
    };
    use crate::{
        CaptureTransferError,
        model::{ClockDomain, ColorSpace, PixelFormat},
        native::{NativeFrameBackend, NativeStreamParams, NativeTrackProducer, PoolExhaustionPolicy},
    };

    #[test]
    fn pipewire_runtime_probe_reports_dmabuf_producer_primitives() {
        let probe = PipeWireRuntimeProbe::probe().unwrap();
        assert!(probe.can_init);
        assert!(probe.can_create_thread_loop);
        assert!(probe.library_version.as_deref().is_some_and(|version| !version.is_empty()));
        assert_eq!(probe.spa_data_dmabuf, 3);
        assert_eq!(probe.spa_data_syncobj, 5);
        assert!(probe.spa_meta_header != 0);
        assert!(probe.spa_meta_video_damage != 0);
        assert_eq!(probe.spa_video_max_planes, 4);
        assert!(probe.spa_format_video_modifier != 0);
        assert_eq!(
            probe.spa_format_for_pixel_format(PixelFormat::Bgra8Unorm),
            Some(probe.spa_video_format_bgra)
        );
        assert_eq!(
            probe.spa_format_for_pixel_format(PixelFormat::Rgba8Unorm),
            Some(probe.spa_video_format_rgba)
        );
        assert_eq!(probe.spa_format_for_pixel_format(PixelFormat::Unknown), None);
        assert_eq!(
            probe.pixel_format_for_spa_format(probe.spa_video_format_bgra),
            Some(PixelFormat::Bgra8Unorm)
        );
        assert_eq!(
            probe.pixel_format_for_spa_format(probe.spa_video_format_rgba),
            Some(PixelFormat::Rgba8Unorm)
        );
        assert_eq!(probe.pixel_format_for_spa_format(u32::MAX), None);
        assert!(probe.supports_dmabuf_producer_primitives());
    }

    #[test]
    fn pipewire_modifier_offer_includes_linear_fallback() {
        let modifiers = supported_pipewire_modifiers();
        assert!(!modifiers.is_empty());
        assert!(modifiers.len() <= super::PIPEWIRE_MAX_MODIFIERS);
        assert!(modifiers.contains(&DRM_FORMAT_MOD_LINEAR));
        for (index, modifier) in modifiers.iter().enumerate() {
            assert_eq!(modifiers.iter().position(|candidate| candidate == modifier), Some(index));
        }
    }

    #[test]
    fn pipewire_stream_open_reports_invalid_remote_fd() {
        let error = PipeWireStream::open_remote_fd(
            -1,
            PipeWireStreamTarget {
                node_id: 7,
                object_serial: Some(99),
            },
            None,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CaptureTransferError::NativeBackend {
                operation: "linux-pipewire-stream-open",
                ..
            }
        ));
    }

    #[test]
    fn pipewire_stream_callbacks_describe_buffers_by_slot() {
        #[derive(Default)]
        struct Observed {
            configs: Vec<PipeWireStreamConfig>,
            added: Vec<(u32, usize)>,
            removed: Vec<u32>,
            frames: Vec<(u32, u64, usize)>,
        }

        struct Observer {
            observed: Arc<Mutex<Observed>>,
        }

        impl PipeWireStreamObserver for Observer {
            fn config_changed(&mut self, config: PipeWireStreamConfig) {
                self.observed.lock().unwrap().configs.push(config);
            }

            fn buffer_added(&mut self, slot_id: u32, descriptor: crate::Result<PipeWireBufferDescriptor>) {
                self.observed
                    .lock()
                    .unwrap()
                    .added
                    .push((slot_id, descriptor.unwrap().planes.len()));
            }

            fn buffer_removed(&mut self, slot_id: u32) {
                self.observed.lock().unwrap().removed.push(slot_id);
            }

            fn frame_ready(&mut self, slot_id: u32, stream_time_ns: u64, descriptor: crate::Result<PipeWireBufferDescriptor>) {
                self.observed
                    .lock()
                    .unwrap()
                    .frames
                    .push((slot_id, stream_time_ns, descriptor.unwrap().planes.len()));
            }
        }

        let probe = PipeWireRuntimeProbe::probe().unwrap();
        let fd = tempfile_fd();
        let mut chunk = SpaChunk {
            offset: 0,
            size: 4096,
            stride: 512,
            flags: 0,
        };
        let mut data = [SpaData {
            data_type: probe.spa_data_dmabuf,
            flags: 0,
            fd: fd.as_raw_fd() as i64,
            mapoffset: 0,
            maxsize: 4096,
            data: std::ptr::null_mut(),
            chunk: &mut chunk,
        }];
        let buffer = SpaBuffer {
            n_metas: 0,
            n_datas: data.len() as u32,
            metas: std::ptr::null_mut(),
            datas: data.as_mut_ptr(),
        };
        let observed = Arc::new(Mutex::new(Observed::default()));
        let state = PipeWireStreamObserverState {
            observer: Mutex::new(Box::new(Observer {
                observed: Arc::clone(&observed),
            })),
        };
        let user_data = (&state as *const PipeWireStreamObserverState).cast_mut().cast();

        unsafe {
            pipewire_config_changed(
                user_data,
                &PortholeNativeLinuxPipewireStreamConfig {
                    struct_size: size_of::<PortholeNativeLinuxPipewireStreamConfig>() as u32,
                    width: 64,
                    height: 32,
                    spa_format: probe.spa_video_format_bgra,
                    flags: 1,
                    modifier: DRM_FORMAT_MOD_LINEAR,
                },
            );
            pipewire_buffer_added(user_data, 3, (&buffer as *const SpaBuffer).cast());
            pipewire_frame_ready(user_data, 3, 1234, (&buffer as *const SpaBuffer).cast());
            pipewire_buffer_removed(user_data, 3);
        }

        let observed = observed.lock().unwrap();
        assert_eq!(
            observed.configs,
            vec![PipeWireStreamConfig {
                width: 64,
                height: 32,
                spa_format: probe.spa_video_format_bgra,
                flags: 1,
                modifier: DRM_FORMAT_MOD_LINEAR,
            }]
        );
        assert_eq!(observed.added, vec![(3, 1)]);
        assert_eq!(observed.frames, vec![(3, 1234, 1)]);
        assert_eq!(observed.removed, vec![3]);
    }

    #[test]
    fn maps_pipewire_dmabuf_frame_to_linux_surface_handle() {
        let probe = PipeWireRuntimeProbe::probe().unwrap();
        let frame = PipeWireDmabufFrame {
            width: 64,
            height: 32,
            spa_format: probe.spa_video_format_bgra,
            modifier: DRM_FORMAT_MOD_LINEAR,
            planes: vec![PipeWireDmabufPlane {
                fd: tempfile_fd(),
                offset: 128,
                stride: 256,
            }],
        };

        let surface = frame.into_surface_handle(&probe).unwrap();
        assert_eq!(surface.handle_kind, 2);
        assert_eq!(surface.width, 64);
        assert_eq!(surface.height, 32);
        assert_eq!(surface.pixel_format, PixelFormat::Bgra8Unorm as u32);
        assert_eq!(surface.modifier, DRM_FORMAT_MOD_LINEAR);
        assert_eq!(surface.planes.len(), 1);
        assert_eq!(surface.planes[0].offset, 128);
        assert_eq!(surface.planes[0].stride, 256);
    }

    #[test]
    fn rejects_invalid_pipewire_dmabuf_descriptors() {
        let probe = PipeWireRuntimeProbe::probe().unwrap();

        assert!(
            PipeWireDmabufFrame {
                width: 0,
                height: 32,
                spa_format: probe.spa_video_format_bgra,
                modifier: 0,
                planes: vec![plane(256)],
            }
            .into_surface_handle(&probe)
            .is_err()
        );

        assert!(
            PipeWireDmabufFrame {
                width: 64,
                height: 32,
                spa_format: u32::MAX,
                modifier: 0,
                planes: vec![plane(256)],
            }
            .into_surface_handle(&probe)
            .is_err()
        );

        assert!(
            PipeWireDmabufFrame {
                width: 64,
                height: 32,
                spa_format: probe.spa_video_format_bgra,
                modifier: 0,
                planes: Vec::new(),
            }
            .into_surface_handle(&probe)
            .is_err()
        );

        assert!(
            PipeWireDmabufFrame {
                width: 64,
                height: 32,
                spa_format: probe.spa_video_format_bgra,
                modifier: 0,
                planes: vec![plane(1), plane(1), plane(1), plane(1), plane(1)],
            }
            .into_surface_handle(&probe)
            .is_err()
        );

        assert!(
            PipeWireDmabufFrame {
                width: 64,
                height: 32,
                spa_format: probe.spa_video_format_bgra,
                modifier: 0,
                planes: vec![plane(0)],
            }
            .into_surface_handle(&probe)
            .is_err()
        );
    }

    #[test]
    fn maps_jackstay_pixel_formats_to_drm_fourcc() {
        assert_eq!(drm_fourcc_for_pixel_format(PixelFormat::Bgra8Unorm), Some(DRM_FORMAT_BGRA8888));
        assert_eq!(drm_fourcc_for_pixel_format(PixelFormat::Rgba8Unorm), Some(DRM_FORMAT_RGBA8888));
        assert_eq!(drm_fourcc_for_pixel_format(PixelFormat::Unknown), None);
    }

    #[test]
    fn pipewire_native_backend_publishes_the_delivered_buffer_slot() {
        let mut backend = match PipeWireNativeBackend::open("/dev/dri/renderD128") {
            Ok(backend) => backend,
            Err(CaptureTransferError::NativeBackend {
                operation: "linux-drm-open" | "linux-pipewire-drm-syncobj-timeline-cap",
                message,
            }) if message.contains("Permission denied") || message.contains("No such file") => return,
            Err(error) => panic!("unexpected PipeWire native backend open error: {error}"),
        };
        let spa_format = backend.probe().spa_video_format_bgra;
        let pool = backend
            .allocate_pool_from_frames(vec![
                PipeWireDmabufFrame {
                    width: 64,
                    height: 32,
                    spa_format,
                    modifier: DRM_FORMAT_MOD_LINEAR,
                    planes: vec![plane(256)],
                },
                PipeWireDmabufFrame {
                    width: 64,
                    height: 32,
                    spa_format,
                    modifier: DRM_FORMAT_MOD_LINEAR,
                    planes: vec![plane(256)],
                },
            ])
            .unwrap();
        assert_eq!(pool.pool_id(), 1);
        assert_eq!(pool.slot_count(), 2);
        let fence = backend.create_fence().unwrap();
        let mut producer = NativeTrackProducer::from_allocated_parts(
            backend,
            NativeStreamParams {
                width: 64,
                height: 32,
                pixel_format: PixelFormat::Bgra8Unorm,
                color_space: ColorSpace::Srgb,
                clock_domain: ClockDomain::MediaTime,
                modifier: DRM_FORMAT_MOD_LINEAR,
            },
            1,
            pool.slot_count(),
            pool,
            fence,
            PoolExhaustionPolicy::DropFrame,
        )
        .unwrap();

        let outcome = producer.publish(&PipeWireNativeFrame { slot_id: 1 }, 1234).unwrap();
        let cursor = outcome.cursor().expect("frame should publish");
        let entry = producer.control_page().read_entry_for_cursor(cursor).unwrap();
        assert_eq!(entry.slot_id, 1);
        assert_eq!(entry.pool_id, 1);

        let grant = producer.grant_attach(7).unwrap();
        assert_eq!(grant.pool_id, 1);
        assert_eq!(grant.pool_slot_count, 2);
        assert_eq!(grant.surface_handles.len(), 2);
        assert_eq!(grant.surface_handles[1].planes[0].stride, 256);
    }

    #[test]
    fn pipewire_native_producer_observer_builds_pool_and_publishes_frames() {
        let backend = match PipeWireNativeBackend::open("/dev/dri/renderD128") {
            Ok(backend) => backend,
            Err(CaptureTransferError::NativeBackend {
                operation: "linux-drm-open" | "linux-pipewire-drm-syncobj-timeline-cap",
                message,
            }) if message.contains("Permission denied") || message.contains("No such file") => return,
            Err(error) => panic!("unexpected PipeWire native backend open error: {error}"),
        };
        let spa_format = backend.probe().spa_video_format_bgra;
        let (mut observer, handle) = PipeWireNativeProducerObserver::new(backend, 1, PoolExhaustionPolicy::DropFrame);
        observer.config_changed(PipeWireStreamConfig {
            width: 64,
            height: 32,
            spa_format,
            flags: 0,
            modifier: DRM_FORMAT_MOD_LINEAR,
        });
        let fd0 = tempfile_fd();
        let fd1 = tempfile_fd();
        let descriptor0 = PipeWireBufferDescriptor {
            planes: vec![PipeWireBufferPlaneDescriptor {
                fd: fd0.as_raw_fd(),
                offset: 0,
                stride: 256,
                maxsize: 8192,
            }],
            header: None,
        };
        let descriptor1 = PipeWireBufferDescriptor {
            planes: vec![PipeWireBufferPlaneDescriptor {
                fd: fd1.as_raw_fd(),
                offset: 0,
                stride: 256,
                maxsize: 8192,
            }],
            header: None,
        };
        observer.buffer_added(0, Ok(descriptor0));
        observer.buffer_added(1, Ok(descriptor1.clone()));
        observer.frame_ready(1, 1234, Ok(descriptor1));

        let producer = handle.producer().expect("observer should build producer on first frame");
        let producer = producer.lock().unwrap();
        let cursor = producer.control_page().latest_cursor().unwrap();
        let entry = producer.control_page().read_entry_for_cursor(cursor).unwrap();
        assert_eq!(entry.slot_id, 1);
        assert_eq!(entry.width, 64);
        assert_eq!(entry.height, 32);
        assert_eq!(entry.timestamp_ns, 1234);
        assert!(
            handle.last_error().is_none(),
            "unexpected observer error: {:?}",
            handle.last_error()
        );
    }

    #[test]
    fn pipewire_native_producer_observer_reconfigures_existing_producer_pool() {
        let backend = match PipeWireNativeBackend::open("/dev/dri/renderD128") {
            Ok(backend) => backend,
            Err(CaptureTransferError::NativeBackend {
                operation: "linux-drm-open" | "linux-pipewire-drm-syncobj-timeline-cap",
                message,
            }) if message.contains("Permission denied") || message.contains("No such file") => return,
            Err(error) => panic!("unexpected PipeWire native backend open error: {error}"),
        };
        let spa_format = backend.probe().spa_video_format_bgra;
        let (mut observer, handle) = PipeWireNativeProducerObserver::new(backend, 1, PoolExhaustionPolicy::DropFrame);
        observer.config_changed(PipeWireStreamConfig {
            width: 64,
            height: 32,
            spa_format,
            flags: 0,
            modifier: DRM_FORMAT_MOD_LINEAR,
        });
        let fd0 = tempfile_fd();
        let fd1 = tempfile_fd();
        observer.buffer_added(0, Ok(descriptor(fd0.as_raw_fd(), 256)));
        observer.buffer_added(1, Ok(descriptor(fd1.as_raw_fd(), 256)));
        observer.frame_ready(1, 1234, Ok(descriptor(fd1.as_raw_fd(), 256)));

        let producer = handle.producer().expect("observer should build producer on first frame");
        let first = {
            let producer = producer.lock().unwrap();
            let cursor = producer.control_page().latest_cursor().unwrap();
            producer.control_page().read_entry_for_cursor(cursor).unwrap()
        };
        assert_eq!(first.pool_id, 1);
        assert_eq!(first.slot_id, 1);
        assert_eq!((first.width, first.height), (64, 32));

        observer.config_changed(PipeWireStreamConfig {
            width: 128,
            height: 64,
            spa_format,
            flags: 0,
            modifier: DRM_FORMAT_MOD_LINEAR,
        });
        let fd10 = tempfile_fd();
        let fd11 = tempfile_fd();
        observer.buffer_added(10, Ok(descriptor(fd10.as_raw_fd(), 512)));
        observer.buffer_added(11, Ok(descriptor(fd11.as_raw_fd(), 512)));
        observer.frame_ready(11, 2234, Ok(descriptor(fd11.as_raw_fd(), 512)));

        let producer = handle.producer().expect("producer handle should stay live after reconfigure");
        let producer = producer.lock().unwrap();
        let cursor = producer.control_page().latest_cursor().unwrap();
        let second = producer.control_page().read_entry_for_cursor(cursor).unwrap();
        assert_eq!(second.cursor, first.cursor + 1);
        assert_eq!(second.sequence, first.sequence + 1);
        assert_eq!(second.pool_id, 2);
        assert_eq!(second.slot_id, 1);
        assert_eq!(second.config_generation, first.config_generation + 1);
        assert_eq!((second.width, second.height), (128, 64));
        assert_eq!(second.fence_id, first.fence_id);
        assert_eq!(second.fence_value, first.fence_value + 1);
        assert!(
            handle.last_error().is_none(),
            "unexpected observer error: {:?}",
            handle.last_error()
        );
    }

    #[test]
    fn describes_spa_buffer_dmabuf_planes_and_header() {
        let probe = PipeWireRuntimeProbe::probe().unwrap();
        let fd = tempfile_fd();
        let mut chunk = SpaChunk {
            offset: 128,
            size: 4096,
            stride: 512,
            flags: 0,
        };
        let mut data = [SpaData {
            data_type: probe.spa_data_dmabuf,
            flags: 0,
            fd: fd.as_raw_fd() as i64,
            mapoffset: 64,
            maxsize: 8192,
            data: std::ptr::null_mut(),
            chunk: &mut chunk,
        }];
        let mut header = SpaMetaHeader {
            flags: 7,
            offset: 0,
            pts: 42,
            dts_offset: 0,
            seq: 99,
        };
        let mut metas = [SpaMeta {
            meta_type: probe.spa_meta_header,
            size: size_of::<SpaMetaHeader>() as u32,
            data: (&mut header as *mut SpaMetaHeader).cast(),
        }];
        let buffer = SpaBuffer {
            n_metas: metas.len() as u32,
            n_datas: data.len() as u32,
            metas: metas.as_mut_ptr(),
            datas: data.as_mut_ptr(),
        };

        let descriptor = unsafe { PipeWireBufferDescriptor::describe_spa_buffer((&buffer as *const SpaBuffer).cast()) }.unwrap();
        assert_eq!(descriptor.planes.len(), 1);
        assert_eq!(descriptor.planes[0].fd, fd.as_raw_fd());
        assert_eq!(descriptor.planes[0].offset, 192);
        assert_eq!(descriptor.planes[0].stride, 512);
        assert_eq!(descriptor.planes[0].maxsize, 8192);
        assert_eq!(descriptor.header.as_ref().unwrap().flags, 7);
        assert_eq!(descriptor.header.as_ref().unwrap().pts, 42);
        assert_eq!(descriptor.header.as_ref().unwrap().sequence, 99);

        let frame = descriptor
            .to_owned_frame(64, 32, probe.spa_video_format_bgra, DRM_FORMAT_MOD_LINEAR)
            .unwrap();
        let surface = frame.into_surface_handle(&probe).unwrap();
        assert_eq!(surface.planes[0].offset, 192);
        assert_eq!(surface.planes[0].stride, 512);
    }

    #[test]
    fn rejects_spa_buffer_without_dmabuf_data() {
        let fd = tempfile_fd();
        let mut chunk = SpaChunk {
            offset: 0,
            size: 4096,
            stride: 512,
            flags: 0,
        };
        let mut data = [SpaData {
            data_type: 2,
            flags: 0,
            fd: fd.as_raw_fd() as i64,
            mapoffset: 0,
            maxsize: 4096,
            data: std::ptr::null_mut(),
            chunk: &mut chunk,
        }];
        let buffer = SpaBuffer {
            n_metas: 0,
            n_datas: data.len() as u32,
            metas: std::ptr::null_mut(),
            datas: data.as_mut_ptr(),
        };

        assert!(unsafe { PipeWireBufferDescriptor::describe_spa_buffer((&buffer as *const SpaBuffer).cast()) }.is_err());
    }

    fn plane(stride: u32) -> PipeWireDmabufPlane {
        PipeWireDmabufPlane {
            fd: tempfile_fd(),
            offset: 0,
            stride,
        }
    }

    fn tempfile_fd() -> OwnedFd {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"dmabuf").unwrap();
        OwnedFd::from(file)
    }

    fn descriptor(fd: RawFd, stride: u32) -> PipeWireBufferDescriptor {
        PipeWireBufferDescriptor {
            planes: vec![PipeWireBufferPlaneDescriptor {
                fd,
                offset: 0,
                stride,
                maxsize: stride * 64,
            }],
            header: None,
        }
    }

    #[repr(C)]
    struct SpaChunk {
        offset: u32,
        size: u32,
        stride: i32,
        flags: i32,
    }

    #[repr(C)]
    struct SpaData {
        data_type: u32,
        flags: u32,
        fd: i64,
        mapoffset: u32,
        maxsize: u32,
        data: *mut std::ffi::c_void,
        chunk: *mut SpaChunk,
    }

    #[repr(C)]
    struct SpaBuffer {
        n_metas: u32,
        n_datas: u32,
        metas: *mut SpaMeta,
        datas: *mut SpaData,
    }

    #[repr(C)]
    struct SpaMeta {
        meta_type: u32,
        size: u32,
        data: *mut std::ffi::c_void,
    }

    #[repr(C)]
    struct SpaMetaHeader {
        flags: u32,
        offset: u32,
        pts: i64,
        dts_offset: i64,
        seq: u64,
    }
}
