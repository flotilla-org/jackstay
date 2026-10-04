#include "scroll_self_test.h"
#include <assert.h>

static void drain(void) {
  SDL_Event event;
  while (SDL_PollEvent(&event)) {}
}

int main(void) {
  assert(SDL_Init(SDL_INIT_VIDEO) == 0);
  SDL_Window *window = SDL_CreateWindow("scroll driver", 0, 0, 640, 480, SDL_WINDOW_HIDDEN);
  assert(window);
  SDL_Renderer *renderer = SDL_CreateRenderer(window, -1, SDL_RENDERER_SOFTWARE);
  assert(renderer);
  viewer_affordances a = {.renderer = renderer};
  a.scroll.present = 1;
  a.scroll.snapshot.capabilities = 3;
  /* Each gesture must wait through initial/late frame replacement and window
   * resize. Resuming the target fit queues the real SDL gesture exactly once. */
  for (int stage = 0; stage < 5; stage++) {
    a.scroll.snapshot.x.position = stage == 3 ? 600 : 50;
    a.scroll.snapshot.y.position = stage == 1 ? 600 : 100;
    int next = stage;
    drain();
    for (int retry = 0; retry < 20; retry++) {
      scroll_self_test(&a, window, 320, 180, &next);
      scroll_self_test(&a, window, 0, 0, &next);
    }
    assert(next == stage);
    assert(!SDL_HasEvents(SDL_MOUSEMOTION, SDL_MOUSEWHEEL));
    SDL_SetWindowSize(window, 320, 180);
    drain();
    scroll_self_test(&a, window, 320, 180, &next);
    assert(next == stage);
    assert(!SDL_HasEvents(SDL_MOUSEMOTION, SDL_MOUSEWHEEL));
    SDL_SetWindowSize(window, 640, 480);
    drain();
    /* A navigation strip reserves rows, so fixed scroll fixture points must
     * wait even when frame and window dimensions otherwise match. */
    a.navigation.visible = 1;
    scroll_self_test(&a, window, 640, 480, &next);
    assert(next == stage);
    assert(!SDL_HasEvents(SDL_MOUSEMOTION, SDL_MOUSEWHEEL));
    a.navigation.visible = 0;
    scroll_self_test(&a, window, 640, 480, &next);
    assert(next == stage + 1);
    assert(SDL_HasEvents(SDL_MOUSEMOTION, SDL_MOUSEWHEEL));
  }
  SDL_DestroyRenderer(renderer);
  SDL_DestroyWindow(window);
  SDL_Quit();
  return 0;
}
