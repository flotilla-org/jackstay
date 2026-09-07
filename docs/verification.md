# Verification

The repository gates are listed in AGENTS.md. CI runs them on macOS and Linux,
then builds/tests the matching native feature. It compiles the library on Windows.
C11 and Zig checks compile the public headers independently. The SDL dummy-video
smoke checks generated payloads through the real C ABI and software renderer.

Native unit checks exercise handles, leases, pool configuration, attach messages
and synchronization. Some inherited Linux tests opportunistically probe DRM and
skip when devices are absent; a passing hosted CI job is not live Linux evidence.
To exercise those probes on hardware, enable `backend-linux`, check access to
`/dev/dri/renderD128` and `/dev/dma_heap/system`, and record which probes ran.

Porthole owns the live desktop tests because it owns the authorized capture
session. After switching its pinned dependency, run its native macOS viewer smoke
under the installed launchd job. On KWin, run its live PipeWire attach and
lease-release tests with a producing desktop source and actual portal consent.
Check screenshot and recording separately. Record both repository revisions and
hardware/session details. A blocked or unrun live check is not a pass.

Extraction issue: https://github.com/flotilla-org/porthole/issues/113
Integration issue: https://github.com/flotilla-org/porthole/issues/114
