use std::{
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::SeqCst},
    },
    time::{Duration, Instant},
};

use super::{ACTIVE, ArenaConsumer, ArenaError, CAPACITY_EPOCH, LATEST, RECONFIGURATION_EPOCH, WAIT_INTEREST};

pub(super) const DATA: u64 = 1;
pub(super) const CAPACITY: u64 = 2;
pub(super) const RECONFIGURATION: u64 = 4;
pub(super) const CLOSED: u64 = 8;

#[derive(Debug)]
pub(super) struct Wake(UnixStream);

#[derive(Debug)]
pub(super) struct Receiver(UnixStream);

pub(super) fn channel() -> io::Result<(Arc<Wake>, Receiver)> {
    let (writer, reader) = UnixStream::pair()?;
    writer.set_nonblocking(true)?;
    reader.set_nonblocking(true)?;
    Ok((Arc::new(Wake(writer)), Receiver(reader)))
}

impl Wake {
    pub(super) fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        let stream = UnixStream::from(fd);
        stream.set_nonblocking(true)?;
        Ok(Self(stream))
    }

    pub(super) fn fd(&self) -> io::Result<OwnedFd> {
        self.0.try_clone().map(OwnedFd::from)
    }

    pub(super) fn signal(&self) -> io::Result<()> {
        // UnixStream writes suppress SIGPIPE, including when this library is
        // called from a C host: https://doc.rust-lang.org/std/os/unix/net/struct.UnixStream.html#sigpipe
        loop {
            match (&self.0).write(&[1]) {
                Ok(1) => return Ok(()),
                Ok(_) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                // A pending byte is sufficient: the waiter rechecks shared
                // state, rather than counting notifications as frame events.
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }
}

impl Receiver {
    pub(super) fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        let stream = UnixStream::from(fd);
        stream.set_nonblocking(true)?;
        Ok(Self(stream))
    }

    pub(super) fn into_fd(self) -> OwnedFd {
        self.0.into()
    }

    fn drain(&mut self) -> io::Result<()> {
        // Bounded drain: a notification flood must not starve cancellation or
        // the predicate recheck. Residual bytes keep the next poll readable.
        let mut bytes = [0; 4096];
        loop {
            match self.0.read(&mut bytes) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }
}

/// Snapshot BEFORE checking the acquisition predicate, then pass it to wait if
/// acquisition reports no usable result. Epochs are notifications, not leases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitEvents {
    pub data_cursor: u64,
    pub capacity_epoch: u64,
    pub reconfiguration_epoch: u64,
    pub closed: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct WaitInterest(u64);

impl WaitInterest {
    pub const DATA: Self = Self(DATA);
    pub const CAPACITY: Self = Self(CAPACITY);
    pub const RECONFIGURATION: Self = Self(RECONFIGURATION);
    pub const ALL: Self = Self(DATA | CAPACITY | RECONFIGURATION);

    fn changed(self, before: WaitEvents, after: WaitEvents) -> bool {
        after.closed
            || before.reconfiguration_epoch != after.reconfiguration_epoch
            || (self.0 & DATA != 0 && before.data_cursor != after.data_cursor)
            || (self.0 & CAPACITY != 0 && before.capacity_epoch != after.capacity_epoch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    Changed(WaitEvents),
    Cancelled,
    TimedOut,
}

#[derive(Debug)]
struct CancellationInner {
    cancelled: AtomicBool,
    wake: Arc<Wake>,
    reader: Receiver,
}

/// A one-way cancellation token. Clone it to another thread; cancellation is
/// permanent and level-triggered, including for multiple arena waiters. Create
/// another token for a new operation. Cancelling never releases frame leases.
#[derive(Debug, Clone)]
pub struct Cancellation(Arc<CancellationInner>);

impl Cancellation {
    pub fn new() -> io::Result<Self> {
        let (wake, reader) = channel()?;
        Ok(Self(Arc::new(CancellationInner {
            cancelled: AtomicBool::new(false),
            wake,
            reader,
        })))
    }

    pub fn cancel(&self) -> io::Result<()> {
        if !self.0.cancelled.swap(true, SeqCst) {
            self.0.wake.signal()?;
        }
        Ok(())
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(SeqCst)
    }
}

impl ArenaConsumer {
    #[must_use]
    pub fn events(&self) -> WaitEvents {
        WaitEvents {
            data_cursor: self.inner.map.word(LATEST).load(SeqCst),
            capacity_epoch: self.inner.claims.word(CAPACITY_EPOCH).load(SeqCst),
            reconfiguration_epoch: self.inner.map.word(RECONFIGURATION_EPOCH).load(SeqCst),
            closed: self.is_closed(),
        }
    }

    /// Efficient wait for changes relative to a snapshot taken before the
    /// acquisition predicate. `&mut self` gives this notification channel one
    /// reader; held frames can still release from other threads. Closure and
    /// reconfiguration are always relevant: releasing old frames may be needed
    /// before more data can arrive. Cancellation wins over a simultaneous event.
    pub fn wait(
        &mut self,
        observed: WaitEvents,
        interest: WaitInterest,
        cancel: &Cancellation,
        timeout: Option<Duration>,
    ) -> Result<WaitOutcome, ArenaError> {
        let deadline = match timeout {
            Some(duration) => Some(
                Instant::now()
                    .checked_add(duration)
                    .ok_or(ArenaError::Configuration("wait deadline overflow"))?,
            ),
            None => None,
        };
        // Arm before draining/rechecking. A writer either precedes the recheck
        // (its state change is seen) or observes the arm and signals the poll.
        self.inner
            .claims
            .word(WAIT_INTEREST)
            .store(interest.0 | CLOSED | RECONFIGURATION, SeqCst);
        struct Disarm<'a>(&'a super::ClaimMap);
        impl Drop for Disarm<'_> {
            fn drop(&mut self) {
                self.0.word(WAIT_INTEREST).store(0, SeqCst);
            }
        }
        let _disarm = Disarm(&self.inner.claims);
        loop {
            self.receiver.drain()?;
            if cancel.is_cancelled() {
                return Ok(WaitOutcome::Cancelled);
            }
            let current = self.events();
            if interest.changed(observed, current) {
                return Ok(WaitOutcome::Changed(current));
            }
            let milliseconds = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Ok(WaitOutcome::TimedOut);
                    }
                    remaining
                        .as_millis()
                        .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0))
                        .min(i32::MAX as u128) as i32
                }
                None => -1,
            };
            #[cfg(test)]
            super::concurrency_tests::run_hook(super::concurrency_tests::Phase::BeforeSleep);
            let mut descriptors = [
                libc::pollfd {
                    fd: self.receiver.0.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: cancel.0.reader.0.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: both FDs and the initialized two-entry pollfd array live
            // through the call. No application locks are held while sleeping.
            let result = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as libc::nfds_t, milliseconds) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error.into());
                }
            } else if descriptors.iter().any(|fd| fd.revents & (libc::POLLERR | libc::POLLNVAL) != 0) {
                return Err(io::Error::other("acquisition wait channel failed").into());
            }
            // Cancellation is deliberately never drained: every current and
            // future waiter must see it. Data bytes are coalesced above.
        }
    }
}

impl super::ClaimMap {
    pub(super) fn signal(&self, interest: u64) -> io::Result<()> {
        if self.word(WAIT_INTEREST).load(SeqCst) & interest == 0 {
            return Ok(());
        }
        match self.wake.signal() {
            // The consumer has destroyed its wait reader. Outstanding frame
            // claims still drain normally; this is not a reclamation signal.
            Err(error) if matches!(error.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset) => {
                self.word(ACTIVE).store(0, SeqCst);
                Ok(())
            }
            result => result,
        }
    }
}
