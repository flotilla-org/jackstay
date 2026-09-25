#![cfg(any(target_os = "macos", target_os = "linux"))]
// Windows: exercises the Unix socket setup channel; its named-pipe counterpart
// is flotilla-org/jackstay#27.

use std::{
    os::{fd::IntoRawFd, unix::net::UnixStream},
    ptr,
};

use jackstay::{
    acquisition::arena::FrameDescriptor,
    ffi::*,
    ffi_acquisition::{producer::*, session::*, setup_server::*, *},
};

fn config() -> FtCpuProducerConfig {
    FtCpuProducerConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        max_incarnations: 2,
        payload_capacity: 4,
        memory_budget: 1024 * 1024,
        drain_timeout_ns: 5_000_000_000,
    }
}

#[test]
fn generic_c_setup_publishes_without_a_daemon_and_preserves_frames_after_cancellation() {
    // SAFETY: owned, disjoint C handles; connected sockets belong to this
    // process, and borrowed frame bytes are read only while the frame is held.
    unsafe {
        let mut producer = ptr::null_mut();
        assert_eq!(ft_cpu_producer_create(&config(), &mut producer), FT_STATUS_OK);
        let (server, client) = UnixStream::pair().unwrap();
        let mut server_fd = server.into_raw_fd();
        let mut client_fd = client.into_raw_fd();
        let mut setup = ptr::null_mut();
        let mut connection = ptr::null_mut();
        let mut consumer = ptr::null_mut();
        assert_eq!(ft_cpu_producer_serve(producer, &mut server_fd, &mut setup), FT_STATUS_OK);
        assert_eq!(server_fd, -1);
        assert_eq!(ft_acquisition_cpu_connection_create(&mut client_fd, &mut connection), FT_STATUS_OK);
        assert_eq!(client_fd, -1);
        assert_eq!(ft_acquisition_cpu_attach(connection, 2, &mut consumer), FT_STATUS_OK);
        let descriptor = FrameDescriptor {
            width: 1,
            height: 1,
            stride: 4,
            pixel_format: FT_PIXEL_FORMAT_BGRA8_UNORM,
            ..Default::default()
        };
        let mut cursor = 0;
        assert_eq!(
            ft_cpu_producer_publish(producer, &descriptor, b"held".as_ptr(), 4, &mut cursor),
            FT_STATUS_OK
        );
        let mut frame = ptr::null_mut();
        let mut range = FtAcquisitionRange::default();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut frame, &mut range),
            FT_STATUS_OK
        );
        assert_eq!(ft_cpu_setup_server_destroy(&mut setup), FT_STATUS_CANCELLED);
        assert!(setup.is_null());
        let mut next = ptr::null_mut();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, cursor, &mut next, &mut range),
            FT_STATUS_CLOSED
        );
        ft_acquisition_consumer_destroy(&mut consumer);
        ft_acquisition_cpu_connection_destroy(&mut connection);
        assert_eq!(ft_cpu_producer_destroy(&mut producer), FT_STATUS_DRAINING);
        let mut data = ptr::null();
        let mut len = 0;
        assert_eq!(ft_acquired_frame_bytes(frame, &mut data, &mut len), FT_STATUS_OK);
        assert_eq!(std::slice::from_raw_parts(data, len), b"held");
        assert_eq!(ft_acquired_frame_release(&mut frame), FT_STATUS_OK);
        assert_eq!(ft_cpu_producer_destroy(&mut producer), FT_STATUS_OK);
    }
}

#[test]
fn client_cancel_interrupts_attach_waiting_for_a_response() {
    use std::{io::Read, sync::mpsc, time::Duration};
    // SAFETY: the handle stays alive until both threads return. Only cancellation
    // overlaps attach. The silent peer sends no grants or malformed resources.
    unsafe {
        let (mut peer, client) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut fd = client.into_raw_fd();
        let mut connection = ptr::null_mut();
        assert_eq!(ft_acquisition_cpu_connection_create(&mut fd, &mut connection), FT_STATUS_OK);
        let address = connection as usize;
        let (done, result) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut consumer = ptr::null_mut();
            let status = ft_acquisition_cpu_attach(address as *mut FtCpuAcquisitionConnection, 1, &mut consumer);
            done.send((status, consumer.is_null())).unwrap();
        });
        // Wait for real request traffic, so this tests blocked I/O, not merely
        // the pre-call cancellation check.
        peer.read_exact(&mut [0; 4]).unwrap();
        ft_acquisition_cpu_connection_cancel(connection);
        assert_eq!(result.recv_timeout(Duration::from_secs(5)).unwrap(), (FT_STATUS_CANCELLED, true));
        worker.join().unwrap();
        ft_acquisition_cpu_connection_destroy(&mut connection);
        assert!(connection.is_null());
    }
}

#[test]
fn server_cancellation_interrupts_partial_setup_and_releases_its_producer_owner() {
    use std::io::Write;
    // SAFETY: all handles are disjoint owned outputs, destroyed once. A partial
    // request is deliberate; no grants are received or forwarded.
    unsafe {
        let mut producer = ptr::null_mut();
        assert_eq!(ft_cpu_producer_create(&config(), &mut producer), FT_STATUS_OK);
        let (server, mut client) = UnixStream::pair().unwrap();
        let mut fd = server.into_raw_fd();
        let mut setup = ptr::null_mut();
        assert_eq!(ft_cpu_producer_serve(producer, &mut fd, &mut setup), FT_STATUS_OK);
        client.write_all(&[0]).unwrap();
        assert_eq!(ft_cpu_setup_server_poll(setup), FT_STATUS_DRAINING);
        assert_eq!(ft_cpu_producer_destroy(&mut producer), FT_STATUS_DRAINING);
        ft_cpu_setup_server_cancel(setup);
        assert_eq!(ft_cpu_setup_server_destroy(&mut setup), FT_STATUS_CANCELLED);
        assert_eq!(ft_cpu_producer_destroy(&mut producer), FT_STATUS_OK);
    }
}

#[test]
fn fd_transfer_distinguishes_argument_rejection_from_socket_validation_failure() {
    use std::fs::File;
    // SAFETY: valid socket/file descriptors are owned and outputs start null.
    unsafe {
        let (_peer, client) = UnixStream::pair().unwrap();
        let mut fd = client.into_raw_fd();
        let original = fd;
        assert_eq!(
            ft_acquisition_cpu_connection_create(&mut fd, ptr::null_mut()),
            FT_STATUS_INVALID_ARGUMENT
        );
        assert_eq!(fd, original);
        let mut connection = ptr::null_mut();
        assert_eq!(ft_acquisition_cpu_connection_create(&mut fd, &mut connection), FT_STATUS_OK);
        assert_eq!(fd, -1);
        let mut consumer = ptr::null_mut();
        assert_eq!(ft_acquisition_cpu_attach(connection, 0, &mut consumer), FT_STATUS_INVALID_ARGUMENT);
        assert!(consumer.is_null());
        ft_acquisition_cpu_connection_destroy(&mut connection);
        let mut fd = File::open("/dev/null").unwrap().into_raw_fd();
        assert_eq!(ft_acquisition_cpu_connection_create(&mut fd, &mut connection), FT_STATUS_ERROR);
        assert_eq!(fd, -1);
        assert!(connection.is_null());
        assert_eq!(
            ft_acquisition_cpu_connection_create(&mut fd, &mut connection),
            FT_STATUS_INVALID_ARGUMENT
        );
    }
}

#[test]
fn server_reports_orderly_eof_separately_from_protocol_failure() {
    use std::{
        io::Write,
        time::{Duration, Instant},
    };
    for malformed in [false, true] {
        // SAFETY: owned handles and descriptors; no grants requested or imported.
        unsafe {
            let mut producer = ptr::null_mut();
            assert_eq!(ft_cpu_producer_create(&config(), &mut producer), FT_STATUS_OK);
            let (server, mut client) = UnixStream::pair().unwrap();
            let mut fd = server.into_raw_fd();
            let mut setup = ptr::null_mut();
            assert_eq!(ft_cpu_producer_serve(producer, &mut fd, &mut setup), FT_STATUS_OK);
            if malformed {
                client.write_all(&[0]).unwrap();
            }
            client.shutdown(std::net::Shutdown::Write).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let status = loop {
                let status = ft_cpu_setup_server_poll(setup);
                if status != FT_STATUS_DRAINING {
                    break status;
                }
                assert!(Instant::now() < deadline, "worker did not finish");
                std::thread::yield_now();
            };
            assert_eq!(status, if malformed { FT_STATUS_ERROR } else { FT_STATUS_OK });
            assert_eq!(ft_cpu_setup_server_poll(setup), status);
            assert_eq!(ft_cpu_setup_server_destroy(&mut setup), status);
            assert_eq!(ft_cpu_producer_destroy(&mut producer), FT_STATUS_OK);
        }
    }
}
