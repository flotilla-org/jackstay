//! The attach handshake for the native handle path: how a consumer is
//! introduced to a stream, transport-agnostically.
//!
//! Everything a consumer needs crosses **exactly once**, at attach — the ring
//! mapping, the pool's surface handles, and the serialized sync handle.
//! Steady state after that is shared-memory only: zero setup-channel traffic
//! per frame.
//!
//! Only the *protocol* lives here: what crosses, in what order, under what
//! authorization, exactly once per session. The transport that moves it is
//! platform-specific and out of scope — the CPU fallback uses plain UDS +
//! SCM_RIGHTS, the macOS native path uses XPC over `portholed`'s named mach
//! service (ADR-0007, #84) because `MTLSharedEventHandle` refuses plain
//! serialization. Tests drive the protocol with an in-memory fake transport.

use std::os::fd::OwnedFd;

use thiserror::Error;

use crate::{
    error::CaptureTransferError,
    native::{NativeFrameBackend, NativeTrackProducer},
};

/// A consumer-to-producer request on the setup channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachRequest {
    /// Present a bearer token. Must precede `Attach` when the endpoint
    /// requires authorization.
    Authorize { bearer_token: String },
    /// Request the one-time handle transfer for `consumer_id`.
    Attach { consumer_id: u64 },
}

/// A producer-to-consumer response on the setup channel.
#[derive(Debug)]
pub enum AttachResponse {
    Authorized,
    Granted(Box<AttachGrant>),
}

/// Everything a consumer needs, transferred once. After this message the
/// consumer maps the ring, resolves its surface and fence handles, and never
/// touches the setup channel again in steady state.
#[derive(Debug)]
pub struct AttachGrant {
    pub consumer_id: u64,
    /// The consumer-cursor table slot assigned to this consumer.
    pub consumer_slot: u64,
    /// The control page; map read-only for `ring_map_len` bytes.
    pub ring_fd: OwnedFd,
    pub ring_map_len: u64,
    pub pool_id: u64,
    pub pool_slot_count: u32,
    /// One serialized surface handle per pool slot, indexed by `slot_id`.
    /// Opaque to the protocol; the transport and backend agree on the
    /// encoding (e.g. XPC-encodable IOSurface references on macOS).
    pub surface_handles: Vec<Vec<u8>>,
    pub fence_id: u64,
    /// The serialized sync handle (e.g. an NSXPCCoder-encoded
    /// MTLSharedEventHandle on macOS).
    pub sync_handle: Vec<u8>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AttachError {
    #[error("attach requires authorization first")]
    NotAuthorized,
    #[error("bearer token rejected")]
    InvalidToken,
    #[error("consumer is already attached on this session")]
    AlreadyAttached,
    #[error("consumer id must be non-zero")]
    InvalidConsumerId,
    #[error("assembling the attach grant failed: {0}")]
    Grant(#[from] CaptureTransferError),
}

/// Per-connection handshake state. A transport creates one per accepted
/// connection; reconnecting (a crashed consumer re-attaching) is a new
/// session, which is the recovery path rather than an error.
#[derive(Debug, Default)]
pub struct AttachSession {
    authorized: bool,
    attached: bool,
}

/// The producer-side endpoint: validates ordering and authorization, then
/// asks the producer for the one-time grant.
#[derive(Debug)]
pub struct AttachEndpoint {
    /// `None` means the endpoint is unauthenticated (synthetic/test streams);
    /// `Some` requires a matching `Authorize` before `Attach`.
    expected_bearer: Option<String>,
}

/// Token equality that does not leak how many leading bytes matched through
/// timing. Length still leaks; tokens are fixed-format so that reveals
/// nothing. The transport is a local IPC channel today, but the comparison
/// must stay safe if the protocol is ever promoted to a network path.
fn constant_time_token_eq(expected: &str, presented: &str) -> bool {
    let (a, b) = (expected.as_bytes(), presented.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

impl AttachEndpoint {
    #[must_use]
    pub fn new(expected_bearer: Option<String>) -> Self {
        Self { expected_bearer }
    }

    /// Apply one request to a session. The transport's job is exactly this
    /// loop: decode a request, call `handle`, encode the response or error.
    pub fn handle<B: NativeFrameBackend>(
        &self,
        session: &mut AttachSession,
        producer: &mut NativeTrackProducer<B>,
        request: AttachRequest,
    ) -> Result<AttachResponse, AttachError> {
        match request {
            AttachRequest::Authorize { bearer_token } => {
                match &self.expected_bearer {
                    Some(expected) if constant_time_token_eq(expected, &bearer_token) => {
                        session.authorized = true;
                        Ok(AttachResponse::Authorized)
                    }
                    Some(_) => Err(AttachError::InvalidToken),
                    // Authorizing against an unauthenticated endpoint is
                    // harmless; accept so clients can be unconditional.
                    None => {
                        session.authorized = true;
                        Ok(AttachResponse::Authorized)
                    }
                }
            }
            AttachRequest::Attach { consumer_id } => {
                if self.expected_bearer.is_some() && !session.authorized {
                    return Err(AttachError::NotAuthorized);
                }
                if session.attached {
                    return Err(AttachError::AlreadyAttached);
                }
                if consumer_id == 0 {
                    return Err(AttachError::InvalidConsumerId);
                }
                let grant = producer.grant_attach(consumer_id)?;
                session.attached = true;
                Ok(AttachResponse::Granted(Box::new(grant)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AttachEndpoint, AttachError, AttachGrant, AttachRequest, AttachResponse, AttachSession};
    use crate::{
        control_page::VideoTrackControlPage,
        model::{ClockDomain, ColorSpace, PixelFormat},
        native::{
            NativeStreamParams, NativeTrackProducer, PoolExhaustionPolicy,
            fake::{FakeCapturedFrame, FakeNativeBackend, FakeSurfaceRegistry},
        },
    };

    fn params() -> NativeStreamParams {
        NativeStreamParams {
            width: 640,
            height: 480,
            pixel_format: PixelFormat::Bgra8Unorm,
            color_space: ColorSpace::Srgb,
            clock_domain: ClockDomain::HostTime,
            modifier: 0,
        }
    }

    fn producer(registry: &FakeSurfaceRegistry) -> NativeTrackProducer<FakeNativeBackend> {
        NativeTrackProducer::new(
            FakeNativeBackend::new(registry.clone()),
            params(),
            2,
            3,
            PoolExhaustionPolicy::default(),
        )
        .unwrap()
    }

    /// The fake transport: an in-memory request/response loop that records
    /// every message that crosses the setup channel, so tests can assert the
    /// exactly-once and zero-steady-state-traffic properties.
    struct FakeTransport {
        endpoint: AttachEndpoint,
        session: AttachSession,
        messages: usize,
    }

    impl FakeTransport {
        fn new(expected_bearer: Option<&str>) -> Self {
            Self {
                endpoint: AttachEndpoint::new(expected_bearer.map(str::to_string)),
                session: AttachSession::default(),
                messages: 0,
            }
        }

        fn request(
            &mut self,
            producer: &mut NativeTrackProducer<FakeNativeBackend>,
            request: AttachRequest,
        ) -> Result<AttachResponse, AttachError> {
            // One request and one response (or error) cross the channel.
            self.messages += 2;
            self.endpoint.handle(&mut self.session, producer, request)
        }
    }

    fn attach(transport: &mut FakeTransport, producer: &mut NativeTrackProducer<FakeNativeBackend>, consumer_id: u64) -> AttachGrant {
        match transport.request(producer, AttachRequest::Attach { consumer_id }) {
            Ok(AttachResponse::Granted(grant)) => *grant,
            other => panic!("attach not granted: {other:?}"),
        }
    }

    #[test]
    fn attach_transfers_handles_once_and_steady_state_is_shared_memory_only() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry);
        let mut transport = FakeTransport::new(Some("pta_agent.secret"));

        // Handshake: authorize, then attach.
        assert!(matches!(
            transport.request(
                &mut producer,
                AttachRequest::Authorize {
                    bearer_token: "pta_agent.secret".to_string(),
                }
            ),
            Ok(AttachResponse::Authorized)
        ));
        let grant = attach(&mut transport, &mut producer, 7);
        let handshake_messages = transport.messages;
        // Authorize + Attach: two request/response pairs, nothing else.
        assert_eq!(handshake_messages, 4);

        // The grant is complete: ring mapping + per-slot surface handles +
        // sync handle, all in one transfer.
        assert_eq!(grant.consumer_id, 7);
        assert_eq!(grant.pool_slot_count, 3);
        assert_eq!(grant.surface_handles.len(), 3);
        let ring = VideoTrackControlPage::map_read_only(grant.ring_fd, grant.ring_map_len as usize).unwrap();
        let sync_fence_id = u64::from_le_bytes(grant.sync_handle.as_slice().try_into().unwrap());
        assert_eq!(sync_fence_id, grant.fence_id);

        // Steady state: the producer publishes and the consumer follows along
        // using only the grant artifacts. Nothing further crosses the
        // setup channel.
        for sequence in 1..=3u64 {
            producer
                .publish(
                    &FakeCapturedFrame {
                        bytes: vec![sequence as u8],
                    },
                    sequence,
                )
                .unwrap();
            let entry = ring.read_latest_lossy_entry().unwrap().unwrap();
            assert_eq!(entry.sequence, sequence);
            assert_eq!(entry.pool_id, grant.pool_id);
            assert_eq!(entry.fence_id, grant.fence_id);
            assert!(registry.fence_value(entry.fence_id).unwrap() >= entry.fence_value);
            // The surface handle for this frame was already transferred at
            // attach; resolving it is a local operation.
            let handle = &grant.surface_handles[entry.slot_id as usize];
            let handle_pool = u64::from_le_bytes(handle[0..8].try_into().unwrap());
            let handle_slot = u32::from_le_bytes(handle[8..12].try_into().unwrap());
            assert_eq!((handle_pool, handle_slot), (entry.pool_id, entry.slot_id));
            assert_eq!(registry.surface_bytes(handle_pool, handle_slot).unwrap(), vec![sequence as u8]);
        }
        assert_eq!(transport.messages, handshake_messages, "setup channel used after attach");
    }

    #[test]
    fn attach_before_authorize_is_rejected_when_token_required() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry);
        let mut transport = FakeTransport::new(Some("pta_agent.secret"));

        let error = transport
            .request(&mut producer, AttachRequest::Attach { consumer_id: 7 })
            .unwrap_err();
        assert_eq!(error, AttachError::NotAuthorized);
    }

    #[test]
    fn wrong_bearer_token_is_rejected_and_does_not_authorize() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry);
        let mut transport = FakeTransport::new(Some("pta_agent.secret"));

        let error = transport
            .request(
                &mut producer,
                AttachRequest::Authorize {
                    bearer_token: "pta_agent.wrong".to_string(),
                },
            )
            .unwrap_err();
        assert_eq!(error, AttachError::InvalidToken);

        // The failed authorize must not have moved the session forward.
        let error = transport
            .request(&mut producer, AttachRequest::Attach { consumer_id: 7 })
            .unwrap_err();
        assert_eq!(error, AttachError::NotAuthorized);
    }

    #[test]
    fn second_attach_on_the_same_session_is_rejected() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry);
        let mut transport = FakeTransport::new(None);

        let _grant = attach(&mut transport, &mut producer, 7);
        let error = transport
            .request(&mut producer, AttachRequest::Attach { consumer_id: 7 })
            .unwrap_err();
        assert_eq!(error, AttachError::AlreadyAttached);
    }

    #[test]
    fn unauthenticated_endpoint_attaches_without_authorize() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry);
        let mut transport = FakeTransport::new(None);

        let grant = attach(&mut transport, &mut producer, 7);
        assert_eq!(grant.consumer_id, 7);
    }

    #[test]
    fn zero_consumer_id_is_rejected() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry);
        let mut transport = FakeTransport::new(None);

        let error = transport
            .request(&mut producer, AttachRequest::Attach { consumer_id: 0 })
            .unwrap_err();
        assert_eq!(error, AttachError::InvalidConsumerId);
    }

    #[test]
    fn reattach_on_a_new_session_reuses_the_consumer_slot() {
        let registry = FakeSurfaceRegistry::default();
        let mut producer = producer(&registry);

        // A crashed consumer reconnects: new session, same consumer id. This
        // is the recovery path, not an error, and the cursor-table slot is
        // stable across it.
        let first = attach(&mut FakeTransport::new(None), &mut producer, 7);
        let second = attach(&mut FakeTransport::new(None), &mut producer, 7);
        assert_eq!(first.consumer_slot, second.consumer_slot);
        assert_eq!(first.pool_id, second.pool_id);
        assert_eq!(first.fence_id, second.fence_id);
    }
}
