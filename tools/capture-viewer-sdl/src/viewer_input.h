#ifndef VIEWER_INPUT_H
#define VIEWER_INPUT_H
#include <SDL.h>
#include "jackstay_input.h"
typedef struct { SDL_Renderer *renderer; int frame_width, frame_height, strip_height; ft_input_client *client; ft_input_config config; int failed; int resetting; int scroll_ready; uint32_t mode; Uint32 native_scroll_type; void *scroll_monitor; uint32_t buttons; uint8_t keys[SDL_NUM_SCANCODES]; } viewer_input;
int viewer_input_open(viewer_input *input, const char *path);
void viewer_input_install_scroll_capture(viewer_input *input, SDL_Window *window);
void viewer_input_scroll(viewer_input *input, SDL_Window *window, double x, double y, uint32_t unit, uint32_t direction);
/* Native capture seam: plain values enter the same SDL queue as live AppKit events. */
int viewer_input_capture_scroll(viewer_input *input, SDL_Window *window, double x, double y,
                                uint32_t unit, unsigned long phase, unsigned long momentum,
                                int inverted, double pointer_x, double pointer_y);
void viewer_input_event(viewer_input *input, const SDL_Event *event, SDL_Window *window);
void viewer_input_poll(viewer_input *input);
int viewer_input_close(viewer_input *input);
void viewer_input_self_test(viewer_input *input, SDL_Window *window);
#endif
