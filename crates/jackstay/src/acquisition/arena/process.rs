//! Kernel lifetime observations, installed before a process-bound grant escapes.
//! No PID lookup after admission is used as evidence of exit or permission to
//! reclaim. See docs/design/acquisition-process-cleanup.md for source evidence.

use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
};

#[derive(Debug)]
pub(super) struct ProcessWatch {
    fd: OwnedFd,
    #[cfg(target_os = "macos")]
    pid: u32,
}

impl ProcessWatch {
    pub(super) fn new(pid: u32) -> io::Result<Self> {
        if pid == 0 || pid > i32::MAX as u32 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid consumer process ID"));
        }
        Self::open(pid)
    }

    pub(super) fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    #[cfg(target_os = "macos")]
    fn open(pid: u32) -> io::Result<Self> {
        // SAFETY: kqueue returns a new owned descriptor on success.
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fresh descriptor above, transferred into this sole owner.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: owned descriptor, no pointer argument.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let change = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT | libc::EV_RECEIPT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let mut receipt = change;
        let timeout = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: initialized input/output structs and the FD outlive the call.
        let count = unsafe { libc::kevent(fd.as_raw_fd(), &change, 1, &mut receipt, 1, &timeout) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        if count != 1 || receipt.flags & libc::EV_ERROR == 0 {
            return Err(io::Error::other("missing process-watch registration receipt"));
        }
        if receipt.data != 0 {
            return Err(io::Error::from_raw_os_error(receipt.data as i32));
        }
        Ok(Self { fd, pid })
    }

    #[cfg(target_os = "macos")]
    pub(super) fn exited(&self) -> io::Result<bool> {
        let mut event = libc::kevent {
            ident: 0,
            filter: 0,
            flags: 0,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let timeout = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        loop {
            // SAFETY: initialized event/timeout and owned FD live through kevent.
            let count = unsafe { libc::kevent(self.fd(), std::ptr::null(), 0, &mut event, 1, &timeout) };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if count == 0 {
                return Ok(false);
            }
            if event.flags & libc::EV_ERROR != 0 {
                return Err(io::Error::from_raw_os_error(event.data as i32));
            }
            if event.filter != libc::EVFILT_PROC || event.ident != self.pid as libc::uintptr_t || event.fflags & libc::NOTE_EXIT == 0 {
                return Err(io::Error::other("unexpected process-watch event"));
            }
            return Ok(true);
        }
    }

    #[cfg(target_os = "linux")]
    fn open(pid: u32) -> io::Result<Self> {
        // No PIDFD_THREAD: readability must mean the entire thread group ended.
        // SAFETY: scalar syscall arguments; success returns a fresh CLOEXEC FD.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful pidfd_open returned this owned descriptor.
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(raw as RawFd) },
        })
    }

    #[cfg(target_os = "linux")]
    pub(super) fn exited(&self) -> io::Result<bool> {
        let mut event = libc::pollfd {
            fd: self.fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // SAFETY: initialized pollfd and its owned descriptor remain alive.
            let count = unsafe { libc::poll(&mut event, 1, 0) };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if event.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                return Err(io::Error::other("process-watch descriptor failed"));
            }
            // POLLIN: last thread exited; POLLHUP: that process was reaped.
            return Ok(event.revents & (libc::POLLIN | libc::POLLHUP) != 0);
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn open(_pid: u32) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process lifetime observation is unavailable on this platform",
        ))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub(super) fn exited(&self) -> io::Result<bool> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process lifetime observation is unavailable on this platform",
        ))
    }
}
