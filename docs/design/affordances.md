# Affordances channel, JSON version 1

Status: accepted design for the [toolkit slice (#39)](https://github.com/flotilla-org/jackstay/issues/39); not an implemented ABI.
[ADR-0002](../adr/0002-affordances-channel.md) records the decision and
[prior art](../affordance-prior-art-2026-09-25.md) records its evidence.
The producer owns application state. The host owns chrome and publishes
presentation preferences. No message describes a widget, layout, font, color or
chrome pixel buffer. Content frames continue on media.

## Bootstrap and independent ownership

Extend [source bootstrap](source-bootstrap.md), rather than creating a public
listener. The host associates all channels with the same selected, authorized
source. The existing `JSBOOT01` fixed-size exchange supports only input: this
contract requires a new bootstrap version, not extra bytes appended to that
exchange. An old bootstrap is treated as lacking affordances only when the caller
explicitly selects its old protocol; do not retry on a partially consumed stream.

The new negotiation declares channel capability bits `INPUT` and `AFFORDANCES`,
with independent client requests `none`, `optional` or `required`. Input retains
its typing-mode and admission rules. Each offer identifies its `channel` string
(`input` or `affordances`) and `version` integer; affordances selects version 1
only when both peers support it. Reserve independent channel identities and
capability bits for future typed streams, not affordance-domain bits in the
bootstrap. The toolkit slice allocates the numeric bootstrap bits and binary
layout together with the ABI version update.

A successful affordances offer transfers one endpoint of its own duplex
socketpair (the equivalent private pipe pair on Windows). Use bootstrap's existing
descriptor receipt and ownership rule: retain the sender's copy until receipt,
then close it within the five-second bootstrap deadline. Preserve the original
connected stream for media setup and its peer identity. No channel's payload is
multiplexed behind media, input or another typed channel.

An unavailable channel or no common version is a clean optional refusal, reported
to the caller without losing media or admitted input. Required refusal fails
setup. Malformed negotiation, uncertain transfer or missing descriptor receipt
fails setup for either request mode. Close all newly owned channels on setup
failure and continue any required input cleanup. After setup each channel owns
its own worker, bounded queues and lifetime. Affordances closure clears cached
state and disables its controls; it proves nothing about input cleanup. Media
failure does not implicitly close the other owners. A host abandoning the source
must close them explicitly and poll input through its cleanup barrier.

## Framing and exact envelope

The toolkit reuses the Rust input framing implementation: a four-byte unsigned
big-endian JSON byte length followed by that many UTF-8 bytes. C callers do not
implement this encoding; specifying the affordances payload here does not make
input JSON a public C contract. Length must be 1 through 131072 bytes; queued wire bytes
are bounded to 524288 including prefixes. These are separate from media and
input queue bounds. Invalid framing/JSON or invalid required field types closes
only the established affordances channel. Overflow closes it visibly rather
than silently dropping verbs. Callers do not replay verbs after uncertainty. Mid-session closure, including
overflow, never fails media or input merely because affordances was optional.
V1 has no heartbeat or idle expiry: an idle source may have unchanged state
indefinitely. EOF/I/O failure detects disconnect; bounded output detects a
stalled reader only when traffic accumulates. Silent stalls have no guaranteed
detection deadline; hosts may close an unresponsive channel by their own policy.

Every JSON object has `version: 1`, `domain` (string), `domain_version: 1`,
`kind` (`snapshot` or `verb`) and `body` (object). A verb additionally has `verb`
(string). There is no request ID, reply, execution acknowledgement or error
message. Example:

```json
{"version":1,"domain":"media","domain_version":1,"kind":"verb","verb":"seek_absolute","body":{"position":42.5}}
```

Snapshots replace the complete state of one domain, including its `capabilities`
object where applicable. Send an initial snapshot for every supported domain on
each fresh connection, then on change. No old state survives reconnect; verbs
are never replayed. Absent domains are unavailable until their first snapshot.
A snapshot with `body: null` withdraws a domain and its controls. The receiver
clears all cached state and capabilities for it; the producer ignores verbs for
a withdrawn domain until it publishes that domain again. A null snapshot body
is the sole exception to the object body rule. Nullable fields explicitly clear values;
missing required fields are malformed, not a partial update. Delivery order is
stream order in each direction; there is no total order across directions or
across channels. Hosts may display the latest snapshot without retaining history.

Producer-to-host snapshots use `media`, `navigation`, `cursor`, `scroll` or
`window`; host-to-producer snapshots use `presentation`. Only the host sends
verbs, in `media`, `navigation` or `scroll`. A known message in the wrong
direction is malformed and closes the channel.

All field names below are exact and required unless explicitly called optional.
Numbers are finite JSON numbers representable as f64; strings are UTF-8.
Out-of-range/nonfinite numbers, negative media position/duration, nonpositive
scale or size dimensions, negative scroll lengths and out-of-range snapshot
scroll positions are malformed and close the channel. Negative zero counts as
zero (valid for nonnegative fields, invalid for strictly positive fields). Media
rate is signed: negative means reverse playback; zero means no advancement.
Verb seek/set-position clamping is explicitly described in their domains; no
snapshot field is silently clamped. These validation and closure rules apply
symmetrically: producers validate host presentation snapshots and verbs, and
hosts validate producer snapshots. Capabilities are booleans in
the snapshot's `body.capabilities`, named exactly like their verbs. Missing
capability flags mean false. State availability and executable capability differ:
`can_go_back`, for example, describes history, while `capabilities.back` describes
verb support. Hosts enable back only when both are true.

A receiver ignores unknown domains, domain versions, kinds, verbs and optional
fields. Unsupported verbs, including ones disabled since the last snapshot, are
ignored without error or reply. Validate known supported bodies before execution;
malformed known messages close the channel. Unknown enum values are ignored at
the affected field, preserving its prior known value (or its domain fallback
below on first publication or after withdrawal clears the cached value); never
interpret them as another executable value.
Future standard enum additions require a newer `domain_version`; vendors use
`x-<vendor>-<name>` for domains, verbs, fields and enum values. An unrecognized
extension cannot enable a standard capability. No generic widget escape hatch.

## Media (producer snapshot; host verbs)

| Snapshot field | Type and meaning |
|---|---|
| `status` | `playing`, `paused`, `stopped`, `buffering` or `unknown`; fallback `unknown` |
| `position` | Nonnegative seconds at snapshot publication, or null if unknown |
| `rate` | Playback seconds per elapsed second; zero while not advancing |
| `duration` | Nonnegative seconds or null for unknown/unbounded content |
| `title` | String or null |
| `artwork` | Null, `{"kind":"url","url":"…"}` or `{"kind":"icon","name":"…"}`; unknown kind falls back to null |
| `capabilities` | Flags for every verb below |

| Verb / capability flag | Exact `body` |
|---|---|
| `play` | `{}` |
| `pause` | `{}` |
| `stop` | `{}` |
| `next` | `{}` |
| `previous` | `{}` |
| `seek_absolute` | `{"position": number}` in nonnegative seconds |
| `seek_relative` | `{"offset": number}` in signed seconds |

A host may extrapolate from local receipt time using position and rate, bounded
by known duration. This is approximate and includes transport delay; v1 adds no
shared-clock synchronization. Republish on seeks, rate/status changes and content
changes. The producer clamps valid seeks to its available range. Asset references
convey no pixel ownership or authority to fetch; the host applies its own URL
and icon resolution policy. Artwork is content metadata, not control appearance.

## Navigation (producer snapshot; host verbs)

| Snapshot field | Type and meaning |
|---|---|
| `url`, `title` | Each string or null |
| `can_go_back`, `can_go_forward` | Boolean history availability |
| `loading` | Boolean |
| `capabilities` | Flags for every verb below |

| Verb / capability flag | Exact `body` |
|---|---|
| `back` | `{}` |
| `forward` | `{}` |
| `reload` | `{}` |
| `stop` | `{}` |
| `load` | `{"url": string}` |

Verb names and capability flags are domain-scoped: `media.stop` and
`navigation.stop` are distinct operations. C discriminators must preserve the
domain/verb pair, even if verb tags share numeric values.

The producer owns URL interpretation and policy. A load is a semantic navigation
verb, not injected typing into an address field. No completion reply is implied;
subsequent snapshots describe actual state.

## Cursor (producer snapshot)

| Snapshot field | Type and meaning |
|---|---|
| `shape` | Named CSS cursor value; unknown values fall back to `default` |

V1 values are `auto`, `default`, `none`, `context-menu`, `help`, `pointer`,
`progress`, `wait`, `cell`, `crosshair`, `text`, `vertical-text`, `alias`, `copy`,
`move`, `no-drop`, `not-allowed`, `grab`, `grabbing`, `e-resize`, `n-resize`,
`ne-resize`, `nw-resize`, `s-resize`, `se-resize`, `sw-resize`, `w-resize`,
`ew-resize`, `ns-resize`, `nesw-resize`, `nwse-resize`, `col-resize`, `row-resize`,
`all-scroll`, `zoom-in` and `zoom-out`. The host resolves `auto` and chooses its
native theme/scale. There are no cursor verbs, custom images or capability flags.

## Scroll (producer snapshot; host verbs)

| Snapshot field | Type and meaning |
|---|---|
| `x`, `y` | Axis objects with the fields below |
| `x.scrollable`, `y.scrollable` | Boolean |
| `x.content_length`, `y.content_length` | Nonnegative number in producer units |
| `x.viewport_length`, `y.viewport_length` | Nonnegative number in the same units |
| `x.position`, `y.position` | Number in `[0, max(0, content_length - viewport_length)]` |
| `capabilities` | `scroll_by_step` and `set_position` flags |

| Verb / capability flag | Exact `body` |
|---|---|
| `scroll_by_step` | `{"axis":"x" or "y","step":"small" or "large","direction":"decrement" or "increment"}` |
| `set_position` | `{"axis":"x" or "y","position": number}` |

Units belong to the producer and remain consistent within an axis; they need not
be pixels or match between axes. Zero is the producer's content origin; positive
position advances along its chosen content order. The host derives thumb ratios
from lengths, not locale-dependent percentages. The producer chooses small/large
step distance and clamps finite set-position values to the current range. A
non-scrollable axis ignores both verbs; its position is zero. Unknown axis, step
or direction enum values cause the whole verb to be ignored. No input wheel event
or inferred pointer position is generated by this protocol.

## Window (producer snapshot)

| Snapshot field | Type and meaning |
|---|---|
| `title` | String or null; window chrome title, independent of media/navigation titles |
| `requested_size` | Null or `{"width":number,"height":number}`, positive logical units |
| `ready` | Boolean: producer considers its content ready to present |

After window withdrawal, the host treats readiness as false and title/requested
size as absent; it keeps its own presentation policy until republication.
There are no window verbs or capability flags in v1. Requested size is a
preference, not a host resize command. Ready does not acknowledge a frame lease.

## Presentation (host snapshot)

| Snapshot field | Type and meaning |
|---|---|
| `visible` | Boolean host visibility hint |
| `preferred_size` | Null or `{"width":number,"height":number}`, positive logical units |
| `scale` | Positive device pixels per logical unit |
| `focused` | Boolean; snapshots carry both focus-in and focus-out |

No presentation verbs or capability flags exist in v1. Hints may be ignored,
including `visible`; hiding a source never implies media pause. Focus hints do
not admit a controller, advance an input epoch or confirm held-state release.
The host must separately perform input focus-loss/reset and cleanup operations.

## Commands and input authority

An affordances connection supplies semantic verbs, not keyboard/pointer authority.
A producer can implement play or navigation directly in its own application.
If an adapter implements a verb through input execution, it must use the existing
admitted controller and obey epochs, ordering, cancellation and cleanup. Without
admission, or while cleanup blocks replacement, it cannot execute that input.
The channel cannot open a second route around those constraints. No connection,
capability flag, presentation hint or enqueue success grants desktop authority.

## C ABI parity (planned naming and handle shape)

The toolkit slice adds `jackstay_affordances.h`, backed by the same Rust channel
implementation, with opaque owned `ft_affordances_producer` and
`ft_affordances_host` handles. Producer and host are application roles, not a
requirement that transport remain one-way. Bootstrap's extended accept result
returns the producer handle beside media and input-server ownership; connect
returns the host handle beside media and input-client ownership. Unavailable
optional channels return null plus an explicit refusal status. Existing bootstrap
functions are not silently extended; new entry points and ABI version are part
of the toolkit slice.

| Planned operation names | Handle and ownership shape |
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
