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
SDL_Rect viewer_navigation_fit(int w, int h, int fw, int fh, int strip);
int viewer_navigation_event(viewer_navigation *n, ft_affordances_host *host, const SDL_Event *event);
int viewer_navigation_draw(const viewer_navigation *n, SDL_Renderer *renderer, SDL_Window *window);
#endif
