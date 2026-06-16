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

## Native (IOSurface/Metal) capture

On macOS the viewer also has a native present path: it attaches to a native
capture session over XPC, latches the received `IOSurface` into an
`MTLTexture` zero-copy, GPU-waits on the per-frame `MTLSharedEvent` value
before sampling, and presents — no pixel readback. This needs the library
built with the `backend-macos` feature so the `ft_native_*` symbols are
present, and `portholed` running from `Porthole.app` under launchd so it owns
the `work.flotilla.porthole.attach` mach service:

```sh
cargo build -p capture-transfer --features backend-macos --locked
cmake -S tools/capture-viewer-sdl -B target/capture-viewer-sdl \
  -DCAPTURE_TRANSFER_LIB="$PWD/target/debug/libcapture_transfer.dylib"
cmake --build target/capture-viewer-sdl

surface_id="$(porthole attach --frontmost --json | jq -r .surface_id)"
descriptor="$(porthole capture-session surface "$surface_id" --native)"
mach_service="$(printf '%s\n' "$descriptor" | awk '/^mach_service:/ { print $2 }')"
attach_token="$(printf '%s\n' "$descriptor" | awk '/^attach_token:/ { print $2 }')"

target/capture-viewer-sdl/capture-viewer-sdl \
  --native --mach-service "$mach_service" --token "$attach_token"
```

The bounded smoke test for this path:

```sh
./scripts/manual-native-viewer-smoke.sh --frames 600
```

Scope: one native session at a time (launchd permits a single listener per
mach-service name).
