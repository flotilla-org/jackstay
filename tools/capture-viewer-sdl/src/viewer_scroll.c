#include "viewer_scroll.h"
#include <math.h>
#include <string.h>
int viewer_scroll_thickness(double drawable_scale) { return (int)ceil(8.0 * drawable_scale); }
static double clamp(double value, double max) { return fmax(0, fmin(value, max)); }
int viewer_scroll_geometry_for(ft_aff_axis a, SDL_Rect frame, int vertical, int thickness, viewer_scroll_geometry *g) {
  memset(g, 0, sizeof(*g));
  if (!a.scrollable || !isfinite(a.content_length) || !isfinite(a.viewport_length) ||
      !isfinite(a.position) || a.viewport_length < 0 || a.content_length <= a.viewport_length ||
      frame.w <= 0 || frame.h <= 0 || thickness <= 0) return 0;
  g->track = frame;
  if (vertical) { g->track.w = thickness < frame.w ? thickness : frame.w; g->track.x += frame.w - g->track.w; }
  else { g->track.h = thickness < frame.h ? thickness : frame.h; g->track.y += frame.h - g->track.h; }
  g->start = vertical ? frame.y : frame.x;
  g->length = vertical ? frame.h : frame.w;
  g->thumb_length = g->length * (a.viewport_length / a.content_length);
  g->thumb_start = g->start + (g->length - g->thumb_length) *
    (clamp(a.position, a.content_length - a.viewport_length) / (a.content_length - a.viewport_length));
  return 1;
}
double viewer_scroll_position(ft_aff_axis a, viewer_scroll_geometry g, double pointer, double grab) {
  double travel = g.length - g.thumb_length;
  if (travel <= 0 || a.content_length <= a.viewport_length || !a.scrollable) return 0;
  return clamp((pointer - grab - g.start) / travel, 1) * (a.content_length - a.viewport_length);
}
void viewer_scroll_snapshot(viewer_scroll *s, const ft_aff_scroll *snapshot, Uint32 now) {
  if (!snapshot) {
    s->present = s->pending = s->dragging = s->changed = 0;
    memset(&s->snapshot, 0, sizeof(s->snapshot));
    /* Keep ownership until release: withdrawal must not leak half a gesture. */
    return;
  }
  if (!s->present || snapshot->x.position != s->snapshot.x.position || snapshot->y.position != s->snapshot.y.position) {
    s->changed = 1; s->changed_at = now;
  }
  s->snapshot = *snapshot; s->present = 1;
  int axis = s->dragging ? s->dragging - 1 : s->pending_axis;
  ft_aff_axis a = axis == 0 ? snapshot->x : snapshot->y;
  if (!(snapshot->capabilities & FT_AFF_SCROLL_SET_POSITION) || !a.scrollable || a.content_length <= a.viewport_length) {
    s->pending = s->dragging = 0;
  }
}
int viewer_scroll_visible(const viewer_scroll *s, Uint32 now) {
  return s->present && (s->hovered || s->dragging || (s->changed && now - s->changed_at < 1000));
}
static int contains(SDL_Rect r, double x, double y) {
  return x >= r.x && y >= r.y && x < r.x + r.w && y < r.y + r.h;
}
int viewer_scroll_event(viewer_scroll *s, ft_affordances_host *host, const SDL_Event *e,
                        SDL_Rect frame, double sx, double sy, int content_drag, Uint32 now) {
  if (e->type == SDL_WINDOWEVENT && (e->window.event == SDL_WINDOWEVENT_LEAVE ||
      e->window.event == SDL_WINDOWEVENT_FOCUS_LOST)) {
    s->hovered = 0;
    if (e->window.event == SDL_WINDOWEVENT_FOCUS_LOST) {
      s->cancelled_button = s->owned; s->owned = s->pending = s->dragging = 0;
      SDL_CaptureMouse(SDL_FALSE);
    }
    return 0;
  }
  if (e->type != SDL_MOUSEMOTION && e->type != SDL_MOUSEBUTTONDOWN && e->type != SDL_MOUSEBUTTONUP) return 0;
  double x = (e->type == SDL_MOUSEMOTION ? e->motion.x : e->button.x) * sx;
  double y = (e->type == SDL_MOUSEMOTION ? e->motion.y : e->button.y) * sy;
  /* Remember an abandoned release without blocking subsequent motion. A new
   * down of that button starts a fresh gesture and makes the tombstone obsolete. */
  if (e->type == SDL_MOUSEBUTTONDOWN && e->button.button == s->cancelled_button)
    s->cancelled_button = 0;
  if (!s->owned && e->type == SDL_MOUSEBUTTONUP && e->button.button == s->cancelled_button) {
    s->cancelled_button = 0; return 1;
  }
  if (s->owned && e->type == SDL_MOUSEMOTION && !(e->motion.state & SDL_BUTTON(s->owned))) {
    s->cancelled_button = s->owned; s->owned = s->pending = s->dragging = 0;
    SDL_CaptureMouse(SDL_FALSE);
  }
  s->hovered = contains(frame, x, y);
  if (s->owned) {
    if (s->dragging && (e->type == SDL_MOUSEMOTION ||
        (e->type == SDL_MOUSEBUTTONUP && e->button.button == SDL_BUTTON_LEFT))) {
      int vertical = s->dragging == 2;
      ft_aff_axis a = vertical ? s->snapshot.y : s->snapshot.x;
      viewer_scroll_geometry g;
      if (viewer_scroll_geometry_for(a, frame, vertical, viewer_scroll_thickness(sx), &g)) {
        s->position = viewer_scroll_position(a, g, vertical ? y : x, s->grab);
        s->pending = 1; s->pending_axis = vertical;
      }
    }
    if (e->type == SDL_MOUSEBUTTONUP && e->button.button == s->owned) {
      s->owned = 0; s->dragging = 0; SDL_CaptureMouse(SDL_FALSE);
    }
    return 1;
  }
  if (content_drag || !viewer_scroll_visible(s, now)) return 0;
  /* Vertical owns the shared corner. Button releases without a host-owned down
   * must pass through so producer-held buttons can always be released. */
  for (int vertical = 1; vertical >= 0; vertical--) {
    ft_aff_axis a = vertical ? s->snapshot.y : s->snapshot.x;
    viewer_scroll_geometry g;
    if (!viewer_scroll_geometry_for(a, frame, vertical, viewer_scroll_thickness(sx), &g) || !contains(g.track, x, y)) continue;
    if (e->type == SDL_MOUSEBUTTONUP) return 0;
    if (e->type == SDL_MOUSEBUTTONDOWN) {
      double pointer = vertical ? y : x;
      s->owned = e->button.button; SDL_CaptureMouse(SDL_TRUE);
      if (e->button.button != SDL_BUTTON_LEFT) return 1;
      if (pointer >= g.thumb_start && pointer < g.thumb_start + g.thumb_length) {
        if (s->snapshot.capabilities & FT_AFF_SCROLL_SET_POSITION) {
          s->dragging = vertical + 1; s->grab = pointer - g.thumb_start;
        }
      } else if (host && (s->snapshot.capabilities & FT_AFF_SCROLL_SCROLL_BY_STEP)) {
        ft_aff_verb verb = {.domain = FT_AFF_DOMAIN_SCROLL, .verb = FT_AFF_SCROLL_SCROLL_BY_STEP,
          .axis = vertical, .step = 1, .direction = pointer >= g.thumb_start + g.thumb_length};
        if (ft_affordances_host_send(host, &verb) != FT_STATUS_OK) return -1;
      }
    }
    return 1;
  }
  return 0;
}
int viewer_scroll_draw(viewer_scroll *s, ft_affordances_host *host, SDL_Renderer *renderer,
                       SDL_Rect frame, int thickness, Uint32 now) {
  /* Called once per presented frame: intermediate motion only replaces position. */
  if (s->pending) {
    s->pending = 0;
    if (host && s->present && (s->snapshot.capabilities & FT_AFF_SCROLL_SET_POSITION)) {
      ft_aff_verb verb = {.domain = FT_AFF_DOMAIN_SCROLL, .verb = FT_AFF_SCROLL_SET_POSITION,
        .axis = s->pending_axis, .number = s->position};
      if (ft_affordances_host_send(host, &verb) != FT_STATUS_OK) return 1;
    }
  }
  if (!viewer_scroll_visible(s, now)) return 0;
  Uint8 r, g, b, a; SDL_BlendMode blend;
  SDL_GetRenderDrawColor(renderer, &r, &g, &b, &a); SDL_GetRenderDrawBlendMode(renderer, &blend);
  SDL_SetRenderDrawBlendMode(renderer, SDL_BLENDMODE_BLEND);
  int failed = 0;
  for (int vertical = 0; vertical <= 1; vertical++) {
    viewer_scroll_geometry geometry;
    if (!viewer_scroll_geometry_for(vertical ? s->snapshot.y : s->snapshot.x, frame, vertical, thickness, &geometry)) continue;
    SDL_SetRenderDrawColor(renderer, 24, 24, 24, 80);
    failed |= SDL_RenderFillRect(renderer, &geometry.track) != 0;
    SDL_FRect thumb = {(float)geometry.track.x, (float)geometry.track.y, (float)geometry.track.w, (float)geometry.track.h};
    if (vertical) { thumb.y = (float)geometry.thumb_start; thumb.h = (float)geometry.thumb_length; }
    else { thumb.x = (float)geometry.thumb_start; thumb.w = (float)geometry.thumb_length; }
    SDL_SetRenderDrawColor(renderer, 220, 220, 220, 190);
    failed |= SDL_RenderFillRectF(renderer, &thumb) != 0;
  }
  SDL_SetRenderDrawBlendMode(renderer, blend); SDL_SetRenderDrawColor(renderer, r, g, b, a);
  return failed;
}
