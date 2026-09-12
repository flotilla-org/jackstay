#![cfg(unix)]

use std::ptr;

use jackstay::{
    acquisition::arena::FrameDescriptor,
    ffi::*,
    ffi_acquisition::{producer::*, *},
};

fn config(payload_capacity: u64, memory_budget: u64) -> FtCpuProducerConfig {
    FtCpuProducerConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        max_incarnations: 2,
        payload_capacity,
        memory_budget,
        drain_timeout_ns: 5_000_000_000,
    }
}

unsafe fn publish(producer: *mut FtCpuProducer, bytes: &[u8]) -> FtStatus {
    let descriptor = FrameDescriptor {
        width: (bytes.len() / 4) as u32,
        height: 1,
        stride: bytes.len() as u32,
        pixel_format: FT_PIXEL_FORMAT_BGRA8_UNORM,
        ..Default::default()
    };
    let mut cursor = 0;
    // SAFETY: caller owns producer; descriptor and byte/output buffers are live.
    unsafe { ft_cpu_producer_publish(producer, &descriptor, bytes.as_ptr(), bytes.len(), &mut cursor) }
}

#[test]
fn c_producer_reserves_duplicate_holds_and_refuses_destruction_until_frames_retire() {
    // SAFETY: exclusive handles and valid disjoint buffers; each owner is
    // destroyed once, and byte borrows stay within the held frame lifetime.
    unsafe {
        let mut producer = ptr::null_mut();
        let mut consumer = ptr::null_mut();
        assert_eq!(ft_cpu_producer_create(&config(4, 1024 * 1024), &mut producer), FT_STATUS_OK);
        assert_eq!(ft_cpu_producer_attach(producer, 4, &mut consumer), FT_STATUS_CAPACITY);
        assert!(consumer.is_null());
        assert_eq!(ft_cpu_producer_attach(producer, 2, &mut consumer), FT_STATUS_OK);
        assert_eq!(publish(producer, b"held"), FT_STATUS_OK);
        let mut first = ptr::null_mut();
        let mut second = ptr::null_mut();
        let mut spare = ptr::null_mut();
        let mut range = FtAcquisitionRange::default();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut first, &mut range),
            FT_STATUS_OK
        );
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut second, &mut range),
            FT_STATUS_OK
        );
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut spare, &mut range),
            FT_STATUS_HOLDING_LIMIT
        );
        for _ in 0..100 {
            assert_eq!(publish(producer, b"next"), FT_STATUS_OK);
        }
        assert_eq!(ft_acquired_frame_release(&mut first), FT_STATUS_OK);
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut spare, &mut range),
            FT_STATUS_OK
        );
        assert_eq!(ft_cpu_producer_destroy(&mut producer), FT_STATUS_DRAINING);
        assert!(!producer.is_null());
        let mut rejected = ptr::null_mut();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut rejected, &mut range),
            FT_STATUS_CLOSED
        );
        assert_eq!(publish(producer, b"late"), FT_STATUS_CLOSED);
        ft_acquisition_consumer_destroy(&mut consumer);
        let mut bytes = ptr::null();
        let mut len = 0;
        assert_eq!(ft_acquired_frame_bytes(second, &mut bytes, &mut len), FT_STATUS_OK);
        assert_eq!(std::slice::from_raw_parts(bytes, len), b"held");
        assert_eq!(ft_acquired_frame_release(&mut spare), FT_STATUS_OK);
        assert_eq!(ft_cpu_producer_destroy(&mut producer), FT_STATUS_DRAINING);
        assert_eq!(ft_acquired_frame_release(&mut second), FT_STATUS_OK);
        assert_eq!(ft_cpu_producer_destroy(&mut producer), FT_STATUS_OK);
        assert!(producer.is_null());
        assert_eq!(ft_cpu_producer_destroy(&mut producer), FT_STATUS_OK);
    }
}

#[test]
fn c_producer_reports_a_capacity_pause_and_resumes_after_the_old_frame_retires() {
    // SAFETY: exclusively owned matching handles, no concurrent operations.
    unsafe {
        let mut producer = ptr::null_mut();
        let mut consumer = ptr::null_mut();
        assert_eq!(ft_cpu_producer_create(&config(32 * 1024, 512 * 1024), &mut producer), FT_STATUS_OK);
        assert_eq!(ft_cpu_producer_attach(producer, 1, &mut consumer), FT_STATUS_OK);
        assert_eq!(publish(producer, b"held"), FT_STATUS_OK);
        let mut old = ptr::null_mut();
        let mut range = FtAcquisitionRange::default();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut old, &mut range),
            FT_STATUS_OK
        );
        let mut transition = FtCpuReconfiguration::default();
        assert_eq!(
            ft_cpu_producer_reconfigure(producer, 64 * 1024, &mut transition),
            FT_STATUS_PAUSED_CAPACITY
        );
        assert!(transition.requested_bytes > transition.available_bytes);
        assert_eq!(publish(producer, b"drop"), FT_STATUS_DROPPED);
        assert_eq!(ft_acquisition_relinquish_configuration(consumer), FT_STATUS_OK);
        assert_eq!(ft_cpu_producer_configure_consumer(producer, consumer), FT_STATUS_EMPTY);
        assert_eq!(ft_cpu_producer_advance(producer, &mut transition), FT_STATUS_PAUSED_CAPACITY);
        assert_eq!(ft_acquired_frame_release(&mut old), FT_STATUS_OK);
        assert_eq!(ft_cpu_producer_advance(producer, &mut transition), FT_STATUS_OK);
        assert!(transition.generation > 1);
        assert_eq!(ft_cpu_producer_configure_consumer(producer, consumer), FT_STATUS_OK);
        assert_eq!(ft_cpu_producer_configure_consumer(producer, consumer), FT_STATUS_EMPTY);
        assert_eq!(publish(producer, b"new size"), FT_STATUS_OK);
        let mut frame = ptr::null_mut();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut frame, &mut range),
            FT_STATUS_OK
        );
        let mut descriptor = FrameDescriptor::default();
        assert_eq!(ft_acquired_frame_describe(frame, &mut descriptor), FT_STATUS_OK);
        assert_eq!(descriptor.config_generation, transition.generation);
        assert_eq!(descriptor.sync_kind, FT_FRAME_SYNC_CPU_COPY_COMPLETE);
        assert_eq!(ft_acquired_frame_release(&mut frame), FT_STATUS_OK);
        ft_acquisition_consumer_destroy(&mut consumer);
        assert_eq!(ft_cpu_producer_destroy(&mut producer), FT_STATUS_OK);
    }
}

#[test]
fn a_foreign_current_configuration_is_rejected_even_if_incarnation_numbers_match() {
    // SAFETY: valid exclusive handles; cross-producer association is checked
    // by the API and must return an error before importing an offer.
    unsafe {
        let mut first = ptr::null_mut();
        let mut second = ptr::null_mut();
        let mut consumer = ptr::null_mut();
        let mut other = ptr::null_mut();
        assert_eq!(ft_cpu_producer_create(&config(4, 1024 * 1024), &mut first), FT_STATUS_OK);
        assert_eq!(ft_cpu_producer_create(&config(4, 1024 * 1024), &mut second), FT_STATUS_OK);
        assert_eq!(ft_cpu_producer_attach(second, 1, &mut consumer), FT_STATUS_OK);
        assert_eq!(ft_cpu_producer_attach(first, 1, &mut other), FT_STATUS_OK);
        assert_eq!(ft_cpu_producer_configure_consumer(first, consumer), FT_STATUS_INVALID_ARGUMENT);
        ft_acquisition_consumer_destroy(&mut consumer);
        ft_acquisition_consumer_destroy(&mut other);
        assert_eq!(ft_cpu_producer_destroy(&mut first), FT_STATUS_OK);
        assert_eq!(ft_cpu_producer_destroy(&mut second), FT_STATUS_OK);
    }
}

#[test]
fn a_drain_timeout_keeps_the_owner_and_storage_until_actual_release() {
    // SAFETY: valid exclusive handles and disjoint outputs; no GPU or other
    // asynchronous access is submitted, so dropping the held frame ends use.
    unsafe {
        let mut producer = ptr::null_mut();
        let mut consumer = ptr::null_mut();
        let mut limits = config(4, 1024 * 1024);
        limits.drain_timeout_ns = 1;
        assert_eq!(ft_cpu_producer_create(&limits, &mut producer), FT_STATUS_OK);
        assert_eq!(ft_cpu_producer_attach(producer, 1, &mut consumer), FT_STATUS_OK);
        assert_eq!(publish(producer, b"held"), FT_STATUS_OK);
        let mut frame = ptr::null_mut();
        let mut range = FtAcquisitionRange::default();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut frame, &mut range),
            FT_STATUS_OK
        );
        ft_acquisition_consumer_destroy(&mut consumer);
        assert!(matches!(
            ft_cpu_producer_destroy(&mut producer),
            FT_STATUS_DRAINING | FT_STATUS_RECOVERY_REQUIRED
        ));
        std::thread::sleep(std::time::Duration::from_millis(1));
        assert_eq!(ft_cpu_producer_destroy(&mut producer), FT_STATUS_RECOVERY_REQUIRED);
        assert!(!producer.is_null());
        let mut bytes = ptr::null();
        let mut len = 0;
        assert_eq!(ft_acquired_frame_bytes(frame, &mut bytes, &mut len), FT_STATUS_OK);
        assert_eq!(std::slice::from_raw_parts(bytes, len), b"held");
        assert_eq!(ft_acquired_frame_release(&mut frame), FT_STATUS_OK);
        assert_eq!(ft_cpu_producer_destroy(&mut producer), FT_STATUS_OK);
    }
}
