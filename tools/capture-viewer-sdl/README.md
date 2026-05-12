# Capture Viewer SDL

Small C-ABI dogfood viewer for `capture-transfer`.

The viewer has two modes:

- default: runs a synthetic producer in-process and consumes frames through the
  public C ABI.
- `--porthole-socket`: asks `portholed` to create a synthetic capture session,
  connects back through a session descriptor, then receives frames through the
  fd-passing transport.
- `--porthole-socket` plus `--session-id`: attaches to an existing daemon
  capture session without creating one.

Build:

```sh
cargo build -p capture-transfer -p porthole -p portholed --locked
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

Attach-only daemon smoke test:

```sh
runtime_dir="$(mktemp -d)"
PORTHOLE_RUNTIME_DIR="$runtime_dir" target/debug/portholed >"$runtime_dir/portholed.log" 2>&1 &
portholed_pid=$!

for _ in $(seq 1 50); do
  [ -S "$runtime_dir/porthole.sock" ] && break
  sleep 0.1
done

descriptor="$(PORTHOLE_RUNTIME_DIR="$runtime_dir" target/debug/porthole capture-session synthetic)"
session_id="$(printf '%s\n' "$descriptor" | awk '/^session_id:/ { print $2 }')"

SDL_VIDEODRIVER=dummy target/capture-viewer-sdl/capture-viewer-sdl \
  --porthole-socket "$runtime_dir/porthole.sock" \
  --session-id "$session_id" \
  --frames 3

kill "$portholed_pid"
rm -rf "$runtime_dir"
```

Real surface capture:

```sh
surface_id="$(porthole attach --frontmost --json | jq -r .surface_id)"
descriptor="$(porthole capture-session surface "$surface_id")"
session_id="$(printf '%s\n' "$descriptor" | awk '/^session_id:/ { print $2 }')"
porthole_socket="$(printf '%s\n' "$descriptor" | awk '/^porthole_socket:/ { print $2 }')"

target/capture-viewer-sdl/capture-viewer-sdl \
  --porthole-socket "$porthole_socket" \
  --session-id "$session_id"
```

The same flow is available as a bounded smoke test:

```sh
./scripts/manual-capture-transfer-smoke.sh --frames 300
```
