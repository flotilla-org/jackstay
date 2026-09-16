#![cfg(unix)]

use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    thread,
    time::{Duration, Instant},
};

use jackstay::{
    bootstrap::{self, InputRequest},
    input::{Config, Event, Mode, Operation, Outcome, Status, Target},
};

#[test]
fn one_connection_bootstraps_input_without_consuming_media_bytes() {
    let target = Target::new(Config::default()).unwrap();
    let (host, peer) = UnixStream::pair().unwrap();
    let host_target = target.clone();
    let worker = thread::spawn(move || bootstrap::accept(host, Some(host_target)).unwrap());
    let mut connection = bootstrap::connect(peer, InputRequest::Required(Mode::Cooperative)).unwrap();
    let mut accepted = worker.join().unwrap();
    assert!(connection.input_error.is_none());
    assert!(accepted.input.is_some());
    connection.media.write_all(b"media setup").unwrap();
    let mut message = [0; 11];
    accepted.media.read_exact(&mut message).unwrap();
    assert_eq!(&message, b"media setup");
    // Input keeps progressing when the media setup connection has closed.
    drop(connection.media);
    drop(accepted.media);
    let client = connection.input.unwrap();
    let sequence = client.send(Event::Text("hello 🐈".into())).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(work) = target.next() {
            assert!(matches!(&work.operation, Operation::Event(Event::Text(text)) if text == "hello 🐈"));
            target.complete(work.id, Outcome::Executed).unwrap();
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    loop {
        if let Some(status) = client.poll() {
            assert!(matches!(status, Status::Completed { sequence: s, outcome: Outcome::Executed } if s == sequence));
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    drop(client);
    drop(accepted.input);
}

#[test]
fn observer_does_not_claim_input_and_optional_denial_preserves_media() {
    for request in [InputRequest::None, InputRequest::Optional(Mode::Cooperative)] {
        let (host, peer) = UnixStream::pair().unwrap();
        let target = Target::new(Config::default()).unwrap();
        let offered = if matches!(request, InputRequest::None) {
            Some(target.clone())
        } else {
            None
        };
        let worker = thread::spawn(move || bootstrap::accept(host, offered).unwrap());
        let mut connected = bootstrap::connect(peer, request).unwrap();
        let mut accepted = worker.join().unwrap();
        assert!(connected.input.is_none());
        assert!(accepted.input.is_none());
        if matches!(request, InputRequest::None) {
            assert!(connected.input_error.is_none());
            assert!(target.admit(Mode::Cooperative).is_ok());
        } else {
            assert_eq!(connected.input_error, Some(jackstay::input::Error::Unsupported));
        }
        connected.media.write_all(b"ok").unwrap();
        let mut message = [0; 2];
        accepted.media.read_exact(&mut message).unwrap();
        assert_eq!(&message, b"ok");
    }
}

#[test]
fn required_input_denial_closes_media_and_optional_busy_is_explicit() {
    let (host, peer) = UnixStream::pair().unwrap();
    let worker = thread::spawn(move || bootstrap::accept(host, None).unwrap());
    assert!(matches!(
        bootstrap::connect(peer, InputRequest::Required(Mode::Cooperative)),
        Err(bootstrap::Error::Input(jackstay::input::Error::Unsupported))
    ));
    let mut accepted = worker.join().unwrap();
    assert_eq!(accepted.media.read(&mut [0]).unwrap(), 0);

    let target = Target::new(Config::default()).unwrap();
    let _existing = target.admit(Mode::Cooperative).unwrap();
    let (host, peer) = UnixStream::pair().unwrap();
    let worker = thread::spawn(move || bootstrap::accept(host, Some(target)).unwrap());
    let connected = bootstrap::connect(peer, InputRequest::Optional(Mode::Cooperative)).unwrap();
    assert_eq!(connected.input_error, Some(jackstay::input::Error::Busy));
    assert!(connected.input.is_none());
    drop(worker.join().unwrap());
}

#[test]
fn malformed_preface_and_truncated_input_offer_fail_instead_of_downgrading() {
    let (host, mut peer) = UnixStream::pair().unwrap();
    peer.write_all(b"BADBOOT1\0\0\0\0").unwrap();
    assert!(matches!(bootstrap::accept(host, None), Err(bootstrap::Error::Protocol(_))));
    let (mut host, peer) = UnixStream::pair().unwrap();
    let worker = thread::spawn(move || {
        let mut hello = [0; 12];
        host.read_exact(&mut hello).unwrap();
        host.write_all(b"JSBOOT01\0\0\0\x01").unwrap();
        // Close before the promised input descriptor arrives.
    });
    assert!(bootstrap::connect(peer, InputRequest::Optional(Mode::Cooperative)).is_err());
    worker.join().unwrap();
}

#[test]
fn silent_peer_cannot_hold_bootstrap_indefinitely() {
    let (host, _peer) = UnixStream::pair().unwrap();
    let started = Instant::now();
    assert!(matches!(bootstrap::accept(host, None), Err(bootstrap::Error::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut));
    assert!(started.elapsed() < Duration::from_secs(8));
}
