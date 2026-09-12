#![cfg(unix)]

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering::SeqCst},
};

use jackstay::{
    Result,
    acquisition::arena::{AcquireOutcome, ArenaConfig, ArenaConsumer, ArenaProducer, FrameDescriptor, ReleaseTimeline},
};

#[derive(Debug, Default)]
struct ControlledTimeline(AtomicU64);
impl ReleaseTimeline for ControlledTimeline {
    fn completed_value(&self) -> Result<u64> {
        Ok(self.0.load(SeqCst))
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
    timeline.0.store(4, SeqCst);
    assert_eq!(producer.poll_release_completions().unwrap(), 0);
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));
    timeline.0.store(5, SeqCst);
    assert_eq!(producer.poll_release_completions().unwrap(), 1);
    assert_eq!(consumer.events().capacity_epoch, before.capacity_epoch + 1);
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Frame(_)));
    assert_eq!(producer.poll_release_completions().unwrap(), 0);
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
    assert_eq!(producer.poll_release_completions().unwrap(), 0);
    assert!(producer.attach(1).is_err());
    timeline.0.store(9, SeqCst);
    assert_eq!(producer.poll_release_completions().unwrap(), 1);
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
    assert_eq!(first.poll_release_completions().unwrap(), 0);
}

#[test]
fn a_failed_release_observer_quarantines_its_incarnation_without_stalling_a_healthy_one() {
    use std::sync::atomic::AtomicBool;
    #[derive(Debug)]
    struct FailingTimeline {
        failed: AtomicBool,
        value: AtomicU64,
    }
    impl ReleaseTimeline for FailingTimeline {
        fn completed_value(&self) -> Result<u64> {
            if self.failed.load(SeqCst) {
                Err(jackstay::CaptureTransferError::NativeBackend {
                    operation: "test-release",
                    message: "event unavailable".to_owned(),
                })
            } else {
                Ok(self.value.load(SeqCst))
            }
        }
    }
    let mut producer = producer(2);
    let failed = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let healthy = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let source = Arc::new(FailingTimeline {
        failed: AtomicBool::new(true),
        value: AtomicU64::new(0),
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
    good_source.0.store(1, SeqCst);
    assert_eq!(producer.poll_release_completions().unwrap(), 1);
    let failures = producer.release_recovery_failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].incarnation, failed.incarnation());
    assert!(failures[0].reason.contains("event unavailable"));
    assert!(matches!(failed.acquire_latest(0).unwrap(), AcquireOutcome::Closed));
    producer.publish(FrameDescriptor::default(), b"wxyz").unwrap();
    assert!(matches!(healthy.acquire_latest(0).unwrap(), AcquireOutcome::Frame(_)));
    source.failed.store(false, SeqCst);
    source.value.store(1, SeqCst);
    // Recovery is explicit; reaching a timeout or catching an error never clears
    // the claim. Retrying observes actual completion through the retained source.
    producer.retry_release_cleanup(failed.incarnation()).unwrap();
    assert_eq!(producer.poll_release_completions().unwrap(), 1);
    assert!(producer.release_recovery_failures().is_empty());
}
