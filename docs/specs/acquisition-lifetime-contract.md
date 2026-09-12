# Acquisition lifetime contract

Status: agreed design; implementation pending. The bounded design interview
settled the behavior below. Atomic layouts and backend mechanisms still need
technical design and verification.

The [ownership audit and implementation design](../design/acquisition-ownership.md)
tracks the current paths, proposed claim protocol, and verification by slice.

## Acquisition and publication

A successful acquisition provides an immutable descriptor, a lease on the
corresponding storage, and producer-readiness synchronization. The storage cannot
be reused until every lease preventing reuse has completed release. A consumer
must satisfy readiness synchronization before reading, including GPU sampling.

Publication-ring entries may expire independently of acquired frames. Before
acquisition succeeds, losing a selected frame is a normal acquisition miss.
After success, expiration of its ring entry cannot invalidate the lease.
Contradictory available metadata, invalid resource mappings, and invalid identities
remain errors. This contract does not turn arbitrary failures into empty results.

Latest delivery favors freshness and can skip intermediate frames. Ordered
delivery returns retained frames in order and exposes gaps. Neither mode forces
unbounded history retention or lossless production. Exact-cursor requests, where
retained as a lower-level operation, must not silently substitute another frame.
The exact API shape and gap representation remain technical design work.

## Admission and capacity

Each consumer incarnation negotiates a holding reservation. Refuse an admission
request or offer a smaller reservation when the requested capacity cannot fit.
Bounded allocation during admission is allowed; existing reservations and producer
working capacity must remain covered.

For equally sized resources, a conservative sizing bound is:

```
capacity >= sum(consumer reservations) + retained history + producer working reserve
```

Distinct resource holdings determine actual storage use. Multiple consumers can
hold the same resource; the admission bound assumes distinct holdings. Format
changes require byte accounting as well as resource counts. Concrete limits and
defaults remain to be selected from the implementation and memory budget.

An incarnation reaching its reservation receives an immediate, distinct
holding-limit result. It retains its connection and existing leases. Releases
pending GPU completion count against that reservation. Do not regain acquisition
credit merely by submitting a deferred release.

If no safe producer write target exists, drop incoming frames rather than
reusing leased storage. Admission makes demand predictable; dropping still handles
transient exhaustion and exceptional conditions. Surface budget and drop outcomes
to the host without assigning capture authority to Jackstay.

## Wait and release

Provide efficient, cancellable waits for relevant state changes. A check followed
by wait must not lose a wakeup. Consumers must be able to distinguish new data,
capacity becoming available, reconfiguration, and terminal state. A wait does not
release frames the caller continues to hold. Notification primitives and the
poll/recheck protocol require a concurrency design.

Immediate release means all use has finished. Deferred release names a completion
signal; retain the resource until that completion is established. Submitting GPU
commands is not completion. Preserve the existing native distinction between
`NativeLeaseRelease::Now` and `TimelineValue`, while verifying each backend's
actual resource-reuse behavior.

## Failure and restart

A restart creates a new consumer incarnation and requires fresh admission.
Names and authorization identities do not let it inherit old leases or release
them. Old incarnations continue to consume the capacity associated with their
unresolved resources until cleanup establishes safe reuse.

Cleanup has an explicit owner and a tested completion path. Stop new acquisitions
on disconnect. Known-safe resources can be reclaimed; uncertain resources stay
unavailable while backend cleanup resolves them. A timeout alone is not evidence
that CPU or GPU access has ended. Connection loss must not be equated with process
termination or GPU completion.

Healthy admitted consumers retain their capacity guarantees. New admission can
be refused while old resources are draining. Bound both retained resources and
replacement allocations. If cleanup cannot establish safe reclamation, expose
an explicit recovery failure rather than silently retaining abandoned reservations
forever. Backend teardown guarantees and the recovery action are proof obligations,
not mechanisms already supplied by this document.

## Reconfiguration

Keep existing leases valid with their original descriptors. Charge both old and
new allocations to the memory budget during a size or format transition. Notify
consumers of the transition so they can release old frames without waiting
indefinitely for a new one.

When a transition cannot fit, pause publication and expose the capacity condition.
Do not silently exceed the memory budget. Define the transition and admission
ordering so new attachments cannot consume capacity already committed to it.

## Implementation slices

1. Map the current CPU and native ownership paths. Specify the acquisition
   linearization point, resource-generation validation, and how a claim prevents
   reuse. Keep descriptor consistency separate from storage retention. Review
   the atomic protocol before implementing it; a seqlock alone does not pin storage.
2. Implement and test admission accounting and incarnation ownership. Cover
   overlapping and distinct holdings, producer reserve, deferred-release credit,
   rejected admission, and restart while an older incarnation is draining.
3. Implement the CPU shared-memory acquisition path with latest/ordered outcomes
   and cancellable waits. Deterministically exercise publication during selection,
   claim, validation and release, including ring wraparound and missed wakeups.
4. Apply the same contract to an existing GPU path, using the current native lease
   and timeline machinery where it meets the contract. Prove both readiness and
   completion ordering; do not count submission as release completion.
5. Exercise crash cleanup and bounded reconfiguration on both acceptance paths.
   Include repeated restarts, unresolved GPU work, exhausted transition capacity,
   and cancellation while waiting. Record any backend recovery limitations.
6. Update the shared Rust/C API, ABI version checks, reference consumers, and
   Porthole integration. Run each changed repo's required gates and relevant
   backend checks. Re-run authorized live CPU and GPU acceptance, including long
   playback and deliberately delayed consumers, without replacing live content
   with synthetic content.

The first existing GPU acceptance target is to be selected during the path audit;
macOS is the currently available live desktop. Preserve working platform paths
through staging. Windows capture, reverse input routing, network streaming, and
approval UI are not new requirements of this acquisition work.

## Reproduction evidence

The reported viewer failure after roughly 129 seconds is not a demonstrated
connection deadline. During diagnosis, the unchanged direct viewer completed
10,000 acquisitions in 195.77 seconds; a Rust consumer completed the same count
in 191.88 seconds. A slower Rust consumer failed after 2.016 seconds and 23
acquisitions with a typed ring-overrun error at the shadow metadata read.

A deterministic socket-consumer regression reproduces the post-lease case:
a two-entry metadata ring advances to cursor 3 before the response for leased
cursor 1 is read. Cursor 1's payload remains available, but the current client
fails its shadow check. The regression is present locally; no runtime fix has
been applied. A complete implementation must also handle loss before acquisition,
rather than only suppressing the observed post-acquisition failure.
