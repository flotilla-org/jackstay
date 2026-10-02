#include "jackstay_affordances.h"
#include "capture_transfer.h"
#include "jackstay_bootstrap.h"
#include <string.h>

/* Run by acquisition_ffi.rs through this actual C translation unit. The test
 * producer wraps history and destroys its consumer between take and finish. */
ft_status jackstay_c_acquisition_take(ft_acquisition_consumer *consumer,
                                      ft_acquired_frame **out) {
  if (ft_abi_version() != FT_ABI_VERSION) return FT_STATUS_ERROR;
  ft_acquisition_range range = {0};
  return ft_acquisition_acquire(consumer, FT_ACQUIRE_LATEST, 0, out, &range);
}

int jackstay_c_acquisition_finish(ft_acquired_frame **frame) {
  ft_acquired_frame_descriptor descriptor = {0};
  const uint8_t *bytes = NULL;
  size_t len = 0;
  if (ft_acquired_frame_describe(*frame, &descriptor) != FT_STATUS_OK ||
      ft_acquired_frame_bytes(*frame, &bytes, &len) != FT_STATUS_OK) return 1;
  int failed = descriptor.sequence != 7 || descriptor.timestamp_ns != 42 ||
      descriptor.cursor != 1 || descriptor.width != 1 || descriptor.height != 1 ||
      descriptor.stride != 4 || descriptor.flags != 0x1234 ||
      len != 4 || bytes == NULL || memcmp(bytes, "abcd", 4) != 0;
#if defined(__APPLE__)
  void *surface = &descriptor;
  void *readiness = &descriptor;
  if (ft_acquired_frame_macos_resources(*frame, &surface, &readiness) != FT_STATUS_UNSUPPORTED ||
      surface != NULL || readiness != NULL) failed = 1;
#endif
#if defined(_WIN32) && defined(JACKSTAY_BACKEND_WINDOWS)
  void *texture = &descriptor;
  void *ready = &descriptor;
  if (ft_acquired_frame_d3d11_resources(*frame, &texture, &ready) != FT_STATUS_UNSUPPORTED ||
      texture != NULL || ready != NULL) failed = 1;
#endif
  if (ft_acquired_frame_release(frame) != FT_STATUS_OK || *frame != NULL) return 1;
  return failed;
}

/* ABI 0.9 Local Endpoint setup, declared on every platform. */
int jackstay_c_local_endpoint_smoke(void) {
  ft_local_endpoint endpoint = {FT_ENDPOINT_SCOPE_SESSION, FT_ENDPOINT_TRANSPORT_LOCAL_STREAM, "smoke"};
  ft_peer_identity peer = {0};
  ft_os_object none = FT_OS_OBJECT_NONE;
  ft_status (*listen_fn)(const ft_local_endpoint *, ft_local_listener **) = ft_local_listener_create;
  ft_status (*accept_fn)(const ft_local_listener *, ft_local_connection **) = ft_local_listener_accept;
  ft_status (*connect_fn)(const ft_local_endpoint *, ft_local_connection **) = ft_local_connect;
  ft_status (*peer_fn)(const ft_local_connection *, ft_peer_identity *) = ft_local_connection_peer;
  ft_status (*alive_fn)(const ft_local_connection *) = ft_local_connection_alive;
  ft_status (*write_fn)(ft_local_connection *, const uint8_t *, size_t, uint32_t) = ft_local_connection_write;
  ft_status (*read_until_fn)(ft_local_connection *, uint8_t, uint8_t *, size_t, size_t *, uint32_t) =
      ft_local_connection_read_until;
  ft_status (*serve_fn)(ft_cpu_producer *, ft_local_connection **, ft_cpu_setup_server **) =
      ft_cpu_producer_serve_local;
  ft_status (*create_fn)(ft_local_connection **, ft_cpu_acquisition_connection **) =
      ft_acquisition_cpu_connection_create_local;
  ft_status (*setup_alive_fn)(const ft_cpu_acquisition_connection *) = ft_acquisition_cpu_connection_alive;
  ft_status (*bootstrap_accept_fn)(ft_local_connection **, ft_input_target *, ft_input_server **) =
      ft_source_bootstrap_accept_local;
  ft_status (*bootstrap_connect_fn)(ft_local_connection **, uint32_t, uint32_t, ft_input_client **,
                                    ft_status *) = ft_source_bootstrap_connect_local;
  ft_status (*input_serve_fn)(ft_input_target *, ft_local_connection **, ft_input_server **) =
      ft_input_target_serve_local;
  ft_status (*input_connect_fn)(ft_local_connection **, uint32_t, ft_input_client **) =
      ft_input_client_connect_local;
  ft_status (*import_fn)(const uint8_t *, size_t, ft_os_object[5], ft_acquisition_consumer **) =
      ft_acquisition_import_cpu;
  char rendered[512];
  if (ft_local_endpoint_render(&endpoint, rendered, sizeof rendered) != FT_STATUS_OK) return -1;
  return (int)(sizeof peer + (none == FT_OS_OBJECT_NONE) + (listen_fn != NULL) + (accept_fn != NULL) +
               (connect_fn != NULL) + (peer_fn != NULL) + (alive_fn != NULL) + (write_fn != NULL) +
               (read_until_fn != NULL) + (serve_fn != NULL) +
               (create_fn != NULL) + (setup_alive_fn != NULL) + (bootstrap_accept_fn != NULL) +
               (bootstrap_connect_fn != NULL) + (input_serve_fn != NULL) + (input_connect_fn != NULL) +
               (import_fn != NULL) + FT_STATUS_ADDRESS_IN_USE + FT_STATUS_UNTRUSTED_PEER);
}

#if defined(_WIN32)
/* ABI 0.10 D3D11 consumer declarations. The build defines
 * JACKSTAY_BACKEND_WINDOWS when the library has backend-windows, so the entry
 * points are referenced only where they link. */
int jackstay_c_d3d11_smoke(void) {
  ft_d3d11_adapter adapter = {0};
  int total = (int)(sizeof adapter + FT_D3D11_ADAPTER_DESCRIPTION_LEN + FT_STATUS_ADAPTER_MISMATCH +
                    FT_NATIVE_HANDLE_D3D11_TEXTURE + FT_NATIVE_SYNC_D3D11_FENCE);
#if defined(JACKSTAY_BACKEND_WINDOWS)
  ft_status (*create_fn)(ft_local_connection **, ft_d3d11_acquisition_connection **) =
      ft_acquisition_d3d11_connection_create_local;
  ft_status (*alive_fn)(const ft_d3d11_acquisition_connection *) = ft_acquisition_d3d11_connection_alive;
  void (*cancel_fn)(const ft_d3d11_acquisition_connection *) = ft_acquisition_d3d11_connection_cancel;
  ft_status (*describe_fn)(const ft_d3d11_acquisition_connection *, ft_d3d11_adapter *) =
      ft_acquisition_d3d11_describe;
  ft_status (*attach_fn)(const ft_d3d11_acquisition_connection *, void *, uint32_t, ft_acquisition_consumer **) =
      ft_acquisition_d3d11_attach;
  ft_status (*install_fn)(const ft_d3d11_acquisition_connection *, ft_acquisition_consumer *) =
      ft_acquisition_d3d11_install_configuration;
  ft_status (*register_fn)(const ft_d3d11_acquisition_connection *, const ft_acquisition_consumer *, void *,
                           ft_acquisition_release_timeline **) = ft_acquisition_d3d11_register_release;
  void (*destroy_fn)(ft_d3d11_acquisition_connection **) = ft_acquisition_d3d11_connection_destroy;
  ft_status (*resources_fn)(const ft_acquired_frame *, void **, void **) = ft_acquired_frame_d3d11_resources;
  ft_status (*fence_fn)(void *) = ft_d3d11_fence_alive;
  total += (create_fn != NULL) + (alive_fn != NULL) + (cancel_fn != NULL) + (describe_fn != NULL) +
           (attach_fn != NULL) + (install_fn != NULL) + (register_fn != NULL) + (destroy_fn != NULL) +
           (resources_fn != NULL) + (fence_fn != NULL);
#endif
  return total;
}
#endif

#if !defined(_WIN32)
/* The native attach ABI has no Windows implementation yet (#28). */
int porthole_capture_transfer_c_abi_header_smoke(void) {
  ft_native_attach_descriptor descriptor = {
      .struct_size = sizeof(ft_native_attach_descriptor),
      .transport_kind = FT_NATIVE_ATTACH_TRANSPORT_UNIX_SOCKET,
      .requested_consumer_id = 0,
      .endpoint = "unused",
      .bearer_token = NULL,
      .flags = 0,
  };
  ft_native_grant grant = {
      .struct_size = sizeof(ft_native_grant),
  };
  ft_native_pool pool = {
      .struct_size = sizeof(ft_native_pool),
  };
  ft_native_frame frame = {
      .struct_size = sizeof(ft_native_frame),
  };
  ft_native_release release = {
      .struct_size = sizeof(ft_native_release),
      .release_kind = FT_NATIVE_RELEASE_NOW,
  };
  ft_native_sync sync = {
      .struct_size = sizeof(ft_native_sync),
      .sync_kind = FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE,
  };
  ft_native_event event = {
      .struct_size = sizeof(ft_native_event),
  };

  ft_status (*connect_fn)(const ft_native_attach_descriptor *, ft_native_attach **) =
      ft_native_attach_connect;
  ft_status (*grant_fn)(const ft_native_attach *, ft_native_grant *) =
      ft_native_attach_grant;
  ft_status (*wait_fn)(ft_native_attach *, uint64_t, uint64_t, uint64_t *) =
      ft_native_wait_frame;
  ft_status (*acquire_fn)(ft_native_attach *, uint64_t, ft_native_frame *) =
      ft_native_acquire_latest;
  ft_status (*register_release_sync_fn)(ft_native_attach *, const ft_native_sync *, uint64_t *) =
      ft_native_register_release_sync;
  ft_status (*release_fn)(ft_native_attach *, const ft_native_release *) =
      ft_native_release_frame;
  ft_status (*poll_fn)(ft_native_attach *, ft_native_event *) =
      ft_native_poll_event;
  ft_status (*get_pool_fn)(ft_native_attach *, uint64_t, ft_native_pool *) =
      ft_native_get_pool;
  void (*destroy_fn)(ft_native_attach *) = ft_native_attach_destroy;

  return (int)(descriptor.struct_size + grant.struct_size + pool.struct_size +
               frame.struct_size + release.struct_size + sync.struct_size +
               event.struct_size + FT_NATIVE_HANDLE_DMABUF +
               FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE + (connect_fn != NULL) +
               (grant_fn != NULL) + (wait_fn != NULL) + (acquire_fn != NULL) +
               (register_release_sync_fn != NULL) + (release_fn != NULL) +
               (poll_fn != NULL) + (get_pool_fn != NULL) +
               (destroy_fn != NULL));
}
#endif

/* C/Rust layout parity: sizes, alignments and every field offset. */
size_t jackstay_c_affordances_layout(uint32_t index) {
  switch (index) {
    case 0: return sizeof(ft_aff_string);
    case 1: return _Alignof(ft_aff_string);
    case 2: return offsetof(ft_aff_string, data);
    case 3: return offsetof(ft_aff_string, len);
    case 4: return sizeof(ft_aff_optional_string);
    case 5: return _Alignof(ft_aff_optional_string);
    case 6: return offsetof(ft_aff_optional_string, present);
    case 7: return offsetof(ft_aff_optional_string, value);
    case 8: return sizeof(ft_aff_optional_number);
    case 9: return _Alignof(ft_aff_optional_number);
    case 10: return offsetof(ft_aff_optional_number, present);
    case 11: return offsetof(ft_aff_optional_number, value);
    case 12: return sizeof(ft_aff_size);
    case 13: return _Alignof(ft_aff_size);
    case 14: return offsetof(ft_aff_size, present);
    case 15: return offsetof(ft_aff_size, width);
    case 16: return offsetof(ft_aff_size, height);
    case 17: return sizeof(ft_aff_artwork);
    case 18: return _Alignof(ft_aff_artwork);
    case 19: return offsetof(ft_aff_artwork, kind);
    case 20: return offsetof(ft_aff_artwork, value);
    case 21: return sizeof(ft_aff_media);
    case 22: return _Alignof(ft_aff_media);
    case 23: return offsetof(ft_aff_media, status);
    case 24: return offsetof(ft_aff_media, position);
    case 25: return offsetof(ft_aff_media, rate);
    case 26: return offsetof(ft_aff_media, duration);
    case 27: return offsetof(ft_aff_media, title);
    case 28: return offsetof(ft_aff_media, artwork);
    case 29: return offsetof(ft_aff_media, capabilities);
    case 30: return sizeof(ft_aff_navigation);
    case 31: return _Alignof(ft_aff_navigation);
    case 32: return offsetof(ft_aff_navigation, url);
    case 33: return offsetof(ft_aff_navigation, title);
    case 34: return offsetof(ft_aff_navigation, can_go_back);
    case 35: return offsetof(ft_aff_navigation, can_go_forward);
    case 36: return offsetof(ft_aff_navigation, loading);
    case 37: return offsetof(ft_aff_navigation, capabilities);
    case 38: return sizeof(ft_aff_axis);
    case 39: return _Alignof(ft_aff_axis);
    case 40: return offsetof(ft_aff_axis, scrollable);
    case 41: return offsetof(ft_aff_axis, content_length);
    case 42: return offsetof(ft_aff_axis, viewport_length);
    case 43: return offsetof(ft_aff_axis, position);
    case 44: return sizeof(ft_aff_scroll);
    case 45: return _Alignof(ft_aff_scroll);
    case 46: return offsetof(ft_aff_scroll, x);
    case 47: return offsetof(ft_aff_scroll, y);
    case 48: return offsetof(ft_aff_scroll, capabilities);
    case 49: return sizeof(ft_aff_window);
    case 50: return _Alignof(ft_aff_window);
    case 51: return offsetof(ft_aff_window, title);
    case 52: return offsetof(ft_aff_window, requested_size);
    case 53: return offsetof(ft_aff_window, ready);
    case 54: return sizeof(ft_aff_presentation);
    case 55: return _Alignof(ft_aff_presentation);
    case 56: return offsetof(ft_aff_presentation, visible);
    case 57: return offsetof(ft_aff_presentation, preferred_size);
    case 58: return offsetof(ft_aff_presentation, scale);
    case 59: return offsetof(ft_aff_presentation, focused);
    case 60: return sizeof(ft_aff_snapshot);
    case 61: return _Alignof(ft_aff_snapshot);
    case 62: return offsetof(ft_aff_snapshot, domain);
    case 63: return offsetof(ft_aff_snapshot, withdrawn);
    case 64: return offsetof(ft_aff_snapshot, media);
    case 65: return offsetof(ft_aff_snapshot, navigation);
    case 66: return offsetof(ft_aff_snapshot, cursor);
    case 67: return offsetof(ft_aff_snapshot, scroll);
    case 68: return offsetof(ft_aff_snapshot, window);
    case 69: return offsetof(ft_aff_snapshot, presentation);
    case 70: return sizeof(ft_aff_verb);
    case 71: return _Alignof(ft_aff_verb);
    case 72: return offsetof(ft_aff_verb, domain);
    case 73: return offsetof(ft_aff_verb, verb);
    case 74: return offsetof(ft_aff_verb, number);
    case 75: return offsetof(ft_aff_verb, url);
    case 76: return offsetof(ft_aff_verb, axis);
    case 77: return offsetof(ft_aff_verb, step);
    case 78: return offsetof(ft_aff_verb, direction);
    case 79: return sizeof(ft_aff_event_view);
    case 80: return _Alignof(ft_aff_event_view);
    case 81: return offsetof(ft_aff_event_view, kind);
    case 82: return offsetof(ft_aff_event_view, snapshot);
    case 83: return offsetof(ft_aff_event_view, verb);
    default: return (size_t)-1;
  }
}

/* Real C constructs typed input and calls the Rust-backed implementation. */
ft_status jackstay_c_affordances_publish(ft_affordances_producer *producer) {
  ft_aff_snapshot snapshot = {0};
  snapshot.domain = FT_AFF_DOMAIN_WINDOW;
  snapshot.window.ready = 1;
  snapshot.window.title.present = 1;
  snapshot.window.title.value.data = (const uint8_t *)"C producer";
  snapshot.window.title.value.len = 10;
  return ft_affordances_producer_publish(producer, &snapshot);
}
