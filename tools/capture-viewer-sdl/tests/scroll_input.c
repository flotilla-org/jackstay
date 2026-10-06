/* Real input target/client over the socket boundary; no desktop capture. */
#include "viewer_input.h"
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <unistd.h>
static void check(int value) { if (!value) { fputs("scroll contract failed\n", stderr); exit(1); } }
static void expect_scroll(ft_input_target *target, uint32_t unit, double x, double y) {
  ft_input_work *work = NULL;
  uint32_t start = SDL_GetTicks();
  while (ft_input_target_next(target, &work) == FT_STATUS_EMPTY && SDL_GetTicks() - start < 2000) SDL_Delay(1);
  check(work != NULL);
  ft_input_operation op;
  check(ft_input_work_describe(work, &op) == FT_STATUS_OK);
  /* The scroll unit and signed deltas are observable at the real executor. */
  check(op.event.kind == FT_INPUT_SCROLL && op.event.scroll_unit == unit && op.event.x == x && op.event.y == y);
  check(ft_input_work_complete(&work, FT_INPUT_EXECUTED) == FT_STATUS_OK);
}
int main(void) {
  check(SDL_Init(SDL_INIT_VIDEO) == 0);
  SDL_Window *window = SDL_CreateWindow("scroll contract", 0, 0, 100, 50, 0);
  check(window != NULL);
  ft_input_config config; ft_input_config_default(&config);
  config.geometry.width = 200; config.geometry.height = 100;
  ft_input_target *target = NULL; ft_input_server *server = NULL;
  check(ft_input_target_create(&config, &target) == FT_STATUS_OK);
  int sockets[2]; check(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) == 0);
  int32_t source = sockets[0], client = sockets[1];
  check(ft_input_target_serve(target, &source, &server) == FT_STATUS_OK);
  viewer_input input = {.mode = FT_INPUT_MODE_COOPERATIVE};
  check(ft_input_client_connect(&client, input.mode, &input.client) == FT_STATUS_OK);
  uint64_t controller, epoch;
  check(ft_input_client_describe(input.client, &input.config, &controller, &epoch) == FT_STATUS_OK);
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
