# CPU acquisition setup over Unix sockets

`acquisition::socket` supplies the setup boundary for the common CPU arena on
macOS and Linux. Porthole's CPU host, recorder and SDL reference viewer now
use it in the coordinated worktree. The old daemon acquire/release protocol and
its Rust/C frame wrappers have been removed. Successful frames no longer depend
on the diagnostic ring entry remaining readable.

## Host and transport responsibilities

The host chooses and authorizes a producer before handing the connection to
`serve_cpu`. The transport core does not interpret Porthole session IDs, track
IDs, agent tokens or desktop permissions. A standalone producer can serve a
connection directly. Porthole first performs its routing/authorization exchange.
Those preface reads must not buffer bytes belonging to the subsequent setup
protocol. Only the setup object may then use the stream.

`serve_cpu` obtains the peer PID through `LOCAL_PEERPID` on macOS or
`SO_PEERCRED` on Linux. It binds the arena grant to that kernel-observed process
with `attach_process` before exporting mappings. No request contains a PID.
Each connection admits at most one incarnation. A refused reservation can be
retried with a smaller request; an admitted connection cannot attach again.

The server uses blocking I/O, including on macOS sockets accepted from a
nonblocking listener. The host can set read/write timeouts and keep a socket
clone for cancellation with `shutdown`. Network I/O never holds the producer
mutex. The host continues idle cleanup and retains its producer teardown owner
after the connection ends. Connection EOF closes acquisition; it does not
declare process exit or acknowledge mappings and held frames.

`CpuSetupClient::from_stream` is unsafe for the same reason as raw grant import:
the caller must trust the peer to follow the sole-producer memory protocol and
must not fork, forward or replay process-bound mappings. An authorization token
alone does not establish that trust. The supplied stream must use blocking I/O.

## Wire and mapping ownership

Each message has the `JSCPU001` version marker, a little-endian 32-bit JSON length
and at most 16 KiB of JSON. Exact reads leave the ancillary byte for `recvmsg`.
Initial admission transfers the common grant descriptor and five FDs;
replacement transfers a configuration descriptor and one resource FD. Received
FDs are made close-on-exec before import.

After sending ancillary data, the server drops its transport FD copies and then
sends a transfer-finished byte. The recipient waits for that byte before importing
the grant. This prevents a fast recipient from acknowledging mapping retirement
while sender FD copies still retain backing storage. A truncated transfer fails
instead of producing a usable consumer. As with other failed exports, EOF alone
cannot prove that escaped storage is safely retired; unresolved grants remain
subject to the arena's process watch and explicit recovery reporting.

The client checks both incarnation and random claim scope before replacement.
It skips a setup request when its installed generation is already current. The
XPC client uses this same local check. An allocation pause returns no offer;
after the host advances the transition, the consumer can install the replacement
while earlier leased frames retain their original maps and descriptors.

## Acquisition and verification

After setup, callers use `ArenaConsumer` and `FrameLease` directly for latest,
ordered or exact selection, holding limits, release and cancellable waits. There
are no per-frame socket messages and no socket lease table. CPU access follows
the arena's release contract; this setup protocol does not register a device
completion source for asynchronous GPU use of CPU mappings.

Tests exercise reservation rejection/retry, one attachment per connection,
100 history wraps with a held frame, consumer/connection teardown, old/new frames
under one reservation, foreign claim scopes and 100 replacement generations.
A separate child process proves that the socket's kernel PID is used, that EOF
does not reclaim its live CPU claim, and that confirmed process exit returns
admission. Transport tests reject oversized messages and an FD transfer missing
its completion marker. A capacity-pause test holds the old frame until the host
can allocate the replacement and the same connection resumes acquisition.

## Porthole clients and host lifetime

`daemon::ConnectedSession::connect(info, holding)` sends a bounded JSON preface:
`open_cpu_acquisition`, session ID, track ID and optional bearer token. Porthole
checks session ownership and lifecycle before replying `cpu_opened` and handing
the socket to `serve_cpu`. No grant is transferred on rejection. Exact preface
reads leave the subsequent binary setup message intact.

C ABI 0.4 adds `ft_acquisition_cpu_connect_session`, configuration installation
and setup destruction. It returns the same common consumer/frame handles as
raw CPU import. It removes `ft_consumer_connect_session` and its descriptor.
Rust removes `DaemonConsumer` and `DaemonFrame`; callers use `ArenaConsumer`
and `FrameLease` through `ConnectedSession`.

Porthole publishes copied CPU frames into an arena with eight resources, two
history positions, one producer reserve, four maximum incarnations and a
512 MiB allocation budget. The host retains a teardown owner after close and
runs cleanup and paused-reconfiguration advancement while capture is idle.
Draining waits for claims, mappings and setup owners; the five-second deadline
reports recovery required without force-reclaiming them. A cancelled startup
aborts its capture task. Status keeps a source failure after allocation retirement
and advertises the installed format while a replacement is pending.

The CPU viewer reserves two holds and releases its frame after `SDL_UpdateTexture`
returns. Presentation then uses SDL's texture. It handles empty acquisition,
reconfiguration, capacity and closure with common wait notifications and bounded
waits for its SDL event loop. The recorder reserves one hold, copies through its
movie writer before release, and waits against a snapshot taken before selection.
Ordered gaps retain the existing strict/best-effort recording behavior. Because
the movie writer has fixed settings, a configuration change ends the recording
with an explicit error requesting a new recording.

`acquisition_session` tests the authorized session preface, host rejection, history
wrap with a held frame, C configuration replacement and frames surviving both
API owners. The Porthole tests cover startup cancellation, retained failure
status, duplicate holding credit, consumer restart, recorder waits and gaps.
The separately invoked `cpu_viewer_e2e` test runs a built SDL viewer in two child
processes against an in-memory Porthole host and checks final retirement. These
are synthetic checks. Live CPU playback, delayed-consumer capture and native GPU
acceptance remain required. Metal shared-event allocation on kiwi remains the
recorded blocker for native runtime acceptance.
