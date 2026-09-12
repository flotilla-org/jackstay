#![cfg(all(target_os = "macos", feature = "backend-macos"))]

use std::sync::{Arc, Mutex};

use jackstay::{
    acquisition::arena::{AcquireOutcome, ArenaConfig},
    model::{ClockDomain, ColorSpace, PixelFormat},
    native::{
        NativeFrameBackend, NativeStreamParams,
        arena::NativeArenaProducer,
        macos::{
            ConsumerFence, IoSurface, MacosCapturedFrame, MacosFrameBackend, MetalContext, SharedEventHandle,
            xpc::arena::{XpcArenaClient, XpcArenaServer},
        },
    },
};

fn producer() -> Arc<Mutex<NativeArenaProducer<MacosFrameBackend>>> {
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
    Arc::new(Mutex::new(
        NativeArenaProducer::from_allocated_parts(
            backend,
            pool,
            fence,
            params,
            ArenaConfig {
                resource_capacity: 6,
                retained_history: 2,
                producer_reserve: 1,
                payload_capacity: 0,
                memory_budget: 1024 * 1024,
                max_incarnations: 1,
                drain_timeout: std::time::Duration::from_secs(5),
            },
        )
        .unwrap(),
    ))
}

#[test]
fn c_native_frames_keep_their_generation_handles_after_replacement_and_api_teardown() {
    use std::ptr;

    use jackstay::{
        acquisition::arena::FrameDescriptor,
        ffi::*,
        ffi_acquisition::{macos::*, *},
    };
    let producer = producer();
    let (_server, endpoint) = XpcArenaServer::start_anonymous(None, producer.clone()).unwrap();
    let client = XpcArenaClient::connect_endpoint(&endpoint).unwrap();
    let (mut connection, mut consumer) = FtMacosAcquisitionConnection::attach(client, 2).unwrap();
    let source = IoSurface::allocate(16, 16, PixelFormat::Bgra8Unorm).unwrap();
    source.write_pixels(&vec![31; 16 * 16 * 4]).unwrap();
    producer
        .lock()
        .unwrap()
        .publish(&MacosCapturedFrame { surface: source }, 1)
        .unwrap();
    // SAFETY: exclusive C handles. Borrowed native resources are imported only
    // while their corresponding frame is held, and sampling completes before
    // immediate release. No copied transport handles survive frame release.
    unsafe {
        let mut old = ptr::null_mut();
        let mut new = ptr::null_mut();
        let mut range = FtAcquisitionRange::default();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut old, &mut range),
            FT_STATUS_OK
        );
        producer
            .lock()
            .unwrap()
            .reconfigure(NativeStreamParams {
                width: 17,
                height: 19,
                pixel_format: PixelFormat::Rgba8Unorm,
                color_space: ColorSpace::Srgb,
                clock_domain: ClockDomain::HostTime,
                modifier: 0,
            })
            .unwrap();
        assert_eq!(ft_acquisition_macos_install_configuration(connection, consumer), FT_STATUS_OK);
        assert_eq!(ft_acquisition_macos_install_configuration(connection, consumer), FT_STATUS_EMPTY);
        let source = IoSurface::allocate(17, 19, PixelFormat::Rgba8Unorm).unwrap();
        source.write_pixels(&vec![83; 17 * 19 * 4]).unwrap();
        producer
            .lock()
            .unwrap()
            .publish(&MacosCapturedFrame { surface: source }, 2)
            .unwrap();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut new, &mut range),
            FT_STATUS_OK
        );
        ft_acquisition_consumer_destroy(&mut consumer);
        ft_acquisition_macos_connection_destroy(&mut connection);
        let metal = MetalContext::new().unwrap();
        for (frame, expected, generation) in [(&mut old, vec![31; 16 * 16 * 4], 1), (&mut new, vec![83; 17 * 19 * 4], 2)] {
            let mut descriptor = FrameDescriptor::default();
            let mut surface = ptr::null_mut();
            let mut readiness = ptr::null_mut();
            assert_eq!(ft_acquired_frame_describe(*frame, &mut descriptor), FT_STATUS_OK);
            assert_eq!(descriptor.config_generation, generation);
            assert_eq!(
                ft_acquired_frame_macos_resources(*frame, &mut surface, &mut readiness),
                FT_STATUS_OK
            );
            {
                let surface = IoSurface::from_borrowed(std::ptr::NonNull::new(surface).unwrap());
                let readiness = ConsumerFence::from_borrowed_handle(&metal, std::ptr::NonNull::new(readiness).unwrap()).unwrap();
                assert_eq!(
                    readiness
                        .sample_offscreen(&metal, &surface, descriptor.fence_value, descriptor.width, descriptor.height)
                        .unwrap(),
                    expected
                );
            }
            assert_eq!(ft_acquired_frame_release(frame), FT_STATUS_OK);
        }
    }
}

#[test]
fn xpc_admits_its_authenticated_peer_and_transfers_owned_acquisition_resources() {
    let producer = producer();
    let (_server, endpoint) = XpcArenaServer::start_anonymous(Some("test-authority".to_owned()), producer.clone()).unwrap();
    let mut client = XpcArenaClient::connect_endpoint(&endpoint).unwrap();
    assert!(client.attach(2).is_err());
    assert!(client.authorize("wrong-authority").is_err());
    client.authorize("test-authority").unwrap();
    let consumer = client.attach(2).unwrap();
    assert!(client.attach(1).is_err(), "a connection attached twice");
    let source = IoSurface::allocate(16, 16, PixelFormat::Bgra8Unorm).unwrap();
    source.write_pixels(&vec![47; 16 * 16 * 4]).unwrap();
    producer
        .lock()
        .unwrap()
        .publish(&MacosCapturedFrame { surface: source }, 1)
        .unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("no transferred native frame")
    };
    let native = frame.native_resources::<IoSurface, SharedEventHandle>().unwrap();
    let metal = MetalContext::new().unwrap();
    let readiness = ConsumerFence::from_handle(&metal, native.sync_handle).unwrap();
    assert_eq!(
        readiness
            .sample_offscreen(&metal, native.surface, frame.descriptor().fence_value, 16, 16)
            .unwrap(),
        vec![47; 16 * 16 * 4]
    );
}

#[test]
fn xpc_replacement_keeps_an_acquired_old_generation_and_installs_new_native_handles() {
    use jackstay::acquisition::arena::ConfigurationInstall;
    let producer = producer();
    let (_server, endpoint) = XpcArenaServer::start_anonymous(None, producer.clone()).unwrap();
    let mut client = XpcArenaClient::connect_endpoint(&endpoint).unwrap();
    let mut consumer = client.attach(2).unwrap();
    let source = IoSurface::allocate(16, 16, PixelFormat::Bgra8Unorm).unwrap();
    source.write_pixels(&vec![29; 16 * 16 * 4]).unwrap();
    producer
        .lock()
        .unwrap()
        .publish(&MacosCapturedFrame { surface: source }, 1)
        .unwrap();
    let AcquireOutcome::Frame(old) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing old frame")
    };
    producer
        .lock()
        .unwrap()
        .reconfigure(NativeStreamParams {
            width: 17,
            height: 19,
            pixel_format: PixelFormat::Rgba8Unorm,
            color_space: ColorSpace::Srgb,
            clock_domain: ClockDomain::HostTime,
            modifier: 0,
        })
        .unwrap();
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Reconfiguration));
    assert_eq!(
        client.install_configuration(&mut consumer).unwrap(),
        Some(ConfigurationInstall::Installed)
    );
    let source = IoSurface::allocate(17, 19, PixelFormat::Rgba8Unorm).unwrap();
    source.write_pixels(&vec![82; 17 * 19 * 4]).unwrap();
    producer
        .lock()
        .unwrap()
        .publish(&MacosCapturedFrame { surface: source }, 2)
        .unwrap();
    let AcquireOutcome::Frame(new) = consumer.acquire_latest(old.cursor()).unwrap() else {
        panic!("missing replacement frame")
    };
    assert_ne!(old.descriptor().pool_id, new.descriptor().pool_id);
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));
    let metal = MetalContext::new().unwrap();
    for (frame, expected) in [(&old, vec![29; 16 * 16 * 4]), (&new, vec![82; 17 * 19 * 4])] {
        let native = frame.native_resources::<IoSurface, SharedEventHandle>().unwrap();
        let readiness = ConsumerFence::from_handle(&metal, native.sync_handle).unwrap();
        assert_eq!(
            readiness
                .sample_offscreen(
                    &metal,
                    native.surface,
                    frame.descriptor().fence_value,
                    frame.descriptor().width,
                    frame.descriptor().height
                )
                .unwrap(),
            expected
        );
    }
}

#[test]
fn xpc_registers_the_consumers_actual_gpu_completion_event_through_c() {
    use std::ptr;

    use jackstay::{
        acquisition::arena::FrameDescriptor,
        ffi::*,
        ffi_acquisition::{macos::*, *},
        native::macos::SampleCompletion,
    };
    let producer = producer();
    let (_server, endpoint) = XpcArenaServer::start_anonymous(None, producer.clone()).unwrap();
    let client = XpcArenaClient::connect_endpoint(&endpoint).unwrap();
    let (mut connection, mut consumer) = FtMacosAcquisitionConnection::attach(client, 1).unwrap();
    let metal = MetalContext::new().unwrap();
    let release = ConsumerFence::new(&metal).unwrap();
    let exported = release.export_handle().unwrap();
    let mut binding = ptr::null_mut();
    // SAFETY: all C handles are exclusively owned. The exported event remains
    // live through registration; the library must then retain its own observer.
    unsafe {
        assert_eq!(
            ft_acquisition_macos_register_release(connection, consumer, exported.as_raw(), &mut binding),
            FT_STATUS_OK
        );
        drop(exported);
        let source = IoSurface::allocate(16, 16, PixelFormat::Bgra8Unorm).unwrap();
        source.write_pixels(&vec![68; 16 * 16 * 4]).unwrap();
        producer
            .lock()
            .unwrap()
            .publish(&MacosCapturedFrame { surface: source }, 1)
            .unwrap();
        let mut frame = ptr::null_mut();
        let mut range = FtAcquisitionRange::default();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut frame, &mut range),
            FT_STATUS_OK
        );
        let mut descriptor = FrameDescriptor::default();
        let mut raw_surface = ptr::null_mut();
        let mut raw_readiness = ptr::null_mut();
        assert_eq!(ft_acquired_frame_describe(frame, &mut descriptor), FT_STATUS_OK);
        assert_eq!(
            ft_acquired_frame_macos_resources(frame, &mut raw_surface, &mut raw_readiness),
            FT_STATUS_OK
        );
        let readiness = ConsumerFence::from_borrowed_handle(&metal, std::ptr::NonNull::new(raw_readiness).unwrap()).unwrap();
        let surface = IoSurface::from_borrowed(std::ptr::NonNull::new(raw_surface).unwrap());
        let gate = ConsumerFence::new(&metal).unwrap();
        let submitted = ConsumerFence::new(&metal).unwrap();
        let mut cancellation = ptr::null_mut();
        assert_eq!(ft_acquisition_cancellation_create(&mut cancellation), FT_STATUS_OK);
        std::thread::scope(|scope| {
            struct OpenOnDrop<'a>(&'a ConsumerFence);
            impl Drop for OpenOnDrop<'_> {
                fn drop(&mut self) {
                    self.0.signal_cpu(1);
                }
            }
            let _open_on_unwind = OpenOnDrop(&gate);
            let sample = scope.spawn(|| {
                let surface = surface;
                let readiness = readiness;
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
            assert!(submitted.wait(1, 5000));
            assert_eq!(ft_acquired_frame_defer_release(&mut frame, binding, 1), FT_STATUS_OK);
            assert!(frame.is_null());
            assert_eq!(release.signaled_value(), 0);
            assert_eq!(producer.lock().unwrap().poll_cleanup().unwrap(), 0);
            let mut unavailable = ptr::null_mut();
            assert_eq!(
                ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut unavailable, &mut range),
                FT_STATUS_HOLDING_LIMIT
            );
            let mut before = FtAcquisitionEvents::default();
            let mut after = FtAcquisitionEvents::default();
            assert_eq!(ft_acquisition_snapshot(consumer, &mut before), FT_STATUS_OK);
            gate.signal_cpu(1);
            assert_eq!(
                ft_acquisition_wait(consumer, &before, FT_WAIT_CAPACITY, cancellation, 5_000_000_000, &mut after),
                FT_STATUS_OK
            );
            assert!(after.capacity_epoch > before.capacity_epoch);
            assert_eq!(sample.join().unwrap().unwrap(), vec![68; 16 * 16 * 4]);
            assert_eq!(producer.lock().unwrap().poll_cleanup().unwrap(), 1);
        });
        ft_acquisition_cancellation_destroy(&mut cancellation);
        ft_acquisition_release_timeline_destroy(&mut binding);
        ft_acquisition_consumer_destroy(&mut consumer);
        ft_acquisition_macos_connection_destroy(&mut connection);
    }
}

#[test]
fn xpc_disconnect_closes_acquisition_without_reclaiming_a_live_peers_frame() {
    use jackstay::acquisition::arena::{Cancellation, WaitInterest, WaitOutcome};
    let producer = producer();
    let (_server, endpoint) = XpcArenaServer::start_anonymous(None, producer.clone()).unwrap();
    let mut client = XpcArenaClient::connect_endpoint(&endpoint).unwrap();
    let mut consumer = client.attach(1).unwrap();
    let incarnation = consumer.incarnation();
    let source = IoSurface::allocate(16, 16, PixelFormat::Bgra8Unorm).unwrap();
    source.write_pixels(&vec![39; 16 * 16 * 4]).unwrap();
    producer
        .lock()
        .unwrap()
        .publish(&MacosCapturedFrame { surface: source }, 1)
        .unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("no frame before disconnect")
    };
    let before = consumer.events();
    drop(client);
    assert!(matches!(
        consumer
            .wait(
                before,
                WaitInterest::DATA,
                &Cancellation::new().unwrap(),
                Some(std::time::Duration::from_secs(5))
            )
            .unwrap(),
        WaitOutcome::Changed(events) if events.closed
    ));
    let mut restarted = XpcArenaClient::connect_endpoint(&endpoint).unwrap();
    assert!(restarted.attach(1).is_err(), "EOF was mistaken for process exit");
    {
        let native = frame.native_resources::<IoSurface, SharedEventHandle>().unwrap();
        let metal = MetalContext::new().unwrap();
        let readiness = ConsumerFence::from_handle(&metal, native.sync_handle).unwrap();
        assert_eq!(
            readiness
                .sample_offscreen(&metal, native.surface, frame.descriptor().fence_value, 16, 16)
                .unwrap(),
            vec![39; 16 * 16 * 4]
        );
    }
    drop(frame);
    drop(consumer);
    let replacement = restarted.attach(1).unwrap();
    assert_ne!(replacement.incarnation(), incarnation);
}

#[test]
fn xpc_setup_rejects_a_foreign_claim_scope_even_when_incarnation_numbers_match() {
    let first_producer = producer();
    let second_producer = producer();
    let (_first_server, first_endpoint) = XpcArenaServer::start_anonymous(None, first_producer).unwrap();
    let (_second_server, second_endpoint) = XpcArenaServer::start_anonymous(None, second_producer).unwrap();
    let mut first = XpcArenaClient::connect_endpoint(&first_endpoint).unwrap();
    let mut second = XpcArenaClient::connect_endpoint(&second_endpoint).unwrap();
    let first_consumer = first.attach(1).unwrap();
    let mut second_consumer = second.attach(1).unwrap();
    assert_eq!(first_consumer.incarnation(), second_consumer.incarnation());
    assert!(first.install_configuration(&mut second_consumer).is_err());
    let metal = MetalContext::new().unwrap();
    let event = Arc::new(ConsumerFence::new(&metal).unwrap());
    assert!(first.register_release_timeline(&second_consumer, event.clone()).is_err());
    // A rejected foreign binding must not consume the first incarnation's
    // single registration slot on the producer.
    first.register_release_timeline(&first_consumer, event).unwrap();
}
