//! A CPU producer and a consumer in separate processes, connected only through
//! the C ABI's Local Endpoint entry points (named pipes on Windows, Unix
//! sockets elsewhere): frames and input across resize, setup cancellation, and
//! killing either process, with cleanup verified on the surviving side.

use std::{
    ffi::CString,
    process::Command,
    ptr, thread,
    time::{Duration, Instant},
};

use jackstay::{
    acquisition::arena::FrameDescriptor,
    ffi::*,
    ffi_acquisition::{producer::*, session::*, setup_server::*, *},
    ffi_bootstrap::*,
    ffi_input::*,
    ffi_local::*,
};
use serde::{Deserialize, Serialize};

#[path = "support/child.rs"]
mod child;
#[path = "support/setup.rs"]
mod setup;

const DEADLINE: Duration = Duration::from_secs(10);
const COOPERATIVE: u32 = 4;

#[derive(Debug, Serialize, Deserialize, PartialEq)]
enum Report {
    Server { pid: u32, user: String },
    Frame(Vec<u8>),
    Typed,
    Held { old: Vec<u8>, new: Vec<u8> },
    Closed,
    Pressed,
    Listening,
    Published,
}

fn unique(prefix: &str) -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{prefix}-{}-{nonce}", std::process::id())
}

fn endpoint(name: &CString) -> FtLocalEndpoint {
    FtLocalEndpoint {
        scope: FT_ENDPOINT_SCOPE_USER,
        transport: FT_ENDPOINT_TRANSPORT_LOCAL_STREAM,
        name: name.as_ptr(),
    }
}

fn user(identity: &FtPeerIdentity) -> String {
    // SAFETY: the library NUL-terminates the fixed buffer.
    unsafe { std::ffi::CStr::from_ptr(identity.user.as_ptr()) }
        .to_str()
        .unwrap()
        .to_owned()
}

fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(2));
    }
}

/// Handles cross threads here only for one owner at a time.
#[derive(Clone, Copy)]
struct Raw(usize);

fn producer_config() -> FtCpuProducerConfig {
    FtCpuProducerConfig {
        resource_capacity: 6,
        retained_history: 2,
        producer_reserve: 1,
        max_incarnations: 2,
        payload_capacity: 8,
        memory_budget: 1024 * 1024,
        drain_timeout_ns: 5_000_000_000,
    }
}

/// # Safety
/// `producer` is a live, exclusively used producer handle.
unsafe fn publish(producer: *mut FtCpuProducer, bytes: &[u8]) -> u64 {
    let descriptor = FrameDescriptor {
        width: (bytes.len() / 4) as u32,
        height: 1,
        stride: bytes.len() as u32,
        pixel_format: FT_PIXEL_FORMAT_BGRA8_UNORM,
        ..Default::default()
    };
    let mut cursor = 0;
    // SAFETY: caller's live producer; descriptor and bytes are live and disjoint.
    assert_eq!(
        unsafe { ft_cpu_producer_publish(producer, &descriptor, bytes.as_ptr(), bytes.len(), &mut cursor) },
        FT_STATUS_OK
    );
    cursor
}

/// Wait for a frame after `after`, installing a replacement configuration when
/// acquisition reports one.
///
/// # Safety
/// Live, exclusively used consumer and setup connection handles.
unsafe fn acquire_after(consumer: *mut FtAcquisitionConsumer, setup: *mut FtCpuAcquisitionConnection, after: u64) -> *mut FtAcquiredFrame {
    let mut frame = ptr::null_mut();
    until("a frame", || {
        let mut range = FtAcquisitionRange::default();
        // SAFETY: live consumer; frame starts null each attempt that returns non-OK.
        match unsafe { ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, after, &mut frame, &mut range) } {
            FT_STATUS_OK => true,
            FT_STATUS_RECONFIGURATION => {
                // SAFETY: matching live setup connection and consumer.
                let installed = unsafe { ft_acquisition_cpu_install_configuration(setup, consumer) };
                assert!(matches!(installed, FT_STATUS_OK | FT_STATUS_EMPTY), "install: {installed}");
                false
            }
            FT_STATUS_EMPTY => false,
            other => panic!("acquire: {other}"),
        }
    });
    frame
}

/// # Safety
/// A live frame handle.
unsafe fn bytes(frame: *const FtAcquiredFrame) -> (u64, Vec<u8>) {
    let mut descriptor = FrameDescriptor::default();
    let (mut data, mut len) = (ptr::null(), 0);
    // SAFETY: live frame; outputs are disjoint locals.
    unsafe {
        assert_eq!(ft_acquired_frame_describe(frame, &mut descriptor), FT_STATUS_OK);
        assert_eq!(ft_acquired_frame_bytes(frame, &mut data, &mut len), FT_STATUS_OK);
        (descriptor.cursor, std::slice::from_raw_parts(data, len).to_vec())
    }
}

/// Send one event and wait for its execution result.
///
/// # Safety
/// A live input client handle.
unsafe fn submit(client: *mut FtInputClient, event: &FtInputEvent) {
    let mut sequence = 0;
    // SAFETY: live client; the event and its borrowed text are live for the call.
    assert_eq!(unsafe { ft_input_client_send(client, event, &mut sequence) }, FT_STATUS_OK);
    let mut status = FtInputStatus::default();
    // SAFETY: live client and writable status.
    until(
        "an input completion",
        || unsafe { ft_input_client_poll(client, &mut status) } == FT_STATUS_OK,
    );
    assert_eq!(
        (status.kind, status.sequence, status.result),
        (1, sequence, 0),
        "input was not executed"
    );
}

/// Complete available input work; returns the operations seen.
///
/// # Safety
/// A live input target handle.
unsafe fn pump(target: *mut FtInputTarget) -> Vec<FtInputOperation> {
    let mut seen = Vec::new();
    loop {
        let mut work = ptr::null_mut();
        // SAFETY: live target; work starts null.
        if unsafe { ft_input_target_next(target, &mut work) } != FT_STATUS_OK {
            return seen;
        }
        let mut operation = FtInputOperation::default();
        // SAFETY: work is the live item just taken; complete consumes it.
        unsafe {
            assert_eq!(ft_input_work_describe(work, &mut operation), FT_STATUS_OK);
            // Describe borrows text only until completion; keep just the metadata.
            operation.event.text = ptr::null();
            assert_eq!(ft_input_work_complete(&mut work, 0), FT_STATUS_OK);
        }
        seen.push(operation);
    }
}

fn text_event(text: &'static str) -> FtInputEvent {
    FtInputEvent {
        kind: 2,
        text: text.as_ptr(),
        text_len: text.len(),
        ..Default::default()
    }
}

fn key_down(press: u64, name: &str) -> FtInputEvent {
    let mut event = FtInputEvent {
        kind: 1,
        action: 1,
        key_kind: 1,
        press,
        ..Default::default()
    };
    for (slot, byte) in event.key.iter_mut().zip(name.bytes()) {
        *slot = byte as std::ffi::c_char;
    }
    event
}

fn my_identity() -> String {
    let name = CString::new(unique("identity")).unwrap();
    let endpoint = endpoint(&name);
    // SAFETY: owned handles in this process only, destroyed below.
    unsafe {
        let mut listener = ptr::null_mut();
        assert_eq!(ft_local_listener_create(&endpoint, &mut listener), FT_STATUS_OK);
        let address = Raw(listener as usize);
        let accepting = thread::spawn(move || {
            let mut connection = ptr::null_mut();
            assert_eq!(
                ft_local_listener_accept(address.0 as *const FtLocalListener, &mut connection),
                FT_STATUS_OK
            );
            Raw(connection as usize)
        });
        let mut connection = ptr::null_mut();
        assert_eq!(ft_local_connect(&endpoint, &mut connection), FT_STATUS_OK);
        let mut identity = FtPeerIdentity::default();
        assert_eq!(ft_local_connection_peer(connection, &mut identity), FT_STATUS_OK);
        let mut accepted = accepting.join().unwrap().0 as *mut FtLocalConnection;
        ft_local_connection_destroy(&mut accepted);
        ft_local_connection_destroy(&mut connection);
        ft_local_listener_destroy(&mut listener);
        user(&identity)
    }
}

#[test]
fn a_killed_consumer_process_is_cleaned_up_after_resize_input_and_cancellation() {
    let control = setup::Listener::bind("local-ffi-consumer");
    let name = CString::new(unique("local-ffi-consumer")).unwrap();
    let endpoint = endpoint(&name);
    let me = my_identity();
    // SAFETY: every handle is created, used and destroyed by this test, once.
    unsafe {
        let mut producer = ptr::null_mut();
        assert_eq!(ft_cpu_producer_create(&producer_config(), &mut producer), FT_STATUS_OK);
        let mut config = FtInputConfig::default();
        ft_input_config_default(&mut config);
        let mut target = ptr::null_mut();
        assert_eq!(ft_input_target_create(&config, &mut target), FT_STATUS_OK);
        let mut listener = ptr::null_mut();
        assert_eq!(ft_local_listener_create(&endpoint, &mut listener), FT_STATUS_OK);
        // A second listener cannot take over the live endpoint.
        let mut squatter = ptr::null_mut();
        assert_eq!(ft_local_listener_create(&endpoint, &mut squatter), FT_STATUS_ADDRESS_IN_USE);
        assert!(squatter.is_null());

        let mut child = child::KillOnDrop(
            Command::new(std::env::current_exe().unwrap())
                .args(["--ignored", "--exact", "local_ffi_consumer_child", "--nocapture"])
                .env("JACKSTAY_LOCAL_FFI_CONTROL", control.address())
                .env("JACKSTAY_LOCAL_FFI_ENDPOINT", name.to_str().unwrap())
                .spawn()
                .unwrap(),
        );
        // Accept on a worker so a lost child cancels instead of hanging.
        // The watchdog is stopped and joined as soon as accept returns, so it
        // can never touch the listener after destruction.
        let address = Raw(listener as usize);
        let (accepted, stopped) = std::sync::mpsc::channel::<()>();
        let watchdog = thread::spawn(move || {
            if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(DEADLINE) {
                ft_local_listener_cancel(address.0 as *const FtLocalListener);
            }
        });
        let mut connection = ptr::null_mut();
        let status = ft_local_listener_accept(listener, &mut connection);
        drop(accepted);
        watchdog.join().unwrap();
        assert_eq!(status, FT_STATUS_OK);
        let mut peer = FtPeerIdentity::default();
        assert_eq!(ft_local_connection_peer(connection, &mut peer), FT_STATUS_OK);
        assert_eq!(peer.pid, child.id(), "accept must report the kernel's peer");
        assert_eq!(user(&peer), me);
        assert_eq!(peer.has_session, u32::from(cfg!(windows)));
        let mut link = control.accept();
        assert_eq!(
            link.recv::<Report>(),
            Report::Server {
                pid: std::process::id(),
                user: me.clone()
            },
            "the client must verify this server"
        );

        let mut input_server = ptr::null_mut();
        assert_eq!(
            ft_source_bootstrap_accept_local(&mut connection, target, &mut input_server),
            FT_STATUS_OK
        );
        assert!(!input_server.is_null());
        let mut setup = ptr::null_mut();
        assert_eq!(ft_cpu_producer_serve_local(producer, &mut connection, &mut setup), FT_STATUS_OK);
        assert!(connection.is_null());

        publish(producer, b"frameA__");
        assert_eq!(link.recv::<Report>(), Report::Frame(b"frameA__".to_vec()));
        let mut typed = Vec::new();
        until("typed input", || {
            typed.extend(pump(target));
            !typed.is_empty()
        });
        assert_eq!(typed[0].event.kind, 2);
        assert_eq!(link.recv::<Report>(), Report::Typed);

        // Resize: a larger allocation while the consumer holds the old frame.
        let mut transition = FtCpuReconfiguration::default();
        assert_eq!(ft_cpu_producer_reconfigure(producer, 16, &mut transition), FT_STATUS_OK);
        publish(producer, b"frameB__resized_");
        link.write_byte(1);
        assert_eq!(
            link.recv::<Report>(),
            Report::Held {
                old: b"frameA__".to_vec(),
                new: b"frameB__resized_".to_vec()
            }
        );

        // Cancel setup from the host; frames and input stay independent.
        ft_cpu_setup_server_cancel(setup);
        assert_eq!(ft_cpu_setup_server_destroy(&mut setup), FT_STATUS_CANCELLED);
        link.write_byte(2);
        assert_eq!(link.recv::<Report>(), Report::Closed);
        until("the held key", || pump(target).iter().any(|operation| operation.event.kind == 1));
        assert_eq!(link.recv::<Report>(), Report::Pressed);

        // The consumer holds two frames, a claim on a replaced allocation and a
        // pressed key. Kill it without any cleanup of its own.
        assert_eq!(ft_cpu_producer_destroy(&mut producer), FT_STATUS_DRAINING);
        child.kill().unwrap();
        child.wait().unwrap();

        // Input: the target runs the controller's cleanup, then can retire.
        let mut cleaned = false;
        until("input cleanup after peer death", || {
            cleaned |= pump(target).iter().any(|operation| operation.reason == 3);
            cleaned && ft_input_server_poll(input_server) == FT_STATUS_OK
        });
        ft_input_server_destroy(&mut input_server);
        until("input target retirement", || {
            pump(target);
            ft_input_target_destroy(&mut target) == FT_STATUS_OK
        });
        // Media: the process watch reclaims the dead consumer's claims.
        until("producer retirement after consumer death", || {
            ft_cpu_producer_poll_cleanup(producer);
            ft_cpu_producer_destroy(&mut producer) == FT_STATUS_OK
        });
        assert!(producer.is_null());

        ft_local_listener_destroy(&mut listener);
    }
}

#[test]
#[ignore = "subprocess helper for a_killed_consumer_process_is_cleaned_up_after_resize_input_and_cancellation"]
fn local_ffi_consumer_child() {
    let mut link = setup::Link::connect(&std::env::var("JACKSTAY_LOCAL_FFI_CONTROL").unwrap());
    let name = CString::new(std::env::var("JACKSTAY_LOCAL_FFI_ENDPOINT").unwrap()).unwrap();
    // SAFETY: this process owns every handle; frames are read while held.
    unsafe {
        let mut connection = ptr::null_mut();
        assert_eq!(ft_local_connect(&endpoint(&name), &mut connection), FT_STATUS_OK);
        let mut server = FtPeerIdentity::default();
        assert_eq!(ft_local_connection_peer(connection, &mut server), FT_STATUS_OK);
        link.send(&Report::Server {
            pid: server.pid,
            user: user(&server),
        });
        let (mut input, mut input_status) = (ptr::null_mut(), FT_STATUS_ERROR);
        assert_eq!(
            ft_source_bootstrap_connect_local(
                &mut connection,
                FT_BOOTSTRAP_INPUT_REQUIRED,
                COOPERATIVE,
                &mut input,
                &mut input_status
            ),
            FT_STATUS_OK
        );
        assert_eq!(input_status, FT_STATUS_OK);
        let mut setup = ptr::null_mut();
        assert_eq!(
            ft_acquisition_cpu_connection_create_local(&mut connection, &mut setup),
            FT_STATUS_OK
        );
        let mut consumer = ptr::null_mut();
        assert_eq!(ft_acquisition_cpu_attach(setup, 2, &mut consumer), FT_STATUS_OK);

        let old = acquire_after(consumer, setup, 0);
        let (old_cursor, old_bytes) = bytes(old);
        link.send(&Report::Frame(old_bytes.clone()));
        submit(input, &text_event("typed"));
        link.send(&Report::Typed);

        assert_eq!(link.read_byte(), 1);
        let new = acquire_after(consumer, setup, old_cursor);
        link.send(&Report::Held {
            old: bytes(old).1,
            new: bytes(new).1,
        });

        assert_eq!(link.read_byte(), 2);
        until("setup closure", || ft_acquisition_cpu_connection_alive(setup) == FT_STATUS_CLOSED);
        let (mut next, mut range) = (ptr::null_mut(), FtAcquisitionRange::default());
        assert_eq!(
            ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &mut next, &mut range),
            FT_STATUS_CLOSED
        );
        link.send(&Report::Closed);
        submit(input, &key_down(1, "KeyA"));
        link.send(&Report::Pressed);
        // Hold frames, claims and the pressed key until killed.
        loop {
            thread::park();
        }
    }
}

#[test]
fn a_killed_producer_process_is_observed_through_setup_liveness_and_input() {
    let control = setup::Listener::bind("local-ffi-producer");
    let name = CString::new(unique("local-ffi-producer")).unwrap();
    let mut child = child::KillOnDrop(
        Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "local_ffi_producer_child", "--nocapture"])
            .env("JACKSTAY_LOCAL_FFI_CONTROL", control.address())
            .env("JACKSTAY_LOCAL_FFI_ENDPOINT", name.to_str().unwrap())
            .spawn()
            .unwrap(),
    );
    let mut link = control.accept();
    assert_eq!(link.recv::<Report>(), Report::Listening);
    // SAFETY: this test owns every handle; frames are read while held.
    unsafe {
        let mut connection = ptr::null_mut();
        assert_eq!(ft_local_connect(&endpoint(&name), &mut connection), FT_STATUS_OK);
        let mut server = FtPeerIdentity::default();
        assert_eq!(ft_local_connection_peer(connection, &mut server), FT_STATUS_OK);
        assert_eq!(server.pid, child.id(), "connect must verify the actual server process");
        assert_eq!(ft_local_connection_alive(connection), FT_STATUS_OK);
        let (mut input, mut input_status) = (ptr::null_mut(), FT_STATUS_ERROR);
        assert_eq!(
            ft_source_bootstrap_connect_local(
                &mut connection,
                FT_BOOTSTRAP_INPUT_REQUIRED,
                COOPERATIVE,
                &mut input,
                &mut input_status
            ),
            FT_STATUS_OK
        );
        let mut setup = ptr::null_mut();
        assert_eq!(
            ft_acquisition_cpu_connection_create_local(&mut connection, &mut setup),
            FT_STATUS_OK
        );
        let mut consumer = ptr::null_mut();
        assert_eq!(ft_acquisition_cpu_attach(setup, 1, &mut consumer), FT_STATUS_OK);
        assert_eq!(link.recv::<Report>(), Report::Published);
        let mut frame = acquire_after(consumer, setup, 0);
        assert_eq!(bytes(frame).1, b"produced");
        submit(input, &text_event("to the producer"));
        assert_eq!(ft_acquisition_cpu_connection_alive(setup), FT_STATUS_OK);

        child.kill().unwrap();
        child.wait().unwrap();
        // Producer exit is a setup-channel event: liveness reports it, while the
        // held frame's mapping and bytes stay valid.
        until("producer loss", || ft_acquisition_cpu_connection_alive(setup) == FT_STATUS_CLOSED);
        assert_eq!(bytes(frame).1, b"produced");
        let mut status = FtInputStatus::default();
        until("input closure", || {
            ft_input_client_poll(input, &mut status) == FT_STATUS_OK && status.kind == 4
        });
        assert_eq!(status.clean, 0, "a dead executor cannot confirm cleanup");
        assert_eq!(ft_acquired_frame_release(&mut frame), FT_STATUS_OK);
        ft_acquisition_consumer_destroy(&mut consumer);
        ft_acquisition_cpu_connection_destroy(&mut setup);
        ft_input_client_destroy(&mut input);
    }
}

#[test]
#[ignore = "subprocess helper for a_killed_producer_process_is_observed_through_setup_liveness_and_input"]
fn local_ffi_producer_child() {
    let mut link = setup::Link::connect(&std::env::var("JACKSTAY_LOCAL_FFI_CONTROL").unwrap());
    let name = CString::new(std::env::var("JACKSTAY_LOCAL_FFI_ENDPOINT").unwrap()).unwrap();
    // SAFETY: this process owns every handle and is killed by its parent.
    unsafe {
        let mut producer = ptr::null_mut();
        assert_eq!(ft_cpu_producer_create(&producer_config(), &mut producer), FT_STATUS_OK);
        let mut config = FtInputConfig::default();
        ft_input_config_default(&mut config);
        let mut target = ptr::null_mut();
        assert_eq!(ft_input_target_create(&config, &mut target), FT_STATUS_OK);
        let mut listener = ptr::null_mut();
        assert_eq!(ft_local_listener_create(&endpoint(&name), &mut listener), FT_STATUS_OK);
        link.send(&Report::Listening);
        let mut connection = ptr::null_mut();
        assert_eq!(ft_local_listener_accept(listener, &mut connection), FT_STATUS_OK);
        let mut input_server = ptr::null_mut();
        assert_eq!(
            ft_source_bootstrap_accept_local(&mut connection, target, &mut input_server),
            FT_STATUS_OK
        );
        let mut setup = ptr::null_mut();
        assert_eq!(ft_cpu_producer_serve_local(producer, &mut connection, &mut setup), FT_STATUS_OK);
        publish(producer, b"produced");
        link.send(&Report::Published);
        loop {
            pump(target);
            thread::sleep(Duration::from_millis(2));
        }
    }
}
