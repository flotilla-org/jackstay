//! Byte exchanges bounded by one absolute deadline.
//!
//! A per-operation timeout lets a stalled or trickling peer extend an exchange
//! indefinitely: each partial read or write re-arms it. [`Bounded`] instead
//! bounds every operation by the time left before one deadline, so the whole
//! exchange returns in time however the peer paces its bytes. Bootstrap's
//! preface and a host's own exchange before setup (ABI 0.11) both use it.
//!
//! - Unix: the socket is made non-blocking and each operation first `poll`s
//!   for the time left; no socket option is touched. [`Bounded::finish`] puts
//!   the stream back to blocking.
//! - Windows: named pipes are overlapped; each operation runs with the pipe's
//!   read and write timeouts set to the time left, and `finish` restores the
//!   timeouts the stream had before.
//!
//! Operations never read more than asked, so a caller reading one byte at a
//! time consumes nothing past what it needs.
//!
//! SIGPIPE: Local Endpoint connections are created with it suppressed
//! (`local::unix` connect and accept), and bootstrap suppresses it on the
//! host-supplied streams it takes, so writes here report `BrokenPipe` in a C
//! host that keeps the default disposition. Setting the option here instead
//! would fail on macOS once the peer has closed.

use std::{
    borrow::BorrowMut,
    io::{self, Read, Write},
    time::{Duration, Instant},
};

use super::Stream;

/// A stream (owned, or borrowed as `&mut Stream`) whose operations share one
/// deadline. `TimedOut` reports that the deadline passed.
pub(crate) struct Bounded<S: BorrowMut<Stream>> {
    stream: Option<S>,
    deadline: Instant,
    #[cfg(windows)]
    timeouts: (Option<Duration>, Option<Duration>),
}

impl<S: BorrowMut<Stream>> Bounded<S> {
    /// Start an exchange that must finish within `timeout` from now.
    pub(crate) fn new(stream: S, timeout: Duration) -> io::Result<Self> {
        let deadline = Instant::now() + timeout;
        #[cfg(unix)]
        {
            stream.borrow().set_nonblocking(true)?;
            Ok(Self {
                stream: Some(stream),
                deadline,
            })
        }
        #[cfg(windows)]
        {
            let pipe = stream.borrow();
            pipe.set_nonblocking(false)?;
            let timeouts = (pipe.read_timeout()?, pipe.write_timeout()?);
            Ok(Self {
                stream: Some(stream),
                deadline,
                timeouts,
            })
        }
    }

    pub(crate) fn stream(&self) -> &Stream {
        self.stream.as_ref().expect("live exchange").borrow()
    }

    pub(crate) fn stream_mut(&mut self) -> &mut Stream {
        self.stream.as_mut().expect("live exchange").borrow_mut()
    }

    fn remaining(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| io::ErrorKind::TimedOut.into())
    }

    /// Wait, within the deadline, until the stream can be read.
    pub(crate) fn readable(&self) -> io::Result<()> {
        self.ready(true)
    }

    /// Wait, within the deadline, until the stream can be written.
    pub(crate) fn writable(&self) -> io::Result<()> {
        self.ready(false)
    }

    #[cfg(unix)]
    fn ready(&self, read: bool) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        loop {
            let remaining = self.remaining()?;
            let mut fd = libc::pollfd {
                fd: self.stream().as_raw_fd(),
                events: if read { libc::POLLIN } else { libc::POLLOUT },
                revents: 0,
            };
            let millis = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
            // SAFETY: one initialized pollfd; the stream owns its live descriptor.
            let result = unsafe { libc::poll(&mut fd, 1, millis) };
            if result > 0 {
                return Ok(());
            }
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }

    // The next pipe operation blocks for at most the time left.
    #[cfg(windows)]
    fn ready(&self, _read: bool) -> io::Result<()> {
        let remaining = self.remaining()?;
        let pipe = self.stream();
        pipe.set_read_timeout(Some(remaining))?;
        pipe.set_write_timeout(Some(remaining))
    }

    /// One read of at most `bytes.len()` bytes; `Ok(0)` at end of stream.
    pub(crate) fn read_some(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            self.readable()?;
            match self.stream_mut().read(bytes) {
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
                other => return other,
            }
        }
    }

    /// One write of some of `bytes`; `Ok(0)` if the peer takes none.
    pub(crate) fn write_some(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            self.writable()?;
            match self.stream_mut().write(bytes) {
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
                other => return other,
            }
        }
    }

    /// Fill `bytes`. `UnexpectedEof` if the peer closes first.
    pub(crate) fn read_exact(&mut self, mut bytes: &mut [u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            match self.read_some(bytes)? {
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                count => bytes = &mut bytes[count..],
            }
        }
        Ok(())
    }

    /// Write all of `bytes`. `WriteZero` if the peer takes none.
    pub(crate) fn write_all(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            match self.write_some(bytes)? {
                0 => return Err(io::ErrorKind::WriteZero.into()),
                count => bytes = &bytes[count..],
            }
        }
        self.writable()?;
        self.stream_mut().flush()
    }

    /// End the exchange and hand the stream back as it was: blocking on Unix,
    /// with its previous timeouts on Windows.
    pub(crate) fn finish(mut self) -> io::Result<S> {
        let result = self.restore();
        let stream = self.stream.take().expect("live exchange");
        result.map(|()| stream)
    }

    fn restore(&self) -> io::Result<()> {
        let Some(stream) = self.stream.as_ref() else {
            return Ok(());
        };
        let stream = stream.borrow();
        #[cfg(unix)]
        return stream.set_nonblocking(false);
        #[cfg(windows)]
        {
            let read = stream.set_read_timeout(self.timeouts.0);
            let write = stream.set_write_timeout(self.timeouts.1);
            read.and(write)
        }
    }
}

// An abandoned exchange (an early return with `?`) still hands a borrowed
// stream back in its original mode.
impl<S: BorrowMut<Stream>> Drop for Bounded<S> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use super::*;

    #[cfg(unix)]
    fn pair() -> (Stream, Stream) {
        Stream::pair().unwrap()
    }

    #[cfg(windows)]
    fn pair() -> (Stream, Stream) {
        super::super::pipe_pair().unwrap()
    }

    #[test]
    fn partial_operations_share_one_deadline() {
        let (mut near, mut far) = pair();
        let trickle = thread::spawn(move || {
            // One byte every 40 ms: each read completes well inside a
            // per-read timeout, but not all of them inside the deadline.
            for _ in 0..30 {
                if far.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(40));
            }
            far
        });
        let started = Instant::now();
        let mut exchange = Bounded::new(&mut near, Duration::from_millis(300)).unwrap();
        let mut received = 0;
        let error = loop {
            let mut byte = [0];
            match exchange.read_some(&mut byte) {
                Ok(1) => received += 1,
                Ok(_) => panic!("unexpected end of stream"),
                Err(error) => break error,
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(received >= 2, "partial reads made progress: {received}");
        assert!(started.elapsed() < Duration::from_millis(900), "{:?}", started.elapsed());
        exchange.finish().unwrap();
        drop(near);
        drop(trickle.join().unwrap());
    }

    #[test]
    fn a_closed_peer_ends_reads_and_writes() {
        let (mut near, far) = pair();
        drop(far);
        let mut exchange = Bounded::new(&mut near, Duration::from_secs(5)).unwrap();
        let mut byte = [0];
        assert_eq!(exchange.read_some(&mut byte).unwrap(), 0);
        assert_eq!(exchange.read_exact(&mut byte).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        let error = exchange.write_all(&[1; 4096]).unwrap_err();
        assert!(
            matches!(error.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset),
            "{error:?}"
        );
    }

    // Rust ignores SIGPIPE at startup; a C host need not. Write to a closed
    // Local Endpoint peer with the default disposition in a child process.
    #[cfg(unix)]
    #[test]
    fn a_closed_local_endpoint_peer_is_an_error_with_default_sigpipe() {
        use crate::local::{Endpoint, Listener, Scope, Transport, connect};
        const CHILD: &str = "JACKSTAY_TEST_DEADLINE_SIGPIPE";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "local::deadline::tests::a_closed_local_endpoint_peer_is_an_error_with_default_sigpipe",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success(), "C-host signal disposition killed the child: {status}");
            return;
        }
        // SAFETY: process-wide state, changed only in this dedicated child.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        let name = format!("deadline-sigpipe-{}", std::process::id());
        let endpoint = Endpoint::new(Scope::User, &name, Transport::LocalStream).unwrap();
        let listener = Listener::bind(&endpoint).unwrap();
        let accepting = thread::spawn(move || listener.accept().unwrap().into_stream());
        let client = connect(&endpoint).unwrap().into_stream();
        let mut host = accepting.join().unwrap();
        drop(client);
        let mut exchange = Bounded::new(&mut host, Duration::from_secs(5)).unwrap();
        let error = exchange.write_all(&[1; 1 << 16]).unwrap_err();
        assert!(
            matches!(error.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset),
            "{error:?}"
        );
    }

    #[test]
    fn an_exchange_returns_the_stream_in_its_original_mode() {
        let (mut near, mut far) = pair();
        #[cfg(windows)]
        near.set_read_timeout(Some(Duration::from_secs(7))).unwrap();
        {
            let mut exchange = Bounded::new(&mut near, Duration::from_secs(5)).unwrap();
            far.write_all(b"ab").unwrap();
            let mut two = [0; 2];
            exchange.read_exact(&mut two).unwrap();
            assert_eq!(&two, b"ab");
            exchange.finish().unwrap();
        }
        #[cfg(windows)]
        {
            assert_eq!(near.read_timeout().unwrap(), Some(Duration::from_secs(7)));
            assert_eq!(near.write_timeout().unwrap(), None);
        }
        // Blocking again: a read waits for data instead of reporting WouldBlock.
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            far.write_all(b"c").unwrap();
            far
        });
        let mut byte = [0];
        near.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"c");
        drop(writer.join().unwrap());
    }
}
