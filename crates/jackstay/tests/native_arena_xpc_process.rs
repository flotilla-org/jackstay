#![cfg(all(target_os = "macos", feature = "backend-macos"))]

use std::{
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::net::{UnixListener, UnixStream},
    },
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

use jackstay::{
    acquisition::arena::{AcquireOutcome, ArenaConfig, Cancellation, ConfigurationInstall, WaitInterest, WaitOutcome},
    model::{ClockDomain, ColorSpace, PixelFormat},
    native::{
        NativeFrameBackend, NativeStreamParams,
        arena::NativeArenaProducer,
        macos::{
            ConsumerFence, IoSurface, MacosCapturedFrame, MacosFrameBackend, MetalContext, SampleCompletion, SharedEventHandle,
            xpc::arena::{XpcArenaClient, XpcArenaServer},
        },
    },
};
use serde_json::{Value, json};

#[path = "support/child.rs"]
mod child;

const TOKEN: &str = "isolated-native-acquisition-test";

fn send(stream: &mut UnixStream, value: &Value) {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    stream.write_all(&bytes).unwrap();
}
fn receive(stream: &mut UnixStream) -> Value {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        if byte[0] == b'\n' {
            break;
        }
        bytes.push(byte[0]);
        assert!(bytes.len() < 65536, "oversized test control message");
    }
    serde_json::from_slice(&bytes).unwrap()
}
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

struct Service {
    directory: Option<tempfile::TempDir>,
    name: String,
    target: String,
    control: Option<UnixStream>,
    stopped: bool,
}
impl Service {
    fn start() -> Self {
        let directory = tempfile::Builder::new().prefix("jsxpc-").tempdir_in("/tmp").unwrap();
        let name = format!(
            "work.flotilla.jackstay.test.{}.{}",
            std::process::id(),
            directory.path().file_name().unwrap().to_str().unwrap()
        );
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let mut service = Self {
            target: format!("{domain}/{name}"),
            name,
            directory: Some(directory),
            control: None,
            stopped: false,
        };
        let directory = service.directory.as_ref().unwrap().path();
        let socket = directory.join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let plist = directory.join("job.plist");
        let executable = std::env::current_exe().unwrap();
        std::fs::write(&plist, format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>{name}</string>
<key>ProgramArguments</key><array><string>{executable}</string><string>--ignored</string><string>--exact</string><string>launchd_producer_child</string><string>--nocapture</string></array>
<key>RunAtLoad</key><true/>
<key>MachServices</key><dict><key>{name}</key><true/></dict>
<key>EnvironmentVariables</key><dict><key>JACKSTAY_TEST_SERVICE</key><string>{name}</string><key>JACKSTAY_TEST_CONTROL</key><string>{socket}</string></dict>
<key>StandardOutPath</key><string>{stdout}</string><key>StandardErrorPath</key><string>{stderr}</string>
</dict></plist>"#,
            name=xml(&service.name), executable=xml(executable.to_str().unwrap()), socket=xml(socket.to_str().unwrap()),
            stdout=xml(directory.join("stdout.log").to_str().unwrap()), stderr=xml(directory.join("stderr.log").to_str().unwrap()))).unwrap();
        let result = Command::new("launchctl").args(["bootstrap", &domain]).arg(&plist).output().unwrap();
        assert!(
            result.status.success(),
            "launchd bootstrap: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let mut descriptor = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, 10000) };
        assert!(
            ready > 0 && descriptor.revents & libc::POLLIN != 0,
            "producer did not start: {}",
            std::fs::read_to_string(directory.join("stderr.log")).unwrap_or_default()
        );
        let (mut control, _) = listener.accept().unwrap();
        control.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        control.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
        let ready = receive(&mut control);
        assert_ne!(ready["pid"].as_u64().unwrap(), u64::from(std::process::id()));
        service.control = Some(control);
        service
    }
    fn request(&mut self, value: Value) -> Value {
        let control = self.control.as_mut().unwrap();
        send(control, &value);
        receive(control)
    }
    fn stop(&mut self) {
        let reply = self.request(json!({"command":"quit"}));
        assert_eq!(reply["ok"], true);
        let result = Command::new("launchctl").args(["bootout", &self.target]).output().unwrap();
        assert!(
            result.status.success(),
            "launchd bootout: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        self.stopped = true;
        let present = Command::new("launchctl").args(["print", &self.target]).output().unwrap();
        assert!(!present.status.success(), "test service still registered after bootout");
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = Command::new("launchctl").args(["bootout", &self.target]).output();
        }
        if std::thread::panicking() {
            if let Some(directory) = self.directory.take() {
                eprintln!("XPC test diagnostics: {}", directory.keep().display());
            }
        }
    }
}

#[test]
#[ignore = "requires a GUI/Metal device and JACKSTAY_VIEWER_TEST_BINARY; opens two temporary SDL windows"]
fn reference_viewer_completes_bgra_and_rgba_frames_and_returns_admission() {
    let viewer = std::fs::canonicalize(std::env::var("JACKSTAY_VIEWER_TEST_BINARY").expect("build the native SDL viewer first")).unwrap();
    let mut service = Service::start();
    let directory = service.directory.as_ref().unwrap().path().to_owned();
    for cycle in 0..2 {
        if cycle == 1 {
            assert_eq!(service.request(json!({"command":"resize"}))["ok"], true);
        }
        let stdout = directory.join(format!("viewer-{cycle}.stdout.log"));
        let stderr = directory.join(format!("viewer-{cycle}.stderr.log"));
        let mut viewer = child::KillOnDrop(
            Command::new(&viewer)
                .args([
                    "--native",
                    "--mach-service",
                    &service.name,
                    "--token",
                    TOKEN,
                    "--frames",
                    "8",
                    "--hold-ms",
                    if cycle == 0 { "0" } else { "250" },
                ])
                .env_remove("SDL_VIDEODRIVER")
                .stdout(std::fs::File::create(&stdout).unwrap())
                .stderr(std::fs::File::create(&stderr).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = viewer.try_wait().unwrap() {
                assert!(status.success(), "viewer failed: {}", std::fs::read_to_string(&stderr).unwrap());
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "viewer stalled: {}",
                std::fs::read_to_string(&stderr).unwrap()
            );
            service.request(json!({"command":"publish", "seed":51 + cycle}));
            std::thread::sleep(Duration::from_millis(20));
        }
        let output = std::fs::read_to_string(&stdout).unwrap();
        assert!(
            output.lines().any(|line| line == "presented_frames=8"),
            "viewer did not confirm eight GPU completions: {output}"
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let stats = service.request(json!({"command":"stats"}));
            assert!(
                stats["failures"].as_array().unwrap().is_empty(),
                "clean viewer exit left recovery failures: {stats}"
            );
            if stats["admission_available"] == true {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "viewer exit did not return its reservation: {stats}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    service.stop();
}

#[test]
#[ignore = "requires a launchd GUI bootstrap session and a Metal device; creates and removes an isolated test service"]
fn named_xpc_transfers_frames_replacements_and_gpu_release_across_processes() {
    let mut service = Service::start();
    // SAFETY: this test registered the unique name for its own conforming
    // producer executable in the current user's launchd namespace.
    let mut client = unsafe { XpcArenaClient::connect_named(&service.name) }.unwrap();
    client.authorize(TOKEN).unwrap();
    let mut consumer = client.attach(2).unwrap();
    assert!(
        service.request(json!({"command":"publish", "seed":51}))["cursor"]
            .as_u64()
            .is_some()
    );
    let AcquireOutcome::Frame(old) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing cross-process frame")
    };
    service.request(json!({"command":"resize"}));
    assert_eq!(
        client.install_configuration(&mut consumer).unwrap(),
        Some(ConfigurationInstall::Installed)
    );
    service.request(json!({"command":"publish", "seed":86}));
    let AcquireOutcome::Frame(new) = consumer.acquire_latest(old.cursor()).unwrap() else {
        panic!("missing replacement")
    };
    let metal = MetalContext::new().unwrap();
    let release = Arc::new(ConsumerFence::new(&metal).unwrap());
    let binding = client.register_release_timeline(&consumer, release.clone()).unwrap();
    {
        let native = old.native_resources::<IoSurface, SharedEventHandle>().unwrap();
        let readiness = ConsumerFence::from_handle(&metal, native.sync_handle).unwrap();
        assert_eq!(
            readiness
                .sample_offscreen_with_completion(
                    &metal,
                    native.surface,
                    old.descriptor().fence_value,
                    (16, 16),
                    SampleCompletion {
                        release: (&release, 1),
                        before_sample: None,
                        submitted: None
                    }
                )
                .unwrap(),
            vec![51; 16 * 16 * 4]
        );
    }
    let before = consumer.events();
    old.defer_release(&binding, 1).unwrap();
    assert!(
        matches!(consumer.wait(before, WaitInterest::CAPACITY, &Cancellation::new().unwrap(), Some(Duration::from_secs(5))).unwrap(),
        WaitOutcome::Changed(events) if events.capacity_epoch > before.capacity_epoch)
    );
    {
        let native = new.native_resources::<IoSurface, SharedEventHandle>().unwrap();
        let readiness = ConsumerFence::from_handle(&metal, native.sync_handle).unwrap();
        assert_eq!(
            readiness
                .sample_offscreen(&metal, native.surface, new.descriptor().fence_value, 17, 19)
                .unwrap(),
            vec![86; 17 * 19 * 4]
        );
    }
    drop(new);
    drop(consumer);
    drop(client);
    service.stop();
}

#[test]
#[ignore = "requires a launchd GUI bootstrap session and a Metal device; kills an isolated consumer with submitted GPU work"]
fn a_crashed_xpc_consumer_keeps_its_submitted_gpu_lease_until_completion_or_visible_quarantine() {
    let mut service = Service::start();
    service.request(json!({"command":"publish", "seed":73}));
    let directory = service.directory.as_ref().unwrap().path();
    let socket = directory.join("consumer.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut child = child::KillOnDrop(
        Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "submitted_gpu_consumer_child", "--nocapture"])
            .env("JACKSTAY_TEST_SERVICE", &service.name)
            .env("JACKSTAY_TEST_CONSUMER_CONTROL", &socket)
            .stdout(std::fs::File::create(directory.join("consumer.stdout.log")).unwrap())
            .stderr(std::fs::File::create(directory.join("consumer.stderr.log")).unwrap())
            .spawn()
            .unwrap(),
    );
    let mut descriptor = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert!(unsafe { libc::poll(&mut descriptor, 1, 10000) } > 0, "consumer did not connect");
    let (mut control, _) = listener.accept().unwrap();
    control.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let ready = receive(&mut control);
    assert_eq!(ready["pid"].as_u64().unwrap(), u64::from(child.id()));
    assert_eq!(ready["submitted"], true);
    let mut last = 0;
    for _ in 0..100 {
        let reply = service.request(json!({"command":"publish", "seed":91}));
        if let Some(cursor) = reply["cursor"].as_u64() {
            last = cursor;
        }
    }
    assert!(last > 4, "publication ring did not wrap around the deferred frame");
    assert_eq!(service.request(json!({"command":"stats"}))["admission_available"], false);
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    // Let the producer's actual drain deadline expire, with the GPU gate
    // still closed. Process exit and timeout must not manufacture completion.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let stalled = loop {
        let stats = service.request(json!({"command":"stats"}));
        assert_eq!(
            stats["admission_available"], false,
            "process exit released unfinished GPU storage: {stats}"
        );
        if !stats["failures"].as_array().unwrap().is_empty() {
            break stats;
        }
        assert!(std::time::Instant::now() < deadline, "stalled cleanup was not reported");
        std::thread::sleep(Duration::from_millis(10));
    };
    eprintln!("crashed consumer before GPU gate: {stalled}");
    service.request(json!({"command":"finish_gpu"}));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let outcome = loop {
        let stats = service.request(json!({"command":"stats"}));
        if stats["admission_available"] == true || std::time::Instant::now() >= deadline {
            break stats;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Either actual completion returns admission, or unresolved GPU use stays
    // charged with an explicit recovery failure. Neither result assumes that
    // killing the process cancels or completes submitted Metal commands.
    if outcome["admission_available"] != true {
        assert!(!outcome["failures"].as_array().unwrap().is_empty());
    }
    eprintln!("crashed consumer after GPU gate: {outcome}");
    service.stop();
}

#[test]
#[ignore = "subprocess helper for the submitted GPU crash test"]
fn submitted_gpu_consumer_child() {
    let name = std::env::var("JACKSTAY_TEST_SERVICE").unwrap();
    // SAFETY: the supervising test registered this unique, conforming producer.
    let mut client = unsafe { XpcArenaClient::connect_named(&name) }.unwrap();
    client.authorize(TOKEN).unwrap();
    let consumer = client.attach(1).unwrap();
    let metal = MetalContext::new().unwrap();
    let release = Arc::new(ConsumerFence::new(&metal).unwrap());
    let binding = client.register_release_timeline(&consumer, release.clone()).unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing crash-test frame")
    };
    let native = frame.native_resources::<IoSurface, SharedEventHandle>().unwrap();
    let readiness = ConsumerFence::from_handle(&metal, native.sync_handle).unwrap();
    let surface = native.surface.clone();
    let ready_value = frame.descriptor().fence_value;
    let submitted = ConsumerFence::new(&metal).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let surface = surface;
            let _ = readiness.sample_offscreen_with_completion(
                &metal,
                &surface,
                ready_value,
                (16, 16),
                SampleCompletion {
                    release: (&release, 1),
                    before_sample: Some((&readiness, u64::MAX)),
                    submitted: Some((&submitted, 1)),
                },
            );
        });
        assert!(submitted.wait(1, 5000), "consumer commands were not submitted");
        frame.defer_release(&binding, 1).unwrap();
        assert_eq!(release.signaled_value(), 0);
        let mut control = UnixStream::connect(std::env::var("JACKSTAY_TEST_CONSUMER_CONTROL").unwrap()).unwrap();
        send(&mut control, &json!({"pid":std::process::id(), "submitted":true}));
        loop {
            std::thread::park();
        }
    });
}

#[test]
#[ignore = "launchd subprocess helper invoked by named_xpc_transfers_frames_replacements_and_gpu_release_across_processes"]
fn launchd_producer_child() {
    let name = std::env::var("JACKSTAY_TEST_SERVICE").unwrap();
    let mut params = NativeStreamParams {
        width: 16,
        height: 16,
        pixel_format: PixelFormat::Bgra8Unorm,
        color_space: ColorSpace::Srgb,
        clock_domain: ClockDomain::HostTime,
        modifier: 0,
    };
    let mut backend = MacosFrameBackend::new().unwrap();
    let pool = backend.allocate_surface_pool(&params, 6).unwrap();
    let fence = backend.create_fence().unwrap();
    let readiness = ConsumerFence::from_handle(backend.metal(), &backend.export_sync_handle(&fence).unwrap()).unwrap();
    let producer = Arc::new(Mutex::new(
        NativeArenaProducer::from_allocated_parts(
            backend,
            pool,
            fence,
            params.clone(),
            ArenaConfig {
                resource_capacity: 6,
                retained_history: 2,
                producer_reserve: 1,
                payload_capacity: 0,
                memory_budget: 1024 * 1024,
                max_incarnations: 1,
                drain_timeout: Duration::from_millis(250),
            },
        )
        .unwrap(),
    ));
    let _server = XpcArenaServer::start_named(&name, Some(TOKEN.to_owned()), producer.clone()).unwrap();
    let mut control = UnixStream::connect(std::env::var("JACKSTAY_TEST_CONTROL").unwrap()).unwrap();
    control.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    send(&mut control, &json!({"pid":std::process::id()}));
    let mut timestamp = 0;
    let mut last_ready = 0;
    let mut finished = false;
    loop {
        let request = receive(&mut control);
        match request["command"].as_str().unwrap() {
            "publish" => {
                assert!(!finished, "no publication is allowed after the terminal test GPU gate");
                let source = IoSurface::allocate(params.width, params.height, params.pixel_format).unwrap();
                source
                    .write_pixels(&vec![
                        request["seed"].as_u64().unwrap() as u8;
                        (params.width * params.height * 4) as usize
                    ])
                    .unwrap();
                timestamp += 1;
                let outcome = producer
                    .lock()
                    .unwrap()
                    .publish(&MacosCapturedFrame { surface: source }, timestamp)
                    .unwrap();
                match outcome {
                    jackstay::acquisition::arena::PublishOutcome::Published { cursor } => {
                        last_ready = cursor;
                        send(&mut control, &json!({"cursor":cursor}));
                    }
                    jackstay::acquisition::arena::PublishOutcome::Dropped => send(&mut control, &json!({"dropped":true})),
                }
            }
            "resize" => {
                params.width = 17;
                params.height = 19;
                params.pixel_format = PixelFormat::Rgba8Unorm;
                producer.lock().unwrap().reconfigure(params.clone()).unwrap();
                send(&mut control, &json!({"ok":true}));
            }
            "quit" => {
                send(&mut control, &json!({"ok":true}));
                break;
            }
            "stats" => {
                let mut producer = producer.lock().unwrap();
                let released = producer.poll_cleanup().unwrap();
                let failures: Vec<_> = producer.cleanup_failures().into_iter().map(|failure| failure.reason).collect();
                let admission_available = producer.attach(1).is_ok();
                send(
                    &mut control,
                    &json!({"released":released, "failures":failures, "admission_available":admission_available}),
                );
            }
            "finish_gpu" => {
                assert!(readiness.wait(last_ready, 5000), "producer writes did not finish");
                // All producer writes have actually finished and no further
                // publication is permitted. Advancing to this terminal value
                // opens the consumer's diagnostic GPU gate without asserting
                // completion of any unexecuted producer work.
                finished = true;
                readiness.signal_cpu(u64::MAX);
                send(&mut control, &json!({"ok":true}));
            }
            command => panic!("unknown test command: {command}"),
        }
    }
}
