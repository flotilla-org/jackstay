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
silently dropped. Initial code need not coalesce motion.

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

The transport uses version-1 length-prefixed JSON messages internal to the Rust
implementation; C/Zig callers do not implement that encoding. It bounds a frame
to 128 KiB and queued wire bytes to 512 KiB, independently of configured event
queues. Text commits are at most 16 KiB of valid UTF-8, including embedded NUL;
the frame bound covers worst-case JSON escaping. Queue byte accounting charges
96 bytes plus UTF-8 payload per event; it is not a promise about total allocator
usage. Held keys and result queues are bounded too. Motion coalescing is not yet
implemented; overflow terminates visibly instead of losing transitions.

Each connection has a worker with bounded nonblocking I/O and heartbeat handling.
The initial implementation polls at 5 ms; readiness/wakeup optimization can follow
without changing execution semantics. Missing peer traffic expires the controller
independently of video progress. Application execution may still stall; expiry
cannot complete its in-flight work or claim cleanup on the executor's behalf.
The transport is currently Unix-only; the Rust state model builds on Windows.

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
