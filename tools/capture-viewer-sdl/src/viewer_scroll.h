#ifndef VIEWER_SCROLL_H
#define VIEWER_SCROLL_H
#include "jackstay_affordances.h"
#include <SDL.h>
/* Geometry stays in drawable pixels, positions stay in producer units. */
typedef struct {
  SDL_Rect track;
  double start, length, thumb_start, thumb_length;
} viewer_scroll_geometry;
typedef struct {
  ft_aff_scroll snapshot;
  int present, hovered, changed, owned, dragging, pending, pending_axis;
  Uint32 changed_at;
  double grab, position;
} viewer_scroll;
int viewer_scroll_geometry_for(ft_aff_axis axis, SDL_Rect frame, int vertical, int thickness, viewer_scroll_geometry *g);
double viewer_scroll_position(ft_aff_axis axis, viewer_scroll_geometry g, double pointer, double grab);
void viewer_scroll_snapshot(viewer_scroll *s, const ft_aff_scroll *snapshot, Uint32 now);
int viewer_scroll_visible(const viewer_scroll *s, Uint32 now);
/* Returns consumed. A content drag already in progress cannot become a scrollbar drag. */
int viewer_scroll_event(viewer_scroll *s, ft_affordances_host *host, const SDL_Event *e,
                        SDL_Rect frame, double scale_x, double scale_y, int content_drag, Uint32 now);
int viewer_scroll_draw(viewer_scroll *s, ft_affordances_host *host, SDL_Renderer *renderer,
                       SDL_Rect frame, int thickness, Uint32 now);
#endif
