//! Native setup and borrowed resource access for the common C acquisition API.

use std::{
    ffi::{CStr, c_char, c_void},
    ptr::{self, NonNull},
    sync::Arc,
};

use super::{FtAcquiredFrame, FtAcquisitionConsumer, FtAcquisitionReleaseTimeline, status};
use crate::{
    acquisition::arena::{ArenaError, ConfigurationInstall},
    ffi::*,
    native::macos::{ConsumerFence, IoSurface, MetalContext, SharedEventHandle, xpc::arena::XpcArenaClient},
};

pub struct FtMacosAcquisitionConnection(XpcArenaClient);

impl FtMacosAcquisitionConnection {
    /// Admit through an already connected/authorized client and transfer both
    /// handles to C. Neither handle owns or invalidates acquired frame handles.
    pub fn attach(mut client: XpcArenaClient, holding: u32) -> Result<(*mut Self, *mut FtAcquisitionConsumer), ArenaError> {
        let consumer = client.attach(holding)?;
        Ok((Box::into_raw(Box::new(Self(client))), FtAcquisitionConsumer::into_raw(consumer)))
    }
}

/// Connect to a trusted host's named acquisition service and request admission.
/// The token is optional and copied during authorization.
///
/// # Safety
/// The service must be a conforming sole producer obeying the shared acquisition
/// protocol; a client token does not authenticate the producer. `endpoint` and
/// non-null `token` must be live NUL-terminated UTF-8 strings. Both outputs must
/// point to null handles, be writable, and not alias other arguments. Do not fork,
/// forward or replay the resulting mappings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_macos_connect(
    endpoint: *const c_char,
    token: *const c_char,
    holding: u32,
    out_connection: *mut *mut FtMacosAcquisitionConnection,
    out_consumer: *mut *mut FtAcquisitionConsumer,
) -> FtStatus {
    if endpoint.is_null() || holding == 0 {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: callers supply valid, writable, non-aliasing outputs.
    let (Some(out_connection), Some(out_consumer)) = (unsafe { out_connection.as_mut() }, unsafe { out_consumer.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out_connection.is_null() || !out_consumer.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: caller guarantees a live NUL-terminated string.
    let Ok(endpoint) = unsafe { CStr::from_ptr(endpoint) }.to_str() else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if endpoint.is_empty() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let token = if token.is_null() {
        None
    } else {
        // SAFETY: the non-null token is a live NUL-terminated string.
        let Ok(token) = unsafe { CStr::from_ptr(token) }.to_str() else {
            return FT_STATUS_INVALID_ARGUMENT;
        };
        Some(token)
    };
    let result = (|| {
        // SAFETY: the caller selected a trusted conforming producer.
        let mut client = unsafe { XpcArenaClient::connect_named(endpoint) }?;
        if let Some(token) = token {
            client.authorize(token)?;
        }
        FtMacosAcquisitionConnection::attach(client, holding)
    })();
    match result {
        Ok((connection, consumer)) => {
            *out_connection = connection;
            *out_consumer = consumer;
            FT_STATUS_OK
        }
        Err(error) => status(error),
    }
}

/// Ask the host for a replacement and atomically install its map/native handles.
/// EMPTY means no offer; STALE means a valid superseded offer was disposed.
///
/// # Safety
/// Both handles must be live, non-aliasing and exclusively accessed. They must
/// belong to the same admitted connection; foreign incarnations are rejected.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_macos_install_configuration(
    connection: *mut FtMacosAcquisitionConnection,
    consumer: *mut FtAcquisitionConsumer,
) -> FtStatus {
    // SAFETY: caller guarantees exclusive access to valid, non-aliasing handles.
    let (Some(connection), Some(consumer)) = (unsafe { connection.as_mut() }, unsafe { consumer.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    match connection.0.install_configuration(&mut consumer.0) {
        Ok(Some(ConfigurationInstall::Installed)) => FT_STATUS_OK,
        Ok(Some(ConfigurationInstall::Stale)) => FT_STATUS_STALE,
        Ok(None) => FT_STATUS_EMPTY,
        Err(error) => status(error),
    }
}

/// Register the consumer's actual Metal completion event. The returned binding
/// feeds common deferred release; registration performs setup RPCs, release does
/// not. The library imports its own event; the input handle remains caller-owned.
///
/// # Safety
/// Handles must be live and non-aliasing with serialized connection/consumer
/// calls. `event_handle` must be a valid borrowed MTLSharedEventHandle for this
/// call. `out` must be writable and contain null. Later release values must cover
/// all submitted use on this event; command submission alone is not completion.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_macos_register_release(
    connection: *mut FtMacosAcquisitionConnection,
    consumer: *const FtAcquisitionConsumer,
    event_handle: *mut c_void,
    out: *mut *mut FtAcquisitionReleaseTimeline,
) -> FtStatus {
    // SAFETY: caller supplies valid, non-aliasing handles and writable output.
    let (Some(connection), Some(consumer), Some(event_handle), Some(out)) = (
        unsafe { connection.as_mut() },
        unsafe { consumer.as_ref() },
        NonNull::new(event_handle),
        unsafe { out.as_mut() },
    ) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    let result = (|| {
        let metal = MetalContext::new()?;
        // SAFETY: this call borrows the caller's valid MTLSharedEventHandle.
        let event = unsafe { ConsumerFence::from_borrowed_handle(&metal, event_handle) }?;
        connection.0.register_release_timeline(&consumer.0, Arc::new(event))
    })();
    match result {
        Ok(binding) => {
            *out = FtAcquisitionReleaseTimeline::into_raw(binding);
            FT_STATUS_OK
        }
        Err(error) => status(error),
    }
}

/// Borrow the acquired generation's IOSurfaceRef and MTLSharedEventHandle.
/// No resource cache or additional ownership is created at this boundary.
///
/// # Safety
/// `frame` must be live and not concurrently released. Outputs must be writable,
/// non-aliasing pointers. Borrowed resources may be used only within the lease,
/// including its declared deferred completion. Satisfy descriptor readiness
/// before sampling. Dispose any imported handle copies before releasing their
/// frame so mapping retirement remains proof of resource-handle retirement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquired_frame_macos_resources(
    frame: *const FtAcquiredFrame,
    out_surface: *mut *mut c_void,
    out_readiness: *mut *mut c_void,
) -> FtStatus {
    // SAFETY: caller guarantees writable, non-aliasing outputs and live frame.
    let (Some(out_surface), Some(out_readiness)) = (unsafe { out_surface.as_mut() }, unsafe { out_readiness.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    *out_surface = ptr::null_mut();
    *out_readiness = ptr::null_mut();
    // SAFETY: the caller retains this live frame throughout the borrow.
    let Some(frame) = (unsafe { frame.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    match frame
        .0
        .as_ref()
        .expect("live C frame")
        .native_resources::<IoSurface, SharedEventHandle>()
    {
        Ok(resources) => {
            *out_surface = resources.surface.as_raw();
            *out_readiness = resources.sync_handle.as_raw();
            FT_STATUS_OK
        }
        Err(_) => FT_STATUS_UNSUPPORTED,
    }
}

/// Close the setup connection without declaring pending CPU/GPU use complete.
/// Null input/handle is harmless; a live handle is consumed and nulled.
///
/// # Safety
/// Pointer must be writable and exclusively own the connection. No concurrent
/// setup call may still be using it. Frame and consumer handles are separate.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_macos_connection_destroy(connection: *mut *mut FtMacosAcquisitionConnection) {
    // SAFETY: exclusive ownership and pointer validity are caller obligations.
    if let Some(connection) = unsafe { connection.as_mut() } {
        if !connection.is_null() {
            // SAFETY: this box was returned by connect/attach and is consumed once.
            drop(unsafe { Box::from_raw(std::mem::replace(connection, ptr::null_mut())) });
        }
    }
}
