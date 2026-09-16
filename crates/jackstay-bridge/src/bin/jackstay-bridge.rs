//! `jackstay-bridge`: the egress and ingress halves as processes, and a
//! loopback that runs both on one machine against a synthetic native producer.
//!
//! `loopback` is the standalone proof for the bridge: it needs no porthole, no
//! launchd and no capture permission. A synthetic producer publishes drawn
//! frames over an anonymous XPC endpoint; the egress half consumes them,
//! encodes and sends over a socket pair; the ingress half decodes and
//! republishes over another anonymous endpoint; a verifying consumer attaches
//! there and compares what it acquires with what was drawn.
//!
//! `loopback --viewer-service NAME` instead registers the ingress half with
//! launchd as a Mach service so the reference viewer can attach to the
//! republished publication from another process:
//! `capture-viewer-sdl --native --mach-service NAME --token TOKEN`.
//!
//! `ingress` and `egress` are the halves as processes, connecting to the media
//! and control sockets a coordinator forwarded.

#[cfg(not(all(target_os = "macos", feature = "backend-macos")))]
fn main() {
    eprintln!("jackstay-bridge: build with --features backend-macos on macOS");
    std::process::exit(2);
}

#[cfg(all(target_os = "macos", feature = "backend-macos"))]
fn main() {
    if let Err(e) = macos::main() {
        eprintln!("jackstay-bridge: {e}");
        std::process::exit(1);
    }
}

#[cfg(all(target_os = "macos", feature = "backend-macos"))]
mod macos {
    use std::{
        os::unix::net::{UnixListener, UnixStream},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::{Duration, Instant},
    };

    use jackstay::{
        acquisition::arena::{AcquireOutcome, ArenaConfig, Cancellation, PublishOutcome, WaitInterest, WaitOutcome},
        model::{ClockDomain, ColorSpace, PixelFormat},
        native::{
            NativeStreamParams,
            arena::NativeArenaProducer,
            macos::{
                ConsumerFence, IoSurface, MacosCapturedFrame, MacosFrameBackend, MetalContext, SharedEventHandle,
                xpc::arena::{XpcArenaClient, XpcArenaEndpoint, XpcArenaServer},
            },
        },
    };
    use jackstay_bridge::{egress, ingress};
    use jackstay_graph::ChromaPolicy;

    type Error = Box<dyn std::error::Error + Send + Sync>;

    /// One machine-readable status line on stdout, for a coordinator
    /// (`jackstay_graph::export`) watching this process.
    fn event(e: &jackstay_graph::export::HalfEvent) {
        use std::io::Write;
        if let Ok(line) = serde_json::to_string(e) {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    }

    struct Args {
        command: String,
        frames: u64,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        chroma: ChromaPolicy,
        media: Option<String>,
        control: Option<String>,
        service: Option<String>,
        viewer_service: Option<String>,
        viewer_token: Option<String>,
        source_token: Option<String>,
        link_token: String,
        /// Bind the media and control paths and accept one peer each, instead of connecting.
        listen: bool,
        /// Also publish the ingress over a generic CPU setup socket at this path.
        cpu_socket: Option<std::path::PathBuf>,
    }

    fn parse() -> Result<Args, Error> {
        let mut args = Args {
            command: "loopback".into(),
            frames: 120,
            width: 1280,
            height: 720,
            fps: 30,
            bitrate: 20_000_000,
            chroma: ChromaPolicy::Prefer444,
            media: None,
            control: None,
            service: None,
            viewer_service: None,
            viewer_token: None,
            source_token: None,
            link_token: String::new(),
            listen: false,
            cpu_socket: None,
        };
        let mut frames_given = false;
        let mut it = std::env::args().skip(1);
        if let Some(c) = it.next() {
            args.command = c;
        }
        while let Some(a) = it.next() {
            let mut value = || it.next().ok_or_else(|| format!("{a} needs a value"));
            match a.as_str() {
                "--frames" => {
                    args.frames = value()?.parse()?;
                    frames_given = true;
                }
                "--width" => args.width = value()?.parse()?,
                "--height" => args.height = value()?.parse()?,
                "--fps" => args.fps = value()?.parse()?,
                "--bitrate" => args.bitrate = value()?.parse()?,
                "--chroma" => {
                    args.chroma = match value()?.as_str() {
                        "require444" => ChromaPolicy::Require444,
                        "prefer444" => ChromaPolicy::Prefer444,
                        "any" => ChromaPolicy::Any,
                        other => return Err(format!("unknown chroma policy {other}").into()),
                    }
                }
                "--media" => args.media = Some(value()?),
                "--control" => args.control = Some(value()?),
                "--service" => args.service = Some(value()?),
                "--viewer-service" => args.viewer_service = Some(value()?),
                "--viewer-token" => args.viewer_token = Some(value()?),
                "--source-token" => args.source_token = Some(value()?),
                "--link-token" => args.link_token = value()?,
                "--listen" => args.listen = true,
                "--cpu-socket" => args.cpu_socket = Some(value()?.into()),
                other => return Err(format!("unknown argument {other}").into()),
            }
        }
        if args.viewer_service.is_some() && !frames_given {
            args.frames = 0; // run until interrupted
        }
        Ok(args)
    }

    pub fn main() -> Result<(), Error> {
        let args = parse()?;
        match args.command.as_str() {
            "loopback" if args.viewer_service.is_some() => loopback_viewer(&args),
            "loopback" => loopback(&args),
            "ingress" => ingress_process(&args),
            "egress" => egress_process(&args),
            "synthetic" => synthetic(&args),
            "ingress-service" => ingress_service(&args),
            "synthetic-child" => synthetic_child(&args),
            "probe" => {
                let caps = egress::local_capabilities();
                println!("{}", serde_json::to_string_pretty(&caps)?);
                Ok(())
            }
            other => Err(format!("unknown command {other}; use loopback, ingress, egress, synthetic or probe").into()),
        }
    }

    // ---- interrupt --------------------------------------------------------------------

    static INTERRUPTED: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_interrupt(_: libc::c_int) {
        INTERRUPTED.store(true, Ordering::Relaxed);
    }

    fn watch_interrupt(stop: Arc<AtomicBool>) {
        // SAFETY: installing a signal handler that only touches an atomic.
        unsafe {
            libc::signal(libc::SIGINT, on_interrupt as extern "C" fn(libc::c_int) as libc::sighandler_t);
            libc::signal(libc::SIGTERM, on_interrupt as extern "C" fn(libc::c_int) as libc::sighandler_t);
        }
        std::thread::spawn(move || {
            while !INTERRUPTED.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
            stop.store(true, Ordering::Relaxed);
        });
    }

    // ---- the synthetic source ------------------------------------------------------------

    /// Draws a frame whose content is a function of `index`: coloured panels
    /// plus a moving bar, sharp edges everywhere. The verifier regenerates it.
    fn draw(width: u32, height: u32, index: u64) -> Vec<u8> {
        let mut px = vec![0u8; (width * height * 4) as usize];
        let bar = ((index * 7) % u64::from(width)) as u32;
        for y in 0..height {
            for x in 0..width {
                let o = ((y * width + x) * 4) as usize;
                let panel = (x * 5 / width) as u8;
                let (b, g, r) = match panel {
                    0 => (40u8, 40u8, 230u8),
                    1 => (230, 150, 40),
                    2 => (60, 180, 60),
                    3 => (30, 150, 230),
                    _ => (200, 60, 200),
                };
                let cell = ((x / 16 + y / 16) % 2) == 0;
                let (mut b, mut g, mut r) = if cell { (b, g, r) } else { (255, 255, 255) };
                if x >= bar && x < bar + 24 {
                    b = 0;
                    g = 0;
                    r = 0;
                }
                px[o] = b;
                px[o + 1] = g;
                px[o + 2] = r;
                px[o + 3] = 255;
            }
        }
        px
    }

    fn mean_abs_error(a: &[u8], b: &[u8]) -> f64 {
        let mut sum = 0u64;
        let mut n = 0u64;
        for (pa, pb) in a.chunks(4).zip(b.chunks(4)) {
            for c in 0..3 {
                sum += u64::from(pa[c].abs_diff(pb[c]));
                n += 1;
            }
        }
        sum as f64 / n.max(1) as f64
    }

    type Producer = Arc<Mutex<NativeArenaProducer<MacosFrameBackend>>>;
    type Drawn = Arc<Mutex<Vec<(u64, u64)>>>;

    struct SourceHandle {
        producer: Producer,
        _server: XpcArenaServer,
        endpoint: Option<XpcArenaEndpoint>,
        thread: std::thread::JoinHandle<u64>,
        drawn: Drawn,
    }

    /// Publishes drawn frames at `fps` until `frames` are out (0 = until stopped),
    /// over an anonymous endpoint or, with `named`, a launchd-registered service.
    fn start_source(args: &Args, stop: Arc<AtomicBool>, named: Option<(String, Option<String>)>) -> Result<SourceHandle, Error> {
        let (width, height, fps, frames) = (args.width, args.height, args.fps.max(1), args.frames);
        let params = NativeStreamParams {
            width,
            height,
            pixel_format: PixelFormat::Bgra8Unorm,
            color_space: ColorSpace::Srgb,
            clock_domain: ClockDomain::HostTime,
            modifier: 0,
        };
        let backend = MacosFrameBackend::new()?;
        let producer = Arc::new(Mutex::new(NativeArenaProducer::new(
            backend,
            params,
            ArenaConfig {
                resource_capacity: 8,
                retained_history: 2,
                producer_reserve: 1,
                payload_capacity: 0,
                memory_budget: 256 * 1024 * 1024,
                max_incarnations: 2,
                drain_timeout: Duration::from_secs(5),
            },
        )?));
        let (server, endpoint) = match named {
            Some((service, token)) => (XpcArenaServer::start_named(&service, token, producer.clone())?, None),
            None => {
                let (s, e) = XpcArenaServer::start_anonymous(None, producer.clone())?;
                (s, Some(e))
            }
        };
        let drawn: Drawn = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let producer = producer.clone();
            let drawn = drawn.clone();
            std::thread::Builder::new().name("loopback-producer".into()).spawn(move || {
                let mut published = 0u64;
                let mut index = 0u64;
                let period = Duration::from_micros(1_000_000 / u64::from(fps));
                // draw a small cycle of frames once; drawing is the slow part of a synthetic source
                let cycle: Vec<Vec<u8>> = (0..24).map(|i| draw(width, height, i)).collect();
                while !stop.load(Ordering::Relaxed) && (frames == 0 || published < frames) {
                    let started = Instant::now();
                    let surface = IoSurface::allocate(width, height, PixelFormat::Bgra8Unorm).expect("surface");
                    surface.write_pixels(&cycle[(index % 24) as usize]).expect("write");
                    let timestamp = jackstay_bridge::host_now_ns();
                    let outcome = producer
                        .lock()
                        .expect("producer")
                        .publish(&MacosCapturedFrame { surface }, timestamp);
                    match outcome {
                        Ok(PublishOutcome::Published { .. }) => {
                            let mut d = drawn.lock().expect("drawn");
                            d.push((timestamp, index % 24));
                            if d.len() > 256 {
                                d.remove(0);
                            }
                            published += 1;
                        }
                        Ok(PublishOutcome::Dropped) => {}
                        Err(e) => {
                            eprintln!("loopback: source publish failed: {e}");
                            break;
                        }
                    }
                    index += 1;
                    let _ = producer.lock().expect("producer").poll_cleanup();
                    if let Some(rest) = period.checked_sub(started.elapsed()) {
                        std::thread::sleep(rest);
                    }
                }
                published
            })?
        };
        Ok(SourceHandle {
            producer,
            _server: server,
            endpoint,
            thread,
            drawn,
        })
    }

    fn spawn_egress(
        source: egress::Source,
        media: UnixStream,
        control: UnixStream,
        args: &Args,
        token: String,
        stop: Arc<AtomicBool>,
    ) -> Result<std::thread::JoinHandle<Result<egress::EgressReport, egress::EgressError>>, Error> {
        let config = egress::EgressConfig {
            chroma_policy: args.chroma,
            bitrate_bps: args.bitrate,
            token,
            ..egress::EgressConfig::default()
        };
        Ok(std::thread::Builder::new()
            .name("loopback-egress".into())
            .spawn(move || egress::run(source, media, control, config, stop, Box::new(|_| {})))?)
    }

    fn print_reports(published: u64, args: &Args, egress_report: &egress::EgressReport, ingress_report: Option<&ingress::IngressReport>) {
        println!("source: published {published} frames at {}x{}", args.width, args.height);
        println!(
            "egress: {} ({:?}) acquired {} encoded {} sent {} dropped-at-sender {} keyframes {} errors {} bytes {}",
            egress_report.encoder_id,
            egress_report.decision.as_ref().map(|d| (d.codec, d.chroma)),
            egress_report.frames_acquired,
            egress_report.frames_encoded,
            egress_report.frames_sent,
            egress_report.frames_dropped_at_sender,
            egress_report.keyframes,
            egress_report.encoder_errors,
            egress_report.bytes_sent
        );
        if let Some(r) = ingress_report {
            println!(
                "ingress: received {} decoded {} published {} arena-dropped {} errors {} keyframe-requests {} hw-decode {:?} pool-shared {:?}",
                r.frames_received,
                r.frames_decoded,
                r.frames_published,
                r.frames_dropped_by_arena,
                r.decode_errors,
                r.keyframe_requests,
                r.decoder_hardware,
                r.decoder_pool_shared
            );
            if args.cpu_socket.is_some() {
                println!(
                    "ingress cpu: published {} dropped {} errors {}",
                    r.cpu_frames_published, r.cpu_frames_dropped, r.cpu_errors
                );
            }
            if let Some(c) = r.clock {
                println!(
                    "clock: offset {} ns drift {} ppb rtt-min {} us samples {}",
                    c.offset_ns,
                    c.drift_ppb,
                    c.rtt_min_ns / 1000,
                    c.samples
                );
            }
        }
    }

    // ---- in-process loopback with a verifying consumer ----------------------------------------------

    fn loopback(args: &Args) -> Result<(), Error> {
        let (width, height, fps) = (args.width, args.height, args.fps.max(1));
        let stop = Arc::new(AtomicBool::new(false));
        watch_interrupt(stop.clone());
        let mut source = start_source(args, stop.clone(), None)?;
        let (media_a, media_b) = UnixStream::pair()?;
        let (control_a, control_b) = UnixStream::pair()?;
        let token = jackstay_graph::mint_token()?;

        let (ready_tx, ready_rx) = mpsc::channel();
        let ingress_thread = {
            let stop = stop.clone();
            let config = ingress::IngressConfig {
                chroma_policy: args.chroma,
                token: token.clone(),
                cpu_socket: args.cpu_socket.clone(),
                ..ingress::IngressConfig::default()
            };
            std::thread::Builder::new().name("loopback-ingress".into()).spawn(move || {
                ingress::run(
                    ingress::Publish::Anonymous,
                    media_b,
                    control_b,
                    config,
                    stop,
                    Box::new(move |endpoint| {
                        let _ = ready_tx.send(endpoint);
                    }),
                )
            })?
        };
        let source_endpoint = source.endpoint.take().expect("anonymous endpoint");
        let egress_thread = spawn_egress(
            egress::Source::Endpoint(source_endpoint),
            media_a,
            control_a,
            args,
            token,
            stop.clone(),
        )?;

        let endpoint = ready_rx
            .recv_timeout(Duration::from_secs(20))
            .map_err(|_| "ingress did not publish within 20 s")?
            .ok_or("ingress published a named service, not an endpoint")?;
        let mut client = XpcArenaClient::connect_endpoint(&endpoint)?;
        let mut consumer = client.attach(2)?;
        let metal = MetalContext::new()?;
        let cancellation = Cancellation::new()?;
        let mut last_cursor = 0u64;
        let (mut verified, mut acquired, mut mismatched) = (0u64, 0u64, 0u64);
        let mut worst = 0f64;
        let mut fence: Option<(u64, ConsumerFence)> = None;
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        let budget = if args.frames == 0 {
            Duration::from_secs(3600)
        } else {
            Duration::from_millis(args.frames * 1000 / u64::from(fps))
        };
        let deadline = Instant::now() + Duration::from_secs(5) + budget;
        while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
            if source.thread.is_finished() && acquired > 0 && verified + mismatched >= args.frames.saturating_sub(4) {
                break;
            }
            let before = consumer.events();
            let mut interest = WaitInterest::DATA;
            match consumer.acquire_latest(last_cursor)? {
                AcquireOutcome::Frame(lease) => {
                    acquired += 1;
                    let d = *lease.descriptor();
                    last_cursor = d.cursor;
                    let resources = lease.native_resources::<IoSurface, SharedEventHandle>()?;
                    if fence.as_ref().is_none_or(|(id, _)| *id != d.fence_id) {
                        fence = Some((d.fence_id, ConsumerFence::from_handle(&metal, resources.sync_handle)?));
                    }
                    let f = &fence.as_ref().expect("fence").1;
                    if !f.wait(d.fence_value, 1000) {
                        eprintln!("loopback: republished frame never became ready");
                        mismatched += 1;
                        continue;
                    }
                    resources.surface.read_pixels(&mut pixels)?;
                    let expected = {
                        let drawn = source.drawn.lock().expect("drawn");
                        drawn
                            .iter()
                            .min_by_key(|(ts, _)| ts.abs_diff(d.timestamp_ns))
                            .map(|(ts, i)| (*ts, *i))
                    };
                    match expected {
                        Some((ts, index)) if ts.abs_diff(d.timestamp_ns) < 5_000_000 => {
                            let error = mean_abs_error(&pixels, &draw(width, height, index));
                            worst = worst.max(error);
                            if error < 6.0 {
                                verified += 1;
                            } else {
                                mismatched += 1;
                                eprintln!("loopback: frame {index} mean abs error {error:.2}");
                            }
                        }
                        _ => {
                            mismatched += 1;
                            eprintln!("loopback: no drawn frame near timestamp {}", d.timestamp_ns);
                        }
                    }
                    drop(lease);
                    continue;
                }
                AcquireOutcome::Closed => break,
                AcquireOutcome::Reconfiguration => {
                    interest = WaitInterest::ALL;
                    consumer.relinquish_configuration();
                    if client.install_configuration(&mut consumer)?.is_some() {
                        fence = None;
                        continue;
                    }
                }
                AcquireOutcome::HoldingLimit => interest = WaitInterest::CAPACITY,
                AcquireOutcome::Empty | AcquireOutcome::Miss { .. } | AcquireOutcome::Gap { .. } => {}
            }
            if let WaitOutcome::Changed(e) = consumer.wait(before, interest, &cancellation, Some(Duration::from_millis(100)))? {
                if e.closed {
                    break;
                }
            }
        }
        drop(consumer);
        drop(client);

        stop.store(true, Ordering::Relaxed);
        let published = source.thread.join().map_err(|_| "producer thread panicked")?;
        source.producer.lock().expect("producer").stop();
        let egress_report = egress_thread.join().map_err(|_| "egress thread panicked")??;
        let ingress_report = ingress_thread.join().map_err(|_| "ingress thread panicked")??;
        print_reports(published, args, &egress_report, Some(&ingress_report));
        println!("verifier: acquired {acquired} verified {verified} mismatched {mismatched} worst-mean-abs-error {worst:.2}");
        if verified == 0 || mismatched > 0 {
            return Err("loopback verification failed".into());
        }
        Ok(())
    }

    // ---- loopback with a launchd-registered ingress for the reference viewer ---------------------------

    use jackstay_graph::launchd::LaunchdJob;

    fn accept_with_timeout(listener: &UnixListener, timeout: Duration) -> Result<UnixStream, Error> {
        listener.set_nonblocking(true)?;
        let deadline = Instant::now() + timeout;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false)?;
                    return Ok(stream);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() > deadline {
                        return Err("the ingress process did not connect in time".into());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn loopback_viewer(args: &Args) -> Result<(), Error> {
        let service = args.viewer_service.clone().expect("viewer service");
        let viewer_token = args.viewer_token.clone().unwrap_or(jackstay_graph::mint_token()?);
        let link_token = jackstay_graph::mint_token()?;
        let stop = Arc::new(AtomicBool::new(false));
        watch_interrupt(stop.clone());

        let directory = std::env::temp_dir().join(format!("jackstay-bridge-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let media_path = directory.join("media.sock");
        let control_path = directory.join("control.sock");
        let media_listener = UnixListener::bind(&media_path)?;
        let control_listener = UnixListener::bind(&control_path)?;
        let exe = std::env::current_exe()?.to_str().ok_or("executable path")?.to_owned();
        let program = vec![
            exe,
            "ingress".into(),
            "--media".into(),
            media_path.to_str().ok_or("path")?.to_owned(),
            "--control".into(),
            control_path.to_str().ok_or("path")?.to_owned(),
            "--service".into(),
            service.clone(),
            "--viewer-token".into(),
            viewer_token.clone(),
            "--link-token".into(),
            link_token.clone(),
        ];
        let job = LaunchdJob::bootstrap(&service, &directory, &program)?;
        let media = accept_with_timeout(&media_listener, Duration::from_secs(15))?;
        let control = accept_with_timeout(&control_listener, Duration::from_secs(15))?;

        let mut source = start_source(args, stop.clone(), None)?;
        let source_endpoint = source.endpoint.take().expect("anonymous endpoint");
        let egress_thread = spawn_egress(
            egress::Source::Endpoint(source_endpoint),
            media,
            control,
            args,
            link_token,
            stop.clone(),
        )?;
        println!("ingress registered with launchd as {service}");
        println!("attach the reference viewer with:");
        println!("  build/viewer/capture-viewer-sdl --native --mach-service {service} --token {viewer_token}");
        println!("logs in {}; interrupt to stop", directory.display());

        while !stop.load(Ordering::Relaxed) && !source.thread.is_finished() && !egress_thread.is_finished() {
            std::thread::sleep(Duration::from_millis(100));
        }
        stop.store(true, Ordering::Relaxed);
        let published = source.thread.join().map_err(|_| "producer thread panicked")?;
        source.producer.lock().expect("producer").stop();
        let egress_report = egress_thread.join().map_err(|_| "egress thread panicked")??;
        print_reports(published, args, &egress_report, None);
        drop(job);
        let _ = std::fs::remove_file(&media_path);
        let _ = std::fs::remove_file(&control_path);
        Ok(())
    }

    // ---- the halves as processes ----------------------------------------------------------------------

    fn connect(path: Option<&String>, what: &str) -> Result<UnixStream, Error> {
        let path = path.ok_or_else(|| format!("--{what} PATH is required"))?;
        Ok(UnixStream::connect(path)?)
    }

    fn bind(path: Option<&String>, what: &str) -> Result<(UnixListener, String), Error> {
        let path = path.ok_or_else(|| format!("--{what} PATH is required"))?.clone();
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        eprintln!("listening on {path}");
        event(&jackstay_graph::export::HalfEvent::Listening { path: path.clone() });
        Ok((listener, path))
    }

    fn link(args: &Args) -> Result<(UnixStream, UnixStream), Error> {
        if args.listen {
            // Bind both before accepting either: the peer connects to both in
            // quick succession, possibly through a forward that fails the open
            // outright when the target path does not exist yet.
            let (media_listener, media_path) = bind(args.media.as_ref(), "media")?;
            let (control_listener, control_path) = bind(args.control.as_ref(), "control")?;
            let (media, _) = media_listener.accept()?;
            let (control, _) = control_listener.accept()?;
            let _ = std::fs::remove_file(media_path);
            let _ = std::fs::remove_file(control_path);
            Ok((media, control))
        } else {
            Ok((connect(args.media.as_ref(), "media")?, connect(args.control.as_ref(), "control")?))
        }
    }

    /// Registers a launchd job that runs `synthetic-child` as the named service,
    /// then waits for an interrupt and removes the job.
    fn synthetic(args: &Args) -> Result<(), Error> {
        let service = args.service.clone().ok_or("--service NAME is required")?;
        let token = args.source_token.clone().unwrap_or(jackstay_graph::mint_token()?);
        let stop = Arc::new(AtomicBool::new(false));
        watch_interrupt(stop.clone());
        let directory = std::env::temp_dir().join(format!("jackstay-synthetic-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let exe = std::env::current_exe()?.to_str().ok_or("executable path")?.to_owned();
        let program = vec![
            exe,
            "synthetic-child".into(),
            "--service".into(),
            service.clone(),
            "--source-token".into(),
            token.clone(),
            "--width".into(),
            args.width.to_string(),
            "--height".into(),
            args.height.to_string(),
            "--fps".into(),
            args.fps.to_string(),
            "--frames".into(),
            args.frames.to_string(),
        ];
        let job = LaunchdJob::bootstrap(&service, &directory, &program)?;
        println!("synthetic source registered with launchd as {service}");
        println!("attach with: --service {service} --source-token {token}");
        println!("logs in {}; interrupt to stop", directory.display());
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
        drop(job);
        Ok(())
    }

    /// Registers a launchd job that runs `ingress` as the named service the
    /// viewer attaches to, connecting to the media and control paths given (for
    /// example the local ends of an SSH forward), then waits for an interrupt.
    fn ingress_service(args: &Args) -> Result<(), Error> {
        let service = args.service.clone().ok_or("--service NAME is required")?;
        let viewer_token = args.viewer_token.clone().unwrap_or(jackstay_graph::mint_token()?);
        let media = args.media.clone().ok_or("--media PATH is required")?;
        let control = args.control.clone().ok_or("--control PATH is required")?;
        let stop = Arc::new(AtomicBool::new(false));
        watch_interrupt(stop.clone());
        let directory = std::env::temp_dir().join(format!("jackstay-ingress-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let exe = std::env::current_exe()?.to_str().ok_or("executable path")?.to_owned();
        let program = vec![
            exe,
            "ingress".into(),
            "--media".into(),
            media,
            "--control".into(),
            control,
            "--service".into(),
            service.clone(),
            "--viewer-token".into(),
            viewer_token.clone(),
            "--link-token".into(),
            args.link_token.clone(),
        ];
        let job = LaunchdJob::bootstrap(&service, &directory, &program)?;
        println!("ingress registered with launchd as {service}");
        println!("attach the reference viewer with:");
        println!("  build/viewer/capture-viewer-sdl --native --mach-service {service} --token {viewer_token}");
        println!("logs in {}; interrupt to stop", directory.display());
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
        drop(job);
        Ok(())
    }

    fn synthetic_child(args: &Args) -> Result<(), Error> {
        let service = args.service.clone().ok_or("--service NAME is required")?;
        let stop = Arc::new(AtomicBool::new(false));
        watch_interrupt(stop.clone());
        let source = start_source(args, stop.clone(), Some((service, args.source_token.clone())))?;
        eprintln!("synthetic source is up");
        let published = source.thread.join().map_err(|_| "producer thread panicked")?;
        source.producer.lock().expect("producer").stop();
        eprintln!("synthetic source published {published} frames");
        Ok(())
    }

    fn ingress_process(args: &Args) -> Result<(), Error> {
        let (media, control) = link(args)?;
        let service = args.service.clone().ok_or("--service NAME is required")?;
        let stop = Arc::new(AtomicBool::new(false));
        watch_interrupt(stop.clone());
        let config = ingress::IngressConfig {
            chroma_policy: args.chroma,
            token: args.link_token.clone(),
            cpu_socket: args.cpu_socket.clone(),
            ..ingress::IngressConfig::default()
        };
        let viewer_token = args.viewer_token.clone();
        let cpu_socket = args.cpu_socket.as_ref().map(|p| p.to_string_lossy().into_owned());
        let announce = format!(
            "ingress: publication is up; attach with --native --mach-service {service}{}{}",
            viewer_token.as_ref().map_or(String::new(), |t| format!(" --token {t}")),
            cpu_socket.as_ref().map_or(String::new(), |p| format!(", or --cpu-socket {p}"))
        );
        let up = jackstay_graph::export::HalfEvent::PublicationUp {
            service: service.clone(),
            token: viewer_token.clone(),
            cpu_socket,
        };
        let report = ingress::run(
            ingress::Publish::Named {
                service,
                token: viewer_token,
            },
            media,
            control,
            config,
            stop,
            Box::new(move |_| {
                eprintln!("{announce}");
                event(&up);
            }),
        );
        match report {
            Ok(report) => {
                eprintln!("ingress: {report:?}");
                event(&jackstay_graph::export::HalfEvent::Report {
                    report: serde_json::to_value(&report)?,
                });
                Ok(())
            }
            Err(e) => {
                event(&jackstay_graph::export::HalfEvent::Failed { message: e.to_string() });
                Err(e.into())
            }
        }
    }

    fn egress_process(args: &Args) -> Result<(), Error> {
        let (media, control) = link(args)?;
        let service = args.service.clone().ok_or("--service NAME is required")?;
        let stop = Arc::new(AtomicBool::new(false));
        watch_interrupt(stop.clone());
        let config = egress::EgressConfig {
            chroma_policy: args.chroma,
            bitrate_bps: args.bitrate,
            token: args.link_token.clone(),
            ..egress::EgressConfig::default()
        };
        let report = egress::run(
            egress::Source::Named {
                service,
                token: args.source_token.clone(),
            },
            media,
            control,
            config,
            stop,
            Box::new(|decision| {
                event(&jackstay_graph::export::HalfEvent::Ready {
                    decision: decision.clone(),
                })
            }),
        );
        match report {
            Ok(report) => {
                eprintln!("egress: {report:?}");
                event(&jackstay_graph::export::HalfEvent::Report {
                    report: serde_json::to_value(&report)?,
                });
                Ok(())
            }
            Err(e) => {
                event(&jackstay_graph::export::HalfEvent::Failed { message: e.to_string() });
                Err(e.into())
            }
        }
    }
}
