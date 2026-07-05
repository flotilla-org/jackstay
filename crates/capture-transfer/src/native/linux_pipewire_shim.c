#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <pipewire/pipewire.h>
#include <pipewire/keys.h>
#include <pipewire/properties.h>
#include <pipewire/stream.h>
#include <pipewire/thread-loop.h>
#include <pipewire/version.h>
#include <spa/buffer/buffer.h>
#include <spa/buffer/meta.h>
#include <spa/param/format.h>
#include <spa/pod/builder.h>
#include <spa/param/video/raw.h>
#include <spa/param/video/raw-utils.h>

struct porthole_native_linux_pipewire_probe {
  uint32_t struct_size;
  uint32_t can_init;
  uint32_t can_create_thread_loop;
  uint32_t spa_data_dmabuf;
  uint32_t spa_data_syncobj;
  uint32_t spa_meta_header;
  uint32_t spa_meta_video_damage;
  uint32_t spa_video_format_bgra;
  uint32_t spa_video_format_rgba;
  uint32_t spa_video_max_planes;
  uint32_t spa_format_video_modifier;
  const char *library_version;
};

#define PORTHOLE_NATIVE_LINUX_PIPEWIRE_MAX_PLANES 4
#define PORTHOLE_NATIVE_LINUX_PIPEWIRE_MAX_MODIFIERS 64

struct porthole_native_linux_pipewire_plane {
  int64_t fd;
  uint32_t offset;
  int32_t stride;
  uint32_t maxsize;
  uint32_t data_type;
};

struct porthole_native_linux_pipewire_buffer {
  uint32_t struct_size;
  uint32_t plane_count;
  uint32_t has_header;
  uint32_t header_flags;
  int64_t header_pts;
  uint64_t header_sequence;
  struct porthole_native_linux_pipewire_plane planes[PORTHOLE_NATIVE_LINUX_PIPEWIRE_MAX_PLANES];
};

struct porthole_native_linux_pipewire_stream_desc {
  uint32_t struct_size;
  int32_t remote_fd;
  uint32_t node_id;
  uint64_t object_serial;
  uint32_t modifier_count;
  uint64_t modifiers[PORTHOLE_NATIVE_LINUX_PIPEWIRE_MAX_MODIFIERS];
};

struct porthole_native_linux_pipewire_stream_config {
  uint32_t struct_size;
  uint32_t width;
  uint32_t height;
  uint32_t spa_format;
  uint32_t flags;
  uint64_t modifier;
};

typedef void (*porthole_native_linux_pipewire_config_changed_callback)(
  void *user_data,
  const struct porthole_native_linux_pipewire_stream_config *config);
typedef void (*porthole_native_linux_pipewire_buffer_added_callback)(void *user_data,
                                                                    uint32_t slot_id,
                                                                    const struct spa_buffer *buffer);
typedef void (*porthole_native_linux_pipewire_buffer_removed_callback)(void *user_data,
                                                                      uint32_t slot_id);
typedef void (*porthole_native_linux_pipewire_frame_callback)(void *user_data,
                                                             uint32_t slot_id,
                                                             uint64_t stream_time_ns,
                                                             const struct spa_buffer *buffer);

struct porthole_native_linux_pipewire_stream_callbacks {
  uint32_t struct_size;
  void *user_data;
  porthole_native_linux_pipewire_config_changed_callback config_changed;
  porthole_native_linux_pipewire_buffer_added_callback buffer_added;
  porthole_native_linux_pipewire_buffer_removed_callback buffer_removed;
  porthole_native_linux_pipewire_frame_callback frame_ready;
};

struct porthole_native_linux_pipewire_buffer_user_data {
  uint32_t slot_id;
};

struct porthole_native_linux_pipewire_stream {
  struct pw_thread_loop *loop;
  struct pw_context *context;
  struct pw_core *core;
  struct pw_stream *stream;
  struct spa_hook stream_listener;
  struct porthole_native_linux_pipewire_stream_callbacks callbacks;
  enum pw_stream_state state;
  int error_code;
  uint32_t next_slot_id;
  char error_message[256];
};

static struct spa_pod *porthole_pipewire_build_dmabuf_format(
    uint8_t *storage,
    size_t storage_len,
    enum spa_video_format format,
    const uint64_t *modifiers,
    uint32_t modifier_count) {
  struct spa_pod_builder builder = SPA_POD_BUILDER_INIT(storage, (uint32_t)storage_len);
  struct spa_pod_frame frame;
  spa_pod_builder_push_object(&builder, &frame, SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat);
  spa_pod_builder_add(&builder,
                      SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video),
                      SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw),
                      SPA_FORMAT_VIDEO_format, SPA_POD_Id(format),
                      0);
  if (modifier_count == 1) {
    spa_pod_builder_prop(&builder, SPA_FORMAT_VIDEO_modifier, SPA_POD_PROP_FLAG_MANDATORY);
    spa_pod_builder_long(&builder, (int64_t)modifiers[0]);
  } else {
    struct spa_pod_frame choice;
    spa_pod_builder_prop(
      &builder,
      SPA_FORMAT_VIDEO_modifier,
      SPA_POD_PROP_FLAG_MANDATORY | SPA_POD_PROP_FLAG_DONT_FIXATE);
    spa_pod_builder_push_choice(&builder, &choice, SPA_CHOICE_Enum, 0);
    spa_pod_builder_long(&builder, (int64_t)modifiers[0]);
    for (uint32_t i = 0; i < modifier_count; i++) {
      spa_pod_builder_long(&builder, (int64_t)modifiers[i]);
    }
    spa_pod_builder_pop(&builder, &choice);
  }
  return (struct spa_pod *)spa_pod_builder_pop(&builder, &frame);
}

static void porthole_pipewire_stream_state_changed(void *data,
                                                   enum pw_stream_state old_state,
                                                   enum pw_stream_state state,
                                                   const char *error) {
  (void)old_state;
  struct porthole_native_linux_pipewire_stream *handle = data;
  handle->state = state;
  if (state == PW_STREAM_STATE_ERROR) {
    handle->error_code = errno != 0 ? errno : EIO;
    if (error != NULL) {
      snprintf(handle->error_message, sizeof(handle->error_message), "%s", error);
    }
  }
  pw_thread_loop_signal(handle->loop, false);
}

static void porthole_pipewire_stream_add_buffer(void *data, struct pw_buffer *buffer) {
  struct porthole_native_linux_pipewire_stream *handle = data;
  struct porthole_native_linux_pipewire_buffer_user_data *buffer_data = calloc(1, sizeof(*buffer_data));
  if (buffer_data == NULL) {
    handle->error_code = ENOMEM;
    handle->state = PW_STREAM_STATE_ERROR;
    snprintf(handle->error_message, sizeof(handle->error_message), "failed to allocate PipeWire buffer user data");
    pw_thread_loop_signal(handle->loop, false);
    return;
  }
  buffer_data->slot_id = handle->next_slot_id++;
  buffer->user_data = buffer_data;
  if (handle->callbacks.buffer_added != NULL) {
    handle->callbacks.buffer_added(handle->callbacks.user_data, buffer_data->slot_id, buffer->buffer);
  }
}

static void porthole_pipewire_stream_remove_buffer(void *data, struct pw_buffer *buffer) {
  struct porthole_native_linux_pipewire_stream *handle = data;
  struct porthole_native_linux_pipewire_buffer_user_data *buffer_data = buffer->user_data;
  if (buffer_data != NULL) {
    if (handle->callbacks.buffer_removed != NULL) {
      handle->callbacks.buffer_removed(handle->callbacks.user_data, buffer_data->slot_id);
    }
    free(buffer_data);
    buffer->user_data = NULL;
  }
}

static void porthole_pipewire_stream_process(void *data) {
  struct porthole_native_linux_pipewire_stream *handle = data;
  struct pw_buffer *buffer = NULL;
  while ((buffer = pw_stream_dequeue_buffer(handle->stream)) != NULL) {
    struct porthole_native_linux_pipewire_buffer_user_data *buffer_data = buffer->user_data;
    if (buffer_data != NULL && handle->callbacks.frame_ready != NULL) {
      handle->callbacks.frame_ready(
        handle->callbacks.user_data,
        buffer_data->slot_id,
        buffer->time,
        buffer->buffer);
    }
    /* Known gap (#104): this hands the dmabuf straight back to the
     * compositor, which may write the next frame into it before a consumer
     * lease resolves. Lease gating only covers the producer's own republish
     * of a slot; gating this handback needs a deeper buffer negotiation or
     * an explicit-sync release point. Consumers can observe torn content
     * until that lands. */
    pw_stream_queue_buffer(handle->stream, buffer);
  }
}

static void porthole_pipewire_stream_param_changed(void *data, uint32_t id, const struct spa_pod *param) {
  struct porthole_native_linux_pipewire_stream *handle = data;
  if (id != SPA_PARAM_Format || param == NULL || handle->callbacks.config_changed == NULL) {
    return;
  }

  struct spa_video_info_raw info = { 0 };
  int result = spa_format_video_raw_parse(param, &info);
  if (result < 0) {
    handle->error_code = -result;
    return;
  }

  struct porthole_native_linux_pipewire_stream_config config = {
    .struct_size = sizeof(config),
    .width = info.size.width,
    .height = info.size.height,
    .spa_format = info.format,
    .flags = info.flags,
    .modifier = info.modifier,
  };
  handle->callbacks.config_changed(handle->callbacks.user_data, &config);
}

static const struct pw_stream_events porthole_pipewire_stream_events = {
  PW_VERSION_STREAM_EVENTS,
  .state_changed = porthole_pipewire_stream_state_changed,
  .param_changed = porthole_pipewire_stream_param_changed,
  .add_buffer = porthole_pipewire_stream_add_buffer,
  .remove_buffer = porthole_pipewire_stream_remove_buffer,
  .process = porthole_pipewire_stream_process,
};

int porthole_native_linux_pipewire_probe(struct porthole_native_linux_pipewire_probe *out) {
  if (out == NULL || out->struct_size < sizeof(*out)) {
    return EINVAL;
  }

  out->can_init = 0;
  out->can_create_thread_loop = 0;
  out->spa_data_dmabuf = SPA_DATA_DmaBuf;
  out->spa_data_syncobj = SPA_DATA_SyncObj;
  out->spa_meta_header = SPA_META_Header;
  out->spa_meta_video_damage = SPA_META_VideoDamage;
  out->spa_video_format_bgra = SPA_VIDEO_FORMAT_BGRA;
  out->spa_video_format_rgba = SPA_VIDEO_FORMAT_RGBA;
  out->spa_video_max_planes = SPA_VIDEO_MAX_PLANES;
  out->spa_format_video_modifier = SPA_FORMAT_VIDEO_modifier;
  out->library_version = NULL;

  pw_init(NULL, NULL);
  out->can_init = 1;
  out->library_version = pw_get_library_version();

  struct pw_thread_loop *loop = pw_thread_loop_new("porthole-pipewire-probe", NULL);
  if (loop != NULL) {
    out->can_create_thread_loop = 1;
    pw_thread_loop_destroy(loop);
  }

  pw_deinit();
  return 0;
}

int porthole_native_linux_pipewire_describe_buffer(const struct spa_buffer *buffer,
                                                   struct porthole_native_linux_pipewire_buffer *out) {
  if (buffer == NULL || out == NULL || out->struct_size < sizeof(*out)) {
    return EINVAL;
  }
  if (buffer->n_datas == 0 || buffer->n_datas > PORTHOLE_NATIVE_LINUX_PIPEWIRE_MAX_PLANES || buffer->datas == NULL) {
    return EINVAL;
  }

  const uint32_t struct_size = out->struct_size;
  memset(out, 0, sizeof(*out));
  out->struct_size = struct_size;
  out->plane_count = buffer->n_datas;

  for (uint32_t i = 0; i < buffer->n_datas; i++) {
    const struct spa_data *data = &buffer->datas[i];
    if (data->type != SPA_DATA_DmaBuf || data->fd < 0) {
      return EINVAL;
    }

    uint64_t offset = data->mapoffset;
    int32_t stride = 0;
    if (data->chunk != NULL) {
      offset += data->chunk->offset;
      stride = data->chunk->stride;
    }
    if (offset > UINT32_MAX || stride <= 0) {
      return EINVAL;
    }

    out->planes[i].fd = data->fd;
    out->planes[i].offset = (uint32_t)offset;
    out->planes[i].stride = stride;
    out->planes[i].maxsize = data->maxsize;
    out->planes[i].data_type = data->type;
  }

  if (buffer->metas != NULL) {
    for (uint32_t i = 0; i < buffer->n_metas; i++) {
      const struct spa_meta *meta = &buffer->metas[i];
      if (meta->type == SPA_META_Header && meta->data != NULL && meta->size >= sizeof(struct spa_meta_header)) {
        const struct spa_meta_header *header = meta->data;
        out->has_header = 1;
        out->header_flags = header->flags;
        out->header_pts = header->pts;
        out->header_sequence = header->seq;
        break;
      }
    }
  }

  return 0;
}

static void porthole_native_linux_pipewire_stream_free(struct porthole_native_linux_pipewire_stream *handle) {
  if (handle == NULL) {
    return;
  }
  if (handle->loop != NULL) {
    pw_thread_loop_stop(handle->loop);
  }
  if (handle->stream != NULL) {
    pw_stream_destroy(handle->stream);
  }
  if (handle->core != NULL) {
    pw_core_disconnect(handle->core);
  }
  if (handle->context != NULL) {
    pw_context_destroy(handle->context);
  }
  if (handle->loop != NULL) {
    pw_thread_loop_destroy(handle->loop);
  }
  free(handle);
}

int porthole_native_linux_pipewire_stream_open(
  const struct porthole_native_linux_pipewire_stream_desc *desc,
  const struct porthole_native_linux_pipewire_stream_callbacks *callbacks,
  struct porthole_native_linux_pipewire_stream **out) {
  if (desc == NULL || out == NULL || desc->struct_size < sizeof(*desc) || desc->remote_fd < 0) {
    return EINVAL;
  }
  if (desc->modifier_count == 0 || desc->modifier_count > PORTHOLE_NATIVE_LINUX_PIPEWIRE_MAX_MODIFIERS) {
    return EINVAL;
  }
  if (callbacks != NULL && callbacks->struct_size < sizeof(*callbacks)) {
    return EINVAL;
  }
  *out = NULL;

  struct porthole_native_linux_pipewire_stream *handle = calloc(1, sizeof(*handle));
  if (handle == NULL) {
    return ENOMEM;
  }
  handle->state = PW_STREAM_STATE_UNCONNECTED;
  if (callbacks != NULL) {
    handle->callbacks = *callbacks;
  }

  pw_init(NULL, NULL);

  handle->loop = pw_thread_loop_new("porthole-pipewire-stream", NULL);
  if (handle->loop == NULL) {
    porthole_native_linux_pipewire_stream_free(handle);
    return errno != 0 ? errno : ENOMEM;
  }
  if (pw_thread_loop_start(handle->loop) < 0) {
    int error = errno != 0 ? errno : EIO;
    porthole_native_linux_pipewire_stream_free(handle);
    return error;
  }

  pw_thread_loop_lock(handle->loop);

  handle->context = pw_context_new(pw_thread_loop_get_loop(handle->loop), NULL, 0);
  if (handle->context == NULL) {
    int error = errno != 0 ? errno : ENOMEM;
    pw_thread_loop_unlock(handle->loop);
    porthole_native_linux_pipewire_stream_free(handle);
    return error;
  }

  int remote_fd = dup(desc->remote_fd);
  if (remote_fd < 0) {
    int error = errno != 0 ? errno : EIO;
    pw_thread_loop_unlock(handle->loop);
    porthole_native_linux_pipewire_stream_free(handle);
    return error;
  }

  handle->core = pw_context_connect_fd(handle->context, remote_fd, NULL, 0);
  if (handle->core == NULL) {
    int error = errno != 0 ? errno : EIO;
    close(remote_fd);
    pw_thread_loop_unlock(handle->loop);
    porthole_native_linux_pipewire_stream_free(handle);
    return error;
  }

  struct pw_properties *props = pw_properties_new(
    PW_KEY_MEDIA_TYPE, "Video",
    PW_KEY_MEDIA_CATEGORY, "Capture",
    PW_KEY_MEDIA_ROLE, "Screen",
    NULL);
  if (props == NULL) {
    pw_thread_loop_unlock(handle->loop);
    porthole_native_linux_pipewire_stream_free(handle);
    return ENOMEM;
  }
  char target_object[32];
  uint32_t target_id = PW_ID_ANY;
  if (desc->object_serial != 0) {
    snprintf(target_object, sizeof(target_object), "%" PRIu64, desc->object_serial);
    pw_properties_set(props, PW_KEY_TARGET_OBJECT, target_object);
  } else {
    target_id = desc->node_id;
  }

  handle->stream = pw_stream_new(handle->core, "porthole-screen-capture", props);
  if (handle->stream == NULL) {
    int error = errno != 0 ? errno : EIO;
    pw_thread_loop_unlock(handle->loop);
    porthole_native_linux_pipewire_stream_free(handle);
    return error;
  }

  pw_stream_add_listener(handle->stream, &handle->stream_listener, &porthole_pipewire_stream_events, handle);

  uint8_t param_storage[2][1024];
  const struct spa_pod *params[2];
  params[0] = porthole_pipewire_build_dmabuf_format(
    param_storage[0],
    sizeof(param_storage[0]),
    SPA_VIDEO_FORMAT_BGRA,
    desc->modifiers,
    desc->modifier_count);
  params[1] = porthole_pipewire_build_dmabuf_format(
    param_storage[1],
    sizeof(param_storage[1]),
    SPA_VIDEO_FORMAT_RGBA,
    desc->modifiers,
    desc->modifier_count);
  if (params[0] == NULL || params[1] == NULL) {
    pw_thread_loop_unlock(handle->loop);
    porthole_native_linux_pipewire_stream_free(handle);
    return ENOSPC;
  }

  int connect_result = pw_stream_connect(
    handle->stream,
    PW_DIRECTION_INPUT,
    target_id,
    PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS | PW_STREAM_FLAG_DONT_RECONNECT,
    params,
    2);
  if (connect_result < 0) {
    int error = errno != 0 ? errno : -connect_result;
    pw_thread_loop_unlock(handle->loop);
    porthole_native_linux_pipewire_stream_free(handle);
    return error;
  }

  *out = handle;
  pw_thread_loop_unlock(handle->loop);
  return 0;
}

void porthole_native_linux_pipewire_stream_close(struct porthole_native_linux_pipewire_stream *handle) {
  porthole_native_linux_pipewire_stream_free(handle);
  pw_deinit();
}
