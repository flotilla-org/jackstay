# Affordances channel, JSON version 1

Status: implemented by `jackstay::affordances`, the v3 track handshake and the
ABI 0.12 typed `jackstay_affordances.h` surface in the toolkit slice (#39).
[ADR-0002](../adr/0002-affordances-channel.md) records the decision and
[prior art](../affordance-prior-art-2026-09-25.md) records its evidence.
The producer owns application state. The host owns chrome and publishes
presentation preferences. No message describes a widget, layout, font, color or
chrome pixel buffer. Content frames continue on media.

## Track selection and independent ownership

Affordances is a track kind (`affordances`, direction `both`) in the
[track handshake](track-handshake.md), rather than a public listener. The host
associates all tracks with the same selected, authorized source, and the
consumer selects affordances as optional or required like any other track.

An admitted affordances track transfers one endpoint of its own duplex
socketpair (the equivalent private pipe pair on Windows), under the handshake's
descriptor receipt and ownership rule and five-second deadline. The original
connected stream stays the media setup stream with its peer identity. No
channel's payload is multiplexed behind media, input or another typed channel.

The channel cannot open a second route around those constraints. No connection,
capability flag, presentation hint or enqueue success grants desktop authority.

## C ABI parity

ABI 0.12 adds `jackstay_affordances.h`, backed by the same Rust channel
implementation, with opaque owned `ft_affordances_producer` and
`ft_affordances_host` handles. Producer and host are application roles, not a
requirement that transport remain one-way. `ft_handshake_accept_local` returns
the producer handle beside media and input-server ownership;
`ft_handshake_select` returns the host handle beside media and input-client
ownership. An unadmitted track returns null, with its refusal in the admissions.

| Operation names | Handle and ownership shape |
|---|---|
| `ft_affordances_producer_publish` | Producer handle + borrowed typed snapshot; copies data before return |
| `ft_affordances_producer_poll` | Producer handle; receives host snapshot or typed verb |
| `ft_affordances_host_publish` | Host handle + borrowed presentation snapshot; copies before return |
| `ft_affordances_host_send` | Host handle + typed domain/verb body; enqueue success only, even if the channel later closes before delivery |
| `ft_affordances_host_poll` | Host handle; receives producer snapshot or closure |
| `ft_affordances_event_destroy` | Owned `ft_affordances_event`; releases borrowed payload views |
| `ft_affordances_producer_close`, `ft_affordances_host_close` | Begin channel close; do not confirm input cleanup |
| `ft_affordances_producer_destroy`, `ft_affordances_host_destroy` | Pointer-to-handle; end worker and null owned handle |

Poll yields an opaque owned `ft_affordances_event`; its typed discriminator and
payload view remain valid until event destruction. Rust enums map to C domain and
verb tags plus typed structs, UTF-8 pointer/length strings and explicit nullable
fields. C callers never serialize JSON. Constructors, event-view accessor names,
layouts, numeric tags and signatures are implementation work, not a promise of
an existing header. Both language interfaces must cover every v1 domain and verb.

## Deferred work

Menus are the second slice: dbusmenu-shaped trees of `label`, `enabled`, `checked`
and `children`, with `activate` and `about-to-show`. Lazy population and item
identity need a separate contract; no generic widget tree is introduced here.
Dialogs and pickers require request/reply, deferral, cancellation and lifetime
rules, so v1 deliberately has none. Pixel-buffer assets require resource ownership
and bounds; v1 carries only URL/named-icon references. Further media controls,
window management, cross-host translation and synchronized position clocks are
not implied by the initial vocabulary. Extension remains per domain and version,
with vendor prefixes for private additions.

## C views

The C header documents all domain and verb tags, capability bits, cursor tags
and payload layouts. Event payload views last until event destruction. C
callers supply typed data, never JSON.
