#include "viewer_affordances.h"
#include <SDL.h>
#include <stdio.h>

static void log_snapshot(const ft_aff_snapshot *s) {
  static const char *names[] = {"unknown", "media", "navigation", "cursor", "scroll", "window", "presentation"};
  fprintf(stderr, "affordances domain=%s withdrawn=%u", s->domain <= 6 ? names[s->domain] : names[0], s->withdrawn);
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
int viewer_affordances_poll(viewer_affordances *a, int log_snapshots) {
  if (!a->host || a->closed) return 0;
  ft_affordances_event *event = NULL;
  ft_status status;
  while ((status = ft_affordances_host_poll(a->host, &event)) == FT_STATUS_OK) {
    ft_aff_event_view view = {0};
    ft_status described = ft_affordances_event_view(event, &view);
    if (described == FT_STATUS_OK) {
      if (view.kind == 1 && log_snapshots) log_snapshot(&view.snapshot);
      if (view.kind == 3) a->closed = 1;
    }
    ft_affordances_event_destroy(&event);
    if (described != FT_STATUS_OK) { fprintf(stderr, "affordances event: %d\n", described); return 1; }
  }
  if (status != FT_STATUS_EMPTY) { fprintf(stderr, "affordances poll: %d\n", status); return 1; }
  return 0;
}
int viewer_affordances_close(viewer_affordances *a) {
  if (!a->host) return 0;
  /* Independent channel closure proves no input cleanup; input closes separately. */
  ft_affordances_host_close(a->host);
  uint32_t start = SDL_GetTicks();
  int failed = 0;
  while (!a->closed && SDL_GetTicks() - start < 3000) {
    if (viewer_affordances_poll(a, 0)) { failed = 1; break; }
    if (!a->closed) SDL_Delay(2);
  }
  ft_affordances_host_destroy(&a->host);
  printf("affordances_cleanup=%s\n", a->closed ? "completed" : "unconfirmed");
  return failed || !a->closed;
}
