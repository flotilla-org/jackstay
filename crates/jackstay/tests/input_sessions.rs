use jackstay::input::*;

#[test]
fn disconnected_controller_waits_for_in_flight_down_and_confirmed_cleanup() {
    let target = Target::new(Config::default()).unwrap();
    let controller = target.admit(Mode::Cooperative).unwrap();
    controller
        .submit(
            1,
            1,
            Event::Key {
                press: 42,
                action: Action::Down,
                key: Key::Physical("KeyA".into()),
                modifiers: 0,
            },
        )
        .unwrap();
    let down = target.next().unwrap();
    drop(controller);
    assert_eq!(target.admit(Mode::Cooperative).err(), Some(Error::Busy));
    assert!(target.next().is_none());
    target.complete(down.id, Outcome::Executed).unwrap();
    let cleanup = target.next().unwrap();
    assert!(matches!(cleanup.operation, Operation::Cleanup { scope: Scope::All, .. }));
    assert_eq!(target.admit(Mode::Cooperative).err(), Some(Error::Busy));
    target.complete(cleanup.id, Outcome::Executed).unwrap();
    assert!(target.admit(Mode::Cooperative).is_ok());
}

fn down(press: u64) -> Event {
    Event::Key {
        press,
        action: Action::Down,
        key: Key::Physical("KeyA".into()),
        modifiers: 0,
    }
}
fn finish(target: &Target) -> Work {
    let w = target.next().unwrap();
    target.complete(w.id, Outcome::Executed).unwrap();
    w
}
#[test]
fn reset_discards_queued_text_but_retains_assignment_with_fresh_epoch() {
    let t = Target::new(Config::default()).unwrap();
    let c = t.admit(Mode::Cooperative).unwrap();
    c.submit(1, 1, down(1)).unwrap();
    finish(&t);
    c.submit(1, 2, Event::Text("must not execute".into())).unwrap();
    c.reset().unwrap();
    assert_eq!(c.submit(1, 3, down(2)), Err(Error::Busy));
    assert!(matches!(finish(&t).operation, Operation::Cleanup { scope: Scope::All, .. }));
    assert_eq!(c.epoch(), Ok(2));
    assert!(t.next().is_none());
    assert_eq!(c.submit(1, 4, down(2)), Err(Error::Stale));
    c.submit(2, 5, down(2)).unwrap();
    finish(&t);
    assert_eq!(t.admit(Mode::Cooperative).err(), Some(Error::Busy));
}
#[test]
fn release_uses_original_binding_even_when_sender_key_meaning_changes() {
    let t = Target::new(Config::default()).unwrap();
    let c = t.admit(Mode::Cooperative).unwrap();
    c.submit(1, 1, down(8)).unwrap();
    finish(&t);
    c.submit(
        1,
        2,
        Event::Key {
            press: 8,
            action: Action::Up,
            key: Key::Logical("different".into()),
            modifiers: 4,
        },
    )
    .unwrap();
    assert!(matches!(finish(&t).operation, Operation::Event(Event::Key { key: Key::Physical(ref n), modifiers: 4, .. }) if n == "KeyA"));
}
#[test]
fn failed_cleanup_quarantines_target_until_host_resolves_it() {
    let t = Target::new(Config::default()).unwrap();
    let c = t.admit(Mode::Cooperative).unwrap();
    drop(c);
    let w = t.next().unwrap();
    assert_eq!(t.complete(w.id, Outcome::Uncertain), Err(Error::CleanupFailed));
    assert_eq!(t.admit(Mode::Cooperative).err(), Some(Error::CleanupFailed));
    t.resolve_failed_cleanup().unwrap();
    assert!(t.admit(Mode::Cooperative).is_ok());
}
#[test]
fn geometry_cleanup_preserves_keyboard_binding_and_rejects_stale_clicks() {
    let t = Target::new(Config::default()).unwrap();
    let c = t.admit(Mode::Cooperative).unwrap();
    c.submit(1, 1, down(1)).unwrap();
    finish(&t);
    let p = Position {
        revision: 1,
        x: 2.0,
        y: 3.0,
    };
    c.submit(
        1,
        2,
        Event::Button {
            button: 1,
            action: Action::Down,
            position: p,
        },
    )
    .unwrap();
    finish(&t);
    t.set_geometry(Geometry {
        revision: 2,
        width: 640.0,
        height: 360.0,
    })
    .unwrap();
    assert!(matches!(finish(&t).operation, Operation::Cleanup { scope: Scope::Pointer, .. }));
    assert_eq!(c.submit(2, 3, Event::Motion(p)), Err(Error::Stale));
    c.submit(
        2,
        4,
        Event::Key {
            press: 1,
            action: Action::Up,
            key: Key::Physical("KeyA".into()),
            modifiers: 0,
        },
    )
    .unwrap();
    finish(&t);
}
#[test]
fn partial_execution_discards_pending_text_and_requires_cleanup() {
    let t = Target::new(Config::default()).unwrap();
    let c = t.admit(Mode::Cooperative).unwrap();
    c.submit(1, 1, down(1)).unwrap();
    c.submit(1, 2, Event::Text("never".into())).unwrap();
    let w = t.next().unwrap();
    t.complete(w.id, Outcome::Partial).unwrap();
    assert!(matches!(
        finish(&t).operation,
        Operation::Cleanup {
            scope: Scope::All,
            reason: Reason::Execution
        }
    ));
    assert_eq!(c.epoch(), Err(Error::Closed));
    assert!(t.next().is_none());
}
#[test]
fn overflow_ends_session_instead_of_losing_a_transition() {
    let t = Target::new(Config {
        max_events: 1,
        ..Config::default()
    })
    .unwrap();
    let c = t.admit(Mode::Cooperative).unwrap();
    c.submit(1, 1, down(1)).unwrap();
    assert_eq!(c.submit(1, 2, down(2)), Err(Error::Overflow));
    assert!(matches!(
        finish(&t).operation,
        Operation::Cleanup {
            reason: Reason::Overflow,
            ..
        }
    ));
    assert_eq!(c.epoch(), Err(Error::Closed));
}

#[test]
fn clean_rejections_preserve_session_and_physical_mode_has_no_source_repeat() {
    let t = Target::new(Config {
        modes: 7,
        capabilities: CAP_PHYSICAL | CAP_TEXT,
        ..Config::default()
    })
    .unwrap();
    let c = t.admit(Mode::Physical).unwrap();
    assert_eq!(
        c.submit(
            1,
            1,
            Event::Key {
                press: 1,
                action: Action::Down,
                key: Key::Logical("a".into()),
                modifiers: 0
            }
        ),
        Err(Error::Unsupported)
    );
    c.submit(1, 2, down(2)).unwrap();
    finish(&t);
    assert_eq!(
        c.submit(
            1,
            3,
            Event::Key {
                press: 2,
                action: Action::Repeat,
                key: Key::Physical("KeyA".into()),
                modifiers: 0
            }
        ),
        Err(Error::Unsupported)
    );
    // Release identity survives even a change to an unsupported key namespace.
    c.submit(
        1,
        4,
        Event::Key {
            press: 2,
            action: Action::Up,
            key: Key::Logical("a".into()),
            modifiers: 0,
        },
    )
    .unwrap();
    finish(&t);
    c.submit(1, 5, Event::Text("explicit insertion".into())).unwrap();
    finish(&t);
    assert_eq!(c.epoch(), Ok(1));
}
#[test]
fn stalled_controller_expires_and_cannot_revive_by_sending_a_late_heartbeat() {
    let t = Target::new(Config {
        idle_timeout: std::time::Duration::from_millis(100),
        ..Config::default()
    })
    .unwrap();
    let c = t.admit(Mode::Cooperative).unwrap();
    c.submit(1, 1, down(1)).unwrap();
    finish(&t);
    std::thread::sleep(std::time::Duration::from_millis(120));
    let _ = c.heartbeat();
    assert_eq!(c.submit(1, 2, down(2)), Err(Error::Busy));
    assert!(matches!(
        finish(&t).operation,
        Operation::Cleanup {
            reason: Reason::Expired,
            ..
        }
    ));
    assert_eq!(c.epoch(), Err(Error::Closed));
}

#[test]
fn release_after_geometry_cleanup_is_a_safe_noop_without_touching_local_input() {
    let t = Target::new(Config::default()).unwrap();
    let c = t.admit(Mode::Cooperative).unwrap();
    let p = Position {
        revision: 1,
        x: 1.0,
        y: 1.0,
    };
    c.submit(
        1,
        1,
        Event::Button {
            button: 1,
            action: Action::Down,
            position: p,
        },
    )
    .unwrap();
    finish(&t);
    t.set_geometry(Geometry {
        revision: 2,
        width: 640.0,
        height: 360.0,
    })
    .unwrap();
    finish(&t);
    while c.poll().is_some() {}
    c.submit(
        2,
        2,
        Event::Button {
            button: 1,
            action: Action::Up,
            position: p,
        },
    )
    .unwrap();
    assert!(t.next().is_none());
    assert_eq!(
        c.poll(),
        Some(Status::Completed {
            sequence: 2,
            outcome: Outcome::Executed
        })
    );
}

fn motion(x: f64) -> Event {
    Event::Motion(Position { revision: 1, x, y: 1.0 })
}

// Consecutive queued motions retain only the latest position and sequence, even
// at both queue bounds. Explicit burst lengths span empty replacement, duplicate,
// boundary and far-beyond-bound cases using the real public session.
#[test]
fn motion_bursts_charge_one_event_and_report_superseded_count() {
    for length in [1, 2, 6, 257, 1024] {
        let t = Target::new(Config {
            max_events: 1,
            max_bytes: 96,
            ..Config::default()
        })
        .unwrap();
        let c = t.admit(Mode::Cooperative).unwrap();
        assert!(t.next().is_none());
        for sequence in 1..=length {
            c.submit(1, sequence, motion((sequence % 100) as f64)).unwrap();
        }
        if length > 1 {
            assert_eq!(c.poll(), Some(Status::Coalesced { count: length - 1 }));
        }
        let w = finish(&t);
        assert_eq!(w.sequence, length);
        assert_eq!(w.operation, Operation::Event(motion((length % 100) as f64)));
        assert_eq!(
            c.poll(),
            Some(Status::Completed {
                sequence: length,
                outcome: Outcome::Executed
            })
        );
        assert!(c.poll().is_none());
        assert!(t.next().is_none());
    }
}

// Every non-motion family separates runs; neither last position may be lost.
#[test]
fn motions_do_not_merge_across_transitions() {
    let p = Position {
        revision: 1,
        x: 2.0,
        y: 1.0,
    };
    for separator in [
        Event::Button {
            button: 1,
            action: Action::Down,
            position: p,
        },
        down(1),
        Event::Text("text".into()),
        Event::Scroll {
            x: 1.0,
            y: 0.0,
            position: p,
            unit: ScrollUnit::Pixel,
        },
    ] {
        let t = Target::new(Config {
            max_events: 3,
            max_bytes: 400,
            ..Config::default()
        })
        .unwrap();
        let c = t.admit(Mode::Cooperative).unwrap();
        c.submit(1, 1, motion(1.0)).unwrap();
        c.submit(1, 2, motion(2.0)).unwrap();
        c.submit(1, 3, separator.clone()).unwrap();
        c.submit(1, 4, motion(3.0)).unwrap();
        c.submit(1, 5, motion(4.0)).unwrap();
        for (sequence, event) in [(2, motion(2.0)), (3, separator), (5, motion(4.0))] {
            let w = finish(&t);
            assert_eq!(w.sequence, sequence);
            assert_eq!(w.operation, Operation::Event(event));
        }
        assert!(t.next().is_none());
    }
}

// Invalid motion cannot replace an accepted tail or contribute to coalescing.
#[test]
fn invalid_motion_preserves_the_queued_position() {
    let t = Target::new(Config::default()).unwrap();
    let c = t.admit(Mode::Cooperative).unwrap();
    c.submit(1, 1, motion(1.0)).unwrap();
    assert_eq!(c.submit(1, 2, motion(-1.0)), Err(Error::Invalid));
    assert_eq!(finish(&t).operation, Operation::Event(motion(1.0)));
    assert!(matches!(c.poll(), Some(Status::Completed { sequence: 1, .. })));
    assert!(c.poll().is_none());
}

// Dispatched motions settle before cleanup. Queued coalesced motion remains
// subject to the same focus/geometry cancellation as any queued pointer event.
#[test]
fn motion_in_flight_settles_before_cleanup_and_queued_motion_is_cancelled() {
    for geometry_change in [false, true] {
        let t = Target::new(Config::default()).unwrap();
        let c = t.admit(Mode::Cooperative).unwrap();
        c.submit(1, 1, motion(1.0)).unwrap();
        let flight = t.next().unwrap();
        c.submit(1, 2, motion(2.0)).unwrap();
        c.submit(1, 3, motion(3.0)).unwrap();
        assert_eq!(c.poll(), Some(Status::Coalesced { count: 1 }));
        if geometry_change {
            t.set_geometry(Geometry {
                revision: 2,
                width: 320.0,
                height: 180.0,
            })
            .unwrap();
        } else {
            c.reset().unwrap();
        }
        assert_eq!(
            c.poll(),
            Some(Status::Rejected {
                sequence: 3,
                error: Error::Stale
            })
        );
        assert!(t.next().is_none());
        assert_eq!(c.submit(1, 4, motion(4.0)), Err(Error::Busy));
        t.complete(flight.id, Outcome::Executed).unwrap();
        assert_eq!(
            c.poll(),
            Some(Status::Completed {
                sequence: 1,
                outcome: Outcome::Executed
            })
        );
        assert!(matches!(finish(&t).operation, Operation::Cleanup { .. }));
        assert!(matches!(c.poll(), Some(Status::Reset { epoch: 2, .. })));
        assert!(t.next().is_none());
        c.submit(
            2,
            5,
            Event::Motion(Position {
                revision: if geometry_change { 2 } else { 1 },
                x: 5.0,
                y: 1.0,
            }),
        )
        .unwrap();
        assert_eq!(finish(&t).sequence, 5);
    }
}

// An in-flight motion is separately charged and cannot be replaced by submit.
#[test]
fn in_flight_motion_still_consumes_queue_capacity() {
    let t = Target::new(Config {
        max_events: 1,
        max_bytes: 96,
        ..Config::default()
    })
    .unwrap();
    let c = t.admit(Mode::Cooperative).unwrap();
    c.submit(1, 1, motion(1.0)).unwrap();
    let w = t.next().unwrap();
    assert_eq!(c.submit(1, 2, motion(2.0)), Err(Error::Overflow));
    t.complete(w.id, Outcome::Executed).unwrap();
    assert!(matches!(
        finish(&t).operation,
        Operation::Cleanup {
            reason: Reason::Overflow,
            ..
        }
    ));
    assert_eq!(c.epoch(), Err(Error::Closed));
}

// Counts are deltas: consuming an observation starts a new count. Coalescing
// frees no extra capacity for a non-motion transition at the byte bound.
#[test]
fn coalescing_counts_are_deltas_and_non_motion_byte_overflow_still_closes() {
    let t = Target::new(Config {
        max_events: 4,
        max_bytes: 192,
        ..Config::default()
    })
    .unwrap();
    let c = t.admit(Mode::Cooperative).unwrap();
    c.submit(1, 1, motion(1.0)).unwrap();
    c.submit(1, 2, motion(2.0)).unwrap();
    assert_eq!(c.poll(), Some(Status::Coalesced { count: 1 }));
    c.submit(1, 3, motion(2.0)).unwrap();
    assert_eq!(c.poll(), Some(Status::Coalesced { count: 1 }));
    assert_eq!(c.submit(1, 4, down(1)), Err(Error::Overflow)); // 96 + key payload exceeds 192.
    assert!(matches!(
        finish(&t).operation,
        Operation::Cleanup {
            reason: Reason::Overflow,
            ..
        }
    ));
    assert_eq!(c.epoch(), Err(Error::Closed));
}
