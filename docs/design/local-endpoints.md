# Local Endpoints and the Windows setup channel

CPU acquisition setup, source bootstrap and input now run over named pipes on
Windows as well as Unix sockets. Wheelhouse
[ADR 0011](https://github.com/flotilla-org/wheelhouse/blob/84d3a46ee8419d38e8daefc8e5348eb973d5cd96/docs/adr/0011-windows-local-ipc-uses-named-pipes-with-logical-endpoints.md)
is the contract; this records how Jackstay meets it
([#27](https://github.com/flotilla-org/jackstay/issues/27)).

## Endpoints

`local::Endpoint` is a logical address: a scope (`User` or `Session`), a name
(1 to 64 ASCII letters, digits, `.`, `_`, `-`, not starting with `.`) and a
transport kind (`LocalStream`, the only kind so far). Each platform renders it
directly; nothing maps a path to a pipe name.

| Scope | Windows | macOS / Linux |
| --- | --- | --- |
| `User` | `\\.\pipe\jackstay.<user SID>.<name>` | `<runtime>/jackstay-<euid>/<name>.sock` |
| `Session` | `\\.\pipe\jackstay.<user SID>.s<session>.<name>` | same as `User` |

`<runtime>` is `$XDG_RUNTIME_DIR` when set, otherwise the temporary directory.
The per-user directory is created `0700` and must be a directory owned by the
user with no group or other access. macOS and Linux have no session boundary to
enforce, so `Session` equals `User` there, as the ADR allows.

`local::Listener::bind` creates the endpoint, `accept` returns a `Connection`
carrying the kernel-reported `PeerIdentity` (PID, user, and the Windows
session), and `local::connect` connects and verifies the server. A connection's
stream then feeds exactly one of `serve_cpu`/`CpuSetupClient`,
`bootstrap::accept`/`connect` or the input `Server`/`Client`, unchanged from
Unix. The host still authorizes the accepted peer and selects the source;
binding an endpoint grants nothing by itself.

## Windows security

A listener creates byte-mode pipes with:

- a protected DACL, `D:P(A;;GA;;;SY)(A;;GA;;;<SID>)`, where the SID is the user
  for `User` scope and the process's logon SID for `Session` scope;
- `PIPE_REJECT_REMOTE_CLIENTS`;
- `FILE_FLAG_FIRST_PIPE_INSTANCE` on the first instance. Any existing pipe of
  that name, whoever owns it, makes `bind` fail with `InUse`. A listener keeps
  one unconnected instance at all times, so the name is never released while
  it lives.

A client opens the pipe at `SECURITY_IDENTIFICATION` level, so the server can
identify but not impersonate it. Before any protocol byte it takes
`GetNamedPipeServerProcessId`, opens that process for query, reads its token
owner and requires the current user (and, for `Session` scope, the current
session). It then re-checks that the pipe is still connected to the same PID,
so the verified process is the server, not a reused PID. A mismatch is
`UntrustedServer`.

Accept takes `GetNamedPipeClientProcessId` and opens that process once with
`SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_DUP_HANDLE`, then
applies the same re-check. It reports PID, token user and
`GetNamedPipeClientSessionId`. A `Session` endpoint refuses a peer from another
session (`RefusedPeer`) and stays usable; its logon-SID DACL normally stops such
a process from opening the pipe at all, so the check is the second line.

## Streams

`PipeStream` behaves like a `UnixStream` for the protocols above. Every handle is
overlapped: a read or write waits on its own completion, a shutdown event and
an optional timeout, and settles (cancelling if needed) before its buffer goes
away. That gives blocking I/O with timeouts, a shutdown handle that interrupts a
blocked call from another thread (`CpuSetupClient` cancellation and C setup
cancellation), and a nonblocking mode for the input worker without
`PIPE_NOWAIT`. A nonblocking write the pipe cannot buffer at once stays in flight
with an owned copy of its bytes; `flush` reports `WouldBlock` until it lands, and
the input server waits for that before closing so its final cleanup
acknowledgement is not cancelled. Reads report EOF after the peer closes and its
buffered data is drained. `local::is_alive` peeks without consuming bytes: a
closed pipe (or a shut-down local end) is not alive.

`pipe_pair` makes the equivalent of a socket pair: a random, single-instance
pipe with the user DACL whose client end must be this process.

## Handle transfer and admission

`serve_cpu` admits the connected peer with `attach_process_handle`, using the
process handle opened at accept (or, for a stream not from a listener, opened
from the pipe's client PID while it is connected). No PID from a request is used.

Grants move with `local::send_handles`: each handle is duplicated into that same
peer process with `DUPLICATE_CLOSE_SOURCE`, so the producer's copy is closed
before the peer can learn any value, then the values are sent (a `u32` count and
`u64` values) and the peer acknowledges with one byte. This replaces the Unix
marker byte sent after the sender's FD copies drop: either way the recipient can
never acknowledge mapping retirement while the sender still holds storage. If
sending the values fails, the duplicates are closed again inside the peer, which
cannot have seen them. A missing acknowledgement fails setup; handles the peer
may already own stay with it and die with it.

Each handle gets the least access its import needs
([process cleanup](acquisition-process-cleanup.md)): `FILE_MAP_READ` for the
control and resource sections, `FILE_MAP_READ | FILE_MAP_WRITE` for the claim
page, `SYNCHRONIZE | EVENT_MODIFY_STATE` for the consumer event and
`EVENT_MODIFY_STATE` for the producer event. A replacement configuration sends
one read-only section. The consumer adopts handles only from a server that
`connect` verified, under `CpuSetupClient::from_stream`'s existing contract.

Bootstrap's optional input channel is one end of a `pipe_pair`, duplicated into
the verified peer the same way, within the bootstrap's five-second deadline.
The receiver checks it is a connected pipe. D3D11 pool textures and fences use
the same transfer once per consumer incarnation and pool generation
([D3D11 backend](acquisition-d3d11.md)).

## C ABI 0.9

`capture_transfer.h` adds `ft_local_endpoint` (scope, transport, name) and
`ft_peer_identity` (PID, session, user string), with
`ft_local_listener_create`/`accept`/`cancel`/`destroy`, `ft_local_connect`,
`ft_local_connection_peer`, `ft_local_connection_alive` and
`ft_local_endpoint_render`. New statuses: `FT_STATUS_ADDRESS_IN_USE` (19) for a
taken endpoint and `FT_STATUS_UNTRUSTED_PEER` (20) for a failed owner or session
check.

A connection is consumed by exactly one `_local` setup call, which nulls the
caller's handle after basic argument checks, as the FD calls set -1:

| Host | Client |
| --- | --- |
| `ft_source_bootstrap_accept_local` (hands the connection back) | `ft_source_bootstrap_connect_local` (hands it back) |
| `ft_cpu_producer_serve_local` | `ft_acquisition_cpu_connection_create_local`, then the existing attach, cancel and configuration calls |
| `ft_input_target_serve_local` | `ft_input_client_connect_local` |
| a Rust `serve_d3d11` host | `ft_acquisition_d3d11_connection_create_local` (ABI 0.10, [D3D11](acquisition-d3d11.md#c-abi-010)) |

`ft_acquisition_cpu_connection_alive` reports whether the producer still holds
its end of setup (OK, CLOSED, or CANCELLED after cancellation) without consuming
setup bytes, so a consumer no longer peeks at a borrowed descriptor to notice a
vanished producer. It answers OK while another setup call owns the connection.

The acquisition, producer, bootstrap and input APIs are now declared and built on
Windows. The FD-taking setup calls (`ft_cpu_producer_serve`,
`ft_acquisition_cpu_connection_create`, `ft_source_bootstrap_accept`/`connect`,
`ft_input_target_serve`, `ft_input_client_connect`) and the Porthole daemon's
`ft_acquisition_cpu_connect_session` remain POSIX-only. Raw grant import takes
`ft_os_object`: an `int32_t` FD on POSIX (unchanged) or a `HANDLE` on Windows,
set to `FT_OS_OBJECT_NONE` once consumed.

On Windows the build compiles `c_abi_header_smoke.c` with MSVC in C11 mode, and
`acquisition_ffi` runs its C translation unit against a Rust producer.


## C ABI 0.11: a host's own exchange

A host may run its own exchange on a connection before the setup call that
consumes it, for example to present a host-issued attach token and read which
publication follows. Porthole's Windows native capture sessions work this way:
the consumer sends one JSON line with the session's attach token, reads one
reply line, and Jackstay's D3D11 or CPU setup then runs on the same stream.

`ft_local_connection_write` sends bytes and `ft_local_connection_read_until`
reads up to and including a delimiter byte, one byte at a time, so it never
consumes the start of setup. Jackstay adds no framing and interprets nothing.
Both take a nonzero timeout in milliseconds that bounds the whole call,
however the peer paces its bytes, and hand the stream back as it was.
After a failure (CLOSED, TIMEOUT, CAPACITY) the
stream position is unknown and the caller destroys the connection. The calls
are transport-neutral: the host protocol, not Jackstay, decides what the bytes
mean, so no host's authority model enters the transport core.

They share one helper with the bootstrap preface, `local::Bounded`: a stream
whose operations are all bounded by one absolute deadline. On Unix it makes
the socket non-blocking and `poll`s for the time left before each operation,
touching no socket option, and puts the socket back to blocking at the end.
On Windows each pipe operation runs with the pipe's timeouts set to the time
left, and the previous timeouts are restored at the end. Its unit tests cover
a deadline across many partial reads, a closed peer, and the stream's mode
being handed back.

## Evidence

Windows unit tests cover rendering, identity reported both ways, a taken name
(our own listener, and a squatter's default-DACL pipe) failing with `InUse`,
remote-client rejection (the loopback SMB path to the endpoint is denied while a
control pipe with the same DACL but no reject flag accepts it), cancelled
accepts, stream timeouts, shutdown, closure, nonblocking writes three times the
pipe buffer, and handle transfer with restricted access.

The owner check is tested two ways: against the RPC endpoint mapper's pipe
(`\\.\pipe\epmapper`), a real server run by a service account, and against our
own server with an injected expected owner (SYSTEM). Refusing a peer or server
from another Windows session is tested with an injected identity: creating a
second logon session is outside what the test can do. A `Session` endpoint
accepting its own session is tested live.

`acquisition_socket`, `bootstrap` and `input_transport` now run on Windows,
including a child process admitted through a pipe listener and killed while it
holds a claim. So do `acquisition_ffi`, `acquisition_producer_ffi`,
`acquisition_cpu_server_ffi`, `bootstrap_ffi` and `input_ffi`, with `_local`
counterparts of the FD tests (argument rejection versus consumption, blocked
attach cancellation, partial-request server cancellation, EOF versus protocol
failure, liveness). `acquisition_session` stays POSIX-only: it drives the Porthole
daemon's Unix session protocol, which has no Windows implementation.

`local_ffi_process` runs a producer and a consumer in separate processes using
only the C entry points. The consumer child connects by endpoint, verifies the
server's PID and user, bootstraps required input and attaches; the host checks
the accepted peer is that child. They exchange a frame and text input; the host
resizes the allocation and the consumer installs the replacement while holding
the old frame; the host cancels setup and the consumer observes closure through
liveness while its frames stay readable; the consumer presses a key and is
killed. The host then sees the input target run the controller's disconnect
cleanup and retire, and the producer, which refused destruction while the
consumer lived, reclaims the dead process's claims and is destroyed. In the
second test the producer child is killed while the consumer holds a frame: setup
liveness turns CLOSED, the held bytes stay intact, and the input client closes
without confirmed cleanup. A third test runs a host exchange (ABI 0.11) in
one process: a token line and reply, then CPU setup and a frame on the same
connection; a reply without the delimiter (CAPACITY); a silent host (TIMEOUT,
after which the host sees CLOSED); and a host trickling bytes, which cannot
stretch a 300 ms call. The arena suites' parent/child helper
(`tests/support/setup.rs`) now uses the pipe endpoint and handle transfer instead
of loopback TCP and parent-side duplication.
