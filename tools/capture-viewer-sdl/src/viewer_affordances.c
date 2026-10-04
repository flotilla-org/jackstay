#include "viewer_affordances.h"
#include "viewer_fit.h"
#include <SDL.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <limits.h>

/* Event tags are documented in jackstay_affordances.h, without macros. */
enum { AFF_EVENT_SNAPSHOT = 1, AFF_EVENT_CLOSED = 3 };

/* Indexed by the public v1 CSS cursor tags; NONE uses visibility separately. */
SDL_SystemCursor viewer_cursor_shape(uint32_t tag) {
  static const SDL_SystemCursor shapes[] = {
    [FT_AFF_CURSOR_AUTO] = SDL_SYSTEM_CURSOR_ARROW,
    [FT_AFF_CURSOR_DEFAULT] = SDL_SYSTEM_CURSOR_ARROW,
    [FT_AFF_CURSOR_NONE] = SDL_SYSTEM_CURSOR_ARROW,
    [FT_AFF_CURSOR_CONTEXT_MENU] = SDL_SYSTEM_CURSOR_ARROW,
    [FT_AFF_CURSOR_HELP] = SDL_SYSTEM_CURSOR_ARROW,
    [FT_AFF_CURSOR_POINTER] = SDL_SYSTEM_CURSOR_HAND,
    [FT_AFF_CURSOR_PROGRESS] = SDL_SYSTEM_CURSOR_WAITARROW,
    [FT_AFF_CURSOR_WAIT] = SDL_SYSTEM_CURSOR_WAIT,
    [FT_AFF_CURSOR_CELL] = SDL_SYSTEM_CURSOR_CROSSHAIR,
    [FT_AFF_CURSOR_CROSSHAIR] = SDL_SYSTEM_CURSOR_CROSSHAIR,
    [FT_AFF_CURSOR_TEXT] = SDL_SYSTEM_CURSOR_IBEAM,
    [FT_AFF_CURSOR_VERTICAL_TEXT] = SDL_SYSTEM_CURSOR_IBEAM,
    [FT_AFF_CURSOR_ALIAS] = SDL_SYSTEM_CURSOR_HAND,
    [FT_AFF_CURSOR_COPY] = SDL_SYSTEM_CURSOR_ARROW,
    [FT_AFF_CURSOR_MOVE] = SDL_SYSTEM_CURSOR_SIZEALL,
    [FT_AFF_CURSOR_NO_DROP] = SDL_SYSTEM_CURSOR_NO,
    [FT_AFF_CURSOR_NOT_ALLOWED] = SDL_SYSTEM_CURSOR_NO,
    [FT_AFF_CURSOR_GRAB] = SDL_SYSTEM_CURSOR_SIZEALL,
    [FT_AFF_CURSOR_GRABBING] = SDL_SYSTEM_CURSOR_SIZEALL,
    [FT_AFF_CURSOR_E_RESIZE] = SDL_SYSTEM_CURSOR_SIZEWE,
    [FT_AFF_CURSOR_N_RESIZE] = SDL_SYSTEM_CURSOR_SIZENS,
    [FT_AFF_CURSOR_NE_RESIZE] = SDL_SYSTEM_CURSOR_SIZENESW,
    [FT_AFF_CURSOR_NW_RESIZE] = SDL_SYSTEM_CURSOR_SIZENWSE,
    [FT_AFF_CURSOR_S_RESIZE] = SDL_SYSTEM_CURSOR_SIZENS,
    [FT_AFF_CURSOR_SE_RESIZE] = SDL_SYSTEM_CURSOR_SIZENWSE,
    [FT_AFF_CURSOR_SW_RESIZE] = SDL_SYSTEM_CURSOR_SIZENESW,
    [FT_AFF_CURSOR_W_RESIZE] = SDL_SYSTEM_CURSOR_SIZEWE,
    [FT_AFF_CURSOR_EW_RESIZE] = SDL_SYSTEM_CURSOR_SIZEWE,
    [FT_AFF_CURSOR_NS_RESIZE] = SDL_SYSTEM_CURSOR_SIZENS,
    [FT_AFF_CURSOR_NESW_RESIZE] = SDL_SYSTEM_CURSOR_SIZENESW,
    [FT_AFF_CURSOR_NWSE_RESIZE] = SDL_SYSTEM_CURSOR_SIZENWSE,
    [FT_AFF_CURSOR_COL_RESIZE] = SDL_SYSTEM_CURSOR_SIZEWE,
    [FT_AFF_CURSOR_ROW_RESIZE] = SDL_SYSTEM_CURSOR_SIZENS,
    [FT_AFF_CURSOR_ALL_SCROLL] = SDL_SYSTEM_CURSOR_SIZEALL,
    [FT_AFF_CURSOR_ZOOM_IN] = SDL_SYSTEM_CURSOR_ARROW,
    [FT_AFF_CURSOR_ZOOM_OUT] = SDL_SYSTEM_CURSOR_ARROW,
  };
  return tag < sizeof(shapes) / sizeof(shapes[0]) ? shapes[tag] : SDL_SYSTEM_CURSOR_ARROW;
}
static void cursor_apply(viewer_affordances *a, uint32_t tag) {
  SDL_Cursor *cursor = a->cursors[viewer_cursor_shape(tag)];
  SDL_SetCursor(cursor ? cursor : SDL_GetDefaultCursor());
  SDL_ShowCursor(tag == FT_AFF_CURSOR_NONE ? SDL_DISABLE : SDL_ENABLE);
}
void viewer_affordances_cursor_init(viewer_affordances *a) {
  for (int i = 0; i < SDL_NUM_SYSTEM_CURSORS; ++i)
    a->cursors[i] = SDL_CreateSystemCursor((SDL_SystemCursor)i);
}
void viewer_affordances_cursor_update(viewer_affordances *a) {
  int x, y, w, h, dw, dh; double fx, fy;
  uint32_t tag = FT_AFF_CURSOR_DEFAULT;
  if (a->window && !a->closed && SDL_GetMouseFocus() == a->window) {
    SDL_GetMouseState(&x, &y); SDL_GetWindowSize(a->window, &w, &h);
    if (w > 0 && h > 0 && !SDL_GetRendererOutputSize(a->renderer, &dw, &dh) &&
        viewer_map(viewer_fit(dw, dh, a->frame_width, a->frame_height),
                   (double)x * dw / w, (double)y * dh / h, 1, 1, &fx, &fy)) tag = a->cursor;
  }
  cursor_apply(a, tag);
}
static void log_snapshot(const ft_aff_snapshot *s) {
  static const char *names[] = {"unknown", "media", "navigation", "cursor", "scroll", "window", "presentation"};
  fprintf(stderr, "affordances domain=%s withdrawn=%u", s->domain <= FT_AFF_DOMAIN_PRESENTATION ? names[s->domain] : names[0], s->withdrawn);
  if (!s->withdrawn) switch (s->domain) {
    case FT_AFF_DOMAIN_MEDIA:
      fprintf(stderr, " status=%u rate=%g capabilities=%u", s->media.status, s->media.rate, s->media.capabilities); break;
    case FT_AFF_DOMAIN_NAVIGATION:
      fprintf(stderr, " back=%u forward=%u loading=%u capabilities=%u", s->navigation.can_go_back,
              s->navigation.can_go_forward, s->navigation.loading, s->navigation.capabilities); break;
    case FT_AFF_DOMAIN_CURSOR: fprintf(stderr, " cursor=%u", s->cursor); break;
    case FT_AFF_DOMAIN_SCROLL:
      fprintf(stderr, " x=%g/%g y=%g/%g capabilities=%u", s->scroll.x.position, s->scroll.x.content_length,
              s->scroll.y.position, s->scroll.y.content_length, s->scroll.capabilities); break;
    case FT_AFF_DOMAIN_WINDOW:
      fprintf(stderr, " ready=%u title=", s->window.ready);
      if (s->window.title.present) fwrite(s->window.title.value.data, 1, s->window.title.value.len, stderr);
      else fputs("null", stderr);
      break;
    default: break;
  }
  fputc('\n', stderr);
}
static void copy_title(char **to, ft_aff_optional_string from) {
  free(*to); *to = NULL;
  if (from.present) {
    *to = malloc(from.value.len + 1);
    if (*to) { memcpy(*to, from.value.data, from.value.len); (*to)[from.value.len] = 0; }
  }
}
void viewer_affordances_snapshot(viewer_affordances *a, const ft_aff_snapshot *s) {
  if (!a->window) return;
  if (s->domain == FT_AFF_DOMAIN_CURSOR) {
    a->cursor = s->withdrawn ? FT_AFF_CURSOR_DEFAULT : s->cursor;
    viewer_affordances_cursor_update(a);
  }
  if (s->domain == FT_AFF_DOMAIN_WINDOW) {
    a->has_window = !s->withdrawn;
    a->ready = !s->withdrawn && s->window.ready;
    copy_title(&a->title, s->withdrawn ? (ft_aff_optional_string){0} : s->window.title);
    ft_aff_size size = s->window.requested_size;
    if (!s->withdrawn && size.present && !a->user_resized &&
        size.width >= 1 && size.height >= 1 && size.width <= INT_MAX && size.height <= INT_MAX) {
      int w, h; SDL_GetWindowSize(a->window, &w, &h);
      if (w != (int)size.width || h != (int)size.height) {
        a->requested_width = (int)size.width; a->requested_height = (int)size.height;
        SDL_SetWindowSize(a->window, a->requested_width, a->requested_height);
        a->dirty = 1;
      }
    }
  } else if (s->domain == FT_AFF_DOMAIN_NAVIGATION) {
    copy_title(&a->navigation_title, s->withdrawn ? (ft_aff_optional_string){0} : s->navigation.title);
    copy_title(&a->url, s->withdrawn ? (ft_aff_optional_string){0} : s->navigation.url);
  }
  SDL_SetWindowTitle(a->window, a->title ? a->title : a->navigation_title ? a->navigation_title :
                     a->url ? a->url : "capture-viewer-sdl");
}
void viewer_affordances_event(viewer_affordances *a, const SDL_Event *event) {
  if (!a->window || event->type != SDL_WINDOWEVENT) return;
  switch (event->window.event) {
    case SDL_WINDOWEVENT_RESIZED:
      /* Some window managers acknowledge a requested size with RESIZED.
       * Only a different size establishes user ownership of the window size. */
      if (event->window.data1 != a->requested_width || event->window.data2 != a->requested_height)
        a->user_resized = 1;
      /* fall through */
    case SDL_WINDOWEVENT_SIZE_CHANGED:
      a->resized_at = SDL_GetTicks(); a->dirty = 1; break;
    case SDL_WINDOWEVENT_MINIMIZED: case SDL_WINDOWEVENT_HIDDEN: a->visible = 0; a->dirty = 1; break;
    case SDL_WINDOWEVENT_RESTORED: case SDL_WINDOWEVENT_SHOWN: a->visible = 1; a->dirty = 1; break;
    case SDL_WINDOWEVENT_MOVED:
#if SDL_VERSION_ATLEAST(2, 0, 18)
    case SDL_WINDOWEVENT_DISPLAY_CHANGED:
#endif
      a->dirty = 1; break;
    case SDL_WINDOWEVENT_FOCUS_GAINED: a->focused = 1; a->dirty = 1; break;
    case SDL_WINDOWEVENT_FOCUS_LOST: a->focused = 0; a->dirty = 1; break;
    default: break;
  }
}
int viewer_affordances_tick(viewer_affordances *a) {
  if (!a->window) return 0;
  viewer_affordances_cursor_update(a);
  Uint32 now = SDL_GetTicks();
  if (!a->shown && (!a->has_window || a->ready || now - a->started >= 2000)) {
    SDL_ShowWindow(a->window); a->shown = 1; a->visible = 1; a->dirty = 1;
  }
  if (!a->host || a->closed || !a->dirty || (a->resized_at && now - a->resized_at < 100)) return 0;
  int w, h, dw, dh; SDL_GetWindowSize(a->window, &w, &h);
  if (SDL_GetRendererOutputSize(a->renderer, &dw, &dh) || w <= 0 || h <= 0 || dw <= 0 || dh <= 0) return 0;
  ft_aff_snapshot s = {.domain = FT_AFF_DOMAIN_PRESENTATION,
    .presentation = {.visible = a->visible, .focused = a->focused,
      .preferred_size = {.present = 1, .width = w, .height = h}, .scale = (double)dw / w}};
  if (ft_affordances_host_publish(a->host, &s) != FT_STATUS_OK) return 1;
  a->dirty = 0; return 0;
}
int viewer_affordances_poll(viewer_affordances *a, int log_snapshots) {
  if (!a->host || a->closed) return 0;
  ft_affordances_event *event = NULL;
  ft_status status;
  while ((status = ft_affordances_host_poll(a->host, &event)) == FT_STATUS_OK) {
    ft_aff_event_view view = {0};
    ft_status described = ft_affordances_event_view(event, &view);
    if (described == FT_STATUS_OK) {
      if (view.kind == AFF_EVENT_SNAPSHOT) {
        viewer_affordances_snapshot(a, &view.snapshot);
        if (log_snapshots) log_snapshot(&view.snapshot);
      }
      if (view.kind == AFF_EVENT_CLOSED) {
        a->closed = 1; a->cursor = FT_AFF_CURSOR_DEFAULT; cursor_apply(a, a->cursor);
      }
    }
    ft_affordances_event_destroy(&event);
    if (described != FT_STATUS_OK) { fprintf(stderr, "affordances event: %d\n", described); return 1; }
  }
  if (status != FT_STATUS_EMPTY) { fprintf(stderr, "affordances poll: %d\n", status); return 1; }
  return 0;
}
int viewer_affordances_close(viewer_affordances *a, int log_snapshots) {
  a->cursor = FT_AFF_CURSOR_DEFAULT;
  SDL_SetCursor(SDL_GetDefaultCursor()); SDL_ShowCursor(SDL_ENABLE);
  for (int i = 0; i < SDL_NUM_SYSTEM_CURSORS; ++i) {
    SDL_FreeCursor(a->cursors[i]); a->cursors[i] = NULL;
  }
  free(a->title); free(a->navigation_title); free(a->url);
  a->title = a->navigation_title = a->url = NULL; a->window = NULL;
  if (!a->host) return 0;
  /* Independent channel closure proves no input cleanup; input closes separately. */
  ft_affordances_host_close(a->host);
  uint32_t start = SDL_GetTicks();
  int failed = 0;
  while (!a->closed && SDL_GetTicks() - start < 3000) {
    if (viewer_affordances_poll(a, log_snapshots)) { failed = 1; break; }
    if (!a->closed) SDL_Delay(2);
  }
  ft_affordances_host_destroy(&a->host);
  printf("affordances_cleanup=%s\n", a->closed ? "completed" : "unconfirmed");
  return failed || !a->closed;
}
