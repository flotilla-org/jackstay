#include "viewer_affordances.h"
#ifdef NDEBUG
#undef NDEBUG
#endif
#include <assert.h>
#include <stdint.h>
/* SDL cursor and pointer APIs are the OS boundary; dummy SDL has no cursors. */
static SDL_Window *focus;
static int mx, my, shown, created, freed, sets, shows;
static SDL_Cursor *selected;
static SDL_Cursor *create_cursor(SDL_SystemCursor id) { ++created; return (SDL_Cursor *)(uintptr_t)(id + 1); }
static void free_cursor(SDL_Cursor *cursor) { if (cursor) ++freed; }
static void set_cursor(SDL_Cursor *cursor) { selected = cursor; ++sets; }
static int show_cursor(int toggle) { shown = toggle; ++shows; return toggle; }
static SDL_Window *mouse_focus(void) { return focus; }
static Uint32 mouse_state(int *x, int *y) { *x = mx; *y = my; return 0; }
#define SDL_CreateSystemCursor create_cursor
#define SDL_FreeCursor free_cursor
#define SDL_SetCursor set_cursor
#define SDL_ShowCursor show_cursor
#define SDL_GetMouseFocus mouse_focus
#define SDL_GetMouseState mouse_state
#include "../src/viewer_affordances.c"

int main(void) {
  /* Issue #61: every v1 CSS name maps to the nearest system shape, unknown to arrow.
   * Constant-map glue: exhaustive enumeration covers the entire finite input space. */
  const struct { uint32_t tag; SDL_SystemCursor shape; } cases[] = {
    {FT_AFF_CURSOR_AUTO, SDL_SYSTEM_CURSOR_ARROW},
    {FT_AFF_CURSOR_DEFAULT, SDL_SYSTEM_CURSOR_ARROW},
    {FT_AFF_CURSOR_NONE, SDL_SYSTEM_CURSOR_ARROW},
    {FT_AFF_CURSOR_CONTEXT_MENU, SDL_SYSTEM_CURSOR_ARROW},
    {FT_AFF_CURSOR_HELP, SDL_SYSTEM_CURSOR_ARROW},
    {FT_AFF_CURSOR_POINTER, SDL_SYSTEM_CURSOR_HAND},
    {FT_AFF_CURSOR_PROGRESS, SDL_SYSTEM_CURSOR_WAITARROW},
    {FT_AFF_CURSOR_WAIT, SDL_SYSTEM_CURSOR_WAIT},
    {FT_AFF_CURSOR_CELL, SDL_SYSTEM_CURSOR_CROSSHAIR},
    {FT_AFF_CURSOR_CROSSHAIR, SDL_SYSTEM_CURSOR_CROSSHAIR},
    {FT_AFF_CURSOR_TEXT, SDL_SYSTEM_CURSOR_IBEAM},
    {FT_AFF_CURSOR_VERTICAL_TEXT, SDL_SYSTEM_CURSOR_IBEAM},
    {FT_AFF_CURSOR_ALIAS, SDL_SYSTEM_CURSOR_HAND},
    {FT_AFF_CURSOR_COPY, SDL_SYSTEM_CURSOR_ARROW},
    {FT_AFF_CURSOR_MOVE, SDL_SYSTEM_CURSOR_SIZEALL},
    {FT_AFF_CURSOR_NO_DROP, SDL_SYSTEM_CURSOR_NO},
    {FT_AFF_CURSOR_NOT_ALLOWED, SDL_SYSTEM_CURSOR_NO},
    {FT_AFF_CURSOR_GRAB, SDL_SYSTEM_CURSOR_SIZEALL},
    {FT_AFF_CURSOR_GRABBING, SDL_SYSTEM_CURSOR_SIZEALL},
    {FT_AFF_CURSOR_E_RESIZE, SDL_SYSTEM_CURSOR_SIZEWE},
    {FT_AFF_CURSOR_N_RESIZE, SDL_SYSTEM_CURSOR_SIZENS},
    {FT_AFF_CURSOR_NE_RESIZE, SDL_SYSTEM_CURSOR_SIZENESW},
    {FT_AFF_CURSOR_NW_RESIZE, SDL_SYSTEM_CURSOR_SIZENWSE},
    {FT_AFF_CURSOR_S_RESIZE, SDL_SYSTEM_CURSOR_SIZENS},
    {FT_AFF_CURSOR_SE_RESIZE, SDL_SYSTEM_CURSOR_SIZENWSE},
    {FT_AFF_CURSOR_SW_RESIZE, SDL_SYSTEM_CURSOR_SIZENESW},
    {FT_AFF_CURSOR_W_RESIZE, SDL_SYSTEM_CURSOR_SIZEWE},
    {FT_AFF_CURSOR_EW_RESIZE, SDL_SYSTEM_CURSOR_SIZEWE},
    {FT_AFF_CURSOR_NS_RESIZE, SDL_SYSTEM_CURSOR_SIZENS},
    {FT_AFF_CURSOR_NESW_RESIZE, SDL_SYSTEM_CURSOR_SIZENESW},
    {FT_AFF_CURSOR_NWSE_RESIZE, SDL_SYSTEM_CURSOR_SIZENWSE},
    {FT_AFF_CURSOR_COL_RESIZE, SDL_SYSTEM_CURSOR_SIZEWE},
    {FT_AFF_CURSOR_ROW_RESIZE, SDL_SYSTEM_CURSOR_SIZENS},
    {FT_AFF_CURSOR_ALL_SCROLL, SDL_SYSTEM_CURSOR_SIZEALL},
    {FT_AFF_CURSOR_ZOOM_IN, SDL_SYSTEM_CURSOR_ARROW},
    {FT_AFF_CURSOR_ZOOM_OUT, SDL_SYSTEM_CURSOR_ARROW},
  };
  for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); ++i)
    assert(viewer_cursor_shape(cases[i].tag) == cases[i].shape);
  assert(viewer_cursor_shape(UINT32_MAX) == SDL_SYSTEM_CURSOR_ARROW);
  assert(SDL_Init(SDL_INIT_VIDEO) == 0);
  SDL_Window *window = SDL_CreateWindow("cursor", 0, 0, 100, 100, SDL_WINDOW_HIDDEN);
  SDL_Renderer *renderer = SDL_CreateRenderer(window, -1, SDL_RENDERER_SOFTWARE);
  assert(window && renderer);
  viewer_affordances a = {.window = window, .renderer = renderer, .frame_width = 100, .frame_height = 50};
  viewer_affordances_cursor_init(&a);
  assert(created == SDL_NUM_SYSTEM_CURSORS);
  focus = window; mx = 50; my = 50;
  /* Until a frame size is known the producer cursor must not apply. */
  a.frame_width = 0; a.cursor = FT_AFF_CURSOR_POINTER;
  viewer_affordances_cursor_update(&a);
  assert(selected == a.cursors[SDL_SYSTEM_CURSOR_ARROW]);
  a.frame_width = 100;
  /* Cursor snapshots apply inside the shared fit; letterbox and exclusive edges restore arrow. */
  ft_aff_snapshot s = {.domain = FT_AFF_DOMAIN_CURSOR, .cursor = FT_AFF_CURSOR_POINTER};
  viewer_affordances_snapshot(&a, &s);
  assert(selected == a.cursors[SDL_SYSTEM_CURSOR_HAND] && shown == SDL_ENABLE);
  /* Repeated ticks with the same shape/visibility avoid redundant OS calls. */
  int previous_sets = sets, previous_shows = shows;
  viewer_affordances_cursor_update(&a); viewer_affordances_cursor_update(&a);
  assert(sets == previous_sets && shows == previous_shows);
  const int points[][3] = {{0,25,1},{99,74,1},{50,24,0},{50,75,0},{100,50,0},{-1,50,0}};
  for (size_t i = 0; i < sizeof(points)/sizeof(points[0]); ++i) {
    mx = points[i][0]; my = points[i][1]; viewer_affordances_cursor_update(&a);
    assert(selected == a.cursors[points[i][2] ? SDL_SYSTEM_CURSOR_HAND : SDL_SYSTEM_CURSOR_ARROW]);
  }
  mx = my = 50; s.cursor = FT_AFF_CURSOR_NONE; viewer_affordances_snapshot(&a, &s);
  assert(shown == SDL_DISABLE);
  focus = NULL; viewer_affordances_cursor_update(&a); assert(shown == SDL_ENABLE);
  focus = window; viewer_affordances_cursor_update(&a); assert(shown == SDL_DISABLE);
  /* Withdrawal and channel closure restore visibility even without pointer motion. */
  s.withdrawn = 1; viewer_affordances_snapshot(&a, &s);
  assert(shown == SDL_ENABLE && selected == a.cursors[SDL_SYSTEM_CURSOR_ARROW]);
  s.withdrawn = 0; viewer_affordances_snapshot(&a, &s); assert(shown == SDL_DISABLE);
  a.closed = 1; viewer_affordances_cursor_update(&a);
  assert(shown == SDL_ENABLE && selected == a.cursors[SDL_SYSTEM_CURSOR_ARROW]);
  viewer_affordances_close(&a, 0); assert(freed == created);
  assert(!a.cursor_applied && a.applied_cursor == NULL && shown == SDL_ENABLE);
  SDL_DestroyRenderer(renderer); SDL_DestroyWindow(window); SDL_Quit();
  return 0;
}
