//! Handshake v3 through the C ABI, on Local Endpoint connections on every
//! platform (named pipes on Windows).
use std::{
    ffi::{CStr, c_char},
    ptr, slice, thread,
    time::{Duration, Instant},
};

use jackstay::{
    ffi::*,
    ffi_handshake::*,
    ffi_input::*,
    ffi_local::{FtLocalConnection, ft_local_connection_destroy},
};

#[path = "support/local.rs"]
mod local;
use local::c_pair as local_pair;

/// Moves a C handle across threads in a test.
struct Owned<T>(T);
// SAFETY: test-only transfer of an exclusively owned handle to one thread.
unsafe impl<T> Send for Owned<T> {}

const DESCRIPTOR: &[u8] = b"opaque";

fn offer(id: u64, kind: &'static CStr, direction: u32, descriptor: &'static [u8]) -> FtTrackOffer {
    FtTrackOffer {
        id,
        kind: kind.as_ptr(),
        label: c"default".as_ptr(),
        descriptor: descriptor.as_ptr(),
        descriptor_len: descriptor.len(),
        direction,
        reserved: 0,
    }
}

fn admission(id: u64, result: u32) -> FtTrackAdmission {
    FtTrackAdmission { id, result, reserved: 0 }
}

/// Offer video 1, input 2 and an app track 7 on a host worker; it returns the
/// host status, admissions and input server.
fn host(
    connection: *mut FtLocalConnection,
    target: *mut FtInputTarget,
) -> thread::JoinHandle<(FtStatus, Vec<FtTrackAdmission>, Owned<*mut FtInputServer>)> {
    let (connection, target) = (Owned(connection), Owned(target));
    thread::spawn(move || {
        let (mut connection, target) = (connection, target);
        let tracks = [
            offer(1, c"video", FT_TRACK_FROM_SOURCE, b""),
            offer(2, c"input", FT_TRACK_TO_SOURCE, b""),
            offer(7, c"org.example.cue", FT_TRACK_FROM_SOURCE, DESCRIPTOR),
        ];
        let mut admissions = [FtTrackAdmission::default(); 3];
        let (mut count, mut server, mut affordances) = (0, ptr::null_mut(), ptr::null_mut());
        // SAFETY: exclusively owned connection; arrays and outputs are disjoint.
        let status = unsafe {
            ft_handshake_accept_local(
                &mut connection.0,
                c"C source".as_ptr(),
                tracks.as_ptr(),
                tracks.len(),
                target.0,
                admissions.as_mut_ptr(),
                &mut count,
                &mut server,
                &mut affordances,
            )
        };
        assert!(affordances.is_null());
        // SAFETY: null, or the returned live connection handle.
        unsafe { ft_local_connection_destroy(&mut connection.0) };
        (status, admissions[..count].to_vec(), Owned(server))
    })
}

struct Selected {
    status: FtStatus,
    admissions: Vec<FtTrackAdmission>,
    connection: *mut FtLocalConnection,
    input: *mut FtInputClient,
    input_status: FtStatus,
}

/// Connect, check the described offer, and select.
fn consume(connection: *mut FtLocalConnection, selection: &[FtTrackSelection]) -> Selected {
    let mut connection = connection;
    let mut handle = ptr::null_mut();
    // SAFETY: every handle is exclusively owned; outputs are disjoint and the
    // borrowed offer views are read before select consumes the offer.
    unsafe {
        assert_eq!(ft_handshake_connect_local(&mut connection, &mut handle), FT_STATUS_OK);
        assert!(connection.is_null());
        let (mut source, mut tracks, mut count): (*const c_char, *const FtTrackOffer, usize) = (ptr::null(), ptr::null(), 0);
        assert_eq!(
            ft_handshake_offer_describe(handle, &mut source, &mut tracks, &mut count),
            FT_STATUS_OK
        );
        assert_eq!(CStr::from_ptr(source), c"C source");
        let tracks = slice::from_raw_parts(tracks, count);
        let kinds: Vec<_> = tracks.iter().map(|t| (t.id, CStr::from_ptr(t.kind), t.direction)).collect();
        assert_eq!(
            kinds,
            [
                (1, c"video", FT_TRACK_FROM_SOURCE),
                (2, c"input", FT_TRACK_TO_SOURCE),
                (7, c"org.example.cue", FT_TRACK_FROM_SOURCE)
            ]
        );
        assert_eq!(slice::from_raw_parts(tracks[2].descriptor, tracks[2].descriptor_len), DESCRIPTOR);
        let mut admissions = vec![FtTrackAdmission::default(); selection.len()];
        let mut selected = Selected {
            status: FT_STATUS_ERROR,
            admissions: Vec::new(),
            connection: ptr::null_mut(),
            input: ptr::null_mut(),
            input_status: FT_STATUS_ERROR,
        };
        let mut affordances = ptr::null_mut();
        selected.status = ft_handshake_select(
            &mut handle,
            selection.as_ptr(),
            selection.len(),
            4,
            admissions.as_mut_ptr(),
            &mut selected.connection,
            &mut selected.input,
            &mut selected.input_status,
            &mut affordances,
        );
        assert!(handle.is_null() && affordances.is_null());
        selected.admissions = admissions;
        selected
    }
}

#[test]
fn layouts_match_the_sizes_the_c_header_pins() {
    use std::mem::size_of;

    use jackstay::ffi_acquisition::{FtCpuConfigurationDescriptor, FtCpuGrantDescriptor};
    assert_eq!(
        (size_of::<FtCpuGrantDescriptor>(), size_of::<FtCpuConfigurationDescriptor>()),
        (120, 88)
    );
    assert_eq!((size_of::<FtTrackSelection>(), size_of::<FtTrackAdmission>()), (16, 16));
    assert_eq!(size_of::<FtTrackOffer>(), 16 + 3 * size_of::<*const u8>() + size_of::<usize>());
}

#[test]
fn c_offer_selection_and_admission_deliver_required_input() {
    // SAFETY: every handle is exclusively owned, created and destroyed once;
    // the text buffer outlives the send that copies it.
    unsafe {
        let mut config = FtInputConfig::default();
        ft_input_config_default(&mut config);
        let mut target = ptr::null_mut();
        assert_eq!(ft_input_target_create(&config, &mut target), FT_STATUS_OK);
        let (host_end, peer) = local_pair();
        let worker = host(host_end, target);
        let selection = [
            FtTrackSelection {
                id: 1,
                required: 1,
                write: 0,
            },
            FtTrackSelection {
                id: 2,
                required: 1,
                write: 1,
            },
            FtTrackSelection {
                id: 7,
                required: 0,
                write: 0,
            },
        ];
        let mut selected = consume(peer, &selection);
        assert_eq!(selected.status, FT_STATUS_OK);
        let expected = vec![
            admission(1, FT_TRACK_ADMITTED),
            admission(2, FT_TRACK_ADMITTED),
            admission(7, FT_TRACK_ADMITTED),
        ];
        assert_eq!(selected.admissions, expected);
        assert_eq!(selected.input_status, FT_STATUS_OK);
        assert!(!selected.connection.is_null());
        let (status, admissions, mut server) = worker.join().unwrap();
        assert_eq!((status, admissions), (FT_STATUS_OK, expected));
        let text = "hello from C";
        let event = FtInputEvent {
            kind: 2,
            text: text.as_ptr(),
            text_len: text.len(),
            ..Default::default()
        };
        let mut sequence = 0;
        assert_eq!(ft_input_client_send(selected.input, &event, &mut sequence), FT_STATUS_OK);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut work = ptr::null_mut();
        while ft_input_target_next(target, &mut work) == FT_STATUS_EMPTY {
            assert!(Instant::now() < deadline, "no input arrived");
            thread::sleep(Duration::from_millis(2));
        }
        let mut operation = FtInputOperation::default();
        assert_eq!(ft_input_work_describe(work, &mut operation), FT_STATUS_OK);
        assert_eq!(
            slice::from_raw_parts(operation.event.text, operation.event.text_len),
            text.as_bytes()
        );
        assert_eq!(ft_input_work_complete(&mut work, 0), FT_STATUS_OK);
        let mut status = FtInputStatus::default();
        while ft_input_client_poll(selected.input, &mut status) == FT_STATUS_EMPTY {
            assert!(Instant::now() < deadline, "no completion arrived");
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!((status.kind, status.sequence, status.result), (1, sequence, 0));
        ft_input_client_destroy(&mut selected.input);
        ft_input_server_destroy(&mut server.0);
        ft_local_connection_destroy(&mut selected.connection);
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

#[test]
fn c_optional_refusals_are_reported_and_required_refusals_consume_setup() {
    // No input authority: optional input and an unknown track are refused,
    // and the media connection survives.
    let (host_end, peer) = local_pair();
    let worker = host(host_end, ptr::null_mut());
    let selection = [
        FtTrackSelection {
            id: 1,
            required: 1,
            write: 0,
        },
        FtTrackSelection {
            id: 2,
            required: 0,
            write: 1,
        },
        FtTrackSelection {
            id: 9,
            required: 0,
            write: 0,
        },
    ];
    let mut selected = consume(peer, &selection);
    let expected = vec![
        admission(1, FT_TRACK_ADMITTED),
        admission(2, FT_TRACK_REFUSED_UNAVAILABLE),
        admission(9, FT_TRACK_REFUSED_UNKNOWN),
    ];
    assert_eq!((selected.status, &selected.admissions), (FT_STATUS_OK, &expected));
    assert_eq!(selected.input_status, FT_STATUS_EMPTY);
    assert!(selected.input.is_null() && !selected.connection.is_null());
    assert_eq!(worker.join().unwrap().0, FT_STATUS_OK);
    // SAFETY: the returned live connection handle.
    unsafe { ft_local_connection_destroy(&mut selected.connection) };

    // Required input without authority closes setup on both sides.
    let (host_end, peer) = local_pair();
    let worker = host(host_end, ptr::null_mut());
    let selected = consume(
        peer,
        &[FtTrackSelection {
            id: 2,
            required: 1,
            write: 1,
        }],
    );
    assert_eq!(selected.status, FT_STATUS_UNSUPPORTED);
    assert!(selected.connection.is_null() && selected.input.is_null());
    let (status, admissions, server) = worker.join().unwrap();
    assert_eq!((status, admissions.len()), (FT_STATUS_UNSUPPORTED, 0));
    assert!(server.0.is_null());
}
