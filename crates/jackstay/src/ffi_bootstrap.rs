//! C bootstrap ownership; the same negotiation is available to Rust hosts.
use std::{os::fd::IntoRawFd, ptr};

use crate::{
    bootstrap::{self, InputRequest},
    ffi::*,
    ffi_input::{self, FtInputClient, FtInputServer, FtInputTarget},
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

/// Request independent input through the same source endpoint as media.
/// # Safety
/// Arguments are valid/disjoint, fd solely owns the original connected Unix
/// stream, output handle starts null. Peer is the trusted selected source host.
/// No retained caller descriptor copies or concurrent stream use is allowed.
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
    let request = match (input_request, input_mode) {
        (FT_BOOTSTRAP_INPUT_NONE, 0) => InputRequest::None,
        (FT_BOOTSTRAP_INPUT_OPTIONAL, mode) => match ffi_input::mode(mode) {
            Some(mode) => InputRequest::Optional(mode),
            None => return FT_STATUS_INVALID_ARGUMENT,
        },
        (FT_BOOTSTRAP_INPUT_REQUIRED, mode) => match ffi_input::mode(mode) {
            Some(mode) => InputRequest::Required(mode),
            None => return FT_STATUS_INVALID_ARGUMENT,
        },
        _ => return FT_STATUS_INVALID_ARGUMENT,
    };
    *input_status = FT_STATUS_EMPTY;
    let Ok(stream) = (unsafe { crate::ffi_acquisition::setup_server::take_stream(fd) }) else {
        return FT_STATUS_ERROR;
    };
    match bootstrap::connect(stream, request) {
        Ok(connection) => {
            *input_status = connection.input_error.map_or(
                if connection.input.is_some() {
                    FT_STATUS_OK
                } else {
                    FT_STATUS_EMPTY
                },
                ffi_input::status,
            );
            *out = connection
                .input
                .map_or(ptr::null_mut(), |client| Box::into_raw(Box::new(FtInputClient(client))));
            *fd = connection.media.into_raw_fd();
            FT_STATUS_OK
        }
        Err(error) => status(error),
    }
}
