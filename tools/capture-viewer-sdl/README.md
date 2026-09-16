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

ABI 0.7 adds the optional shared input channel. Start the interactive synthetic
source in a private directory, then connect the existing viewer to both sockets:

```sh
demo_dir=$(mktemp -d /tmp/jackstay-input.XXXXXX)
./build/viewer/capture-input-source "$demo_dir/media" "$demo_dir/input" &
source_pid=$!
while [ ! -S "$demo_dir/input" ]; do
  kill -0 "$source_pid" 2>/dev/null || break
  sleep 0.05
done
./build/viewer/capture-viewer-sdl --cpu-socket "$demo_dir/media" --input-socket "$demo_dir/input"
wait
rmdir "$demo_dir"
```

Wait for the source's `ready` line before starting the viewer. The source accepts
one viewer and exits after it disconnects and cleanup finishes. Held keys tint
the top strip green; text fills a bottom bar; the pointer becomes red while a
button is held. The source prints final input counts and held state on exit.
This is a cooperative target, not native desktop injection.

`--input-socket` explicitly opts into control of a generic CPU publication.
The host must ensure the media and input endpoints belong to the intended target.
Without this option the viewer remains observation-only. The reference sender
uses cooperative key events, separate committed text and source repeat. Focus
loss clears held state but retains the controller; reconnecting starts empty.
The input connection has its own heartbeat worker, independent of rendering.

`ctest --test-dir build/viewer --output-on-failure` includes separate-process
cooperative input and viewer-process-death cleanup. `--input-self-test` is a
fixture for that test, not ordinary interactive behavior. The text fixture feeds
the same event translator as live SDL input because sdl2-compat cannot translate
pushed SDL2 text-input events. Native desktop input and live desktop capture are
separate acceptance work. See [the contract](../../docs/design/input.md) and
[the C interface](../../crates/jackstay/include/jackstay_input.h).
