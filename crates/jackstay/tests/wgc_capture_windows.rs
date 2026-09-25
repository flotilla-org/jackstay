#![cfg(all(windows, feature = "backend-windows"))]
//! Windows.Graphics.Capture of a window this test creates and owns. These need
//! an interactive desktop, so they are ignored by default:
//!
//! ```text
//! cargo test -p jackstay --locked --features backend-windows --test wgc_capture_windows -- --ignored --test-threads=1 --nocapture
//! ```

#[path = "support/window.rs"]
mod window;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use jackstay::{
    acquisition::arena::{AcquireOutcome, ArenaConsumer, ConfigurationInstall, FrameLease},
    native::windows::{
        AdapterSelection, D3d11Device, SharedFenceHandle, SharedTextureHandle,
        capture::{
            CapturePolicy, CaptureState, CaptureStatus, CaptureTarget, DesktopMonitor, DesktopUnavailable, OutputSize, Publication,
            PublicationMode, PublicationPreference, WgcCapture, capture_item_for_window,
        },
    },
};
use window::TestWindow;

const TIMEOUT: Duration = Duration::from_secs(10);
const RED: [u8; 3] = [255, 0, 0];
const BLUE: [u8; 3] = [0, 0, 255];
const GREEN: [u8; 3] = [0, 255, 0];

fn bgra(rgb: [u8; 3]) -> [u8; 4] {
    [rgb[2], rgb[1], rgb[0], 255]
}

#[derive(Debug, Default)]
struct FakeDesktop(Mutex<Option<DesktopUnavailable>>);

impl DesktopMonitor for FakeDesktop {
    fn unavailable(&self) -> Option<DesktopUnavailable> {
        *self.0.lock().unwrap()
    }
}

fn wait(capture: &WgcCapture, what: &str, mut done: impl FnMut(&CaptureStatus) -> bool) -> CaptureStatus {
    let deadline = Instant::now() + TIMEOUT;
    let mut status = capture.status();
    while !done(&status) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}: {status:?}");
        status = capture.wait_for_change(status.revision, Duration::from_millis(100));
    }
    status
}

fn start(window: &TestWindow, policy: CapturePolicy) -> WgcCapture {
    let item = capture_item_for_window(window.hwnd()).unwrap();
    WgcCapture::start(CaptureTarget::window_client_area(item, window.hwnd()), policy).unwrap()
}

fn d3d11(capture: &WgcCapture) -> (u64, jackstay::native::windows::setup::D3d11Producer) {
    match capture.publication().unwrap() {
        (epoch, Publication::D3d11(producer)) => (epoch, producer),
        (_, other) => panic!("expected D3D11 publication, got {other:?}"),
    }
}

struct Reader {
    device: D3d11Device,
}

impl Reader {
    fn pixels(&self, frame: &FrameLease) -> Vec<u8> {
        let native = frame.native_resources::<SharedTextureHandle, SharedFenceHandle>().unwrap();
        let texture = self.device.open_texture(native.surface).unwrap();
        let ready = self.device.open_fence(native.sync_handle).unwrap();
        self.device
            .read_pixels(&texture, &[(&ready, frame.descriptor().fence_value)], TIMEOUT)
            .unwrap()
    }

    /// Acquire until the newest frame is uniformly `rgb` at `size`; install
    /// replacement pools on the way.
    fn until_uniform(
        &self,
        producer: &jackstay::native::windows::setup::D3d11Producer,
        consumer: &mut ArenaConsumer,
        rgb: [u8; 3],
        size: (u32, u32),
    ) -> FrameLease {
        let deadline = Instant::now() + TIMEOUT;
        let mut after = 0;
        loop {
            assert!(Instant::now() < deadline, "no uniform {rgb:?} {size:?} frame");
            match consumer.acquire_latest(after).unwrap() {
                AcquireOutcome::Frame(frame) => {
                    after = frame.cursor();
                    let descriptor = *frame.descriptor();
                    let pixels = self.pixels(&frame);
                    if std::env::var_os("JACKSTAY_WGC_DEBUG").is_some() {
                        let mut colors = std::collections::BTreeMap::new();
                        for (index, pixel) in pixels.chunks_exact(4).enumerate() {
                            colors.entry(pixel.to_vec()).or_insert((0, index)).0 += 1;
                        }
                        eprintln!("frame {}x{}: {colors:?}", descriptor.width, descriptor.height);
                    }
                    if (descriptor.width, descriptor.height) == size && pixels.chunks_exact(4).all(|pixel| pixel == bgra(rgb)) {
                        return frame;
                    }
                }
                AcquireOutcome::Reconfiguration => {
                    let offer = producer.lock().unwrap().configuration_offer(consumer.incarnation()).unwrap();
                    if let Some(offer) = offer {
                        assert_eq!(offer.install(consumer).unwrap(), ConfigurationInstall::Installed);
                    }
                }
                AcquireOutcome::Closed => panic!("publication closed"),
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

#[test]
#[ignore = "needs an interactive desktop; creates and captures its own window"]
fn a_captured_window_follows_resize_pause_device_loss_and_close() {
    let mut window = TestWindow::open(160, 120, RED);
    let desktop = Arc::new(FakeDesktop::default());
    let capture = start(
        &window,
        CapturePolicy {
            cursor: false,
            desktop: Some(desktop.clone()),
            ..CapturePolicy::default()
        },
    );
    let status = wait(&capture, "first frame", |status| status.frames_published > 0);
    eprintln!("started: {status:?}");
    let PublicationMode::D3d11 { adapter } = &status.mode else {
        panic!("fences should work here: {:?}", status.mode)
    };
    let (epoch, producer) = d3d11(&capture);
    assert_eq!(epoch, 1);
    let reader = Reader {
        device: D3d11Device::new(AdapterSelection::Luid(adapter.luid)).unwrap(),
    };
    let mut consumer = producer.lock().unwrap().attach(2).unwrap().into_consumer().unwrap();
    let red = reader.until_uniform(&producer, &mut consumer, RED, (160, 120));
    window.set_color(BLUE);
    let blue = reader.until_uniform(&producer, &mut consumer, BLUE, (160, 120));
    drop(blue);
    // Resize while holding the red frame: a new pool generation.
    window.resize(200, 90);
    wait(&capture, "resized publication", |status| status.size == (200, 90));
    let resized = reader.until_uniform(&producer, &mut consumer, BLUE, (200, 90));
    assert_ne!(resized.descriptor().pool_id, red.descriptor().pool_id);
    assert!(
        reader.pixels(&red).chunks_exact(4).all(|pixel| pixel == bgra(RED)),
        "held frame changed across resize"
    );
    drop((red, resized));
    // A locked desktop pauses publication without failing.
    *desktop.0.lock().unwrap() = Some(DesktopUnavailable::Locked);
    let paused = wait(&capture, "pause", |status| {
        status.state == CaptureState::Paused(DesktopUnavailable::Locked)
    });
    window.set_color(GREEN);
    std::thread::sleep(Duration::from_millis(500));
    let still = capture.status();
    assert_eq!(still.frames_published, paused.frames_published, "published while paused");
    *desktop.0.lock().unwrap() = None;
    wait(&capture, "resume", |status| status.state == CaptureState::Running);
    window.set_color(RED);
    drop(reader.until_uniform(&producer, &mut consumer, RED, (200, 90)));
    // Device loss: a new device, session and arena under a new epoch.
    capture.simulate_device_loss("test");
    let recovered = wait(&capture, "recovery", |status| {
        status.epoch == 2 && status.state == CaptureState::Running
    });
    eprintln!("recovered: {recovered:?}");
    let deadline = Instant::now() + TIMEOUT;
    while !matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Closed) {
        assert!(Instant::now() < deadline, "old publication stayed open");
        std::thread::sleep(Duration::from_millis(10));
    }
    let (epoch, replacement) = d3d11(&capture);
    assert_eq!(epoch, 2);
    let mut consumer = replacement.lock().unwrap().attach(1).unwrap().into_consumer().unwrap();
    window.set_color(BLUE);
    drop(reader.until_uniform(&replacement, &mut consumer, BLUE, (200, 90)));
    // Closing the window closes the item: terminal.
    window.close();
    let closed = wait(&capture, "closed", |status| status.state == CaptureState::Closed);
    eprintln!("closed: {closed:?}");
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Closed));
}

#[test]
#[ignore = "needs an interactive desktop; creates and captures its own window"]
fn output_size_policies_scale_preserving_aspect() {
    let window = TestWindow::open(160, 80, BLUE);
    let capture = start(
        &window,
        CapturePolicy {
            cursor: false,
            output_size: OutputSize::Fixed { width: 64, height: 64 },
            ..CapturePolicy::default()
        },
    );
    let status = wait(&capture, "first frame", |status| status.frames_published > 0);
    assert_eq!(status.size, (64, 64));
    let (_, producer) = d3d11(&capture);
    let PublicationMode::D3d11 { adapter } = status.mode else {
        panic!()
    };
    let reader = Reader {
        device: D3d11Device::new(AdapterSelection::Luid(adapter.luid)).unwrap(),
    };
    let consumer = producer.lock().unwrap().attach(1).unwrap().into_consumer().unwrap();
    window.set_color(BLUE);
    let deadline = Instant::now() + TIMEOUT;
    let pixels = loop {
        assert!(Instant::now() < deadline);
        if let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() {
            break reader.pixels(&frame);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let at = |x: usize, y: usize| &pixels[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4];
    // 160x80 fits 64x32, centred: black bars above and below.
    assert_eq!(at(32, 4), [0, 0, 0, 255]);
    assert_eq!(at(32, 32), bgra(BLUE));
    assert_eq!(at(32, 60), [0, 0, 0, 255]);
    drop(capture);

    let capture = start(
        &window,
        CapturePolicy {
            cursor: false,
            output_size: OutputSize::Fit {
                max_width: 80,
                max_height: 80,
            },
            ..CapturePolicy::default()
        },
    );
    assert_eq!(wait(&capture, "first frame", |status| status.frames_published > 0).size, (80, 40));
}

#[test]
#[ignore = "needs an interactive desktop; creates and captures its own window"]
fn cpu_publication_is_reported_and_carries_the_client_area() {
    let window = TestWindow::open(160, 40, GREEN);
    let capture = start(
        &window,
        CapturePolicy {
            cursor: false,
            publication: PublicationPreference::Cpu,
            ..CapturePolicy::default()
        },
    );
    let status = wait(&capture, "first frame", |status| status.frames_published > 0);
    let PublicationMode::Cpu { reason, .. } = &status.mode else {
        panic!("expected CPU publication: {:?}", status.mode)
    };
    assert!(reason.contains("requested"), "{reason}");
    let Some((_, Publication::Cpu(producer))) = capture.publication() else {
        panic!("expected a CPU arena")
    };
    let consumer = jackstay::acquisition::arena::ArenaConsumer::from_grant(producer.lock().unwrap().attach(1).unwrap()).unwrap();
    window.set_color(GREEN);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        assert!(Instant::now() < deadline, "no green CPU frame");
        if let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() {
            let descriptor = frame.descriptor();
            assert_eq!((descriptor.width, descriptor.height, descriptor.stride), (160, 40, 160 * 4));
            if frame.bytes().chunks_exact(4).all(|pixel| pixel == bgra(GREEN)) {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "needs an interactive desktop; creates and captures its own window"]
fn capture_publishes_on_every_adapter_with_a_minimum_update_interval() {
    let window = TestWindow::open(160, 120, RED);
    for adapter in jackstay::native::windows::adapters().unwrap() {
        let capture = start(
            &window,
            CapturePolicy {
                cursor: false,
                adapter: AdapterSelection::Luid(adapter.luid),
                min_update_interval: Some(Duration::from_millis(20)),
                ..CapturePolicy::default()
            },
        );
        let status = wait(&capture, "first frame", |status| status.frames_published > 0);
        eprintln!(
            "adapter {} {:?}: {:?} notes {:?}",
            adapter.luid, adapter.description, status.mode, status.notes
        );
        assert_eq!(status.mode, PublicationMode::D3d11 { adapter: adapter.clone() });
        assert!(status.notes.is_empty(), "{:?}", status.notes);
        let (_, producer) = d3d11(&capture);
        let reader = Reader {
            device: D3d11Device::new(AdapterSelection::Luid(adapter.luid)).unwrap(),
        };
        let mut consumer = producer.lock().unwrap().attach(1).unwrap().into_consumer().unwrap();
        window.set_color(GREEN);
        drop(reader.until_uniform(&producer, &mut consumer, GREEN, (160, 120)));
        window.set_color(RED);
    }
}

#[test]
#[ignore = "needs an interactive desktop; creates and captures its own window"]
fn a_device_loss_known_while_the_desktop_is_unavailable_waits_without_spinning() {
    let window = TestWindow::open(160, 120, RED);
    let desktop = Arc::new(FakeDesktop::default());
    let capture = start(
        &window,
        CapturePolicy {
            cursor: false,
            desktop: Some(desktop.clone()),
            ..CapturePolicy::default()
        },
    );
    wait(&capture, "first frame", |status| status.frames_published > 0);
    *desktop.0.lock().unwrap() = Some(DesktopUnavailable::Disconnected);
    capture.simulate_device_loss("injected while disconnected");
    wait(&capture, "pause", |status| {
        status.state == CaptureState::Paused(DesktopUnavailable::Disconnected)
    });
    // A pending loss the watcher already knows about must not wake it: at a
    // 100 ms interval, one second is about ten iterations, not thousands.
    let before = capture.watch_iterations();
    std::thread::sleep(Duration::from_secs(1));
    let iterations = capture.watch_iterations() - before;
    assert!(iterations <= 20, "watcher spun {iterations} times in one second");
    assert_eq!(capture.status().epoch, 1, "recovered while the desktop was unavailable");
    *desktop.0.lock().unwrap() = None;
    let recovered = wait(&capture, "recovery", |status| {
        status.epoch == 2 && status.state == CaptureState::Running
    });
    eprintln!("{iterations} watch iterations in 1 s while paused with a pending loss; then {recovered:?}");
}

#[test]
#[ignore = "needs an interactive desktop; creates and captures its own window"]
fn a_minimized_window_starts_and_publishes_once_restored() {
    let window = TestWindow::open(160, 120, BLUE);
    window.minimize();
    std::thread::sleep(Duration::from_millis(300));
    for output_size in [
        OutputSize::Source,
        OutputSize::Fit {
            max_width: 0,
            max_height: 0,
        },
    ] {
        let capture = start(
            &window,
            CapturePolicy {
                cursor: false,
                output_size,
                ..CapturePolicy::default()
            },
        );
        eprintln!("started minimized with {output_size:?}: {:?}", capture.status());
    }
    let capture = start(
        &window,
        CapturePolicy {
            cursor: false,
            ..CapturePolicy::default()
        },
    );
    window.restore();
    let (_, producer) = d3d11(&capture);
    let status = wait(&capture, "restored frame", |status| {
        status.size == (160, 120) && status.frames_published > 0
    });
    let PublicationMode::D3d11 { adapter } = status.mode else {
        panic!()
    };
    let reader = Reader {
        device: D3d11Device::new(AdapterSelection::Luid(adapter.luid)).unwrap(),
    };
    let mut consumer = producer.lock().unwrap().attach(1).unwrap().into_consumer().unwrap();
    window.set_color(BLUE);
    drop(reader.until_uniform(&producer, &mut consumer, BLUE, (160, 120)));
}

#[test]
#[ignore = "needs an interactive desktop; creates and captures its own window"]
fn a_replacement_paused_for_capacity_publishes_a_static_window_once_capacity_frees() {
    use jackstay::{
        model::{ClockDomain, ColorSpace, PixelFormat},
        native::{NativeStreamParams, arena::ArenaNativeBackend, windows::D3d11FrameBackend},
    };
    let window = TestWindow::open(160, 120, RED);
    // Room for one pool of either size plus arena metadata, never two.
    let device = Arc::new(D3d11Device::new(AdapterSelection::Default).unwrap());
    let bound = D3d11FrameBackend::new(device)
        .pool_allocation_upper_bound(
            &NativeStreamParams {
                width: 200,
                height: 120,
                pixel_format: PixelFormat::Bgra8Unorm,
                color_space: ColorSpace::Srgb,
                clock_domain: ClockDomain::HostTime,
                modifier: 0,
            },
            6,
        )
        .unwrap();
    let capture = start(
        &window,
        CapturePolicy {
            cursor: false,
            arena: jackstay::native::windows::capture::CaptureArena {
                memory_budget: bound + 16 * 4096,
                ..Default::default()
            },
            ..CapturePolicy::default()
        },
    );
    let status = wait(&capture, "first frame", |status| status.frames_published > 0);
    let PublicationMode::D3d11 { adapter } = status.mode else {
        panic!()
    };
    let (_, producer) = d3d11(&capture);
    let reader = Reader {
        device: D3d11Device::new(AdapterSelection::Luid(adapter.luid)).unwrap(),
    };
    let mut consumer = producer.lock().unwrap().attach(1).unwrap().into_consumer().unwrap();
    let held = reader.until_uniform(&producer, &mut consumer, RED, (160, 120));
    window.resize(200, 90);
    let deadline = Instant::now() + TIMEOUT;
    while !producer.lock().unwrap().reconfiguration_pending() {
        assert!(Instant::now() < deadline, "the replacement never paused for capacity");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The window is now static: WGC sends no further frames.
    std::thread::sleep(Duration::from_millis(500));
    let before = capture.status();
    assert_eq!(before.size, (160, 120));
    drop(held);
    consumer.relinquish_configuration();
    let after = wait(&capture, "deferred publication", |status| status.size == (200, 90));
    assert!(after.frames_published > before.frames_published);
    drop(reader.until_uniform(&producer, &mut consumer, RED, (200, 90)));
}

#[test]
#[ignore = "needs an interactive desktop; creates and captures its own window"]
fn a_failed_start_leaves_the_item_reusable() {
    let window = TestWindow::open(160, 120, GREEN);
    let item = capture_item_for_window(window.hwnd()).unwrap();
    let starved = CapturePolicy {
        arena: jackstay::native::windows::capture::CaptureArena {
            memory_budget: 1,
            ..Default::default()
        },
        ..CapturePolicy::default()
    };
    for _ in 0..3 {
        assert!(WgcCapture::start(CaptureTarget::window_client_area(item.clone(), window.hwnd()), starved.clone()).is_err());
    }
    let capture = WgcCapture::start(
        CaptureTarget::window_client_area(item, window.hwnd()),
        CapturePolicy {
            cursor: false,
            ..CapturePolicy::default()
        },
    )
    .unwrap();
    wait(&capture, "first frame", |status| status.frames_published > 0);
}

#[test]
#[ignore = "needs an interactive desktop; creates and captures its own window"]
fn published_timestamps_never_go_backwards_under_rapid_repaints() {
    let window = TestWindow::open(160, 120, RED);
    for publication in [PublicationPreference::D3d11, PublicationPreference::Cpu] {
        let capture = start(
            &window,
            CapturePolicy {
                cursor: false,
                publication,
                ..CapturePolicy::default()
            },
        );
        wait(&capture, "first frame", |status| status.frames_published > 0);
        let consumer = match capture.publication().unwrap().1 {
            Publication::D3d11(producer) => producer.lock().unwrap().attach(1).unwrap().into_consumer().unwrap(),
            Publication::Cpu(producer) => ArenaConsumer::from_grant(producer.lock().unwrap().attach(1).unwrap()).unwrap(),
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        let (mut after, mut last, mut seen) = (0, 0, 0);
        let mut color = 0;
        while Instant::now() < deadline {
            color += 1;
            window.set_color([RED, GREEN, BLUE][color % 3]);
            if let AcquireOutcome::Frame(frame) = consumer.acquire_latest(after).unwrap() {
                after = frame.cursor();
                let timestamp = frame.descriptor().timestamp_ns;
                assert!(timestamp > last, "{publication:?}: timestamp went from {last} to {timestamp}");
                last = timestamp;
                seen += 1;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        eprintln!("{publication:?}: {seen} frames with increasing timestamps; {:?}", capture.status());
        assert!(seen > 10);
    }
}
