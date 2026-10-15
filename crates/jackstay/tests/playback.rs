//! Playback track contract, in-process over a local connection: offer,
//! selection, ring transfer, reading and the doorbell.

use std::{thread, time::Duration};

use jackstay::{
    acquisition::arena::Cancellation,
    audio::{Descriptor, Format, Layout, PlaybackReader, PlaybackWriter, Sample, Settings},
    handshake::{self, Accepted, Connected, Kind, Offer, Refusal, Selected},
    input::Mode,
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
