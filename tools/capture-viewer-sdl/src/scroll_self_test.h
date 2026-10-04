#ifndef SCROLL_SELF_TEST_H
#define SCROLL_SELF_TEST_H
#include "viewer_affordances.h"
#include "viewer_fit.h"

/* Offline integration driver uses the real SDL queue and normal routing.
 * The scroll toolkit fixture requests a 640x480 window with an unletterboxed
 * frame, 1000-unit content, 200-unit viewports, and initial positions x=50/y=100.
 * Fixed pointer points and thresholds below intentionally test that geometry. */
static void scroll_self_test(viewer_affordances *a, SDL_Window *window, int frame_width, int frame_height, int *stage) {
  int w, h, dw = 0, dh = 0;
  SDL_GetWindowSize(window, &w, &h);
  SDL_GetRendererOutputSize(a->renderer, &dw, &dh);
  SDL_Rect fit = viewer_fit(dw, dh, frame_width, frame_height);
  /* Wait for the acquired frame's fit, including after replacement. A frame
   * count cannot establish that the fixed points below hit either track. */
  if (a->navigation.visible || w != 640 || h != 480 || fit.x != 0 || fit.y != 0 ||
      fit.w != dw || fit.h != dh || dw <= 0 || dh <= 0) return;
  viewer_scroll *s = &a->scroll;
  if (!s->present) return;
  double x = s->snapshot.x.position, y = s->snapshot.y.position;
  if (((s->snapshot.capabilities & 3) == 3 && ((*stage == 1 && y < 500) || (*stage == 2 && y > 500) ||
      (*stage == 3 && x < 500) || (*stage == 4 && x > 500))) || *stage >= 5) return;
  int px = *stage < 2 ? 636 : *stage < 4 ? 96 : 636;
  int py = *stage == 0 ? 96 : *stage == 1 ? 5 : *stage < 4 ? 476 : 200;
  if (*stage == 3) px = 5;
  SDL_Event e = {.button = {.type = SDL_MOUSEBUTTONDOWN, .button = SDL_BUTTON_LEFT, .x = px, .y = py}};
  if (*stage == 4) {
    SDL_WarpMouseInWindow(window, px, py);
    e = (SDL_Event){.wheel = {.type = SDL_MOUSEWHEEL, .y = -2}};
#if SDL_VERSION_ATLEAST(2, 0, 18)
    e.wheel.preciseY = -2;
#endif
    SDL_PushEvent(&e);
  } else {
    SDL_PushEvent(&e);
    if (*stage == 0 || *stage == 2) {
      for (int i = 0; i < 3; i++) {
        SDL_Event motion = {.motion = {.type = SDL_MOUSEMOTION,
          .state = SDL_BUTTON_LMASK, .x = *stage == 0 ? px : 380 + 10 * i, .y = *stage == 0 ? 300 + 10 * i : py}};
        SDL_PushEvent(&motion);
      }
      e.button.x = *stage == 0 ? px : 400; e.button.y = *stage == 0 ? 320 : py;
    }
    e.type = SDL_MOUSEBUTTONUP; SDL_PushEvent(&e);
  }
  (*stage)++;
}

#endif
