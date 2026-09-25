//! Windows wake channels: one unnamed manual-reset event per direction.
//!
//! The Unix socket pair carries two independent directions. Here each direction
//! is its own event: the consumer event wakes the consumer's single wait reader
//! (data, capacity, reconfiguration, closure) and the producer event wakes the
//! incarnation's cleanup owner (release handoff, quiescence, completion). An
//! event is level-triggered and coalescing by construction: any number of
//! signals before the reader resets it leave exactly one pending wake, as a
//! full nonblocking socket does. The sole reader of each event resets it before
//! rechecking shared state, so a signal after that reset keeps the next wait
//! satisfied and none is lost.
//!
//! Events have no peer-closure state. That never weakens reclamation: closure
//! is carried by the shared claim page, and process exit only by the admitted
//! process handle, never by channel EOF (docs/design/acquisition-process-cleanup.md).
//!
//! Both events are unnamed and non-inheritable; their handles are the only way
//! to reach them. A setup channel duplicates them into a verified peer
//! (Wheelhouse ADR 0011): the consumer endpoint needs `SYNCHRONIZE |
//! EVENT_MODIFY_STATE` (it waits on it and signals its own capacity wakes), the
//! producer endpoint only `EVENT_MODIFY_STATE`.

use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    sync::Arc,
    time::Instant,
};

use windows_sys::Win32::{
    Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForMultipleObjects},
};

use super::super::process::ProcessWatch;

#[derive(Debug)]
pub(in crate::acquisition::arena) struct Wake(OwnedHandle);

#[derive(Debug)]
pub(in crate::acquisition::arena) struct Receiver(OwnedHandle);

fn event() -> io::Result<OwnedHandle> {
    // SAFETY: null attributes give a non-inheritable handle; a null name keeps
    // the event out of every object namespace. Manual reset, initially clear.
    let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: raw is a fresh event handle owned by nothing else.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// One direction, both ends in this process: cancellation.
pub(super) fn channel() -> io::Result<(Arc<Wake>, Receiver)> {
    let event = event()?;
    Ok((Arc::new(Wake(event.try_clone()?)), Receiver(event)))
}

/// A new incarnation's wakes: toward the consumer, toward the producer, and the
/// consumer's receiver. The consumer receiver is the consumer setup endpoint.
pub(in crate::acquisition::arena) fn incarnation() -> io::Result<(Arc<Wake>, Arc<Wake>, Receiver)> {
    let (to_consumer, receiver) = channel()?;
    let to_producer = Arc::new(Wake(event()?));
    Ok((to_consumer, to_producer, receiver))
}

/// The producer setup endpoint: the event the consumer signals for releases.
pub(in crate::acquisition::arena) fn producer_endpoint(_to_consumer: &Wake, to_producer: &Wake) -> io::Result<OwnedHandle> {
    to_producer.0.try_clone()
}

/// The cleanup owner is the producer event's sole reader.
pub(in crate::acquisition::arena) fn producer_receiver(_to_consumer: &Wake, to_producer: &Wake) -> io::Result<Receiver> {
    Ok(Receiver(to_producer.0.try_clone()?))
}

/// Rebuild both wakes from imported setup endpoints. The consumer endpoint is
/// retained by the caller for its receiver.
pub(in crate::acquisition::arena) fn import(consumer: &OwnedHandle, producer: OwnedHandle) -> io::Result<(Arc<Wake>, Arc<Wake>)> {
    Ok((Arc::new(Wake(consumer.try_clone()?)), Arc::new(Wake(producer))))
}

impl Wake {
    pub(in crate::acquisition::arena) fn signal(&self) -> io::Result<()> {
        // SAFETY: the owned event handle is live for the call.
        if unsafe { SetEvent(self.0.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Receiver {
    pub(in crate::acquisition::arena) fn sleep(&self, process: Option<&ProcessWatch>, deadline: Option<Instant>) -> io::Result<()> {
        match process {
            // A signaled process handle is a wake; the lifetime observer
            // validates the exit itself.
            Some(process) => wait_any(&[self.0.as_raw_handle(), process.handle()], deadline),
            None => wait_any(&[self.0.as_raw_handle()], deadline),
        }
        .map_err(|error| io::Error::other(format!("cleanup observation channel failed: {error}")))
    }

    /// Sleep until this receiver or `cancel` is signaled, or the deadline. The
    /// caller rechecks its predicate after every return.
    pub(super) fn sleep_or_cancel(&self, cancel: &Receiver, deadline: Option<Instant>) -> io::Result<()> {
        wait_any(&[self.0.as_raw_handle(), cancel.0.as_raw_handle()], deadline)
            .map_err(|error| io::Error::other(format!("acquisition wait channel failed: {error}")))
    }

    pub(in crate::acquisition::arena) fn from_fd(handle: OwnedHandle) -> io::Result<Self> {
        Ok(Self(handle))
    }

    pub(in crate::acquisition::arena) fn into_fd(self) -> OwnedHandle {
        self.0
    }

    /// Consume a pending wake. Only this receiver resets its event, and it
    /// rechecks shared state afterwards; a later signal stays pending.
    pub(in crate::acquisition::arena) fn drain(&mut self) -> io::Result<()> {
        // SAFETY: the owned event handle is live for the call.
        if unsafe { ResetEvent(self.0.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

fn wait_any(handles: &[HANDLE], deadline: Option<Instant>) -> io::Result<()> {
    // SAFETY: every handle is borrowed from an owner that outlives the call,
    // and the count matches the slice. No application locks are held.
    let result = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, milliseconds(deadline)) };
    match result {
        WAIT_FAILED => Err(io::Error::last_os_error()),
        WAIT_TIMEOUT => Ok(()),
        signaled if (WAIT_OBJECT_0..WAIT_OBJECT_0 + handles.len() as u32).contains(&signaled) => Ok(()),
        // Events and processes cannot be abandoned mutexes.
        other => Err(io::Error::other(format!("unexpected wait result {other:#x}"))),
    }
}

/// Wait timeout, rounded up to whole milliseconds; INFINITE without a deadline.
fn milliseconds(deadline: Option<Instant>) -> u32 {
    deadline.map_or(INFINITE, |deadline| {
        let remaining = deadline.saturating_duration_since(Instant::now());
        remaining
            .as_millis()
            .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0))
            .min(u128::from(INFINITE - 1)) as u32
    })
}
