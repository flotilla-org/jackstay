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
The receiver checks it is a connected pipe. D3D11 slots will use the same
transfer once per incarnation (#28).

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
holds a claim. The arena suites' parent/child helper
(`tests/support/setup.rs`) now uses the pipe endpoint and handle transfer instead
of loopback TCP and parent-side duplication.
