#![cfg(unix)]

use std::{
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::net::{UnixListener, UnixStream},
    },
    process::Command,
    time::Duration,
};

use jackstay::{
    acquisition::arena::{
        AcquireOutcome, ArenaConfig, ArenaConsumer, ArenaProducer, Cancellation, ConsumerGrant, FrameDescriptor, PublishOutcome,
        WaitInterest, WaitOutcome,
    },
    fdpass,
};

fn config() -> ArenaConfig {
    ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 4,
        memory_budget: 1024 * 1024,
        max_incarnations: 4,
        drain_timeout: std::time::Duration::from_secs(5),
    }
}

#[test]
fn a_mapped_cpu_lease_keeps_its_descriptor_and_bytes_after_many_ring_wraps() {
    let mut producer = ArenaProducer::new(config()).unwrap();
    let consumer = ArenaConsumer::from_grant(producer.attach(2).unwrap()).unwrap();
    let descriptor = FrameDescriptor {
        sequence: 7,
        width: 1,
        height: 1,
        stride: 4,
        ..FrameDescriptor::default()
    };
    assert_eq!(
        producer.publish(descriptor, b"abcd").unwrap(),
        PublishOutcome::Published { cursor: 1 }
    );
    let AcquireOutcome::Frame(held) = consumer.acquire_latest(0).unwrap() else {
        panic!("first frame missing")
    };
    assert_eq!(held.bytes(), b"abcd");
    for sequence in 8..108 {
        assert!(matches!(
            producer.publish(FrameDescriptor { sequence, ..descriptor }, b"wxyz").unwrap(),
            PublishOutcome::Published { .. }
        ));
        assert_eq!(held.bytes(), b"abcd");
        assert_eq!(held.descriptor().sequence, 7);
    }
    let AcquireOutcome::Frame(latest) = consumer.acquire_latest(held.cursor()).unwrap() else {
        panic!("latest frame missing")
    };
    assert_eq!(latest.bytes(), b"wxyz");
    assert_eq!(latest.descriptor().sequence, 107);
}

#[test]
fn duplicate_and_overlapping_leases_keep_independent_holding_credit() {
    let mut producer = ArenaProducer::new(config()).unwrap();
    let first = ArenaConsumer::from_grant(producer.attach(2).unwrap()).unwrap();
    let second = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(a) = first.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    let AcquireOutcome::Frame(b) = first.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    let AcquireOutcome::Frame(c) = second.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    assert!(matches!(first.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));
    assert!(matches!(second.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));
    drop(a);
    for _ in 0..100 {
        assert!(matches!(
            producer.publish(FrameDescriptor::default(), b"wxyz").unwrap(),
            PublishOutcome::Published { .. }
        ));
    }
    assert_eq!(b.bytes(), b"abcd");
    assert_eq!(c.bytes(), b"abcd");
    let AcquireOutcome::Frame(new) = first.acquire_latest(0).unwrap() else {
        panic!("released credit unavailable")
    };
    assert_eq!(new.bytes(), b"wxyz");
    assert!(matches!(first.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));
}

#[test]
fn closure_stops_acquisition_but_old_leases_keep_capacity_until_their_owner_finishes() {
    let mut settings = config();
    settings.max_incarnations = 1;
    let mut producer = ArenaProducer::new(settings).unwrap();
    let consumer = ArenaConsumer::from_grant(producer.attach(2).unwrap()).unwrap();
    let old_id = consumer.incarnation();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(held) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    producer.close(old_id).unwrap();
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Closed));
    assert!(producer.attach(1).is_err());
    drop(consumer);
    assert!(producer.attach(1).is_err());
    for _ in 0..20 {
        producer.publish(FrameDescriptor::default(), b"wxyz").unwrap();
    }
    assert_eq!(held.bytes(), b"abcd");
    drop(held);
    let restarted = ArenaConsumer::from_grant(producer.attach(2).unwrap()).unwrap();
    assert_ne!(old_id, restarted.incarnation());
    let AcquireOutcome::Frame(frame) = restarted.acquire_latest(0).unwrap() else {
        panic!("restart cannot acquire")
    };
    assert_eq!(frame.bytes(), b"wxyz");
}

#[test]
fn an_abandoned_grant_returns_its_reservation_without_a_consumer_process() {
    let mut settings = config();
    settings.max_incarnations = 1;
    let mut producer = ArenaProducer::new(settings).unwrap();
    let grant = producer.attach(2).unwrap();
    assert!(producer.attach(1).is_err());
    drop(grant);
    let _consumer = ArenaConsumer::from_grant(producer.attach(2).unwrap()).unwrap();
}

#[test]
fn producer_shutdown_preserves_held_payload_and_closes_new_acquisitions() {
    let mut producer = ArenaProducer::new(config()).unwrap();
    let consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    drop(producer);
    assert_eq!(frame.bytes(), b"abcd");
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Closed));
}

#[test]
fn ordered_delivery_reports_a_published_gap_and_exact_selection_never_substitutes() {
    let mut producer = ArenaProducer::new(config()).unwrap();
    let consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    assert!(matches!(consumer.acquire_next(0).unwrap(), AcquireOutcome::Empty));
    for sequence in 1..=5 {
        producer
            .publish(
                FrameDescriptor {
                    sequence,
                    ..FrameDescriptor::default()
                },
                b"abcd",
            )
            .unwrap();
    }
    assert!(matches!(
        consumer.acquire_next(1).unwrap(),
        AcquireOutcome::Gap { first: 2, last: 3 }
    ));
    assert!(matches!(consumer.acquire_exact(2).unwrap(), AcquireOutcome::Miss { cursor: 2 }));
    let AcquireOutcome::Frame(first) = consumer.acquire_next(0).unwrap() else {
        panic!("oldest retained missing")
    };
    assert_eq!(first.cursor(), 4);
    drop(first);
    let AcquireOutcome::Frame(next) = consumer.acquire_next(4).unwrap() else {
        panic!("ordered successor missing")
    };
    assert_eq!(next.cursor(), 5);
    drop(next);
    assert!(matches!(consumer.acquire_next(5).unwrap(), AcquireOutcome::Empty));
    assert!(matches!(consumer.acquire_next(u64::MAX).unwrap(), AcquireOutcome::Empty));
    assert!(matches!(consumer.acquire_exact(u64::MAX).unwrap(), AcquireOutcome::Empty));
    assert!(consumer.acquire_exact(0).is_err());
}

#[test]
fn a_separate_consumer_process_retains_a_frame_with_no_per_frame_broker_exchange() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("arena.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut producer = ArenaProducer::new(config()).unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "mapped_arena_child", "--nocapture"])
        .env("JACKSTAY_ARENA_TEST_SOCKET", &socket)
        .spawn()
        .unwrap();
    let (descriptor, fds) = producer.attach_process(2, child.id()).unwrap().into_parts().unwrap();
    let (mut stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let json = serde_json::to_vec(&descriptor).unwrap();
    stream.write_all(&(json.len() as u32).to_le_bytes()).unwrap();
    stream.write_all(&json).unwrap();
    fdpass::send_fds(&stream, &fds.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>()).unwrap();
    let mut ready = [0];
    stream.read_exact(&mut ready).unwrap();
    assert_eq!(ready, [1]);
    for sequence in 2..=100 {
        assert!(matches!(
            producer
                .publish(
                    FrameDescriptor {
                        sequence,
                        ..FrameDescriptor::default()
                    },
                    b"wxyz"
                )
                .unwrap(),
            PublishOutcome::Published { .. }
        ));
    }
    // Test barrier only: no frame metadata, acquire, or release message crosses
    // this channel after setup. The child's next acquire is shared-memory only.
    stream.write_all(&[2]).unwrap();
    stream.read_exact(&mut ready).unwrap();
    assert_eq!(ready, [3]);
    producer
        .publish(
            FrameDescriptor {
                sequence: 101,
                ..FrameDescriptor::default()
            },
            b"next",
        )
        .unwrap();
    assert!(child.wait().unwrap().success());
}

#[test]
#[ignore = "subprocess helper invoked by a_separate_consumer_process_retains_a_frame_with_no_per_frame_broker_exchange"]
fn mapped_arena_child() {
    let mut stream = UnixStream::connect(std::env::var("JACKSTAY_ARENA_TEST_SOCKET").unwrap()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut len = [0; 4];
    stream.read_exact(&mut len).unwrap();
    let mut bytes = vec![0; u32::from_le_bytes(len) as usize];
    stream.read_exact(&mut bytes).unwrap();
    let descriptor = serde_json::from_slice(&bytes).unwrap();
    let fds = fdpass::recv_fds(&stream, 4).unwrap().try_into().unwrap();
    // SAFETY: the parent is the sole conforming producer; this process is the
    // only recipient of the single-use grant and does not fork its mappings.
    let grant = unsafe { ConsumerGrant::from_parts(descriptor, fds) }.unwrap();
    let mut consumer = ArenaConsumer::from_grant(grant).unwrap();
    let AcquireOutcome::Frame(held) = consumer.acquire_latest(0).unwrap() else {
        panic!("child missing initial frame")
    };
    stream.write_all(&[1]).unwrap();
    let mut done = [0];
    stream.read_exact(&mut done).unwrap();
    assert_eq!(done, [2]);
    assert_eq!(held.bytes(), b"abcd");
    let AcquireOutcome::Frame(latest) = consumer.acquire_latest(held.cursor()).unwrap() else {
        panic!("child missing latest frame")
    };
    assert_eq!(latest.bytes(), b"wxyz");
    assert_eq!(latest.descriptor().sequence, 100);
    drop(latest);
    let observed = consumer.events();
    stream.write_all(&[3]).unwrap();
    let cancel = Cancellation::new().unwrap();
    let WaitOutcome::Changed(events) = consumer
        .wait(observed, WaitInterest::DATA, &cancel, Some(Duration::from_secs(2)))
        .unwrap()
    else {
        panic!("cross-process publication wake was lost")
    };
    assert_eq!(events.data_cursor, 101);
    assert_eq!(held.bytes(), b"abcd");
    let AcquireOutcome::Frame(next) = consumer.acquire_latest(100).unwrap() else {
        panic!("child missing frame after wait")
    };
    assert_eq!(next.bytes(), b"next");
}
