pub mod control_page;
#[cfg(unix)]
pub mod daemon;
#[cfg(windows)]
pub mod daemon {
    use crate::{
        error::{CaptureTransferError, Result},
        model::PixelFormat,
        video::VideoFrameDesc,
    };

    #[derive(Debug, Clone)]
    pub struct SyntheticSession {
        pub session_id: String,
        pub source_id: u64,
        pub track_id: u64,
        pub fd_socket_path: String,
    }

    #[derive(Clone)]
    pub struct SessionInfo {
        pub session_id: String,
        pub source_id: u64,
        pub track_id: u64,
        pub width: u32,
        pub height: u32,
        pub stride: u32,
        pub pixel_format: PixelFormat,
        pub fd_socket_path: String,
        pub bearer_token: Option<String>,
    }

    impl std::fmt::Debug for SessionInfo {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("SessionInfo")
                .field("session_id", &self.session_id)
                .field("source_id", &self.source_id)
                .field("track_id", &self.track_id)
                .field("width", &self.width)
                .field("height", &self.height)
                .field("stride", &self.stride)
                .field("pixel_format", &self.pixel_format)
                .field("fd_socket_path", &self.fd_socket_path)
                .field("bearer_token", &self.bearer_token.as_deref().map(|_| "<redacted>"))
                .finish()
        }
    }

    #[derive(Debug)]
    pub struct DaemonFrame {
        pub desc: VideoFrameDesc,
        pub producer_cursor: u64,
        pub len: usize,
    }

    impl DaemonFrame {
        #[must_use]
        pub fn bytes(&self) -> &[u8] {
            &[]
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct DaemonFrameUnavailable {
        pub track_id: u64,
        pub after_producer_cursor: u64,
        pub oldest_available_cursor: u64,
        pub latest_available_cursor: u64,
        pub skipped_count: u64,
        pub reason: String,
    }

    #[derive(Debug)]
    pub enum DaemonFrameAcquire {
        Frame(DaemonFrame),
        Unavailable(DaemonFrameUnavailable),
    }

    #[derive(Debug)]
    pub struct DaemonConsumer {
        _info: SessionInfo,
    }

    impl DaemonConsumer {
        pub fn connect(info: SessionInfo) -> Result<Self> {
            let _ = info;
            Err(unsupported())
        }

        pub fn latest_frame(&mut self, _track_id: u64) -> Result<DaemonFrame> {
            Err(unsupported())
        }

        pub fn next_frame_after(&mut self, _track_id: u64, _after_producer_cursor: u64) -> Result<DaemonFrameAcquire> {
            Err(unsupported())
        }

        pub fn release_frame(&mut self, _frame: DaemonFrame) -> Result<()> {
            Err(unsupported())
        }
    }

    pub fn create_synthetic_session(_control_socket_path: &str) -> Result<SyntheticSession> {
        Err(unsupported())
    }

    pub fn get_session(_control_socket_path: &str, _session_id: &str) -> Result<SessionInfo> {
        Err(unsupported())
    }

    /// Copy a Rust string into a caller-provided C buffer with a NUL terminator.
    ///
    /// # Safety
    ///
    /// `out` must point to a writable buffer of at least `len` bytes and must
    /// remain valid for the duration of the call.
    pub unsafe fn copy_string_to_c_buffer(value: &str, out: *mut libc::c_char, len: usize) -> bool {
        if out.is_null() || len == 0 || value.len() + 1 > len {
            return false;
        }
        // SAFETY: caller provided a valid writable buffer of len bytes; the
        // bounds check above guarantees room for the bytes and NUL terminator.
        unsafe {
            std::ptr::copy_nonoverlapping(value.as_ptr(), out.cast::<u8>(), value.len());
            *out.add(value.len()) = 0;
        }
        true
    }

    fn unsupported() -> CaptureTransferError {
        CaptureTransferError::DaemonTransport {
            operation: "windows-capture",
            message: "capture not supported on Windows yet".to_string(),
        }
    }
}
pub mod error;
#[cfg(unix)]
pub mod fdpass;
pub mod ffi;
#[cfg(any(
    all(target_os = "macos", feature = "backend-macos"),
    all(target_os = "linux", feature = "backend-linux")
))]
pub mod ffi_native;
pub mod model;
pub mod native;
pub mod shm;
pub mod state;
pub mod transfer_channel;
pub mod video;

pub use error::{CaptureTransferError, Result};
