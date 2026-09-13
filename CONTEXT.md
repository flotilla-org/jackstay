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
