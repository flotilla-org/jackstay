//! C host setup: one worker per explicitly authorized connection. No listener,
//! routing, token policy, or frame copies belong to this setup owner.

use std::{
    net::Shutdown,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::net::UnixStream,
    },
    ptr,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use super::producer::FtCpuProducer;
use crate::{
    acquisition::socket::{peer_pid, serve_cpu},
    ffi::*,
};

pub struct FtCpuSetupServer {
    shutdown: UnixStream,
    cancelled: AtomicBool,
    state: Mutex<ServerWorker>,
}

struct ServerWorker {
    worker: Option<JoinHandle<FtStatus>>,
    result: Option<FtStatus>,
}

/// Consume a descriptor after basic pointer/value checks performed by the caller.
/// A wrong socket or setup error still consumes it. The duplicate retained by a
/// connection owner is private and used only to interrupt synchronous I/O.
pub(super) unsafe fn take_stream(fd: &mut i32) -> std::io::Result<UnixStream> {
    // SAFETY: caller transfers sole ownership of this live descriptor.
    let stream = unsafe { UnixStream::from_raw_fd(std::mem::replace(fd, -1)) };
    let mut kind = 0 as libc::c_int;
    let mut len = std::mem::size_of_val(&kind) as libc::socklen_t;
    // SAFETY: owned live descriptor and writable correctly sized socket option.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut len,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if kind != libc::SOCK_STREAM {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "expected connected Unix stream",
        ));
    }
    // SAFETY: sockaddr_storage can hold any socket address and all-zero is valid
    // initialization before getsockname fills its reported length.
    let mut address: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut address_len = std::mem::size_of_val(&address) as libc::socklen_t;
    // SAFETY: owned descriptor and adequately sized writable address storage.
    if unsafe {
        libc::getsockname(
            stream.as_raw_fd(),
            (&mut address as *mut libc::sockaddr_storage).cast(),
            &mut address_len,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    if address.ss_family as libc::c_int != libc::AF_UNIX {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "expected Unix socket family"));
    }
    stream.peer_addr()?;
    // peer_pid also rejects sockets without the required kernel peer identity.
    peer_pid(&stream).map_err(std::io::Error::other)?;
    stream.set_nonblocking(false)?;
    // SAFETY: this exclusively owned setup FD must not leak through exec.
    if unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(stream)
}

/// Start serving a host-selected/authorized stream. OK means the worker started,
/// not that its peer has completed admission. No socket I/O holds the arena lock.
///
/// # Safety
/// Producer is live; fd owns a connected Unix stream; out is writable and null.
/// Arguments are disjoint. Host producer calls are serialized. After basic
/// validation fd is consumed/set to -1 on every outcome. No retained caller
/// descriptor copies, concurrent stream I/O, or forwarded grants are allowed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_producer_serve(producer: *mut FtCpuProducer, fd: *mut i32, out: *mut *mut FtCpuSetupServer) -> FtStatus {
    // SAFETY: caller supplies live disjoint arguments.
    let (Some(producer), Some(fd), Some(out)) = (unsafe { producer.as_ref() }, unsafe { fd.as_mut() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if *fd < 0 || !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: basic validation passed and caller transfers sole FD ownership.
    let Ok(stream) = (unsafe { take_stream(fd) }) else {
        return FT_STATUS_ERROR;
    };
    let Ok(shutdown) = stream.try_clone() else {
        return FT_STATUS_ERROR;
    };
    let arena = producer.0.clone();
    let worker = match thread::Builder::new()
        .name("jackstay-cpu-setup".into())
        .spawn(move || match serve_cpu(stream, arena) {
            Ok(()) => FT_STATUS_OK,
            Err(_) => FT_STATUS_ERROR,
        }) {
        Ok(worker) => worker,
        Err(_) => return FT_STATUS_ERROR,
    };
    *out = Box::into_raw(Box::new(FtCpuSetupServer {
        shutdown,
        cancelled: AtomicBool::new(false),
        state: Mutex::new(ServerWorker {
            worker: Some(worker),
            result: None,
        }),
    }));
    FT_STATUS_OK
}

/// Interrupt setup I/O; already acquired frames are not released.
///
/// # Safety
/// Server is null or live. May run concurrently with poll, but not destroy.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_setup_server_cancel(server: *const FtCpuSetupServer) {
    // SAFETY: caller keeps the handle alive until this call returns.
    if let Some(server) = unsafe { server.as_ref() } {
        server.cancelled.store(true, Ordering::Release);
        let _ = server.shutdown.shutdown(Shutdown::Both);
    }
}

impl FtCpuSetupServer {
    fn finish(&self, wait: bool) -> FtStatus {
        let Ok(mut state) = self.state.lock() else {
            return FT_STATUS_ERROR;
        };
        if state.result.is_none() {
            if !wait && state.worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
                return FT_STATUS_DRAINING;
            }
            state.result = Some(
                state
                    .worker
                    .take()
                    .map_or(FT_STATUS_ERROR, |worker| worker.join().unwrap_or(FT_STATUS_ERROR)),
            );
        }
        if self.cancelled.load(Ordering::Acquire) {
            FT_STATUS_CANCELLED
        } else {
            state.result.unwrap_or(FT_STATUS_ERROR)
        }
    }
}

/// Observe terminal worker status without blocking. DRAINING means still running.
///
/// # Safety
/// Server is live and exclusively used, except for concurrent cancel.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_setup_server_poll(server: *mut FtCpuSetupServer) -> FtStatus {
    // SAFETY: caller serializes polling and destruction.
    let Some(server) = (unsafe { server.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    server.finish(false)
}

/// Cancel, join the setup worker and destroy its handle, returning final status.
/// This can wait for an in-progress arena operation, but interrupts socket I/O.
///
/// # Safety
/// Pointer exclusively owns the handle. All other calls have returned. Null
/// *server is harmless. This closes admission, not outstanding frame ownership.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_cpu_setup_server_destroy(server: *mut *mut FtCpuSetupServer) -> FtStatus {
    // SAFETY: caller supplies exclusive writable handle storage.
    let Some(out) = (unsafe { server.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if out.is_null() {
        return FT_STATUS_OK;
    }
    // SAFETY: exactly this API allocated the handle, now exclusively owned.
    let server = unsafe { Box::from_raw(std::mem::replace(out, ptr::null_mut())) };
    if server.finish(false) == FT_STATUS_DRAINING {
        server.cancelled.store(true, Ordering::Release);
        let _ = server.shutdown.shutdown(Shutdown::Both);
    }
    server.finish(true)
}
