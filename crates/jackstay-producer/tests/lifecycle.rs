use std::{
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use jackstay::{
    acquisition::{
        arena::{AcquireOutcome, ArenaConfig, FrameDescriptor},
        socket::CpuSetupClient,
    },
    bootstrap::{self, ChannelRequest, InputRequest},
    input::{Config, Event, Mode, Operation, Outcome, Work},
    local::{self, Endpoint, Scope, Transport},
};
use jackstay_producer::{Builder, Frame, Producer};
struct TestProducer(Arc<Mutex<Vec<Operation>>>);
impl Producer for TestProducer {
    fn frame(&mut self) -> Option<Frame> {
        Some(Frame {
            descriptor: FrameDescriptor {
                width: 1,
                height: 1,
                stride: 4,
                ..Default::default()
            },
            bytes: vec![1, 2, 3, 4],
        })
    }
    fn execute(&mut self, w: Work) -> Outcome {
        self.0.lock().unwrap().push(w.operation);
        Outcome::Executed
    }
}
fn config() -> ArenaConfig {
    ArenaConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        payload_capacity: 4,
        memory_budget: 1024 * 1024,
        max_incarnations: 4,
        drain_timeout: Duration::from_secs(5),
    }
}
fn endpoint(suffix: &str) -> Endpoint {
    Endpoint::new(
        Scope::User,
        &format!("toolkit-{}-{suffix}", std::process::id()),
        Transport::LocalStream,
    )
    .unwrap()
}
fn wait(mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < end, "lifecycle timed out");
        thread::sleep(Duration::from_millis(2));
    }
}
// Issue #39: start, input and media from one consumer, disconnect, executor
// cleanup, clean stop, then rebind. Use real sockets and CPU arena collaborators.
#[test]
fn consumer_input_disconnect_and_clean_stop() {
    let ep = endpoint("lifecycle");
    let events = Arc::new(Mutex::new(Vec::new()));
    let source = Builder::new(ep.clone(), config(), Config::default(), TestProducer(events.clone()))
        .start()
        .unwrap();
    let c = bootstrap::connect_v2(
        local::connect(&ep).unwrap().into_stream(),
        InputRequest::Required(Mode::Cooperative),
        ChannelRequest::Required,
    )
    .unwrap();
    let input = c.input.unwrap();
    let mut setup = unsafe { CpuSetupClient::from_stream(c.media) };
    let consumer = setup.attach(1).unwrap();
    input.send(Event::Text("hello".into())).unwrap();
    wait(|| {
        events
            .lock()
            .unwrap()
            .iter()
            .any(|o| matches!(o,Operation::Event(Event::Text(t))if t=="hello"))
    });
    wait(|| {
        if let AcquireOutcome::Frame(frame) = consumer.acquire_latest(0).unwrap() {
            assert_eq!(frame.bytes(), &[1, 2, 3, 4]);
            true
        } else {
            false
        }
    });
    drop(input);
    drop(c.affordances);
    drop(consumer);
    drop(setup);
    wait(|| events.lock().unwrap().iter().any(|o| matches!(o, Operation::Cleanup { .. })));
    source.stop().unwrap();
    assert!(local::connect(&ep).is_err());
    let restarted = Builder::new(ep, config(), Config::default(), TestProducer(events)).start().unwrap();
    restarted.stop().unwrap();
}
// Issue #39: abrupt consumer process death must schedule cleanup and permit
// source stop, without interpreting media EOF as process death proof.
#[test]
fn consumer_death_and_stop() {
    let ep = endpoint("death");
    let events = Arc::new(Mutex::new(Vec::new()));
    let source = Builder::new(ep.clone(), config(), Config::default(), TestProducer(events.clone()))
        .start()
        .unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "consumer_child"])
        .env("JACKSTAY_TOOLKIT_CHILD_ENDPOINT", ep.name())
        .spawn()
        .unwrap();
    wait(|| events.lock().unwrap().iter().any(|o| matches!(o, Operation::Event(_))));
    child.kill().unwrap();
    child.wait().unwrap();
    wait(|| events.lock().unwrap().iter().any(|o| matches!(o, Operation::Cleanup { .. })));
    source.stop().unwrap();
}
#[test]
#[ignore]
fn consumer_child() {
    let name = std::env::var("JACKSTAY_TOOLKIT_CHILD_ENDPOINT").unwrap();
    let ep = Endpoint::new(Scope::User, &name, Transport::LocalStream).unwrap();
    let c = bootstrap::connect_v2(
        local::connect(&ep).unwrap().into_stream(),
        InputRequest::Required(Mode::Cooperative),
        ChannelRequest::None,
    )
    .unwrap();
    let mut setup = unsafe { CpuSetupClient::from_stream(c.media) };
    let _consumer = setup.attach(1).unwrap();
    c.input.as_ref().unwrap().send(Event::Text("child".into())).unwrap();
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}
// Worker bounds include silent bootstrap peers; stop interrupts their reads.
#[test]
fn silent_bootstrap_peer_does_not_block_stop() {
    let ep = endpoint("silent");
    let source = Builder::new(ep.clone(), config(), Config::default(), TestProducer(Arc::default()))
        .max_connections(1)
        .start()
        .unwrap();
    let _peer = local::connect(&ep).unwrap();
    let start = Instant::now();
    source.stop().unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
}
// Media EOF does not close input or affordances; trigger another accept to
// exercise connection reaping, then verify both independent channels progress.
#[test]
fn media_disconnect_keeps_other_channels() {
    let ep = endpoint("independent");
    let events = Arc::new(Mutex::new(Vec::new()));
    let source = Builder::new(ep.clone(), config(), Config::default(), TestProducer(events.clone()))
        .start()
        .unwrap();
    let c = bootstrap::connect_v2(
        local::connect(&ep).unwrap().into_stream(),
        InputRequest::Required(Mode::Cooperative),
        ChannelRequest::Required,
    )
    .unwrap();
    drop(c.media);
    thread::sleep(Duration::from_millis(20));
    let _second = local::connect(&ep).unwrap();
    c.input.as_ref().unwrap().send(Event::Text("after media".into())).unwrap();
    wait(|| {
        events
            .lock()
            .unwrap()
            .iter()
            .any(|o| matches!(o,Operation::Event(Event::Text(t))if t=="after media"))
    });
    c.affordances.as_ref().unwrap().publish(Default::default()).unwrap();
    drop(c.input);
    drop(c.affordances);
    source.stop().unwrap();
}
// Stop must drop active input servers before freeing Target, execute their
// cleanup barrier, and finish even while the consumer still owns its input end.
#[test]
fn stop_cleans_up_an_active_controller() {
    let ep = endpoint("active-stop");
    let events = Arc::new(Mutex::new(Vec::new()));
    let source = Builder::new(ep.clone(), config(), Config::default(), TestProducer(events.clone()))
        .start()
        .unwrap();
    let c = bootstrap::connect_v2(
        local::connect(&ep).unwrap().into_stream(),
        InputRequest::Required(Mode::Cooperative),
        ChannelRequest::None,
    )
    .unwrap();
    c.input.as_ref().unwrap().send(Event::Text("active".into())).unwrap();
    wait(|| events.lock().unwrap().iter().any(|o| matches!(o, Operation::Event(_))));
    source.stop().unwrap();
    assert!(events.lock().unwrap().iter().any(|o| matches!(o, Operation::Cleanup { .. })));
    drop(c);
}
// Descriptor size changes replace CPU storage, preserve the admitted consumer,
// and publish matching bytes only after the consumer installs the new mapping.
#[test]
fn frame_resize_reconfigures_existing_consumer() {
    use std::sync::atomic::{AtomicU32, Ordering};
    struct Resizing(Arc<AtomicU32>);
    impl Producer for Resizing {
        fn frame(&mut self) -> Option<Frame> {
            let width = self.0.load(Ordering::Acquire);
            if width == 0 {
                return None;
            }
            Some(Frame {
                descriptor: FrameDescriptor {
                    width,
                    height: 1,
                    stride: width * 4,
                    ..Default::default()
                },
                bytes: vec![7; width as usize * 4],
            })
        }
        fn execute(&mut self, _: Work) -> Outcome {
            Outcome::Executed
        }
    }
    let ep = endpoint("resize");
    let width = Arc::new(AtomicU32::new(0));
    let source = Builder::new(ep.clone(), config(), Config::default(), Resizing(width.clone()))
        .start()
        .unwrap();
    let c = bootstrap::connect_v2(local::connect(&ep).unwrap().into_stream(), InputRequest::None, ChannelRequest::None).unwrap();
    let mut setup = unsafe { CpuSetupClient::from_stream(c.media) };
    let mut consumer = setup.attach(1).unwrap();
    width.store(1, Ordering::Release);
    wait(|| matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Frame(_)));
    width.store(2, Ordering::Release);
    wait(|| setup.install_configuration(&mut consumer).unwrap().is_some());
    wait(|| {
        if let AcquireOutcome::Frame(f) = consumer.acquire_latest(0).unwrap() {
            f.descriptor().width == 2 && f.bytes() == [7; 8]
        } else {
            false
        }
    });
    drop(consumer);
    drop(setup);
    source.stop().unwrap();
}

// #56 toolkit behavior: malformed bodies for disabled and withdrawn verbs
// remain ignorable; enabled malformed bodies close only the controls channel.
// Raw wire peer stands in for an untrusted host at the socket boundary.
#[cfg(unix)]
#[test]
fn malformed_ignored_verbs_never_reach_callbacks() {
    use std::{
        io::{Read, Write},
        sync::atomic::{AtomicU32, Ordering},
    };

    use jackstay::affordances::{Domain, Event as AffEvent, Media, Snapshot};
    struct Controls {
        mode: Arc<AtomicU32>,
        events: Arc<Mutex<Vec<AffEvent>>>,
    }
    impl Producer for Controls {
        fn frame(&mut self) -> Option<Frame> {
            None
        }
        fn execute(&mut self, _: Work) -> Outcome {
            Outcome::Executed
        }
        fn snapshots(&mut self) -> Vec<Snapshot> {
            let mode = self.mode.load(Ordering::Acquire);
            vec![if mode == 1 {
                Snapshot::Withdraw(Domain::Media)
            } else {
                Snapshot::Media(Media {
                    status: "paused".into(),
                    position: None,
                    rate: 0.,
                    duration: None,
                    title: None,
                    artwork: None,
                    capabilities: std::collections::BTreeMap::from([("seek_absolute".into(), mode == 2)]),
                })
            }]
        }
        fn affordance(&mut self, e: AffEvent) {
            self.events.lock().unwrap().push(e)
        }
    }
    fn send(s: &mut local::Stream, json: &str) {
        s.write_all(&(json.len() as u32).to_be_bytes()).unwrap();
        s.write_all(json.as_bytes()).unwrap();
    }
    fn receive(s: &mut local::Stream) -> String {
        let mut size = [0; 4];
        s.read_exact(&mut size).unwrap();
        let mut bytes = vec![0; u32::from_be_bytes(size) as usize];
        s.read_exact(&mut bytes).unwrap();
        String::from_utf8(bytes).unwrap()
    }
    let ep = endpoint("ignored-verbs");
    let mode = Arc::new(AtomicU32::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let source = Builder::new(
        ep.clone(),
        config(),
        Config::default(),
        Controls {
            mode: mode.clone(),
            events: events.clone(),
        },
    )
    .start()
    .unwrap();
    let mut media = local::connect(&ep).unwrap().into_stream();
    media.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut request = [0; 24];
    request[..8].copy_from_slice(b"JSBOOT02");
    request[15] = 1;
    request[19] = 1;
    media.write_all(&request).unwrap();
    let mut reply = [0; 48];
    media.read_exact(&mut reply).unwrap();
    let fd = jackstay::fdpass::recv_fd(&media).unwrap();
    let mut controls = local::Stream::from(fd);
    media.write_all(&[1]).unwrap();
    controls.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    assert!(receive(&mut controls).contains("seek_absolute"));
    for withdrawn in [false, true] {
        if withdrawn {
            mode.store(1, Ordering::Release);
            assert!(receive(&mut controls).contains("\"body\":null"));
        }
        for body in ["", " ,\"body\":null", " ,\"body\":[]", " ,\"body\":{\"position\":\"bad\"}"] {
            send(
                &mut controls,
                &format!("{{\"version\":1,\"domain\":\"media\",\"domain_version\":1,\"kind\":\"verb\",\"verb\":\"seek_absolute\"{body}}}"),
            );
        }
        let count = events.lock().unwrap().len();
        send(
            &mut controls,
            "{\"version\":1,\"domain\":\"presentation\",\"domain_version\":1,\"kind\":\"snapshot\",\"body\":{\"visible\":true,\"focused\":false,\"scale\":1,\"preferred_size\":null}}",
        );
        wait(|| events.lock().unwrap().len() > count);
        assert!(events.lock().unwrap().iter().all(|e| matches!(e, AffEvent::Snapshot(_))));
    }
    mode.store(2, Ordering::Release);
    assert!(receive(&mut controls).contains("seek_absolute"));
    send(
        &mut controls,
        "{\"version\":1,\"domain\":\"media\",\"domain_version\":1,\"kind\":\"verb\",\"verb\":\"seek_absolute\",\"body\":null}",
    );
    wait(|| events.lock().unwrap().contains(&AffEvent::Closed));
    assert!(local::is_alive(&media));
    source.stop().unwrap();
}

// Review #57: all four user callback seams may panic. Complete in-flight input
// as uncertain, close the owners, execute the cleanup barrier and report failure.
#[test]
fn callback_panics_still_run_ordered_shutdown() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use jackstay::affordances::Event as AffEvent;
    struct Panics {
        stage: u32,
        trigger: Arc<AtomicBool>,
        cleanups: Arc<AtomicU32>,
    }
    use std::sync::atomic::AtomicU32;
    impl Panics {
        fn fire(&self, stage: u32) {
            if self.stage == stage && self.trigger.swap(false, Ordering::AcqRel) {
                panic!("callback stage {stage}")
            }
        }
    }
    impl Producer for Panics {
        fn frame(&mut self) -> Option<Frame> {
            self.fire(1);
            None
        }
        fn snapshots(&mut self) -> Vec<jackstay::affordances::Snapshot> {
            self.fire(2);
            Vec::new()
        }
        fn affordance(&mut self, _: AffEvent) {
            self.fire(3)
        }
        fn execute(&mut self, w: Work) -> Outcome {
            if matches!(w.operation, Operation::Event(_)) {
                self.fire(0)
            } else {
                self.cleanups.fetch_add(1, Ordering::AcqRel);
            }
            Outcome::Executed
        }
    }
    for stage in 0..4 {
        let ep = endpoint(&format!("panic-{stage}"));
        let trigger = Arc::new(AtomicBool::new(false));
        let cleanups = Arc::new(AtomicU32::new(0));
        let source = Builder::new(
            ep.clone(),
            config(),
            Config::default(),
            Panics {
                stage,
                trigger: trigger.clone(),
                cleanups: cleanups.clone(),
            },
        )
        .start()
        .unwrap();
        let c = bootstrap::connect_v2(
            local::connect(&ep).unwrap().into_stream(),
            InputRequest::Required(Mode::Cooperative),
            ChannelRequest::Required,
        )
        .unwrap();
        trigger.store(true, Ordering::Release);
        if stage == 0 {
            c.input.as_ref().unwrap().send(Event::Text("panic".into())).unwrap();
        } else if stage == 3 {
            c.affordances.as_ref().unwrap().publish(Default::default()).unwrap();
        }
        wait(|| !trigger.load(Ordering::Acquire));
        wait(|| source.is_finished());
        let error = source.stop().unwrap_err();
        assert!(error.to_string().contains("producer callback panicked"));
        assert!(cleanups.load(Ordering::Acquire) > 0);
        assert!(local::connect(&ep).is_err());
        drop(c);
    }
}
// Review #57: a silent peer occupies the one worker slot. Additional peers are
// visibly disconnected and stop interrupts the admitted peer's stalled read.
#[test]
fn connection_limit_rejects_peers_while_bootstrap_stalls() {
    let ep = endpoint("limit");
    let source = Builder::new(ep.clone(), config(), Config::default(), TestProducer(Arc::default()))
        .max_connections(1)
        .start()
        .unwrap();
    let first = local::connect(&ep).unwrap();
    let second = local::connect(&ep).unwrap();
    wait(|| !second.is_alive());
    assert!(first.is_alive());
    source.stop().unwrap();
}

#[test]
fn recycling_scaled_frames_preserves_storage_and_logical_geometry() {
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    struct Scaled {
        width: Arc<AtomicU32>,
        returned: Arc<AtomicUsize>,
        bytes: Option<Vec<u8>>,
        pointer: usize,
    }
    impl Producer for Scaled {
        fn frame(&mut self) -> Option<Frame> {
            let width = self.width.load(Ordering::Acquire);
            if width == 0 {
                return None;
            }
            let mut bytes = self.bytes.take().expect("previous storage returned");
            assert_eq!(bytes.as_ptr() as usize, self.pointer);
            bytes.resize(width as usize * 4, 7);
            Some(Frame {
                descriptor: FrameDescriptor {
                    width,
                    height: 1,
                    stride: width * 4,
                    ..Default::default()
                },
                bytes,
            })
        }
        fn recycle(&mut self, frame: Frame) {
            assert_eq!(frame.bytes.as_ptr() as usize, self.pointer);
            self.bytes = Some(frame.bytes);
            self.returned.fetch_add(1, Ordering::Release);
        }
        fn input_size(&mut self, _: u32, _: u32) -> (f64, f64) {
            (1., 1.)
        }
        fn execute(&mut self, _: Work) -> Outcome {
            Outcome::Executed
        }
    }
    let width = Arc::new(AtomicU32::new(0));
    let returned = Arc::new(AtomicUsize::new(0));
    let bytes = Vec::with_capacity(8);
    let pointer = bytes.as_ptr() as usize;
    let ep = endpoint("scaled-recycle");
    let source = Builder::new(
        ep.clone(),
        config(),
        Config {
            geometry: jackstay::input::Geometry {
                revision: 1,
                width: 1.,
                height: 1.,
            },
            ..Default::default()
        },
        Scaled {
            width: width.clone(),
            returned: returned.clone(),
            bytes: Some(bytes),
            pointer,
        },
    )
    .start()
    .unwrap();
    let connected = bootstrap::connect_v2(
        local::connect(&ep).unwrap().into_stream(),
        InputRequest::Required(Mode::Cooperative),
        ChannelRequest::None,
    )
    .unwrap();
    let input = connected.input.unwrap();
    let mut setup = unsafe { CpuSetupClient::from_stream(connected.media) };
    let mut consumer = setup.attach(1).unwrap();
    width.store(1, Ordering::Release);
    wait(|| returned.load(Ordering::Acquire) >= 2);
    width.store(2, Ordering::Release);
    wait(|| setup.install_configuration(&mut consumer).unwrap().is_some());
    wait(|| matches!(consumer.acquire_latest(0).unwrap(), AcquireOutcome::Frame(f) if f.descriptor().width == 2));
    // Input remains in logical units even after replacement by a 2x buffer.
    assert_eq!(input.welcome().config.geometry.width, 1.);
    assert_eq!(input.welcome().config.geometry.height, 1.);
    drop((input, consumer, setup));
    source.stop().unwrap();
}
