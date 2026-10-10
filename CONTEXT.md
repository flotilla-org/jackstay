# Jackstay

Jackstay transfers frame resources between producers and consumers. A capture
host chooses sources and supplies authority; Jackstay governs resource lifetime.

## Language

**Frame resource**:
The backing storage and descriptor for one published frame, including the
synchronization needed to use it.
_Avoid_: pixels, when the resource could remain on the GPU

**Publication ring**:
The bounded advertisement of recently published frames. An entry's presence is
an opportunity to acquire that frame, not ownership of it.

**Frame lease**:
A successful acquisition's right to use a stable frame descriptor and its
corresponding storage until release completes.
_Avoid_: ring entry, when referring to retained ownership

**Consumer incarnation**:
One admitted lifetime of a consumer. A restarted program has a new incarnation,
even when its name and authorization identity are unchanged.

**Holding reservation**:
The admitted capacity for a consumer incarnation's unresolved frame leases,
including releases waiting for GPU completion.
_Avoid_: holding limit, when the capacity guarantee matters

**Producer working reserve**:
Capacity reserved for producer work that consumer admission must preserve.

**Immediate release**:
A declaration that all use of an acquired frame has completed.

**Deferred release**:
A declaration of the completion signal after which all use of an acquired frame
will have completed. The resource remains retained until then.

**Delivery gap**:
Published frames no longer available to an ordered consumer. This is distinct
from incoming frames the producer dropped before publication.

**Reconfiguration**:
A transition in frame size, format, or resource allocation requirements. Existing
leases continue to describe their original frames through the transition.

**Publication**:
One source's running set of tracks behind a local endpoint.
_Avoid_: stream, when the whole set of tracks is meant

**Track**:
One stream within a publication, with a kind and a direction. Kinds are open:
core kinds such as video, playback, recording, input and affordances, plus
app-defined kinds whose descriptors Jackstay treats as opaque.
_Avoid_: channel, which is reserved for audio speaker channels

**Track offer**:
The source host's description of a publication's current tracks, given to a
consumer at attachment and updated as tracks are added, removed or changed.

**Track selection**:
A consumer's choice of which offered tracks its incarnation uses, including any
request to be the writer of a track that flows toward the source.

**Audio track**:
A track of ordered audio samples, described by its own sample format.
_Avoid_: media, which names the playback-state affordance domain, not sample data

**Playback track**:
An audio track the source's app plays, flowing from the source to its consumers.
_Avoid_: output, capture, which flip meaning depending on whose side you are on

**Recording track**:
An audio track the source's app records, supplied by one admitted consumer back
to the source.
_Avoid_: input, which names the input channel; microphone, which names a device
