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
  uint64_t pool_id;
  uint64_t slot_id;
  uint64_t slot_generation;
  uint64_t payload_offset;
  uint64_t payload_len;
  uint64_t payload_map_len;
  uint32_t clock_domain;
  uint32_t color_space;
  uint32_t sync_kind;
  uint32_t damage_kind;
  uint64_t damage_base_sequence;
  uint64_t dropped_before_publish;
  uint64_t producer_drop_count;
  uint64_t evicted_count;
  uint64_t consumer_skipped_count;
} ft_video_frame_desc;

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

#ifdef __cplusplus
}
#endif

#endif
