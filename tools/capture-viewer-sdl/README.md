# Capture Viewer SDL

Small C-ABI dogfood viewer for `capture-transfer`.

The current viewer runs a synthetic producer in-process because cross-process
session discovery is not implemented yet. It still consumes frames only through
the public C ABI, which keeps the producer/consumer boundary honest before
porthole publishes real ScreenCaptureKit frames.

Build:

```sh
cargo build -p capture-transfer --locked
cmake -S tools/capture-viewer-sdl -B target/capture-viewer-sdl \
  -DCAPTURE_TRANSFER_LIB="$PWD/target/debug/libcapture_transfer.dylib"
cmake --build target/capture-viewer-sdl
```

Run:

```sh
target/capture-viewer-sdl/capture-viewer-sdl
```

Headless smoke test:

```sh
SDL_VIDEODRIVER=dummy target/capture-viewer-sdl/capture-viewer-sdl --frames 3
```

Expected future command shape once porthole has session discovery:

```sh
capture-viewer-sdl --session <session-descriptor-or-uds-path>
```
