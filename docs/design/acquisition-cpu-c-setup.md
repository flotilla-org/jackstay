# Generic CPU setup from C

C ABI 0.6 exposes the existing Unix CPU setup protocol to C and Zig clients on
macOS and Linux. A host can publish a stream and admit another process without
Porthole, grant JSON construction or raw mapping management in the connector.
The host creates the listener, selects the source and authorizes each connection.
Jackstay binds admission to the connected peer's kernel process identity.

## Host and consumer

The host creates an `ft_cpu_producer` with explicit resource, holding and memory
limits. For each authorized connected Unix stream, it calls
`ft_cpu_producer_serve(producer, &fd, &server)`. Success consumes `fd`, sets it to
-1 and returns a setup-worker owner. Success means the worker started; the peer
has not necessarily completed admission. One worker serves one consumer
incarnation. The host continues its serialized publish, replacement and cleanup
calls while the worker uses the same arena through internal locking. Socket I/O
never holds the arena lock.

The client connects to the host's selected setup endpoint and calls
`ft_acquisition_cpu_connection_create(&fd, &connection)`. This consumes the socket
without starting admission I/O. The client can register cancellation with its
worker before calling `ft_acquisition_cpu_attach(connection, holding, &consumer)`.
Attach blocks for setup and returns the common acquired-frame consumer. Reuse
`ft_acquisition_cpu_install_configuration(connection, consumer)` when acquisition
reports a pending replacement. No frame data travels over the setup socket.

A connector uses `ft_acquisition_snapshot`, acquire and `ft_acquisition_wait` for
frame readiness. Setup cancellation and acquisition-wait cancellation are
separate handles: cancel both when shutting down a worker that can wait in either
path. Copying a held CPU frame into KS-owned presentation memory is sufficient
for the first connector. Release the frame after the copy; borrowing instead
requires retaining it through every asynchronous use.

`ft_acquisition_cpu_connect_session` remains the convenience route for a trusted
Porthole-style host session. It returns the same connection/consumer owners and
can use the same replacement and destruction calls. Its combined selection and
attachment call returns no handle for cancellation while it is still connecting;
clients needing that control use the generic create/attach sequence after host
selection and authorization.

## Ownership, errors and cancellation

Both FD-taking calls require disjoint writable arguments, a null output handle
and a live, exclusively owned connected Unix `SOCK_STREAM`. Null arguments,
negative descriptors and occupied outputs are rejected without transferring the
FD. Once these basic checks pass, every outcome consumes the FD and sets it to
-1, including a wrong socket family/type or later initialization failure. A
nonnegative but already-closed descriptor violates the ownership contract.

The client must trust a conforming sole producer and be the original connected
peer. Callers must not retain descriptor copies, fork or forward mappings, replay
grants or perform their own setup-stream I/O. Each setup owner retains a private
socket duplicate solely to interrupt I/O with shutdown. Grant transfer still
uses the existing Rust protocol and its sender-FD retirement marker.

Serialize attach and configuration calls on a connection. They return ERROR for
protocol, I/O or admission rejection; the generic setup wire protocol currently
carries rejection text rather than typed C status codes. Output handles remain
null on failure. A failed attachment should be followed by connection destruction
and a fresh connection if retrying admission.

`ft_acquisition_cpu_connection_cancel` may overlap attach or configuration. It
permanently shuts down setup I/O; the interrupted call returns CANCELLED. All
callers must return before connection destruction. Consumer and acquired-frame
handles have independent lifetimes.

`ft_cpu_setup_server_poll` returns DRAINING while the worker runs, OK after an
orderly protocol EOF, ERROR after setup/protocol failure, or CANCELLED after
explicit cancellation. The host can poll between event-loop turns; it need not
block publication waiting for attachment. `ft_cpu_setup_server_cancel` can run
concurrently with poll. `ft_cpu_setup_server_destroy` cancels a running worker,
joins it, clears its handle and returns terminal status. It interrupts socket
I/O but can wait for an in-progress arena operation. Do not overlap destruction
with other calls on the same server handle.

Closing setup stops new acquisition for that incarnation. Held frames retain
their descriptor and storage until release. EOF does not prove that the consumer
process has died; verified process-death cleanup uses the existing arena rules.
`ft_cpu_producer_destroy` stops publication and admission, and returns DRAINING
while setup workers or outstanding storage owners remain. Stop and join setup
workers, finish consumer work, continue cleanup and retry producer destruction.
A timeout is never permission to discard its handle or force storage reclamation.

## Standalone acceptance

[The C test](../../tools/capture-viewer-sdl/tests/cpu_setup_smoke.c) uses only the
public header and Unix sockets. It starts the consumer through exec before any
Jackstay producer or mappings exist. The child connects itself, so admission
uses its actual PID. Separate test-control pipes coordinate publication and
replacement; the test never writes private messages into Jackstay's setup stream.

It holds an old frame across replacement, acquires the new allocation, closes
setup, verifies closure and both frames' contents, and releases them after
consumer destruction. A second run exits without releasing either frame and
checks that verified process-death cleanup permits producer retirement.

Run it with the existing CMake checks:

```sh
SDL_VIDEODRIVER=dummy scripts/smoke-viewer.sh
ctest --test-dir build/viewer --output-on-failure
```

`cargo test --locked --test acquisition_setup_ffi` checks blocked-attach
cancellation, partial-request server cancellation, FD-transfer errors, worker
retention before admission, EOF/error status and held-frame lifetime after close.
These are CPU transport checks. KS integration, input, audio, desktop permissions,
GPU imports and Windows setup need their own implementation and evidence.
