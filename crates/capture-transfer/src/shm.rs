use std::{
    fs::{self, File, OpenOptions},
    os::fd::AsRawFd,
    path::PathBuf,
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    error::{CaptureTransferError, Result},
    model::PayloadKind,
};

static NEXT_SEGMENT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct SharedMemorySegment {
    ptr: NonNull<u8>,
    len: usize,
    path: PathBuf,
    _file: File,
}

impl SharedMemorySegment {
    pub fn new(len: usize) -> Result<Self> {
        if len == 0 {
            return Err(CaptureTransferError::InvalidSharedMemoryLength);
        }

        let path = unique_path();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| CaptureTransferError::SharedMemory {
                operation: "create",
                message: error.to_string(),
            })?;
        file.set_len(len as u64).map_err(|error| CaptureTransferError::SharedMemory {
            operation: "resize",
            message: error.to_string(),
        })?;

        // SAFETY: mmap is called with a valid file descriptor, non-zero length,
        // and read/write shared permissions. The mapping is released in Drop.
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };

        if raw == libc::MAP_FAILED {
            let error = std::io::Error::last_os_error();
            let _ = fs::remove_file(&path);
            return Err(CaptureTransferError::SharedMemory {
                operation: "mmap",
                message: error.to_string(),
            });
        }

        Ok(Self {
            ptr: NonNull::new(raw.cast::<u8>()).expect("mmap returned null without MAP_FAILED"),
            len,
            path,
            _file: file,
        })
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub const fn payload_kind(&self) -> PayloadKind {
        PayloadKind::CpuShm
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr points to a live mapping of len bytes owned by self.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: ptr points to a live mutable mapping of len bytes owned by self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for SharedMemorySegment {
    fn drop(&mut self) {
        // SAFETY: ptr/len describe the mapping created in new.
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
        let _ = fs::remove_file(&self.path);
    }
}

fn unique_path() -> PathBuf {
    let id = NEXT_SEGMENT_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("capture-transfer-{}-{id}.shm", std::process::id()))
}

#[cfg(test)]
mod tests {
    use crate::shm::SharedMemorySegment;

    #[test]
    fn mapped_segment_roundtrips_bytes() {
        let mut segment = SharedMemorySegment::new(4).unwrap();

        segment.as_mut_slice().copy_from_slice(&[1, 2, 3, 4]);

        assert_eq!(segment.len(), 4);
        assert_eq!(segment.as_slice(), &[1, 2, 3, 4]);
    }

    #[test]
    fn zero_length_segment_is_rejected() {
        assert!(SharedMemorySegment::new(0).is_err());
    }
}
