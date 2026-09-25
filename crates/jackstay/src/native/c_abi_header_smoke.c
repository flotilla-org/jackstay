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
               (connect_fn != NULL) + (peer_fn != NULL) + (alive_fn != NULL) + (serve_fn != NULL) +
               (create_fn != NULL) + (setup_alive_fn != NULL) + (bootstrap_accept_fn != NULL) +
               (bootstrap_connect_fn != NULL) + (input_serve_fn != NULL) + (input_connect_fn != NULL) +
               (import_fn != NULL) + FT_STATUS_ADDRESS_IN_USE + FT_STATUS_UNTRUSTED_PEER);
}

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
