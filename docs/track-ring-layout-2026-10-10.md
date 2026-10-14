# Track ring layout: one ring per track or one shared ring per publication

Status: research input for
[#106](https://github.com/flotilla-org/jackstay/issues/106) (map
[#97](https://github.com/flotilla-org/jackstay/issues/97)), feeding the grilling
ticket "Audio delivery contract"
([#100](https://github.com/flotilla-org/jackstay/issues/100)). It makes a
recommendation; it records no decision.

Researched 2026-10-10 against primary sources: upstream source at pinned tags
(PipeWire 1.4.0, GStreamer 1.24.12, JACK2 v1.9.22, Chromium 130.0.6723.58,
FFmpeg n7.1, Linux v6.11), libwebrtc via the `webrtc-sdk/webrtc` fork at
`m125_release` (upstream googlesource was unreachable from the research
environment), and Apple and Android developer reference pages. IETF hosts were
unreachable, so RFC wording is marked **UNVERIFIED** where it was not read
directly.

## The question

A publication is one source's set of tracks (#102). Video already moves through
the acquisition arena: leased frames, holding reservations, a publication ring of
128-byte descriptors ([ADR 0001](adr/0001-acquisition-leases-and-reservations.md),
[`jackstay_ring.h`](../crates/jackstay/include/jackstay_ring.h)). Audio is
proposed as a seqlock broadcast ring of fixed-size blocks per track, copied out by
readers. Should several tracks (one video, several translated audio and caption
tracks) instead share one interleaved ring that consumers filter? The concerns are
timing alignment and avoiding lock fiddling.

## Summary

- **No surveyed live IPC system interleaves tracks in one shared buffer to keep
  them in sync.** PipeWire, GStreamer, JACK, Chromium, AVFoundation and WebRTC
  all keep per-stream (per-port, per-pad, per-SSRC) buffers. They align streams
  by stamping each buffer on one shared clock and letting the consumer schedule
  against that clock.
- **Interleaving shows up in containers and transports** (MPEG-TS, Matroska,
  BUNDLEd RTP). It serves locality: one file, one sequential read, one network
  flow or NAT binding. The costs are explicit in the code. Muxers buffer until
  every stream has data (FFmpeg defaults to up to 10 s). Readers that want a
  subset still parse every packet header and discard the rest. Demuxers then need
  per-stream queues (GStreamer `multiqueue`) whose fullest queue blocks every
  stream.
- **Wakeups are where systems do share.** A PipeWire node with many ports has
  one eventfd and one activation record, and links between two nodes collapse
  into a single target. This matches Jackstay's existing one coalescing
  doorbell per incarnation.
- **Recommendation: a hybrid.** Use one single-writer data ring per track, each
  with its own geometry and its own read-only mapping. Share one clock domain,
  one track manifest and one coalescing doorbell per incarnation (with a track
  bitmask). Do not add a per-publication interleaved index of every block.

## Survey

### PipeWire

- **Per-port buffers, per-node scheduling.** Buffers are negotiated per port as
  a count of buffers, blocks per buffer and block size (`SPA_PARAM_BUFFERS_*`,
  [buffers.h L21-28](https://gitlab.freedesktop.org/pipewire/pipewire/-/blob/1.4.0/spa/include/spa/param/buffers.h#L21-28)).
  The per-port exchange area is just `{status, buffer_id}`
  ([io.h L45-82](https://gitlab.freedesktop.org/pipewire/pipewire/-/blob/1.4.0/spa/include/spa/node/io.h#L45-82)).
  Each `spa_data` is a memfd, a dmabuf or a pointer
  ([buffer.h L36-39](https://gitlab.freedesktop.org/pipewire/pipewire/-/blob/1.4.0/spa/include/spa/buffer/buffer.h#L36-39)),
  so a video port carries dmabufs while an audio port carries small memfd blocks,
  each in its own pool. "All of the node resources such as buffers, io areas and
  metadata will be set up in shared memory before the node is scheduled to run"
  ([scheduling.dox L19-21](https://gitlab.freedesktop.org/pipewire/pipewire/-/blob/1.4.0/doc/dox/internals/scheduling.dox#L19-21)).
- **One doorbell per node, not per port.** Each node has "an eventfd to signal
  the node that it can start processing" and a shared-memory activation record
  (L31-46). "Multiple links between A and B will only result in 1 target link
  between A and B" (L89). Remote clients hold peers' eventfds and trigger them
  directly (L189-203). `pw_filter` exposes several ports on one node
  ([filter.h](https://gitlab.freedesktop.org/pipewire/pipewire/-/blob/1.4.0/src/pipewire/filter.h)),
  which is how one client keeps several streams in one process cycle.
- **Cross-stream timing.** The driver writes `spa_io_clock.nsec`, "Time in
  nanoseconds against monotonic clock (CLOCK_MONOTONIC)", together with
  `position`, `rate_diff` and `next_nsec`, and every node sees it through
  `spa_io_position` ([io.h L127-160, L293](https://gitlab.freedesktop.org/pipewire/pipewire/-/blob/1.4.0/spa/include/spa/node/io.h#L127-160)).
  Buffers carry `spa_meta_header.pts` and `seq`
  ([meta.h L70-83](https://gitlab.freedesktop.org/pipewire/pipewire/-/blob/1.4.0/spa/include/spa/buffer/meta.h#L70-83)).
  `pw_time` gives `now`, `ticks`, `delay`, `queued` and `buffered`. The docs say
  the delay "requires the application to send the stream early relative to other
  synchronized streams" ([stream.h L293-371](https://gitlab.freedesktop.org/pipewire/pipewire/-/blob/1.4.0/src/pipewire/stream.h#L293-371)).
  Sync is timestamp arithmetic against one monotonic clock, not co-location.
- **Subset cost.** Unlinked ports cost nothing: a consumer links only the ports
  it wants.
- **Slow reader.** The graph is synchronous per cycle. At the next cycle the
  driver checks "Did it complete? Mark xrun on unfinished nodes"
  (scheduling.dox L152). That is the opposite of Jackstay's "producer never
  waits", but the per-port isolation of buffers still holds.

### GStreamer

- **One pad per elementary stream.** Demuxers expose `GST_PAD_SOMETIMES` source
  pads, one per track, and signal `no-more-pads`
  ([matroska-demux.c L110-124, L1792, L3465](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/1.24.12/subprojects/gst-plugins-good/gst/matroska/matroska-demux.c#L110-124)).
- **Cross-stream timing.** There is one `GstClock` per pipeline. Each buffer's
  `running_time` comes from its segment, and a sink renders when
  `absolute_time = B.running_time + base_time`. "For multiple streams this means
  that buffers with the same running_time are to be displayed at the same time.
  A demuxer must make sure that the SEGMENT it emits on its output pads yield the
  same running_time for buffers that should be played synchronized"
  ([synchronisation.md L142-198](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/1.24.12/subprojects/gst-docs/markdown/additional/design/synchronisation.md#L142-198)).
- **Cost of a shared interleaved upstream.** A demuxer's flow return is
  `NOT_LINKED` "only if all returns are NOT_LINKED too"
  ([gstflowcombiner.c L29-52](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/1.24.12/subprojects/gstreamer/libs/gst/base/gstflowcombiner.c#L29-52)),
  so it keeps reading and parsing every track while any one track is wanted.
  `multiqueue` must grow per-stream queues so that "no stream is risking data
  starvation". It pushes to non-linked pads in order so they do not race ahead,
  and "Any attempt to push more buffers into the queue will block the pushing
  thread until more space becomes available" (defaults: 5 buffers, 10 MB, 2 s)
  ([gstmultiqueue.c L25-90](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/1.24.12/subprojects/gstreamer/plugins/elements/gstmultiqueue.c#L25-90)).
  One slow branch therefore throttles the shared source.
- **shmsink/shmsrc** carries one pad's byte buffers. Its wire messages are "new
  shm area", "close", "shm buffer: offset, bufsize" and "ack buffer: offset",
  and "The client should never write in the SHM"
  ([shmpipe.c L50-73](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/1.24.12/subprojects/gst-plugins-bad/sys/shm/shmpipe.c#L50-73)).
  The protocol has no caps or timestamps. A multi-track source needs one
  shmsink per stream. The producer blocks on `g_cond_wait` when allocation fails
  because readers still hold unacked buffers, or when `buffer-time` is exceeded
  ([gstshmsink.c L645-660, L693-740](https://gitlab.freedesktop.org/gstreamer/gstreamer/-/blob/1.24.12/subprojects/gst-plugins-bad/sys/shm/gstshmsink.c#L645-740)).
  This is the reader-holds-storage model that Jackstay's leases bound with
  reservations.

### JACK

- Each port has its own buffer for the current period. `jack_port_get_buffer`
  "returns a pointer to the memory area associated with the specified port", and
  output-port buffers "have to be retrieved in each callback"
  ([jack.h L755-776](https://github.com/jackaudio/jack2/blob/v1.9.22/common/jack/jack.h#L755-776)).
  All ports of all clients are processed in one cycle. Timing is the cycle's frame
  time plus `jack_get_cycle_times` (frames, current/next usecs, DLL-smoothed
  period) (L1344-1389). Alignment comes from the shared cycle, so JACK has no
  per-buffer timestamps. JACK does not carry video.

### Chromium audio IPC

- **One shared-memory region plus one socket per audio stream.**
  `ReadOnlyAudioDataPipe { ReadOnlySharedMemoryRegion shared_memory; handle<platform> socket; }`
  ([audio_data_pipe.mojom L28-36](https://chromium.googlesource.com/chromium/src/+/refs/tags/130.0.6723.58/media/mojo/mojom/audio_data_pipe.mojom)).
  Each buffer is a small header followed by PCM. The output header has
  `delay_us`, `delay_timestamp_us` (TimeTicks), and glitch duration and count. The
  input header has `capture_time_us`. The fields are laid out for mixed 32/64-bit
  access ([audio_parameters.h L38-71](https://chromium.googlesource.com/chromium/src/+/refs/tags/130.0.6723.58/media/base/audio_parameters.h)).
  `SyncSocket` is a blocking socket used for "rudimentary cross-process
  synchronization with low latency"
  ([sync_socket.h L8-10](https://chromium.googlesource.com/chromium/src/+/refs/tags/130.0.6723.58/base/sync_socket.h)).
- **Slow peer.** The audio service waits at most half a buffer duration. On
  timeout it zero-fills, counts a glitch and carries on
  ([services/audio/sync_reader.cc L67, L192-208](https://chromium.googlesource.com/chromium/src/+/refs/tags/130.0.6723.58/services/audio/sync_reader.cc)).
  The device is never blocked by the renderer.
- **Video is a separate mechanism.** Capture buffers are shared memory or GPU
  handles (`VideoBufferHandle` union,
  [video_capture_buffer.mojom L50-59](https://chromium.googlesource.com/chromium/src/+/refs/tags/130.0.6723.58/media/capture/mojom/video_capture_buffer.mojom)).
  A/V sync happens in the renderer: `AudioRendererImpl` is the `TimeSource`
  ([audio_renderer_impl.h L59](https://chromium.googlesource.com/chromium/src/+/refs/tags/130.0.6723.58/media/renderers/audio_renderer_impl.h)),
  and `VideoRendererImpl` maps frame timestamps to wall clock through
  `wall_clock_time_cb` ([video_renderer_impl.h L77, L178-197](https://chromium.googlesource.com/chromium/src/+/refs/tags/130.0.6723.58/media/renderers/video_renderer_impl.h)).
  The audio device is master. This matches Jackstay's consumer-side decision.

### Apple AVFoundation / CoreMedia (optional)

- Audio and video capture use separate outputs (`AVCaptureAudioDataOutput`,
  `AVCaptureVideoDataOutput`), each delivering `CMSampleBuffer`s. "All capture
  output sample buffer timestamps are on the synchronization clock's timebase"
  ([AVCaptureSession.synchronizationClock](https://developer.apple.com/documentation/avfoundation/avcapturesession/synchronizationclock)).
  Per-stream slow-reader policy: `alwaysDiscardsLateVideoFrames` discards frames
  while the delegate blocks. The alternative lets "application memory usage …
  increase significantly"
  ([docs](https://developer.apple.com/documentation/avfoundation/avcapturevideodataoutput/alwaysdiscardslatevideoframes)).
  `CMSimpleQueue` is "a simple lockless queue of elements"
  ([docs](https://developer.apple.com/documentation/coremedia/cmsimplequeue)).

### Android AAudio (optional)

- "A stream can only move data in one direction". Several streams may be
  processed together by using "one stream as the master"
  ([AAudio guide](https://developer.android.com/ndk/guides/audio/aaudio/aaudio)).
  The MMAP and timestamp clock details were not confirmed from the fetched page
  (**UNVERIFIED**).

### Interleaved containers and transports

- **MPEG-TS** multiplexes 188-byte packets tagged by a 13-bit PID. FFmpeg reads
  the PID from every packet, then drops packets of unwanted programs
  (`pid = AV_RB16(packet + 1) & 0x1fff; … if (tss->discard) return 0;`,
  [mpegts.c L2772-2791](https://github.com/FFmpeg/FFmpeg/blob/n7.1/libavformat/mpegts.c#L2772-2791);
  [mpegts.h L29](https://github.com/FFmpeg/FFmpeg/blob/n7.1/libavformat/mpegts.h#L29)).
  Linux DVB moves PID filtering into the demux device ("a filter that is based
  just on the packet identifier",
  [dmx-set-pes-filter.rst](https://github.com/torvalds/linux/blob/v6.11/Documentation/userspace-api/media/dvb/dmx-set-pes-filter.rst)).
  The reader still pays transport bandwidth for every track.
- **Matroska/WebM** interleaves Blocks tagged by TrackNumber inside Clusters. The
  demuxer parses each block's track number before discarding
  (`if (st->discard >= AVDISCARD_ALL) return res;`,
  [matroskadec.c L4105-4128](https://github.com/FFmpeg/FFmpeg/blob/n7.1/libavformat/matroskadec.c#L4105-4128)).
  The muxer side pays too. `ff_interleave_packet_per_dts` holds packets until
  every interleaved stream has one buffered, or until the spread exceeds
  `max_interleave_delta` (default 10 000 000 µs)
  ([mux.c L959-1023](https://github.com/FFmpeg/FFmpeg/blob/n7.1/libavformat/mux.c#L959-1023),
  [options_table.h L89](https://github.com/FFmpeg/FFmpeg/blob/n7.1/libavformat/options_table.h#L89)).
  For a live ring this is a choice between added latency and arrival-order
  writes, which give no alignment benefit.
- **RTP.** RFC 3550 §5.2 says audio and video should travel in separate RTP
  sessions. One reason it gives is that receivers can take only a subset, e.g.
  audio when video exceeds bandwidth (**UNVERIFIED** wording, IETF hosts
  unreachable; [RFC 3550](https://www.rfc-editor.org/rfc/rfc3550#section-5.2)).
  BUNDLE ([RFC 8843](https://www.rfc-editor.org/rfc/rfc8843)) later put several
  media in one 5-tuple for NAT and port economy. Receivers then demux by MID,
  RSID, SSRC or payload type into per-stream sinks
  ([libwebrtc rtp_demuxer.h L74-103](https://github.com/webrtc-sdk/webrtc/blob/m125_release/call/rtp_demuxer.h#L74-103)).
  Every stream keeps its own sequence and timestamp space after the demux.
- **WebRTC A/V sync** maps each stream's RTP timestamps to NTP using RTCP sender
  report pairs (`RtpToNtpEstimator`: "needs to be trained with (at least 2)
  RTP/NTP timestamp pairs",
  [rtp_to_ntp_estimator.h L25-51](https://github.com/webrtc-sdk/webrtc/blob/m125_release/system_wrappers/include/rtp_to_ntp_estimator.h#L25-51)).
  It then computes a relative delay and adjusts each stream's minimum playout
  delay, at most 80 ms per step
  ([stream_synchronization.cc L21-89](https://github.com/webrtc-sdk/webrtc/blob/m125_release/video/stream_synchronization.cc#L21-89);
  [rtp_streams_synchronizer2.cc L120-160](https://github.com/webrtc-sdk/webrtc/blob/m125_release/video/rtp_streams_synchronizer2.cc#L120-160)).
  Even over one bundled transport, sync is by shared-clock timestamps.

## Comparison

| System | Data per stream? | Cross-stream timing | Reader wants a subset | Slow reader | Big leased video + small copied audio |
|---|---|---|---|---|---|
| PipeWire | per port pool | driver clock on CLOCK_MONOTONIC + pts | link only those ports | cycle xrun | separate pools, dmabuf vs memfd |
| GStreamer | per pad | pipeline clock, running_time | demuxer still parses all; multiqueue | blocks upstream (queue / shm alloc) | separate pads and allocators |
| JACK | per port | shared cycle | connect only those ports | xrun | no video |
| Chromium | per stream shm + socket | renderer TimeSource (audio master) | open only that stream | zero-fill + glitch count | separate mechanisms |
| AVFoundation | per output | session sync clock | add only those outputs | discard late frames | separate outputs |
| MPEG-TS / Matroska | interleaved | PCR / block timestamps | parse every packet, discard | n/a (file) or bandwidth | one byte stream |
| RTP / WebRTC | per SSRC (maybe bundled transport) | RTCP SR RTP to NTP | separate sessions or demux | jitter buffer per stream | separate packetization |

## What a shared Jackstay ring would cost

Each point is tied to a decision already taken.

1. **Single-writer seqlock breaks.** The ring header's protocol is "single
   producer, many readers": one `producer_cursor` and plain stores of the slot
   body ([`jackstay_ring.h`](../crates/jackstay/include/jackstay_ring.h)). Audio
   tracks are typically written from audio callback threads, video from a render
   or capture thread, captions from somewhere else. A shared ring would need a
   multi-producer cursor reservation (CAS or a lock) and would lose "producer
   never blocks or knows readers". This is the lock fiddling the user wants to
   avoid. Per-track rings keep every writer lock-free.
2. **Lapping becomes cross-track.** Ring depth is a count of slots. In a shared
   ring, the ≥250 ms audio depth must be sized for the sum of all tracks' block
   rates. A reader of one caption track gets lapped by audio it never reads. A
   burst on one track shortens every other track's history. Per-track rings let
   each track set its own depth in its own units.
3. **Slot shapes do not match.** Video advertisements are 128-byte descriptors
   whose payload sits under a lease in the arena. Audio blocks are header plus
   interleaved PCM, sized by the producer, and copied out. Captions are sparse
   and variable. One ring means max-sized slots (wasting the audio-block size on
   every video descriptor) or variable-length records. Variable-length records
   break mask indexing and the `publication_sequence == cursor` identity check.
4. **Two lifetime regimes cannot share an advertisement.** ADR 0001 separates
   the ring entry (an opportunity) from the lease (ownership), with holding
   reservations. Audio has no lease: the seqlock copy is the whole read.
   Interleaving them would put leased and copy-out entries in one cursor space
   with different validity rules.
5. **Read-only mapping is the authorization unit.** A shared ring mapped
   read-only exposes every track's bytes to every consumer, including recording
   audio to a consumer that selected only one playback language. Per-track
   mappings let the capture host grant per track. That matches #102's
   per-incarnation subset selection, and Jackstay's rule that the capture host
   supplies authority.
6. **Alignment gains nothing.** Every system surveyed aligns by timestamp on a
   shared clock. Jackstay already has one host-time domain per publication, and
   every audio block header carries the host time of its first sample. A consumer
   aligns by reading each selected ring and comparing timestamps; it never needs
   an atomic snapshot across rings. Muxers that interleave for alignment pay
   buffering latency (FFmpeg: up to 10 s by default) to do it.

What a shared ring would buy is one mapping, one cursor to poll and a global
arrival order. The doorbell already gives the first two. Arrival order is not
presentation order (pw_time's delay discussion, WebRTC's relative-delay
correction), so consumers cannot use it for alignment.

## Recommendation: per-track data rings, per-publication clock and manifest, per-incarnation doorbell

- **Data.** One ring per track, single writer, own geometry. Video keeps the
  arena publication ring and leases. Each audio track gets a seqlock broadcast
  ring of producer-sized blocks (≥250 ms). Sparse tracks (captions, affordance
  events) get whatever shape suits them, decided per track kind.
- **Mapping and authorization.** One read-only mapping per track. The setup
  channel hands a consumer incarnation only the mappings for tracks it selected
  and is authorized for.
- **Shared per publication, and immutable or rarely written.** The clock domain
  (host monotonic time), the track manifest (ids, kinds, language and role,
  current descriptor generation), and lifecycle and discontinuity epochs if #100
  wants them. These are written on reconfiguration, not on every block, so a
  seqlock-on-generation record (like `jackstay_stream_config`) is enough.
- **Wake.** Keep one coalescing doorbell per incarnation
  ([wait.rs](../crates/jackstay/src/acquisition/arena/wait.rs): `DATA`,
  `CAPACITY`, `RECONFIGURATION`, `CLOSED` over a socket pair). Add a per-track
  "changed" bitmask the consumer reads and clears, so a consumer of one video and
  one audio track wakes once per batch, not once per ring. PipeWire does the same
  with one eventfd per node over many ports.
- **No per-publication block index.** An index of every block across tracks
  would recreate the shared ring's multi-writer cursor (point 1) and its
  cross-track lapping (point 2).

For the motivating case (video plus N translated audio and caption tracks, a
consumer wanting one language): the consumer maps three rings (video, audio-xx,
captions-xx), reads zero bytes of the other languages, and wakes on one doorbell.
It aligns them by host-time timestamps, with its audio device as master.

## Open questions for #100 (Audio delivery contract)

1. **Doorbell payload.** A per-track bitmask (caps tracks per incarnation at 64)
   or a scan of each selected ring's cursor on wake? Who clears the bits, and is
   a lost bit tolerable when the cursor scan is authoritative?
2. **Discontinuities across tracks.** Are seek, pause, source restart and
   clock-domain resets per-track block flags, a publication-level epoch, or both?
   GStreamer needs the same SEGMENT on all pads for this.
3. **Writer threads.** Are tracks of one publication ever written by one thread
   in lockstep? If so, is a batched multi-ring publish worth defining, or should
   it stay out of the contract?
4. **Drift.** Each block carries sample position and host time. Does the
   contract require consumers to estimate rate drift from consecutive blocks
   (as `pw_time` / `jack_get_cycle_times` / `RtpToNtpEstimator` do), or does the
   producer publish a smoothed rate?
5. **Sparse tracks.** Fixed-size blocks fit PCM. Captions and app-defined tracks
   need either a variable-payload pool beside the ring or a separate track kind.
   Is that in #100's scope?
6. **Depth in time vs count.** Is ≥250 ms a per-track floor stated in time,
   leaving the producer to convert to slots from block size and rate?
7. **Mapping budget.** Per-track segments multiply fds and mappings (and Mach
   ports on macOS). Is there a cap on tracks per incarnation, or a fallback that
   carves several tracks out of one segment when they share an authorization?
8. **Per-track authorization.** Is a track the unit of grant (playback audio vs
   recording audio vs captions), and does track selection change only through
   the setup channel on a new grant, or mid-incarnation?
9. **Late consumer start.** When a consumer joins mid-stream, does it start each
   track at "latest minus latency target" independently and then align, or does
   the contract name a common start host time?
