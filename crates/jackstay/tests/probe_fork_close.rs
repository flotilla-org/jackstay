//! CI probe for jackstay#48 (not for merge): does a process spawned by another
//! thread keep a just-closed Unix socket end open long enough for the peer's
//! one-shot MSG_PEEK|MSG_DONTWAIT (ffi_native's linux_stream_peer_closed) to
//! miss the EOF?
#![cfg(target_os = "linux")]

use std::{
    os::{fd::AsRawFd, unix::net::UnixStream},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

fn peer_closed_now(stream: &UnixStream) -> bool {
    let mut byte = 0u8;
    // SAFETY: live socket; one-byte peek consumes nothing.
    let result = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&mut byte as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    result == 0
}

fn misses(spawning: bool) -> usize {
    let stop = Arc::new(AtomicBool::new(false));
    let spawner = spawning.then(|| {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = Command::new("true").status();
            }
        })
    });
    let mut missed = 0;
    for _ in 0..5000 {
        let (client, server) = UnixStream::pair().unwrap();
        // The server end closes on another thread, as the test's server does.
        thread::spawn(move || drop(server)).join().unwrap();
        if !peer_closed_now(&client) {
            missed += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    if let Some(spawner) = spawner {
        spawner.join().unwrap();
    }
    missed
}

#[test]
fn probe_fork_holds_closed_socket() {
    let quiet = misses(false);
    let spawning = misses(true);
    println!("PROBE misses without spawning: {quiet}/5000, with a spawning thread: {spawning}/5000");
    assert_eq!(quiet, 0);
    assert!(spawning > 0, "no miss observed");
}
