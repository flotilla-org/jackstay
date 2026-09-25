//! Watch a Windows.Graphics.Capture session across lock, unlock and RDP
//! disconnect/reconnect, for the human-coordinated checks in
//! `docs/design/acquisition-d3d11.md`. Needs no Porthole: it opens its own
//! small window, repaints it in a cycle of colours, captures it into a D3D11
//! arena, reads frames back through an in-process consumer, and logs every
//! capture status change with the session's raw WTS state and the adapters.
//!
//! ```text
//! cargo run -p jackstay --features backend-windows --example wgc_session_watch -- --seconds 600 --log wgc-watch.log
//! ```
//!
//! It only captures the window it creates, and closes it on exit.

#[cfg(windows)]
#[path = "../tests/support/window.rs"]
mod window;

#[cfg(not(windows))]
fn main() {
    eprintln!("wgc_session_watch needs Windows");
}

#[cfg(windows)]
fn main() {
    watch::main();
}

#[cfg(windows)]
mod watch {

    use std::{
        fs::File,
        io::Write,
        sync::{Arc, Mutex},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use jackstay::{
        acquisition::arena::{AcquireOutcome, ArenaConsumer},
        native::windows::{
            AdapterSelection, D3d11Device, SharedFenceHandle, SharedTextureHandle, adapters,
            capture::{CapturePolicy, CaptureState, CaptureTarget, Publication, SessionDesktop, WgcCapture, capture_item_for_window},
            setup::D3d11Producer,
        },
    };

    use super::window::TestWindow;

    const COLORS: [[u8; 3]; 3] = [[255, 0, 0], [0, 160, 0], [0, 0, 255]];

    struct Log(Mutex<Option<File>>, Instant);

    impl Log {
        fn line(&self, text: &str) {
            let wall = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |time| time.as_secs_f64());
            let line = format!("[{wall:.3} +{:>8.3}s] {text}", self.1.elapsed().as_secs_f64());
            println!("{line}");
            if let Some(file) = self.0.lock().unwrap().as_mut() {
                let _ = writeln!(file, "{line}");
                let _ = file.flush();
            }
        }
    }

    fn describe_adapters(log: &Log) {
        match adapters() {
            Ok(list) => {
                for adapter in list {
                    let fences = D3d11Device::new(AdapterSelection::Luid(adapter.luid))
                        .and_then(|device| device.probe_shared_fences())
                        .map_or_else(|error| format!("unavailable ({error})"), |()| "ok".to_owned());
                    log.line(&format!(
                        "  adapter {} {:?} vendor {:#x} software {}: shared fences {fences}",
                        adapter.luid, adapter.description, adapter.vendor_id, adapter.software
                    ));
                }
            }
            Err(error) => log.line(&format!("  adapters unavailable: {error}")),
        }
    }

    struct Reader {
        epoch: u64,
        producer: D3d11Producer,
        consumer: ArenaConsumer,
        device: D3d11Device,
        after: u64,
    }

    impl Reader {
        fn attach(capture: &WgcCapture) -> Option<Self> {
            let (epoch, Publication::D3d11(producer)) = capture.publication()? else {
                return None;
            };
            let luid = producer.lock().unwrap().backend().device().luid();
            let device = D3d11Device::new(AdapterSelection::Luid(luid)).ok()?;
            let consumer = producer.lock().unwrap().attach(1).ok()?.into_consumer().ok()?;
            Some(Self {
                epoch,
                producer,
                consumer,
                device,
                after: 0,
            })
        }

        /// The newest frame's size and whether it is one uniform palette colour.
        fn check(&mut self) -> Option<String> {
            match self.consumer.acquire_latest(self.after).ok()? {
                AcquireOutcome::Frame(frame) => {
                    self.after = frame.cursor();
                    let descriptor = *frame.descriptor();
                    let native = frame.native_resources::<SharedTextureHandle, SharedFenceHandle>().ok()?;
                    let texture = self.device.open_texture(native.surface).ok()?;
                    let ready = self.device.open_fence(native.sync_handle).ok()?;
                    if ready.is_abandoned() {
                        return Some("frame abandoned: producer device lost".to_owned());
                    }
                    let pixels = self
                        .device
                        .read_pixels(&texture, &[(&ready, descriptor.fence_value)], Duration::from_secs(2))
                        .ok()?;
                    let uniform = COLORS
                        .iter()
                        .any(|rgb| pixels.chunks_exact(4).all(|pixel| pixel == [rgb[2], rgb[1], rgb[0], 255]));
                    Some(format!(
                        "frame {}x{} gen {} {}",
                        descriptor.width,
                        descriptor.height,
                        descriptor.config_generation,
                        if uniform { "verified" } else { "NOT uniform" }
                    ))
                }
                AcquireOutcome::Reconfiguration => {
                    let offer = self
                        .producer
                        .lock()
                        .unwrap()
                        .configuration_offer(self.consumer.incarnation())
                        .ok()?;
                    if let Some(offer) = offer {
                        offer.install(&mut self.consumer).ok()?;
                    }
                    Some("installed a replacement pool".to_owned())
                }
                AcquireOutcome::Closed => Some("publication closed".to_owned()),
                _ => None,
            }
        }
    }

    pub fn main() {
        let mut seconds = 600;
        let mut path = None;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--seconds" => seconds = args.next().and_then(|value| value.parse().ok()).expect("--seconds N"),
                "--log" => path = args.next(),
                other => panic!("unknown argument {other}"),
            }
        }
        let log = Arc::new(Log(
            Mutex::new(path.map(|path| File::create(path).expect("log file"))),
            Instant::now(),
        ));
        log.line(&format!("session state at start: {:?}", SessionDesktop::query()));
        describe_adapters(&log);

        let mut window = TestWindow::open(320, 200, COLORS[0]);
        let item = capture_item_for_window(window.hwnd()).expect("capture item for our own window");
        let mut capture = WgcCapture::start(
            CaptureTarget::window_client_area(item, window.hwnd()),
            CapturePolicy {
                cursor: false,
                ..CapturePolicy::default()
            },
        )
        .expect("capture start");
        let mut reader = Reader::attach(&capture);
        let mut status = capture.status();
        log.line(&format!("capture: {status:?}"));
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let mut color = 0;
        let mut last_paint = Instant::now();
        let mut last_check = String::new();
        let mut paused_at_dropped = None;
        while Instant::now() < deadline && !status.state.is_terminal() {
            if last_paint.elapsed() >= Duration::from_millis(500) {
                color = (color + 1) % COLORS.len();
                window.set_color(COLORS[color]);
                last_paint = Instant::now();
            }
            let next = capture.wait_for_change(status.revision, Duration::from_millis(250));
            if next.state != status.state || next.epoch != status.epoch || next.mode != status.mode || next.size != status.size {
                log.line(&format!(
                    "state {:?} epoch {} mode {:?} size {:?} published {} dropped {} notes {:?}; session {:?}",
                    next.state,
                    next.epoch,
                    next.mode,
                    next.size,
                    next.frames_published,
                    next.frames_dropped,
                    next.notes,
                    SessionDesktop::query()
                ));
                match (&status.state, &next.state) {
                    (_, CaptureState::Paused(_)) => paused_at_dropped = Some(next.frames_dropped),
                    (CaptureState::Paused(_), _) => {
                        if let Some(before) = paused_at_dropped.take() {
                            log.line(&format!(
                                "  WGC delivered {} frames while paused (dropped, not published)",
                                next.frames_dropped - before
                            ));
                        }
                    }
                    _ => {}
                }
                if next.epoch != status.epoch {
                    log.line("  new epoch: adapters now");
                    describe_adapters(&log);
                    reader = Reader::attach(&capture);
                }
            }
            status = next;
            if reader.as_ref().is_none_or(|reader| reader.epoch != status.epoch) {
                reader = Reader::attach(&capture);
            }
            if let Some(check) = reader.as_mut().and_then(Reader::check)
                && check != last_check
            {
                log.line(&format!("  consumer: {check}"));
                last_check = check;
            }
        }
        log.line(&format!("finished: {:?}", capture.status()));
        capture.stop();
        window.close();
    }
}
