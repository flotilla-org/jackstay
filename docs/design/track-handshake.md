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

Kinds are open. The core kinds are `video`, `input` and `affordances`; an app
kind is a reverse-domain name of lowercase labels, such as `org.example.cue`,
whose descriptor Jackstay treats as opaque. A host offers at most one video, one
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
`Producer` for admitted tracks. Serve CPU setup on the media stream when a
video track was admitted.

Consumers call `handshake::connect(stream)`, inspect `Offered::offer()`, then
`select(&selection, mode)`. `Connected` holds the media stream, the admissions,
an input `Client` or an optional input's clean admission rejection, and the
affordances `Host`.

`jackstay_handshake.h` exposes the same exchange as typed structs on Local
Endpoint connections: `ft_handshake_accept_local` takes the clock domain and
`ft_track_offer`s and writes `ft_track_admission`s; `ft_handshake_connect_local`,
`ft_handshake_offer_describe` and `ft_handshake_select` read the offer and
submit `ft_track_selection`s. POSIX callers with a connected socket wrap it with
`ft_local_connection_adopt`. After basic checks the connection or offer is
consumed on failure; a required refusal returns `FT_STATUS_UNSUPPORTED`.

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
covers the same exchange across processes.
