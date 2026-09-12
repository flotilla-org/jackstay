# Acquisition runtime verification

On 2026-09-12, kiwi's independent Foundation/Metal probe returned
`device=Apple M4 shared_event=created`. Earlier observations of shared-event
allocation failure remain recorded in `/tmp/jsxpc-weUTlB/README.md`; the cause of
that failure and subsequent recovery is unknown. A restart is no longer a
prerequisite for the native tests that passed below.

The common native arena and anonymous XPC suites passed 18 tests on the Metal
device, including producer readiness, deferred GPU release, retained old pools,
crash quarantine, C handles and teardown. Log:
`/tmp/jackstay-native-acquisition-resumed-tests.log`.

Three isolated named-service checks also passed:

- Cross-process frames, replacement and GPU-signalled release:
  `/tmp/jackstay-named-xpc-resumed.log`.
- Consumer death after submitting GPU work:
  `/tmp/jackstay-xpc-crash-resumed.log`.
- BGRA/RGBA reference viewer presentation and return of admission after exit:
  `/tmp/jackstay-native-viewer-resumed.log`.

The crash check establishes visible quarantine, not eventual recovery of the
reservation. Both before and after opening the GPU gate, its producer reported
`admission_available=false`, zero released claims and a drain-deadline recovery
failure. No timeout or process-exit observation was treated as GPU completion.

These tests allocate real IOSurfaces and execute Metal work, but their pixels are
generated fixtures. They do not prove the required live ScreenCaptureKit CPU/GPU
capture, long playback, delayed consumer or host resize acceptance. The separate
live runs below supply playback evidence; live host resize remains pending.

The delayed reference-viewer check also passed on 2026-09-12. It completed eight
BGRA frames normally and eight RGBA frames with a 250 ms hold before each GPU
submission, while the producer continued publishing every 20 ms. Both exits
returned admission. Log: `/tmp/jackstay-delayed-native-viewer.log`. The full
non-ignored macOS backend suite passed in
`/tmp/jackstay-delayed-full-native-tests.log`; the required default gates passed
on macOS and Linux, as did each platform's backend Clippy check. The two offline
SDL CTests passed, including the CPU delay option. These remain generated-fixture
checks; see the separate live results below.

## Live Simulator capture

After an authorized install/restart of Porthole `67fa206`, the CPU and native
viewers each completed 10,000 live frames from the existing iPhone Simulator
window. Runtime was 178.59 s for CPU and 179.60 s for native. Concurrent delayed
consumers completed 800 frames each with 250 ms holds in 231.16 s and 217.02 s,
respectively. All four exited successfully with empty stderr. CPU held-byte
comparisons found no mutations; viewer screenshots showed the Simulator clock
advancing. These runs used the installed ScreenCaptureKit host, not fixture pixels.

Explicit session close reached `closed` for both paths. Replacement sessions and
new viewer processes then completed eight frames each, including reuse of the
named native service. Artifacts: `/tmp/porthole-live-acquisition-qrl79wnm/`.
Porthole's `docs/2026-09-12-live-acquisition-acceptance.md` records provenance,
retirement, counters and limitations.

Simulator rejected AX size writes, and a manual resize was requested. Both
replacement sessions later adopted 912×1944 pixels at scale 2 with unchanged
logical window bounds. New delayed viewers completed 300 frames each at that
format, then both sessions closed and the test identity was revoked. No format
transition was observed while those delayed viewers were running, so live
reconfiguration with held frames remains unverified. The successful playback
and normal closure do not resolve the submitted-GPU crash quarantine or prove
process-wide graceful daemon drainage.
