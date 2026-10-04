#include "viewer_affordances.h"
#include "viewer_fit.h"
#include <assert.h>
#include <math.h>
#include <string.h>

int main(void) {
  /* Generated sizes cover portrait/landscape/square frames and 1x/2x drawable
   * scales. Fit preserves aspect and maps the interior into input geometry;
   * all four exterior edges, including exclusive far boundaries, reject input. */
  for (int w = 100; w <= 700; w += 100) for (int h = 100; h <= 700; h += 100)
  for (int fw = 100; fw <= 500; fw += 100) for (int fh = 100; fh <= 500; fh += 100)
  for (int scale = 1; scale <= 2; scale++) {
    SDL_Rect r = viewer_fit(w * scale, h * scale, fw, fh);
    assert(r.w <= w * scale && r.h <= h * scale && r.w > 0 && r.h > 0);
    assert(fabs((double)r.w / fw - (double)r.h / fh) <= 1.0 / fmin(fw, fh));
    double x, y;
    assert(viewer_map(r, r.x + r.w * .5, r.y + r.h * .5, 800, 600, &x, &y));
    assert(fabs(x - 400) < .0001 && fabs(y - 300) < .0001);
    assert(!viewer_map(r, r.x - 1, r.y, 800, 600, &x, &y));
    assert(!viewer_map(r, r.x, r.y - 1, 800, 600, &x, &y));
    assert(!viewer_map(r, r.x + r.w, r.y, 800, 600, &x, &y));
    assert(!viewer_map(r, r.x, r.y + r.h, 800, 600, &x, &y));
  }
  double x, y;
  assert(!viewer_map(viewer_fit(0, 0, 0, 0), 0, 0, 1, 1, &x, &y));
  assert(SDL_Init(SDL_INIT_VIDEO) == 0);
  SDL_Window *window = SDL_CreateWindow("default", 0, 0, 320, 240, SDL_WINDOW_HIDDEN | SDL_WINDOW_RESIZABLE);
  assert(window);
  SDL_Renderer *renderer = SDL_CreateRenderer(window, -1, SDL_RENDERER_SOFTWARE);
  assert(renderer);
  viewer_affordances a = {.window = window, .renderer = renderer, .started = SDL_GetTicks(), .has_window = 1};
  /* A window domain defers showing until ready, or until the two second
   * deadline. Requested sizes stop taking effect after a user resize. */
  assert(!viewer_affordances_tick(&a)); assert(!a.shown);
  ft_aff_snapshot s = {.domain = FT_AFF_DOMAIN_WINDOW,
    .window = {.requested_size = {.present = 1, .width = 400, .height = 300}}};
  viewer_affordances_snapshot(&a, &s);
  int width, height; SDL_GetWindowSize(window, &width, &height);
  assert(width == 400 && height == 300);
  SDL_Event event = {.window = {.type = SDL_WINDOWEVENT, .event = SDL_WINDOWEVENT_RESIZED}};
  viewer_affordances_event(&a, &event);
  s.window.requested_size.width = 500; viewer_affordances_snapshot(&a, &s);
  SDL_GetWindowSize(window, &width, &height); assert(width == 400);
  s.window.ready = 1; viewer_affordances_snapshot(&a, &s);
  assert(!viewer_affordances_tick(&a)); assert(a.shown);
  a.shown = 0; a.ready = 0; a.started = SDL_GetTicks() - 2000;
  assert(!viewer_affordances_tick(&a)); assert(a.shown);
  a.shown = 0; a.has_window = 0; a.started = SDL_GetTicks();
  assert(!viewer_affordances_tick(&a)); assert(a.shown);
  /* Title fallback follows window title, navigation title, URL, default,
   * and withdrawals remove cached values. Empty titles remain valid. */
  ft_aff_snapshot n = {.domain = FT_AFF_DOMAIN_NAVIGATION,
    .navigation = {.url = {.present = 1, .value = {.data = (const uint8_t *)"URL", .len = 3}}}};
  viewer_affordances_snapshot(&a, &n); assert(!strcmp(SDL_GetWindowTitle(window), "URL"));
  n.navigation.title = (ft_aff_optional_string){.present = 1, .value = {.data = (const uint8_t *)"Nav", .len = 3}};
  viewer_affordances_snapshot(&a, &n); assert(!strcmp(SDL_GetWindowTitle(window), "Nav"));
  s.window.title = (ft_aff_optional_string){.present = 1, .value = {.data = (const uint8_t *)"", .len = 0}};
  viewer_affordances_snapshot(&a, &s); assert(!strcmp(SDL_GetWindowTitle(window), ""));
  s.withdrawn = 1; viewer_affordances_snapshot(&a, &s); assert(!strcmp(SDL_GetWindowTitle(window), "Nav"));
  n.withdrawn = 1; viewer_affordances_snapshot(&a, &n); assert(!strcmp(SDL_GetWindowTitle(window), "capture-viewer-sdl"));
  /* Every visibility/focus event updates its independent presentation hint. */
  const Uint8 events[] = {SDL_WINDOWEVENT_HIDDEN, SDL_WINDOWEVENT_SHOWN, SDL_WINDOWEVENT_MINIMIZED,
    SDL_WINDOWEVENT_RESTORED, SDL_WINDOWEVENT_FOCUS_GAINED, SDL_WINDOWEVENT_FOCUS_LOST};
  for (unsigned i = 0; i < sizeof(events); i++) {
    event.window.event = events[i]; viewer_affordances_event(&a, &event);
    if (i < 4) assert(a.visible == (int)(i % 2));
    else assert(a.focused == (i == 4));
  }
  viewer_affordances_close(&a, 0);
  SDL_DestroyRenderer(renderer); SDL_DestroyWindow(window); SDL_Quit();
  return 0;
}
