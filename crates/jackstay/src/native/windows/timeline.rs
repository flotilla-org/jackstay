//! `ReleaseTimeline` for `ID3D11Fence`: `SetEventOnCompletion` sets an
//! auto-reset event that one persistent system thread-pool wait watches. Each
//! wake fires every pending notification whose value has completed, so
//! coalesced event sets lose nothing.

use std::{
    ffi::c_void,
    os::windows::io::{AsHandle, OwnedHandle},
    sync::{Arc, Mutex, OnceLock},
};

use ::windows::{
    Win32::{
        Foundation::{HANDLE, INVALID_HANDLE_VALUE},
        Graphics::Direct3D11::ID3D11Fence,
        System::Threading::{CreateEventW, INFINITE, RegisterWaitForSingleObject, UnregisterWaitEx, WT_EXECUTEDEFAULT},
    },
    core::PCWSTR,
};

use super::{D3d11Fence, failure, owned, raw};
use crate::{
    acquisition::arena::{ReleaseNotification, ReleaseTimeline},
    error::Result,
};

struct Waiter {
    fence: ID3D11Fence,
    event: OwnedHandle,
    pending: Mutex<Vec<(u64, ReleaseNotification)>>,
}

// SAFETY: the fence is free-threaded (see D3d11Fence); the rest is Sync.
unsafe impl Send for Waiter {}
unsafe impl Sync for Waiter {}

impl Waiter {
    fn fire(&self) {
        // SAFETY: plain query on a live fence. A removed device reports
        // u64::MAX, which releases every waiter: no GPU work can still run.
        let completed = unsafe { self.fence.GetCompletedValue() };
        let mut pending = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.retain(|(value, notification)| {
            if *value <= completed {
                let _ = notification.notify();
                false
            } else {
                true
            }
        });
    }
}

struct Registration {
    wait: HANDLE,
    waiter: Arc<Waiter>,
}

// SAFETY: the wait handle is only passed to UnregisterWaitEx, once.
unsafe impl Send for Registration {}
unsafe impl Sync for Registration {}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: INVALID_HANDLE_VALUE makes this block until any running
        // callback returns, so the Waiter it borrows outlives every callback.
        let _ = unsafe { UnregisterWaitEx(self.wait, Some(INVALID_HANDLE_VALUE)) };
    }
}

unsafe extern "system" fn fired(context: *mut c_void, _timed_out: bool) {
    // SAFETY: the context is the Waiter kept alive by its Registration, which
    // unregisters (waiting for callbacks) before releasing it.
    unsafe { &*context.cast::<Waiter>() }.fire();
}

/// Lazily registered: fences that are only polled (producer readiness) never
/// occupy a thread-pool wait.
pub(super) struct Notifier {
    fence: ID3D11Fence,
    registration: OnceLock<std::result::Result<Registration, String>>,
}

impl Notifier {
    pub(super) fn new(fence: ID3D11Fence) -> Self {
        Self {
            fence,
            registration: OnceLock::new(),
        }
    }

    fn registration(&self) -> Result<&Registration> {
        self.registration
            .get_or_init(|| {
                // SAFETY: an unnamed auto-reset event, adopted at once.
                let event = owned(unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.map_err(|error| error.to_string())?);
                let waiter = Arc::new(Waiter {
                    fence: self.fence.clone(),
                    event,
                    pending: Mutex::new(Vec::new()),
                });
                let mut wait = HANDLE::default();
                // SAFETY: the callback's context is the Waiter, which the
                // Registration keeps alive until the wait is unregistered.
                unsafe {
                    RegisterWaitForSingleObject(
                        &mut wait,
                        raw(waiter.event.as_handle()),
                        Some(fired),
                        Some(Arc::as_ptr(&waiter).cast()),
                        INFINITE,
                        WT_EXECUTEDEFAULT,
                    )
                }
                .map_err(|error| error.to_string())?;
                Ok(Registration { wait, waiter })
            })
            .as_ref()
            .map_err(|message| failure("fence-notification", message))
    }

    fn notify_at(&self, value: u64, notification: ReleaseNotification) -> Result<()> {
        let registration = self.registration()?;
        let waiter = &registration.waiter;
        // Arm the event before recording the waiter: an error then means no
        // notification was registered, as the trait requires. A completion
        // that races the recording is caught by the explicit check below.
        // SAFETY: the event lives as long as the registration.
        unsafe { self.fence.SetEventOnCompletion(value, raw(waiter.event.as_handle())) }
            .map_err(|error| failure("fence-notification", error))?;
        waiter
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((value, notification));
        waiter.fire();
        Ok(())
    }
}

impl ReleaseTimeline for D3d11Fence {
    fn completed_value(&self) -> Result<u64> {
        Ok(D3d11Fence::completed_value(self))
    }

    fn notify_at(&self, value: u64, notification: ReleaseNotification) -> Result<()> {
        self.notifier.notify_at(value, notification)
    }
}
