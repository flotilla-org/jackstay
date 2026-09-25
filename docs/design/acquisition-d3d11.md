# D3D11 arena backend and Windows.Graphics.Capture

`backend-windows` adds the Windows native backend
([#28](https://github.com/flotilla-org/jackstay/issues/28)). It implements the
D3D11 frame-sharing decision
([#22](https://github.com/flotilla-org/jackstay/issues/22)) and the capture
placement decision ([#23](https://github.com/flotilla-org/jackstay/issues/23)).
It mirrors the macOS backend: a Jackstay-owned pool, a GPU copy followed by a
timeline signal, handles moved by a setup channel, and consumer release
timelines that drive deferred release.

The feature pulls in the `windows` crate (0.62.2). `windows-sys`, which the crate
already used, has no COM or WinRT projections. `windows` comes from the same
windows-rs release and shares `windows-link` 0.2.1 with it. The lockfile gains
`windows`, `windows-core`, `windows-future`, `windows-collections`,
`windows-numerics`, `windows-threading`, `windows-result`, `windows-strings`,
`windows-implement`, `windows-interface` and `syn` 2. The dependency is optional,
so builds without the feature do not compile it.

## Modules

| Module | Role |
| --- | --- |
| `native::windows` | `D3d11Device`, `D3d11FrameBackend` (implements `NativeFrameBackend` and `ArenaNativeBackend`), `D3d11Fence` (implements `ReleaseTimeline`), `SharedTextureHandle`, `SharedFenceHandle`, `AdapterLuid`, `adapters()` |
| `native::windows::setup` | `serve_d3d11` and `D3d11SetupClient`: the named-pipe setup channel |
| `native::windows::capture` | `WgcCapture`: a WGC session publishing into an arena, with `CaptureTarget`, `CapturePolicy`, `CaptureStatus` and `Publication` |

## Pool

Each pool generation is a set of `ID3D11Texture2D`s (BGRA8 by default; RGBA8 is
also accepted) with one mip level. Pool textures are `DEFAULT` usage, bound as
shader resources and render targets, and created
`SHARED | SHARED_NTHANDLE`. Each slot has one unnamed NT handle from
`IDXGIResource1::CreateSharedHandle`. There are at most 15 slots, so that the
setup channel can move a whole generation (the slots plus the fence) in one
bounded handle batch.

D3D11 does not report how much memory a texture actually uses. The arena
therefore charges a conservative bound for tiled layouts: width and height each
rounded up to 64 texels, at 4 bytes per texel, then rounded up to 64 KiB per
slot. Preflight checks this bound against the budget before allocating.

`stage_frame` queues a `CopySubresourceRegion` from the captured texture into
the slot. The copy box crops to the published region, for example a window's
client area. `signal_fence` then queues `ID3D11DeviceContext4::Signal` on the
same immediate context and flushes, so the published fence value always names
submitted work. A captured WGC buffer is only read by that queued copy. The
capture closes the WGC frame straight after `publish`, which returns the buffer
to WGC; Jackstay never leases a WGC buffer to a consumer.

Slot reuse relies on the arena's claims, consumer release timelines and the
producer's own completed fence value. D3D11 has no cross-process "in use" query
to add to these.

## Handle transfer

`serve_d3d11` runs on a Local Endpoint named pipe (Wheelhouse ADR 0011; see
[local endpoints](local-endpoints.md)). The host has already routed and
authorized the connection. Requests are length-prefixed JSON behind the magic
`JSD3D001`:

| Request | Reply and objects |
| --- | --- |
| `describe` | the producer's `AdapterInfo` (LUID, description, vendor and device IDs, whether it is software) |
| `attach { holding, adapter }` | `refused { adapter_mismatch }` before any admission, or `attached`. The 5 arena handles, then the slot textures plus the fence, move as two `send_handles` batches. |
| `configuration` | `empty`, or the replacement section plus the new generation's textures and fence |
| `register_release { handle }` | `registered { registration }` |

Admission uses `attach_process_handle` with the process handle kept from accept.
No PID from a request is used. `send_handles` duplicates each object into that
process with `DUPLICATE_CLOSE_SOURCE`, and the consumer acknowledges receipt.
Handles therefore move once per consumer incarnation for each pool generation,
and per-frame data travels only in the shared ring (slot, fence value,
generation). Textures go to the consumer with `DXGI_SHARED_RESOURCE_READ` access
only; reading and `OpenSharedResource1` work with that access. The fence keeps
`GENERIC_ALL`, the only access that `ID3D11Fence` shared handles support.

The consumer keeps its release fence handle open while it sends the handle's
value. The producer duplicates it out of the consumer process: it has held that
process with `PROCESS_DUP_HANDLE` since accept. The producer then opens the
duplicate with `OpenSharedFence` on its own device. The open validates the
object: any other handle named here fails, and its duplicate closes.

## Synchronisation

- Producer readiness is one shared `ID3D11Fence` per producer. A consumer opens
  it on its own device and queues `ID3D11DeviceContext4::Wait(fence, value)`
  before sampling, so the consumer's CPU never waits.
- Release is the consumer's own shared fence, registered once. The consumer
  signals it after its last GPU use of a frame, then defers the release to that
  value. The producer's import implements `ReleaseTimeline`:
  `SetEventOnCompletion` sets an auto-reset event, which one persistent
  thread-pool wait (`RegisterWaitForSingleObject`) watches. Each wake fires every
  pending notification whose value has completed. Registration is lazy, so a
  fence that is only polled, such as producer readiness for retired pools, uses
  no wait. Dropping the fence unregisters the wait with
  `INVALID_HANDLE_VALUE`, which waits for any callback in flight.
- The design uses no keyed mutexes, and no `ID3D11Multithread` Enter/Leave or
  Flush-based sharing of one device between capture and consumers. That wcap
  pattern works only within one process. Each `D3d11Device` guards its
  immediate context with its own lock, and WGC frame callbacks use it only under
  the capture's lock.

### Abandoned fences (observed)

When the process that owns a fence's device exits, other processes read that
shared fence's completed value as `u64::MAX`. That value also satisfies GPU
waits on the fence, although the work it covered never ran.
`D3d11Fence::is_abandoned` reports this case.

- **Producer killed with a copy pending.** The producer queued a GPU wait on a
  gate ahead of staging and published, then was killed before the gate opened.
  The consumer's readiness fence read `0xffffffffffffffff`, and a consumer
  readback that GPU-waited on the frame's value completed instead of hanging.
  The pixels are the slot's old contents, so **a consumer must discard frames
  whose readiness fence is abandoned** and treat the producer as lost. The setup
  client's `is_alive` turns false.
- **Consumer killed with GPU work pending.** The consumer queued a readback of a
  frame behind a gate held by the test, signalled its release fence after the
  readback, deferred the release and was then killed. While the consumer lived,
  30 further frames never reused that slot, and its holding credit stayed
  charged. After the kill, with the gate still closed, the producer's import of
  the release fence read `u64::MAX`, and the reservation returned within 0.24 to
  0.48 ms. This is safe: a dead process's device can run no more work, and
  consumer imports are read-only.

A native claim held without a registered release at exit still follows the
common rule: it is quarantined with "lacking completion evidence".

## Adapters

`AdapterLuid` is `(HighPart << 32) | LowPart`. The grant carries the producer's
LUID. A consumer that attaches with a device on another adapter is refused with
`Refusal::AdapterMismatch { producer, consumer }`, and its message suggests a CPU
publication. The refusal admits nothing, and the same connection can attach
again on the right adapter. There are no cross-adapter copies. A consumer calls
`describe()` first and creates its device with
`AdapterSelection::Luid(producer.adapter.luid)`.

## Capture (`WgcCapture`)

The host creates the `GraphicsCaptureItem` for a window it selected and
authorized. The helper `capture_item_for_window` calls
`IGraphicsCaptureItemInterop::CreateForWindow`. The host passes the item as
`CaptureTarget::window_client_area(item, hwnd)`, or `CaptureTarget::item(item)`
to publish the whole frame. `CapturePolicy` carries:

- `cursor`: `IsCursorCaptureEnabled`.
- `border`: `Show`, `PreferHidden` (the default) or `RequireHidden`. For hidden,
  the capture requests `GraphicsCaptureAccess` `Borderless` and sets
  `IsBorderRequired(false)`. `RequireHidden` fails the start if either step is
  refused; otherwise `status.border_shown` reports the result.
- `min_update_interval`: `MinUpdateInterval`. If the system cannot apply it,
  `status.notes` says so.
- `output_size`: `Source`, `Fit { max_width, max_height }` (scale down only,
  keeping the aspect ratio) or `Fixed { width, height }` (fit the image with
  its aspect ratio, centred on black). Scaling is one draw with a runtime-compiled
  (`D3DCompile`) shader into a private texture, which is then copied into the
  slot.
- `adapter`, `publication` (`D3d11`, or `Cpu` on request), `arena` sizing, and
  an optional `DesktopMonitor`.

Jackstay owns the D3D11 device, a free-threaded `Direct3D11CaptureFramePool` of
two BGRA8 buffers, the session, the copy, fences and adapter identity.
`FrameArrived` runs on a thread-pool thread. It drains the frame pool to the
newest frame (older buffers go straight back), crops to the client area, and
publishes.

To find the client area, the capture compares `DwmGetWindowAttribute`
(`DWMWA_EXTENDED_FRAME_BOUNDS`) with `GetClientRect` and `ClientToScreen`, all
evaluated per-monitor DPI aware, whatever the host's DPI awareness.

Lifecycle:

| Event | Behaviour | Status |
| --- | --- | --- |
| content size changes | `Recreate` the frame pool at the new size | — |
| output size changes | arena reconfiguration: a new pool generation that consumers install. Held frames keep their old pool. | `size` |
| `Closed`, or window destroyed | publication stops. Terminal; nothing restarts it. | `Closed` |
| session locked or disconnected | frames are dropped and counted; the capture resumes when the desktop returns | `Paused(Locked \| Disconnected)` |
| device removed | once the desktop is available: a new device (default adapter, or the named LUID), frame pool, session and arena. The old publication is stopped. Retried at the watch interval for up to 300 attempts. | `Recovering`, then `Running` with `epoch + 1` and the new `mode`/LUID |
| shared fences unavailable | CPU arena. Frames are copied to a staging texture, mapped and published with `CpuCopyComplete`. | `mode: Cpu { reason }` |

In #23, "resize → new arena incarnation" became a reconfiguration generation,
because it keeps consumers attached, and a frame held across the resize stays
readable. Device loss replaces the whole arena (an epoch), because the fence,
device and LUID all change; consumers attach again, which re-imports on the new
adapter. The host serves `capture.publication()`: `serve_d3d11` for
`Publication::D3d11`, or `serve_cpu` for `Publication::Cpu`. It serves the new
publication again when `epoch` advances.

`SessionDesktop` reports the desktop as unavailable from
`WTSQuerySessionInformation(WTSSessionInfoEx)`: a connect state other than active
or connected, or the lock flag `WTS_SESSIONSTATE_LOCK`. A failed query counts as
available. A watch thread polls it every `watch_interval` (default 100 ms). The
same thread checks `GetDeviceRemovedReason` and the window's existence.

On Beaufort, WGC's `GraphicsCaptureItem.Closed` was not observed for a destroyed
test window. `WgcCapture` therefore also treats `IsWindow == false` as closure.
The note in the status records which signal ended the capture.

## Consumer obligations (for the C accessors, #29, and Wheelhouse, #68)

1. `describe()`, then create the device on that LUID, then `attach(holding, &device)`.
2. On `AcquireOutcome::Reconfiguration`, call `install_configuration`.
3. For a frame, `frame.native_resources::<SharedTextureHandle,
   SharedFenceHandle>()` borrows the NT handles for that generation. Open the
   texture once per pool slot and cache it by `pool_id`; open the readiness
   fence once per `fence_id`.
4. Check `ready.is_abandoned()`. Then `gpu_wait(fence_value)` and sample.
5. After the last GPU use of the frame, signal your registered release fence
   and call `defer_release(binding, value)`. If you have finished on the CPU,
   drop the frame instead (immediate release).

## Evidence (Beaufort, 2026-09-25)

Host: Windows 11 Pro 10.0.26200, Rust 1.98.1 MSVC. The runs were in RDP session
1 (`rdp-tcp#0`, active and unlocked for the whole run). DXGI enumerated three
adapters. D3DKMT device-interface lookup
(`D3DKMTOpenAdapterFromDeviceName`) identifies them:

| LUID | DXGI description | Device | D3DKMT type flags |
| --- | --- | --- | --- |
| `00000000:00009fe5` | AMD Radeon(TM) Graphics | `PCI\VEN_1002&DEV_1681` (the GPU; it also carries the RDP desktop output `\\.\DISPLAY257`) | render, display, POST, hybrid-integrated |
| `00000000:7d738999` | AMD Radeon(TM) Graphics | `SWD\REMOTEDISPLAYENUM\RDPIDD_INDIRECTDISPLAY&SESSIONID_0001`: the **Microsoft Remote Display Adapter** (an IddCx display, reporting its render GPU's description) | display, indirect display |
| `00000000:0000ba31` | Microsoft Basic Render Driver | `ROOT\BasicRender` | software |

**`ID3D11Fence` works on all three adapters,** including the Remote Display
Adapter:

- **Shared fence and texture across devices.** A shared fence was created,
  signalled from one device's context and GPU-waited on another. A shared
  texture was imported read-only and read back. Test:
  `every_adapter_shares_fences_and_textures_across_devices`.
- **Separate consumer process.** The full consumer-process scenario passed on
  each adapter.
- **Real capture.** A real WGC capture published and was verified on each
  adapter, with a 20 ms minimum update interval applied and a hidden border.
  Test: `capture_publishes_on_every_adapter_with_a_minimum_update_interval`.

By default, a capture's device goes on `9fe5`.

| Scenario | Test | Result |
| --- | --- | --- |
| pool preflight and charge bounds; unsupported layouts | `native_arena_windows` | pass |
| GPU-ordered import on a second device, crop, wrong size rejected | `native_arena_windows` | pass |
| reconfiguration keeps held frames; new size published | `native_arena_windows` | pass |
| deferred release returns credit only after gated consumer GPU work | `native_arena_windows` | pass |
| stopped producer drains its own gated GPU copy before shutdown | `native_arena_windows` | pass |
| setup over a pipe: generations, release registration | `native_arena_windows` | pass |
| adapter mismatch refused before admission, then attach on the right adapter | `native_arena_windows` | pass |
| synthetic producer → consumer process: resize, frame held across resize and ring wrap, 30 frames without reusing a slot with pending GPU work, consumer killed with GPU work pending (credit returned in 0.24–0.48 ms) | `native_arena_windows_process` (each adapter) | pass |
| synthetic producer process killed with a gated copy pending: consumer frames survive, liveness false, fence abandoned, GPU wait completes | `native_arena_windows_process` | pass |
| WGC window producer → consumer process: the same resize, hold and kill sequence | `native_arena_windows_process` (ignored by default) | pass |
| WGC window producer process killed with a gated copy pending | `native_arena_windows_process` (ignored by default) | pass |
| WGC in-process: client-area pixels, recolour, resize, injected lock pause and resume, simulated device loss (epoch 2, old publication closed), window close → `Closed` | `wgc_capture_windows` (ignored by default) | pass |
| output size `Fixed` (letterboxed) and `Fit` | `wgc_capture_windows` | pass |
| CPU publication is reported and carries the client area | `wgc_capture_windows` | pass |

The window tests capture only windows the test creates. Each window is small,
shown without activation and closed afterwards; the test process disables
Windows 11 rounded corners so its client area is uniform. They need an
interactive desktop:

```
cargo test -p jackstay --locked --features backend-windows --test native_arena_windows_process -- --include-ignored --test-threads=1 --nocapture
cargo test -p jackstay --locked --features backend-windows --test wgc_capture_windows -- --ignored --test-threads=1 --nocapture
```

CI runs build, test and clippy with `backend-windows`. On CI the process and
backend tests use whatever adapters the runner has. The window tests are
ignored there.

### Lock and RDP disconnect: not yet observed

Pausing is implemented and tested with an injected `DesktopMonitor`, and device
loss with `simulate_device_loss`. No real lock, RDP disconnect or device removal
was performed: a person is using Beaufort. A human-coordinated check should:

1. On Beaufort, in the RDP session that will be tested, run:
   `cargo run -p jackstay --features backend-windows --example wgc_session_watch -- --seconds 900 --log %TEMP%\wgc-watch.log`.
   It opens and captures only its own 320×200 window, recolours it every 0.5 s,
   verifies frames through an in-process consumer, and logs each status change.
   Each log line includes the raw WTS session state; each start and each new
   epoch also lists the adapters and their fence support.
2. **Lock**: press Win+L, wait 30 s, unlock. Record the log lines. Expected:
   `Paused(Locked)`, a "WGC delivered N frames while paused" line (N shows
   whether WGC keeps delivering while locked), then `Running` and
   `consumer: frame ... verified`. Note whether the epoch changes; it should
   not.
3. **RDP disconnect**: disconnect the RDP client (do not sign out), wait 60 s,
   reconnect from the same client. Record the log lines. Expected:
   `Paused(Disconnected)`, then either `Running` (same epoch) or
   `Recovering { device removed ... }`, then `Running` with `epoch + 1`. Record
   the adapter list after the reconnect: whether the Remote Display Adapter's
   LUID changed, and whether fences still report `ok`.
4. Optionally, reconnect at a different resolution or from another client to
   force an adapter change, and repeat step 3.
5. Attach the log to #28. A `Failed` state, frames that are not uniform after
   resume, or a missing resume counts as a failure.
