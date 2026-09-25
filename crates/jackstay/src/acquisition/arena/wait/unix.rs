//! Unix wake channels: a nonblocking socket pair per incarnation. The two
//! directions are independent: the producer end writes data/capacity/closure
//! wakes that the consumer end reads, and the consumer end writes release
//! handoff wakes that the producer's cleanup owner reads.

use std::{
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    sync::Arc,
    time::Instant,
};

use super::super::process::ProcessWatch;

#[derive(Debug)]
pub(in crate::acquisition::arena) struct Wake(UnixStream);

#[derive(Debug)]
pub(in crate::acquisition::arena) struct Receiver(UnixStream);

/// One direction, both ends in this process: cancellation.
pub(super) fn channel() -> io::Result<(Arc<Wake>, Receiver)> {
    let (writer, reader) = UnixStream::pair()?;
    crate::socket_options::suppress_sigpipe(&writer)?;
    writer.set_nonblocking(true)?;
    reader.set_nonblocking(true)?;
    Ok((Arc::new(Wake(writer)), Receiver(reader)))
}

/// A new incarnation's wakes: toward the consumer, toward the producer, and the
/// consumer's receiver. The consumer receiver is the consumer setup endpoint.
pub(in crate::acquisition::arena) fn incarnation() -> io::Result<(Arc<Wake>, Arc<Wake>, Receiver)> {
    let (to_consumer, receiver) = channel()?;
    let to_producer = Arc::new(Wake::from_fd(receiver.fd()?)?);
    Ok((to_consumer, to_producer, receiver))
}

/// The producer setup endpoint: the socket end the producer writes on.
pub(in crate::acquisition::arena) fn producer_endpoint(to_consumer: &Wake, _to_producer: &Wake) -> io::Result<OwnedFd> {
    to_consumer.fd()
}

/// The cleanup owner reads the reverse direction on the producer's end.
pub(in crate::acquisition::arena) fn producer_receiver(to_consumer: &Wake, _to_producer: &Wake) -> io::Result<Receiver> {
    Receiver::from_fd(to_consumer.fd()?)
}

/// Rebuild both wakes from imported setup endpoints. The consumer endpoint is
/// retained by the caller for its receiver.
pub(in crate::acquisition::arena) fn import(consumer: &OwnedFd, producer: OwnedFd) -> io::Result<(Arc<Wake>, Arc<Wake>)> {
    Ok((Arc::new(Wake::from_fd(producer)?), Arc::new(Wake::from_fd(consumer.try_clone()?)?)))
}

impl Wake {
    fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        let stream = UnixStream::from(fd);
        crate::socket_options::suppress_sigpipe(&stream)?;
        stream.set_nonblocking(true)?;
        Ok(Self(stream))
    }

    fn fd(&self) -> io::Result<OwnedFd> {
        self.0.try_clone().map(OwnedFd::from)
    }

    pub(in crate::acquisition::arena) fn signal(&self) -> io::Result<()> {
        // The writer is configured at creation/import so a closed receiver is
        // an I/O error even when the embedding host has default SIGPIPE handling.
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
    fn fd(&self) -> io::Result<OwnedFd> {
        self.0.try_clone().map(OwnedFd::from)
    }

    pub(in crate::acquisition::arena) fn sleep(&self, process: Option<&ProcessWatch>, deadline: Option<Instant>) -> io::Result<()> {
        let mut descriptors = [
            libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: process.map_or(-1, ProcessWatch::fd),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // SAFETY: initialized descriptors and their owned FDs live through poll.
            let result = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, milliseconds(deadline)) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if descriptors[0].revents & libc::POLLHUP != 0
                || descriptors
                    .iter()
                    .any(|descriptor| descriptor.revents & (libc::POLLERR | libc::POLLNVAL) != 0)
            {
                return Err(io::Error::other("cleanup observation channel failed"));
            }
            // A process descriptor's HUP is a wake, not a socket failure. The
            // lifetime observer validates its backend-specific terminal event.
            return Ok(());
        }
    }

    /// Sleep until this receiver or `cancel` is readable, or the deadline. An
    /// interrupted sleep returns early; the caller rechecks its predicate.
    pub(super) fn sleep_or_cancel(&self, cancel: &Receiver, deadline: Option<Instant>) -> io::Result<()> {
        let mut descriptors = [
            libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: cancel.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: both FDs and the initialized two-entry pollfd array live
        // through the call. No application locks are held while sleeping.
        let result = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as libc::nfds_t, milliseconds(deadline)) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        } else if descriptors.iter().any(|fd| fd.revents & (libc::POLLERR | libc::POLLNVAL) != 0) {
            return Err(io::Error::other("acquisition wait channel failed"));
        }
        Ok(())
    }

    pub(in crate::acquisition::arena) fn from_fd(fd: OwnedFd) -> io::Result<Self> {
        let stream = UnixStream::from(fd);
        stream.set_nonblocking(true)?;
        Ok(Self(stream))
    }

    pub(in crate::acquisition::arena) fn into_fd(self) -> OwnedFd {
        self.0.into()
    }

    pub(in crate::acquisition::arena) fn drain(&mut self) -> io::Result<()> {
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

/// poll(2) timeout, rounded up to whole milliseconds; -1 sleeps indefinitely.
fn milliseconds(deadline: Option<Instant>) -> i32 {
    deadline.map_or(-1, |deadline| {
        let remaining = deadline.saturating_duration_since(Instant::now());
        remaining
            .as_millis()
            .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0))
            .min(i32::MAX as u128) as i32
    })
}
