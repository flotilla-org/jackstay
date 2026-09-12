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
terminal changes. The wait operation drains notifications, checks relevant
epochs and its predicate again, then waits on notification and cancellation
handles together. Updates publish state before signaling. If notification finds
a full nonblocking channel, a wakeup is already pending. One wait reader owns a
channel; multiple application waiters must serialize or have distinct channels.
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
| 2: admission/incarnations | `acquisition::AdmissionBook` implements worst-case holding reservations, memory and incarnation-table limits, fresh IDs, and retention of capacity until cleanup acknowledgement. Four public-boundary tests pass. Shared claim integration and actual deferred-release credit remain pending. |
| 3: CPU shared acquisition/waits | Pending; existing slow-consumer regression still fails. |
| 4: existing GPU | macOS selected; readiness encoding exists, acquisition/completion integration pending. |
| 5: cleanup/reconfiguration | Pending; GPU process-death proof remains open. |
| 6: ABI/host/live acceptance | Pending. |

Admission-layer validation: `cargo test --locked --test acquisition_admission`,
workspace build, all-targets clippy with warnings denied, and pinned formatting
passed. The full suite is not claimed clean: the deliberately failing existing
socket/shadow regression is still present, awaiting the data-path replacement.
The admission book's cleanup acknowledgement is accounting only; it must be
called by the future arena cleanup owner after actual resource and mapping
reclamation, never directly on disconnect.

Next: implement mapped claim slots and resource-generation validation under the
ordering above. Exercise duplicate/overlapping leases and deferred-release credit
through actual claims, then join admission, publication, and acquisition in the
shared CPU/native arena. Do not add a per-frame broker update just to keep the
admission book informed; reserved claim slots are the holding credit.
