#include "viewer_audio.h"
#include "host_time.h"
#include <inttypes.h>
#include <math.h>
#include <stdio.h>
#include <string.h>

/* The controller: a ratio correction of KP per second of playout error, plus
 * an integral that settles on the producer's drift (critically damped, about
 * ten seconds). Errors beyond RESYNC_NS realign at once instead. */
#define KP 1.0
#define KI 0.25
#define MAX_CORRECTION 0.01
#define RESYNC_NS 20e6

static double clamp(double x, double limit) { return x > limit ? limit : x < -limit ? -limit : x; }

static uint32_t sample_bytes(uint8_t sample) {
  return sample == FT_AUDIO_SAMPLE_U8 ? 1 : sample == FT_AUDIO_SAMPLE_S16 ? 2 : 4;
}

/* One little-endian sample as float. */
static float sample_at(const uint8_t *p, uint8_t sample) {
  if (sample == FT_AUDIO_SAMPLE_U8) return (p[0] - 128) / 128.0f;
  if (sample == FT_AUDIO_SAMPLE_S16) return (int16_t)(uint16_t)(p[0] | p[1] << 8) / 32768.0f;
  uint32_t v = (uint32_t)p[0] | (uint32_t)p[1] << 8 | (uint32_t)p[2] << 16 | (uint32_t)p[3] << 24;
  if (sample == FT_AUDIO_SAMPLE_S32) return (float)((int32_t)v / 2147483648.0);
  float f;
  memcpy(&f, &v, sizeof f);
  return f;
}

static int usable(const ft_audio_block *b) {
  const ft_audio_format *f = &b->format;
  return f->rate && f->channels && f->sample >= FT_AUDIO_SAMPLE_U8 && f->sample <= FT_AUDIO_SAMPLE_F32 &&
         (uint64_t)b->frames * f->channels * sample_bytes(f->sample) <= b->samples_len;
}

/* Crossfade from the last output frame over the next VIEWER_AUDIO_FADE frames. */
static void conceal(viewer_audio *a) {
  memcpy(a->held, a->last, sizeof a->held);
  a->conceal = VIEWER_AUDIO_FADE;
}

/* Moves b to a and converts the next source frame into b: 0 in underrun. */
static int advance(viewer_audio *a) {
  while (a->index >= a->block.frames) {
    a->index = 0;
    if (!a->read(a->context, &a->block)) {
      a->block.frames = 0;
      return 0;
    }
    if (!usable(&a->block)) a->block.frames = 0;
    else if (a->block.flags) {
      a->gaps += !!(a->block.flags & FT_AUDIO_BLOCK_GAP);
      conceal(a);
    }
  }
  const ft_audio_format *f = &a->block.format;
  uint32_t size = sample_bytes(f->sample);
  const uint8_t *frame = a->block.samples + (size_t)a->index * f->channels * size;
  memcpy(a->a, a->b, sizeof a->a);
  a->a_time = a->b_time;
  /* Mono feeds every output channel; other layouts map channel by channel. */
  for (uint32_t c = 0; c < a->channels; c++)
    a->b[c] = f->channels == 1 ? sample_at(frame, f->sample) : c < f->channels ? sample_at(frame + c * size, f->sample) : 0;
  a->period_ns = 1e9 / f->rate;
  a->b_time = (double)a->block.host_time_ns + a->index * a->period_ns;
  a->index++;
  return 1;
}

void viewer_audio_init(viewer_audio *a, uint32_t rate, uint32_t channels, uint64_t latency_ns) {
  memset(a, 0, sizeof *a);
  a->rate = rate;
  a->channels = channels;
  a->latency_ns = latency_ns;
  atomic_store(&a->clock_offset_ns, INT64_MIN);
}

void viewer_audio_render(viewer_audio *a, float *out, uint32_t frames, uint64_t now_ns) {
  uint32_t wait = 0;
  int fade = 0;
  double step = 0;
  if (!a->primed && advance(a)) {
    memcpy(a->a, a->b, sizeof a->a);
    a->a_time = a->b_time;
    a->phase = 0;
    a->primed = 1;
  }
  if (a->primed) {
    /* Due: the source time to hear in this buffer's first frame. */
    double due = (double)now_ns + frames * 1e9 / a->rate - (double)a->latency_ns;
    double error = due - (a->a_time + a->phase * a->period_ns);
    if (!a->aligned || fabs(error) > RESYNC_NS) {
      /* Behind: drop source frames. Ahead: play silence until due. */
      while (a->primed && error > a->period_ns / 2) {
        if (advance(a)) error = due - (a->a_time + a->phase * a->period_ns);
        else a->primed = 0;
      }
      if (error < 0) wait = (uint32_t)fmin(frames, ceil(-error * a->rate / 1e9));
      a->resyncs += a->aligned; /* realignments after lock, not starts */
      a->aligned = a->primed && wait < frames;
      fade = 1;
      error = 0;
    } else {
      double dt = (double)frames / a->rate;
      a->drift = clamp(a->drift + KI * error / 1e9 * dt, MAX_CORRECTION);
      a->correction = clamp(KP * error / 1e9 + a->drift, MAX_CORRECTION);
    }
    a->error_ns = error;
    step = a->period_ns > 0 ? 1e9 / a->period_ns / a->rate * (1 + a->correction) : 0;
    atomic_store(&a->clock_offset_ns, -(int64_t)a->latency_ns - (int64_t)error);
  }
  for (uint32_t k = 0; k < frames; k++) {
    if (fade && (k == 0 || k == wait)) conceal(a);
    int playing = a->primed && k >= wait;
    float *o = out + (size_t)k * a->channels, g = (float)a->conceal / VIEWER_AUDIO_FADE;
    if (a->conceal) a->conceal--;
    for (uint32_t c = 0; c < a->channels; c++) {
      float x = playing ? a->a[c] + (a->b[c] - a->a[c]) * (float)a->phase : 0;
      a->last[c] = x + (a->held[c] - x) * g;
      o[c] = a->muted ? 0 : a->last[c];
    }
    if (!playing) continue;
    for (a->phase += step; a->phase >= 1; a->phase -= 1) {
      if (!advance(a)) {
        a->primed = a->aligned = 0;
        a->underruns++;
        conceal(a);
        break;
      }
    }
  }
}

static int read_ring(void *reader, ft_audio_block *out) { return ft_playback_reader_read(reader, out) == FT_STATUS_OK; }

static void callback(void *user, Uint8 *stream, int len) {
  viewer_audio *a = user;
  viewer_audio_render(a, (float *)stream, (uint32_t)len / (uint32_t)(sizeof(float) * a->channels), host_time_ns());
}

int viewer_playback_offered(viewer_playback *p, const ft_track_offer *offer) {
  if (strcmp(offer->kind, FT_TRACK_KIND_PLAYBACK)) return 0;
  const ft_audio_format *f = &offer->audio.format;
  fprintf(stderr, "audio track id=%" PRIu64 " label=%s rate=%u channels=%u\n", offer->id, offer->label, f->rate,
          f->channels);
  return p->label && !p->ring && !strcmp(offer->label, p->label);
}

int viewer_playback_start(viewer_playback *p) {
  const ft_audio_format *f = &p->descriptor.format;
  /* 10 ms periods. */
  SDL_AudioSpec want = {.freq = (int)f->rate, .format = AUDIO_F32SYS,
                        .channels = (Uint8)(f->channels < VIEWER_AUDIO_CHANNELS ? f->channels : VIEWER_AUDIO_CHANNELS),
                        .samples = (Uint16)(f->rate / 100), .callback = callback, .userdata = &p->audio};
  SDL_AudioSpec have;
  if ((!SDL_WasInit(SDL_INIT_AUDIO) && SDL_InitSubSystem(SDL_INIT_AUDIO) != 0) ||
      !(p->device = SDL_OpenAudioDevice(NULL, 0, &want, &have, SDL_AUDIO_ALLOW_FREQUENCY_CHANGE | SDL_AUDIO_ALLOW_CHANNELS_CHANGE))) {
    fprintf(stderr, "audio device: %s\n", SDL_GetError());
    return 1;
  }
  if (have.channels > VIEWER_AUDIO_CHANNELS ||
      ft_playback_reader_create(p->ring, p->latency_ns, &p->reader) != FT_STATUS_OK) {
    fprintf(stderr, "audio playout setup failed\n");
    return 1;
  }
  viewer_audio_init(&p->audio, (uint32_t)have.freq, have.channels, p->latency_ns);
  p->audio.read = read_ring;
  p->audio.context = p->reader;
  p->audio.muted = p->muted;
  SDL_PauseAudioDevice(p->device, 0);
  return 0;
}

static void viewer_playback_stop(viewer_playback *p) {
  if (p->device) {
    SDL_CloseAudioDevice(p->device);
    p->device = 0;
    const viewer_audio *a = &p->audio;
    printf("audio track=%" PRIu64 " underruns=%" PRIu64 " gaps=%" PRIu64 " resyncs=%" PRIu64
           " drift_ppm=%.0f error_ms=%.2f\n", p->track, a->underruns, a->gaps, a->resyncs, a->drift * 1e6,
           a->error_ns / 1e6);
  }
  ft_playback_reader_destroy(&p->reader);
  ft_playback_ring_destroy(&p->ring);
}

int viewer_playback_poll(viewer_playback *p) {
  ft_track_notice notice;
  ft_status status = FT_STATUS_EMPTY;
  while (p->tracks && (status = ft_tracks_next(p->tracks, 0, &notice)) == FT_STATUS_OK) {
    if (notice.type == FT_TRACK_NOTICE_REMOVED && p->ring && notice.track.id == p->track) {
      fprintf(stderr, "audio track id=%" PRIu64 " removed\n", p->track);
      viewer_playback_stop(p);
    }
    if (notice.type != FT_TRACK_NOTICE_ADDED || !viewer_playback_offered(p, &notice.track)) continue;
    /* A late selection, answered by the serving host. */
    ft_track_selection selection = {notice.track.id, 0, 0};
    ft_track_admission admission;
    ft_playback_doorbell *doorbell = NULL;
    uint32_t position;
    if (ft_tracks_select(p->tracks, &selection, &admission, &position, &p->ring, &doorbell) != FT_STATUS_OK ||
        admission.result != FT_TRACK_ADMITTED || !p->ring) {
      fprintf(stderr, "late audio selection failed\n");
      return 1;
    }
    /* The viewer reads its ring when the device asks; it needs no doorbell. */
    ft_playback_doorbell_destroy(&doorbell);
    p->track = notice.track.id;
    p->descriptor = notice.track.audio;
    if (viewer_playback_start(p)) return 1;
  }
  /* The channel ends with the source. */
  if (p->tracks && status != FT_STATUS_EMPTY) ft_tracks_destroy(&p->tracks);
  return 0;
}

int64_t viewer_playback_clock(viewer_playback *p) {
  return p->device ? atomic_load(&p->audio.clock_offset_ns) : INT64_MIN;
}

void viewer_playback_close(viewer_playback *p) {
  viewer_playback_stop(p);
  ft_tracks_destroy(&p->tracks);
}
