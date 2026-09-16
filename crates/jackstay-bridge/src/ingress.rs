//! The ingress half: an ordinary native Jackstay producer that decodes what
//! arrives over the link and republishes it as a new local publication.
//!
//! The producer reuses the transport core's macOS backend and therefore owns a
//! fixed BGRA pool. VideoToolbox will not decode into caller-owned surfaces and
//! the backend blits same-format surfaces only, so each decoded frame is first
//! transferred into a bridge-owned BGRA staging surface (the YCbCr to RGB
//! conversion the viewer needs regardless) and then published, which blits it
//! into a pool slot on the GPU and signals the arena fence.
//!
//! Threads: the caller's thread reads the media stream and drives the decoder;
//! VideoToolbox's output thread runs the decoder sink, which transfers and
//! publishes; a control thread sends the target, pings the clock once a second
//! and relays keyframe requests; a maintenance thread polls arena cleanup.

use std::{
    ffi::c_void,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use jackstay::{
    acquisition::arena::{ArenaConfig, FrameDescriptor, PublishOutcome, ReconfigurationStatus},
    model::{ClockDomain, ColorSpace, PixelFormat},
    native::{
        NativeStreamParams,
        arena::NativeArenaProducer,
        macos::{
            IoSurface, MacosCapturedFrame, MacosFrameBackend,
            xpc::arena::{XpcArenaEndpoint, XpcArenaServer},
        },
    },
};
use jackstay_graph::{ChromaPolicy, CodecDecision, Target, decide};

use crate::{
    clock::{ClockEstimator, Estimate, Sample},
    vt::{self, DecodedFrame, Decoder, Transfer},
    wire::{self, CodecConfig, FrameBody, Hello, Kind, Message, Op, Role},
};

#[derive(Debug, thiserror::Error)]
pub enum IngressError {
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

fn arena<E: std::fmt::Display>(e: E) -> IngressError {
    IngressError::Arena(e.to_string())
}

/// How the republished publication is exposed.
pub enum Publish {
    /// An in-process anonymous endpoint, handed to `on_ready`.
    Anonymous,
    /// A launchd-registered Mach service this process was started under.
    Named { service: String, token: Option<String> },
}

#[derive(Debug, Clone)]
pub struct IngressConfig {
    pub chroma_policy: ChromaPolicy,
    pub token: String,
    pub target: Target,
    pub arena: ArenaConfig,
    /// Staging surfaces rotated between decoder output and the pool blit.
    pub staging_depth: usize,
}

impl Default for IngressConfig {
    fn default() -> Self {
        Self {
            chroma_policy: ChromaPolicy::Prefer444,
            token: String::new(),
            target: Target {
                holding: Some(2),
                ..Target::default()
            },
            arena: ArenaConfig {
                resource_capacity: 8,
                retained_history: 2,
                producer_reserve: 1,
                payload_capacity: 0,
                memory_budget: 512 * 1024 * 1024,
                max_incarnations: 4,
                drain_timeout: Duration::from_secs(5),
            },
            staging_depth: 4,
        }
    }
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct IngressReport {
    pub decision: Option<CodecDecision>,
    pub frames_received: u64,
    pub frames_decoded: u64,
    pub frames_published: u64,
    pub frames_dropped_by_arena: u64,
    pub decode_errors: u64,
    pub keyframe_requests: u64,
    pub configurations: u64,
    pub decoder_hardware: Option<bool>,
    pub decoder_pool_shared: Option<bool>,
    pub clock: Option<Estimate>,
    /// Why the media loop ended: `stopped`, `link closed`, or an error.
    pub ended: String,
}

type Producer = Arc<Mutex<NativeArenaProducer<MacosFrameBackend>>>;

struct Republisher {
    producer: Producer,
    transfer: Transfer,
    staging: Vec<IoSurface>,
    next_staging: usize,
    clock: Arc<Mutex<ClockEstimator>>,
    published: AtomicU64,
    dropped: AtomicU64,
    decoded: AtomicU64,
    errors: AtomicU64,
}

impl Republisher {
    fn on_decoded(&mut self, frame: DecodedFrame) {
        // SAFETY: refcon is the Box<FrameDescriptor> the media loop leaked for this frame.
        let descriptor = unsafe { Box::from_raw(frame.refcon.cast::<FrameDescriptor>()) };
        let Some(image) = frame.image.filter(|_| frame.status == 0) else {
            self.errors.fetch_add(1, Ordering::Relaxed);
            return;
        };
        self.decoded.fetch_add(1, Ordering::Relaxed);
        let staging = &self.staging[self.next_staging % self.staging.len()];
        self.next_staging = self.next_staging.wrapping_add(1);
        if let Err(e) = self.transfer.to_surface(&image, staging) {
            eprintln!("ingress: transfer failed: {e}");
            self.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        drop(image);
        let timestamp = self
            .clock
            .lock()
            .ok()
            .and_then(|c| c.estimate())
            .map_or_else(crate::host_now_ns, |e| e.local_from_remote(descriptor.timestamp_ns));
        let captured = MacosCapturedFrame { surface: staging.clone() };
        let outcome = self.producer.lock().expect("producer poisoned").publish(&captured, timestamp);
        match outcome {
            Ok(PublishOutcome::Published { .. }) => {
                self.published.fetch_add(1, Ordering::Relaxed);
            }
            Ok(PublishOutcome::Dropped) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                eprintln!("ingress: publish failed: {e}");
                self.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn params_for(config: &CodecConfig) -> NativeStreamParams {
    NativeStreamParams {
        width: config.width,
        height: config.height,
        pixel_format: PixelFormat::Bgra8Unorm,
        color_space: ColorSpace::Srgb,
        clock_domain: ClockDomain::HostTime,
        modifier: 0,
    }
}

fn control_loop(
    mut control: UnixStream,
    target: Target,
    clock: Arc<Mutex<ClockEstimator>>,
    keyframe_requests: mpsc::Receiver<()>,
    keyframes_sent: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) {
    let Ok(mut reader) = control.try_clone() else { return };
    let mut seq = 0u64;
    if let Ok(m) = wire::json_message(Kind::Target, seq, &target) {
        if m.write_to(&mut control).is_err() {
            return;
        }
    }
    let pending: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let reader_pending = pending.clone();
    let reader_clock = clock;
    let reader_stop = stop.clone();
    let reader_thread = std::thread::spawn(move || {
        while !reader_stop.load(Ordering::Relaxed) {
            let Ok(Some(m)) = Message::read_from(&mut reader) else { break };
            if m.header.kind == Kind::ClockPong {
                let t4 = crate::host_now_ns();
                if let Ok(pong) = wire::ClockPong::decode(&m.body) {
                    let expected = reader_pending.lock().expect("pending poisoned").take();
                    if expected == Some(pong.t1) {
                        reader_clock.lock().expect("clock poisoned").add(Sample {
                            t1: pong.t1,
                            t2: pong.t2,
                            t3: pong.t3,
                            t4,
                        });
                    }
                }
            }
        }
        reader_stop.store(true, Ordering::Relaxed);
    });
    let mut last_ping = Instant::now() - Duration::from_secs(1);
    while !stop.load(Ordering::Relaxed) {
        if last_ping.elapsed() >= Duration::from_secs(1) {
            last_ping = Instant::now();
            let t1 = crate::host_now_ns();
            *pending.lock().expect("pending poisoned") = Some(t1);
            seq += 1;
            if Message::new(Kind::ClockPing, seq, wire::ClockPing { t1 }.encode())
                .write_to(&mut control)
                .is_err()
            {
                break;
            }
        }
        if keyframe_requests.recv_timeout(Duration::from_millis(50)).is_ok() {
            seq += 1;
            if Message::new(Kind::KeyframeRequest, seq, Vec::new()).write_to(&mut control).is_err() {
                break;
            }
            keyframes_sent.fetch_add(1, Ordering::Relaxed);
        }
    }
    stop.store(true, Ordering::Relaxed);
    let _ = control.shutdown(std::net::Shutdown::Both);
    let _ = reader_thread.join();
}

fn maintenance_loop(producer: Producer, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        {
            let mut guard = producer.lock().expect("producer poisoned");
            if let Err(e) = guard.poll_cleanup() {
                eprintln!("ingress: cleanup: {e}");
            }
            match guard.advance_reconfiguration() {
                Ok(ReconfigurationStatus::Ready { .. } | ReconfigurationStatus::PausedCapacity { .. }) => {}
                Err(e) => eprintln!("ingress: reconfiguration: {e}"),
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Runs the ingress half until the link closes or `stop` is set. `on_ready` is
/// called once the publication exists, with its anonymous endpoint when
/// `publish` is [`Publish::Anonymous`]. Blocks the calling thread.
pub fn run(
    publish: Publish,
    mut media: UnixStream,
    control: UnixStream,
    config: IngressConfig,
    stop: Arc<AtomicBool>,
    on_ready: Box<dyn FnOnce(Option<XpcArenaEndpoint>) + Send>,
) -> Result<IngressReport, IngressError> {
    let mut report = IngressReport::default();

    // 1. hello in both directions, then the codec decision
    let mine = vt::probe_capabilities();
    let mut control_reader = control.try_clone()?;
    let peer = Message::read_from(&mut control_reader)?.ok_or(IngressError::Other("link closed before hello".into()))?;
    if peer.header.kind != Kind::Hello {
        return Err(IngressError::BadHello(peer.header.kind));
    }
    let peer: Hello = wire::json_body(&peer)?;
    if peer.token != config.token {
        return Err(IngressError::Token);
    }
    let mut control_writer = control.try_clone()?;
    wire::json_message(
        Kind::Hello,
        0,
        &Hello {
            role: Role::Ingress,
            protocol_version: wire::VERSION,
            capabilities: mine,
            chroma_policy: config.chroma_policy,
            token: config.token.clone(),
        },
    )?
    .write_to(&mut control_writer)?;
    drop(control_writer);
    drop(control_reader);
    let decision = decide(peer.chroma_policy, peer.capabilities, mine)?;
    report.decision = Some(decision.clone());

    // 2. control thread
    let clock = Arc::new(Mutex::new(ClockEstimator::default()));
    let (keyframe_tx, keyframe_rx) = mpsc::channel::<()>();
    let keyframes_sent = Arc::new(AtomicU64::new(0));
    let control_thread = {
        let clock = clock.clone();
        let stop = stop.clone();
        let keyframes_sent = keyframes_sent.clone();
        let target = config.target;
        std::thread::Builder::new()
            .name("jackstay-ingress-control".into())
            .spawn(move || control_loop(control, target, clock, keyframe_rx, keyframes_sent, stop))?
    };

    // 3. the media loop; producer, decoder and republisher appear with the first configuration
    let mut producer: Option<Producer> = None;
    let mut server: Option<XpcArenaServer> = None;
    let mut maintenance: Option<std::thread::JoinHandle<()>> = None;
    let mut republisher: Option<Arc<Mutex<Republisher>>> = None;
    let mut decoder: Option<Decoder> = None;
    let mut on_ready = Some(on_ready);
    let mut current: Option<CodecConfig> = None;
    let mut last_seq: Option<u64> = None;
    let mut waiting_for_keyframe = true;
    let outcome: Result<(), IngressError> = loop {
        if stop.load(Ordering::Relaxed) {
            report.ended = "stopped (control link or interrupt)".into();
            break Ok(());
        }
        let message = match Message::read_from(&mut media) {
            Ok(Some(m)) => m,
            Ok(None) => {
                report.ended = "media link closed".into();
                break Ok(());
            }
            Err(e) => break Err(e.into()),
        };
        match message.header.kind {
            Kind::CodecConfig => {
                let cfg: CodecConfig = wire::json_body(&message)?;
                if cfg.codec != decision.codec || cfg.chroma != decision.chroma {
                    break Err(IngressError::Other(format!(
                        "egress configured {:?}/{:?} but the decision was {:?}/{:?}",
                        cfg.codec, cfg.chroma, decision.codec, decision.chroma
                    )));
                }
                let params = params_for(&cfg);
                match &producer {
                    None => {
                        let backend = MacosFrameBackend::new().map_err(arena)?;
                        let p = Arc::new(Mutex::new(NativeArenaProducer::new(backend, params, config.arena).map_err(arena)?));
                        let endpoint = match &publish {
                            Publish::Anonymous => {
                                let (s, endpoint) = XpcArenaServer::start_anonymous(None, p.clone()).map_err(arena)?;
                                server = Some(s);
                                Some(endpoint)
                            }
                            Publish::Named { service, token } => {
                                server = Some(XpcArenaServer::start_named(service, token.clone(), p.clone()).map_err(arena)?);
                                None
                            }
                        };
                        maintenance = Some({
                            let p = p.clone();
                            let stop = stop.clone();
                            std::thread::Builder::new()
                                .name("jackstay-ingress-maintain".into())
                                .spawn(move || maintenance_loop(p, stop))?
                        });
                        let staging = (0..config.staging_depth.max(2))
                            .map(|_| IoSurface::allocate(cfg.width, cfg.height, PixelFormat::Bgra8Unorm).map_err(arena))
                            .collect::<Result<Vec<_>, _>>()?;
                        republisher = Some(Arc::new(Mutex::new(Republisher {
                            producer: p.clone(),
                            transfer: Transfer::new()?,
                            staging,
                            next_staging: 0,
                            clock: clock.clone(),
                            published: AtomicU64::new(0),
                            dropped: AtomicU64::new(0),
                            decoded: AtomicU64::new(0),
                            errors: AtomicU64::new(0),
                        })));
                        producer = Some(p);
                        if let Some(ready) = on_ready.take() {
                            ready(endpoint);
                        }
                    }
                    Some(p) => {
                        let changed = current.as_ref().is_none_or(|c| c.width != cfg.width || c.height != cfg.height);
                        if changed {
                            p.lock().expect("producer poisoned").reconfigure(params).map_err(arena)?;
                            if let Some(r) = &republisher {
                                let mut r = r.lock().expect("republisher poisoned");
                                r.staging = (0..config.staging_depth.max(2))
                                    .map(|_| IoSurface::allocate(cfg.width, cfg.height, PixelFormat::Bgra8Unorm).map_err(arena))
                                    .collect::<Result<Vec<_>, _>>()?;
                            }
                        }
                    }
                }
                let destination = vt::matched_output_format(cfg.chroma, cfg.full_range);
                let d = match decoder.take() {
                    Some(d) => d,
                    None => {
                        let r = republisher.clone().expect("republisher exists");
                        Decoder::new(
                            cfg.codec,
                            Box::new(move |frame| {
                                r.lock().expect("republisher poisoned").on_decoded(frame);
                            }),
                        )?
                    }
                };
                d.configure(&cfg.parameter_sets, destination)?;
                report.decoder_hardware = d.using_hardware();
                report.decoder_pool_shared = d.pool_shared();
                report.configurations += 1;
                decoder = Some(d);
                current = Some(cfg);
            }
            Kind::Frame => {
                report.frames_received += 1;
                let keyframe = message.header.flags & wire::flags::KEYFRAME != 0;
                let gap = last_seq.is_some_and(|s| message.header.seq != s + 1);
                last_seq = Some(message.header.seq);
                if keyframe {
                    waiting_for_keyframe = false;
                } else if gap || waiting_for_keyframe {
                    if !waiting_for_keyframe {
                        waiting_for_keyframe = true;
                    }
                    let _ = keyframe_tx.send(());
                    report.keyframe_requests += 1;
                    continue;
                }
                let Some(d) = &decoder else {
                    let _ = keyframe_tx.send(());
                    continue;
                };
                let body = FrameBody::decode(&message.body)?;
                for op in body.ops {
                    match op {
                        Op::Video { access_unit, .. } => {
                            let refcon = Box::into_raw(Box::new(body.descriptor));
                            let pts = i64::try_from(body.descriptor.timestamp_ns).unwrap_or(0);
                            if let Err(e) = d.decode(&access_unit, pts, refcon.cast::<c_void>()) {
                                // SAFETY: the shim did not take the refcon on failure.
                                drop(unsafe { Box::from_raw(refcon) });
                                report.decode_errors += 1;
                                eprintln!("ingress: decode failed: {e}");
                                waiting_for_keyframe = true;
                                let _ = keyframe_tx.send(());
                            }
                        }
                    }
                }
            }
            Kind::Hello | Kind::Target | Kind::Release | Kind::KeyframeRequest | Kind::ClockPing | Kind::ClockPong | Kind::Input => {}
        }
    };

    // 4. drain and stop
    if let Some(d) = decoder.take() {
        let _ = d.flush();
        drop(d);
    }
    stop.store(true, Ordering::Relaxed);
    let _ = control_thread.join();
    if let Some(m) = maintenance {
        let _ = m.join();
    }
    if let Some(r) = &republisher {
        let r = r.lock().expect("republisher poisoned");
        report.frames_decoded = r.decoded.load(Ordering::Relaxed);
        report.frames_published = r.published.load(Ordering::Relaxed);
        report.frames_dropped_by_arena = r.dropped.load(Ordering::Relaxed);
        report.decode_errors += r.errors.load(Ordering::Relaxed);
    }
    report.clock = clock.lock().ok().and_then(|c| c.estimate());
    if let Some(p) = &producer {
        let mut guard = p.lock().expect("producer poisoned");
        guard.stop();
        let started = Instant::now();
        loop {
            match guard.poll_shutdown_ready() {
                Ok(true) => break,
                Ok(false) if started.elapsed() < config.arena.drain_timeout => {
                    drop(guard);
                    std::thread::sleep(Duration::from_millis(20));
                    guard = p.lock().expect("producer poisoned");
                }
                Ok(false) => {
                    eprintln!("ingress: producer did not drain within the timeout");
                    break;
                }
                Err(e) => {
                    eprintln!("ingress: shutdown needs recovery: {e}");
                    break;
                }
            }
        }
    }
    drop(server);
    drop(republisher);
    outcome.map(|()| report)
}
