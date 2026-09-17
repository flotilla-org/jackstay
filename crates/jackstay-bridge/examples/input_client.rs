//! Connects to an ingress input socket as a controller, sends one motion
//! event and closes, printing what comes back: a relay check without the
//! SDL viewer.
use std::{
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

use jackstay::input::{Event, Mode, Position, transport::Client};

fn main() {
    let path = std::env::args().nth(1).expect("socket path");
    let stream = UnixStream::connect(&path).expect("connect");
    let started = Instant::now();
    let client = match Client::connect(stream, Mode::Cooperative) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("connect failed after {:?}: {e:?}", started.elapsed());
            std::process::exit(1);
        }
    };
    eprintln!("welcome after {:?}: {:?}", started.elapsed(), client.welcome().config.geometry);
    let seq = client
        .send(Event::Motion(Position {
            revision: 1,
            x: 10.0,
            y: 20.0,
        }))
        .expect("send");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(s) = client.poll() {
            eprintln!("status for {seq}: {s:?}");
            break;
        }
        if Instant::now() > deadline {
            eprintln!("no completion within 3 s");
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    client.close();
    std::thread::sleep(Duration::from_millis(300));
    while let Some(s) = client.poll() {
        eprintln!("after close: {s:?}");
    }
}
