#![cfg(all(windows, feature = "backend-windows"))]
//! The D3D11 arena backend in one process: pool preflight, fence sharing on
//! every adapter, GPU-ordered import on a separate device, reconfiguration,
//! deferred GPU release, producer shutdown, and setup over a pipe pair
//! (including adapter refusal).

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use jackstay::{
    acquisition::arena::{
        AcquireOutcome, ArenaConfig, ArenaError, Cancellation, ConfigurationInstall, PublishOutcome, ReconfigurationStatus, WaitInterest,
        WaitOutcome,
    },
    model::{ClockDomain, ColorSpace, PayloadKind, PixelFormat},
    native::{
        NativeFrameBackend, NativeStreamParams,
        arena::{ArenaNativeBackend, NativeArenaProducer},
        windows::{
            AdapterSelection, D3d11CapturedFrame, D3d11Device, D3d11Fence, D3d11FrameBackend, SharedFenceHandle, SharedTextureHandle,
            adapters,
            setup::{D3d11SetupClient, Refusal, SetupError, serve_d3d11},
        },
    },
};

const TIMEOUT: Duration = Duration::from_secs(5);

fn device() -> Arc<D3d11Device> {
    Arc::new(D3d11Device::new(AdapterSelection::Default).unwrap_or_else(|_| D3d11Device::new(AdapterSelection::Warp).unwrap()))
}

/// Another device on the same adapter, as a consumer process would create.
fn peer(device: &D3d11Device) -> D3d11Device {
    D3d11Device::new(AdapterSelection::Luid(device.luid())).unwrap()
}

fn params(width: u32, height: u32) -> NativeStreamParams {
    NativeStreamParams {
        width,
        height,
        pixel_format: PixelFormat::Bgra8Unorm,
        color_space: ColorSpace::Srgb,
        clock_domain: ClockDomain::HostTime,
        modifier: 0,
    }
}

fn config(max_incarnations: u32) -> ArenaConfig {
    ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 0,
        memory_budget: 256 * 1024 * 1024,
        max_incarnations,
        drain_timeout: TIMEOUT,
    }
}

fn pattern(width: u32, height: u32, seed: u8) -> Vec<u8> {
    (0..width * height * 4)
        .map(|index| (index as u8).wrapping_mul(7).wrapping_add(seed))
        .collect()
}

fn frame(device: &D3d11Device, width: u32, height: u32, seed: u8) -> D3d11CapturedFrame {
    D3d11CapturedFrame::new(
        device
            .upload(width, height, PixelFormat::Bgra8Unorm, &pattern(width, height, seed))
            .unwrap(),
    )
}

fn producer(device: &Arc<D3d11Device>, max_incarnations: u32) -> NativeArenaProducer<D3d11FrameBackend> {
    NativeArenaProducer::new(D3d11FrameBackend::new(Arc::clone(device)), params(16, 16), config(max_incarnations)).unwrap()
}

/// Import a held frame's surface and readiness on `consumer` and read it
/// back behind a GPU wait (no CPU wait on the producer fence).
fn sample(consumer: &D3d11Device, frame: &jackstay::acquisition::arena::FrameLease) -> Vec<u8> {
    let native = frame.native_resources::<SharedTextureHandle, SharedFenceHandle>().unwrap();
    let texture = consumer.open_texture(native.surface).unwrap();
    let ready = consumer.open_fence(native.sync_handle).unwrap();
    consumer
        .read_pixels(&texture, &[(&ready, frame.descriptor().fence_value)], TIMEOUT)
        .unwrap()
}

/// Opens a gate on drop so a failing assertion cannot leave GPU work queued
/// behind it forever.
struct Gate {
    device: D3d11Device,
    fence: D3d11Fence,
}

impl Gate {
    /// A shared fence at 0, signalled from its own device (D3D11 fences have
    /// no CPU signal).
    fn new(on: &D3d11Device) -> (Self, D3d11Fence) {
        let device = peer(on);
        let fence = device.create_shared_fence().unwrap();
        let opened = on.open_fence(&fence.export_handle().unwrap()).unwrap();
        (Self { device, fence }, opened)
    }

    fn open(&self) {
        self.fence.signal_gpu(&self.device, 1).unwrap();
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        self.open();
    }
}

#[test]
fn every_adapter_shares_fences_and_textures_across_devices() {
    let adapters = adapters().unwrap();
    assert!(!adapters.is_empty());
    for adapter in adapters {
        let selection = AdapterSelection::Luid(adapter.luid);
        let result = (|| -> jackstay::Result<()> {
            let producer = D3d11Device::new(selection)?;
            producer.probe_shared_fences()?;
            let consumer = D3d11Device::new(selection)?;
            // Producer copy + signal; consumer GPU-waits on its own device.
            let fence = producer.create_shared_fence()?;
            let pixels = pattern(8, 4, 3);
            let texture = producer.upload(8, 4, PixelFormat::Bgra8Unorm, &pixels)?;
            let mut backend = D3d11FrameBackend::new(Arc::new(producer));
            let mut pool = backend.allocate_surface_pool(&params(8, 4), 1)?;
            let mut fence_owner = backend.create_fence()?;
            backend.stage_frame(&mut pool, 0, &D3d11CapturedFrame::new(texture))?;
            backend.signal_fence(&mut fence_owner, 1)?;
            let imported = consumer.open_texture(&backend.export_surface_handles(&pool)?.remove(0))?;
            let ready = consumer.open_fence(&backend.export_sync_handle(&fence_owner)?)?;
            assert_eq!(consumer.read_pixels(&imported, &[(&ready, 1)], TIMEOUT)?, pixels);
            assert!(fence.wait_cpu(0, TIMEOUT));
            Ok(())
        })();
        eprintln!(
            "adapter {} {:?} (vendor {:#x}, software {}): shared ID3D11Fence and texture import {}",
            adapter.luid,
            adapter.description,
            adapter.vendor_id,
            adapter.software,
            match &result {
                Ok(()) => "OK".to_owned(),
                Err(error) => format!("FAILED: {error}"),
            }
        );
        result.unwrap();
    }
}

#[test]
fn pool_preflight_bounds_the_charge_and_rejects_unsupported_layouts() {
    let mut backend = D3d11FrameBackend::new(device());
    for (width, height, slots) in [(1, 1, 1), (17, 19, 3), (1708, 1039, 6)] {
        let bound = backend.pool_allocation_upper_bound(&params(width, height), slots).unwrap();
        assert!(
            backend
                .allocate_surface_pool_bounded(&params(width, height), slots, bound - 1)
                .is_err()
        );
        let pool = backend.allocate_surface_pool_bounded(&params(width, height), slots, bound).unwrap();
        let charged = backend.allocated_pool_bytes(&pool).unwrap();
        assert!(charged >= u64::from(width) * u64::from(height) * 4 * u64::from(slots) && charged <= bound);
        assert_eq!(backend.export_surface_handles(&pool).unwrap().len(), slots as usize);
    }
    for (width, height, slots) in [(0, 1, 1), (1, 0, 1), (16385, 1, 1), (1, 1, 0), (1, 1, 16)] {
        assert!(backend.pool_allocation_upper_bound(&params(width, height), slots).is_err());
    }
    let mut unknown = params(1, 1);
    unknown.pixel_format = PixelFormat::Unknown;
    assert!(backend.pool_allocation_upper_bound(&unknown, 1).is_err());
}

#[test]
fn a_consumer_device_imports_published_frames_behind_gpu_waits_and_crops() {
    let device = device();
    let mut producer = producer(&device, 1);
    let consumer = producer.attach(1).unwrap().into_consumer().unwrap();
    let reader = peer(&device);
    assert_eq!(
        producer.publish(&frame(&device, 16, 16, 9), 1).unwrap(),
        PublishOutcome::Published { cursor: 1 }
    );
    let AcquireOutcome::Frame(first) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    assert_eq!(first.descriptor().payload_kind, PayloadKind::D3dSharedResource as u32);
    assert_eq!(sample(&reader, &first), pattern(16, 16, 9));
    drop(first);
    // A 16x16 region at (4, 2) of a larger texture, as a client-area crop.
    let whole = pattern(24, 20, 5);
    let texture = device.upload(24, 20, PixelFormat::Bgra8Unorm, &whole).unwrap();
    producer
        .publish(&D3d11CapturedFrame::cropped(texture, 4, 2, 16, 16).unwrap(), 2)
        .unwrap();
    let AcquireOutcome::Frame(cropped) = consumer.acquire_latest(1).unwrap() else {
        panic!("missing cropped frame")
    };
    let expected: Vec<u8> = (2..18).flat_map(|y| whole[(y * 24 + 4) * 4..(y * 24 + 20) * 4].to_vec()).collect();
    assert_eq!(sample(&reader, &cropped), expected);
    // The wrong size is a resize, never a silent partial copy.
    drop(cropped);
    assert!(D3d11CapturedFrame::cropped(device.upload(4, 4, PixelFormat::Bgra8Unorm, &pattern(4, 4, 0)).unwrap(), 2, 2, 4, 4).is_err());
    assert!(producer.publish(&frame(&device, 8, 8, 1), 3).is_err());
}

#[test]
fn reconfiguration_keeps_held_frames_readable_and_publishes_the_new_size() {
    let device = device();
    let mut producer = producer(&device, 1);
    let mut consumer = producer.attach(2).unwrap().into_consumer().unwrap();
    let reader = peer(&device);
    producer.publish(&frame(&device, 16, 16, 31), 1).unwrap();
    let AcquireOutcome::Frame(old) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing old frame")
    };
    let ReconfigurationStatus::Ready { generation } = producer.reconfigure(params(33, 17)).unwrap() else {
        panic!("replacement should fit")
    };
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Reconfiguration));
    let offer = producer.configuration_offer(consumer.incarnation()).unwrap().unwrap();
    assert_eq!(offer.install(&mut consumer).unwrap(), ConfigurationInstall::Installed);
    for timestamp in 2..40 {
        producer.publish(&frame(&device, 33, 17, 83), timestamp).unwrap();
    }
    let AcquireOutcome::Frame(new) = consumer.acquire_latest(old.cursor()).unwrap() else {
        panic!("missing replacement frame")
    };
    assert_eq!(new.descriptor().config_generation, generation);
    assert_ne!(new.descriptor().pool_id, old.descriptor().pool_id);
    assert_eq!((new.descriptor().width, new.descriptor().height), (33, 17));
    drop(consumer);
    producer.poll_cleanup().unwrap();
    assert_eq!(sample(&reader, &old), pattern(16, 16, 31));
    assert_eq!(sample(&reader, &new), pattern(33, 17, 83));
}

#[test]
fn deferred_release_returns_credit_only_after_the_consumer_gpu_work_completes() {
    let device = device();
    let mut producer = producer(&device, 1);
    let mut consumer = producer.attach(1).unwrap().into_consumer().unwrap();
    let reader = Arc::new(peer(&device));
    let release = Arc::new(reader.create_shared_fence().unwrap());
    // The producer observes its own import of the consumer's fence.
    let observed = Arc::new(device.open_fence(&release.export_handle().unwrap()).unwrap());
    let registration = producer
        .register_release_timeline(consumer.incarnation(), observed.clone())
        .unwrap();
    let binding = consumer.bind_release_timeline(&registration, release.clone()).unwrap();
    producer.publish(&frame(&device, 16, 16, 37), 1).unwrap();
    let AcquireOutcome::Frame(held) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    let cursor = held.cursor();
    let native = held.native_resources::<SharedTextureHandle, SharedFenceHandle>().unwrap();
    let texture = reader.open_texture(native.surface).unwrap();
    let ready = reader.open_fence(native.sync_handle).unwrap();
    let (gate, gate_on_reader) = Gate::new(&reader);
    // Consumer GPU work: wait for the producer, then the gate, copy, then
    // signal the release fence. Submitted, not complete.
    let pending = reader
        .submit_readback(
            &texture,
            &[(&ready, held.descriptor().fence_value), (&gate_on_reader, 1)],
            Some((&release, 1)),
        )
        .unwrap();
    held.defer_release(&binding, 1).unwrap();
    assert_eq!(observed.completed_value(), 0);
    assert_eq!(producer.poll_cleanup().unwrap(), 0);
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));
    for timestamp in 2..20 {
        producer.publish(&frame(&device, 16, 16, 91), timestamp).unwrap();
    }
    assert_eq!(producer.poll_cleanup().unwrap(), 0);
    let before = consumer.events();
    gate.open();
    assert!(
        matches!(
            consumer.wait(before, WaitInterest::CAPACITY, &Cancellation::new().unwrap(), Some(TIMEOUT)).unwrap(),
            WaitOutcome::Changed(events) if events.capacity_epoch == before.capacity_epoch + 1
        ),
        "GPU completion did not return credit"
    );
    assert!(release.wait_cpu(1, TIMEOUT));
    assert_eq!(pending.finish(&reader, TIMEOUT).unwrap(), pattern(16, 16, 37));
    assert_eq!(producer.poll_cleanup().unwrap(), 1);
    let AcquireOutcome::Frame(latest) = consumer.acquire_latest(cursor).unwrap() else {
        panic!("credit was not returned")
    };
    assert_eq!(sample(&reader, &latest), pattern(16, 16, 91));
}

#[test]
fn a_stopped_producer_drains_its_own_gpu_writes_before_shutdown() {
    let device = device();
    let (gate, gate_on_producer) = Gate::new(&device);
    // An actual GPU dependency ahead of staging on the producer's context.
    gate_on_producer.gpu_wait(&device, 1).unwrap();
    let mut producer = producer(&device, 1);
    let consumer = producer.attach(1).unwrap().into_consumer().unwrap();
    producer.publish(&frame(&device, 16, 16, 29), 1).unwrap();
    let AcquireOutcome::Frame(held) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    producer.stop();
    assert!(matches!(producer.publish(&frame(&device, 16, 16, 1), 2), Err(ArenaError::Closed)));
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Closed));
    drop(held);
    drop(consumer);
    assert!(!producer.poll_shutdown_ready().unwrap(), "shutdown ignored the gated copy");
    gate.open();
    let deadline = std::time::Instant::now() + TIMEOUT;
    while !producer.poll_shutdown_ready().unwrap() {
        assert!(std::time::Instant::now() < deadline, "producer writes did not drain");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn setup_over_a_pipe_transfers_pools_once_per_generation_and_registers_release() {
    let device = device();
    let producer = Arc::new(Mutex::new(producer(&device, 2)));
    let (server, client) = jackstay::local::pipe_pair().unwrap();
    client.set_read_timeout(Some(TIMEOUT)).unwrap();
    let serving = {
        let producer = Arc::clone(&producer);
        std::thread::spawn(move || serve_d3d11(server, producer))
    };
    // SAFETY: the sole producer is the arena above, served on this pipe.
    let mut setup = unsafe { D3d11SetupClient::from_stream(client) };
    let description = setup.describe().unwrap();
    assert_eq!(description.adapter.luid, device.luid());
    let reader = Arc::new(D3d11Device::new(AdapterSelection::Luid(description.adapter.luid)).unwrap());
    let mut consumer = setup.attach(2, &reader).unwrap();
    let release = Arc::new(reader.create_shared_fence().unwrap());
    let binding = setup.register_release_timeline(&consumer, release.clone()).unwrap();
    producer.lock().unwrap().publish(&frame(&device, 16, 16, 3), 1).unwrap();
    let AcquireOutcome::Frame(old) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    assert_eq!(sample(&reader, &old), pattern(16, 16, 3));
    assert!(matches!(
        producer.lock().unwrap().reconfigure(params(20, 10)).unwrap(),
        ReconfigurationStatus::Ready { .. }
    ));
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Reconfiguration));
    assert_eq!(
        setup.install_configuration(&mut consumer).unwrap(),
        Some(ConfigurationInstall::Installed)
    );
    producer.lock().unwrap().publish(&frame(&device, 20, 10, 4), 2).unwrap();
    let AcquireOutcome::Frame(new) = consumer.acquire_latest(old.cursor()).unwrap() else {
        panic!("missing replacement frame")
    };
    assert_eq!(sample(&reader, &new), pattern(20, 10, 4));
    // Deferred release through the transferred registration.
    let native = new.native_resources::<SharedTextureHandle, SharedFenceHandle>().unwrap();
    let ready = reader.open_fence(native.sync_handle).unwrap();
    let texture = reader.open_texture(native.surface).unwrap();
    let pending = reader
        .submit_readback(&texture, &[(&ready, new.descriptor().fence_value)], Some((&release, 1)))
        .unwrap();
    new.defer_release(&binding, 1).unwrap();
    assert_eq!(pending.finish(&reader, TIMEOUT).unwrap(), pattern(20, 10, 4));
    drop(old);
    let deadline = std::time::Instant::now() + TIMEOUT;
    while binding.pending_releases() != 0 {
        assert!(std::time::Instant::now() < deadline, "release did not complete");
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(consumer);
    drop(setup);
    serving.join().unwrap().unwrap();
}

#[test]
fn a_consumer_on_another_adapter_is_refused_before_admission() {
    let device = device();
    let other = adapters()
        .unwrap()
        .into_iter()
        .map(|adapter| AdapterSelection::Luid(adapter.luid))
        .chain([AdapterSelection::Warp])
        .filter_map(|selection| D3d11Device::new(selection).ok())
        .find(|candidate| candidate.luid() != device.luid());
    let Some(other) = other else {
        eprintln!("skipped: only one adapter LUID is available");
        return;
    };
    let producer = Arc::new(Mutex::new(producer(&device, 1)));
    let (server, client) = jackstay::local::pipe_pair().unwrap();
    client.set_read_timeout(Some(TIMEOUT)).unwrap();
    let serving = {
        let producer = Arc::clone(&producer);
        std::thread::spawn(move || serve_d3d11(server, producer))
    };
    // SAFETY: the sole producer is the arena above, served on this pipe.
    let mut setup = unsafe { D3d11SetupClient::from_stream(client) };
    match setup.attach(1, &other) {
        Err(SetupError::Refused(Refusal::AdapterMismatch { producer, consumer })) => {
            assert_eq!(producer.luid, device.luid());
            assert_eq!(consumer, other.luid());
            let message = Refusal::AdapterMismatch { producer, consumer }.to_string();
            assert!(message.contains("CPU publication"), "{message}");
        }
        other => panic!("expected an adapter refusal, got {other:?}"),
    }
    // Refusal admitted nothing, and the connection still attaches on the
    // right adapter.
    let reader = peer(&device);
    let consumer = setup.attach(1, &reader).unwrap();
    drop(consumer);
    drop(setup);
    serving.join().unwrap().unwrap();
}
