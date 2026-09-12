//! Process-bound acquisition setup over NSXPC. Authorization belongs to the
//! host; mappings, surfaces, and readiness handles feed the common arena.
//! This channel carries no per-frame acquisition or release requests.

use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_void},
    ptr::NonNull,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};

use crate::{
    acquisition::{
        IncarnationId,
        arena::{
            ArenaConsumer, ArenaError, ConfigurationDescriptor, ConfigurationGrant, ConfigurationInstall, ConsumerGrant,
            ConsumerReleaseTimeline, GrantDescriptor, ReleaseTimelineRegistration,
        },
    },
    native::{
        arena::{NativeArenaGrant, NativeArenaProducer},
        macos::{ConsumerFence, MacosFrameBackend, MetalContext, SharedEventHandle},
    },
};
mod wire;
use wire::{Envelope, Payload, ffi};

type Producer = Arc<Mutex<NativeArenaProducer<MacosFrameBackend>>>;

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Request {
    Authorize { token: String },
    Attach { holding: u32 },
    Configuration,
    RegisterRelease,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Response {
    Authorized,
    Empty,
    Registered {
        registration: ReleaseTimelineRegistration,
    },
    Attached {
        descriptor: GrantDescriptor,
        pool_id: u64,
        fence_id: u64,
    },
    Configuration {
        descriptor: ConfigurationDescriptor,
        pool_id: u64,
        fence_id: u64,
    },
    Error {
        message: String,
    },
}

fn failure(message: impl Into<String>) -> ArenaError {
    crate::CaptureTransferError::NativeBackend {
        operation: "acquisition-xpc",
        message: message.into(),
    }
    .into()
}

struct Session {
    pid: u32,
    authorized: bool,
    incarnation: Option<IncarnationId>,
}
struct ServerState {
    token: Option<String>,
    producer: Producer,
    sessions: Mutex<HashMap<u64, Session>>,
}

impl ServerState {
    fn handle(&self, identity: u64, pid: u32, request: Payload<Request>) -> Result<Envelope, ArenaError> {
        if !request.fds.is_empty()
            || !request.surfaces.is_empty()
            || request.event.is_some() != matches!(&request.metadata, Request::RegisterRelease)
        {
            return Err(ArenaError::Mapping("unexpected acquisition request resources"));
        }
        // The connection table and producer always use this lock order.
        let mut sessions = self.sessions.lock().expect("XPC acquisition sessions");
        let session = sessions.entry(identity).or_insert_with(|| Session {
            pid,
            authorized: self.token.is_none(),
            incarnation: None,
        });
        if session.pid != pid || pid == 0 {
            return Err(ArenaError::Mapping("XPC peer process changed"));
        }
        match request.metadata {
            Request::Authorize { token } => {
                if session.incarnation.is_some() {
                    return Err(failure("connection already attached"));
                }
                if self.token.as_ref().is_some_and(|expected| *expected != token) {
                    return Err(failure("authorization rejected"));
                }
                session.authorized = true;
                Envelope::new(&Response::Authorized, &[], &[], None)
            }
            Request::Attach { holding } => {
                if !session.authorized {
                    return Err(failure("attach requires authorization"));
                }
                if session.incarnation.is_some() {
                    return Err(failure("connection already attached"));
                }
                let mut producer = self.producer.lock().expect("XPC acquisition producer");
                let grant = producer.attach_process(holding, pid)?;
                session.incarnation = Some(grant.consumer.incarnation());
                let (descriptor, fds) = grant.consumer.into_parts()?;
                Envelope::new(
                    &Response::Attached {
                        descriptor,
                        pool_id: grant.pool_id,
                        fence_id: grant.fence_id,
                    },
                    &fds,
                    &grant.surface_handles,
                    Some(&grant.sync_handle),
                )
            }
            Request::Configuration => {
                let incarnation = session.incarnation.ok_or_else(|| failure("configuration requires attachment"))?;
                let mut producer = self.producer.lock().expect("XPC acquisition producer");
                let Some(grant) = producer.configuration_offer(incarnation)? else {
                    return Envelope::new(&Response::Empty, &[], &[], None);
                };
                let (descriptor, fd) = grant.consumer.into_parts()?;
                Envelope::new(
                    &Response::Configuration {
                        descriptor,
                        pool_id: grant.pool_id,
                        fence_id: grant.fence_id,
                    },
                    &[fd],
                    &grant.surface_handles,
                    Some(&grant.sync_handle),
                )
            }
            Request::RegisterRelease => {
                let incarnation = session
                    .incarnation
                    .ok_or_else(|| failure("release registration requires attachment"))?;
                let handle = request.event.expect("release registration event checked");
                let metal = MetalContext::new()?;
                let timeline = Arc::new(ConsumerFence::from_handle(&metal, &handle)?);
                let registration = self
                    .producer
                    .lock()
                    .expect("XPC acquisition producer")
                    .register_release_timeline(incarnation, timeline)?;
                Envelope::new(&Response::Registered { registration }, &[], &[], None)
            }
        }
    }

    fn close(&self, identity: u64) {
        let mut sessions = self.sessions.lock().expect("XPC acquisition sessions");
        if let Some(Session {
            incarnation: Some(incarnation),
            ..
        }) = sessions.remove(&identity)
        {
            // EOF closes acquisition. Only the arena's mapping, completion,
            // and kernel process-lifetime proofs can return reservation credit.
            let _ = self.producer.lock().expect("XPC acquisition producer").close(incarnation);
        }
    }
}

extern "C" fn request_callback(context: *mut c_void, identity: u64, pid: u32, request: *mut c_void) -> *mut c_void {
    let state = unsafe { &*context.cast::<ServerState>() };
    let raw = unsafe { ffi::jsa_object_retain(request) };
    let envelope = Envelope(NonNull::new(raw).expect("XPC request object"));
    let reply = envelope.decode().and_then(|request| state.handle(identity, pid, request));
    match reply {
        Ok(reply) => reply.into_raw(),
        Err(error) => Envelope::new(
            &Response::Error {
                message: error.to_string(),
            },
            &[],
            &[],
            None,
        )
        .map_or(std::ptr::null_mut(), Envelope::into_raw),
    }
}
extern "C" fn close_callback(context: *mut c_void, identity: u64) {
    unsafe { &*context.cast::<ServerState>() }.close(identity);
}
extern "C" fn destroy_callback(context: *mut c_void) {
    drop(unsafe { Box::from_raw(context.cast::<ServerState>()) });
}

#[derive(Debug)]
pub struct XpcArenaEndpoint {
    raw: NonNull<c_void>,
}
unsafe impl Send for XpcArenaEndpoint {}
impl Drop for XpcArenaEndpoint {
    fn drop(&mut self) {
        unsafe { ffi::jsa_object_release(self.raw.as_ptr()) };
    }
}

/// One host-authorized native track. A named service needs its launchd
/// MachServices entry; anonymous endpoints can be handed over an existing XPC
/// connection or directly to another component in the same process.
#[derive(Debug)]
pub struct XpcArenaServer {
    raw: NonNull<c_void>,
}
unsafe impl Send for XpcArenaServer {}
impl XpcArenaServer {
    pub fn start_anonymous(token: Option<String>, producer: Producer) -> Result<(Self, XpcArenaEndpoint), ArenaError> {
        let server = Self::start(None, token, producer);
        let raw = unsafe { ffi::jsa_server_endpoint(server.raw.as_ptr()) };
        Ok((
            server,
            XpcArenaEndpoint {
                raw: NonNull::new(raw).expect("anonymous XPC endpoint"),
            },
        ))
    }
    pub fn start_named(name: &str, token: Option<String>, producer: Producer) -> Result<Self, ArenaError> {
        let name = CString::new(name).map_err(|_| failure("Mach service name contains NUL"))?;
        Ok(Self::start(Some(&name), token, producer))
    }
    fn start(name: Option<&CStr>, token: Option<String>, producer: Producer) -> Self {
        let state = Box::into_raw(Box::new(ServerState {
            token,
            producer,
            sessions: Mutex::new(HashMap::new()),
        }));
        let raw = unsafe {
            ffi::jsa_server_start(
                name.map_or(std::ptr::null(), CStr::as_ptr),
                state.cast(),
                request_callback,
                close_callback,
                destroy_callback,
            )
        };
        Self {
            raw: NonNull::new(raw).expect("XPC acquisition listener"),
        }
    }
}
impl Drop for XpcArenaServer {
    fn drop(&mut self) {
        unsafe { ffi::jsa_server_stop(self.raw.as_ptr()) };
    }
}

#[derive(Debug)]
pub struct XpcArenaClient {
    raw: NonNull<c_void>,
    incarnation: Option<IncarnationId>,
    claim_scope: Option<[u8; 16]>,
}
unsafe impl Send for XpcArenaClient {}
impl XpcArenaClient {
    pub fn connect_endpoint(endpoint: &XpcArenaEndpoint) -> Result<Self, ArenaError> {
        let raw = unsafe { ffi::jsa_client_connect(std::ptr::null(), endpoint.raw.as_ptr()) };
        Self::from_raw(raw)
    }
    /// Connect to a host-selected acquisition service.
    ///
    /// # Safety
    /// The caller must trust the service to be a conforming sole producer:
    /// shared storage and descriptors must obey the acquisition lifetime
    /// protocol. Client authorization alone does not authenticate the server.
    pub unsafe fn connect_named(name: &str) -> Result<Self, ArenaError> {
        let name = CString::new(name).map_err(|_| failure("Mach service name contains NUL"))?;
        Self::from_raw(unsafe { ffi::jsa_client_connect(name.as_ptr(), std::ptr::null_mut()) })
    }
    fn from_raw(raw: *mut c_void) -> Result<Self, ArenaError> {
        Ok(Self {
            raw: NonNull::new(raw).ok_or_else(|| failure("could not create acquisition connection"))?,
            incarnation: None,
            claim_scope: None,
        })
    }
    fn request(&self, request: Request, event: Option<&SharedEventHandle>) -> Result<Payload<Response>, ArenaError> {
        let input = Envelope::new(&request, &[], &[], event)?;
        let mut output = std::ptr::null_mut();
        super::super::check("acquisition-xpc", unsafe {
            ffi::jsa_client_request(self.raw.as_ptr(), input.0.as_ptr(), &mut output)
        })?;
        let reply: Payload<Response> = Envelope(NonNull::new(output).ok_or_else(|| failure("missing acquisition reply"))?).decode()?;
        if let Response::Error { message } = &reply.metadata {
            return Err(failure(message.clone()));
        }
        Ok(reply)
    }
    pub fn authorize(&mut self, token: &str) -> Result<(), ArenaError> {
        let reply = self.request(Request::Authorize { token: token.to_owned() }, None)?;
        if !matches!(reply.metadata, Response::Authorized) || !reply.fds.is_empty() || !reply.surfaces.is_empty() || reply.event.is_some() {
            return Err(ArenaError::Mapping("unexpected authorization reply"));
        }
        Ok(())
    }
    pub fn attach(&mut self, holding: u32) -> Result<ArenaConsumer, ArenaError> {
        if self.incarnation.is_some() {
            return Err(failure("connection already attached"));
        }
        let reply = self.request(Request::Attach { holding }, None)?;
        let Response::Attached {
            descriptor,
            pool_id,
            fence_id,
        } = reply.metadata
        else {
            return Err(ArenaError::Mapping("unexpected acquisition attach reply"));
        };
        if reply.surfaces.len() != descriptor.resources as usize {
            return Err(ArenaError::Mapping("native setup surface count mismatch"));
        }
        let sync_handle = reply.event.ok_or(ArenaError::Mapping("missing native readiness handle"))?;
        let fds = reply
            .fds
            .try_into()
            .map_err(|_| ArenaError::Mapping("acquisition attach requires five FDs"))?;
        // SAFETY: the XPC peer is this connection's authorized sole producer;
        // its callback bound the grant to the kernel-provided peer PID before
        // transferring these single-use mappings. This client never forwards
        // grants, and from_parts validates the receiving process and scopes.
        let consumer = unsafe { ConsumerGrant::from_parts(descriptor, fds) }?;
        let consumer = NativeArenaGrant {
            consumer,
            pool_id,
            surface_handles: reply.surfaces,
            fence_id,
            sync_handle,
        }
        .into_consumer()?;
        self.incarnation = Some(consumer.incarnation());
        self.claim_scope = Some(consumer.claim_scope());
        Ok(consumer)
    }

    fn check_consumer(&self, consumer: &ArenaConsumer) -> Result<(), ArenaError> {
        if self.incarnation != Some(consumer.incarnation()) || self.claim_scope != Some(consumer.claim_scope()) {
            return Err(ArenaError::Mapping("consumer belongs to another XPC acquisition connection"));
        }
        Ok(())
    }

    pub fn install_configuration(&mut self, consumer: &mut ArenaConsumer) -> Result<Option<ConfigurationInstall>, ArenaError> {
        self.check_consumer(consumer)?;
        let reply = self.request(Request::Configuration, None)?;
        if matches!(reply.metadata, Response::Empty) && reply.fds.is_empty() && reply.surfaces.is_empty() && reply.event.is_none() {
            return Ok(None);
        }
        let Response::Configuration {
            descriptor,
            pool_id,
            fence_id,
        } = reply.metadata
        else {
            return Err(ArenaError::Mapping("unexpected configuration reply"));
        };
        if reply.surfaces.len() != descriptor.resources as usize || reply.fds.len() != 1 {
            return Err(ArenaError::Mapping("configuration setup resource count mismatch"));
        }
        let sync_handle = reply.event.ok_or(ArenaError::Mapping("missing replacement readiness handle"))?;
        let fd = reply.fds.into_iter().next().expect("one replacement FD");
        // SAFETY: this is the same conforming XPC producer and process-bound
        // incarnation as initial setup. The envelope's extra copies are gone;
        // the single-use offer is neither forwarded nor replayed.
        let grant = unsafe { ConfigurationGrant::from_parts(consumer, descriptor, fd) }?;
        NativeArenaGrant {
            consumer: grant,
            pool_id,
            surface_handles: reply.surfaces,
            fence_id,
            sync_handle,
        }
        .install(consumer)
        .map(Some)
    }

    /// Register one actual consumer completion source during setup. Reuse the
    /// returned binding for frame releases; no XPC request is made per frame.
    pub fn register_release_timeline(
        &mut self,
        consumer: &ArenaConsumer,
        timeline: Arc<ConsumerFence>,
    ) -> Result<ConsumerReleaseTimeline, ArenaError> {
        self.check_consumer(consumer)?;
        let event = timeline.export_handle()?;
        let reply = self.request(Request::RegisterRelease, Some(&event))?;
        if !reply.fds.is_empty() || !reply.surfaces.is_empty() || reply.event.is_some() {
            return Err(ArenaError::Mapping("unexpected release registration resources"));
        }
        let Response::Registered { registration } = reply.metadata else {
            return Err(ArenaError::Mapping("unexpected release registration reply"));
        };
        consumer.bind_release_timeline(&registration, timeline)
    }
}
impl Drop for XpcArenaClient {
    fn drop(&mut self) {
        unsafe { ffi::jsa_client_close(self.raw.as_ptr()) };
    }
}
