#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::{
    os::unix::net::UnixStream,
    sync::{Arc, Mutex},
    time::Duration,
};

use jackstay::acquisition::{
    arena::{AcquireOutcome, ArenaConfig, ArenaProducer, FrameDescriptor},
    socket::{CpuSetupClient, serve_cpu},
};

#[path = "support/child.rs"]
mod child;

fn new_producer() -> Arc<Mutex<ArenaProducer>> {
    Arc::new(Mutex::new(
        ArenaProducer::new(ArenaConfig {
            resource_capacity: 6,
            retained_history: 2,
            producer_reserve: 1,
            payload_capacity: 4,
            memory_budget: 1024 * 1024,
            max_incarnations: 2,
            drain_timeout: Duration::from_secs(5),
        })
        .unwrap(),
    ))
}

#[test]
fn socket_setup_admits_once_and_frames_outlive_connection_and_history() {
    let producer = new_producer();
    let (server, stream) = UnixStream::pair().unwrap();
    let served = producer.clone();
    let task = std::thread::spawn(move || serve_cpu(server, served));
    // SAFETY: this stream's sole producer is the conforming arena above. The
    // client never forks or forwards its process-bound grant/mappings.
    let mut client = unsafe { CpuSetupClient::from_stream(stream) };
    assert!(client.attach(4).is_err(), "admitted beyond history + reserve");
    let consumer = client.attach(2).unwrap();
    assert!(client.attach(1).is_err(), "attached twice on one connection");
    producer.lock().unwrap().publish(FrameDescriptor::default(), b"held").unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    let original = *frame.descriptor();
    for _ in 0..100 {
        producer.lock().unwrap().publish(FrameDescriptor::default(), b"next").unwrap();
    }
    drop(client);
    task.join().unwrap().unwrap();
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Closed));
    drop(consumer);
    assert_eq!(frame.descriptor(), &original);
    assert_eq!(frame.bytes(), b"held");
    producer.lock().unwrap().stop();
    assert!(!producer.lock().unwrap().poll_shutdown_ready().unwrap());
    drop(frame);
    assert!(producer.lock().unwrap().poll_shutdown_ready().unwrap());
}

fn connect(
    producer: &Arc<Mutex<ArenaProducer>>,
) -> (
    CpuSetupClient,
    std::thread::JoinHandle<Result<(), jackstay::acquisition::socket::SocketError>>,
) {
    let (server, stream) = UnixStream::pair().unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    let producer = producer.clone();
    let task = std::thread::spawn(move || serve_cpu(server, producer));
    // SAFETY: this stream carries only the test's conforming producer and is
    // neither forwarded nor forked after receiving the process-bound grant.
    (unsafe { CpuSetupClient::from_stream(stream) }, task)
}

#[test]
fn socket_replacement_preserves_old_leases_and_rejects_foreign_claim_scopes() {
    use jackstay::acquisition::arena::{ConfigurationInstall, ReconfigurationStatus};
    let producer = new_producer();
    let (mut client, task) = connect(&producer);
    let mut consumer = client.attach(2).unwrap();
    producer.lock().unwrap().publish(FrameDescriptor::default(), b"old!").unwrap();
    let AcquireOutcome::Frame(old) = consumer.acquire_latest(0).unwrap() else {
        panic!("missing frame")
    };
    let original = *old.descriptor();
    let ReconfigurationStatus::Ready { generation } = producer.lock().unwrap().reconfigure_cpu(8).unwrap() else {
        panic!("replacement should fit")
    };
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Reconfiguration));
    assert_eq!(
        client.install_configuration(&mut consumer).unwrap(),
        Some(ConfigurationInstall::Installed)
    );
    assert_eq!(client.install_configuration(&mut consumer).unwrap(), None);
    producer.lock().unwrap().publish(FrameDescriptor::default(), b"new size").unwrap();
    let AcquireOutcome::Frame(new) = consumer.acquire_latest(old.cursor()).unwrap() else {
        panic!("missing replacement")
    };
    assert_eq!(new.descriptor().config_generation, generation);
    assert_eq!(new.bytes(), b"new size");
    assert_eq!(old.bytes(), b"old!");
    assert_eq!(old.descriptor(), &original);
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));

    let other = new_producer();
    let (mut other_client, other_task) = connect(&other);
    let mut foreign = other_client.attach(1).unwrap();
    assert_eq!(
        consumer.incarnation(),
        foreign.incarnation(),
        "numeric IDs should collide across producers"
    );
    assert!(client.install_configuration(&mut foreign).is_err());
    drop(other_client);
    other_task.join().unwrap().unwrap();
    drop(foreign);
    drop(old);
    drop(new);
    for _ in 0..100 {
        consumer.relinquish_configuration();
        assert!(matches!(
            producer.lock().unwrap().reconfigure_cpu(8).unwrap(),
            ReconfigurationStatus::Ready { .. }
        ));
        assert_eq!(
            client.install_configuration(&mut consumer).unwrap(),
            Some(ConfigurationInstall::Installed)
        );
    }
    drop(consumer);
    drop(client);
    task.join().unwrap().unwrap();
    producer.lock().unwrap().stop();
    assert!(producer.lock().unwrap().poll_shutdown_ready().unwrap());
}

#[test]
fn socket_setup_can_resume_a_capacity_pause_after_the_old_frame_retires() {
    use jackstay::acquisition::arena::{ConfigurationInstall, PublishOutcome, ReconfigurationStatus};
    let producer = Arc::new(Mutex::new(
        ArenaProducer::new(ArenaConfig {
            resource_capacity: 6,
            retained_history: 2,
            producer_reserve: 1,
            payload_capacity: 32 * 1024,
            memory_budget: 512 * 1024,
            max_incarnations: 2,
            drain_timeout: Duration::from_secs(5),
        })
        .unwrap(),
    ));
    let (mut client, task) = connect(&producer);
    let mut consumer = client.attach(1).unwrap();
    producer.lock().unwrap().publish(FrameDescriptor::default(), b"held").unwrap();
    let AcquireOutcome::Frame(old) = consumer.acquire_latest(0).unwrap() else {
        panic!("no old frame")
    };
    assert!(matches!(
        producer.lock().unwrap().reconfigure_cpu(64 * 1024).unwrap(),
        ReconfigurationStatus::PausedCapacity { .. }
    ));
    consumer.relinquish_configuration();
    assert_eq!(client.install_configuration(&mut consumer).unwrap(), None);
    assert_eq!(
        producer.lock().unwrap().publish(FrameDescriptor::default(), b"drop").unwrap(),
        PublishOutcome::Dropped
    );
    assert!(matches!(
        producer.lock().unwrap().advance_reconfiguration().unwrap(),
        ReconfigurationStatus::PausedCapacity { .. }
    ));
    assert_eq!(old.bytes(), b"held");
    drop(old);
    assert!(matches!(
        producer.lock().unwrap().advance_reconfiguration().unwrap(),
        ReconfigurationStatus::Ready { .. }
    ));
    assert_eq!(
        client.install_configuration(&mut consumer).unwrap(),
        Some(ConfigurationInstall::Installed)
    );
    producer.lock().unwrap().publish(FrameDescriptor::default(), b"resumed").unwrap();
    let AcquireOutcome::Frame(new) = consumer.acquire_latest(0).unwrap() else {
        panic!("pause did not resume")
    };
    assert_eq!(new.bytes(), b"resumed");
    drop(new);
    drop(consumer);
    drop(client);
    task.join().unwrap().unwrap();
    producer.lock().unwrap().stop();
    assert!(producer.lock().unwrap().poll_shutdown_ready().unwrap());
}

#[test]
fn socket_eof_keeps_a_live_peers_claim_but_verified_process_exit_returns_admission() {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixListener,
        process::Command,
        time::Instant,
    };

    use jackstay::acquisition::socket::peer_pid;
    let producer = new_producer();
    producer.lock().unwrap().publish(FrameDescriptor::default(), b"held").unwrap();
    let directory = tempfile::tempdir().unwrap();
    let setup_path = directory.path().join("setup.sock");
    let control_path = directory.path().join("control.sock");
    let setup = UnixListener::bind(&setup_path).unwrap();
    let control = UnixListener::bind(&control_path).unwrap();
    setup.set_nonblocking(true).unwrap();
    control.set_nonblocking(true).unwrap();
    let mut child = child::KillOnDrop(
        Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "socket_claim_child", "--nocapture"])
            .env("JACKSTAY_SOCKET_SETUP_TEST", &setup_path)
            .env("JACKSTAY_SOCKET_CONTROL_TEST", &control_path)
            .spawn()
            .unwrap(),
    );
    let accept = |listener: &UnixListener| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "child did not connect");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept: {error}"),
            }
        }
    };
    let stream = accept(&setup);
    assert_eq!(peer_pid(&stream).unwrap(), child.id());
    assert_ne!(peer_pid(&stream).unwrap(), std::process::id());
    let served = producer.clone();
    let task = std::thread::spawn(move || serve_cpu(stream, served));
    let mut control = accept(&control);
    control.set_nonblocking(false).unwrap();
    control.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    control.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut response = [0; 4];
    control.read_exact(&mut response).unwrap();
    assert_eq!(&response, b"held");
    control.write_all(b"close").unwrap();
    control.read_exact(&mut response).unwrap();
    assert_eq!(&response, b"held");
    task.join().unwrap().unwrap();
    assert!(child.try_wait().unwrap().is_none(), "EOF was not supposed to mean process exit");
    let (mut replacement, replacement_task) = connect(&producer);
    assert!(replacement.attach(2).is_err(), "socket EOF discarded a live claim");
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(5);
    let consumer = loop {
        producer.lock().unwrap().poll_cleanup().unwrap();
        match replacement.attach(2) {
            Ok(consumer) => break consumer,
            Err(error) => {
                assert!(Instant::now() < deadline, "dead CPU process did not retire: {error}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    };
    assert!(producer.lock().unwrap().cleanup_failures().is_empty());
    producer.lock().unwrap().publish(FrameDescriptor::default(), b"back").unwrap();
    let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() else {
        panic!("replacement cannot acquire")
    };
    assert_eq!(frame.bytes(), b"back");
    drop(frame);
    drop(consumer);
    drop(replacement);
    replacement_task.join().unwrap().unwrap();
    producer.lock().unwrap().stop();
    assert!(producer.lock().unwrap().poll_shutdown_ready().unwrap());
}

#[test]
#[ignore = "subprocess helper invoked by socket_eof_keeps_a_live_peers_claim_but_verified_process_exit_returns_admission"]
fn socket_claim_child() {
    use std::io::{Read, Write};
    let stream = UnixStream::connect(std::env::var("JACKSTAY_SOCKET_SETUP_TEST").unwrap()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    // SAFETY: the parent binds this single-use grant to the child's real socket
    // peer identity. The child never forks or forwards its mappings.
    let mut client = unsafe { CpuSetupClient::from_stream(stream) };
    let consumer = client.attach(2).unwrap();
    let frame = match consumer.acquire_latest(0).unwrap() {
        AcquireOutcome::Frame(frame) => frame,
        other => panic!("no child frame: {other:?}"),
    };
    let mut control = UnixStream::connect(std::env::var("JACKSTAY_SOCKET_CONTROL_TEST").unwrap()).unwrap();
    control.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    control.write_all(frame.bytes()).unwrap();
    let mut command = [0; 5];
    control.read_exact(&mut command).unwrap();
    assert_eq!(&command, b"close");
    drop(client);
    control.write_all(frame.bytes()).unwrap();
    loop {
        std::thread::park();
    }
}
