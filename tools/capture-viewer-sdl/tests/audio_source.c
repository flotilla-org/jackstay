/* A synthetic publication for the viewer's audio: numbered video frames plus
 * a 440 Hz stereo tone labelled "tone", in 10 ms blocks stamped with host
 * time. --late offers the tone only after the viewer attaches; --skew-ppm runs
 * the tone's clock fast or slow against host time. It serves until the
 * viewer leaves. */
#define _POSIX_C_SOURCE 200809L
#include "host_time.h"
#include "jackstay_handshake.h"
#include "synthetic.h"
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
static void checked(ft_status s, const char *operation) {
  if (s != FT_STATUS_OK) { fprintf(stderr, "audio source %s: status=%d\n", operation, s); exit(1); }
}
#define check(status) checked((status), #status)
enum { RATE = 48000, BLOCK = 480 };
int main(int argc, char **argv) {
  int late = 0;
  double skew = 0;
  for (int i = 2; i < argc; i++) {
    if (!strcmp(argv[i], "--late")) late = 1;
    else if (!strcmp(argv[i], "--skew-ppm") && i + 1 < argc) skew = atof(argv[++i]) / 1e6;
    else return 1;
  }
  if (argc < 2) { fprintf(stderr, "usage: audio-source-fixture ENDPOINT [--late] [--skew-ppm PPM]\n"); return 1; }
  ft_local_endpoint endpoint = {FT_ENDPOINT_SCOPE_USER, FT_ENDPOINT_TRANSPORT_LOCAL_STREAM, argv[1]};
  ft_local_listener *listener = NULL; ft_local_connection *local = NULL;
  ft_cpu_producer *producer = NULL; ft_cpu_setup_server *server = NULL;
  ft_input_server *input = NULL; ft_affordances_producer *affordances = NULL; ft_track_notices *notices = NULL;
  ft_playback_writer *writer = NULL; ft_playback *playback = NULL;
  /* Retains 133 ms of video, beyond the viewer's default latency target. */
  ft_cpu_producer_config config = {12, 8, 1, 2, STRIDE * HEIGHT, 8 * 1024 * 1024, 5000000000ULL};
  const ft_audio_format format = {RATE, 2, FT_AUDIO_LAYOUT_STEREO, FT_AUDIO_SAMPLE_S16};
  check(ft_cpu_producer_create(&config, &producer));
  check(ft_playback_writer_create(2, &format, BLOCK, 250000000ULL, &writer, &playback));
  check(ft_local_listener_create(&endpoint, &listener));
  puts("ready"); fflush(stdout);
  check(ft_local_listener_accept(listener, &local));
  ft_track_offer tracks[2] = {{1, FT_TRACK_KIND_VIDEO, "default", NULL, 0, FT_TRACK_FROM_SOURCE, 0, {{0}, 0, 0}},
                              {2, FT_TRACK_KIND_PLAYBACK, "tone", NULL, 0, FT_TRACK_FROM_SOURCE, 0, {{0}, 0, 0}}};
  check(ft_playback_writer_descriptor(writer, &tracks[1].audio));
  ft_track_admission admissions[2]; size_t count = 0;
  check(ft_handshake_accept_local(&local, "audio fixture", FT_CLOCK_DOMAIN_HOST_TIME, tracks, late ? 1 : 2,
                                  &playback, late ? 0 : 1, NULL, admissions, &count, &input, &affordances, &notices));
  ft_local_listener_destroy(&listener);
  check(ft_cpu_producer_serve_local(producer, &local, &server));
  if (late) check(ft_track_notices_add(notices, &tracks[1], playback));
  static uint8_t pixels[STRIDE * HEIGHT];
  int16_t samples[BLOCK * 2];
  uint64_t written = 0, frames = 0, start = host_time_ns(), next_frame = start;
  const double rate = RATE * (1 + skew);
  while (ft_cpu_setup_server_poll(server) == FT_STATUS_DRAINING) {
    if (notices) {
      ft_status served = ft_track_notices_serve(notices, 0);
      if (served != FT_STATUS_OK && served != FT_STATUS_EMPTY) ft_track_notices_destroy(&notices);
    }
    uint64_t now = host_time_ns();
    /* Each block is written once its last frame has played at the skewed rate. */
    while (start + (written + BLOCK) * 1e9 / rate <= now) {
      for (int i = 0; i < BLOCK; i++)
        samples[i * 2] = samples[i * 2 + 1] = (int16_t)(8000 * sin(2 * 3.14159265358979 * 440 * (double)(written + i) / RATE));
      check(ft_playback_writer_write(writer, samples, sizeof samples, start + (uint64_t)(written * 1e9 / rate), NULL));
      written += BLOCK;
    }
    if (now >= next_frame) {
      fill_frame(pixels, ++frames);
      ft_acquired_frame_descriptor desc = {.sequence = frames, .width = WIDTH, .height = HEIGHT, .stride = STRIDE,
                                           .pixel_format = FT_PIXEL_FORMAT_BGRA8_UNORM};
      uint64_t cursor;
      ft_status s = ft_cpu_producer_publish(producer, &desc, pixels, sizeof pixels, &cursor);
      if (s != FT_STATUS_OK && s != FT_STATUS_DROPPED) check(s);
      next_frame += 16666667;
    }
    struct timespec pause = {0, 2000000};
    nanosleep(&pause, NULL);
  }
  ft_track_notices_destroy(&notices);
  ft_status setup = ft_cpu_setup_server_destroy(&server);
  if (setup != FT_STATUS_OK && setup != FT_STATUS_CANCELLED) check(setup);
  check(ft_cpu_producer_destroy(&producer));
  ft_playback_writer_destroy(&writer);
  ft_playback_destroy(&playback);
  printf("audio_blocks=%llu\n", (unsigned long long)(written / BLOCK));
  return 0;
}
