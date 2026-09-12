# CPU acquisition setup over Unix sockets

`acquisition::socket` supplies the setup boundary for the common CPU arena on
macOS and Linux. It is the transport needed to replace the older daemon
acquire/release protocol. Porthole and the CPU reference viewer have not yet
switched to it; the existing daemon shadow-ring regression remains unresolved.

## Host and transport responsibilities

The host chooses and authorizes a producer before handing the connection to
`serve_cpu`. Jackstay does not interpret Porthole session IDs, track IDs, agent
tokens or desktop permissions. A standalone producer can serve a connection
directly; Porthole can first perform its own routing/authorization exchange.
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

All four integration cases and both transport unit tests pass on macOS and
Linux. Workspace build, default/macOS Clippy, Linux Clippy and pinned formatting
pass; the offline SDL smoke acquires 30 frames. The macOS full suite passes every
integration binary and reports 126 passing library tests with the one known
daemon shadow-ring regression failing. The log is
`/tmp/jackstay-acquisition-socket-workspace-tests.log`. The XPC current-generation
check compiles, but its native runtime tests still await Metal shared-event
recovery on kiwi.

Remaining integration work is to publish Porthole CPU frames into the arena,
replace its per-frame socket handler with authorized setup, and migrate the
daemon Rust/C consumers, recorder and SDL viewer. Their live acceptance and
the macOS native acceptance blocked by shared-event allocation are still required.
