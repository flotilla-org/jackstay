#![cfg(unix)]

use std::sync::{Arc, Mutex, atomic::Ordering::SeqCst};

use jackstay::{
    Result,
    acquisition::arena::{
        AcquireOutcome, ArenaConfig, ArenaConsumer, ArenaProducer, FrameDescriptor, ReleaseNotification, ReleaseTimeline,
    },
};

#[derive(Debug, Default)]
struct ControlledTimeline(Mutex<(u64, Vec<(u64, ReleaseNotification)>)>);
impl ControlledTimeline {
    fn signal(&self, value: u64) {
        let mut state = self.0.lock().unwrap();
        state.0 = value;
        state.1.retain(|(target, notification)| {
            if *target <= value {
                notification.notify().unwrap();
                false
            } else {
                true
            }
        });
    }
}
impl ReleaseTimeline for ControlledTimeline {
    fn completed_value(&self) -> Result<u64> {
        Ok(self.0.lock().unwrap().0)
    }
    fn notify_at(&self, value: u64, notification: ReleaseNotification) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        if state.0 >= value {
            notification.notify().unwrap();
        } else {
            state.1.push((value, notification));
        }
        Ok(())
    }
}

fn producer(max_incarnations: u32) -> ArenaProducer {
    ArenaProducer::new(ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 4,
        memory_budget: 1024 * 1024,
        max_incarnations,
    })
    .unwrap()
}

#[test]
fn deferred_release_keeps_storage_and_credit_until_the_registered_timeline_completes() {
    let mut producer = producer(2);
    let consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let timeline = Arc::new(ControlledTimeline::default());
    let registration = producer
        .register_release_timeline(consumer.incarnation(), timeline.clone())
        .unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    let address = frame.bytes().as_ptr();
    let before = consumer.events();
    frame.defer_release(&registration, 5).unwrap();
    for _ in 0..50 {
        producer.publish(FrameDescriptor::default(), b"wxyz").unwrap();
    }
    // SAFETY: this consumer still owns the mapping, publication has finished,
    // and the declared asynchronous use is unresolved. The raw reader models
    // an external user retaining the address under the deferred lease.
    assert_eq!(unsafe { std::slice::from_raw_parts(address, 4) }, b"abcd");
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));
    assert_eq!(consumer.events().capacity_epoch, before.capacity_epoch);
    timeline.signal(4);
    assert_eq!(producer.poll_cleanup().unwrap(), 0);
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));
    timeline.signal(5);
    assert_eq!(producer.poll_cleanup().unwrap(), 1);
    assert_eq!(consumer.events().capacity_epoch, before.capacity_epoch + 1);
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Frame(_)));
    assert_eq!(producer.poll_cleanup().unwrap(), 0);
}

#[test]
fn consumer_shutdown_keeps_pending_gpu_reservations_until_completion_then_allows_restart() {
    let mut producer = producer(1);
    let consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let old_id = consumer.incarnation();
    let timeline = Arc::new(ControlledTimeline::default());
    let registration = producer.register_release_timeline(old_id, timeline.clone()).unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    frame.defer_release(&registration, 9).unwrap();
    producer.close(old_id).unwrap();
    drop(consumer);
    assert_eq!(producer.poll_cleanup().unwrap(), 0);
    assert!(producer.attach(1).is_err());
    timeline.signal(9);
    assert_eq!(producer.poll_cleanup().unwrap(), 1);
    let restarted = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    assert_ne!(old_id, restarted.incarnation());
}

#[test]
fn a_registration_from_another_arena_cannot_release_a_matching_local_incarnation_id() {
    let mut first = producer(1);
    let mut second = producer(1);
    let first_consumer = ArenaConsumer::from_grant(first.attach(1).unwrap()).unwrap();
    let second_consumer = ArenaConsumer::from_grant(second.attach(1).unwrap()).unwrap();
    let local = first
        .register_release_timeline(first_consumer.incarnation(), Arc::new(ControlledTimeline::default()))
        .unwrap();
    let foreign = second
        .register_release_timeline(second_consumer.incarnation(), Arc::new(ControlledTimeline::default()))
        .unwrap();
    first.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(frame) = first_consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    let rejected = frame.defer_release(&foreign, 1).unwrap_err();
    assert_eq!(rejected.frame.bytes(), b"abcd");
    assert!(matches!(first_consumer.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));
    rejected.frame.defer_release(&local, 1).unwrap();
    assert_eq!(first.poll_cleanup().unwrap(), 0);
}

#[test]
fn a_failed_release_observer_quarantines_its_incarnation_without_stalling_a_healthy_one() {
    use std::sync::atomic::AtomicBool;
    #[derive(Debug)]
    struct FailingTimeline {
        failed: AtomicBool,
        timeline: ControlledTimeline,
    }
    impl ReleaseTimeline for FailingTimeline {
        fn completed_value(&self) -> Result<u64> {
            if self.failed.load(SeqCst) {
                Err(jackstay::CaptureTransferError::NativeBackend {
                    operation: "test-release",
                    message: "event unavailable".to_owned(),
                })
            } else {
                self.timeline.completed_value()
            }
        }
        fn notify_at(&self, value: u64, notification: ReleaseNotification) -> Result<()> {
            self.timeline.notify_at(value, notification)
        }
    }
    let mut producer = producer(2);
    let failed = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let healthy = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let source = Arc::new(FailingTimeline {
        failed: AtomicBool::new(true),
        timeline: ControlledTimeline::default(),
    });
    let bad_reg = producer.register_release_timeline(failed.incarnation(), source.clone()).unwrap();
    let good_source = Arc::new(ControlledTimeline::default());
    let good_reg = producer
        .register_release_timeline(healthy.incarnation(), good_source.clone())
        .unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(bad_frame) = failed.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    let AcquireOutcome::Frame(good_frame) = healthy.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    bad_frame.defer_release(&bad_reg, 1).unwrap();
    good_frame.defer_release(&good_reg, 1).unwrap();
    good_source.signal(1);
    assert_eq!(producer.poll_cleanup().unwrap(), 1);
    let failures = producer.cleanup_failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].incarnation, failed.incarnation());
    assert!(failures[0].reason.contains("event unavailable"));
    assert!(matches!(failed.acquire_latest(0).unwrap(), AcquireOutcome::Closed));
    producer.publish(FrameDescriptor::default(), b"wxyz").unwrap();
    assert!(matches!(healthy.acquire_latest(0).unwrap(), AcquireOutcome::Frame(_)));
    source.failed.store(false, SeqCst);
    source.timeline.signal(1);
    // Recovery is explicit; reaching a timeout or catching an error never clears
    // the claim. Retrying observes actual completion through the retained source.
    producer.retry_cleanup(failed.incarnation()).unwrap();
    assert_eq!(producer.poll_cleanup().unwrap(), 1);
    assert!(producer.cleanup_failures().is_empty());
}

#[test]
fn deferred_completion_wakes_a_capacity_wait_without_more_producer_calls() {
    use std::time::Duration;

    use jackstay::acquisition::arena::{Cancellation, WaitInterest, WaitOutcome};
    let mut producer = producer(1);
    let mut consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let timeline = Arc::new(ControlledTimeline::default());
    let registration = producer
        .register_release_timeline(consumer.incarnation(), timeline.clone())
        .unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    let observed = consumer.events();
    frame.defer_release(&registration, 5).unwrap();
    timeline.signal(5);
    assert!(
        matches!(consumer.wait(observed, WaitInterest::CAPACITY, &Cancellation::new().unwrap(), Some(Duration::from_secs(1))).unwrap(), WaitOutcome::Changed(events) if events.capacity_epoch == observed.capacity_epoch + 1)
    );
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Frame(_)));
}

#[test]
fn an_imported_grant_hands_off_release_and_wakes_without_a_frame_broker() {
    use std::{
        io::{Read, Write},
        os::{fd::AsRawFd, unix::net::UnixListener},
        process::Command,
        time::Duration,
    };

    use jackstay::fdpass;
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("release.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut producer = producer(1);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "mapped_release_child", "--nocapture"])
        .env("JACKSTAY_RELEASE_TEST_SOCKET", &socket)
        .spawn()
        .unwrap();
    let grant = producer.attach_process(1, child.id()).unwrap();
    let old = grant.incarnation();
    let timeline = Arc::new(ControlledTimeline::default());
    let registration = producer.register_release_timeline(old, timeline.clone()).unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let (descriptor, fds) = grant.into_parts().unwrap();
    let (mut stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let json = serde_json::to_vec(&(descriptor, registration)).unwrap();
    stream.write_all(&(json.len() as u32).to_le_bytes()).unwrap();
    stream.write_all(&json).unwrap();
    fdpass::send_fds(&stream, &fds.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>()).unwrap();
    let mut ready = [0];
    stream.read_exact(&mut ready).unwrap();
    assert_eq!(ready, [1]);
    // Barrier only: no frame acquire/release or metadata messages after setup.
    // Completion alone must wake the child, without a producer call here.
    timeline.signal(5);
    assert!(child.wait().unwrap().success());
    assert_ne!(producer.attach(1).unwrap().incarnation(), old);
}

#[test]
#[ignore = "subprocess helper invoked by an_imported_grant_hands_off_release_and_wakes_without_a_frame_broker"]
fn mapped_release_child() {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        time::Duration,
    };

    use jackstay::{
        acquisition::arena::{Cancellation, ConsumerGrant, GrantDescriptor, ReleaseTimelineRegistration, WaitInterest, WaitOutcome},
        fdpass,
    };
    let mut stream = UnixStream::connect(std::env::var("JACKSTAY_RELEASE_TEST_SOCKET").unwrap()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut len = [0; 4];
    stream.read_exact(&mut len).unwrap();
    let mut json = vec![0; u32::from_le_bytes(len) as usize];
    stream.read_exact(&mut json).unwrap();
    let (descriptor, registration): (GrantDescriptor, ReleaseTimelineRegistration) = serde_json::from_slice(&json).unwrap();
    let fds = fdpass::recv_fds(&stream, 4).unwrap().try_into().unwrap();
    // SAFETY: the parent is the sole conforming producer and this process is
    // the sole recipient. It does not fork or pass on these mapped claims.
    let grant = unsafe { ConsumerGrant::from_parts(descriptor, fds) }.unwrap();
    let mut consumer = ArenaConsumer::from_grant(grant).unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    let observed = consumer.events();
    frame.defer_release(&registration, 5).unwrap();
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));
    stream.write_all(&[1]).unwrap();
    assert!(
        matches!(consumer.wait(observed, WaitInterest::CAPACITY, &Cancellation::new().unwrap(), Some(Duration::from_secs(5))).unwrap(), WaitOutcome::Changed(events) if events.capacity_epoch == observed.capacity_epoch + 1)
    );
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("credit not returned")
    };
    assert_eq!(frame.bytes(), b"abcd");
}

#[test]
fn producer_shutdown_joins_the_observer_without_waiting_for_unfinished_gpu_work() {
    use std::{sync::mpsc, time::Duration};
    let mut producer = producer(1);
    let consumer = ArenaConsumer::from_grant(producer.attach(2).unwrap()).unwrap();
    let timeline = Arc::new(ControlledTimeline::default());
    let registration = producer
        .register_release_timeline(consumer.incarnation(), timeline.clone())
        .unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(retained) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing retained frame")
    };
    let AcquireOutcome::Frame(deferred) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing deferred frame")
    };
    deferred.defer_release(&registration, 5).unwrap();
    assert_eq!(producer.poll_cleanup().unwrap(), 0);
    let (finished, wait) = mpsc::channel();
    let shutdown = std::thread::spawn(move || {
        drop(producer);
        finished.send(()).unwrap();
    });
    wait.recv_timeout(Duration::from_secs(2))
        .expect("observer shutdown waited for GPU completion");
    shutdown.join().unwrap();
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Closed));
    assert_eq!(retained.bytes(), b"abcd");
    // An already-queued backend callback may arrive after its observer stops.
    // Its wake has no ownership authority and cannot invalidate surviving leases.
    timeline.signal(5);
    assert_eq!(retained.bytes(), b"abcd");
}

#[test]
fn a_backend_notification_registration_failure_can_be_retried_without_releasing_early() {
    use std::sync::atomic::AtomicBool;
    #[derive(Debug, Default)]
    struct FailingRegistration {
        failed: AtomicBool,
        timeline: ControlledTimeline,
    }
    impl ReleaseTimeline for FailingRegistration {
        fn completed_value(&self) -> Result<u64> {
            self.timeline.completed_value()
        }
        fn notify_at(&self, value: u64, notification: ReleaseNotification) -> Result<()> {
            if self.failed.load(SeqCst) {
                Err(jackstay::CaptureTransferError::NativeBackend {
                    operation: "test-notify",
                    message: "could not register event notification".to_owned(),
                })
            } else {
                self.timeline.notify_at(value, notification)
            }
        }
    }
    let mut producer = producer(1);
    let consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let old = consumer.incarnation();
    let timeline = Arc::new(FailingRegistration::default());
    timeline.failed.store(true, SeqCst);
    let registration = producer.register_release_timeline(old, timeline.clone()).unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    frame.defer_release(&registration, 5).unwrap();
    assert_eq!(producer.poll_cleanup().unwrap(), 0);
    assert_eq!(producer.cleanup_failures().len(), 1);
    assert_eq!(consumer.events().capacity_epoch, 0);
    timeline.failed.store(false, SeqCst);
    producer.retry_cleanup(old).unwrap();
    assert_eq!(producer.poll_cleanup().unwrap(), 0);
    assert_eq!(consumer.events().capacity_epoch, 0);
    drop(consumer);
    assert!(producer.attach(1).is_err());
    timeline.timeline.signal(5);
    assert_eq!(producer.poll_cleanup().unwrap(), 1);
    assert_ne!(producer.attach(1).unwrap().incarnation(), old);
}
