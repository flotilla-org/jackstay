# Jackstay

Jackstay moves buffers between local producers and consumers. It provides a
broadcast ring, native handle transfer, explicit synchronization, leases and a
Rust implementation shared by Rust and C-ABI consumers.

This repository starts at **0.1.0** with no API or ABI stability promise. The C
headers carry their own ABI version; check that version when loading the library.
Porthole consumes a pinned Git revision. The repository is public; cloning and
dependent CI builds need no GitHub credential.

## Build and run

Install Rust, CMake, pkg-config and SDL2 development files. On macOS, Xcode command
line tools supply the Objective-C and Metal frameworks. On Linux, the optional
`backend-linux` feature also requires PipeWire development files
(`libpipewire-0.3-dev` on Debian/Ubuntu).

```sh
./scripts/smoke-viewer.sh
```

The SDL viewer creates a bounded in-process CPU arena, reserves two frame holds,
publishes generated frames through the C API, and renders 30 frames through the
same acquisition and rendering loop used for Porthole CPU sessions. It prints
`acquired_frames=30` and fails if publishing, acquisition, payload validation or
rendering fails. No porthole daemon, desktop capture permission or second checkout
is required. To run the same check without a display:

```sh
SDL_VIDEODRIVER=dummy ./scripts/smoke-viewer.sh
```

The synthetic example exercises common acquisition and checks that producer
storage drains before destruction. It does not prove cross-process handle
transfer or live capture. The [C producer API](docs/design/acquisition-c-boundary.md#cpu-producers)
exposes admission limits, dropped publication, byte-budgeted replacement and
retained destruction directly.

[Generic C CPU setup](docs/design/acquisition-cpu-c-setup.md) lets C/Zig hosts
serve already-authorized Unix connections and clients attach without a daemon.
After the smoke build, `ctest --test-dir build/viewer --output-on-failure` also
runs a standalone C producer and a separately executed consumer through resize,
setup cancellation and process-exit cleanup. Match the ABI 0.7 header and library.

For delayed-consumer checks, add `--hold-ms 250` to either CPU or native viewing.
The viewer keeps each acquired lease for at least that delay before consuming
it, while still handling window-close events. CPU mode compares the held bytes
before and after the delay; native mode delays GPU submission and then retains
the frame until actual completion. Apply the option to an authorized live source
for capture acceptance; synthetic mode only checks the consumer machinery.

Build just the Rust library with `cargo build --workspace --locked`. For macOS
native capture consumers, add `--features backend-macos`; for Linux native
transport add `--features backend-linux`. On Windows the library, shared memory,
the acquisition arena, the named-pipe setup channel and the acquisition,
bootstrap and input C ABI ([Local Endpoints](docs/design/local-endpoints.md),
ABI 0.9) build and are tested. `--features backend-windows` adds the D3D11 arena
backend and Windows.Graphics.Capture sessions
([D3D11 backend](docs/design/acquisition-d3d11.md)), and ABI 0.10's D3D11
consumer calls. The SDL viewer supports macOS and Linux; its native
presentation mode is Metal-only. Linux native reference checks use the Vulkan
consumer in the library. On Windows,
[`tools/capture-viewer-d3d11`](tools/capture-viewer-d3d11/README.md) is the
reference viewer and `scripts/smoke-viewer.ps1` its smoke (add `-HoldMs 250`
for the delayed-consumer check).

## Library and host boundary

Jackstay owns handles, synchronization, buffer lifetime and transport. Reusable
capture mechanisms may live here: PipeWire accepts an already-open connection
and stream identifier from its host. The host obtains consent, chooses the source,
authorizes callers and supervises its desktop session. Porthole, a compositor or
a desktop environment can each supply that role; Jackstay does not require
porthole's token system. The optional `daemon` module and viewer
`--porthole-socket` mode adapt the existing porthole control API, but the standalone
example does not use them.

Porthole's installed macOS integration keeps its launchd-owned XPC broker and OS
permissions. Network streaming is future work: a bridge could consume a stream
here and publish another local stream on the receiving host. No Tender dependency
or requirements follow from this extraction.

The agreed next acquisition contract is recorded in
[ADR-0001](docs/adr/0001-acquisition-leases-and-reservations.md), with
[implementation slices](docs/specs/acquisition-lifetime-contract.md) and a
[glossary](CONTEXT.md). These describe work not yet implemented.

## Linking and verification

Rust consumers depend on package `jackstay`. The shared library is
`libjackstay.dylib` on macOS or `libjackstay.so` on Linux. Public headers live in
`crates/jackstay/include`; the C entry points retain their existing `ft_*` names
in `capture_transfer.h` and the ring layout remains in `jackstay_ring.h`.
The viewer's CMake build accepts `JACKSTAY_LIB` and `JACKSTAY_INCLUDE_DIR` for
explicit external library/header locations.

See [viewer instructions](tools/capture-viewer-sdl/README.md) for optional porthole
integration and native presentation, and [verification](docs/verification.md)
for offline versus hardware checks. API stability, Windows continuous capture
and direct Katzensteg integration are separate later milestones.

## Swift

On macOS, `import Jackstay` exposes the C ABI through
`crates/jackstay/include/module.modulemap`. Build the CPU library and put the
dylib beside the Swift executable:

```sh
cargo build --locked -p jackstay --release
mkdir -p build/swift
cp "${CARGO_TARGET_DIR:-target}/release/libjackstay.dylib" build/swift/
otool -D build/swift/libjackstay.dylib # install name: @rpath/libjackstay.dylib
swiftc main.swift -I crates/jackstay/include -L build/swift -ljackstay \
  -Xlinker -rpath -Xlinker @executable_path -o build/swift/main
build/swift/main
```

`-I` finds both the public headers and the module map automatically. No bridging
header is needed. Check the header/library version at startup; Swift cannot
import the cast in the C `FT_ABI_VERSION` macro, so use its components:

```swift
import Jackstay
let FT_ABI_VERSION = UInt32((FT_ABI_VERSION_MAJOR << 16) | FT_ABI_VERSION_MINOR)
precondition(ft_abi_version() == FT_ABI_VERSION)
```

Ship the executable and `libjackstay.dylib` together from the same Jackstay
revision. The dylib's install name is `@rpath/libjackstay.dylib`; the executable's
`@executable_path` rpath finds it in the executable's directory, independent of
the working directory or checkout. `otool -L build/swift/main` must show
`@rpath/libjackstay.dylib`. If the library is missing, dyld reports
`Library not loaded: @rpath/libjackstay.dylib` before Swift starts.
CPU-only builds need no native backend feature; use `--features backend-macos`
when native capture is required. This uses the existing `cdylib` output and
avoids adding a static archive to every platform's builds.

Run `scripts/smoke-swift.sh` for the macOS CI check: a Rust producer exports a
payload object and reserves a slot, the Swift child calls `ft_cpu_writer_import`,
`ft_cpu_writer_slot_view` and `ft_cpu_writer_destroy`, and Rust verifies the
written bytes after the child exits. Keep the export and reservation alive until
delegate writes finish; never reuse a slot while the delegate is writing it.
The check also verifies the dylib install name and executable rpath, requires
loading to fail when the packaged dylib is removed, and requires Swift to reject
a disposable header copy with a broken bootstrap include. On macOS, `cargo test`
runs this script through the `swift_bindings` integration test with an isolated
Cargo target directory. Each invocation builds the CPU library and exporter
again in that fresh directory, adding a debug build to both the CPU-only and
native-feature CI test passes.

## Origin and license

Extracted with source history from [porthole](https://github.com/flotilla-org/porthole).
The Rust package retains the original `MIT OR Apache-2.0` license declaration.
Original commit authors remain in the filtered history; see
[source history](docs/source-history.md) for the extraction paths and commit map.

## Shared input

The optional [input interface](docs/design/input.md) supplies ordered controller
sessions, execution results and cleanup over a host-authorized Unix connection.
The [SDL viewer and interactive synthetic source](tools/capture-viewer-sdl/README.md#cooperative-input-reference)
exercise both ends through `jackstay_input.h` (ABI 0.7). Porthole native desktop
execution and Katzensteg connector integration are separate consumers of this
interface; neither is implied by the reference demo.

## One source endpoint

ABI 0.8 adds [shared source bootstrap](docs/design/source-bootstrap.md). Hosts can
expose one authorized endpoint for CPU media and optional shared input, while
retaining independent channel ownership and processing. Rust uses `bootstrap`;
C/Zig clients use `jackstay_bootstrap.h`. The SDL reference source and viewer
exercise it with `--source-socket`; KS's connector adoption is a separate change.

Native Rust producers can use the **`jackstay-producer`** workspace crate. It is
convenience scaffolding over Jackstay, with no rendering dependency or desktop
authority. `Builder::new(endpoint, arena_config, input_config, callbacks).start()`
returns a running `Source`; `Source::stop()` performs ordered shutdown. Implement
`Producer` for CPU frames, input execution (including cleanup), state snapshots,
and affordance callbacks. See
[the minimal example](crates/jackstay-producer/examples/minimal.rs) and
[the toolkit contract](crates/jackstay-producer/README.md).

Affordances are available through explicit `bootstrap::accept_v2` /
`connect_v2` negotiation and the ABI 0.12 typed
[`jackstay_affordances.h`](crates/jackstay/include/jackstay_affordances.h) surface.
The legacy bootstrap entry points explicitly select v1. No automatic downgrade
or retry occurs on a partially consumed stream.
