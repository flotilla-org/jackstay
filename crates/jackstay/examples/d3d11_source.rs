//! A D3D11 source for the Windows reference viewer
//! (`tools/capture-viewer-d3d11`) and `scripts/smoke-viewer.ps1`. It serves
//! D3D11 frames on a Local Endpoint named pipe with
//! [`serve_d3d11`](jackstay::native::windows::setup::serve_d3d11) and needs no
//! Porthole.
//!
//! - `--source synthetic` (the default) publishes an animated test pattern from
//!   the backend's own device: a moving bar over a colour gradient, with the
//!   frame number in binary along the top.
//! - `--source window` opens its own small window, recolours it every half
//!   second and publishes it through Windows.Graphics.Capture. It captures
//!   nothing else and closes the window on exit.
//!
//! ```text
//! cargo run -p jackstay --features backend-windows --example d3d11_source -- --endpoint jackstay-d3d11-demo
//! ```
//!
//! Options: `--session-scope`, `--adapter default|warp|<LUID hex>`,
//! `--size WxH`, `--resize-every-ms MS` (alternates with a wider size, to
//! exercise reconfiguration), `--fps N`, `--seconds N` (default 120) and
//! `--clients N` (exit once N viewers have disconnected). It prints one `ready`
//! line once the endpoint is listening.

#[cfg(windows)]
#[path = "../tests/support/window.rs"]
mod window;

#[cfg(not(windows))]
fn main() {
    eprintln!("d3d11_source needs Windows");
}

#[cfg(windows)]
fn main() {
    if let Err(error) = source::main() {
        eprintln!("d3d11_source: {error}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod source {
    use std::{
        io::Write,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU32, Ordering},
            mpsc,
        },
        time::{Duration, Instant},
    };

    use jackstay::{
        acquisition::arena::{ArenaConfig, ReconfigurationStatus},
        local::{Endpoint, Listener, Scope, Transport},
        model::{ClockDomain, ColorSpace, PixelFormat},
        native::{
            NativeStreamParams,
            arena::NativeArenaProducer,
            windows::{
                AdapterLuid, AdapterSelection, D3d11CapturedFrame, D3d11Device, D3d11FrameBackend,
                capture::{CapturePolicy, CaptureTarget, Publication, WgcCapture, capture_item_for_window},
                setup::{D3d11Producer, serve_d3d11},
            },
        },
    };

    use super::window::TestWindow;

    type Error = Box<dyn std::error::Error>;

    struct Options {
        endpoint: String,
        scope: Scope,
        window: bool,
        adapter: AdapterSelection,
        size: (u32, u32),
        resize_every: Option<Duration>,
        fps: u32,
        seconds: u64,
        clients: u32,
    }

    fn parse() -> Result<Options, Error> {
        let mut options = Options {
            endpoint: String::new(),
            scope: Scope::User,
            window: false,
            adapter: AdapterSelection::Default,
            size: (480, 270),
            resize_every: None,
            fps: 60,
            seconds: 120,
            clients: 0,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
            match arg.as_str() {
                "--endpoint" => options.endpoint = value()?,
                "--session-scope" => options.scope = Scope::Session,
                "--source" => {
                    options.window = match value()?.as_str() {
                        "synthetic" => false,
                        "window" => true,
                        other => return Err(format!("unknown source {other}").into()),
                    }
                }
                "--adapter" => {
                    options.adapter = match value()?.as_str() {
                        "default" => AdapterSelection::Default,
                        "warp" => AdapterSelection::Warp,
                        luid => AdapterSelection::Luid(AdapterLuid(u64::from_str_radix(luid.trim_start_matches("0x"), 16)?)),
                    }
                }
                "--size" => {
                    let text = value()?;
                    let (width, height) = text.split_once('x').ok_or("--size WxH")?;
                    options.size = (width.parse()?, height.parse()?);
                }
                "--resize-every-ms" => options.resize_every = Some(Duration::from_millis(value()?.parse()?)),
                "--fps" => options.fps = value()?.parse::<u32>()?.max(1),
                "--seconds" => options.seconds = value()?.parse()?,
                "--clients" => options.clients = value()?.parse()?,
                other => return Err(format!("unknown argument {other}").into()),
            }
        }
        if options.endpoint.is_empty() {
            return Err("--endpoint NAME is required".into());
        }
        Ok(options)
    }

    fn params(size: (u32, u32)) -> NativeStreamParams {
        NativeStreamParams {
            width: size.0,
            height: size.1,
            pixel_format: PixelFormat::Bgra8Unorm,
            color_space: ColorSpace::Srgb,
            clock_domain: ClockDomain::HostTime,
            modifier: 0,
        }
    }

    /// BGRA test pattern for `frame`: a horizontal hue gradient, a white bar
    /// sweeping left to right, and the frame number as 16 bit blocks on top
    /// (white is 1, dark grey is 0).
    fn pattern(size: (u32, u32), frame: u64) -> Vec<u8> {
        let (width, height) = (size.0 as usize, size.1 as usize);
        let mut pixels = vec![0u8; width * height * 4];
        let bar = (frame as usize * 4) % width.max(1);
        let bar_width = (width / 24).max(2);
        let block = (width / 16).max(1);
        let strip = (height / 10).max(1);
        for y in 0..height {
            for x in 0..width {
                let t = x * 255 / width.max(1);
                let v = y * 255 / height.max(1);
                let mut bgra = [(255 - t) as u8, (v / 2) as u8, t as u8, 255];
                if (bar..bar + bar_width).contains(&x) {
                    bgra = [255, 255, 255, 255];
                }
                if y < strip {
                    let bit = x / block;
                    let on = bit < 16 && frame >> (15 - bit) & 1 == 1;
                    let edge = x % block == 0;
                    bgra = if edge {
                        [0, 0, 0, 255]
                    } else if on {
                        [255, 255, 255, 255]
                    } else {
                        [48, 48, 48, 255]
                    };
                }
                pixels[(y * width + x) * 4..][..4].copy_from_slice(&bgra);
            }
        }
        pixels
    }

    enum Source {
        Synthetic {
            device: Arc<D3d11Device>,
            producer: D3d11Producer,
            frame: u64,
        },
        Window {
            window: TestWindow,
            capture: WgcCapture,
            color: usize,
        },
    }

    const COLORS: [[u8; 3]; 4] = [[220, 40, 40], [40, 160, 60], [40, 80, 220], [230, 200, 40]];

    impl Source {
        fn open(options: &Options) -> Result<Self, Error> {
            if options.window {
                let window = TestWindow::open(options.size.0, options.size.1, COLORS[0]);
                let item = capture_item_for_window(window.hwnd())?;
                let capture = WgcCapture::start(
                    CaptureTarget::window_client_area(item, window.hwnd()),
                    CapturePolicy {
                        cursor: false,
                        adapter: options.adapter,
                        ..CapturePolicy::default()
                    },
                )?;
                return Ok(Self::Window { window, capture, color: 0 });
            }
            let device = Arc::new(D3d11Device::new(options.adapter)?);
            device.probe_shared_fences()?;
            let producer = NativeArenaProducer::new(
                D3d11FrameBackend::new(Arc::clone(&device)),
                params(options.size),
                ArenaConfig {
                    resource_capacity: 6,
                    retained_history: 2,
                    producer_reserve: 1,
                    payload_capacity: 0,
                    memory_budget: 256 * 1024 * 1024,
                    max_incarnations: 8,
                    drain_timeout: Duration::from_secs(5),
                },
            )?;
            Ok(Self::Synthetic {
                device,
                producer: Arc::new(Mutex::new(producer)),
                frame: 0,
            })
        }

        fn producer(&self) -> Result<D3d11Producer, Error> {
            match self {
                Self::Synthetic { producer, .. } => Ok(Arc::clone(producer)),
                Self::Window { capture, .. } => match capture.publication() {
                    Some((_, Publication::D3d11(producer))) => Ok(producer),
                    other => Err(format!("the capture does not publish D3D11 frames: {other:?}").into()),
                },
            }
        }

        fn describe(&self) -> Result<String, Error> {
            let producer = self.producer()?;
            let producer = producer.lock().map_err(|_| "producer mutex poisoned")?;
            let adapter = producer.backend().device().adapter();
            let kind = match self {
                Self::Synthetic { .. } => "synthetic",
                Self::Window { .. } => "window",
            };
            Ok(format!(
                "adapter={} description={:?} source={kind}",
                adapter.luid, adapter.description
            ))
        }

        /// Publish the next frame at `size`, or repaint the captured window.
        fn tick(&mut self, size: (u32, u32), elapsed: Duration) -> Result<(), Error> {
            match self {
                Self::Synthetic { device, producer, frame } => {
                    let mut producer = producer.lock().map_err(|_| "producer mutex poisoned")?;
                    if (producer.params().width, producer.params().height) != size {
                        producer.reconfigure(params(size))?;
                    }
                    if !matches!(producer.advance_reconfiguration()?, ReconfigurationStatus::Ready { .. }) {
                        // Paused for capacity until held frames drain.
                        return Ok(());
                    }
                    *frame += 1;
                    let texture = device.upload(size.0, size.1, PixelFormat::Bgra8Unorm, &pattern(size, *frame))?;
                    producer.publish(&D3d11CapturedFrame::new(texture), elapsed.as_nanos() as u64)?;
                    let _ = producer.poll_cleanup();
                }
                Self::Window { window, capture, color } => {
                    let next = (elapsed.as_millis() / 500) as usize % COLORS.len();
                    if next != *color {
                        *color = next;
                        window.set_color(COLORS[next]);
                    }
                    if capture.status().size != size {
                        window.resize(size.0, size.1);
                    }
                    if capture.status().state.is_terminal() {
                        return Err(format!("capture ended: {:?}", capture.status()).into());
                    }
                }
            }
            Ok(())
        }

        fn stop(self) {
            match self {
                Self::Synthetic { producer, .. } => {
                    let Ok(mut producer) = producer.lock() else { return };
                    producer.stop();
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !producer.poll_shutdown_ready().unwrap_or(true) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                Self::Window {
                    mut window, mut capture, ..
                } => {
                    capture.stop();
                    window.close();
                }
            }
        }
    }

    pub fn main() -> Result<(), Error> {
        let options = parse()?;
        let endpoint = Endpoint::new(options.scope, &options.endpoint, Transport::LocalStream)?;
        let mut source = Source::open(&options)?;
        let listener = Arc::new(Listener::bind(&endpoint)?);
        let (finished, disconnected) = mpsc::channel();
        let served = Arc::new(AtomicU32::new(0));
        let producer = source.producer()?;
        let accepting = Arc::clone(&listener);
        let counting = Arc::clone(&served);
        std::thread::spawn(move || {
            while let Ok(connection) = accepting.accept() {
                let producer = Arc::clone(&producer);
                let finished = finished.clone();
                let number = counting.fetch_add(1, Ordering::Relaxed) + 1;
                println!("client {number}: pid {}", connection.peer().pid);
                std::thread::spawn(move || {
                    let result = serve_d3d11(connection.into_stream(), producer);
                    println!("client {number}: setup ended: {result:?}");
                    let _ = finished.send(());
                });
            }
        });
        println!("ready endpoint={} {}", endpoint.render()?, source.describe()?);
        std::io::stdout().flush()?;

        let start = Instant::now();
        let deadline = start + Duration::from_secs(options.seconds);
        let interval = Duration::from_secs(1) / options.fps;
        let wide = (options.size.0 * 3 / 2, options.size.1);
        let mut done = 0;
        let mut next = start;
        let result = loop {
            let now = Instant::now();
            if now >= deadline {
                break Ok(());
            }
            while disconnected.try_recv().is_ok() {
                done += 1;
            }
            if options.clients != 0 && done >= options.clients {
                break Ok(());
            }
            let elapsed = now - start;
            let size = match options.resize_every {
                Some(every) if (elapsed.as_millis() / every.as_millis().max(1)) % 2 == 1 => wide,
                _ => options.size,
            };
            if let Err(error) = source.tick(size, elapsed) {
                break Err(error);
            }
            next += interval;
            if let Some(wait) = next.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            } else {
                next = Instant::now();
            }
        };
        listener.cancel();
        println!("served {} clients", served.load(Ordering::Relaxed));
        source.stop();
        result
    }
}
