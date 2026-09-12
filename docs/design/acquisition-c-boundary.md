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

## Evidence and remaining work

The boundary tests cover process-bound CPU import, FD disposal on malformed
setup, duplicate credit, immutable bytes across 100 history wraps and consumer
destruction, misses/gaps, publication between snapshot and wait, cancellation,
and rejected/accepted deferred release through separate completion observers.
A compiled C translation unit also acquires, describes, reads and releases a
Rust producer's frame after consumer destruction and history wrap.

All six tests pass on macOS with `backend-macos` and Linux with `backend-linux`;
the five tests that do not require the compiled C shim also pass with Linux's
default features. macOS workspace build, default and macOS-feature Clippy, pinned
formatting, and the SDL smoke pass. The full workspace test run reports 124
passing library tests and the existing daemon shadow-ring regression failure,
then stops before running integration binaries. These checks do not establish
live capture or actual GPU completion at the C boundary.

This is an integration step, not completion of the acquisition contract. The
reference viewer and Porthole still use the legacy setup/data paths. Remaining
work includes C native setup and resource access, replacement configuration
import, viewer completion signaling, host migration and live acceptance. The
legacy daemon shadow-ring regression is intentionally still failing. The
independent Metal shared-event allocation blocker and native rerun commands are
recorded in [acquisition-native-setup.md](acquisition-native-setup.md).
