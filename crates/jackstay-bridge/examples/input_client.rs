//! A test controller: connects to an ingress input socket and performs an
//! action, printing statuses. Usage:
//!   input_client SOCKET                  one motion, then close
//!   input_client SOCKET tap X Y          move, button down, up at X,Y
//!   input_client SOCKET tapw X Y         tap, then wait for all completions
//!   input_client SOCKET swipe X1 Y1 X2 Y2   press, drag, release
//!   input_client SOCKET text STRING      type a string
//!   input_client SOCKET motions          five spaced motions, print each
//!   input_client SOCKET watch            repeat motions, report resets
//!   input_client SOCKET bench [N]        N motions, print per-event latency
//!
//! `tapw` and `bench` wait for each event to complete before continuing, so
//! they measure and exercise a persistent session rather than racing a close.
use std::{
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

use jackstay::input::{Action, Event, Mode, Position, transport::Client};

fn drain(client: &Client, label: &str) {
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if let Some(s) = client.poll() {
            eprintln!("{label}: {s:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("socket path");
    let stream = UnixStream::connect(path).expect("connect");
    let client = Client::connect(stream, Mode::Cooperative).expect("connect input");
    let geo = client.welcome().config.geometry;
    eprintln!("welcome: extent {}x{} rev {}", geo.width, geo.height, geo.revision);
    // Settle: drain statuses briefly so a geometry reset updates our revision,
    // exactly as a real controller tracks the extent.
    let mut rev = geo.revision;
    let settle = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < settle {
        while let Some(s) = client.poll() {
            if let jackstay::input::Status::Reset { geometry, .. } = s {
                rev = geometry.revision;
                eprintln!("reset: extent {}x{} rev {}", geometry.width, geometry.height, rev);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    match args.get(2).map(String::as_str) {
        Some("tap") => {
            let x: f64 = args[3].parse().unwrap();
            let y: f64 = args[4].parse().unwrap();
            let pos = |r| Position { revision: r, x, y };
            client.send(Event::Motion(pos(rev))).unwrap();
            client
                .send(Event::Button {
                    button: 1,
                    action: Action::Down,
                    position: pos(rev),
                })
                .unwrap();
            std::thread::sleep(Duration::from_millis(60));
            client
                .send(Event::Button {
                    button: 1,
                    action: Action::Up,
                    position: pos(rev),
                })
                .unwrap();
            eprintln!("tapped at {x},{y}");
        }
        Some("bench") => {
            // Persistent connection: send N motions, each waited to completion,
            // printing per-event round-trip so we can see the focus-once effect.
            let n: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10);
            for i in 0..n {
                let x = 100.0 + (i % 50) as f64 * 3.0;
                let t = Instant::now();
                let seq = client
                    .send(Event::Motion(Position {
                        revision: rev,
                        x,
                        y: 200.0,
                    }))
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(8);
                'wait: while Instant::now() < deadline {
                    while let Some(s) = client.poll() {
                        if let jackstay::input::Status::Completed { sequence, .. } | jackstay::input::Status::Rejected { sequence, .. } = &s
                        {
                            if *sequence == seq {
                                eprintln!("bench[{i}] seq={seq} {:?} in {:?}", s, t.elapsed());
                                break 'wait;
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
        Some("tapw") => {
            // Tap, then wait for all three events to complete before closing,
            // so a slow first injection does not get the queue cancelled.
            let x: f64 = args[3].parse().unwrap();
            let y: f64 = args[4].parse().unwrap();
            let pos = |r| Position { revision: r, x, y };
            client.send(Event::Motion(pos(rev))).unwrap();
            client
                .send(Event::Button {
                    button: 1,
                    action: Action::Down,
                    position: pos(rev),
                })
                .unwrap();
            std::thread::sleep(Duration::from_millis(80));
            client
                .send(Event::Button {
                    button: 1,
                    action: Action::Up,
                    position: pos(rev),
                })
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(8);
            let mut completed = 0;
            let mut rejected = 0;
            while Instant::now() < deadline && completed + rejected < 3 {
                while let Some(s) = client.poll() {
                    match s {
                        jackstay::input::Status::Completed { .. } => completed += 1,
                        jackstay::input::Status::Rejected { .. } => rejected += 1,
                        _ => {}
                    }
                    eprintln!("tapw: {s:?}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            eprintln!("tapw done at {x},{y}: completed={completed} rejected={rejected}");
        }
        Some("swipe") => {
            let (x1, y1, x2, y2): (f64, f64, f64, f64) = (
                args[3].parse().unwrap(),
                args[4].parse().unwrap(),
                args[5].parse().unwrap(),
                args[6].parse().unwrap(),
            );
            let pos = |x, y| Position { revision: rev, x, y };
            client.send(Event::Motion(pos(x1, y1))).unwrap();
            client
                .send(Event::Button {
                    button: 1,
                    action: Action::Down,
                    position: pos(x1, y1),
                })
                .unwrap();
            for i in 1..=10 {
                let t = i as f64 / 10.0;
                client.send(Event::Motion(pos(x1 + (x2 - x1) * t, y1 + (y2 - y1) * t))).unwrap();
                std::thread::sleep(Duration::from_millis(15));
            }
            client
                .send(Event::Button {
                    button: 1,
                    action: Action::Up,
                    position: pos(x2, y2),
                })
                .unwrap();
            eprintln!("swiped {x1},{y1} -> {x2},{y2}");
        }
        Some("text") => {
            client.send(Event::Text(args[3].clone())).unwrap();
            eprintln!("typed {:?}", args[3]);
        }
        Some("watch") => {
            for i in 0..12 {
                let st = client.send(Event::Motion(Position {
                    revision: rev,
                    x: 100.0,
                    y: 200.0,
                }));
                std::thread::sleep(Duration::from_millis(300));
                while let Some(s) = client.poll() {
                    if let jackstay::input::Status::Reset { geometry, .. } = &s {
                        rev = geometry.revision;
                        eprintln!("RESET -> rev {} extent {}x{}", rev, geometry.width, geometry.height);
                    } else {
                        eprintln!("{i}: {s:?} (sent rev {rev}, send={st:?})");
                    }
                }
            }
        }
        Some("motions") => {
            for i in 0..5 {
                let x = 100.0 + i as f64 * 20.0;
                client
                    .send(Event::Motion(Position {
                        revision: rev,
                        x,
                        y: 200.0,
                    }))
                    .unwrap();
                std::thread::sleep(Duration::from_millis(40));
                while let Some(st) = client.poll() {
                    eprintln!("motion {i}: {st:?}");
                }
            }
        }
        _ => {
            client
                .send(Event::Motion(Position {
                    revision: rev,
                    x: 10.0,
                    y: 20.0,
                }))
                .unwrap();
        }
    }
    drain(&client, "status");
    client.close();
    std::thread::sleep(Duration::from_millis(300));
    drain(&client, "after close");
}
