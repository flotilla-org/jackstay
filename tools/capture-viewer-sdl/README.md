# SDL reference viewer

The default mode creates synthetic frames and consumes them through Jackstay's
public C API in the same process. This mode needs no porthole daemon. The existing
SDL rendering path remains available to Katzensteg's SDL interception.

From the Jackstay checkout:

```sh
./scripts/smoke-viewer.sh
```

Leave `--frames` off the resulting `build/viewer/capture-viewer-sdl` command to
keep the window open. `SDL_VIDEODRIVER=dummy` supports offline synthetic checks;
it is not a substitute for real desktop capture evidence.

## External library build

Build Jackstay first, with `--features backend-macos` on macOS. CMake defaults to
the current Jackstay checkout's headers and debug library. For an installed or
separately built library, pass both locations:

```sh
cmake -S tools/capture-viewer-sdl -B build/viewer \
  -DJACKSTAY_LIB=/absolute/path/to/libjackstay.dylib \
  -DJACKSTAY_INCLUDE_DIR=/absolute/path/to/jackstay/include
cmake --build build/viewer
ctest --test-dir build/viewer --output-on-failure
```

The viewer requires an exact header/library ABI version match during 0.x and
exits before connecting when they differ. Rebuild both together after an ABI change.

Linux uses `libjackstay.so`. The CTest smoke checks 30 generated frames through
the C ABI and SDL software renderer. It fails on missing/corrupt frames or renderer
failure. The library and viewer support synthetic operation on macOS/Linux; the
SDL native presenter uses Metal and rejects `--native` on Linux. Linux native
import/sync verification lives in the Vulkan reference consumer checks.

## Generic CPU publication

Connect directly to a same-user Jackstay CPU setup socket, such as a Katzensteg
publication, without Porthole session selection:

```sh
./build/viewer/capture-viewer-sdl --cpu-socket /tmp/mi2-media.sock
```

This uses one held-frame reservation and supports configuration replacement when
the source resizes. The source must have a free consumer reservation. The viewer
is observation-only; keyboard and mouse input are not sent to the publisher.
Do not combine `--cpu-socket` with `--native`, `--porthole-socket` or `--session-id`.

## Optional porthole integration

These modes require a separately installed and authorized porthole. They are not
needed to build or test the standalone example.

`--porthole-socket PATH` creates a porthole synthetic capture session; adding
`--session-id ID` attaches to an existing CPU session instead. The library's
viewer reads `PORTHOLE_AGENT_TOKEN` and passes it explicitly through the C session
descriptor for protected CPU sessions. Use the identity that created the capture
session; inheriting a token alone does not grant access. The library does not
read the environment or approve requests. The host
remains responsible for capture permission, source selection and session cleanup.

For a real macOS native capture session, keep porthole running as its installed
launchd job so it owns its attach MachService. Use porthole's capture-session
command to obtain the endpoint and attach token, then supply them to the viewer:

The native viewer now requires the common XPC acquisition service
(`XpcArenaServer`). Porthole's host migration is still pending on this branch;
its legacy `XpcAttachServer` is not compatible with this viewer's native mode.

```sh
./build/viewer/capture-viewer-sdl --native \
  --transport-kind 1 --endpoint "$attach_endpoint" --token "$attach_token" \
  --frames 120
```

Transport kind 1 is the macOS XPC endpoint; `--mach-service NAME` also selects it.
Use values returned by the host, not guessed surface handles or IDs. Consult the
porthole native-viewer smoke for its complete authorized session lifecycle.
The viewer requests a two-frame holding reservation. It waits for producer
readiness on the GPU and retains each frame and its native imports until the
Metal completion callback. Replacement uses the acquired generation's own
resources; the viewer keeps no persistent surface or texture cache.
Native capture needs separate live validation; the offline
synthetic test makes no claim about GPU copies or desktop permissions.

A bounded native run reports `presented_frames=N` and fails if it ends before
that count or presentation/release fails. This counts completed GPU command
buffers whose leases have been released. Shutdown waits up to five seconds for
pending completion owners; timeout reports failure without releasing frames
still in use. This is not a copy-overhead measurement.

## Native verification

The optional pipeline smoke compiles the real Metal shaders and initializes the
presenter without capture or shared events:

```sh
cmake --build build/viewer --target metal-presenter-smoke
./build/viewer/metal-presenter-smoke
```

The separate-process acceptance test opens two temporary viewer windows, renders
BGRA and RGBA frames from its own native producer, and verifies clean exit returns
the admission reservation. Build with `scripts/smoke-viewer.sh` first so the
library includes the macOS bridge:

```sh
JACKSTAY_VIEWER_TEST_BINARY="$PWD/build/viewer/capture-viewer-sdl" \
  cargo test -p jackstay --locked --features backend-macos --test native_arena_xpc_process \
  -- --ignored --exact reference_viewer_completes_bgra_and_rgba_frames_and_returns_admission --nocapture
```

This test requires a logged-in GUI session and working Metal shared events. It
uses synthetic native sources; authorized live capture acceptance is separate.

## Cooperative input reference

ABI 0.12 supports bootstrap v2 over named Local Endpoints. Start the toolkit
example and connect to its user endpoint:

```sh
cargo run -p jackstay-producer --example minimal &
# After the source starts (the example runs for ten seconds):
./build/viewer/capture-viewer-sdl --source-endpoint minimal-producer --log-affordances
```

`--source-endpoint NAME` uses `ft_local_connect` and bootstrap v2; add
`--session-scope` when the producer uses a session endpoint. `--affordances
none|optional|required` defaults to `optional` and applies only to this path.
Required absence fails setup; optional absence preserves media. `--observe`
requests no input while still allowing affordances. `--log-affordances` prints
each producer snapshot's domain and summary to stderr. The viewer publishes an
initial `presentation` snapshot with visible=true, window focus, scale=1 and no
preferred size. Media, input and affordances are independently owned and closed;
affordances closure does not confirm input cleanup. Affordances are not rendered.

`--source-socket PATH` retains bootstrap v1 for arbitrary POSIX paths; it cannot
connect to a v2-only toolkit producer. Start the legacy interactive source in
a private directory:

```sh
demo_dir=$(mktemp -d /tmp/jackstay-input.XXXXXX)
./build/viewer/capture-input-source "$demo_dir/source" &
source_pid=$!
while [ ! -S "$demo_dir/source" ]; do
  kill -0 "$source_pid" 2>/dev/null || break
  sleep 0.05
done
./build/viewer/capture-viewer-sdl --source-socket "$demo_dir/source"
wait
rmdir "$demo_dir"
```

Wait for the source's `ready` line before starting the viewer. The source accepts
one viewer and exits after it disconnects and cleanup finishes. Held keys tint
the top strip green; text fills a bottom bar; the pointer becomes red while a
button is held. The source prints final input counts and held state on exit.
This is a cooperative target, not native desktop injection.

The source also listens on a [Local Endpoint](../../docs/design/local-endpoints.md)
with `--endpoint NAME` (add `--session-scope` for a session endpoint), the only
form on Windows. This legacy source still uses bootstrap v1, including on named
endpoints; the new viewer `--source-endpoint` path requires a v2 producer. It
needs no SDL, so build it there directly, for example from a
Visual Studio developer prompt:

```bat
cl /std:c11 /I crates\jackstay\include tools\capture-viewer-sdl\src\input_source.c ^
  /Fe:capture-input-source.exe /link target\debug\jackstay.dll.lib
```

Keep `jackstay.dll` beside the executable. `--log-input` prints one line per
executed input operation. `--resize-every-ms MS` alternates the frame between
320x180 and 480x360, replacing the allocation on the first growth and updating
the input geometry, to exercise consumer reconfiguration. `--repeat` keeps the
endpoint and serves viewers one after another until the source is killed, so a
viewer can reconnect, for example to request input again.

`--source-socket` and `--source-endpoint` request optional cooperative input. Use `--observe` to request
no input, or `--require-input` to fail if control is unavailable. Optional
refusal is reported while observation continues. The source can withhold input
with `--observe-only`. The low-level `--cpu-socket` plus `--input-socket` pair
remains available for independent adapter tests. Without `--input-socket`, that
low-level CPU connection is observation-only. The reference sender
uses `--typing cooperative|text|physical` on either bootstrap path (default
`cooperative`). With `--observe`, typing has no effect because no input channel
is requested. Text mode sends committed text and suppresses keys. Physical mode
sends physical keys, suppresses committed text and SDL repeat events (repeat belongs
to the target). Cooperative mode sends physical keys, committed text and source
repeat. Unsupported typing modes can be refused by the producer. The low-level
input socket retains cooperative behavior. Focus
loss clears held state but retains the controller; reconnecting starts empty.
The input connection has its own heartbeat worker, independent of rendering.

`ctest --test-dir build/viewer --output-on-failure` includes separate-process
cooperative input, all typing modes, viewer-process-death cleanup, v2 channel
negotiation/presentation/teardown, and the actual toolkit minimal example. The
latter uses Cargo in offline mode and an isolated runtime directory; cached or
vendored crates are required, and its first run may build the example. `--input-self-test` is a
fixture for that test, not ordinary interactive behavior. The text fixture feeds
the same event translator as live SDL input because sdl2-compat cannot translate
pushed SDL2 text-input events. Native desktop input and live desktop capture are
separate acceptance work. See [the contract](../../docs/design/input.md) and
[the C interface](../../crates/jackstay/include/jackstay_input.h).

See [bootstrap ownership and failure semantics](../../docs/design/source-bootstrap.md).

### Scroll units and SDL2 on macOS

SDL2's [Cocoa wheel adapter](https://github.com/libsdl-org/SDL/blob/release-2.32.10/src/video/cocoa/SDL_cocoamouse.m#L509)
passes Cocoa `deltaX/Y` to `SDL_SendMouseWheel`; `preciseX/Y` retain these float
values. They are not AppKit's logical-pixel `scrollingDeltaX/Y`. SDL2 rounds
non-precise wheel values away from zero and leaves precise values fractional,
but it drops `hasPreciseScrollingDeltas`: an integral trackpad delta cannot be
identified from `preciseX/Y` alone. This finding is from upstream source inspection,
not a live hardware measurement.

The C viewer captures precise wheel metadata during SDL's event filter, inside
Cocoa dispatch. It reads the current `NSEvent` through the Objective-C C runtime: precise devices use
`scrollingDeltaX/Y` as `Pixel`, scaled from window logical coordinates into target
geometry. Value-only user events preserve ordering with focus and key events;
notched wheels use SDL's `Line` values without a line-height multiplier.
Positive deltas mean right/down after `SDL_MOUSEWHEEL_FLIPPED` inversion. Momentum
arrives as further pixel events; no phases or momentum protocol is synthesized.
Outside Cocoa, SDL2 provides no portable unit/device flag: the fallback treats
fractional wheel values as continuous `Pixel` deltas and integral values as
`Line`. Integral precise-device deltas remain ambiguous on that fallback, and
SDL2 before 2.0.18 exposes only integer values. Live macOS trackpad/HiDPI checks
remain separate from the dummy-video tests.
