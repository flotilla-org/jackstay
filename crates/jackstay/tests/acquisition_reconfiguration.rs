use jackstay::acquisition::arena::{
    AcquireOutcome, ArenaConfig, ArenaConsumer, ArenaProducer, ConfigurationInstall, FrameDescriptor, PublishOutcome, ReconfigurationStatus,
};

#[path = "support/child.rs"]
mod child;
#[path = "support/setup.rs"]
mod setup;

#[test]
fn a_stale_offer_with_a_contradictory_resource_header_is_an_error() {
    use jackstay::acquisition::arena::{ArenaError, ConfigurationGrant, ConsumerGrant};
    let mut producer = ArenaProducer::new(ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 4,
        memory_budget: 1024 * 1024,
        max_incarnations: 1,
        drain_timeout: std::time::Duration::from_secs(5),
    })
    .unwrap();
    let (descriptor, fds) = producer.attach_process(1, std::process::id()).unwrap().into_parts().unwrap();
    // Keep a copy only while the consumer's current mapping still retains this
    // generation. It is consumed/dropped before that mapping is relinquished.
    let old_fd = fds[1].try_clone().unwrap();
    // SAFETY: this process is the sole admitted recipient of this local producer.
    let grant = unsafe { ConsumerGrant::from_parts(descriptor, fds) }.unwrap();
    let mut consumer = ArenaConsumer::from_grant(grant).unwrap();
    producer.reconfigure_cpu(4).unwrap();
    let (descriptor, correct_fd) = producer
        .configuration_offer(consumer.incarnation())
        .unwrap()
        .unwrap()
        .into_parts()
        .unwrap();
    drop(correct_fd);
    producer.reconfigure_cpu(4).unwrap();
    // SAFETY: both maps are initialized by the conforming sole producer and
    // retained throughout this call. The intentionally wrong header must be
    // rejected before acquisition, even though the offered generation is stale.
    let grant = unsafe { ConfigurationGrant::from_parts(&consumer, descriptor, old_fd) }.unwrap();
    assert!(matches!(consumer.install_configuration(grant), Err(ArenaError::Mapping(_))));
    assert!(producer.configuration_offer(consumer.incarnation()).unwrap().is_some());
}

#[test]
fn a_separate_process_installs_replacement_storage_while_retaining_its_old_frame() {
    use std::{process::Command, time::Duration};

    let listener = setup::Listener::bind("configuration");
    let mut producer = ArenaProducer::new(ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 4,
        memory_budget: 1024 * 1024,
        max_incarnations: 1,
        drain_timeout: Duration::from_secs(5),
    })
    .unwrap();
    producer
        .publish(
            FrameDescriptor {
                width: 1,
                ..FrameDescriptor::default()
            },
            b"old!",
        )
        .unwrap();
    let mut child = child::KillOnDrop(
        Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "mapped_configuration_child", "--nocapture"])
            .env("JACKSTAY_CONFIGURATION_TEST_SOCKET", listener.address())
            .spawn()
            .unwrap(),
    );
    let grant = producer.attach_process(2, child.id()).unwrap();
    let incarnation = grant.incarnation();
    let (descriptor, fds) = grant.into_parts().unwrap();
    let mut stream = listener.accept();
    stream.send(&descriptor);
    stream.send_objects(&child, &fds);
    drop(fds);
    assert_eq!(stream.read_byte(), 1);
    producer.reconfigure_cpu(8).unwrap();
    let (replacement, fd) = producer.configuration_offer(incarnation).unwrap().unwrap().into_parts().unwrap();
    for _ in 0..100 {
        producer
            .publish(
                FrameDescriptor {
                    width: 2,
                    ..FrameDescriptor::default()
                },
                b"new-data",
            )
            .unwrap();
    }
    stream.send(&replacement);
    stream.send_objects(&child, std::slice::from_ref(&fd));
    drop(fd);
    assert!(child.wait().unwrap().success());
    assert_ne!(producer.attach(1).unwrap().incarnation(), incarnation);
}

#[test]
#[ignore = "subprocess helper invoked by a_separate_process_installs_replacement_storage_while_retaining_its_old_frame"]
fn mapped_configuration_child() {
    use jackstay::acquisition::arena::{ConfigurationGrant, ConsumerGrant};
    let mut stream = setup::Link::connect(&std::env::var("JACKSTAY_CONFIGURATION_TEST_SOCKET").unwrap());
    let descriptor = stream.recv();
    let fds = stream.recv_objects(5).try_into().unwrap();
    // SAFETY: the parent is the sole conforming producer. This process is the
    // admitted sole recipient and does not fork or forward any setup grant.
    let grant = unsafe { ConsumerGrant::from_parts(descriptor, fds) }.unwrap();
    let mut consumer = ArenaConsumer::from_grant(grant).unwrap();
    let AcquireOutcome::Frame(old) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing old frame")
    };
    stream.write_byte(1);
    let descriptor = stream.recv();
    let fd = stream.recv_objects(1).pop().unwrap();
    // SAFETY: this is the parent's single-use replacement offer for the same
    // admitted process. It retains its allocation until this recipient retires it.
    let offer = unsafe { ConfigurationGrant::from_parts(&consumer, descriptor, fd) }.unwrap();
    assert_eq!(consumer.install_configuration(offer).unwrap(), ConfigurationInstall::Installed);
    let AcquireOutcome::Frame(new) = consumer.acquire_latest(1).unwrap() else {
        panic!("missing new frame")
    };
    assert_eq!(old.bytes(), b"old!");
    assert_eq!(old.descriptor().width, 1);
    assert_eq!(new.bytes(), b"new-data");
    assert_eq!(new.descriptor().width, 2);
    assert_eq!(new.cursor(), 101);
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));
    drop(old);
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Frame(_)));
}

#[test]
fn one_stale_offer_does_not_queue_more_offers_or_block_healthy_configuration_changes() {
    let mut producer = ArenaProducer::new(ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 4,
        memory_budget: 256 * 1024,
        max_incarnations: 2,
        drain_timeout: std::time::Duration::from_secs(5),
    })
    .unwrap();
    let mut slow = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let mut healthy = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    producer.reconfigure_cpu(8).unwrap();
    let stale = producer.configuration_offer(slow.incarnation()).unwrap().unwrap();
    healthy
        .install_configuration(producer.configuration_offer(healthy.incarnation()).unwrap().unwrap())
        .unwrap();
    for _ in 0..100 {
        assert!(matches!(producer.reconfigure_cpu(8).unwrap(), ReconfigurationStatus::Ready { .. }));
        assert!(producer.configuration_offer(slow.incarnation()).unwrap().is_none());
        healthy
            .install_configuration(producer.configuration_offer(healthy.incarnation()).unwrap().unwrap())
            .unwrap();
        producer.publish(FrameDescriptor::default(), b"healthy").unwrap();
        assert!(matches!(healthy.acquire_latest(0).unwrap(), AcquireOutcome::Frame(_)));
    }
    assert_eq!(slow.install_configuration(stale).unwrap(), ConfigurationInstall::Stale);
    assert!(matches!(slow.acquire_latest(0).unwrap(), AcquireOutcome::Reconfiguration));
    slow.install_configuration(producer.configuration_offer(slow.incarnation()).unwrap().unwrap())
        .unwrap();
    let AcquireOutcome::Frame(frame) = slow.acquire_latest(0).unwrap() else {
        panic!("slow consumer did not catch up")
    };
    assert_eq!(frame.bytes(), b"healthy");
}

#[test]
fn replacement_reports_only_published_gaps_and_never_reads_the_old_ring_as_new_storage() {
    let mut producer = ArenaProducer::new(ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 4,
        memory_budget: 1024 * 1024,
        max_incarnations: 1,
        drain_timeout: std::time::Duration::from_secs(5),
    })
    .unwrap();
    let mut consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    for _ in 0..5 {
        producer.publish(FrameDescriptor::default(), b"old!").unwrap();
    }
    producer.reconfigure_cpu(8).unwrap();
    consumer
        .install_configuration(producer.configuration_offer(consumer.incarnation()).unwrap().unwrap())
        .unwrap();
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Empty));
    assert!(matches!(
        consumer.acquire_next(2).unwrap(),
        AcquireOutcome::Gap { first: 3, last: 5 }
    ));
    assert!(matches!(consumer.acquire_exact(5).unwrap(), AcquireOutcome::Miss { cursor: 5 }));
    assert!(matches!(consumer.acquire_next(5).unwrap(), AcquireOutcome::Empty));
    assert_eq!(
        producer.publish(FrameDescriptor::default(), b"new!").unwrap(),
        PublishOutcome::Published { cursor: 6 }
    );
    assert!(matches!(
        consumer.acquire_next(2).unwrap(),
        AcquireOutcome::Gap { first: 3, last: 5 }
    ));
    let AcquireOutcome::Frame(new) = consumer.acquire_next(5).unwrap() else {
        panic!("missing new cursor")
    };
    assert_eq!(new.cursor(), 6);
    assert_eq!(new.bytes(), b"new!");
    assert!(matches!(consumer.acquire_exact(5).unwrap(), AcquireOutcome::Miss { cursor: 5 }));
}

#[test]
fn an_exhausted_transition_waits_for_mapping_and_lease_retirement_before_allocating() {
    use jackstay::acquisition::{
        AdmissionError,
        arena::{ArenaError, Cancellation, WaitInterest, WaitOutcome},
    };
    let mut producer = ArenaProducer::new(ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 32 * 1024,
        memory_budget: 512 * 1024,
        max_incarnations: 2,
        drain_timeout: std::time::Duration::from_secs(5),
    })
    .unwrap();
    let mut consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(old) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    assert!(matches!(
        producer.reconfigure_cpu(64 * 1024).unwrap(),
        ReconfigurationStatus::PausedCapacity { .. }
    ));
    assert!(matches!(
        producer.attach(1),
        Err(ArenaError::Admission(AdmissionError::ReconfigurationPending))
    ));
    assert_eq!(
        producer.publish(FrameDescriptor::default(), b"dropped").unwrap(),
        PublishOutcome::Dropped
    );
    consumer.relinquish_configuration();
    assert!(matches!(
        producer.advance_reconfiguration().unwrap(),
        ReconfigurationStatus::PausedCapacity { .. }
    ));
    assert_eq!(old.bytes(), b"abcd");
    let observed = consumer.events();
    let cancellation = Cancellation::new().unwrap();
    cancellation.cancel().unwrap();
    assert_eq!(
        consumer.wait(observed, WaitInterest::RECONFIGURATION, &cancellation, None).unwrap(),
        WaitOutcome::Cancelled
    );
    drop(old);
    assert!(matches!(
        producer.advance_reconfiguration().unwrap(),
        ReconfigurationStatus::Ready { .. }
    ));
    let offer = producer.configuration_offer(consumer.incarnation()).unwrap().unwrap();
    consumer.install_configuration(offer).unwrap();
    assert_eq!(
        producer.publish(FrameDescriptor::default(), b"replacement").unwrap(),
        PublishOutcome::Published { cursor: 2 }
    );
    let AcquireOutcome::Frame(new) = consumer.acquire_latest(1).unwrap() else {
        panic!("missing replacement")
    };
    assert_eq!(new.bytes(), b"replacement");
}

#[test]
fn a_consumer_installs_a_replacement_while_its_old_frame_keeps_its_descriptor_storage_and_credit() {
    let mut producer = ArenaProducer::new(ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 4,
        memory_budget: 1024 * 1024,
        max_incarnations: 2,
        drain_timeout: std::time::Duration::from_secs(5),
    })
    .unwrap();
    let mut consumer = ArenaConsumer::from_grant(producer.attach(2).unwrap()).unwrap();
    producer
        .publish(
            FrameDescriptor {
                width: 1,
                height: 1,
                stride: 4,
                ..FrameDescriptor::default()
            },
            b"abcd",
        )
        .unwrap();
    let AcquireOutcome::Frame(old) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing old frame")
    };
    let old_descriptor = *old.descriptor();
    assert_eq!(old_descriptor.config_generation, 1);
    let before = consumer.events();
    assert!(matches!(producer.reconfigure_cpu(8).unwrap(), ReconfigurationStatus::Ready { .. }));
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Reconfiguration));
    assert!(consumer.events().reconfiguration_epoch > before.reconfiguration_epoch);
    let offer = producer.configuration_offer(consumer.incarnation()).unwrap().unwrap();
    assert_eq!(consumer.install_configuration(offer).unwrap(), ConfigurationInstall::Installed);
    assert_eq!(
        producer
            .publish(
                FrameDescriptor {
                    width: 2,
                    height: 1,
                    stride: 8,
                    ..FrameDescriptor::default()
                },
                b"12345678"
            )
            .unwrap(),
        PublishOutcome::Published { cursor: 2 }
    );
    let AcquireOutcome::Frame(new) = consumer.acquire_latest(1).unwrap() else {
        panic!("missing replacement frame")
    };
    assert_eq!(old.descriptor(), &old_descriptor);
    assert_eq!(old.bytes(), b"abcd");
    assert_eq!(new.descriptor().width, 2);
    assert_eq!(new.descriptor().config_generation, 2);
    assert_eq!(new.bytes(), b"12345678");
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));
    drop(old);
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Frame(_)));
}
