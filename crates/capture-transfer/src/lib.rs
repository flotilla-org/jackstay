pub mod control_page;
pub mod daemon;
pub mod error;
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
