# Acquisition ownership and concurrency

This is the implementation design for the
[acquisition lifetime contract](../specs/acquisition-lifetime-contract.md).
The source audit below describes the original baseline, commit `6eeaf04` and
Porthole at `bf371e6`. The protocol was specified before changing the data path.
The status table records the current implementation; the subsequent notes retain
the history of individual slices. Live acceptance remains incomplete.

## Ownership paths at the baseline

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

| Slice | Current evidence and remaining work |
| --- | --- |
| 1: ownership and protocol | Baseline audit, SC ordering argument and bounded model checks are recorded above. Compiled arena tests exercise selection, claim publication, validation and reuse separately. The model is not a weak-memory proof. |
| 2: admission/incarnations | Separate claim pages, independent duplicate holds, overlapping storage, deferred-release credit and fresh admission after restart are implemented and tested. CPU process-exit cleanup and unresolved native reservations remain distinct. |
| 3: CPU shared acquisition/waits | The common arena implements latest, ordered gaps, exact misses, holding limits, cancellation and configuration replacement. Unix setup binds grants to the peer process. Standalone C producers, Porthole sessions, the recorder and CPU viewer now use this path; the socket/shadow path has been removed. |
| 4: existing GPU | macOS uses common claims, IOSurface storage and Metal events for readiness and deferred completion. Native arena, anonymous/named XPC and the reference viewer checks passed with real GPU work. The delayed viewer test holds RGBA frames for 250 ms while publication continues. Pixels in these tests are generated fixtures. |
| 5: cleanup/reconfiguration | CPU crash reclamation, budgeted CPU/native replacement, retained mappings and pending producer writes are tested. The submitted-GPU crash test demonstrated visible quarantine, not reclaimed capacity. Completion after process death remains a backend recovery limitation. See the [runtime record](acquisition-runtime-verification.md). |
| 6: ABI/host/live acceptance | Rust/C clients and Porthole use common acquisition at C ABI 0.5. Required macOS/Linux gates pass at Jackstay `f482a5c` and Porthole `67fa206`; relevant native and SDL checks also pass. Live CPU/GPU long playback, delayed desktop consumers and host resize acceptance remain outstanding. |

Porthole `67fa206` also retains native cleanup in a dedicated worker after session
and async-runtime teardown. A regression reproduced the old aborted cleanup; a
real gated Metal write then verified retention until completion and final owner
release. The run is recorded in `/tmp/porthole-native-owner-retirement-metal.log`.
It establishes cleanup within a surviving process, not completion after daemon
process death.

These rows do not claim the contract complete. Live capture must still exercise
the installed Porthole host, and no timeout, EOF or daemon restart may be counted
as proof of GPU completion. Porthole's process-wide graceful drain API remains
open; its session cleanup and process-exit guarantees must be kept separate.

### Historical implementation notes

The notes below record evidence and open work at each implementation milestone.
Statements about pending migrations, failing old regressions or unavailable Metal
events are historical; use the table and runtime record for current status.

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

The arena supports local CPU replacement; external native resources still use a
fixed allocation. `native::arena::NativeArenaProducer` uses the same retirement
and claim scan before staging into an IOSurface; it also checks producer GPU
completion before reusing a staging target. Actual IOSurface allocation sizes
count against the arena byte budget. The old host/native and C paths have not
yet been replaced. The new setup descriptor (version 7)
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

Next: finish [bounded reconfiguration](acquisition-reconfiguration.md),
then replace the existing CPU/native acquisition paths with the shared arena. Do not add a
per-frame broker update to keep admission informed; reserved claim slots are the
holding credit.

The control/resource split is implemented. Publication history and wait state
live in the persistent control mapping; resource state, descriptors, and inline
payloads live in a separate mapping. A shared consumer lifetime owns the claim
page, so destroying one resource owner cannot acknowledge shutdown while another
owner or lease remains. At that checkpoint, configuration replacement and consumer-side retention of
mappings consumed by deferred release were still pending; both CPU paths are
implemented by the subsequent changes described below.

Validation of the split passed on macOS and Linux: eight arena tests, four
cleanup tests, eight release tests, and four wait tests, including their invoked
subprocess helpers. The three native arena tests and three concurrency tests
passed on macOS. Workspace build, default and macOS-feature all-targets clippy,
and pinned formatting passed. The old host regression remains unresolved.

Allocation accounting now distinguishes persistent control bytes, claim pages,
and individually identified resource generations. The admission book pauses new
admission from proposal through installation, reserves replacement bytes before
allocation, retains old-generation charges until cleanup, and preserves the pause
through allocation failure/retry. Two further admission tests cover overlap and
insufficient-overlap scenarios, bringing the admission suite to six tests. These
and the arena tests pass on macOS and Linux; the cleanup/release/wait and native
tests remain green, as do build, default and macOS clippy, and pinned formatting.
The subsequent CPU transition implementation now invokes the ledger at runtime.
Consumer mapping-reference slots provide retirement acknowledgements separately
from frame claims; each map is destroyed before its acknowledgement. Outstanding
offers remain charged and limited to one per incarnation. Allocation reclamation
requires both the mapping references and frame claims to be clear.

Four local CPU transition tests pass on macOS and Linux, along with a real-process
crash that unblocks an exhausted replacement. They cover retained descriptors and
bytes, shared holding credit, overlap pause/retry, cancellation, published cursor
gaps, and repeated healthy changes while a stale offer remains outstanding. The
existing acquisition suites, three concurrency tests, three native GPU tests,
workspace build, default/macOS clippy, and pinned formatting remain green.
Native replacement now uses the common allocation ledger, as described in
[acquisition-reconfiguration.md](acquisition-reconfiguration.md). Native setup handles follow
the shared acquired resource lifetime, as described below. Cross-process replacement grants now transfer a single resource
FD to the existing process-bound incarnation. A child-process test retains the
old frame through 100 publications before installing the replacement; another
test rejects a contradictory resource header even when its offer is stale. Both
pass on macOS and Linux. The existing acquisition suites, native GPU tests,
build, clippy, and formatting remain green. The full suite and live
acceptance are not claimed complete.

Process-bound cleanup now uses kqueue on macOS and pidfds on Linux. Remote grants
are export-only and mandatory for setup FD transfer; local grants use ordinary
Rust ownership. The process-exit, failure, and native quarantine rules and source
evidence are recorded in [acquisition-process-cleanup.md](acquisition-process-cleanup.md).


Deferred release now binds a consumer-local completion source as well as the
producer's imported registration. One bounded local worker retains pending
resource maps through configuration replacement and API teardown, without an
ownership cycle or dependence on the producer observer staying alive. The
consumer acknowledges local mapping retirement before the producer may return
holding credit; both independently check actual completion. Source errors and
drain deadlines expose recovery failures without unmapping resources. Consumer
API closure starts that deadline even when an ordinary lease survives.

Three consumer-retirement tests pass on macOS and Linux, covering the exact raw
address after both API owners are dropped, exhausted replacement capacity, and
late completion after deadline failure. The release, cleanup, arena, transition,
and wait suites remain green; the three concurrency and three native tests,
workspace build, default/macOS clippy, and pinned formatting also pass. Remaining native reconfiguration evidence, host/C replacement,
full-suite gates, and live CPU/GPU acceptance remain outstanding.


`NativeArenaGrant::into_consumer` now transfers native surface/readiness handles
into the common resource owner. Successful frame acquisition checks their
pool/fence/slot identity, and `FrameLease::native_resources` borrows the particular
surface and readiness handle from that acquired generation. This uses the same
owner retained by deferred completion; native handles are destroyed before the
mapping-retirement acknowledgement. There is no separate native lease book.

Two added native tests prove sampling after setup and both API owners are gone,
and rejection of a contradictory pool identity without leaking a claim. The
existing actual-GPU ring-wrap and deferred-completion tests use the new ownership
path. Five native tests, the CPU suites, workspace build, default/macOS clippy,
and pinned formatting pass.

Native replacement now preflights aligned IOSurface bytes, reserves them with the
new resource map, and retains old pools in the common retirement owner. That owner
checks actual producer readiness independently of consumer claims. Native mapping
and handle installation is atomic, including disposal ordering on rejected offers.
Initial setup uses the same ownership path. The readiness timeline stays monotonic
across configuration generations.

Ten native arena tests now pass: the added tests exercise allocation bounds,
size/format replacement with both generations held, a capacity pause, and 100
replacements under a budget too small for overlapping pools. The gated GPU test
also retains a deferred frame through pool replacement. Seven existing macOS
backend tests pass. The CPU suites remain green on macOS and Linux. A real GPU
queue dependency now proves that delayed producer writes retain the old pool's
charge even without any consumers. The crash test now also samples a replacement
through a healthy consumer while preserving the crashed incarnation's quarantine.
The child still does not submit GPU work; that remaining crash proof needs native
handles and release events transferred through process-bound setup. Paused
transitions require an explicit host retry for now.
Host/C integration, full-suite gates, and live acceptance remain outstanding.

The new [XPC acquisition setup](acquisition-native-setup.md) binds `attach_process`
to the connection's actual peer PID, transfers five initial mapping/notification
FDs and typed native handles, installs one-FD replacement offers, and imports
consumer GPU release events. It rejects foreign claim scopes even when numeric
incarnation IDs match. EOF closes acquisition without claiming process death or
GPU completion. Invalidation serializes with requests so a queued request cannot
recreate a closed session. Five real XPC/Metal tests pass, but keep both endpoints
in the same OS process. An initial named-service run also passed across separate
processes, with GPU release and replacement. The subsequent crash harness is
currently unverified because shared-event creation fails even in a standalone
Metal program; see the setup document for diagnostics and rerun commands. The legacy
setup remains in use by the C/viewer path until those callers migrate.

The [common Rust/C ownership boundary](acquisition-c-boundary.md) now exposes
CPU grant import, independent frame handles, full immutable descriptors, distinct
selection outcomes, cancellable waits and deferred release through the arena's
existing retirement owner. ABI 0.3 updates the header and library together. The
boundary and compiled-C tests pass on macOS and Linux, and the existing SDL ABI smoke still
acquires 30 frames. The reference viewer and Porthole remain on their legacy
paths; their migration and the outstanding live acceptance are still required.

C CPU consumers can also install a one-FD replacement offer, distinguish a stale
offer, and relinquish their unleased current mapping during a capacity pause.
Held frames retain their mapping and credit across these operations. Publication
now stamps the installed allocation's configuration generation for CPU frames
as well as native frames. Eight boundary tests and the CPU arena/replacement
suites pass on macOS and Linux; the default full-suite run has only the existing
legacy daemon regression failure.

The native C bridge now wraps the process-bound XPC connection, configuration
installer and completion registration. Resource getters borrow directly from
the common acquired frame's retained generation. Its C/GPU scenarios compile;
runtime verification is pending the shared-event allocation recovery described
in [acquisition-native-setup.md](acquisition-native-setup.md#c-native-bridge).
CPU boundary tests, workspace build, default/macOS Clippy, pinned formatting and
the existing SDL smoke pass. The default full-suite run still reports only the
known legacy daemon regression. No native viewer or Porthole migration is claimed.

The [native viewer migration](acquisition-viewer.md) now removes its separate pool
cache and uses common acquisition, waits and configuration installation. Each
submitted frame and its imported native objects live through Metal completion;
shutdown drains those owners with a bounded wait. The real shader/pipeline setup
test and macOS CPU/SDL smoke pass. The named-service BGRA/RGBA viewer acceptance
test compiles but awaits working shared events. Linux viewer verification awaits
CMake/SDL2 development dependencies on paneer. Porthole host migration and live
capture acceptance remain outstanding.


## CPU host and consumer migration

The coordinated Porthole CPU host and reference consumers now use the common
arena through the Unix setup boundary. The old daemon shadow-ring validation and
per-frame socket lease API have been removed. See
[CPU setup](acquisition-cpu-setup.md) for the host limits, cancellation and
retirement behavior, C ABI 0.4 client entry points and remaining live acceptance.
The standalone API has since moved to the same arena through ABI 0.5's
`ft_cpu_producer` functions. The legacy `VideoSlotManager` and C wrappers are
removed. Standalone and Porthole CPU viewing now share a render/acquire loop.
The C producer retains ownership when destruction reports draining or recovery
required. See [the C boundary](acquisition-c-boundary.md#cpu-producers) for its
explicit limits and replacement operations. Live acceptance is still outstanding.

Reconfiguration notification epochs now use checked increments. Exhaustion at
either retirement or installation closes admission, publication and waiting
consumers without revoking acquired storage. A deterministic test sets each
boundary near exhaustion and checks closure, unchanged bytes and final drainage.
The reference viewer's `--hold-ms` option keeps a lease across a deliberate
consumption delay; CPU mode also checks its bytes before and after that delay.
