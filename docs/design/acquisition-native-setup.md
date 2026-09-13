# Native acquisition setup over XPC

The acquisition arena now has a macOS setup path in
`native/macos/xpc/arena.rs`, with object transfer in
`native/macos_acquisition_xpc_shim.m`. The C bridge and native reference viewer
use this path. Porthole integration is in progress; the legacy CPU/session path
still needs migration before its ownership code can be removed.

`XpcArenaServer` serves one native arena. The host supplies its authorization
token policy and either a launchd Mach service name or an anonymous listener.
An admitted connection gets one incarnation. A second attachment on that
connection is rejected; a new connection negotiates a fresh reservation.

The server obtains the PID from the current `NSXPCConnection`, as documented by
Apple's [currentConnection](https://developer.apple.com/documentation/foundation/nsxpcconnection/current())
and [processIdentifier](https://developer.apple.com/documentation/foundation/nsxpcconnection/processidentifier)
APIs. Request metadata cannot choose a PID or inherit an incarnation. The server
calls `attach_process` before exporting anything, so the kernel lifetime watch
exists before the grant escapes.

Initial setup sends the versioned `GrantDescriptor`, five `NSFileHandle` objects
(control, resources, claims, notification reader, notification writer), the
pool's typed IOSurfaces, and its typed `MTLSharedEventHandle`.
The client validates resource counts and the process/scopes, then consumes the
bundle into the common `ArenaConsumer`. Mapping FDs are duplicated into Rust
ownership; the received envelope's copies disappear before grant installation.

Replacement uses a `ConfigurationDescriptor` and one resource FD, plus the new
pool's native handles. The client checks its connection's incarnation and random
claim scope before requesting or installing setup. The common native installer
keeps map and handle ownership together and disposes stale offers through the
existing mapping-reference acknowledgement. Replacement grants no new credit.

Release registration transfers the consumer's actual `MTLSharedEventHandle` to
the producer. The returned registration binds the producer's imported source and
the consumer's retained source to the same incarnation and claim scope. The
caller reuses that binding for deferred frame releases. Acquisition and deferred
release handoff use the shared maps; neither is an XPC request.

Connection invalidation closes acquisition, without declaring process exit or
GPU completion. Invalidation and request execution serialize on the exported
session object: a request queued before EOF cannot recreate a removed Rust
session after its close callback. The listener/session objects retain Rust state
until no in-flight callback can reach it. Explicit autorelease pools bound the
temporary Foundation objects created from Rust threads.

Anonymous endpoints constructed by this server carry a known producer. Connecting
to a named service is an unsafe Rust boundary: the host must trust that service
to obey shared-memory lifetime rules. The bearer token authorizes the consumer;
it does not authenticate the producer. This preserves the trust obligation of
`ConsumerGrant::from_parts` instead of hiding it behind a safe arbitrary-name API.

Before the shared-event allocation failure below, five tests passed using real
anonymous XPC connections and Metal objects: authorization
and initial GPU sampling; old/new sampling through replacement under shared
holding credit; registration of the event signalled by actual gated GPU work;
disconnect while a live peer retains a frame, followed by fresh admission; and
foreign claim-scope rejection when numeric incarnation IDs match.
The four existing XPC tests and ten native arena tests also passed, along with
workspace build, default/macOS clippy, and pinned formatting.

These ordinary tests keep producer and consumer in the same OS process.
`native_arena_xpc_process.rs` adds explicit, ignored acceptance runs using a
unique launchd Mach service and the test executable as its producer. A temporary
directory contains the job plist, control socket, and logs. Normal teardown
verifies bootout removed the job; failed runs retain diagnostics while removing
the job. These tests require a GUI bootstrap session and Metal device:

```
cargo test -p jackstay --locked --features backend-macos --test native_arena_xpc_process -- --ignored --exact named_xpc_transfers_frames_replacements_and_gpu_release_across_processes --nocapture
cargo test -p jackstay --locked --features backend-macos --test native_arena_xpc_process -- --ignored --exact a_crashed_xpc_consumer_keeps_its_submitted_gpu_lease_until_completion_or_visible_quarantine --nocapture
```

The first cross-process run passed on 2026-09-12, including old/new GPU sampling,
GPU-signalled deferred release, and verified service removal. The crash harness
was then added: a separate consumer submits sampling behind a real GPU event,
hands off deferred release, and is killed after publication wraps. It requires
the producer to retain its reservation through process exit and drain timeout,
then records actual late completion or explicit continuing quarantine.

The crash attempt on `kiwi` could not reach that experiment. Producer event creation began
returning `nil`; an existing backend test and a standalone Metal-only program
failed the same way after all test jobs/processes were gone. Diagnostics are at
`/tmp/jsxpc-weUTlB/README.md`. The harness compiles, but the current revision needs
both acceptance commands rerun after shared-event allocation recovers. The cause
of that device-wide allocation failure has not been established, and no crash
cleanup outcome is claimed from this attempt.

## C native bridge

`ffi_acquisition::macos` now exposes the existing XPC setup through C. Connection
and consumer handles have separate lifetimes. Named connection requests admission
with a holding reservation; optional authorization precedes attachment. Native
replacement verifies the original connection/incarnation and installs the native
handles with their resource mapping through the common installer.

`ft_acquired_frame_macos_resources` borrows the particular acquired generation's
IOSurface and readiness handle. It does not export a separate pool cache. The
common immutable descriptor provides dimensions, format and the readiness value.
Caller imports must remain within the lease's declared lifetime.

Release registration imports the caller's borrowed `MTLSharedEventHandle` into
an independently owned `ConsumerFence`, registers the same actual event with
the producer, and returns the common C release binding. The original handle may
be disposed after registration. Deferred release still uses the shared arena and
its retirement owner, without per-frame XPC requests.

The gated GPU release test now goes through the C registration/acquire/defer/wait
functions. An additional test acquires old/new native generations through C,
destroys both setup/consumer API handles, then samples each held frame using its
borrowed resources. Both compile, but neither current C/GPU scenario has runtime
verification: the standalone probe still returns `shared_event=nil` on kiwi.
The eight CPU/C boundary tests and SDL smoke pass; they do not prove native
sampling. After recovery, run the ordinary suite as well as the two named-process
acceptance commands above:

```
cargo test -p jackstay --locked --features backend-macos --test native_arena_xpc
```

The [native reference viewer](acquisition-viewer.md) now uses this bridge and
retains acquired frames through Metal completion. Its native rendering/cleanup
acceptance is still unverified while shared-event allocation is unavailable.

Viewer runtime verification, host resumption of paused transitions, Porthole
integration, full-suite gates, and live capture acceptance remain outstanding.

## Producer allocation and teardown boundary

`NativeArenaProducer::new` preflights the native pool's allocation bound together
with the arena's metadata charge before allocating surfaces or a readiness
event. Hosts supplying an existing pool can continue to use
`from_allocated_parts`; they own that initial allocation decision.

`stop` closes publication and admission and wakes consumers with terminal state.
It does not reclaim their frames. `poll_shutdown_ready` first drains common
claims, mapping offers and retired allocations, then checks the native producer's
actual submitted-write completion. A backend publication failure reports
`RecoveryRequired`: the last known fence cannot prove that a failed submission
left no unresolved work. Expiring a host deadline does not change this result.

After readiness, the host must release every remaining producer/setup owner
before counting the allocation as destroyed. In particular, XPC listener and
in-flight session callbacks can retain an `Arc` after listener invalidation.

The CPU shutdown test verifies that both a consumer mapping and its held frame
must retire. The macOS initial-budget test passes with real native layout
preflight. A new native shutdown test gates actual producer GPU work, drops all
consumer ownership, then opens the gate and expects shutdown to finish. It
compiles but awaits the shared-event recovery described above for execution.
