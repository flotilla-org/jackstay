/* The viewer's playout in simulated time: conversion, concealment, underrun,
 * mute, and drift correction against skewed producer clocks. */
#include "viewer_audio.h"
#include <math.h>
#include <stdio.h>
#include <string.h>

#define CHECK(x) do { if (!(x)) { fprintf(stderr, "%s:%d: %s\n", __FILE__, __LINE__, #x); return 1; } } while (0)
#define RATE 48000u
#define PERIOD 512u
#define BLOCK 480u

/* A source of queued blocks. */
typedef struct queue {
  ft_audio_block blocks[4];
  int count, next;
} queue;

static int pop(void *context, ft_audio_block *out) {
  queue *q = context;
  if (q->next == q->count) return 0;
  *out = q->blocks[q->next++];
  return 1;
}

/* Renders one period with q's first block due exactly at its first frame. */
static void render_due(viewer_audio *a, queue *q, float *out, uint32_t frames) {
  uint64_t now = 1000000000u;
  for (int i = 0; i < q->count; i++)
    q->blocks[i].host_time_ns = now + (uint64_t)frames * 1000000000u / RATE + (uint64_t)i * BLOCK * 1000000000u / RATE;
  viewer_audio_render(a, out, frames, now);
}

static void start(viewer_audio *a, queue *q) {
  viewer_audio_init(a, RATE, 2, 0);
  a->read = pop;
  a->context = q;
}

static float max_step(const float *out, uint32_t from, uint32_t to) {
  float worst = 0;
  for (uint32_t k = from + 1; k < to; k++)
    for (int c = 0; c < 2; c++) worst = fmaxf(worst, fabsf(out[k * 2 + c] - out[(k - 1) * 2 + c]));
  return worst;
}

/* Every sample format converts to float, mono feeding both channels. */
static int converts_formats(void) {
  static uint8_t u8[BLOCK], s16[BLOCK * 2], s32[BLOCK * 4], f32[BLOCK * 4];
  float half = 0.5f;
  for (uint32_t i = 0; i < BLOCK; i++) {
    u8[i] = 192;
    s16[i * 2 + 1] = 0x40;
    s32[i * 4 + 3] = 0x40;
    memcpy(f32 + i * 4, &half, 4);
  }
  const uint8_t *data[] = {u8, s16, s32, f32};
  for (uint8_t sample = FT_AUDIO_SAMPLE_U8; sample <= FT_AUDIO_SAMPLE_F32; sample++) {
    queue q = {.count = 1};
    q.blocks[0] = (ft_audio_block){.samples = data[sample - 1], .samples_len = BLOCK * (sample == 1 ? 1 : sample == 2 ? 2 : 4),
                                   .format = {RATE, 1, FT_AUDIO_LAYOUT_MONO, sample}, .frames = BLOCK};
    viewer_audio a;
    start(&a, &q);
    float out[PERIOD / 2 * 2];
    render_due(&a, &q, out, PERIOD / 2);
    /* Past the fade-in, the output is the source's 0.5 on both channels. */
    CHECK(out[200 * 2] == 0.5f && out[200 * 2 + 1] == 0.5f && out[0] == 0);
    CHECK(max_step(out, 0, PERIOD / 2) <= 0.5f / VIEWER_AUDIO_FADE + 1e-6f);
  }
  return 0;
}

static int16_t level[2][BLOCK * 2];

static ft_audio_block constant(int which, uint32_t flags) {
  return (ft_audio_block){.samples = (const uint8_t *)level[which], .samples_len = sizeof level[which],
                          .format = {RATE, 2, FT_AUDIO_LAYOUT_STEREO, FT_AUDIO_SAMPLE_S16}, .frames = BLOCK,
                          .flags = flags};
}

/* A gap crossfades rather than jumping, and an underrun fades to silence. */
static int conceals_gaps_and_underruns(void) {
  for (uint32_t i = 0; i < BLOCK * 2; i++) level[0][i] = 16384, level[1][i] = -16384;
  queue q = {.count = 2};
  q.blocks[0] = constant(0, 0);
  q.blocks[1] = constant(1, FT_AUDIO_BLOCK_GAP);
  viewer_audio a;
  start(&a, &q);
  static float out[PERIOD * 4 * 2];
  render_due(&a, &q, out, PERIOD * 4);
  CHECK(out[300 * 2] == 0.5f && out[700 * 2] == -0.5f && a.gaps == 1);
  /* 0.5 to -0.5 over the fade, not in one step. */
  CHECK(max_step(out, 0, PERIOD * 4) <= 1.0f / VIEWER_AUDIO_FADE + 1e-3f);
  /* Then silence once both blocks are spent. */
  CHECK(a.underruns == 1 && out[(PERIOD * 4 - 1) * 2] == 0 && !a.primed);
  return 0;
}

static int mute_keeps_the_clock(void) {
  queue q = {.count = 1};
  q.blocks[0] = constant(0, 0);
  viewer_audio a;
  start(&a, &q);
  a.muted = 1;
  a.latency_ns = 40000000;
  float out[PERIOD * 2];
  render_due(&a, &q, out, PERIOD);
  for (uint32_t i = 0; i < PERIOD * 2; i++) CHECK(out[i] == 0);
  /* Ahead of due by the latency target: wait in silence, publishing the clock. */
  CHECK(atomic_load(&a.clock_offset_ns) == -40000000 && a.primed && !a.aligned);
  return 0;
}

/* A producer whose clock runs at RATE * (1 + skew) frames per host second:
 * a 440 Hz tone in 10 ms blocks, readable once written. */
typedef struct producer {
  double rate;
  uint64_t next, now_ns;
  int16_t samples[BLOCK * 2];
} producer;

static int produce(void *context, ft_audio_block *out) {
  producer *p = context;
  double start = p->next * BLOCK * 1e9 / p->rate;
  if (start + BLOCK * 1e9 / p->rate > (double)p->now_ns) return 0;
  for (uint32_t i = 0; i < BLOCK; i++)
    p->samples[i * 2] = p->samples[i * 2 + 1] =
        (int16_t)(16384 * sin(2 * M_PI * 440 * (double)(p->next * BLOCK + i) / RATE));
  *out = (ft_audio_block){.cursor = p->next + 1, .sample_position = p->next * BLOCK, .host_time_ns = (uint64_t)start,
                          .samples = (const uint8_t *)p->samples, .samples_len = sizeof p->samples,
                          .format = {RATE, 2, FT_AUDIO_LAYOUT_STEREO, FT_AUDIO_SAMPLE_S16}, .frames = BLOCK};
  p->next++;
  return 1;
}

/* Ten simulated minutes: after settling, the playout error stays under a
 * millisecond, with no underrun or realignment, and the tone stays smooth. */
static int drift_correction_holds_sync(double skew) {
  const uint64_t latency = 60000000, start = 10000000000u;
  producer p = {.rate = RATE * (1 + skew)};
  /* A reader starts about its latency target behind the write cursor. */
  p.next = (uint64_t)((start - latency) / 1e9 * p.rate / BLOCK);
  viewer_audio a;
  viewer_audio_init(&a, RATE, 2, latency);
  a.read = produce;
  a.context = &p;
  static float out[PERIOD * 2];
  float last = 0, worst_step = 0;
  double worst_error = 0;
  for (uint64_t n = 0; n < 600ull * RATE / PERIOD; n++) {
    p.now_ns = start + n * PERIOD * 1000000000u / RATE;
    viewer_audio_render(&a, out, PERIOD, p.now_ns);
    for (uint32_t k = 0; k < PERIOD; k++) {
      worst_step = fmaxf(worst_step, fabsf(out[k * 2] - last));
      last = out[k * 2];
    }
    if (n * PERIOD > 30u * RATE) worst_error = fmax(worst_error, fabs(a.error_ns));
  }
  fprintf(stderr, "skew=%+.4f drift=%+.6f worst_error_ms=%.3f worst_step=%.4f\n", skew, a.drift, worst_error / 1e6,
          worst_step);
  CHECK(a.underruns == 0 && a.resyncs == 0 && a.gaps == 0);
  CHECK(worst_error < 1e6 && fabs(a.drift - skew) < 1e-4);
  /* The tone's largest step is 2 pi 440 / 48000 / 2: no clicks. */
  CHECK(worst_step < 0.04f);
  return 0;
}

int main(void) {
  return converts_formats() || conceals_gaps_and_underruns() || mute_keeps_the_clock() ||
         drift_correction_holds_sync(0) || drift_correction_holds_sync(0.005) || drift_correction_holds_sync(-0.005);
}
