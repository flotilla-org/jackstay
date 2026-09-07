//! Narrow dma-heap wrapper for Linux dmabuf allocation.
//!
//! This is the allocation half of the ADR-0009 synthetic dmabuf producer. The
//! kernel heap returns a real dmabuf fd; format, modifier, and GPU import are
//! handled by the backend layer that consumes this primitive.

use std::{
    fs::File,
    io,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::Path,
};

use crate::{
    error::{CaptureTransferError, Result},
    native::linux::{LinuxDmabufPlaneHandle, LinuxSurfaceHandle},
};

pub const FT_NATIVE_HANDLE_DMABUF: u32 = 2;

mod ffi {
    unsafe extern "C" {
        pub fn porthole_native_linux_dma_heap_alloc(heap_fd: i32, len: u64, fd_flags: u32, heap_flags: u64, out_fd: *mut i32) -> i32;
    }
}

#[derive(Debug)]
pub struct DmaHeap {
    file: File,
}

impl DmaHeap {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| CaptureTransferError::NativeBackend {
                operation: "linux-dma-heap-open",
                message: error.to_string(),
            })?;
        Ok(Self { file })
    }

    #[must_use]
    pub fn raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    pub fn allocate(&self, len: u64) -> Result<DmaBuf> {
        if len == 0 {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-dma-heap-alloc",
                message: "dmabuf allocation length must be greater than zero".to_string(),
            });
        }
        let mut fd = -1;
        check("linux-dma-heap-alloc", unsafe {
            ffi::porthole_native_linux_dma_heap_alloc(self.raw_fd(), len, (libc::O_CLOEXEC | libc::O_RDWR) as u32, 0, &mut fd)
        })?;
        if fd < 0 {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-dma-heap-alloc",
                message: "dma-heap returned a negative fd without an error".to_string(),
            });
        }
        Ok(DmaBuf {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            len,
        })
    }
}

#[derive(Debug)]
pub struct DmaBuf {
    fd: OwnedFd,
    len: u64,
}

impl DmaBuf {
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn try_clone_fd(&self) -> Result<OwnedFd> {
        self.fd
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| CaptureTransferError::FdPassing {
                operation: "clone-linux-dmabuf",
                message: error.to_string(),
            })
    }

    pub fn into_fd(self) -> OwnedFd {
        self.fd
    }

    pub fn into_bgra_surface_handle(self, width: u32, height: u32, pixel_format: u32, modifier: u64, stride: u32) -> LinuxSurfaceHandle {
        LinuxSurfaceHandle {
            handle_kind: FT_NATIVE_HANDLE_DMABUF,
            width,
            height,
            pixel_format,
            modifier,
            planes: vec![LinuxDmabufPlaneHandle {
                fd: self.into_fd(),
                offset: 0,
                stride,
            }],
        }
    }
}

fn check(operation: &'static str, errno: i32) -> Result<()> {
    if errno == 0 {
        return Ok(());
    }
    Err(CaptureTransferError::NativeBackend {
        operation,
        message: io::Error::from_raw_os_error(errno).to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::DmaHeap;
    use crate::{error::CaptureTransferError, model::PixelFormat};

    #[test]
    fn rejects_zero_length_allocation_before_calling_kernel() {
        let Err(error) = DmaHeap::open("/dev/dma_heap/system").and_then(|heap| heap.allocate(0)) else {
            return;
        };
        if let CaptureTransferError::NativeBackend { operation, message } = error {
            if operation == "linux-dma-heap-open" {
                return;
            }
            assert_eq!(operation, "linux-dma-heap-alloc");
            assert!(message.contains("greater than zero"));
            return;
        }
        panic!("unexpected error: {error}");
    }

    #[test]
    fn dma_heap_allocation_returns_real_dmabuf_when_heap_is_available() {
        let heap = match DmaHeap::open("/dev/dma_heap/system") {
            Ok(heap) => heap,
            Err(CaptureTransferError::NativeBackend {
                operation: "linux-dma-heap-open",
                message,
            }) if message.contains("Permission denied") || message.contains("No such file") => return,
            Err(error) => panic!("unexpected dma-heap open error: {error}"),
        };

        let buffer = heap.allocate(4096).unwrap();
        assert_eq!(buffer.len(), 4096);
        assert!(buffer.raw_fd() >= 0);
        let surface = buffer.into_bgra_surface_handle(16, 16, PixelFormat::Bgra8Unorm as u32, 0, 64);
        assert_eq!(surface.handle_kind, 2);
        assert_eq!(surface.planes.len(), 1);
        assert_eq!(surface.planes[0].stride, 64);
    }
}
