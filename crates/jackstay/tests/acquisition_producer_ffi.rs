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

// Issue #75: the C reserve/commit path publishes without copy-in, preserves
// leased bytes across reuse, and abandon makes reservation capacity available.
#[test]
fn c_reservations_publish_and_preserve_live_leases() {
    use jackstay::acquisition::arena::{WriterDescriptor, WriterSlot};
    // SAFETY: serialized exclusive owners, disjoint output buffers. All views
    // finish before commit/abandon and exported views are closed before owners.
    unsafe {
        let mut producer = ptr::null_mut();
        let mut consumer = ptr::null_mut();
        assert_eq!(ft_cpu_producer_create(&config(4, 1 << 20), &mut producer), FT_STATUS_OK);
        assert_eq!(ft_cpu_producer_attach(producer, 1, &mut consumer), FT_STATUS_OK);
        assert_eq!(publish(producer, b"held"), FT_STATUS_OK);
        let mut held = ptr::null_mut();
        let mut range = FtAcquisitionRange::default();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut held, &mut range),
            FT_STATUS_OK
        );
        for value in 0..32u8 {
            let mut reservation = ptr::null_mut();
            let mut bytes = ptr::null_mut();
            let mut len = 0;
            let mut slot = WriterSlot {
                arena_scope: [0; 16],
                generation: 0,
                slot: 0,
            };
            assert_eq!(
                ft_cpu_producer_reserve(producer, &mut reservation, &mut bytes, &mut len, &mut slot),
                FT_STATUS_OK
            );
            assert_eq!(len, 4);
            if value % 3 == 0 {
                assert_eq!(ft_cpu_producer_abandon(&mut reservation), FT_STATUS_OK);
            } else {
                std::slice::from_raw_parts_mut(bytes, len).fill(value);
                let descriptor = FrameDescriptor {
                    payload_len: 4,
                    width: 1,
                    height: 1,
                    stride: 4,
                    pixel_format: FT_PIXEL_FORMAT_BGRA8_UNORM,
                    ..Default::default()
                };
                let mut cursor = 0;
                assert_eq!(ft_cpu_producer_commit(&mut reservation, &descriptor, &mut cursor), FT_STATUS_OK);
                assert!(cursor > 1);
            }
            assert!(reservation.is_null());
            let mut bytes = ptr::null();
            let mut len = 0;
            assert_eq!(ft_acquired_frame_bytes(held, &mut bytes, &mut len), FT_STATUS_OK);
            assert_eq!(std::slice::from_raw_parts(bytes, len), b"held");
        }
        let mut export = ptr::null_mut();
        let mut object = FT_OS_OBJECT_NONE;
        let mut layout = WriterDescriptor {
            arena_scope: [0; 16],
            generation: 0,
            map_len: 0,
            slot_capacity: 0,
            slots: 0,
        };
        assert_eq!(
            ft_cpu_producer_export_writer(producer, &mut export, &mut layout, &mut object),
            FT_STATUS_OK
        );
        let mut writer = ptr::null_mut();
        assert_eq!(ft_cpu_writer_import(&layout, &mut object, &mut writer), FT_STATUS_OK);
        let mut reservation = ptr::null_mut();
        let mut bytes = ptr::null_mut();
        let mut len = 0;
        let mut slot = WriterSlot {
            arena_scope: [0; 16],
            generation: 0,
            slot: 0,
        };
        assert_eq!(
            ft_cpu_producer_reserve(producer, &mut reservation, &mut bytes, &mut len, &mut slot),
            FT_STATUS_OK
        );
        let mut transition = FtCpuReconfiguration::default();
        assert_eq!(ft_cpu_producer_reconfigure(producer, 8, &mut transition), FT_STATUS_OK);
        let descriptor = FrameDescriptor {
            payload_len: 4,
            width: 1,
            height: 1,
            stride: 4,
            pixel_format: FT_PIXEL_FORMAT_BGRA8_UNORM,
            ..Default::default()
        };
        let mut cursor = 0;
        assert_eq!(
            ft_cpu_producer_commit(&mut reservation, &descriptor, &mut cursor),
            FT_STATUS_INVALID_ARGUMENT
        );
        assert!(reservation.is_null());
        assert_eq!(
            ft_cpu_producer_reserve(producer, &mut reservation, &mut bytes, &mut len, &mut slot),
            FT_STATUS_OK
        );
        assert_eq!(ft_cpu_writer_slot_view(writer, &slot, &mut bytes, &mut len), FT_STATUS_STALE);
        assert!(bytes.is_null());
        assert_eq!(len, 0);
        ft_cpu_producer_abandon(&mut reservation);
        ft_cpu_writer_destroy(&mut writer);
        ft_cpu_writer_export_destroy(&mut export);
        ft_acquired_frame_release(&mut held);
        ft_acquisition_consumer_destroy(&mut consumer);
        assert_eq!(ft_cpu_producer_destroy(&mut producer), FT_STATUS_OK);
    }
}

// Issue #75: C reserve/export return no object during a capacity pause, and
// export lifetime keeps old payload charged until every child mapping closes.
#[test]
fn c_reserve_and_export_pause_until_old_export_retires() {
    use jackstay::acquisition::arena::{WriterDescriptor, WriterSlot};
    // SAFETY: exclusive handles and disjoint outputs; duplicate object is closed
    // before its export owner, with no outstanding delegate writes or views.
    unsafe {
        let mut producer = ptr::null_mut();
        assert_eq!(ft_cpu_producer_create(&config(16, 48 * 1024), &mut producer), FT_STATUS_OK);
        let mut export = ptr::null_mut();
        let mut object = FT_OS_OBJECT_NONE;
        let mut layout = WriterDescriptor {
            arena_scope: [0; 16],
            generation: 0,
            map_len: 0,
            slot_capacity: 0,
            slots: 0,
        };
        assert_eq!(
            ft_cpu_producer_export_writer(producer, &mut export, &mut layout, &mut object),
            FT_STATUS_OK
        );
        let mut writer = ptr::null_mut();
        assert_eq!(ft_cpu_writer_import(&layout, &mut object, &mut writer), FT_STATUS_OK);
        let mut transition = FtCpuReconfiguration::default();
        // 6 * 6 KiB payload + record/control = 44 KiB; old 8 KiB makes it pause.
        assert_eq!(
            ft_cpu_producer_reconfigure(producer, 6 * 1024, &mut transition),
            FT_STATUS_PAUSED_CAPACITY
        );
        let mut reservation = ptr::null_mut();
        let mut bytes = ptr::null_mut();
        let mut len = 0;
        let mut slot = WriterSlot {
            arena_scope: [0; 16],
            generation: 0,
            slot: 0,
        };
        assert_eq!(
            ft_cpu_producer_reserve(producer, &mut reservation, &mut bytes, &mut len, &mut slot),
            FT_STATUS_DROPPED
        );
        assert!(reservation.is_null());
        assert!(bytes.is_null());
        assert_eq!(len, 0);
        let mut paused_export = ptr::null_mut();
        assert_eq!(
            ft_cpu_producer_export_writer(producer, &mut paused_export, &mut layout, &mut object),
            FT_STATUS_DROPPED
        );
        assert!(paused_export.is_null());
        assert_eq!(object, FT_OS_OBJECT_NONE);
        assert_eq!(ft_cpu_writer_destroy(&mut writer), FT_STATUS_OK);
        assert_eq!(ft_cpu_writer_export_destroy(&mut export), FT_STATUS_OK);
        assert_eq!(ft_cpu_producer_advance(producer, &mut transition), FT_STATUS_OK);
        assert_eq!(
            ft_cpu_producer_reserve(producer, &mut reservation, &mut bytes, &mut len, &mut slot),
            FT_STATUS_OK
        );
        assert_eq!(len, 6 * 1024);
        ft_cpu_producer_abandon(&mut reservation);
        assert_eq!(ft_cpu_producer_destroy(&mut producer), FT_STATUS_OK);
    }
}
