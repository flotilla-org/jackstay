#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(windows)]
use std::os::windows::io::OwnedHandle as OwnedFd;
#[cfg(unix)]
use std::{fs::File, os::fd::AsRawFd, ptr::NonNull};

use crate::{
    error::{CaptureTransferError, Result},
    model::PayloadKind,
};

#[derive(Debug)]
pub struct SharedMemorySegment {
    #[cfg(unix)]
    ptr: NonNull<u8>,
    #[cfg(unix)]
    len: usize,
    #[cfg(unix)]
    writable: bool,
    #[cfg(unix)]
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

#[cfg(unix)]
impl SharedMemorySegment {
    pub fn new(len: usize) -> Result<Self> {
        if len == 0 {
            return Err(CaptureTransferError::InvalidSharedMemoryLength);
        }

        let file = create_anonymous_backing(len)?;

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
            return Err(CaptureTransferError::SharedMemory {
                operation: "mmap",
                message: std::io::Error::last_os_error().to_string(),
            });
        }

        Ok(Self {
            ptr: NonNull::new(raw.cast::<u8>()).expect("mmap returned null without MAP_FAILED"),
            len,
            writable: true,
            _file: file,
        })
    }

    pub fn map_read_only(fd: OwnedFd, len: usize) -> Result<Self> {
        if len == 0 {
            return Err(CaptureTransferError::InvalidSharedMemoryLength);
        }
        let file = File::from(fd);
        let backing_len = file.metadata().map_err(|error| CaptureTransferError::SharedMemory {
            operation: "stat-read-only",
            message: error.to_string(),
        })?;
        if len as u64 > backing_len.len() {
            return Err(CaptureTransferError::SharedMemory {
                operation: "mmap-read-only",
                message: format!("requested map length {len} exceeds backing file length {}", backing_len.len()),
            });
        }
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
            writable: false,
            _file: file,
        })
    }

    pub fn map_read_write(fd: OwnedFd, len: usize) -> Result<Self> {
        if len == 0 {
            return Err(CaptureTransferError::InvalidSharedMemoryLength);
        }
        let file = File::from(fd);
        let backing_len = file.metadata().map_err(|error| CaptureTransferError::SharedMemory {
            operation: "stat-read-write",
            message: error.to_string(),
        })?;
        if len as u64 > backing_len.len() {
            return Err(CaptureTransferError::SharedMemory {
                operation: "mmap-read-write",
                message: format!("requested map length {len} exceeds backing file length {}", backing_len.len()),
            });
        }
        // SAFETY: mmap is called with a valid fd, non-zero length, and
        // read/write shared permissions. The mapping is released in Drop.
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
            return Err(CaptureTransferError::SharedMemory {
                operation: "mmap-read-write",
                message: std::io::Error::last_os_error().to_string(),
            });
        }
        Ok(Self {
            ptr: NonNull::new(raw.cast::<u8>()).expect("mmap returned null without MAP_FAILED"),
            len,
            writable: true,
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

#[cfg(unix)]
impl Drop for SharedMemorySegment {
    fn drop(&mut self) {
        // SAFETY: ptr/len describe the mapping created in new. The kernel keeps
        // the anonymous backing object alive while any other fd or mapping
        // (including in other processes) still references it.
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}

#[cfg(windows)]
impl SharedMemorySegment {
    pub fn new(len: usize) -> Result<Self> {
        if len == 0 {
            return Err(CaptureTransferError::InvalidSharedMemoryLength);
        }
        Err(unimplemented_windows_shm("create"))
    }

    pub fn map_read_only(_fd: OwnedFd, len: usize) -> Result<Self> {
        if len == 0 {
            return Err(CaptureTransferError::InvalidSharedMemoryLength);
        }
        Err(unimplemented_windows_shm("mmap-read-only"))
    }

    pub fn map_read_write(_fd: OwnedFd, len: usize) -> Result<Self> {
        if len == 0 {
            return Err(CaptureTransferError::InvalidSharedMemoryLength);
        }
        Err(unimplemented_windows_shm("mmap-read-write"))
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        0
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        true
    }

    #[must_use]
    pub const fn payload_kind(&self) -> PayloadKind {
        PayloadKind::CpuShm
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &[]
    }

    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        panic!("shm backing unimplemented on Windows")
    }

    #[must_use]
    pub fn slice_at(&self, offset: usize, len: usize) -> &[u8] {
        assert_eq!(offset, 0);
        assert_eq!(len, 0);
        &[]
    }

    pub fn with_slice_at_mut<R>(&self, _offset: usize, _len: usize, _f: impl FnOnce(&mut [u8]) -> R) -> R {
        panic!("shm backing unimplemented on Windows")
    }

    pub fn write_at(&self, _offset: usize, _bytes: &[u8]) {
        panic!("shm backing unimplemented on Windows")
    }

    pub fn try_clone_fd(&self) -> Result<OwnedFd> {
        Err(unimplemented_windows_shm("clone-fd"))
    }
}

#[cfg(windows)]
fn unimplemented_windows_shm(operation: &'static str) -> CaptureTransferError {
    CaptureTransferError::SharedMemory {
        operation,
        message: "shm backing unimplemented on Windows".to_string(),
    }
}

/// Creates the anonymous memory object backing a segment.
///
/// The object must never appear in a filesystem namespace once this returns:
/// the fd (and fds passed to consumers) is the only handle, so a killed
/// process leaks nothing, and pages are swap-backed rather than written back
/// to disk by the pager.
#[cfg(target_os = "macos")]
fn create_anonymous_backing(len: usize) -> Result<File> {
    use std::{
        ffi::CString,
        os::fd::FromRawFd,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_SEGMENT_ID: AtomicU64 = AtomicU64::new(1);

    for _ in 0..64 {
        let id = NEXT_SEGMENT_ID.fetch_add(1, Ordering::Relaxed);
        // POSIX shm names are capped at PSHMNAMLEN (31) bytes on macOS; hex
        // pid + id stays within it. The name only exists between shm_open and
        // shm_unlink below; uniqueness just avoids EEXIST against strays.
        let name = CString::new(format!("/js.{:x}.{id:x}", std::process::id())).expect("shm name contains no NUL");
        // SAFETY: name is a valid NUL-terminated string; mode is passed for O_CREAT.
        let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600 as libc::c_uint) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EEXIST) {
                continue;
            }
            return Err(CaptureTransferError::SharedMemory {
                operation: "shm-open",
                message: error.to_string(),
            });
        }
        // SAFETY: fd was just returned by shm_open and is owned by nothing else.
        let file = unsafe { File::from_raw_fd(fd) };
        // Drop the name immediately: the kernel refcounts the object (xnu holds
        // a usecount reference per fd and mapping), so only handles keep it
        // alive from here on.
        // SAFETY: name is the valid NUL-terminated string opened above.
        let _ = unsafe { libc::shm_unlink(name.as_ptr()) };
        // ftruncate is only honoured once for a macOS shm object; segments are
        // sized exactly once at creation and pools are replaced, never resized.
        file.set_len(len as u64).map_err(|error| CaptureTransferError::SharedMemory {
            operation: "resize",
            message: error.to_string(),
        })?;
        return Ok(file);
    }
    Err(CaptureTransferError::SharedMemory {
        operation: "shm-open",
        message: "could not find an unused shm name".to_string(),
    })
}

#[cfg(target_os = "linux")]
fn create_anonymous_backing(len: usize) -> Result<File> {
    use std::os::fd::FromRawFd;

    // The name is purely diagnostic (/proc/self/fd); memfds are anonymous.
    let name = c"jackstay-segment";
    // SAFETY: name is a valid NUL-terminated string.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(CaptureTransferError::SharedMemory {
            operation: "memfd-create",
            message: std::io::Error::last_os_error().to_string(),
        });
    }
    // SAFETY: fd was just returned by memfd_create and is owned by nothing else.
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_len(len as u64).map_err(|error| CaptureTransferError::SharedMemory {
        operation: "resize",
        message: error.to_string(),
    })?;
    Ok(file)
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn create_anonymous_backing(_len: usize) -> Result<File> {
    Err(CaptureTransferError::SharedMemory {
        operation: "create",
        message: "anonymous shm backing is only implemented for macOS and Linux".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use crate::{CaptureTransferError, shm::SharedMemorySegment};

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
    fn zero_length_read_only_mapping_is_rejected() {
        let segment = SharedMemorySegment::new(4).unwrap();

        let error = SharedMemorySegment::map_read_only(segment.try_clone_fd().unwrap(), 0).unwrap_err();

        assert_eq!(error, CaptureTransferError::InvalidSharedMemoryLength);
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

    #[test]
    fn read_only_mapping_rejects_len_larger_than_backing_file() {
        let segment = SharedMemorySegment::new(4).unwrap();

        // Anonymous shm objects are page-rounded by the kernel, so the guard
        // is page-granular: overshoot by far more than any page size.
        let error = SharedMemorySegment::map_read_only(segment.try_clone_fd().unwrap(), 1 << 21).unwrap_err();

        assert!(matches!(
            error,
            CaptureTransferError::SharedMemory {
                operation: "mmap-read-only",
                ..
            }
        ));
    }
}
