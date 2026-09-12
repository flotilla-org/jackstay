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

Setup version 7 includes the recipient PID in the descriptor and immutable claim
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

Each admitted incarnation has one cleanup owner, including local CPU consumers.
This lets consumer-initiated closure report a stalled local lease while the host
is idle. The sleeping worker polls the process descriptor alongside its existing
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

## Drain deadlines

`ArenaConfig::drain_timeout` is a required positive interval. Current fixtures use
five seconds unless testing expiry. The owner starts its monotonic deadline when
it observes closure; active consumers can hold their reservation indefinitely.
Host closure, consumer closure, and final quiescence acknowledgement wake the
owner. Closed-incarnation releases wake it too, so a final CPU lease can finish
cleanup without a further publication.

The worker sleeps until a handoff, process event, completion callback, or drain
deadline. Expiry adds a visible recovery failure but does not clear a claim,
acknowledge quiescence, return credit, or cancel native completion observation.
Once failure is reported there is no repeated expired-timer polling. A late valid
completion still returns credit; once claims and claim-page use finish, the
failure disappears and the next refresh/admission collects the incarnation.
Retry does not restart the grace period or hide expired, unresolved work.

The thread count remains bounded by the incarnation limit. Worker stacks and
handle bookkeeping are outside the resource/control-mapping byte budget, as
before. An owner exits after acknowledged quiescence and an empty claim page;
producer teardown also cancels and joins it without waiting for GPU completion.

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

Still required: reconfiguration with old and new allocations charged together, real GPU
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


The two deadline tests passed on macOS and Paneer Linux. One holds an active
consumer beyond the configured drain interval without failure, then closes it,
waits for background expiry, checks its bytes and credit through ring wrap, and
finishes reclamation after a late completion. The other drops a local consumer
while retaining a CPU lease: failure is reported without a host cleanup poll,
and dropping that lease permits a fresh incarnation. Updated arena, release,
wait, native arena, and deterministic concurrency tests passed on macOS; the
Linux cleanup/wait tests and all-targets clippy also passed. macOS workspace build,
macOS-feature all-targets clippy, and pinned formatting passed. These are targeted
checks; final full-suite and live acceptance remain required.


Consumer-local deferred retirement now has an additional bounded owner. The
consumer binds its own handle for the registered completion source and retains
its mapping until actual completion, even after the producer observer shuts
down. Release state 1 retains consumer use; state 2 acknowledges local mapping
retirement. On verified process exit the producer can establish state 2, but must
still observe actual external completion before reclaiming a deferred claim.
Library shutdown acknowledges quiescence after local retirement finishes; mapping
references and claims remain separate reclamation checks.

The consumer owner reports source failures and drain expiry through its binding
handle. API closure starts the deadline even when other ordinary leases survive;
final lifetime teardown and retries do not reset it. Late completion remains
observable and releases the mapping without either original API object alive.
The mapped-process release/crash tests now transfer an independently observable
controlled completion event at setup. These and three new retirement tests pass
on macOS and Linux; they do not prove native GPU command retirement after process
termination.
