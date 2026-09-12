# Process-bound acquisition cleanup

The arena's cleanup owner now observes process lifetime as well as registered
release timelines. This implements CPU crash reclamation and native quarantine;
it does not yet finish the contract's recovery and reconfiguration slice.

## Binding before handoff

`attach_process(holding, pid)` installs a kernel process watch before allocating
and exporting the recipient's grant. It returns `RemoteConsumerGrant`, which can
only export its descriptor and four setup FDs. The safe local constructor accepts
`ConsumerGrant`; it cannot accept a remote grant tied to some other process's
exit. `attach` remains the local constructor, and its grant cannot export FDs.

The host must obtain the recipient PID from its trusted setup connection, not
from a client-supplied identity field. That is a transport provenance requirement,
not an authorization policy assigned to Jackstay. XPC and Unix-socket host
integration still need to supply that binding.

Setup version 4 includes the recipient PID in the descriptor and immutable claim
header. Import rejects a different PID. The unsafe import contract additionally
requires the exact process lifetime admitted by the sender: a restarted process,
a reused PID, or a child inheriting handles cannot adopt an old grant. A new
process needs fresh admission, a new mapping, and a new incarnation. The kernel
watch stays attached to the originally observed process; cleanup never looks up
a PID later and treats its absence or replacement as proof.

## Exit evidence

On macOS, `EVFILT_PROC` attaches its knote to the process object found during
registration. The implementation requests `NOTE_EXIT` and requires a successful
registration receipt. Registration captures subsequent events; it is therefore
installed before any grant can escape. XNU's exit path is entered by the last
terminating thread, detaches the BSD process from its task, and then emits
`NOTE_EXIT`. This supports reclamation of ordinary CPU access. It does not prove
completion of GPU commands or other declared asynchronous uses. See Apple's
[kqueue filter implementation](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/kern_event.c#L1099)
and [process exit path](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/kern_exit.c#L1635).

On Linux, the owner opens a pidfd with flags zero, without `PIDFD_THREAD`. Its
readability means the last thread in the group has exited; hangup means the
process was reaped. The descriptor remains the lifetime witness after PID reuse.
This follows the Linux [pidfd_open documentation](https://www.man7.org/linux/man-pages/man2/pidfd_open.2.html).
Unsupported process observation is an admission error, never a fallback to PID
polling or connection EOF.

The sleeping cleanup worker polls the process descriptor alongside its existing
handoff/completion wake channel. A host can also call `poll_cleanup`; both paths
serialize through the same incarnation mutex. A worker retains the process FD
while sleeping, even if a concurrent refresh consumes the exit event. Joining
and cancelling the worker use the existing wake channel.

## What each observation permits

| Observation | Cleanup behavior |
| --- | --- |
| Local library shutdown acknowledgement | Mark claim-page access quiescent; retain any pending deferred claims. |
| Control connection EOF / explicit close | Stop new acquisitions; keep claims and reservations while use may continue. |
| Bound process exits, ordinary CPU claims | Mark claim access quiescent and return those claims, including claims interrupted before validation. |
| Bound process exits, submitted deferred claims | Keep claims and imported completion sources; reclaim only after the named value actually completes. |
| Bound process exits, native or asynchronous claims without a submitted release | Retain storage and reservations; report `RecoveryRequired`. |
| Completion source fails | Close the incarnation and retain claims; an explicit retry can observe a repaired source. |

Registering a release timeline opts a CPU incarnation into conservative
asynchronous crash handling. Registration must happen before starting such use.
A process might die after submission but before publishing its deferred-release
metadata, so an ordinary-looking claim in that incarnation is no longer proof
of CPU-only use. Native resource arenas are always conservative, even when no
release timeline was registered.

`cleanup_failures` reports unresolved native claims as well as observer errors.
Retrying observation does not invent a missing release signal; it still reports
`RecoveryRequired` for those claims. Pending registered completions can continue
to drain while other claims remain quarantined. A failed internal observation
channel cannot be repaired by retrying a backend. No timeout authorizes reuse.
Acknowledged empty incarnations are collected during publication, admission, or
an explicit cleanup refresh. Until then their allocations remain charged.

## Evidence and remaining work

The CPU crash test closes the child's control socket while the child still owns
a mapped lease, verifies that admission remains blocked, then kills the child
with `SIGKILL`. It repeats this three times while another consumer retains its
original bytes through hundreds of publications. These crashes bypass Rust
lease drops and shutdown acknowledgements. Another test kills a child after it
submits deferred release, verifies that exit and an earlier timeline value do
not return its reservation, and completes cleanup only at the registered value.
That test uses a controlled external timeline; it does not prove a Metal command
buffer survives process death.

The native crash test allocates real IOSurfaces. A child acquires a native claim
and dies without a release. The parent verifies quarantine, rejected replacement
admission, and continued healthy-consumer publication. Metal samples the original
pixels from both the quarantined surface and a distinct healthy lease after ring
wrap. The child does not submit GPU work in that test; it proves conservative
retention, not backend retirement of commands from a dead process.

Still required: bounded reporting when a valid submitted completion never
arrives, reconfiguration with old and new allocations charged together, real GPU
work across consumer death, and the host/C API replacement and live acceptance.
There is no force-reclaim or restart-in-place operation for unproven native work.

Validation at this checkpoint: the two crash tests passed on macOS and on Paneer
Linux, using real child processes. The existing seven arena, eight deferred-release,
and four wait tests also passed on both platforms, including their explicitly
invoked subprocess helpers. Three native arena tests passed with `backend-macos`,
including the new real-IOSurface quarantine test. The three deterministic arena
concurrency tests, macOS workspace build, macOS-feature all-targets clippy, Linux
default all-targets clippy, and pinned formatting passed. The Linux checks ran in
`/tmp/jackstay-acquisition-apjY1L`; they did not modify an installed daemon or
existing checkout. The full suite is still not claimed clean: the old local
socket/shadow regression awaits replacement of that data path.
