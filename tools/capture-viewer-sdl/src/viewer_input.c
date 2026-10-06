#include "viewer_input.h"
#include "viewer_fit.h"
#include "viewer_navigation.h"
#include <stdio.h>
#include <stddef.h>
#include <math.h>
#ifdef __APPLE__
#include <objc/message.h>
#include <objc/runtime.h>
#endif
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#ifdef __APPLE__
/* A value-only SDL user event preserves wheel ordering with focus and keys.
 * No heap payload or extra queue needs ownership during shutdown. */
typedef struct { Uint32 type, timestamp, window_id, direction; double x, y; } precise_wheel;
/* SDL identifies queued events by the first type member of SDL_Event. */
_Static_assert(offsetof(precise_wheel, type) == offsetof(SDL_CommonEvent, type), "SDL event type offset");
_Static_assert(sizeof(precise_wheel) <= sizeof(SDL_Event), "precise wheel fits SDL event");
#endif

static int send_event(viewer_input *input, ft_input_event *event) {
  uint64_t sequence = 0;
  ft_status s = ft_input_client_send(input->client, event, &sequence);
  /* Geometry can advance before the viewer polls its reset notification.
   * Drop the rejected action without recording a hold or replaying it. */
  if (s == FT_STATUS_STALE) { fprintf(stderr, "input send: stale action dropped\n"); return 0; }
  if (s != FT_STATUS_OK) { fprintf(stderr, "input send: %d\n", s); input->failed = 1; return 0; }
  return 1;
}
int viewer_input_open(viewer_input *input, const char *path) {
  struct sockaddr_un addr = {0}; addr.sun_family = AF_UNIX;
  if (strlen(path) >= sizeof(addr.sun_path)) return -1;
  memcpy(addr.sun_path, path, strlen(path) + 1);
  int fd = socket(AF_UNIX, SOCK_STREAM, 0);
  if (fd < 0) return -1;
  if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0) { close(fd); return -1; }
  int32_t owned = fd;
  ft_status status = ft_input_client_connect(&owned, FT_INPUT_MODE_COOPERATIVE, &input->client);
  if (owned >= 0) close(owned);
  if (status != FT_STATUS_OK) return -1;
  uint64_t controller, epoch;
  if (ft_input_client_describe(input->client, &input->config, &controller, &epoch) != FT_STATUS_OK) return -1;
  input->mode = FT_INPUT_MODE_COOPERATIVE; SDL_StartTextInput(); return 0;
}
/* DOM code vocabulary at the SDL adapter, never SDL numeric values on the wire. */
static int key_name(SDL_Scancode key, char out[64]) {
  if (key >= SDL_SCANCODE_A && key <= SDL_SCANCODE_Z) { snprintf(out, 64, "Key%c", 'A' + key - SDL_SCANCODE_A); return 1; }
  if (key >= SDL_SCANCODE_1 && key <= SDL_SCANCODE_9) { snprintf(out, 64, "Digit%d", 1 + key - SDL_SCANCODE_1); return 1; }
  if (key >= SDL_SCANCODE_F1 && key <= SDL_SCANCODE_F12) { snprintf(out, 64, "F%d", 1 + key - SDL_SCANCODE_F1); return 1; }
  const char *name = NULL;
  switch (key) {
#define KEY(s, n) case SDL_SCANCODE_##s: name = n; break
    KEY(0, "Digit0"); KEY(RETURN, "Enter"); KEY(ESCAPE, "Escape"); KEY(BACKSPACE, "Backspace"); KEY(TAB, "Tab"); KEY(SPACE, "Space");
    KEY(LEFT, "ArrowLeft"); KEY(RIGHT, "ArrowRight"); KEY(UP, "ArrowUp"); KEY(DOWN, "ArrowDown");
    KEY(LSHIFT, "ShiftLeft"); KEY(RSHIFT, "ShiftRight"); KEY(LCTRL, "ControlLeft"); KEY(RCTRL, "ControlRight");
    KEY(LALT, "AltLeft"); KEY(RALT, "AltRight"); KEY(LGUI, "MetaLeft"); KEY(RGUI, "MetaRight");
    KEY(HOME, "Home"); KEY(END, "End"); KEY(PAGEUP, "PageUp"); KEY(PAGEDOWN, "PageDown"); KEY(INSERT, "Insert"); KEY(DELETE, "Delete");
    KEY(MINUS, "Minus"); KEY(EQUALS, "Equal"); KEY(LEFTBRACKET, "BracketLeft"); KEY(RIGHTBRACKET, "BracketRight");
    KEY(BACKSLASH, "Backslash"); KEY(SEMICOLON, "Semicolon"); KEY(APOSTROPHE, "Quote"); KEY(GRAVE, "Backquote");
    KEY(COMMA, "Comma"); KEY(PERIOD, "Period"); KEY(SLASH, "Slash"); KEY(CAPSLOCK, "CapsLock");
    KEY(KP_ENTER, "NumpadEnter"); KEY(KP_0, "Numpad0"); KEY(KP_1, "Numpad1"); KEY(KP_2, "Numpad2");
    KEY(KP_3, "Numpad3"); KEY(KP_4, "Numpad4"); KEY(KP_5, "Numpad5"); KEY(KP_6, "Numpad6");
    KEY(KP_7, "Numpad7"); KEY(KP_8, "Numpad8"); KEY(KP_9, "Numpad9"); KEY(KP_PERIOD, "NumpadDecimal");
    KEY(KP_PLUS, "NumpadAdd"); KEY(KP_MINUS, "NumpadSubtract"); KEY(KP_MULTIPLY, "NumpadMultiply"); KEY(KP_DIVIDE, "NumpadDivide");
#undef KEY
    default: return 0;
  }
  snprintf(out, 64, "%s", name); return 1;
}
static uint32_t modifiers(SDL_Keymod m) {
  return ((m & KMOD_SHIFT) ? FT_INPUT_SHIFT : 0) | ((m & KMOD_CTRL) ? FT_INPUT_CONTROL : 0) |
    ((m & KMOD_ALT) ? FT_INPUT_ALT : 0) | ((m & KMOD_GUI) ? FT_INPUT_SUPER : 0) |
    ((m & KMOD_MODE) ? FT_INPUT_ALT_GRAPH : 0) | ((m & KMOD_CAPS) ? FT_INPUT_CAPS_LOCK : 0) |
    ((m & KMOD_NUM) ? FT_INPUT_NUM_LOCK : 0);
}
static SDL_Rect input_fit(viewer_input *input, SDL_Window *window, int *w, int *h, int *dw, int *dh) {
  SDL_GetWindowSize(window, w, h); *dw = *w; *dh = *h;
  if (input->renderer) SDL_GetRendererOutputSize(input->renderer, dw, dh);
  return input->frame_width ? viewer_navigation_fit(*dw, *dh, input->frame_width, input->frame_height, viewer_navigation_strip_height(*dh, *h, input->strip_height != 0)) : (SDL_Rect){0, 0, *dw, *dh};
}
static int position(viewer_input *input, SDL_Window *window, int x, int y, ft_input_event *e) {
  int w, h, dw, dh; SDL_Rect r = input_fit(input, window, &w, &h, &dw, &dh);
  if (w <= 0 || h <= 0) return 0;
  e->geometry_revision = input->config.geometry.revision;
  return viewer_map(r, (double)x * dw / w, (double)y * dh / h,
                    input->config.geometry.width, input->config.geometry.height, &e->x, &e->y);
}
void viewer_input_scroll(viewer_input *input, SDL_Window *window, double x, double y, uint32_t unit, uint32_t direction) {
  if (!input || !input->client || input->failed || input->resetting) return;
  ft_input_event e = {0};
  int pointer_x, pointer_y; SDL_GetMouseState(&pointer_x, &pointer_y);
  if (!position(input, window, pointer_x, pointer_y, &e)) return;
  e.kind = FT_INPUT_SCROLL; e.scroll_unit = unit; e.pointer_x = e.x; e.pointer_y = e.y;
  if (unit == FT_INPUT_SCROLL_PIXEL) {
    int w, h, dw, dh; SDL_Rect r = input_fit(input, window, &w, &h, &dw, &dh);
    x *= w > 0 && r.w > 0 ? input->config.geometry.width * dw / (w * (double)r.w) : 0;
    y *= h > 0 && r.h > 0 ? input->config.geometry.height * dh / (h * (double)r.h) : 0;
  }
  /* Zero logical displacement is no input operation. */
  if (x == 0 && y == 0) return;
  e.x = x; e.y = y;
  if (direction == SDL_MOUSEWHEEL_FLIPPED) { e.x = -e.x; e.y = -e.y; }
  send_event(input, &e);
}
static void reset_input(viewer_input *input) {
  if (ft_input_client_reset(input->client) != FT_STATUS_OK) input->failed = 1;
  input->resetting = 1; input->buttons = 0; memset(input->keys, 0, sizeof(input->keys));
}
void viewer_input_event(viewer_input *input, const SDL_Event *event, SDL_Window *window) {
  if (!input || !input->client || input->failed) return;
  if (event->type == SDL_WINDOWEVENT && event->window.event == SDL_WINDOWEVENT_FOCUS_LOST) {
    reset_input(input); return;
  }
  if (input->resetting) return;
  ft_input_event e = {0};
#ifdef __APPLE__
  if (event->type == input->precise_wheel_type) {
    precise_wheel wheel; memcpy(&wheel, event, sizeof(wheel));
    viewer_input_scroll(input, window, wheel.x, wheel.y, FT_INPUT_SCROLL_PIXEL, wheel.direction);
    return;
  }
#endif
  switch (event->type) {
    case SDL_KEYDOWN: case SDL_KEYUP: {
      if (input->mode == FT_INPUT_MODE_SOURCE_TEXT) return;
      if (input->mode == FT_INPUT_MODE_PHYSICAL && event->key.repeat) return;
      SDL_Scancode sc = event->key.keysym.scancode;
      if (sc <= SDL_SCANCODE_UNKNOWN || sc >= SDL_NUM_SCANCODES || !key_name(sc, e.key)) {
        fprintf(stderr, "unmapped SDL physical key: %d\n", sc); return;
      }
      if (event->type == SDL_KEYUP && !input->keys[sc]) return;
      e.kind = FT_INPUT_KEY; e.key_kind = FT_INPUT_PHYSICAL_KEY; e.press = (uint64_t)sc + 1;
      e.action = event->type == SDL_KEYUP ? FT_INPUT_UP : event->key.repeat ? FT_INPUT_REPEAT : FT_INPUT_DOWN;
      e.modifiers = modifiers((SDL_Keymod)event->key.keysym.mod);
      if (send_event(input, &e)) input->keys[sc] = event->type == SDL_KEYDOWN;
      return;
    }
    case SDL_TEXTINPUT: if (input->mode == FT_INPUT_MODE_PHYSICAL) return; e.kind = FT_INPUT_TEXT; e.text = (const uint8_t *)event->text.text; e.text_len = strlen(event->text.text); break;
    case SDL_MOUSEMOTION: e.kind = FT_INPUT_MOTION; if (!position(input, window, event->motion.x, event->motion.y, &e)) return; break;
    case SDL_MOUSEBUTTONDOWN: case SDL_MOUSEBUTTONUP:
      e.kind = FT_INPUT_BUTTON; e.action = event->type == SDL_MOUSEBUTTONDOWN ? FT_INPUT_DOWN : FT_INPUT_UP;
      /* Canonical buttons: primary=1, secondary=2, auxiliary=3, back=4, forward=5. */
      e.button = event->button.button == SDL_BUTTON_RIGHT ? 2 : event->button.button == SDL_BUTTON_MIDDLE ? 3 : event->button.button;
      if (!position(input, window, event->button.x, event->button.y, &e)) {
        /* No pointer event in the bars, but an outside release must not leave
         * source input held. Reset releases all held state through cleanup. */
        if (event->type == SDL_MOUSEBUTTONUP && e.button < 32 && (input->buttons & (1u << e.button))) reset_input(input);
        return;
      }
      if (send_event(input, &e) && e.button < 32) {
        if (e.action == FT_INPUT_DOWN) input->buttons |= 1u << e.button;
        else input->buttons &= ~(1u << e.button);
      }
      return;
      break;
    case SDL_MOUSEWHEEL: {
      double x, y; uint32_t unit = FT_INPUT_SCROLL_LINE;
#if SDL_VERSION_ATLEAST(2, 0, 18)
      x = event->wheel.preciseX; y = -event->wheel.preciseY;
#else
      x = event->wheel.x; y = -event->wheel.y;
#endif
#ifndef __APPLE__
      /* SDL2 has no portable device-unit flag. Fractional wheel values are the
       * best available continuous-device signal outside Cocoa; integral values
       * remain lines. This fallback cannot identify integral precise deltas. */
      if (x != trunc(x) || y != trunc(y)) unit = FT_INPUT_SCROLL_PIXEL;
#endif
      viewer_input_scroll(input, window, x, y, unit, event->wheel.direction);
      return;
    }
    default: return;
  }
  send_event(input, &e);
}
#ifdef __APPLE__
static int wheel_filter(void *userdata, SDL_Event *event) {
  viewer_input *input = userdata;
  /* SDL_PushEvent may invoke filters on another thread. Only the Cocoa pump
   * thread may inspect AppKit; other events remain queued for normal handling. */
  if (SDL_ThreadID() != input->event_thread || event->type != SDL_MOUSEWHEEL) return 1;
  /* SDL calls the filter inside the Cocoa scrollWheel: dispatch. Capture the
   * precise-device flag and logical deltas while currentEvent is still valid.
   * SDL2 preciseX/Y alone contain deltaX/Y and omit that flag. */
  id app = ((id (*)(id, SEL))objc_msgSend)((id)objc_getClass("NSApplication"), sel_registerName("sharedApplication"));
  id native = ((id (*)(id, SEL))objc_msgSend)(app, sel_registerName("currentEvent"));
  if (native && ((unsigned long (*)(id, SEL))objc_msgSend)(native, sel_registerName("type")) == 22 &&
      ((BOOL (*)(id, SEL))objc_msgSend)(native, sel_registerName("hasPreciseScrollingDeltas"))) {
    precise_wheel wheel = {input->precise_wheel_type, event->wheel.timestamp, event->wheel.windowID,
      event->wheel.direction,
      -((double (*)(id, SEL))objc_msgSend)(native, sel_registerName("scrollingDeltaX")),
      -((double (*)(id, SEL))objc_msgSend)(native, sel_registerName("scrollingDeltaY"))};
    memset(event, 0, sizeof(*event)); memcpy(event, &wheel, sizeof(wheel));
  }
  return 1;
}
#endif
void viewer_input_install_wheel_filter(viewer_input *input, SDL_Window *window) {
  (void)window;
#ifdef __APPLE__
  input->event_thread = SDL_ThreadID();
  input->precise_wheel_type = SDL_RegisterEvents(1);
  if (input->precise_wheel_type == (Uint32)-1) {
    fprintf(stderr, "SDL precise wheel event registration failed: %s\n", SDL_GetError());
    input->failed = 1; return;
  }
  SDL_SetEventFilter(wheel_filter, input);
#else
  (void)input;
#endif
}
void viewer_input_poll(viewer_input *input) {
  if (!input || !input->client) return;
  ft_input_status s;
  while (ft_input_client_poll(input->client, &s) == FT_STATUS_OK) {
    if (s.kind == FT_INPUT_COALESCED) {
      /* Count-only notification: the C ABI stores the count in sequence and
       * carries no failure result. This viewer keeps no in-flight list, so no
       * retirement is needed; the surviving latest motion completes separately. */
      continue;
    }
    if (s.kind == FT_INPUT_RESET) { input->config.geometry = s.geometry; input->resetting = 0; }
    if (s.kind == FT_INPUT_REFUSED || (s.kind == FT_INPUT_COMPLETED && s.result != FT_INPUT_EXECUTED)) {
      fprintf(stderr, "input operation %llu result=%d\n", (unsigned long long)s.sequence, s.result);
    }
    if (s.kind == FT_INPUT_CLOSED) { input->failed = 1; fprintf(stderr, "input closed reason=%u clean=%u\n", s.reason, s.clean); }
  }
}
int viewer_input_close(viewer_input *input) {
  if (!input->client) return input->failed;
  ft_input_client_close(input->client);
  uint32_t start = SDL_GetTicks(); int clean = 0;
  while (SDL_GetTicks() - start < 3000) {
    ft_input_status s;
    if (ft_input_client_poll(input->client, &s) == FT_STATUS_OK) {
      if (s.kind == FT_INPUT_CLOSED) { clean = s.clean; break; }
    } else SDL_Delay(2);
  }
  ft_input_client_destroy(&input->client);
  printf("input_cleanup=%s\n", clean ? "completed" : "unconfirmed");
  return input->failed || !clean;
}
void viewer_input_self_test(viewer_input *input, SDL_Window *window) {
  SDL_Event e; memset(&e, 0, sizeof(e)); e.type = SDL_KEYDOWN; e.key.state = SDL_PRESSED; e.key.windowID = SDL_GetWindowID(window); e.key.keysym.scancode = SDL_SCANCODE_A; SDL_PushEvent(&e);
  e.key.repeat = 1; SDL_PushEvent(&e); e.type = SDL_KEYUP; e.key.state = SDL_RELEASED; e.key.repeat = 0; SDL_PushEvent(&e);
  memset(&e, 0, sizeof(e)); e.type = SDL_TEXTINPUT; snprintf(e.text.text, sizeof(e.text.text), "hé🙂");
  /* sdl2-compat explicitly cannot translate a pushed SDL2 TEXTINPUT to SDL3.
   * Feed the captured-event fixture through the same translator as live input. */
  viewer_input_event(input, &e, window);
  memset(&e, 0, sizeof(e)); e.type = SDL_KEYDOWN; e.key.state = SDL_PRESSED; e.key.windowID = SDL_GetWindowID(window); e.key.keysym.scancode = SDL_SCANCODE_LSHIFT; SDL_PushEvent(&e);
  memset(&e, 0, sizeof(e)); e.type = SDL_MOUSEBUTTONDOWN; e.button.button = SDL_BUTTON_LEFT; e.button.x = 20; e.button.y = 30; SDL_PushEvent(&e);
  uint8_t text[1024]; memset(text, 'x', sizeof(text));
  ft_input_event long_text = {.kind = FT_INPUT_TEXT, .text = text, .text_len = sizeof(text)}; if (input->mode != FT_INPUT_MODE_PHYSICAL) send_event(input, &long_text);
}
