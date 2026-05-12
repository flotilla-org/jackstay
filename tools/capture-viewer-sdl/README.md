# Capture Viewer SDL

Small C-ABI dogfood viewer for `capture-transfer`.

The viewer has two modes:

- default: runs a synthetic producer in-process and consumes frames through the
  public C ABI.
- `--porthole-socket`: asks `portholed` to create a synthetic capture session,
  connects back through a session descriptor, then receives frames through the
  fd-passing transport.

Build:

```sh
cargo build -p capture-transfer -p portholed --locked
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

Daemon-backed smoke test:

```sh
runtime_dir="$(mktemp -d)"
PORTHOLE_RUNTIME_DIR="$runtime_dir" target/debug/portholed >"$runtime_dir/portholed.log" 2>&1 &
portholed_pid=$!

for _ in $(seq 1 50); do
  [ -S "$runtime_dir/porthole.sock" ] && break
  sleep 0.1
done

SDL_VIDEODRIVER=dummy target/capture-viewer-sdl/capture-viewer-sdl \
  --porthole-socket "$runtime_dir/porthole.sock" \
  --frames 3

kill "$portholed_pid"
rm -rf "$runtime_dir"
```
