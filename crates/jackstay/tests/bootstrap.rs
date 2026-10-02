//! Source bootstrap over a local connection: a Unix socket, or a Windows pipe.

use std::{
    io::{Read, Write},
    thread,
    time::{Duration, Instant},
};

use jackstay::{
    bootstrap::{self, InputRequest},
    input::{Config, Event, Mode, Operation, Outcome, Status, Target},
    local::Stream,
};

#[cfg(unix)]
fn pair() -> (Stream, Stream) {
    Stream::pair().unwrap()
}

#[cfg(windows)]
fn pair() -> (Stream, Stream) {
    jackstay::local::pipe_pair().unwrap()
}

#[test]
fn one_connection_bootstraps_input_without_consuming_media_bytes() {
    exercise_input_roundtrip();
}

#[test]
fn concurrent_descriptor_handoffs_keep_input_connected() {
    thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..25 {
                    exercise_input_roundtrip();
                }
            });
        }
    });
}

fn exercise_input_roundtrip() {
    let target = Target::new(Config::default()).unwrap();
    let (host, peer) = pair();
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
        let (host, peer) = pair();
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
    let (host, peer) = pair();
    let worker = thread::spawn(move || bootstrap::accept(host, None).unwrap());
    assert!(matches!(
        bootstrap::connect(peer, InputRequest::Required(Mode::Cooperative)),
        Err(bootstrap::Error::Input(jackstay::input::Error::Unsupported))
    ));
    let mut accepted = worker.join().unwrap();
    assert_eq!(accepted.media.read(&mut [0]).unwrap(), 0);

    let target = Target::new(Config::default()).unwrap();
    let _existing = target.admit(Mode::Cooperative).unwrap();
    let (host, peer) = pair();
    let worker = thread::spawn(move || bootstrap::accept(host, Some(target)).unwrap());
    let connected = bootstrap::connect(peer, InputRequest::Optional(Mode::Cooperative)).unwrap();
    assert_eq!(connected.input_error, Some(jackstay::input::Error::Busy));
    assert!(connected.input.is_none());
    drop(worker.join().unwrap());
}

#[test]
fn malformed_preface_and_truncated_input_offer_fail_instead_of_downgrading() {
    let (host, mut peer) = pair();
    peer.write_all(b"BADBOOT1\0\0\0\0").unwrap();
    assert!(matches!(bootstrap::accept(host, None), Err(bootstrap::Error::Protocol(_))));
    let (mut host, peer) = pair();
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
    let (host, _peer) = pair();
    let started = Instant::now();
    assert!(matches!(bootstrap::accept(host, None), Err(bootstrap::Error::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut));
    assert!(started.elapsed() < Duration::from_secs(8));
}

// V2 preserves media identity and independent channel lifetimes; every request
// combination negotiates clean optional refusals without consuming media bytes.
#[test]
fn v2_independent_offers_and_refusals() {
    use bootstrap::ChannelRequest;
    for enabled in [false, true] {
        for request in [ChannelRequest::None, ChannelRequest::Optional, ChannelRequest::Required] {
            let (a, b) = pair();
            let w = thread::spawn(move || bootstrap::accept_v2(a, None, enabled).unwrap());
            let c = bootstrap::connect_v2(b, InputRequest::Optional(Mode::Cooperative), request);
            let mut a = w.join().unwrap();
            if !enabled && request == ChannelRequest::Required {
                assert!(c.is_err());
                continue;
            }
            let mut c = c.unwrap();
            assert_eq!(c.input_error, Some(jackstay::input::Error::Unsupported));
            assert_eq!(c.affordances.is_some(), enabled && request != ChannelRequest::None);
            assert_eq!(c.affordances_refused, !enabled && request != ChannelRequest::None);
            c.media.write_all(b"v2").unwrap();
            let mut bytes = [0; 2];
            a.media.read_exact(&mut bytes).unwrap();
            assert_eq!(bytes, *b"v2");
        }
    }
}

// Channel names and versions are explicit offers; malformed offers fail even
// when both requests are optional. No downgrade on a partially read stream.
#[test]
fn v2_malformed_offers_do_not_downgrade() {
    for field in [8, 12, 16, 20, 24, 36] {
        let (mut a, b) = pair();
        let w = thread::spawn(move || {
            let mut request = [0; 24];
            a.read_exact(&mut request).unwrap();
            let mut reply = [0; 48];
            reply[..8].copy_from_slice(b"JSBOOT02");
            reply[24..36].copy_from_slice(b"input\0\0\0\0\0\0\0");
            reply[36..48].copy_from_slice(b"affordances\0");
            reply[field] = 255;
            a.write_all(&reply).unwrap();
        });
        assert!(bootstrap::connect_v2(b, InputRequest::Optional(Mode::Cooperative), bootstrap::ChannelRequest::Optional).is_err());
        w.join().unwrap();
    }
}
// No common affordances version is a clean refusal, with original media usable.
#[test]
fn v2_unknown_affordances_version_is_optional_refusal() {
    let (a, mut b) = pair();
    let w = thread::spawn(move || bootstrap::accept_v2(a, None, true).unwrap());
    let mut request = [0; 24];
    request[..8].copy_from_slice(b"JSBOOT02");
    request[15] = 1;
    request[19] = 2;
    b.write_all(&request).unwrap();
    let mut reply = [0; 48];
    b.read_exact(&mut reply).unwrap();
    assert_eq!(&reply[8..24], &[0; 16]);
    let mut a = w.join().unwrap();
    assert!(a.affordances.is_none());
    b.write_all(b"media").unwrap();
    let mut bytes = [0; 5];
    a.media.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, *b"media");
}
