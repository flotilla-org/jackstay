use thiserror::Error;

use crate::model::{SourceId, TrackId};

pub type Result<T> = std::result::Result<T, CaptureTransferError>;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CaptureTransferError {
    #[error("unknown source id {source_id:?}")]
    UnknownSource { source_id: SourceId },

    #[error("unknown track id {track_id:?}")]
    UnknownTrack { track_id: TrackId },
}
