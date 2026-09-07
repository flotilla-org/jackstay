//! Narrow DRM syncobj wrapper for the Linux native backend.
//!
//! The ADR-0009 Linux path needs real drm_syncobj timeline synchronization,
//! but the rest of `capture-transfer` should not grow raw ioctl knowledge.

use std::{
    fs::File,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::Path,
    sync::Arc,
};

use crate::{
    error::{CaptureTransferError, Result},
    native::linux::LinuxSyncHandle,
};

pub const DRM_CAP_SYNCOBJ: u64 = 0x13;
pub const DRM_CAP_SYNCOBJ_TIMELINE: u64 = 0x14;
pub const DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED: u32 = 1 << 0;
pub const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL: u32 = 1 << 0;
pub const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT: u32 = 1 << 1;
pub const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE: u32 = 1 << 2;
pub const FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE: u32 = 2;

mod ffi {
    unsafe extern "C" {
        pub fn porthole_native_linux_drm_get_cap(drm_fd: i32, capability: u64, out_value: *mut u64) -> i32;
        pub fn porthole_native_linux_syncobj_create(drm_fd: i32, flags: u32, out_handle: *mut u32) -> i32;
        pub fn porthole_native_linux_syncobj_destroy(drm_fd: i32, handle: u32) -> i32;
        pub fn porthole_native_linux_syncobj_export_timeline_fd(drm_fd: i32, handle: u32, out_fd: *mut i32) -> i32;
        pub fn porthole_native_linux_syncobj_import_timeline_fd(drm_fd: i32, fd: i32, out_handle: *mut u32) -> i32;
        pub fn porthole_native_linux_syncobj_timeline_signal(drm_fd: i32, handle: u32, point: u64) -> i32;
        pub fn porthole_native_linux_syncobj_timeline_query(drm_fd: i32, handle: u32, flags: u32, out_point: *mut u64) -> i32;
        pub fn porthole_native_linux_syncobj_timeline_wait(drm_fd: i32, handle: u32, point: u64, timeout_ns: u64, flags: u32) -> i32;
    }
}

#[derive(Debug)]
pub struct DrmDevice {
    file: Arc<File>,
}

impl DrmDevice {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| CaptureTransferError::NativeBackend {
                operation: "linux-drm-open",
                message: error.to_string(),
            })?;
        Ok(Self { file: Arc::new(file) })
    }

    #[must_use]
    pub fn raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    pub fn get_cap(&self, capability: u64) -> Result<u64> {
        let mut value = 0;
        check("linux-drm-get-cap", unsafe {
            ffi::porthole_native_linux_drm_get_cap(self.raw_fd(), capability, &mut value)
        })?;
        Ok(value)
    }

    pub fn supports_syncobj_timeline(&self) -> Result<bool> {
        Ok(self.get_cap(DRM_CAP_SYNCOBJ)? != 0 && self.get_cap(DRM_CAP_SYNCOBJ_TIMELINE)? != 0)
    }

    pub fn create_syncobj_timeline(&self) -> Result<DrmSyncobjTimeline> {
        let mut handle = 0;
        check("linux-drm-syncobj-create", unsafe {
            ffi::porthole_native_linux_syncobj_create(self.raw_fd(), 0, &mut handle)
        })?;
        Ok(DrmSyncobjTimeline {
            device: Arc::clone(&self.file),
            handle,
        })
    }

    pub fn import_syncobj_timeline_fd(&self, fd: RawFd) -> Result<DrmSyncobjTimeline> {
        let mut handle = 0;
        check("linux-drm-syncobj-import-timeline-fd", unsafe {
            ffi::porthole_native_linux_syncobj_import_timeline_fd(self.raw_fd(), fd, &mut handle)
        })?;
        Ok(DrmSyncobjTimeline {
            device: Arc::clone(&self.file),
            handle,
        })
    }
}

#[derive(Debug)]
pub struct DrmSyncobjTimeline {
    device: Arc<File>,
    handle: u32,
}

impl DrmSyncobjTimeline {
    #[must_use]
    pub fn handle(&self) -> u32 {
        self.handle
    }

    pub fn export_fd(&self) -> Result<OwnedFd> {
        let mut fd = -1;
        check("linux-drm-syncobj-export-timeline-fd", unsafe {
            ffi::porthole_native_linux_syncobj_export_timeline_fd(self.device.as_raw_fd(), self.handle, &mut fd)
        })?;
        if fd < 0 {
            return Err(CaptureTransferError::NativeBackend {
                operation: "linux-drm-syncobj-export-timeline-fd",
                message: "DRM returned a negative fd without an error".to_string(),
            });
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn export_handle(&self, sync_id: u64) -> Result<LinuxSyncHandle> {
        Ok(LinuxSyncHandle {
            sync_kind: FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE,
            sync_id,
            fd: self.export_fd()?,
        })
    }

    pub fn signal(&self, point: u64) -> Result<()> {
        check("linux-drm-syncobj-timeline-signal", unsafe {
            ffi::porthole_native_linux_syncobj_timeline_signal(self.device.as_raw_fd(), self.handle, point)
        })
    }

    pub fn query(&self, flags: u32) -> Result<u64> {
        let mut point = 0;
        check("linux-drm-syncobj-timeline-query", unsafe {
            ffi::porthole_native_linux_syncobj_timeline_query(self.device.as_raw_fd(), self.handle, flags, &mut point)
        })?;
        Ok(point)
    }

    pub fn wait(&self, point: u64, timeout_ns: u64, flags: u32) -> Result<()> {
        check("linux-drm-syncobj-timeline-wait", unsafe {
            ffi::porthole_native_linux_syncobj_timeline_wait(self.device.as_raw_fd(), self.handle, point, timeout_ns, flags)
        })
    }
}

impl Drop for DrmSyncobjTimeline {
    fn drop(&mut self) {
        let _ = unsafe { ffi::porthole_native_linux_syncobj_destroy(self.device.as_raw_fd(), self.handle) };
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
    use std::os::fd::AsRawFd;

    use super::{
        DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT, DrmDevice,
    };

    #[test]
    fn drm_syncobj_timeline_round_trips_through_exported_fd_when_render_node_available() {
        let Ok(device) = DrmDevice::open("/dev/dri/renderD128") else {
            return;
        };
        if !device.supports_syncobj_timeline().unwrap_or(false) {
            return;
        }

        let timeline = device.create_syncobj_timeline().unwrap();
        timeline.signal(7).unwrap();
        assert_eq!(timeline.query(DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED).unwrap(), 7);
        timeline
            .wait(7, 0, DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT)
            .unwrap();

        let exported = timeline.export_fd().unwrap();
        let imported = device.import_syncobj_timeline_fd(exported.as_raw_fd()).unwrap();
        assert_eq!(imported.query(DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED).unwrap(), 7);
        imported.signal(9).unwrap();
        assert_eq!(timeline.query(DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED).unwrap(), 9);
    }
}
