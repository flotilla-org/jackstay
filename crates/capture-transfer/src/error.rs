use thiserror::Error;

use crate::model::{SourceId, TrackId};

pub type Result<T> = std::result::Result<T, CaptureTransferError>;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CaptureTransferError {
    #[error("fd passing {operation} failed: {message}")]
    FdPassing { operation: &'static str, message: String },

    #[error("fd passing response did not include a file descriptor")]
    MissingPassedFd,

    #[error("daemon transport {operation} failed: {message}")]
    DaemonTransport { operation: &'static str, message: String },

    #[error("shared memory segment length must be greater than zero")]
    InvalidSharedMemoryLength,

    #[error("shared memory {operation} failed: {message}")]
    SharedMemory { operation: &'static str, message: String },

    #[error("unknown source id {source_id:?}")]
    UnknownSource { source_id: SourceId },

    #[error("unknown track id {track_id:?}")]
    UnknownTrack { track_id: TrackId },
}
