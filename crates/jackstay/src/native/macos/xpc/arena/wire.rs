use std::{
    ffi::{c_char, c_void},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    ptr::NonNull,
};

use serde::{Serialize, de::DeserializeOwned};

use crate::{
    acquisition::arena::ArenaError,
    native::macos::{IoSurface, SharedEventHandle},
};

pub(super) mod ffi {
    use super::*;
    unsafe extern "C" {
        pub fn jsa_message_new(
            bytes: *const u8,
            length: usize,
            fds: *const i32,
            fd_count: usize,
            surfaces: *const *mut c_void,
            surface_count: usize,
            event: *mut c_void,
        ) -> *mut c_void;
        pub fn jsa_object_release(object: *mut c_void);
        pub fn jsa_object_retain(object: *mut c_void) -> *mut c_void;
        pub fn jsa_message_bytes(message: *mut c_void, length: *mut usize) -> *const u8;
        pub fn jsa_message_fd_count(message: *mut c_void) -> usize;
        pub fn jsa_message_surface_count(message: *mut c_void) -> usize;
        pub fn jsa_message_copy_fd(message: *mut c_void, index: usize) -> i32;
        pub fn jsa_message_copy_surface(message: *mut c_void, index: usize) -> *mut c_void;
        pub fn jsa_message_copy_event(message: *mut c_void) -> *mut c_void;
        pub fn jsa_server_start(
            name: *const c_char,
            context: *mut c_void,
            request: extern "C" fn(*mut c_void, u64, u32, *mut c_void) -> *mut c_void,
            close: extern "C" fn(*mut c_void, u64),
            destroy: extern "C" fn(*mut c_void),
        ) -> *mut c_void;
        pub fn jsa_server_endpoint(server: *mut c_void) -> *mut c_void;
        pub fn jsa_server_stop(server: *mut c_void);
        pub fn jsa_client_connect(name: *const c_char, endpoint: *mut c_void) -> *mut c_void;
        pub fn jsa_client_close(client: *mut c_void);
        pub fn jsa_client_request(client: *mut c_void, input: *mut c_void, output: *mut *mut c_void) -> *mut c_char;
    }
}

pub(super) struct Envelope(pub NonNull<c_void>);
pub(super) struct Payload<T> {
    pub metadata: T,
    pub fds: Vec<OwnedFd>,
    pub surfaces: Vec<IoSurface>,
    pub event: Option<SharedEventHandle>,
}

impl Envelope {
    pub fn new<T: Serialize>(
        metadata: &T,
        fds: &[OwnedFd],
        surfaces: &[IoSurface],
        event: Option<&SharedEventHandle>,
    ) -> Result<Self, ArenaError> {
        let bytes = serde_json::to_vec(metadata).map_err(|error| super::failure(error.to_string()))?;
        let fds: Vec<_> = fds.iter().map(AsRawFd::as_raw_fd).collect();
        let surfaces: Vec<_> = surfaces.iter().map(IoSurface::as_raw).collect();
        let raw = unsafe {
            ffi::jsa_message_new(
                bytes.as_ptr(),
                bytes.len(),
                fds.as_ptr(),
                fds.len(),
                surfaces.as_ptr(),
                surfaces.len(),
                event.map_or(std::ptr::null_mut(), SharedEventHandle::as_raw),
            )
        };
        NonNull::new(raw)
            .map(Self)
            .ok_or_else(|| super::failure("could not prepare XPC setup resources"))
    }

    pub fn decode<T: DeserializeOwned>(self) -> Result<Payload<T>, ArenaError> {
        let mut length = 0;
        let bytes = unsafe { ffi::jsa_message_bytes(self.0.as_ptr(), &mut length) };
        if length == 0 || length > 1024 * 1024 || bytes.is_null() {
            return Err(ArenaError::Mapping("invalid XPC setup metadata length"));
        }
        let metadata = serde_json::from_slice(unsafe { std::slice::from_raw_parts(bytes, length) })
            .map_err(|_| ArenaError::Mapping("invalid XPC setup metadata"))?;
        let fd_count = unsafe { ffi::jsa_message_fd_count(self.0.as_ptr()) };
        let surface_count = unsafe { ffi::jsa_message_surface_count(self.0.as_ptr()) };
        if fd_count > 5 || surface_count > 65536 {
            return Err(ArenaError::Mapping("invalid XPC setup resource count"));
        }
        let mut fds = Vec::with_capacity(fd_count);
        for index in 0..fd_count {
            let fd = unsafe { ffi::jsa_message_copy_fd(self.0.as_ptr(), index) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            fds.push(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let mut surfaces = Vec::with_capacity(surface_count);
        for index in 0..surface_count {
            let raw = unsafe { ffi::jsa_message_copy_surface(self.0.as_ptr(), index) };
            let raw = NonNull::new(raw).ok_or(ArenaError::Mapping("missing XPC surface"))?;
            surfaces.push(unsafe { IoSurface::from_retained(raw) });
        }
        let event = NonNull::new(unsafe { ffi::jsa_message_copy_event(self.0.as_ptr()) })
            .map(|raw| unsafe { SharedEventHandle::from_retained(raw) });
        // The envelope's received FD/object copies are destroyed before the
        // caller constructs a grant that could acknowledge their retirement.
        Ok(Payload {
            metadata,
            fds,
            surfaces,
            event,
        })
    }

    pub fn into_raw(self) -> *mut c_void {
        let raw = self.0.as_ptr();
        std::mem::forget(self);
        raw
    }
}
impl Drop for Envelope {
    fn drop(&mut self) {
        unsafe { ffi::jsa_object_release(self.0.as_ptr()) };
    }
}
