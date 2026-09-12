#![cfg(all(target_os = "macos", feature = "backend-macos"))]

#[path = "support/child.rs"]
mod child;
use child::KillOnDrop;
use jackstay::{
    acquisition::arena::{AcquireOutcome, ArenaConfig, ArenaConsumer, Cancellation, WaitInterest, WaitOutcome},
    model::{ClockDomain, ColorSpace, PixelFormat},
    native::{
        NativeFrameBackend, NativeStreamParams,
        arena::{ArenaNativeBackend, NativeArenaProducer},
        macos::{ConsumerFence, IoSurface, MacosCapturedFrame, MacosFrameBackend, MetalContext, SampleCompletion, SharedEventHandle},
    },
};

#[test]
fn native_pool_preflight_bounds_actual_storage_and_preserves_odd_sized_pixels() {
    let mut backend = MacosFrameBackend::new().unwrap();
    for format in [PixelFormat::Bgra8Unorm, PixelFormat::Rgba8Unorm] {
        for (width, height, slots) in [(1, 1, 1), (17, 19, 3), (65, 33, 2)] {
            let params = NativeStreamParams {
                width,
                height,
                pixel_format: format,
                color_space: ColorSpace::Srgb,
                clock_domain: ClockDomain::HostTime,
                modifier: 0,
            };
            let bound = backend.pool_allocation_upper_bound(&params, slots).unwrap();
            assert!(backend.allocate_surface_pool_bounded(&params, slots, bound - 1).is_err());
            let pool = backend.allocate_surface_pool_bounded(&params, slots, bound).unwrap();
            let actual = backend.allocated_pool_bytes(&pool).unwrap();
            assert!(actual > 0 && actual <= bound, "actual {actual}, reserved {bound}");
            let pixels: Vec<_> = (0..width * height * 4).map(|index| (index % 251) as u8).collect();
            for surface in backend.export_surface_handles(&pool).unwrap() {
                surface.write_pixels(&pixels).unwrap();
                let mut readback = vec![0; pixels.len()];
                surface.read_pixels(&mut readback).unwrap();
                assert_eq!(readback, pixels);
            }
        }
    }
}

#[test]
fn native_pool_preflight_rejects_empty_unsupported_and_overflowing_layouts() {
    let backend = MacosFrameBackend::new().unwrap();
    let mut params = NativeStreamParams {
        width: 1,
        height: 1,
        pixel_format: PixelFormat::Bgra8Unorm,
        color_space: ColorSpace::Srgb,
        clock_domain: ClockDomain::HostTime,
        modifier: 0,
    };
    assert!(backend.pool_allocation_upper_bound(&params, 0).is_err());
    for (width, height, slots) in [(0, 1, 1), (1, 0, 1), (u32::MAX, u32::MAX, 1), (1, u32::MAX, u32::MAX)] {
        params.width = width;
        params.height = height;
        assert!(backend.pool_allocation_upper_bound(&params, slots).is_err());
    }
    params.width = 1;
    params.height = 1;
    params.pixel_format = PixelFormat::Unknown;
    assert!(backend.pool_allocation_upper_bound(&params, 1).is_err());
}

fn captured(seed: u8) -> MacosCapturedFrame {
    let surface = IoSurface::allocate(16, 16, PixelFormat::Bgra8Unorm).unwrap();
    surface.write_pixels(&vec![seed; 16 * 16 * 4]).unwrap();
    MacosCapturedFrame { surface }
}

#[test]
fn native_reconfiguration_retains_old_pixels_and_shares_holding_credit_with_the_new_pool() {
    use jackstay::acquisition::arena::{ConfigurationInstall, ReconfigurationStatus};
    let mut producer = producer(1);
    let mut consumer = producer.attach(2).unwrap().into_consumer().unwrap();
    producer.publish(&captured(31), 1).unwrap();
    let AcquireOutcome::Frame(old) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing old frame")
    };
    let original = *old.descriptor();
    let params = NativeStreamParams {
        width: 17,
        height: 19,
        pixel_format: PixelFormat::Rgba8Unorm,
        color_space: ColorSpace::Srgb,
        clock_domain: ClockDomain::HostTime,
        modifier: 0,
    };
    let ReconfigurationStatus::Ready { generation } = producer.reconfigure(params).unwrap() else {
        panic!("replacement should fit")
    };
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Reconfiguration));
    let offer = producer.configuration_offer(consumer.incarnation()).unwrap().unwrap();
    assert_eq!(offer.install(&mut consumer).unwrap(), ConfigurationInstall::Installed);
    let surface = IoSurface::allocate(17, 19, PixelFormat::Rgba8Unorm).unwrap();
    surface.write_pixels(&vec![83; 17 * 19 * 4]).unwrap();
    producer.publish(&MacosCapturedFrame { surface }, 2).unwrap();
    let AcquireOutcome::Frame(new) = consumer.acquire_latest(original.cursor).unwrap() else {
        panic!("missing replacement frame")
    };
    assert_eq!(new.descriptor().config_generation, generation);
    assert_ne!(new.descriptor().pool_id, original.pool_id);
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));
    drop(consumer);
    producer.poll_cleanup().unwrap();
    let metal = MetalContext::new().unwrap();
    for (frame, expected) in [(&old, vec![31; 16 * 16 * 4]), (&new, vec![83; 17 * 19 * 4])] {
        let native = frame.native_resources::<IoSurface, SharedEventHandle>().unwrap();
        let fence = ConsumerFence::from_handle(&metal, native.sync_handle).unwrap();
        let descriptor = frame.descriptor();
        let pixels = fence
            .sample_offscreen(&metal, native.surface, descriptor.fence_value, descriptor.width, descriptor.height)
            .unwrap();
        assert_eq!(pixels, expected);
    }
    assert_eq!(old.descriptor(), &original);
}

#[test]
fn native_replacement_waits_for_old_storage_and_reclaims_repeated_pool_generations() {
    use jackstay::acquisition::arena::{ConfigurationInstall, PublishOutcome, ReconfigurationStatus};
    let mut params = NativeStreamParams {
        width: 16,
        height: 16,
        pixel_format: PixelFormat::Bgra8Unorm,
        color_space: ColorSpace::Srgb,
        clock_domain: ClockDomain::HostTime,
        modifier: 0,
    };
    let mut backend = MacosFrameBackend::new().unwrap();
    let bound = backend.pool_allocation_upper_bound(&params, 6).unwrap();
    // One native pool plus control/resource/claim pages and one spare page.
    // This can replace a retired pool but cannot overlap two native pools.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let config = ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 0,
        memory_budget: bound + 4 * page,
        max_incarnations: 1,
        drain_timeout: std::time::Duration::from_secs(5),
    };
    let pool = backend.allocate_surface_pool_bounded(&params, 6, bound).unwrap();
    let fence = backend.create_fence().unwrap();
    let mut producer = NativeArenaProducer::from_allocated_parts(backend, pool, fence, params.clone(), config).unwrap();
    let mut consumer = producer.attach(1).unwrap().into_consumer().unwrap();
    producer.publish(&captured(61), 1).unwrap();
    let AcquireOutcome::Frame(old) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing old frame")
    };
    params.width = 17;
    assert!(matches!(
        producer.reconfigure(params.clone()).unwrap(),
        ReconfigurationStatus::PausedCapacity { .. }
    ));
    assert!(producer.attach(1).is_err(), "transition admitted another consumer");
    assert!(producer.configuration_offer(consumer.incarnation()).unwrap().is_none());
    assert_eq!(producer.publish(&captured(99), 2).unwrap(), PublishOutcome::Dropped);
    consumer.relinquish_configuration();
    assert!(matches!(
        producer.advance_reconfiguration().unwrap(),
        ReconfigurationStatus::PausedCapacity { .. }
    ));
    {
        let native = old.native_resources::<IoSurface, SharedEventHandle>().unwrap();
        let metal = MetalContext::new().unwrap();
        let readiness = ConsumerFence::from_handle(&metal, native.sync_handle).unwrap();
        assert_eq!(
            readiness
                .sample_offscreen(&metal, native.surface, old.descriptor().fence_value, 16, 16)
                .unwrap(),
            vec![61; 16 * 16 * 4]
        );
    }
    drop(old);
    assert!(matches!(
        producer.advance_reconfiguration().unwrap(),
        ReconfigurationStatus::Ready { .. }
    ));
    let offer = producer.configuration_offer(consumer.incarnation()).unwrap().unwrap();
    assert_eq!(offer.install(&mut consumer).unwrap(), ConfigurationInstall::Installed);
    for _ in 0..100 {
        consumer.relinquish_configuration();
        assert!(matches!(
            producer.reconfigure(params.clone()).unwrap(),
            ReconfigurationStatus::Ready { .. }
        ));
        let offer = producer.configuration_offer(consumer.incarnation()).unwrap().unwrap();
        assert_eq!(offer.install(&mut consumer).unwrap(), ConfigurationInstall::Installed);
    }
    let surface = IoSurface::allocate(17, 16, PixelFormat::Bgra8Unorm).unwrap();
    surface.write_pixels(&vec![92; 17 * 16 * 4]).unwrap();
    producer.publish(&MacosCapturedFrame { surface }, 3).unwrap();
    let AcquireOutcome::Frame(new) = consumer.acquire_latest(0).unwrap() else {
        panic!("replacement did not resume delivery")
    };
    assert_eq!(new.descriptor().width, 17);
    assert_eq!(new.descriptor().producer_drop_count, 1);
}

fn producer(max_incarnations: u32) -> NativeArenaProducer<MacosFrameBackend> {
    let params = NativeStreamParams {
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
    let config = ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 0,
        memory_budget: 1024 * 1024,
        max_incarnations,
        drain_timeout: std::time::Duration::from_secs(5),
    };
    NativeArenaProducer::from_allocated_parts(backend, pool, fence, params, config).unwrap()
}

#[test]
fn an_acquired_native_frame_owns_its_surface_and_readiness_after_setup_and_api_teardown() {
    let mut producer = producer(1);
    let consumer = producer.attach(1).unwrap().into_consumer().unwrap();
    producer.publish(&captured(53), 1).unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    drop(consumer);
    drop(producer);
    // The setup grant was consumed, and neither API owner remains. Resolve and
    // sample using only the successful frame acquisition's retained resources.
    let native = frame.native_resources::<IoSurface, SharedEventHandle>().unwrap();
    let metal = MetalContext::new().unwrap();
    let readiness = ConsumerFence::from_handle(&metal, native.sync_handle).unwrap();
    let pixels = readiness
        .sample_offscreen(&metal, native.surface, frame.descriptor().fence_value, 16, 16)
        .unwrap();
    assert_eq!(pixels, vec![53; 16 * 16 * 4]);
}

#[test]
fn native_acquisition_rejects_a_descriptor_that_disagrees_with_its_retained_pool() {
    use jackstay::acquisition::arena::ArenaError;
    let mut producer = producer(1);
    let mut grant = producer.attach(1).unwrap();
    grant.pool_id += 1;
    let consumer = grant.into_consumer().unwrap();
    producer.publish(&captured(19), 1).unwrap();
    assert!(matches!(consumer.acquire_latest(0), Err(ArenaError::Mapping(_))));
    drop(consumer);
    let consumer = producer.attach(1).unwrap().into_consumer().unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("failed acquisition leaked its claim")
    };
    assert!(frame.native_resources::<IoSurface, SharedEventHandle>().is_ok());
}

#[test]
fn a_shared_native_lease_retains_its_iosurface_through_ring_wrap_and_samples_after_readiness() {
    let mut producer = producer(2);
    let grant = producer.attach(1).unwrap();
    let pool_id = grant.pool_id;
    let consumer = grant.into_consumer().unwrap();
    let metal = MetalContext::new().unwrap();
    producer.publish(&captured(7), 1).unwrap();
    let AcquireOutcome::Frame(held) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing native lease")
    };
    let descriptor = *held.descriptor();
    let native = held.native_resources::<IoSurface, SharedEventHandle>().unwrap();
    let readiness = ConsumerFence::from_handle(&metal, native.sync_handle).unwrap();
    for timestamp in 2..=100 {
        producer.publish(&captured(91), timestamp).unwrap();
    }
    let pixels = readiness
        .sample_offscreen(&metal, native.surface, descriptor.fence_value, 16, 16)
        .unwrap();
    assert_eq!(pixels, vec![7; 16 * 16 * 4]);
    assert_eq!(held.descriptor().pool_id, pool_id);
    drop(held);
    let AcquireOutcome::Frame(latest) = consumer.acquire_latest(descriptor.cursor).unwrap() else {
        panic!("no new native frame")
    };
    assert!(latest.cursor() > 4, "the native publication ring did not wrap");
}

#[test]
fn submitted_gpu_work_keeps_a_deferred_iosurface_lease_through_pool_replacement() {
    use std::sync::Arc;
    let mut producer = producer(2);
    let grant = producer.attach(1).unwrap();
    let mut consumer = grant.into_consumer().unwrap();
    let metal = MetalContext::new().unwrap();
    let release = ConsumerFence::new(&metal).unwrap();
    let gate = ConsumerFence::new(&metal).unwrap();
    let submitted = ConsumerFence::new(&metal).unwrap();
    let observer_metal = MetalContext::new().unwrap();
    let observed_release = Arc::new(ConsumerFence::from_handle(&observer_metal, &release.export_handle().unwrap()).unwrap());
    let registration = producer
        .register_release_timeline(consumer.incarnation(), observed_release.clone())
        .unwrap();
    let registration = consumer.bind_release_timeline(&registration, observed_release.clone()).unwrap();
    producer.publish(&captured(37), 1).unwrap();
    let AcquireOutcome::Frame(held) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    let descriptor = *held.descriptor();
    let native = held.native_resources::<IoSurface, SharedEventHandle>().unwrap();
    let readiness = ConsumerFence::from_handle(&metal, native.sync_handle).unwrap();
    // The diagnostic sampler's extra reference drops when its scoped call
    // returns, before the producer's first post-completion retirement poll.
    let old_surface = native.surface.clone();
    std::thread::scope(|scope| {
        struct OpenOnDrop<'a>(&'a ConsumerFence);
        impl Drop for OpenOnDrop<'_> {
            fn drop(&mut self) {
                self.0.signal_cpu(1);
            }
        }
        let _open_on_unwind = OpenOnDrop(&gate);
        let sample = scope.spawn(|| {
            let surface = old_surface;
            readiness.sample_offscreen_with_completion(
                &metal,
                &surface,
                descriptor.fence_value,
                (16, 16),
                SampleCompletion {
                    release: (&release, 1),
                    before_sample: Some((&gate, 1)),
                    submitted: Some((&submitted, 1)),
                },
            )
        });
        assert!(submitted.wait(1, 5000), "consumer commands were not submitted");
        held.defer_release(&registration, 1).unwrap();
        assert_eq!(observed_release.signaled_value(), 0);
        assert_eq!(producer.poll_cleanup().unwrap(), 0);
        assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));
        assert!(matches!(
            producer
                .reconfigure(NativeStreamParams {
                    width: 16,
                    height: 16,
                    pixel_format: PixelFormat::Bgra8Unorm,
                    color_space: ColorSpace::Srgb,
                    clock_domain: ClockDomain::HostTime,
                    modifier: 0,
                })
                .unwrap(),
            jackstay::acquisition::arena::ReconfigurationStatus::Ready { .. }
        ));
        let offer = producer.configuration_offer(consumer.incarnation()).unwrap().unwrap();
        assert_eq!(
            offer.install(&mut consumer).unwrap(),
            jackstay::acquisition::arena::ConfigurationInstall::Installed
        );
        for timestamp in 2..=100 {
            producer.publish(&captured(91), timestamp).unwrap();
        }
        assert_eq!(producer.poll_cleanup().unwrap(), 0);
        let before_completion = consumer.events();
        gate.signal_cpu(1);
        assert!(
            matches!(consumer.wait(before_completion, WaitInterest::CAPACITY, &Cancellation::new().unwrap(), Some(std::time::Duration::from_secs(5))).unwrap(),
            WaitOutcome::Changed(events) if events.capacity_epoch == before_completion.capacity_epoch + 1),
            "GPU completion did not return credit while the producer was idle"
        );
        assert!(observed_release.wait(1, 5000), "GPU release did not complete");
        assert_eq!(sample.join().unwrap().unwrap(), vec![37; 16 * 16 * 4]);
        assert_eq!(producer.poll_cleanup().unwrap(), 1);
        let AcquireOutcome::Frame(latest) = consumer.acquire_latest(descriptor.cursor).unwrap() else {
            panic!("credit was not returned")
        };
        assert_ne!(latest.descriptor().pool_id, descriptor.pool_id);
        assert!(latest.cursor() > 4, "publication did not wrap the retained ring");
    });
}

#[test]
fn a_crashed_native_consumer_without_completion_evidence_keeps_its_iosurface_quarantined() {
    use std::{
        io::{Read, Write},
        os::{fd::AsRawFd, unix::net::UnixListener},
        process::Command,
        time::Duration,
    };

    use jackstay::fdpass;
    let mut producer = producer(2);
    let healthy_grant = producer.attach(1).unwrap();
    let healthy = ArenaConsumer::from_grant(healthy_grant.consumer).unwrap();
    producer.publish(&captured(7), 1).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("native-crash.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "native_claim_crash_child", "--nocapture"])
        .env("JACKSTAY_NATIVE_CRASH_SOCKET", &socket)
        .spawn()
        .unwrap();
    let mut child = KillOnDrop(child);
    let (mut stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let grant = producer.attach_process(1, child.id()).unwrap();
    let dead = grant.consumer.incarnation();
    let (descriptor, fds) = grant.consumer.into_parts().unwrap();
    let json = serde_json::to_vec(&descriptor).unwrap();
    stream.write_all(&(json.len() as u32).to_le_bytes()).unwrap();
    stream.write_all(&json).unwrap();
    fdpass::send_fds(&stream, &fds.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>()).unwrap();
    let mut held = [0; 12];
    stream.read_exact(&mut held).unwrap();
    let slot = u32::from_le_bytes(held[..4].try_into().unwrap()) as usize;
    let fence_value = u64::from_le_bytes(held[4..].try_into().unwrap());
    assert_eq!(stream.read(&mut held).unwrap(), 0);
    producer.publish(&captured(91), 2).unwrap();
    let AcquireOutcome::Frame(healthy_frame) = healthy.acquire_latest(1).unwrap() else {
        panic!("healthy consumer missed frame")
    };
    assert_ne!(healthy_frame.descriptor().slot_id as usize, slot);
    producer.close(dead).unwrap();
    producer.poll_cleanup().unwrap();
    assert!(child.try_wait().unwrap().is_none());
    assert!(producer.attach(1).is_err());
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    producer.poll_cleanup().unwrap();
    let failures = producer.cleanup_failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].incarnation, dead);
    assert!(failures[0].reason.contains("lacking completion evidence"));
    assert!(
        producer.retry_cleanup(dead).is_err(),
        "retry invented evidence for an unsubmitted native release"
    );
    for timestamp in 3..=100 {
        producer.publish(&captured(99), timestamp).unwrap();
    }
    assert!(producer.attach(1).is_err(), "quarantine disappeared during publication");
    let metal = MetalContext::new().unwrap();
    let readiness = ConsumerFence::from_handle(&metal, &grant.sync_handle).unwrap();
    assert_eq!(
        readiness
            .sample_offscreen(&metal, &grant.surface_handles[slot], fence_value, 16, 16)
            .unwrap(),
        vec![7; 16 * 16 * 4]
    );
    assert_eq!(
        readiness
            .sample_offscreen(
                &metal,
                &grant.surface_handles[healthy_frame.descriptor().slot_id as usize],
                healthy_frame.descriptor().fence_value,
                16,
                16
            )
            .unwrap(),
        vec![91; 16 * 16 * 4]
    );
    drop(healthy_frame);
    let AcquireOutcome::Frame(latest) = healthy.acquire_latest(2).unwrap() else {
        panic!("healthy consumer stopped progressing")
    };
    assert!(latest.cursor() > 4, "the ring did not wrap");
}

#[test]
#[ignore = "subprocess helper invoked by a_crashed_native_consumer_without_completion_evidence_keeps_its_iosurface_quarantined"]
fn native_claim_crash_child() {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
    };

    use jackstay::{acquisition::arena::ConsumerGrant, fdpass};
    let mut stream = UnixStream::connect(std::env::var("JACKSTAY_NATIVE_CRASH_SOCKET").unwrap()).unwrap();
    let mut len = [0; 4];
    stream.read_exact(&mut len).unwrap();
    let mut json = vec![0; u32::from_le_bytes(len) as usize];
    stream.read_exact(&mut json).unwrap();
    let descriptor = serde_json::from_slice(&json).unwrap();
    let fds = fdpass::recv_fds(&stream, 5).unwrap().try_into().unwrap();
    // SAFETY: this is the exact lifetime to which the parent's producer bound
    // its single-use grant. The child never forks or forwards mapped claims.
    let grant = unsafe { ConsumerGrant::from_parts(descriptor, fds) }.unwrap();
    let consumer = ArenaConsumer::from_grant(grant).unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing native claim")
    };
    // Test barrier reports which acquired surface the parent must inspect. No
    // release is submitted; the producer must assume native use is unresolved.
    stream.write_all(&frame.descriptor().slot_id.to_le_bytes()).unwrap();
    stream.write_all(&frame.descriptor().fence_value.to_le_bytes()).unwrap();
    drop(stream);
    loop {
        std::thread::park();
    }
}
