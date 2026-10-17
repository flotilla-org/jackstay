//! Playback track contract, in-process over a local connection: offer,
//! selection, ring transfer, reading, the doorbell and notices while attached.

use std::{
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use jackstay::{
    acquisition::{
        arena::{
            AcquireOutcome, ArenaConfig, ArenaConsumer, ArenaProducer, Cancellation, FrameDescriptor, PublishOutcome, WaitInterest,
            WaitOutcome,
        },
        socket::{CpuSetupClient, serve_cpu_with_doorbell},
    },
    audio::{Descriptor, Doorbell, Format, Layout, PlaybackReader, PlaybackRing, PlaybackWriter, Sample, Settings},
    handshake::{self, Accepted, Connected, Direction, Kind, Notice, Notices, Offer, Refusal, Selected, Track},
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

const STEREO: Format = Format {
    rate: 48_000,
    channels: 2,
    layout: Layout::Stereo,
    sample: Sample::S16,
};
/// 10 ms blocks of 480 frames, 4 bytes each.
const FRAMES: u32 = 480;
const BLOCK: usize = 480 * 4;

fn writer(id: u64) -> PlaybackWriter {
    PlaybackWriter::new(
        id,
        Settings {
            format: STEREO,
            block_frames: FRAMES,
            depth: Duration::from_millis(250),
        },
    )
    .unwrap()
}

/// Block `n` of a writer: every byte is `n`.
fn write(writer: &mut PlaybackWriter, n: u8) -> u64 {
    writer.write(&[n; BLOCK], u64::from(n) * 10_000_000).unwrap()
}

fn connect(writers: &[&PlaybackWriter], selection: &[Selected]) -> (Accepted, Connected) {
    let offer = Offer {
        source: "test source".into(),
        clock: ClockDomain::HostTime,
        tracks: writers.iter().map(|w| w.track("default")).collect(),
    };
    let rings: Vec<_> = writers.iter().map(|w| w.playback()).collect();
    let (host, peer) = pair();
    let worker = thread::spawn(move || handshake::accept(host, &offer, None, &rings));
    let connected = handshake::connect(peer).unwrap().select(selection, Mode::Cooperative).unwrap();
    (worker.join().unwrap().unwrap(), connected)
}

fn select(id: u64) -> Selected {
    Selected {
        id,
        required: true,
        write: false,
    }
}

#[test]
fn offer_carries_the_descriptor_and_settings_floor_the_depth() {
    let writer = writer(7);
    let (host, peer) = pair();
    let offer = Offer {
        source: "test source".into(),
        clock: ClockDomain::HostTime,
        tracks: vec![writer.track("default")],
    };
    let rings = [writer.playback()];
    let worker = thread::spawn(move || handshake::accept(host, &offer, None, &rings));
    let offered = handshake::connect(peer).unwrap();
    let track = &offered.offer().tracks[0];
    assert_eq!(track.kind, Kind::Playback);
    let descriptor = Descriptor::decode(&track.descriptor).unwrap();
    // 250 ms at 48 kHz in 10 ms blocks.
    assert_eq!(
        descriptor,
        Descriptor {
            format: STEREO,
            block_frames: FRAMES,
            blocks: 25,
        }
    );
    // Playback flows only from the source.
    let refused = offered
        .select(
            &[Selected {
                id: 7,
                required: false,
                write: true,
            }],
            Mode::Cooperative,
        )
        .unwrap();
    assert_eq!(refused.admissions[0].refusal, Some(Refusal::NotWritable));
    assert!(refused.playback.is_empty() && refused.doorbell.is_none());
    worker.join().unwrap().unwrap();

    let shallow = Settings {
        format: STEREO,
        block_frames: FRAMES,
        depth: Duration::from_millis(249),
    };
    assert!(PlaybackWriter::new(1, shallow).is_err());
    // A playback track offered without its ring is an invalid offer.
    let (host, _peer) = pair();
    let offer = Offer {
        source: "test source".into(),
        clock: ClockDomain::HostTime,
        tracks: vec![writer.track("default")],
    };
    assert!(handshake::accept(host, &offer, None, &[]).is_err());
    // An offered descriptor that disagrees with its ring is an invalid offer.
    let mut track = writer.track("default");
    let mut descriptor = Descriptor::decode(&track.descriptor).unwrap();
    descriptor.block_frames += 1;
    track.descriptor = handshake::Bytes(flotilla_cbor::to_vec(&descriptor, &handshake::LIMITS).unwrap());
    let (host, _peer) = pair();
    let offer = Offer {
        source: "test source".into(),
        clock: ClockDomain::HostTime,
        tracks: vec![track],
    };
    let result = handshake::accept(host, &offer, None, &[writer.playback()]);
    assert!(matches!(result, Err(handshake::Error::Protocol("invalid offer"))));
}

#[test]
fn writes_continue_without_readers_and_never_block() {
    let mut writer = writer(1);
    for n in 0..1000 {
        write(&mut writer, n as u8);
    }
    // A connected consumer that never drains its doorbell does not stall
    // the writer either: the wake coalesces.
    let (_accepted, connected) = connect(&[&writer], &[select(1)]);
    for n in 0..10_000 {
        write(&mut writer, n as u8);
    }
    let reader = PlaybackReader::new(connected.playback[0].clone(), Duration::from_millis(50));
    assert_eq!(reader.available(), 5);
}

#[test]
fn each_consumer_starts_at_cursor_minus_its_latency_target() {
    let mut ahead = writer(1);
    for n in 1..=10 {
        write(&mut ahead, n);
    }
    let (_accepted, connected) = connect(&[&ahead], &[select(1)]);
    let ring = &connected.playback[0];
    let mut near = PlaybackReader::new(ring.clone(), Duration::from_millis(20));
    let mut far = PlaybackReader::new(ring.clone(), Duration::from_millis(45));
    let block = near.read().unwrap();
    assert_eq!((block.cursor, block.sample_position, block.samples[0]), (9, 8 * 480, 9));
    assert!(!block.gap);
    // 45 ms rounds up to five blocks.
    assert_eq!(far.read().unwrap().cursor, 6);
    // A reader joining an empty ring starts with its first block.
    let mut empty = writer(2);
    let (_accepted, connected) = connect(&[&empty], &[select(2)]);
    let mut first = PlaybackReader::new(connected.playback[0].clone(), Duration::from_millis(40));
    assert!(first.read().is_none());
    write(&mut empty, 1);
    assert_eq!(first.read().unwrap().cursor, 1);
}

#[test]
fn underruns_are_counted_locally() {
    let mut writer = writer(1);
    let (_accepted, connected) = connect(&[&writer], &[select(1)]);
    let mut reader = PlaybackReader::new(connected.playback[0].clone(), Duration::ZERO);
    assert!(reader.read().is_none());
    write(&mut writer, 1);
    // Zero latency waits for the next block after the write cursor.
    assert!(reader.read().is_none());
    write(&mut writer, 2);
    assert_eq!(reader.read().unwrap().samples[0], 2);
    assert!(reader.read().is_none());
    assert_eq!((reader.underruns(), reader.gaps()), (3, 0));
}

#[test]
fn a_lapped_reader_resynchronizes_with_a_gap_and_others_are_unaffected() {
    let mut writer = writer(1);
    let (_accepted, connected) = connect(&[&writer], &[select(1)]);
    let mut slow = PlaybackReader::new(connected.playback[0].clone(), Duration::from_millis(30));
    let mut fast = PlaybackReader::new(connected.playback[0].clone(), Duration::from_millis(30));
    write(&mut writer, 1);
    assert_eq!(slow.read().unwrap().cursor, 1);
    assert_eq!(fast.read().unwrap().cursor, 1);
    for n in 2..=100 {
        write(&mut writer, n);
        assert_eq!(fast.read().unwrap().cursor, u64::from(n));
    }
    // The ring holds 25 blocks: cursor 2 is long gone.
    let block = slow.read().unwrap();
    assert!(block.gap);
    assert_eq!((block.cursor, block.samples[0]), (98, 98));
    assert!(!slow.read().unwrap().gap);
    assert_eq!((slow.gaps(), fast.gaps()), (1, 0));
}

#[test]
fn discontinuity_and_reconfiguration_mark_exact_samples() {
    let mut writer = writer(1);
    let (_accepted, connected) = connect(&[&writer], &[select(1)]);
    let mut reader = PlaybackReader::new(connected.playback[0].clone(), Duration::from_millis(250));
    write(&mut writer, 1);
    writer.discontinuity();
    writer.write(&[2; 100 * 4], 5_000_000_000).unwrap();
    let mono = Format {
        rate: 44_100,
        channels: 1,
        layout: Layout::Mono,
        sample: Sample::F32,
    };
    writer.reconfigure(mono).unwrap();
    writer.write(&[3; 64 * 4], 6_000_000_000).unwrap();
    write(&mut writer, 4);

    let first = reader.read().unwrap();
    assert!(!first.discontinuity && !first.reconfiguration);
    let paused = reader.read().unwrap();
    assert!(paused.discontinuity && !paused.reconfiguration);
    assert_eq!(
        (paused.sample_position, paused.host_time_ns, paused.frames),
        (480, 5_000_000_000, 100)
    );
    let changed = reader.read().unwrap();
    assert!(changed.reconfiguration && !changed.discontinuity);
    assert_eq!(
        (changed.sample_position, changed.frames, changed.format, changed.generation),
        (580, 64, mono, 2)
    );
    assert_eq!(changed.samples, &[3; 64 * 4][..]);
    let after = reader.read().unwrap();
    assert!(!after.reconfiguration);
    assert_eq!((after.format, after.generation, after.sample_position), (mono, 2, 644));
    // Same bytes per block: 480 S16 stereo frames are 480 F32 mono frames.
    assert_eq!(after.frames, 480);
    // The new format must fit the block size fixed at creation.
    let wide = Format {
        rate: 48_000,
        channels: 2,
        layout: Layout::Stereo,
        sample: Sample::F32,
    };
    assert!(writer.reconfigure(wide).is_ok());
    assert!(writer.write(&[0; BLOCK * 2], 0).is_err());
}

#[cfg(unix)]
#[test]
fn consumers_map_rings_read_only() {
    use std::os::fd::AsRawFd;

    let mut writer = writer(1);
    let (_accepted, connected) = connect(&[&writer], &[select(1)]);
    let fd = connected.playback[0].try_clone_fd().unwrap();
    // SAFETY: probing mmap of a live fd; a successful mapping is unmapped.
    let mapped = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if mapped != libc::MAP_FAILED {
        // SAFETY: the mapping above, unmapped once.
        unsafe { libc::munmap(mapped, 4096) };
        panic!("a consumer could map its playback ring writable");
    }
    // SAFETY: a plain write and truncation attempt on a live fd.
    assert!(unsafe { libc::write(fd.as_raw_fd(), [0_u8].as_ptr().cast(), 1) } < 0);
    // SAFETY: as above.
    assert!(unsafe { libc::ftruncate(fd.as_raw_fd(), 0) } < 0);
    // The producer keeps writing and the consumer keeps reading.
    write(&mut writer, 1);
    let mut reader = PlaybackReader::new(connected.playback[0].clone(), Duration::from_millis(10));
    assert_eq!(reader.read().unwrap().samples[0], 1);
}

#[test]
fn the_doorbell_hints_changed_tracks_and_cursors_remain_authoritative() {
    let (mut a, mut b) = (writer(1), writer(2));
    // Bits follow selection positions: b is position 0, a is position 1.
    let (accepted, mut connected) = connect(&[&a, &b], &[select(2), select(1)]);
    assert_eq!(connected.playback.iter().map(|r| r.id()).collect::<Vec<_>>(), [2, 1]);
    let doorbell = connected.doorbell.as_mut().unwrap();
    let cancel = Cancellation::new().unwrap();
    assert_eq!(doorbell.wait(&cancel, Some(Duration::from_millis(10))).unwrap(), 0);
    let waiter = thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        write(&mut a, 1);
        a
    });
    assert_eq!(doorbell.wait(&cancel, Some(Duration::from_secs(5))).unwrap(), 0b10);
    let mut a = waiter.join().unwrap();
    write(&mut b, 1);
    write(&mut a, 2);
    write(&mut b, 2);
    assert_eq!(doorbell.wait(&cancel, None).unwrap(), 0b11);
    assert_eq!(doorbell.take(), 0);
    cancel.cancel().unwrap();
    assert_eq!(doorbell.wait(&cancel, None).unwrap(), 0);

    // The hint is optional: without the host's bell, cursors still advance.
    drop(accepted);
    write(&mut b, 3);
    assert_eq!(connected.doorbell.as_ref().unwrap().take(), 0);
    let mut reader = PlaybackReader::new(connected.playback[0].clone(), Duration::from_millis(30));
    assert_eq!(reader.available(), 3);
    assert_eq!(reader.read().unwrap().samples[0], 1);
}

/// What a consumer holds after selecting a CPU-served video track.
struct Video {
    producer: Arc<Mutex<ArenaProducer>>,
    /// Dropping it ends CPU setup, which closes acquisition.
    _client: CpuSetupClient,
    consumer: ArenaConsumer,
    doorbell: Option<Doorbell>,
    playback: Vec<PlaybackRing>,
}

/// Offer a video track (id 9) and `audio`'s playback tracks, make `selection`
/// and attach to the arena over CPU setup on the same connection.
fn connect_video(audio: &[&PlaybackWriter], selection: &[Selected]) -> Video {
    let video = Track {
        id: 9,
        kind: Kind::Video,
        direction: Direction::FromSource,
        label: "screen".into(),
        descriptor: Default::default(),
    };
    let offer = Offer {
        source: "test source".into(),
        clock: ClockDomain::HostTime,
        tracks: std::iter::once(video).chain(audio.iter().map(|w| w.track("default"))).collect(),
    };
    let rings: Vec<_> = audio.iter().map(|w| w.playback()).collect();
    let producer = Arc::new(Mutex::new(
        ArenaProducer::new(ArenaConfig {
            resource_capacity: 4,
            retained_history: 2,
            producer_reserve: 1,
            payload_capacity: 4,
            memory_budget: 1024 * 1024,
            max_incarnations: 1,
            drain_timeout: Duration::from_secs(5),
        })
        .unwrap(),
    ));
    let (host, peer) = pair();
    let served = producer.clone();
    thread::spawn(move || {
        let Accepted {
            media,
            notices,
            video_bell,
            ..
        } = handshake::accept(host, &offer, None, &rings).unwrap();
        // Notices holds the playback doorbell: keep it while serving.
        let _notices = notices;
        serve_cpu_with_doorbell(media, served, video_bell)
    });
    let connected = handshake::connect(peer).unwrap().select(selection, Mode::Cooperative).unwrap();
    // SAFETY: the arena above is this stream's sole, conforming producer; this
    // test never forks or forwards its grant.
    let mut client = unsafe { CpuSetupClient::from_stream(connected.media) };
    let consumer = client.attach(1).unwrap();
    Video {
        producer,
        _client: client,
        consumer,
        doorbell: connected.doorbell,
        playback: connected.playback,
    }
}

fn publish(producer: &Mutex<ArenaProducer>) -> u64 {
    match producer.lock().unwrap().publish(FrameDescriptor::default(), b"rgba").unwrap() {
        PublishOutcome::Published { cursor } => cursor,
        PublishOutcome::Dropped => panic!("frame dropped"),
    }
}

fn latest(consumer: &ArenaConsumer) -> u64 {
    match consumer.acquire_latest(0).unwrap() {
        AcquireOutcome::Frame(frame) => frame.cursor(),
        outcome => panic!("no frame: {outcome:?}"),
    }
}

#[test]
fn one_doorbell_wakes_a_consumer_for_video_and_playback() {
    let mut audio = writer(1);
    // Video is selection position 0 and playback position 1.
    let Video {
        producer,
        consumer,
        doorbell,
        playback,
        _client: _setup,
    } = connect_video(&[&audio], &[select(9), select(1)]);
    let mut doorbell = doorbell.unwrap();
    let mut reader = PlaybackReader::new(playback[0].clone(), Duration::from_millis(250));
    let cancel = Cancellation::new().unwrap();
    assert_eq!(doorbell.wait(&cancel, Some(Duration::from_millis(10))).unwrap(), 0);
    let waiter = thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        publish(&producer);
        producer
    });
    assert_eq!(doorbell.wait(&cancel, Some(Duration::from_secs(5))).unwrap(), 0b01);
    let producer = waiter.join().unwrap();
    assert_eq!(latest(&consumer), 1);
    write(&mut audio, 1);
    assert_eq!(doorbell.wait(&cancel, Some(Duration::from_secs(5))).unwrap(), 0b10);
    assert_eq!(reader.read().unwrap().samples[0], 1);
    publish(&producer);
    write(&mut audio, 2);
    assert_eq!(doorbell.wait(&cancel, None).unwrap(), 0b11);
    assert_eq!((latest(&consumer), reader.read().unwrap().samples[0]), (2, 2));
    // Closure rings video's bit too, and acquisition reports it.
    producer.lock().unwrap().stop();
    assert_eq!(doorbell.wait(&cancel, Some(Duration::from_secs(5))).unwrap(), 0b01);
    assert!(matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Closed));
}

#[test]
fn a_video_only_selection_gets_the_doorbell_and_keeps_the_arena_wait() {
    let Video {
        producer,
        mut consumer,
        doorbell,
        playback,
        _client: _setup,
    } = connect_video(&[], &[select(9)]);
    assert!(playback.is_empty());
    let mut doorbell = doorbell.unwrap();
    let cancel = Cancellation::new().unwrap();
    let observed = consumer.events();
    publish(&producer);
    assert_eq!(doorbell.wait(&cancel, Some(Duration::from_secs(5))).unwrap(), 0b1);
    // The arena's own wait still works for consumers that use it.
    let outcome = consumer
        .wait(observed, WaitInterest::DATA, &cancel, Some(Duration::from_secs(5)))
        .unwrap();
    assert!(matches!(outcome, WaitOutcome::Changed(events) if events.data_cursor == 1));
}

/// Answer one late selection on a host worker that polls with a zero timeout.
fn serve(mut notices: Notices) -> thread::JoinHandle<Notices> {
    thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !notices.serve(Duration::ZERO).unwrap() {
            assert!(std::time::Instant::now() < deadline);
        }
        notices
    })
}

#[test]
fn a_track_added_after_attach_is_selected_late_without_disturbing_others() {
    let (mut a, mut b) = (writer(1), writer(2));
    let (accepted, mut connected) = connect(&[], &[]);
    let (mut notices, tracks) = (accepted.notices, &mut connected.tracks);
    assert!(tracks.next(Duration::ZERO).unwrap().is_none());
    // A ring belongs only with a playback track, and playback needs one.
    assert!(notices.add(a.track("default"), None).is_err());
    let app = Track {
        id: 3,
        kind: Kind::Other("org.example.cue".into()),
        direction: Direction::FromSource,
        label: "cue".into(),
        descriptor: handshake::Bytes(vec![1]),
    };
    assert!(notices.add(app.clone(), Some(a.playback())).is_err());
    notices.add(a.track("default"), Some(a.playback())).unwrap();
    // A zero timeout polls: the notice already sent is returned.
    assert_eq!(
        tracks.next(Duration::ZERO).unwrap(),
        Some(Notice::Added { track: a.track("default") })
    );
    let server = serve(notices);
    let late = tracks.select(select(1)).unwrap();
    let mut notices = server.join().unwrap();
    assert_eq!((late.admission.refusal, late.position), (None, 0));
    // The incarnation's first playback track brings the doorbell.
    let (mut doorbell, ring) = (late.doorbell.unwrap(), late.playback.unwrap());
    let mut reader_a = PlaybackReader::new(ring, Duration::from_millis(250));
    write(&mut a, 1);

    // Selecting before reading the notice queues it; a's reader is undisturbed.
    notices.add(b.track("cue"), Some(b.playback())).unwrap();
    let server = serve(notices);
    let late = tracks.select(select(2)).unwrap();
    let mut notices = server.join().unwrap();
    assert!(late.doorbell.is_none() && late.position == 1);
    assert!(matches!(tracks.next(Duration::ZERO).unwrap(), Some(Notice::Added { track }) if track.id == 2));
    // An app-kind track is selected late without a ring; writing a
    // source-bound track is refused.
    notices.add(app, None).unwrap();
    let server = serve(notices);
    let writing = Selected {
        id: 3,
        required: false,
        write: true,
    };
    assert_eq!(tracks.select(writing).unwrap().admission.refusal, Some(Refusal::NotWritable));
    let server = serve(server.join().unwrap());
    let cue = tracks.select(select(3)).unwrap();
    // Notices holds the doorbell: keep it.
    let _notices = server.join().unwrap();
    assert!(cue.admission.refusal.is_none() && cue.playback.is_none() && cue.doorbell.is_none());
    let mut reader_b = PlaybackReader::new(late.playback.unwrap(), Duration::from_millis(250));
    write(&mut a, 2);
    write(&mut b, 1);
    let cancel = Cancellation::new().unwrap();
    assert_eq!(doorbell.wait(&cancel, None).unwrap(), 0b11);
    assert_eq!((reader_a.read().unwrap().cursor, reader_a.read().unwrap().cursor), (1, 2));
    assert_eq!(reader_b.read().unwrap().samples[0], 1);
}

#[test]
fn a_reopen_updates_the_track_and_keeps_the_selection() {
    let mut writer = writer(1);
    let (accepted, mut connected) = connect(&[&writer], &[select(1)]);
    let mut notices = accepted.notices;
    let mut reader = PlaybackReader::new(connected.playback[0].clone(), Duration::from_millis(250));
    write(&mut writer, 1);
    let mono = Format {
        rate: 44_100,
        channels: 1,
        layout: Layout::Mono,
        sample: Sample::F32,
    };
    writer.reconfigure(mono).unwrap();
    notices.update(writer.track("default")).unwrap();
    let Some(Notice::Updated { track }) = connected.tracks.next(Duration::from_secs(5)).unwrap() else {
        panic!("expected an update");
    };
    assert_eq!(Descriptor::decode(&track.descriptor).unwrap().format, mono);
    assert_eq!(connected.tracks.tracks(), [track]);
    write(&mut writer, 2);
    assert!(!reader.read().unwrap().reconfiguration);
    let block = reader.read().unwrap();
    assert!(block.reconfiguration && block.format == mono);
    // Still selected: selecting it again is refused.
    let server = serve(notices);
    let again = connected.tracks.select(select(1)).unwrap();
    assert_eq!(again.admission.refusal, Some(Refusal::Unavailable));
    let mut notices = server.join().unwrap();
    // A descriptor the ring cannot hold is not an update.
    let mut track = writer.track("default");
    let mut descriptor = Descriptor::decode(&track.descriptor).unwrap();
    descriptor.blocks += 1;
    track.descriptor = handshake::Bytes(flotilla_cbor::to_vec(&descriptor, &handshake::LIMITS).unwrap());
    assert!(notices.update(track).is_err());
}

#[test]
fn removal_ends_the_read_state_cleanly() {
    let mut writer = writer(1);
    let (accepted, mut connected) = connect(&[&writer], &[select(1)]);
    let mut notices = accepted.notices;
    let mut reader = PlaybackReader::new(connected.playback.remove(0), Duration::from_millis(250));
    write(&mut writer, 1);
    write(&mut writer, 2);
    let track = writer.track("default");
    notices.remove(1).unwrap();
    drop(writer);
    assert_eq!(
        connected.tracks.next(Duration::from_secs(5)).unwrap(),
        Some(Notice::Removed { id: 1 })
    );
    assert!(connected.tracks.tracks().is_empty());
    // Blocks already written stay readable, then the ring is quiet.
    assert_eq!(reader.read().unwrap().samples[0], 1);
    assert_eq!(reader.read().unwrap().samples[0], 2);
    assert!(reader.read().is_none());
    drop(reader);
    let server = serve(notices);
    assert_eq!(
        connected.tracks.select(select(1)).unwrap().admission.refusal,
        Some(Refusal::Unknown)
    );
    let mut notices = server.join().unwrap();
    // IDs are never reused.
    let reused = PlaybackWriter::new(
        1,
        Settings {
            format: STEREO,
            block_frames: FRAMES,
            depth: Duration::from_millis(250),
        },
    )
    .unwrap();
    assert!(notices.add(track, Some(reused.playback())).is_err());
    // The consumer's going ends the host's channel.
    drop(connected);
    assert!(notices.serve(Duration::from_secs(5)).is_err());
}
