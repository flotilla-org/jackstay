# Native viewer acquisition lifetime

The SDL Metal viewer uses the common C acquisition API and requests a holding
reservation of two frames. It acquires the latest frame after its last submitted
cursor, borrows that frame's native resources, and transfers the frame handle to
the presenter only when command preparation succeeds. Earlier failures submit no
GPU work and leave the frame owned by the caller.

## Completion ownership

One `JackstayFrameUse` owns each submitted frame, its imported source texture and
readiness event, and its drawable. The presenter and this owner retain everything
needed by an unretained-reference command buffer until its completion callback.
Temporary encoder/descriptor references are drained before commit. The callback
disposes imported resources before releasing the common frame handle.

Apple documents that unretained command buffers leave resource retention to the
application, and that completion handlers execute after the GPU finishes the
commands. These two properties let the viewer control when its imported native
references disappear relative to the lease's mapping-retirement acknowledgement.
See [unretained command buffers](https://developer.apple.com/documentation/metal/mtlcommandqueue/makecommandbufferwithunretainedreferences())
and [completion handlers](https://developer.apple.com/documentation/metal/mtlcommandbuffer/addcompletedhandler(_:)).

The reference viewer keeps an ordinary lease through actual GPU completion, then
uses immediate release. It does not treat submission as completion. Consumers
that hand work off to another completion owner can still use the common deferred
release API and registered Metal timeline. A viewer crash before its completion
callback leaves native use uncertain; the existing process cleanup must quarantine
that incarnation and report recovery failure, not infer GPU completion from exit.

There is no persistent surface/texture cache. A small render pipeline samples
the IOSurface-backed texture directly into the drawable. Texture format follows
the acquired descriptor, so BGRA and RGBA sources share the same output pipeline
without copying frame data through CPU memory.

## Wait, replacement and shutdown

The viewer snapshots acquisition events before trying to acquire. Empty/missed
latest frames wait for data; holding limit waits for capacity. All waits respond
to reconfiguration and closure, and use a 16 ms deadline to keep SDL event
processing responsive. Unexpected errors terminate with diagnostics.

On reconfiguration, the viewer relinquishes its unleased current mapping and
requests the new native configuration. Each configuration epoch causes at most
one request, avoiding repeated setup RPCs while allocation is paused. Old GPU
frames retain their own mappings. A successful install preserves the incarnation
and holding reservation; the producer must retry a paused allocation after
retirement permits it.

Shutdown waits on a dispatch group for at most five seconds. Completion owners
remain valid independently of the presenter API handle. Timeout reports failure
and never forcibly releases outstanding frames. `presented_frames` counts only
successful command-buffer completions whose frame releases also succeeded.

## Verification status

The viewer builds with `-Wall -Wextra -Werror` for both C and Objective-C, and its
CPU/SDL smoke passes on macOS. The explicit Metal
presenter smoke successfully compiles the shaders and creates the real pipeline
on kiwi. This does not verify drawing or frame release ordering.

A manual named-service test now launches the reference viewer against an isolated
native producer in BGRA and RGBA configurations and checks completed-frame counts
and admission return after each exit. It compiles but has not run: Metal
shared-event allocation is still failing on kiwi. Long playback, delayed use,
in-session replacement and authorized live capture remain to be verified.

The Linux viewer smoke is also pending: paneer lacks CMake and SDL2 development
files. The user has been asked for those dependencies. CPU rendering code remains
on its existing path. Porthole still needs migration to the common native setup
and CPU acquisition paths; its installed viewer/daemon have not been replaced.
