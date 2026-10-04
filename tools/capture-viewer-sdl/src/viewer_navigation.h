#ifndef VIEWER_NAVIGATION_H
#define VIEWER_NAVIGATION_H
#include "jackstay_affordances.h"
#include <SDL.h>
#define VIEWER_NAV_HEIGHT 28
#define VIEWER_NAV_BUTTON 28
typedef struct {
  int visible, editing, selected, restore_text_input;
  uint32_t buttons; uint8_t keys[SDL_NUM_SCANCODES];
  ft_aff_navigation state;
  char *url, *edit;
} viewer_navigation;
int viewer_navigation_enabled(const viewer_navigation *n, uint32_t verb);
void viewer_navigation_snapshot(viewer_navigation *n, const ft_aff_navigation *state);
void viewer_navigation_destroy(viewer_navigation *n);
/* Reserve every device row intersecting the logical strip, including a
 * fractional boundary row. Rendering and input use the same ceiling. */
static inline int viewer_navigation_strip_height(int drawable_h, int window_h, int visible) {
  if (!visible || drawable_h <= 0 || window_h <= 0) return 0;
  return (int)(((int64_t)drawable_h * VIEWER_NAV_HEIGHT + window_h - 1) / window_h);
}
SDL_Rect viewer_navigation_fit(int w, int h, int fw, int fh, int strip);
int viewer_navigation_event(viewer_navigation *n, ft_affordances_host *host, const SDL_Event *event);
int viewer_navigation_draw(const viewer_navigation *n, SDL_Renderer *renderer, SDL_Window *window);
#endif
