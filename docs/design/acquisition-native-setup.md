# Native acquisition setup over XPC

The acquisition arena now has a macOS setup path in
`native/macos/xpc/arena.rs`, with object transfer in
`native/macos_acquisition_xpc_shim.m`. The existing viewer still uses the legacy
`XpcAttachClient` path. That caller and the C ABI must migrate before the old path
can be removed; this is staging for the acquisition contract, not a compatibility
promise.

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

Five tests use real anonymous XPC connections and Metal objects: authorization
and initial GPU sampling; old/new sampling through replacement under shared
holding credit; registration of the event signalled by actual gated GPU work;
disconnect while a live peer retains a frame, followed by fresh admission; and
foreign claim-scope rejection when numeric incarnation IDs match. All pass.
The four existing XPC tests and ten native arena tests also pass, along with
workspace build, default/macOS clippy, and pinned formatting.

These tests keep producer and consumer in the same OS process. The next required
evidence is a named service with a separate consumer that submits GPU work and
exits before completion. Named-service lifecycle and real cross-process event
transfer must be exercised there. The C ABI, reference viewer, host resumption of
paused transitions, Porthole integration, full-suite gates, and live capture
acceptance remain outstanding.
