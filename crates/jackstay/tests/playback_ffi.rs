//! The playback contract through the C ABI: `c_playback_contract.c` runs the
//! scenarios of `playback.rs` as a compiled C caller, built with each
//! platform's header smoke.
#![cfg(any(
    all(target_os = "linux", feature = "backend-linux"),
    all(target_os = "macos", feature = "backend-macos"),
    windows
))]

use std::ffi::c_void;

#[path = "support/local.rs"]
mod local;

unsafe extern "C" {
    fn jackstay_c_playback_scenario(index: u32, host: *mut c_void, peer: *mut c_void) -> i32;
}

#[test]
fn compiled_c_drives_the_playback_contract() {
    for index in 0.. {
        let (host, peer) = local::c_pair();
        // SAFETY: the scenario takes ownership of both live connections.
        match unsafe { jackstay_c_playback_scenario(index, host.cast(), peer.cast()) } {
            -1 => {
                assert_eq!(index, 10, "scenario count");
                break;
            }
            line => assert_eq!(line, 0, "scenario {index} failed at c_playback_contract.c:{line}"),
        }
    }
}
