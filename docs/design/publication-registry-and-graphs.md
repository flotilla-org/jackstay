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
