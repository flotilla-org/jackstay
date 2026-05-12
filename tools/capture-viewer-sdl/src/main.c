#include <SDL.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "capture_transfer.h"

enum { WIDTH = 320, HEIGHT = 180, STRIDE = WIDTH * 4 };

static void fill_frame(uint8_t *pixels, uint64_t sequence) {
  for (uint32_t y = 0; y < HEIGHT; y++) {
    for (uint32_t x = 0; x < WIDTH; x++) {
      size_t offset = (size_t)y * STRIDE + (size_t)x * 4;
      pixels[offset + 0] = (uint8_t)((x + sequence * 3) % 256);
      pixels[offset + 1] = (uint8_t)((y + sequence * 5) % 256);
      pixels[offset + 2] = (uint8_t)((x + y + sequence * 7) % 256);
      pixels[offset + 3] = 255;
    }
  }
}

static int parse_frames(int argc, char **argv) {
  for (int i = 1; i + 1 < argc; i++) {
    if (strcmp(argv[i], "--frames") == 0) {
      return atoi(argv[i + 1]);
    }
  }
  return 0;
}

static int require_ok(ft_status status, const char *operation) {
  if (status == FT_STATUS_OK) {
    return 0;
  }
  fprintf(stderr, "%s failed with status %d\n", operation, status);
  return 1;
}

int main(int argc, char **argv) {
  int max_frames = parse_frames(argc, argv);

  ft_producer *producer = NULL;
  ft_producer_options producer_options = {0};
  if (require_ok(ft_producer_create(&producer_options, &producer), "ft_producer_create")) {
    return 1;
  }

  ft_source_id source_id = 0;
  ft_source_desc source_desc = {
      .kind = FT_SOURCE_KIND_WINDOW,
      .label = "synthetic",
  };
  if (require_ok(ft_producer_register_source(producer, &source_desc, &source_id),
                 "ft_producer_register_source")) {
    ft_producer_destroy(producer);
    return 1;
  }

  ft_track_id track_id = 0;
  ft_track_desc track_desc = {
      .track_type = FT_TRACK_TYPE_VIDEO,
      .video = {.width = WIDTH, .height = HEIGHT, .pixel_format = FT_PIXEL_FORMAT_BGRA8_UNORM},
  };
  if (require_ok(ft_producer_register_track(producer, source_id, &track_desc, &track_id),
                 "ft_producer_register_track")) {
    ft_producer_destroy(producer);
    return 1;
  }

  ft_consumer *consumer = NULL;
  ft_consumer_options consumer_options = {.producer = producer};
  if (require_ok(ft_consumer_connect(&consumer_options, &consumer), "ft_consumer_connect")) {
    ft_producer_destroy(producer);
    return 1;
  }

  if (SDL_Init(SDL_INIT_VIDEO) != 0) {
    fprintf(stderr, "SDL_Init failed: %s\n", SDL_GetError());
    ft_consumer_destroy(consumer);
    ft_producer_destroy(producer);
    return 1;
  }

  SDL_Window *window = SDL_CreateWindow("capture-viewer-sdl", SDL_WINDOWPOS_CENTERED,
                                        SDL_WINDOWPOS_CENTERED, WIDTH, HEIGHT, SDL_WINDOW_SHOWN);
  SDL_Renderer *renderer = NULL;
  SDL_Texture *texture = NULL;
  if (window != NULL) {
    renderer = SDL_CreateRenderer(window, -1, SDL_RENDERER_ACCELERATED | SDL_RENDERER_PRESENTVSYNC);
    if (renderer == NULL) {
      renderer = SDL_CreateRenderer(window, -1, SDL_RENDERER_SOFTWARE);
    }
  }
  if (renderer != NULL) {
    texture = SDL_CreateTexture(renderer, SDL_PIXELFORMAT_BGRA32, SDL_TEXTUREACCESS_STREAMING, WIDTH,
                                HEIGHT);
  }
  if (window == NULL || renderer == NULL || texture == NULL) {
    fprintf(stderr, "SDL setup failed: %s\n", SDL_GetError());
    if (texture != NULL) {
      SDL_DestroyTexture(texture);
    }
    if (renderer != NULL) {
      SDL_DestroyRenderer(renderer);
    }
    if (window != NULL) {
      SDL_DestroyWindow(window);
    }
    SDL_Quit();
    ft_consumer_destroy(consumer);
    ft_producer_destroy(producer);
    return 1;
  }

  uint8_t *pixels = malloc((size_t)STRIDE * HEIGHT);
  if (pixels == NULL) {
    fprintf(stderr, "pixel allocation failed\n");
    SDL_DestroyTexture(texture);
    SDL_DestroyRenderer(renderer);
    SDL_DestroyWindow(window);
    SDL_Quit();
    ft_consumer_destroy(consumer);
    ft_producer_destroy(producer);
    return 1;
  }

  int running = 1;
  uint64_t sequence = 1;
  while (running && (max_frames <= 0 || sequence <= (uint64_t)max_frames)) {
    SDL_Event sdl_event;
    while (SDL_PollEvent(&sdl_event)) {
      if (sdl_event.type == SDL_QUIT) {
        running = 0;
      }
    }

    fill_frame(pixels, sequence);
    ft_video_frame_desc frame_desc = {
        .sequence = sequence,
        .timestamp_ns = sequence * 16666667,
        .width = WIDTH,
        .height = HEIGHT,
        .stride = STRIDE,
        .pixel_format = FT_PIXEL_FORMAT_BGRA8_UNORM,
    };
    if (require_ok(ft_producer_publish_video_frame(producer, track_id, &frame_desc, pixels,
                                                   (size_t)STRIDE * HEIGHT),
                   "ft_producer_publish_video_frame")) {
      running = 0;
      break;
    }

    ft_event event;
    while (ft_consumer_poll_event(consumer, &event) == FT_STATUS_OK) {
    }

    ft_video_frame frame = {0};
    if (ft_consumer_acquire_latest_video_frame(consumer, track_id, &frame) == FT_STATUS_OK) {
      SDL_UpdateTexture(texture, NULL, frame.data, (int)frame.desc.stride);
      SDL_RenderClear(renderer);
      SDL_RenderCopy(renderer, texture, NULL, NULL);
      SDL_RenderPresent(renderer);
      ft_consumer_release_video_frame(consumer, &frame);
    }

    sequence++;
    SDL_Delay(16);
  }

  free(pixels);
  SDL_DestroyTexture(texture);
  SDL_DestroyRenderer(renderer);
  SDL_DestroyWindow(window);
  SDL_Quit();
  ft_consumer_destroy(consumer);
  ft_producer_destroy(producer);
  return 0;
}
