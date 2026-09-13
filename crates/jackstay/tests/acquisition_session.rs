#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::{
    ffi::CString,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    ptr,
    sync::{Arc, Mutex},
    time::Duration,
};

use jackstay::{
    acquisition::{
        arena::{AcquireOutcome, ArenaConfig, ArenaProducer, FrameDescriptor},
        socket::serve_cpu,
    },
    daemon::{ConnectedSession, SessionInfo},
    ffi::*,
    ffi_acquisition::{session::*, *},
    model::PixelFormat,
};

struct Host {
    _directory: tempfile::TempDir,
    producer: Arc<Mutex<ArenaProducer>>,
    info: SessionInfo,
    task: std::thread::JoinHandle<()>,
}

impl Host {
    fn new(accept: bool) -> Self {
        // Keep sockaddr_un paths short even under macOS's long default TMPDIR.
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let socket = directory.path().join("setup");
        let listener = UnixListener::bind(&socket).unwrap();
        let producer = Arc::new(Mutex::new(
            ArenaProducer::new(ArenaConfig {
                resource_capacity: 6,
                retained_history: 2,
                producer_reserve: 1,
                payload_capacity: 4,
                memory_budget: 1024 * 1024,
                max_incarnations: 2,
                drain_timeout: Duration::from_secs(5),
            })
            .unwrap(),
        ));
        let served = producer.clone();
        let task = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut line = String::new();
            BufReader::with_capacity(1, &mut stream).read_line(&mut line).unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(
                request,
                serde_json::json!({
                    "op": "open_cpu_acquisition", "session_id": "test-session",
                    "track_id": 7, "bearer_token": "test-token",
                })
            );
            if accept {
                writeln!(stream, "{{\"op\":\"cpu_opened\"}}").unwrap();
                serve_cpu(stream, served).unwrap();
            } else {
                writeln!(stream, "{{\"op\":\"rejected\",\"message\":\"session access denied\"}}").unwrap();
            }
        });
        Self {
            info: SessionInfo {
                session_id: "test-session".into(),
                source_id: 1,
                track_id: 7,
                width: 1,
                height: 1,
                stride: 4,
                pixel_format: PixelFormat::Bgra8Unorm,
                fd_socket_path: socket.to_str().unwrap().into(),
                bearer_token: Some("test-token".into()),
            },
            _directory: directory,
            producer,
            task,
        }
    }

    fn publish(&self, bytes: &[u8]) {
        self.producer.lock().unwrap().publish(FrameDescriptor::default(), bytes).unwrap();
    }
}

#[test]
fn connected_session_keeps_a_lease_after_history_wrap_and_both_api_owners_drop() {
    let host = Host::new(true);
    host.publish(b"held");
    // SAFETY: our conforming producer; this process never forks or forwards maps.
    let session = unsafe { ConnectedSession::connect(host.info.clone(), 1) }.unwrap();
    let AcquireOutcome::Frame(frame) = session.consumer.acquire_latest(0).unwrap() else {
        panic!("no frame")
    };
    let descriptor = *frame.descriptor();
    for _ in 0..100 {
        host.publish(b"next");
    }
    assert!(matches!(session.consumer.acquire_latest(0).unwrap(), AcquireOutcome::HoldingLimit));
    drop(session);
    host.task.join().unwrap();
    assert_eq!(frame.bytes(), b"held");
    assert_eq!(frame.descriptor(), &descriptor);
    host.producer.lock().unwrap().stop();
    assert!(!host.producer.lock().unwrap().poll_shutdown_ready().unwrap());
    drop(frame);
    assert!(host.producer.lock().unwrap().poll_shutdown_ready().unwrap());
}

#[test]
fn rejection_does_not_admit_a_consumer_or_hide_the_host_error() {
    let host = Host::new(false);
    // SAFETY: our host rejects before transferring any mappings.
    let error = unsafe { ConnectedSession::connect(host.info.clone(), 1) }
        .err()
        .expect("accepted rejected session");
    assert!(error.to_string().contains("session access denied"));
    host.task.join().unwrap();
    host.producer.lock().unwrap().stop();
    assert!(host.producer.lock().unwrap().poll_shutdown_ready().unwrap());
}

#[test]
fn c_session_replacement_and_frame_ownership_use_the_common_arena() {
    let host = Host::new(true);
    let control_path = host._directory.path().join("control");
    let control = UnixListener::bind(&control_path).unwrap();
    let info = host.info.clone();
    let http = std::thread::spawn(move || {
        let (mut stream, _) = control.accept().unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        assert_eq!(request, "GET /capture-sessions/test-session HTTP/1.1\r\n");
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
        }
        let body = serde_json::json!({
            "session_id": info.session_id, "source_id": 1, "track_id": 7,
            "width": 1, "height": 1, "stride": 4, "pixel_format": "bgra8_unorm",
            "fd_socket_path": info.fd_socket_path,
        })
        .to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    host.publish(b"held");
    let path = CString::new(control_path.to_str().unwrap()).unwrap();
    let mut connection = ptr::null_mut();
    let mut consumer = ptr::null_mut();
    let mut track = 0;
    // SAFETY: exclusive handles and correctly sized outputs; conforming test
    // host with no fork/forward/replay. Byte borrows never outlive their frame.
    unsafe {
        assert_eq!(
            ft_acquisition_cpu_connect_session(
                path.as_ptr(),
                c"test-session".as_ptr(),
                c"test-token".as_ptr(),
                2,
                &mut connection,
                &mut consumer,
                &mut track
            ),
            FT_STATUS_OK
        );
        assert_eq!(track, 7);
        let mut old = ptr::null_mut();
        let mut new = ptr::null_mut();
        let mut range = FtAcquisitionRange::default();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut old, &mut range),
            FT_STATUS_OK
        );
        host.producer.lock().unwrap().reconfigure_cpu(8).unwrap();
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut new, &mut range),
            FT_STATUS_RECONFIGURATION
        );
        assert_eq!(ft_acquisition_cpu_install_configuration(connection, consumer), FT_STATUS_OK);
        assert_eq!(ft_acquisition_cpu_install_configuration(connection, consumer), FT_STATUS_EMPTY);
        host.publish(b"new size");
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut new, &mut range),
            FT_STATUS_OK
        );
        ft_acquisition_consumer_destroy(&mut consumer);
        ft_acquisition_cpu_connection_destroy(&mut connection);
        assert!(consumer.is_null() && connection.is_null());
        host.task.join().unwrap();
        for (frame, expected) in [(old, b"held".as_slice()), (new, b"new size".as_slice())] {
            let mut data = ptr::null();
            let mut len = 0;
            assert_eq!(ft_acquired_frame_bytes(frame, &mut data, &mut len), FT_STATUS_OK);
            assert_eq!(std::slice::from_raw_parts(data, len), expected);
        }
        host.producer.lock().unwrap().stop();
        assert!(!host.producer.lock().unwrap().poll_shutdown_ready().unwrap());
        assert_eq!(ft_acquired_frame_release(&mut old), FT_STATUS_OK);
        assert_eq!(ft_acquired_frame_release(&mut new), FT_STATUS_OK);
        assert!(host.producer.lock().unwrap().poll_shutdown_ready().unwrap());
    }
    http.join().unwrap();
}
