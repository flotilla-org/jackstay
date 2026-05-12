#include <SDL.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "capture_transfer.h"

enum { WIDTH = 320, HEIGHT = 180, STRIDE = WIDTH * 4 };

typedef struct viewer_options {
  int max_frames;
  const char *porthole_socket;
  const char *session_id;
} viewer_options;

typedef struct stream_state {
  ft_producer *producer;
  ft_consumer *consumer;
  ft_track_id track_id;
  uint32_t width;
  uint32_t height;
  uint32_t stride;
  int daemon_mode;
} stream_state;

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

static viewer_options parse_options(int argc, char **argv) {
  viewer_options options = {0};
  for (int i = 1; i + 1 < argc; i++) {
    if (strcmp(argv[i], "--frames") == 0) {
      options.max_frames = atoi(argv[i + 1]);
      i++;
    } else if (strcmp(argv[i], "--porthole-socket") == 0) {
      options.porthole_socket = argv[i + 1];
      i++;
    } else if (strcmp(argv[i], "--session-id") == 0) {
      options.session_id = argv[i + 1];
      i++;
    }
  }
  return options;
}

static int require_ok(ft_status status, const char *operation) {
  if (status == FT_STATUS_OK) {
    return 0;
  }
  fprintf(stderr, "%s failed with status %d\n", operation, status);
  return 1;
}

int main(int argc, char **argv) {
  viewer_options options = parse_options(argc, argv);

  stream_state stream = {
      .producer = NULL,
      .consumer = NULL,
      .track_id = 0,
      .width = WIDTH,
      .height = HEIGHT,
      .stride = STRIDE,
      .daemon_mode = options.porthole_socket != NULL,
  };

  ft_synthetic_session synthetic_session = {0};
  if (stream.daemon_mode) {
    const char *session_id = options.session_id;
    if (session_id == NULL) {
      if (require_ok(ft_create_synthetic_session(options.porthole_socket, &synthetic_session),
                     "ft_create_synthetic_session")) {
        return 1;
      }
      session_id = synthetic_session.session_id;
    }
    ft_session_descriptor descriptor = {
        .control_socket_path = options.porthole_socket,
        .session_id = session_id,
    };
    if (require_ok(ft_consumer_connect_session(&descriptor, &stream.consumer),
                   "ft_consumer_connect_session")) {
      return 1;
    }
  } else {
    ft_producer_options producer_options = {0};
    if (require_ok(ft_producer_create(&producer_options, &stream.producer), "ft_producer_create")) {
      return 1;
    }

    ft_source_id source_id = 0;
    ft_source_desc source_desc = {
        .kind = FT_SOURCE_KIND_WINDOW,
        .label = "synthetic",
    };
    if (require_ok(ft_producer_register_source(stream.producer, &source_desc, &source_id),
                   "ft_producer_register_source")) {
      ft_producer_destroy(stream.producer);
      return 1;
    }

    ft_track_desc track_desc = {
        .track_type = FT_TRACK_TYPE_VIDEO,
        .video = {.width = WIDTH, .height = HEIGHT, .pixel_format = FT_PIXEL_FORMAT_BGRA8_UNORM},
    };
    if (require_ok(ft_producer_register_track(stream.producer, source_id, &track_desc, &stream.track_id),
                   "ft_producer_register_track")) {
      ft_producer_destroy(stream.producer);
      return 1;
    }

    ft_consumer_options consumer_options = {.producer = stream.producer};
    if (require_ok(ft_consumer_connect(&consumer_options, &stream.consumer), "ft_consumer_connect")) {
      ft_producer_destroy(stream.producer);
      return 1;
    }
  }

  ft_event event;
  while (ft_consumer_poll_event(stream.consumer, &event) == FT_STATUS_OK) {
    if (event.kind == FT_EVENT_TRACK_REGISTERED && event.track_type == FT_TRACK_TYPE_VIDEO) {
      stream.track_id = event.track_id;
      stream.width = event.width;
      stream.height = event.height;
      stream.stride = event.width * 4;
    }
  }
  if (stream.track_id == 0) {
    fprintf(stderr, "no video track registered\n");
    ft_consumer_destroy(stream.consumer);
    if (stream.producer != NULL) {
      ft_producer_destroy(stream.producer);
    }
    return 1;
  }

  if (SDL_Init(SDL_INIT_VIDEO) != 0) {
    fprintf(stderr, "SDL_Init failed: %s\n", SDL_GetError());
    ft_consumer_destroy(stream.consumer);
    if (stream.producer != NULL) {
      ft_producer_destroy(stream.producer);
    }
    return 1;
  }

  int window_width = stream.width < WIDTH ? WIDTH : (int)stream.width;
  int window_height = stream.height < HEIGHT ? HEIGHT : (int)stream.height;
  SDL_Window *window = SDL_CreateWindow("capture-viewer-sdl", SDL_WINDOWPOS_CENTERED,
                                        SDL_WINDOWPOS_CENTERED, window_width, window_height,
                                        SDL_WINDOW_SHOWN);
  SDL_Renderer *renderer = NULL;
  SDL_Texture *texture = NULL;
  if (window != NULL) {
    renderer = SDL_CreateRenderer(window, -1, SDL_RENDERER_ACCELERATED | SDL_RENDERER_PRESENTVSYNC);
    if (renderer == NULL) {
      renderer = SDL_CreateRenderer(window, -1, SDL_RENDERER_SOFTWARE);
    }
  }
  if (renderer != NULL) {
    texture = SDL_CreateTexture(renderer, SDL_PIXELFORMAT_BGRA32, SDL_TEXTUREACCESS_STREAMING,
                                stream.width, stream.height);
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
    ft_consumer_destroy(stream.consumer);
    if (stream.producer != NULL) {
      ft_producer_destroy(stream.producer);
    }
    return 1;
  }

  uint8_t *pixels = stream.daemon_mode ? NULL : malloc((size_t)STRIDE * HEIGHT);
  if (!stream.daemon_mode && pixels == NULL) {
    fprintf(stderr, "pixel allocation failed\n");
    SDL_DestroyTexture(texture);
    SDL_DestroyRenderer(renderer);
    SDL_DestroyWindow(window);
    SDL_Quit();
    ft_consumer_destroy(stream.consumer);
    ft_producer_destroy(stream.producer);
    return 1;
  }

  int running = 1;
  uint64_t sequence = 1;
  while (running && (options.max_frames <= 0 || sequence <= (uint64_t)options.max_frames)) {
    SDL_Event sdl_event;
    while (SDL_PollEvent(&sdl_event)) {
      if (sdl_event.type == SDL_QUIT) {
        running = 0;
      }
    }

    if (!stream.daemon_mode) {
      fill_frame(pixels, sequence);
      ft_video_frame_desc frame_desc = {
          .sequence = sequence,
          .timestamp_ns = sequence * 16666667,
          .width = WIDTH,
          .height = HEIGHT,
          .stride = STRIDE,
          .pixel_format = FT_PIXEL_FORMAT_BGRA8_UNORM,
      };
      if (require_ok(ft_producer_publish_video_frame(stream.producer, stream.track_id, &frame_desc, pixels,
                                                     (size_t)STRIDE * HEIGHT),
                     "ft_producer_publish_video_frame")) {
        running = 0;
        break;
      }
    }

    while (ft_consumer_poll_event(stream.consumer, &event) == FT_STATUS_OK) {
      if (event.kind == FT_EVENT_TRACK_UPDATED && event.track_id == stream.track_id &&
          event.track_type == FT_TRACK_TYPE_VIDEO) {
        stream.width = event.width;
        stream.height = event.height;
        stream.stride = event.width * 4;
      }
    }

    ft_video_frame frame = {0};
    if (ft_consumer_acquire_latest_video_frame(stream.consumer, stream.track_id, &frame) == FT_STATUS_OK) {
      if (frame.desc.width != stream.width || frame.desc.height != stream.height) {
        SDL_Texture *resized_texture = SDL_CreateTexture(renderer, SDL_PIXELFORMAT_BGRA32,
                                                         SDL_TEXTUREACCESS_STREAMING,
                                                         frame.desc.width, frame.desc.height);
        if (resized_texture == NULL) {
          fprintf(stderr, "SDL resize texture failed: %s\n", SDL_GetError());
          ft_consumer_release_video_frame(stream.consumer, &frame);
          running = 0;
          break;
        }
        SDL_DestroyTexture(texture);
        texture = resized_texture;
        SDL_SetWindowSize(window, frame.desc.width < WIDTH ? WIDTH : (int)frame.desc.width,
                          frame.desc.height < HEIGHT ? HEIGHT : (int)frame.desc.height);
        stream.width = frame.desc.width;
        stream.height = frame.desc.height;
      }
      stream.stride = frame.desc.stride;
      SDL_UpdateTexture(texture, NULL, frame.data, (int)frame.desc.stride);
      SDL_RenderClear(renderer);
      SDL_RenderCopy(renderer, texture, NULL, NULL);
      SDL_RenderPresent(renderer);
      ft_consumer_release_video_frame(stream.consumer, &frame);
    }

    sequence++;
    SDL_Delay(16);
  }

  free(pixels);
  SDL_DestroyTexture(texture);
  SDL_DestroyRenderer(renderer);
  SDL_DestroyWindow(window);
  SDL_Quit();
  ft_consumer_destroy(stream.consumer);
  if (stream.producer != NULL) {
    ft_producer_destroy(stream.producer);
  }
  return 0;
}
