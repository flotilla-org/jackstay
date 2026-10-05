# jackstay-producer

This crate owns the repetitive lifecycle of a native CPU producer. It uses
Jackstay's Local Endpoint, bootstrap, input executor and CPU arena APIs. It adds
no transport, renderer, source discovery, host chrome or desktop authority.

Construct `Builder` with a Local Endpoint, `ArenaConfig`, input `Config`, and a
`Producer` callback implementation. `start` binds the endpoint and returns a
`Source`. `stop` cancels accepts, interrupts and joins setup/media workers,
stops input servers, completes the input cleanup barrier, and drains media
before freeing the owners. Dropping the source follows the same sequence.
Errors in input cleanup or media draining are returned from `stop`.

Fatal pump errors stop the source internally; `stop()` retrieves their error.
`is_finished()` reports when the pump has ended after teardown; poll it to
detect a source that ended on its own, then call `stop()` to retrieve the result.
`Drop` completes shutdown but discards its result. Both worker joins are
completed before any error is returned, including after an accept-thread panic.

The endpoint uses an owner-only Unix runtime directory/socket or a Windows
private named pipe. Endpoint access is scoped to the current user/session;
the host still selects and authorizes the source. The builder input config
specifies supported typing modes and capabilities; semantic callbacks confer
no input authority. Connections are bounded to eight by default; use
`max_connections` to change the limit. A silent bootstrap cannot hold shutdown.
At the limit, newly accepted connections are closed without negotiation. A peer
continues to count after media EOF while input or affordances remains alive.
Finished peer records and threads are reaped on the next accept or shutdown;
the connection limit bounds this retained bookkeeping.
Each accepted connection runs the explicit v2 bootstrap on a worker bounded
by the existing five-second handshake deadline. Successful media streams keep
their original peer identity and go to the CPU setup server.

Callbacks run serially on the source pump, roughly every five milliseconds.
Each channel worker also polls at five milliseconds, so callback delivery can take about ten milliseconds or longer
with scheduling/callback delays; this is polling rather than event driven.
They must return promptly. Callback panics are caught, input execution is
completed as uncertain where needed, and ordered shutdown still runs. Cleanup
callbacks are attempted even after a panic; a failed cleanup is reported.
`frame` returns owned bytes and a complete descriptor, or `None` for unchanged content. Size/stride changes
reconfigure CPU storage; capacity pauses skip that publication and retry on
later frames. `recycle` receives consumed frame storage after publication or
capacity-paused discard, once the arena no longer borrows it. Pool these buffers
to avoid allocating a pixel `Vec` on every frame. The default callback drops them.
`input_size` maps reconfigured pixel dimensions to logical input dimensions; its
default returns the pixel size. Scaled renderers can return their logical viewport.
Capacity pauses can call `input_size` more than once for the same resize; keep
it cheap and idempotent.
The initial allocation keeps the builder geometry; a first frame exceeding its
capacity also calls `input_size` during reconfiguration. Only a change in logical dimensions advances the input geometry revision.
`input_geometry` optionally reports the current logical viewport on every pump
turn, before input execution, even when `frame` returns `None`. Direct arena
producers use it after applying a viewport change. Changed dimensions advance
the revision and trigger the normal pointer cleanup barrier; unchanged dimensions
retain the revision. Invalid dimensions end the pump with an error.
`execute` receives `Work`, including cleanup: return `Executed` only after
actual execution/release. Return `Uncertain` for execution uncertainty. Failed
cleanup is reported, never silently treated as successful release.

`snapshots` supplies complete changed domain state. Return an empty `Vec` on
unchanged ticks (it allocates no heap storage); snapshots need only allocate
when application state changes. The toolkit retains the latest state, publishes every supported domain on a new connection, and sends
changes thereafter. Return `Snapshot::Withdraw(domain)` to remove a domain;
omitting it from a later callback leaves its last snapshot intact. Snapshot
publication is validated by Jackstay. Invalid snapshots are rejected
instead of enabling invalid state. Input, media and affordances have
independent lifetimes; media EOF does not imply input cleanup or close controls.
`affordance` receives host presentation snapshots, enabled verbs, and closure.
Presentation withdrawal implies defaults: visible true, focused false, scale 1,
no preferred size. Hints can be ignored and never imply media pause or admission.

The producer owns semantic execution and clamping. `media.seek_absolute.position`
must be finite and nonnegative; clamp valid seeks to the available range.
`media.seek_relative.offset` is finite and signed. `scroll.set_position.position`
is finite and signed, then clamped to the current axis range by the producer.
No snapshot value is silently clamped. Unsupported, disabled or withdrawn verbs
are ignored **before** validating their bodies, including malformed bodies.
Malformed enabled verbs close only affordances. `navigation.load.url` is
**untrusted host input**: the callback owns URL interpretation, allowed schemes,
file/network access and any other URL policy.

Stop waits for live media leases to retire. The host must drop consumers and
leases when abandoning a source, or concurrent shutdown may wait until the
arena's drain timeout reports failure. Media EOF alone is never process-death
proof. Host verbs are not replayed or acknowledged; enqueue success is not
execution proof. If implemented through input, callbacks must use an admitted
controller and obey its epochs, ordering, cancellation and cleanup barrier.

Run `cargo run -p jackstay-producer --example minimal` for a ten-second
platform-independent CPU source. It proves toolkit integration without changing
SDL reference acceptance tests. Lifecycle tests use real source consumers,
including a killed child process; no desktop capture is involved.
