/* Interactive synthetic CPU source. Uses the same pixels as the reference
 * viewer and the public input/media interfaces; no Porthole daemon required.
 * It listens on a Local Endpoint (--endpoint, every platform) or, on POSIX, a
 * socket path. */
#if !defined(_WIN32)
#define _POSIX_C_SOURCE 200809L
#endif
#include "jackstay_bootstrap.h"
#include "synthetic.h"
#include <assert.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#if defined(_WIN32)
#include <windows.h>
static void pause_ms(unsigned ms) { Sleep(ms); }
#else
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>
static void pause_ms(unsigned ms) { struct timespec t = {(time_t)(ms / 1000), (long)(ms % 1000) * 1000000}; nanosleep(&t, NULL); }
static int listener(const char *path) {
  struct sockaddr_un address = {0}; address.sun_family = AF_UNIX;
  if (strlen(path) >= sizeof(address.sun_path)) return -1;
  memcpy(address.sun_path, path, strlen(path) + 1);
  int fd = socket(AF_UNIX, SOCK_STREAM, 0);
  if (fd < 0) return -1;
  /* Refuse existing paths; never unlink a socket owned by another process. */
  if (bind(fd, (struct sockaddr *)&address, sizeof(address)) || listen(fd, 1)) { close(fd); return -1; }
  return fd;
}
#endif
/* --resize-every-ms alternates between the base size and this larger one, with
 * a different aspect ratio so the change is visible in any viewer. */
enum { LARGE_WIDTH = WIDTH * 3 / 2, LARGE_HEIGHT = HEIGHT * 2 };
static void checked(ft_status status) { if (status != FT_STATUS_OK) { fprintf(stderr, "reference source status=%d\n", status); exit(1); } }
static const char *action_name(uint32_t action) {
  return action == FT_INPUT_DOWN ? "down" : action == FT_INPUT_UP ? "up" : action == FT_INPUT_REPEAT ? "repeat" : "?";
}
/* One line per executed operation, for evidence that input reached the source. */
static void log_input(const ft_input_operation *op) {
  const ft_input_event *e = &op->event;
  switch (e->kind) {
    case FT_INPUT_KEY: printf("input key %s %s\n", e->key, action_name(e->action)); break;
    case FT_INPUT_TEXT: printf("input text \"%.*s\"\n", (int)e->text_len, (const char *)e->text); break;
    case FT_INPUT_BUTTON: printf("input button %u %s at %.1f,%.1f\n", e->button, action_name(e->action), e->x, e->y); break;
    case FT_INPUT_MOTION: printf("input motion %.1f,%.1f\n", e->x, e->y); break;
    case FT_INPUT_SCROLL: printf("input scroll %.2f,%.2f at %.1f,%.1f\n", e->x, e->y, e->pointer_x, e->pointer_y); break;
    case FT_INPUT_CLEANUP: printf("input cleanup scope=%u reason=%u\n", op->scope, op->reason); break;
    default: printf("input kind=%u\n", e->kind);
  }
  fflush(stdout);
}
static void usage(void) {
  fprintf(stderr, "usage: capture-input-source (SOURCE_SOCKET | --endpoint NAME [--session-scope])\n"
                  "       [--report-state] [--observe-only] [--log-input] [--resize-every-ms MS] [--repeat]\n"
                  "SOURCE_SOCKET is POSIX only; --endpoint names a Local Endpoint on every platform.\n");
}
int main(int argc, char **argv) {
  if (ft_abi_version() != FT_ABI_VERSION) { fprintf(stderr, "Jackstay ABI mismatch\n"); return 1; }
  int report_state = 0, observe_only = 0, log_events = 0, session_scope = 0, repeat = 0;
  unsigned resize_ms = 0;
  const char *path = NULL, *endpoint_name = NULL;
  for (int i = 1; i < argc; i++) {
    if (!strcmp(argv[i], "--report-state")) report_state = 1;
    else if (!strcmp(argv[i], "--observe-only")) observe_only = 1;
    else if (!strcmp(argv[i], "--log-input")) log_events = 1;
    else if (!strcmp(argv[i], "--session-scope")) session_scope = 1;
    else if (!strcmp(argv[i], "--repeat")) repeat = 1;
    else if (!strcmp(argv[i], "--endpoint") && i + 1 < argc) endpoint_name = argv[++i];
    else if (!strcmp(argv[i], "--resize-every-ms") && i + 1 < argc) resize_ms = (unsigned)strtoul(argv[++i], NULL, 10);
    else if (argv[i][0] != '-' && !path) path = argv[i];
    else { fprintf(stderr, "unknown option: %s\n", argv[i]); usage(); return 1; }
  }
  if (!path == !endpoint_name) { usage(); return 1; }
#if defined(_WIN32)
  if (path) { fprintf(stderr, "socket paths are POSIX only; use --endpoint NAME\n"); return 1; }
#else
  int media_listener = -1;
  if (path) {
    umask(0077);
    media_listener = listener(path); if (media_listener < 0) { perror("source listener"); return 1; }
  }
#endif
  ft_local_listener *local_listener = NULL;
  if (endpoint_name) {
    ft_local_endpoint endpoint = {session_scope ? FT_ENDPOINT_SCOPE_SESSION : FT_ENDPOINT_SCOPE_USER,
                                  FT_ENDPOINT_TRANSPORT_LOCAL_STREAM, endpoint_name};
    ft_status bound = ft_local_listener_create(&endpoint, &local_listener);
    if (bound != FT_STATUS_OK) { fprintf(stderr, "source endpoint %s: status=%d\n", endpoint_name, bound); return 1; }
  }
  ft_cpu_producer *producer = NULL;
  ft_cpu_producer_config media_config = {6, 2, 1, 2, STRIDE * HEIGHT, 8 * 1024 * 1024, 5000000000ULL};
  checked(ft_cpu_producer_create(&media_config, &producer));
  ft_input_target *target = NULL;
  ft_input_config config; ft_input_config_default(&config); config.independent_contributions = 1; config.interaction_cancel = 1;
  config.geometry.width = WIDTH; config.geometry.height = HEIGHT; checked(ft_input_target_create(&config, &target));
  printf("ready\n"); fflush(stdout);
  uint8_t *pixels = malloc((size_t)LARGE_WIDTH * 4 * LARGE_HEIGHT); if (!pixels) return 1;
  uint32_t width = WIDTH, height = HEIGHT; uint64_t geometry_revision = config.geometry.revision, capacity = STRIDE * HEIGHT;
  unsigned since_resize = 0, held_count = 0, buttons = 0, downs = 0, repeats = 0, releases = 0, cleanup = 0;
  size_t text_bytes = 0;
  uint64_t sequence = 1;
  /* One viewer per session. Without --repeat the source exits after the first;
   * with it, it serves viewers one after another until it is killed. */
  for (unsigned session = 1;; session++) {
    ft_cpu_setup_server *media_server = NULL; ft_input_server *input_server = NULL;
    /* This example authorizes its private same-user endpoint for the selected
     * synthetic source. Passing NULL intentionally withholds input authority. */
    if (local_listener) {
      ft_local_connection *connection = NULL;
      checked(ft_local_listener_accept(local_listener, &connection));
      checked(ft_source_bootstrap_accept_local(&connection, observe_only ? NULL : target, &input_server));
      checked(ft_cpu_producer_serve_local(producer, &connection, &media_server));
      /* Release the name so a restarted source can bind it. */
      if (!repeat) ft_local_listener_destroy(&local_listener);
    }
#if !defined(_WIN32)
    else {
      int32_t media_fd = accept(media_listener, NULL, NULL); if (media_fd < 0) return 1;
      checked(ft_source_bootstrap_accept(&media_fd, observe_only ? NULL : target, &input_server));
      checked(ft_cpu_producer_serve(producer, &media_fd, &media_server));
      if (!repeat) { close(media_listener); unlink(path); }
    }
#endif
    if (log_events) { printf("session %u: viewer admitted, input %s\n", session, input_server ? "offered" : "none"); fflush(stdout); }
    uint64_t held[256] = {0}; held_count = buttons = downs = repeats = releases = cleanup = 0;
    text_bytes = 0; double pointer_x = 0, pointer_y = 0; int finished = 0;
    for (; !finished; sequence++) {
      ft_input_work *work = NULL;
      while (ft_input_target_next(target, &work) == FT_STATUS_OK) {
        ft_input_operation op; checked(ft_input_work_describe(work, &op)); ft_input_event *e = &op.event;
        uint32_t outcome = FT_INPUT_EXECUTED;
        if (log_events) log_input(&op);
        switch (e->kind) {
          case FT_INPUT_KEY: {
            /* This reference supports physical controls and a small logical subset.
             * No keyboard-layout reconstruction is claimed. */
            if (e->key_kind == FT_INPUT_LOGICAL_KEY && strcmp(e->key, "Enter") && strcmp(e->key, "ArrowLeft") && strcmp(e->key, "ArrowRight")) {
              outcome = FT_INPUT_UNSUPPORTED; break;
            }
            if (e->action == FT_INPUT_DOWN) { if (held_count == 256) outcome = FT_INPUT_REJECTED; else { held[held_count++] = e->press; downs++; } }
            else if (e->action == FT_INPUT_REPEAT) repeats++;
            else { for (unsigned i = 0; i < held_count; i++) if (held[i] == e->press) { held[i] = held[--held_count]; releases++; break; } }
            break;
          }
          case FT_INPUT_TEXT: text_bytes += e->text_len; break;
          case FT_INPUT_BUTTON:
            if (e->action == FT_INPUT_DOWN) buttons |= 1u << (e->button - 1); else buttons &= ~(1u << (e->button - 1));
            pointer_x = e->x; pointer_y = e->y; break;
          case FT_INPUT_MOTION: pointer_x = e->x; pointer_y = e->y; break;
          case FT_INPUT_SCROLL: pointer_x += e->x; pointer_y += e->y; break;
          case FT_INPUT_CLEANUP:
            if (op.scope == FT_INPUT_SCOPE_ALL) held_count = 0;
            buttons = 0; cleanup++;
            if (op.reason != FT_INPUT_REASON_FOCUS && op.reason != FT_INPUT_REASON_GEOMETRY) finished = 1;
            break;
          default: outcome = FT_INPUT_UNSUPPORTED;
        }
        checked(ft_input_work_complete(&work, outcome));
        if (report_state) {
          printf("state downs=%u repeats=%u releases=%u text_bytes=%zu held=%u buttons=%u\n", downs, repeats, releases, text_bytes, held_count, buttons);
          fflush(stdout);
        }
      }
      if (!input_server && ft_cpu_setup_server_poll(media_server) != FT_STATUS_DRAINING) finished = 1;
      if (resize_ms && (since_resize += 16) >= resize_ms) {
        /* Growing past the allocation replaces it while consumers may hold old
         * frames; shrinking reuses it. Pointer work from the old size is cancelled. */
        since_resize = 0;
        width = width == WIDTH ? LARGE_WIDTH : WIDTH; height = height == HEIGHT ? LARGE_HEIGHT : HEIGHT;
        if ((uint64_t)width * 4 * height > capacity) {
          ft_cpu_reconfiguration replacement = {0};
          capacity = (uint64_t)width * 4 * height;
          checked(ft_cpu_producer_reconfigure(producer, capacity, &replacement));
        }
        ft_input_geometry geometry = {++geometry_revision, width, height};
        checked(ft_input_target_geometry(target, &geometry));
        pointer_x = pointer_x < width ? pointer_x : 0; pointer_y = pointer_y < height ? pointer_y : 0;
        if (log_events) { printf("resize %ux%u\n", width, height); fflush(stdout); }
      }
      uint32_t stride = width * 4;
      fill_frame_sized(pixels, sequence, width, height, stride);
      /* Held keys tint the top strip; committed text fills a bottom progress bar;
       * the pointer is a white square, red while a button is held. */
      for (uint32_t y = 0; y < height; y++) for (uint32_t x = 0; x < width; x++) {
        uint8_t *p = pixels + (size_t)y * stride + (size_t)x * 4;
        if (y < 15 && held_count) { p[0] = 0; p[1] = 255; p[2] = 0; }
        if (y > height - 15 && x < text_bytes % width) { p[0] = 255; p[1] = 255; p[2] = 255; }
        if ((double)x >= pointer_x && (double)x < pointer_x + 8 && (double)y >= pointer_y && (double)y < pointer_y + 8) {
          p[0] = buttons ? 0 : 255; p[1] = buttons ? 0 : 255; p[2] = 255;
        }
      }
      ft_acquired_frame_descriptor desc = {.sequence = sequence, .timestamp_ns = sequence * 16000000,
        .width = width, .height = height, .stride = stride, .pixel_format = FT_PIXEL_FORMAT_BGRA8_UNORM};
      uint64_t cursor; ft_status s = ft_cpu_producer_publish(producer, &desc, pixels, (size_t)stride * height, &cursor);
      if (s != FT_STATUS_OK && s != FT_STATUS_DROPPED) checked(s);
      pause_ms(16);
    }
    /* Wait for the worker to flush the actual completion response and finish. */
    unsigned waits = 0;
    while (input_server && ft_input_server_poll(input_server) == FT_STATUS_EMPTY && waits++ < 3000) pause_ms(1);
    if (input_server && ft_input_server_poll(input_server) != FT_STATUS_OK) { fprintf(stderr, "input reply drain timeout\n"); return 1; }
    ft_input_server_destroy(&input_server);
    ft_status setup = ft_cpu_setup_server_destroy(&media_server);
    if (setup != FT_STATUS_OK && setup != FT_STATUS_CANCELLED) checked(setup);
    if (!repeat) break;
    if (log_events) { printf("session %u ended; waiting for the next viewer\n", session); fflush(stdout); }
  }
  checked(ft_input_target_destroy(&target));
  ft_status s = FT_STATUS_DRAINING;
  for (int n = 0; n < 500 && s == FT_STATUS_DRAINING; n++) { s = ft_cpu_producer_destroy(&producer); if (s == FT_STATUS_DRAINING) pause_ms(10); }
  checked(s); free(pixels);
  printf("input_source downs=%u repeats=%u releases=%u text_bytes=%zu cleanup=%u held=%u buttons=%u\n",
    downs, repeats, releases, text_bytes, cleanup, held_count, buttons);
  return 0;
}
