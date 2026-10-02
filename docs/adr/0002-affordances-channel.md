---
status: accepted
---
# Affordances are typed state and verbs on an optional channel

The host needs to render controls for a source without receiving producer-drawn
chrome. We adopt the [rulings on #32](https://github.com/flotilla-org/jackstay/issues/32):
Jackstay owns a third optional bootstrap channel beside media and input. It uses
its own socketpair and input's length-prefixed framing, carrying typed producer
state snapshots, host presentation snapshots, and a small closed verb set per
domain. Each verb has a capability flag; unsupported verbs are ignored. There
are no request/reply operations or chrome pixels in v1. Asset references are URLs
or named icons. The [design contract](../design/affordances.md) fixes the v1 fields
and the planned C handle shape in `jackstay_affordances.h`.

The [prior-art note](../affordance-prior-art-2026-09-25.md) shows the recurring
state/capability/verb split in media APIs, web views, cursor protocols and scroll
providers. Its open questions are research inputs, resolved by the rulings above;
its unverified details are not requirements. Affordances are the first typed
bidirectional stream anticipated by [Porthole ADR-0008](https://github.com/flotilla-org/porthole/blob/main/docs/adr/0008-jackstay-general-transport-non-preclusion.md).
Channel identity and versioning must allow further typed channels without
turning this one into a generic widget protocol.

## Alternatives considered

- **Producer-drawn chrome:** duplicates host themes, scaling, accessibility and
  control layout. Producers describe application meaning; hosts choose controls
  and appearance. Content frames remain on media.
- **Porthole control-API vocabulary:** couples app-to-app state to one desktop
  coordinator. These domains require no desktop authority; Porthole is one host
  and authorization remains host supplied.
- **Generic widgets:** transfers layout and toolkit semantics, growing a remote
  UI protocol rather than the bounded domain contracts supported by the evidence.
  Extend by domain, versioned enum values and vendor prefixes instead.

## Consequences and boundaries

V1 covers media, navigation, named CSS cursors, scroll extents in producer units,
window title/requested size/ready, and host visibility/size/scale/focus hints.
Full snapshots simplify reconnect and eliminate dependence on property deltas;
capability flags can change with state. Hosts cannot infer execution completion
from enqueue success or demand replies. Independent channel owners prevent media
stalls from blocking affordances or input.

Commands are not input and must never bypass input admission or its cleanup
barrier. A cooperative application can execute its own semantic verbs; an adapter
that implements one through input must use the admitted input path. Presentation
hints are preferences: producers may ignore `visible`. Media `pause` is a playback
verb and is never implied by invisibility. Focus hints alone do not establish or
release input ownership.

Menus are deferred to a second slice using dbusmenu's tree of label, enabled,
checked and children with activate/about-to-show; lazy population deserves its
own contract. Dialogs and pickers are deferred because they require request/reply,
deferral and cancellation semantics. Pixel buffers for assets are deferred
because they require ownership, bounds and lifetime rules beyond references.
The toolkit slice implements Rust and C parity; this decision implements no code.
