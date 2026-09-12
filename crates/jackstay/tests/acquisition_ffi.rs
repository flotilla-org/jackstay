#![cfg(unix)]

use std::{ptr, time::Duration};

use jackstay::{
    acquisition::arena::{ArenaConfig, ArenaConsumer, ArenaProducer, FrameDescriptor},
    ffi::*,
    ffi_acquisition::*,
};

#[path = "support/completion.rs"]
mod completion;

#[cfg(any(
    all(target_os = "macos", feature = "backend-macos"),
    all(target_os = "linux", feature = "backend-linux")
))]
#[test]
fn a_compiled_c_consumer_retains_and_reads_the_rust_producers_frame() {
    use std::ffi::c_void;
    unsafe extern "C" {
        // Only opaque pointers cross this boundary; C never sees Rust layouts.
        fn jackstay_c_acquisition_take(consumer: *mut c_void, out: *mut *mut c_void) -> FtStatus;
        fn jackstay_c_acquisition_finish(frame: *mut *mut c_void) -> i32;
    }
    let mut producer = producer();
    let mut consumer = FtAcquisitionConsumer::into_raw(ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap());
    producer
        .publish(
            FrameDescriptor {
                sequence: 7,
                timestamp_ns: 42,
                width: 1,
                height: 1,
                stride: 4,
                flags: 0x1234,
                ..Default::default()
            },
            b"abcd",
        )
        .unwrap();
    // SAFETY: the C helper uses the public header and takes/releases this one
    // exclusively owned frame. Its expected descriptor/bytes are set above.
    unsafe {
        let mut frame = ptr::null_mut();
        assert_eq!(jackstay_c_acquisition_take(consumer.cast(), &mut frame), FT_STATUS_OK);
        ft_acquisition_consumer_destroy(&mut consumer);
        for _ in 0..100 {
            producer.publish(FrameDescriptor::default(), b"wxyz").unwrap();
        }
        assert_eq!(jackstay_c_acquisition_finish(&mut frame), 0);
        assert!(frame.is_null());
    }
}

#[test]
fn malformed_cpu_grant_consumes_all_transferred_fds_without_returning_a_consumer() {
    use std::{
        io::Read,
        os::{fd::IntoRawFd, unix::net::UnixStream},
    };
    let (sender, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let mut fds: [i32; 5] = std::array::from_fn(|_| sender.try_clone().unwrap().into_raw_fd());
    drop(sender);
    let mut consumer = ptr::null_mut();
    // SAFETY: the malformed JSON is rejected before maps are accessed. All five
    // FDs are valid, distinct, exclusively owned and transferred exactly once.
    assert_eq!(
        unsafe { ft_acquisition_import_cpu(b"!".as_ptr(), 1, fds.as_mut_ptr(), &mut consumer) },
        FT_STATUS_INVALID_ARGUMENT
    );
    assert_eq!(fds, [-1; 5]);
    assert!(consumer.is_null());
    assert_eq!(peer.read(&mut [0]).unwrap(), 0, "one of the five transferred FDs leaked");
}

fn producer() -> ArenaProducer {
    ArenaProducer::new(ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 4,
        memory_budget: 1024 * 1024,
        max_incarnations: 2,
        drain_timeout: Duration::from_secs(5),
    })
    .unwrap()
}

fn imported_consumer(producer: &mut ArenaProducer, holding: u32) -> (jackstay::acquisition::IncarnationId, *mut FtAcquisitionConsumer) {
    use std::os::fd::IntoRawFd;
    let grant = producer.attach_process(holding, std::process::id()).unwrap();
    let incarnation = grant.incarnation();
    let (descriptor, fds) = grant.into_parts().unwrap();
    let json = serde_json::to_vec(&descriptor).unwrap();
    let mut fds = fds.map(IntoRawFd::into_raw_fd);
    let mut consumer = ptr::null_mut();
    // SAFETY: conforming producer; sole intended recipient of this single-use
    // grant. No other FD copies remain and this test does not fork mappings.
    assert_eq!(
        unsafe { ft_acquisition_import_cpu(json.as_ptr(), json.len(), fds.as_mut_ptr(), &mut consumer) },
        FT_STATUS_OK
    );
    assert_eq!(fds, [-1; 5]);
    (incarnation, consumer)
}

#[test]
fn c_replacement_distinguishes_stale_offers_and_preserves_old_frames_and_credit() {
    use std::os::fd::IntoRawFd;
    let mut producer = producer();
    let (incarnation, mut consumer) = imported_consumer(&mut producer, 2);
    producer
        .publish(
            FrameDescriptor {
                width: 1,
                ..Default::default()
            },
            b"abcd",
        )
        .unwrap();
    // SAFETY: this test is the only owner of these process-bound grants and
    // handles; old byte borrows remain within their explicitly held frame.
    unsafe {
        let mut old = ptr::null_mut();
        let mut new = ptr::null_mut();
        let mut range = FtAcquisitionRange::default();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut old, &mut range),
            FT_STATUS_OK
        );
        producer.reconfigure_cpu(8).unwrap();
        let (stale, fd) = producer.configuration_offer(incarnation).unwrap().unwrap().into_parts().unwrap();
        producer.reconfigure_cpu(12).unwrap();
        let json = serde_json::to_vec(&stale).unwrap();
        let mut fd = fd.into_raw_fd();
        assert_eq!(
            ft_acquisition_install_cpu_configuration(consumer, json.as_ptr(), json.len(), &mut fd),
            FT_STATUS_STALE
        );
        assert_eq!(fd, -1);
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut new, &mut range),
            FT_STATUS_RECONFIGURATION
        );
        let (current, fd) = producer.configuration_offer(incarnation).unwrap().unwrap().into_parts().unwrap();
        let json = serde_json::to_vec(&current).unwrap();
        let mut fd = fd.into_raw_fd();
        assert_eq!(
            ft_acquisition_install_cpu_configuration(consumer, json.as_ptr(), json.len(), &mut fd),
            FT_STATUS_OK
        );
        assert_eq!(fd, -1);
        producer
            .publish(
                FrameDescriptor {
                    width: 3,
                    ..Default::default()
                },
                b"replacement!",
            )
            .unwrap();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut new, &mut range),
            FT_STATUS_OK
        );
        let mut unavailable = ptr::null_mut();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut unavailable, &mut range),
            FT_STATUS_HOLDING_LIMIT
        );
        let mut before = FrameDescriptor::default();
        let mut after = FrameDescriptor::default();
        assert_eq!(ft_acquired_frame_describe(old, &mut before), FT_STATUS_OK);
        assert_eq!(ft_acquired_frame_describe(new, &mut after), FT_STATUS_OK);
        assert_eq!((before.width, after.width), (1, 3));
        assert_eq!(before.config_generation, 1);
        assert_eq!(after.config_generation, current.generation);
        ft_acquisition_consumer_destroy(&mut consumer);
        let mut bytes = ptr::null();
        let mut len = 0;
        assert_eq!(ft_acquired_frame_bytes(old, &mut bytes, &mut len), FT_STATUS_OK);
        assert_eq!(std::slice::from_raw_parts(bytes, len), b"abcd");
        assert_eq!(ft_acquired_frame_release(&mut old), FT_STATUS_OK);
        assert_eq!(ft_acquired_frame_bytes(new, &mut bytes, &mut len), FT_STATUS_OK);
        assert_eq!(std::slice::from_raw_parts(bytes, len), b"replacement!");
        assert_eq!(ft_acquired_frame_release(&mut new), FT_STATUS_OK);
    }
}

#[test]
fn c_relinquish_does_not_allow_an_exhausted_transition_to_reuse_a_held_frame() {
    use jackstay::acquisition::arena::ReconfigurationStatus;
    let mut producer = ArenaProducer::new(ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 32 * 1024,
        memory_budget: 512 * 1024,
        max_incarnations: 2,
        drain_timeout: Duration::from_secs(5),
    })
    .unwrap();
    let (_, mut consumer) = imported_consumer(&mut producer, 1);
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    // SAFETY: exclusive handles and no byte access after the frame is released.
    unsafe {
        let mut frame = ptr::null_mut();
        let mut range = FtAcquisitionRange::default();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut frame, &mut range),
            FT_STATUS_OK
        );
        assert!(matches!(
            producer.reconfigure_cpu(64 * 1024).unwrap(),
            ReconfigurationStatus::PausedCapacity { .. }
        ));
        assert_eq!(ft_acquisition_relinquish_configuration(consumer), FT_STATUS_OK);
        assert!(matches!(
            producer.advance_reconfiguration().unwrap(),
            ReconfigurationStatus::PausedCapacity { .. }
        ));
        let mut bytes = ptr::null();
        let mut len = 0;
        assert_eq!(ft_acquired_frame_bytes(frame, &mut bytes, &mut len), FT_STATUS_OK);
        assert_eq!(std::slice::from_raw_parts(bytes, len), b"abcd");
        assert_eq!(ft_acquired_frame_release(&mut frame), FT_STATUS_OK);
        assert!(matches!(
            producer.advance_reconfiguration().unwrap(),
            ReconfigurationStatus::Ready { .. }
        ));
        ft_acquisition_consumer_destroy(&mut consumer);
    }
}

#[test]
fn c_frames_keep_independent_credit_and_survive_consumer_destruction() {
    let mut producer = producer();
    let mut consumer = FtAcquisitionConsumer::into_raw(ArenaConsumer::from_grant(producer.attach(2).unwrap()).unwrap());
    producer
        .publish(
            FrameDescriptor {
                sequence: 7,
                ..Default::default()
            },
            b"abcd",
        )
        .unwrap();
    // SAFETY: this test owns each opaque handle, serializes consumer calls, and
    // keeps each byte borrow within its frame's lifetime.
    unsafe {
        let mut first = ptr::null_mut();
        let mut second = ptr::null_mut();
        let mut range = FtAcquisitionRange::default();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut first, &mut range),
            FT_STATUS_OK
        );
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_EXACT, 1, &mut second, &mut range),
            FT_STATUS_OK
        );
        let mut unavailable = ptr::null_mut();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut unavailable, &mut range),
            FT_STATUS_HOLDING_LIMIT
        );
        assert!(unavailable.is_null());
        assert_eq!(ft_acquired_frame_release(&mut first), FT_STATUS_OK);
        assert!(first.is_null());
        ft_acquisition_consumer_destroy(&mut consumer);
        assert!(consumer.is_null());
        for _ in 0..100 {
            producer.publish(FrameDescriptor::default(), b"wxyz").unwrap();
        }
        let mut descriptor = FrameDescriptor::default();
        let mut data = ptr::null();
        let mut len = 0;
        assert_eq!(ft_acquired_frame_describe(second, &mut descriptor), FT_STATUS_OK);
        assert_eq!(descriptor.sequence, 7);
        assert_eq!(ft_acquired_frame_bytes(second, &mut data, &mut len), FT_STATUS_OK);
        assert_eq!(std::slice::from_raw_parts(data, len), b"abcd");
        assert_eq!(ft_acquired_frame_release(&mut second), FT_STATUS_OK);
        assert_eq!(ft_acquired_frame_release(&mut second), FT_STATUS_INVALID_ARGUMENT);
        assert!(producer.attach(2).is_ok());
    }
}

#[test]
fn c_selection_preserves_misses_gaps_and_empty_results() {
    let mut producer = producer();
    use std::os::fd::IntoRawFd;
    let (grant, fds) = producer.attach_process(1, std::process::id()).unwrap().into_parts().unwrap();
    let json = serde_json::to_vec(&grant).unwrap();
    let mut fds = fds.map(IntoRawFd::into_raw_fd);
    let mut consumer = ptr::null_mut();
    // SAFETY: conforming producer, intended recipient, five uniquely owned FDs,
    // one import, and no fork or other copies of the transferred mappings.
    assert_eq!(
        unsafe { ft_acquisition_import_cpu(json.as_ptr(), json.len(), fds.as_mut_ptr(), &mut consumer) },
        FT_STATUS_OK
    );
    assert_eq!(fds, [-1; 5]);
    for _ in 0..5 {
        producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    }
    // SAFETY: all pointers refer to live, exclusively owned handles or outputs.
    unsafe {
        let mut frame = ptr::null_mut();
        let mut range = FtAcquisitionRange::default();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_NEXT, 1, &mut frame, &mut range),
            FT_STATUS_GAP
        );
        assert_eq!((range.first, range.last), (2, 3));
        assert!(frame.is_null());
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_EXACT, 2, &mut frame, &mut range),
            FT_STATUS_MISS
        );
        assert_eq!((range.first, range.last), (2, 2));
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 5, &mut frame, &mut range),
            FT_STATUS_EMPTY
        );
        assert_eq!((range.first, range.last), (0, 0));
        ft_acquisition_consumer_destroy(&mut consumer);
    }
}

#[test]
fn c_wait_observes_publication_between_snapshot_and_wait_and_cancellation() {
    let mut producer = producer();
    let mut consumer = FtAcquisitionConsumer::into_raw(ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap());
    // SAFETY: pointers remain live for the whole serialized operation.
    unsafe {
        let mut cancel = ptr::null_mut();
        assert_eq!(ft_acquisition_cancellation_create(&mut cancel), FT_STATUS_OK);
        let mut observed = FtAcquisitionEvents::default();
        assert_eq!(ft_acquisition_snapshot(consumer, &mut observed), FT_STATUS_OK);
        producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
        let mut after = FtAcquisitionEvents::default();
        assert_eq!(
            ft_acquisition_wait(consumer, &observed, FT_WAIT_DATA, cancel, 0, &mut after),
            FT_STATUS_OK
        );
        assert_eq!(after.data_cursor, 1);
        observed = after;
        assert_eq!(
            ft_acquisition_wait(consumer, &observed, FT_WAIT_DATA, cancel, 0, &mut after),
            FT_STATUS_TIMEOUT
        );
        assert_eq!(ft_acquisition_cancellation_cancel(cancel), FT_STATUS_OK);
        assert_eq!(
            ft_acquisition_wait(consumer, &observed, FT_WAIT_DATA, cancel, u64::MAX, &mut after),
            FT_STATUS_CANCELLED
        );
        ft_acquisition_cancellation_destroy(&mut cancel);
        ft_acquisition_consumer_destroy(&mut consumer);
    }
}

#[test]
fn rejected_c_deferred_release_preserves_the_handle_and_success_retains_storage_until_completion() {
    use std::{sync::Arc, time::Instant};
    let mut producer = producer();
    let consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let foreign = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let source = Arc::new(completion::SharedCompletion::new());
    let imported = Arc::new(completion::SharedCompletion::from_fd(source.export_fd()));
    let local_registration = producer.register_release_timeline(consumer.incarnation(), source.clone()).unwrap();
    let foreign_registration = producer.register_release_timeline(foreign.incarnation(), source.clone()).unwrap();
    let local = consumer.bind_release_timeline(&local_registration, imported.clone()).unwrap();
    let foreign = foreign.bind_release_timeline(&foreign_registration, imported).unwrap();
    let mut local_handle = FtAcquisitionReleaseTimeline::into_raw(local.clone());
    let mut foreign_handle = FtAcquisitionReleaseTimeline::into_raw(foreign);
    let mut consumer = FtAcquisitionConsumer::into_raw(consumer);
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    // SAFETY: all handles are exclusively owned. The borrowed bytes are read
    // only while the submitted completion is known to remain unresolved.
    unsafe {
        let mut frame = ptr::null_mut();
        let mut range = FtAcquisitionRange::default();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut frame, &mut range),
            FT_STATUS_OK
        );
        let original = frame;
        assert_eq!(ft_acquired_frame_defer_release(&mut frame, foreign_handle, 5), FT_STATUS_ERROR);
        assert_eq!(frame, original);
        let mut bytes = ptr::null();
        let mut len = 0;
        assert_eq!(ft_acquired_frame_bytes(frame, &mut bytes, &mut len), FT_STATUS_OK);
        assert_eq!(ft_acquired_frame_defer_release(&mut frame, local_handle, 5), FT_STATUS_OK);
        assert!(frame.is_null());
        let mut unavailable = ptr::null_mut();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut unavailable, &mut range),
            FT_STATUS_HOLDING_LIMIT
        );
        ft_acquisition_consumer_destroy(&mut consumer);
        ft_acquisition_release_timeline_destroy(&mut local_handle);
        ft_acquisition_release_timeline_destroy(&mut foreign_handle);
        for _ in 0..100 {
            producer.publish(FrameDescriptor::default(), b"wxyz").unwrap();
        }
        assert_eq!(std::slice::from_raw_parts(bytes, len), b"abcd");
        assert_eq!(producer.poll_cleanup().unwrap(), 0);
        source.signal(5);
        let deadline = Instant::now() + Duration::from_secs(5);
        while local.pending_releases() != 0 {
            assert!(Instant::now() < deadline, "retirement did not finish");
            std::thread::yield_now();
        }
        assert_eq!(producer.poll_cleanup().unwrap(), 1);
        assert!(producer.attach(1).is_ok());
    }
}
