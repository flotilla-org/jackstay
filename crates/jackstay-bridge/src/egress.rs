//! The egress half: an ordinary native Jackstay consumer that encodes what it
//! acquires and sends it over the link.
//!
//! Threads: the caller's thread runs the acquisition loop (snapshot, acquire
//! latest, wait), VideoToolbox's output thread runs the encoder sink, a sender
//! thread writes the media stream, and a control thread answers clock pings
//! and applies targets. Drop happens at the sender: one slot holds the newest
//! keyframe (with its codec configuration), one the newest delta; a keyframe
//! replaces everything queued; a queued delta is replaced by a newer one. A
//! frame whose first byte is on the wire is never abandoned.

use std::{
    collections::HashMap,
    ffi::c_void,
    io::Write,
    os::unix::net::UnixStream,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use jackstay::{
    acquisition::arena::{
        AcquireOutcome, ArenaConsumer, Cancellation, ConfigurationInstall, FrameDescriptor, FrameLease, WaitInterest, WaitOutcome,
    },
    native::macos::{
        ConsumerFence, IoSurface, MetalContext, SharedEventHandle,
        xpc::arena::{XpcArenaClient, XpcArenaEndpoint},
    },
};
use jackstay_graph::{Chroma, ChromaPolicy, CodecCapabilities, CodecDecision, Target, decide};

use crate::{
    input_relay::InputRelay,
    vt::{self, EncodedFrame, Encoder, EncoderConfig},
    wire::{self, CodecConfig, FrameBody, Hello, Kind, Message, Op, Rect, Role, flags},
};

#[derive(Debug, thiserror::Error)]
pub enum EgressError {
    #[error("arena: {0}")]
    Arena(String),
    #[error("video toolbox: {0}")]
    Vt(#[from] vt::VtError),
    #[error("wire: {0}")]
    Wire(#[from] wire::WireError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("codec decision: {0}")]
    Decision(#[from] jackstay_graph::DecisionError),
    #[error("peer sent {0:?} where a hello was expected")]
    BadHello(Kind),
    #[error("peer token does not match the export grant")]
    Token,
    #[error("{0}")]
    Other(String),
}

fn arena<E: std::fmt::Display>(e: E) -> EgressError {
    EgressError::Arena(e.to_string())
}

/// Where the publication to consume lives.
pub enum Source {
    /// An in-process anonymous endpoint (loopback, tests).
    Endpoint(XpcArenaEndpoint),
    /// A launchd-registered Mach service, with the attach token the
    /// coordinator minted for this consumer.
    Named { service: String, token: Option<String> },
}

#[derive(Debug, Clone)]
pub struct EgressConfig {
    /// Frames this half may hold at once; two or three is enough because the
    /// encoder is one-in-one-out.
    pub holding: u32,
    pub chroma_policy: ChromaPolicy,
    pub bitrate_bps: u32,
    pub low_latency: bool,
    /// Token both halves were given by the coordinator; empty means trusted link.
    pub token: String,
    /// The executor's input socket on this host; a relayed input stream the
    /// peer opens is connected here. `None` refuses input streams.
    pub input_socket: Option<std::path::PathBuf>,
}

impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            holding: 3,
            chroma_policy: ChromaPolicy::Prefer444,
            bitrate_bps: 20_000_000,
            low_latency: false,
            token: String::new(),
            input_socket: None,
        }
    }
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct EgressReport {
    pub decision: Option<CodecDecision>,
    pub encoder_id: String,
    pub frames_acquired: u64,
    pub frames_encoded: u64,
    pub frames_sent: u64,
    pub frames_dropped_at_sender: u64,
    pub frames_skipped_for_rate: u64,
    pub keyframes: u64,
    pub encoder_errors: u64,
    pub reconfigurations: u64,
    pub bytes_sent: u64,
    /// Relayed input streams the peer opened, and bytes each way.
    pub input_streams_opened: u64,
    pub input_bytes_from_link: u64,
    pub input_bytes_to_link: u64,
    /// Why the acquisition loop ended: `stopped`, `closed`, `cancelled`.
    pub ended: String,
}

// ---- sender queue -------------------------------------------------------------------

#[derive(Default)]
struct SendSlots {
    /// Codec configuration plus the keyframe it precedes.
    keyframe: Option<(Message, Message)>,
    delta: Option<Message>,
    closed: bool,
    dropped: u64,
}

struct Sender {
    slots: Mutex<SendSlots>,
    ready: Condvar,
}

impl Sender {
    fn push_keyframe(&self, config: Message, frame: Message) {
        let mut s = self.slots.lock().expect("sender poisoned");
        if s.keyframe.is_some() {
            s.dropped += 1;
        }
        if s.delta.take().is_some() {
            s.dropped += 1;
        }
        s.keyframe = Some((config, frame));
        self.ready.notify_one();
    }

    fn push_delta(&self, frame: Message) {
        let mut s = self.slots.lock().expect("sender poisoned");
        if s.delta.replace(frame).is_some() {
            s.dropped += 1;
        }
        self.ready.notify_one();
    }

    fn close(&self) {
        self.slots.lock().expect("sender poisoned").closed = true;
        self.ready.notify_all();
    }

    fn run(&self, mut media: UnixStream, stats: &SenderStats) {
        loop {
            let next = {
                let mut s = self.slots.lock().expect("sender poisoned");
                loop {
                    if let Some((config, frame)) = s.keyframe.take() {
                        break Some(vec![config, frame]);
                    }
                    if let Some(frame) = s.delta.take() {
                        break Some(vec![frame]);
                    }
                    if s.closed {
                        break None;
                    }
                    s = self.ready.wait(s).expect("sender poisoned");
                }
            };
            let Some(messages) = next else { return };
            for m in messages {
                if m.write_to(&mut media).is_err() {
                    self.close();
                    return;
                }
                stats.sent.fetch_add(1, Ordering::Relaxed);
                stats
                    .bytes
                    .fetch_add(m.body.len() as u64 + wire::HEADER_LEN as u64, Ordering::Relaxed);
            }
            let _ = media.flush();
        }
    }
}

#[derive(Default)]
struct SenderStats {
    sent: AtomicU64,
    bytes: AtomicU64,
}

// ---- control ----------------------------------------------------------------------------

#[derive(Default)]
struct ControlState {
    max_fps: Option<u32>,
    keyframe_requested: bool,
}

fn control_loop(
    mut reader: UnixStream,
    writer: Arc<Mutex<UnixStream>>,
    state: Arc<Mutex<ControlState>>,
    stop: Arc<AtomicBool>,
    relay: Arc<InputRelay>,
) {
    let mut seq = 0u64;
    while !stop.load(Ordering::Relaxed) {
        let message = match Message::read_from(&mut reader) {
            Ok(Some(m)) => m,
            _ => break,
        };
        match message.header.kind {
            Kind::ClockPing => {
                let t2 = crate::host_now_ns();
                let Ok(ping) = wire::ClockPing::decode(&message.body) else {
                    continue;
                };
                seq += 1;
                let pong = wire::ClockPong {
                    t1: ping.t1,
                    t2,
                    t3: crate::host_now_ns(),
                };
                let reply = Message::new(Kind::ClockPong, seq, pong.encode());
                if reply.write_to(&mut *writer.lock().expect("control writer poisoned")).is_err() {
                    break;
                }
            }
            Kind::Input => relay.on_message(&message),
            Kind::Target => {
                if let Ok(target) = wire::json_body::<Target>(&message) {
                    state.lock().expect("control poisoned").max_fps = target.max_fps;
                }
            }
            Kind::KeyframeRequest => {
                state.lock().expect("control poisoned").keyframe_requested = true;
            }
            _ => {}
        }
    }
    stop.store(true, Ordering::Relaxed);
}

// ---- the encoder sink -------------------------------------------------------------------

/// What the encoder hands back with each output: the lease it was reading
/// from and the descriptor to put on the wire.
struct Pending {
    lease: FrameLease,
    descriptor: FrameDescriptor,
}

struct SinkShared {
    sender: Arc<Sender>,
    encoded: AtomicU64,
    keyframes: AtomicU64,
    errors: AtomicU64,
    seq: AtomicU64,
    stream_id: u32,
    decision: CodecDecision,
    profile: &'static str,
}

fn make_encoder(
    config: EncoderConfig,
    shared: Arc<SinkShared>,
    stream_info: Arc<Mutex<Option<Arc<Encoder>>>>,
) -> Result<Arc<Encoder>, EgressError> {
    let sink_shared = shared;
    let info_source = stream_info;
    let encoder = Encoder::new(
        config,
        Box::new(move |frame: EncodedFrame| {
            // SAFETY: refcon is the Box<Pending> `submit` leaked for exactly this callback.
            let pending = unsafe { Box::from_raw(frame.refcon.cast::<Pending>()) };
            let Pending { lease, descriptor } = *pending;
            if frame.status != 0 || frame.dropped || frame.annexb.is_empty() {
                sink_shared.errors.fetch_add(1, Ordering::Relaxed);
                drop(lease);
                return;
            }
            let seq = sink_shared.seq.fetch_add(1, Ordering::Relaxed) + 1;
            let body = FrameBody {
                descriptor,
                ops: vec![Op::Video {
                    rect: Rect {
                        x: 0,
                        y: 0,
                        width: descriptor.width,
                        height: descriptor.height,
                    },
                    stream_id: sink_shared.stream_id,
                    plane_mask: 0,
                    access_unit: frame.annexb,
                }],
            };
            let mut message = Message::new(Kind::Frame, seq, body.encode());
            message.header.stream_id = sink_shared.stream_id;
            message.header.timestamp_ns = descriptor.timestamp_ns;
            sink_shared.encoded.fetch_add(1, Ordering::Relaxed);
            if frame.keyframe {
                message.header.flags |= flags::KEYFRAME | flags::CONFIG_CHANGED;
                sink_shared.keyframes.fetch_add(1, Ordering::Relaxed);
                let config = info_source
                    .lock()
                    .ok()
                    .and_then(|e| e.as_ref().map(Arc::clone))
                    .and_then(|encoder| encoder.stream_info().ok())
                    .map(|info| CodecConfig {
                        codec: sink_shared.decision.codec,
                        chroma: sink_shared.decision.chroma,
                        full_range: info.full_range,
                        bit_depth: 8,
                        width: descriptor.width,
                        height: descriptor.height,
                        profile: sink_shared.profile.to_owned(),
                        colour: info.colour,
                        parameter_sets: info.parameter_sets,
                    });
                match config.and_then(|c| wire::json_message(Kind::CodecConfig, seq, &c).ok()) {
                    Some(config_message) => sink_shared.sender.push_keyframe(config_message, message),
                    None => sink_shared.errors.fetch_add(1, Ordering::Relaxed).then_none(),
                }
            } else {
                message.header.flags |= flags::DISCARDABLE;
                sink_shared.sender.push_delta(message);
            }
            // VideoToolbox is done reading the surface once it has produced the output.
            drop(lease);
        }),
    )?;
    Ok(Arc::new(encoder))
}

trait ThenNone {
    fn then_none(self);
}
impl ThenNone for u64 {
    fn then_none(self) {}
}

// ---- the half -------------------------------------------------------------------------------

/// Runs the egress half until the publication closes, the link drops, or
/// `stop` is set. Blocks the calling thread.
pub fn run(
    source: Source,
    media: UnixStream,
    control: UnixStream,
    config: EgressConfig,
    stop: Arc<AtomicBool>,
    on_ready: Box<dyn FnOnce(&CodecDecision) + Send>,
) -> Result<EgressReport, EgressError> {
    let mut report = EgressReport::default();

    // 1. attach to the publication
    let mut client = match &source {
        Source::Endpoint(endpoint) => XpcArenaClient::connect_endpoint(endpoint).map_err(arena)?,
        Source::Named { service, token } => {
            // SAFETY: the coordinator that spawned this half vouches for the service name.
            let mut client = unsafe { XpcArenaClient::connect_named(service) }.map_err(arena)?;
            if let Some(token) = token {
                client.authorize(token).map_err(arena)?;
            }
            client
        }
    };
    let mut consumer: ArenaConsumer = client.attach(config.holding).map_err(arena)?;

    // 2. hello in both directions, then the codec decision
    let mine = vt::probe_capabilities();
    let mut control_writer = control.try_clone()?;
    wire::json_message(
        Kind::Hello,
        0,
        &Hello {
            role: Role::Egress,
            protocol_version: wire::VERSION,
            capabilities: mine,
            chroma_policy: config.chroma_policy,
            token: config.token.clone(),
        },
    )?
    .write_to(&mut control_writer)?;
    let mut control_reader = control.try_clone()?;
    let peer = Message::read_from(&mut control_reader)?.ok_or(EgressError::Other("link closed before hello".into()))?;
    if peer.header.kind != Kind::Hello {
        return Err(EgressError::BadHello(peer.header.kind));
    }
    let peer: Hello = wire::json_body(&peer)?;
    if peer.token != config.token {
        return Err(EgressError::Token);
    }
    let decision = decide(config.chroma_policy, mine, peer.capabilities)?;
    report.decision = Some(decision.clone());
    on_ready(&decision);

    // 3. threads: sender, control
    let sender = Arc::new(Sender {
        slots: Mutex::new(SendSlots::default()),
        ready: Condvar::new(),
    });
    let sender_stats = Arc::new(SenderStats::default());
    let sender_thread = {
        let sender = sender.clone();
        let stats = sender_stats.clone();
        std::thread::Builder::new()
            .name("jackstay-egress-send".into())
            .spawn(move || sender.run(media, &stats))?
    };
    let control_state = Arc::new(Mutex::new(ControlState::default()));
    let control_shared = Arc::new(Mutex::new(control.try_clone()?));
    let relay = InputRelay::new(control_shared.clone(), config.input_socket.clone(), stop.clone());
    let control_thread = {
        let state = control_state.clone();
        let stop = stop.clone();
        let relay = relay.clone();
        std::thread::Builder::new()
            .name("jackstay-egress-control".into())
            .spawn(move || control_loop(control, control_shared, state, stop, relay))?
    };

    // 4. the acquisition loop
    let metal = MetalContext::new().map_err(arena)?;
    let cancellation = Cancellation::new()?;
    let stop_watch = {
        let cancellation = cancellation.clone();
        let stop = stop.clone();
        std::thread::Builder::new().name("jackstay-egress-stop".into()).spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = cancellation.cancel();
        })?
    };
    let shared = Arc::new(SinkShared {
        sender: sender.clone(),
        encoded: AtomicU64::new(0),
        keyframes: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        seq: AtomicU64::new(0),
        stream_id: 1,
        decision: decision.clone(),
        profile: vt::profile_string(decision.codec, decision.chroma),
    });
    let encoder_slot: Arc<Mutex<Option<Arc<Encoder>>>> = Arc::new(Mutex::new(None));
    let mut encoder_dims = (0u32, 0u32);
    let mut fences: HashMap<u64, ConsumerFence> = HashMap::new();
    let mut last_cursor = 0u64;
    let mut last_encode = Instant::now() - Duration::from_secs(1);
    let mut force_keyframe = true;
    let mut requested_epoch: Option<u64> = None;
    let outcome: Result<(), EgressError> = loop {
        if stop.load(Ordering::Relaxed) {
            report.ended = "stopped (link or interrupt)".into();
            break Ok(());
        }
        let before = consumer.events();
        let mut interest = WaitInterest::DATA;
        match consumer.acquire_latest(last_cursor).map_err(arena)? {
            AcquireOutcome::Frame(lease) => {
                report.frames_acquired += 1;
                let descriptor = *lease.descriptor();
                last_cursor = descriptor.cursor;
                let (max_fps, key_req) = {
                    let mut s = control_state.lock().expect("control poisoned");
                    (s.max_fps, std::mem::take(&mut s.keyframe_requested))
                };
                if let Some(fps) = max_fps.filter(|f| *f > 0) {
                    if last_encode.elapsed() < Duration::from_micros(1_000_000 / u64::from(fps)) {
                        report.frames_skipped_for_rate += 1;
                        drop(lease);
                        continue;
                    }
                }
                force_keyframe |= key_req;
                let dims = (descriptor.width, descriptor.height);
                if dims != encoder_dims || encoder_slot.lock().expect("encoder poisoned").is_none() {
                    if let Some(old) = encoder_slot.lock().expect("encoder poisoned").take() {
                        let _ = old.flush();
                        report.reconfigurations += 1;
                    }
                    let encoder = make_encoder(
                        EncoderConfig {
                            width: dims.0,
                            height: dims.1,
                            codec: decision.codec,
                            chroma: decision.chroma,
                            low_latency: config.low_latency,
                            bitrate_bps: config.bitrate_bps,
                            fps: 60,
                        },
                        shared.clone(),
                        encoder_slot.clone(),
                    )?;
                    report.encoder_id = encoder.encoder_id();
                    *encoder_slot.lock().expect("encoder poisoned") = Some(encoder);
                    encoder_dims = dims;
                    force_keyframe = true;
                }
                // the producer's blit must have landed before the encoder reads
                let resources = lease.native_resources::<IoSurface, SharedEventHandle>().map_err(arena)?;
                let fence = match fences.get(&descriptor.fence_id) {
                    Some(f) => f,
                    None => {
                        let f = ConsumerFence::from_handle(&metal, resources.sync_handle).map_err(arena)?;
                        fences.entry(descriptor.fence_id).or_insert(f)
                    }
                };
                if !fence.wait(descriptor.fence_value, 500) {
                    report.encoder_errors += 1;
                    drop(lease);
                    continue;
                }
                let encoder = encoder_slot
                    .lock()
                    .expect("encoder poisoned")
                    .as_ref()
                    .map(Arc::clone)
                    .expect("encoder present");
                let pts = i64::try_from(descriptor.timestamp_ns).unwrap_or(0);
                let pending = Box::into_raw(Box::new(Pending { lease, descriptor }));
                let surface = {
                    // SAFETY: `pending` owns the lease that keeps the surface alive; the shim
                    // retains the surface through a CVPixelBuffer before encode returns.
                    let lease = unsafe { &(*pending).lease };
                    lease
                        .native_resources::<IoSurface, SharedEventHandle>()
                        .map_err(arena)?
                        .surface
                        .clone()
                };
                match encoder.encode(&surface, pts, force_keyframe, pending.cast::<c_void>()) {
                    Ok(()) => {
                        force_keyframe = false;
                        last_encode = Instant::now();
                    }
                    Err(e) => {
                        report.encoder_errors += 1;
                        // SAFETY: the shim did not take the refcon on failure.
                        drop(unsafe { Box::from_raw(pending) });
                        eprintln!("egress: encode failed: {e}");
                    }
                }
                continue;
            }
            AcquireOutcome::Closed => {
                report.ended = "publication closed".into();
                break Ok(());
            }
            AcquireOutcome::Reconfiguration => {
                interest = WaitInterest::ALL;
                if requested_epoch != Some(before.reconfiguration_epoch) {
                    requested_epoch = Some(before.reconfiguration_epoch);
                    consumer.relinquish_configuration();
                    match client.install_configuration(&mut consumer).map_err(arena)? {
                        Some(ConfigurationInstall::Installed) => {
                            fences.clear();
                            continue;
                        }
                        Some(ConfigurationInstall::Stale) | None => {}
                    }
                }
            }
            AcquireOutcome::HoldingLimit => interest = WaitInterest::CAPACITY,
            AcquireOutcome::Empty | AcquireOutcome::Miss { .. } | AcquireOutcome::Gap { .. } => {}
        }
        match consumer
            .wait(before, interest, &cancellation, Some(Duration::from_millis(50)))
            .map_err(arena)?
        {
            WaitOutcome::Cancelled => break Ok(()),
            WaitOutcome::Changed(events) if events.closed => break Ok(()),
            WaitOutcome::Changed(_) | WaitOutcome::TimedOut => {}
        }
    };

    // 5. drain and stop
    if let Some(encoder) = encoder_slot.lock().expect("encoder poisoned").take() {
        let _ = encoder.flush();
        // the flush delivered the last outputs synchronously; dropping invalidates the session
        drop(encoder);
    }
    sender.close();
    stop.store(true, Ordering::Relaxed);
    let _ = sender_thread.join();
    let _ = stop_watch.join();
    drop(control_reader);
    drop(control_writer);
    relay.shutdown();
    let _ = control_thread.join();
    report.input_streams_opened = relay.opened.load(Ordering::Relaxed);
    report.input_bytes_from_link = relay.bytes_in.load(Ordering::Relaxed);
    report.input_bytes_to_link = relay.bytes_out.load(Ordering::Relaxed);
    report.frames_encoded = shared.encoded.load(Ordering::Relaxed);
    report.keyframes = shared.keyframes.load(Ordering::Relaxed);
    report.encoder_errors += shared.errors.load(Ordering::Relaxed);
    report.frames_sent = sender_stats.sent.load(Ordering::Relaxed);
    report.bytes_sent = sender_stats.bytes.load(Ordering::Relaxed);
    report.frames_dropped_at_sender = sender.slots.lock().expect("sender poisoned").dropped;
    drop(consumer);
    outcome.map(|()| report)
}

/// The chroma an egress would use for a given decision; exposed for status.
#[must_use]
pub fn chroma_label(chroma: Chroma) -> &'static str {
    match chroma {
        Chroma::Full => "4:4:4",
        Chroma::Subsampled => "4:2:0",
    }
}

#[must_use]
pub fn local_capabilities() -> CodecCapabilities {
    vt::probe_capabilities()
}
