#ifndef CAPTURE_TRANSFER_H
#define CAPTURE_TRANSFER_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/*
 * ABI version this header describes, as (major << 16) | minor. Compare
 * against ft_abi_version() from the linked library at startup: a major
 * mismatch is incompatible; a larger library minor is compatible (additions
 * arrive as new functions and new attach-stream operations, never as changes
 * to existing struct layouts — those are pinned by the _Static_asserts
 * below).
 *
 * Major 0 means pre-stabilization: layouts may still change freely, with a
 * minor bump as the only signal. Require an exact version match at major 0. The 1.0 stamp waits until an external
 * consumer needs the stability promise.
 */
#define FT_ABI_VERSION_MAJOR 0
#define FT_ABI_VERSION_MINOR 3
#define FT_ABI_VERSION ((uint32_t)((FT_ABI_VERSION_MAJOR << 16) | FT_ABI_VERSION_MINOR))

uint32_t ft_abi_version(void);

#define FT_STATUS_OK 0
#define FT_STATUS_EMPTY 1
#define FT_STATUS_INVALID_ARGUMENT 2
#define FT_STATUS_ERROR 3
#define FT_STATUS_TIMEOUT 4
#define FT_STATUS_CLOSED 5
#define FT_STATUS_UNSUPPORTED 6
#define FT_STATUS_INVALID_STATE 7
#define FT_STATUS_HOLDING_LIMIT 8
#define FT_STATUS_RECONFIGURATION 9
#define FT_STATUS_MISS 10
#define FT_STATUS_GAP 11
#define FT_STATUS_CANCELLED 12

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

#if defined(__unix__) || defined(__APPLE__)
/* Common acquisition ownership (Unix shared arena). Import a host's CPU grant,
 * or transfer an admitted Rust ArenaConsumer via FtAcquisitionConsumer::into_raw.
 * Serialize calls on a consumer. Frame handles are independent owners and may
 * outlive it. Never copy ownership, fork mappings, or call through stale handles.
 */
typedef struct ft_acquisition_consumer ft_acquisition_consumer;
typedef struct ft_acquired_frame ft_acquired_frame;
typedef struct ft_acquisition_cancellation ft_acquisition_cancellation;
typedef struct ft_acquisition_release_timeline ft_acquisition_release_timeline;

#define FT_ACQUIRE_LATEST 1
#define FT_ACQUIRE_NEXT 2
#define FT_ACQUIRE_EXACT 3
#define FT_WAIT_DATA 1
#define FT_WAIT_CAPACITY 2
#define FT_WAIT_ALL 3
#define FT_ACQUISITION_WAIT_INFINITE UINT64_MAX

/* Same repr(C) descriptor used by Rust FrameLease. Remains immutable and valid
 * even when publication history expires or a new configuration is installed. */
typedef struct ft_acquired_frame_descriptor {
  uint64_t cursor;
  uint64_t sequence;
  uint64_t timestamp_ns;
  uint64_t config_generation;
  uint64_t pool_id;
  uint64_t payload_offset;
  uint64_t payload_len;
  uint64_t modifier;
  uint64_t fence_id;
  uint64_t fence_value;
  uint64_t damage_base_sequence;
  uint64_t producer_drop_count;
  uint32_t width;
  uint32_t height;
  uint32_t stride;
  uint32_t pixel_format;
  uint32_t slot_id;
  uint32_t clock_domain;
  uint32_t color_space;
  uint32_t sync_kind;
  uint32_t payload_kind;
  uint32_t damage_kind;
  uint32_t dropped_before_publish;
  uint32_t flags;
} ft_acquired_frame_descriptor;

typedef struct ft_acquisition_range {
  uint64_t first;
  uint64_t last;
} ft_acquisition_range;

typedef struct ft_acquisition_events {
  uint64_t data_cursor;
  uint64_t capacity_epoch;
  uint64_t reconfiguration_epoch;
  uint32_t closed;
  uint32_t reserved;
} ft_acquisition_events;

/* Import GrantDescriptor JSON and its five owned setup FDs. The trusted producer
 * must follow Jackstay's shared-memory protocol and bind the grant to this PID.
 * Grants are single-use: no replay, forwarding, fork or retained transport FD
 * copies. JSON length must be 1..1048576; *out must start NULL. Invalid pointers,
 * lengths, occupied outputs or negative/duplicate FDs reject without transfer.
 * Once these argument checks pass, all five FDs are consumed and set to -1,
 * including on malformed JSON or failed mapping. Import CPU grants only; native
 * resources require the backend setup that retains their handles with leases. */
ft_status ft_acquisition_import_cpu(const uint8_t *json, size_t len, int32_t fds[5],
                                   ft_acquisition_consumer **out);

/* Latest/next select after cursor; exact selects cursor (zero is invalid).
 * *out must start NULL. Success owns one frame, including duplicate acquisitions.
 * MISS returns [cursor,cursor]; GAP returns an inclusive published range. Other
 * outcomes clear range. No non-success outcome transfers a frame. All pointer
 * arguments are required and must not alias. An occupied *out is rejected. */
ft_status ft_acquisition_acquire(const ft_acquisition_consumer *, uint32_t mode,
                                uint64_t cursor, ft_acquired_frame **out,
                                ft_acquisition_range *range);
ft_status ft_acquired_frame_describe(const ft_acquired_frame *, ft_acquired_frame_descriptor *out);
/* Borrow bytes through release or declared deferred completion. Satisfy producer
 * readiness before reading. Native frames return NULL,0; query native resources
 * through their backend binding. */
ft_status ft_acquired_frame_bytes(const ft_acquired_frame *, const uint8_t **data, size_t *len);
/* Immediate release asserts all use (including GPU work) has completed. */
ft_status ft_acquired_frame_release(ft_acquired_frame **);
/* Host supplies a registered, consumer-bound timeline. Success clears the frame
 * handle but keeps storage and credit until completion. Failure preserves the
 * exact handle and all ownership so the caller can retry. */
ft_status ft_acquired_frame_defer_release(ft_acquired_frame **,
                                         const ft_acquisition_release_timeline *, uint64_t value);
void ft_acquisition_release_timeline_destroy(ft_acquisition_release_timeline **);
/* Take snapshot BEFORE checking acquisition. Reconfiguration and closure always
 * wake. Cancellation takes precedence and never releases held frames.
 * Outputs must not alias inputs. timeout_ns=0 checks once, UINT64_MAX waits
 * indefinitely. OK/CLOSED supply a new snapshot; other outcomes clear it. */
ft_status ft_acquisition_snapshot(const ft_acquisition_consumer *, ft_acquisition_events *out);
ft_status ft_acquisition_wait(ft_acquisition_consumer *, const ft_acquisition_events *observed,
                             uint32_t interest, const ft_acquisition_cancellation *,
                             uint64_t timeout_ns, ft_acquisition_events *out);
ft_status ft_acquisition_cancellation_create(ft_acquisition_cancellation **out);
/* May run concurrently with wait; destroy only after all callers return. */
ft_status ft_acquisition_cancellation_cancel(const ft_acquisition_cancellation *);
void ft_acquisition_cancellation_destroy(ft_acquisition_cancellation **);
/* Destroys this API handle; acquired/deferred frames keep their own owners.
 * All destroy functions accept NULL or *handle=NULL and clear live handles. */
void ft_acquisition_consumer_destroy(ft_acquisition_consumer **);

#if defined(__STDC_VERSION__) && __STDC_VERSION__ >= 201112L
_Static_assert(sizeof(ft_acquired_frame_descriptor) == 144, "acquired descriptor size");
_Static_assert(offsetof(ft_acquired_frame_descriptor, fence_value) == 72, "acquired readiness packing");
_Static_assert(offsetof(ft_acquired_frame_descriptor, width) == 96, "acquired dimensions packing");
_Static_assert(offsetof(ft_acquired_frame_descriptor, flags) == 140, "acquired flags packing");
_Static_assert(sizeof(ft_acquisition_range) == 16, "acquisition range size");
_Static_assert(sizeof(ft_acquisition_events) == 32, "acquisition events size");
#endif
#endif

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
  /* Optional; copied during connect. NULL for public/synthetic sessions. */
  const char *bearer_token;
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
