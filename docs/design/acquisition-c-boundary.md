# Acquisition across Rust and C

The common arena now has an ownership boundary in `ffi_acquisition.rs`, declared
in `capture_transfer.h` under ABI 0.3. It uses `ArenaConsumer`, `FrameLease`,
`Cancellation` and `ConsumerReleaseTimeline` directly. It has no C lease book.

## Setup and ownership

A Rust host can transfer an admitted consumer with
`FtAcquisitionConsumer::into_raw`. A C process can import an authorized CPU grant
using `ft_acquisition_import_cpu`: versioned `GrantDescriptor` JSON and the five
owned setup FDs from `RemoteConsumerGrant::into_parts`. The producer must obey
the shared-memory protocol and bind the grant to the recipient process. This
does not introduce a Porthole authorization requirement.

The C import consumes all five FDs after basic argument validation, including
when parsing or mapping fails, and writes -1 into the caller's array. A caller
must not retain other transport copies, replay a grant or fork its mappings.
Native setup must install its resource handles with the grant; it cannot use
the CPU import to omit them.

Every successful acquisition owns one opaque frame handle, including duplicate
acquisitions of one published frame. The handle retains the original descriptor,
mapping and any native attachment independently of the consumer API handle.
Destroying the consumer closes acquisition and starts draining; it does not
invalidate held frames or claim their use has finished.

The descriptor is the same 144-byte `repr(C)` value used by Rust, including
configuration generation, storage identity, producer readiness and damage/drop
metadata. CPU bytes are borrowed from the frame owner. Immediate release consumes
and nulls the frame handle and requires all use to have finished.

A host transfers an already bound completion observer with
`FtAcquisitionReleaseTimeline::into_raw`. Deferred release transfers the lease to
the existing retirement owner. Success nulls the C handle; rejection preserves
its exact address and ownership. Pending work retains its completion observer
even after the C consumer and binding handles are destroyed. Actual completion
still determines mapping retirement and acquisition credit.

## Selection and waits

Latest and next modes select after a cursor; exact mode selects that cursor.
Empty, miss, gap, holding limit, reconfiguration and closure remain distinct
statuses. Miss returns the requested cursor as a singleton range; a gap returns
an inclusive published range. An unsuccessful selection transfers no frame.
Acquire outputs must start null, preventing accidental overwrite of a live handle.

Call `ft_acquisition_snapshot` before checking acquisition, then pass that
snapshot to `ft_acquisition_wait` if necessary. Data and capacity interests are
selectable; reconfiguration and closure always wake. Cancellation is one-way,
thread-safe and does not release frames. Consumer calls require external
serialization; cancellation handles must outlive all concurrent callers.

## Configuration replacement

`ft_acquisition_install_cpu_configuration` consumes a single resource FD and its
`ConfigurationDescriptor` JSON for the existing incarnation. Installed and stale
offers have distinct results. A stale offer is disposed before another offer can
be accepted; contradictory metadata or mappings remain errors. Existing frame
handles keep their original descriptor and storage, and installing a new
configuration adds no holding credit.

During a capacity pause, `ft_acquisition_relinquish_configuration` drops the
consumer's unleased current mapping. Leased and deferred frames retain their own
mapping owners. The host explicitly retries allocation; relinquishing alone
cannot make still-held storage reusable. The arena stamps `config_generation`
from the installed resource allocation at publication, including for CPU frames,
so the acquired descriptor agrees with its setup grant.

## Evidence and remaining work

The boundary tests cover process-bound CPU import, FD disposal on malformed
setup, duplicate credit, immutable bytes across 100 history wraps and consumer
destruction, misses/gaps, publication between snapshot and wait, cancellation,
and rejected/accepted deferred release through separate completion observers.
A compiled C translation unit also acquires, describes, reads and releases a
Rust producer's frame after consumer destruction and history wrap.

Two further tests cover stale/current replacement offers with old and new frames
held together, and a capacity pause that cannot finish until an old C frame is
released. The Rust replacement test also checks the arena-stamped configuration
generation. These checks do not establish live capture or actual GPU completion
at the C boundary.

All eight boundary tests, the arena suite and the replacement suite pass on
macOS with `backend-macos` and Linux with `backend-linux`. Backend-feature Clippy
passes on both. The SDL smoke acquires 30 frames. The macOS default workspace
test run with `--no-fail-fast` passes every integration binary and reports just
the known legacy daemon regression among its library tests (124 passed, one
failed). The full log is `/tmp/jackstay-acquisition-c-workspace-tests.log` on kiwi.

The [native C bridge](acquisition-native-setup.md#c-native-bridge) adds named XPC
connection, native replacement, frame-scoped resource access and actual Metal
completion-event registration. Its C/GPU tests compile but await recovery of
Metal shared-event allocation before runtime verification.

This is an integration step, not completion of the acquisition contract. The
native reference viewer now uses this boundary and retains frames through Metal
completion; its runtime verification remains pending. The CPU viewer and Porthole
still use legacy setup/data paths. Remaining work includes host migration and live acceptance. The
legacy daemon shadow-ring regression is intentionally still failing. The
independent Metal shared-event allocation blocker and native rerun commands are
recorded in [acquisition-native-setup.md](acquisition-native-setup.md).

The [CPU socket setup boundary](acquisition-cpu-setup.md) now transfers the common
arena's process-bound grants and replacement maps without per-frame messages.
Its Rust client returns `ArenaConsumer` directly. Connecting it to Porthole's
authorization/routing and the CPU Rust/C consumers is the next integration step;
the new transport alone does not fix the older daemon path.
