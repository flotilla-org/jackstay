#ifdef __linux__
#define _GNU_SOURCE
#endif
#include <SDL.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#if defined(__APPLE__) || defined(__linux__)
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#endif

#include "capture_transfer.h"
#include "metal_present.h"
#include "viewer_input.h"
#include "jackstay_bootstrap.h"

#include "synthetic.h"

// Sizes the window to the frame's dimensions, scaled down uniformly if the
// frame is larger than most of the display, so the presented aspect is the
// frame's. Points, not pixels: SDL windows are sized in points.
static void fit_window_to_frame(SDL_Window *window, uint32_t frame_width, uint32_t frame_height) {
  if (window == NULL || frame_width == 0 || frame_height == 0) return;
  double width = (double)frame_width, height = (double)frame_height;
  SDL_DisplayMode mode;
  int display = SDL_GetWindowDisplayIndex(window);
  if (display >= 0 && SDL_GetCurrentDisplayMode(display, &mode) == 0 && mode.w > 0 && mode.h > 0) {
    double max_width = mode.w * 0.85, max_height = mode.h * 0.85;
    double scale = 1.0;
    if (width > max_width) scale = max_width / width;
    if (height * scale > max_height) scale = max_height / height;
    width *= scale;
    height *= scale;
  }
  if (width < 64) width = 64;
  if (height < 64) height = 64;
  SDL_SetWindowSize(window, (int)width, (int)height);
}


typedef struct viewer_options {
  int max_frames;
  uint32_t hold_ms;
  int invalid;
  const char *cpu_socket;
  const char *source_socket;
  uint32_t bootstrap_input;
  int input_policy_set;
  const char *input_socket;
  int input_self_test;
  const char *porthole_socket;
  const char *session_id;
  int native;
  uint32_t transport_kind;
  const char *endpoint;
  const char *token;
} viewer_options;

static viewer_options parse_options(int argc, char **argv) {
  viewer_options options = {.bootstrap_input = FT_BOOTSTRAP_INPUT_OPTIONAL};
  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "--input-self-test") == 0) {
      options.input_self_test = 1;
    } else if (strcmp(argv[i], "--source-socket") == 0) {
      if (++i >= argc || !argv[i][0]) { options.invalid = 1; return options; }
      options.source_socket = argv[i];
    } else if (strcmp(argv[i], "--observe") == 0) {
      options.bootstrap_input = FT_BOOTSTRAP_INPUT_NONE; options.input_policy_set = 1;
    } else if (strcmp(argv[i], "--require-input") == 0) {
      options.bootstrap_input = FT_BOOTSTRAP_INPUT_REQUIRED; options.input_policy_set = 1;
    } else if (strcmp(argv[i], "--input-socket") == 0) {
      if (++i >= argc || !argv[i][0]) { options.invalid = 1; return options; }
      options.input_socket = argv[i];
    } else if (strcmp(argv[i], "--native") == 0) {
      options.native = 1;
    } else if (strcmp(argv[i], "--hold-ms") == 0) {
      if (++i >= argc || argv[i][0] < '0' || argv[i][0] > '9') {
        fprintf(stderr, "--hold-ms requires an unsigned millisecond count\n");
        options.invalid = 1;
        return options;
      }
      errno = 0;
      char *end = NULL;
      unsigned long value = strtoul(argv[i], &end, 10);
      if (errno || *end != '\0' || value > UINT32_MAX) {
        fprintf(stderr, "invalid --hold-ms value\n");
        options.invalid = 1;
        return options;
      }
      options.hold_ms = (uint32_t)value;
    } else if (strcmp(argv[i], "--cpu-socket") == 0) {
      if (++i >= argc || argv[i][0] == '\0') {
        fprintf(stderr, "--cpu-socket requires a Unix socket path\n");
        options.invalid = 1;
        return options;
      }
      options.cpu_socket = argv[i];
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
  if (options.cpu_socket != NULL && (options.native || options.porthole_socket != NULL || options.session_id != NULL)) {
    fprintf(stderr, "--cpu-socket cannot be combined with native or Porthole session selection\n");
    options.invalid = 1;
  }
  if ((options.source_socket && (options.cpu_socket || options.input_socket || options.native || options.porthole_socket || options.session_id)) ||
      (options.input_policy_set && !options.source_socket)) {
    fprintf(stderr, "bootstrap input policy requires --source-socket, which selects one source\n"); options.invalid = 1;
  }
  if ((options.input_socket && (options.native || !options.cpu_socket)) ||
      (options.input_self_test && !options.input_socket && (!options.source_socket || options.bootstrap_input == FT_BOOTSTRAP_INPUT_NONE))) {
    fprintf(stderr, "input requires a generic CPU socket source; self-test requires input\n"); options.invalid = 1;
  }
  if (options.input_self_test && options.source_socket) options.bootstrap_input = FT_BOOTSTRAP_INPUT_REQUIRED;
  return options;
}

/* Keep the actual lease while deliberately delaying consumption. Pump events
 * so even a long requested delay can be cancelled by closing this window. */
static int hold_frame(uint32_t remaining_ms, viewer_input *input, SDL_Window *window) {
  while (remaining_ms != 0) {
    uint32_t chunk = remaining_ms < 16 ? remaining_ms : 16;
    SDL_Delay(chunk);
    remaining_ms -= chunk;
    SDL_Event event;
    while (SDL_PollEvent(&event)) { if (event.type == SDL_QUIT) return 0; viewer_input_event(input, &event, window); }
    viewer_input_poll(input);
  }
  return 1;
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
  uint32_t shown_width = 0, shown_height = 0;
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
      if (!hold_frame(options->hold_ms, NULL, window)) {
        ft_acquired_frame_release(&frame);
        break;
      }
      ft_acquired_frame_descriptor descriptor = {0};
      if (require_ok(ft_acquired_frame_describe(frame, &descriptor), "ft_acquired_frame_describe")) {
        if (frame != NULL) ft_acquired_frame_release(&frame);
        failed = 1;
        break;
      }
      if (descriptor.width != shown_width || descriptor.height != shown_height) {
        // Follow the frame's aspect: the window starts at the synthetic size and
        // a captured portrait window would otherwise be squashed into it.
        fit_window_to_frame(window, descriptor.width, descriptor.height);
        shown_width = descriptor.width;
        shown_height = descriptor.height;
      }
      if (mp_present(presenter, &frame) != 0) {
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
/* Generic CPU setup belongs to the connecting process. Selection and desktop
 * authorization, when needed, remain with the host that supplies this path. */
static int connect_cpu_socket(const char *path, ft_cpu_acquisition_connection **connection,
                              ft_acquisition_consumer **consumer, const viewer_options *options, viewer_input *input) {
  struct sockaddr_un address = {0};
  address.sun_family = AF_UNIX;
  if (strlen(path) >= sizeof(address.sun_path)) {
    fprintf(stderr, "CPU socket path is too long\n");
    return 1;
  }
  strcpy(address.sun_path, path);
#ifdef __APPLE__
  address.sun_len = (uint8_t)(offsetof(struct sockaddr_un, sun_path) + strlen(path) + 1);
#endif
  int32_t fd = socket(AF_UNIX, SOCK_STREAM, 0);
  if (fd < 0) { perror("CPU socket"); return 1; }
  int failed = 1;
  if (fcntl(fd, F_SETFD, FD_CLOEXEC) != 0) { perror("CPU socket close-on-exec"); goto cleanup; }
  if (connect(fd, (struct sockaddr *)&address, sizeof(address)) != 0) {
    perror("CPU socket connect");
    goto cleanup;
  }
#ifdef __APPLE__
  uid_t uid;
  gid_t gid;
  if (getpeereid(fd, &uid, &gid) != 0) { perror("CPU peer identity"); goto cleanup; }
#else
  struct ucred credentials;
  socklen_t size = sizeof(credentials);
  if (getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &credentials, &size) != 0 || size != sizeof(credentials)) {
    fprintf(stderr, "CPU peer identity unavailable\n");
    goto cleanup;
  }
  uid_t uid = credentials.uid;
#endif
  if (uid != geteuid()) { fprintf(stderr, "CPU socket peer is not the current user\n"); goto cleanup; }
  if (options->source_socket) {
    ft_status input_status;
    uint32_t mode = options->bootstrap_input == FT_BOOTSTRAP_INPUT_NONE ? 0 : FT_INPUT_MODE_COOPERATIVE;
    if (require_ok(ft_source_bootstrap_connect(&fd, options->bootstrap_input, mode, &input->client, &input_status), "ft_source_bootstrap_connect")) goto cleanup;
    if (input->client) {
      uint64_t controller, epoch;
      if (require_ok(ft_input_client_describe(input->client, &input->config, &controller, &epoch), "ft_input_client_describe")) goto cleanup;
    } else if (input_status != FT_STATUS_EMPTY) {
      fprintf(stderr, "source input unavailable: %d; continuing observation\n", input_status);
    }
  }
  if (require_ok(ft_acquisition_cpu_connection_create(&fd, connection), "ft_acquisition_cpu_connection_create") ||
      require_ok(ft_acquisition_cpu_attach(*connection, 1, consumer), "ft_acquisition_cpu_attach")) goto cleanup;
  failed = 0;
cleanup:
  if (fd >= 0) close(fd);
  return failed;
}

static int run_cpu(const viewer_options *options) {
  ft_synthetic_session synthetic = {0};
  const char *session_id = options->session_id;
  if (options->porthole_socket != NULL && session_id == NULL) {
    if (require_ok(ft_create_synthetic_session(options->porthole_socket, &synthetic), "ft_create_synthetic_session")) return 1;
    session_id = synthetic.session_id;
  }
  viewer_input input = {0};
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
  if (options->cpu_socket != NULL || options->source_socket != NULL) {
    if (connect_cpu_socket(options->source_socket ? options->source_socket : options->cpu_socket, &connection, &consumer, options, &input)) goto cleanup;
  } else if (options->porthole_socket != NULL) {
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
  if (options->input_socket && viewer_input_open(&input, options->input_socket) != 0) { fprintf(stderr, "input connection failed\n"); goto cleanup; }
  if (input.client) SDL_StartTextInput();
  int input_test_sent = 0;
  failed = 0;
  while (running && (options->max_frames <= 0 || acquired < (uint64_t)options->max_frames)) {
    SDL_Event event;
    while (SDL_PollEvent(&event)) { if (event.type == SDL_QUIT) running = 0; viewer_input_event(&input, &event, window); }
    viewer_input_poll(&input);
    if (input.failed) { failed = 1; break; }
    if (options->input_self_test && !input_test_sent && acquired >= 2 && !input.resetting) {
      viewer_input_self_test(&input, window); input_test_sent = 1;
    }
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
      if (options->hold_ms != 0) {
        uint8_t *before_hold = malloc(len);
        if (before_hold == NULL) {
          fprintf(stderr, "held-frame validation allocation failed\n");
          ft_acquired_frame_release(&frame); failed = 1; break;
        }
        memcpy(before_hold, bytes, len);
        int continuing = hold_frame(options->hold_ms, &input, window);
        int unchanged = memcmp(before_hold, bytes, len) == 0;
        free(before_hold);
        if (!unchanged || !continuing) {
          if (!unchanged) { fprintf(stderr, "acquired CPU pixels changed while held\n"); failed = 1; }
          ft_acquired_frame_release(&frame);
          break;
        }
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
  if (viewer_input_close(&input)) failed = 1;
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
  if (options.invalid) return 1;
  return options.native ? run_native(&options) : run_cpu(&options);
}
