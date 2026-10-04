#include "viewer_affordances.h"
#include "viewer_fit.h"
#ifdef NDEBUG
#undef NDEBUG
#endif
#include <assert.h>
#include <string.h>
int main(void) {
  /* Exhaustively generate publication, boolean state triples, all capability
   * combinations and verbs. Both state and capability must permit a verb. */
  for (int visible = 0; visible < 2; ++visible)
  for (unsigned state = 0; state < 8; ++state)
  for (unsigned caps = 0; caps < 32; ++caps) {
    viewer_navigation n = {.visible = visible, .state = {.can_go_back = state & 1,
      .can_go_forward = (state >> 1) & 1, .loading = (state >> 2) & 1, .capabilities = caps}};
    int gates[] = {0, !!(state & 1), !!(state & 2), !(state & 4), !!(state & 4), 1};
    for (unsigned verb = 1; verb <= 5; ++verb)
      assert(viewer_navigation_enabled(&n, verb) == (visible && gates[verb] && !!(caps & (1u << (verb - 1)))));
    assert(!viewer_navigation_enabled(&n, 0)); assert(!viewer_navigation_enabled(&n, 6));
  }
  /* Generated portrait/landscape/narrow/empty frames at 1x/2x: the strip shifts
   * only y, preserving frame size; strip and exclusive edges reject input. */
  for (int w = 1; w <= 641; w += 80) for (int h = 1; h <= 481; h += 60)
  for (int fw = 0; fw <= 640; fw += 160) for (int fh = 0; fh <= 480; fh += 120)
  for (int scale = 1; scale <= 2; ++scale) {
    SDL_Rect b = viewer_fit(w * scale, h * scale, fw, fh);
    SDL_Rect a = viewer_navigation_fit(w * scale, (h + VIEWER_NAV_HEIGHT) * scale, fw, fh, VIEWER_NAV_HEIGHT * scale);
    assert(b.x == a.x && b.w == a.w && b.h == a.h && a.y == b.y + VIEWER_NAV_HEIGHT * scale);
    double x, y;
    assert(!viewer_map(a, a.x, VIEWER_NAV_HEIGHT * scale - 1, 640, 480, &x, &y));
    assert(!viewer_map(a, a.x + a.w, a.y, 640, 480, &x, &y));
  }
  assert(SDL_Init(SDL_INIT_VIDEO) == 0);
  SDL_Window *window = SDL_CreateWindow("navigation", 0, 0, 320, 240, SDL_WINDOW_HIDDEN | SDL_WINDOW_RESIZABLE); assert(window);
  SDL_Renderer *renderer = SDL_CreateRenderer(window, -1, SDL_RENDERER_SOFTWARE); assert(renderer);
  viewer_affordances a = {.window = window, .renderer = renderer};
  ft_aff_snapshot s = {.domain = FT_AFF_DOMAIN_NAVIGATION, .navigation = {
    .url = {.present = 1, .value = {.data = (const uint8_t *)"https://example.test", .len = 20}}, .capabilities = 16}};
  /* Duplicate snapshots, withdrawal, and re-publication preserve content size
   * and do not turn resize acknowledgements into user ownership. */
  for (int i = 0; i < 6; ++i) {
    s.withdrawn = i == 2 || i == 5; viewer_affordances_snapshot(&a, &s);
    int w, h; SDL_GetWindowSize(window, &w, &h);
    assert(w == 320 && h == 240 + (s.withdrawn ? 0 : VIEWER_NAV_HEIGHT));
    SDL_Event e = {.window = {.type = SDL_WINDOWEVENT, .event = SDL_WINDOWEVENT_RESIZED, .data1 = w, .data2 = h}};
    viewer_affordances_event(&a, &e); assert(!a.user_resized);
    /* Software renderer surfaces may be refreshed only after pumping resize
     * events and clearing, particularly with SDL's macOS dummy driver. */
    SDL_PumpEvents(); assert(!SDL_RenderClear(renderer));
    int dw, dh; assert(!SDL_GetRendererOutputSize(renderer, &dw, &dh));
    SDL_Rect fit = viewer_affordances_fit(&a, 320, 240);
    assert(fit.w == dw && fit.h == dh - viewer_affordances_strip(&a));
  }
  s.withdrawn = 0; viewer_affordances_snapshot(&a, &s);
  /* URL selection is replaced by text, UTF-8 backspace removes one character,
   * Escape preserves published URL; capability loss/withdrawal cancel editing. */
  SDL_StopTextInput();
  SDL_Event click = {.button = {.type = SDL_MOUSEBUTTONDOWN, .button = SDL_BUTTON_LEFT, .x = 100, .y = 10}};
  assert(viewer_navigation_event(&a.navigation, NULL, &click) == 1); assert(a.navigation.editing && a.navigation.selected && SDL_IsTextInputActive());
  SDL_Event text = {.text = {.type = SDL_TEXTINPUT, .text = "a\xc3\xa9"}};
  assert(viewer_navigation_event(&a.navigation, NULL, &text) == 1); assert(!strcmp(a.navigation.edit, "a\xc3\xa9"));
  SDL_Event key = {.key = {.type = SDL_KEYDOWN, .keysym = {.sym = SDLK_BACKSPACE}}};
  assert(viewer_navigation_event(&a.navigation, NULL, &key) == 1); assert(!strcmp(a.navigation.edit, "a"));
  key.key.keysym.sym = SDLK_ESCAPE;
  assert(viewer_navigation_event(&a.navigation, NULL, &key) == 1); assert(!a.navigation.editing && !SDL_IsTextInputActive());
  assert(!strcmp(a.navigation.url, "https://example.test"));
  /* Escape keyup and toolbar mouseup outside the strip remain captured. */
  key.type = SDL_KEYUP; assert(viewer_navigation_event(&a.navigation, NULL, &key) == 1);
  SDL_Event release = click; release.type = SDL_MOUSEBUTTONUP; release.button.y = 200;
  assert(viewer_navigation_event(&a.navigation, NULL, &release) == 1);
  viewer_navigation_event(&a.navigation, NULL, &click);
  s.navigation.capabilities = 0; viewer_affordances_snapshot(&a, &s); assert(!a.navigation.editing && !SDL_IsTextInputActive());
  s.navigation.capabilities = 16; viewer_affordances_snapshot(&a, &s);
  viewer_navigation_event(&a.navigation, NULL, &click);
  s.withdrawn = 1; viewer_affordances_snapshot(&a, &s);
  assert(!a.navigation.editing && !a.navigation.visible && !viewer_navigation_event(&a.navigation, NULL, &click));
  /* Software rendering also supports windows too narrow for a URL field. */
  s.withdrawn = 0; viewer_affordances_snapshot(&a, &s);
  assert(!viewer_navigation_draw(&a.navigation, renderer, window));
  SDL_SetWindowSize(window, 20, 240); SDL_PumpEvents(); assert(!SDL_RenderClear(renderer));
  assert(!viewer_navigation_draw(&a.navigation, renderer, window));
  /* A 1.25x drawable target simulates fractional DPI. The strip fills exactly
   * 35 device rows, and the first frame row below it remains untouched. */
  SDL_SetWindowSize(window, 320, 240); SDL_PumpEvents();
  SDL_Texture *target = SDL_CreateTexture(renderer, SDL_PIXELFORMAT_RGBA32, SDL_TEXTUREACCESS_TARGET, 400, 300); assert(target);
  assert(!SDL_SetRenderTarget(renderer, target));
  assert(!SDL_SetRenderDrawColor(renderer, 0, 0, 0, 255)); assert(!SDL_RenderClear(renderer));
  assert(!viewer_navigation_draw(&a.navigation, renderer, window));
  unsigned char pixels[8]; SDL_Rect boundary = {10, 34, 1, 2};
  assert(!SDL_RenderReadPixels(renderer, &boundary, SDL_PIXELFORMAT_RGBA32, pixels, 4));
  assert(pixels[0] == 35 && pixels[1] == 38 && pixels[2] == 42);
  assert(pixels[4] == 0 && pixels[5] == 0 && pixels[6] == 0);
  assert(!SDL_SetRenderTarget(renderer, NULL)); SDL_DestroyTexture(target);
  /* Existing frame text-input ownership survives an editor cancellation. */
  SDL_StartTextInput(); viewer_navigation_event(&a.navigation, NULL, &click);
  key.type = SDL_KEYDOWN; key.key.keysym.sym = SDLK_ESCAPE;
  viewer_navigation_event(&a.navigation, NULL, &key); assert(SDL_IsTextInputActive());
  SDL_StopTextInput();
  assert(!viewer_affordances_close(&a, 0));
  SDL_DestroyRenderer(renderer); SDL_DestroyWindow(window); SDL_Quit(); return 0;
}
