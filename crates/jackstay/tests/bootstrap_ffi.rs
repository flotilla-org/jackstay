//! C source bootstrap: the POSIX fd entry points, and Local Endpoint
//! connections on every platform (named pipes on Windows).
#[cfg(unix)]
use std::os::{
    fd::{AsRawFd, IntoRawFd},
    unix::net::UnixStream,
};
use std::{ptr, thread};

use jackstay::{
    ffi::*,
    ffi_bootstrap::*,
    ffi_input::*,
    ffi_local::FtLocalConnection,
    local::{self, Endpoint, Scope, Transport},
};

fn local_pair() -> (*mut FtLocalConnection, *mut FtLocalConnection) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = format!("bootstrap-ffi-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
    let endpoint = Endpoint::new(Scope::User, &name, Transport::LocalStream).unwrap();
    let listener = local::Listener::bind(&endpoint).unwrap();
    let client = thread::spawn(move || local::connect(&endpoint).unwrap());
    let accepted = listener.accept().unwrap();
    let connected = client.join().unwrap();
    (Box::into_raw(Box::new(accepted.into())), Box::into_raw(Box::new(connected.into())))
}

/// Moves a C handle across threads in a test.
struct Owned<T>(T);
// SAFETY: test-only transfer of an exclusively owned handle to one thread.
unsafe impl<T> Send for Owned<T> {}

#[cfg(unix)]
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

#[cfg(unix)]
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

#[test]
fn local_observer_keeps_the_connection_for_media_without_an_input_owner() {
    let (host, peer) = local_pair();
    let host = Owned(host);
    let worker = thread::spawn(move || {
        let mut connection = host;
        let mut server = ptr::null_mut();
        // SAFETY: exclusively owned connection handle; output starts null.
        assert_eq!(
            unsafe { ft_source_bootstrap_accept_local(&mut connection.0, ptr::null_mut(), &mut server) },
            FT_STATUS_OK
        );
        assert!(server.is_null());
        assert!(!connection.0.is_null());
        // SAFETY: the returned connection handle is live and owned here.
        unsafe { jackstay::ffi_local::ft_local_connection_destroy(&mut connection.0) };
    });
    let mut connection = peer;
    let mut client = ptr::null_mut();
    let mut input_status = FT_STATUS_ERROR;
    // SAFETY: exclusively owned connection handle; outputs are disjoint.
    assert_eq!(
        unsafe { ft_source_bootstrap_connect_local(&mut connection, FT_BOOTSTRAP_INPUT_NONE, 0, &mut client, &mut input_status) },
        FT_STATUS_OK
    );
    assert_eq!(input_status, FT_STATUS_EMPTY);
    assert!(client.is_null());
    assert!(!connection.is_null());
    // SAFETY: live, owned connection handle.
    unsafe { jackstay::ffi_local::ft_local_connection_destroy(&mut connection) };
    worker.join().unwrap();
}

#[test]
fn local_argument_errors_keep_the_connection_but_failed_negotiation_consumes_it() {
    let (host, peer) = local_pair();
    let mut connection = peer;
    let original = connection;
    let mut client = ptr::null_mut();
    let mut input_status = FT_STATUS_ERROR;
    // SAFETY: owned connection handle; invalid mode is rejected before transfer.
    assert_eq!(
        unsafe { ft_source_bootstrap_connect_local(&mut connection, FT_BOOTSTRAP_INPUT_REQUIRED, 0, &mut client, &mut input_status) },
        FT_STATUS_INVALID_ARGUMENT
    );
    assert_eq!(connection, original);
    let host = Owned(host);
    let worker = thread::spawn(move || {
        let mut connection = host;
        let mut server = ptr::null_mut();
        // SAFETY: exclusively owned connection handle; output starts null.
        assert_eq!(
            unsafe { ft_source_bootstrap_accept_local(&mut connection.0, ptr::null_mut(), &mut server) },
            FT_STATUS_OK
        );
        // SAFETY: returned live handle.
        unsafe { jackstay::ffi_local::ft_local_connection_destroy(&mut connection.0) };
    });
    // SAFETY: as above; required input the host did not authorize fails.
    assert_eq!(
        unsafe { ft_source_bootstrap_connect_local(&mut connection, FT_BOOTSTRAP_INPUT_REQUIRED, 4, &mut client, &mut input_status) },
        FT_STATUS_UNSUPPORTED
    );
    assert!(connection.is_null());
    assert!(client.is_null());
    worker.join().unwrap();
}

#[test]
fn local_bootstrap_delivers_required_input_through_the_c_api() {
    use std::time::{Duration, Instant};
    // SAFETY: every handle is exclusively owned, created and destroyed once;
    // the text buffer outlives the send that copies it.
    unsafe {
        let mut config = FtInputConfig::default();
        ft_input_config_default(&mut config);
        let mut target = ptr::null_mut();
        assert_eq!(ft_input_target_create(&config, &mut target), FT_STATUS_OK);
        let (host, peer) = local_pair();
        let (host, shared_target) = (Owned(host), Owned(target));
        let worker = thread::spawn(move || {
            let (mut connection, target) = (host, shared_target);
            let mut server = ptr::null_mut();
            assert_eq!(
                ft_source_bootstrap_accept_local(&mut connection.0, target.0, &mut server),
                FT_STATUS_OK
            );
            assert!(!server.is_null());
            jackstay::ffi_local::ft_local_connection_destroy(&mut connection.0);
            Owned(server)
        });
        let mut connection = peer;
        let mut client = ptr::null_mut();
        let mut input_status = FT_STATUS_ERROR;
        assert_eq!(
            ft_source_bootstrap_connect_local(&mut connection, FT_BOOTSTRAP_INPUT_REQUIRED, 4, &mut client, &mut input_status),
            FT_STATUS_OK
        );
        assert_eq!(input_status, FT_STATUS_OK);
        let mut server = worker.join().unwrap();
        let text = "hello from C";
        let event = FtInputEvent {
            kind: 2,
            text: text.as_ptr(),
            text_len: text.len(),
            ..Default::default()
        };
        let mut sequence = 0;
        assert_eq!(ft_input_client_send(client, &event, &mut sequence), FT_STATUS_OK);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut work = ptr::null_mut();
        while ft_input_target_next(target, &mut work) == FT_STATUS_EMPTY {
            assert!(Instant::now() < deadline, "no input arrived");
            thread::sleep(Duration::from_millis(2));
        }
        let mut operation = FtInputOperation::default();
        assert_eq!(ft_input_work_describe(work, &mut operation), FT_STATUS_OK);
        assert_eq!(
            std::slice::from_raw_parts(operation.event.text, operation.event.text_len),
            text.as_bytes()
        );
        assert_eq!(ft_input_work_complete(&mut work, 0), FT_STATUS_OK);
        let mut status = FtInputStatus::default();
        while ft_input_client_poll(client, &mut status) == FT_STATUS_EMPTY {
            assert!(Instant::now() < deadline, "no completion arrived");
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!((status.kind, status.sequence, status.result), (1, sequence, 0));
        ft_input_client_destroy(&mut client);
        ft_input_server_destroy(&mut server.0);
        jackstay::ffi_local::ft_local_connection_destroy(&mut connection);
        loop {
            let mut cleanup = ptr::null_mut();
            if ft_input_target_next(target, &mut cleanup) == FT_STATUS_OK {
                assert_eq!(ft_input_work_complete(&mut cleanup, 0), FT_STATUS_OK);
            }
            if ft_input_target_destroy(&mut target) == FT_STATUS_OK {
                break;
            }
            assert!(Instant::now() < deadline, "input cleanup did not settle");
            thread::sleep(Duration::from_millis(2));
        }
    }
}
