#ifndef VIEWER_FIT_H
#define VIEWER_FIT_H
#include <SDL.h>
/* Rendering and input share the same integer fit rectangle at drawable resolution. */
static inline SDL_Rect viewer_fit(int w, int h, int fw, int fh) {
  SDL_Rect r = {0};
  if (w <= 0 || h <= 0 || fw <= 0 || fh <= 0) return r;
  if ((double)w / fw < (double)h / fh) { r.w = w; r.h = (int)((double)fh * w / fw); }
  else { r.h = h; r.w = (int)((double)fw * h / fh); }
  r.x = (w - r.w) / 2; r.y = (h - r.h) / 2; return r;
}
static inline int viewer_map(SDL_Rect r, double x, double y, double gw, double gh, double *out_x, double *out_y) {
  if (r.w <= 0 || r.h <= 0 || x < r.x || y < r.y || x >= r.x + r.w || y >= r.y + r.h) return 0;
  *out_x = (x - r.x) * gw / r.w; *out_y = (y - r.y) * gh / r.h; return 1;
}
#endif
