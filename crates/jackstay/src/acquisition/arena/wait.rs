use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::SeqCst},
    },
    time::{Duration, Instant},
};

use super::{ACTIVE, ArenaConsumer, ArenaError, CAPACITY_EPOCH, LATEST, RECONFIGURATION_EPOCH, WAIT_INTEREST};

// Each OS supplies the coalescing wake primitive and its sleep. Everything
// else here (arming, epochs, cancellation precedence) is platform-neutral.
#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
mod windows;
pub(super) use sys::{Receiver, Wake, import, incarnation, producer_endpoint, producer_receiver};
#[cfg(windows)]
use windows as sys;

pub(super) const DATA: u64 = 1;
pub(super) const CAPACITY: u64 = 2;
pub(super) const RECONFIGURATION: u64 = 4;
pub(super) const CLOSED: u64 = 8;

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
        let (wake, reader) = sys::channel()?;
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
            data_cursor: self.control.word(LATEST).load(SeqCst),
            capacity_epoch: self.lifetime.claims.word(CAPACITY_EPOCH).load(SeqCst),
            reconfiguration_epoch: self.control.word(RECONFIGURATION_EPOCH).load(SeqCst),
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
        self.lifetime
            .claims
            .word(WAIT_INTEREST)
            .store(interest.0 | CLOSED | RECONFIGURATION, SeqCst);
        struct Disarm<'a>(&'a super::ClaimMap);
        impl Drop for Disarm<'_> {
            fn drop(&mut self) {
                self.0.word(WAIT_INTEREST).store(0, SeqCst);
            }
        }
        let _disarm = Disarm(&self.lifetime.claims);
        loop {
            self.receiver.drain()?;
            if cancel.is_cancelled() {
                return Ok(WaitOutcome::Cancelled);
            }
            let current = self.events();
            if interest.changed(observed, current) {
                return Ok(WaitOutcome::Changed(current));
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(WaitOutcome::TimedOut);
            }
            #[cfg(test)]
            super::concurrency_tests::run_hook(super::concurrency_tests::Phase::BeforeSleep);
            self.receiver.sleep_or_cancel(&cancel.0.reader, deadline)?;
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
            // Windows events never report this; closure uses the claim page.
            Err(error) if matches!(error.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset) => {
                self.word(ACTIVE).store(0, SeqCst);
                Ok(())
            }
            result => result,
        }
    }
}
