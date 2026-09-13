#![cfg(unix)]

use std::sync::{Arc, Mutex};

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
        drain_timeout: std::time::Duration::from_secs(5),
    })
    .unwrap()
}

#[test]
fn deferred_use_keeps_the_consumer_address_mapped_after_both_api_owners_are_destroyed() {
    use std::time::{Duration, Instant};
    let mut producer = producer(1);
    let consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let timeline = Arc::new(ControlledTimeline::default());
    let registration = producer
        .register_release_timeline(consumer.incarnation(), timeline.clone())
        .unwrap();
    let completion = consumer.bind_release_timeline(&registration, timeline.clone()).unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    let address = frame.bytes().as_ptr();
    frame.defer_release(&completion, 5).unwrap();
    drop(consumer);
    drop(producer);
    assert_eq!(completion.pending_releases(), 1);
    // SAFETY: the declared asynchronous use has not completed. Successful
    // deferred release must retain this exact consumer mapping, independently
    // of either API owner's lifetime, until the bound completion reaches five.
    assert_eq!(unsafe { std::slice::from_raw_parts(address, 4) }, b"abcd");
    timeline.signal(4);
    assert_eq!(completion.pending_releases(), 1);
    timeline.signal(5);
    let deadline = Instant::now() + Duration::from_secs(5);
    while completion.pending_releases() != 0 {
        assert!(Instant::now() < deadline, "consumer retirement did not complete");
        std::thread::yield_now();
    }
}

#[test]
fn deferred_mapping_retirement_unblocks_an_exhausted_replacement_only_after_completion() {
    use std::time::{Duration, Instant};

    use jackstay::acquisition::arena::ReconfigurationStatus;
    let mut producer = ArenaProducer::new(ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 32 * 1024,
        memory_budget: 512 * 1024,
        max_incarnations: 1,
        drain_timeout: Duration::from_secs(5),
    })
    .unwrap();
    let mut consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let timeline = Arc::new(ControlledTimeline::default());
    let registration = producer
        .register_release_timeline(consumer.incarnation(), timeline.clone())
        .unwrap();
    let completion = consumer.bind_release_timeline(&registration, timeline.clone()).unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    let address = frame.bytes().as_ptr();
    frame.defer_release(&completion, 5).unwrap();
    assert!(matches!(
        producer.reconfigure_cpu(64 * 1024).unwrap(),
        ReconfigurationStatus::PausedCapacity { .. }
    ));
    consumer.relinquish_configuration();
    assert!(matches!(
        producer.advance_reconfiguration().unwrap(),
        ReconfigurationStatus::PausedCapacity { .. }
    ));
    // SAFETY: deferred completion is unresolved; relinquishing the current
    // configuration must leave this exact asynchronous address mapped.
    assert_eq!(unsafe { std::slice::from_raw_parts(address, 4) }, b"abcd");
    timeline.signal(5);
    let deadline = Instant::now() + Duration::from_secs(5);
    while completion.pending_releases() != 0 {
        assert!(Instant::now() < deadline, "local retirement stalled");
        std::thread::yield_now();
    }
    assert!(matches!(
        producer.advance_reconfiguration().unwrap(),
        ReconfigurationStatus::Ready { .. }
    ));
    consumer
        .install_configuration(producer.configuration_offer(consumer.incarnation()).unwrap().unwrap())
        .unwrap();
    producer.publish(FrameDescriptor::default(), b"new data").unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(1).unwrap() else {
        panic!("missing replacement")
    };
    assert_eq!(frame.bytes(), b"new data");
}

#[test]
fn a_consumer_retirement_deadline_reports_failure_without_unmapping_and_late_completion_still_finishes() {
    use std::time::{Duration, Instant};
    let mut producer = ArenaProducer::new(ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 4,
        memory_budget: 1024 * 1024,
        max_incarnations: 1,
        drain_timeout: Duration::from_millis(20),
    })
    .unwrap();
    let consumer = ArenaConsumer::from_grant(producer.attach(2).unwrap()).unwrap();
    let timeline = Arc::new(ControlledTimeline::default());
    let registration = producer
        .register_release_timeline(consumer.incarnation(), timeline.clone())
        .unwrap();
    let completion = consumer.bind_release_timeline(&registration, timeline.clone()).unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    let AcquireOutcome::Frame(ordinary) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing ordinary lease")
    };
    let address = frame.bytes().as_ptr();
    frame.defer_release(&completion, 5).unwrap();
    drop(consumer);
    drop(producer);
    let deadline = Instant::now() + Duration::from_secs(5);
    while completion.cleanup_failure().is_none() {
        assert!(Instant::now() < deadline, "missing consumer drain failure");
        std::thread::yield_now();
    }
    assert!(completion.cleanup_failure().unwrap().contains("deadline"));
    assert_eq!(ordinary.bytes(), b"abcd");
    assert!(completion.retry_cleanup().is_err());
    assert_eq!(completion.pending_releases(), 1);
    // SAFETY: expiration is not completion; the owner must retain this address.
    assert_eq!(unsafe { std::slice::from_raw_parts(address, 4) }, b"abcd");
    timeline.signal(5);
    while completion.pending_releases() != 0 {
        assert!(Instant::now() < deadline, "late completion did not retire the mapping");
        std::thread::yield_now();
    }
    assert!(completion.cleanup_failure().is_none());
}
