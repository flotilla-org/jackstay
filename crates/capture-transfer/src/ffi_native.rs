//! C ABI for the macOS native handle path: connect to a jackstay XPC attach
//! service, receive the one-time grant (ring fd + IOSurfaces +
//! MTLSharedEventHandle), and read the ring's latest descriptor — the pieces
//! a C/Metal viewer needs that cannot be reimplemented in C (the XPC
//! handshake and the seqlock ring read).
//!
//! Only available in a `backend-macos` build (macOS). The viewer does its own
//! Metal: it takes the raw `IOSurfaceRef`s and the raw `MTLSharedEventHandle`
//! from the grant, resolves the handle into an `MTLSharedEvent` on its own
//! device, and GPU-waits on each frame's `fence_value` before sampling.
//!
//! Lifetime: the `IOSurfaceRef`s and `MTLSharedEventHandle` exposed by
//! [`ft_native_attach_grant`] are borrowed — owned by the `ft_native_attach`
//! and valid only until [`ft_native_attach_destroy`]. A viewer that builds
//! `MTLTexture`s / an `MTLSharedEvent` from them holds its own retains (ARC),
//! so those survive, but it must not dereference the raw grant pointers after
//! destroy.

use std::ffi::{CStr, c_char, c_void};

use crate::{
    control_page::VideoTrackControlPage,
    ffi::{FT_STATUS_EMPTY, FT_STATUS_ERROR, FT_STATUS_INVALID_ARGUMENT, FT_STATUS_OK, FtStatus},
    native::macos::{IoSurface, SharedEventHandle, xpc::XpcAttachClient},
};

/// Opaque consumer-side attach: owns the XPC connection, the grant's handles,
/// and the mapped ring. Single-threaded, like the rest of the C ABI.
pub struct FtNativeAttach {
    // Keeps the XPC connection alive for the attach's lifetime. Steady-state
    // frame flow is shared-memory only and does not use it; holding it lets a
    // future revision observe connection invalidation (producer death).
    _client: XpcAttachClient,
    page: VideoTrackControlPage,
    // Owned here so their Drop releases the IOSurfaces; borrowed out as raw
    // pointers via `surface_ptrs` in the grant.
    _surfaces: Vec<IoSurface>,
    surface_ptrs: Vec<*mut c_void>,
    sync_handle: SharedEventHandle,
    consumer_id: u64,
    consumer_slot: u64,
    pool_id: u64,
    pool_slot_count: u32,
    fence_id: u64,
}

/// The one-time grant, as a C view. `surfaces` points at
/// `pool_slot_count` `IOSurfaceRef`s indexed by `slot_id`; `sync_handle` is an
/// `MTLSharedEventHandle`. Both are borrowed (see module lifetime note).
#[repr(C)]
pub struct FtNativeGrant {
    pub consumer_id: u64,
    pub consumer_slot: u64,
    pub pool_id: u64,
    pub fence_id: u64,
    pub surfaces: *const *mut c_void,
    pub sync_handle: *mut c_void,
    pub pool_slot_count: u32,
}

/// A validated latest-frame descriptor (drop-to-latest). The viewer GPU-waits
/// for `fence_value` on the grant's fence before sampling `surfaces[slot_id]`.
#[repr(C)]
pub struct FtNativeFrame {
    pub cursor: u64,
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub fence_value: u64,
    pub fence_id: u64,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub slot_id: u32,
    pub flags: u32,
}

const _: () = {
    assert!(std::mem::size_of::<FtNativeGrant>() == 56);
    assert!(std::mem::offset_of!(FtNativeGrant, surfaces) == 32);
    assert!(std::mem::offset_of!(FtNativeGrant, sync_handle) == 40);
    assert!(std::mem::offset_of!(FtNativeGrant, pool_slot_count) == 48);
    assert!(std::mem::size_of::<FtNativeFrame>() == 64);
    assert!(std::mem::offset_of!(FtNativeFrame, width) == 40);
    assert!(std::mem::offset_of!(FtNativeFrame, slot_id) == 52);
    assert!(std::mem::offset_of!(FtNativeFrame, flags) == 56);
};

impl FtNativeAttach {
    /// Build from an already-connected, already-authorized client by running
    /// the one-time attach and mapping the ring. Shared by the public
    /// connect-by-name entry point and the in-process endpoint test path.
    fn from_client(client: XpcAttachClient, consumer_id: u64) -> Result<Box<Self>, FtStatus> {
        let grant = client.attach(consumer_id).map_err(|_| FT_STATUS_ERROR)?;
        let page = VideoTrackControlPage::map_read_only(grant.ring_fd, grant.ring_map_len as usize).map_err(|_| FT_STATUS_ERROR)?;
        let surface_ptrs = grant.surface_handles.iter().map(IoSurface::as_raw).collect();
        Ok(Box::new(Self {
            _client: client,
            page,
            surface_ptrs,
            _surfaces: grant.surface_handles,
            sync_handle: grant.sync_handle,
            consumer_id: grant.consumer_id,
            consumer_slot: grant.consumer_slot,
            pool_id: grant.pool_id,
            pool_slot_count: grant.pool_slot_count,
            fence_id: grant.fence_id,
        }))
    }

    /// In-process attach over an anonymous endpoint, for tests (no launchd
    /// MachServices name needed).
    #[cfg(test)]
    fn connect_endpoint_for_test(
        endpoint: &crate::native::macos::xpc::XpcListenerEndpoint,
        bearer_token: Option<&str>,
        consumer_id: u64,
    ) -> Result<Box<Self>, FtStatus> {
        let client = XpcAttachClient::connect_endpoint(endpoint).map_err(|_| FT_STATUS_ERROR)?;
        if let Some(token) = bearer_token {
            client.authorize(token).map_err(|_| FT_STATUS_ERROR)?;
        }
        Self::from_client(client, consumer_id)
    }
}

/// Connect to the named XPC attach service, authorize with `bearer_token`
/// (NULL for an unauthenticated service), attach for `consumer_id`, and map
/// the ring.
///
/// # Safety
/// `mach_service_name` must be a valid NUL-terminated string. `bearer_token`
/// is either NULL or a valid NUL-terminated string. `out` must point to
/// writable storage for one pointer; the result is freed with
/// [`ft_native_attach_destroy`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_native_attach_connect(
    mach_service_name: *const c_char,
    bearer_token: *const c_char,
    consumer_id: u64,
    out: *mut *mut FtNativeAttach,
) -> FtStatus {
    if mach_service_name.is_null() || out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let Ok(name) = (unsafe { CStr::from_ptr(mach_service_name) }).to_str() else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let client = match XpcAttachClient::connect_named(name) {
        Ok(client) => client,
        Err(_) => return FT_STATUS_ERROR,
    };
    if !bearer_token.is_null() {
        let Ok(token) = (unsafe { CStr::from_ptr(bearer_token) }).to_str() else {
            return FT_STATUS_INVALID_ARGUMENT;
        };
        if client.authorize(token).is_err() {
            return FT_STATUS_ERROR;
        }
    }
    match FtNativeAttach::from_client(client, consumer_id) {
        Ok(attach) => {
            unsafe { *out = Box::into_raw(attach) };
            FT_STATUS_OK
        }
        Err(status) => status,
    }
}

/// Fill `out_grant` with a borrowed view of the attach's handles.
///
/// # Safety
/// `attach` must be a live pointer from [`ft_native_attach_connect`];
/// `out_grant` must point to writable storage for one [`FtNativeGrant`]. The
/// borrowed pointers are valid until [`ft_native_attach_destroy`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_native_attach_grant(attach: *const FtNativeAttach, out_grant: *mut FtNativeGrant) -> FtStatus {
    let Some(attach) = (unsafe { attach.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if out_grant.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    unsafe {
        *out_grant = FtNativeGrant {
            consumer_id: attach.consumer_id,
            consumer_slot: attach.consumer_slot,
            pool_id: attach.pool_id,
            fence_id: attach.fence_id,
            surfaces: attach.surface_ptrs.as_ptr(),
            sync_handle: attach.sync_handle.as_raw(),
            pool_slot_count: attach.pool_slot_count,
        };
    }
    FT_STATUS_OK
}

/// Load-acquire the latest published descriptor (drop-to-latest). Returns
/// `FT_STATUS_EMPTY` when the ring has no frames yet, `FT_STATUS_ERROR` on a
/// read fault (e.g. a config the reader can no longer resolve).
///
/// # Safety
/// `attach` must be a live pointer; `out_frame` must point to writable storage
/// for one [`FtNativeFrame`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_native_read_latest(attach: *const FtNativeAttach, out_frame: *mut FtNativeFrame) -> FtStatus {
    let Some(attach) = (unsafe { attach.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if out_frame.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    match attach.page.read_latest_lossy_entry() {
        Ok(Some(entry)) => {
            unsafe {
                *out_frame = FtNativeFrame {
                    cursor: entry.cursor,
                    sequence: entry.sequence,
                    timestamp_ns: entry.timestamp_ns,
                    fence_value: entry.fence_value,
                    fence_id: entry.fence_id,
                    width: entry.width,
                    height: entry.height,
                    pixel_format: entry.pixel_format,
                    slot_id: entry.slot_id,
                    flags: entry.flags,
                };
            }
            FT_STATUS_OK
        }
        Ok(None) => FT_STATUS_EMPTY,
        Err(_) => FT_STATUS_ERROR,
    }
}

/// Destroy the attach: drops the XPC connection and releases the grant's
/// IOSurfaces and sync handle. After this the grant's raw pointers are
/// invalid.
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

#[cfg(test)]
mod tests {
    use std::{
        ptr,
        sync::{Arc, Mutex},
    };

    use super::{FtNativeAttach, FtNativeFrame, FtNativeGrant, ft_native_attach_grant, ft_native_read_latest};
    use crate::{
        ffi::{FT_STATUS_EMPTY, FT_STATUS_OK},
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
        // Ring 4 / pool 6 so four rapid publishes each take a fresh slot —
        // the test exercises drop-to-latest, not pool exhaustion.
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
    fn native_ffi_attach_grant_and_drop_to_latest() {
        let producer = producer();
        let endpoint = AttachEndpoint::new(Some("pta_agent.ffi".to_string()));
        let (_server, listener) = XpcAttachServer::start_anonymous(endpoint, Arc::clone(&producer)).unwrap();

        let attach = FtNativeAttach::connect_endpoint_for_test(&listener, Some("pta_agent.ffi"), 5).unwrap();
        let attach_ptr = Box::into_raw(attach);

        // The grant view is complete and self-consistent.
        let mut grant = FtNativeGrant {
            consumer_id: 0,
            consumer_slot: 0,
            pool_id: 0,
            fence_id: 0,
            surfaces: ptr::null(),
            sync_handle: ptr::null_mut(),
            pool_slot_count: 0,
        };
        assert_eq!(unsafe { ft_native_attach_grant(attach_ptr, &mut grant) }, FT_STATUS_OK);
        assert_eq!(grant.consumer_id, 5);
        assert_eq!(grant.pool_slot_count, 6);
        assert!(!grant.surfaces.is_null());
        assert!(!grant.sync_handle.is_null());

        // Empty before any publish.
        let mut frame = blank_frame();
        assert_eq!(unsafe { ft_native_read_latest(attach_ptr, &mut frame) }, FT_STATUS_EMPTY);

        // Publish several; read_latest returns the newest (drop-to-latest),
        // never an older one, and never stalls the producer.
        for sequence in 1..=4u64 {
            producer.lock().unwrap().publish(&captured(sequence as u8), sequence).unwrap();
        }
        assert_eq!(unsafe { ft_native_read_latest(attach_ptr, &mut frame) }, FT_STATUS_OK);
        assert_eq!(frame.sequence, 4);
        assert_eq!(frame.width, WIDTH);
        assert_eq!(frame.height, HEIGHT);
        assert!(frame.slot_id < grant.pool_slot_count);

        unsafe { super::ft_native_attach_destroy(attach_ptr) };
    }

    fn blank_frame() -> FtNativeFrame {
        FtNativeFrame {
            cursor: 0,
            sequence: 0,
            timestamp_ns: 0,
            fence_value: 0,
            fence_id: 0,
            width: 0,
            height: 0,
            pixel_format: 0,
            slot_id: 0,
            flags: 0,
        }
    }
}
