use jackstay::{ffi::*, ffi_affordances::*};
// ABI layout is a contract: compare sizes, alignments and every field offset
// against the independently compiled C translation unit on native-feature CI.
#[cfg(any(
    all(target_os = "linux", feature = "backend-linux"),
    all(target_os = "macos", feature = "backend-macos"),
    windows
))]
#[test]
fn c_and_rust_affordances_layouts_agree() {
    unsafe extern "C" {
        fn jackstay_c_affordances_layout(index: u32) -> usize;
    }
    let expected = [
        std::mem::size_of::<FtAffString>(),
        std::mem::align_of::<FtAffString>(),
        std::mem::offset_of!(FtAffString, data),
        std::mem::offset_of!(FtAffString, len),
        std::mem::size_of::<FtAffOptionalString>(),
        std::mem::align_of::<FtAffOptionalString>(),
        std::mem::offset_of!(FtAffOptionalString, present),
        std::mem::offset_of!(FtAffOptionalString, value),
        std::mem::size_of::<FtAffOptionalNumber>(),
        std::mem::align_of::<FtAffOptionalNumber>(),
        std::mem::offset_of!(FtAffOptionalNumber, present),
        std::mem::offset_of!(FtAffOptionalNumber, value),
        std::mem::size_of::<FtAffSize>(),
        std::mem::align_of::<FtAffSize>(),
        std::mem::offset_of!(FtAffSize, present),
        std::mem::offset_of!(FtAffSize, width),
        std::mem::offset_of!(FtAffSize, height),
        std::mem::size_of::<FtAffArtwork>(),
        std::mem::align_of::<FtAffArtwork>(),
        std::mem::offset_of!(FtAffArtwork, kind),
        std::mem::offset_of!(FtAffArtwork, value),
        std::mem::size_of::<FtAffMedia>(),
        std::mem::align_of::<FtAffMedia>(),
        std::mem::offset_of!(FtAffMedia, status),
        std::mem::offset_of!(FtAffMedia, position),
        std::mem::offset_of!(FtAffMedia, rate),
        std::mem::offset_of!(FtAffMedia, duration),
        std::mem::offset_of!(FtAffMedia, title),
        std::mem::offset_of!(FtAffMedia, artwork),
        std::mem::offset_of!(FtAffMedia, capabilities),
        std::mem::size_of::<FtAffNavigation>(),
        std::mem::align_of::<FtAffNavigation>(),
        std::mem::offset_of!(FtAffNavigation, url),
        std::mem::offset_of!(FtAffNavigation, title),
        std::mem::offset_of!(FtAffNavigation, can_go_back),
        std::mem::offset_of!(FtAffNavigation, can_go_forward),
        std::mem::offset_of!(FtAffNavigation, loading),
        std::mem::offset_of!(FtAffNavigation, capabilities),
        std::mem::size_of::<FtAffAxis>(),
        std::mem::align_of::<FtAffAxis>(),
        std::mem::offset_of!(FtAffAxis, scrollable),
        std::mem::offset_of!(FtAffAxis, content_length),
        std::mem::offset_of!(FtAffAxis, viewport_length),
        std::mem::offset_of!(FtAffAxis, position),
        std::mem::size_of::<FtAffScroll>(),
        std::mem::align_of::<FtAffScroll>(),
        std::mem::offset_of!(FtAffScroll, x),
        std::mem::offset_of!(FtAffScroll, y),
        std::mem::offset_of!(FtAffScroll, capabilities),
        std::mem::size_of::<FtAffWindow>(),
        std::mem::align_of::<FtAffWindow>(),
        std::mem::offset_of!(FtAffWindow, title),
        std::mem::offset_of!(FtAffWindow, requested_size),
        std::mem::offset_of!(FtAffWindow, ready),
        std::mem::size_of::<FtAffPresentation>(),
        std::mem::align_of::<FtAffPresentation>(),
        std::mem::offset_of!(FtAffPresentation, visible),
        std::mem::offset_of!(FtAffPresentation, preferred_size),
        std::mem::offset_of!(FtAffPresentation, scale),
        std::mem::offset_of!(FtAffPresentation, focused),
        std::mem::size_of::<FtAffSnapshot>(),
        std::mem::align_of::<FtAffSnapshot>(),
        std::mem::offset_of!(FtAffSnapshot, domain),
        std::mem::offset_of!(FtAffSnapshot, withdrawn),
        std::mem::offset_of!(FtAffSnapshot, media),
        std::mem::offset_of!(FtAffSnapshot, navigation),
        std::mem::offset_of!(FtAffSnapshot, cursor),
        std::mem::offset_of!(FtAffSnapshot, scroll),
        std::mem::offset_of!(FtAffSnapshot, window),
        std::mem::offset_of!(FtAffSnapshot, presentation),
        std::mem::size_of::<FtAffVerb>(),
        std::mem::align_of::<FtAffVerb>(),
        std::mem::offset_of!(FtAffVerb, domain),
        std::mem::offset_of!(FtAffVerb, verb),
        std::mem::offset_of!(FtAffVerb, number),
        std::mem::offset_of!(FtAffVerb, url),
        std::mem::offset_of!(FtAffVerb, axis),
        std::mem::offset_of!(FtAffVerb, step),
        std::mem::offset_of!(FtAffVerb, direction),
        std::mem::size_of::<FtAffEventView>(),
        std::mem::align_of::<FtAffEventView>(),
        std::mem::offset_of!(FtAffEventView, kind),
        std::mem::offset_of!(FtAffEventView, snapshot),
        std::mem::offset_of!(FtAffEventView, verb),
    ];
    for (i, value) in expected.iter().enumerate() {
        assert_eq!(unsafe { jackstay_c_affordances_layout(i as u32) }, *value, "layout index {i}");
    }
}
#[path = "support/local.rs"]
mod local;
use std::{
    ptr, thread,
    time::{Duration, Instant},
};

use jackstay::{ffi_bootstrap::*, ffi_local::ft_local_connection_destroy};
fn poll(mut f: impl FnMut(*mut *mut FtAffordancesEvent) -> FtStatus) -> *mut FtAffordancesEvent {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut event = ptr::null_mut();
    while f(&mut event) == FT_STATUS_EMPTY {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    assert!(!event.is_null());
    event
}
// The public C API negotiates independently owned channels, copies borrowed
// snapshot/string input, exposes typed event views, and nulls destroyed owners.
#[test]
fn c_bootstrap_snapshot_verb_and_destroy() {
    unsafe {
        let (a, mut b) = local::c_pair();
        let a = a as usize;
        let worker = thread::spawn(move || {
            let mut a = a as *mut jackstay::ffi_local::FtLocalConnection;
            let mut input = ptr::null_mut();
            let mut aff = ptr::null_mut();
            assert_eq!(
                ft_source_bootstrap_accept_v2_local(&mut a, ptr::null_mut(), 1, &mut input, &mut aff),
                FT_STATUS_OK
            );
            assert!(input.is_null());
            (a as usize, aff as usize)
        });
        let mut input = ptr::null_mut();
        let mut input_status = 0;
        let mut host = ptr::null_mut();
        let mut aff_status = 0;
        assert_eq!(
            ft_source_bootstrap_connect_v2_local(&mut b, 0, 0, 2, &mut input, &mut input_status, &mut host, &mut aff_status),
            FT_STATUS_OK
        );
        assert_eq!(aff_status, FT_STATUS_OK);
        let (a, p) = worker.join().unwrap();
        let mut a = a as *mut jackstay::ffi_local::FtLocalConnection;
        let mut producer = p as *mut FtAffordancesProducer;
        let mut snapshot: FtAffSnapshot = std::mem::zeroed();
        snapshot.domain = 1;
        snapshot.media.status = 2;
        snapshot.media.capabilities = 1;
        let mut title = String::from("copied title");
        snapshot.media.title = FtAffOptionalString {
            present: 1,
            value: FtAffString {
                data: title.as_ptr(),
                len: title.len(),
            },
        };
        assert_eq!(ft_affordances_producer_publish(producer, &snapshot), FT_STATUS_OK);
        title.clear();
        let mut event = poll(|out| ft_affordances_host_poll(host, out));
        let mut view: FtAffEventView = std::mem::zeroed();
        assert_eq!(ft_affordances_event_view(event, &mut view), FT_STATUS_OK);
        assert_eq!(view.snapshot.domain, 1);
        assert_eq!(
            std::slice::from_raw_parts(view.snapshot.media.title.value.data, view.snapshot.media.title.value.len),
            b"copied title"
        );
        ft_affordances_event_destroy(&mut event);
        assert!(event.is_null());
        let mut verb: FtAffVerb = std::mem::zeroed();
        verb.domain = 1;
        verb.verb = 1;
        assert_eq!(ft_affordances_host_send(host, &verb), FT_STATUS_OK);
        let mut event = poll(|out| ft_affordances_producer_poll(producer, out));
        assert_eq!(ft_affordances_event_view(event, &mut view), FT_STATUS_OK);
        assert_eq!((view.kind, view.verb.domain, view.verb.verb), (2, 1, 1));
        ft_affordances_event_destroy(&mut event);
        ft_local_connection_destroy(&mut a);
        ft_local_connection_destroy(&mut b);
        ft_affordances_host_destroy(&mut host);
        assert!(host.is_null());
        let mut event = poll(|out| ft_affordances_producer_poll(producer, out));
        assert_eq!(ft_affordances_event_view(event, &mut view), FT_STATUS_OK);
        assert_eq!(view.kind, 3);
        ft_affordances_event_destroy(&mut event);
        ft_affordances_producer_destroy(&mut producer);
        assert!(producer.is_null());
    }
}
// ADR-0005: a compiled C caller publishes typed state consumed by the same Rust
// implementation, so C parity is exercised beyond declaration and layout checks.
#[cfg(any(
    all(target_os = "linux", feature = "backend-linux"),
    all(target_os = "macos", feature = "backend-macos"),
    windows
))]
#[test]
fn compiled_c_publishes_to_rust_host() {
    unsafe extern "C" {
        fn jackstay_c_affordances_publish(producer: *mut std::ffi::c_void) -> FtStatus;
    }
    #[cfg(unix)]
    let (a, b) = jackstay::local::Stream::pair().unwrap();
    #[cfg(windows)]
    let (a, b) = jackstay::local::pipe_pair().unwrap();
    let mut producer = FtAffordancesProducer(jackstay::affordances::Producer::start(a).unwrap());
    let host = jackstay::affordances::Host::start(b).unwrap();
    assert_eq!(
        unsafe { jackstay_c_affordances_publish((&mut producer as *mut FtAffordancesProducer).cast()) },
        FT_STATUS_OK
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(event) = host.poll() {
            assert_eq!(
                event,
                jackstay::affordances::Event::Snapshot(jackstay::affordances::Snapshot::Window(jackstay::affordances::Window {
                    title: Some("C producer".into()),
                    requested_size: None,
                    ready: true
                }))
            );
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
}
