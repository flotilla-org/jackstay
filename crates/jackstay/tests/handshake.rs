//! Handshake v3 contract over a local connection: a Unix socket, or a Windows pipe.

use std::{
    io::{Read, Write},
    thread,
    time::{Duration, Instant},
};

use jackstay::{
    handshake::{self, Admission, Bytes, Direction, Kind, Offer, Refusal, Selected, Track},
    input::{self, Config, Event, Mode, Operation, Outcome, Status, Target},
    local::Stream,
    model::ClockDomain,
};

#[cfg(unix)]
fn pair() -> (Stream, Stream) {
    Stream::pair().unwrap()
}

#[cfg(windows)]
fn pair() -> (Stream, Stream) {
    jackstay::local::pipe_pair().unwrap()
}

fn track(id: u64, kind: Kind, direction: Direction) -> Track {
    Track {
        id,
        kind,
        direction,
        label: "default".into(),
        descriptor: Bytes::default(),
    }
}

/// Video 1, input 2 and affordances 3.
fn offer() -> Offer {
    Offer {
        source: "test source".into(),
        clock: ClockDomain::HostTime,
        tracks: vec![
            track(1, Kind::Video, Direction::FromSource),
            track(2, Kind::Input, Direction::ToSource),
            track(3, Kind::Affordances, Direction::Both),
        ],
    }
}

fn select(id: u64, required: bool, write: bool) -> Selected {
    Selected { id, required, write }
}

fn admitted(id: u64) -> Admission {
    Admission { id, refusal: None }
}

fn refused(id: u64, refusal: Refusal) -> Admission {
    Admission {
        id,
        refusal: Some(refusal),
    }
}

/// Run both sides; the host result comes back from its worker.
fn exchange(
    offer: Offer,
    target: Option<Target>,
    selection: &[Selected],
) -> (
    Result<handshake::Accepted, handshake::Error>,
    Result<handshake::Connected, handshake::Error>,
) {
    let (host, peer) = pair();
    let worker = thread::spawn(move || handshake::accept(host, &offer, target));
    let offered = handshake::connect(peer).unwrap();
    assert_eq!(offered.offer().source, "test source");
    assert_eq!(offered.offer().clock, ClockDomain::HostTime);
    let connected = offered.select(selection, Mode::Cooperative);
    (worker.join().unwrap(), connected)
}

#[test]
fn offer_selection_and_admission_carry_video_input_and_affordances() {
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
    let selection = [select(1, true, false), select(2, true, true), select(3, false, false)];
    let (accepted, connected) = exchange(offer(), Some(target.clone()), &selection);
    let (mut accepted, mut connected) = (accepted.unwrap(), connected.unwrap());
    let expected = vec![admitted(1), admitted(2), admitted(3)];
    assert_eq!((&accepted.admissions, &connected.admissions), (&expected, &expected));
    assert!(connected.input_error.is_none());
    assert!(accepted.affordances.is_some() && connected.affordances.is_some());
    // The video track continues on the original connection, unconsumed.
    connected.media.write_all(b"media setup").unwrap();
    let mut message = [0; 11];
    accepted.media.read_exact(&mut message).unwrap();
    assert_eq!(&message, b"media setup");
    // Input keeps progressing on its own channel when media has closed.
    drop(connected.media);
    drop(accepted.media);
    let client = connected.input.unwrap();
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
fn optional_refusals_are_reported_and_required_refusals_close_setup() {
    // Without input authority, input is unavailable; video cannot be written;
    // 9 is not offered. All optional: media stays usable.
    let selection = [select(1, true, true), select(2, false, true), select(9, false, false)];
    let (accepted, connected) = exchange(offer(), None, &selection[1..]);
    let (mut accepted, mut connected) = (accepted.unwrap(), connected.unwrap());
    let expected = vec![refused(2, Refusal::Unavailable), refused(9, Refusal::Unknown)];
    assert_eq!((&accepted.admissions, &connected.admissions), (&expected, &expected));
    assert!(connected.input.is_none() && accepted.input.is_none());
    connected.media.write_all(b"ok").unwrap();
    let mut message = [0; 2];
    accepted.media.read_exact(&mut message).unwrap();
    assert_eq!(&message, b"ok");

    let (accepted, connected) = exchange(offer(), None, &selection[..1]);
    assert!(matches!(
        accepted,
        Err(handshake::Error::Refused {
            id: 1,
            refusal: Refusal::NotWritable
        })
    ));
    assert!(matches!(
        connected,
        Err(handshake::Error::Refused {
            id: 1,
            refusal: Refusal::NotWritable
        })
    ));

    // A busy input target is a clean input admission rejection: reported for
    // optional input, fatal for required input.
    let target = Target::new(Config::default()).unwrap();
    let _existing = target.admit(Mode::Cooperative).unwrap();
    let (_accepted, connected) = exchange(offer(), Some(target.clone()), &[select(2, false, true)]);
    let connected = connected.unwrap();
    assert_eq!(connected.input_error, Some(input::Error::Busy));
    assert!(connected.input.is_none());
    let (_accepted, connected) = exchange(offer(), Some(target), &[select(2, true, true)]);
    assert!(matches!(connected, Err(handshake::Error::Input(input::Error::Busy))));
}

#[test]
fn app_kinds_carry_opaque_descriptors_up_to_four_kibibytes() {
    let mut app = track(u64::MAX, Kind::Other("org.example.cue".into()), Direction::FromSource);
    app.descriptor = Bytes(vec![7; 4096]);
    let mut with_app = offer();
    with_app.tracks.push(app.clone());
    let (host, peer) = pair();
    let host_offer = with_app.clone();
    let worker = thread::spawn(move || handshake::accept(host, &host_offer, None));
    let offered = handshake::connect(peer).unwrap();
    assert_eq!(offered.offer(), &with_app);
    let connected = offered.select(&[select(u64::MAX, true, false)], Mode::Cooperative).unwrap();
    assert_eq!(connected.admissions, vec![admitted(u64::MAX)]);
    assert_eq!(worker.join().unwrap().unwrap().admissions, vec![admitted(u64::MAX)]);

    // Oversized descriptors and kinds that are neither core nor reverse-domain
    // are never offered.
    for (kind, len) in [("org.example.cue", 4097), ("audio", 0), ("Org.example", 0)] {
        let mut bad = offer();
        bad.tracks.push(Track {
            descriptor: Bytes(vec![0; len]),
            ..track(4, Kind::Other(kind.into()), Direction::FromSource)
        });
        let (host, _peer) = pair();
        assert!(handshake::accept(host, &bad, None).is_err(), "{kind} {len}");
    }
}

#[test]
fn offers_carry_at_most_one_track_of_each_core_kind() {
    // One incarnation carries one media stream and one channel per kind.
    for kind in [Kind::Video, Kind::Input, Kind::Affordances] {
        let mut twice = offer();
        twice.tracks.push(track(9, kind.clone(), Direction::Both));
        let (host, _peer) = pair();
        assert!(handshake::accept(host, &twice, None).is_err(), "{kind:?}");
    }
}

#[test]
fn consumers_reject_their_own_invalid_selection_before_sending() {
    let (host, peer) = pair();
    let worker = thread::spawn(move || handshake::accept(host, &offer(), None));
    let offered = handshake::connect(peer).unwrap();
    let result = offered.select(&[select(1, false, false), select(1, false, false)], Mode::Cooperative);
    assert!(matches!(result, Err(handshake::Error::Protocol("invalid selection"))));
    assert!(worker.join().unwrap().is_err());
}

#[test]
fn malformed_frames_fail_instead_of_downgrading() {
    let (host, mut peer) = pair();
    let worker = thread::spawn(move || handshake::accept(host, &offer(), None));
    let mut frame = [0; 12];
    peer.read_exact(&mut frame).unwrap();
    assert_eq!(&frame[..8], &handshake::MAGIC);
    peer.write_all(b"JSBOOT02\0\0\0\0").unwrap();
    assert!(matches!(worker.join().unwrap(), Err(handshake::Error::Message(_))));
}

#[test]
fn silent_peer_cannot_hold_the_handshake_indefinitely() {
    let (host, _peer) = pair();
    let started = Instant::now();
    assert!(matches!(
        handshake::accept(host, &offer(), None),
        Err(handshake::Error::Io(error)) if error.kind() == std::io::ErrorKind::TimedOut
    ));
    assert!(started.elapsed() < Duration::from_secs(8));
}
