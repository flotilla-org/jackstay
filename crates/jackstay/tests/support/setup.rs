//! Test-only stand-in for a setup channel between a parent test and the child
//! process it spawned: length-prefixed JSON, barrier bytes, and OS objects.
//!
//! Unix passes FDs with SCM_RIGHTS over a Unix socket. Windows has no handle
//! passing on a byte stream, so the parent duplicates each handle straight into
//! the child it spawned and sends the resulting values over loopback TCP. The
//! real Windows setup channel (named pipes per Wheelhouse ADR 0011) is separate
//! work; this only moves already-created objects, as that channel will.
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
type Stream = std::net::TcpStream;

/// The parent's listening end. `address` goes to the child in an environment
/// variable.
pub struct Listener {
    #[cfg(unix)]
    listener: std::os::unix::net::UnixListener,
    #[cfg(unix)]
    _directory: tempfile::TempDir,
    #[cfg(windows)]
    listener: std::net::TcpListener,
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
    pub fn bind(_name: &str) -> Self {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        Self { listener, address }
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn accept(&self) -> Link {
        let (stream, _) = self.listener.accept().unwrap();
        Link::new(stream)
    }
}

pub struct Link(Stream);

impl Link {
    fn new(stream: Stream) -> Self {
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Self(stream)
    }

    pub fn connect(address: &str) -> Self {
        Self::new(Stream::connect(address).unwrap())
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

    /// Duplicate each handle into `peer` (same access, not inheritable) and
    /// send the values the child can adopt. The sender keeps its own handles.
    #[cfg(windows)]
    pub fn send_objects(&mut self, peer: &Child, objects: &[OwnedObject]) {
        use std::os::windows::io::AsRawHandle;

        use windows_sys::Win32::{
            Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle},
            System::Threading::GetCurrentProcess,
        };
        let mut values = Vec::with_capacity(objects.len());
        for object in objects {
            let mut duplicate = std::ptr::null_mut();
            // SAFETY: both process handles and the source handle are live;
            // the duplicate is owned by the child from here on.
            let duplicated = unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    object.as_raw_handle(),
                    peer.as_raw_handle(),
                    &mut duplicate,
                    0,
                    0,
                    DUPLICATE_SAME_ACCESS,
                )
            };
            assert_ne!(duplicated, 0, "{}", std::io::Error::last_os_error());
            values.push(duplicate as usize as u64);
        }
        self.send(&values);
    }

    /// Offer OS objects to the parent process that spawned this one. The
    /// parent collects them with [`Self::take_objects`]; on Windows this
    /// process keeps its own handles open until it exits.
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
        use std::os::windows::io::AsRawHandle;
        let values: Vec<u64> = objects.iter().map(|object| object.as_raw_handle() as usize as u64).collect();
        self.send(&values);
    }

    /// Duplicate the handles a child offered into this process.
    #[cfg(windows)]
    pub fn take_objects(&mut self, peer: &Child, count: usize) -> Vec<OwnedObject> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle};

        use windows_sys::Win32::{
            Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle},
            System::Threading::GetCurrentProcess,
        };
        let values: Vec<u64> = self.recv();
        assert_eq!(values.len(), count);
        values
            .into_iter()
            .map(|value| {
                let mut duplicate = std::ptr::null_mut();
                // SAFETY: the child keeps each offered handle open while parked;
                // the duplicate is a fresh handle owned by this process.
                let duplicated = unsafe {
                    DuplicateHandle(
                        peer.as_raw_handle(),
                        value as usize as _,
                        GetCurrentProcess(),
                        &mut duplicate,
                        0,
                        0,
                        DUPLICATE_SAME_ACCESS,
                    )
                };
                assert_ne!(duplicated, 0, "{}", std::io::Error::last_os_error());
                // SAFETY: DuplicateHandle succeeded; nothing else owns it.
                unsafe { OwnedObject::from_raw_handle(duplicate) }
            })
            .collect()
    }

    #[cfg(windows)]
    pub fn recv_objects(&mut self, count: usize) -> Vec<OwnedObject> {
        use std::os::windows::io::FromRawHandle;
        let values: Vec<u64> = self.recv();
        assert_eq!(values.len(), count);
        values
            .into_iter()
            // SAFETY: the parent duplicated each handle into this process for
            // this receiver alone; nothing else owns or closes these values.
            .map(|value| unsafe { OwnedObject::from_raw_handle(value as usize as _) })
            .collect()
    }
}
