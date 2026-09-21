# SIGPIPE during Wheelhouse source recovery

Wheelhouse issue: https://github.com/flotilla-org/wheelhouse/issues/37

On macOS, stopping a source while its input client was flushing a heartbeat
could terminate the C host with SIGPIPE. The standalone Wheelhouse session test
failed with `AssertionError: ('recovery', -13)` both locally and in CI.

## Cause

A temporary send interposer captured `EPIPE` in this path:

```text
Jackstay input transport Client worker
  drive_client
  Framed::flush
  UnixStream::write
  send
```

The sending worker had SIGPIPE blocked. A diagnostic handler nevertheless ran
on the main thread, interrupted in `usleep`. Darwin's socket error path uses
`psignal(p, SIGPIPE)` unless the socket or send suppresses the signal, so a
worker-local signal mask is not sufficient to protect the process.

The optional bootstrap input socket originates in `UnixStream::pair` and reaches
the consumer through descriptor passing. In Rust 1.98, the Darwin socket-pair
constructor sets close-on-exec but does not set `SO_NOSIGPIPE`; importing a
descriptor also does not add that option. Rust executables normally ignore
SIGPIPE at startup, masking this failure in ordinary Rust tests. C hosts need
not ignore it.

References:

- [Darwin socket send error handling](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/uipc_syscalls.c#L1309-L1313)
- [Rust 1.98 socket constructors](https://github.com/rust-lang/rust/blob/1.98.0/library/std/src/sys/net/connection/socket/unix.rs#L59-L132)

## Fix

Set `SO_NOSIGPIPE` on Apple sockets at input/bootstrap admission and when creating
or importing frame wakeup writers. Protect acquisition setup writes too.
Descriptor sends configure the same socket option and use `MSG_NOSIGNAL`, which
also protects raw `sendmsg` on Linux. Fail setup/write if socket configuration
fails. No process-wide signal disposition or host thread mask is changed.

The regression runs the real input framing code in a dedicated subprocess with
SIGPIPE restored to its default disposition, closes the peer, and requires
`BrokenPipe` from the write. It failed with signal 13 before the fix and passed
afterward. The child process keeps this signal change out of the parent test
runner and unrelated tests.

## Validation

- The original standalone Wheelhouse suite reproduced the failure.
- A recovery-only loop failed on its third attempt before the fix.
- The patched library passed ten consecutive recovery attempts and then the
  complete Wheelhouse session suite, including recovery without input reacquisition.
- `cargo build --workspace --locked` passed.
- `cargo test --workspace --locked` passed.
- `cargo clippy --workspace --all-targets --locked -- -D warnings` passed.
- `cargo +nightly-2026-03-12 fmt --check` passed.
- Workspace tests and Clippy also passed with `--features backend-macos`.
- `SDL_VIDEODRIVER=dummy bash scripts/smoke-viewer.sh` passed with 30 acquired frames.

The SDL smoke test is synthetic; this investigation did not exercise live desktop
capture. Linux runtime validation remains for CI. The Wheelhouse recovery loop
used the unmodified C consumer and source against the patched library, without
the temporary signal handler or interposer. No diagnostic instrumentation is
included in the patch.
