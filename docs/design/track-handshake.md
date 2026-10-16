# Track handshake (v3)

Handshake v3 replaces source bootstrap v1 and v2. A source host exposes one
endpoint for one publication: a source label plus an ordered set of
[tracks](../../CONTEXT.md). A consumer attaches once and chooses which tracks
its incarnation uses. Jackstay creates no listener, registry or permission
policy: the host has already selected the source and supplies its authorized
input target, or none.

## Exchange

All messages are CBOR under flotilla ADR 0063 (deterministic writers, unknown
keys ignored, unknown `type`s rejected), each framed as the 8-byte magic
`JSSETUP3`, a big-endian u32 length and one map. A message is at most 64 KiB and
any byte string, such as a descriptor, at most 4 KiB.

1. **offer** (host): `source`, `clock` (`host-time` or `unknown`, see
   [Clock](#clock)), and `tracks`, each with a stable u64 `id` (never
   reused within the publication), `kind`, `direction` (`from-source`,
   `to-source` or `both`), `label` and an opaque `descriptor` byte string.
2. **selection** (consumer): `tracks`, each an offered `id`, `required` and
   `write`. `write` asks to write a track that flows toward the source;
   selecting input always asks for it. A selection is no longer than the offer
   and names each id at most once.
3. **admitted** (host): one entry per selected track, in selection order, with
   a `refusal` of null (admitted), `unknown`, `unavailable` or `not-writable`.

A required refusal closes setup on both sides. Optional refusals are reported
and setup continues. After **admitted**, the host passes one channel per
admitted input or affordances track, in selection order: a socketpair end by
SCM_RIGHTS on Unix, or a private pipe end duplicated into the verified peer on
Windows, each acknowledged before the sender drops its copy. Input and
affordances keep their existing protocols on those channels.

After the channels the host passes one more: the incarnation's tracks
channel, on which it announces **added** (`track`), **removed** (`id`) and
**updated** (`track`) notices with the same framing. An update keeps the ID
and the selection; a device reopen is one. The consumer may select an offered
audio or app-kind track late with a one-track **selection**; the host
answers **admitted** and, for audio, passes the ring, preceded by the doorbell
if this is the incarnation's first playback track. A late selection takes the next
selection position, which is its doorbell hint bit. A removed track's mapping
stays valid until the consumer drops it; its ring just stops advancing. The
tracks channel is always sent, one extra channel per admission. It is separate
because CPU setup owns the media stream once video is admitted, and is
consumer-driven.

Kinds are open. The core kinds are `video`, `input` and `affordances`; an app
kind is a reverse-domain name of lowercase labels, such as `org.example.cue`,
whose descriptor Jackstay treats as opaque.

A `recording` track flows toward the source in the playback ring layout with
the roles swapped. Selecting it requests writing, and one consumer at a time is
admitted: it alone receives a ring, read-write, and any other request is
refused `unavailable`. The admission lasts until that incarnation's `Notices`
is dropped or the track is removed. Each admission creates a fresh ring, so a
previous writer keeps only an orphaned mapping that the source never reads:
it cannot reach the source after its admission ends. Each writer's first
block is a discontinuity, and the source's `RecordingReader` switches to the
new writer's ring at its latency target. With no writer it reads silence. A host offers at most one video, one
input and one affordances track. Admitting an app-kind track only records the
agreement: Jackstay opens no channel for it, so its transport is the host's and
consumer's own. Consumers skip kinds they do not know. Today's core
kinds have empty descriptors.

One incarnation carries the selected tracks. An admitted video track continues
on the same connection as [CPU setup](acquisition-cpu-setup.md), which keeps
the consumer's kernel identity for grants; its frames use the CPU arena. The
whole exchange, from the offer to the last channel receipt, has a five-second
absolute deadline (`local::Bounded`); input admission then has its own
five-second bound.

## Clock

A publication declares one clock domain in its offer, and every track's
timestamps use it; frame descriptors carry none. Live publications use host
time, the platform monotonic clock in nanoseconds (`model::host_time_ns`):
`CLOCK_MONOTONIC` on Linux, `CLOCK_UPTIME_RAW` (`mach_absolute_time` in ns) on
macOS and QPC on Windows. `unknown` is for producers that cannot say. A
timestamp is the source presentation time; the CPU and native producers stamp a
zero timestamp with host time at publish. Consumers own drift and A/V
correction, and video-only consumers may ignore timestamps. See
[#99](https://github.com/flotilla-org/jackstay/issues/99).

## Rust and C

Hosts call `handshake::accept(stream, &offer, target)` on a worker. `Accepted`
holds the media stream, the admissions, and the input `Server` and affordances
`Producer` for admitted tracks, and the `Notices` handle: `add`, `update` and
`remove` send notices, and `serve` answers one late selection (a zero timeout
polls). Serve CPU setup on the media stream when a video track was admitted.

Consumers call `handshake::connect(stream)`, inspect `Offered::offer()`, then
`select(&selection, mode)`. `Connected` holds the media stream, the admissions,
an input `Client` or an optional input's clean admission rejection, and the
affordances `Host`, and `Tracks`: `next` returns notices and `select` makes a
late selection.

`jackstay_handshake.h` exposes the same exchange as typed structs on Local
Endpoint connections: `ft_handshake_accept_local` takes the clock domain and
`ft_track_offer`s and writes `ft_track_admission`s; `ft_handshake_connect_local`,
`ft_handshake_offer_describe` and `ft_handshake_select` read the offer and
submit `ft_track_selection`s. POSIX callers with a connected socket wrap it with
`ft_local_connection_adopt`. After basic checks the connection or offer is
consumed on failure; a required refusal returns `FT_STATUS_UNSUPPORTED`.
Hosts pass each playback track's `ft_playback` to accept and get
`ft_track_notices` (add, update, remove, serve); consumers get the admitted
rings by selection position, the doorbell and `ft_tracks` (next, select).
Playback descriptors cross as `ft_audio_descriptor` in `ft_track_offer.audio`,
never as bytes. `jackstay_audio.h` covers writers, readers and the doorbell,
and states the threading rules: block writes are safe on a real-time thread.

Media and channel lifetimes stay independent. If CPU attach later fails and the
host abandons the source, close and poll the input client for cleanup, then
destroy it. Do not destroy the host's target until its input servers have
stopped and cleanup has settled.

## Reference and acceptance

```sh
build/viewer/capture-input-source /tmp/jackstay-source.sock
build/viewer/capture-viewer-sdl --source-socket /tmp/jackstay-source.sock
```

With `--endpoint NAME` the source listens on a [Local Endpoint](local-endpoints.md)
instead; that is its only form on Windows. The viewer requires the video track,
selects input optionally by default (`--observe` skips it, `--require-input`
requires it) and affordances by `--affordances none|optional|required`.

`tests/handshake.rs` and `tests/handshake_ffi.rs` run the offer and selection
scenarios against the Rust API and through the C ABI: admission of video, input
and affordances, optional and required refusals, busy input, app kinds with
4 KiB descriptors, malformed frames and a stalled peer. The viewer's ctest suite
covers the same exchange across processes. `tests/playback.rs` covers playback
tracks, and `tests/playback_ffi.rs` runs the same scenarios as compiled C
(`src/native/c_playback_contract.c`) on Linux, macOS and Windows.
