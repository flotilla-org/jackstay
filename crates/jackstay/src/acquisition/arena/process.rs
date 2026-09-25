//! Kernel lifetime observations, installed before a process-bound grant escapes.
//! No PID lookup after admission is used as evidence of exit or permission to
//! reclaim. See docs/design/acquisition-process-cleanup.md for source evidence.

use std::io;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle, RawHandle};

#[derive(Debug)]
pub(super) struct ProcessWatch {
    #[cfg(unix)]
    fd: OwnedFd,
    #[cfg(target_os = "macos")]
    pid: u32,
    /// A process handle with `SYNCHRONIZE`. It pins the process object, so
    /// PID reuse after exit cannot redirect the watch to another process.
    #[cfg(windows)]
    handle: OwnedHandle,
}

impl ProcessWatch {
    pub(super) fn new(pid: u32) -> io::Result<Self> {
        if pid == 0 || (cfg!(unix) && pid > i32::MAX as u32) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid consumer process ID"));
        }
        Self::open(pid)
    }

    #[cfg(unix)]
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

    /// Resolve the PID to a process object now. As with `pidfd_open`, the
    /// caller must know the PID still names the admitted process at this
    /// point: for example an unwaited child (whose handle, held by the parent,
    /// keeps the PID from being reused), or a setup-channel peer whose
    /// connection is still established.
    #[cfg(windows)]
    fn open(pid: u32) -> io::Result<Self> {
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};

        // SAFETY: scalar arguments; success returns a fresh, non-inheritable handle.
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw is a fresh process handle owned by nothing else.
        Ok(Self {
            handle: unsafe { OwnedHandle::from_raw_handle(raw) },
        })
    }

    /// Watch a process through a handle the caller already holds, returning
    /// the watch and that process's ID. The handle names the process object
    /// itself, so no PID resolution happens here at all. It needs
    /// `SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION`.
    #[cfg(windows)]
    pub(super) fn from_handle(process: BorrowedHandle<'_>) -> io::Result<(Self, u32)> {
        use windows_sys::Win32::{
            Foundation::DuplicateHandle,
            System::Threading::{GetCurrentProcess, GetProcessId, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE},
        };

        let mut raw = std::ptr::null_mut();
        // SAFETY: the borrowed handle is live for the call; the pseudo-handle
        // for this process needs no closing. The duplicate is non-inheritable
        // and limited to the rights the watch needs.
        let duplicated = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                process.as_raw_handle(),
                GetCurrentProcess(),
                &mut raw,
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                0,
            )
        };
        if duplicated == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: DuplicateHandle succeeded, so raw is a fresh owned handle.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        // SAFETY: live process handle with PROCESS_QUERY_LIMITED_INFORMATION.
        let pid = unsafe { GetProcessId(handle.as_raw_handle()) };
        if pid == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((Self { handle }, pid))
    }

    #[cfg(windows)]
    pub(super) fn handle(&self) -> RawHandle {
        self.handle.as_raw_handle()
    }

    /// A process object is signaled once its last thread has terminated, and
    /// stays signaled: the event cannot be consumed or missed.
    #[cfg(windows)]
    pub(super) fn exited(&self) -> io::Result<bool> {
        use windows_sys::Win32::{
            Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
            System::Threading::WaitForSingleObject,
        };

        // SAFETY: the owned handle is live for the call; zero timeout polls.
        match unsafe { WaitForSingleObject(self.handle(), 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    fn open(_pid: u32) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process lifetime observation is unavailable on this platform",
        ))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    pub(super) fn exited(&self) -> io::Result<bool> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process lifetime observation is unavailable on this platform",
        ))
    }
}
