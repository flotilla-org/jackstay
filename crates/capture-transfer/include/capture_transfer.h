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

/*
 * Native handle path (macOS, backend-macos builds only).
 *
 * The consumer connects to a jackstay XPC attach service, receives a one-time
 * grant (ring fd + IOSurfaces + MTLSharedEventHandle), and reads the ring's
 * latest descriptor. The viewer does its own Metal: take the raw IOSurfaceRefs
 * and the raw MTLSharedEventHandle from the grant, resolve the handle into an
 * MTLSharedEvent on your own MTLDevice, and on each frame encode a GPU wait for
 * fence_value (encodeWaitForEvent:value:) before sampling surfaces[slot_id].
 *
 * Lifetime: ft_native_grant.surfaces and .sync_handle are BORROWED — owned by
 * the ft_native_attach and valid only until ft_native_attach_destroy. Textures
 * and the shared event you build from them hold their own retains and survive,
 * but do not dereference the raw grant pointers after destroy.
 *
 * These symbols are present only in a macOS backend-macos build of the library.
 */
typedef struct ft_native_attach ft_native_attach;

typedef struct ft_native_grant {
  uint64_t consumer_id;
  uint64_t consumer_slot;
  uint64_t pool_id;             /* unique forever; never reused */
  uint64_t fence_id;            /* names the stream's MTLSharedEvent */
  void *const *surfaces;        /* IOSurfaceRef[pool_slot_count], borrowed */
  void *sync_handle;            /* MTLSharedEventHandle, borrowed */
  uint32_t pool_slot_count;
} ft_native_grant;

typedef struct ft_native_frame {
  uint64_t cursor;
  uint64_t sequence;
  uint64_t timestamp_ns;
  uint64_t fence_value;         /* GPU-wait this on the fence before sampling */
  uint64_t fence_id;
  uint32_t width;
  uint32_t height;
  uint32_t pixel_format;
  uint32_t slot_id;             /* index into grant.surfaces */
  uint32_t flags;
} ft_native_frame;

#if defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
/* Pin the native ABI; mirror is the #[repr(C)] block in src/ffi_native.rs. */
_Static_assert(sizeof(ft_native_grant) == 56, "native grant size");
_Static_assert(offsetof(ft_native_grant, surfaces) == 32, "native grant packing");
_Static_assert(offsetof(ft_native_grant, sync_handle) == 40, "native grant packing");
_Static_assert(offsetof(ft_native_grant, pool_slot_count) == 48, "native grant packing");
_Static_assert(sizeof(ft_native_frame) == 64, "native frame size");
_Static_assert(offsetof(ft_native_frame, width) == 40, "native frame packing");
_Static_assert(offsetof(ft_native_frame, slot_id) == 52, "native frame packing");
_Static_assert(offsetof(ft_native_frame, flags) == 56, "native frame packing");
#endif

/* mach_service_name: the launchd MachServices name to look up. bearer_token:
 * NULL for an unauthenticated service, else the session's attach secret. */
ft_status ft_native_attach_connect(const char *mach_service_name,
                                   const char *bearer_token,
                                   uint64_t consumer_id,
                                   ft_native_attach **out);
ft_status ft_native_attach_grant(const ft_native_attach *attach, ft_native_grant *out_grant);
/* FT_STATUS_EMPTY when the ring has no frame yet; FT_STATUS_ERROR on a read
 * fault. Always returns the newest published frame (drop-to-latest). */
ft_status ft_native_read_latest(const ft_native_attach *attach, ft_native_frame *out_frame);
void ft_native_attach_destroy(ft_native_attach *attach);

#ifdef __cplusplus
}
#endif

#endif
