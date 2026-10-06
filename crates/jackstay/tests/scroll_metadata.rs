//! ADR 0003: observable ordered scroll lifecycle through the public target API.
use jackstay::input::*;
fn scroll(phase: Option<ScrollPhase>, momentum_phase: Option<MomentumPhase>) -> Event {
    Event::Scroll {
        x: 0.0,
        y: 0.0,
        unit: ScrollUnit::Pixel,
        position: Position {
            revision: 10,
            x: 1.0,
            y: 2.0,
        },
        phase,
        momentum_phase,
        inverted_from_device: Some(false),
    }
}
fn target() -> (Target, Controller) {
    let t = Target::new(Config {
        geometry: Geometry {
            revision: 10,
            ..Config::default().geometry
        },
        ..Config::default()
    })
    .unwrap();
    let c = t.admit(Mode::Cooperative).unwrap();
    (t, c)
}
fn execute(t: &Target, event: Event) {
    let w = t.next().expect("ordered event");
    assert_eq!(w.operation, Operation::Event(event));
    t.complete(w.id, Outcome::Executed).unwrap();
}
// Cartesian generator covers every phase, momentum, inversion and unit, including
// unknown vs known-none/false. Every valid sample retains its serialized fields.
#[test]
fn metadata_round_trip_and_shared_stationary_validation() {
    let phases = [
        None,
        Some(ScrollPhase::None),
        Some(ScrollPhase::MayBegin),
        Some(ScrollPhase::Began),
        Some(ScrollPhase::Stationary),
        Some(ScrollPhase::Changed),
        Some(ScrollPhase::Ended),
        Some(ScrollPhase::Cancelled),
    ];
    let momenta = [
        None,
        Some(MomentumPhase::None),
        Some(MomentumPhase::Began),
        Some(MomentumPhase::Changed),
        Some(MomentumPhase::Ended),
    ];
    for phase in phases {
        for momentum in momenta {
            for inversion in [None, Some(false), Some(true)] {
                for unit in [ScrollUnit::Pixel, ScrollUnit::Line, ScrollUnit::Page] {
                    let (t, c) = target();
                    let begin = scroll(Some(ScrollPhase::Began), None);
                    c.submit(1, 1, begin.clone()).unwrap();
                    execute(&t, begin);
                    let mut event = scroll(phase, momentum);
                    if let Event::Scroll {
                        inverted_from_device,
                        unit: u,
                        ..
                    } = &mut event
                    {
                        *inverted_from_device = inversion;
                        *u = unit;
                    }
                    let encoded = serde_json::to_vec(&event).unwrap();
                    assert_eq!(serde_json::from_slice::<Event>(&encoded).unwrap(), event);
                    c.submit(1, 2, event.clone()).unwrap();
                    execute(&t, event.clone());
                    if let Event::Scroll { x, .. } = &mut event {
                        *x = 1.0;
                    }
                    if phase == Some(ScrollPhase::Stationary) {
                        assert_eq!(c.submit(1, 3, event), Err(Error::Invalid));
                        assert!(t.next().is_none());
                        let changed = scroll(Some(ScrollPhase::Changed), None);
                        c.submit(1, 4, changed.clone()).unwrap();
                        execute(&t, changed);
                    }
                }
            }
        }
    }
}
// Admission and completed cleanup close the gate. Orphan terminal events and
// continuations reject cleanly, while unphased wheels do not open the gate.
fn closed_gate(t: &Target, c: &Controller, epoch: u64, sequence: &mut u64, revision: u64) {
    for (phase, momentum) in [
        (Some(ScrollPhase::Stationary), None),
        (Some(ScrollPhase::Changed), None),
        (Some(ScrollPhase::Ended), None),
        (Some(ScrollPhase::Cancelled), None),
        (None, Some(MomentumPhase::Began)),
        (None, Some(MomentumPhase::Changed)),
        (Some(ScrollPhase::None), Some(MomentumPhase::Ended)),
    ] {
        *sequence += 1;
        let mut e = scroll(phase, momentum);
        if let Event::Scroll { position, .. } = &mut e {
            position.revision = revision;
        }
        assert_eq!(c.submit(epoch, *sequence, e), Err(Error::Stale));
        assert!(t.next().is_none());
    }
    for (phase, momentum) in [(None, None), (Some(ScrollPhase::None), Some(MomentumPhase::None))] {
        *sequence += 1;
        let mut e = scroll(phase, momentum);
        if let Event::Scroll { position, .. } = &mut e {
            position.revision = revision;
        }
        c.submit(epoch, *sequence, e.clone()).unwrap();
        execute(t, e);
    }
    *sequence += 1;
    let mut orphan = scroll(None, Some(MomentumPhase::Began));
    if let Event::Scroll { position, .. } = &mut orphan {
        position.revision = revision;
    }
    assert_eq!(c.submit(epoch, *sequence, orphan), Err(Error::Stale));
    assert!(t.next().is_none());
}
#[test]
fn resize_race_cancels_once_and_late_tail_cannot_cancel_fresh_gesture() {
    let (t, c) = target();
    let mut seq = 0;
    closed_gate(&t, &c, 1, &mut seq, 10);
    // Exercise the ADR's exact epoch-7 / geometry-10 race.
    for _ in 0..6 {
        c.reset().unwrap();
        let cleanup = t.next().unwrap();
        t.complete(cleanup.id, Outcome::Executed).unwrap();
    }
    assert_eq!(c.epoch(), Ok(7));
    let begin = scroll(Some(ScrollPhase::Began), None);
    seq += 1;
    c.submit(7, seq, begin.clone()).unwrap();
    execute(&t, begin);
    let change = scroll(Some(ScrollPhase::Changed), None);
    seq += 1;
    c.submit(7, seq, change).unwrap();
    let flight = t.next().unwrap();
    seq += 1;
    c.submit(7, seq, scroll(Some(ScrollPhase::Ended), None)).unwrap();
    t.set_geometry(Geometry {
        revision: 11,
        ..t.config().geometry
    })
    .unwrap();
    assert!(t.next().is_none());
    t.complete(flight.id, Outcome::Executed).unwrap();
    let cleanup = t.next().unwrap();
    assert_eq!(
        cleanup.operation,
        Operation::Cleanup {
            scope: Scope::Pointer,
            reason: Reason::Geometry
        }
    );
    assert_eq!(c.epoch(), Ok(7));
    t.complete(cleanup.id, Outcome::Executed).unwrap();
    assert_eq!(c.epoch(), Ok(8));
    seq += 1;
    assert_eq!(c.submit(7, seq, scroll(Some(ScrollPhase::Ended), None)), Err(Error::Stale));
    seq += 1;
    assert_eq!(c.submit(8, seq, scroll(Some(ScrollPhase::Ended), None)), Err(Error::Stale));
    closed_gate(&t, &c, 8, &mut seq, 11);
    let mut fresh = scroll(Some(ScrollPhase::MayBegin), Some(MomentumPhase::Began));
    if let Event::Scroll { position, .. } = &mut fresh {
        position.revision = 11;
    }
    seq += 1;
    c.submit(8, seq, fresh.clone()).unwrap();
    execute(&t, fresh);
    seq += 1;
    assert_eq!(c.submit(7, seq, scroll(Some(ScrollPhase::Ended), None)), Err(Error::Stale));
    assert!(t.next().is_none());
    assert_eq!(c.epoch(), Ok(8));
    let resets: Vec<_> = std::iter::from_fn(|| c.poll())
        .filter(|s| matches!(s, Status::Reset { epoch: 8, .. }))
        .collect();
    assert_eq!(resets.len(), 1);
}
// Repeated changes and zero-delta handoffs/ends are real ordered events; motions
// coalesce only within their own runs, and never across a scroll boundary.
#[test]
fn every_scroll_survives_between_motion_runs() {
    let (t, c) = target();
    let p = Position {
        revision: 10,
        x: 1.0,
        y: 1.0,
    };
    let events = [
        Event::Motion(p),
        Event::Motion(Position { x: 2.0, ..p }),
        scroll(Some(ScrollPhase::Began), None),
        scroll(Some(ScrollPhase::Changed), None),
        scroll(Some(ScrollPhase::Changed), None),
        scroll(Some(ScrollPhase::Ended), Some(MomentumPhase::Began)),
        scroll(Some(ScrollPhase::None), Some(MomentumPhase::Ended)),
        Event::Motion(p),
        Event::Motion(Position { x: 3.0, ..p }),
    ];
    for (i, e) in events.iter().enumerate() {
        c.submit(1, i as u64 + 1, e.clone()).unwrap();
    }
    for i in [1, 2, 3, 4, 5, 6, 8] {
        execute(&t, events[i].clone());
    }
    assert!(t.next().is_none());
}
// Fixed queue accounting rejects old limits at construction, fits exactly one
// payload-free scroll at 112, and visibly cleans up rather than dropping an end.
#[test]
fn queue_byte_bound_and_cleanup_failure_cover_active_momentum() {
    for bound in [96, 111] {
        assert!(matches!(
            Target::new(Config {
                max_bytes: bound,
                ..Config::default()
            }),
            Err(Error::Invalid)
        ));
    }
    for outcome in [Outcome::Executed, Outcome::Uncertain] {
        let t = Target::new(Config {
            max_bytes: 112,
            ..Config::default()
        })
        .unwrap();
        let c = t.admit(Mode::Cooperative).unwrap();
        let mut e = scroll(Some(ScrollPhase::Began), Some(MomentumPhase::Began));
        if let Event::Scroll { position, .. } = &mut e {
            position.revision = 1;
        }
        assert_eq!(e.bytes(), 112);
        c.submit(1, 1, e.clone()).unwrap();
        let flight = t.next().unwrap();
        assert_eq!(c.submit(1, 2, e), Err(Error::Overflow));
        assert!(t.next().is_none());
        t.complete(flight.id, Outcome::Executed).unwrap();
        let cleanup = t.next().unwrap();
        assert_eq!(
            cleanup.operation,
            Operation::Cleanup {
                scope: Scope::All,
                reason: Reason::Overflow
            }
        );
        let result = t.complete(cleanup.id, outcome);
        if outcome == Outcome::Executed {
            assert_eq!(result, Ok(()));
            assert!(t.idle());
        } else {
            assert_eq!(result, Err(Error::CleanupFailed));
            assert!(t.failed());
        }
    }
}
// Focus cleanup retains assignment with a closed gate; disconnect closes it.
#[test]
fn focus_and_disconnect_cancel_scroll_without_buttons() {
    let (t, c) = target();
    c.submit(1, 1, scroll(Some(ScrollPhase::Began), None)).unwrap();
    let w = t.next().unwrap();
    t.complete(w.id, Outcome::Executed).unwrap();
    c.reset().unwrap();
    let w = t.next().unwrap();
    assert_eq!(
        w.operation,
        Operation::Cleanup {
            scope: Scope::All,
            reason: Reason::Focus
        }
    );
    t.complete(w.id, Outcome::Executed).unwrap();
    let mut seq = 1;
    closed_gate(&t, &c, 2, &mut seq, 10);
    c.close();
    let w = t.next().unwrap();
    assert_eq!(
        w.operation,
        Operation::Cleanup {
            scope: Scope::All,
            reason: Reason::Disconnect
        }
    );
    t.complete(w.id, Outcome::Executed).unwrap();
    assert!(t.idle());
}
// Local client validation rejects stationary movement before wire admission;
// zero-delta handoff and stationary/momentum end remain independent fields.
#[test]
fn local_client_preserves_metadata_and_rejects_invalid_stationary() {
    use std::{
        thread,
        time::{Duration, Instant},
    };

    use jackstay::{
        input::transport::{Client, Server},
        local::Stream,
    };
    #[cfg(unix)]
    let (a, b) = Stream::pair().unwrap();
    #[cfg(windows)]
    let (a, b) = jackstay::local::pipe_pair().unwrap();
    fn wait<T>(mut f: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
    }
    let (t, _) = target(); // Drop the unused initial controller and settle its cleanup.
    let cleanup = t.next().unwrap();
    t.complete(cleanup.id, Outcome::Executed).unwrap();
    let _server = Server::start(t.clone(), a).unwrap();
    let client = Client::connect(b, Mode::Cooperative).unwrap();
    assert_eq!(client.welcome().version, 2);
    let begin = scroll(Some(ScrollPhase::Began), Some(MomentumPhase::None));
    client.send(begin.clone()).unwrap();
    let w = wait(|| t.next());
    assert_eq!(w.operation, Operation::Event(begin));
    t.complete(w.id, Outcome::Executed).unwrap();
    wait(|| client.poll());
    let mut invalid = scroll(Some(ScrollPhase::Stationary), None);
    if let Event::Scroll { y, .. } = &mut invalid {
        *y = -1.0;
    }
    assert_eq!(client.send(invalid), Err(Error::Invalid));
    assert!(t.next().is_none());
    for event in [
        scroll(Some(ScrollPhase::Ended), Some(MomentumPhase::Began)),
        scroll(Some(ScrollPhase::Stationary), Some(MomentumPhase::Ended)),
    ] {
        client.send(event.clone()).unwrap();
        let w = wait(|| t.next());
        assert_eq!(w.operation, Operation::Event(event));
        t.complete(w.id, Outcome::Executed).unwrap();
        wait(|| client.poll());
    }
    // A local stale-geometry rejection before receiving Reset neither resets
    // again nor closes assignment; valid unknown metadata resumes afterward.
    t.set_geometry(Geometry {
        revision: 11,
        ..t.config().geometry
    })
    .unwrap();
    let w = t.next().unwrap();
    t.complete(w.id, Outcome::Executed).unwrap();
    wait(|| (client.welcome().config.geometry.revision == 11).then_some(()));
    assert_eq!(client.send(scroll(Some(ScrollPhase::Ended), None)), Err(Error::Stale));
    let mut wheel = scroll(None, None);
    if let Event::Scroll { position, .. } = &mut wheel {
        position.revision = 11;
    }
    client.send(wheel.clone()).unwrap();
    let w = wait(|| t.next());
    assert_eq!(w.operation, Operation::Event(wheel));
    t.complete(w.id, Outcome::Executed).unwrap();
    assert!(t.next().is_none());
}
