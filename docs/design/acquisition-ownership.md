# Acquisition ownership and concurrency

This is the implementation design for the
[acquisition lifetime contract](../specs/acquisition-lifetime-contract.md).
The source audit below describes commit `6eeaf04` (and Porthole's tracked tree
at `bf371e6`). The protocol is specified here before changing the data path.
It does not yet constitute live acceptance or a completed implementation.

## Current ownership paths

| Path | Acquisition and reuse | Disconnect / outstanding work |
| --- | --- | --- |
| CPU, `video.rs` | `acquire_ring_entry` inserts a consumer ID in `StoredFrame::pinned_by`, copies the descriptor and retains the segment. `release` removes that ID. The set does not distinguish two acquisitions of the same frame by the same consumer. | `disconnect_consumer` removes every pin for the ID. A live process can still have a mapped payload after connection loss. |
| CPU, Porthole `capture_registry.rs` | The registry mutex serializes selection and pinning. `send_frame_reply` transfers a connection-local lease ID. `DaemonConsumer::receive_frame_acquire` then requires the diagnostic ring entry to remain readable. | `handle_fd_connection` releases every connection lease at EOF. This is not proof that mapped reads ended. |
| macOS native, `ffi_native.rs` | `acquire_latest` reads the ring, then adds an entry to a **consumer-local** `NativeLeaseBook`. Neither action changes producer-visible ownership. `MacosFrameBackend::claim_reusable_slot` checks `IOSurfaceIsInUse` after excluding live ring entries. | XPC's `session_ended_callback` removes session/grant objects. It has no per-incarnation claim cleanup. `ft_native_register_release_sync` is unsupported on macOS. |
| macOS viewer | `mp_present` encodes a producer timeline wait before the blit. It commits the command buffer and returns; `main.c` immediately sends `FT_NATIVE_RELEASE_NOW`. It does not explicitly increment an IOSurface use count. | Submission is not completion. Texture references and IOSurface use counts must not be assumed to implement the API's explicit lease contract. |
| Linux native | The attach server serializes `AcquireLease` with publication, but the backend book accepts the supplied identity without revalidating the selected generation. The book retains deferred releases until the backend observes its release timeline. | Lease entries are backend-global, without incarnation ownership. `serve_linux_attach_stream` has no lease-drain action after its receive loop. A new attach cannot be allowed to release an old incarnation's IDs. |

The native producer currently retains only two retired pools, independently of
leases. The CPU path has no admission reservation. Both need byte accounting
through transitions. The native wait API polls every millisecond and lacks
cancellation.

The current control-page seqlocks copy concurrently writable descriptor bytes
with plain loads/stores. Checking a sequence afterward does not make a data race
valid in Rust. The replacement must avoid copying unprotected mutable bytes;
adding stronger sequence fences alone is insufficient.

## Shared claim protocol

Use one mechanism for CPU and native resources. The host owns an arena with a
bounded resource table and publication ring. Admission assigns a fresh
incarnation and a private shared claim page with exactly its holding reservation
in claim slots. Authorization names are separate from incarnation IDs.

Each publication has a nonzero, never-reused generation. A resource's atomic
state is `Published(g)` or `Retiring(g)`; uninitialized storage is unavailable.
Generation exhaustion is an explicit terminal condition, not wraparound. The
resource contains its complete immutable descriptor, including configuration and
producer readiness. It does not refer to a config-ring entry that can expire.

The publication ring advertises resource indices using atomic words. A consumer
selects a cursor, loads that ring position's resource index, and must validate
that resource against the selected generation. An overwritten advertisement may
cause a miss; it cannot identify a lease by itself. No descriptor bytes are read
at this stage. Table indices stay bound to their allocation until safe retirement
and setup-channel registration of any replacement. Pool IDs never repeat.

All claim, resource-state, and incarnation-state operations in the first
implementation use sequentially consistent atomics. Do not weaken them based on
a stress test. Each operation below is separately preemptible:

1. Consumer atomically claims an empty slot in its own page, storing generation
   `g`. If none is empty, return `HoldingLimit` immediately.
2. Consumer loads the resource state and requires `Published(g)`.
3. Consumer loads the incarnation state and requires `Active`.
4. Only after both validations may it copy the resource descriptor and access
   storage, after satisfying producer readiness. Failure clears this provisional
   claim and returns a miss or closed outcome without reading the resource.
5. Release clears precisely that lease's claim slot only after all use completes.
   A deferred release leaves the claim in place until completion is established.

The successful resource-state validation is the acquisition linearization point,
conditional on the subsequent incarnation validation. Closing races may reject
an acquisition that could otherwise succeed; they never revoke a returned lease.
Multiple leases on one generation occupy separate claim slots and therefore
cannot release each other. A lease ID includes its incarnation and a monotonic
local identity; reusing a free slot cannot make a stale release valid.

The producer first removes a frame from retained history, then changes its state
from `Published(g)` to `Retiring(g)`. It scans every admitted or draining claim
page for `g`. A matching claim prevents writing or deallocation. With no match,
and after the backend establishes that producer/consumer GPU use has ended, it
may write and eventually publish a new generation. A retired but held resource
stays retired; later scans permit reclamation after release. Retirement alone
does not revoke existing claims.

### Why selection cannot race into reuse

Let `C` be claim publication, `V` successful resource validation, `R` producer
retirement, and `S` the scan of that claim. Program order gives `C < V` and
`R < S`. Success for the old generation requires `V < R` in the sequentially
consistent order. Therefore `C < V < R < S`: the scan sees the claim unless its
release has already completed. If the scan precedes claim publication, validation
cannot succeed for that generation. Unique generations rule out ABA after reuse.

This also establishes descriptor safety. The producer cannot write while a
successful claim is held. The consumer reads descriptor bytes only under that
claim. Failed validation reads no plain descriptor/payload bytes. Shared atomic
storage must be accessed through atomic pointers, without constructing plain
references spanning mutable atomic words. Public safe APIs must not expose
unrestricted writes into a leased mapping.

### Why claims rather than just a reference count

A packed generation/reference-count CAS can prevent concurrent reuse, but a
crash after incrementing it and before recording the owner leaves an anonymous
pin. A reserved claim slot records ownership in the operation that protects the
resource. The producer can enumerate each incarnation's unresolved resources.

## Closure and cleanup ownership

The producer's arena owns the incarnation registry, process-lifetime observer,
release-completion observers, and reclamation queue. Control-channel EOF stores
`Closing` before allowing any cleanup, stopping further successful validation.
It does **not** clear existing claims. Claims and their reserved capacity remain
visible through `Draining` and any `RecoveryRequired` state.

A process still running after EOF may be paused in provisional claim publication.
Consequently, an empty page is not by itself proof that its memory can be reused
for a new incarnation. Each incarnation gets a different claim mapping. Reclaim
the old page only after a library shutdown acknowledgement that joins all users
of that page, or an OS observation proving the admitted process lifetime ended.
PIDs alone, name matching, and elapsed time are insufficient. Fork/handle transfer
to an unadmitted process is outside the claim-page ownership contract; a child
must attach independently before using the stream.

CPU claims can drain on explicit release, acknowledged shutdown after all mapped
reads finish, or verified process exit. GPU claims additionally need completion
evidence. The producer retains imported release events and observes them even
after EOF. It never returns deferred-release credit when merely registering a
timeline value. An incomplete signal after process death needs backend-specific
proof or an explicit recovery failure; a timer may report failure but may not
authorize reuse.

macOS is the first live GPU acceptance target. The installed SDK's
`IOSurfaceRef.h` documents that process termination removes that process's use
count. It does **not** establish that every outstanding Metal command is complete
when the connection closes or that a Metal texture reference is an explicit
IOSurface use count. Before GPU crash cleanup is accepted, verify the OS
retirement mechanism and resource ownership across process death. Pending GPU
work cannot be cleared on the strength of that use-count comment alone.

## Admission, transitions, and waits

The same producer-owned ledger covers resource capacity, allocation bytes,
incarnation reservations, and transition commitments. Admission and starting a
transition are serialized transactions. For uniform resources, preserve
`sum(reservations) + history + producer reserve`. Existing draining incarnations
remain included. Claim-page/control allocation and old pools also consume the
bounded byte budget. A transition reserves its capacity before another admission
can consume it. Insufficient capacity produces a visible paused condition.

Notifications carry no frame ownership. Each consumer gets a coalescing kernel
wakeup channel; shared epochs distinguish data, capacity, reconfiguration, and
terminal changes. Before sleeping, the waiter publishes its interest mask,
drains notifications, and rechecks the relevant epochs. A writer updates state
before reading the interest mask and signaling. With SC operations, either the
writer observes the arm, or its state update precedes the waiter's recheck. If
notification finds a full nonblocking channel, a wakeup is already pending.
The waiter polls notification and cancellation handles together. One wait reader
owns a channel (`&mut ArenaConsumer`); separate incarnations have separate
channels. A capacity-only wait does not arm notifications for each new frame.
Closure and reconfiguration always interrupt a wait because a paused transition
may require old frames to be released before new data can arrive.

Callers take `events()` before evaluating the acquisition predicate, and pass
that snapshot to `wait` if needed. `WaitInterest::DATA` suits an empty/latest
miss; `CAPACITY` suits holding-limit recovery. A one-way `Cancellation` can be
shared with another thread and multiple waiters. Its notification is never
drained, so cancellation remains visible. It does not release held frames.
No per-frame request/reply is required for CPU or GPU acquisition or immediate
release. Deferred GPU signal registration remains a setup/control operation.

## Verification and implementation record

The agreed seams are producer/admission, CPU acquisition, native acquisition,
and the Rust/C consumer boundary. Deterministic interleaving checks at those
seams must cover provisional claims, validation, ring wrap, repeated leases,
release completion, closure, generation reuse, and missed wakeups. The protocol
model supplements those tests; it is not evidence about compiled atomics or an
OS backend. Live CPU/GPU checks remain required by the contract.

`python3 scripts/check-acquisition-model.py` explored 1,658 states without
closure and 9,236 with independent incarnation closure, for two consumers and two producer reuse
attempts, with no unsafe read. Negative controls found counterexamples for
scanning before retirement, reusing generation IDs, and releasing before use
completes. These are bounded SC checks, not a weak-memory or liveness proof.

| Slice | Status / next evidence |
| --- | --- |
| 1: ownership and protocol | Source audit, ordering argument and bounded SC interleaving checks recorded. Compiled atomic/mapping tests follow in slice 3. |
| 2: admission/incarnations | Admission now allocates a separate mapped claim page with exactly the holding reservation. Duplicate acquisitions consume independent slots; overlapping consumers share storage without sharing credit. Closing retains reservations until the last library owner finishes. Deferred-release claims retain credit until registered completion is observed. Process-bound remote grants now reclaim ordinary CPU claims on verified exit; asynchronous claims still require completion evidence. |
| 3: CPU shared acquisition/waits | `acquisition::arena` publishes complete descriptors and inline CPU storage under the SC claim protocol. Latest, ordered gaps, exact misses, holding-limit outcomes, and cancellable notification waits are implemented. Mapped, cross-process, and deterministic missed-wakeup tests pass. Reconfiguration events await the transition implementation. Host integration remains pending; the old socket/shadow regression still fails. |
| 4: existing GPU | The new native arena uses shared claims for IOSurface selection and imported Metal events for readiness and deferred release. Two real offscreen GPU tests pass. Completion observation wakes capacity waits while publication is idle. Replacement of the existing host path remains pending. |
| 5: cleanup/reconfiguration | CPU process-exit cleanup and native quarantine implemented; see the [process cleanup design](acquisition-process-cleanup.md). Unfinished drains now report recovery failure without revocation; bounded reconfiguration and real GPU command retirement after process death remain pending. |
| 6: ABI/host/live acceptance | Pending. |

Admission-layer validation: `cargo test --locked --test acquisition_admission`,
workspace build, all-targets clippy with warnings denied, and pinned formatting
passed. The full suite is not claimed clean: the deliberately failing existing
socket/shadow regression is still present, awaiting the data-path replacement.
The admission book's cleanup acknowledgement is accounting only; it must be
called by the future arena cleanup owner after actual resource and mapping
reclamation, never directly on disconnect.

The first arena implementation has seven integration tests (one invokes an
ignored child-process helper explicitly), plus three deterministic concurrency
tests. They hold CPU bytes across 100 publications; distinguish duplicate and
overlapping leases; exercise closure with a surviving lease, fresh restart after
acknowledged cleanup, and producer shutdown; report ordered gaps and exact misses;
and transfer setup FDs to a separate process with no per-frame broker exchange.
Scheduler hooks force publication after selection, after claim publication, and
after successful generation validation. They also close an incarnation during
acquisition. Another hook injects data, release, closure, or cancellation after
the final predicate check and immediately before `poll`. Four wait integration
tests cover publication before sleeping, capacity interest filtering, closure,
and persistent cancellation without lease release. The child-process test also
waits for publication through transferred notification handles while retaining
an older frame. Hooks exist only in the test build, and assertions use public
publication/acquisition outcomes. These tests do not establish GPU safety or
unexpected-process-death reclamation.

The arena is currently a fixed allocation, with inline CPU payloads or external
native resources. `native::arena::NativeArenaProducer` uses the same retirement
and claim scan before staging into an IOSurface; it also checks producer GPU
completion before reusing a staging target. Actual IOSurface allocation sizes
count against the arena byte budget. The old host/native and C paths have not
yet been replaced. The new setup descriptor (version 5)
carries five FDs: control, resources, claim page, notification reader,
notification writer. Control and resource headers carry an arena scope; the
claim header has an independent incarnation scope. Import rejects mappings
mixed between otherwise identical arenas.
Both the producer and the consumer's release path can notify that consumer.
Its version/layout and handles must be included in the Rust/C ABI update; this
is not the old control-page ABI. The memory budget covers resource/control
mappings; kernel notification buffers and bookkeeping are additionally bounded
by the incarnation limit, rather than described as a process RSS limit.

Eight deferred-release tests cover storage/credit retention, shutdown with pending
GPU work, registrations scoped to an arena and incarnation, and failed observer
isolation with explicit cleanup retry. The producer retains imported timeline
handles and pending claims after consumer shutdown. A failed observer closes only
its incarnation and reports a persistent recovery failure; it does not clear
claims or block healthy incarnations. Backend notification-registration errors
also retain claims and allow explicit retry. Failure of the internal observation
channel remains a persistent recovery failure; retrying a backend cannot repair
that channel.

Each admitted incarnation gets one producer-owned cleanup thread. It sleeps on
the reverse direction of the existing notification socket, alongside any process
watch and drain deadline. After storing deferred-release metadata,
the consumer writes a coalesced wake to that reverse channel. The observer drains
before inspecting shared claims and then sleeps, so a racing handoff either
appears in its scan or leaves a readable notification. Backend completion uses
the same channel. On macOS, `MTLSharedEvent::notifyListener:atValue:block:` wakes
the observer, which rechecks actual completion before returning credit. Callback
registration covers completion racing the registration itself. No timed polling
or per-frame broker request is required.

Manual refresh and background observation serialize through one mutex per
incarnation. A holding slot permits at most one outstanding completion callback.
Even if a refresh observes completion before its queued callback runs, that
callback remains charged to the slot until it fires; immediate slot reuse cannot
accumulate callbacks. Callback objects retain only their wake, not the event,
claim map, or frame. Worker stacks and callback bookkeeping are bounded by the
incarnation and holding limits; they are outside the resource-mapping byte total.
Producer shutdown wakes and joins the observer without waiting for incomplete GPU
work. Shutdown does not make old storage available for reuse or invalidate leases.
The producer collects acknowledged, empty incarnations during publication,
admission, or `poll_cleanup`. Remote process observation now supplies CPU exit
proof. Drain deadlines report unresolved use while preserving claims and
continuing completion observation.

The added tests cover idle capacity wakeup, a deferred handoff from an imported
cross-process grant, observer shutdown with an unfinished event, and a late
callback after shutdown. The child-process helper is invoked explicitly by its
parent test. These are ordinary shutdown tests, not process-crash proof.

Two macOS tests use actual IOSurfaces and Metal commands. The first retains a
lease across ring wrap and samples after producer readiness. The second proves
submission occurred, blocks sampling behind a GPU gate while further publications
wrap the ring, and retains holding credit until the GPU samples the original
pixels and signals the registered release event. Publication stops before that
completion, and a capacity wait wakes without any further producer call. These
are offscreen tests using synthetic pixels, not live desktop acceptance. The eight
release tests (plus the invoked child helper), arena and wait tests, three
concurrency tests, both native arena tests, workspace build, default and
macOS-feature all-targets clippy, and pinned formatting passed during this slice.
The eleven existing macOS backend/XPC tests passed before adding automatic
observation; their existing sampling wrapper was unchanged by that addition.

Next: implement [bounded reconfiguration](acquisition-reconfiguration.md),
then replace the existing CPU/native acquisition paths with the shared arena. Do not add a
per-frame broker update to keep admission informed; reserved claim slots are the
holding credit.

The control/resource split is implemented. Publication history and wait state
live in the persistent control mapping; resource state, descriptors, and inline
payloads live in a separate mapping. A shared consumer lifetime owns the claim
page, so destroying one resource owner cannot acknowledge shutdown while another
owner or lease remains. This does not yet implement configuration replacement
or consumer-side retention of mappings consumed by deferred release.

Validation of the split passed on macOS and Linux: eight arena tests, four
cleanup tests, eight release tests, and four wait tests, including their invoked
subprocess helpers. The three native arena tests and three concurrency tests
passed on macOS. Workspace build, default and macOS-feature all-targets clippy,
and pinned formatting passed. The old host regression remains unresolved.

Process-bound cleanup now uses kqueue on macOS and pidfds on Linux. Remote grants
are export-only and mandatory for setup FD transfer; local grants use ordinary
Rust ownership. The process-exit, failure, and native quarantine rules and source
evidence are recorded in [acquisition-process-cleanup.md](acquisition-process-cleanup.md).
