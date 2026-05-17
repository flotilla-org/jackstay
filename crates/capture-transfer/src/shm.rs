use std::{
    fs::{self, File, OpenOptions},
    os::fd::{AsRawFd, OwnedFd},
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
    path: Option<PathBuf>,
    writable: bool,
    unlink_on_drop: bool,
    _file: File,
}

// SAFETY: SharedMemorySegment owns a file-backed mmap. Shared reads are safe;
// producer writes use explicit offset/length APIs and are guarded by the slot
// manager so pinned ranges are not overwritten. Drop unmaps only when the final
// owner goes away.
unsafe impl Send for SharedMemorySegment {}
// SAFETY: Interior-mutable writes through with_slice_at_mut/write_at are safe
// across threads only under the VideoSlotManager claim/commit discipline: claims
// reserve exclusive slot ranges, and committed frames are not overwritten while
// pinned by consumers.
unsafe impl Sync for SharedMemorySegment {}

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
            path: Some(path),
            writable: true,
            unlink_on_drop: true,
            _file: file,
        })
    }

    pub fn map_read_only(fd: OwnedFd, len: usize) -> Result<Self> {
        if len == 0 {
            return Err(CaptureTransferError::InvalidSharedMemoryLength);
        }
        let file = File::from(fd);
        // SAFETY: mmap is called with a valid fd, non-zero length, and read-only
        // shared permissions. The mapping is released in Drop.
        let raw = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd(), 0) };
        if raw == libc::MAP_FAILED {
            return Err(CaptureTransferError::SharedMemory {
                operation: "mmap-read-only",
                message: std::io::Error::last_os_error().to_string(),
            });
        }
        Ok(Self {
            ptr: NonNull::new(raw.cast::<u8>()).expect("mmap returned null without MAP_FAILED"),
            len,
            path: None,
            writable: false,
            unlink_on_drop: false,
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
        assert!(self.writable);
        // SAFETY: ptr points to a live mutable mapping of len bytes owned by self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    #[must_use]
    pub fn slice_at(&self, offset: usize, len: usize) -> &[u8] {
        assert!(offset <= self.len);
        assert!(len <= self.len - offset);
        // SAFETY: offset and len were bounds-checked against the live mapping.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().add(offset), len) }
    }

    pub fn with_slice_at_mut<R>(&self, offset: usize, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
        assert!(self.writable);
        assert!(offset <= self.len);
        assert!(len <= self.len - offset);
        // SAFETY: offset and len were bounds-checked. The slot manager only
        // hands this out for producer-owned slots that are not pinned.
        let slice = unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().add(offset), len) };
        f(slice)
    }

    pub fn write_at(&self, offset: usize, bytes: &[u8]) {
        assert!(self.writable);
        assert!(offset <= self.len);
        assert!(bytes.len() <= self.len - offset);
        // SAFETY: offset and len were bounds-checked. Callers only write into a
        // producer-owned slot that is not pinned by acquired frames.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.as_ptr().add(offset), bytes.len());
        }
    }

    pub fn try_clone_fd(&self) -> Result<OwnedFd> {
        self._file
            .try_clone()
            .map(OwnedFd::from)
            .map_err(|error| CaptureTransferError::SharedMemory {
                operation: "clone-fd",
                message: error.to_string(),
            })
    }
}

impl Drop for SharedMemorySegment {
    fn drop(&mut self) {
        // SAFETY: ptr/len describe the mapping created in new.
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
        if self.unlink_on_drop
            && let Some(path) = &self.path
        {
            let _ = fs::remove_file(path);
        }
    }
}

fn unique_path() -> PathBuf {
    // Uniqueness only; no ordering with other memory is required.
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

    #[test]
    fn slice_at_returns_bounded_subrange() {
        let mut segment = SharedMemorySegment::new(8).unwrap();
        segment.as_mut_slice().copy_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7]);

        assert_eq!(segment.slice_at(2, 3), &[2, 3, 4]);
    }

    #[test]
    fn write_at_updates_bounded_subrange() {
        let segment = SharedMemorySegment::new(8).unwrap();

        segment.write_at(2, &[7, 8, 9]);

        assert_eq!(segment.slice_at(0, 6), &[0, 0, 7, 8, 9, 0]);
    }

    #[test]
    fn with_slice_at_mut_updates_bounded_subrange() {
        let segment = SharedMemorySegment::new(8).unwrap();

        segment.with_slice_at_mut(3, 2, |slice| slice.copy_from_slice(&[5, 6]));

        assert_eq!(segment.slice_at(0, 6), &[0, 0, 0, 5, 6, 0]);
    }

    #[test]
    fn read_only_mapping_clones_contents_from_fd() {
        let mut segment = SharedMemorySegment::new(4).unwrap();
        segment.as_mut_slice().copy_from_slice(&[1, 2, 3, 4]);
        let clone = SharedMemorySegment::map_read_only(segment.try_clone_fd().unwrap(), 4).unwrap();

        assert_eq!(clone.as_slice(), &[1, 2, 3, 4]);
    }
}
