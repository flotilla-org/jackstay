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
