#ifndef VIEWER_AFFORDANCES_H
#define VIEWER_AFFORDANCES_H
#include "jackstay_affordances.h"
#include <SDL.h>
typedef struct {
  ft_affordances_host *host; int closed;
  SDL_Window *window; SDL_Renderer *renderer;
  Uint32 started, resized_at; int shown, user_resized;
  int has_window, ready, dirty, visible, focused;
  char *title, *navigation_title, *url;
} viewer_affordances;
void viewer_affordances_snapshot(viewer_affordances *a, const ft_aff_snapshot *snapshot);
void viewer_affordances_event(viewer_affordances *a, const SDL_Event *event);
int viewer_affordances_tick(viewer_affordances *a);
int viewer_affordances_poll(viewer_affordances *affordances, int log_snapshots);
int viewer_affordances_close(viewer_affordances *affordances, int log_snapshots);
#endif
