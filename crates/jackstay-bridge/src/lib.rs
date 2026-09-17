//! Cross-host bridge for Jackstay publications.
//!
//! A bridge is a translator peer (porthole ADR-0008): the *egress half* is an
//! ordinary Jackstay consumer on host A that encodes what it acquires and sends
//! it over a link; the *ingress half* is an ordinary Jackstay producer on host B
//! that decodes what arrives and republishes it as a new local publication.
//! Neither half is a coordinator; whoever spawned them hands each a grant.
//!
//! The link is two byte streams, media and control, carried by whatever the
//! coordinator forwarded (a Unix socket pair on one host, an SSH stream-local
//! forward between hosts). The framing in [`wire`] is carrier-agnostic and the
//! same message set is meant to ride QUIC streams or Media over QUIC groups
//! later without change.

pub mod clock;
/// The framing carries the arena's frame descriptor, and the arena is Unix-only.
#[cfg(unix)]
pub mod wire;

#[cfg(unix)]
pub mod input_relay;

#[cfg(all(target_os = "macos", feature = "backend-macos"))]
pub mod vt;

#[cfg(all(target_os = "macos", feature = "backend-macos"))]
pub mod egress;

#[cfg(all(target_os = "macos", feature = "backend-macos"))]
pub mod ingress;

#[cfg(all(target_os = "macos", feature = "backend-macos"))]
pub mod cpu_publication;

/// Monotonic nanoseconds on this host, in the clock domain Jackstay's macOS
/// producers stamp (`mach_absolute_time`, which is also CoreMedia's host clock).
/// On other platforms a monotonic clock with an arbitrary epoch.
#[must_use]
pub fn host_now_ns() -> u64 {
    #[cfg(target_os = "macos")]
    {
        // CLOCK_UPTIME_RAW is mach_absolute_time in nanoseconds, the clock
        // CoreMedia's host clock and ScreenCaptureKit timestamps are on.
        // SAFETY: plain libc call with a valid out-pointer.
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut ts) };
        u64::try_from(ts.tv_sec)
            .unwrap_or(0)
            .saturating_mul(1_000_000_000)
            .saturating_add(u64::try_from(ts.tv_nsec).unwrap_or(0))
    }
    #[cfg(not(target_os = "macos"))]
    {
        use std::sync::OnceLock;
        static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
        let epoch = *EPOCH.get_or_init(std::time::Instant::now);
        u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}
