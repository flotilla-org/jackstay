#![cfg(unix)]

use std::{thread, time::Duration};

use jackstay::acquisition::arena::{
    AcquireOutcome, ArenaConfig, ArenaConsumer, ArenaProducer, Cancellation, FrameDescriptor, WaitInterest, WaitOutcome,
};

fn producer() -> ArenaProducer {
    ArenaProducer::new(ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 4,
        memory_budget: 1024 * 1024,
        max_incarnations: 3,
        drain_timeout: std::time::Duration::from_secs(5),
    })
    .unwrap()
}

#[test]
fn wait_observes_data_published_between_the_predicate_and_sleep() {
    let mut producer = producer();
    let mut consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let cancel = Cancellation::new().unwrap();
    let observed = consumer.events();
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Empty));
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let WaitOutcome::Changed(events) = consumer.wait(observed, WaitInterest::DATA, &cancel, Some(Duration::ZERO)).unwrap() else {
        panic!("lost publication before wait")
    };
    assert_eq!(events.data_cursor, 1);
    assert!(!events.closed);
}

#[test]
fn cancellation_is_persistent_and_does_not_release_a_held_frame() {
    let mut producer = producer();
    let mut consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(held) = consumer.acquire_latest(0).unwrap() else {
        panic!("no frame")
    };
    let observed = consumer.events();
    let cancel = Cancellation::new().unwrap();
    let cancelling = cancel.clone();
    let thread = thread::spawn(move || {
        cancelling.cancel().unwrap();
    });
    assert!(matches!(
        consumer
            .wait(observed, WaitInterest::ALL, &cancel, Some(Duration::from_secs(2)))
            .unwrap(),
        WaitOutcome::Cancelled
    ));
    thread.join().unwrap();
    assert!(matches!(
        consumer.wait(observed, WaitInterest::ALL, &cancel, Some(Duration::ZERO)).unwrap(),
        WaitOutcome::Cancelled
    ));
    assert_eq!(held.bytes(), b"abcd");
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));
}

#[test]
fn a_capacity_wait_ignores_new_frames_and_observes_release_credit() {
    let mut producer = producer();
    let mut consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(held) = consumer.acquire_latest(0).unwrap() else {
        panic!("no frame")
    };
    let observed = consumer.events();
    let cancel = Cancellation::new().unwrap();
    producer.publish(FrameDescriptor::default(), b"wxyz").unwrap();
    assert!(matches!(
        consumer
            .wait(observed, WaitInterest::CAPACITY, &cancel, Some(Duration::ZERO))
            .unwrap(),
        WaitOutcome::TimedOut
    ));
    assert_eq!(held.bytes(), b"abcd");
    drop(held);
    let WaitOutcome::Changed(events) = consumer
        .wait(observed, WaitInterest::CAPACITY, &cancel, Some(Duration::ZERO))
        .unwrap()
    else {
        panic!("missed returned holding credit")
    };
    assert_eq!(events.capacity_epoch, observed.capacity_epoch + 1);
    assert_eq!(events.data_cursor, 2);
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Frame(_)));
}

#[test]
fn closure_is_reported_even_when_the_wait_interest_is_only_capacity() {
    let mut producer = producer();
    let mut consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let observed = consumer.events();
    let cancel = Cancellation::new().unwrap();
    producer.close(consumer.incarnation()).unwrap();
    let WaitOutcome::Changed(events) = consumer
        .wait(observed, WaitInterest::CAPACITY, &cancel, Some(Duration::ZERO))
        .unwrap()
    else {
        panic!("missed closure")
    };
    assert!(events.closed);
}
