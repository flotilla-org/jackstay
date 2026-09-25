//! Local Endpoints: logical addresses for same-host setup connections.
//!
//! Wheelhouse [ADR 0011] is the contract. An endpoint is a scope, a name and a
//! transport kind. Each platform renders it directly, never by mapping a path
//! to something else: a Unix socket in a private per-user runtime directory, or
//! a byte-mode named pipe under `\\.\pipe\` on Windows. The connecting side
//! verifies the server's owner before trusting it; accepting reports the peer's
//! process, user and (on Windows) session.
//!
//! This module only establishes connections. The host still authorizes each
//! accepted peer and selects the source; Jackstay's setup, bootstrap and input
//! protocols then run over the returned [`Stream`] exactly as they do over a
//! host-supplied Unix socket.
//!
//! [ADR 0011]: https://github.com/flotilla-org/wheelhouse/blob/84d3a46ee8419d38e8daefc8e5348eb973d5cd96/docs/adr/0011-windows-local-ipc-uses-named-pipes-with-logical-endpoints.md

use std::{fmt, io, time::Duration};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::ShutdownHandle;
#[cfg(windows)]
pub use windows::{Access, PipeShutdown as ShutdownHandle, PipeStream, pipe_pair, receive_handles, send_handles};

/// The byte stream every setup protocol runs over: a connected Unix stream
/// socket, or a named-pipe instance on Windows.
#[cfg(unix)]
pub type Stream = std::os::unix::net::UnixStream;
/// The byte stream every setup protocol runs over: a connected Unix stream
/// socket, or a named-pipe instance on Windows.
#[cfg(windows)]
pub type Stream = PipeStream;

/// How long [`connect`] waits for a busy endpoint to offer an instance.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Who may reach an endpoint, and so where it lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Any process of the current user (and SYSTEM, on Windows).
    User,
    /// Only processes in the current logon session. On Windows the pipe's DACL
    /// names the logon SID and accept refuses a peer from another Windows
    /// session, so a Session 0 SSH process cannot pass for a desktop one.
    /// macOS and Linux have no such session boundary; there it equals `User`.
    Session,
}

/// The transport an endpoint uses. Only local streams exist so far; the kind
/// leaves room for remote variants without pretending they are local paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    LocalStream,
}

/// A logical address. Its platform rendering is derived, never supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    scope: Scope,
    name: String,
    transport: Transport,
}

/// Longest accepted endpoint name, in bytes.
pub const MAX_NAME: usize = 64;

impl Endpoint {
    /// `name` is 1..=64 ASCII letters, digits, `.`, `_` or `-`, not starting
    /// with `.`; it cannot name a directory or escape the endpoint namespace.
    pub fn new(scope: Scope, name: &str, transport: Transport) -> io::Result<Self> {
        let valid = !name.is_empty()
            && name.len() <= MAX_NAME
            && !name.starts_with('.')
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
        if !valid {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid Local Endpoint name"));
        }
        Ok(Self {
            scope,
            name: name.to_owned(),
            transport,
        })
    }

    #[must_use]
    pub fn scope(&self) -> Scope {
        self.scope
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn transport(&self) -> Transport {
        self.transport
    }

    /// The platform address: a socket path, or a `\\.\pipe\` name. For
    /// diagnostics; [`Listener::bind`] and [`connect`] render it themselves.
    pub fn render(&self) -> io::Result<String> {
        #[cfg(unix)]
        return unix::render(self).map(|path| path.display().to_string());
        #[cfg(windows)]
        return windows::render(self);
    }
}

/// Kernel-reported identity of the process on the other end of a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    pub pid: u32,
    /// Windows: the process token's user SID (`S-1-5-...`). Unix: the
    /// effective UID in decimal.
    pub user: String,
    /// Windows session ID. macOS and Linux have no session concept here.
    pub session: Option<u32>,
}

/// Why a connection was not established or not accepted.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Local Endpoint I/O: {0}")]
    Io(#[from] io::Error),
    /// The server is not owned by the expected user or is in another session.
    #[error("Local Endpoint server refused: {0}")]
    UntrustedServer(String),
    /// The accepted peer failed the endpoint's policy; it has been disconnected
    /// and the listener remains usable.
    #[error("Local Endpoint peer refused: {0}")]
    RefusedPeer(String),
    /// Another server already owns this endpoint (or a stale one could not be
    /// verified away). Creation never takes over an existing endpoint.
    #[error("Local Endpoint is already in use")]
    InUse,
    #[error("Local Endpoint wait cancelled")]
    Cancelled,
}

/// One connected stream and the verified identity of its peer.
pub struct Connection {
    stream: Stream,
    peer: PeerIdentity,
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connection").field("peer", &self.peer).finish_non_exhaustive()
    }
}

impl Connection {
    #[must_use]
    pub fn peer(&self) -> &PeerIdentity {
        &self.peer
    }

    /// Hand the stream to one setup protocol (CPU setup, bootstrap or input).
    #[must_use]
    pub fn into_stream(self) -> Stream {
        self.stream
    }

    #[must_use]
    pub fn stream(&self) -> &Stream {
        &self.stream
    }

    /// Whether the peer still holds its end, without consuming protocol bytes.
    pub fn is_alive(&self) -> bool {
        is_alive(&self.stream)
    }
}

/// A bound endpoint. It holds the address for its whole life: on Windows at
/// least one pipe instance always exists, created first with
/// `FILE_FLAG_FIRST_PIPE_INSTANCE`; on Unix the socket file.
pub struct Listener {
    #[cfg(unix)]
    inner: unix::Listener,
    #[cfg(windows)]
    inner: windows::PipeListener,
}

impl Listener {
    /// Create the endpoint. An endpoint served by another live listener is
    /// [`Error::InUse`], never taken over.
    pub fn bind(endpoint: &Endpoint) -> Result<Self, Error> {
        Ok(Self {
            #[cfg(unix)]
            inner: unix::Listener::bind(endpoint)?,
            #[cfg(windows)]
            inner: windows::PipeListener::bind(endpoint)?,
        })
    }

    /// Wait for one client. Clients that leave before they can be identified
    /// are skipped. [`Error::RefusedPeer`] reports a client that failed the
    /// endpoint's policy (another Windows session on a session-bound
    /// endpoint); the listener stays usable, as it does after other errors. [`Error::Cancelled`] follows
    /// [`Self::cancel`]. Accepts on one listener are serialized.
    pub fn accept(&self) -> Result<Connection, Error> {
        self.inner.accept()
    }

    /// Permanently wake and fail current and future accepts. Thread-safe.
    pub fn cancel(&self) {
        self.inner.cancel();
    }
}

/// Connect to an endpoint and verify its server before any protocol bytes flow:
/// the server process must belong to the current user and, for a session-bound
/// endpoint, to the current Windows session. Waits up to [`CONNECT_TIMEOUT`]
/// for a busy server.
pub fn connect(endpoint: &Endpoint) -> Result<Connection, Error> {
    #[cfg(unix)]
    return unix::connect(endpoint);
    #[cfg(windows)]
    return windows::connect(endpoint, &windows::ServerPolicy::current(endpoint.scope)?);
}

/// Whether the peer still holds its end of `stream`. Nonblocking and never
/// consumes bytes: pending protocol data counts as alive. A closed or broken
/// connection, or one shut down locally, reports false.
pub fn is_alive(stream: &Stream) -> bool {
    #[cfg(unix)]
    return unix::is_alive(stream);
    #[cfg(windows)]
    return stream.is_alive();
}

/// A private handle that only interrupts I/O on its stream.
pub fn shutdown_handle(stream: &Stream) -> io::Result<ShutdownHandle> {
    #[cfg(unix)]
    return stream.try_clone().map(ShutdownHandle);
    #[cfg(windows)]
    return Ok(stream.shutdown_handle());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cannot_escape_the_endpoint_namespace() {
        for name in ["", ".hidden", "a/b", "a\\b", "..", "white space", "é", &"x".repeat(MAX_NAME + 1)] {
            assert!(Endpoint::new(Scope::User, name, Transport::LocalStream).is_err(), "{name:?}");
        }
        assert!(Endpoint::new(Scope::User, "source-1.cpu_2", Transport::LocalStream).is_ok());
    }
}
