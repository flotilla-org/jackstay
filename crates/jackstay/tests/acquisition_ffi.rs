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
