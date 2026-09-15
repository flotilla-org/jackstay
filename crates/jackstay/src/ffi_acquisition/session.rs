//! Owned CPU setup connections, with optional host session selection.

use std::{
    ffi::{CStr, c_char},
    net::Shutdown,
    os::unix::net::UnixStream,
    ptr,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use super::{FtAcquisitionConsumer, status};
use crate::{
    acquisition::{arena::ConfigurationInstall, socket::CpuSetupClient},
    daemon::{self, ConnectedSession},
    ffi::*,
};

pub struct FtCpuAcquisitionConnection {
    client: Mutex<CpuSetupClient>,
    shutdown: UnixStream,
    cancelled: AtomicBool,
}

impl FtCpuAcquisitionConnection {
    fn new(client: CpuSetupClient) -> std::io::Result<Self> {
        let shutdown = client.shutdown_handle()?;
        Ok(Self {
            client: Mutex::new(client),
            shutdown,
            cancelled: AtomicBool::new(false),
        })
    }
}

/// Own an already connected host-selected/authorized Unix setup stream.
/// No admission I/O happens until attach, so cancellation can be installed first.
///
/// # Safety
/// fd and out are writable/disjoint; fd owns the live stream; *out is null.
/// The peer is the conforming sole producer. This process must be the original
/// peer and sole recipient of grants; no caller FD copies, fork, forwarding or
/// replay is allowed. After basic checks fd is consumed/set to -1 on all outcomes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_cpu_connection_create(fd: *mut i32, out: *mut *mut FtCpuAcquisitionConnection) -> FtStatus {
    // SAFETY: caller supplies valid disjoint writable storage.
    let (Some(fd), Some(out)) = (unsafe { fd.as_mut() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if *fd < 0 || !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: caller transfers sole ownership and grants are bound to this peer.
    let Ok(stream) = (unsafe { super::setup_server::take_stream(fd) }) else {
        return FT_STATUS_ERROR;
    };
    // SAFETY: caller guarantees the trusted producer and sole-recipient contract.
    let client = unsafe { CpuSetupClient::from_stream(stream) };
    match FtCpuAcquisitionConnection::new(client) {
        Ok(connection) => {
            *out = Box::into_raw(Box::new(connection));
            FT_STATUS_OK
        }
        Err(_) => FT_STATUS_ERROR,
    }
}

/// Admit a process-bound consumer. May block on setup I/O; cancel interrupts it.
///
/// # Safety
/// Connection is live, output is writable, disjoint and null. Serialize attach
/// and configuration operations; cancel may run concurrently. Imported maps and
/// frames must never be forked, forwarded or replayed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_cpu_attach(
    connection: *mut FtCpuAcquisitionConnection,
    holding: u32,
    out: *mut *mut FtAcquisitionConsumer,
) -> FtStatus {
    // SAFETY: caller supplies live connection and disjoint writable output.
    let (Some(connection), Some(out)) = (unsafe { connection.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if holding == 0 || !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    if connection.cancelled.load(Ordering::Acquire) {
        return FT_STATUS_CANCELLED;
    }
    let Ok(mut client) = connection.client.lock() else {
        return FT_STATUS_ERROR;
    };
    let result = client.attach(holding);
    if connection.cancelled.load(Ordering::Acquire) {
        return FT_STATUS_CANCELLED;
    }
    match result {
        Ok(consumer) => {
            *out = FtAcquisitionConsumer::into_raw(consumer);
            FT_STATUS_OK
        }
        Err(_) => FT_STATUS_ERROR,
    }
}

/// Permanently interrupt this connection's setup I/O. Frames remain owned.
///
/// # Safety
/// Connection is null or live. May run concurrently with attach/configuration,
/// but all calls must return before destroying the connection handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_cpu_connection_cancel(connection: *const FtCpuAcquisitionConnection) {
    // SAFETY: caller keeps the handle alive until this operation returns.
    if let Some(connection) = unsafe { connection.as_ref() } {
        connection.cancelled.store(true, Ordering::Release);
        let _ = connection.shutdown.shutdown(Shutdown::Both);
    }
}

/// Select a trusted daemon's session, authorize it and request holding capacity.
///
/// # Safety
/// Strings are live NUL-terminated UTF-8 (token may be null). Outputs are writable,
/// exclusive and do not alias; both handle outputs start null. The daemon must
/// obey the common sole-producer contract. Never fork/forward/replay mappings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_cpu_connect_session(
    control_path: *const c_char,
    session_id: *const c_char,
    token: *const c_char,
    holding: u32,
    out_connection: *mut *mut FtCpuAcquisitionConnection,
    out_consumer: *mut *mut FtAcquisitionConsumer,
    out_track: *mut u64,
) -> FtStatus {
    if control_path.is_null() || session_id.is_null() || holding == 0 {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: output validity and exclusivity are caller obligations.
    let (Some(connection), Some(consumer), Some(track)) = (unsafe { out_connection.as_mut() }, unsafe { out_consumer.as_mut() }, unsafe {
        out_track.as_mut()
    }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !connection.is_null() || !consumer.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    *track = 0;
    // SAFETY: the caller supplies live NUL-terminated strings.
    let (Ok(control_path), Ok(session_id)) = (
        unsafe { CStr::from_ptr(control_path) }.to_str(),
        unsafe { CStr::from_ptr(session_id) }.to_str(),
    ) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let token = if token.is_null() {
        None
    } else {
        // SAFETY: non-null token is a live string for this call.
        let Ok(token) = unsafe { CStr::from_ptr(token) }.to_str() else {
            return FT_STATUS_INVALID_ARGUMENT;
        };
        Some(token.to_owned())
    };
    let Ok(mut info) = daemon::get_session(control_path, session_id) else {
        return FT_STATUS_ERROR;
    };
    info.bearer_token = token;
    // SAFETY: delegated to the caller's trusted daemon / sole-recipient contract.
    let Ok(session) = (unsafe { ConnectedSession::connect(info, holding) }) else {
        return FT_STATUS_ERROR;
    };
    let Ok(setup) = FtCpuAcquisitionConnection::new(session.setup) else {
        return FT_STATUS_ERROR;
    };
    *track = session.info.track_id;
    *connection = Box::into_raw(Box::new(setup));
    *consumer = FtAcquisitionConsumer::into_raw(session.consumer);
    FT_STATUS_OK
}

/// Install a pending CPU generation on this connection's existing consumer.
///
/// # Safety
/// Handles belong to the matching setup. Consumer is exclusive. Serialize setup
/// calls; connection cancellation may run concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_cpu_install_configuration(
    connection: *mut FtCpuAcquisitionConnection,
    consumer: *mut FtAcquisitionConsumer,
) -> FtStatus {
    // SAFETY: caller supplies live exclusive handles.
    let (Some(connection), Some(consumer)) = (unsafe { connection.as_ref() }, unsafe { consumer.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if connection.cancelled.load(Ordering::Acquire) {
        return FT_STATUS_CANCELLED;
    }
    let Ok(mut client) = connection.client.lock() else {
        return FT_STATUS_ERROR;
    };
    let result = client.install_configuration(&mut consumer.0);
    if connection.cancelled.load(Ordering::Acquire) {
        return FT_STATUS_CANCELLED;
    }
    match result {
        Ok(Some(ConfigurationInstall::Installed)) => FT_STATUS_OK,
        Ok(Some(ConfigurationInstall::Stale)) => FT_STATUS_STALE,
        Ok(None) => FT_STATUS_EMPTY,
        Err(crate::acquisition::socket::SocketError::Arena(error)) => status(error),
        Err(_) => FT_STATUS_ERROR,
    }
}

/// # Safety
/// Pointer is writable and exclusively owns its handle. No concurrent setup call
/// may use it. Consumer and acquired-frame handles retain independent ownership.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_cpu_connection_destroy(connection: *mut *mut FtCpuAcquisitionConnection) {
    // SAFETY: exclusive handle ownership is a caller obligation.
    if let Some(connection) = unsafe { connection.as_mut() }
        && !connection.is_null()
    {
        // SAFETY: this box was produced by this API and is consumed once.
        drop(unsafe { Box::from_raw(std::mem::replace(connection, ptr::null_mut())) });
    }
}
