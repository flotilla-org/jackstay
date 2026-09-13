use std::ffi::{CStr, c_char};

use crate::daemon;

pub type FtStatus = i32;

/// ABI version of this library, as `(major << 16) | minor`. Mirror of
/// `FT_ABI_VERSION` in `capture_transfer.h`; consumers compare their header's
/// constant against [`ft_abi_version`] at startup. Additions land as new
/// functions and new attach-stream operations under a minor bump; existing
/// struct layouts never change without a major bump. Major 0 means
/// pre-stabilization: layouts may still change freely, with a minor bump as
/// the only signal; 1.0 waits until an external consumer needs the promise.
pub const FT_ABI_VERSION_MAJOR: u32 = 0;
pub const FT_ABI_VERSION_MINOR: u32 = 5;
pub const FT_ABI_VERSION: u32 = (FT_ABI_VERSION_MAJOR << 16) | FT_ABI_VERSION_MINOR;

/// Report the linked library's ABI version.
#[unsafe(no_mangle)]
pub extern "C" fn ft_abi_version() -> u32 {
    FT_ABI_VERSION
}

pub const FT_STATUS_OK: FtStatus = 0;
pub const FT_STATUS_EMPTY: FtStatus = 1;
pub const FT_STATUS_INVALID_ARGUMENT: FtStatus = 2;
pub const FT_STATUS_ERROR: FtStatus = 3;
pub const FT_STATUS_TIMEOUT: FtStatus = 4;
pub const FT_STATUS_CLOSED: FtStatus = 5;
pub const FT_STATUS_UNSUPPORTED: FtStatus = 6;
pub const FT_STATUS_INVALID_STATE: FtStatus = 7;
pub const FT_STATUS_HOLDING_LIMIT: FtStatus = 8;
pub const FT_STATUS_RECONFIGURATION: FtStatus = 9;
pub const FT_STATUS_MISS: FtStatus = 10;
pub const FT_STATUS_GAP: FtStatus = 11;
pub const FT_STATUS_CANCELLED: FtStatus = 12;
pub const FT_STATUS_STALE: FtStatus = 13;
pub const FT_STATUS_DRAINING: FtStatus = 14;
pub const FT_STATUS_DROPPED: FtStatus = 15;
pub const FT_STATUS_PAUSED_CAPACITY: FtStatus = 16;
pub const FT_STATUS_CAPACITY: FtStatus = 17;
pub const FT_STATUS_RECOVERY_REQUIRED: FtStatus = 18;

pub const FT_SOURCE_KIND_WINDOW: u32 = 1;
pub const FT_SOURCE_KIND_DISPLAY: u32 = 2;
pub const FT_SOURCE_KIND_SURFACE: u32 = 3;

pub const FT_TRACK_TYPE_VIDEO: u32 = 1;

pub const FT_PIXEL_FORMAT_UNKNOWN: u32 = 0;
pub const FT_PIXEL_FORMAT_BGRA8_UNORM: u32 = 1;
pub const FT_PIXEL_FORMAT_RGBA8_UNORM: u32 = 2;

pub const FT_CLOCK_DOMAIN_UNKNOWN: u32 = 0;
pub const FT_CLOCK_DOMAIN_UNIX_TIME: u32 = 1;
pub const FT_CLOCK_DOMAIN_MEDIA_TIME: u32 = 2;
pub const FT_CLOCK_DOMAIN_HOST_TIME: u32 = 3;

pub const FT_COLOR_SPACE_UNKNOWN: u32 = 0;
pub const FT_COLOR_SPACE_SRGB: u32 = 1;

pub const FT_FRAME_SYNC_UNKNOWN: u32 = 0;
pub const FT_FRAME_SYNC_CPU_COPY_COMPLETE: u32 = 1;
pub const FT_FRAME_SYNC_SCK_SAMPLE_READY: u32 = 2;
pub const FT_FRAME_SYNC_NATIVE_TIMELINE: u32 = 3;

pub const FT_DAMAGE_UNKNOWN: u32 = 0;
pub const FT_DAMAGE_FULL_FRAME: u32 = 1;
pub const FT_DAMAGE_NONE: u32 = 2;
pub const FT_DAMAGE_INLINE_RECTS: u32 = 3;
pub const FT_DAMAGE_SIDECAR_RECTS: u32 = 4;

pub const FT_EVENT_PRODUCER_STARTED: u32 = 1;
pub const FT_EVENT_SOURCE_REGISTERED: u32 = 2;
pub const FT_EVENT_SOURCE_UPDATED: u32 = 3;
pub const FT_EVENT_TRACK_REGISTERED: u32 = 4;
pub const FT_EVENT_TRACK_UPDATED: u32 = 5;
pub const FT_EVENT_SOURCE_UNREGISTERED: u32 = 6;
pub const FT_EVENT_PRODUCER_STOPPED: u32 = 7;

#[repr(C)]
#[derive(Debug)]
pub struct FtSyntheticSession {
    pub session_id: [c_char; 64],
    pub source_id: u64,
    pub track_id: u64,
    pub fd_socket_path: [c_char; 4096],
}

/// # Safety
///
/// `control_socket_path` must point to a NUL-terminated string. `out` must
/// point to writable storage. String buffers in `out` are filled on success.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ft_create_synthetic_session(control_socket_path: *const c_char, out: *mut FtSyntheticSession) -> FtStatus {
    if control_socket_path.is_null() || out.is_null() {
        return FT_STATUS_INVALID_ARGUMENT;
    }
    // SAFETY: control_socket_path was checked for null and must be NUL-terminated by caller.
    let Some(control_socket_path) = (unsafe { c_string_to_string(control_socket_path) }) else {
        return FT_STATUS_INVALID_ARGUMENT;
    };
    let Ok(session) = daemon::create_synthetic_session(&control_socket_path) else {
        return FT_STATUS_ERROR;
    };
    let mut ffi = FtSyntheticSession {
        session_id: [0; 64],
        source_id: session.source_id,
        track_id: session.track_id,
        fd_socket_path: [0; 4096],
    };
    // SAFETY: ffi owns both fixed-size destination buffers.
    if !(unsafe { daemon::copy_string_to_c_buffer(&session.session_id, ffi.session_id.as_mut_ptr(), ffi.session_id.len()) })
        || !(unsafe { daemon::copy_string_to_c_buffer(&session.fd_socket_path, ffi.fd_socket_path.as_mut_ptr(), ffi.fd_socket_path.len()) })
    {
        return FT_STATUS_ERROR;
    }
    // SAFETY: out was checked for null and points to caller-owned storage.
    unsafe {
        *out = ffi;
    }
    FT_STATUS_OK
}

unsafe fn c_string_to_string(value: *const c_char) -> Option<String> {
    if value.is_null() {
        return None;
    }
    // SAFETY: caller guarantees value points to a NUL-terminated C string.
    unsafe { CStr::from_ptr(value) }.to_str().ok().map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    /// The header and this crate each state the ABI version; nothing else
    /// ties them together, so a bump that touches one side and not the other
    /// would silently break the startup check `ft_abi_version()` exists for.
    /// Parse the header's defines and compare.
    #[test]
    fn header_and_library_agree_on_the_abi_version() {
        let header = include_str!("../include/capture_transfer.h");
        let define = |name: &str| -> u32 {
            let marker = format!("#define {name} ");
            header
                .lines()
                .find_map(|line| line.strip_prefix(&marker))
                .unwrap_or_else(|| panic!("{name} not defined in capture_transfer.h"))
                .trim()
                .parse()
                .unwrap_or_else(|error| panic!("{name} is not a bare integer: {error}"))
        };
        assert_eq!(define("FT_ABI_VERSION_MAJOR"), super::FT_ABI_VERSION_MAJOR);
        assert_eq!(define("FT_ABI_VERSION_MINOR"), super::FT_ABI_VERSION_MINOR);
        assert_eq!(super::ft_abi_version(), super::FT_ABI_VERSION);
        assert_eq!(super::FT_ABI_VERSION, 0x0000_0005);
    }
}
