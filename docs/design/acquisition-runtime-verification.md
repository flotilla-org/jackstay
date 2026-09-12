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
capture, long playback, delayed consumer or host resize acceptance. Those remain
outstanding. The installed Porthole daemon still serves an existing user capture;
updating that installation would interrupt it.
