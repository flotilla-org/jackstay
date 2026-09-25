#![cfg(all(windows, feature = "backend-windows"))]
//! The ABI 0.10 D3D11 consumer calls against a real D3D11 producer served over
//! a Local Endpoint pipe: describe, adapter refusal, attach with the caller's
//! device, borrowed handles imported and read behind a GPU wait, release fence
//! registration and deferred release, reconfiguration, fence liveness, and
//! cancellation. The C translation unit's D3D11 declarations are linked too;
//! `acquisition_ffi` checks from C that a CPU frame has no D3D11 resources.

#[path = "support/local.rs"]
mod local_support;

use std::{
    ffi::CStr,
    ptr,
    sync::{Arc, Mutex},
    time::Duration,
};

use jackstay::{
    acquisition::arena::{ArenaConfig, FrameDescriptor, ReconfigurationStatus},
    ffi::*,
    ffi_acquisition::{
        FT_ACQUIRE_LATEST, FtAcquiredFrame, FtAcquisitionConsumer, FtAcquisitionRange, FtAcquisitionReleaseTimeline,
        d3d11::{
            FtD3d11AcquisitionConnection, FtD3d11Adapter, ft_acquired_frame_d3d11_resources, ft_acquisition_d3d11_attach,
            ft_acquisition_d3d11_connection_alive, ft_acquisition_d3d11_connection_cancel, ft_acquisition_d3d11_connection_create_local,
            ft_acquisition_d3d11_connection_destroy, ft_acquisition_d3d11_describe, ft_acquisition_d3d11_install_configuration,
            ft_acquisition_d3d11_register_release, ft_d3d11_fence_alive,
        },
        ft_acquired_frame_defer_release, ft_acquired_frame_describe, ft_acquired_frame_release, ft_acquisition_acquire,
        ft_acquisition_consumer_destroy, ft_acquisition_release_timeline_destroy, ft_acquisition_relinquish_configuration,
    },
    model::{ClockDomain, ColorSpace, PixelFormat},
    native::{
        NativeStreamParams,
        arena::NativeArenaProducer,
        windows::{
            AdapterSelection, D3d11CapturedFrame, D3d11Device, D3d11Fence, D3d11FrameBackend, adapters,
            setup::{D3d11Producer, serve_d3d11},
        },
    },
};
use windows::{
    Win32::{
        Foundation::HANDLE,
        Graphics::Direct3D11::{ID3D11Device1, ID3D11Fence, ID3D11Texture2D},
    },
    core::Interface,
};

const TIMEOUT: Duration = Duration::from_secs(5);

unsafe extern "C" {
    fn jackstay_c_d3d11_smoke() -> i32;
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

fn pattern(width: u32, height: u32, seed: u8) -> Vec<u8> {
    (0..width * height * 4)
        .map(|index| (index as u8).wrapping_mul(7).wrapping_add(seed))
        .collect()
}

fn publish(device: &D3d11Device, producer: &D3d11Producer, size: (u32, u32), seed: u8) {
    let mut producer = producer.lock().unwrap();
    if (producer.params().width, producer.params().height) != size {
        assert!(matches!(
            producer.reconfigure(params(size.0, size.1)).unwrap(),
            ReconfigurationStatus::Ready { .. }
        ));
    }
    let texture = device
        .upload(size.0, size.1, PixelFormat::Bgra8Unorm, &pattern(size.0, size.1, seed))
        .unwrap();
    producer.publish(&D3d11CapturedFrame::new(texture), 0).unwrap();
}

fn acquire(consumer: *mut FtAcquisitionConsumer) -> (FtStatus, *mut FtAcquiredFrame) {
    let mut frame = ptr::null_mut();
    let mut range = FtAcquisitionRange::default();
    // SAFETY: a live consumer and local outputs.
    let status = unsafe { ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut frame, &mut range) };
    (status, frame)
}

/// Import a held frame's borrowed handles on `device` and read it back behind
/// a GPU wait on its readiness value, as a C renderer would.
fn sample(device: &D3d11Device, frame: *mut FtAcquiredFrame) -> (FrameDescriptor, Vec<u8>) {
    let mut descriptor = FrameDescriptor::default();
    let mut texture = ptr::null_mut();
    let mut readiness = ptr::null_mut();
    // SAFETY: the frame is live and held; outputs are locals.
    unsafe {
        assert_eq!(ft_acquired_frame_describe(frame, &mut descriptor), FT_STATUS_OK);
        assert_eq!(ft_acquired_frame_d3d11_resources(frame, &mut texture, &mut readiness), FT_STATUS_OK);
    }
    assert!(!texture.is_null() && !readiness.is_null());
    let device1: ID3D11Device1 = device.raw().cast().unwrap();
    // SAFETY: borrowed NT handles of a held frame, imported on its adapter.
    let texture: ID3D11Texture2D = unsafe { device1.OpenSharedResource1(HANDLE(texture)) }.unwrap();
    let mut fence: Option<ID3D11Fence> = None;
    // SAFETY: as above, for the readiness fence.
    unsafe { device.raw().OpenSharedFence(HANDLE(readiness), &mut fence) }.unwrap();
    let fence = fence.unwrap();
    // SAFETY: a live fence borrowed for the call.
    assert_eq!(unsafe { ft_d3d11_fence_alive(fence.as_raw()) }, FT_STATUS_OK);
    let ready = D3d11Fence::from_raw(fence);
    let pixels = device.read_pixels(&texture, &[(&ready, descriptor.fence_value)], TIMEOUT).unwrap();
    (descriptor, pixels)
}

#[test]
fn a_c_consumer_imports_d3d11_frames_through_the_abi() {
    // SAFETY: the C translation unit only takes addresses and sizes.
    assert!(unsafe { jackstay_c_d3d11_smoke() } > 0);

    let device =
        Arc::new(D3d11Device::new(AdapterSelection::Default).unwrap_or_else(|_| D3d11Device::new(AdapterSelection::Warp).unwrap()));
    let producer: D3d11Producer = Arc::new(Mutex::new(
        NativeArenaProducer::new(
            D3d11FrameBackend::new(Arc::clone(&device)),
            params(32, 16),
            ArenaConfig {
                resource_capacity: 6,
                retained_history: 2,
                producer_reserve: 1,
                payload_capacity: 0,
                memory_budget: 64 * 1024 * 1024,
                max_incarnations: 2,
                drain_timeout: TIMEOUT,
            },
        )
        .unwrap(),
    ));
    let (accepted, connected) = local_support::pair();
    let served = Arc::clone(&producer);
    let server = std::thread::spawn(move || serve_d3d11(accepted.into_stream(), served));

    let mut local = local_support::to_c(connected);
    let mut connection: *mut FtD3d11AcquisitionConnection = ptr::null_mut();
    let mut consumer: *mut FtAcquisitionConsumer = ptr::null_mut();
    let mut timeline: *mut FtAcquisitionReleaseTimeline = ptr::null_mut();
    // SAFETY: every handle below comes from this API and is used as documented.
    unsafe {
        assert_eq!(
            ft_acquisition_d3d11_connection_create_local(&mut local, &mut connection),
            FT_STATUS_OK
        );
        assert!(local.is_null(), "the local connection is consumed");
        assert_eq!(ft_acquisition_d3d11_connection_alive(connection), FT_STATUS_OK);

        let mut adapter = FtD3d11Adapter::default();
        assert_eq!(ft_acquisition_d3d11_describe(connection, &mut adapter), FT_STATUS_OK);
        assert_eq!(adapter.luid, device.luid().0);
        let description = CStr::from_ptr(adapter.description.as_ptr()).to_str().unwrap();
        assert_eq!(description, device.adapter().description);

        // A device on another adapter is refused before admission, and the
        // connection stays usable.
        let other = adapters()
            .unwrap()
            .into_iter()
            .filter(|info| info.luid != device.luid())
            .find_map(|info| D3d11Device::new(AdapterSelection::Luid(info.luid)).ok());
        match &other {
            Some(other) => {
                assert_eq!(
                    ft_acquisition_d3d11_attach(connection, other.raw().as_raw(), 1, &mut consumer),
                    FT_STATUS_ADAPTER_MISMATCH
                );
                assert!(consumer.is_null());
            }
            None => eprintln!("only one adapter: adapter refusal not exercised"),
        }
        assert_eq!(
            ft_acquisition_d3d11_attach(connection, ptr::null_mut(), 1, &mut consumer),
            FT_STATUS_INVALID_ARGUMENT
        );

        let reader = D3d11Device::new(AdapterSelection::Luid(device.luid())).unwrap();
        assert_eq!(
            ft_acquisition_d3d11_attach(connection, reader.raw().as_raw(), 2, &mut consumer),
            FT_STATUS_OK
        );
        assert!(!consumer.is_null());

        let release = reader.create_shared_fence().unwrap();
        assert_eq!(
            ft_acquisition_d3d11_register_release(connection, consumer, release.raw().as_raw(), &mut timeline),
            FT_STATUS_OK
        );
        let unshared = {
            let mut fence: Option<ID3D11Fence> = None;
            reader
                .raw()
                .CreateFence(0, windows::Win32::Graphics::Direct3D11::D3D11_FENCE_FLAG_NONE, &mut fence)
                .unwrap();
            fence.unwrap()
        };
        let mut refused = ptr::null_mut();
        assert_eq!(
            ft_acquisition_d3d11_register_release(connection, consumer, unshared.as_raw(), &mut refused),
            FT_STATUS_ERROR,
            "a release fence must be shared"
        );
        assert!(refused.is_null());

        // A frame: import the borrowed handles, read behind the GPU wait,
        // signal the release fence after that work and defer the release.
        publish(&device, &producer, (32, 16), 3);
        let (status, mut frame) = acquire(consumer);
        assert_eq!(status, FT_STATUS_OK);
        let (descriptor, pixels) = sample(&reader, frame);
        assert_eq!((descriptor.width, descriptor.height), (32, 16));
        assert_eq!(pixels, pattern(32, 16, 3));
        release.signal_gpu(&reader, 1).unwrap();
        assert_eq!(ft_acquired_frame_defer_release(&mut frame, timeline, 1), FT_STATUS_OK);
        assert!(frame.is_null());

        // Reconfiguration: a held frame keeps its pool while the consumer
        // installs the replacement.
        publish(&device, &producer, (32, 16), 4);
        let (status, mut held) = acquire(consumer);
        assert_eq!(status, FT_STATUS_OK);
        publish(&device, &producer, (48, 24), 5);
        let (status, none) = acquire(consumer);
        assert_eq!(status, FT_STATUS_RECONFIGURATION);
        assert!(none.is_null());
        assert_eq!(ft_acquisition_relinquish_configuration(consumer), FT_STATUS_OK);
        assert_eq!(ft_acquisition_d3d11_install_configuration(connection, consumer), FT_STATUS_OK);
        assert_eq!(ft_acquisition_d3d11_install_configuration(connection, consumer), FT_STATUS_EMPTY);
        publish(&device, &producer, (48, 24), 6);
        let (status, mut resized) = acquire(consumer);
        assert_eq!(status, FT_STATUS_OK);
        let (descriptor, pixels) = sample(&reader, resized);
        assert_eq!((descriptor.width, descriptor.height), (48, 24));
        assert_eq!(pixels, pattern(48, 24, 6));
        let (old, pixels) = sample(&reader, held);
        assert_eq!((old.width, old.height), (32, 16));
        assert_eq!(pixels, pattern(32, 16, 4), "the held frame keeps its original pool");
        // readbacks finished on the CPU: immediate release is truthful here.
        assert_eq!(ft_acquired_frame_release(&mut held), FT_STATUS_OK);
        assert_eq!(ft_acquired_frame_release(&mut resized), FT_STATUS_OK);

        // Cancellation interrupts setup; liveness reports it.
        ft_acquisition_d3d11_connection_cancel(connection);
        assert_eq!(ft_acquisition_d3d11_connection_alive(connection), FT_STATUS_CANCELLED);
        assert_eq!(ft_acquisition_d3d11_describe(connection, &mut adapter), FT_STATUS_CANCELLED);

        ft_acquisition_release_timeline_destroy(&mut timeline);
        ft_acquisition_consumer_destroy(&mut consumer);
        ft_acquisition_d3d11_connection_destroy(&mut connection);
        assert!(connection.is_null());
    }
    let _ = server.join().unwrap();
    // The deferred release completed (the release fence reached 1), so the
    // producer can drain and stop.
    let mut producer = producer.lock().unwrap();
    producer.stop();
    let deadline = std::time::Instant::now() + TIMEOUT;
    while !producer.poll_shutdown_ready().unwrap() {
        assert!(std::time::Instant::now() < deadline, "producer did not drain");
        std::thread::sleep(Duration::from_millis(10));
    }
}
