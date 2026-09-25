# D3D11 reference viewer (Windows)

`capture-viewer-d3d11` shows a Jackstay D3D11 source through the public C ABI
(0.10). It is the Windows counterpart of the SDL viewer's native mode:

1. `ft_local_connect` to the source's Local Endpoint, then
   `ft_acquisition_d3d11_connection_create_local`.
2. `ft_acquisition_d3d11_describe` reports the producer's adapter. The viewer
   creates its own D3D11 device on that LUID and attaches with it
   (`ft_acquisition_d3d11_attach`, holding 2 frames by default).
3. It creates a shared `ID3D11Fence` and registers it as its release timeline
   (`ft_acquisition_d3d11_register_release`).
4. For each frame: `ft_acquired_frame_d3d11_resources` lends the NT handles of
   the pool texture and the producer's readiness fence. The viewer imports the
   texture once per pool slot (`OpenSharedResource1`, cached by `pool_id` and
   `slot_id`) and the fence once per `fence_id` (`OpenSharedFence`). It discards
   the frame if `ft_d3d11_fence_alive` reports the fence abandoned, queues
   `ID3D11DeviceContext4::Wait` for the frame's `fence_value`, draws, presents,
   signals its release fence on the same context and defers the frame's release
   to that value. The CPU never waits for the producer.
5. On `RECONFIGURATION` it relinquishes and installs the replacement
   (`ft_acquisition_d3d11_install_configuration`). The first frame of a new
   pool drops the imports of older pools: the viewer holds no frame across
   iterations, and D3D11 keeps a resource alive for GPU work already queued.

It exits when the producer closes (setup liveness turns CLOSED, or acquisition
reports CLOSED) and waits up to five seconds for its own GPU work before
releasing anything. A bounded run prints `presented_frames=N` and fails if it
presents fewer.

## Why Win32 and not SDL

The SDL viewer's D3D11 renderer (SDL2) picks its adapter from the display and
has no way to sample an external shared texture or to queue a fence wait on its
context. This path needs all three: the device must be on the producer's LUID
(the producer refuses other adapters), and each draw must GPU-wait the frame's
readiness fence. SDL would only provide the window, so the viewer uses Win32
directly and needs nothing beyond the Windows SDK.

## Build and run

`scripts/smoke-viewer.ps1` builds Jackstay with `backend-windows`, the
`d3d11_source` example and this viewer (CMake with MSVC; the Visual Studio
bundled CMake is found automatically), serves the source on a private endpoint
and runs the viewer against it:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\smoke-viewer.ps1
powershell -ExecutionPolicy Bypass -File scripts\smoke-viewer.ps1 -HoldMs 250 -Frames 12 -Screenshot $env:TEMP\viewer.png
powershell -ExecutionPolicy Bypass -File scripts\smoke-viewer.ps1 -Source window -ResizeEveryMs 1500 -Frames 16 -HoldMs 250
powershell -ExecutionPolicy Bypass -File scripts\smoke-viewer.ps1 -Adapter warp -ResizeEveryMs 400 -Frames 90
```

- `-Source synthetic` (default): an animated pattern (hue gradient, a sweeping
  white bar, the frame number in binary along the top) published from the
  backend's own device.
- `-Source window`: the source opens its own small window, recolours it every
  half second and publishes it through Windows.Graphics.Capture. It captures
  nothing else.
- `-ResizeEveryMs` alternates the source between 480x270 and 720x270, so the
  viewer installs replacement pools.
- `-Adapter default|warp|<LUID hex>` places the source on an adapter; the
  viewer follows it.
- `-Screenshot PATH` saves the viewer's own window with `PrintWindow` while it
  runs. It never captures other windows or the screen.

To run the pieces by hand:

```powershell
cargo build --workspace --locked --features backend-windows --lib --examples
cmake -S tools/capture-viewer-d3d11 -B build/viewer-d3d11
cmake --build build/viewer-d3d11 --config Debug
target\debug\examples\d3d11_source.exe --endpoint my-source
build\viewer-d3d11\Debug\capture-viewer-d3d11.exe --endpoint my-source --hold-ms 250
```

Viewer options: `--endpoint NAME`, `--session-scope`, `--frames N` (0 or
absent: until closed), `--hold-ms MS` (keep each lease at least that long
before drawing, while the window stays responsive), `--holding N`.

The build links `target/debug/jackstay.dll.lib` from a `backend-windows` build
and copies `jackstay.dll` beside the executable; set `JACKSTAY_LIB` and
`JACKSTAY_INCLUDE_DIR` for another build. The viewer requires an exact
header/library ABI match during 0.x.
