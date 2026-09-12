# Bounded acquisition reconfiguration

Status: CPU transitions, local and cross-process replacement setup, mapping
retirement, admission-book accounting, and deferred CPU mapping retention are
implemented. Native setup handles now share the acquired resource lifetime; native
pool replacement remains unfinished.

## Separate control lifetime from resource lifetime

The original arena put wait epochs, publication history, descriptors, and inline
CPU payloads in one mapping. Keeping that mapping to wait for new configuration
also keeps its old allocation alive. If old and new allocations cannot coexist
within budget, that creates a cycle: the consumer needs the old mapping to wait,
while the producer needs it gone before allocating the replacement.

The persistent control mapping is now separate from resource mappings.
Control holds terminal/reconfiguration state and publication notifications. A
resource generation owns its descriptors, resource states, CPU payload storage,
or imported native pool handles. Configuration transfer is a setup operation;
frame acquisition and release remain shared-memory operations.

Separate incarnation lifetime from mapped configuration lifetime in the consumer.
The claim page and its quiescence acknowledgement belong to one shared lifetime
owner across all configurations. Switching the current resource mapping must not
acknowledge shutdown while old frame leases still exist. A returned lease owns
its original immutable descriptor, resource generation, and incarnation lifetime.
All generations use the same holding slots, so old plus new holdings cannot
exceed the incarnation's reservation.

Deferred release now retains the consumer's resource mapping independently of
both public API owners. Before asynchronous use, the consumer binds its local
completion handle to the producer's registration with `bind_release_timeline`.
`FrameLease::defer_release` consumes that binding rather than bare setup metadata.
Both handles must observe the same actual completion source.

Each incarnation has one local retirement worker with H pending slots and at most
H bound sources. Notifications wake a condition variable; the worker rechecks
actual completion, with no timed polling while active. It retains the original
consumer mapping, unmaps it before acknowledging local retirement, and keeps at
most one outstanding callback per slot. Resource owners hold the claim page, not
the incarnation lifetime, so pending resources cannot form an ownership cycle.

Deferred-release state 1 means the consumer still owns the pending use. State 2
acknowledges local mapping retirement. Only after state 2 and independently
observed producer-side completion does the producer return holding credit.
Verified process exit also supplies local mapping-retirement proof, but does not
replace producer-side GPU completion. The local worker can complete and release
its mapping even after the producer's observer has been destroyed.

Consumer API shutdown starts its drain deadline even if another ordinary frame
lease remains alive. Final lifetime teardown and retries do not reset that
interval. Binding handles expose pending local releases, cleanup failure, and
explicit retry without keeping completed resources alive. Expiry reports failure
and retains the mapping; late actual completion still retires it. Native setup now attaches surface and readiness handles to that same resource
owner through `NativeArenaGrant::into_consumer`. `FrameLease::native_resources`
borrows the acquired slot and readiness handle from it. The owner destroys those
handles before acknowledging mapping retirement, including on the deferred path.
Acquisition validates the descriptor against the retained pool/fence/slot identity.
Native replacement must install each replacement bundle atomically with its new
resource mapping; that transition is still pending.

## Transition ordering

Starting a transition is a producer-serialized transaction with admission.
Record the proposed allocation and stop further admission until its installation
is settled. This simple ordering prevents new claim mappings or reservations from
consuming capacity needed by the transition. Existing incarnations retain their
reservations throughout.

Retire the old generation's publication state before scanning its claims, using
the existing SC ordering. Increment reconfiguration state and notify every
consumer. Acquisitions interrupted before success return a reconfiguration
outcome; already successful leases remain valid. Keep publication cursors unique
and monotonic across generations. Ordered delivery reports gaps only for actual
published cursors; exact acquisition never substitutes a new-generation frame.

If the budget covers old and new allocations together, allocate the new resource
generation and offer it to existing consumers while old leases remain valid.
Consumers can install it and resume acquisition without releasing those leases.
A configuration grant must identify its arena, incarnation, and generation; stale
or foreign grants must not replace the consumer's current mapping.

If allocation cannot fit, expose a paused-capacity condition and drop incoming
frames. Consumers can relinquish their current configuration while continuing to
own previously acquired leases. That operation drops unleased mappings/handles
and acknowledges their retirement, but leaves the persistent control mapping and
cancellable wait usable. Once enough old allocations are reclaimable, reserve and
allocate the replacement before allowing admission again. No temporary allocation
may exceed the budget.

Limit each incarnation to one outstanding configuration offer. Later replacements
may proceed for healthy consumers if the budget covers every retained allocation;
an unresponsive recipient must not create an unbounded offer queue or an extra
global acknowledgement barrier. Keep its one older offer charged. When it handles
a stale offer, it relinquishes that offer's mapping/handles, acknowledges disposal,
and requests the current generation. Staleness is a normal setup retry; foreign
identity or contradictory metadata remains an error. Old generations held by
actual leases or outstanding offers remain charged while later generations are
installed, within the same byte budget and holding limits.

## What permits retiring an allocation

Both conditions are necessary:

- No incarnation can still acquire or access its unleased mapping/imported pool
  handles: recipients acknowledged retirement, completed library shutdown, or
  their bound process exited as appropriate.
- No successful lease or pending producer/consumer GPU operation can still use
  its resources. Resource claims and native readiness/completion evidence supply
  this proof; ring expiry and elapsed time do not.

Every outstanding configuration offer is part of that bookkeeping. Raw setup
consumers must obey the import/retirement contract and relinquish unleased setup
FDs and native handles when acknowledging retirement. The library cannot infer
that a delivered-but-unapplied offer was discarded merely from its current-map
field. Unresolved native claims keep the corresponding allocation charged and
continue to report recovery failure.

The admission book now tracks fixed control bytes, claim mappings, current and
retired resource allocations, and a pending allocation commitment. A transition
closes admission before reservation; insufficient overlap capacity leaves it
pending. Installation releases only the unused portion of a conservative size
bound. Retired allocations stay charged until explicit cleanup acknowledgement;
allocation failure can return its reservation while keeping admission paused for
retry. Allocation IDs are never reused. Like incarnation cleanup, the accounting
acknowledgement supplies no lifetime proof itself.

The CPU arena now uses this ledger through `reconfigure_cpu` and
`advance_reconfiguration`. A pending transition drops incoming publications.
The host retries advancement after retirement; allocation failure leaves the
proposal paused for retry. `configuration_offer` supplies an opaque local setup
grant, and `install_configuration` or dropping the grant disposes that offer.
`ConfigurationGrant::into_parts` exports one resource FD with a
`ConfigurationDescriptor`, only for a process-bound incarnation. Unsafe import
uses the existing consumer's claim mapping and checks process, arena, incarnation,
mapping slot, outstanding offer, and layout. It creates no new holding reservation
or notification channel. The raw setup contract prohibits replay and forwarding,
and requires relinquishing extra setup FD copies. Installation validates the
resource header even for a stale offer; contradictory metadata remains an error.

Claim maps contain `H + 2` mapping-reference slots: at most H generations retained
by leases, one current mapping, and one outstanding offer. A resource owner keeps
its slot until it has unmapped; an abandoned offer closes its FD before clearing
its slot. The producer reclaims a retired allocation only after no mapping slot
or resource claim names it, and destroys its own mapping before returning bytes.
Process-exit proof clears mapping references; unresolved asynchronous claims still
prevent allocation reclamation. Initial setup uses the same bookkeeping. Setup
version 7 retains the resource generation and mapping slot, carries the consumer
drain interval, and changes deferred-release state to require local retirement.
The resource header remains bound to its generation.

Native staging
needs an allocation upper bound before creating a replacement pool; checking its
actual size only after allocation would permit a temporary budget violation.
The macOS backend's allocation contract must establish that bound, then validate
actual IOSurface bytes against it. This preflight mechanism still needs technical
verification against the native API.

## Implementation evidence required

The existing mapped, cross-process, SC-interleaving, wait, and native tests remain
green. Four CPU transition tests pass on macOS and Linux: old/new frame retention
under shared holding credit; exhausted overlap capacity and cancellation; cursor
gaps before and after replacement publication; and 100 healthy replacements while
another consumer retains one stale offer. A separate real-process crash test
unblocks a capacity-paused CPU replacement after process-exit proof. These tests
do not cover native pool replacement. Three further retirement tests now cover
the exact consumer address after both API owners are destroyed, deferred use
blocking an exhausted replacement, and deadline failure with a surviving ordinary
lease followed by late completion. They pass on macOS and Linux. Existing release
tests now bind local completion explicitly; the subprocess tests transfer a
controlled completion source over its own event channel.

Two additional tests pass on macOS and Linux: a separate process keeps an old
frame through 100 publications, imports the replacement, and shares holding credit
across generations; an imported stale offer with a contradictory resource header
returns an error and disposes its offer. The transition suite now has six tests
plus its explicitly invoked subprocess helper. Existing arena, cleanup, release,
and wait suites and native GPU tests remain green, as do workspace build, default
and macOS-feature clippy, and pinned formatting.

Complete the remaining transition evidence through the agreed seams:

- Acquire old CPU and native frames, install a new size/format, and verify both
  old descriptors/pixels and new delivery. Old and new leases share holding credit.
- Exhaust overlap capacity, observe the pause and reconfiguration wake, relinquish
  unleased mappings, release old leases, and resume within the original byte budget.
- Attempt admission during the transition and configuration changes while earlier
  offers are outstanding; neither can consume promised capacity or grow a queue.
- Cancel a wait during a paused transition, and close/crash a consumer while old
  allocations are retained. Apply the existing cleanup proofs to each generation.
- Reject stale/foreign configuration grants and preserve published-cursor gaps.
- Exercise real native producer readiness and deferred consumer completion through
  retirement. An unresolved event keeps its allocation charged after timeout.

The C ABI and Porthole integration follow this implementation; the current viewer
still uses the old data path. A passing offline transition test is not live
capture acceptance.


Native ownership evidence: a real Metal test consumes setup, acquires a frame,
destroys both API owners, and then resolves readiness and samples the original
IOSurface using only the acquired frame. A second test rejects a descriptor that
names a different retained pool without leaking its claim. The existing ring-wrap
and deferred GPU-completion tests now consume native setup into the common
resource owner too. All five native tests pass, along with the existing CPU
suites, workspace build, default/macOS clippy, and pinned formatting. These remain
offscreen synthetic tests, not live capture acceptance.
