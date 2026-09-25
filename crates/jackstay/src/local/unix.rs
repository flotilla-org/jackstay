//! Unix-socket Local Endpoints in a private per-user runtime directory.
//!
//! `<runtime>/jackstay-<euid>/<name>.sock`, where the runtime directory is
//! `$XDG_RUNTIME_DIR` when set and the temporary directory otherwise. The
//! per-user directory must be owned by this user with no group or other
//! access. Clients check the server's peer UID before trusting it.

use std::{
    fs,
    io::{self, ErrorKind},
    net::Shutdown,
    os::unix::{
        fs::{DirBuilderExt, MetadataExt},
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

use super::{Connection, Endpoint, Error, PeerIdentity};

/// A private socket duplicate that only interrupts I/O with `shutdown`.
#[derive(Debug)]
pub struct ShutdownHandle(pub(crate) UnixStream);

impl ShutdownHandle {
    pub fn shutdown(&self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

fn euid() -> u32 {
    // SAFETY: geteuid has no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

fn directory() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(std::env::temp_dir);
    base.join(format!("jackstay-{}", euid()))
}

pub(super) fn render(endpoint: &Endpoint) -> io::Result<PathBuf> {
    // Scope: macOS and Linux have no session boundary to enforce here.
    Ok(directory().join(format!("{}.sock", endpoint.name)))
}

/// The per-user directory must be ours alone: a directory (not a symlink),
/// owned by this user, with no group/other permissions.
fn private_directory(create: bool) -> io::Result<PathBuf> {
    let directory = directory();
    if create {
        match fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let metadata = fs::symlink_metadata(&directory)?;
    if !metadata.is_dir() || metadata.uid() != euid() || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("{} is not a private directory owned by this user", directory.display()),
        ));
    }
    Ok(directory)
}

/// Kernel-reported (pid, uid) of the socket's peer.
pub(crate) fn peer_credentials(stream: &UnixStream) -> io::Result<(u32, u32)> {
    use std::os::fd::AsRawFd;
    let pid = crate::acquisition::socket::peer_pid(stream).map_err(io::Error::other)?;
    #[cfg(target_os = "linux")]
    let uid = {
        let mut credentials = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut len = std::mem::size_of_val(&credentials) as libc::socklen_t;
        // SAFETY: SO_PEERCRED fills a ucred of the given size.
        if unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                std::ptr::addr_of_mut!(credentials).cast(),
                &mut len,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        credentials.uid
    };
    #[cfg(not(target_os = "linux"))]
    let uid = {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: live socket and writable outputs.
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        uid
    };
    Ok((pid, uid))
}

fn identity(stream: &UnixStream) -> io::Result<PeerIdentity> {
    let (pid, uid) = peer_credentials(stream)?;
    Ok(PeerIdentity {
        pid,
        user: uid.to_string(),
        session: None,
    })
}

pub(super) struct Listener {
    listener: UnixListener,
    cancelled: AtomicBool,
    path: PathBuf,
}

impl Listener {
    pub(super) fn bind(endpoint: &Endpoint) -> Result<Self, Error> {
        private_directory(true)?;
        let path = render(endpoint)?;
        if fs::symlink_metadata(&path).is_ok() {
            // A live server keeps its endpoint; a stale socket from a crashed
            // one (nothing accepting) is replaced.
            if UnixStream::connect(&path).is_ok() {
                return Err(Error::InUse);
            }
            fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path).map_err(|error| {
            if error.kind() == ErrorKind::AddrInUse {
                Error::InUse
            } else {
                Error::Io(error)
            }
        })?;
        Ok(Self {
            listener,
            cancelled: AtomicBool::new(false),
            path,
        })
    }

    pub(super) fn accept(&self) -> Result<Connection, Error> {
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return Err(Error::Cancelled);
            }
            let (stream, _) = self.listener.accept()?;
            if self.cancelled.load(Ordering::Acquire) {
                return Err(Error::Cancelled);
            }
            // Accepted sockets can inherit O_NONBLOCK on macOS; setup is blocking.
            stream.set_nonblocking(false)?;
            match identity(&stream) {
                Ok(peer) => return Ok(Connection { stream, peer }),
                // A client that left before identification, such as another
                // bind's liveness probe: macOS then has no peer PID. Skip it.
                Err(error) if !is_alive(&stream) => drop(error),
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub(super) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        // Wake a blocked accept with a throwaway connection it then discards.
        let _ = UnixStream::connect(&self.path);
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(super) fn connect(endpoint: &Endpoint) -> Result<Connection, Error> {
    let directory = private_directory(false)?;
    let stream = UnixStream::connect(directory.join(format!("{}.sock", endpoint.name)))?;
    let server = identity(&stream)?;
    if server.user != euid().to_string() {
        return Err(Error::UntrustedServer(format!(
            "server process {} runs as uid {}, not {}",
            server.pid,
            server.user,
            euid()
        )));
    }
    Ok(Connection { stream, peer: server })
}

pub(super) fn is_alive(stream: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let mut byte = 0_u8;
    // SAFETY: live socket; a one-byte peek never consumes protocol data.
    let peeked = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            (&mut byte as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    match peeked {
        0 => false,
        count if count > 0 => true,
        _ => matches!(io::Error::last_os_error().kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted),
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;
    use crate::local::{Scope, Transport};

    fn unique(prefix: &str) -> Endpoint {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        Endpoint::new(
            Scope::User,
            &format!("{prefix}-{}-{nonce}", std::process::id()),
            Transport::LocalStream,
        )
        .unwrap()
    }

    #[test]
    fn accept_and_connect_report_peer_credentials_and_liveness() {
        let endpoint = unique("identity");
        let listener = super::super::Listener::bind(&endpoint).unwrap();
        assert!(matches!(super::super::Listener::bind(&endpoint), Err(Error::InUse)));
        let client = thread::spawn(move || super::super::connect(&endpoint).unwrap());
        let accepted = listener.accept().unwrap();
        let connected = client.join().unwrap();
        for peer in [accepted.peer(), connected.peer()] {
            assert_eq!(peer.pid, std::process::id());
            assert_eq!(peer.user, euid().to_string());
        }
        assert!(connected.is_alive());
        drop(accepted);
        assert!(!connected.is_alive());
    }

    #[test]
    fn a_stale_socket_is_replaced_but_cancel_wakes_accept() {
        let endpoint = unique("stale");
        drop(super::super::Listener::bind(&endpoint).unwrap());
        // Simulate a crashed server's leftover socket file.
        private_directory(true).unwrap();
        let stale = UnixListener::bind(render(&endpoint).unwrap()).unwrap();
        drop(stale);
        let listener = std::sync::Arc::new(super::super::Listener::bind(&endpoint).unwrap());
        let waiting = {
            let listener = listener.clone();
            thread::spawn(move || listener.accept())
        };
        thread::sleep(std::time::Duration::from_millis(50));
        listener.cancel();
        assert!(matches!(waiting.join().unwrap(), Err(Error::Cancelled)));
    }
}
