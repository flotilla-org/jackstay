//! Recording track contract, in-process over a local connection: one
//! admitted writer, refusal of others, writer handover and silence.

use std::{thread, time::Duration};

use jackstay::{
    audio::{Format, Layout, RecordingReader, Sample, Settings},
    handshake::{self, Accepted, Connected, Direction, Kind, Notices, Offer, Refusal, Selected},
    input::Mode,
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

const FORMAT: Format = Format {
    rate: 48_000,
    channels: 1,
    layout: Layout::Mono,
    sample: Sample::U8,
};
/// 10 ms blocks.
const FRAMES: usize = 480;

fn reader() -> RecordingReader {
    let settings = Settings {
        format: FORMAT,
        block_frames: FRAMES as u32,
        depth: Duration::from_millis(250),
    };
    RecordingReader::new(1, settings, Duration::from_millis(20)).unwrap()
}

fn connect(source: &RecordingReader, write: bool) -> (Accepted, Connected) {
    let offer = Offer {
        source: "test source".into(),
        clock: ClockDomain::HostTime,
        tracks: vec![source.track("microphone")],
    };
    let rings = [source.ring()];
    let (host, peer) = pair();
    let worker = thread::spawn(move || handshake::accept(host, &offer, None, &rings));
    let selected = Selected {
        id: 1,
        required: false,
        write,
    };
    let connected = handshake::connect(peer).unwrap().select(&[selected], Mode::Cooperative).unwrap();
    (worker.join().unwrap().unwrap(), connected)
}

/// Assert the source reads a block of silence.
fn silent(source: &mut RecordingReader) {
    let block = source.read().unwrap();
    assert_eq!((block.cursor, block.frames, block.format), (0, FRAMES as u32, FORMAT));
    assert_eq!(block.samples, &[0x80; FRAMES][..]);
}

#[test]
fn one_writer_is_admitted_and_its_blocks_reach_the_source() {
    let mut source = reader();
    let track = source.track("microphone");
    assert_eq!((track.kind, track.direction), (Kind::Recording, Direction::ToSource));
    let (_accepted, mut connected) = connect(&source, true);
    assert_eq!(connected.admissions[0].refusal, None);
    assert!(connected.playback.is_empty() && connected.doorbell.is_none());
    // The read-write mapping: the writer's blocks are the source's.
    let writer = &mut connected.recording[0];
    assert_eq!(writer.id(), 1);
    assert!(source.read().is_none());
    writer.write(&[1; FRAMES], 10).unwrap();
    writer.write(&[2; FRAMES], 20).unwrap();
    let first = source.read().unwrap();
    assert!(first.discontinuity);
    assert_eq!((first.cursor, first.sample_position, first.host_time_ns), (1, 0, 10));
    assert_eq!(first.samples, &[1; FRAMES][..]);
    let second = source.read().unwrap();
    assert!(!second.discontinuity && second.samples[0] == 2);
}

#[test]
fn a_second_writer_and_a_reader_are_refused_without_the_ring() {
    let source = reader();
    let (_writing, _) = connect(&source, true);
    let (_, busy) = connect(&source, true);
    assert_eq!(busy.admissions[0].refusal, Some(Refusal::Unavailable));
    let (_, watching) = connect(&source, false);
    assert_eq!(watching.admissions[0].refusal, Some(Refusal::Unavailable));
    assert!(busy.recording.is_empty() && watching.recording.is_empty());
    // A required writer request is refused cleanly too.
    let offer = Offer {
        source: "test source".into(),
        clock: ClockDomain::HostTime,
        tracks: vec![source.track("microphone")],
    };
    let rings = [source.ring()];
    let (host, peer) = pair();
    let worker = thread::spawn(move || handshake::accept(host, &offer, None, &rings));
    let required = Selected {
        id: 1,
        required: true,
        write: true,
    };
    let result = handshake::connect(peer).unwrap().select(&[required], Mode::Cooperative);
    let refused = handshake::Error::Refused {
        id: 1,
        refusal: Refusal::Unavailable,
    };
    assert_eq!(result.err().unwrap().to_string(), refused.to_string());
    assert!(worker.join().unwrap().is_err());
}

/// Answer one late selection on a host worker.
fn serve(mut notices: Notices) -> Notices {
    assert!(notices.serve(Duration::from_secs(5)).unwrap());
    notices
}

#[test]
fn a_writer_change_starts_at_the_new_writers_blocks_with_a_discontinuity() {
    let mut source = reader();
    let (first, mut a) = connect(&source, true);
    let (second, mut b) = connect(&source, true);
    assert_eq!(b.admissions[0].refusal, Some(Refusal::Unavailable));
    for n in 1..=3 {
        a.recording[0].write(&[n; FRAMES], 0).unwrap();
    }
    assert_eq!(source.read().unwrap().cursor, 2);
    // A's incarnation ends with blocks unread; B selects late.
    drop((first, a));
    silent(&mut source);
    let server = thread::spawn(move || serve(second.notices));
    let late = b.tracks.select(Selected {
        id: 1,
        required: false,
        write: true,
    });
    let _notices = server.join().unwrap();
    let late = late.unwrap();
    assert_eq!(late.admission.refusal, None);
    assert!(late.playback.is_none() && late.doorbell.is_none());
    let mut writer = late.recording.unwrap();
    assert!(source.read().is_none());
    writer.write(&[4; FRAMES], 0).unwrap();
    writer.write(&[5; FRAMES], 0).unwrap();
    let block = source.read().unwrap();
    assert!(block.discontinuity);
    assert_eq!((block.cursor, block.sample_position, block.samples[0]), (4, 0, 4));
    let block = source.read().unwrap();
    assert!(!block.discontinuity && block.samples[0] == 5);
}

#[test]
fn the_source_reads_silence_without_a_writer() {
    let mut source = reader();
    silent(&mut source);
    silent(&mut source);
    let (accepted, mut connected) = connect(&source, true);
    assert!(source.read().is_none());
    connected.recording[0].write(&[7; FRAMES], 0).unwrap();
    assert_eq!(source.read().unwrap().samples[0], 7);
    // Removing the track ends the writer's admission.
    let mut notices = accepted.notices;
    notices.remove(1).unwrap();
    silent(&mut source);
}
