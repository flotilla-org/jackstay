//! C bootstrap ownership; the same negotiation is available to Rust hosts.
#[cfg(unix)]
use std::os::fd::IntoRawFd;
use std::ptr;

use crate::{
    bootstrap::{self, InputRequest},
    ffi::*,
    ffi_input::{self, FtInputClient, FtInputServer, FtInputTarget},
    ffi_local::{FtLocalConnection, restore_connection, take_connection},
};

pub const FT_BOOTSTRAP_INPUT_NONE: u32 = 0;
pub const FT_BOOTSTRAP_INPUT_OPTIONAL: u32 = 1;
pub const FT_BOOTSTRAP_INPUT_REQUIRED: u32 = 2;

fn status(error: bootstrap::Error) -> FtStatus {
    match error {
        bootstrap::Error::Input(error) => ffi_input::status(error),
        bootstrap::Error::Io(error) if error.kind() == std::io::ErrorKind::TimedOut => FT_STATUS_TIMEOUT,
        _ => FT_STATUS_ERROR,
    }
}

/// Authorize optional input for the host's selected source before CPU setup.
/// # Safety
/// fd and output are valid/disjoint, fd solely owns a connected Unix stream,
/// output starts null, target is null or live until return. Host selects and
/// authorizes the matching media/input resources; its executor keeps pumping.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_source_bootstrap_accept(
    fd: *mut i32,
    authorized_input: *mut FtInputTarget,
    out_input_server: *mut *mut FtInputServer,
) -> FtStatus {
    let (Some(fd), Some(out)) = (unsafe { fd.as_mut() }, unsafe { out_input_server.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if *fd < 0 || !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let target = unsafe { authorized_input.as_ref() }.map(|target| target.0.clone());
    let Ok(stream) = (unsafe { crate::ffi_acquisition::setup_server::take_stream(fd) }) else {
        return FT_STATUS_ERROR;
    };
    match bootstrap::accept(stream, target) {
        Ok(accepted) => {
            *out = accepted
                .input
                .map_or(ptr::null_mut(), |server| Box::into_raw(Box::new(FtInputServer { _server: server })));
            *fd = accepted.media.into_raw_fd();
            FT_STATUS_OK
        }
        Err(error) => status(error),
    }
}

/// `ft_source_bootstrap_accept` on a Local Endpoint connection the host
/// accepted: the verified peer receives any input channel by handle transfer.
/// # Safety
/// `connection` and output are valid/disjoint; `*connection` is a live,
/// exclusively owned handle; output starts null; target is null or live until
/// return. After basic checks the connection is nulled; on success the same
/// connection is returned in `*connection`, ready for CPU setup.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_source_bootstrap_accept_local(
    connection: *mut *mut FtLocalConnection,
    authorized_input: *mut FtInputTarget,
    out_input_server: *mut *mut FtInputServer,
) -> FtStatus {
    let Some(out) = (unsafe { out_input_server.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let target = unsafe { authorized_input.as_ref() }.map(|target| target.0.clone());
    let Some(taken) = (unsafe { take_connection(connection) }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let FtLocalConnection { stream, peer } = *taken;
    match bootstrap::accept(stream, target) {
        Ok(accepted) => {
            *out = accepted
                .input
                .map_or(ptr::null_mut(), |server| Box::into_raw(Box::new(FtInputServer { _server: server })));
            // SAFETY: take_connection nulled this valid slot above.
            unsafe {
                restore_connection(
                    connection,
                    FtLocalConnection {
                        stream: accepted.media,
                        peer,
                    },
                )
            };
            FT_STATUS_OK
        }
        Err(error) => status(error),
    }
}

fn parse_request(input_request: u32, input_mode: u32) -> Option<InputRequest> {
    Some(match (input_request, input_mode) {
        (FT_BOOTSTRAP_INPUT_NONE, 0) => InputRequest::None,
        (FT_BOOTSTRAP_INPUT_OPTIONAL, mode) => InputRequest::Optional(ffi_input::mode(mode)?),
        (FT_BOOTSTRAP_INPUT_REQUIRED, mode) => InputRequest::Required(ffi_input::mode(mode)?),
        _ => return None,
    })
}

fn connected_input(connection: &mut bootstrap::Connected) -> (FtStatus, *mut FtInputClient) {
    let status = connection.input_error.map_or(
        if connection.input.is_some() {
            FT_STATUS_OK
        } else {
            FT_STATUS_EMPTY
        },
        ffi_input::status,
    );
    let client = connection
        .input
        .take()
        .map_or(ptr::null_mut(), |client| Box::into_raw(Box::new(FtInputClient(client))));
    (status, client)
}

/// Request independent input through the same source endpoint as media.
/// # Safety
/// Arguments are valid/disjoint, fd solely owns the original connected Unix
/// stream, output handle starts null. Peer is the trusted selected source host.
/// No retained caller descriptor copies or concurrent stream use is allowed.
#[cfg(unix)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_source_bootstrap_connect(
    fd: *mut i32,
    input_request: u32,
    input_mode: u32,
    out_input: *mut *mut FtInputClient,
    out_input_status: *mut FtStatus,
) -> FtStatus {
    let (Some(fd), Some(out), Some(input_status)) = (unsafe { fd.as_mut() }, unsafe { out_input.as_mut() }, unsafe {
        out_input_status.as_mut()
    }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if *fd < 0 || !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let Some(request) = parse_request(input_request, input_mode) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    *input_status = FT_STATUS_EMPTY;
    let Ok(stream) = (unsafe { crate::ffi_acquisition::setup_server::take_stream(fd) }) else {
        return FT_STATUS_ERROR;
    };
    match bootstrap::connect(stream, request) {
        Ok(mut connection) => {
            (*input_status, *out) = connected_input(&mut connection);
            *fd = connection.media.into_raw_fd();
            FT_STATUS_OK
        }
        Err(error) => status(error),
    }
}

/// `ft_source_bootstrap_connect` on a connection from `ft_local_connect`, whose
/// server was verified. An input channel arrives by handle transfer.
/// # Safety
/// Arguments are valid/disjoint; `*connection` is a live, exclusively owned
/// handle; output handle starts null. After basic checks the connection is
/// nulled; on success the same connection is returned in `*connection`, ready
/// for `ft_acquisition_cpu_connection_create_local`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_source_bootstrap_connect_local(
    connection: *mut *mut FtLocalConnection,
    input_request: u32,
    input_mode: u32,
    out_input: *mut *mut FtInputClient,
    out_input_status: *mut FtStatus,
) -> FtStatus {
    let (Some(out), Some(input_status)) = (unsafe { out_input.as_mut() }, unsafe { out_input_status.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let Some(request) = parse_request(input_request, input_mode) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // Validate before taking ownership, as for the FD form.
    if unsafe { connection.as_ref() }.is_none_or(|slot| slot.is_null()) {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    *input_status = FT_STATUS_EMPTY;
    let Some(taken) = (unsafe { take_connection(connection) }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let FtLocalConnection { stream, peer } = *taken;
    match bootstrap::connect(stream, request) {
        Ok(mut connected) => {
            (*input_status, *out) = connected_input(&mut connected);
            // SAFETY: take_connection nulled this valid slot above.
            unsafe {
                restore_connection(
                    connection,
                    FtLocalConnection {
                        stream: connected.media,
                        peer,
                    },
                )
            };
            FT_STATUS_OK
        }
        Err(error) => status(error),
    }
}
