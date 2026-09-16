# vt-probe

VideoToolbox capability and latency probe for the cross-host bridge. It answers,
on the machine it runs on, the questions the bridge halves must answer at session
start: which hardware encoders and profiles exist, whether hardware 4:4:4 encode
and decode work, what the encode and decode latencies are, whether the decoder
shares its pool with the client, and how much RGB error a chosen path introduces
on text-like content. It needs no porthole, no Jackstay library and no capture
permission; frames are drawn with CoreText.

```sh
tools/vt-probe/build.sh            # -> build/vt-probe
build/vt-probe                     # full matrix, human readable
build/vt-probe --json all          # same, machine readable
build/vt-probe caps                # encoder list, profile lists, decode support
build/vt-probe ltr                 # EnableLTR on the low-latency encoders
build/vt-probe roundtrip hevc HEVC_Main444_AutoLevel bgra 2560x1440
build/vt-probe roundtrip h264 H264_High444Predictive_AutoLevel bgra
build/vt-probe roundtrip hevc HEVC_Main444_AutoLevel 444f 1920x1080 --lowlatency
```

Each roundtrip encodes 60 frames (after 3 warm-up frames) at 60 fps pacing with
hardware required, `RealTime`, no frame reordering, a 20 Mbit/s average target and
709/sRGB colour tags, then decodes them with hardware required into the output
format whose chroma matches the profile and whose range matches the stream, and
converts the last decoded frame back to BGRA to compare with its source.

Fields worth reading first:

- `using_hw_encoder`, `encoder_id`, `sps_general_profile_idc` (4 = HEVC RExt) or
  `sps_profile_idc` and `sps_chroma_format_idc` (244 and 3 = H.264 High 4:4:4).
  The low-latency encoders never report `using_hw_encoder`; trust the SPS.
- `decoder_pool_is_shared`. When 0 the decoder keeps a private pool and copies
  every frame; decode latency rises three to eight times. It happens when the
  requested output range does not match the stream's range.
- `encode_ms_p50`, `decode_ms_p50`: encode call to output callback, decode call
  to output callback, steady state. `warmup_first_frame_ms` is the price of a
  cold session.
- `rgb_roundtrip`: mean and max absolute error per channel and the share of
  pixels with any channel off by more than 8, source BGRA versus decoded frame
  converted back to BGRA.

`results/` holds dated runs. `kiwi-2026-09-16` and `comte-2026-09-16` are both
Apple M4 (macOS 26.6 and 26.5.1) and agree: hardware HEVC Main 4:4:4 and H.264
High 4:4:4 encode and decode, about 5 to 6 ms encode and 3 ms decode at 1080p,
8 to 9 ms encode at 1440p, under 0.2 percent of text pixels visibly off in 4:4:4
against 8 percent in 4:2:0. Runs on M1 to M3 machines are wanted.

The runtime capability check in the bridge's Objective-C shim should mirror the
`caps` and first `roundtrip` steps: create the session with the profile string,
read the SPS back, and record the decision in session status.
