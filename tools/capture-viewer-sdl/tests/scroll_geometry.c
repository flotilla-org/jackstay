#include "viewer_scroll.h"
#include <assert.h>
#include <math.h>
int main(void) {
  /* Generated ratios, offsets and both orientations cover the full range:
   * proportional thumbs round-trip positions and clamp overscroll/drag edges. */
  for (int v = 0; v <= 1; v++) for (int length = 1; length <= 1024; length *= 2)
  for (int content = 1; content <= 10000; content *= 10)
  for (int ratio = 0; ratio <= 12; ratio++) for (int offset = -1; offset <= 11; offset++) {
    ft_aff_axis a = {.scrollable = 1, .content_length = content,
      .viewport_length = content * ratio / 10.0, .position = content * offset / 10.0};
    viewer_scroll_geometry g;
    SDL_Rect frame = {17, 29, length, length};
    int valid = viewer_scroll_geometry_for(a, frame, v, 8, &g);
    assert(valid == (a.content_length > a.viewport_length));
    if (valid) {
      double range = a.content_length - a.viewport_length;
      double expected = fmax(0, fmin(a.position, range));
      assert(fabs(g.thumb_length - length * ratio / 10.0) < 1e-8);
      assert(fabs(g.thumb_start - (v ? frame.y : frame.x) - (length - g.thumb_length) * expected / range) < 1e-8);
      for (int grab = 0; grab <= 2; grab++) {
        double handle = g.thumb_length * grab / 2;
        assert(fabs(viewer_scroll_position(a, g, g.thumb_start + handle, handle) - expected) < 1e-7);
        assert(viewer_scroll_position(a, g, g.start - 100, handle) == 0);
        assert(viewer_scroll_position(a, g, g.start + length + 100, handle) == range);
      }
      assert(g.track.x >= frame.x && g.track.y >= frame.y);
      assert(g.track.x + g.track.w <= frame.x + frame.w && g.track.y + g.track.h <= frame.y + frame.h);
    }
    /* A non-scrollable axis never draws, even if its lengths otherwise qualify. */
    a.scrollable = 0; assert(!viewer_scroll_geometry_for(a, frame, v, 8, &g));
    assert(viewer_scroll_position(a, g, 100, 0) == 0);
  }
  /* Invalid/empty geometry is rejected before division or SDL conversion. */
  viewer_scroll_geometry g;
  ft_aff_axis a = {1, 1000, 200, 0};
  assert(!viewer_scroll_geometry_for(a, (SDL_Rect){0}, 1, 8, &g));
  a.content_length = NAN; assert(!viewer_scroll_geometry_for(a, (SDL_Rect){0,0,100,100}, 1, 8, &g));
  a.content_length = INFINITY; assert(!viewer_scroll_geometry_for(a, (SDL_Rect){0,0,100,100}, 1, 8, &g));
  a.content_length = 1000; a.viewport_length = -1;
  assert(!viewer_scroll_geometry_for(a, (SDL_Rect){0,0,100,100}, 1, 8, &g));
  assert(SDL_Init(SDL_INIT_VIDEO) == 0);
  SDL_Window *window = SDL_CreateWindow("scroll contract", 0, 0, 100, 100, 0);
  assert(window);
  SDL_Renderer *renderer = SDL_CreateRenderer(window, -1, SDL_RENDERER_SOFTWARE);
  assert(renderer);
  SDL_Rect frame = {10, 10, 80, 80};
  viewer_scroll state = {0};
  ft_aff_scroll snapshot = {.x = {1,1000,200,0}, .y = {1,1000,200,100},
    .capabilities = FT_AFF_SCROLL_SET_POSITION | FT_AFF_SCROLL_SCROLL_BY_STEP};
  /* Duplicate snapshots cannot prolong the one-second reveal; hover can.
   * Unsigned tick subtraction must also behave across the SDL tick wrap. */
  viewer_scroll_snapshot(&state, &snapshot, UINT32_MAX - 500);
  assert(viewer_scroll_visible(&state, 498));
  viewer_scroll_snapshot(&state, &snapshot, 498);
  assert(!viewer_scroll_visible(&state, 499));
  SDL_Event motion = {.motion = {.type = SDL_MOUSEMOTION, .x = 50, .y = 50}};
  assert(!viewer_scroll_event(&state, NULL, &motion, frame, 1, 1, 0, 2000));
  assert(viewer_scroll_visible(&state, 2000));
  motion.motion.x = 0;
  assert(!viewer_scroll_event(&state, NULL, &motion, frame, 1, 1, 0, 2000));
  assert(!viewer_scroll_visible(&state, 2000));
  /* A thumb grab preserves its offset, coalesces many motions, and retains the
   * final axis after release even with both axes present. Wheel is never owned. */
  SDL_Event down = {.button = {.type = SDL_MOUSEBUTTONDOWN, .button = SDL_BUTTON_LEFT, .x = 85, .y = 25}};
  assert(viewer_scroll_event(&state, NULL, &down, frame, 1, 1, 0, 2000) == 1);
  assert(state.dragging == 2);
  motion.motion.x = 85; motion.motion.y = 45; motion.motion.state = SDL_BUTTON_LMASK;
  assert(viewer_scroll_event(&state, NULL, &motion, frame, 1, 1, 0, 2000) == 1);
  motion.motion.y = 65;
  assert(viewer_scroll_event(&state, NULL, &motion, frame, 1, 1, 0, 2000) == 1);
  assert(state.pending && state.pending_axis == 1 && fabs(state.position - 600) < 1e-8);
  SDL_Event wheel = {.wheel = {.type = SDL_MOUSEWHEEL, .y = 1}};
  assert(!viewer_scroll_event(&state, NULL, &wheel, frame, 1, 1, 0, 2000));
  SDL_Event up = down; up.type = SDL_MOUSEBUTTONUP; up.button.y = 65;
  assert(viewer_scroll_event(&state, NULL, &up, frame, 1, 1, 0, 2000) == 1);
  assert(!state.owned && !state.dragging && state.pending_axis == 1);
  assert(viewer_scroll_draw(&state, NULL, renderer, frame, 8, 2000) == 0);
  assert(!state.pending);
  /* Capability removal cancels queued drags. Read-only thumbs still consume
   * the complete gesture, without emitting a pending set_position. */
  assert(viewer_scroll_event(&state, NULL, &down, frame, 1, 1, 0, 2000) == 1);
  assert(viewer_scroll_event(&state, NULL, &motion, frame, 1, 1, 0, 2000) == 1);
  assert(state.pending);
  snapshot.capabilities = 0; viewer_scroll_snapshot(&state, &snapshot, 2000);
  assert(!state.dragging && !state.pending);
  assert(viewer_scroll_event(&state, NULL, &up, frame, 1, 1, 0, 2000) == 1);
  assert(viewer_scroll_event(&state, NULL, &down, frame, 1, 1, 0, 2000) == 1);
  assert(!state.dragging && !state.pending);
  assert(viewer_scroll_event(&state, NULL, &motion, frame, 1, 1, 0, 2000) == 1);
  assert(!state.pending);
  /* Withdrawal/closure cancels drawing and pending work, but swallows the
   * matching release. Producer drags cannot be intercepted over a scrollbar. */
  viewer_scroll_snapshot(&state, NULL, 2000);
  assert(!viewer_scroll_visible(&state, 2000));
  assert(viewer_scroll_event(&state, NULL, &up, frame, 1, 1, 0, 2000) == 1);
  viewer_scroll_snapshot(&state, &snapshot, 2000);
  assert(!viewer_scroll_event(&state, NULL, &down, frame, 1, 1, 1, 2000));
  assert(!viewer_scroll_event(&state, NULL, &up, frame, 1, 1, 0, 2000));
  /* HiDPI event conversion hits the same drawable track. Focus loss ends the
   * drag and reveal, while suppressing a late matching release. */
  snapshot.capabilities = FT_AFF_SCROLL_SET_POSITION;
  viewer_scroll_snapshot(&state, &snapshot, 2000);
  SDL_Rect retina = {20,20,160,160};
  assert(viewer_scroll_event(&state, NULL, &down, retina, 2, 2, 0, 2000) == 1);
  assert(state.dragging == 2);
  SDL_Event focus = {.window = {.type = SDL_WINDOWEVENT, .event = SDL_WINDOWEVENT_FOCUS_LOST}};
  assert(!viewer_scroll_event(&state, NULL, &focus, retina, 2, 2, 0, 3000));
  assert(!state.dragging && !state.pending && !viewer_scroll_visible(&state, 3000));
  assert(viewer_scroll_event(&state, NULL, &up, retina, 2, 2, 0, 3000) == 1);
  /* A release lost outside the window cannot swallow later ordinary motion.
   * This holds for focus loss and withdrawal, and for every captured button. */
  for (int button = SDL_BUTTON_LEFT; button <= SDL_BUTTON_RIGHT; button++) {
    down.button.button = button;
    motion.motion.state = SDL_BUTTON(button);
    viewer_scroll_snapshot(&state, &snapshot, 4000);
    assert(viewer_scroll_event(&state, NULL, &down, frame, 1, 1, 0, 4000) == 1);
    assert(!viewer_scroll_event(&state, NULL, &focus, frame, 1, 1, 0, 4000));
    motion.motion.x = 50; motion.motion.y = 50; motion.motion.state = 0;
    assert(!viewer_scroll_event(&state, NULL, &motion, frame, 1, 1, 0, 4000));
    assert(!state.owned);
    assert(viewer_scroll_event(&state, NULL, &down, frame, 1, 1, 0, 4000) == 1);
    viewer_scroll_snapshot(&state, NULL, 4000);
    assert(!viewer_scroll_event(&state, NULL, &motion, frame, 1, 1, 0, 4000));
    assert(!state.owned && !state.pending);
  }
  viewer_scroll_snapshot(&state, &snapshot, 3000);
  /* Drawing uses host pixels only within the fitted frame, restores SDL state,
   * and disappears on withdrawal. Inspect actual software-rendered pixels. */
  state.hovered = 1;
  SDL_SetRenderDrawColor(renderer, 0, 0, 0, 255); SDL_RenderClear(renderer);
  assert(!viewer_scroll_draw(&state, NULL, renderer, frame, 8, 3000));
  Uint8 r, green, b, alpha;
  SDL_GetRenderDrawColor(renderer, &r, &green, &b, &alpha);
  assert(r == 0 && green == 0 && b == 0 && alpha == 255);
  Uint32 pixels[100 * 100];
  assert(!SDL_RenderReadPixels(renderer, NULL, SDL_PIXELFORMAT_ARGB8888, pixels, 100 * 4));
  assert(pixels[25 * 100 + 85] != pixels[25 * 100 + 50]);
  assert(pixels[25 * 100 + 95] == pixels[25 * 100 + 50]);
  viewer_scroll_snapshot(&state, NULL, 3000);
  SDL_RenderClear(renderer);
  assert(!viewer_scroll_draw(&state, NULL, renderer, frame, 8, 3000));
  assert(!SDL_RenderReadPixels(renderer, NULL, SDL_PIXELFORMAT_ARGB8888, pixels, 100 * 4));
  assert(pixels[25 * 100 + 85] == pixels[25 * 100 + 50]);
  SDL_DestroyRenderer(renderer); SDL_DestroyWindow(window); SDL_Quit();
  return 0;
}
