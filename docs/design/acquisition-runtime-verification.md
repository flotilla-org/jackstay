# Acquisition runtime verification

On 2026-09-21, kiwi passed the full non-ignored macOS backend suite again after
the shared-event exhaustion was addressed. The four workspace gates, native
Clippy and offline SDL smoke also passed.

The ignored `named_publications_route_independently_and_retired_connections_cannot_rebind`
test passed against a separate launchd producer process. Three publications
shared one Mach service, including an untokened publication. GPU pixel readback
distinguished their frames; an unknown explicit token was rejected. Replacing
one registration preserved the others and rejected reuse of its old connection.
Dropping every registration and registering again on the same service also
delivered the new pixels. The test removed its launchd service on completion.
These are generated native frames; Porthole's bridge validation records desktop
capture separately.

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
live runs below supply playback and actual output-reconfiguration evidence.

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
transition was observed while those delayed viewers were running, so that run
did not verify reconfiguration with held frames. The later explicit-output run
below supplies the missing evidence. The successful playback
and normal closure do not resolve the submitted-GPU crash quarantine or prove
process-wide graceful daemon drainage.


## Window resize does not imply output resize

On 2026-09-13, a live Porthole/TextEdit check separated source-window geometry
from capture-buffer dimensions. The window resized from 673×439 to 850×560
logical points at scale 2, while both existing CPU/native streams stayed at
1346×878 pixels. A probe holding real CPU/native frames observed no allocation
generation change. Fresh sessions at the larger size produced 1700×1120 pixels
and retained that buffer size when the window was restored.

At that build, Porthole configured ScreenCaptureKit output dimensions only at
startup. Actual pool-replacement verification therefore required an output
configuration change; more manual window resizing alone was insufficient.
Robert subsequently agreed to fixed output with explicit reconfiguration; the
next run exercises that control.
See Porthole's `docs/2026-09-13-capture-output-sizing.md` and local evidence in
`/tmp/porthole-live-resize-wqbxm8zv/`. Both delayed viewers passed 300 frames at
250 ms holds; the generation-change probe correctly failed. All test sessions
and the test document window were closed, and the dedicated identity revoked.


## Live output replacement with old frames held

Porthole `03594a1` added owner-controlled output sizing and passed all four
required gates on macOS and Linux. Its installed signed development bundle
retained the existing TCC identity and permissions. Jackstay remained at
`f482a5c`, C ABI 0.5; no transport code change was needed for this check.

A dedicated TextEdit window stayed at 673×439 logical points, scale 2. The
installed SCK host changed CPU/native outputs from 1346×878 to 1700×1120 and back
through authenticated output requests. Before each request, a probe held a CPU
frame and a native frame, recording their descriptors and pixels. After each
transition, it acquired frames from the new allocation generation and verified
that both old descriptors and all old pixels remained identical. GPU pixels
were sampled through Metal with producer-readiness synchronization. CPU
generations advanced 2→3→4; native generations advanced 1→2→3.

Concurrent delayed CPU/native viewers each completed 300 frames with 250 ms
holds and empty stderr (86.53 s CPU, 81.47 s native). Each probe connection and
viewer reserved two holds, so each producer admitted four consumer holds during
the transitions. Both sessions retired; fresh sessions admitted new eight-frame
viewers and then retired too. The test document was closed and its dedicated
identity revoked. No synthetic capture or dummy SDL driver was used.

Evidence: Porthole's `docs/2026-09-13-live-output-reconfiguration.md` and
`/tmp/porthole-live-output-k1bmnjiq/`. The latter contains private test credentials;
publish only selected evidence. The probe source remains in
`/tmp/porthole-live-resize-wqbxm8zv/probe/src/main.rs`.

Together with the Simulator playback runs, this completes the selected live
CPU/macOS GPU acceptance. The submitted-GPU crash quarantine and lack of a
process-wide graceful Porthole drain API remain documented limitations; normal
session retirement does not establish recovery after process death.
