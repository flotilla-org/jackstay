#include <SDL.h>
#include <inttypes.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "capture_transfer.h"
#include "metal_present.h"

enum { WIDTH = 320, HEIGHT = 180, STRIDE = WIDTH * 4 };

typedef struct viewer_options {
  int max_frames;
  const char *porthole_socket;
  const char *session_id;
  int native;
  uint32_t transport_kind;
  const char *endpoint;
  const char *token;
} viewer_options;

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

#ifdef __APPLE__
static int run_native(const viewer_options *options) {
  if (options->endpoint == NULL || options->transport_kind != FT_NATIVE_ATTACH_TRANSPORT_MACOS_XPC) {
    fprintf(stderr, "--native requires a macOS acquisition endpoint (--mach-service <name>)\n");
    return 1;
  }

  ft_macos_acquisition_connection *connection = NULL;
  ft_acquisition_consumer *consumer = NULL;
  ft_acquisition_cancellation *cancellation = NULL;
  SDL_Window *window = NULL;
  SDL_MetalView view = NULL;
  mp_presenter *presenter = NULL;
  int failed = 1;
  int sdl_started = 0;
  // Two outstanding frames allow presentation to overlap while keeping the
  // consumer's resource demand explicit and bounded at admission.
  if (require_ok(ft_acquisition_macos_connect(options->endpoint, options->token, 2,
                                             &connection, &consumer), "ft_acquisition_macos_connect") ||
      require_ok(ft_acquisition_cancellation_create(&cancellation), "ft_acquisition_cancellation_create")) {
    goto cleanup;
  }
  if (SDL_Init(SDL_INIT_VIDEO) != 0) {
    fprintf(stderr, "SDL_Init failed: %s\n", SDL_GetError());
    goto cleanup;
  }
  sdl_started = 1;
  window = SDL_CreateWindow("capture-viewer-sdl (native)", SDL_WINDOWPOS_CENTERED, SDL_WINDOWPOS_CENTERED,
                            WIDTH, HEIGHT, SDL_WINDOW_SHOWN | SDL_WINDOW_METAL | SDL_WINDOW_RESIZABLE);
  view = window != NULL ? SDL_Metal_CreateView(window) : NULL;
  void *layer = view != NULL ? SDL_Metal_GetLayer(view) : NULL;
  presenter = layer != NULL ? mp_create(layer) : NULL;
  if (presenter == NULL) {
    fprintf(stderr, "native present setup failed: %s\n", SDL_GetError());
    goto cleanup;
  }

  failed = 0;
  uint64_t submitted = 0;
  uint64_t last_cursor = 0;
  uint64_t requested_configuration_epoch = 0;
  int requested_configuration = 0;
  int running = 1;
  while (running && (options->max_frames <= 0 || submitted < (uint64_t)options->max_frames)) {
    SDL_Event event;
    while (SDL_PollEvent(&event)) {
      if (event.type == SDL_QUIT) running = 0;
    }
    if (!running) break;
    if (mp_failed(presenter)) { failed = 1; break; }

    // Snapshot before acquisition so publication/release/reconfiguration
    // between the check and wait cannot be lost.
    ft_acquisition_events before = {0};
    if (require_ok(ft_acquisition_snapshot(consumer, &before), "ft_acquisition_snapshot")) {
      failed = 1;
      break;
    }
    ft_acquired_frame *frame = NULL;
    ft_acquisition_range range = {0};
    ft_status status = ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, last_cursor, &frame, &range);
    uint32_t interest = FT_WAIT_DATA;
    if (status == FT_STATUS_OK) {
      ft_acquired_frame_descriptor descriptor = {0};
      if (require_ok(ft_acquired_frame_describe(frame, &descriptor), "ft_acquired_frame_describe") ||
          mp_present(presenter, &frame) != 0) {
        // Presentation failures leave the frame owned here and submit no GPU
        // work. Successful submission transfers it to the completion owner.
        if (frame != NULL) ft_acquired_frame_release(&frame);
        failed = 1;
        break;
      }
      last_cursor = descriptor.cursor;
      submitted++;
      continue;
    }
    if (status == FT_STATUS_CLOSED) break;
    if (status == FT_STATUS_RECONFIGURATION) {
      interest = FT_WAIT_ALL;
      if (!requested_configuration || requested_configuration_epoch != before.reconfiguration_epoch) {
        requested_configuration = 1;
        requested_configuration_epoch = before.reconfiguration_epoch;
        if (require_ok(ft_acquisition_relinquish_configuration(consumer), "ft_acquisition_relinquish_configuration")) {
          failed = 1;
          break;
        }
        status = ft_acquisition_macos_install_configuration(connection, consumer);
        if (status == FT_STATUS_OK) continue;
        if (status == FT_STATUS_CLOSED) break;
        if (status != FT_STATUS_EMPTY && status != FT_STATUS_STALE) {
          fprintf(stderr, "native configuration failed with status %d\n", status);
          failed = 1;
          break;
        }
      }
    } else if (status == FT_STATUS_HOLDING_LIMIT) {
      interest = FT_WAIT_CAPACITY;
    } else if (status != FT_STATUS_EMPTY && status != FT_STATUS_MISS) {
      fprintf(stderr, "native acquisition failed with status %d\n", status);
      failed = 1;
      break;
    }
    ft_acquisition_events after = {0};
    status = ft_acquisition_wait(consumer, &before, interest, cancellation, 16 * 1000 * 1000, &after);
    if (status == FT_STATUS_CLOSED || status == FT_STATUS_CANCELLED) break;
    if (status != FT_STATUS_OK && status != FT_STATUS_TIMEOUT) {
      fprintf(stderr, "native acquisition wait failed with status %d\n", status);
      failed = 1;
      break;
    }
  }

  // A timeout reports failure; it never releases resources still in GPU use.
  if (mp_drain(presenter, 5 * UINT64_C(1000) * 1000 * 1000) != 0) failed = 1;
  uint64_t completed = mp_completed_frames(presenter);
  if (options->max_frames > 0 && completed != (uint64_t)options->max_frames) {
    fprintf(stderr, "expected %d native frames, completed %" PRIu64 "\n", options->max_frames, completed);
    failed = 1;
  }
  printf("presented_frames=%" PRIu64 "\n", completed);

cleanup:
  mp_destroy(presenter);
  if (view != NULL) SDL_Metal_DestroyView(view);
  if (window != NULL) SDL_DestroyWindow(window);
  if (sdl_started) SDL_Quit();
  ft_acquisition_cancellation_destroy(&cancellation);
  ft_acquisition_consumer_destroy(&consumer);
  ft_acquisition_macos_connection_destroy(&connection);
  return failed ? 1 : 0;
}

#else
static int run_native(const viewer_options *options) {
  (void)options;
  fprintf(stderr, "native SDL presentation requires macOS/Metal; use the Linux Vulkan reference checks for dmabuf frames\n");
  return 1;
}
#endif

#if defined(__APPLE__) || defined(__linux__)
static int run_cpu(const viewer_options *options) {
  ft_synthetic_session synthetic = {0};
  const char *session_id = options->session_id;
  if (options->porthole_socket != NULL && session_id == NULL) {
    if (require_ok(ft_create_synthetic_session(options->porthole_socket, &synthetic), "ft_create_synthetic_session")) return 1;
    session_id = synthetic.session_id;
  }
  ft_cpu_acquisition_connection *connection = NULL;
  ft_cpu_producer *producer = NULL;
  uint8_t *pixels = NULL;
  ft_acquisition_consumer *consumer = NULL;
  ft_acquisition_cancellation *cancellation = NULL;
  uint64_t track = 0, acquired = 0, requested_epoch = 0;
  int failed = 1, sdl_started = 0, running = 1, requested_configuration = 0;
  uint32_t width = 0, height = 0, format = 0;
  SDL_Window *window = NULL;
  SDL_Renderer *renderer = NULL;
  SDL_Texture *texture = NULL;
  if (options->porthole_socket != NULL) {
    if (require_ok(ft_acquisition_cpu_connect_session(options->porthole_socket, session_id,
                      options->token != NULL ? options->token : getenv("PORTHOLE_AGENT_TOKEN"), 2,
                      &connection, &consumer, &track), "ft_acquisition_cpu_connect_session")) goto cleanup;
  } else {
    ft_cpu_producer_config config = {
      .resource_capacity = 6, .retained_history = 2, .producer_reserve = 1,
      .max_incarnations = 2, .payload_capacity = STRIDE * HEIGHT,
      .memory_budget = 8 * 1024 * 1024, .drain_timeout_ns = 5 * UINT64_C(1000000000),
    };
    if (require_ok(ft_cpu_producer_create(&config, &producer), "ft_cpu_producer_create") ||
        require_ok(ft_cpu_producer_attach(producer, 2, &consumer), "ft_cpu_producer_attach")) goto cleanup;
    pixels = malloc((size_t)STRIDE * HEIGHT);
    if (pixels == NULL) { fprintf(stderr, "synthetic pixel allocation failed\n"); goto cleanup; }
  }
  if (require_ok(ft_acquisition_cancellation_create(&cancellation), "ft_acquisition_cancellation_create")) goto cleanup;
  if (SDL_Init(SDL_INIT_VIDEO) != 0) { fprintf(stderr, "SDL init: %s\n", SDL_GetError()); goto cleanup; }
  sdl_started = 1;
  window = SDL_CreateWindow("capture-viewer-sdl", SDL_WINDOWPOS_CENTERED, SDL_WINDOWPOS_CENTERED, WIDTH, HEIGHT, SDL_WINDOW_SHOWN);
  if (window != NULL) {
    renderer = SDL_CreateRenderer(window, -1, SDL_RENDERER_ACCELERATED | SDL_RENDERER_PRESENTVSYNC);
    if (renderer == NULL) renderer = SDL_CreateRenderer(window, -1, SDL_RENDERER_SOFTWARE);
  }
  if (renderer == NULL) { fprintf(stderr, "SDL setup: %s\n", SDL_GetError()); goto cleanup; }
  failed = 0;
  while (running && (options->max_frames <= 0 || acquired < (uint64_t)options->max_frames)) {
    SDL_Event event;
    while (SDL_PollEvent(&event)) if (event.type == SDL_QUIT) running = 0;
    if (!running) break;
    uint64_t published_cursor = 0;
    if (producer != NULL) {
      fill_frame(pixels, acquired + 1);
      ft_acquired_frame_descriptor descriptor = {
        .sequence = acquired + 1, .timestamp_ns = (acquired + 1) * UINT64_C(16666667),
        .width = WIDTH, .height = HEIGHT, .stride = STRIDE, .pixel_format = FT_PIXEL_FORMAT_BGRA8_UNORM,
      };
      ft_status publication = ft_cpu_producer_publish(producer, &descriptor, pixels, (size_t)STRIDE * HEIGHT, &published_cursor);
      if (publication != FT_STATUS_OK && publication != FT_STATUS_DROPPED) {
        fprintf(stderr, "CPU publication: %d\n", publication); failed = 1; break;
      }
    }
    ft_acquisition_events before = {0};
    if (require_ok(ft_acquisition_snapshot(consumer, &before), "ft_acquisition_snapshot")) { failed = 1; break; }
    ft_acquired_frame *frame = NULL;
    ft_acquisition_range range = {0};
    // Repeated presentations may acquire the same latest frame. This also
    // supports the daemon's static synthetic session with a bounded frame count.
    ft_status status = ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, &frame, &range);
    uint32_t interest = FT_WAIT_DATA;
    if (status == FT_STATUS_OK) {
      ft_acquired_frame_descriptor desc = {0};
      const uint8_t *bytes = NULL;
      size_t len = 0;
      if (require_ok(ft_acquired_frame_describe(frame, &desc), "ft_acquired_frame_describe") ||
          require_ok(ft_acquired_frame_bytes(frame, &bytes, &len), "ft_acquired_frame_bytes")) {
        ft_acquired_frame_release(&frame); failed = 1; break;
      }
      if (producer != NULL && published_cursor != 0 &&
          (desc.cursor != published_cursor || desc.sequence != acquired + 1 ||
           len != (size_t)STRIDE * HEIGHT || memcmp(bytes, pixels, len) != 0)) {
        fprintf(stderr, "synthetic acquisition payload/sequence mismatch\n");
        ft_acquired_frame_release(&frame); failed = 1; break;
      }
      uint32_t pixel_format = desc.pixel_format == FT_PIXEL_FORMAT_BGRA8_UNORM ? SDL_PIXELFORMAT_BGRA32 :
                              desc.pixel_format == FT_PIXEL_FORMAT_RGBA8_UNORM ? SDL_PIXELFORMAT_RGBA32 : 0;
      if (!pixel_format || !desc.width || !desc.height || desc.width > INT_MAX || desc.height > INT_MAX ||
          desc.stride > INT_MAX || (uint64_t)desc.width * 4 > desc.stride || (uint64_t)desc.stride * desc.height != len) {
        fprintf(stderr, "invalid CPU frame layout\n"); ft_acquired_frame_release(&frame); failed = 1; break;
      }
      if (desc.width != width || desc.height != height || format != pixel_format) {
        SDL_Texture *replacement = SDL_CreateTexture(renderer, pixel_format, SDL_TEXTUREACCESS_STREAMING, (int)desc.width, (int)desc.height);
        if (!replacement) { fprintf(stderr, "SDL texture: %s\n", SDL_GetError()); ft_acquired_frame_release(&frame); failed = 1; break; }
        SDL_DestroyTexture(texture); texture = replacement;
        width = desc.width; height = desc.height; format = pixel_format;
        SDL_SetWindowSize(window, width < WIDTH ? WIDTH : (int)width, height < HEIGHT ? HEIGHT : (int)height);
      }
      int updated = SDL_UpdateTexture(texture, NULL, bytes, (int)desc.stride);
      // SDL_UpdateTexture copies the CPU bytes. Subsequent rendering uses SDL's
      // texture, so no submitted GPU work retains this acquisition mapping.
      if (require_ok(ft_acquired_frame_release(&frame), "ft_acquired_frame_release") || updated != 0 ||
          SDL_RenderClear(renderer) != 0 || SDL_RenderCopy(renderer, texture, NULL, NULL) != 0) {
        fprintf(stderr, "SDL render: %s\n", SDL_GetError()); failed = 1; break;
      }
      SDL_RenderPresent(renderer);
      acquired++;
      SDL_Delay(16);
      continue;
    }
    if (status == FT_STATUS_CLOSED) break;
    if (status == FT_STATUS_RECONFIGURATION) {
      interest = FT_WAIT_ALL;
      if (!requested_configuration || requested_epoch != before.reconfiguration_epoch) {
        requested_configuration = 1; requested_epoch = before.reconfiguration_epoch;
        if (require_ok(ft_acquisition_relinquish_configuration(consumer), "ft_acquisition_relinquish_configuration")) { failed = 1; break; }
        status = producer != NULL ? ft_cpu_producer_configure_consumer(producer, consumer) :
                                   ft_acquisition_cpu_install_configuration(connection, consumer);
        if (status == FT_STATUS_OK) continue;
        if (status == FT_STATUS_CLOSED) break;
        if (status != FT_STATUS_EMPTY && status != FT_STATUS_STALE) { fprintf(stderr, "CPU configuration: %d\n", status); failed = 1; break; }
      }
    } else if (status == FT_STATUS_HOLDING_LIMIT) {
      interest = FT_WAIT_CAPACITY;
    } else if (status != FT_STATUS_EMPTY && status != FT_STATUS_MISS) {
      fprintf(stderr, "CPU acquisition: %d\n", status); failed = 1; break;
    }
    ft_acquisition_events after = {0};
    status = ft_acquisition_wait(consumer, &before, interest, cancellation, 16 * 1000 * 1000, &after);
    if (status == FT_STATUS_CLOSED || status == FT_STATUS_CANCELLED) break;
    if (status != FT_STATUS_OK && status != FT_STATUS_TIMEOUT) { fprintf(stderr, "CPU wait: %d\n", status); failed = 1; break; }
  }
  if (options->max_frames > 0 && acquired != (uint64_t)options->max_frames) failed = 1;
  printf("acquired_frames=%" PRIu64 "\n", acquired);
cleanup:
  SDL_DestroyTexture(texture);
  SDL_DestroyRenderer(renderer);
  SDL_DestroyWindow(window);
  if (sdl_started) SDL_Quit();
  ft_acquisition_cancellation_destroy(&cancellation);
  ft_acquisition_consumer_destroy(&consumer);
  ft_acquisition_cpu_connection_destroy(&connection);
  free(pixels);
  if (require_ok(ft_cpu_producer_destroy(&producer), "ft_cpu_producer_destroy")) failed = 1;
  return failed ? 1 : 0;
}
#else
static int run_cpu(const viewer_options *options) {
  (void)options;
  fprintf(stderr, "CPU session transport requires macOS or Linux\n");
  return 1;
}
#endif

int main(int argc, char **argv) {
  if (ft_abi_version() != FT_ABI_VERSION) {
    fprintf(stderr, "Jackstay ABI mismatch: rebuild the viewer and library together\n");
    return 1;
  }
  viewer_options options = parse_options(argc, argv);
  return options.native ? run_native(&options) : run_cpu(&options);
}
