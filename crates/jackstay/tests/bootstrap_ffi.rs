#![cfg(unix)]
use std::{
    os::{
        fd::{AsRawFd, IntoRawFd},
        unix::net::UnixStream,
    },
    ptr, thread,
};

use jackstay::{ffi::*, ffi_bootstrap::*};

#[test]
fn c_observer_returns_original_media_fds_without_an_input_owner() {
    let (host, peer) = UnixStream::pair().unwrap();
    let host_fd = host.as_raw_fd();
    let peer_fd = peer.as_raw_fd();
    let worker = thread::spawn(move || {
        let mut fd = host.into_raw_fd();
        let mut server = ptr::null_mut();
        assert_eq!(
            unsafe { ft_source_bootstrap_accept(&mut fd, ptr::null_mut(), &mut server) },
            FT_STATUS_OK
        );
        assert!(server.is_null());
        assert_eq!(fd, host_fd);
        unsafe {
            libc::close(fd);
        }
    });
    let mut fd = peer.into_raw_fd();
    let mut client = ptr::null_mut();
    let mut input_status = FT_STATUS_ERROR;
    assert_eq!(
        unsafe { ft_source_bootstrap_connect(&mut fd, FT_BOOTSTRAP_INPUT_NONE, 0, &mut client, &mut input_status) },
        FT_STATUS_OK
    );
    assert_eq!(input_status, FT_STATUS_EMPTY);
    assert!(client.is_null());
    assert_eq!(fd, peer_fd);
    unsafe {
        libc::close(fd);
    }
    worker.join().unwrap();
}

#[test]
fn c_basic_argument_errors_keep_fd_but_failed_negotiation_consumes_it() {
    let (host, peer) = UnixStream::pair().unwrap();
    let mut fd = peer.into_raw_fd();
    let original = fd;
    let mut client = ptr::null_mut();
    let mut input_status = FT_STATUS_ERROR;
    assert_eq!(
        unsafe { ft_source_bootstrap_connect(&mut fd, FT_BOOTSTRAP_INPUT_REQUIRED, 0, &mut client, &mut input_status) },
        FT_STATUS_INVALID_ARGUMENT
    );
    assert_eq!(fd, original);
    let worker = thread::spawn(move || {
        let mut fd = host.into_raw_fd();
        let mut server = ptr::null_mut();
        assert_eq!(
            unsafe { ft_source_bootstrap_accept(&mut fd, ptr::null_mut(), &mut server) },
            FT_STATUS_OK
        );
        unsafe {
            libc::close(fd);
        }
    });
    assert_eq!(
        unsafe { ft_source_bootstrap_connect(&mut fd, FT_BOOTSTRAP_INPUT_REQUIRED, 4, &mut client, &mut input_status) },
        FT_STATUS_UNSUPPORTED
    );
    assert_eq!(fd, -1);
    assert!(client.is_null());
    worker.join().unwrap();
}
