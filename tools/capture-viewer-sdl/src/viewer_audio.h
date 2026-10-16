#ifndef VIEWER_AUDIO_H
#define VIEWER_AUDIO_H
#include <SDL.h>
#include <stdatomic.h>
#include <stdint.h>
#include "jackstay_handshake.h"

/* Playout of one playback track: format conversion, linear-interpolation
 * resampling whose ratio a PI controller steers so each sample is heard
 * latency_ns after its source host time, short crossfades over gaps, and
 * silence in underrun. Rendering runs on the audio thread: it neither blocks
 * nor allocates. */
enum { VIEWER_AUDIO_CHANNELS = 8, VIEWER_AUDIO_FADE = 128 };

/* Copies out the next block: 1, or 0 in underrun. */
typedef int (*viewer_audio_read)(void *context, ft_audio_block *out);

typedef struct viewer_audio {
  /* Set before the first render. */
  viewer_audio_read read;
  void *context;
  uint32_t rate, channels; /* the output device's */
  uint64_t latency_ns;
  atomic_int muted;
  /* Source host time heard at host time t is t + clock_offset_ns, INT64_MIN
   * while nothing plays. Any thread may load it. */
  _Atomic int64_t clock_offset_ns;
  /* Audio thread only: read them once it has stopped. */
  uint64_t underruns, resyncs, gaps;
  double drift;             /* the producer's rate against the device's, less one */
  double correction, error_ns; /* the latest ratio correction and playout error */
  ft_audio_block block;
  uint32_t index, conceal;
  int primed, aligned;
  double phase, period_ns, a_time, b_time;
  float a[VIEWER_AUDIO_CHANNELS], b[VIEWER_AUDIO_CHANNELS], held[VIEWER_AUDIO_CHANNELS], last[VIEWER_AUDIO_CHANNELS];
} viewer_audio;

void viewer_audio_init(viewer_audio *audio, uint32_t rate, uint32_t channels, uint64_t latency_ns);
/* Fills frames interleaved float frames whose first is heard at
 * now_ns + frames / rate. */
void viewer_audio_render(viewer_audio *audio, float *out, uint32_t frames, uint64_t now_ns);

/* One selected playback track on its own SDL output device. */
typedef struct viewer_playback {
  const char *label; /* the track to play; NULL plays none */
  uint64_t latency_ns;
  int muted;
  uint64_t track;
  ft_audio_descriptor descriptor;
  ft_playback_ring *ring;
  ft_playback_reader *reader;
  ft_tracks *tracks;
  SDL_AudioDeviceID device;
  viewer_audio audio;
} viewer_playback;

/* Logs offered audio tracks; 1 when this one is the track to select. */
int viewer_playback_offered(viewer_playback *playback, const ft_track_offer *offer);
/* Opens the device for an admitted ring (SDL audio initialized). The track is
 * optional: a failure is logged and leaves video playing without it. */
void viewer_playback_start(viewer_playback *playback);
/* Follows track notices, selecting the labelled track when it is added late. */
void viewer_playback_poll(viewer_playback *playback);
/* The playout clock offset (see viewer_audio), INT64_MIN without audio. */
int64_t viewer_playback_clock(viewer_playback *playback);
void viewer_playback_close(viewer_playback *playback);
#endif
