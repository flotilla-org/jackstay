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
  /* Natural-scrolling inversion is applied once for either sign. */
  event.wheel.direction = SDL_MOUSEWHEEL_FLIPPED;
  viewer_input_event(&input, &event, window);
  expect_scroll(target, FT_INPUT_SCROLL_LINE, -1, -2);
#if SDL_VERSION_ATLEAST(2, 0, 18) && !defined(__APPLE__)
  /* Portable fractional fallback scales window logical deltas to target units. */
  event.wheel.preciseX = .5; event.wheel.preciseY = -.25;
  event.wheel.direction = SDL_MOUSEWHEEL_NORMAL;
  viewer_input_event(&input, &event, window);
  expect_scroll(target, FT_INPUT_SCROLL_PIXEL, 1, .5);
  event.wheel.direction = SDL_MOUSEWHEEL_FLIPPED;
  viewer_input_event(&input, &event, window);
  expect_scroll(target, FT_INPUT_SCROLL_PIXEL, -1, -.5);
#endif
  /* Finish the actual executor cleanup before destroying independent handles. */
  ft_input_client_close(input.client);
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
