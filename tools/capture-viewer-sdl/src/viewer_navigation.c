#include "viewer_navigation.h"
#include "viewer_fit.h"
#include <stdlib.h>
#include <string.h>

int viewer_navigation_enabled(const viewer_navigation *n, uint32_t verb) {
  if (!n->visible || verb < 1 || verb > 5 || !(n->state.capabilities & (1u << (verb - 1)))) return 0;
  switch (verb) {
    case FT_AFF_NAVIGATION_BACK: return !!n->state.can_go_back;
    case FT_AFF_NAVIGATION_FORWARD: return !!n->state.can_go_forward;
    case FT_AFF_NAVIGATION_RELOAD: return !n->state.loading;
    case FT_AFF_NAVIGATION_STOP: return !!n->state.loading;
    default: return 1;
  }
}
static char *copy_bytes(const uint8_t *bytes, size_t len) {
  char *text = malloc(len + 1);
  if (text) { if (len) memcpy(text, bytes, len); text[len] = 0; }
  return text;
}
static void end_edit(viewer_navigation *n) {
  if (n->editing) {
    SDL_StopTextInput();
    /* Cooperative/source-text input may already own SDL text input. Restore
     * that owner rather than disabling committed text delivery to the frame. */
    if (n->restore_text_input) SDL_StartTextInput();
  }
  n->editing = n->selected = n->restore_text_input = 0;
  free(n->edit); n->edit = NULL;
}
void viewer_navigation_snapshot(viewer_navigation *n, const ft_aff_navigation *state) {
  free(n->url); n->url = NULL;
  n->visible = state != NULL;
  n->state = state ? *state : (ft_aff_navigation){0};
  /* Event string views are borrowed. Retain only our own URL and scalar state. */
  n->state.url = n->state.title = (ft_aff_optional_string){0};
  if (state && state->url.present) n->url = copy_bytes(state->url.value.data, state->url.value.len);
  if (!viewer_navigation_enabled(n, FT_AFF_NAVIGATION_LOAD)) {
    end_edit(n);
  }
}
void viewer_navigation_destroy(viewer_navigation *n) {
  end_edit(n); free(n->url); *n = (viewer_navigation){0};
}
SDL_Rect viewer_navigation_fit(int w, int h, int fw, int fh, int strip) {
  if (strip < 0) strip = 0;
  SDL_Rect r = viewer_fit(w, h - strip, fw, fh);
  r.y += strip; return r;
}
static int send_verb(viewer_navigation *n, ft_affordances_host *host, uint32_t verb) {
  if (!host || !viewer_navigation_enabled(n, verb)) return 0;
  ft_aff_verb v = {.domain = FT_AFF_DOMAIN_NAVIGATION, .verb = verb};
  if (verb == FT_AFF_NAVIGATION_LOAD && n->edit)
    v.url = (ft_aff_string){.data = (const uint8_t *)n->edit, .len = strlen(n->edit)};
  return ft_affordances_host_send(host, &v) == FT_STATUS_OK ? 0 : -1;
}
int viewer_navigation_event(viewer_navigation *n, ft_affordances_host *host, const SDL_Event *e) {
  /* Releases belong to whoever received the press, even after editing ends
   * or the domain is withdrawn. Never leak an unmatched release to input. */
  if (e->type == SDL_KEYUP && e->key.keysym.scancode < SDL_NUM_SCANCODES && n->keys[e->key.keysym.scancode]) {
    n->keys[e->key.keysym.scancode] = 0; return 1;
  }
  if (e->type == SDL_MOUSEBUTTONUP && e->button.button < 32 && (n->buttons & (1u << e->button.button))) {
    n->buttons &= ~(1u << e->button.button); return 1;
  }
  if (e->type == SDL_WINDOWEVENT && e->window.event == SDL_WINDOWEVENT_FOCUS_LOST) {
    end_edit(n); memset(n->keys, 0, sizeof(n->keys)); n->buttons = 0;
  }
  if (!n->visible) return 0;
  if (n->editing && (e->type == SDL_KEYDOWN || e->type == SDL_KEYUP || e->type == SDL_TEXTINPUT || e->type == SDL_TEXTEDITING)) {
    if (e->type == SDL_KEYDOWN) {
      if (e->key.keysym.scancode < SDL_NUM_SCANCODES) n->keys[e->key.keysym.scancode] = 1;
      SDL_Keycode key = e->key.keysym.sym;
      if (key == SDLK_ESCAPE) { end_edit(n); }
      else if (key == SDLK_RETURN || key == SDLK_KP_ENTER) {
        int result = send_verb(n, host, FT_AFF_NAVIGATION_LOAD);
        end_edit(n); return result < 0 ? -1 : 1;
      } else if (key == SDLK_BACKSPACE && n->edit) {
        size_t len = strlen(n->edit);
        if (n->selected) n->edit[0] = 0;
        else if (len) { do { --len; } while (len && ((unsigned char)n->edit[len] & 0xc0) == 0x80); n->edit[len] = 0; }
        n->selected = 0;
      } else if (key == SDLK_a && (e->key.keysym.mod & KMOD_CTRL)) n->selected = 1;
    } else if (e->type == SDL_TEXTINPUT) {
      size_t old = n->selected || !n->edit ? 0 : strlen(n->edit), extra = strlen(e->text.text);
      char *text = realloc(n->edit, old + extra + 1);
      if (!text) return -1;
      memcpy(text + old, e->text.text, extra + 1); n->edit = text; n->selected = 0;
    }
    return 1;
  }
  int y = -1, x = 0;
  if (e->type == SDL_MOUSEBUTTONDOWN || e->type == SDL_MOUSEBUTTONUP) { x = e->button.x; y = e->button.y; }
  else if (e->type == SDL_MOUSEMOTION) { x = e->motion.x; y = e->motion.y; }
  if (y < 0 || y >= VIEWER_NAV_HEIGHT) {
    if (e->type == SDL_MOUSEBUTTONDOWN) end_edit(n);
    return 0;
  }
  if (e->type == SDL_MOUSEBUTTONDOWN && e->button.button < 32) n->buttons |= 1u << e->button.button;
  if (e->type == SDL_MOUSEBUTTONDOWN && e->button.button == SDL_BUTTON_LEFT && x >= 0) {
    uint32_t verb = x < VIEWER_NAV_BUTTON ? FT_AFF_NAVIGATION_BACK :
      x < VIEWER_NAV_BUTTON * 2 ? FT_AFF_NAVIGATION_FORWARD :
      x < VIEWER_NAV_BUTTON * 3 ? (n->state.loading ? FT_AFF_NAVIGATION_STOP : FT_AFF_NAVIGATION_RELOAD) : FT_AFF_NAVIGATION_LOAD;
    if (verb == FT_AFF_NAVIGATION_LOAD && viewer_navigation_enabled(n, verb)) {
      end_edit(n); n->edit = copy_bytes((const uint8_t *)(n->url ? n->url : ""), n->url ? strlen(n->url) : 0);
      if (!n->edit) return -1;
      n->restore_text_input = SDL_IsTextInputActive();
      n->editing = n->selected = 1; SDL_StartTextInput();
    } else if (verb != FT_AFF_NAVIGATION_LOAD && send_verb(n, host, verb)) return -1;
  }
  return 1;
}

/* Original embedded 5x7 bitmap glyphs. No font library or runtime assets. */
static const unsigned char glyphs[95][7] = {
  {0, 0, 0, 0, 0, 0, 0}, /* 32 */
  {4, 4, 4, 4, 4, 0, 4}, /* 33 */
  {10, 10, 0, 0, 0, 0, 0}, /* 34 */
  {10, 10, 31, 10, 31, 10, 10}, /* 35 */
  {4, 15, 20, 14, 5, 30, 4}, /* 36 */
  {25, 26, 4, 8, 22, 6, 0}, /* 37 */
  {12, 18, 20, 8, 21, 18, 13}, /* 38 */
  {4, 4, 0, 0, 0, 0, 0}, /* 39 */
  {2, 4, 8, 8, 8, 4, 2}, /* 40 */
  {8, 4, 2, 2, 2, 4, 8}, /* 41 */
  {0, 21, 14, 31, 14, 21, 0}, /* 42 */
  {0, 4, 4, 31, 4, 4, 0}, /* 43 */
  {0, 0, 0, 0, 4, 4, 8}, /* 44 */
  {0, 0, 0, 31, 0, 0, 0}, /* 45 */
  {0, 0, 0, 0, 0, 4, 4}, /* 46 */
  {1, 2, 2, 4, 8, 8, 16}, /* 47 */
  {14, 17, 19, 21, 25, 17, 14}, /* 48 */
  {4, 12, 4, 4, 4, 4, 14}, /* 49 */
  {14, 17, 1, 2, 4, 8, 31}, /* 50 */
  {30, 1, 1, 14, 1, 1, 30}, /* 51 */
  {2, 6, 10, 18, 31, 2, 2}, /* 52 */
  {31, 16, 16, 30, 1, 1, 30}, /* 53 */
  {14, 16, 16, 30, 17, 17, 14}, /* 54 */
  {31, 1, 2, 4, 8, 8, 8}, /* 55 */
  {14, 17, 17, 14, 17, 17, 14}, /* 56 */
  {14, 17, 17, 15, 1, 1, 14}, /* 57 */
  {0, 4, 4, 0, 4, 4, 0}, /* 58 */
  {0, 4, 4, 0, 4, 4, 8}, /* 59 */
  {2, 4, 8, 16, 8, 4, 2}, /* 60 */
  {0, 0, 31, 0, 31, 0, 0}, /* 61 */
  {8, 4, 2, 1, 2, 4, 8}, /* 62 */
  {14, 17, 1, 2, 4, 0, 4}, /* 63 */
  {14, 17, 23, 21, 23, 16, 14}, /* 64 */
  {14, 17, 17, 31, 17, 17, 17}, /* 65 */
  {30, 17, 17, 30, 17, 17, 30}, /* 66 */
  {15, 16, 16, 16, 16, 16, 15}, /* 67 */
  {30, 17, 17, 17, 17, 17, 30}, /* 68 */
  {31, 16, 16, 30, 16, 16, 31}, /* 69 */
  {31, 16, 16, 30, 16, 16, 16}, /* 70 */
  {15, 16, 16, 23, 17, 17, 15}, /* 71 */
  {17, 17, 17, 31, 17, 17, 17}, /* 72 */
  {31, 4, 4, 4, 4, 4, 31}, /* 73 */
  {7, 2, 2, 2, 18, 18, 12}, /* 74 */
  {17, 18, 20, 24, 20, 18, 17}, /* 75 */
  {16, 16, 16, 16, 16, 16, 31}, /* 76 */
  {17, 27, 21, 21, 17, 17, 17}, /* 77 */
  {17, 25, 21, 19, 17, 17, 17}, /* 78 */
  {14, 17, 17, 17, 17, 17, 14}, /* 79 */
  {30, 17, 17, 30, 16, 16, 16}, /* 80 */
  {14, 17, 17, 17, 21, 18, 13}, /* 81 */
  {30, 17, 17, 30, 20, 18, 17}, /* 82 */
  {15, 16, 16, 14, 1, 1, 30}, /* 83 */
  {31, 4, 4, 4, 4, 4, 4}, /* 84 */
  {17, 17, 17, 17, 17, 17, 14}, /* 85 */
  {17, 17, 17, 17, 17, 10, 4}, /* 86 */
  {17, 17, 17, 21, 21, 27, 17}, /* 87 */
  {17, 17, 10, 4, 10, 17, 17}, /* 88 */
  {17, 17, 10, 4, 4, 4, 4}, /* 89 */
  {31, 1, 2, 4, 8, 16, 31}, /* 90 */
  {14, 8, 8, 8, 8, 8, 14}, /* 91 */
  {16, 8, 8, 4, 2, 2, 1}, /* 92 */
  {14, 2, 2, 2, 2, 2, 14}, /* 93 */
  {4, 10, 17, 0, 0, 0, 0}, /* 94 */
  {0, 0, 0, 0, 0, 0, 31}, /* 95 */
  {8, 4, 0, 0, 0, 0, 0}, /* 96 */
  {0, 0, 14, 1, 15, 17, 15}, /* 97 */
  {16, 16, 30, 17, 17, 17, 30}, /* 98 */
  {0, 0, 15, 16, 16, 16, 15}, /* 99 */
  {1, 1, 15, 17, 17, 17, 15}, /* 100 */
  {0, 0, 14, 17, 31, 16, 15}, /* 101 */
  {6, 9, 8, 28, 8, 8, 8}, /* 102 */
  {0, 15, 17, 17, 15, 1, 14}, /* 103 */
  {16, 16, 30, 17, 17, 17, 17}, /* 104 */
  {4, 0, 12, 4, 4, 4, 14}, /* 105 */
  {2, 0, 6, 2, 2, 18, 12}, /* 106 */
  {16, 16, 18, 20, 24, 20, 18}, /* 107 */
  {12, 4, 4, 4, 4, 4, 14}, /* 108 */
  {0, 0, 26, 21, 21, 21, 21}, /* 109 */
  {0, 0, 30, 17, 17, 17, 17}, /* 110 */
  {0, 0, 14, 17, 17, 17, 14}, /* 111 */
  {0, 30, 17, 17, 30, 16, 16}, /* 112 */
  {0, 15, 17, 17, 15, 1, 1}, /* 113 */
  {0, 0, 23, 24, 16, 16, 16}, /* 114 */
  {0, 0, 15, 16, 14, 1, 30}, /* 115 */
  {8, 8, 28, 8, 8, 9, 6}, /* 116 */
  {0, 0, 17, 17, 17, 17, 15}, /* 117 */
  {0, 0, 17, 17, 17, 10, 4}, /* 118 */
  {0, 0, 17, 17, 21, 21, 10}, /* 119 */
  {0, 0, 17, 10, 4, 10, 17}, /* 120 */
  {0, 17, 17, 17, 15, 1, 14}, /* 121 */
  {0, 0, 31, 2, 4, 8, 31}, /* 122 */
  {3, 4, 4, 8, 4, 4, 3}, /* 123 */
  {4, 4, 4, 4, 4, 4, 4}, /* 124 */
  {24, 4, 4, 2, 4, 4, 24}, /* 125 */
  {0, 0, 9, 22, 0, 0, 0}, /* 126 */
};
static int text_draw(SDL_Renderer *r, const char *text, int x, int y, int right) {
  for (const unsigned char *p = (const unsigned char *)(text ? text : ""); *p && x + 5 <= right; ++p, x += 6) {
    unsigned char c = *p >= 32 && *p <= 126 ? *p : '?';
    for (int row = 0; row < 7; ++row) for (int col = 0; col < 5; ++col)
      if ((glyphs[c - 32][row] >> (4 - col)) & 1)
        if (SDL_RenderDrawPoint(r, x + col, y + row)) return -1;
  }
  return 0;
}
int viewer_navigation_draw(const viewer_navigation *n, SDL_Renderer *r, SDL_Window *window) {
  if (!n->visible) return 0;
  int w, h, dw, dh; SDL_GetWindowSize(window, &w, &h);
  if (w <= 0 || h <= 0 || SDL_GetRendererOutputSize(r, &dw, &dh)) return -1;
  /* SDL logical scaling keeps glyphs and hit targets in window coordinates. */
  float sx, sy; SDL_RenderGetScale(r, &sx, &sy);
  if (SDL_RenderSetScale(r, (float)dw / w, (float)dh / h)) return -1;
  SDL_Rect bar = {0, 0, w, VIEWER_NAV_HEIGHT};
  int result = SDL_SetRenderDrawColor(r, 35, 38, 42, 255) || SDL_RenderFillRect(r, &bar) ? -1 : 0;
  uint32_t verbs[] = {FT_AFF_NAVIGATION_BACK, FT_AFF_NAVIGATION_FORWARD,
    n->state.loading ? FT_AFF_NAVIGATION_STOP : FT_AFF_NAVIGATION_RELOAD};
  const char *labels[] = {"<", ">", n->state.loading ? "X" : "R"};
  for (int i = 0; i < 3; ++i) {
    int enabled = viewer_navigation_enabled(n, verbs[i]);
    if (SDL_SetRenderDrawColor(r, enabled ? 235 : 95, enabled ? 235 : 95, enabled ? 235 : 95, 255) ||
        text_draw(r, labels[i], i * VIEWER_NAV_BUTTON + 11, 10, (i + 1) * VIEWER_NAV_BUTTON)) result = -1;
  }
  SDL_Rect url = {VIEWER_NAV_BUTTON * 3, 3, w - VIEWER_NAV_BUTTON * 3, VIEWER_NAV_HEIGHT - 6};
  if (url.w > 0) {
    if (SDL_SetRenderDrawColor(r, n->editing ? 65 : 48, n->selected ? 85 : 55, 65, 255) || SDL_RenderFillRect(r, &url) ||
        SDL_SetRenderDrawColor(r, 235, 235, 235, 255)) result = -1;
    const char *text = n->editing ? n->edit : n->url;
    /* While editing, keep the insertion end visible after selection is replaced. */
    int chars = (url.w - 8) / 6;
    if (n->editing && !n->selected && text && chars > 0 && strlen(text) > (size_t)chars) text += strlen(text) - chars;
    if (text_draw(r, text, url.x + 4, 10, url.x + url.w - 4)) result = -1;
  }
  if (SDL_RenderSetScale(r, sx, sy)) result = -1;
  /* The next frame clears with black, rather than inheriting toolbar text color. */
  if (SDL_SetRenderDrawColor(r, 0, 0, 0, 255)) result = -1;
  return result;
}
