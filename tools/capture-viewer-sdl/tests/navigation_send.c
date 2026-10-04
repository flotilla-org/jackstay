#include "viewer_navigation.h"
#ifdef NDEBUG
#undef NDEBUG
#endif
#include <assert.h>
#include <string.h>
/* The public ABI sender is the socket boundary. This fake records enqueue
 * attempts and injects a closed-channel result, without a fake SDL renderer. */
static int sends;
static ft_status result;
static ft_status send_to_socket(ft_affordances_host *host, const ft_aff_verb *verb) {
  (void)host; ++sends;
  assert(verb->domain == FT_AFF_DOMAIN_NAVIGATION && verb->verb == FT_AFF_NAVIGATION_LOAD);
  assert(verb->url.len == 5 && !memcmp(verb->url.data, "typed", 5));
  return result;
}
#define ft_affordances_host_send send_to_socket
#include "../src/viewer_navigation.c"
int main(void) {
  assert(SDL_Init(SDL_INIT_VIDEO) == 0);
  SDL_Window *window = SDL_CreateWindow("send", 0, 0, 100, 100, SDL_WINDOW_HIDDEN); assert(window);
  viewer_navigation n = {0};
  ft_aff_navigation state = {.capabilities = 16};
  SDL_Event click = {.button = {.type = SDL_MOUSEBUTTONDOWN, .button = SDL_BUTTON_LEFT, .x = 90, .y = 10}};
  SDL_Event text = {.text = {.type = SDL_TEXTINPUT, .text = "typed"}};
  SDL_Event enter = {.key = {.type = SDL_KEYDOWN, .keysym = {.sym = SDLK_RETURN, .scancode = SDL_SCANCODE_RETURN}}};
  /* Each Enter exits edit mode. An enabled load enqueues the edited bytes;
   * a gated load never touches the sender; a channel failure is propagated. */
  for (int mode = 0; mode < 3; ++mode) {
    SDL_StopTextInput(); viewer_navigation_snapshot(&n, &state);
    viewer_navigation_event(&n, NULL, &click); viewer_navigation_event(&n, NULL, &text);
    n.state.capabilities = mode == 1 ? 0 : 16;
    result = mode == 2 ? FT_STATUS_CLOSED : FT_STATUS_OK;
    int before = sends;
    /* A non-null handle supplies sender presence; the boundary fake never
     * dereferences it. Storage belongs to this test and outlives the call. */
    char handle_storage;
    int sent = viewer_navigation_event(&n, (ft_affordances_host *)&handle_storage, &enter);
    assert(sent == (mode == 2 ? -1 : 1));
    assert(sends == before + (mode != 1));
    assert(!n.editing && !SDL_IsTextInputActive() && !n.edit);
    SDL_Event up = enter; up.type = SDL_KEYUP;
    assert(viewer_navigation_event(&n, NULL, &up) == 1);
  }
  viewer_navigation_destroy(&n); SDL_DestroyWindow(window); SDL_Quit(); return 0;
}
