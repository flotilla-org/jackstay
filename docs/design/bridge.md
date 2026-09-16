# Cross-host bridge

Status: first working slice, 2026-09-16. A bridge is a translator peer in the
sense of porthole ADR-0008: the egress half is an ordinary native consumer that
encodes what it acquires and sends it over a link; the ingress half is an
ordinary native producer that decodes what arrives and republishes it as a new
local publication. Neither half is a coordinator. The design and the research
behind it are in the project-map report
`reports/jackstay-remoting-research-2026-09-16.md`; this note records what the
crates do, what was verified, and what was not.

## Crates

`jackstay-graph` holds the graph-manager vocabulary an export needs and nothing
else: `ChromaPolicy` (`require_444 | prefer_444 | any`), `CodecCapabilities` as
reported by a probe, `decide` which picks HEVC 4:4:4, then H.264 4:4:4, then
4:2:0 only if the policy allows, `Target`, the two identities an export keeps
apart (source and running publication), `ExportRequest`, and `mint_token`.

`jackstay-bridge` holds the halves. `wire` is the carrier-agnostic framing: a
32-byte little-endian header, binary frame bodies carrying the 144-byte
descriptor verbatim plus an op list, JSON control bodies. `clock` maps the
producer's clock domain onto the consumer host from four-timestamp pings.
`vt` wraps `vt_shim.m`: hardware encode from an `IoSurface`, hardware decode to
a YCbCr pixel buffer, transfer into a BGRA `IoSurface`, and the capability
probe. `egress` and `ingress` are the halves; `jackstay-bridge` is the binary.

## What the halves do

The egress half attaches to a publication over XPC (an anonymous endpoint in
process, or a launchd-registered service with an attach token), exchanges a
hello on the control connection, and decides the codec from both ends'
probed capabilities. Its acquisition loop is the reference viewer's: snapshot
events, acquire latest, wait. For each frame it waits on the producer fence,
wraps the surface in a pixel buffer and submits it to VideoToolbox; the lease
travels as the frame refcon and is released in the encoder's output callback,
which is when VideoToolbox has finished reading the surface. Output goes to a
sender with two slots, newest keyframe with its codec configuration and newest
delta; a keyframe replaces everything queued. Encoder sessions are recreated on
a size change and the next frame is forced to a keyframe.

The ingress half reads the media stream. A codec configuration creates or
reconfigures the decoder with the output format whose range matches the
stream, and creates the producer on first sight. The producer is the transport
core's macOS backend, so it owns a fixed BGRA pool and blits same-format
surfaces only; each decoded frame is therefore transferred into one of a small
ring of bridge-owned BGRA staging surfaces, which is the YCbCr to RGB
conversion the viewer needs regardless, and published, which blits it into a
pool slot and signals the arena fence on the GPU. A sequence gap or a decode
error puts the half back into waiting-for-keyframe and sends a keyframe
request on the control connection. Timestamps are remapped with the clock
estimate; the clock domain value is left as the producer stamped it because the
model has no remote-mapped domain yet.

With `--cpu-socket PATH` the ingress also serves the republication over the
transport core's generic CPU setup socket (`acquisition::socket::serve_cpu`),
for consumers that have no native attach path, such as katzensteg's
`jackstay-source`. After each frame's transfer into the staging surface the
same surface is read back, one locked copy, into a CPU arena sized for the
current frame; a size change reconfigures that arena and drops frames while
the reconfiguration waits on capacity. On Apple silicon the staging surface is
in unified memory, so the readback is a memcpy rather than a bus transfer. The
socket is bound fresh and never replaces an existing path, is owner-only
(mode 0600, the same rule katzensteg applies to its own endpoints), accepts
any number of setup connections each served on its own thread, and is unlinked
at shutdown. The native publication is unaffected: the CPU copy happens before
the pool blit is queued and the two producers keep separate arenas and
counters (`cpu_frames_published`, `cpu_frames_dropped`, `cpu_errors` in the
report). Which kinds a republication offers is the coordinator's choice per
request; the graph crate carries it as `IngressSpec::cpu_socket` and reports
the bound path in `publication_up`.

## Driving the halves from a coordinator

The halves print one JSON object per line on stdout as they progress
(`listening`, `ready` with the codec decision, `publication_up` with the
service and token, `report` with the final report, `failed`). `jackstay-graph`
reads them: `export::spawn_egress` runs an egress half as a child and watches
its stdout; `export::IngressJob` registers an ingress half as a launchd job
through `launchd::LaunchdJob` and reads the job's stdout file. Both expose a
`HalfStatus` with the phase, decision, publication and report. portholed uses
exactly these to implement exports and republications; see porthole's
`docs/2026-09-16-publications-and-exports.md`.

## Verified on 2026-09-16

All on Apple M4, macOS 26.6, debug and release builds.

`jackstay-bridge loopback`: a synthetic producer publishes drawn frames over an
anonymous endpoint, both halves run in one process over socket pairs, and a
verifying consumer reads back republished frames and compares them with what
was drawn. 60 frames at 1280x720: every frame the egress acquired was encoded,
sent, decoded and published with hardware HEVC 4:4:4 at both ends and a shared
decoder pool; all acquired frames verified with a worst mean absolute error of
0.21 per channel. 300 frames at 2560x1440 and 60 fps in a release build: 282
received, 282 published, 263 verified, none mismatched, worst mean error 0.23,
no drops at the sender or in the arena. Clock estimate on the same host: offset
under 10 microseconds, round trip about 25 microseconds.

`jackstay-bridge loopback --viewer-service NAME`: the ingress half runs under
launchd as a Mach service; the unmodified reference viewer attached from
another process with `--native --mach-service NAME --token TOKEN` and
presented 90 frames; the egress and ingress counters agreed (169 acquired,
encoded, received, decoded, published); the launchd job and sockets were
removed on interrupt.

Two hosts over an SSH stream-local forward, 2026-09-16: `jackstay-bridge
synthetic` on comte (M4, macOS 26.5.1) as a launchd-registered source,
`jackstay-bridge egress --listen` on comte, `ssh -N -L` from kiwi with
`aes128-gcm@openssh.com` forwarding both Unix sockets, `jackstay-bridge
ingress-service` on kiwi registering the ingress with launchd, and the
reference viewer attached on kiwi. The viewer presented 150 frames; both halves
counted 159 frames acquired, encoded, received, decoded and published, one
keyframe, no drops, hardware HEVC 4:4:4 at both ends, shared decoder pool.
The clock estimate reported a minimum round trip of 0.56 ms through the
forward and an offset of about 3.4e15 ns, which is the difference between the
two machines' uptime epochs and is what the mapping exists to absorb.

Real capture across two hosts, 2026-09-16: portholed from the
shared-native-acquisition branch on comte (installed bundle, Screen Recording
granted) captured the booted iPhone 16 Pro simulator window as a native
session; `jackstay-bridge egress --listen` on comte attached to
`work.flotilla.porthole.attach` with the session's attach token, exactly as the
reference viewer would; the same SSH forward, launchd ingress on kiwi and
reference viewer as above. Over about a minute the viewer presented 85 frames
and the halves counted 86 acquired, encoded, received, decoded and published,
no drops, hardware HEVC 4:4:4 both ends, shared decoder pool, 0.53 ms round
trip; the simulator was visible on kiwi. ScreenCaptureKit delivers only on
change, so an idle simulator produces a frame or two per second; the low count
is the source, not the link. The viewer's native window is a fixed 320x180 at
that revision, which squashed the portrait frame; the window now follows the
frame's aspect.

A first attempt failed because the listening half bound its second socket only
after accepting the first, and the forward fails an open outright when the
target path does not exist; both listeners are now bound before either is
accepted. launchd relaunches an on-demand Mach service each time something
looks it up, so an ingress whose link died was respawned repeatedly by a
viewer's connection attempts; the job is removed on interrupt but the
relaunch behaviour is worth a `LaunchOnlyOnce` or a viewer-side timeout later.

The CPU publication was verified in loopback on kiwi (640x360, 600 frames):
the ingress published every decoded frame to both arenas, two SDL viewers
attached in turn on the CPU socket (`--cpu-socket`, 45 and 30 frames), the
socket was created owner-only and was gone after the run.

The gates (`cargo build`, `test`, `clippy -D warnings`, pinned `fmt`) pass
with and without `backend-macos`. The `vt` round-trip test runs in the normal
test set and skips itself on a machine without hardware 4:4:4.

## Not verified

- Any machine other than M4. The probe decides at session start, so other
  chips will get a recorded fallback rather than a surprise, but no fallback
  path has been exercised.
- A capture with motion: the real-capture run showed an idle simulator. A
  scrolling or animating app on comte would exercise the sender's drop policy
  and the rate cap over the forward.
- The staging ring reuses a surface after `staging_depth` frames without
  confirming that the pool blit reading it has completed. On this hardware the
  blit finishes within a frame period; a completion check belongs in the
  backend when the conversion moves there.
- Owner-side output sizing, targets beyond `max_fps`, deferred release, and
  more than one consumer on the republished publication.
- The `Copy`, `Pixels` and region `Video` ops are reserved names in the wire
  and are not implemented.
