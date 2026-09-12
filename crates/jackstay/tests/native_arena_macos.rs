#![cfg(all(target_os = "macos", feature = "backend-macos"))]

use jackstay::{
    acquisition::arena::{AcquireOutcome, ArenaConfig, ArenaConsumer, Cancellation, WaitInterest, WaitOutcome},
    model::{ClockDomain, ColorSpace, PixelFormat},
    native::{
        NativeFrameBackend, NativeStreamParams,
        arena::NativeArenaProducer,
        macos::{ConsumerFence, IoSurface, MacosCapturedFrame, MacosFrameBackend, MetalContext, SampleCompletion},
    },
};

fn captured(seed: u8) -> MacosCapturedFrame {
    let surface = IoSurface::allocate(16, 16, PixelFormat::Bgra8Unorm).unwrap();
    surface.write_pixels(&vec![seed; 16 * 16 * 4]).unwrap();
    MacosCapturedFrame { surface }
}

#[test]
fn a_shared_native_lease_retains_its_iosurface_through_ring_wrap_and_samples_after_readiness() {
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
        max_incarnations: 2,
    };
    let mut producer = NativeArenaProducer::from_allocated_parts(backend, pool, fence, params, config).unwrap();
    let grant = producer.attach(1).unwrap();
    let consumer = ArenaConsumer::from_grant(grant.consumer).unwrap();
    let metal = MetalContext::new().unwrap();
    let readiness = ConsumerFence::from_handle(&metal, &grant.sync_handle).unwrap();
    producer.publish(&captured(7), 1).unwrap();
    let AcquireOutcome::Frame(held) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing native lease")
    };
    let descriptor = *held.descriptor();
    for timestamp in 2..=100 {
        producer.publish(&captured(91), timestamp).unwrap();
    }
    let pixels = readiness
        .sample_offscreen(
            &metal,
            &grant.surface_handles[descriptor.slot_id as usize],
            descriptor.fence_value,
            16,
            16,
        )
        .unwrap();
    assert_eq!(pixels, vec![7; 16 * 16 * 4]);
    assert_eq!(held.descriptor().pool_id, grant.pool_id);
    drop(held);
    let AcquireOutcome::Frame(latest) = consumer.acquire_latest(descriptor.cursor).unwrap() else {
        panic!("no new native frame")
    };
    assert!(latest.cursor() > 4, "the native publication ring did not wrap");
}

#[test]
fn submitted_gpu_work_keeps_a_deferred_iosurface_lease_until_the_gpu_release_event() {
    use std::sync::Arc;
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
        max_incarnations: 2,
    };
    let mut producer = NativeArenaProducer::from_allocated_parts(backend, pool, fence, params, config).unwrap();
    let grant = producer.attach(1).unwrap();
    let mut consumer = ArenaConsumer::from_grant(grant.consumer).unwrap();
    let metal = MetalContext::new().unwrap();
    let readiness = ConsumerFence::from_handle(&metal, &grant.sync_handle).unwrap();
    let release = ConsumerFence::new(&metal).unwrap();
    let gate = ConsumerFence::new(&metal).unwrap();
    let submitted = ConsumerFence::new(&metal).unwrap();
    let observer_metal = MetalContext::new().unwrap();
    let observed_release = Arc::new(ConsumerFence::from_handle(&observer_metal, &release.export_handle().unwrap()).unwrap());
    let registration = producer
        .register_release_timeline(consumer.incarnation(), observed_release.clone())
        .unwrap();
    producer.publish(&captured(37), 1).unwrap();
    let AcquireOutcome::Frame(held) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    let descriptor = *held.descriptor();
    let old_surface = &grant.surface_handles[descriptor.slot_id as usize];
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
                old_surface,
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
        assert_eq!(producer.poll_release_completions().unwrap(), 0);
        assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));
        for timestamp in 2..=100 {
            producer.publish(&captured(91), timestamp).unwrap();
        }
        assert_eq!(producer.poll_release_completions().unwrap(), 0);
        let before_completion = consumer.events();
        gate.signal_cpu(1);
        assert!(
            matches!(consumer.wait(before_completion, WaitInterest::CAPACITY, &Cancellation::new().unwrap(), Some(std::time::Duration::from_secs(5))).unwrap(),
            WaitOutcome::Changed(events) if events.capacity_epoch == before_completion.capacity_epoch + 1),
            "GPU completion did not return credit while the producer was idle"
        );
        assert!(observed_release.wait(1, 5000), "GPU release did not complete");
        assert_eq!(sample.join().unwrap().unwrap(), vec![37; 16 * 16 * 4]);
        assert_eq!(producer.poll_release_completions().unwrap(), 1);
        let AcquireOutcome::Frame(latest) = consumer.acquire_latest(descriptor.cursor).unwrap() else {
            panic!("credit was not returned")
        };
        assert!(latest.cursor() > 4, "publication did not wrap the retained ring");
    });
}
