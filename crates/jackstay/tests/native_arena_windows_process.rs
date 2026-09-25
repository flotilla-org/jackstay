#![cfg(all(windows, feature = "backend-windows"))]
//! D3D11 frames between separate processes over the named-pipe setup channel:
//! resize, a held frame across it, and killing either process while GPU work
//! is pending. The producer is synthetic (uploaded patterns) or a real
//! Windows.Graphics.Capture session on a window the test creates. Window
//! scenarios need an interactive desktop and are ignored by default:
//!
//! ```text
//! cargo test -p jackstay --locked --features backend-windows --test native_arena_windows_process -- --include-ignored --test-threads=1 --nocapture
//! ```

#[path = "support/child.rs"]
mod child;
#[path = "support/setup.rs"]
mod setup;
#[path = "support/window.rs"]
mod window;

use std::{
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use child::KillOnDrop;
use jackstay::{
    acquisition::arena::{AcquireOutcome, ArenaConfig, ArenaConsumer, FrameLease},
    local::{Endpoint, Scope, Transport},
    model::{ClockDomain, ColorSpace, PixelFormat},
    native::{
        NativeStreamParams,
        arena::NativeArenaProducer,
        windows::{
            AdapterLuid, AdapterSelection, D3d11CapturedFrame, D3d11Device, D3d11Fence, D3d11FrameBackend, SharedFenceHandle,
            SharedTextureHandle,
            capture::{CapturePolicy, CaptureTarget, Publication, WgcCapture, capture_item_for_window},
            setup::{D3d11Producer, D3d11SetupClient, serve_d3d11},
        },
    },
};
use serde::{Deserialize, Serialize};
use setup::{Link, Listener};
use window::TestWindow;

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum Check {
    Pattern(u8),
    Uniform([u8; 4]),
}

impl Check {
    fn matches(self, width: u32, height: u32, pixels: &[u8]) -> bool {
        match self {
            Self::Pattern(seed) => pixels == pattern(width, height, seed),
            Self::Uniform(bgra) => pixels.chunks_exact(4).all(|pixel| pixel == bgra),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum Order {
    /// Acquire until the newest frame matches; hold it or release it through
    /// the GPU release fence.
    Expect {
        size: (u32, u32),
        check: Check,
        hold: bool,
    },
    /// Re-read every held frame.
    CheckHeld,
    /// Release held frames through GPU work and the release fence.
    ReleaseHeld,
    /// Acquire a matching frame and submit GPU work on it behind the gate
    /// handle that follows, deferring its release; then wait to be killed.
    PendingGpu {
        size: (u32, u32),
        check: Check,
    },
    // Producer-side orders.
    Publish {
        seed: u8,
        size: (u32, u32),
    },
    Paint {
        rgb: [u8; 3],
        size: (u32, u32),
    },
    /// Queue a GPU wait on the gate that follows ahead of the next staged
    /// frame, then publish it; wait to be killed.
    GatedPublish {
        seed: u8,
        rgb: [u8; 3],
    },
}

#[derive(Debug, Serialize, Deserialize)]
enum Report {
    Ready { adapter: u64 },
    Seen { cursor: u64, slot: u32, fence_value: u64 },
    HeldOk { count: usize },
    Released,
    Pending { slot: u32, fence_value: u64, release_value: u64 },
    Published { frames: u64 },
    GatedPublished { frames: u64 },
}

fn pattern(width: u32, height: u32, seed: u8) -> Vec<u8> {
    (0..width * height * 4)
        .map(|index| (index as u8).wrapping_mul(7).wrapping_add(seed))
        .collect()
}

fn bgra(rgb: [u8; 3]) -> [u8; 4] {
    [rgb[2], rgb[1], rgb[0], 255]
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

fn config() -> ArenaConfig {
    ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 0,
        memory_budget: 256 * 1024 * 1024,
        max_incarnations: 4,
        drain_timeout: Duration::from_secs(3),
    }
}

fn endpoint(name: &str) -> Endpoint {
    Endpoint::new(Scope::User, name, Transport::LocalStream).unwrap()
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}-{}", std::process::id(), rand_seed())
}

fn rand_seed() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos()
}

fn spawn(helper: &str, setup: &str, control: &Listener, source: &str) -> KillOnDrop {
    KillOnDrop(
        Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", helper, "--nocapture", "--test-threads=1"])
            .env("JACKSTAY_D3D11_SETUP", setup)
            .env("JACKSTAY_D3D11_CONTROL", control.address())
            .env("JACKSTAY_D3D11_SOURCE", source)
            .spawn()
            .unwrap(),
    )
}

/// Serve every connection to `name` against `producer` until the listener
/// is dropped (cancelled).
fn serve(name: &str, producer: D3d11Producer) -> Arc<jackstay::local::Listener> {
    let listener = Arc::new(jackstay::local::Listener::bind(&endpoint(name)).unwrap());
    let accepting = Arc::clone(&listener);
    std::thread::spawn(move || {
        while let Ok(connection) = accepting.accept() {
            let producer = Arc::clone(&producer);
            std::thread::spawn(move || {
                let _ = serve_d3d11(connection.into_stream(), producer);
            });
        }
    });
    listener
}

/// Consumer side, shared by both process roles.
struct Consumer {
    setup: D3d11SetupClient,
    consumer: ArenaConsumer,
    device: D3d11Device,
    release: Arc<D3d11Fence>,
    binding: jackstay::acquisition::arena::ConsumerReleaseTimeline,
    release_value: u64,
    after: u64,
    held: Vec<(FrameLease, Check)>,
}

impl Consumer {
    fn connect(name: &str, holding: u32) -> Self {
        let stream = jackstay::local::connect(&endpoint(name)).unwrap().into_stream();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        // SAFETY: the endpoint's server is this test's verified producer.
        let mut setup = unsafe { D3d11SetupClient::from_stream(stream) };
        let adapter = setup.describe().unwrap().adapter;
        let device = D3d11Device::new(AdapterSelection::Luid(adapter.luid)).unwrap();
        let consumer = setup.attach(holding, &device).unwrap();
        let release = Arc::new(device.create_shared_fence().unwrap());
        let binding = setup.register_release_timeline(&consumer, release.clone()).unwrap();
        Self {
            setup,
            consumer,
            device,
            release,
            binding,
            release_value: 0,
            after: 0,
            held: Vec::new(),
        }
    }

    fn read(&self, frame: &FrameLease) -> Vec<u8> {
        let native = frame.native_resources::<SharedTextureHandle, SharedFenceHandle>().unwrap();
        let texture = self.device.open_texture(native.surface).unwrap();
        let ready = self.device.open_fence(native.sync_handle).unwrap();
        self.device
            .read_pixels(&texture, &[(&ready, frame.descriptor().fence_value)], TIMEOUT)
            .unwrap()
    }

    fn expect(&mut self, size: (u32, u32), check: Check) -> FrameLease {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "no {check:?} frame at {size:?}");
            match self.consumer.acquire_latest(self.after).unwrap() {
                AcquireOutcome::Frame(frame) => {
                    self.after = frame.cursor();
                    let descriptor = *frame.descriptor();
                    if (descriptor.width, descriptor.height) == size && check.matches(size.0, size.1, &self.read(&frame)) {
                        return frame;
                    }
                }
                AcquireOutcome::Reconfiguration => {
                    self.setup.install_configuration(&mut self.consumer).unwrap();
                }
                AcquireOutcome::Closed => panic!("publication closed"),
                _ => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    /// Release through GPU work: read the frame on the GPU, then signal the
    /// release fence behind that read.
    fn release_gpu(&mut self, frame: FrameLease) {
        let native = frame.native_resources::<SharedTextureHandle, SharedFenceHandle>().unwrap();
        let texture = self.device.open_texture(native.surface).unwrap();
        let ready = self.device.open_fence(native.sync_handle).unwrap();
        self.release_value += 1;
        let pending = self
            .device
            .submit_readback(
                &texture,
                &[(&ready, frame.descriptor().fence_value)],
                Some((&self.release, self.release_value)),
            )
            .unwrap();
        frame.defer_release(&self.binding, self.release_value).unwrap();
        pending.finish(&self.device, TIMEOUT).unwrap();
    }

    fn obey(&mut self, link: &mut Link, order: Order) {
        match order {
            Order::Expect { size, check, hold } => {
                let frame = self.expect(size, check);
                let descriptor = *frame.descriptor();
                if hold {
                    self.held.push((frame, check));
                } else {
                    self.release_gpu(frame);
                }
                link.send(&Report::Seen {
                    cursor: descriptor.cursor,
                    slot: descriptor.slot_id,
                    fence_value: descriptor.fence_value,
                });
            }
            Order::CheckHeld => {
                for (frame, check) in &self.held {
                    let descriptor = frame.descriptor();
                    assert!(
                        check.matches(descriptor.width, descriptor.height, &self.read(frame)),
                        "a held frame changed"
                    );
                }
                link.send(&Report::HeldOk { count: self.held.len() });
            }
            Order::ReleaseHeld => {
                for (frame, _) in std::mem::take(&mut self.held) {
                    self.release_gpu(frame);
                }
                link.send(&Report::Released);
            }
            Order::PendingGpu { size, check } => {
                let gate = SharedFenceHandle::from_owned(link.recv_objects(1).pop().unwrap());
                let gate = self.device.open_fence(&gate).unwrap();
                let frame = self.expect(size, check);
                let native = frame.native_resources::<SharedTextureHandle, SharedFenceHandle>().unwrap();
                let texture = self.device.open_texture(native.surface).unwrap();
                let ready = self.device.open_fence(native.sync_handle).unwrap();
                self.release_value += 1;
                let _pending = self
                    .device
                    .submit_readback(
                        &texture,
                        &[(&ready, frame.descriptor().fence_value), (&gate, 1)],
                        Some((&self.release, self.release_value)),
                    )
                    .unwrap();
                let descriptor = *frame.descriptor();
                frame.defer_release(&self.binding, self.release_value).unwrap();
                link.send(&Report::Pending {
                    slot: descriptor.slot_id,
                    fence_value: descriptor.fence_value,
                    release_value: self.release_value,
                });
                // A copy of the release fence, so the parent can watch it after death.
                link.offer_objects(&[self.release.export_handle().unwrap().into_owned()]);
                loop {
                    std::thread::park();
                }
            }
            other => panic!("consumer cannot {other:?}"),
        }
    }
}

#[test]
#[ignore = "subprocess helper for the D3D11 process scenarios"]
fn d3d11_consumer_child() {
    let (Ok(setup), Ok(control)) = (std::env::var("JACKSTAY_D3D11_SETUP"), std::env::var("JACKSTAY_D3D11_CONTROL")) else {
        return;
    };
    let mut link = Link::connect(&control);
    let mut consumer = Consumer::connect(&setup, 2);
    link.send(&Report::Ready {
        adapter: consumer.device.luid().0,
    });
    loop {
        let order = link.recv::<Order>();
        consumer.obey(&mut link, order);
    }
}

/// A producer for either role: synthetic patterns or a captured window.
enum Source {
    Synthetic { device: Arc<D3d11Device>, producer: D3d11Producer },
    Window { window: TestWindow, capture: WgcCapture },
}

impl Source {
    fn synthetic(adapter: AdapterSelection) -> Self {
        let device = Arc::new(D3d11Device::new(adapter).unwrap());
        let producer = NativeArenaProducer::new(D3d11FrameBackend::new(Arc::clone(&device)), params((64, 48)), config()).unwrap();
        Self::Synthetic {
            device,
            producer: Arc::new(Mutex::new(producer)),
        }
    }

    fn window() -> Self {
        let window = TestWindow::open(160, 120, [255, 0, 0]);
        let item = capture_item_for_window(window.hwnd()).unwrap();
        let capture = WgcCapture::start(
            CaptureTarget::window_client_area(item, window.hwnd()),
            CapturePolicy {
                cursor: false,
                ..CapturePolicy::default()
            },
        )
        .unwrap();
        Self::Window { window, capture }
    }

    fn producer(&self) -> D3d11Producer {
        match self {
            Self::Synthetic { producer, .. } => Arc::clone(producer),
            Self::Window { capture, .. } => match capture.publication().unwrap().1 {
                Publication::D3d11(producer) => producer,
                other => panic!("expected D3D11 publication, got {other:?}"),
            },
        }
    }

    fn device(&self) -> Arc<D3d11Device> {
        Arc::clone(self.producer().lock().unwrap().backend().device())
    }

    fn published(&self) -> u64 {
        match self {
            Self::Synthetic { .. } => 0,
            Self::Window { capture, .. } => capture.status().frames_published,
        }
    }

    /// Publish synthetic `seed` at `size` (reconfiguring first if needed).
    fn publish(&self, seed: u8, size: (u32, u32)) {
        let Self::Synthetic { device, producer } = self else {
            panic!("synthetic only")
        };
        let mut producer = producer.lock().unwrap();
        if (producer.params().width, producer.params().height) != size {
            assert!(matches!(
                producer.reconfigure(params(size)).unwrap(),
                jackstay::acquisition::arena::ReconfigurationStatus::Ready { .. }
            ));
        }
        let texture = device
            .upload(size.0, size.1, PixelFormat::Bgra8Unorm, &pattern(size.0, size.1, seed))
            .unwrap();
        producer.publish(&D3d11CapturedFrame::new(texture), 0).unwrap();
    }

    /// Paint the window `rgb` at `size` and wait for a capture at that size.
    fn paint(&self, rgb: [u8; 3], size: (u32, u32)) {
        let Self::Window { window, capture } = self else {
            panic!("window only")
        };
        let before = capture.status().frames_published;
        window.resize(size.0, size.1);
        window.set_color(rgb);
        let deadline = Instant::now() + TIMEOUT;
        let mut status = capture.status();
        while status.size != size || status.frames_published <= before {
            assert!(Instant::now() < deadline, "no capture at {size:?}: {status:?}");
            status = capture.wait_for_change(status.revision, Duration::from_millis(100));
        }
    }

    /// Frames of this source's next step, as the consumer should see them.
    fn step(&self, index: u8, size: (u32, u32)) -> Check {
        match self {
            Self::Synthetic { .. } => {
                self.publish(index, size);
                Check::Pattern(index)
            }
            Self::Window { .. } => {
                let rgb = [[255, 0, 0], [0, 0, 255], [0, 255, 0], [255, 255, 0]][usize::from(index) % 4];
                self.paint(rgb, size);
                Check::Uniform(bgra(rgb))
            }
        }
    }

    fn sizes(&self) -> [(u32, u32); 2] {
        match self {
            Self::Synthetic { .. } => [(64, 48), (96, 40)],
            Self::Window { .. } => [(160, 120), (200, 90)],
        }
    }
}

/// Parent produces; a child process consumes, holds a frame across resize,
/// then is killed with GPU work pending on a frame.
fn consumer_process_scenario(source: Source, label: &str) {
    let control = Listener::bind(&format!("{label}-control"));
    let setup_name = unique(&format!("jsd3d-{label}"));
    let listener = serve(&setup_name, source.producer());
    let mut child = spawn("d3d11_consumer_child", &setup_name, &control, label);
    let mut link = control.accept();
    let Report::Ready { adapter } = link.recv() else {
        panic!("child not ready")
    };
    assert_eq!(AdapterLuid(adapter), source.device().luid());
    let [small, large] = source.sizes();

    // A frame held across resize and ring wrap.
    let first = source.step(1, small);
    link.send(&Order::Expect {
        size: small,
        check: first,
        hold: true,
    });
    let Report::Seen { .. } = link.recv() else { panic!() };
    let resized = source.step(2, large);
    link.send(&Order::Expect {
        size: large,
        check: resized,
        hold: false,
    });
    let Report::Seen { .. } = link.recv() else { panic!() };
    for index in 3..12 {
        let check = source.step(index % 2 + 2, large);
        link.send(&Order::Expect {
            size: large,
            check,
            hold: false,
        });
        let Report::Seen { .. } = link.recv() else { panic!() };
    }
    link.send(&Order::CheckHeld);
    let Report::HeldOk { count: 1 } = link.recv() else {
        panic!("held frame lost")
    };
    link.send(&Order::ReleaseHeld);
    let Report::Released = link.recv() else { panic!() };

    // Kill the consumer with submitted GPU work gated on a fence we hold.
    let device = source.device();
    let gate_device = D3d11Device::new(AdapterSelection::Luid(device.luid())).unwrap();
    let gate = gate_device.create_shared_fence().unwrap();
    let pending = source.step(3, large);
    link.send(&Order::PendingGpu {
        size: large,
        check: pending,
    });
    link.send_objects(&child, &[gate.export_handle().unwrap().into_owned()]);
    let Report::Pending {
        slot,
        fence_value,
        release_value,
    } = link.recv()
    else {
        panic!()
    };
    let release_copy = SharedFenceHandle::from_owned(link.take_objects(&child, 1).pop().unwrap());
    let producer = source.producer();
    // An in-process observer of the pool, to check the pending claim's slot.
    let observer = producer.lock().unwrap().attach(1).unwrap();
    let observer_device = D3d11Device::new(AdapterSelection::Luid(device.luid())).unwrap();
    let slot_texture = observer_device.open_texture(&observer.surface_handles[slot as usize]).unwrap();
    let release = observer_device.open_fence(&release_copy).unwrap();
    assert!(
        release.completed_value() < release_value,
        "[{label}] the gated release completed early"
    );
    let ready = observer_device.open_fence(&observer.sync_handle).unwrap();
    drop(observer.into_consumer().unwrap());
    // While the consumer lives, its submitted GPU work keeps the lease:
    // publication continues without reusing that slot or its credit.
    for index in 0..30 {
        match &source {
            Source::Synthetic { .. } => source.publish(4 + index % 2, large),
            Source::Window { .. } => {
                source.paint([[0, 255, 255], [255, 0, 255]][usize::from(index % 2)], large);
            }
        }
        producer.lock().unwrap().poll_cleanup().unwrap();
    }
    let slot_pixels = observer_device
        .read_pixels(&slot_texture, &[(&ready, fence_value)], TIMEOUT)
        .unwrap();
    assert!(
        pending.matches(large.0, large.1, &slot_pixels),
        "[{label}] a slot with pending GPU work was reused"
    );
    // 6 resources - 2 retained - 1 producer reserve: 3 holdable frames.
    assert!(
        producer.lock().unwrap().attach(3).is_err(),
        "[{label}] pending work returned its credit"
    );
    child.kill().unwrap();
    let dead = child.wait().unwrap();
    let killed = Instant::now();
    // The gate is still closed. The dead process's device is destroyed, and
    // its shared release fence then reads u64::MAX: no work of it can run.
    let deadline = Instant::now() + TIMEOUT;
    let outcome = loop {
        let mut producer = producer.lock().unwrap();
        producer.poll_cleanup().unwrap();
        if let Some(failure) = producer.cleanup_failures().first() {
            break Err(failure.reason.clone());
        }
        if producer.attach(3).is_ok() {
            break Ok(killed.elapsed());
        }
        drop(producer);
        assert!(Instant::now() < deadline, "[{label}] neither drained nor quarantined");
        std::thread::sleep(Duration::from_millis(10));
    };
    eprintln!(
        "[{label}] consumer killed ({dead:?}) with release {release_value} pending behind a closed gate: its release fence \
         reads {:#x} (abandoned {}); {outcome:?} (Ok = its reservation returned after that long)",
        release.completed_value(),
        release.is_abandoned()
    );
    assert!(
        release.is_abandoned(),
        "[{label}] expected the dead consumer's release fence to be abandoned"
    );
    assert!(outcome.is_ok(), "[{label}] expected the dead device's release fence to complete");
    gate.signal_gpu(&gate_device, 1).unwrap();
    listener.cancel();
    let _ = &source;
}

/// Parent consumes; a child process produces and is killed with a staged
/// copy still queued behind a gate on its GPU context.
fn producer_process_scenario(label: &str) {
    let control = Listener::bind(&format!("{label}-control"));
    let setup_name = unique(&format!("jsd3d-{label}"));
    let mut child = spawn("d3d11_producer_child", &setup_name, &control, label);
    let mut link = control.accept();
    let Report::Ready { .. } = link.recv() else {
        panic!("child not ready")
    };
    let mut consumer = Consumer::connect(&setup_name, 2);
    let window = label.contains("window");
    let sizes = if window { [(160, 120), (200, 90)] } else { [(64, 48), (96, 40)] };
    let checks = [Check::Pattern(5), Check::Pattern(6), Check::Pattern(7)];
    let colors = [[255, 0, 0], [0, 0, 255], [0, 255, 0]];
    let order = |index: usize, size: (u32, u32)| {
        if window {
            (Order::Paint { rgb: colors[index], size }, Check::Uniform(bgra(colors[index])))
        } else {
            (
                Order::Publish {
                    seed: 5 + index as u8,
                    size,
                },
                checks[index],
            )
        }
    };
    let (publish, first) = order(0, sizes[0]);
    link.send(&publish);
    let Report::Published { .. } = link.recv() else { panic!() };
    let held = consumer.expect(sizes[0], first);
    let (publish, second) = order(1, sizes[1]);
    link.send(&publish);
    let Report::Published { .. } = link.recv() else { panic!() };
    let resized = consumer.expect(sizes[1], second);
    assert!(
        first.matches(sizes[0].0, sizes[0].1, &consumer.read(&held)),
        "held frame changed across resize"
    );
    consumer.release_gpu(resized);

    // Gate the producer's next copy, then kill it before opening the gate.
    let gate_device = D3d11Device::new(AdapterSelection::Luid(consumer.device.luid())).unwrap();
    let gate = gate_device.create_shared_fence().unwrap();
    link.send(&Order::GatedPublish { seed: 7, rgb: colors[2] });
    link.send_objects(&child, &[gate.export_handle().unwrap().into_owned()]);
    let Report::GatedPublished { .. } = link.recv() else { panic!() };
    let gated = loop {
        match consumer.consumer.acquire_latest(consumer.after).unwrap() {
            AcquireOutcome::Frame(frame) => break frame,
            AcquireOutcome::Reconfiguration => {
                consumer.setup.install_configuration(&mut consumer.consumer).unwrap();
            }
            _ => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    let native = gated.native_resources::<SharedTextureHandle, SharedFenceHandle>().unwrap();
    let value = gated.descriptor().fence_value;
    let ready = consumer.device.open_fence(native.sync_handle).unwrap();
    assert!(ready.completed_value() < value, "the gated copy completed early");
    // A separate device GPU-waits on the gated value, so the main one stays usable.
    let waiter = D3d11Device::new(AdapterSelection::Luid(consumer.device.luid())).unwrap();
    let waiter_texture = waiter.open_texture(native.surface).unwrap();
    let waiter_ready = waiter.open_fence(native.sync_handle).unwrap();
    let waiting = waiter.submit_readback(&waiter_texture, &[(&waiter_ready, value)], None).unwrap();
    child.kill().unwrap();
    let status = child.wait().unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while consumer.setup.is_alive() {
        assert!(Instant::now() < deadline, "setup liveness missed producer death");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The completed frame outlives its producer.
    assert!(
        first.matches(sizes[0].0, sizes[0].1, &consumer.read(&held)),
        "held frame lost with the producer"
    );
    // With the gate still closed, the dead producer's fence is abandoned: it
    // reads u64::MAX, which also satisfies GPU waits, so a consumer never
    // hangs on a dead producer. The frame's copy never ran; discard it.
    let abandoned = ready.wait_cpu(value, Duration::from_secs(3));
    let completed = ready.completed_value();
    let finished = waiting.finish(&waiter, Duration::from_secs(3));
    eprintln!(
        "[{label}] producer killed ({status:?}) with a copy gated behind a closed fence: its fence {} (completed {completed:#x}, \
         abandoned {}); a consumer GPU wait on the frame {}",
        if abandoned {
            "satisfied the frame's value"
        } else {
            "never reached the frame's value"
        },
        ready.is_abandoned(),
        match &finished {
            Ok(_) => "completed".to_owned(),
            Err(error) => format!("did not complete: {error}"),
        }
    );
    assert!(ready.is_abandoned() && finished.is_ok());
    gate.signal_gpu(&gate_device, 1).unwrap();
    drop(gated);
    drop(held);
}

#[test]
#[ignore = "subprocess helper for the D3D11 process scenarios"]
fn d3d11_producer_child() {
    let (Ok(setup), Ok(control), Ok(label)) = (
        std::env::var("JACKSTAY_D3D11_SETUP"),
        std::env::var("JACKSTAY_D3D11_CONTROL"),
        std::env::var("JACKSTAY_D3D11_SOURCE"),
    ) else {
        return;
    };
    let mut link = Link::connect(&control);
    let source = if label.contains("window") {
        Source::window()
    } else {
        Source::synthetic(AdapterSelection::Default)
    };
    let listener = serve(&setup, source.producer());
    link.send(&Report::Ready {
        adapter: source.device().luid().0,
    });
    loop {
        match link.recv::<Order>() {
            Order::Publish { seed, size } => {
                source.publish(seed, size);
                link.send(&Report::Published { frames: 0 });
            }
            Order::Paint { rgb, size } => {
                source.paint(rgb, size);
                link.send(&Report::Published {
                    frames: source.published(),
                });
            }
            Order::GatedPublish { seed, rgb } => {
                let gate = SharedFenceHandle::from_owned(link.recv_objects(1).pop().unwrap());
                let device = source.device();
                device.open_fence(&gate).unwrap().gpu_wait(&device, 1).unwrap();
                match &source {
                    Source::Synthetic { .. } => {
                        let producer = source.producer();
                        let size = {
                            let producer = producer.lock().unwrap();
                            (producer.params().width, producer.params().height)
                        };
                        source.publish(seed, size);
                    }
                    Source::Window { window, capture } => {
                        let before = capture.status().frames_published;
                        window.set_color(rgb);
                        let deadline = Instant::now() + TIMEOUT;
                        while capture.status().frames_published <= before {
                            assert!(Instant::now() < deadline);
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                }
                link.send(&Report::GatedPublished {
                    frames: source.published(),
                });
                let _keep = listener;
                loop {
                    std::thread::park();
                }
            }
            other => panic!("producer cannot {other:?}"),
        }
    }
}

#[test]
fn a_synthetic_producer_serves_a_consumer_process_through_resize_hold_and_its_death() {
    // Every adapter this session enumerates, so the Remote Display Adapter of
    // an RDP session is covered as well as the GPU.
    for adapter in jackstay::native::windows::adapters().unwrap() {
        eprintln!("adapter {} {:?}", adapter.luid, adapter.description);
        let label = format!("synthetic-consumer-{:x}", adapter.luid.0);
        consumer_process_scenario(Source::synthetic(AdapterSelection::Luid(adapter.luid)), &label);
    }
}

#[test]
fn a_consumer_process_survives_a_synthetic_producer_killed_with_gpu_work_pending() {
    producer_process_scenario("synthetic-producer");
}

#[test]
#[ignore = "needs an interactive desktop; creates and captures its own window"]
fn a_window_capture_serves_a_consumer_process_through_resize_hold_and_its_death() {
    consumer_process_scenario(Source::window(), "window-consumer");
}

#[test]
#[ignore = "needs an interactive desktop; the child creates and captures its own window"]
fn a_consumer_process_survives_a_window_capture_producer_killed_with_gpu_work_pending() {
    producer_process_scenario("window-producer");
}
