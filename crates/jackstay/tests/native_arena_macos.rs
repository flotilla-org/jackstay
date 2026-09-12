#![cfg(all(target_os = "macos", feature = "backend-macos"))]

#[path = "support/child.rs"]
mod child;
use child::KillOnDrop;
use jackstay::{
    acquisition::arena::{AcquireOutcome, ArenaConfig, ArenaConsumer, Cancellation, WaitInterest, WaitOutcome},
    model::{ClockDomain, ColorSpace, PixelFormat},
    native::{
        NativeFrameBackend, NativeStreamParams,
        arena::NativeArenaProducer,
        macos::{ConsumerFence, IoSurface, MacosCapturedFrame, MacosFrameBackend, MetalContext, SampleCompletion, SharedEventHandle},
    },
};

fn captured(seed: u8) -> MacosCapturedFrame {
    let surface = IoSurface::allocate(16, 16, PixelFormat::Bgra8Unorm).unwrap();
    surface.write_pixels(&vec![seed; 16 * 16 * 4]).unwrap();
    MacosCapturedFrame { surface }
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
fn submitted_gpu_work_keeps_a_deferred_iosurface_lease_until_the_gpu_release_event() {
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
    // The diagnostic sampler owns a reference for its scoped call. It is
    // dropped before the consumer retires this still-current configuration.
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
            readiness.sample_offscreen_with_completion(
                &metal,
                &old_surface,
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
