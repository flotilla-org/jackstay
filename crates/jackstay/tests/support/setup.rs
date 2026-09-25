//! Test-only stand-in for a setup channel between a parent test and the child
//! process it spawned: length-prefixed JSON, barrier bytes, and OS objects.
//!
//! Unix passes FDs with SCM_RIGHTS over a Unix socket. Windows uses Jackstay's
//! named-pipe Local Endpoint (Wheelhouse ADR 0011): the listener identifies the
//! connecting child, and handles are duplicated into whichever end is the peer,
//! which acknowledges them. This only moves already-created objects, as the
//! CPU setup channel does.
#![allow(dead_code, reason = "each test crate uses a different subset")]

#[cfg(unix)]
pub use std::os::fd::OwnedFd as OwnedObject;
#[cfg(windows)]
pub use std::os::windows::io::OwnedHandle as OwnedObject;
use std::{
    io::{Read, Write},
    process::Child,
    time::Duration,
};

use serde::{Serialize, de::DeserializeOwned};

#[cfg(unix)]
type Stream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type Stream = jackstay::local::Stream;

/// The parent's listening end. `address` goes to the child in an environment
/// variable.
pub struct Listener {
    #[cfg(unix)]
    listener: std::os::unix::net::UnixListener,
    #[cfg(unix)]
    _directory: tempfile::TempDir,
    #[cfg(windows)]
    listener: jackstay::local::Listener,
    address: String,
}

impl Listener {
    #[cfg(unix)]
    pub fn bind(name: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(format!("{name}.sock"));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        Self {
            listener,
            _directory: directory,
            address: path.to_str().unwrap().to_owned(),
        }
    }

    #[cfg(windows)]
    pub fn bind(name: &str) -> Self {
        use std::{
            sync::atomic::{AtomicU64, Ordering},
            time::{SystemTime, UNIX_EPOCH},
        };
        // Tests in one binary bind concurrently; the clock alone can collide.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
        let address = format!(
            "test-{name}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let listener = jackstay::local::Listener::bind(&endpoint(&address)).unwrap();
        Self { listener, address }
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    #[cfg(unix)]
    pub fn accept(&self) -> Link {
        let (stream, _) = self.listener.accept().unwrap();
        Link::new(stream)
    }

    #[cfg(windows)]
    pub fn accept(&self) -> Link {
        Link::new(self.listener.accept().unwrap().into_stream())
    }
}

#[cfg(windows)]
fn endpoint(address: &str) -> jackstay::local::Endpoint {
    use jackstay::local::{Endpoint, Scope, Transport};
    Endpoint::new(Scope::User, address, Transport::LocalStream).unwrap()
}

pub struct Link(Stream);

impl Link {
    fn new(stream: Stream) -> Self {
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Self(stream)
    }

    #[cfg(unix)]
    pub fn connect(address: &str) -> Self {
        Self::new(Stream::connect(address).unwrap())
    }

    #[cfg(windows)]
    pub fn connect(address: &str) -> Self {
        Self::new(jackstay::local::connect(&endpoint(address)).unwrap().into_stream())
    }

    pub fn send<T: Serialize>(&mut self, value: &T) {
        let json = serde_json::to_vec(value).unwrap();
        self.0.write_all(&(json.len() as u32).to_le_bytes()).unwrap();
        self.0.write_all(&json).unwrap();
    }

    pub fn recv<T: DeserializeOwned>(&mut self) -> T {
        let mut len = [0; 4];
        self.0.read_exact(&mut len).unwrap();
        let mut json = vec![0; u32::from_le_bytes(len) as usize];
        self.0.read_exact(&mut json).unwrap();
        serde_json::from_slice(&json).unwrap()
    }

    pub fn write_byte(&mut self, byte: u8) {
        self.0.write_all(&[byte]).unwrap();
    }

    pub fn read_byte(&mut self) -> u8 {
        let mut byte = [0];
        self.0.read_exact(&mut byte).unwrap();
        byte[0]
    }

    /// True once the peer has closed its end.
    pub fn at_eof(&mut self) -> bool {
        let mut byte = [0];
        self.0.read(&mut byte).unwrap() == 0
    }

    /// Transfer OS objects to `peer`, the child process on the other end.
    #[cfg(unix)]
    pub fn send_objects(&mut self, _peer: &Child, objects: &[OwnedObject]) {
        use std::os::fd::AsRawFd;
        jackstay::fdpass::send_fds(&self.0, &objects.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>()).unwrap();
    }

    #[cfg(unix)]
    pub fn recv_objects(&mut self, count: usize) -> Vec<OwnedObject> {
        jackstay::fdpass::recv_fds(&self.0, count).unwrap()
    }

    /// Duplicate each handle into the pipe's peer (same access, not
    /// inheritable); it acknowledges adopting them. The sender keeps its own.
    #[cfg(windows)]
    pub fn send_objects(&mut self, _peer: &Child, objects: &[OwnedObject]) {
        self.offer_objects(objects);
    }

    /// Offer OS objects to the parent process that spawned this one. The
    /// parent collects them with [`Self::take_objects`]. On Windows the child
    /// duplicates copies straight into the parent, the pipe's server.
    #[cfg(unix)]
    pub fn offer_objects(&mut self, objects: &[OwnedObject]) {
        use std::os::fd::AsRawFd;
        jackstay::fdpass::send_fds(&self.0, &objects.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>()).unwrap();
    }

    #[cfg(unix)]
    pub fn take_objects(&mut self, _peer: &Child, count: usize) -> Vec<OwnedObject> {
        self.recv_objects(count)
    }

    #[cfg(windows)]
    pub fn offer_objects(&mut self, objects: &[OwnedObject]) {
        use jackstay::local::{Access, send_handles};
        let copies = objects.iter().map(|object| (object.try_clone().unwrap(), Access::Same)).collect();
        send_handles(&mut self.0, copies).unwrap();
    }

    #[cfg(windows)]
    pub fn take_objects(&mut self, _peer: &Child, count: usize) -> Vec<OwnedObject> {
        self.recv_objects(count)
    }

    #[cfg(windows)]
    pub fn recv_objects(&mut self, count: usize) -> Vec<OwnedObject> {
        jackstay::local::receive_handles(&mut self.0, count).unwrap()
    }
}
