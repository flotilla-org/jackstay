#ifndef CAPTURE_TRANSFER_H
#define CAPTURE_TRANSFER_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define FT_STATUS_OK 0
#define FT_STATUS_EMPTY 1
#define FT_STATUS_INVALID_ARGUMENT 2
#define FT_STATUS_ERROR 3
#define FT_STATUS_TIMEOUT 4
#define FT_STATUS_CLOSED 5
#define FT_STATUS_UNSUPPORTED 6
#define FT_STATUS_INVALID_STATE 7

#define FT_SOURCE_KIND_WINDOW 1
#define FT_SOURCE_KIND_DISPLAY 2
#define FT_SOURCE_KIND_SURFACE 3

#define FT_TRACK_TYPE_VIDEO 1

#define FT_PIXEL_FORMAT_UNKNOWN 0
#define FT_PIXEL_FORMAT_BGRA8_UNORM 1
#define FT_PIXEL_FORMAT_RGBA8_UNORM 2

#define FT_CLOCK_DOMAIN_UNKNOWN 0
#define FT_CLOCK_DOMAIN_UNIX_TIME 1
#define FT_CLOCK_DOMAIN_MEDIA_TIME 2
#define FT_CLOCK_DOMAIN_HOST_TIME 3

#define FT_COLOR_SPACE_UNKNOWN 0
#define FT_COLOR_SPACE_SRGB 1

#define FT_FRAME_SYNC_UNKNOWN 0
#define FT_FRAME_SYNC_CPU_COPY_COMPLETE 1
#define FT_FRAME_SYNC_SCK_SAMPLE_READY 2
#define FT_FRAME_SYNC_NATIVE_TIMELINE 3

#define FT_DAMAGE_UNKNOWN 0
#define FT_DAMAGE_FULL_FRAME 1
#define FT_DAMAGE_NONE 2
#define FT_DAMAGE_INLINE_RECTS 3
#define FT_DAMAGE_SIDECAR_RECTS 4

#define FT_EVENT_PRODUCER_STARTED 1
#define FT_EVENT_SOURCE_REGISTERED 2
#define FT_EVENT_SOURCE_UPDATED 3
#define FT_EVENT_TRACK_REGISTERED 4
#define FT_EVENT_TRACK_UPDATED 5
#define FT_EVENT_SOURCE_UNREGISTERED 6
#define FT_EVENT_PRODUCER_STOPPED 7

typedef int32_t ft_status;
typedef uint64_t ft_source_id;
typedef uint64_t ft_track_id;

typedef struct ft_producer ft_producer;
typedef struct ft_consumer ft_consumer;

/*
 * Threading: v1 handles are single-threaded. Do not call capture-transfer C ABI
 * functions concurrently on the same ft_producer or ft_consumer. External
 * synchronization is required if a host application moves handles across
 * threads.
 */

typedef struct ft_producer_options {
  uint32_t reserved;
} ft_producer_options;

typedef struct ft_consumer_options {
  ft_producer *producer;
} ft_consumer_options;

typedef struct ft_session_descriptor {
  const char *control_socket_path;
  const char *session_id;
} ft_session_descriptor;

typedef struct ft_synthetic_session {
  char session_id[64];
  ft_source_id source_id;
  ft_track_id track_id;
  char fd_socket_path[4096];
} ft_synthetic_session;

typedef struct ft_source_desc {
  uint32_t kind;
  const char *label;
} ft_source_desc;

typedef struct ft_video_track_desc {
  uint32_t width;
  uint32_t height;
  uint32_t pixel_format;
} ft_video_track_desc;

typedef struct ft_track_desc {
  uint32_t track_type;
  ft_video_track_desc video;
} ft_track_desc;

typedef struct ft_video_frame_desc {
  uint64_t sequence;
  uint64_t timestamp_ns;
  uint32_t width;
  uint32_t height;
  uint32_t stride;
  uint32_t pixel_format;
  /* Pool ids are unique forever (never reused); no generation needed. */
  uint64_t pool_id;
  uint32_t slot_id;
  uint64_t payload_offset;
  uint64_t payload_len;
  uint64_t payload_map_len;
  uint32_t clock_domain;
  uint32_t color_space;
  uint32_t sync_kind;
  uint32_t damage_kind;
  uint64_t damage_base_sequence;
  uint32_t dropped_before_publish;
  uint64_t producer_drop_count;
  uint64_t evicted_count;
  uint64_t consumer_skipped_count;
  /* Native-handle descriptor: payload_kind selects cpu-shm vs IOSurface/
   * dmabuf/D3D; native frames carry no in-band payload and are sampled
   * after waiting for fence_value on the stream's fence_id. */
  uint32_t payload_kind;
  uint64_t modifier;
  uint64_t fence_id;
  uint64_t fence_value;
  uint32_t flags;
} ft_video_frame_desc;

#if defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
/*
 * Pin the ABI of ft_video_frame_desc. The mirror lives in the Rust
 * FtVideoFrameDesc (#[repr(C)]); these asserts and the matching
 * const _: () block in src/ffi.rs must agree. Narrowing a field or
 * appending to the tail without updating both sides is the trap this
 * guards against.
 */
_Static_assert(sizeof(ft_video_frame_desc) == 168, "frame desc size");
_Static_assert(offsetof(ft_video_frame_desc, pool_id) == 32, "frame desc packing");
_Static_assert(offsetof(ft_video_frame_desc, slot_id) == 40, "slot_id narrowed to u32");
_Static_assert(offsetof(ft_video_frame_desc, dropped_before_publish) == 96, "dropped_before_publish narrowed to u32");
_Static_assert(offsetof(ft_video_frame_desc, payload_kind) == 128, "native-handle tail begins");
_Static_assert(offsetof(ft_video_frame_desc, modifier) == 136, "native-handle tail packing");
_Static_assert(offsetof(ft_video_frame_desc, fence_id) == 144, "native-handle tail packing");
_Static_assert(offsetof(ft_video_frame_desc, fence_value) == 152, "native-handle tail packing");
_Static_assert(offsetof(ft_video_frame_desc, flags) == 160, "native-handle tail packing");
#endif

typedef struct ft_event {
  uint32_t kind;
  ft_source_id source_id;
  ft_track_id track_id;
  uint32_t track_type;
  uint32_t width;
  uint32_t height;
  uint32_t pixel_format;
} ft_event;

typedef struct ft_video_frame {
  ft_video_frame_desc desc;
  const void *data;
  size_t len;
  void *handle;
} ft_video_frame;

ft_status ft_producer_create(const ft_producer_options *options, ft_producer **out);
ft_status ft_producer_register_source(ft_producer *producer,
                                      const ft_source_desc *desc,
                                      ft_source_id *out_source_id);
ft_status ft_producer_register_track(ft_producer *producer,
                                     ft_source_id source_id,
                                     const ft_track_desc *desc,
                                     ft_track_id *out_track_id);
ft_status ft_producer_publish_video_frame(ft_producer *producer,
                                          ft_track_id track_id,
                                          const ft_video_frame_desc *desc,
                                          const void *pixels,
                                          size_t len);
void ft_producer_destroy(ft_producer *producer);

ft_status ft_consumer_connect(const ft_consumer_options *options, ft_consumer **out);
ft_status ft_create_synthetic_session(const char *control_socket_path,
                                      ft_synthetic_session *out);
ft_status ft_consumer_connect_session(const ft_session_descriptor *descriptor,
                                      ft_consumer **out);
ft_status ft_consumer_poll_event(ft_consumer *consumer, ft_event *out_event);
ft_status ft_consumer_acquire_latest_video_frame(ft_consumer *consumer,
                                                 ft_track_id track_id,
                                                 ft_video_frame *out_frame);
void ft_consumer_release_video_frame(ft_consumer *consumer, ft_video_frame *frame);
void ft_consumer_destroy(ft_consumer *consumer);

/* Native handle path. This is intentionally a low-level C ABI:
 *
 * - Every struct with a struct_size field must be initialized by the caller to
 *   sizeof(that struct) before passing it to a ft_native_* function. Output
 *   structs are validated the same way before they are overwritten.
 * - Handles exposed through grants and pools are borrowed from the
 *   ft_native_attach and stay valid until ft_native_attach_destroy or a later
 *   reconfiguration contract retires them after outstanding leases drain.
 * - Consumers acquire frame leases explicitly and must release each successful
 *   acquire with ft_native_release_frame before the producer can safely reuse
 *   that slot.
 */
typedef struct ft_native_attach ft_native_attach;

#define FT_WAIT_INFINITE UINT64_MAX

#define FT_NATIVE_ATTACH_TRANSPORT_MACOS_XPC 1
#define FT_NATIVE_ATTACH_TRANSPORT_UNIX_SOCKET 2

#define FT_NATIVE_HANDLE_IOSURFACE 1
#define FT_NATIVE_HANDLE_DMABUF 2
#define FT_NATIVE_HANDLE_D3D12_RESOURCE 3
#define FT_NATIVE_MAX_PLANES 4

#define FT_NATIVE_SYNC_NONE 0
#define FT_NATIVE_SYNC_MTL_SHARED_EVENT 1
#define FT_NATIVE_SYNC_DRM_SYNCOBJ_TIMELINE 2
#define FT_NATIVE_SYNC_D3D12_FENCE 3

#define FT_NATIVE_RELEASE_NOW 1
#define FT_NATIVE_RELEASE_TIMELINE_VALUE 2

#define FT_NATIVE_EVENT_POOL_ADDED 1
#define FT_NATIVE_EVENT_POOL_REMOVED 2
#define FT_NATIVE_EVENT_STREAM_CONFIG_CHANGED 3
#define FT_NATIVE_EVENT_PRODUCER_STOPPED 4

typedef struct ft_native_attach_descriptor {
  uint32_t struct_size;
  uint32_t transport_kind;
  uint64_t requested_consumer_id; /* 0 = assign */
  const char *endpoint;
  const char *bearer_token;
  uint32_t flags; /* must be 0 in this ABI revision */
} ft_native_attach_descriptor;

typedef struct ft_native_plane {
  int32_t fd;
  uint32_t offset;
  uint32_t stride;
} ft_native_plane;

typedef struct ft_native_surface {
  uint32_t struct_size;
  uint32_t handle_kind;
  uint32_t plane_count;
  uint32_t width;
  uint32_t height;
  uint32_t pixel_format;
  uint64_t modifier;
  void *object; /* IOSurfaceRef / D3D resource wrapper; NULL for dmabuf */
  ft_native_plane planes[FT_NATIVE_MAX_PLANES];
} ft_native_surface;

typedef union ft_native_sync_handle {
  void *object;
  int32_t fd;
} ft_native_sync_handle;

typedef struct ft_native_sync {
  uint32_t struct_size;
  uint32_t sync_kind;
  uint64_t sync_id;
  ft_native_sync_handle handle;
} ft_native_sync;

typedef struct ft_native_pool {
  uint32_t struct_size;
  uint32_t surface_count;
  uint64_t pool_id;
  const ft_native_surface *surfaces;
} ft_native_pool;

typedef struct ft_native_grant {
  uint32_t struct_size;
  uint32_t pool_count;
  uint64_t consumer_id;
  uint64_t consumer_slot;
  const ft_native_pool *pools;
  ft_native_sync producer_sync;
} ft_native_grant;

typedef struct ft_native_frame {
  uint32_t struct_size;
  uint32_t flags;
  uint64_t lease_id;
  uint64_t cursor;
  uint64_t sequence;
  uint64_t timestamp_ns;
  uint64_t pool_id;
  uint32_t slot_id;
  uint32_t width;
  uint32_t height;
  uint32_t pixel_format;
  uint64_t producer_sync_id;
  uint64_t producer_sync_value; /* GPU-wait this before sampling */
} ft_native_frame;

typedef struct ft_native_release {
  uint32_t struct_size;
  uint32_t release_kind;
  uint64_t lease_id;
  uint64_t release_sync_id;
  uint64_t release_value;
} ft_native_release;

typedef struct ft_native_event {
  uint32_t struct_size;
  uint32_t kind;
  uint64_t pool_id;
  uint64_t config_generation;
} ft_native_event;

#if defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
/* Pin the native ABI; mirror is the #[repr(C)] block in src/ffi_native.rs.
 * The sizes below assume 8-byte pointers (pointer-bearing structs like
 * ft_native_sync and ft_native_grant shrink under ILP32). Declare the
 * constraint explicitly so a 32-bit build fails with one clear message
 * instead of a wall of packing asserts. */
_Static_assert(sizeof(void *) == 8, "jackstay native ABI assumes 64-bit pointers (no ILP32 target yet)");
_Static_assert(sizeof(ft_native_attach_descriptor) == 40, "native attach descriptor size");
_Static_assert(offsetof(ft_native_attach_descriptor, requested_consumer_id) == 8, "native attach descriptor packing");
_Static_assert(offsetof(ft_native_attach_descriptor, endpoint) == 16, "native attach descriptor packing");
_Static_assert(offsetof(ft_native_attach_descriptor, bearer_token) == 24, "native attach descriptor packing");
_Static_assert(offsetof(ft_native_attach_descriptor, flags) == 32, "native attach descriptor packing");
_Static_assert(sizeof(ft_native_plane) == 12, "native plane size");
_Static_assert(sizeof(ft_native_surface) == 88, "native surface size");
_Static_assert(offsetof(ft_native_surface, object) == 32, "native surface packing");
_Static_assert(sizeof(ft_native_sync) == 24, "native sync size");
_Static_assert(sizeof(ft_native_pool) == 24, "native pool size");
_Static_assert(sizeof(ft_native_grant) == 56, "native grant size");
_Static_assert(offsetof(ft_native_grant, pools) == 24, "native grant packing");
_Static_assert(offsetof(ft_native_grant, producer_sync) == 32, "native grant packing");
_Static_assert(sizeof(ft_native_frame) == 80, "native frame size");
_Static_assert(offsetof(ft_native_frame, lease_id) == 8, "native frame packing");
_Static_assert(offsetof(ft_native_frame, producer_sync_id) == 64, "native frame packing");
_Static_assert(sizeof(ft_native_release) == 32, "native release size");
_Static_assert(sizeof(ft_native_event) == 24, "native event size");
#endif

ft_status ft_native_attach_connect(const ft_native_attach_descriptor *descriptor,
                                   ft_native_attach **out);
ft_status ft_native_attach_grant(const ft_native_attach *attach, ft_native_grant *out_grant);
ft_status ft_native_wait_frame(ft_native_attach *attach,
                               uint64_t min_cursor,
                               uint64_t timeout_ns,
                               uint64_t *out_cursor);
ft_status ft_native_acquire_latest(ft_native_attach *attach,
                                   uint64_t min_cursor,
                                   ft_native_frame *out_frame);
// Registers a release timeline for FT_NATIVE_RELEASE_TIMELINE_VALUE.
// Currently supported only for Linux DRM syncobj timeline handles; other
// platforms return FT_STATUS_UNSUPPORTED.
ft_status ft_native_register_release_sync(ft_native_attach *attach,
                                          const ft_native_sync *sync,
                                          uint64_t *out_release_sync_id);
ft_status ft_native_release_frame(ft_native_attach *attach,
                                  const ft_native_release *release);
ft_status ft_native_poll_event(ft_native_attach *attach, ft_native_event *out_event);
ft_status ft_native_get_pool(ft_native_attach *attach, uint64_t pool_id, ft_native_pool *out_pool);
void ft_native_attach_destroy(ft_native_attach *attach);

#ifdef __cplusplus
}
#endif

#endif
