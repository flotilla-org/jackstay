# Publication discovery and graph management

Discussion note, 2026-09-16, from planning the Katzensteg connectors. These are
possible later directions, not interface requirements or work needed for the
initial CPU connectors.

A Jackstay publication registry could describe available sources and how to
connect to their active publications. Porthole would often be a natural host,
but a small standalone daemon, perhaps in a container, could provide a subset
of the same facilities. This need not imply a daemon shipped by Jackstay or a
registry dependency in the transport library.

Publication and registration seem useful to keep separate. A publisher could
expose a local endpoint for direct connections, then optionally register its
publication somewhere. It would not need to know every eventual destination.
The initial Katzensteg integration will use same-user local access; credentials
and remote access can be considered together in a wider design later.

Graph management could sit alongside discovery: starting producers, connecting
consumers, arranging remote streams, or inserting processes that resize or
convert frames for different destinations. Such a transform would consume one
publication and produce another. A registry need not offer all those operations,
and the choice of graph manager need not determine the frame transport.

An appealing extension is advertising latent sources: things that could produce
content but are not running yet. Discovery would not itself start capture or
launch an application. Activation could produce a live publication, suggesting
that source identity and running-publication identity should remain distinct.

Open questions include discovery scope, activation authority, registration
lifetime, credentials, remote bridging and who owns the resulting processes.
No wire format, registry interface or implementation location is chosen here.

Addendum, 2026-09-16, from the remoting research
([report](../../../project-map/reports/jackstay-remoting-research-2026-09-16.md)):
a cross-host bridge is the transform described above with its two halves on
different hosts. The egress half consumes a publication on one host; the
ingress half produces a new publication on the other whose source identity is
the original source. An "export" is the graph-manager operation that creates
that edge. Tender also uses the word publication, for a published service
endpoint; a Jackstay publication registered with Tender is one Tender
publication, so the terms nest. Tender never carries frames.

Addendum, 2026-09-18, on bounding latent sources

The latent-source idea above is open-ended as written: "things that could
produce content" invites the registry to become a proxy for everything capable
of producing a Jackstay source, which is unbounded. Two refinements keep it
small.

First, latency is declared, not inferred. The registry advertises a dormant
source only when something has explicitly registered it as producible; it never
lists a source because something is merely capable of producing one. Discovery
enumerates what was declared, not what is conceivable. An explicit catalog is
finite by construction; capability inference is not.

Second, what looked like one category is two. Existing windows are running but
uncaptured, not dormant: they are already enumerable (Porthole's `search`) and
already activatable (`track`, then a capture session), bounded by the OS window
list, and need no registry machinery — folding them into "latent" is what made
the concept feel unbounded. The genuinely dormant, declarable sources — a
Katzensteg profile set, a launch allowlist of known apps — are the only ones a
catalog is for. That catalog is small, authored and opt-in.

Activation authority (an open question above) lives in the same place.
Activating a dormant source is launching the app, which is the privileged act,
so the catalog entry that declares a source producible is the natural home for
who may activate it: the entry carries the policy, or names the grant that
governs it, rather than growing a second authorization system beside Porthole's
agent grants. The first two open questions then collapse into one bounded
object: a dormant-source catalog with per-entry activation policy.

Still deferred: whether that catalog is per-host or shared, its schema, and
remote and credential concerns — now scoped to a small object rather than to
all of discovery.
