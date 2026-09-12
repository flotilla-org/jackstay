# Bounded acquisition reconfiguration

Status: technical design for the next implementation step. The lifetime contract
already requires these outcomes; the mechanisms below are not implemented yet.

## Separate control lifetime from resource lifetime

The current arena puts wait epochs, publication history, descriptors, and inline
CPU payloads in one mapping. Keeping that mapping to wait for new configuration
also keeps its old allocation alive. If old and new allocations cannot coexist
within budget, that creates a cycle: the consumer needs the old mapping to wait,
while the producer needs it gone before allocating the replacement.

Split the persistent control mapping from per-generation resource mappings.
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

Limit outstanding configuration offers. Do not begin another replacement while
an earlier offer remains unacknowledged by a live recipient. A host can retry a
newer size/format request after the current transition settles; it must not queue
an unbounded series of replacement pools. Old generations held by actual leases
remain separately charged and can coexist with later installed generations only
within the same budget and holding limits.

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

The accounting ledger tracks fixed control bytes, claim mappings, current and
retired resource generations, and pending allocation commitments. Native staging
needs an allocation upper bound before creating a replacement pool; checking its
actual size only after allocation would permit a temporary budget violation.
The macOS backend's allocation contract must establish that bound, then validate
actual IOSurface bytes against it. This preflight mechanism still needs technical
verification against the native API.

## Implementation evidence required

First split control and resource lifetime without changing acquisition semantics;
retain the existing mapped, cross-process, SC-interleaving, wait, and native tests.
Then exercise the transition through the agreed producer and consumer seams:

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
