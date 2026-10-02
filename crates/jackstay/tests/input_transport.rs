//! Input over a local connection: a Unix socket, or a Windows named pipe.
use std::{
    thread,
    time::{Duration, Instant},
};

use jackstay::{
    input::{transport::*, *},
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
fn wait<T>(mut f: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(start.elapsed() < Duration::from_secs(3));
        thread::sleep(Duration::from_millis(2));
    }
}
#[test]
fn socket_text_completion_and_disconnect_cleanup_are_executor_acknowledged() {
    let target = Target::new(Config::default()).unwrap();
    let (a, b) = pair();
    let _server = Server::start(target.clone(), a).unwrap();
    let client = Client::connect(b, Mode::Cooperative).unwrap();
    let text = "é🙂".repeat(100);
    let sequence = client.send(Event::Text(text.clone())).unwrap();
    let work = wait(|| target.next());
    assert_eq!(work.operation, Operation::Event(Event::Text(text)));
    assert!(client.poll().is_none());
    target.complete(work.id, Outcome::Executed).unwrap();
    assert_eq!(
        wait(|| client.poll()),
        Status::Completed {
            sequence,
            outcome: Outcome::Executed
        }
    );
    client.close();
    let work = wait(|| target.next());
    assert!(matches!(work.operation, Operation::Cleanup { .. }));
    assert!(client.poll().is_none());
    target.complete(work.id, Outcome::Executed).unwrap();
    assert_eq!(
        wait(|| client.poll()),
        Status::Closed {
            reason: Reason::Disconnect,
            clean: true
        }
    );
    assert!(target.idle());
}
#[test]
fn worker_heartbeats_keep_idle_controller_alive_without_frame_or_application_polling() {
    let target = Target::new(Config {
        idle_timeout: Duration::from_millis(150),
        ..Config::default()
    })
    .unwrap();
    let (a, b) = pair();
    let _server = Server::start(target.clone(), a).unwrap();
    let client = Client::connect(b, Mode::Cooperative).unwrap();
    thread::sleep(Duration::from_millis(400));
    assert!(target.next().is_none());
    client.send(Event::Text("still alive".into())).unwrap();
    let w = wait(|| target.next());
    target.complete(w.id, Outcome::Executed).unwrap();
    assert!(matches!(wait(|| client.poll()), Status::Completed { .. }));
    drop(client);
    assert!(matches!(wait(|| target.next()).operation, Operation::Cleanup { .. }));
}

#[test]
fn maximum_text_survives_json_escaping_and_invalid_input_does_not_close_session() {
    let t = Target::new(Config::default()).unwrap();
    let (a, b) = pair();
    let _server = Server::start(t.clone(), a).unwrap();
    let c = Client::connect(b, Mode::Cooperative).unwrap();
    assert_eq!(c.send(Event::Text("x".repeat(16385))), Err(Error::Invalid));
    let text = "\0".repeat(16384);
    c.send(Event::Text(text.clone())).unwrap();
    let work = wait(|| t.next());
    assert_eq!(work.operation, Operation::Event(Event::Text(text)));
    t.complete(work.id, Outcome::Executed).unwrap();
    assert!(matches!(
        wait(|| c.poll()),
        Status::Completed {
            outcome: Outcome::Executed,
            ..
        }
    ));
}

// Worker ticks must not expand a slow-polling presenter's consecutive motion
// results into an overflowing client queue. Real local transport and heartbeats
// span many ticks while the executor remains paused.
#[test]
fn slow_polling_motion_burst_aggregates_client_results() {
    let target = Target::new(Config {
        max_events: 4,
        max_bytes: 384,
        ..Config::default()
    })
    .unwrap();
    let (a, b) = pair();
    let _server = Server::start(target.clone(), a).unwrap();
    let client = Client::connect(b, Mode::Cooperative).unwrap();
    let mut last = 0;
    for x in 1..=40 {
        last = client
            .send(Event::Motion(Position {
                revision: 1,
                x: x as f64,
                y: 1.0,
            }))
            .unwrap();
        // A slow producer spreads replies across worker ticks without flooding
        // the independent producer-side send queue.
        thread::sleep(Duration::from_millis(30));
    }
    let mut superseded = 0;
    while superseded < 39 {
        match wait(|| client.poll()) {
            Status::Coalesced { count } => superseded += count,
            other => panic!("unexpected motion burst result: {other:?}"),
        }
    }
    assert_eq!(superseded, 39);
    let work = wait(|| target.next());
    assert_eq!(work.sequence, last);
    assert_eq!(
        work.operation,
        Operation::Event(Event::Motion(Position {
            revision: 1,
            x: 40.0,
            y: 1.0
        }))
    );
    target.complete(work.id, Outcome::Executed).unwrap();
    assert_eq!(
        wait(|| client.poll()),
        Status::Completed {
            sequence: last,
            outcome: Outcome::Executed
        }
    );
}
