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
#include <math.h>
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
#include "viewer_affordances.h"
#include "viewer_fit.h"
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
  const char *source_endpoint;
  int session_scope;
  uint32_t affordances;
  int affordances_policy_set;
  int log_affordances;
  uint32_t typing;
  int typing_set;
  uint32_t bootstrap_input;
  int input_policy_set;
  const char *input_socket;
  int input_self_test;
  int window_self_test;
  int scroll_self_test;
  const char *porthole_socket;
  const char *session_id;
  int native;
  uint32_t transport_kind;
  const char *endpoint;
  const char *token;
} viewer_options;

static viewer_options parse_options(int argc, char **argv) {
  viewer_options options = {.bootstrap_input = FT_BOOTSTRAP_INPUT_OPTIONAL, .affordances = 1, .typing = FT_INPUT_MODE_COOPERATIVE};
  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "--scroll-self-test") == 0) {
      options.scroll_self_test = 1;
    } else if (strcmp(argv[i], "--window-self-test") == 0) {
      options.window_self_test = 1;
    } else if (strcmp(argv[i], "--input-self-test") == 0) {
      options.input_self_test = 1;
    } else if (strcmp(argv[i], "--source-endpoint") == 0) {
      if (++i >= argc || !argv[i][0]) { fprintf(stderr, "--source-endpoint requires a name\n"); options.invalid = 1; return options; }
      options.source_endpoint = argv[i];
    } else if (strcmp(argv[i], "--session-scope") == 0) {
      options.session_scope = 1;
    } else if (strcmp(argv[i], "--log-affordances") == 0) {
      options.log_affordances = 1;
    } else if (strcmp(argv[i], "--affordances") == 0) {
      options.affordances_policy_set = 1;
      if (++i >= argc) { fprintf(stderr, "--affordances requires none|optional|required\n"); options.invalid = 1; return options; }
      if (!strcmp(argv[i], "none")) options.affordances = 0;
      else if (!strcmp(argv[i], "optional")) options.affordances = 1;
      else if (!strcmp(argv[i], "required")) options.affordances = 2;
      else { fprintf(stderr, "invalid --affordances: %s\n", argv[i]); options.invalid = 1; return options; }
    } else if (strcmp(argv[i], "--typing") == 0) {
      options.typing_set = 1;
      if (++i >= argc) { fprintf(stderr, "--typing requires cooperative|text|physical\n"); options.invalid = 1; return options; }
      if (!strcmp(argv[i], "cooperative")) options.typing = FT_INPUT_MODE_COOPERATIVE;
      else if (!strcmp(argv[i], "text")) options.typing = FT_INPUT_MODE_SOURCE_TEXT;
      else if (!strcmp(argv[i], "physical")) options.typing = FT_INPUT_MODE_PHYSICAL;
      else { fprintf(stderr, "invalid --typing: %s\n", argv[i]); options.invalid = 1; return options; }
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
  if ((options.source_endpoint && (options.source_socket || options.cpu_socket || options.input_socket || options.native || options.porthole_socket || options.session_id)) ||
      ((options.affordances_policy_set || options.log_affordances) && !options.source_endpoint && !options.source_socket) ||
      (options.session_scope && !options.source_endpoint) ||
      (options.typing_set && !options.source_endpoint && !options.source_socket)) {
    fprintf(stderr, "--source-endpoint selects one source; affordances require a bootstrap source; session scope requires an endpoint\n"); options.invalid = 1;
  }
  if ((options.source_socket && (options.cpu_socket || options.input_socket || options.native || options.porthole_socket || options.session_id)) ||
      (options.input_policy_set && !options.source_socket && !options.source_endpoint)) {
    fprintf(stderr, "bootstrap input policy requires --source-socket or --source-endpoint\n"); options.invalid = 1;
  }
  if ((options.input_socket && (options.native || !options.cpu_socket)) ||
      (options.input_self_test && !options.input_socket && ((!options.source_socket && !options.source_endpoint) || options.bootstrap_input == FT_BOOTSTRAP_INPUT_NONE))) {
    fprintf(stderr, "input requires a generic CPU socket source; self-test requires input\n"); options.invalid = 1;
  }
  /* The input self-test cannot succeed with observation-only fallback. */
  if (options.input_self_test && (options.source_socket || options.source_endpoint)) options.bootstrap_input = FT_BOOTSTRAP_INPUT_REQUIRED;
  return options;
}

/* Route overlay gestures before input, using the same drawable fit as rendering. */
static int viewer_event(viewer_input *input, SDL_Window *window, viewer_affordances *a, const SDL_Event *event) {
  int w, h, dw = 0, dh = 0; SDL_GetWindowSize(window, &w, &h);
  SDL_GetRendererOutputSize(a->renderer, &dw, &dh);
  int consumed = 0;
  if (w > 0 && h > 0)
    consumed = viewer_scroll_event(&a->scroll, a->host, event,
      viewer_fit(dw, dh, input->frame_width, input->frame_height),
      (double)dw / w, (double)dh / h, input->buttons != 0, SDL_GetTicks());
  if (!consumed) viewer_input_event(input, event, window);
  viewer_affordances_event(a, event);
  return consumed < 0;
}

/* Offline integration driver uses the real SDL queue and normal routing. */
static void scroll_self_test(viewer_affordances *a, SDL_Window *window, int *stage) {
  viewer_scroll *s = &a->scroll;
  if (!s->present) return;
  double x = s->snapshot.x.position, y = s->snapshot.y.position;
  if (((s->snapshot.capabilities & 3) == 3 && ((*stage == 1 && y < 500) || (*stage == 2 && y > 500) ||
      (*stage == 3 && x < 500) || (*stage == 4 && x > 500))) || *stage >= 5) return;
  int px = *stage < 2 ? 636 : *stage < 4 ? 96 : 636;
  int py = *stage == 0 ? 96 : *stage == 1 ? 5 : *stage < 4 ? 476 : 200;
  if (*stage == 3) px = 5;
  SDL_Event e = {.button = {.type = SDL_MOUSEBUTTONDOWN, .button = SDL_BUTTON_LEFT, .x = px, .y = py}};
  if (*stage == 4) {
    SDL_WarpMouseInWindow(window, px, py);
    e = (SDL_Event){.wheel = {.type = SDL_MOUSEWHEEL, .y = -2}};
#if SDL_VERSION_ATLEAST(2, 0, 18)
    e.wheel.preciseY = -2;
#endif
    SDL_PushEvent(&e);
  } else {
    SDL_PushEvent(&e);
    if (*stage == 0 || *stage == 2) {
      for (int i = 0; i < 3; i++) {
        SDL_Event motion = {.motion = {.type = SDL_MOUSEMOTION,
          .x = *stage == 0 ? px : 380 + 10 * i, .y = *stage == 0 ? 300 + 10 * i : py}};
        SDL_PushEvent(&motion);
      }
      e.button.x = *stage == 0 ? px : 400; e.button.y = *stage == 0 ? 320 : py;
    }
    e.type = SDL_MOUSEBUTTONUP; SDL_PushEvent(&e);
  }
  (*stage)++;
}

/* Keep the actual lease while deliberately delaying consumption. Pump events
 * so even a long requested delay can be cancelled by closing this window. */
static int hold_frame(uint32_t remaining_ms, viewer_input *input, SDL_Window *window, viewer_affordances *affordances, int log_affordances) {
  while (remaining_ms != 0) {
    uint32_t chunk = remaining_ms < 16 ? remaining_ms : 16;
    SDL_Delay(chunk);
    remaining_ms -= chunk;
    SDL_Event event;
    while (SDL_PollEvent(&event)) { if (event.type == SDL_QUIT) return 0; if (viewer_event(input, window, affordances, &event)) return -1; }
    viewer_input_poll(input);
    if (affordances && (viewer_affordances_poll(affordances, log_affordances) || viewer_affordances_tick(affordances))) return -1;
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
      if (!hold_frame(options->hold_ms, NULL, window, NULL, 0)) {
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
                              ft_acquisition_consumer **consumer, const viewer_options *options, viewer_input *input, viewer_affordances *affordances) {
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
    uint32_t mode = options->bootstrap_input == FT_BOOTSTRAP_INPUT_NONE ? 0 : options->typing;
    if (options->affordances_policy_set || options->log_affordances) {
      ft_status aff_status;
      if (require_ok(ft_source_bootstrap_connect_v2(&fd, options->bootstrap_input, mode,
          options->affordances, &input->client, &input_status, &affordances->host, &aff_status), "source bootstrap v2")) goto cleanup;
    } else if (require_ok(ft_source_bootstrap_connect(&fd, options->bootstrap_input, mode,
                 &input->client, &input_status), "ft_source_bootstrap_connect")) goto cleanup;
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
  viewer_input input = {.mode = options->typing};
  viewer_affordances affordances = {0};
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
  if (options->source_endpoint) {
    ft_local_connection *local = NULL;
    ft_local_endpoint endpoint = {options->session_scope ? FT_ENDPOINT_SCOPE_SESSION : FT_ENDPOINT_SCOPE_USER,
                                  FT_ENDPOINT_TRANSPORT_LOCAL_STREAM, options->source_endpoint};
    ft_status input_status, affordances_status;
    ft_status setup = ft_local_connect(&endpoint, &local);
    if (setup == FT_STATUS_OK) setup = ft_source_bootstrap_connect_v2_local(&local, options->bootstrap_input,
        options->bootstrap_input == FT_BOOTSTRAP_INPUT_NONE ? 0 : options->typing,
        options->affordances, &input.client, &input_status, &affordances.host, &affordances_status);
    if (setup != FT_STATUS_OK) {
      fprintf(stderr, "source endpoint bootstrap v2 failed: status=%d%s\n", setup,
              options->affordances == 2 ? "; required affordances must be offered by a v2 source" : "");
      ft_local_connection_destroy(&local); goto cleanup;
    }
    if (input.client) {
      uint64_t controller, epoch;
      if (require_ok(ft_input_client_describe(input.client, &input.config, &controller, &epoch), "input describe")) {
        ft_local_connection_destroy(&local); goto cleanup;
      }
    } else if (input_status != FT_STATUS_EMPTY) fprintf(stderr, "source input unavailable: %d; continuing observation\n", input_status);
    if (affordances_status == FT_STATUS_UNSUPPORTED) fprintf(stderr, "source affordances unavailable; continuing media\n");
    setup = ft_acquisition_cpu_connection_create_local(&local, &connection);
    ft_local_connection_destroy(&local);
    if (require_ok(setup, "CPU local setup") || require_ok(ft_acquisition_cpu_attach(connection, 1, &consumer), "CPU attach")) goto cleanup;
  } else if (options->cpu_socket != NULL || options->source_socket != NULL) {
    if (connect_cpu_socket(options->source_socket ? options->source_socket : options->cpu_socket, &connection, &consumer, options, &input, &affordances)) goto cleanup;
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
  window = SDL_CreateWindow("capture-viewer-sdl", SDL_WINDOWPOS_CENTERED, SDL_WINDOWPOS_CENTERED, WIDTH, HEIGHT, SDL_WINDOW_HIDDEN | SDL_WINDOW_RESIZABLE | SDL_WINDOW_ALLOW_HIGHDPI);
  if (window != NULL) {
    renderer = SDL_CreateRenderer(window, -1, SDL_RENDERER_ACCELERATED | SDL_RENDERER_PRESENTVSYNC);
    if (renderer == NULL) renderer = SDL_CreateRenderer(window, -1, SDL_RENDERER_SOFTWARE);
  }
  if (renderer == NULL) { fprintf(stderr, "SDL setup: %s\n", SDL_GetError()); goto cleanup; }
  if (options->input_socket && viewer_input_open(&input, options->input_socket) != 0) { fprintf(stderr, "input connection failed\n"); goto cleanup; }
  if (input.client && input.mode != FT_INPUT_MODE_PHYSICAL) SDL_StartTextInput();
  input.renderer = renderer;
  viewer_affordances_cursor_init(&affordances);
  affordances.window = window; affordances.renderer = renderer;
  affordances.started = SDL_GetTicks(); affordances.dirty = 1;
  affordances.focused = !!(SDL_GetWindowFlags(window) & SDL_WINDOW_INPUT_FOCUS);
  if (input.client) viewer_input_install_wheel_filter(&input, window);
  int input_test_sent = 0, window_test_sent = 0, scroll_test_stage = 0;
  failed = 0;
  while (running && (options->max_frames <= 0 || acquired < (uint64_t)options->max_frames)) {
    SDL_Event event;
    while (SDL_PollEvent(&event)) { if (event.type == SDL_QUIT) running = 0; if (viewer_event(&input, window, &affordances, &event)) { failed = 1; running = 0; } }
    viewer_input_poll(&input);
    if (viewer_affordances_poll(&affordances, options->log_affordances)) { failed = 1; break; }
    if (viewer_affordances_tick(&affordances) || input.failed) { failed = 1; break; }
    if (options->input_self_test && !input_test_sent && acquired >= 2 && !input.resetting) {
      viewer_input_self_test(&input, window); input_test_sent = 1;
    }
    if (options->window_self_test && !window_test_sent && acquired >= 2) {
      SDL_SetWindowSize(window, 800, 600);
      SDL_Event resize = {.window = {.type = SDL_WINDOWEVENT, .event = SDL_WINDOWEVENT_RESIZED, .data1 = 800, .data2 = 600}};
      viewer_affordances_event(&affordances, &resize); window_test_sent = 1;
    }
    if (options->scroll_self_test && acquired >= 2) scroll_self_test(&affordances, window, &scroll_test_stage);
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
        int continuing = hold_frame(options->hold_ms, &input, window, &affordances, options->log_affordances);
        int unchanged = memcmp(before_hold, bytes, len) == 0;
        free(before_hold);
        if (!unchanged || continuing <= 0) {
          if (continuing < 0) failed = 1;
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
        if (options->log_affordances) fprintf(stderr, "source frame=%ux%u\n", width, height);
        input.frame_width = (int)width; input.frame_height = (int)height;
      }
      affordances.frame_width = (int)width; affordances.frame_height = (int)height;
      viewer_affordances_cursor_update(&affordances);
      int dw = 0, dh = 0; SDL_GetRendererOutputSize(renderer, &dw, &dh);
      SDL_Rect fit = viewer_fit(dw, dh, (int)width, (int)height);
      int updated = SDL_UpdateTexture(texture, NULL, bytes, (int)desc.stride);
      // SDL_UpdateTexture copies the CPU bytes. Subsequent rendering uses SDL's
      // texture, so no submitted GPU work retains this acquisition mapping.
      if (require_ok(ft_acquired_frame_release(&frame), "ft_acquired_frame_release") || updated != 0 ||
          SDL_RenderClear(renderer) != 0 || SDL_RenderCopy(renderer, texture, NULL, &fit) != 0) {
        fprintf(stderr, "SDL render: %s\n", SDL_GetError()); failed = 1; break;
      }
      int window_w, window_h; SDL_GetWindowSize(window, &window_w, &window_h);
      if (viewer_scroll_draw(&affordances.scroll, affordances.host, renderer, fit,
          window_w > 0 ? (int)ceil(8.0 * dw / window_w) : 8, SDL_GetTicks())) {
        fprintf(stderr, "scroll overlay render/send failed\n"); failed = 1; break;
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
#ifdef __APPLE__
  SDL_SetEventFilter(NULL, NULL);
#endif
  if (viewer_affordances_close(&affordances, options->log_affordances)) failed = 1;
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
