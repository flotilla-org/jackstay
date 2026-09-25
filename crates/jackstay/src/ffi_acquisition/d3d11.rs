//! D3D11 setup and borrowed resource access for the common C acquisition API
//! (ABI 0.10), the Windows counterpart of [`super::macos`].
//!
//! A consumer connects to a Local Endpoint, hands the connection to
//! [`ft_acquisition_d3d11_connection_create_local`], asks the producer for its
//! adapter, creates its own device on that adapter and attaches with it. Frames
//! then use the common acquire/describe/release calls; this module adds only
//! the borrowed NT handles of a frame's texture and readiness fence, the
//! consumer's release fence registration, configuration replacement, and the
//! abandoned-fence rule. Importing, caching and GPU waits stay with the caller's
//! renderer.

use std::{
    ffi::{c_char, c_void},
    os::windows::io::{AsHandle, AsRawHandle},
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use ::windows::{
    Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Fence},
    core::Interface,
};

use super::{FtAcquiredFrame, FtAcquisitionConsumer, FtAcquisitionReleaseTimeline, status};
use crate::{
    acquisition::{arena::ConfigurationInstall, socket::SocketError},
    ffi::*,
    ffi_local::{FtLocalConnection, take_connection},
    native::windows::{
        ABANDONED_FENCE_VALUE, AdapterInfo, D3d11Device, D3d11Fence, SharedFenceHandle, SharedTextureHandle,
        setup::{D3d11SetupClient, SetupError},
    },
};

/// Bytes of `FtD3d11Adapter::description`, including the terminating NUL.
pub const FT_D3D11_ADAPTER_DESCRIPTION_LEN: usize = 128;

/// A producer's adapter, as `ft_acquisition_d3d11_describe` reports it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FtD3d11Adapter {
    /// `(HighPart << 32) | LowPart` of the DXGI adapter LUID.
    pub luid: u64,
    pub vendor_id: u32,
    pub device_id: u32,
    /// Nonzero for WARP / the Basic Render Driver.
    pub software: u32,
    pub reserved: u32,
    /// UTF-8, NUL-terminated, truncated at a character boundary.
    pub description: [c_char; FT_D3D11_ADAPTER_DESCRIPTION_LEN],
}

impl Default for FtD3d11Adapter {
    fn default() -> Self {
        Self {
            luid: 0,
            vendor_id: 0,
            device_id: 0,
            software: 0,
            reserved: 0,
            description: [0; FT_D3D11_ADAPTER_DESCRIPTION_LEN],
        }
    }
}

impl From<&AdapterInfo> for FtD3d11Adapter {
    fn from(info: &AdapterInfo) -> Self {
        let mut adapter = Self {
            luid: info.luid.0,
            vendor_id: info.vendor_id,
            device_id: info.device_id,
            software: u32::from(info.software),
            ..Self::default()
        };
        let mut end = info.description.len().min(FT_D3D11_ADAPTER_DESCRIPTION_LEN - 1);
        while !info.description.is_char_boundary(end) {
            end -= 1;
        }
        for (slot, byte) in adapter.description.iter_mut().zip(&info.description.as_bytes()[..end]) {
            *slot = *byte as c_char;
        }
        adapter
    }
}

pub struct FtD3d11AcquisitionConnection {
    client: Mutex<D3d11SetupClient>,
    shutdown: crate::local::ShutdownHandle,
    cancelled: AtomicBool,
}

fn setup_status(error: SetupError) -> FtStatus {
    match error {
        SetupError::Refused(_) => FT_STATUS_ADAPTER_MISMATCH,
        SetupError::Socket(SocketError::Arena(error)) => status(error),
        SetupError::Socket(_) => FT_STATUS_ERROR,
    }
}

impl FtD3d11AcquisitionConnection {
    // As the CPU connection: a completed operation keeps its result even if
    // cancellation arrives before it returns; cancellation interrupts failures
    // and later calls.
    fn with_client<T>(&self, operation: impl FnOnce(&mut D3d11SetupClient) -> Result<T, SetupError>) -> Result<T, FtStatus> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(FT_STATUS_CANCELLED);
        }
        let mut client = self.client.lock().map_err(|_| FT_STATUS_ERROR)?;
        operation(&mut client).map_err(|error| {
            if self.cancelled.load(Ordering::Acquire) {
                FT_STATUS_CANCELLED
            } else {
                setup_status(error)
            }
        })
    }
}

/// Own a connection from `ft_local_connect` (optionally after source bootstrap)
/// for D3D11 setup. Its server was verified when connecting. No setup I/O
/// happens until describe or attach, so cancellation can be installed first.
///
/// # Safety
/// `connection` and `out` are writable/disjoint; `*connection` is a live,
/// exclusively owned handle; `*out` is null. The server is the conforming sole
/// producer; this process is the sole recipient of its grants and handles (no
/// forwarding or replay). After basic checks the connection is consumed and
/// nulled on all outcomes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_d3d11_connection_create_local(
    connection: *mut *mut FtLocalConnection,
    out: *mut *mut FtD3d11AcquisitionConnection,
) -> FtStatus {
    // SAFETY: caller supplies valid disjoint writable storage.
    let Some(out) = (unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: caller transfers the exclusively owned connection handle.
    let Some(connection) = (unsafe { take_connection(connection) }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: local::connect verified the server; the caller guarantees the
    // trusted producer and sole-recipient contract.
    let client = unsafe { D3d11SetupClient::from_stream(connection.stream) };
    let Ok(shutdown) = client.shutdown_handle() else {
        return FT_STATUS_ERROR;
    };
    *out = Box::into_raw(Box::new(FtD3d11AcquisitionConnection {
        client: Mutex::new(client),
        shutdown,
        cancelled: AtomicBool::new(false),
    }));
    FT_STATUS_OK
}

/// Liveness of the producer's end of setup, never consuming setup bytes: OK
/// while it holds the connection (and while another setup call owns it),
/// CLOSED once it closed, exited or setup failed, CANCELLED after cancellation.
///
/// # Safety
/// Connection is live until this call returns. May run concurrently with other
/// calls on the connection except destruction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_d3d11_connection_alive(connection: *const FtD3d11AcquisitionConnection) -> FtStatus {
    // SAFETY: caller keeps the handle alive for this call.
    let Some(connection) = (unsafe { connection.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if connection.cancelled.load(Ordering::Acquire) {
        return FT_STATUS_CANCELLED;
    }
    match connection.client.try_lock() {
        Ok(client) if client.is_alive() => FT_STATUS_OK,
        Ok(_) => FT_STATUS_CLOSED,
        Err(std::sync::TryLockError::WouldBlock) => FT_STATUS_OK,
        Err(std::sync::TryLockError::Poisoned(_)) => FT_STATUS_ERROR,
    }
}

/// Permanently interrupt this connection's setup I/O. Frames remain owned.
///
/// # Safety
/// Connection is null or live. May run concurrently with other setup calls,
/// but all calls must return before destroying the connection handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_d3d11_connection_cancel(connection: *const FtD3d11AcquisitionConnection) {
    // SAFETY: caller keeps the handle alive until this operation returns.
    if let Some(connection) = unsafe { connection.as_ref() } {
        connection.cancelled.store(true, Ordering::Release);
        connection.shutdown.shutdown();
    }
}

/// Ask the producer for its adapter, so the consumer can create its device on
/// that LUID before attaching.
///
/// # Safety
/// Connection is live; `out` is writable and does not alias it. Serialize with
/// the connection's other setup calls; cancel may run concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_d3d11_describe(
    connection: *const FtD3d11AcquisitionConnection,
    out: *mut FtD3d11Adapter,
) -> FtStatus {
    // SAFETY: caller supplies a live connection and a writable output.
    let (Some(connection), Some(out)) = (unsafe { connection.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    *out = FtD3d11Adapter::default();
    match connection.with_client(D3d11SetupClient::describe) {
        Ok(description) => {
            *out = FtD3d11Adapter::from(&description.adapter);
            FT_STATUS_OK
        }
        Err(status) => status,
    }
}

/// Admit a process-bound consumer that imports on `device`'s adapter. A
/// producer on another adapter refuses with ADAPTER_MISMATCH before admitting
/// anything, and the connection stays usable. The device is borrowed for this
/// call only.
///
/// # Safety
/// Connection is live; `device` is a live `ID3D11Device*` (the runtime must
/// provide `ID3D11Device5`); `out` is writable, disjoint and null. Serialize
/// setup calls; cancel may run concurrently. Never fork, forward or replay the
/// imported mappings or handles.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_d3d11_attach(
    connection: *const FtD3d11AcquisitionConnection,
    device: *mut c_void,
    holding: u32,
    out: *mut *mut FtAcquisitionConsumer,
) -> FtStatus {
    // SAFETY: caller supplies a live connection and disjoint writable output.
    let (Some(connection), Some(out)) = (unsafe { connection.as_ref() }, unsafe { out.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if holding == 0 || !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: the caller lends a live ID3D11Device for this call; cloning
    // takes this function's own reference.
    let Some(device) = (unsafe { ID3D11Device::from_raw_borrowed(&device) }).cloned() else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // Only the adapter identity is used; the caller's immediate context is not.
    let Ok(device) = D3d11Device::from_device(device) else {
        return FT_STATUS_UNSUPPORTED;
    };
    match connection.with_client(|client| client.attach(holding, &device)) {
        Ok(consumer) => {
            *out = FtAcquisitionConsumer::into_raw(consumer);
            FT_STATUS_OK
        }
        Err(status) => status,
    }
}

/// Ask the producer for a replacement pool after RECONFIGURATION and install
/// it. OK installed, EMPTY no offer, STALE disposed a valid superseded offer.
/// Held frames keep their own pool's handles.
///
/// # Safety
/// Both handles are live, non-aliasing and exclusively accessed, and belong to
/// the same admitted connection; foreign consumers are rejected.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_d3d11_install_configuration(
    connection: *const FtD3d11AcquisitionConnection,
    consumer: *mut FtAcquisitionConsumer,
) -> FtStatus {
    // SAFETY: caller supplies live exclusive handles.
    let (Some(connection), Some(consumer)) = (unsafe { connection.as_ref() }, unsafe { consumer.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    match connection.with_client(|client| client.install_configuration(&mut consumer.0)) {
        Ok(Some(ConfigurationInstall::Installed)) => FT_STATUS_OK,
        Ok(Some(ConfigurationInstall::Stale)) => FT_STATUS_STALE,
        Ok(None) => FT_STATUS_EMPTY,
        Err(status) => status,
    }
}

/// Register the consumer's own shared release fence once. The producer opens
/// its own reference; the returned binding feeds common deferred release. The
/// library keeps a reference to `fence`; the caller keeps its own.
///
/// # Safety
/// Handles are live and non-aliasing with serialized setup calls. `fence` is a
/// live `ID3D11Fence*` created with `D3D11_FENCE_FLAG_SHARED` on the attached
/// adapter. `out` is writable and null. Later release values must cover all
/// submitted use of the released frames: signal the fence from the same
/// context after that work, not merely after submitting it elsewhere.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_d3d11_register_release(
    connection: *const FtD3d11AcquisitionConnection,
    consumer: *const FtAcquisitionConsumer,
    fence: *mut c_void,
    out: *mut *mut FtAcquisitionReleaseTimeline,
) -> FtStatus {
    // SAFETY: caller supplies valid, non-aliasing handles and a writable output.
    let (Some(connection), Some(consumer), Some(out)) = (unsafe { connection.as_ref() }, unsafe { consumer.as_ref() }, unsafe {
        out.as_mut()
    }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    if !out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: the caller lends a live ID3D11Fence; cloning takes our reference.
    let Some(fence) = (unsafe { ID3D11Fence::from_raw_borrowed(&fence) }).cloned() else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let fence = Arc::new(D3d11Fence::from_raw(fence));
    match connection.with_client(|client| client.register_release_timeline(&consumer.0, fence)) {
        Ok(binding) => {
            *out = FtAcquisitionReleaseTimeline::into_raw(binding);
            FT_STATUS_OK
        }
        Err(status) => status,
    }
}

/// Close setup without declaring outstanding work complete; clears the handle.
/// Consumer and frame handles have their own lifetimes. NULL is harmless.
///
/// # Safety
/// Pointer is writable and exclusively owns the connection. No concurrent
/// setup call may still be using it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquisition_d3d11_connection_destroy(connection: *mut *mut FtD3d11AcquisitionConnection) {
    // SAFETY: exclusive ownership and pointer validity are caller obligations.
    if let Some(connection) = unsafe { connection.as_mut() }
        && !connection.is_null()
    {
        // SAFETY: this box was returned by create_local and is consumed once.
        drop(unsafe { Box::from_raw(std::mem::replace(connection, ptr::null_mut())) });
    }
}

/// Borrow the NT handles of this frame's pool texture and its producer's
/// readiness fence. No import, cache or extra ownership is created here.
///
/// # Safety
/// `frame` must be live and not concurrently released. Outputs must be
/// writable, non-aliasing pointers. The handles are valid only while the frame
/// is held; import them before releasing it. GPU-wait the descriptor's fence
/// value before sampling.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_acquired_frame_d3d11_resources(
    frame: *const FtAcquiredFrame,
    out_texture: *mut *mut c_void,
    out_readiness: *mut *mut c_void,
) -> FtStatus {
    // SAFETY: caller guarantees writable, non-aliasing outputs and a live frame.
    let (Some(out_texture), Some(out_readiness)) = (unsafe { out_texture.as_mut() }, unsafe { out_readiness.as_mut() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    *out_texture = ptr::null_mut();
    *out_readiness = ptr::null_mut();
    // SAFETY: the caller retains this live frame throughout the borrow.
    let Some(frame) = (unsafe { frame.as_ref() }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    match frame
        .0
        .as_ref()
        .expect("live C frame")
        .native_resources::<SharedTextureHandle, SharedFenceHandle>()
    {
        Ok(resources) => {
            *out_texture = resources.surface.as_handle().as_raw_handle();
            *out_readiness = resources.sync_handle.as_handle().as_raw_handle();
            FT_STATUS_OK
        }
        Err(_) => FT_STATUS_UNSUPPORTED,
    }
}

/// Whether a readiness fence's producer device still exists: OK while it does,
/// CLOSED once it is abandoned (removed, or its process exited). An abandoned
/// fence reads UINT64_MAX and satisfies every wait although the covered copy
/// never ran, so discard frames whose readiness fence is abandoned and treat
/// the producer as lost.
///
/// # Safety
/// `fence` is a live `ID3D11Fence*`, borrowed for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_d3d11_fence_alive(fence: *mut c_void) -> FtStatus {
    // SAFETY: the caller lends a live ID3D11Fence for this call.
    let Some(fence) = (unsafe { ID3D11Fence::from_raw_borrowed(&fence) }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    // SAFETY: plain query on a live fence.
    if unsafe { fence.GetCompletedValue() } == ABANDONED_FENCE_VALUE {
        FT_STATUS_CLOSED
    } else {
        FT_STATUS_OK
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::windows::AdapterLuid;

    /// Mirrors the header's `_Static_assert`s on `ft_d3d11_adapter`.
    #[test]
    fn adapter_layout_matches_the_header() {
        assert_eq!(std::mem::size_of::<FtD3d11Adapter>(), 152);
        assert_eq!(std::mem::offset_of!(FtD3d11Adapter, description), 24);
    }

    #[test]
    fn adapter_description_truncates_at_a_character_boundary() {
        let info = AdapterInfo {
            luid: AdapterLuid(0x1_0000_0002),
            description: "é".repeat(100),
            vendor_id: 0x1002,
            device_id: 0x1681,
            software: true,
        };
        let adapter = FtD3d11Adapter::from(&info);
        assert_eq!(adapter.luid, 0x1_0000_0002);
        assert_eq!(adapter.software, 1);
        let bytes: Vec<u8> = adapter.description.iter().map(|byte| *byte as u8).collect();
        let end = bytes.iter().position(|byte| *byte == 0).unwrap();
        assert_eq!(end, 126, "63 two-byte characters fit before the NUL");
        assert!(std::str::from_utf8(&bytes[..end]).is_ok());
    }

    #[test]
    fn null_arguments_are_rejected_without_effect() {
        // SAFETY: every call below is given null or local storage.
        unsafe {
            let mut out = ptr::null_mut();
            assert_eq!(
                ft_acquisition_d3d11_connection_create_local(ptr::null_mut(), &mut out),
                FT_STATUS_INVALID_ARGUMENT
            );
            assert!(out.is_null());
            assert_eq!(ft_acquisition_d3d11_connection_alive(ptr::null()), FT_STATUS_INVALID_ARGUMENT);
            ft_acquisition_d3d11_connection_cancel(ptr::null());
            let mut adapter = FtD3d11Adapter::default();
            assert_eq!(ft_acquisition_d3d11_describe(ptr::null(), &mut adapter), FT_STATUS_INVALID_ARGUMENT);
            let mut texture = ptr::dangling_mut::<c_void>();
            let mut readiness = ptr::dangling_mut::<c_void>();
            assert_eq!(
                ft_acquired_frame_d3d11_resources(ptr::null(), &mut texture, &mut readiness),
                FT_STATUS_INVALID_ARGUMENT
            );
            assert!(texture.is_null() && readiness.is_null());
            assert_eq!(ft_d3d11_fence_alive(ptr::null_mut()), FT_STATUS_INVALID_ARGUMENT);
            ft_acquisition_d3d11_connection_destroy(ptr::null_mut());
            let mut none: *mut FtD3d11AcquisitionConnection = ptr::null_mut();
            ft_acquisition_d3d11_connection_destroy(&mut none);
        }
    }
}
