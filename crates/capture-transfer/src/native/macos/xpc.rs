//! The macOS attach transport: NSXPC carrying the [`attach`] protocol
//! (ADR-0007). The protocol state machine ([`AttachEndpoint`]) stays
//! transport-neutral; this module is the wiring that moves typed handles —
//! IOSurfaces and the `MTLSharedEventHandle` cross the live connection as
//! objects, the ring fd as an `NSFileHandle`.
//!
//! Server side: [`XpcAttachServer`] hosts a listener (named when launchd
//! grants the process a `MachServices` name — portholed's case — or
//! anonymous for brokered/in-process introduction) and answers each
//! connection with the one-time grant. Client side: [`XpcAttachClient`]
//! connects, authorizes, attaches, and hands back an
//! [`AttachGrant<IoSurface, SharedEventHandle>`].
//!
//! [`attach`]: crate::native::attach

use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_char, c_void},
    os::fd::{FromRawFd, IntoRawFd, OwnedFd},
    ptr::NonNull,
    sync::{Arc, Mutex},
};

use crate::{
    error::{CaptureTransferError, Result},
    native::{
        NativeTrackProducer,
        attach::{AttachEndpoint, AttachError, AttachGrant, AttachRequest, AttachResponse, AttachSession},
        macos::{IoSurface, MacosFrameBackend, SharedEventHandle},
    },
};

#[repr(C)]
struct XpcGrant {
    consumer_slot: u64,
    ring_fd: i32,
    ring_map_len: u64,
    pool_id: u64,
    surface_count: u32,
    surfaces: *const *mut c_void,
    fence_id: u64,
    sync_handle: *mut c_void,
}

type AuthorizeCallback = extern "C" fn(*mut c_void, u64, *const c_char) -> i32;
type AttachCallback = extern "C" fn(*mut c_void, u64, u64, *mut XpcGrant) -> i32;
type GrantReleaseCallback = extern "C" fn(*mut c_void, u64, *mut XpcGrant);
type SessionEndedCallback = extern "C" fn(*mut c_void, u64);
type StateReleaseCallback = extern "C" fn(*mut c_void);

mod ffi {
    use std::ffi::{c_char, c_void};

    use super::{AttachCallback, AuthorizeCallback, GrantReleaseCallback, SessionEndedCallback, StateReleaseCallback};

    unsafe extern "C" {
        pub fn porthole_xpc_listener_start(
            mach_service_name: *const c_char,
            authorize_callback: AuthorizeCallback,
            attach_callback: AttachCallback,
            grant_release_callback: GrantReleaseCallback,
            session_ended_callback: SessionEndedCallback,
            state_release_callback: StateReleaseCallback,
            ctx: *mut c_void,
            out_listener: *mut *mut c_void,
        ) -> *mut c_char;
        pub fn porthole_xpc_listener_stop(listener: *mut c_void);
        pub fn porthole_xpc_listener_copy_endpoint(listener: *mut c_void) -> *mut c_void;
        pub fn porthole_xpc_endpoint_release(endpoint: *mut c_void);

        pub fn porthole_xpc_client_connect_name(mach_service_name: *const c_char, out_client: *mut *mut c_void) -> *mut c_char;
        pub fn porthole_xpc_client_connect_endpoint(endpoint: *mut c_void, out_client: *mut *mut c_void) -> *mut c_char;
        pub fn porthole_xpc_client_destroy(client: *mut c_void);
        pub fn porthole_xpc_client_authorize(client: *mut c_void, token: *const c_char, out_error: *mut *mut c_char) -> i32;
        #[allow(clippy::too_many_arguments)]
        pub fn porthole_xpc_client_attach(
            client: *mut c_void,
            consumer_id: u64,
            out_consumer_slot: *mut u64,
            out_ring_fd: *mut i32,
            out_ring_map_len: *mut u64,
            out_pool_id: *mut u64,
            out_surfaces: *mut *mut *mut c_void,
            out_surface_count: *mut u32,
            out_fence_id: *mut u64,
            out_sync_handle: *mut *mut c_void,
            out_error: *mut *mut c_char,
        ) -> i32;
        pub fn porthole_xpc_surface_array_free(surfaces: *mut *mut c_void);
    }
}

/// Wire status codes for [`AttachError`]; 0 is success, negative is a
/// transport failure (connection invalid, peer gone).
const STATUS_OK: i32 = 0;
const STATUS_NOT_AUTHORIZED: i32 = 1;
const STATUS_INVALID_TOKEN: i32 = 2;
const STATUS_ALREADY_ATTACHED: i32 = 3;
const STATUS_INVALID_CONSUMER_ID: i32 = 4;
const STATUS_GRANT_FAILED: i32 = 5;

fn status_from_error(error: &AttachError) -> i32 {
    match error {
        AttachError::NotAuthorized => STATUS_NOT_AUTHORIZED,
        AttachError::InvalidToken => STATUS_INVALID_TOKEN,
        AttachError::AlreadyAttached => STATUS_ALREADY_ATTACHED,
        AttachError::InvalidConsumerId => STATUS_INVALID_CONSUMER_ID,
        AttachError::Grant(_) => STATUS_GRANT_FAILED,
    }
}

fn error_from_status(status: i32, operation: &'static str) -> AttachError {
    match status {
        STATUS_NOT_AUTHORIZED => AttachError::NotAuthorized,
        STATUS_INVALID_TOKEN => AttachError::InvalidToken,
        STATUS_ALREADY_ATTACHED => AttachError::AlreadyAttached,
        STATUS_INVALID_CONSUMER_ID => AttachError::InvalidConsumerId,
        STATUS_GRANT_FAILED => AttachError::Grant(CaptureTransferError::NativeBackend {
            operation,
            message: "producer-side grant assembly failed".to_string(),
        }),
        other => AttachError::Grant(CaptureTransferError::NativeBackend {
            operation,
            message: format!("unexpected XPC status {other}"),
        }),
    }
}

/// Everything a granted-but-not-yet-replied attach keeps alive: the typed
/// handles whose raw pointers the shim is bridging into the reply, and the
/// pointer array itself.
struct GrantHold {
    _surfaces: Vec<IoSurface>,
    surface_ptrs: Vec<*mut c_void>,
    _sync_handle: SharedEventHandle,
}

struct ServerState {
    endpoint: AttachEndpoint,
    producer: Arc<Mutex<NativeTrackProducer<MacosFrameBackend>>>,
    sessions: Mutex<HashMap<u64, AttachSession>>,
    grant_holds: Mutex<HashMap<u64, GrantHold>>,
}

impl ServerState {
    fn handle(
        &self,
        session_id: u64,
        request: AttachRequest,
    ) -> std::result::Result<AttachResponse<IoSurface, SharedEventHandle>, AttachError> {
        // Lock order is sessions -> producer everywhere that takes both; keep
        // it so the two locks can never deadlock against a future path.
        let mut sessions = self.sessions.lock().expect("xpc session table poisoned");
        let session = sessions.entry(session_id).or_default();
        let mut producer = self.producer.lock().expect("native producer poisoned");
        self.endpoint.handle(session, &mut producer, request)
    }
}

extern "C" fn authorize_callback(ctx: *mut c_void, session_id: u64, token: *const c_char) -> i32 {
    let state = unsafe { &*(ctx.cast::<ServerState>()) };
    let token = unsafe { CStr::from_ptr(token) }.to_string_lossy().into_owned();
    match state.handle(session_id, AttachRequest::Authorize { bearer_token: token }) {
        Ok(_) => STATUS_OK,
        Err(error) => status_from_error(&error),
    }
}

extern "C" fn attach_callback(ctx: *mut c_void, session_id: u64, consumer_id: u64, out_grant: *mut XpcGrant) -> i32 {
    let state = unsafe { &*(ctx.cast::<ServerState>()) };
    let grant = match state.handle(session_id, AttachRequest::Attach { consumer_id }) {
        Ok(AttachResponse::Granted(grant)) => *grant,
        Ok(AttachResponse::Authorized) => unreachable!("attach request never yields Authorized"),
        Err(error) => return status_from_error(&error),
    };
    let AttachGrant {
        consumer_slot,
        ring_fd,
        ring_map_len,
        pool_id,
        surface_handles,
        fence_id,
        sync_handle,
        ..
    } = grant;
    let hold = GrantHold {
        surface_ptrs: surface_handles.iter().map(IoSurface::as_raw).collect(),
        _surfaces: surface_handles,
        _sync_handle: sync_handle,
    };
    let out = unsafe { &mut *out_grant };
    out.consumer_slot = consumer_slot;
    // The shim's NSFileHandle takes ownership (closeOnDealloc).
    out.ring_fd = ring_fd.into_raw_fd();
    out.ring_map_len = ring_map_len;
    out.pool_id = pool_id;
    out.surface_count = hold.surface_ptrs.len() as u32;
    out.surfaces = hold.surface_ptrs.as_ptr();
    out.fence_id = fence_id;
    out.sync_handle = hold._sync_handle.as_raw();
    state.grant_holds.lock().expect("xpc grant holds poisoned").insert(session_id, hold);
    STATUS_OK
}

extern "C" fn grant_release_callback(ctx: *mut c_void, session_id: u64, _grant: *mut XpcGrant) {
    let state = unsafe { &*(ctx.cast::<ServerState>()) };
    state.grant_holds.lock().expect("xpc grant holds poisoned").remove(&session_id);
}

extern "C" fn session_ended_callback(ctx: *mut c_void, session_id: u64) {
    let state = unsafe { &*(ctx.cast::<ServerState>()) };
    state.sessions.lock().expect("xpc session table poisoned").remove(&session_id);
    state.grant_holds.lock().expect("xpc grant holds poisoned").remove(&session_id);
}

extern "C" fn state_release_callback(ctx: *mut c_void) {
    drop(unsafe { Box::from_raw(ctx.cast::<ServerState>()) });
}

/// An anonymous listener's endpoint. In-process hand-off only (an endpoint
/// crosses processes only inside another live XPC connection — the broker
/// introduction path, out of scope until ad-hoc producers land).
#[derive(Debug)]
pub struct XpcListenerEndpoint {
    raw: NonNull<c_void>,
}

unsafe impl Send for XpcListenerEndpoint {}

impl Drop for XpcListenerEndpoint {
    fn drop(&mut self) {
        unsafe { ffi::porthole_xpc_endpoint_release(self.raw.as_ptr()) };
    }
}

/// The producer-side XPC service for one native track.
#[derive(Debug)]
pub struct XpcAttachServer {
    raw: NonNull<c_void>,
}

unsafe impl Send for XpcAttachServer {}

impl XpcAttachServer {
    /// Host the attach service under a launchd-registered `MachServices`
    /// name (portholed's path).
    pub fn start_named(name: &str, endpoint: AttachEndpoint, producer: Arc<Mutex<NativeTrackProducer<MacosFrameBackend>>>) -> Result<Self> {
        let name = CString::new(name).map_err(|_| CaptureTransferError::NativeBackend {
            operation: "xpc-listener-start",
            message: "mach service name contains NUL".to_string(),
        })?;
        Self::start(Some(&name), endpoint, producer)
    }

    /// Host on an anonymous listener and return its endpoint for in-process
    /// hand-off (tests; later, broker introduction).
    pub fn start_anonymous(
        endpoint: AttachEndpoint,
        producer: Arc<Mutex<NativeTrackProducer<MacosFrameBackend>>>,
    ) -> Result<(Self, XpcListenerEndpoint)> {
        let server = Self::start(None, endpoint, producer)?;
        let raw = unsafe { ffi::porthole_xpc_listener_copy_endpoint(server.raw.as_ptr()) };
        let endpoint = XpcListenerEndpoint {
            raw: NonNull::new(raw).expect("anonymous listener always has an endpoint"),
        };
        Ok((server, endpoint))
    }

    fn start(name: Option<&CStr>, endpoint: AttachEndpoint, producer: Arc<Mutex<NativeTrackProducer<MacosFrameBackend>>>) -> Result<Self> {
        let state = Box::into_raw(Box::new(ServerState {
            endpoint,
            producer,
            sessions: Mutex::new(HashMap::new()),
            grant_holds: Mutex::new(HashMap::new()),
        }));
        let mut raw: *mut c_void = std::ptr::null_mut();
        let error = unsafe {
            ffi::porthole_xpc_listener_start(
                name.map_or(std::ptr::null(), CStr::as_ptr),
                authorize_callback,
                attach_callback,
                grant_release_callback,
                session_ended_callback,
                state_release_callback,
                state.cast(),
                &mut raw,
            )
        };
        if let Err(start_error) = super::check("xpc-listener-start", error) {
            // The shim never took ownership of the state on failure.
            drop(unsafe { Box::from_raw(state) });
            return Err(start_error);
        }
        Ok(Self {
            raw: NonNull::new(raw).expect("shim returned NULL listener without error"),
        })
    }
}

impl Drop for XpcAttachServer {
    fn drop(&mut self) {
        // Invalidates the listener and all connections; the Rust state is
        // released by the shim once nothing in-flight can touch it.
        unsafe { ffi::porthole_xpc_listener_stop(self.raw.as_ptr()) };
    }
}

/// A consumer's connection to the attach service.
#[derive(Debug)]
pub struct XpcAttachClient {
    raw: NonNull<c_void>,
}

unsafe impl Send for XpcAttachClient {}

impl XpcAttachClient {
    /// Connect to the named service (the production path: portholed's
    /// `MachServices` name).
    pub fn connect_named(name: &str) -> Result<Self> {
        let name = CString::new(name).map_err(|_| CaptureTransferError::NativeBackend {
            operation: "xpc-connect",
            message: "mach service name contains NUL".to_string(),
        })?;
        let mut raw: *mut c_void = std::ptr::null_mut();
        super::check("xpc-connect", unsafe {
            ffi::porthole_xpc_client_connect_name(name.as_ptr(), &mut raw)
        })?;
        Ok(Self {
            raw: NonNull::new(raw).expect("shim returned NULL client without error"),
        })
    }

    /// Connect via an in-process anonymous endpoint.
    pub fn connect_endpoint(endpoint: &XpcListenerEndpoint) -> Result<Self> {
        let mut raw: *mut c_void = std::ptr::null_mut();
        super::check("xpc-connect", unsafe {
            ffi::porthole_xpc_client_connect_endpoint(endpoint.raw.as_ptr(), &mut raw)
        })?;
        Ok(Self {
            raw: NonNull::new(raw).expect("shim returned NULL client without error"),
        })
    }

    pub fn authorize(&self, token: &str) -> std::result::Result<(), AttachError> {
        let token = CString::new(token).map_err(|_| AttachError::InvalidToken)?;
        let mut error: *mut c_char = std::ptr::null_mut();
        let status = unsafe { ffi::porthole_xpc_client_authorize(self.raw.as_ptr(), token.as_ptr(), &mut error) };
        if let Err(transport) = super::check("xpc-authorize", error) {
            return Err(AttachError::Grant(transport));
        }
        if status == STATUS_OK {
            Ok(())
        } else {
            Err(error_from_status(status, "xpc-authorize"))
        }
    }

    /// The one-time handle transfer. After this returns, steady state is
    /// shared memory only.
    pub fn attach(&self, consumer_id: u64) -> std::result::Result<AttachGrant<IoSurface, SharedEventHandle>, AttachError> {
        let mut consumer_slot = 0u64;
        let mut ring_fd = -1i32;
        let mut ring_map_len = 0u64;
        let mut pool_id = 0u64;
        let mut surfaces: *mut *mut c_void = std::ptr::null_mut();
        let mut surface_count = 0u32;
        let mut fence_id = 0u64;
        let mut sync_handle: *mut c_void = std::ptr::null_mut();
        let mut error: *mut c_char = std::ptr::null_mut();
        let status = unsafe {
            ffi::porthole_xpc_client_attach(
                self.raw.as_ptr(),
                consumer_id,
                &mut consumer_slot,
                &mut ring_fd,
                &mut ring_map_len,
                &mut pool_id,
                &mut surfaces,
                &mut surface_count,
                &mut fence_id,
                &mut sync_handle,
                &mut error,
            )
        };
        if let Err(transport) = super::check("xpc-attach", error) {
            return Err(AttachError::Grant(transport));
        }
        if status != STATUS_OK {
            return Err(error_from_status(status, "xpc-attach"));
        }
        // Adopt everything the reply carried: a dup'd fd, +1 surfaces in a
        // malloc'd array, and the +1 sync handle.
        let surface_handles = (0..surface_count as usize)
            .map(|i| {
                let raw = unsafe { *surfaces.add(i) };
                unsafe { IoSurface::from_retained(NonNull::new(raw).expect("XPC reply surface is never NULL")) }
            })
            .collect();
        if !surfaces.is_null() {
            unsafe { ffi::porthole_xpc_surface_array_free(surfaces) };
        }
        Ok(AttachGrant {
            consumer_id,
            consumer_slot,
            ring_fd: unsafe { OwnedFd::from_raw_fd(ring_fd) },
            ring_map_len,
            pool_id,
            pool_slot_count: surface_count,
            surface_handles,
            fence_id,
            sync_handle: unsafe {
                SharedEventHandle::from_retained(NonNull::new(sync_handle).expect("XPC reply sync handle is never NULL"))
            },
        })
    }
}

impl Drop for XpcAttachClient {
    fn drop(&mut self) {
        unsafe { ffi::porthole_xpc_client_destroy(self.raw.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{XpcAttachClient, XpcAttachServer};
    use crate::{
        control_page::VideoTrackControlPage,
        model::{ClockDomain, ColorSpace, PixelFormat},
        native::{
            NativeStreamParams, NativeTrackProducer, PoolExhaustionPolicy,
            attach::{AttachEndpoint, AttachError},
            macos::{ConsumerFence, IoSurface, MacosCapturedFrame, MacosFrameBackend, MetalContext},
        },
    };

    const WIDTH: u32 = 64;
    const HEIGHT: u32 = 48;

    fn producer() -> Arc<Mutex<NativeTrackProducer<MacosFrameBackend>>> {
        let backend = MacosFrameBackend::new().expect("Metal device required for XPC tests");
        let params = NativeStreamParams {
            width: WIDTH,
            height: HEIGHT,
            pixel_format: PixelFormat::Bgra8Unorm,
            color_space: ColorSpace::Srgb,
            clock_domain: ClockDomain::HostTime,
            modifier: 0,
        };
        Arc::new(Mutex::new(
            NativeTrackProducer::new(backend, params, 2, 3, PoolExhaustionPolicy::Fail).unwrap(),
        ))
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
    fn grant_crosses_a_real_xpc_connection_and_frames_flow_shared_memory_only() {
        let producer = producer();
        let endpoint = AttachEndpoint::new(Some("pta_agent.secret".to_string()));
        let (_server, listener_endpoint) = XpcAttachServer::start_anonymous(endpoint, Arc::clone(&producer)).unwrap();

        let client = XpcAttachClient::connect_endpoint(&listener_endpoint).unwrap();
        client.authorize("pta_agent.secret").unwrap();
        let grant = client.attach(7).unwrap();
        assert_eq!(grant.consumer_id, 7);
        assert_eq!(grant.pool_slot_count, 3);
        assert_eq!(grant.surface_handles.len(), 3);

        // The grant's artifacts are complete: map the ring from the
        // transferred fd, resolve the fence from the transferred handle.
        let ring = VideoTrackControlPage::map_read_only(grant.ring_fd, grant.ring_map_len as usize).unwrap();
        let metal = MetalContext::new().unwrap();
        let fence = ConsumerFence::from_handle(&metal, &grant.sync_handle).unwrap();

        // Steady state: publish on the producer side, observe via shared
        // memory + transferred surfaces only. The XPC channel is idle.
        for sequence in 1..=3u64 {
            producer.lock().unwrap().publish(&captured(sequence as u8), sequence).unwrap();
            let entry = ring.read_latest_lossy_entry().unwrap().unwrap();
            assert_eq!(entry.sequence, sequence);
            assert_eq!(entry.pool_id, grant.pool_id);
            assert!(fence.wait(entry.fence_value, 5_000), "frame {sequence} fence not signalled");
            let surface = &grant.surface_handles[entry.slot_id as usize];
            let mut pixels = vec![0u8; WIDTH as usize * HEIGHT as usize * 4];
            surface.read_pixels(&mut pixels).unwrap();
            assert_eq!(pixels, gradient(sequence as u8), "frame {sequence} pixels wrong");
        }
    }

    #[test]
    fn wrong_token_is_rejected_over_xpc() {
        let producer = producer();
        let endpoint = AttachEndpoint::new(Some("pta_agent.secret".to_string()));
        let (_server, listener_endpoint) = XpcAttachServer::start_anonymous(endpoint, producer).unwrap();

        let client = XpcAttachClient::connect_endpoint(&listener_endpoint).unwrap();
        assert!(matches!(client.authorize("pta_agent.wrong"), Err(AttachError::InvalidToken)));
        assert!(matches!(client.attach(7), Err(AttachError::NotAuthorized)));
    }

    #[test]
    fn second_attach_on_the_same_connection_is_rejected() {
        let producer = producer();
        let (_server, listener_endpoint) = XpcAttachServer::start_anonymous(AttachEndpoint::new(None), producer).unwrap();
        let client = XpcAttachClient::connect_endpoint(&listener_endpoint).unwrap();
        let _grant = client.attach(7).unwrap();
        assert!(matches!(client.attach(7), Err(AttachError::AlreadyAttached)));
    }

    #[test]
    fn a_new_connection_is_a_new_session_and_reuses_the_consumer_slot() {
        let producer = producer();
        let (_server, listener_endpoint) = XpcAttachServer::start_anonymous(AttachEndpoint::new(None), producer).unwrap();

        let first = XpcAttachClient::connect_endpoint(&listener_endpoint).unwrap();
        let first_grant = first.attach(7).unwrap();
        drop(first);

        // The crashed-consumer recovery path: reconnect, re-attach, same slot.
        let second = XpcAttachClient::connect_endpoint(&listener_endpoint).unwrap();
        let second_grant = second.attach(7).unwrap();
        assert_eq!(second_grant.consumer_slot, first_grant.consumer_slot);
    }
}
