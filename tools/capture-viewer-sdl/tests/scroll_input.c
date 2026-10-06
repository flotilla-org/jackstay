/* Real input target/client over the socket boundary; no desktop capture. */
#include "viewer_input.h"
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <unistd.h>
static void check_line(int value, int line) { if (!value) { fprintf(stderr, "scroll contract failed at line %d\n", line); exit(1); } }
#define check(value) check_line((value), __LINE__)
static void expect_metadata(ft_input_target *target, uint32_t unit, double x, double y, uint32_t phase, uint32_t momentum, uint32_t inversion) {
  ft_input_work *work = NULL;
  uint32_t start = SDL_GetTicks();
  while (ft_input_target_next(target, &work) == FT_STATUS_EMPTY && SDL_GetTicks() - start < 2000) SDL_Delay(1);
  if (!work) fprintf(stderr, "expected scroll unit=%u delta=%g,%g phase=%u momentum=%u inversion=%u\n", unit, x, y, phase, momentum, inversion);
  check(work != NULL);
  ft_input_operation op;
  check(ft_input_work_describe(work, &op) == FT_STATUS_OK);
  /* The scroll unit and signed deltas are observable at the real executor. */
  check(op.event.kind == FT_INPUT_SCROLL && op.event.scroll_unit == unit && op.event.x == x && op.event.y == y);
  check(op.event.scroll_phase == phase && op.event.scroll_momentum_phase == momentum && op.event.scroll_inverted_from_device == inversion);
  check(ft_input_work_complete(&work, FT_INPUT_EXECUTED) == FT_STATUS_OK);
}
static void expect_scroll(ft_input_target *target, uint32_t unit, double x, double y) {
  expect_metadata(target, unit, x, y, 0, 0, 0);
}
static void native_sample(viewer_input *input, SDL_Window *window, unsigned long phase, unsigned long momentum,
                          int inversion, double x, double y) {
  check(viewer_input_capture_scroll(input, window, x, y, FT_INPUT_SCROLL_PIXEL, phase, momentum, inversion, 1, 1));
  SDL_Event event;
  check(SDL_PeepEvents(&event, 1, SDL_GETEVENT, input->native_scroll_type, input->native_scroll_type) == 1);
  viewer_input_event(input, &event, window);
}
/* One in-flight scroll still consumes all 112 bytes. A second is refused
 * visibly, then cleanup settles the first instead of losing the terminal. */
static void bounded_scroll(void) {
  ft_input_config config; ft_input_config_default(&config); config.max_bytes = 112;
  ft_input_target *target = NULL; ft_input_server *server = NULL; ft_input_client *client = NULL;
  check(ft_input_target_create(&config, &target) == FT_STATUS_OK);
  int sockets[2]; check(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) == 0);
  int32_t source = sockets[0], controller = sockets[1];
  check(ft_input_target_serve(target, &source, &server) == FT_STATUS_OK);
  check(ft_input_client_connect(&controller, FT_INPUT_MODE_COOPERATIVE, &client) == FT_STATUS_OK);
  ft_input_event e = {.kind = FT_INPUT_SCROLL, .scroll_unit = FT_INPUT_SCROLL_PIXEL,
    .geometry_revision = 1, .pointer_x = 1, .pointer_y = 1, .scroll_phase = FT_INPUT_SCROLL_PHASE_BEGAN};
  uint64_t sequence; check(ft_input_client_send(client, &e, &sequence) == FT_STATUS_OK);
  ft_input_work *flight = NULL; uint32_t start = SDL_GetTicks();
  while (ft_input_target_next(target, &flight) == FT_STATUS_EMPTY && SDL_GetTicks() - start < 2000) SDL_Delay(1);
  check(flight != NULL);
  e.scroll_phase = FT_INPUT_SCROLL_PHASE_ENDED;
  check(ft_input_client_send(client, &e, &sequence) == FT_STATUS_OK);
  ft_input_status status; int refused = 0; start = SDL_GetTicks();
  while (!refused && SDL_GetTicks() - start < 2000) {
    if (ft_input_client_poll(client, &status) == FT_STATUS_OK && status.kind == FT_INPUT_REFUSED) {
      check(status.sequence == sequence && status.result == FT_STATUS_CAPACITY); refused = 1;
    } else SDL_Delay(1);
  }
  check(refused);
  ft_input_work *cleanup = NULL; check(ft_input_target_next(target, &cleanup) == FT_STATUS_EMPTY);
  check(ft_input_work_complete(&flight, FT_INPUT_EXECUTED) == FT_STATUS_OK);
  check(ft_input_target_next(target, &cleanup) == FT_STATUS_OK);
  ft_input_operation op; check(ft_input_work_describe(cleanup, &op) == FT_STATUS_OK);
  check(op.event.kind == FT_INPUT_CLEANUP && op.scope == FT_INPUT_SCOPE_ALL && op.reason == FT_INPUT_REASON_OVERFLOW);
  check(ft_input_work_complete(&cleanup, FT_INPUT_EXECUTED) == FT_STATUS_OK);
  int closed = 0; start = SDL_GetTicks();
  while (!closed && SDL_GetTicks() - start < 2000) {
    if (ft_input_client_poll(client, &status) == FT_STATUS_OK && status.kind == FT_INPUT_CLOSED) {
      check(status.clean == 1); closed = 1;
    } else SDL_Delay(1);
  }
  check(closed); ft_input_client_destroy(&client); ft_input_server_destroy(&server);
  check(ft_input_target_destroy(&target) == FT_STATUS_OK);
}
int main(void) {
  check(SDL_Init(SDL_INIT_VIDEO) == 0);
  bounded_scroll();
  SDL_Window *window = SDL_CreateWindow("scroll contract", 0, 0, 100, 50, 0);
  check(window != NULL);
  ft_input_config config; ft_input_config_default(&config);
  config.geometry.width = 200; config.geometry.height = 100;
  /* C construction shares the 112-byte minimum; caller bounds are not enlarged. */
  for (uint32_t bound = 96; bound < 112; ++bound) {
    ft_input_config small = config; small.max_bytes = bound;
    ft_input_target *invalid = NULL;
    check(ft_input_target_create(&small, &invalid) == FT_STATUS_INVALID_ARGUMENT && invalid == NULL);
  }
  ft_input_config exact = config; exact.max_bytes = 112;
  ft_input_target *one = NULL;
  check(ft_input_target_create(&exact, &one) == FT_STATUS_OK);
  check(ft_input_target_destroy(&one) == FT_STATUS_OK);
  ft_input_target *target = NULL; ft_input_server *server = NULL;
  check(ft_input_target_create(&config, &target) == FT_STATUS_OK);
  int sockets[2]; check(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) == 0);
  int32_t source = sockets[0], client = sockets[1];
  check(ft_input_target_serve(target, &source, &server) == FT_STATUS_OK);
  viewer_input input = {.mode = FT_INPUT_MODE_COOPERATIVE};
  check(ft_input_client_connect(&client, input.mode, &input.client) == FT_STATUS_OK);
  uint64_t controller, epoch;
  check(ft_input_client_describe(input.client, &input.config, &controller, &epoch) == FT_STATUS_OK);
  viewer_input_install_scroll_capture(&input, window);
  check(!input.failed);
  SDL_Event event = {0}; event.type = SDL_MOUSEWHEEL;
  /* A notched wheel sends one line per notch without geometry scaling. */
  event.wheel.x = 1; event.wheel.y = -2;
#if SDL_VERSION_ATLEAST(2, 0, 18)
  event.wheel.preciseX = 1; event.wheel.preciseY = -2;
#endif
  viewer_input_event(&input, &event, window);
  expect_scroll(target, FT_INPUT_SCROLL_LINE, 1, 2);
  /* FLIPPED reports device inversion; content-direction deltas stay unchanged. */
  event.wheel.direction = SDL_MOUSEWHEEL_FLIPPED;
  viewer_input_event(&input, &event, window);
  expect_scroll(target, FT_INPUT_SCROLL_LINE, 1, 2);
  /* Opposite-sign FLIPPED wheel deltas preserve content direction too. */
  event.wheel.x = -2; event.wheel.y = 1;
#if SDL_VERSION_ATLEAST(2, 0, 18)
  event.wheel.preciseX = -2; event.wheel.preciseY = 1;
#endif
  viewer_input_event(&input, &event, window);
  expect_scroll(target, FT_INPUT_SCROLL_LINE, -2, -1);
#if SDL_VERSION_ATLEAST(2, 0, 18) && !defined(__APPLE__)
  /* Portable fractional fallback scales window logical deltas to target units. */
  event.wheel.preciseX = .5; event.wheel.preciseY = -.25;
  event.wheel.direction = SDL_MOUSEWHEEL_NORMAL;
  viewer_input_event(&input, &event, window);
  expect_scroll(target, FT_INPUT_SCROLL_PIXEL, 1, .5);
  event.wheel.direction = SDL_MOUSEWHEEL_FLIPPED;
  viewer_input_event(&input, &event, window);
  expect_scroll(target, FT_INPUT_SCROLL_PIXEL, 1, .5);
#endif
  /* Cocoa's captured native pixel metadata uses this same conversion seam.
   * Precise integral deltas must remain pixels, unlike the portable heuristic. */
  /* Zero displacement creates no operation; the next nonzero scroll is the
   * executor's next work, so this checks absence without a timing sleep. */
  viewer_input_scroll(&input, window, 0, 0, FT_INPUT_SCROLL_PIXEL, SDL_MOUSEWHEEL_NORMAL);
  viewer_input_scroll(&input, window, 1, 2, FT_INPUT_SCROLL_PIXEL, SDL_MOUSEWHEEL_NORMAL);
  expect_scroll(target, FT_INPUT_SCROLL_PIXEL, 2, 4);
  /* Captured native deltas also preserve both signs when FLIPPED is set. */
  viewer_input_scroll(&input, window, -.5, .25, FT_INPUT_SCROLL_PIXEL, SDL_MOUSEWHEEL_FLIPPED);
  expect_scroll(target, FT_INPUT_SCROLL_PIXEL, -1, .5);
  /* Native field mapping, independent domains and known false/none survive
   * the real SDL queue and wire. Inversion never changes either signed delta. */
  native_sample(&input, window, 1, 0, 1, -.5, .25);
  expect_metadata(target, FT_INPUT_SCROLL_PIXEL, -1, .5, FT_INPUT_SCROLL_PHASE_BEGAN, FT_INPUT_MOMENTUM_PHASE_NONE, FT_INPUT_SCROLL_INVERSION_TRUE);
  for (int inverted = 0; inverted <= 1; ++inverted) {
    for (unsigned long phase = 0; phase <= 32; phase = phase ? phase * 2 : 1) {
      static const uint32_t mapped[] = {1, 3, 4, 5, 6, 7, 2};
      unsigned index = phase == 0 ? 0 : phase == 1 ? 1 : phase == 2 ? 2 : phase == 4 ? 3 : phase == 8 ? 4 : phase == 16 ? 5 : 6;
      native_sample(&input, window, phase, 0, inverted, 0, 0);
      expect_metadata(target, FT_INPUT_SCROLL_PIXEL, 0, 0, mapped[index], FT_INPUT_MOMENTUM_PHASE_NONE, inverted ? 2 : 1);
    }
  }
  native_sample(&input, window, 8, 1, 0, 1, -2);
  expect_metadata(target, FT_INPUT_SCROLL_PIXEL, 2, -4, FT_INPUT_SCROLL_PHASE_ENDED, FT_INPUT_MOMENTUM_PHASE_BEGAN, 1);
  native_sample(&input, window, 0, 4, 0, -.5, .25);
  expect_metadata(target, FT_INPUT_SCROLL_PIXEL, -1, .5, FT_INPUT_SCROLL_PHASE_NONE, FT_INPUT_MOMENTUM_PHASE_CHANGED, 1);
  native_sample(&input, window, 0, 8, 0, 0, 0);
  expect_metadata(target, FT_INPUT_SCROLL_PIXEL, 0, 0, FT_INPUT_SCROLL_PHASE_NONE, FT_INPUT_MOMENTUM_PHASE_ENDED, 1);
  /* Unsupported bit combinations map only that domain to unknown. */
  native_sample(&input, window, 3, 2, 1, 0, 0);
  expect_metadata(target, FT_INPUT_SCROLL_PIXEL, 0, 0, 0, 0, 2);
  /* C scalar validation and shared stationary semantics reject before execution. */
  ft_input_event invalid = {.kind = FT_INPUT_SCROLL, .scroll_unit = FT_INPUT_SCROLL_PIXEL,
    .geometry_revision = 1, .pointer_x = 1, .pointer_y = 1};
  uint64_t ignored;
  for (unsigned field = 0; field < 3; ++field) {
    invalid.scroll_phase = field == 0 ? 8 : 0;
    invalid.scroll_momentum_phase = field == 1 ? 5 : 0;
    invalid.scroll_inverted_from_device = field == 2 ? 3 : 0;
    check(ft_input_client_send(input.client, &invalid, &ignored) == FT_STATUS_INVALID_ARGUMENT);
  }
  invalid.scroll_inverted_from_device = 0; invalid.scroll_phase = FT_INPUT_SCROLL_PHASE_STATIONARY; invalid.x = 1;
  check(ft_input_client_send(input.client, &invalid, &ignored) == FT_STATUS_INVALID_ARGUMENT);
  invalid.kind = FT_INPUT_MOTION; invalid.x = 1;
  check(ft_input_client_send(input.client, &invalid, &ignored) == FT_STATUS_INVALID_ARGUMENT);
  native_sample(&input, window, 4, 0, 0, 1, 1);
  expect_metadata(target, FT_INPUT_SCROLL_PIXEL, 2, 2, FT_INPUT_SCROLL_PHASE_CHANGED, FT_INPUT_MOMENTUM_PHASE_NONE, 1);
  /* Cartesian C generator covers unknown/known-none, every closed enum and
   * both known inversion bits through the actual client/wire/work interface. */
  ft_input_event sample = {.kind = FT_INPUT_SCROLL, .scroll_unit = FT_INPUT_SCROLL_PAGE,
    .geometry_revision = 1, .pointer_x = 1, .pointer_y = 1};
  for (uint32_t phase = 0; phase <= 7; ++phase) {
    for (uint32_t momentum = 0; momentum <= 4; ++momentum) {
      for (uint32_t inversion = 0; inversion <= 2; ++inversion) {
        sample.scroll_phase = phase; sample.scroll_momentum_phase = momentum; sample.scroll_inverted_from_device = inversion;
        check(ft_input_client_send(input.client, &sample, &ignored) == FT_STATUS_OK);
        expect_metadata(target, FT_INPUT_SCROLL_PAGE, 0, 0, phase, momentum, inversion);
      }
    }
  }
  /* A drag starting inside and released in a letterbox bar emits no outside
   * pointer event, but executes reset cleanup so the source button cannot latch.
   * The real socket target's next operation also proves outside motion is absent. */
  input.frame_width = 100; input.frame_height = 100;
  SDL_Event button = {.button = {.type = SDL_MOUSEBUTTONDOWN, .button = SDL_BUTTON_LEFT, .x = 50, .y = 25}};
  viewer_input_event(&input, &button, window);
  ft_input_work *drag = NULL; uint32_t drag_start = SDL_GetTicks();
  while (ft_input_target_next(target, &drag) == FT_STATUS_EMPTY && SDL_GetTicks() - drag_start < 2000) SDL_Delay(1);
  check(drag != NULL);
  ft_input_operation drag_op; check(ft_input_work_describe(drag, &drag_op) == FT_STATUS_OK);
  check(drag_op.event.kind == FT_INPUT_BUTTON && drag_op.event.action == FT_INPUT_DOWN);
  check(drag_op.event.x == 100 && drag_op.event.y == 50);
  check(ft_input_work_complete(&drag, FT_INPUT_EXECUTED) == FT_STATUS_OK);
  SDL_Event motion = {.motion = {.type = SDL_MOUSEMOTION, .x = 0, .y = 25}};
  viewer_input_event(&input, &motion, window);
  button.button.type = SDL_MOUSEBUTTONUP; button.button.x = 0;
  viewer_input_event(&input, &button, window);
  drag_start = SDL_GetTicks();
  while (ft_input_target_next(target, &drag) == FT_STATUS_EMPTY && SDL_GetTicks() - drag_start < 2000) SDL_Delay(1);
  check(drag != NULL);
  check(ft_input_work_describe(drag, &drag_op) == FT_STATUS_OK);
  check(drag_op.event.kind == FT_INPUT_CLEANUP);
  check(ft_input_work_complete(&drag, FT_INPUT_EXECUTED) == FT_STATUS_OK);
  drag_start = SDL_GetTicks();
  while (input.resetting && SDL_GetTicks() - drag_start < 2000) { viewer_input_poll(&input); SDL_Delay(1); }
  check(!input.resetting && !input.failed && input.buttons == 0);
  /* A stale positional action is dropped without ending the viewer or recording
   * a rejected button hold. Advance target geometry while the viewer still
   * has its old snapshot, then prove input resumes after polling the reset. */
  input.frame_width = 0; input.frame_height = 0;
  ft_input_geometry resized = input.config.geometry;
  resized.revision++; resized.width *= 2; resized.height *= 2;
  check(ft_input_target_geometry(target, &resized) == FT_STATUS_OK);
  ft_input_work *resize_work = NULL; uint32_t resize_start = SDL_GetTicks();
  while (ft_input_target_next(target, &resize_work) == FT_STATUS_EMPTY && SDL_GetTicks() - resize_start < 2000) SDL_Delay(1);
  check(resize_work != NULL);
  ft_input_operation resize_op;
  check(ft_input_work_describe(resize_work, &resize_op) == FT_STATUS_OK);
  check(resize_op.event.kind == FT_INPUT_CLEANUP && resize_op.scope == FT_INPUT_SCOPE_POINTER);
  check(ft_input_work_complete(&resize_work, FT_INPUT_EXECUTED) == FT_STATUS_OK);
  /* Wait for the transport's new geometry without consuming the viewer reset. */
  ft_input_config current; resize_start = SDL_GetTicks();
  do {
    check(ft_input_client_describe(input.client, &current, &controller, &epoch) == FT_STATUS_OK);
    if (current.geometry.revision == resized.revision) break;
    SDL_Delay(1);
  } while (SDL_GetTicks() - resize_start < 2000);
  check(current.geometry.revision == resized.revision);
  viewer_input_scroll(&input, window, 1, 2, FT_INPUT_SCROLL_LINE, SDL_MOUSEWHEEL_NORMAL);
  check(!input.failed);
  motion.motion.x = 50; motion.motion.y = 25;
  viewer_input_event(&input, &motion, window);
  check(!input.failed);
  button.button.type = SDL_MOUSEBUTTONDOWN; button.button.x = 50;
  viewer_input_event(&input, &button, window);
  check(!input.failed && input.buttons == 0);
  viewer_input_poll(&input);
  check(input.config.geometry.revision == resized.revision && !input.failed);
  /* After Reset the native controller discards the source tail; the target
   * also refuses a directly submitted orphan without another cleanup/reset. */
  native_sample(&input, window, 4, 0, 0, 1, 1);
  sample.geometry_revision = resized.revision; sample.scroll_phase = FT_INPUT_SCROLL_PHASE_CHANGED;
  sample.scroll_momentum_phase = FT_INPUT_MOMENTUM_PHASE_NONE; sample.scroll_inverted_from_device = 0;
  uint64_t orphan_sequence;
  check(ft_input_client_send(input.client, &sample, &orphan_sequence) == FT_STATUS_OK);
  ft_input_status refusal; int refused = 0; uint32_t refusal_start = SDL_GetTicks();
  while (!refused && SDL_GetTicks() - refusal_start < 2000) {
    if (ft_input_client_poll(input.client, &refusal) == FT_STATUS_OK && refusal.sequence == orphan_sequence && refusal.kind == FT_INPUT_REFUSED) {
      check(refusal.result == FT_STATUS_STALE); refused = 1;
    } else SDL_Delay(1);
  }
  check(refused);
  ft_input_work *extra = NULL; check(ft_input_target_next(target, &extra) == FT_STATUS_EMPTY);
  native_sample(&input, window, 1, 0, 0, 0, 0);
  expect_metadata(target, FT_INPUT_SCROLL_PIXEL, 0, 0, FT_INPUT_SCROLL_PHASE_BEGAN, FT_INPUT_MOMENTUM_PHASE_NONE, 1);
  viewer_input_scroll(&input, window, 3, 4, FT_INPUT_SCROLL_LINE, SDL_MOUSEWHEEL_NORMAL);
  expect_scroll(target, FT_INPUT_SCROLL_LINE, 3, 4);
  /* Finish the actual executor cleanup before destroying independent handles. */
  ft_input_client_close(input.client);
  /* A real closed-client send must still stop the viewer. */
  viewer_input_scroll(&input, window, 1, 2, FT_INPUT_SCROLL_LINE, SDL_MOUSEWHEEL_NORMAL);
  check(input.failed);
  ft_input_work *work = NULL; uint32_t start = SDL_GetTicks();
  while (ft_input_target_next(target, &work) == FT_STATUS_EMPTY && SDL_GetTicks() - start < 2000) SDL_Delay(1);
  check(work != NULL);
  ft_input_operation op; check(ft_input_work_describe(work, &op) == FT_STATUS_OK);
  check(op.event.kind == FT_INPUT_CLEANUP);
  check(ft_input_work_complete(&work, FT_INPUT_EXECUTED) == FT_STATUS_OK);
  ft_input_status status; int clean = 0; start = SDL_GetTicks();
  while (!clean && SDL_GetTicks() - start < 2000) {
    if (ft_input_client_poll(input.client, &status) == FT_STATUS_OK && status.kind == FT_INPUT_CLOSED) {
      check(status.clean == 1); clean = 1;
    } else SDL_Delay(1);
  }
  check(clean);
  ft_input_client_destroy(&input.client);
  ft_input_server_destroy(&server);
  check(ft_input_target_destroy(&target) == FT_STATUS_OK);
  SDL_DestroyWindow(window); SDL_Quit();
  return 0;
}
