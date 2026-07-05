#include <SDL.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "capture_transfer.h"
#include "metal_present.h"

enum { WIDTH = 320, HEIGHT = 180, STRIDE = WIDTH * 4, MAX_NATIVE_POOLS = 16 };

typedef struct viewer_options {
  int max_frames;
  const char *porthole_socket;
  const char *session_id;
  int native;
  uint32_t transport_kind;
  const char *endpoint;
  const char *token;
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

typedef struct native_pool_cache {
  ft_native_pool pools[MAX_NATIVE_POOLS];
  uint32_t count;
} native_pool_cache;

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
  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "--native") == 0) {
      options.native = 1;
    } else if (i + 1 >= argc) {
      continue;
    } else if (strcmp(argv[i], "--frames") == 0) {
      options.max_frames = atoi(argv[i + 1]);
      i++;
    } else if (strcmp(argv[i], "--porthole-socket") == 0) {
      options.porthole_socket = argv[i + 1];
      i++;
    } else if (strcmp(argv[i], "--session-id") == 0) {
      options.session_id = argv[i + 1];
      i++;
    } else if (strcmp(argv[i], "--mach-service") == 0) {
      options.transport_kind = FT_NATIVE_ATTACH_TRANSPORT_MACOS_XPC;
      options.endpoint = argv[i + 1];
      i++;
    } else if (strcmp(argv[i], "--transport-kind") == 0) {
      options.transport_kind = (uint32_t)atoi(argv[i + 1]);
      i++;
    } else if (strcmp(argv[i], "--endpoint") == 0) {
      options.endpoint = argv[i + 1];
      i++;
    } else if (strcmp(argv[i], "--token") == 0) {
      options.token = argv[i + 1];
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

static const ft_native_pool *native_pool_find(const native_pool_cache *cache, uint64_t pool_id) {
  for (uint32_t i = 0; i < cache->count; i++) {
    if (cache->pools[i].pool_id == pool_id) {
      return &cache->pools[i];
    }
  }
  return NULL;
}

static int native_pool_cache_add(native_pool_cache *cache, ft_native_pool pool) {
  if (native_pool_find(cache, pool.pool_id) != NULL) {
    return 0;
  }
  if (cache->count >= MAX_NATIVE_POOLS) {
    fprintf(stderr, "native pool cache full; cannot add pool %" PRIu64 "\n", pool.pool_id);
    return 1;
  }
  cache->pools[cache->count++] = pool;
  return 0;
}

static int native_poll_control_events(ft_native_attach *attach, native_pool_cache *cache) {
  for (;;) {
    ft_native_event event = {.struct_size = sizeof(ft_native_event)};
    ft_status status = ft_native_poll_event(attach, &event);
    if (status == FT_STATUS_EMPTY) {
      return 0;
    }
    if (status != FT_STATUS_OK) {
      fprintf(stderr, "ft_native_poll_event failed with status %d\n", status);
      return 1;
    }
    if (event.kind == FT_NATIVE_EVENT_POOL_ADDED) {
      ft_native_pool pool = {
          .struct_size = sizeof(ft_native_pool),
      };
      ft_status pool_status = ft_native_get_pool(attach, event.pool_id, &pool);
      if (pool_status != FT_STATUS_OK) {
        fprintf(stderr, "ft_native_get_pool(%" PRIu64 ") failed with status %d\n", event.pool_id, pool_status);
        return 1;
      }
      if (native_pool_cache_add(cache, pool)) {
        return 1;
      }
    } else if (event.kind == FT_NATIVE_EVENT_PRODUCER_STOPPED) {
      return 1;
    }
  }
}

// The native path: attach through the descriptor endpoint, then present each frame
// zero-copy with a GPU fence wait. Consumer ordering: wait for a newer cursor,
// acquire a frame lease, GPU-wait the producer fence, sample the surface, and
// release the lease when presentation work is complete.
static int run_native(const viewer_options *options) {
  if (options->endpoint == NULL || options->transport_kind == 0) {
    fprintf(stderr, "--native requires --transport-kind <kind> --endpoint <value> (and usually --token <secret>)\n");
    return 1;
  }

  ft_native_attach *attach = NULL;
  ft_native_attach_descriptor descriptor = {
      .struct_size = sizeof(ft_native_attach_descriptor),
      .transport_kind = options->transport_kind,
      .requested_consumer_id = 0,
      .endpoint = options->endpoint,
      .bearer_token = options->token,
      .flags = 0,
  };
  if (require_ok(ft_native_attach_connect(&descriptor, &attach), "ft_native_attach_connect")) {
    return 1;
  }
  ft_native_grant grant = {
      .struct_size = sizeof(ft_native_grant),
  };
  if (require_ok(ft_native_attach_grant(attach, &grant), "ft_native_attach_grant")) {
    ft_native_attach_destroy(attach);
    return 1;
  }
  native_pool_cache pools = {0};
  for (uint32_t i = 0; i < grant.pool_count; i++) {
    if (native_pool_cache_add(&pools, grant.pools[i])) {
      ft_native_attach_destroy(attach);
      return 1;
    }
  }

  if (SDL_Init(SDL_INIT_VIDEO) != 0) {
    fprintf(stderr, "SDL_Init failed: %s\n", SDL_GetError());
    ft_native_attach_destroy(attach);
    return 1;
  }
  SDL_Window *window = SDL_CreateWindow("capture-viewer-sdl (native)", SDL_WINDOWPOS_CENTERED, SDL_WINDOWPOS_CENTERED, WIDTH,
                                        HEIGHT, SDL_WINDOW_SHOWN | SDL_WINDOW_METAL | SDL_WINDOW_RESIZABLE);
  SDL_MetalView view = window != NULL ? SDL_Metal_CreateView(window) : NULL;
  void *layer = view != NULL ? SDL_Metal_GetLayer(view) : NULL;
  void *producer_sync = grant.producer_sync.sync_kind == FT_NATIVE_SYNC_MTL_SHARED_EVENT ? grant.producer_sync.handle.object : NULL;
  mp_presenter *presenter = layer != NULL ? mp_create(layer, producer_sync) : NULL;
  if (presenter == NULL) {
    fprintf(stderr, "native present setup failed: %s\n", SDL_GetError());
    if (view != NULL) {
      SDL_Metal_DestroyView(view);
    }
    if (window != NULL) {
      SDL_DestroyWindow(window);
    }
    SDL_Quit();
    ft_native_attach_destroy(attach);
    return 1;
  }

  int running = 1;
  int presented = 0;
  uint64_t last_cursor = 0;
  while (running && (options->max_frames <= 0 || presented < options->max_frames)) {
    SDL_Event sdl_event;
    while (SDL_PollEvent(&sdl_event)) {
      if (sdl_event.type == SDL_QUIT) {
        running = 0;
      }
    }
    if (native_poll_control_events(attach, &pools)) {
      break;
    }

    uint64_t ready_cursor = 0;
    ft_status wait_status = ft_native_wait_frame(attach, last_cursor, 16 * 1000 * 1000, &ready_cursor);
    ft_native_frame frame = {
        .struct_size = sizeof(ft_native_frame),
    };
    ft_status status = wait_status == FT_STATUS_OK ? ft_native_acquire_latest(attach, last_cursor, &frame) : wait_status;
    if (status == FT_STATUS_OK) {
      const ft_native_pool *pool = native_pool_find(&pools, frame.pool_id);
      if (pool != NULL && frame.slot_id < pool->surface_count) {
        const ft_native_surface *surface = &pool->surfaces[frame.slot_id];
        if (surface->handle_kind == FT_NATIVE_HANDLE_IOSURFACE) {
          (void)mp_present(presenter, surface->object, frame.producer_sync_value, frame.width, frame.height);
        }
      }
      last_cursor = frame.cursor;
      presented++;
      ft_native_release release = {
          .struct_size = sizeof(ft_native_release),
          .release_kind = FT_NATIVE_RELEASE_NOW,
          .lease_id = frame.lease_id,
          .release_sync_id = 0,
          .release_value = 0,
      };
      ft_native_release_frame(attach, &release);
    } else if (status == FT_STATUS_CLOSED) {
      break;
    } else {
      // EMPTY/TIMEOUT (no frame yet) or a read fault / gone producer: hold the
      // window alive with a placeholder and keep polling for (re)connection.
      mp_present_placeholder(presenter);
    }
  }

  mp_destroy(presenter);
  SDL_Metal_DestroyView(view);
  SDL_DestroyWindow(window);
  SDL_Quit();
  ft_native_attach_destroy(attach);
  return 0;
}

int main(int argc, char **argv) {
  viewer_options options = parse_options(argc, argv);

  if (options.native) {
    return run_native(&options);
  }

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
