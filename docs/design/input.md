# Shared input: first cooperative implementation

The host authorizes and supplies a connected transport for one selected target.
Jackstay owns bounded ordering, controller incarnations, interaction epochs,
liveness and the execution/cleanup barrier. The executor runs on its own thread
or event loop and reports completion. No desktop authority belongs in Jackstay.

The initial reference uses the existing SDL viewer and synthetic CPU producer.
KS owns both later adapters: its media publisher executes returning input and
its media presenter sends input upstream. Porthole native execution follows.
The input worker is independent of frame acquisition and presentation.

## Agreed semantics

Typing policy is fixed at admission: destination-interpreted physical input,
source-committed text, or cooperative key events plus text. Repeat belongs to the
destination in physical mode and to the source in the other modes. Executable
capabilities constrain each target; presence in the vocabulary is not support.
Physical DOM code, logical key/shortcut, and UTF-8 commits are separate intents.
Logical Control+C does not implicitly become Command+C or accessibility Copy.

One controller is admitted per target. Local input stays enabled. Focus loss
invalidates queued input and releases held state while retaining assignment.
Disconnect, expiry, overflow or partial/uncertain execution ends assignment.
A replacement waits for cleanup completion; timeout cannot establish release.
Clean pre-execution rejection leaves the session usable. Never replay input after
uncertainty. Policy changes require cleanup and a new controller incarnation.

A press has an opaque identity. Release/repeat use the binding made by its down,
even if key meaning changes. Executors retain actual native bindings themselves.
Cleanup concerns controller-owned input, not globally clearing the keyboard.
Independent local/remote contributions and real interaction cancellation are
explicit capabilities. Native release can commit a drop; cleanup is not undo.

Geometry revisions describe target logical extents. Stale new positional actions
are rejected; releases remain valid. Geometry changes end pointer holds, not
keyboard holds. Pending pointer work is invalidated, in-flight work settles and
pointer cleanup runs before further execution. Text has a bounded UTF-8 byte
length independent of SDL's text event buffer. Scroll has fractional deltas and
explicit units. Queues are bounded in bytes and events; transitions are never
silently dropped. Consecutive queued motions coalesce to the latest position.

### Scroll units

Controllers choose the scroll unit from the device's reported semantics:

- Precise/continuous devices (trackpads, Magic Mouse, and high-resolution wheels
  reporting pixel deltas) send `Pixel` in the target's logical units: the same
  coordinate space as `Geometry` and pointer positions, not device pixels.
- Notched wheels send `Line`, one unit per notch: Windows wheel delta / 120,
  X11 buttons 4/5 emit `y = -1` (up) / `y = +1` (down), buttons 6/7 emit
  `x = -1` (left) / `x = +1` (right), and macOS non-precise
  devices use the line delta. For native positive-up sources (Windows
  `WM_MOUSEWHEEL` delta / 120 and macOS positive-up line deltas), negate the
  vertical delta to make it positive-down. The X11 values above are already
  normalized. Preserve the platform's natural-scrolling setting; do not
  reapply an inversion already included in the platform event.
  Fractions are allowed; controllers do not round.
- `Page` is only for explicit page-scroll gestures, never synthesized from wheels.

Positive `y` scrolls content toward its end (down); positive `x` scrolls toward
its right. Controllers preserve platform deltas as delivered in content direction,
including the user's natural-scrolling setting. Never negate deltas because of
`SDL_MOUSEWHEEL_FLIPPED` or another device-inversion flag. For the SDL reference,
convert SDL's positive-up `y` to positive-down by negating `y`; SDL's
positive-right `x` keeps its sign. This coordinate conversion is independent of
the device-inversion flag.
This sign handling does not determine the unit: precise/continuous device
pixel deltas use `Pixel`, and notched-wheel deltas use `Line`.

The executor owns the line height and page size and converts the received units
as its platform requires. Controllers never pre-multiply line or page deltas by
those sizes.

Input wire v2 carries independent phase, momentum and inversion metadata.
Unknown metadata preserves phaseless delta execution.

### Planned scroll metadata

[ADR 0003](../adr/0003-scroll-metadata.md) records this contract, implemented in
Jackstay [#94](https://github.com/flotilla-org/jackstay/issues/94) with ABI 0.14
and wire v2. Luchs native execution remains a separate follow-on.

Add these independent fields to Rust `Event::Scroll`:

| Field | Rust shape | Meaning when absent |
| --- | --- | --- |
| `phase` | `Option<ScrollPhase>` | The source cannot report the physical gesture phase. |
| `momentum_phase` | `Option<MomentumPhase>` | The source cannot distinguish momentum from direct input. |
| `inverted_from_device` | `Option<bool>` | The source cannot report whether its preference reversed device direction. |

`ScrollPhase` has `None`, `MayBegin`, `Began`, `Stationary`, `Changed`, `Ended`
and `Cancelled`. `Stationary` requires both deltas to be zero.
`MomentumPhase` has `None`, `Began`, `Changed` and `Ended`.
These are closed, platform-neutral enums, not native integer masks. A present
`None` means the source knows that this event has no phase in that domain;
Rust `Option::None` means unknown. Likewise, `Some(false)` differs from an
unknown inversion bit. Unknown metadata preserves phaseless delta execution;
it never authorizes inventing a beginning, ending or momentum timer. Controllers
map an unrecognized native phase, including an unsupported combination, to
absence for that field while retaining the deltas and other known fields.

Gesture and momentum phases are independent. A physical `Ended` can accompany
momentum `Began` in one event; do not split or reorder it. Momentum `Ended`
closes momentum without inventing a new physical gesture. Zero deltas are valid
for every phase, particularly `MayBegin`, `Ended` and `Cancelled`; these events
must pass through capture, admission, transport and execution even when there
is no content movement. Metadata applies to every scroll unit, not just `Pixel`.

The direction policy is the accepted design in
[#88](https://github.com/flotilla-org/jackstay/issues/88). Normalize platform
coordinates to Jackstay's positive-right/down content direction, retaining the
platform's natural-scroll preference. Neither controller nor executor negates deltas because of
`inverted_from_device`, `SDL_MOUSEWHEEL_FLIPPED` or the destination's preference.
The new bit is information for native/application APIs that expose it; it never
changes the existing meaning of `x` and `y`.

#### Ordering, geometry and cleanup

Keep scroll events uncoalesced, including repeated `Changed` events and unknown
phases. Only consecutive queued pointer motions continue to coalesce; no queue
may sum scroll deltas, replace a scroll event or cross a scroll boundary. This
also preserves inversion-bit changes and the last delta before an end. An
overflow remains a visible assignment-ending failure with executor-confirmed
cleanup, rather than dropping an end event to fit the bound. `Event::bytes` in `crates/jackstay/src/input/model.rs` charges 112 bytes per event
plus key/text payload: the three new C fields
occupy 12 bytes, rounded to a 16-byte layout increase on 64-bit targets. This
remains a queue accounting allowance, not a promise about allocator usage.
Apply the new charge consistently to target and client queues, retaining their
event count and wire framing bounds. `Target::new`'s minimum
`Config::max_bytes` in `input/session.rs` is 112; smaller configurations
must fail construction, not pass construction and fail their first event.
The C target constructor shares that Rust validation. Existing configurations
with a byte limit of 96 through 111 need a larger limit when adopting the new
ABI; do not silently enlarge a caller's configured queue.

A geometry change **ends the destination gesture**, even when the next stale
event is its end. It uses the existing pointer cleanup barrier: discard queued
pointer work, let any in-flight operation settle, cancel the executor's active
controller-owned scroll interaction, then acknowledge cleanup and publish the
new epoch/geometry. Cleanup covers gesture and momentum as well as pointer
buttons, even if no button is held. Keyboard holds remain intact. The executor
retains its actual scroll recipient and last successfully executed native
binding until cleanup; cancellation must not hit-test the new geometry or
deliver stale deltas to a different view.

Stale positional scroll events, including zero-delta end events, are rejected
under the existing geometry rule. Rejection does not execute the stale event,
replay it with a new revision or close the viewer. The target's geometry-change
barrier already owns cancellation. The target owns the active epoch, cleanup
barrier and per-epoch gate for known scroll continuations. The gate guards
admission/reset races only; terminal events do not close it. Executors track
and close physical and momentum activity independently. On successful
cleanup completion it advances that epoch and closes the gate; the controller
learns the new values from `Status::Reset`. An `Error::Stale` rejection itself
changes neither the epoch nor the gate and never schedules cleanup, whether
the client rejects locally or the target rejects a submitted operation. This extends
[#89's viewer behaviour](https://github.com/flotilla-org/jackstay/issues/89),
not its fatal-error policy.

For example, a gesture begins at epoch 7 / geometry 10. Setting geometry 11
creates pointer cleanup while epoch 7 is still active; submissions during the
barrier retain the existing busy/stale rejection rules. Once cleanup succeeds,
the target publishes epoch 8 / geometry 11 with its scroll gate closed. An end
tagged epoch 7 is stale because of its epoch; an old end tagged epoch 8 but
geometry 10 is stale because of its geometry. An orphan `Changed` retagged
epoch 8 / geometry 11 is stale because the gate is closed. None schedules a
second cleanup. A fresh `MayBegin` or `Began` at epoch 8 / geometry 11 opens
the gate; a late epoch-7 end after that start still cannot cancel the new
gesture. The target's epoch and gate, not a controller-side guess that cleanup
has completed, distinguish these cases.

At admission and after reset, a phase-aware controller discards any already
running source gesture and its momentum until a fresh physical `MayBegin` or
`Began`.
It must not relabel an old `Changed` as a new `Began`. The target enforces the
same rule at admission and after its cleanup barrier, rejecting orphan physical
continuations and momentum starts/continuations cleanly as stale until a fresh
physical start. With the gate closed and no fresh physical `MayBegin` or
`Began` in the same event, reject physical `Ended`/`Cancelled` and momentum
`Ended` as stale too, including when both deltas are zero. A momentum `Ended`
with physical `None` is an orphan terminal event, not an accepted no-op.
These rejections never schedule cleanup or alter the gate/epoch.
Unknown/known-unphased wheel input can resume after reset using current
geometry, with no inferred interaction. In particular, present
`Some(ScrollPhase::None)` with `Some(MomentumPhase::None)` is unphased and may
be admitted while the gate is closed, just as a sample with both fields absent
may be admitted. Normal epoch, geometry and input validation still apply.
These samples neither open the gate nor create a phased native binding.
There is at most one remote scroll interaction per controller/epoch; no device
or gesture ID is added. A fresh physical start terminates any prior momentum
before starting the new interaction. Executors keep physical and momentum
activity separately so that the combined `Ended`/`Began` handoff stays active.

All-state cleanup on focus loss, disconnect, expiry,
overflow or uncertain execution also closes scroll state. For Luchs, send
zero-delta `Cancelled` to a live physical gesture and zero-delta momentum
`Ended` to active momentum at the retained recipient, then clear the binding.
Do not manufacture momentum on cancellation. Cleanup completion means those
operations have settled, not merely that they were enqueued. Failure retains
the existing quarantine rule; cancellation cannot claim to undo a navigation
or content change that has already committed.

#### Source and executor mappings

On macOS, translate `NSEvent.phase` and `momentumPhase` by named values.
Native `.none` maps to present `None` for each field. Read
`isDirectionInvertedFromDevice` directly, including `false`. The existing SDL
filter can read metadata alongside precise deltas for nonzero wheel events,
but it is insufficient for terminal events:
[SDL2's `SDL_SendMouseWheel`](https://github.com/libsdl-org/SDL/blob/SDL2/src/events/SDL_mouse.c)
returns before queueing when both deltas are zero. The implementation needs
window-scoped native scroll capture before that drop, on the AppKit event
thread, putting value-only events into SDL's ordered queue. It must suppress
the corresponding SDL wheel event to avoid duplicates and preserve ordering
with focus, keys and geometry resets. Test both SDL2 and sdl2-compat; merely
extending `precise_wheel` in the current filter does not satisfy acceptance.

The [portable SDL2 wheel struct](https://wiki.libsdl.org/SDL2/SDL_MouseWheelEvent)
has no gesture or momentum phase. Without native capture, send both as unknown.
The SDL viewer leaves all native metadata unknown for plain SDL wheel events,
as scoped in #94; the native observation path reports the inversion bit directly.
Do not infer phases from fractional deltas or idle gaps. Synthetic adapters
lacking native information also send all fields absent.

Windows precision touchpads delivered as ordinary wheel messages also send
unknown phases. Microsoft's
[precision-touchpad input overview](https://learn.microsoft.com/en-us/windows/win32/input-precisiontouchpad/precision-touchpad-portal)
describes default wheel promotion and opt-in gesture APIs. A future native
adapter may translate verified direct-input/inertia lifecycle callbacks, but
this slice does not infer a macOS lifecycle from wheel timing, touch contact
count or a Direct Manipulation viewport state alone. Report inversion only
when the source API explicitly supplies it. No Windows-specific adapter is
required by this implementation slice.

For a future direct libinput adapter, map the first nonzero finger-scroll
sample to physical `Began`, subsequent samples to `Changed`, and the guaranteed
zero-valued stop to `Ended`. Track horizontal and vertical activity separately;
a stop on one axis ends the gesture only when neither axis remains active.
An absent axis is not a stop. This is an adapter-defined sequence boundary,
not proof of first touch-down. Use present momentum `None` because libinput
does not generate kinetic scrolling; never add a momentum timer in Jackstay.
Wheel and unclassified continuous sources remain phaseless in this slice.
These mappings follow the
[libinput pointer API](https://wayland.freedesktop.org/libinput/doc/latest/api/group__event__pointer.html).
Its natural-scroll configuration can supply the inversion bit only when the
adapter controls that configuration and knows it applies to the delivered
deltas; otherwise leave the bit unknown. Existing portable SDL input still
uses the fallback above. A direct libinput adapter is not added by this slice.

For Luchs, the [macOS spike](../scroll-metadata-spike-2026-10-06.md) confirms that
public CoreGraphics fields preserve physical and momentum phases through
`NSEvent(cgEvent:)`, provided the executor translates the enums. CoreGraphics
and AppKit raw values differ. `Stationary` has no public `CGScrollPhase` value;
Luchs treats that physical component as a no-op rather than issuing a fake
phase. A stationary-only sample completes as `Executed` for its sequence but
neither creates nor advances the retained native recipient or last successful
native position used for cleanup. Any supported momentum transition in the
same event still executes at the retained binding before completion; the
physical no-op must not swallow a momentum end. Shared pre-admission validation
rejects nonzero stationary samples as specified below; they never reach Luchs.
Unknown phase fields become native `.none` without guessing.

No supported inversion setter was found in the public SDK, and the tested
candidate fields did not carry it through conversion. Luchs therefore keeps
the informational bit in its command data but does not claim to expose
`Some(true)` as WebKit's native inversion property. Execute the deltas and
supported phases anyway; do not reverse them, use private field numbers,
subclass `NSEvent`, or inject a separate DOM wheel event to compensate. This
documented executor limitation does not change the transport's optional bit.

#### ABI and implementation acceptance

Public ABI **0.14** carries all three fields together, replacing 0.13.
`ft_abi_version()`, headers, layouts and consumer checks use the same version.

Append three `uint32_t` fields to `ft_input_event`, in this order:
`scroll_phase`, `scroll_momentum_phase`, `scroll_inverted_from_device`.
Their C encodings are:

| Field | Values |
| --- | --- |
| `scroll_phase` | 0 unknown, 1 none, 2 may-begin, 3 began, 4 stationary, 5 changed, 6 ended, 7 cancelled |
| `scroll_momentum_phase` | 0 unknown, 1 none, 2 began, 3 changed, 4 ended |
| `scroll_inverted_from_device` | 0 unknown, 1 false, 2 true |

Zero-initialized events thus retain unknown metadata. These are constants,
not C enums in the struct and not bit masks. Reject out-of-range values as
invalid before queue admission; other event kinds require all three to be
zero. Validate `Stationary`'s zero-delta requirement in the shared semantic
validation (`input::session::validate`) that the local `Client::send` and
target `Controller::submit` both use before queue admission. Rust callers can
construct an `Event` with nonzero stationary deltas, but submission returns
`Error::Invalid`; no constructor-side guarantee is implied. C decoding checks
the scalar enum encodings, then client send runs the same semantic validation.
Wire decoding checks enum shape; target submission checks deltas and phases.
Every entry path thus rejects the invalid event before execution, without
changing the live scroll binding or scheduling cleanup.

On 64-bit targets `ft_input_event` grows from 152 to 168 bytes and its
embedding `ft_input_operation` from 192 to 208 bytes. Update Rust `repr(C)`
layouts, C static assertions, FFI conversions, C/Zig consumers and all exact
version checks together. This is an ABI break, not an append-only promise to
old callers. No compatibility shim or second inversion-only bump is planned.

Use input wire protocol **version 2** in `Hello` and `Welcome`, preserving the
existing framing and bounds. Version 1 peers must fail connection before
submission, rather than accepting deltas while silently ignoring the new
lifecycle fields. Serialize the Rust enums and independent optional values;
the encoding remains internal to Rust, with no new C-side JSON implementation.

The follow-up implementation must verify:

- Rust, local transport and C round trips for absent versus known-none/false,
  all supported enum values, the combined handoff and invalid C values; C/Zig
  layout checks and mismatched wire/ABI versions. Nonzero stationary samples
  must return invalid through Rust controller submission, local client send,
  C send and structurally valid wire submission, with no work dispatched or
  mutation of an existing scroll binding. Zero-delta stationary samples must
  round-trip and complete the Luchs no-op while retaining that binding. Send
  a stationary-only sample at a different valid position and verify that its
  successful completion does not move the cleanup recipient or native position;
  a stationary sample with momentum `Ended` must still dispatch that end once
  at the retained binding.
- Ordered zero-delta starts and ends, repeated changes with no coalescing,
  pointer-motion coalescing on either side without crossing the scroll, and
  overflow cleanup while gesture or momentum is active.
  Configurations with `max_bytes` 96 or 111 must fail target creation in Rust
  and C; 112 must permit one payload-free scroll event in both bounded queues
  when otherwise valid, and a second event must overflow visibly if neither
  has been drained.
- Resize racing a dispatched change or queued end, rejection without viewer
  shutdown, cancellation at the retained recipient, separate keyboard holds,
  suppression of the old gesture tail, and a fresh gesture after reset. Include
  focus loss, disconnect and failed cleanup through the public contract.
  Explicitly exercise the epoch-7 / geometry-10 race above: complete cleanup,
  reject an old-epoch end, reject a current-epoch stale-geometry end and a
  current-epoch orphan change, then begin the new gesture and reject another
  late old-epoch end. Observe exactly one cleanup, no extra reset or epoch
  advance, and an intact new interaction; also cover a local client rejection
  before it receives `Reset`.
  With the gate closed, submit current-epoch/current-geometry zero-delta
  physical `Stationary`, `Changed`, `Ended`, `Cancelled`, and momentum `Ended`
  with physical `None`. Each must be rejected as stale, with no dispatched
  event, cleanup, reset or gate change. Present physical `None` with present
  momentum `None`, and a sample with both fields absent, must be admitted at
  current epoch/geometry without opening the gate or creating a phased binding;
  then verify that a fresh physical start still succeeds.
- macOS native capture through SDL2 and sdl2-compat with no duplicate deltas,
  the enum translation below the Luchs helper protocol, and zero-delta cleanup.
  Keep a platform-independent fixture for the portable unknown-phase fallback.
  Physical-device acceptance separately checks rubber-band release, CSS snap,
  momentum interruption and navigation, plus #88's natural-scroll cases; the
  CGEvent conversion spike does not prove those WebKit behaviours.
  When the combined implementation lands, change ADR 0003's status from
  proposed to accepted.

## Implementation sequence and verification seams

1. Public Rust target/controller interface: admission, validation, ordered work,
   completion, cancellation, cleanup failure and geometry. Test observable work
   and outcomes at this interface, including an already dispatched down racing
   with cleanup. No tests of internal queues or mutexes.
2. Host-supplied Unix stream transport and public C interface. Exercise separate
   processes, peer exit, idle liveness independent of video, byte bounds, UTF-8
   and owned handle teardown. Rust and C share the same target implementation.
3. Extend the SDL viewer and synthetic producer for cooperative input. Inspect
   visible and programmatic state, then retain existing media/ABI acceptance.
4. Give KS's agents the matching contract and integration cases for both ends.

The agreed test seams are the shared input contract/public C interface and the
existing viewer/source process connection. A cooperative reference executor is
an actual supported target, not a replacement for permission-gated native tests.
Native desktop support, comprehensive logical layout translation, full IME,
relative pointer, audio and graph arbitration remain outside this first slice.

## Rust and C integration

`Target::admit` (or a host-authorized `Server`) gives one controller exclusive
remote assignment. `Controller::submit` admits ordered work; `Target::next`
returns at most one in-flight operation. `Target::complete` is the executor's
completion point. Keep polling through cleanup after disconnect. A failed cleanup
quarantines the target until `resolve_failed_cleanup` records an explicit host
resolution. Dropping a Rust Target reference is not proof of cleanup; C target
destruction preserves its handle while draining or quarantined.

`jackstay_input.h` exposes the same implementation in ABI 0.7. The host creates
an input target and supplies each authorized Unix stream to
`ft_input_target_serve`. The execution thread takes a work handle, describes its
borrowed event data, applies it and completes the handle. Text pointers borrowed
from work expire at completion. Asynchronous executors must retain the work until
their dispatched operation has actually settled. A cleanup operation carries
controller ID, the negotiated typing mode, scope and reason; retain each press's actual application/native
binding in the executor until released. An already-released button-up is a
successful no-op and never reaches the executor.

The controller uses `ft_input_client_connect`, then `send` and `poll`. Connect
has a five-second startup bound and belongs off the input/render thread. Send
copies the event and text into a bounded local queue. Its success is not execution
completion. Poll returns per-sequence completion/rejection, epoch/geometry reset,
or closure with whether cleanup was confirmed. Graceful `close` waits for no work
itself; poll observes cleanup completion. `destroy` ends and joins the network
worker without pretending the peer executor has cleaned up.

The host must associate media and input endpoints with the same selected target
and authorize both. Supplying an input socket path alone is not proof that it
controls a particular media publication. The library starts no public listener
and uses no Porthole credentials. The example publishes private same-user sockets;
it is a single-viewer reference, not a discovery/authorization service.

Configuration advertises supported mode and event-family bits, geometry, limits,
and executor guarantees. Family support does not claim every physical key or
logical mapping is supported. Unsupported mappings can still be cleanly rejected.
The modifier mask is source metadata, not an extra held-state owner. Repeats and
releases resolve through the original press identity; current modifier metadata
is preserved. Logical requests are literal key combinations, not cross-platform
application commands. Physical identifiers use DOM code names. Unknown physical
positions must remain logical/text input rather than guessed codes. The SDL
reference currently maps common keyboard positions and logs unmapped keys.

Every executor receives DOM `KeyboardEvent.code` names on the wire for
physical-mode key events and owns their translation to its platform's key identity
(SDL scancode, `NSEvent.keyCode`, Windows virtual key or scan code, or `xkb`
keycode). An executor that cannot translate a code completes the operation as
unsupported, reported to the controller as a rejection, and never guesses;
the SDL logging above concerns source positions that cannot be encoded for sending.
Translation tables belong to the executor, not the library or
the C ABI.

The transport uses version-2 length-prefixed JSON messages internal to the Rust
implementation; C/Zig callers do not implement that encoding. It bounds a frame
to 128 KiB and queued wire bytes to 512 KiB, independently of configured event
queues. Text commits are at most 16 KiB of valid UTF-8, including embedded NUL;
the frame bound covers worst-case JSON escaping. Queue byte accounting charges
112 bytes plus UTF-8 payload per event; it is not a promise about total allocator
usage. Held keys and result queues are bounded too. Only consecutive queued
`Event::Motion` entries coalesce: the tail is replaced with the latest validated
position and sequence, charging one event and 112 bytes. In-flight work is never
replaced, and button, key, scroll, text and cleanup boundaries are never crossed.
The last motion before a transition is retained. Existing cancellation still
invalidates queued motions on focus loss or geometry change; an in-flight motion
settles before cleanup.

`Status::Coalesced { count }` reports superseded accepted operations. Adjacent
unread coalescing statuses aggregate in both target and client result queues,
including across transport worker ticks; each count is a delta, not a lifetime total.
This is cheaper than per-sequence superseded completions: one pending status
covers an arbitrarily long uninterrupted burst without retaining sequence IDs.
A presenter can subtract the count from its outstanding operations; the final
motion still receives ordinary completion or rejection. C polling exposes
`FT_INPUT_COALESCED` with the count in `ft_input_status.sequence`, with no signature
or layout change. Non-motion overflow still terminates visibly instead of losing
transitions. Transport framing and producer-side queue bounds are unchanged.

Each connection has a worker with bounded nonblocking I/O and heartbeat handling.
After requesting graceful close, the client stops sending heartbeats and keeps
receiving until it observes the final cleanup result or a transport failure.
This lets it read the acknowledgement even if the peer has already closed its
socket; a new write at that point could fail before the result is read.
The initial implementation polls at 5 ms; readiness/wakeup optimization can follow
without changing execution semantics. Missing peer traffic expires the controller
independently of video progress. Application execution may still stall; expiry
cannot complete its in-flight work or claim cleanup on the executor's behalf.
The transport runs over Unix sockets and, on Windows, over named pipes
([Local Endpoints](local-endpoints.md)); the protocol is unchanged.

## Reference acceptance

The CTest producer/viewer test uses two executables, checks key down/repeat/up,
Unicode and a 1 KiB text operation, leaves a modifier and mouse button held, and
requires executor-confirmed cleanup. A second run kills the viewer only after
the source reports the held state, and verifies process-death cleanup. Rust tests
cover in-flight cancellation, geometry cleanup, quarantine, stale epochs,
clean rejection, repeat ownership, overflow, heartbeat expiry and worst-case
escaped text. The C layout test and C/Zig compilation check the public interface.

On this development machine, SDL2 is supplied by sdl2-compat. Its `Event2to3`
returns null for pushed SDL2 text input and its `SDL_PushEvent` passes that to
SDL3. The test therefore feeds its text fixture into the ordinary viewer event
translator; keyboard and pointer fixtures still traverse SDL's queue. This
checks our input adapter/transport, not SDL's synthetic text-queue implementation.
Live text continues through ordinary SDL event polling.
[Upstream conversion](https://github.com/libsdl-org/sdl2-compat/blob/main/src/sdl2_compat.c).


ABI 0.8 adds [source bootstrap](source-bootstrap.md): the host can associate and
authorize media plus optional input through one public endpoint. The input
protocol and independent execution/cleanup worker remain unchanged.
