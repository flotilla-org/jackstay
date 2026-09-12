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

Build just the Rust library with `cargo build --workspace --locked`. For macOS
native capture consumers, add `--features backend-macos`; for Linux native
transport add `--features backend-linux`. Windows currently has a library compile
check and explicit unsupported capture paths. The SDL viewer supports macOS and
Linux; its native presentation mode is Metal-only. Linux native reference checks
use the Vulkan consumer in the library.

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

## Origin and license

Extracted with source history from [porthole](https://github.com/flotilla-org/porthole).
The Rust package retains the original `MIT OR Apache-2.0` license declaration.
Original commit authors remain in the filtered history; see
[source history](docs/source-history.md) for the extraction paths and commit map.
