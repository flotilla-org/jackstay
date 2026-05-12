use thiserror::Error;

pub type Result<T> = std::result::Result<T, CaptureTransferError>;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CaptureTransferError {}
