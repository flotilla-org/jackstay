//! Linux native attach transport foundation.
//!
//! ADR-0009 uses UDS + `SCM_RIGHTS` on Linux. The JSON message describes
//! handles by index into the ancillary fd table; after receive, the library
//! owns those fds and can expose borrowed fd numbers through the C grant.

#[cfg(all(target_os = "linux", feature = "backend-linux"))]
pub mod dmabuf;
#[cfg(all(target_os = "linux", feature = "backend-linux"))]
pub mod drm;
#[cfg(all(target_os = "linux", feature = "backend-linux"))]
pub mod pipewire;
#[cfg(all(target_os = "linux", feature = "backend-linux"))]
pub mod synthetic;
#[cfg(all(target_os = "linux", feature = "backend-linux"))]
pub mod vulkan;

use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    os::{
        fd::{AsFd, AsRawFd, OwnedFd, RawFd},
        unix::net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{
    error::{CaptureTransferError, Result},
    fdpass::{recv_fds, send_fds},
    native::{
        NativeFrameBackend, NativeTrackProducer,
        attach::{AttachEndpoint, AttachError, AttachGrant, AttachPool, AttachRequest, AttachResponse, AttachSession},
        lease::{NativeLeaseError, NativeLeaseIdentity, NativeLeaseRelease},
    },
};

pub const LINUX_ATTACH_MAX_PLANES: usize = 4;
pub const LINUX_ATTACH_MAX_FDS: usize = 253;
/// Upper bound on one newline-delimited attach message. The largest real
/// message is a granted pool descriptor, a few KiB; the cap only exists so a
/// peer that never sends a newline cannot grow the read buffer without bound
/// while it holds the serve loop.
pub const LINUX_ATTACH_MAX_JSON_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LinuxAttachRequest {
    Authorize { bearer_token: String },
    Attach { consumer_id: u64 },
    GetPool { pool_id: u64 },
    AcquireLease { identity: LinuxLeaseIdentity },
    RegisterReleaseSync { sync: LinuxSyncDescriptor },
    ReleaseFrame { release: LinuxReleaseDescriptor },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LinuxAttachResponse {
    Authorized,
    Granted { grant: LinuxAttachGrant },
    Pool { pool: LinuxPoolDescriptor },
    LeaseAcquired { lease_id: u64 },
    ReleaseSyncRegistered { release_sync_id: u64 },
    FrameReleased,
    Error { code: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinuxAttachGrant {
    pub consumer_id: u64,
    pub consumer_slot: u64,
    pub ring_fd_index: u32,
    pub ring_map_len: u64,
    pub pools: Vec<LinuxPoolDescriptor>,
    pub producer_sync: LinuxSyncDescriptor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinuxPoolDescriptor {
    pub pool_id: u64,
    pub surfaces: Vec<LinuxSurfaceDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinuxSurfaceDescriptor {
    pub handle_kind: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub modifier: u64,
    pub planes: Vec<LinuxPlaneDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinuxPlaneDescriptor {
    pub fd_index: u32,
    pub offset: u32,
    pub stride: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinuxSyncDescriptor {
    pub sync_kind: u32,
    pub sync_id: u64,
    pub fd_index: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinuxLeaseIdentity {
    pub cursor: u64,
    pub sequence: u64,
    pub pool_id: u64,
    pub slot_id: u32,
}

impl From<LinuxLeaseIdentity> for NativeLeaseIdentity {
    fn from(identity: LinuxLeaseIdentity) -> Self {
        Self {
            cursor: identity.cursor,
            sequence: identity.sequence,
            pool_id: identity.pool_id,
            slot_id: identity.slot_id,
        }
    }
}

impl From<NativeLeaseIdentity> for LinuxLeaseIdentity {
    fn from(identity: NativeLeaseIdentity) -> Self {
        Self {
            cursor: identity.cursor,
            sequence: identity.sequence,
            pool_id: identity.pool_id,
            slot_id: identity.slot_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "release_kind", rename_all = "snake_case")]
pub enum LinuxReleaseDescriptor {
    Now { lease_id: u64 },
    TimelineValue { lease_id: u64, release_sync_id: u64, value: u64 },
}

impl LinuxReleaseDescriptor {
    #[must_use]
    pub fn lease_id(self) -> u64 {
        match self {
            Self::Now { lease_id } | Self::TimelineValue { lease_id, .. } => lease_id,
        }
    }

    #[must_use]
    pub fn release(self) -> NativeLeaseRelease {
        match self {
            Self::Now { .. } => NativeLeaseRelease::Now,
            Self::TimelineValue {
                release_sync_id, value, ..
            } => NativeLeaseRelease::TimelineValue { release_sync_id, value },
        }
    }
}

#[derive(Debug)]
pub struct LinuxDmabufPlaneHandle {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

#[derive(Debug)]
pub struct LinuxSurfaceHandle {
    pub handle_kind: u32,
    pub width: u32,
    pub height: u32,
    pub pixel_format: u32,
    pub modifier: u64,
    pub planes: Vec<LinuxDmabufPlaneHandle>,
}

#[derive(Debug)]
pub struct LinuxSyncHandle {
    pub sync_kind: u32,
    pub sync_id: u64,
    pub fd: OwnedFd,
}

pub trait LinuxNativeLeaseBackend {
    fn acquire_linux_lease(&mut self, identity: NativeLeaseIdentity) -> Result<u64>;
    fn register_linux_release_sync(&mut self, sync: LinuxSyncDescriptor, fd: OwnedFd) -> Result<u64>;
    fn release_linux_lease(&mut self, lease_id: u64, release: NativeLeaseRelease) -> Result<()>;
}

pub struct LinuxAttachServer<B>
where
    B: NativeFrameBackend<SurfaceHandle = LinuxSurfaceHandle, SyncHandle = LinuxSyncHandle> + LinuxNativeLeaseBackend + Send + 'static,
    B::SurfacePool: Send,
    B::Fence: Send,
{
    path: PathBuf,
    running: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    _producer: Arc<Mutex<NativeTrackProducer<B>>>,
}

impl<B> LinuxAttachServer<B>
where
    B: NativeFrameBackend<SurfaceHandle = LinuxSurfaceHandle, SyncHandle = LinuxSyncHandle> + LinuxNativeLeaseBackend + Send + 'static,
    B::SurfacePool: Send,
    B::Fence: Send,
{
    pub fn start(path: impl AsRef<Path>, endpoint: AttachEndpoint, producer: Arc<Mutex<NativeTrackProducer<B>>>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| CaptureTransferError::FdPassing {
                operation: "linux-attach-listen",
                message: error.to_string(),
            })?;
        }
        let listener = UnixListener::bind(&path).map_err(|error| CaptureTransferError::FdPassing {
            operation: "linux-attach-listen",
            message: error.to_string(),
        })?;
        listener.set_nonblocking(true).map_err(|error| CaptureTransferError::FdPassing {
            operation: "linux-attach-listen",
            message: error.to_string(),
        })?;
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);
        let thread_producer = Arc::clone(&producer);
        let thread = thread::Builder::new()
            .name("porthole-linux-native-attach".to_string())
            .spawn(move || {
                while thread_running.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            serve_linux_attach_stream(&stream, &endpoint, &thread_producer);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
            .map_err(|error| CaptureTransferError::FdPassing {
                operation: "linux-attach-listen",
                message: error.to_string(),
            })?;
        Ok(Self {
            path,
            running,
            thread: Some(thread),
            _producer: producer,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl<B> Drop for LinuxAttachServer<B>
where
    B: NativeFrameBackend<SurfaceHandle = LinuxSurfaceHandle, SyncHandle = LinuxSyncHandle> + LinuxNativeLeaseBackend + Send + 'static,
    B::SurfacePool: Send,
    B::Fence: Send,
{
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = UnixStream::connect(&self.path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
pub struct LinuxAttachStreamSession {
    attach: AttachSession,
    next_release_sync_id: u64,
    release_syncs: HashMap<u64, LinuxRegisteredReleaseSync>,
}

#[derive(Debug)]
struct LinuxRegisteredReleaseSync {
    descriptor: LinuxSyncDescriptor,
    _fds: LinuxFdTable,
}

impl LinuxAttachStreamSession {
    #[must_use]
    pub fn new() -> Self {
        Self {
            attach: AttachSession::default(),
            next_release_sync_id: 1,
            release_syncs: HashMap::new(),
        }
    }

    #[must_use]
    pub fn registered_release_sync(&self, release_sync_id: u64) -> Option<&LinuxSyncDescriptor> {
        self.release_syncs.get(&release_sync_id).map(|sync| &sync.descriptor)
    }
}

impl Default for LinuxAttachStreamSession {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxAttachGrant {
    pub fn validate_fd_indices(&self, fd_count: usize) -> Result<()> {
        validate_fd_index("ring-fd", self.ring_fd_index, fd_count)?;
        self.producer_sync.validate_fd_indices(fd_count)?;
        for pool in &self.pools {
            for surface in &pool.surfaces {
                surface.validate_fd_indices(fd_count)?;
            }
        }
        Ok(())
    }
}

impl LinuxSurfaceDescriptor {
    pub fn validate_fd_indices(&self, fd_count: usize) -> Result<()> {
        if self.planes.is_empty() || self.planes.len() > LINUX_ATTACH_MAX_PLANES {
            return Err(CaptureTransferError::FdPassing {
                operation: "validate-linux-attach",
                message: format!("surface has {} planes; expected 1..={LINUX_ATTACH_MAX_PLANES}", self.planes.len()),
            });
        }
        for plane in &self.planes {
            validate_fd_index("dmabuf-plane", plane.fd_index, fd_count)?;
        }
        Ok(())
    }
}

impl LinuxSyncDescriptor {
    pub fn validate_fd_indices(&self, fd_count: usize) -> Result<()> {
        validate_fd_index("sync", self.fd_index, fd_count)
    }
}

fn validate_fd_index(kind: &'static str, fd_index: u32, fd_count: usize) -> Result<()> {
    if (fd_index as usize) < fd_count {
        return Ok(());
    }
    Err(CaptureTransferError::FdPassing {
        operation: "validate-linux-attach",
        message: format!("{kind} fd_index {fd_index} outside fd table length {fd_count}"),
    })
}

#[derive(Debug)]
pub struct LinuxFdTable {
    fds: Vec<OwnedFd>,
}

impl LinuxFdTable {
    #[must_use]
    pub fn new(fds: Vec<OwnedFd>) -> Self {
        Self { fds }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.fds.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fds.is_empty()
    }

    pub fn raw_fd(&self, index: u32) -> Result<RawFd> {
        self.fds
            .get(index as usize)
            .map(AsRawFd::as_raw_fd)
            .ok_or_else(|| CaptureTransferError::FdPassing {
                operation: "linux-fd-table",
                message: format!("fd_index {index} outside fd table length {}", self.fds.len()),
            })
    }

    pub fn try_clone_fd(&self, index: u32) -> Result<OwnedFd> {
        self.fds
            .get(index as usize)
            .ok_or_else(|| CaptureTransferError::FdPassing {
                operation: "linux-fd-table",
                message: format!("fd_index {index} outside fd table length {}", self.fds.len()),
            })?
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| CaptureTransferError::FdPassing {
                operation: "clone-linux-fd",
                message: error.to_string(),
            })
    }

    pub fn extend(&mut self, fds: impl IntoIterator<Item = OwnedFd>) {
        self.fds.extend(fds);
    }

    #[must_use]
    pub fn into_fds(self) -> Vec<OwnedFd> {
        self.fds
    }
}

pub fn send_json<T: Serialize>(stream: &UnixStream, message: &T) -> Result<()> {
    let mut line = serde_json::to_vec(message).map_err(|error| CaptureTransferError::FdPassing {
        operation: "encode-linux-attach",
        message: error.to_string(),
    })?;
    line.push(b'\n');
    let mut stream = stream;
    stream.write_all(&line).map_err(|error| CaptureTransferError::FdPassing {
        operation: "write-linux-attach",
        message: error.to_string(),
    })
}

pub fn recv_json<T: for<'de> Deserialize<'de>>(stream: &UnixStream) -> Result<T> {
    let line = read_json_line(stream)?;
    serde_json::from_str(&line).map_err(|error| CaptureTransferError::FdPassing {
        operation: "decode-linux-attach",
        message: error.to_string(),
    })
}

pub fn send_json_with_fds<T: Serialize>(stream: &UnixStream, message: &T, fds: &[RawFd]) -> Result<()> {
    send_fds(stream, fds)?;
    send_json(stream, message)
}

pub fn send_json_then_fds<T: Serialize>(stream: &UnixStream, message: &T, fds: &[RawFd]) -> Result<()> {
    send_json(stream, message)?;
    send_fds(stream, fds)
}

pub fn recv_json_with_fds<T: for<'de> Deserialize<'de>>(stream: &UnixStream, max_fds: usize) -> Result<(T, LinuxFdTable)> {
    let fds = recv_fds(stream, max_fds)?;
    let line = read_json_line(stream)?;
    let message = serde_json::from_str(&line).map_err(|error| CaptureTransferError::FdPassing {
        operation: "decode-linux-attach",
        message: error.to_string(),
    })?;
    Ok((message, LinuxFdTable::new(fds)))
}

pub fn serve_linux_attach_stream<B>(stream: &UnixStream, endpoint: &AttachEndpoint, producer: &Arc<Mutex<NativeTrackProducer<B>>>)
where
    B: NativeFrameBackend<SurfaceHandle = LinuxSurfaceHandle, SyncHandle = LinuxSyncHandle> + LinuxNativeLeaseBackend,
{
    let mut session = LinuxAttachStreamSession::new();
    while let Ok(request) = recv_json::<LinuxAttachRequest>(stream) {
        let passed_fds = match recv_linux_attach_request_fds(stream, &request) {
            Ok(fds) => fds,
            Err(_) => break,
        };
        let outbound = {
            let mut producer = match producer.lock() {
                Ok(producer) => producer,
                Err(_) => break,
            };
            match build_linux_attach_response(endpoint, &mut session, &mut producer, request, passed_fds) {
                Ok(outbound) => outbound,
                Err(_) => break,
            }
        };
        if outbound.send(stream).is_err() {
            break;
        }
    }
}

pub fn handle_linux_attach_stream_message<B>(
    stream: &UnixStream,
    endpoint: &AttachEndpoint,
    session: &mut LinuxAttachStreamSession,
    producer: &mut NativeTrackProducer<B>,
) -> Result<()>
where
    B: NativeFrameBackend<SurfaceHandle = LinuxSurfaceHandle, SyncHandle = LinuxSyncHandle> + LinuxNativeLeaseBackend,
{
    let request = recv_json::<LinuxAttachRequest>(stream)?;
    let passed_fds = recv_linux_attach_request_fds(stream, &request)?;
    build_linux_attach_response(endpoint, session, producer, request, passed_fds)?.send(stream)
}

struct LinuxAttachOutbound {
    response: LinuxAttachResponse,
    fds: Vec<OwnedFd>,
}

impl LinuxAttachOutbound {
    fn json(response: LinuxAttachResponse) -> Self {
        Self { response, fds: Vec::new() }
    }

    fn with_fds(response: LinuxAttachResponse, fds: Vec<OwnedFd>) -> Self {
        Self { response, fds }
    }

    fn send(self, stream: &UnixStream) -> Result<()> {
        if self.fds.is_empty() {
            send_json(stream, &self.response)
        } else {
            let raw_fds = self.fds.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>();
            send_json_with_fds(stream, &self.response, &raw_fds)
        }
    }
}

fn recv_linux_attach_request_fds(stream: &UnixStream, request: &LinuxAttachRequest) -> Result<Option<Vec<OwnedFd>>> {
    match request {
        LinuxAttachRequest::RegisterReleaseSync { .. } => recv_fds(stream, LINUX_ATTACH_MAX_FDS).map(Some),
        _ => Ok(None),
    }
}

fn build_linux_attach_response<B>(
    endpoint: &AttachEndpoint,
    session: &mut LinuxAttachStreamSession,
    producer: &mut NativeTrackProducer<B>,
    request: LinuxAttachRequest,
    passed_fds: Option<Vec<OwnedFd>>,
) -> Result<LinuxAttachOutbound>
where
    B: NativeFrameBackend<SurfaceHandle = LinuxSurfaceHandle, SyncHandle = LinuxSyncHandle> + LinuxNativeLeaseBackend,
{
    // Handshake requests enforce their own ordering inside
    // `AttachEndpoint::handle`. Everything else operates on producer state a
    // session only legitimately reaches through its grant, so require a
    // completed attach unconditionally — otherwise a peer that skips the
    // handshake could probe pools or release another consumer's lease.
    if !matches!(request, LinuxAttachRequest::Authorize { .. } | LinuxAttachRequest::Attach { .. }) && !session.attach.is_attached() {
        return Ok(LinuxAttachOutbound::json(LinuxAttachResponse::Error {
            code: "not_authorized".to_string(),
            message: "attach stream operations require a completed attach".to_string(),
        }));
    }
    match request {
        LinuxAttachRequest::Authorize { bearer_token } => {
            match endpoint.handle(&mut session.attach, producer, AttachRequest::Authorize { bearer_token }) {
                Ok(AttachResponse::Authorized) => Ok(LinuxAttachOutbound::json(LinuxAttachResponse::Authorized)),
                Ok(AttachResponse::Granted(_)) => Ok(LinuxAttachOutbound::json(LinuxAttachResponse::Error {
                    code: "invalid_state".to_string(),
                    message: "authorize unexpectedly returned an attach grant".to_string(),
                })),
                Err(error) => Ok(LinuxAttachOutbound::json(linux_attach_error_response(error))),
            }
        }
        LinuxAttachRequest::Attach { consumer_id } => {
            match endpoint.handle(&mut session.attach, producer, AttachRequest::Attach { consumer_id }) {
                Ok(AttachResponse::Granted(grant)) => {
                    let encoded = encode_linux_attach_grant(*grant)?;
                    Ok(LinuxAttachOutbound::with_fds(
                        LinuxAttachResponse::Granted { grant: encoded.grant },
                        encoded.fds,
                    ))
                }
                Ok(AttachResponse::Authorized) => Ok(LinuxAttachOutbound::json(LinuxAttachResponse::Error {
                    code: "invalid_state".to_string(),
                    message: "attach unexpectedly returned authorization only".to_string(),
                })),
                Err(error) => Ok(LinuxAttachOutbound::json(linux_attach_error_response(error))),
            }
        }
        LinuxAttachRequest::GetPool { pool_id } => match producer.export_pool(pool_id) {
            Ok(pool) => {
                let encoded = encode_linux_attach_pool(pool)?;
                Ok(LinuxAttachOutbound::with_fds(
                    LinuxAttachResponse::Pool { pool: encoded.pool },
                    encoded.fds,
                ))
            }
            Err(CaptureTransferError::NativeBackend { .. }) => Ok(LinuxAttachOutbound::json(LinuxAttachResponse::Error {
                code: "not_found".to_string(),
                message: format!("pool id {pool_id} is not active or recently retired"),
            })),
            Err(error) => Ok(LinuxAttachOutbound::json(linux_native_error_response(error))),
        },
        LinuxAttachRequest::AcquireLease { identity } => match producer.backend_mut().acquire_linux_lease(identity.into()) {
            Ok(lease_id) => Ok(LinuxAttachOutbound::json(LinuxAttachResponse::LeaseAcquired { lease_id })),
            Err(error) => Ok(LinuxAttachOutbound::json(linux_native_error_response(error))),
        },
        LinuxAttachRequest::RegisterReleaseSync { sync } => {
            let fds = passed_fds.ok_or_else(|| CaptureTransferError::FdPassing {
                operation: "linux-register-release-sync",
                message: "register release sync request did not include passed fds".to_string(),
            })?;
            if let Err(error) = sync.validate_fd_indices(fds.len()) {
                return Ok(LinuxAttachOutbound::json(LinuxAttachResponse::Error {
                    code: "invalid_argument".to_string(),
                    message: error.to_string(),
                }));
            }
            let mut fds = LinuxFdTable::new(fds).into_fds();
            let fd = fds.swap_remove(sync.fd_index as usize);
            let release_sync_id = match producer.backend_mut().register_linux_release_sync(sync.clone(), fd) {
                Ok(release_sync_id) => release_sync_id,
                Err(error) => return Ok(LinuxAttachOutbound::json(linux_native_error_response(error))),
            };
            session.next_release_sync_id = session.next_release_sync_id.max(release_sync_id.saturating_add(1));
            session.release_syncs.insert(
                release_sync_id,
                LinuxRegisteredReleaseSync {
                    descriptor: sync,
                    _fds: LinuxFdTable::new(fds),
                },
            );
            Ok(LinuxAttachOutbound::json(LinuxAttachResponse::ReleaseSyncRegistered {
                release_sync_id,
            }))
        }
        LinuxAttachRequest::ReleaseFrame { release } => {
            let lease_id = release.lease_id();
            if let LinuxReleaseDescriptor::TimelineValue { release_sync_id, .. } = release
                && session.registered_release_sync(release_sync_id).is_none()
            {
                return Ok(LinuxAttachOutbound::json(LinuxAttachResponse::Error {
                    code: "invalid_state".to_string(),
                    message: format!("release sync id {release_sync_id} is not registered on this attach stream"),
                }));
            }
            match producer.backend_mut().release_linux_lease(lease_id, release.release()) {
                Ok(()) => Ok(LinuxAttachOutbound::json(LinuxAttachResponse::FrameReleased)),
                Err(error) => Ok(LinuxAttachOutbound::json(linux_native_error_response(error))),
            }
        }
    }
}

fn linux_attach_error_response(error: AttachError) -> LinuxAttachResponse {
    let code = match error {
        AttachError::NotAuthorized => "not_authorized",
        AttachError::InvalidToken => "invalid_token",
        AttachError::AlreadyAttached => "already_attached",
        AttachError::Grant(_) => "grant_failed",
    };
    LinuxAttachResponse::Error {
        code: code.to_string(),
        message: error.to_string(),
    }
}

fn linux_native_error_response(error: CaptureTransferError) -> LinuxAttachResponse {
    LinuxAttachResponse::Error {
        code: "native_backend".to_string(),
        message: error.to_string(),
    }
}

impl From<NativeLeaseError> for CaptureTransferError {
    fn from(error: NativeLeaseError) -> Self {
        Self::NativeBackend {
            operation: "linux-native-lease",
            message: error.to_string(),
        }
    }
}

struct EncodedLinuxAttachGrant {
    grant: LinuxAttachGrant,
    fds: Vec<OwnedFd>,
}

impl EncodedLinuxAttachGrant {}

struct EncodedLinuxAttachPool {
    pool: LinuxPoolDescriptor,
    fds: Vec<OwnedFd>,
}

impl EncodedLinuxAttachPool {}

fn encode_linux_attach_grant(grant: AttachGrant<LinuxSurfaceHandle, LinuxSyncHandle>) -> Result<EncodedLinuxAttachGrant> {
    let mut fds = Vec::new();
    let ring_fd_index = push_fd(&mut fds, grant.ring_fd)?;
    let pool = encode_linux_pool_descriptor(grant.pool_id, grant.surface_handles, &mut fds)?;
    let producer_sync = LinuxSyncDescriptor {
        sync_kind: grant.sync_handle.sync_kind,
        sync_id: grant.sync_handle.sync_id,
        fd_index: push_fd(&mut fds, grant.sync_handle.fd)?,
    };
    let encoded = LinuxAttachGrant {
        consumer_id: grant.consumer_id,
        consumer_slot: grant.consumer_slot,
        ring_fd_index,
        ring_map_len: grant.ring_map_len,
        pools: vec![pool],
        producer_sync,
    };
    encoded.validate_fd_indices(fds.len())?;
    Ok(EncodedLinuxAttachGrant { grant: encoded, fds })
}

fn encode_linux_attach_pool(pool: AttachPool<LinuxSurfaceHandle>) -> Result<EncodedLinuxAttachPool> {
    if pool.pool_slot_count as usize != pool.surface_handles.len() {
        return Err(CaptureTransferError::FdPassing {
            operation: "encode-linux-attach-pool",
            message: format!(
                "pool {} declares {} slots but exports {} surfaces",
                pool.pool_id,
                pool.pool_slot_count,
                pool.surface_handles.len()
            ),
        });
    }
    let mut fds = Vec::new();
    let pool = encode_linux_pool_descriptor(pool.pool_id, pool.surface_handles, &mut fds)?;
    for surface in &pool.surfaces {
        surface.validate_fd_indices(fds.len())?;
    }
    Ok(EncodedLinuxAttachPool { pool, fds })
}

fn encode_linux_pool_descriptor(
    pool_id: u64,
    surface_handles: Vec<LinuxSurfaceHandle>,
    fds: &mut Vec<OwnedFd>,
) -> Result<LinuxPoolDescriptor> {
    let mut surfaces = Vec::with_capacity(surface_handles.len());
    for surface in surface_handles {
        if surface.planes.is_empty() || surface.planes.len() > LINUX_ATTACH_MAX_PLANES {
            return Err(CaptureTransferError::FdPassing {
                operation: "encode-linux-attach-pool",
                message: format!(
                    "surface has {} planes; expected 1..={LINUX_ATTACH_MAX_PLANES}",
                    surface.planes.len()
                ),
            });
        }
        let mut planes = Vec::with_capacity(surface.planes.len());
        for plane in surface.planes {
            planes.push(LinuxPlaneDescriptor {
                fd_index: push_fd(fds, plane.fd)?,
                offset: plane.offset,
                stride: plane.stride,
            });
        }
        surfaces.push(LinuxSurfaceDescriptor {
            handle_kind: surface.handle_kind,
            width: surface.width,
            height: surface.height,
            pixel_format: surface.pixel_format,
            modifier: surface.modifier,
            planes,
        });
    }
    Ok(LinuxPoolDescriptor { pool_id, surfaces })
}

fn push_fd(fds: &mut Vec<OwnedFd>, fd: OwnedFd) -> Result<u32> {
    if fds.len() >= LINUX_ATTACH_MAX_FDS {
        return Err(CaptureTransferError::FdPassing {
            operation: "encode-linux-attach-grant",
            message: format!("fd table cannot exceed {LINUX_ATTACH_MAX_FDS} fds"),
        });
    }
    let index = fds.len() as u32;
    fds.push(fd);
    Ok(index)
}

fn read_json_line(stream: &UnixStream) -> Result<String> {
    let mut stream = stream;
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let read = stream.read(&mut byte).map_err(|error| CaptureTransferError::FdPassing {
            operation: "read-linux-attach",
            message: error.to_string(),
        })?;
        if read == 0 {
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        if line.len() >= LINUX_ATTACH_MAX_JSON_BYTES {
            return Err(CaptureTransferError::FdPassing {
                operation: "read-linux-attach",
                message: format!("attach message exceeds {LINUX_ATTACH_MAX_JSON_BYTES} bytes without a newline"),
            });
        }
        line.push(byte[0]);
    }
    if line.is_empty() {
        return Err(CaptureTransferError::FdPassing {
            operation: "read-linux-attach",
            message: "empty attach message".to_string(),
        });
    }
    String::from_utf8(line).map_err(|error| CaptureTransferError::FdPassing {
        operation: "decode-linux-attach",
        message: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        io::{Read, Seek, SeekFrom, Write},
        os::{
            fd::{AsFd, AsRawFd},
            unix::net::UnixStream,
        },
        sync::{Arc, Mutex},
    };

    use crate::{
        error::{CaptureTransferError, Result},
        model::{ClockDomain, ColorSpace, PayloadKind, PixelFormat},
        native::{
            NativeFrameBackend, NativeStreamParams, NativeTrackProducer, PoolExhaustionPolicy, SlotClaim, SlotReuseCandidate,
            attach::AttachEndpoint,
            lease::{NativeLeaseBook, NativeLeaseIdentity, NativeLeaseRelease},
            linux::{
                LinuxAttachGrant, LinuxAttachRequest, LinuxAttachResponse, LinuxAttachServer, LinuxAttachStreamSession,
                LinuxDmabufPlaneHandle, LinuxLeaseIdentity, LinuxNativeLeaseBackend, LinuxPlaneDescriptor, LinuxPoolDescriptor,
                LinuxReleaseDescriptor, LinuxSurfaceDescriptor, LinuxSurfaceHandle, LinuxSyncDescriptor, LinuxSyncHandle,
                handle_linux_attach_stream_message, recv_json, recv_json_with_fds, send_json, send_json_then_fds, send_json_with_fds,
            },
        },
    };

    fn grant() -> LinuxAttachGrant {
        LinuxAttachGrant {
            consumer_id: 7,
            consumer_slot: 2,
            ring_fd_index: 0,
            ring_map_len: 4096,
            pools: vec![LinuxPoolDescriptor {
                pool_id: 9,
                surfaces: vec![LinuxSurfaceDescriptor {
                    handle_kind: 2,
                    width: 640,
                    height: 480,
                    pixel_format: 1,
                    modifier: 0,
                    planes: vec![
                        LinuxPlaneDescriptor {
                            fd_index: 1,
                            offset: 0,
                            stride: 2560,
                        },
                        LinuxPlaneDescriptor {
                            fd_index: 1,
                            offset: 640 * 480,
                            stride: 1280,
                        },
                    ],
                }],
            }],
            producer_sync: LinuxSyncDescriptor {
                sync_kind: 2,
                sync_id: 11,
                fd_index: 2,
            },
        }
    }

    #[test]
    fn validates_fd_indices_for_grant_descriptor() {
        grant().validate_fd_indices(3).unwrap();

        let mut invalid = grant();
        invalid.pools[0].surfaces[0].planes[0].fd_index = 3;
        let error = invalid.validate_fd_indices(3).unwrap_err();
        assert!(matches!(
            error,
            CaptureTransferError::FdPassing {
                operation: "validate-linux-attach",
                ..
            }
        ));
        assert!(error.to_string().contains("outside fd table"));
    }

    #[test]
    fn fd_table_limit_matches_linux_scm_rights_limit() {
        assert_eq!(super::LINUX_ATTACH_MAX_FDS, 253);
    }

    #[test]
    fn rejects_empty_or_too_many_planes() {
        let mut empty = grant();
        empty.pools[0].surfaces[0].planes.clear();
        assert!(empty.validate_fd_indices(3).is_err());

        let mut too_many = grant();
        too_many.pools[0].surfaces[0].planes = vec![
            LinuxPlaneDescriptor {
                fd_index: 1,
                offset: 0,
                stride: 1,
            };
            5
        ];
        assert!(too_many.validate_fd_indices(3).is_err());
    }

    #[test]
    fn sends_json_descriptor_with_fd_table() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let ring = tempfile_file_with_contents(b"ring");
        let dmabuf = tempfile_file_with_contents(b"dmabuf");
        let sync = tempfile_file_with_contents(b"sync");
        let message = LinuxAttachResponse::Granted { grant: grant() };

        send_json_with_fds(&sender, &message, &[ring.as_raw_fd(), dmabuf.as_raw_fd(), sync.as_raw_fd()]).unwrap();
        let (received, fd_table): (LinuxAttachResponse, _) = recv_json_with_fds(&receiver, 3).unwrap();

        assert_eq!(received, message);
        let LinuxAttachResponse::Granted { grant } = received else {
            panic!("expected grant");
        };
        grant.validate_fd_indices(fd_table.len()).unwrap();

        let mut fds = fd_table.into_fds();
        let mut dmabuf_file = File::from(fds.remove(1));
        let mut contents = Vec::new();
        dmabuf_file.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"dmabuf");
    }

    #[test]
    fn serves_authorize_and_attach_grant_over_linux_stream() {
        let (client, server) = UnixStream::pair().unwrap();
        let endpoint = AttachEndpoint::new(Some("pta_agent.linux".to_string()));
        let mut session = LinuxAttachStreamSession::new();
        let mut producer = NativeTrackProducer::new(
            TestLinuxBackend::new(),
            NativeStreamParams {
                width: 64,
                height: 32,
                pixel_format: PixelFormat::Bgra8Unorm,
                color_space: ColorSpace::Srgb,
                clock_domain: ClockDomain::HostTime,
                modifier: 0,
            },
            2,
            3,
            PoolExhaustionPolicy::Fail,
        )
        .unwrap();

        send_json(
            &client,
            &LinuxAttachRequest::Authorize {
                bearer_token: "pta_agent.linux".to_string(),
            },
        )
        .unwrap();
        handle_linux_attach_stream_message(&server, &endpoint, &mut session, &mut producer).unwrap();
        assert_eq!(recv_json::<LinuxAttachResponse>(&client).unwrap(), LinuxAttachResponse::Authorized);

        send_json(&client, &LinuxAttachRequest::Attach { consumer_id: 0 }).unwrap();
        handle_linux_attach_stream_message(&server, &endpoint, &mut session, &mut producer).unwrap();
        let (response, fd_table): (LinuxAttachResponse, _) = recv_json_with_fds(&client, 8).unwrap();
        let LinuxAttachResponse::Granted { grant } = response else {
            panic!("expected granted response");
        };

        assert_ne!(grant.consumer_id, 0);
        assert_eq!(grant.consumer_slot, 0);
        assert_eq!(grant.ring_fd_index, 0);
        assert_eq!(grant.pools.len(), 1);
        assert_eq!(grant.pools[0].surfaces.len(), 3);
        assert_eq!(grant.pools[0].surfaces[0].handle_kind, 2);
        assert_eq!(grant.pools[0].surfaces[0].planes[0].fd_index, 1);
        assert_eq!(grant.producer_sync.fd_index, 4);
        grant.validate_fd_indices(fd_table.len()).unwrap();
        assert_eq!(fd_table.len(), 5);

        let mut fds = fd_table.into_fds();
        let mut first_surface = File::from(fds.remove(1));
        let mut contents = Vec::new();
        first_surface.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"surface-0");

        let release_sync = tempfile_file_with_contents(b"release-sync");
        send_json_then_fds(
            &client,
            &LinuxAttachRequest::RegisterReleaseSync {
                sync: LinuxSyncDescriptor {
                    sync_kind: 2,
                    sync_id: 901,
                    fd_index: 0,
                },
            },
            &[release_sync.as_raw_fd()],
        )
        .unwrap();
        handle_linux_attach_stream_message(&server, &endpoint, &mut session, &mut producer).unwrap();
        assert_eq!(
            recv_json::<LinuxAttachResponse>(&client).unwrap(),
            LinuxAttachResponse::ReleaseSyncRegistered { release_sync_id: 1 }
        );
        assert_eq!(
            session.registered_release_sync(1),
            Some(&LinuxSyncDescriptor {
                sync_kind: 2,
                sync_id: 901,
                fd_index: 0,
            })
        );

        send_json(
            &client,
            &LinuxAttachRequest::AcquireLease {
                identity: LinuxLeaseIdentity {
                    cursor: 11,
                    sequence: 22,
                    pool_id: 700,
                    slot_id: 1,
                },
            },
        )
        .unwrap();
        handle_linux_attach_stream_message(&server, &endpoint, &mut session, &mut producer).unwrap();
        assert_eq!(
            recv_json::<LinuxAttachResponse>(&client).unwrap(),
            LinuxAttachResponse::LeaseAcquired { lease_id: 1 }
        );
        assert!(producer.backend_mut().lease_book.slot_has_unresolved_leases(700, 1));

        send_json(
            &client,
            &LinuxAttachRequest::ReleaseFrame {
                release: LinuxReleaseDescriptor::Now { lease_id: 1 },
            },
        )
        .unwrap();
        handle_linux_attach_stream_message(&server, &endpoint, &mut session, &mut producer).unwrap();
        assert_eq!(
            recv_json::<LinuxAttachResponse>(&client).unwrap(),
            LinuxAttachResponse::FrameReleased
        );
        assert!(!producer.backend_mut().lease_book.slot_has_unresolved_leases(700, 1));
    }

    #[test]
    fn operations_on_an_unauthenticated_endpoint_still_require_attach() {
        let (client, server) = UnixStream::pair().unwrap();
        let endpoint = AttachEndpoint::new(None);
        let mut session = LinuxAttachStreamSession::new();
        let mut producer = NativeTrackProducer::new(
            TestLinuxBackend::new(),
            NativeStreamParams {
                width: 64,
                height: 32,
                pixel_format: PixelFormat::Bgra8Unorm,
                color_space: ColorSpace::Srgb,
                clock_domain: ClockDomain::HostTime,
                modifier: 0,
            },
            2,
            3,
            PoolExhaustionPolicy::Fail,
        )
        .unwrap();

        send_json(&client, &LinuxAttachRequest::GetPool { pool_id: 700 }).unwrap();
        handle_linux_attach_stream_message(&server, &endpoint, &mut session, &mut producer).unwrap();
        assert_eq!(
            recv_json::<LinuxAttachResponse>(&client).unwrap(),
            LinuxAttachResponse::Error {
                code: "not_authorized".to_string(),
                message: "attach stream operations require a completed attach".to_string(),
            }
        );
    }

    #[test]
    fn rejects_operations_before_attach_on_an_authenticated_endpoint() {
        let (client, server) = UnixStream::pair().unwrap();
        let endpoint = AttachEndpoint::new(Some("pta_agent.linux".to_string()));
        let mut session = LinuxAttachStreamSession::new();
        let mut producer = NativeTrackProducer::new(
            TestLinuxBackend::new(),
            NativeStreamParams {
                width: 64,
                height: 32,
                pixel_format: PixelFormat::Bgra8Unorm,
                color_space: ColorSpace::Srgb,
                clock_domain: ClockDomain::HostTime,
                modifier: 0,
            },
            2,
            3,
            PoolExhaustionPolicy::Fail,
        )
        .unwrap();

        // Straight to operations, skipping Authorize/Attach entirely.
        send_json(
            &client,
            &LinuxAttachRequest::AcquireLease {
                identity: LinuxLeaseIdentity {
                    cursor: 11,
                    sequence: 22,
                    pool_id: 700,
                    slot_id: 1,
                },
            },
        )
        .unwrap();
        handle_linux_attach_stream_message(&server, &endpoint, &mut session, &mut producer).unwrap();
        let LinuxAttachResponse::Error { code, .. } = recv_json::<LinuxAttachResponse>(&client).unwrap() else {
            panic!("expected not_authorized error");
        };
        assert_eq!(code, "not_authorized");
        assert!(!producer.backend_mut().lease_book.slot_has_unresolved_leases(700, 1));

        send_json(
            &client,
            &LinuxAttachRequest::ReleaseFrame {
                release: LinuxReleaseDescriptor::Now { lease_id: 1 },
            },
        )
        .unwrap();
        handle_linux_attach_stream_message(&server, &endpoint, &mut session, &mut producer).unwrap();
        let LinuxAttachResponse::Error { code, .. } = recv_json::<LinuxAttachResponse>(&client).unwrap() else {
            panic!("expected not_authorized error");
        };
        assert_eq!(code, "not_authorized");

        send_json(&client, &LinuxAttachRequest::GetPool { pool_id: 1 }).unwrap();
        handle_linux_attach_stream_message(&server, &endpoint, &mut session, &mut producer).unwrap();
        let LinuxAttachResponse::Error { code, .. } = recv_json::<LinuxAttachResponse>(&client).unwrap() else {
            panic!("expected not_authorized error");
        };
        assert_eq!(code, "not_authorized");
    }

    #[test]
    fn caps_attach_message_length() {
        let (client, server) = UnixStream::pair().unwrap();
        let mut writer = &client;
        // One byte past the cap, no newline: the reader must fail instead of
        // buffering an unbounded line.
        writer.write_all(&vec![b'x'; super::LINUX_ATTACH_MAX_JSON_BYTES + 1]).unwrap();
        let error = recv_json::<LinuxAttachRequest>(&server).unwrap_err();
        assert!(error.to_string().contains("without a newline"), "unexpected error: {error}");
    }

    #[test]
    fn linux_attach_server_accepts_streams_and_cleans_up_socket_path() {
        let tempdir = tempfile::tempdir().unwrap();
        let socket_path = tempdir.path().join("missing").join("native.sock");
        let producer = Arc::new(Mutex::new(
            NativeTrackProducer::new(
                TestLinuxBackend::new(),
                NativeStreamParams {
                    width: 64,
                    height: 32,
                    pixel_format: PixelFormat::Bgra8Unorm,
                    color_space: ColorSpace::Srgb,
                    clock_domain: ClockDomain::HostTime,
                    modifier: 0,
                },
                2,
                3,
                PoolExhaustionPolicy::Fail,
            )
            .unwrap(),
        ));
        let server = LinuxAttachServer::start(
            &socket_path,
            AttachEndpoint::new(Some("pta_agent.linux".to_string())),
            Arc::clone(&producer),
        )
        .unwrap();
        assert!(socket_path.parent().unwrap().exists());
        assert_eq!(server.path(), socket_path.as_path());

        let client = UnixStream::connect(&socket_path).unwrap();
        send_json(
            &client,
            &LinuxAttachRequest::Authorize {
                bearer_token: "pta_agent.linux".to_string(),
            },
        )
        .unwrap();
        assert_eq!(recv_json::<LinuxAttachResponse>(&client).unwrap(), LinuxAttachResponse::Authorized);

        send_json(&client, &LinuxAttachRequest::Attach { consumer_id: 0 }).unwrap();
        let (response, fd_table): (LinuxAttachResponse, _) = recv_json_with_fds(&client, 8).unwrap();
        let LinuxAttachResponse::Granted { grant } = response else {
            panic!("expected granted response");
        };
        assert_eq!(grant.pools.len(), 1);
        assert_eq!(grant.pools[0].surfaces.len(), 3);
        grant.validate_fd_indices(fd_table.len()).unwrap();
        drop(client);
        drop(server);
        assert!(!socket_path.exists());
    }

    #[test]
    fn release_timeline_must_be_registered_on_the_attach_stream() {
        let (client, server) = UnixStream::pair().unwrap();
        let endpoint = AttachEndpoint::new(None);
        let mut session = LinuxAttachStreamSession::new();
        let mut producer = NativeTrackProducer::new(
            TestLinuxBackend::new(),
            NativeStreamParams {
                width: 64,
                height: 32,
                pixel_format: PixelFormat::Bgra8Unorm,
                color_space: ColorSpace::Srgb,
                clock_domain: ClockDomain::HostTime,
                modifier: 0,
            },
            2,
            3,
            PoolExhaustionPolicy::Fail,
        )
        .unwrap();

        send_json(&client, &LinuxAttachRequest::Attach { consumer_id: 0 }).unwrap();
        handle_linux_attach_stream_message(&server, &endpoint, &mut session, &mut producer).unwrap();
        let (response, _fd_table): (LinuxAttachResponse, _) = recv_json_with_fds(&client, 8).unwrap();
        assert!(matches!(response, LinuxAttachResponse::Granted { .. }));

        send_json(
            &client,
            &LinuxAttachRequest::AcquireLease {
                identity: LinuxLeaseIdentity {
                    cursor: 11,
                    sequence: 22,
                    pool_id: 700,
                    slot_id: 1,
                },
            },
        )
        .unwrap();
        handle_linux_attach_stream_message(&server, &endpoint, &mut session, &mut producer).unwrap();
        assert_eq!(
            recv_json::<LinuxAttachResponse>(&client).unwrap(),
            LinuxAttachResponse::LeaseAcquired { lease_id: 1 }
        );

        send_json(
            &client,
            &LinuxAttachRequest::ReleaseFrame {
                release: LinuxReleaseDescriptor::TimelineValue {
                    lease_id: 1,
                    release_sync_id: 42,
                    value: 5,
                },
            },
        )
        .unwrap();
        handle_linux_attach_stream_message(&server, &endpoint, &mut session, &mut producer).unwrap();
        match recv_json::<LinuxAttachResponse>(&client).unwrap() {
            LinuxAttachResponse::Error { code, message } => {
                assert_eq!(code, "invalid_state");
                assert!(message.contains("release sync id 42"));
            }
            other => panic!("expected invalid_state error, got {other:?}"),
        }
        assert!(producer.backend_mut().lease_book.slot_has_unresolved_leases(700, 1));
    }

    #[derive(Debug)]
    struct TestLinuxBackend {
        next_pool_id: u64,
        fence: Option<File>,
        lease_book: NativeLeaseBook,
    }

    #[derive(Debug)]
    struct TestLinuxPool {
        pool_id: u64,
        surfaces: Vec<File>,
    }

    impl TestLinuxBackend {
        fn new() -> Self {
            Self {
                next_pool_id: 700,
                fence: None,
                lease_book: NativeLeaseBook::new(),
            }
        }
    }

    impl LinuxNativeLeaseBackend for TestLinuxBackend {
        fn acquire_linux_lease(&mut self, identity: NativeLeaseIdentity) -> Result<u64> {
            Ok(self.lease_book.acquire(identity))
        }

        fn register_linux_release_sync(&mut self, _sync: LinuxSyncDescriptor, _fd: std::os::fd::OwnedFd) -> Result<u64> {
            Ok(self.lease_book.register_release_sync())
        }

        fn release_linux_lease(&mut self, lease_id: u64, release: NativeLeaseRelease) -> Result<()> {
            self.lease_book
                .release(lease_id, release)
                .map(|_| ())
                .map_err(CaptureTransferError::from)
        }
    }

    impl NativeFrameBackend for TestLinuxBackend {
        type CapturedFrame = ();
        type SurfacePool = TestLinuxPool;
        type Fence = File;
        type SurfaceHandle = LinuxSurfaceHandle;
        type SyncHandle = LinuxSyncHandle;

        fn payload_kind(&self) -> PayloadKind {
            PayloadKind::DmaBuf
        }

        fn allocate_surface_pool(&mut self, _params: &NativeStreamParams, slot_count: u32) -> Result<Self::SurfacePool> {
            let pool_id = self.next_pool_id;
            self.next_pool_id += 1;
            let mut surfaces = Vec::new();
            for slot_id in 0..slot_count {
                surfaces.push(tempfile_file_with_contents(format!("surface-{slot_id}").as_bytes()));
            }
            Ok(TestLinuxPool { pool_id, surfaces })
        }

        fn pool_id(&self, pool: &Self::SurfacePool) -> u64 {
            pool.pool_id
        }

        fn claim_reusable_slot(&mut self, _pool: &mut Self::SurfacePool, candidates: &[SlotReuseCandidate]) -> Result<SlotClaim> {
            Ok(candidates
                .first()
                .map(|candidate| SlotClaim::Ready {
                    slot_id: candidate.slot_id,
                })
                .unwrap_or(SlotClaim::WouldBlock))
        }

        fn stage_frame(&mut self, _pool: &mut Self::SurfacePool, _slot_id: u32, _frame: &Self::CapturedFrame) -> Result<()> {
            Ok(())
        }

        fn export_surface_handles(&self, pool: &Self::SurfacePool) -> Result<Vec<Self::SurfaceHandle>> {
            pool.surfaces
                .iter()
                .map(|surface| {
                    Ok(LinuxSurfaceHandle {
                        handle_kind: 2,
                        width: 64,
                        height: 32,
                        pixel_format: PixelFormat::Bgra8Unorm as u32,
                        modifier: 0,
                        planes: vec![LinuxDmabufPlaneHandle {
                            fd: surface
                                .as_fd()
                                .try_clone_to_owned()
                                .map_err(|error| CaptureTransferError::FdPassing {
                                    operation: "clone-test-linux-surface",
                                    message: error.to_string(),
                                })?,
                            offset: 0,
                            stride: 256,
                        }],
                    })
                })
                .collect()
        }

        fn create_fence(&mut self) -> Result<Self::Fence> {
            let fence = tempfile_file_with_contents(b"sync");
            self.fence = Some(
                fence
                    .as_fd()
                    .try_clone_to_owned()
                    .map(File::from)
                    .map_err(|error| CaptureTransferError::FdPassing {
                        operation: "clone-test-linux-sync",
                        message: error.to_string(),
                    })?,
            );
            Ok(fence)
        }

        fn fence_id(&self, _fence: &Self::Fence) -> u64 {
            900
        }

        fn signal_fence(&mut self, _fence: &mut Self::Fence, _value: u64) -> Result<()> {
            Ok(())
        }

        fn export_sync_handle(&self, fence: &Self::Fence) -> Result<Self::SyncHandle> {
            Ok(LinuxSyncHandle {
                sync_kind: 2,
                sync_id: 900,
                fd: fence
                    .as_fd()
                    .try_clone_to_owned()
                    .map_err(|error| CaptureTransferError::FdPassing {
                        operation: "clone-test-linux-sync",
                        message: error.to_string(),
                    })?,
            })
        }
    }

    fn tempfile_file_with_contents(contents: &[u8]) -> File {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(contents).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file
    }
}
