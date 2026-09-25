//! D3D11 arena setup over a Local Endpoint named pipe (Wheelhouse ADR 0011),
//! the native counterpart of [`crate::acquisition::socket`].
//!
//! Per consumer incarnation and per pool generation, the producer duplicates
//! each slot texture's NT handle and its fence handle into the verified peer
//! process, which acknowledges receipt ([`crate::local::send_handles`]).
//! Frames never use this channel: the arena's shared ring carries each frame's
//! slot and fence value. The consumer registers its own shared release fence
//! once; the producer duplicates it out of the consumer process (it holds that
//! process open with `PROCESS_DUP_HANDLE` since accept) and waits on it with
//! `SetEventOnCompletion`.
//!
//! Attachment names the consumer's adapter. Shared D3D11 resources are only
//! importable on the adapter that owns them, so a consumer on another adapter
//! is refused with [`Refusal::AdapterMismatch`] before any admission; it may
//! use a CPU publication instead. There are no cross-adapter copies.

use std::{
    fmt,
    os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle},
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::{
    Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE},
    System::Threading::GetCurrentProcess,
};

use super::{AdapterInfo, AdapterLuid, D3d11Device, D3d11Fence, D3d11FrameBackend, SharedFenceHandle, SharedTextureHandle};
use crate::{
    acquisition::{
        IncarnationId,
        arena::{
            ArenaConsumer, ArenaError, ConfigurationDescriptor, ConfigurationGrant, ConfigurationInstall, ConsumerGrant,
            ConsumerReleaseTimeline, GrantDescriptor, ReleaseTimelineRegistration,
        },
        socket::{SocketError, read_framed, receive_objects, send_objects, write_framed},
    },
    local::{Access, Stream},
    native::arena::{NativeArenaGrant, NativeArenaProducer},
};

const MAGIC: &[u8; 8] = b"JSD3D001";

/// A shared D3D11 producer, as the setup server and the capture session share it.
pub type D3d11Producer = Arc<Mutex<NativeArenaProducer<D3d11FrameBackend>>>;

/// Why a producer declined a consumer before admitting it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Refusal {
    /// The consumer's device is on another adapter than the frames.
    AdapterMismatch { producer: AdapterInfo, consumer: AdapterLuid },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AdapterMismatch { producer, consumer } => write!(
                f,
                "adapter mismatch: frames live on adapter {} ({}), the consumer device is on adapter {consumer}; \
                 import on the producer's adapter or use a CPU publication",
                producer.luid, producer.description
            ),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error(transparent)]
    Socket(#[from] SocketError),
    #[error("D3D11 setup refused: {0}")]
    Refused(Refusal),
}

impl From<ArenaError> for SetupError {
    fn from(error: ArenaError) -> Self {
        Self::Socket(error.into())
    }
}

impl From<std::io::Error> for SetupError {
    fn from(error: std::io::Error) -> Self {
        Self::Socket(error.into())
    }
}

impl From<crate::CaptureTransferError> for SetupError {
    fn from(error: crate::CaptureTransferError) -> Self {
        Self::Socket(error.into())
    }
}

/// What a producer tells a consumer before attachment, so the consumer can
/// create its device on the right adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProducerDescription {
    pub adapter: AdapterInfo,
}

/// The native half of a grant; the arena half is its descriptor.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct NativeSetup {
    pool_id: u64,
    fence_id: u64,
    adapter: AdapterLuid,
    slots: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Describe,
    Attach { holding: u32, adapter: AdapterLuid },
    Configuration,
    RegisterRelease { handle: u64 },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Response {
    Described {
        producer: ProducerDescription,
    },
    Attached {
        descriptor: GrantDescriptor,
        native: NativeSetup,
    },
    Configuration {
        descriptor: ConfigurationDescriptor,
        native: NativeSetup,
    },
    Empty,
    Registered {
        registration: ReleaseTimelineRegistration,
    },
    Refused {
        refusal: Refusal,
    },
    Rejected {
        message: String,
    },
}

/// Least access for each native object in the consumer: it reads textures
/// and waits on the fence. `ID3D11Fence` shared handles only support
/// `GENERIC_ALL`, which the duplicate keeps.
const TEXTURE_ACCESS: Access = Access::Rights(::windows::Win32::Graphics::Dxgi::DXGI_SHARED_RESOURCE_READ.0);
const FENCE_ACCESS: Access = Access::Same;

struct Session {
    producer: D3d11Producer,
    incarnation: Option<IncarnationId>,
}

type Native = (Vec<SharedTextureHandle>, SharedFenceHandle);

enum Reply {
    Plain(Response),
    Grant(Response, Vec<OwnedHandle>, Native),
}

impl Session {
    fn reply(&mut self, request: Request, stream: &Stream) -> Result<Reply, SetupError> {
        match request {
            Request::Describe => {
                let producer = self.producer.lock().map_err(|_| SocketError::Protocol("producer mutex poisoned"))?;
                Ok(Reply::Plain(Response::Described {
                    producer: ProducerDescription {
                        adapter: producer.backend().device().adapter().clone(),
                    },
                }))
            }
            Request::Attach { holding, adapter } => {
                if self.incarnation.is_some() {
                    return Err(SocketError::Protocol("connection already attached").into());
                }
                let peer = stream.peer_process()?;
                let mut producer = self.producer.lock().map_err(|_| SocketError::Protocol("producer mutex poisoned"))?;
                let own = producer.backend().device().adapter().clone();
                if adapter != own.luid {
                    return Ok(Reply::Plain(Response::Refused {
                        refusal: Refusal::AdapterMismatch {
                            producer: own,
                            consumer: adapter,
                        },
                    }));
                }
                let grant = producer.attach_process_handle(holding, peer.as_handle())?;
                self.incarnation = Some(grant.consumer.incarnation());
                let native = NativeSetup {
                    pool_id: grant.pool_id,
                    fence_id: grant.fence_id,
                    adapter: own.luid,
                    slots: grant.surface_handles.len() as u32,
                };
                let NativeArenaGrant {
                    surface_handles,
                    sync_handle,
                    consumer,
                    ..
                } = grant;
                let (descriptor, handles) = consumer.into_parts()?;
                Ok(Reply::Grant(
                    Response::Attached { descriptor, native },
                    handles.into(),
                    (surface_handles, sync_handle),
                ))
            }
            Request::Configuration => {
                let incarnation = self.incarnation.ok_or(SocketError::Protocol("configuration requires attachment"))?;
                let mut producer = self.producer.lock().map_err(|_| SocketError::Protocol("producer mutex poisoned"))?;
                let adapter = producer.backend().device().luid();
                let Some(grant) = producer.configuration_offer(incarnation)? else {
                    return Ok(Reply::Plain(Response::Empty));
                };
                let native = NativeSetup {
                    pool_id: grant.pool_id,
                    fence_id: grant.fence_id,
                    adapter,
                    slots: grant.surface_handles.len() as u32,
                };
                let NativeArenaGrant {
                    surface_handles,
                    sync_handle,
                    consumer,
                    ..
                } = grant;
                let (descriptor, handle) = consumer.into_parts()?;
                Ok(Reply::Grant(
                    Response::Configuration { descriptor, native },
                    vec![handle],
                    (surface_handles, sync_handle),
                ))
            }
            Request::RegisterRelease { handle } => {
                let incarnation = self
                    .incarnation
                    .ok_or(SocketError::Protocol("release registration requires attachment"))?;
                let peer = stream.peer_process()?;
                let handle = duplicate_from(&peer, handle)?;
                let device = Arc::clone(
                    self.producer
                        .lock()
                        .map_err(|_| SocketError::Protocol("producer mutex poisoned"))?
                        .backend()
                        .device(),
                );
                // Opening validates the object: any other handle the peer
                // names fails here and its duplicate closes.
                let timeline = Arc::new(device.open_fence(&SharedFenceHandle::from_owned(handle))?);
                let registration = self
                    .producer
                    .lock()
                    .map_err(|_| SocketError::Protocol("producer mutex poisoned"))?
                    .register_release_timeline(incarnation, timeline)?;
                Ok(Reply::Plain(Response::Registered { registration }))
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(incarnation) = self.incarnation
            && let Ok(mut producer) = self.producer.lock()
        {
            // EOF is closure, not proof that this process or its GPU work ended.
            let _ = producer.close(incarnation);
        }
    }
}

/// Copy a handle the peer names in its own process. The peer keeps its handle.
fn duplicate_from(peer: &OwnedHandle, value: u64) -> Result<OwnedHandle, SetupError> {
    if value == 0 {
        return Err(SocketError::Protocol("invalid release fence handle").into());
    }
    let mut duplicate: HANDLE = std::ptr::null_mut();
    // SAFETY: the peer process handle is live with PROCESS_DUP_HANDLE; the
    // value is only interpreted inside that process by the kernel.
    let duplicated = unsafe {
        DuplicateHandle(
            peer.as_raw_handle(),
            value as usize as HANDLE,
            GetCurrentProcess(),
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if duplicated == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: DuplicateHandle returned a fresh handle in this process.
    Ok(unsafe { OwnedHandle::from_raw_handle(duplicate) })
}

fn send_native(stream: &mut Stream, (textures, fence): Native) -> Result<(), SetupError> {
    let mut objects: Vec<_> = textures.into_iter().map(|texture| (texture.into_owned(), TEXTURE_ACCESS)).collect();
    objects.push((fence.into_owned(), FENCE_ACCESS));
    crate::local::send_handles(stream, objects)?;
    Ok(())
}

/// Serve D3D11 setup for one host-selected producer until EOF or error.
/// Call only after the host's routing and authorization. Admission binds to
/// the process the pipe reports as its peer; handles go to that process only.
pub fn serve_d3d11(mut stream: Stream, producer: D3d11Producer) -> Result<(), SetupError> {
    stream.set_nonblocking(false)?;
    let mut session = Session {
        producer,
        incarnation: None,
    };
    while let Some(request) = read_framed(&mut stream, MAGIC)? {
        match session.reply(request, &stream) {
            Ok(Reply::Plain(response)) => write_framed(&mut stream, MAGIC, &response)?,
            Ok(Reply::Grant(response, arena, native)) => {
                // No I/O holds the producer lock. Arena objects go first; the
                // consumer adopts both batches before building its grant.
                write_framed(&mut stream, MAGIC, &response)?;
                send_objects(&mut stream, arena)?;
                send_native(&mut stream, native)?;
            }
            Err(error) => write_framed(
                &mut stream,
                MAGIC,
                &Response::Rejected {
                    message: error.to_string(),
                },
            )?,
        }
    }
    Ok(())
}

/// The consumer end of [`serve_d3d11`].
#[derive(Debug)]
pub struct D3d11SetupClient {
    stream: Stream,
    identity: Option<(IncarnationId, [u8; 16])>,
    adapter: Option<AdapterLuid>,
    failed: bool,
}

impl D3d11SetupClient {
    /// Use a connection whose host routing and authorization have finished.
    ///
    /// # Safety
    /// As [`crate::acquisition::socket::CpuSetupClient::from_stream`]: the peer
    /// must be the conforming sole producer, verified by
    /// [`crate::local::connect`] (transferred handle values are adopted), and
    /// this process the sole recipient of its grants.
    pub unsafe fn from_stream(stream: Stream) -> Self {
        Self {
            stream,
            identity: None,
            adapter: None,
            failed: false,
        }
    }

    /// Whether the producer still holds its end of setup.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        !self.failed && crate::local::is_alive(&self.stream)
    }

    fn fail(&mut self) {
        self.failed = true;
        self.stream.shutdown();
    }

    fn request(&mut self, request: &Request) -> Result<Response, SetupError> {
        if self.failed {
            return Err(SocketError::Protocol("connection failed").into());
        }
        let result = (|| {
            write_framed(&mut self.stream, MAGIC, request)?;
            read_framed(&mut self.stream, MAGIC)?.ok_or(SocketError::Protocol("missing reply"))
        })();
        match result {
            Ok(Response::Rejected { message }) => Err(SocketError::Rejected(message).into()),
            Ok(Response::Refused { refusal }) => Err(SetupError::Refused(refusal)),
            Ok(response) => Ok(response),
            Err(error) => {
                self.fail();
                Err(error.into())
            }
        }
    }

    /// The producer's adapter, to choose the device that will import frames.
    pub fn describe(&mut self) -> Result<ProducerDescription, SetupError> {
        match self.request(&Request::Describe)? {
            Response::Described { producer } => Ok(producer),
            _ => {
                self.fail();
                Err(SocketError::Protocol("unexpected describe reply").into())
            }
        }
    }

    fn receive_native(&mut self, native: NativeSetup, resources: u32) -> Result<Native, SetupError> {
        let mut handles = crate::local::receive_handles(&mut self.stream, native.slots as usize + 1)?;
        if native.slots != resources || Some(native.adapter) != self.adapter {
            return Err(SocketError::Protocol("native setup disagrees with its grant").into());
        }
        let fence = SharedFenceHandle::from_owned(handles.pop().expect("fence handle"));
        Ok((handles.into_iter().map(SharedTextureHandle::from_owned).collect(), fence))
    }

    /// Attach with a holding reservation, importing on `device`'s adapter.
    /// A producer on another adapter refuses with [`Refusal::AdapterMismatch`]
    /// and the connection stays usable (for example to describe it).
    pub fn attach(&mut self, holding: u32, device: &D3d11Device) -> Result<ArenaConsumer, SetupError> {
        if self.identity.is_some() {
            return Err(SocketError::Protocol("connection already attached").into());
        }
        self.adapter = Some(device.luid());
        let response = self.request(&Request::Attach {
            holding,
            adapter: device.luid(),
        })?;
        let result = (|| {
            let Response::Attached { descriptor, native } = response else {
                return Err(SocketError::Protocol("unexpected attach reply").into());
            };
            if descriptor.payload_capacity != 0 {
                return Err(SocketError::Protocol("D3D11 setup cannot import CPU resources").into());
            }
            let arena: [OwnedHandle; 5] = receive_objects(&mut self.stream, 5)?
                .try_into()
                .map_err(|_| SocketError::Protocol("initial setup requires five handles"))?;
            let (surface_handles, sync_handle) = self.receive_native(native, descriptor.resources)?;
            // SAFETY: from_stream's sole-producer contract; send_handles closed
            // the producer's copies before the values reached this process.
            let consumer = unsafe { ConsumerGrant::from_parts(descriptor, arena) }?;
            Ok::<_, SetupError>(
                NativeArenaGrant {
                    consumer,
                    pool_id: native.pool_id,
                    surface_handles,
                    fence_id: native.fence_id,
                    sync_handle,
                }
                .into_consumer()?,
            )
        })();
        match result {
            Ok(consumer) => {
                self.identity = Some((consumer.incarnation(), consumer.claim_scope()));
                Ok(consumer)
            }
            Err(error) => {
                self.fail();
                Err(error)
            }
        }
    }

    fn check(&self, consumer: &ArenaConsumer) -> Result<(), SetupError> {
        if self.identity != Some((consumer.incarnation(), consumer.claim_scope())) {
            return Err(SocketError::Protocol("consumer belongs to another connection").into());
        }
        Ok(())
    }

    /// Install a replacement pool after `AcquireOutcome::Reconfiguration`.
    pub fn install_configuration(&mut self, consumer: &mut ArenaConsumer) -> Result<Option<ConfigurationInstall>, SetupError> {
        self.check(consumer)?;
        if consumer.is_configured() {
            return Ok(None);
        }
        let response = self.request(&Request::Configuration)?;
        let result = (|| match response {
            Response::Empty => Ok(None),
            Response::Configuration { descriptor, native } => {
                if descriptor.payload_capacity != 0 {
                    return Err(SocketError::Protocol("invalid D3D11 configuration resources").into());
                }
                let section = receive_objects(&mut self.stream, 1)?.pop().expect("one handle");
                let (surface_handles, sync_handle) = self.receive_native(native, descriptor.resources)?;
                // SAFETY: same sole producer, process and incarnation as attachment.
                let grant = unsafe { ConfigurationGrant::from_parts(consumer, descriptor, section) }?;
                Ok(Some(
                    NativeArenaGrant {
                        consumer: grant,
                        pool_id: native.pool_id,
                        surface_handles,
                        fence_id: native.fence_id,
                        sync_handle,
                    }
                    .install(consumer)?,
                ))
            }
            _ => Err(SocketError::Protocol("unexpected configuration reply").into()),
        })();
        if result.is_err() {
            self.fail();
        }
        result
    }

    /// Register this consumer's shared release fence once, before deferring
    /// releases to it. The fence must be signalled only by GPU work (or CPU
    /// code) that has finished every use of the frames released to it.
    pub fn register_release_timeline(
        &mut self,
        consumer: &ArenaConsumer,
        fence: Arc<D3d11Fence>,
    ) -> Result<ConsumerReleaseTimeline, SetupError> {
        self.check(consumer)?;
        // The producer duplicates this handle out of this process during the
        // request; it stays open here until the reply arrives.
        let handle = fence.export_handle()?;
        let response = self.request(&Request::RegisterRelease {
            handle: handle.as_handle().as_raw_handle() as usize as u64,
        })?;
        drop(handle);
        let Response::Registered { registration } = response else {
            self.fail();
            return Err(SocketError::Protocol("unexpected release registration reply").into());
        };
        Ok(consumer.bind_release_timeline(&registration, fence)?)
    }
}

impl Drop for D3d11SetupClient {
    fn drop(&mut self) {
        self.fail();
    }
}
