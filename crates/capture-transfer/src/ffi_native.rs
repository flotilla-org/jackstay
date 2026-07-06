//! C ABI for the native handle path. The public ABI is descriptor and lease
//! based (ADR-0009): consumers attach through a transport descriptor, receive
//! borrowed surface/sync descriptors, acquire explicit frame leases, and
//! release each lease when the native surface is reusable.

#[cfg(target_os = "linux")]
use std::collections::HashMap;
use std::{
    collections::VecDeque,
    ffi::{CStr, c_char, c_void},
    mem::size_of,
    time::{Duration, Instant},
};
#[cfg(target_os = "linux")]
use std::{
    os::{fd::AsRawFd, unix::net::UnixStream},
    ptr,
};

#[cfg(target_os = "linux")]
use crate::native::linux::{
    LINUX_ATTACH_MAX_FDS, LinuxAttachRequest, LinuxAttachResponse, LinuxFdTable, LinuxPoolDescriptor, LinuxReleaseDescriptor,
    LinuxSyncDescriptor, recv_json, recv_json_with_fds, send_json, send_json_then_fds,
};
#[cfg(target_os = "macos")]
use crate::native::macos::{IoSurface, SharedEventHandle, xpc::XpcAttachClient};
use crate::{
    control_page::VideoTrackControlPage,
    ffi::{
        FT_STATUS_CLOSED, FT_STATUS_EMPTY, FT_STATUS_ERROR, FT_STATUS_INVALID_ARGUMENT, FT_STATUS_INVALID_STATE, FT_STATUS_OK,
        FT_STATUS_TIMEOUT, FT_STATUS_UNSUPPORTED, FtStatus,
    },
    native::lease::{NativeLeaseBook, NativeLeaseIdentity, NativeLeaseRelease},
};

pub const FT_NATIVE_ATTACH_TRANSPORT_MACOS_XPC: u32 = 1;
pub const FT_NATIVE_ATTACH_TRANSPORT_UNIX_SOCKET: u32 = 2;

pub const FT_NATIVE_HANDLE_IOSURFACE: u32 = 1;
pub const FT_NATIVE_HANDLE_DMABUF: u32 = 2;
pub const FT_NATIVE_HANDLE_D3D12_RESOURCE: u32 = 3;
pub const FT_NATIVE_MAX_PLANES: usize = 4;

pub const FT_NATIVE_SYNC_NONE: u32 = 0;
pub const FT_NATIVE_SYNC_MTL_SHARED_EVENT: u32 = 1;
pub const FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE: u32 = 2;
pub const FT_NATIVE_SYNC_D3D12_FENCE: u32 = 3;

pub const FT_NATIVE_RELEASE_NOW: u32 = 1;
pub const FT_NATIVE_RELEASE_TIMELINE_VALUE: u32 = 2;

pub const FT_NATIVE_EVENT_POOL_ADDED: u32 = 1;
pub const FT_NATIVE_EVENT_POOL_REMOVED: u32 = 2;
pub const FT_NATIVE_EVENT_STREAM_CONFIG_CHANGED: u32 = 3;
pub const FT_NATIVE_EVENT_PRODUCER_STOPPED: u32 = 4;

pub const FT_WAIT_INFINITE: u64 = u64::MAX;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtNativeAttachDescriptor {
    pub struct_size: u32,
    pub transport_kind: u32,
    pub requested_consumer_id: u64,
    pub endpoint: *const c_char,
    pub bearer_token: *const c_char,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FtNativePlane {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtNativeSurface {
    pub struct_size: u32,
    pub handle_kind: u32,
    pub plane_count: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub modifier: u64,
    pub object: *mut c_void,
    pub planes: [FtNativePlane; FT_NATIVE_MAX_PLANES],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union FtNativeSyncHandle {
    pub object: *mut c_void,
    pub fd: i32,
}

impl std::fmt::Debug for FtNativeSyncHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The discriminant lives in FtNativeSync.sync_kind, so Debug cannot
        // know which arm is live — and reading the 8-byte `object` arm after
        // a 4-byte `fd` write would read uninitialized bytes. Stay opaque.
        f.write_str("FtNativeSyncHandle(<opaque>)")
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtNativeSync {
    pub struct_size: u32,
    pub sync_kind: u32,
    pub sync_id: u64,
    pub handle: FtNativeSyncHandle,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtNativePool {
    pub struct_size: u32,
    pub surface_count: u32,
    pub pool_id: u64,
    pub surfaces: *const FtNativeSurface,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtNativeGrant {
    pub struct_size: u32,
    pub pool_count: u32,
    pub consumer_id: u64,
    pub consumer_slot: u64,
    pub pools: *const FtNativePool,
    pub producer_sync: FtNativeSync,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtNativeFrame {
    pub struct_size: u32,
    pub flags: u32,
    pub lease_id: u64,
    pub cursor: u64,
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub pool_id: u64,
    pub slot_id: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub producer_sync_id: u64,
    pub producer_sync_value: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtNativeRelease {
    pub struct_size: u32,
    pub release_kind: u32,
    pub lease_id: u64,
    pub release_sync_id: u64,
    pub release_value: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtNativeEvent {
    pub struct_size: u32,
    pub kind: u32,
    pub pool_id: u64,
    pub config_generation: u64,
}

const _: () = {
    assert!(size_of::<FtNativeAttachDescriptor>() == 40);
    assert!(std::mem::offset_of!(FtNativeAttachDescriptor, requested_consumer_id) == 8);
    assert!(std::mem::offset_of!(FtNativeAttachDescriptor, endpoint) == 16);
    assert!(std::mem::offset_of!(FtNativeAttachDescriptor, bearer_token) == 24);
    assert!(std::mem::offset_of!(FtNativeAttachDescriptor, flags) == 32);
    assert!(size_of::<FtNativePlane>() == 12);
    assert!(size_of::<FtNativeSurface>() == 88);
    assert!(std::mem::offset_of!(FtNativeSurface, object) == 32);
    assert!(size_of::<FtNativeSync>() == 24);
    assert!(size_of::<FtNativePool>() == 24);
    assert!(size_of::<FtNativeGrant>() == 56);
    assert!(std::mem::offset_of!(FtNativeGrant, pools) == 24);
    assert!(std::mem::offset_of!(FtNativeGrant, producer_sync) == 32);
    assert!(size_of::<FtNativeFrame>() == 80);
    assert!(std::mem::offset_of!(FtNativeFrame, lease_id) == 8);
    assert!(std::mem::offset_of!(FtNativeFrame, producer_sync_id) == 64);
    assert!(size_of::<FtNativeRelease>() == 32);
    assert!(size_of::<FtNativeEvent>() == 24);
};

/// Opaque consumer-side attach: owns the transport connection, the grant's
/// borrowed handles, the mapped ring, and outstanding frame leases.
pub struct FtNativeAttach {
    _transport: FtNativeAttachTransport,
    page: VideoTrackControlPage,
    _surface_descriptors: Vec<FtNativeSurface>,
    pools: Vec<FtNativePool>,
    producer_sync: FtNativeSync,
    consumer_id: u64,
    consumer_slot: u64,
    dynamic_pools: Vec<FtNativeDynamicPool>,
    pending_events: VecDeque<FtNativeEvent>,
    last_event_config_generation: u64,
    last_event_pool_id: u64,
    producer_stopped: bool,
    producer_stopped_event_emitted: bool,
    lease_book: NativeLeaseBook,
    #[cfg(target_os = "linux")]
    release_syncs: HashMap<u64, FtNativeSync>,
}

struct FtNativeDynamicPool {
    _surfaces: Box<[FtNativeSurface]>,
    pool: FtNativePool,
}

enum FtNativeAttachTransport {
    #[cfg(target_os = "macos")]
    Macos {
        _client: XpcAttachClient,
        _surfaces: Vec<IoSurface>,
        _sync_handle: SharedEventHandle,
    },
    #[cfg(target_os = "linux")]
    Linux { _stream: UnixStream, _fd_table: LinuxFdTable },
}

impl FtNativeAttach {
    #[cfg(target_os = "macos")]
    fn from_client(client: XpcAttachClient, requested_consumer_id: u64) -> Result<Box<Self>, FtStatus> {
        let grant = client.attach(requested_consumer_id).map_err(|_| FT_STATUS_ERROR)?;
        let page = VideoTrackControlPage::map_read_only(grant.ring_fd, grant.ring_map_len as usize).map_err(|_| FT_STATUS_ERROR)?;
        let surface_descriptors = grant
            .surface_handles
            .iter()
            .map(|surface| FtNativeSurface {
                struct_size: size_of::<FtNativeSurface>() as u32,
                handle_kind: FT_NATIVE_HANDLE_IOSURFACE,
                plane_count: 0,
                width: surface.width(),
                height: surface.height(),
                pixel_format: 0,
                modifier: 0,
                object: surface.as_raw(),
                planes: [FtNativePlane::default(); FT_NATIVE_MAX_PLANES],
            })
            .collect::<Vec<_>>();
        let pools = vec![FtNativePool {
            struct_size: size_of::<FtNativePool>() as u32,
            surface_count: surface_descriptors.len() as u32,
            pool_id: grant.pool_id,
            surfaces: surface_descriptors.as_ptr(),
        }];
        let producer_sync = FtNativeSync {
            struct_size: size_of::<FtNativeSync>() as u32,
            sync_kind: FT_NATIVE_SYNC_MTL_SHARED_EVENT,
            sync_id: grant.fence_id,
            handle: FtNativeSyncHandle {
                object: grant.sync_handle.as_raw(),
            },
        };
        let last_event_config_generation = latest_config_generation(&page);
        let last_event_pool_id = grant.pool_id;
        Ok(Box::new(Self {
            _transport: FtNativeAttachTransport::Macos {
                _client: client,
                _surfaces: grant.surface_handles,
                _sync_handle: grant.sync_handle,
            },
            page,
            _surface_descriptors: surface_descriptors,
            pools,
            producer_sync,
            consumer_id: grant.consumer_id,
            consumer_slot: grant.consumer_slot,
            dynamic_pools: Vec::new(),
            pending_events: VecDeque::new(),
            last_event_config_generation,
            last_event_pool_id,
            producer_stopped: false,
            producer_stopped_event_emitted: false,
            lease_book: NativeLeaseBook::new(),
            #[cfg(target_os = "linux")]
            release_syncs: HashMap::new(),
        }))
    }

    #[cfg(all(test, target_os = "macos", feature = "backend-macos"))]
    fn connect_endpoint_for_test(
        endpoint: &crate::native::macos::xpc::XpcListenerEndpoint,
        bearer_token: Option<&str>,
        requested_consumer_id: u64,
    ) -> Result<Box<Self>, FtStatus> {
        let client = XpcAttachClient::connect_endpoint(endpoint).map_err(|_| FT_STATUS_ERROR)?;
        if let Some(token) = bearer_token {
            client.authorize(token).map_err(|_| FT_STATUS_ERROR)?;
        }
        Self::from_client(client, requested_consumer_id)
    }

    #[cfg(target_os = "linux")]
    fn from_unix_socket(endpoint: &str, bearer_token: Option<&str>, requested_consumer_id: u64) -> Result<Box<Self>, FtStatus> {
        let stream = UnixStream::connect(endpoint).map_err(|_| FT_STATUS_ERROR)?;
        if let Some(token) = bearer_token {
            send_json(
                &stream,
                &LinuxAttachRequest::Authorize {
                    bearer_token: token.to_string(),
                },
            )
            .map_err(|_| FT_STATUS_ERROR)?;
            match recv_json::<LinuxAttachResponse>(&stream).map_err(|_| FT_STATUS_ERROR)? {
                LinuxAttachResponse::Authorized => {}
                LinuxAttachResponse::Error { .. } => return Err(FT_STATUS_ERROR),
                _ => return Err(FT_STATUS_INVALID_STATE),
            }
        }

        send_json(
            &stream,
            &LinuxAttachRequest::Attach {
                consumer_id: requested_consumer_id,
            },
        )
        .map_err(|_| FT_STATUS_ERROR)?;
        let (response, fd_table): (LinuxAttachResponse, _) =
            recv_json_with_fds(&stream, LINUX_ATTACH_MAX_FDS).map_err(|_| FT_STATUS_ERROR)?;
        let grant = match response {
            LinuxAttachResponse::Granted { grant } => grant,
            LinuxAttachResponse::Error { .. } => return Err(FT_STATUS_ERROR),
            _ => return Err(FT_STATUS_INVALID_STATE),
        };
        grant.validate_fd_indices(fd_table.len()).map_err(|_| FT_STATUS_INVALID_ARGUMENT)?;

        let page = VideoTrackControlPage::map_read_only(
            fd_table.try_clone_fd(grant.ring_fd_index).map_err(|_| FT_STATUS_ERROR)?,
            grant.ring_map_len as usize,
        )
        .map_err(|_| FT_STATUS_ERROR)?;
        let mut surface_descriptors = Vec::with_capacity(grant.pools.iter().map(|pool| pool.surfaces.len()).sum());
        let mut pools = Vec::with_capacity(grant.pools.len());
        for pool in &grant.pools {
            let surfaces_offset = surface_descriptors.len();
            surface_descriptors.extend(linux_pool_surfaces(pool, &fd_table)?);
            pools.push(FtNativePool {
                struct_size: size_of::<FtNativePool>() as u32,
                surface_count: pool.surfaces.len() as u32,
                pool_id: pool.pool_id,
                surfaces: ptr::null(),
            });
            let pool_index = pools.len() - 1;
            pools[pool_index].surfaces = surface_descriptors[surfaces_offset..].as_ptr();
        }
        let producer_sync = FtNativeSync {
            struct_size: size_of::<FtNativeSync>() as u32,
            sync_kind: grant.producer_sync.sync_kind,
            sync_id: grant.producer_sync.sync_id,
            handle: FtNativeSyncHandle {
                fd: fd_table
                    .raw_fd(grant.producer_sync.fd_index)
                    .map_err(|_| FT_STATUS_INVALID_ARGUMENT)?,
            },
        };
        let last_event_config_generation = latest_config_generation(&page);
        let last_event_pool_id = grant
            .pools
            .first()
            .map(|pool| pool.pool_id)
            .or_else(|| latest_entry_pool_and_generation(&page).map(|(pool_id, _)| pool_id))
            .unwrap_or(0);
        Ok(Box::new(Self {
            _transport: FtNativeAttachTransport::Linux {
                _stream: stream,
                _fd_table: fd_table,
            },
            page,
            _surface_descriptors: surface_descriptors,
            pools,
            producer_sync,
            consumer_id: grant.consumer_id,
            consumer_slot: grant.consumer_slot,
            dynamic_pools: Vec::new(),
            pending_events: VecDeque::new(),
            last_event_config_generation,
            last_event_pool_id,
            producer_stopped: false,
            producer_stopped_event_emitted: false,
            lease_book: NativeLeaseBook::new(),
            #[cfg(target_os = "linux")]
            release_syncs: HashMap::new(),
        }))
    }

    fn observe_producer_stopped(&mut self) -> bool {
        #[cfg(target_os = "linux")]
        {
            if self.producer_stopped {
                return true;
            }
            let FtNativeAttachTransport::Linux { _stream: stream, .. } = &self._transport;
            if linux_stream_peer_closed(stream) {
                self.producer_stopped = true;
                return true;
            }
        }
        self.producer_stopped
    }

    fn acquire_latest(&mut self, min_cursor: u64) -> FtStatusAndFrame {
        match self.page.read_latest_lossy_entry() {
            Ok(Some(entry)) if entry.cursor > min_cursor => {
                let identity = NativeLeaseIdentity {
                    cursor: entry.cursor,
                    sequence: entry.sequence,
                    pool_id: entry.pool_id,
                    slot_id: entry.slot_id,
                };
                let lease_id = match self.acquire_lease(identity) {
                    Ok(lease_id) => lease_id,
                    Err(status) => {
                        return FtStatusAndFrame { status, frame: None };
                    }
                };
                FtStatusAndFrame {
                    status: FT_STATUS_OK,
                    frame: Some(FtNativeFrame {
                        struct_size: size_of::<FtNativeFrame>() as u32,
                        flags: entry.flags,
                        lease_id,
                        cursor: entry.cursor,
                        sequence: entry.sequence,
                        timestamp_ns: entry.timestamp_ns,
                        pool_id: entry.pool_id,
                        slot_id: entry.slot_id,
                        width: entry.width,
                        height: entry.height,
                        pixel_format: entry.pixel_format,
                        producer_sync_id: entry.fence_id,
                        producer_sync_value: entry.fence_value,
                    }),
                }
            }
            Ok(Some(_)) | Ok(None) => FtStatusAndFrame {
                status: if self.observe_producer_stopped() {
                    FT_STATUS_CLOSED
                } else {
                    FT_STATUS_EMPTY
                },
                frame: None,
            },
            Err(_) => FtStatusAndFrame {
                status: FT_STATUS_ERROR,
                frame: None,
            },
        }
    }

    fn acquire_lease(&mut self, identity: NativeLeaseIdentity) -> Result<u64, FtStatus> {
        #[cfg(target_os = "linux")]
        {
            let FtNativeAttachTransport::Linux { _stream: stream, .. } = &self._transport;
            send_json(stream, &LinuxAttachRequest::AcquireLease { identity }).map_err(|_| FT_STATUS_ERROR)?;
            match recv_json::<LinuxAttachResponse>(stream).map_err(|_| FT_STATUS_ERROR)? {
                LinuxAttachResponse::LeaseAcquired { lease_id } => Ok(lease_id),
                LinuxAttachResponse::Error { .. } => Err(FT_STATUS_INVALID_STATE),
                _ => Err(FT_STATUS_INVALID_STATE),
            }
        }

        #[cfg(not(target_os = "linux"))]
        self.lease_book.acquire(identity).map_err(|_| FT_STATUS_ERROR)
    }
}

fn latest_config_generation(page: &VideoTrackControlPage) -> u64 {
    page.config_snapshot().last().map_or(0, |config| config.config_generation)
}

fn latest_entry_pool_and_generation(page: &VideoTrackControlPage) -> Option<(u64, u64)> {
    page.read_latest_lossy_entry()
        .ok()
        .flatten()
        .map(|entry| (entry.pool_id, entry.config_generation))
}

#[cfg(target_os = "linux")]
fn linux_stream_peer_closed(stream: &UnixStream) -> bool {
    let mut byte = 0u8;
    let result = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&mut byte as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if result == 0 {
        return true;
    }
    if result > 0 {
        return false;
    }
    false
}

#[cfg(target_os = "linux")]
fn linux_pool_surfaces(pool: &LinuxPoolDescriptor, fd_table: &LinuxFdTable) -> Result<Vec<FtNativeSurface>, FtStatus> {
    let mut surfaces = Vec::with_capacity(pool.surfaces.len());
    for surface in &pool.surfaces {
        surface
            .validate_fd_indices(fd_table.len())
            .map_err(|_| FT_STATUS_INVALID_ARGUMENT)?;
        if surface.handle_kind != FT_NATIVE_HANDLE_DMABUF {
            return Err(FT_STATUS_UNSUPPORTED);
        }
        if surface.width == 0 || surface.height == 0 {
            return Err(FT_STATUS_INVALID_ARGUMENT);
        }
        let mut planes = [FtNativePlane::default(); FT_NATIVE_MAX_PLANES];
        for (index, plane) in surface.planes.iter().enumerate() {
            if plane.stride == 0 {
                return Err(FT_STATUS_INVALID_ARGUMENT);
            }
            planes[index] = FtNativePlane {
                fd: fd_table.raw_fd(plane.fd_index).map_err(|_| FT_STATUS_INVALID_ARGUMENT)?,
                offset: plane.offset,
                stride: plane.stride,
            };
        }
        surfaces.push(FtNativeSurface {
            struct_size: size_of::<FtNativeSurface>() as u32,
            handle_kind: surface.handle_kind,
            plane_count: surface.planes.len() as u32,
            width: surface.width,
            height: surface.height,
            pixel_format: surface.pixel_format,
            modifier: surface.modifier,
            object: ptr::null_mut(),
            planes,
        });
    }
    Ok(surfaces)
}

impl FtNativeAttach {
    fn known_pool(&self, pool_id: u64) -> Option<FtNativePool> {
        self.pools
            .iter()
            .copied()
            .chain(self.dynamic_pools.iter().map(|pool| pool.pool))
            .find(|pool| pool.pool_id == pool_id)
    }

    #[cfg(target_os = "linux")]
    fn import_linux_pool(&mut self, pool: LinuxPoolDescriptor, fd_table: LinuxFdTable) -> Result<FtNativePool, FtStatus> {
        let surfaces = linux_pool_surfaces(&pool, &fd_table)?;
        let mut surfaces = surfaces.into_boxed_slice();
        let native_pool = FtNativePool {
            struct_size: size_of::<FtNativePool>() as u32,
            surface_count: surfaces.len() as u32,
            pool_id: pool.pool_id,
            surfaces: surfaces.as_mut_ptr(),
        };
        let FtNativeAttachTransport::Linux { _fd_table, .. } = &mut self._transport;
        _fd_table.extend(fd_table.into_fds());
        self.dynamic_pools.push(FtNativeDynamicPool {
            _surfaces: surfaces,
            pool: native_pool,
        });
        Ok(native_pool)
    }

    #[cfg(target_os = "linux")]
    fn fetch_linux_pool(&mut self, pool_id: u64) -> Result<FtNativePool, FtStatus> {
        if let Some(pool) = self.known_pool(pool_id) {
            return Ok(pool);
        }
        let FtNativeAttachTransport::Linux { _stream: stream, .. } = &self._transport;
        send_json(stream, &LinuxAttachRequest::GetPool { pool_id }).map_err(|_| FT_STATUS_ERROR)?;
        let (response, fd_table) = recv_json_with_fds::<LinuxAttachResponse>(stream, LINUX_ATTACH_MAX_FDS).map_err(|_| FT_STATUS_ERROR)?;
        match response {
            LinuxAttachResponse::Pool { pool } if pool.pool_id == pool_id => self.import_linux_pool(pool, fd_table),
            LinuxAttachResponse::Pool { .. } => Err(FT_STATUS_INVALID_STATE),
            LinuxAttachResponse::Error { .. } => Err(FT_STATUS_INVALID_STATE),
            _ => Err(FT_STATUS_INVALID_STATE),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn fetch_linux_pool(&mut self, pool_id: u64) -> Result<FtNativePool, FtStatus> {
        self.known_pool(pool_id).ok_or(FT_STATUS_INVALID_STATE)
    }
}

struct FtStatusAndFrame {
    status: FtStatus,
    frame: Option<FtNativeFrame>,
}

fn valid_struct_size<T>(size: u32) -> bool {
    size as usize >= size_of::<T>()
}

/// Connect to a native attach endpoint described by `descriptor`.
///
/// # Safety
/// `descriptor` must point to a valid [`FtNativeAttachDescriptor`]. `out`
/// must point to writable storage for one pointer; the result is freed with
/// [`ft_native_attach_destroy`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_native_attach_connect(descriptor: *const FtNativeAttachDescriptor, out: *mut *mut FtNativeAttach) -> FtStatus {
    let Some(out) = (unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    *out = std::ptr::null_mut();
    let Some(descriptor) = (unsafe { descriptor.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !valid_struct_size::<FtNativeAttachDescriptor>(descriptor.struct_size) || descriptor.endpoint.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    if descriptor.flags != 0 {
        return FT_STATUS_UNSUPPORTED;
    }
    let Ok(endpoint) = (unsafe { CStr::from_ptr(descriptor.endpoint) }).to_str() else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let bearer_token = if descriptor.bearer_token.is_null() {
        None
    } else {
        let Ok(token) = (unsafe { CStr::from_ptr(descriptor.bearer_token) }).to_str() else {
            return FT_STATUS_INVALID_ARGUMENT;
        };
        Some(token)
    };
    let attach = match descriptor.transport_kind {
        #[cfg(target_os = "macos")]
        FT_NATIVE_ATTACH_TRANSPORT_MACOS_XPC => {
            let client = match XpcAttachClient::connect_named(endpoint) {
                Ok(client) => client,
                Err(_) => return FT_STATUS_ERROR,
            };
            if let Some(token) = bearer_token
                && client.authorize(token).is_err()
            {
                return FT_STATUS_ERROR;
            }
            FtNativeAttach::from_client(client, descriptor.requested_consumer_id)
        }
        #[cfg(target_os = "linux")]
        FT_NATIVE_ATTACH_TRANSPORT_UNIX_SOCKET => {
            FtNativeAttach::from_unix_socket(endpoint, bearer_token, descriptor.requested_consumer_id)
        }
        _ => return FT_STATUS_UNSUPPORTED,
    };
    match attach {
        Ok(attach) => {
            *out = Box::into_raw(attach);
            FT_STATUS_OK
        }
        Err(status) => status,
    }
}

/// Fill `out_grant` with a borrowed view of the attach's handles.
///
/// # Safety
/// `attach` must be a live pointer from [`ft_native_attach_connect`];
/// `out_grant` must point to writable storage for one [`FtNativeGrant`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_native_attach_grant(attach: *const FtNativeAttach, out_grant: *mut FtNativeGrant) -> FtStatus {
    let Some(attach) = (unsafe { attach.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Some(out_grant) = (unsafe { out_grant.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !valid_struct_size::<FtNativeGrant>(out_grant.struct_size) {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    *out_grant = FtNativeGrant {
        struct_size: size_of::<FtNativeGrant>() as u32,
        pool_count: attach.pools.len() as u32,
        consumer_id: attach.consumer_id,
        consumer_slot: attach.consumer_slot,
        pools: attach.pools.as_ptr(),
        producer_sync: attach.producer_sync,
    };
    FT_STATUS_OK
}

/// Wait until the latest producer cursor is greater than `min_cursor`.
///
/// # Safety
/// `attach` must be live and `out_cursor` must point to writable storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_native_wait_frame(
    attach: *mut FtNativeAttach,
    min_cursor: u64,
    timeout_ns: u64,
    out_cursor: *mut u64,
) -> FtStatus {
    let Some(attach) = (unsafe { attach.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if out_cursor.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let deadline = if timeout_ns == FT_WAIT_INFINITE {
        None
    } else {
        Instant::now().checked_add(Duration::from_nanos(timeout_ns))
    };
    loop {
        match attach.page.read_latest_lossy_entry() {
            Ok(Some(entry)) if entry.cursor > min_cursor => {
                unsafe { *out_cursor = entry.cursor };
                return FT_STATUS_OK;
            }
            Ok(Some(entry)) => unsafe { *out_cursor = entry.cursor },
            Ok(None) => unsafe { *out_cursor = 0 },
            Err(_) => return FT_STATUS_ERROR,
        }
        if attach.observe_producer_stopped() {
            return FT_STATUS_CLOSED;
        }
        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            return FT_STATUS_TIMEOUT;
        }
        let sleep_for = deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(1)))
            .unwrap_or_else(|| Duration::from_millis(1));
        if sleep_for.is_zero() {
            return FT_STATUS_TIMEOUT;
        }
        std::thread::sleep(sleep_for);
    }
}

/// Acquire the newest frame whose cursor is greater than `min_cursor`.
///
/// # Safety
/// `attach` must be live and `out_frame` must point to writable storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_native_acquire_latest(attach: *mut FtNativeAttach, min_cursor: u64, out_frame: *mut FtNativeFrame) -> FtStatus {
    let Some(attach) = (unsafe { attach.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Some(out_frame) = (unsafe { out_frame.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !valid_struct_size::<FtNativeFrame>(out_frame.struct_size) {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let result = attach.acquire_latest(min_cursor);
    if let Some(frame) = result.frame {
        *out_frame = frame;
    }
    result.status
}

/// Register a consumer release timeline for later `release_frame` calls.
///
/// # Safety
/// `attach`, `sync`, and `out_release_sync_id` must be valid pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_native_register_release_sync(
    attach: *mut FtNativeAttach,
    sync: *const FtNativeSync,
    out_release_sync_id: *mut u64,
) -> FtStatus {
    let Some(attach) = (unsafe { attach.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Some(sync) = (unsafe { sync.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Some(out_release_sync_id) = (unsafe { out_release_sync_id.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    *out_release_sync_id = 0;
    if !valid_struct_size::<FtNativeSync>(sync.struct_size) {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    if sync.sync_kind == FT_NATIVE_SYNC_NONE {
        return FT_STATUS_UNSUPPORTED;
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = attach;
        FT_STATUS_UNSUPPORTED
    }
    #[cfg(target_os = "linux")]
    {
        let release_sync_id = match &attach._transport {
            FtNativeAttachTransport::Linux { _stream: stream, .. } => {
                if sync.sync_kind != FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE {
                    return FT_STATUS_UNSUPPORTED;
                }
                let fd = unsafe { sync.handle.fd };
                if fd < 0 {
                    return FT_STATUS_INVALID_ARGUMENT;
                }
                if send_json_then_fds(
                    stream,
                    &LinuxAttachRequest::RegisterReleaseSync {
                        sync: LinuxSyncDescriptor {
                            sync_kind: sync.sync_kind,
                            sync_id: sync.sync_id,
                            fd_index: 0,
                        },
                    },
                    &[fd],
                )
                .is_err()
                {
                    return FT_STATUS_ERROR;
                }
                match recv_json::<LinuxAttachResponse>(stream) {
                    Ok(LinuxAttachResponse::ReleaseSyncRegistered { release_sync_id }) => {
                        // A server handing out the reserved id is a protocol
                        // violation, not a state this client can hold.
                        if attach.lease_book.register_release_sync_id(release_sync_id).is_err() {
                            return FT_STATUS_ERROR;
                        }
                        release_sync_id
                    }
                    Ok(LinuxAttachResponse::Error { .. }) => return FT_STATUS_ERROR,
                    Ok(_) => return FT_STATUS_INVALID_STATE,
                    Err(_) => return FT_STATUS_ERROR,
                }
            }
        };
        attach.release_syncs.insert(release_sync_id, *sync);
        *out_release_sync_id = release_sync_id;
        FT_STATUS_OK
    }
}

/// Release a frame lease.
///
/// # Safety
/// `attach` and `release` must be valid pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_native_release_frame(attach: *mut FtNativeAttach, release: *const FtNativeRelease) -> FtStatus {
    let Some(attach) = (unsafe { attach.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Some(release) = (unsafe { release.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !valid_struct_size::<FtNativeRelease>(release.struct_size) {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let native_release = match release.release_kind {
        FT_NATIVE_RELEASE_NOW => NativeLeaseRelease::Now,
        FT_NATIVE_RELEASE_TIMELINE_VALUE => NativeLeaseRelease::TimelineValue {
            release_sync_id: release.release_sync_id,
            value: release.release_value,
        },
        _ => return FT_STATUS_INVALID_ARGUMENT,
    };
    #[cfg(target_os = "linux")]
    {
        let FtNativeAttachTransport::Linux { _stream: stream, .. } = &attach._transport;
        let release = match native_release {
            NativeLeaseRelease::Now => LinuxReleaseDescriptor::Now {
                lease_id: release.lease_id,
            },
            NativeLeaseRelease::TimelineValue { release_sync_id, value } => LinuxReleaseDescriptor::TimelineValue {
                lease_id: release.lease_id,
                release_sync_id,
                value,
            },
        };
        if send_json(stream, &LinuxAttachRequest::ReleaseFrame { release }).is_err() {
            return FT_STATUS_ERROR;
        }
        match recv_json::<LinuxAttachResponse>(stream) {
            Ok(LinuxAttachResponse::FrameReleased) => FT_STATUS_OK,
            Ok(LinuxAttachResponse::Error { .. }) => FT_STATUS_INVALID_STATE,
            Ok(_) => FT_STATUS_INVALID_STATE,
            Err(_) => FT_STATUS_ERROR,
        }
    }
    #[cfg(not(target_os = "linux"))]
    match attach.lease_book.release(release.lease_id, native_release) {
        Ok(_) => FT_STATUS_OK,
        Err(_) => FT_STATUS_INVALID_STATE,
    }
}

/// Poll the native control event queue.
///
/// # Safety
/// `attach` and `out_event` must be valid pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_native_poll_event(attach: *mut FtNativeAttach, out_event: *mut FtNativeEvent) -> FtStatus {
    let Some(attach) = (unsafe { attach.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Some(out_event) = (unsafe { out_event.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !valid_struct_size::<FtNativeEvent>(out_event.struct_size) {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    if let Some(event) = attach.pending_events.pop_front() {
        *out_event = event;
        return FT_STATUS_OK;
    }
    attach.observe_producer_stopped();
    if attach.producer_stopped && !attach.producer_stopped_event_emitted {
        attach.producer_stopped_event_emitted = true;
        *out_event = FtNativeEvent {
            struct_size: size_of::<FtNativeEvent>() as u32,
            kind: FT_NATIVE_EVENT_PRODUCER_STOPPED,
            pool_id: 0,
            config_generation: attach.last_event_config_generation,
        };
        return FT_STATUS_OK;
    }
    let Some((pool_id, generation)) = latest_entry_pool_and_generation(&attach.page) else {
        return FT_STATUS_EMPTY;
    };
    if generation == 0 || generation <= attach.last_event_config_generation {
        return FT_STATUS_EMPTY;
    }
    let previous_pool_id = attach.last_event_pool_id;
    let pool_changed = previous_pool_id != 0 && previous_pool_id != pool_id;
    if attach.known_pool(pool_id).is_none() {
        match attach.fetch_linux_pool(pool_id) {
            Ok(_) => {
                attach.last_event_config_generation = generation;
                attach.last_event_pool_id = pool_id;
                if pool_changed {
                    attach.pending_events.push_back(FtNativeEvent {
                        struct_size: size_of::<FtNativeEvent>() as u32,
                        kind: FT_NATIVE_EVENT_POOL_ADDED,
                        pool_id,
                        config_generation: generation,
                    });
                    attach.pending_events.push_back(FtNativeEvent {
                        struct_size: size_of::<FtNativeEvent>() as u32,
                        kind: FT_NATIVE_EVENT_STREAM_CONFIG_CHANGED,
                        pool_id,
                        config_generation: generation,
                    });
                    *out_event = FtNativeEvent {
                        struct_size: size_of::<FtNativeEvent>() as u32,
                        kind: FT_NATIVE_EVENT_POOL_REMOVED,
                        pool_id: previous_pool_id,
                        config_generation: generation,
                    };
                    return FT_STATUS_OK;
                }
                attach.pending_events.push_back(FtNativeEvent {
                    struct_size: size_of::<FtNativeEvent>() as u32,
                    kind: FT_NATIVE_EVENT_STREAM_CONFIG_CHANGED,
                    pool_id,
                    config_generation: generation,
                });
                *out_event = FtNativeEvent {
                    struct_size: size_of::<FtNativeEvent>() as u32,
                    kind: FT_NATIVE_EVENT_POOL_ADDED,
                    pool_id,
                    config_generation: generation,
                };
                return FT_STATUS_OK;
            }
            Err(status) => return status,
        }
    }
    attach.last_event_config_generation = generation;
    attach.last_event_pool_id = pool_id;
    if pool_changed {
        attach.pending_events.push_back(FtNativeEvent {
            struct_size: size_of::<FtNativeEvent>() as u32,
            kind: FT_NATIVE_EVENT_POOL_REMOVED,
            pool_id: previous_pool_id,
            config_generation: generation,
        });
    }
    *out_event = FtNativeEvent {
        struct_size: size_of::<FtNativeEvent>() as u32,
        kind: FT_NATIVE_EVENT_STREAM_CONFIG_CHANGED,
        pool_id,
        config_generation: generation,
    };
    FT_STATUS_OK
}

/// Fetch a currently-known pool descriptor by id.
///
/// # Safety
/// `attach` and `out_pool` must be valid pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_native_get_pool(attach: *mut FtNativeAttach, pool_id: u64, out_pool: *mut FtNativePool) -> FtStatus {
    let Some(attach) = (unsafe { attach.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Some(out_pool) = (unsafe { out_pool.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !valid_struct_size::<FtNativePool>(out_pool.struct_size) {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    match attach.fetch_linux_pool(pool_id) {
        Ok(pool) => {
            *out_pool = pool;
            FT_STATUS_OK
        }
        Err(status) => status,
    }
}

/// Destroy the attach and release its borrowed grant handles.
///
/// # Safety
/// `attach` must be a pointer from [`ft_native_attach_connect`] not already
/// destroyed, or NULL (no-op).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_native_attach_destroy(attach: *mut FtNativeAttach) {
    if !attach.is_null() {
        drop(unsafe { Box::from_raw(attach) });
    }
}

#[cfg(all(test, target_os = "macos", feature = "backend-macos"))]
mod tests {
    use std::{
        ptr,
        sync::{Arc, Mutex},
    };

    use super::{
        FT_NATIVE_ATTACH_TRANSPORT_MACOS_XPC, FT_NATIVE_EVENT_STREAM_CONFIG_CHANGED, FT_NATIVE_HANDLE_IOSURFACE, FT_NATIVE_RELEASE_NOW,
        FT_NATIVE_SYNC_MTL_SHARED_EVENT, FtNativeAttach, FtNativeAttachDescriptor, FtNativeFrame, FtNativeGrant, FtNativeRelease,
        ft_native_acquire_latest, ft_native_attach_grant, ft_native_poll_event, ft_native_release_frame, ft_native_wait_frame,
    };
    use crate::{
        ffi::{FT_STATUS_EMPTY, FT_STATUS_OK, FT_STATUS_TIMEOUT},
        model::{ClockDomain, ColorSpace, PixelFormat},
        native::{
            NativeStreamParams, NativeTrackProducer, PoolExhaustionPolicy,
            attach::AttachEndpoint,
            macos::{IoSurface, MacosCapturedFrame, MacosFrameBackend, xpc::XpcAttachServer},
        },
    };

    const WIDTH: u32 = 32;
    const HEIGHT: u32 = 24;

    fn producer() -> Arc<Mutex<NativeTrackProducer<MacosFrameBackend>>> {
        let backend = MacosFrameBackend::new().expect("Metal device required for native ffi tests");
        let params = NativeStreamParams {
            width: WIDTH,
            height: HEIGHT,
            pixel_format: PixelFormat::Bgra8Unorm,
            color_space: ColorSpace::Srgb,
            clock_domain: ClockDomain::HostTime,
            modifier: 0,
        };
        Arc::new(Mutex::new(
            NativeTrackProducer::new(backend, params, 4, 6, PoolExhaustionPolicy::Fail).unwrap(),
        ))
    }

    fn captured(seed: u8) -> MacosCapturedFrame {
        let surface = IoSurface::allocate(WIDTH, HEIGHT, PixelFormat::Bgra8Unorm).unwrap();
        surface
            .write_pixels(
                &(0..WIDTH as usize * HEIGHT as usize * 4)
                    .map(|i| (i as u8).wrapping_add(seed))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        MacosCapturedFrame { surface }
    }

    #[test]
    fn native_ffi_attach_grant_acquire_and_release_lease() {
        let producer = producer();
        let endpoint = AttachEndpoint::new(Some("pta_agent.ffi".to_string()));
        let (_server, listener) = XpcAttachServer::start_anonymous(endpoint, Arc::clone(&producer)).unwrap();

        let attach = FtNativeAttach::connect_endpoint_for_test(&listener, Some("pta_agent.ffi"), 0).unwrap();
        let attach_ptr = Box::into_raw(attach);

        let mut grant = blank_grant();
        assert_eq!(unsafe { ft_native_attach_grant(attach_ptr, &mut grant) }, FT_STATUS_OK);
        assert_ne!(grant.consumer_id, 0);
        assert_eq!(grant.pool_count, 1);
        assert!(!grant.pools.is_null());
        assert_eq!(grant.producer_sync.sync_kind, FT_NATIVE_SYNC_MTL_SHARED_EVENT);
        unsafe {
            let pool = *grant.pools;
            assert_eq!(pool.surface_count, 6);
            assert!(!pool.surfaces.is_null());
            let surface = *pool.surfaces;
            assert_eq!(surface.handle_kind, FT_NATIVE_HANDLE_IOSURFACE);
            assert!(!surface.object.is_null());
        }

        let mut frame = blank_frame();
        assert_eq!(unsafe { ft_native_acquire_latest(attach_ptr, 0, &mut frame) }, FT_STATUS_EMPTY);

        let mut ready_cursor = 99;
        assert_eq!(
            unsafe { ft_native_wait_frame(attach_ptr, 0, 0, &mut ready_cursor) },
            FT_STATUS_TIMEOUT
        );

        for sequence in 1..=4u64 {
            producer.lock().unwrap().publish(&captured(sequence as u8), sequence).unwrap();
        }
        assert_eq!(unsafe { ft_native_wait_frame(attach_ptr, 0, 0, &mut ready_cursor) }, FT_STATUS_OK);
        assert_eq!(ready_cursor, 4);
        assert_eq!(unsafe { ft_native_acquire_latest(attach_ptr, 0, &mut frame) }, FT_STATUS_OK);
        assert_eq!(frame.sequence, 4);
        assert_eq!(frame.cursor, 4);
        assert_ne!(frame.lease_id, 0);
        assert_eq!(frame.width, WIDTH);
        assert_eq!(frame.height, HEIGHT);
        assert_eq!(
            unsafe { ft_native_acquire_latest(attach_ptr, frame.cursor, &mut frame) },
            FT_STATUS_EMPTY
        );

        let release = FtNativeRelease {
            struct_size: std::mem::size_of::<FtNativeRelease>() as u32,
            release_kind: FT_NATIVE_RELEASE_NOW,
            lease_id: frame.lease_id,
            release_sync_id: 0,
            release_value: 0,
        };
        assert_eq!(unsafe { ft_native_release_frame(attach_ptr, &release) }, FT_STATUS_OK);
        assert_eq!(
            unsafe { ft_native_release_frame(attach_ptr, &release) },
            crate::ffi::FT_STATUS_INVALID_STATE
        );

        let mut event = super::FtNativeEvent {
            struct_size: std::mem::size_of::<super::FtNativeEvent>() as u32,
            kind: 0,
            pool_id: 0,
            config_generation: 0,
        };
        assert_eq!(unsafe { ft_native_poll_event(attach_ptr, &mut event) }, FT_STATUS_OK);
        assert_eq!(event.kind, FT_NATIVE_EVENT_STREAM_CONFIG_CHANGED);
        assert_eq!(event.pool_id, frame.pool_id);
        assert_eq!(unsafe { ft_native_poll_event(attach_ptr, &mut event) }, FT_STATUS_EMPTY);

        unsafe { super::ft_native_attach_destroy(attach_ptr) };
    }

    #[test]
    fn native_attach_descriptor_accepts_assigned_consumer_id() {
        let descriptor = FtNativeAttachDescriptor {
            struct_size: std::mem::size_of::<FtNativeAttachDescriptor>() as u32,
            transport_kind: FT_NATIVE_ATTACH_TRANSPORT_MACOS_XPC,
            requested_consumer_id: 0,
            endpoint: ptr::null(),
            bearer_token: ptr::null(),
            flags: 0,
        };
        assert_eq!(
            unsafe { super::ft_native_attach_connect(&descriptor, ptr::null_mut()) },
            crate::ffi::FT_STATUS_INVALID_ARGUMENT
        );
    }

    fn blank_grant() -> FtNativeGrant {
        FtNativeGrant {
            struct_size: std::mem::size_of::<FtNativeGrant>() as u32,
            pool_count: 0,
            consumer_id: 0,
            consumer_slot: 0,
            pools: ptr::null(),
            producer_sync: super::FtNativeSync {
                struct_size: 0,
                sync_kind: 0,
                sync_id: 0,
                handle: super::FtNativeSyncHandle { object: ptr::null_mut() },
            },
        }
    }

    fn blank_frame() -> FtNativeFrame {
        FtNativeFrame {
            struct_size: std::mem::size_of::<FtNativeFrame>() as u32,
            flags: 0,
            lease_id: 0,
            cursor: 0,
            sequence: 0,
            timestamp_ns: 0,
            pool_id: 0,
            slot_id: 0,
            width: 0,
            height: 0,
            pixel_format: 0,
            producer_sync_id: 0,
            producer_sync_value: 0,
        }
    }
}

#[cfg(all(test, target_os = "linux", feature = "backend-linux"))]
mod linux_tests {
    use std::{
        ffi::CString,
        fs::File,
        io::Write,
        os::{fd::AsRawFd, unix::net::UnixListener},
        thread,
    };

    use super::{
        FT_NATIVE_ATTACH_TRANSPORT_UNIX_SOCKET, FT_NATIVE_EVENT_PRODUCER_STOPPED, FT_NATIVE_EVENT_STREAM_CONFIG_CHANGED,
        FT_NATIVE_HANDLE_DMABUF, FT_NATIVE_RELEASE_NOW, FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE, FT_NATIVE_SYNC_MTL_SHARED_EVENT,
        FtNativeAttach, FtNativeAttachDescriptor, FtNativeEvent, FtNativeFrame, FtNativeGrant, FtNativePool, FtNativeRelease, FtNativeSync,
        FtNativeSyncHandle, ft_native_acquire_latest, ft_native_attach_connect, ft_native_attach_destroy, ft_native_attach_grant,
        ft_native_get_pool, ft_native_poll_event, ft_native_register_release_sync, ft_native_release_frame, ft_native_wait_frame,
    };
    use crate::{
        control_page::{PendingVideoRingEntry, VideoTrackControlPage},
        fdpass::recv_fds,
        ffi::{FT_STATUS_CLOSED, FT_STATUS_EMPTY, FT_STATUS_INVALID_ARGUMENT, FT_STATUS_OK, FT_STATUS_UNSUPPORTED},
        model::{ClockDomain, ColorSpace, DamageKind, FrameSyncKind, PayloadKind, PixelFormat},
        native::{
            lease::NativeLeaseIdentity,
            linux::{
                LinuxAttachGrant, LinuxAttachRequest, LinuxAttachResponse, LinuxPlaneDescriptor, LinuxPoolDescriptor,
                LinuxReleaseDescriptor, LinuxSurfaceDescriptor, LinuxSyncDescriptor, recv_json, send_json, send_json_with_fds,
            },
        },
    };

    #[test]
    fn unix_socket_descriptor_connects_and_exposes_borrowed_fd_grant() {
        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("attach.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let mut control_page = VideoTrackControlPage::new(4);
        control_page.push(PendingVideoRingEntry {
            sequence: 10,
            timestamp_ns: 1234,
            width: 64,
            height: 32,
            stride: 0,
            pixel_format: PixelFormat::Bgra8Unorm as u32,
            pool_id: 88,
            slot_id: 0,
            payload_offset: 0,
            payload_len: 0,
            clock_domain: ClockDomain::HostTime as u32,
            color_space: ColorSpace::Srgb as u32,
            sync_kind: FrameSyncKind::NativeTimeline as u32,
            damage_kind: DamageKind::FullFrame as u32,
            damage_base_sequence: 10,
            dropped_before_publish: 0,
            producer_drop_count: 0,
            payload_kind: PayloadKind::DmaBuf as u32,
            modifier: 0,
            fence_id: 55,
            fence_value: 6,
            flags: 0,
        });
        let ring_fd = control_page.try_clone_fd().unwrap();
        let ring_map_len = control_page.mapped_len() as u64;
        let dmabuf = tempfile_file_with_contents(b"dmabuf");
        let reconfigured_dmabuf = tempfile_file_with_contents(b"dmabuf-reconfigured");
        let sync = tempfile_file_with_contents(b"sync");

        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            assert_eq!(
                recv_json::<LinuxAttachRequest>(&stream).unwrap(),
                LinuxAttachRequest::Attach { consumer_id: 0 }
            );
            let response = LinuxAttachResponse::Granted {
                grant: LinuxAttachGrant {
                    consumer_id: 99,
                    consumer_slot: 3,
                    ring_fd_index: 0,
                    ring_map_len,
                    pools: vec![LinuxPoolDescriptor {
                        pool_id: 77,
                        surfaces: vec![LinuxSurfaceDescriptor {
                            handle_kind: FT_NATIVE_HANDLE_DMABUF,
                            width: 64,
                            height: 32,
                            pixel_format: 875_713_112,
                            modifier: 0,
                            planes: vec![LinuxPlaneDescriptor {
                                fd_index: 1,
                                offset: 128,
                                stride: 256,
                            }],
                        }],
                    }],
                    producer_sync: LinuxSyncDescriptor {
                        sync_kind: FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE,
                        sync_id: 55,
                        fd_index: 2,
                    },
                },
            };
            send_json_with_fds(&stream, &response, &[ring_fd.as_raw_fd(), dmabuf.as_raw_fd(), sync.as_raw_fd()]).unwrap();

            assert_eq!(
                recv_json::<LinuxAttachRequest>(&stream).unwrap(),
                LinuxAttachRequest::GetPool { pool_id: 88 }
            );
            let response = LinuxAttachResponse::Pool {
                pool: LinuxPoolDescriptor {
                    pool_id: 88,
                    surfaces: vec![LinuxSurfaceDescriptor {
                        handle_kind: FT_NATIVE_HANDLE_DMABUF,
                        width: 80,
                        height: 32,
                        pixel_format: 875_713_112,
                        modifier: 0,
                        planes: vec![LinuxPlaneDescriptor {
                            fd_index: 0,
                            offset: 64,
                            stride: 320,
                        }],
                    }],
                },
            };
            send_json_with_fds(&stream, &response, &[reconfigured_dmabuf.as_raw_fd()]).unwrap();

            assert_eq!(
                recv_json::<LinuxAttachRequest>(&stream).unwrap(),
                LinuxAttachRequest::AcquireLease {
                    identity: NativeLeaseIdentity {
                        cursor: 2,
                        sequence: 11,
                        pool_id: 88,
                        slot_id: 0,
                    },
                }
            );
            send_json(&stream, &LinuxAttachResponse::LeaseAcquired { lease_id: 501 }).unwrap();

            assert_eq!(
                recv_json::<LinuxAttachRequest>(&stream).unwrap(),
                LinuxAttachRequest::ReleaseFrame {
                    release: LinuxReleaseDescriptor::Now { lease_id: 501 },
                }
            );
            send_json(&stream, &LinuxAttachResponse::FrameReleased).unwrap();

            assert_eq!(
                recv_json::<LinuxAttachRequest>(&stream).unwrap(),
                LinuxAttachRequest::RegisterReleaseSync {
                    sync: LinuxSyncDescriptor {
                        sync_kind: FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE,
                        sync_id: 123,
                        fd_index: 0,
                    },
                }
            );
            let release_fds = recv_fds(&stream, 1).unwrap();
            assert_eq!(release_fds.len(), 1);
            send_json(&stream, &LinuxAttachResponse::ReleaseSyncRegistered { release_sync_id: 42 }).unwrap();
        });

        let endpoint = CString::new(socket_path.to_string_lossy().as_bytes()).unwrap();
        let descriptor = FtNativeAttachDescriptor {
            struct_size: std::mem::size_of::<FtNativeAttachDescriptor>() as u32,
            transport_kind: FT_NATIVE_ATTACH_TRANSPORT_UNIX_SOCKET,
            requested_consumer_id: 0,
            endpoint: endpoint.as_ptr(),
            bearer_token: std::ptr::null(),
            flags: 0,
        };
        let mut attach: *mut FtNativeAttach = std::ptr::null_mut();
        assert_eq!(unsafe { ft_native_attach_connect(&descriptor, &mut attach) }, FT_STATUS_OK);
        assert!(!attach.is_null());

        let mut undersized_grant = FtNativeGrant {
            struct_size: 0,
            pool_count: 0,
            consumer_id: 0,
            consumer_slot: 0,
            pools: std::ptr::null(),
            producer_sync: super::FtNativeSync {
                struct_size: 0,
                sync_kind: 0,
                sync_id: 0,
                handle: super::FtNativeSyncHandle {
                    object: std::ptr::null_mut(),
                },
            },
        };
        assert_eq!(
            unsafe { ft_native_attach_grant(attach, &mut undersized_grant) },
            FT_STATUS_INVALID_ARGUMENT
        );

        let mut grant = blank_grant();
        assert_eq!(unsafe { ft_native_attach_grant(attach, &mut grant) }, FT_STATUS_OK);
        assert_eq!(grant.consumer_id, 99);
        assert_eq!(grant.consumer_slot, 3);
        assert_eq!(grant.pool_count, 1);
        assert_eq!(grant.producer_sync.sync_kind, FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE);
        assert!(unsafe { grant.producer_sync.handle.fd } >= 0);
        unsafe {
            let pool = *grant.pools;
            assert_eq!(pool.pool_id, 77);
            assert_eq!(pool.surface_count, 1);
            let surface = *pool.surfaces;
            assert_eq!(surface.handle_kind, FT_NATIVE_HANDLE_DMABUF);
            assert_eq!(surface.plane_count, 1);
            assert!(surface.object.is_null());
            assert!(surface.planes[0].fd >= 0);
            assert_eq!(surface.planes[0].offset, 128);
            assert_eq!(surface.planes[0].stride, 256);
        }

        let unsupported_release_sync_file = tempfile_file_with_contents(b"unsupported-release-sync");
        let unsupported_release_sync = FtNativeSync {
            struct_size: std::mem::size_of::<FtNativeSync>() as u32,
            sync_kind: FT_NATIVE_SYNC_MTL_SHARED_EVENT,
            sync_id: 123,
            handle: FtNativeSyncHandle {
                fd: unsupported_release_sync_file.as_raw_fd(),
            },
        };
        let mut unsupported_release_sync_id = 99;
        assert_eq!(
            unsafe { ft_native_register_release_sync(attach, &unsupported_release_sync, &mut unsupported_release_sync_id) },
            FT_STATUS_UNSUPPORTED
        );
        assert_eq!(unsupported_release_sync_id, 0);

        let mut event = blank_event();
        assert_eq!(unsafe { ft_native_poll_event(attach, &mut event) }, FT_STATUS_EMPTY);
        control_page.push(PendingVideoRingEntry {
            sequence: 11,
            timestamp_ns: 2234,
            width: 80,
            height: 32,
            stride: 0,
            pixel_format: PixelFormat::Bgra8Unorm as u32,
            pool_id: 88,
            slot_id: 0,
            payload_offset: 0,
            payload_len: 0,
            clock_domain: ClockDomain::HostTime as u32,
            color_space: ColorSpace::Srgb as u32,
            sync_kind: FrameSyncKind::NativeTimeline as u32,
            damage_kind: DamageKind::FullFrame as u32,
            damage_base_sequence: 11,
            dropped_before_publish: 0,
            producer_drop_count: 0,
            payload_kind: PayloadKind::DmaBuf as u32,
            modifier: 0,
            fence_id: 55,
            fence_value: 7,
            flags: 0,
        });
        assert_eq!(unsafe { ft_native_poll_event(attach, &mut event) }, FT_STATUS_OK);
        assert_eq!(event.kind, super::FT_NATIVE_EVENT_POOL_REMOVED);
        assert_eq!(event.pool_id, 77);
        assert_eq!(event.config_generation, 2);

        let mut undersized_pool = FtNativePool {
            struct_size: 0,
            surface_count: 0,
            pool_id: 0,
            surfaces: std::ptr::null(),
        };
        assert_eq!(
            unsafe { ft_native_get_pool(attach, 88, &mut undersized_pool) },
            FT_STATUS_INVALID_ARGUMENT
        );

        let mut pool = FtNativePool {
            struct_size: std::mem::size_of::<FtNativePool>() as u32,
            surface_count: 0,
            pool_id: 0,
            surfaces: std::ptr::null(),
        };
        assert_eq!(unsafe { ft_native_get_pool(attach, 88, &mut pool) }, FT_STATUS_OK);
        assert_eq!(pool.pool_id, 88);
        assert_eq!(pool.surface_count, 1);
        unsafe {
            let surface = *pool.surfaces;
            assert_eq!(surface.width, 80);
            assert_eq!(surface.height, 32);
            assert_eq!(surface.planes[0].offset, 64);
            assert_eq!(surface.planes[0].stride, 320);
            assert!(surface.planes[0].fd >= 0);
        }

        assert_eq!(unsafe { ft_native_poll_event(attach, &mut event) }, FT_STATUS_OK);
        assert_eq!(event.kind, super::FT_NATIVE_EVENT_POOL_ADDED);
        assert_eq!(event.pool_id, 88);
        assert_eq!(event.config_generation, 2);

        assert_eq!(unsafe { ft_native_poll_event(attach, &mut event) }, FT_STATUS_OK);
        assert_eq!(event.kind, FT_NATIVE_EVENT_STREAM_CONFIG_CHANGED);
        assert_eq!(event.pool_id, 88);
        assert_eq!(event.config_generation, 2);

        assert_eq!(unsafe { ft_native_get_pool(attach, 77, &mut pool) }, FT_STATUS_OK);
        assert_eq!(pool.pool_id, 77);
        assert_eq!(pool.surface_count, 1);
        assert_eq!(unsafe { ft_native_poll_event(attach, &mut event) }, FT_STATUS_EMPTY);

        let mut frame = blank_frame();
        let mut undersized_frame = blank_frame();
        undersized_frame.struct_size = 0;
        assert_eq!(
            unsafe { ft_native_acquire_latest(attach, 0, &mut undersized_frame) },
            FT_STATUS_INVALID_ARGUMENT
        );
        assert_eq!(unsafe { ft_native_acquire_latest(attach, 0, &mut frame) }, FT_STATUS_OK);
        assert_eq!(frame.lease_id, 501);
        assert_eq!(frame.cursor, 2);
        assert_eq!(frame.pool_id, 88);
        assert_eq!(frame.slot_id, 0);
        assert_eq!(frame.producer_sync_id, 55);
        assert_eq!(frame.producer_sync_value, 7);

        let release = FtNativeRelease {
            struct_size: std::mem::size_of::<FtNativeRelease>() as u32,
            release_kind: FT_NATIVE_RELEASE_NOW,
            lease_id: frame.lease_id,
            release_sync_id: 0,
            release_value: 0,
        };
        assert_eq!(unsafe { ft_native_release_frame(attach, &release) }, FT_STATUS_OK);

        let release_sync = tempfile_file_with_contents(b"release-sync");
        let sync = FtNativeSync {
            struct_size: std::mem::size_of::<FtNativeSync>() as u32,
            sync_kind: FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE,
            sync_id: 123,
            handle: FtNativeSyncHandle {
                fd: release_sync.as_raw_fd(),
            },
        };
        let mut release_sync_id = 0;
        assert_eq!(
            unsafe { ft_native_register_release_sync(attach, &sync, &mut release_sync_id) },
            FT_STATUS_OK
        );
        assert_eq!(release_sync_id, 42);

        server.join().unwrap();
        let mut ready_cursor = 0;
        assert_eq!(
            unsafe { ft_native_wait_frame(attach, frame.cursor, 0, &mut ready_cursor) },
            FT_STATUS_CLOSED
        );
        assert_eq!(ready_cursor, frame.cursor);
        assert_eq!(unsafe { ft_native_poll_event(attach, &mut event) }, FT_STATUS_OK);
        assert_eq!(event.kind, FT_NATIVE_EVENT_PRODUCER_STOPPED);
        assert_eq!(event.pool_id, 0);
        assert_eq!(event.config_generation, 2);
        assert_eq!(unsafe { ft_native_poll_event(attach, &mut event) }, FT_STATUS_EMPTY);

        unsafe { ft_native_attach_destroy(attach) };
    }

    #[test]
    fn attach_connect_rejects_unknown_flags_and_clears_output_pointer() {
        let endpoint = CString::new("/tmp/porthole-native-flags-should-not-connect.sock").unwrap();
        let descriptor = FtNativeAttachDescriptor {
            struct_size: std::mem::size_of::<FtNativeAttachDescriptor>() as u32,
            transport_kind: FT_NATIVE_ATTACH_TRANSPORT_UNIX_SOCKET,
            requested_consumer_id: 0,
            endpoint: endpoint.as_ptr(),
            bearer_token: std::ptr::null(),
            flags: 1,
        };
        let mut attach = std::ptr::dangling_mut::<FtNativeAttach>();

        assert_eq!(unsafe { ft_native_attach_connect(&descriptor, &mut attach) }, FT_STATUS_UNSUPPORTED);
        assert!(attach.is_null());
    }

    #[test]
    fn attach_connect_rejects_unsupported_linux_surface_kind() {
        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("attach.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let control_page = VideoTrackControlPage::new(4);
        let ring_fd = control_page.try_clone_fd().unwrap();
        let ring_map_len = control_page.mapped_len() as u64;
        let dmabuf = tempfile_file_with_contents(b"dmabuf");
        let sync = tempfile_file_with_contents(b"sync");

        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            assert_eq!(
                recv_json::<LinuxAttachRequest>(&stream).unwrap(),
                LinuxAttachRequest::Attach { consumer_id: 0 }
            );
            let response = LinuxAttachResponse::Granted {
                grant: LinuxAttachGrant {
                    consumer_id: 99,
                    consumer_slot: 3,
                    ring_fd_index: 0,
                    ring_map_len,
                    pools: vec![LinuxPoolDescriptor {
                        pool_id: 77,
                        surfaces: vec![LinuxSurfaceDescriptor {
                            handle_kind: super::FT_NATIVE_HANDLE_D3D12_RESOURCE,
                            width: 64,
                            height: 32,
                            pixel_format: 875_713_112,
                            modifier: 0,
                            planes: vec![LinuxPlaneDescriptor {
                                fd_index: 1,
                                offset: 128,
                                stride: 256,
                            }],
                        }],
                    }],
                    producer_sync: LinuxSyncDescriptor {
                        sync_kind: FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE,
                        sync_id: 55,
                        fd_index: 2,
                    },
                },
            };
            send_json_with_fds(&stream, &response, &[ring_fd.as_raw_fd(), dmabuf.as_raw_fd(), sync.as_raw_fd()]).unwrap();
        });

        let endpoint = CString::new(socket_path.to_string_lossy().as_bytes()).unwrap();
        let descriptor = FtNativeAttachDescriptor {
            struct_size: std::mem::size_of::<FtNativeAttachDescriptor>() as u32,
            transport_kind: FT_NATIVE_ATTACH_TRANSPORT_UNIX_SOCKET,
            requested_consumer_id: 0,
            endpoint: endpoint.as_ptr(),
            bearer_token: std::ptr::null(),
            flags: 0,
        };
        let mut attach = std::ptr::dangling_mut::<FtNativeAttach>();

        assert_eq!(unsafe { ft_native_attach_connect(&descriptor, &mut attach) }, FT_STATUS_UNSUPPORTED);
        assert!(attach.is_null());
        server.join().unwrap();
    }

    fn blank_grant() -> FtNativeGrant {
        FtNativeGrant {
            struct_size: std::mem::size_of::<FtNativeGrant>() as u32,
            pool_count: 0,
            consumer_id: 0,
            consumer_slot: 0,
            pools: std::ptr::null(),
            producer_sync: super::FtNativeSync {
                struct_size: 0,
                sync_kind: 0,
                sync_id: 0,
                handle: super::FtNativeSyncHandle {
                    object: std::ptr::null_mut(),
                },
            },
        }
    }

    fn blank_event() -> FtNativeEvent {
        FtNativeEvent {
            struct_size: std::mem::size_of::<FtNativeEvent>() as u32,
            kind: 0,
            pool_id: 0,
            config_generation: 0,
        }
    }

    fn blank_frame() -> FtNativeFrame {
        FtNativeFrame {
            struct_size: std::mem::size_of::<FtNativeFrame>() as u32,
            flags: 0,
            lease_id: 0,
            cursor: 0,
            sequence: 0,
            timestamp_ns: 0,
            pool_id: 0,
            slot_id: 0,
            width: 0,
            height: 0,
            pixel_format: 0,
            producer_sync_id: 0,
            producer_sync_value: 0,
        }
    }

    fn tempfile_file_with_contents(contents: &[u8]) -> File {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(contents).unwrap();
        file
    }
}
