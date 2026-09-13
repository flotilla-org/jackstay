//! Host session selection followed by common CPU acquisition, without a C lease table.

use std::{
    ffi::{CStr, c_char},
    ptr,
};

use super::{FtAcquisitionConsumer, status};
use crate::{
    acquisition::{arena::ConfigurationInstall, socket::CpuSetupClient},
    daemon::{self, ConnectedSession},
    ffi::*,
};

pub struct FtCpuAcquisitionConnection(CpuSetupClient);

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
    *track = session.info.track_id;
    *connection = Box::into_raw(Box::new(FtCpuAcquisitionConnection(session.setup)));
    *consumer = FtAcquisitionConsumer::into_raw(session.consumer);
    FT_STATUS_OK
}

/// Install a pending CPU generation on this connection's existing consumer.
///
/// # Safety
/// Both pointers must exclusively borrow live handles from the matching setup.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_cpu_install_configuration(
    connection: *mut FtCpuAcquisitionConnection,
    consumer: *mut FtAcquisitionConsumer,
) -> FtStatus {
    // SAFETY: caller supplies live exclusive handles.
    let (Some(connection), Some(consumer)) = (unsafe { connection.as_mut() }, unsafe { consumer.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    match connection.0.install_configuration(&mut consumer.0) {
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
        // SAFETY: this box was produced by connect_session and is consumed once.
        drop(unsafe { Box::from_raw(std::mem::replace(connection, ptr::null_mut())) });
    }
}
