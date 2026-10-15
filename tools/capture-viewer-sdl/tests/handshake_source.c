/* Real process boundary fixture using only public C ABI channels. */
#define _POSIX_C_SOURCE 200809L
#include "jackstay_handshake.h"
#include "synthetic.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
static void checked(ft_status s, const char *operation) {
  if (s != FT_STATUS_OK) { fprintf(stderr, "handshake fixture %s: status=%d\n", operation, s); exit(1); }
}
#define check(status) checked((status), #status)
static void pause_ms(void) { struct timespec t = {0, 16000000}; nanosleep(&t, NULL); }
int main(int argc, char **argv) {
  if (argc != 3 && argc != 4) return 1;
  ft_local_endpoint endpoint = {(argc == 4 && !strcmp(argv[3], "session")) ? FT_ENDPOINT_SCOPE_SESSION : FT_ENDPOINT_SCOPE_USER, FT_ENDPOINT_TRANSPORT_LOCAL_STREAM, argv[1]};
  ft_local_listener *listener = NULL; ft_local_connection *local = NULL;
  ft_cpu_producer *producer = NULL; ft_cpu_setup_server *server = NULL;
  ft_input_server *input = NULL; ft_affordances_producer *affordances = NULL;
  ft_cpu_producer_config config = {6, 2, 1, 2, STRIDE * HEIGHT, 8 * 1024 * 1024, 5000000000ULL};
  check(ft_cpu_producer_create(&config, &producer));
  check(ft_local_listener_create(&endpoint, &listener));
  puts("ready"); fflush(stdout);
  check(ft_local_listener_accept(listener, &local));
  /* Video 1, input 2 (refused: no target), and affordances 3 when offered. */
  ft_track_offer tracks[3] = {{1, FT_TRACK_KIND_VIDEO, "default", NULL, 0, FT_TRACK_FROM_SOURCE, 0, {{0}, 0, 0}},
                              {2, FT_TRACK_KIND_INPUT, "default", NULL, 0, FT_TRACK_TO_SOURCE, 0, {{0}, 0, 0}},
                              {3, FT_TRACK_KIND_AFFORDANCES, "default", NULL, 0, FT_TRACK_BOTH, 0, {{0}, 0, 0}}};
  ft_track_admission admissions[3]; size_t count = 0;
  ft_status handshake = ft_handshake_accept_local(&local, "C fixture", FT_CLOCK_DOMAIN_HOST_TIME, tracks,
                                                  strcmp(argv[2], "offered") ? 2 : 3, NULL, 0, NULL, admissions,
                                                  &count, &input, &affordances, NULL);
  ft_local_listener_destroy(&listener);
  /* A viewer requiring absent affordances abandons the offer, ending here. */
  if (handshake != FT_STATUS_OK || (argc == 4 && !strcmp(argv[3], "refusal"))) {
    ft_local_connection_destroy(&local); check(ft_cpu_producer_destroy(&producer));
    puts("handshake refused"); return 0;
  }
  check(ft_cpu_producer_serve_local(producer, &local, &server));
  if (affordances) {
    const char *title = "C handshake fixture";
    ft_aff_snapshot snapshot = {.domain = FT_AFF_DOMAIN_WINDOW,
      .window = {.ready = 1, .title = {.present = 1, .value = {(const uint8_t *)title, strlen(title)}}}};
    check(ft_affordances_producer_publish(affordances, &snapshot));
  }
  int offered = affordances != NULL;
  int presentation = 0, closed = !affordances;
  uint8_t pixels[STRIDE * HEIGHT];
  for (uint64_t sequence = 1; sequence < 600; sequence++) {
    ft_affordances_event *event = NULL;
    while (affordances && ft_affordances_producer_poll(affordances, &event) == FT_STATUS_OK) {
      ft_aff_event_view view; check(ft_affordances_event_view(event, &view));
      /* The host publishes visibility, focus, scale and logical preferred size. */
      if (view.kind == 1 && view.snapshot.domain == FT_AFF_DOMAIN_PRESENTATION) {
        const ft_aff_presentation *p = &view.snapshot.presentation;
        if (p->visible != 1 || p->scale != 1 || !p->preferred_size.present || p->preferred_size.width <= 0 || p->preferred_size.height <= 0 || p->focused > 1) return 1;
        presentation = 1;
      }
      if (view.kind == 3) closed = 1;
      ft_affordances_event_destroy(&event);
    }
    /* Media and affordances close independently. A media EOF must not race
     * the affordances worker's closure notification (observed on macOS). */
    if (ft_cpu_setup_server_poll(server) != FT_STATUS_DRAINING) {
      if (closed) break;
      pause_ms(); continue;
    }
    fill_frame(pixels, sequence);
    ft_acquired_frame_descriptor desc = {.sequence = sequence,
      .width = WIDTH, .height = HEIGHT, .stride = STRIDE, .pixel_format = FT_PIXEL_FORMAT_BGRA8_UNORM};
    uint64_t cursor; ft_status s = ft_cpu_producer_publish(producer, &desc, pixels, sizeof(pixels), &cursor);
    if (s != FT_STATUS_OK && s != FT_STATUS_DROPPED) check(s);
    pause_ms();
  }
  ft_affordances_producer_destroy(&affordances);
  ft_status setup = ft_cpu_setup_server_destroy(&server);
  if (setup != FT_STATUS_OK && setup != FT_STATUS_CANCELLED) check(setup);
  check(ft_cpu_producer_destroy(&producer));
  printf("presentation=%d affordances_closed=%d\n", presentation, closed);
  return (offered && (!presentation || !closed)) ? 1 : 0;
}
