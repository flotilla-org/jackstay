#ifndef VIEWER_INPUT_H
#define VIEWER_INPUT_H
#include <SDL.h>
#include "jackstay_input.h"
typedef struct { ft_input_client *client; ft_input_config config; int failed; int resetting; uint32_t mode; Uint32 precise_wheel_type; SDL_threadID event_thread; uint8_t keys[SDL_NUM_SCANCODES]; } viewer_input;
int viewer_input_open(viewer_input *input, const char *path);
void viewer_input_install_wheel_filter(viewer_input *input, SDL_Window *window);
void viewer_input_scroll(viewer_input *input, SDL_Window *window, double x, double y, uint32_t unit, uint32_t direction);
void viewer_input_event(viewer_input *input, const SDL_Event *event, SDL_Window *window);
void viewer_input_poll(viewer_input *input);
int viewer_input_close(viewer_input *input);
void viewer_input_self_test(viewer_input *input, SDL_Window *window);
#endif
