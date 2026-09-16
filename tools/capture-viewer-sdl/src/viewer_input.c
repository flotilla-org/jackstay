#include "viewer_input.h"
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static int send_event(viewer_input *input, ft_input_event *event) {
  uint64_t sequence = 0;
  ft_status s = ft_input_client_send(input->client, event, &sequence);
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
  SDL_StartTextInput(); return 0;
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
static void position(viewer_input *input, SDL_Window *window, int x, int y, ft_input_event *e) {
  int w, h; SDL_GetWindowSize(window, &w, &h);
  e->geometry_revision = input->config.geometry.revision;
  e->x = w > 0 ? x * input->config.geometry.width / w : 0;
  e->y = h > 0 ? y * input->config.geometry.height / h : 0;
}
void viewer_input_event(viewer_input *input, const SDL_Event *event, SDL_Window *window) {
  if (!input || !input->client || input->failed) return;
  if (event->type == SDL_WINDOWEVENT && event->window.event == SDL_WINDOWEVENT_FOCUS_LOST) {
    if (ft_input_client_reset(input->client) != FT_STATUS_OK) input->failed = 1;
    input->resetting = 1; memset(input->keys, 0, sizeof(input->keys)); return;
  }
  if (input->resetting) return;
  ft_input_event e = {0};
  switch (event->type) {
    case SDL_KEYDOWN: case SDL_KEYUP: {
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
    case SDL_TEXTINPUT: e.kind = FT_INPUT_TEXT; e.text = (const uint8_t *)event->text.text; e.text_len = strlen(event->text.text); break;
    case SDL_MOUSEMOTION: e.kind = FT_INPUT_MOTION; position(input, window, event->motion.x, event->motion.y, &e); break;
    case SDL_MOUSEBUTTONDOWN: case SDL_MOUSEBUTTONUP:
      e.kind = FT_INPUT_BUTTON; e.action = event->type == SDL_MOUSEBUTTONDOWN ? FT_INPUT_DOWN : FT_INPUT_UP;
      /* Canonical buttons: primary=1, secondary=2, auxiliary=3, back=4, forward=5. */
      e.button = event->button.button == SDL_BUTTON_RIGHT ? 2 : event->button.button == SDL_BUTTON_MIDDLE ? 3 : event->button.button;
      position(input, window, event->button.x, event->button.y, &e); break;
    case SDL_MOUSEWHEEL: {
      int x, y; SDL_GetMouseState(&x, &y); position(input, window, x, y, &e);
      e.kind = FT_INPUT_SCROLL; e.scroll_unit = FT_INPUT_SCROLL_LINE; e.pointer_x = e.x; e.pointer_y = e.y;
#if SDL_VERSION_ATLEAST(2, 0, 18)
      e.x = event->wheel.preciseX; e.y = -event->wheel.preciseY;
#else
      e.x = event->wheel.x; e.y = -event->wheel.y;
#endif
      if (event->wheel.direction == SDL_MOUSEWHEEL_FLIPPED) { e.x = -e.x; e.y = -e.y; }
      break;
    }
    default: return;
  }
  send_event(input, &e);
}
void viewer_input_poll(viewer_input *input) {
  if (!input || !input->client) return;
  ft_input_status s;
  while (ft_input_client_poll(input->client, &s) == FT_STATUS_OK) {
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
  SDL_Event e = {0}; e.type = SDL_KEYDOWN; e.key.state = SDL_PRESSED; e.key.windowID = SDL_GetWindowID(window); e.key.keysym.scancode = SDL_SCANCODE_A; SDL_PushEvent(&e);
  e.key.repeat = 1; SDL_PushEvent(&e); e.type = SDL_KEYUP; e.key.state = SDL_RELEASED; e.key.repeat = 0; SDL_PushEvent(&e);
  e = (SDL_Event){0}; e.type = SDL_TEXTINPUT; snprintf(e.text.text, sizeof(e.text.text), "hé🙂");
  /* sdl2-compat explicitly cannot translate a pushed SDL2 TEXTINPUT to SDL3.
   * Feed the captured-event fixture through the same translator as live input. */
  viewer_input_event(input, &e, window);
  e = (SDL_Event){0}; e.type = SDL_KEYDOWN; e.key.state = SDL_PRESSED; e.key.windowID = SDL_GetWindowID(window); e.key.keysym.scancode = SDL_SCANCODE_LSHIFT; SDL_PushEvent(&e);
  e = (SDL_Event){0}; e.type = SDL_MOUSEBUTTONDOWN; e.button.button = SDL_BUTTON_LEFT; e.button.x = 20; e.button.y = 30; SDL_PushEvent(&e);
  uint8_t text[1024]; memset(text, 'x', sizeof(text));
  ft_input_event long_text = {.kind = FT_INPUT_TEXT, .text = text, .text_len = sizeof(text)}; send_event(input, &long_text);
}
