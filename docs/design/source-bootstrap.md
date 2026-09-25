# Shared source bootstrap

ABI 0.8 adds one connection setup step before the existing CPU media setup.
A source host exposes one endpoint. A client connects once and requests
observation, optional input, or required input. The host has already selected
and authorized the source; it supplies that source's authorized input target,
or no input target. Jackstay creates no listener, registry or permission policy.

The bootstrap preserves the original connected Unix stream for CPU setup.
`serve_cpu` must see the actual consumer's kernel PID when granting mappings;
a server-created socketpair cannot substitute for that connection. Optional
input is established on a separate socketpair whose client end crosses the
bootstrap connection via descriptor passing. Its server and client use the
existing shared input protocol, admission and cleanup implementation. The sender
retains its descriptor copy until the receiver acknowledges ownership over the
bootstrap stream, then closes that copy so it cannot mask controller death.
This receipt is part of the same five-second bootstrap deadline.

One endpoint therefore establishes two independent channel owners. Media uses
its existing frame pool and setup stream. Input uses its ordered duplex stream,
execution acknowledgements and cleanup barrier. Closing or stalling media alone
does not close input; neither channel's data is multiplexed behind the other.

## Host and client

Rust hosts call `bootstrap::accept(stream, authorized_target)` on a setup worker.
Its `Accepted` result contains the original media stream and an optional input
`Server`. Keep the input server alive independently while serving CPU media on
the returned stream. An input server handle means its worker started, not that
controller admission completed. Keep pumping the target executor after close.

Rust clients call `bootstrap::connect(stream, InputRequest)` on a worker. Its
`Connected` result contains the media stream, an admitted input `Client` when
available, and any clean optional-input rejection. Feed the media stream to
`CpuSetupClient::from_stream` under that API's process/sole-producer contract,
then attach. Use the input client's welcome/configuration and event operations
as before.

The C equivalents are in `jackstay_bootstrap.h` (ABI 0.9 adds
`ft_source_bootstrap_accept_local`/`connect_local` for
[Local Endpoint](local-endpoints.md) connections on every platform):

- `ft_source_bootstrap_accept(&fd, authorized_input, &input_server)` precedes
  `ft_cpu_producer_serve(producer, &fd, &media_server)`.
- `ft_source_bootstrap_connect(&fd, request, mode, &input, &input_status)` precedes
  `ft_acquisition_cpu_connection_create(&fd, &connection)` and CPU attach.

These functions temporarily consume `fd`. After basic validation, any failure
closes it and leaves -1. Success returns the same descriptor in `fd`, ready for
media setup. Handle outputs start null; arguments must not alias. No caller
copies, concurrent stream operations, or forwarding of media grants are allowed.

`NONE` uses mode zero and never creates an input server or controller. `OPTIONAL`
and `REQUIRED` use one existing input mode. Optional clean refusal preserves
media and reports why: unavailable/unsupported input, busy target, or failed
cleanup, for example. It is not silently reported as admitted control. Required
refusal closes setup and returns the error. Protocol errors, lost descriptors
and uncertain transport failures fail the entire bootstrap in either mode.

Bootstrap negotiation has a five-second absolute deadline with fixed-size
messages. Input admission then has its existing five-second bound. Run these
calls off GUI/input threads; there is no asynchronous cancellation handle for
this bounded setup step. The resulting media stream is blocking with its prior
socket timeouts unchanged. CPU attachment and replacement retain their existing
cancellable connection API. Ancillary descriptor transfer currently uses the
existing one-shot fd-passing helpers: interrupted or failed `sendmsg`/`recvmsg`
calls fail setup, even when retry might succeed. A caller can establish a fresh
connection after such a failure; no partially negotiated connection is reused.

Bootstrap is not an atomic media-plus-input attachment. It negotiates independent
owners before CPU admission. If CPU attach later fails and the host abandons the
source, close and poll the input client for cleanup, then destroy it. Destruction
alone never confirms cleanup. Server failure or dropping a server schedules the
existing target cleanup barrier; it does not bypass or resolve it. Do not destroy
the host's target until its input servers have stopped and cleanup has settled.

## Reference and acceptance

The interactive reference source now takes one path:

```sh
build/viewer/capture-input-source /tmp/jackstay-source.sock
build/viewer/capture-viewer-sdl --source-socket /tmp/jackstay-source.sock
```

The viewer requests optional cooperative input by default. `--observe` never
requests it; `--require-input` fails rather than attaching without it. The source's
`--observe-only` option withholds input authority. The direct `--cpu-socket` and
`--input-socket` options remain low-level adapter test paths, not the combined
source interface. On Windows the input channel is a private pipe-pair end
duplicated into the verified peer ([Local Endpoints](local-endpoints.md)). This
bootstrap does not yet extend native GPU setup or the cross-host bridge.

Public Rust/C tests cover independent channel lifetimes, observer admission,
optional/required refusal, busy targets, malformed offers, stalled negotiation
and FD ownership. A concurrent handoff regression exercises descriptor lifetime
through input admission and event completion. Separate C source/viewer processes use the single endpoint
for frames, keys, text, graceful cleanup and abrupt controller death. Observation
cases prove frames remain available without creating an input controller.
