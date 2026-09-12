#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::{
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::net::{UnixListener, UnixStream},
    },
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};

#[path = "support/child.rs"]
mod child;
use child::KillOnDrop;
use jackstay::{
    acquisition::{
        IncarnationId,
        arena::{
            AcquireOutcome, ArenaConfig, ArenaConsumer, ArenaProducer, ConsumerGrant, FrameDescriptor, ReleaseNotification, ReleaseTimeline,
        },
    },
    fdpass,
};

fn spawn_consumer(producer: &mut ArenaProducer, timeline: Option<Arc<dyn ReleaseTimeline>>) -> (KillOnDrop, IncarnationId) {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("crash.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "mapped_crash_child", "--nocapture"])
        .env("JACKSTAY_CRASH_TEST_SOCKET", &socket)
        .spawn()
        .unwrap();
    let child = KillOnDrop(child);
    let (mut stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let grant = producer.attach_process(1, child.id()).unwrap();
    let incarnation = grant.incarnation();
    let registration = timeline.map(|timeline| producer.register_release_timeline(incarnation, timeline).unwrap());
    let (descriptor, fds) = grant.into_parts().unwrap();
    let json = serde_json::to_vec(&(descriptor, registration)).unwrap();
    stream.write_all(&(json.len() as u32).to_le_bytes()).unwrap();
    stream.write_all(&json).unwrap();
    fdpass::send_fds(&stream, &fds.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>()).unwrap();
    let mut ready = [0];
    stream.read_exact(&mut ready).unwrap();
    assert_eq!(ready, [1]);
    assert_eq!(stream.read(&mut ready).unwrap(), 0, "child did not close its control connection");
    (child, incarnation)
}

#[test]
fn cpu_process_crash_reclaims_only_that_incarnation_and_never_treats_connection_eof_as_exit() {
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
    let healthy = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(retained) = healthy.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    let mut previous = None;
    for _ in 0..3 {
        let (mut child, incarnation) = spawn_consumer(&mut producer, None);
        assert_ne!(Some(incarnation), previous);
        producer.close(incarnation).unwrap();
        producer.poll_cleanup().unwrap();
        assert!(child.try_wait().unwrap().is_none());
        assert!(producer.attach(1).is_err(), "EOF reclaimed a live consumer's reservation");
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
        producer.poll_cleanup().unwrap();
        assert!(producer.cleanup_failures().is_empty());
        for _ in 0..100 {
            producer.publish(FrameDescriptor::default(), b"wxyz").unwrap();
        }
        assert_eq!(retained.bytes(), b"abcd");
        previous = Some(incarnation);
    }
    let restarted = producer.attach(1).unwrap();
    assert_ne!(Some(restarted.incarnation()), previous);
}

#[test]
fn a_cpu_process_crash_retires_its_old_mapping_and_unblocks_a_capacity_paused_replacement() {
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
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let (mut child, incarnation) = spawn_consumer(&mut producer, None);
    assert!(matches!(
        producer.reconfigure_cpu(64 * 1024).unwrap(),
        ReconfigurationStatus::PausedCapacity { .. }
    ));
    producer.close(incarnation).unwrap();
    assert!(matches!(
        producer.advance_reconfiguration().unwrap(),
        ReconfigurationStatus::PausedCapacity { .. }
    ));
    assert!(child.try_wait().unwrap().is_none());
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    assert!(matches!(
        producer.advance_reconfiguration().unwrap(),
        ReconfigurationStatus::Ready { .. }
    ));
    let restarted = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    assert_ne!(restarted.incarnation(), incarnation);
    producer.publish(FrameDescriptor::default(), b"replacement").unwrap();
    let AcquireOutcome::Frame(frame) = restarted.acquire_latest(0).unwrap() else {
        panic!("no replacement frame")
    };
    assert_eq!(frame.bytes(), b"replacement");
}

#[test]
#[ignore = "subprocess helper invoked by cpu_process_crash_reclaims_only_that_incarnation_and_never_treats_connection_eof_as_exit"]
fn mapped_crash_child() {
    let mut stream = UnixStream::connect(std::env::var("JACKSTAY_CRASH_TEST_SOCKET").unwrap()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut len = [0; 4];
    stream.read_exact(&mut len).unwrap();
    let mut json = vec![0; u32::from_le_bytes(len) as usize];
    stream.read_exact(&mut json).unwrap();
    let (descriptor, registration): (_, Option<jackstay::acquisition::arena::ReleaseTimelineRegistration>) =
        serde_json::from_slice(&json).unwrap();
    let fds = fdpass::recv_fds(&stream, 5).unwrap().try_into().unwrap();
    // SAFETY: the parent owns the producer and bound this single-use grant to
    // this process before handoff. No fork or forwarding of mapped claims.
    let grant = unsafe { ConsumerGrant::from_parts(descriptor, fds) }.unwrap();
    let consumer = ArenaConsumer::from_grant(grant).unwrap();
    let AcquireOutcome::Frame(held) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    assert_eq!(held.bytes().len(), 4);
    let _held = if let Some(registration) = registration {
        held.defer_release(&registration, 5).unwrap();
        None
    } else {
        Some(held)
    };
    stream.write_all(&[1]).unwrap();
    drop(stream);
    // SIGKILL must prevent all Rust destructors and shutdown acknowledgements.
    loop {
        std::thread::park();
    }
}

#[derive(Debug, Default)]
struct Completion(Mutex<(u64, Vec<(u64, ReleaseNotification)>)>);
impl Completion {
    fn signal(&self, value: u64) {
        let mut state = self.0.lock().unwrap();
        state.0 = value;
        state.1.retain(|(target, wake)| {
            if *target <= value {
                wake.notify().unwrap();
                false
            } else {
                true
            }
        });
    }
}
impl ReleaseTimeline for Completion {
    fn completed_value(&self) -> jackstay::Result<u64> {
        Ok(self.0.lock().unwrap().0)
    }
    fn notify_at(&self, value: u64, notification: ReleaseNotification) -> jackstay::Result<()> {
        let mut state = self.0.lock().unwrap();
        if state.0 >= value {
            notification.notify().unwrap();
        } else {
            state.1.push((value, notification));
        }
        Ok(())
    }
}

#[test]
fn a_process_crash_keeps_its_submitted_deferred_claim_until_external_completion() {
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
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let completion = Arc::new(Completion::default());
    let (mut child, old) = spawn_consumer(&mut producer, Some(completion.clone()));
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    producer.poll_cleanup().unwrap();
    assert!(
        producer.cleanup_failures().is_empty(),
        "a valid submitted completion is still draining"
    );
    assert!(producer.attach(1).is_err(), "process death completed external asynchronous work");
    for _ in 0..100 {
        producer.publish(FrameDescriptor::default(), b"wxyz").unwrap();
    }
    completion.signal(4);
    producer.poll_cleanup().unwrap();
    assert!(producer.attach(1).is_err(), "an earlier timeline value completed the lease");
    completion.signal(5);
    producer.poll_cleanup().unwrap();
    assert_ne!(producer.attach(1).unwrap().incarnation(), old);
}

#[test]
fn a_drain_deadline_reports_recovery_without_revoking_storage_and_late_completion_still_reclaims() {
    use std::time::Instant;
    let mut producer = ArenaProducer::new(ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 4,
        memory_budget: 1024 * 1024,
        max_incarnations: 2,
        drain_timeout: Duration::from_millis(20),
    })
    .unwrap();
    let stalled = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let healthy = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let completion = Arc::new(Completion::default());
    let registration = producer
        .register_release_timeline(stalled.incarnation(), completion.clone())
        .unwrap();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(frame) = stalled.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    let address = frame.bytes().as_ptr();
    frame.defer_release(&registration, 5).unwrap();
    // An active consumer may legitimately hold its reserved capacity indefinitely.
    std::thread::sleep(Duration::from_millis(30));
    assert!(producer.cleanup_failures().is_empty());
    producer.close(stalled.incarnation()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while producer.cleanup_failures().is_empty() {
        assert!(Instant::now() < deadline, "idle cleanup never reported the expired drain interval");
        std::thread::sleep(Duration::from_millis(5));
    }
    let failures = producer.cleanup_failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].incarnation, stalled.incarnation());
    assert!(failures[0].reason.contains("drain deadline"));
    assert!(producer.retry_cleanup(stalled.incarnation()).is_err());
    for _ in 0..100 {
        producer.publish(FrameDescriptor::default(), b"wxyz").unwrap();
    }
    assert!(producer.attach(1).is_err());
    assert_eq!(stalled.events().capacity_epoch, 0);
    // SAFETY: stalled still owns this mapping, publication has finished, and
    // the declared external use has not completed. Timeout must retain bytes.
    assert_eq!(unsafe { std::slice::from_raw_parts(address, 4) }, b"abcd");
    assert!(matches!(healthy.acquire_latest(0).unwrap(), AcquireOutcome::Frame(_)));
    completion.signal(5);
    producer.poll_cleanup().unwrap();
    assert_eq!(stalled.events().capacity_epoch, 1);
    let old = stalled.incarnation();
    drop(stalled);
    producer.poll_cleanup().unwrap();
    assert!(producer.cleanup_failures().is_empty());
    assert_ne!(producer.attach(1).unwrap().incarnation(), old);
}

#[test]
fn local_consumer_shutdown_reports_a_stalled_cpu_lease_without_a_host_poll() {
    use std::time::Instant;
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
    let consumer = ArenaConsumer::from_grant(producer.attach(1).unwrap()).unwrap();
    let old = consumer.incarnation();
    producer.publish(FrameDescriptor::default(), b"abcd").unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    drop(consumer);
    let deadline = Instant::now() + Duration::from_secs(2);
    while producer.cleanup_failures().is_empty() {
        assert!(Instant::now() < deadline, "consumer closure did not wake its cleanup owner");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(producer.cleanup_failures()[0].incarnation, old);
    assert_eq!(frame.bytes(), b"abcd");
    assert!(producer.attach(1).is_err());
    drop(frame);
    producer.poll_cleanup().unwrap();
    assert!(producer.cleanup_failures().is_empty());
    assert_ne!(producer.attach(1).unwrap().incarnation(), old);
}
