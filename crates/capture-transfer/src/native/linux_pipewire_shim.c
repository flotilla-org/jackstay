#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <stdbool.h>
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
#define PORTHOLE_NATIVE_LINUX_PIPEWIRE_REQUESTED_BUFFERS 4

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

struct porthole_native_linux_pipewire_held_buffer {
  uint32_t slot_id;
  struct pw_buffer *buffer;
  struct porthole_native_linux_pipewire_held_buffer *next;
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
  uint32_t buffer_count;
  uint32_t held_count;
  uint64_t forced_handback_count;
  struct porthole_native_linux_pipewire_held_buffer *held_head;
  struct porthole_native_linux_pipewire_held_buffer *held_tail;
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
  handle->buffer_count++;
  if (handle->callbacks.buffer_added != NULL) {
    handle->callbacks.buffer_added(handle->callbacks.user_data, buffer_data->slot_id, buffer->buffer);
  }
}

static struct porthole_native_linux_pipewire_held_buffer *
porthole_pipewire_stream_detach_held(struct porthole_native_linux_pipewire_stream *handle, uint32_t slot_id);

static void porthole_pipewire_stream_remove_buffer(void *data, struct pw_buffer *buffer) {
  struct porthole_native_linux_pipewire_stream *handle = data;
  struct porthole_native_linux_pipewire_buffer_user_data *buffer_data = buffer->user_data;
  if (buffer_data != NULL) {
    /* If a consumer's lease has not resolved, this buffer may still be in the
     * held list while PipeWire destroys it (e.g. a renegotiation replaces the
     * pool). Drop the held-list node without queuing it back: the underlying
     * pw_buffer is going away, so a later release or forced handback must not
     * touch it. Runs on the loop thread, serialized against the locked
     * release path. */
    struct porthole_native_linux_pipewire_held_buffer *stale =
      porthole_pipewire_stream_detach_held(handle, buffer_data->slot_id);
    free(stale);
    if (handle->callbacks.buffer_removed != NULL) {
      handle->callbacks.buffer_removed(handle->callbacks.user_data, buffer_data->slot_id);
    }
    free(buffer_data);
    buffer->user_data = NULL;
  }
  if (handle->buffer_count > 0) {
    handle->buffer_count--;
  }
}

static uint32_t porthole_pipewire_stream_max_held(const struct porthole_native_linux_pipewire_stream *handle) {
  if (handle->buffer_count <= 1) {
    return 0;
  }
  return handle->buffer_count - 1;
}

static struct porthole_native_linux_pipewire_held_buffer *
porthole_pipewire_stream_detach_held(struct porthole_native_linux_pipewire_stream *handle, uint32_t slot_id) {
  struct porthole_native_linux_pipewire_held_buffer *previous = NULL;
  struct porthole_native_linux_pipewire_held_buffer *current = handle->held_head;
  while (current != NULL) {
    if (current->slot_id == slot_id) {
      if (previous != NULL) {
        previous->next = current->next;
      } else {
        handle->held_head = current->next;
      }
      if (handle->held_tail == current) {
        handle->held_tail = previous;
      }
      current->next = NULL;
      if (handle->held_count > 0) {
        handle->held_count--;
      }
      return current;
    }
    previous = current;
    current = current->next;
  }
  return NULL;
}

static void porthole_pipewire_stream_queue_held(struct porthole_native_linux_pipewire_stream *handle,
                                                struct porthole_native_linux_pipewire_held_buffer *held) {
  if (held == NULL) {
    return;
  }
  pw_stream_queue_buffer(handle->stream, held->buffer);
  free(held);
}

static void porthole_pipewire_stream_force_oldest_handback(struct porthole_native_linux_pipewire_stream *handle) {
  struct porthole_native_linux_pipewire_held_buffer *held = handle->held_head;
  if (held == NULL) {
    return;
  }
  porthole_pipewire_stream_detach_held(handle, held->slot_id);
  handle->forced_handback_count++;
  porthole_pipewire_stream_queue_held(handle, held);
}

static bool porthole_pipewire_stream_hold_buffer(struct porthole_native_linux_pipewire_stream *handle,
                                                 uint32_t slot_id,
                                                 struct pw_buffer *buffer) {
  uint32_t max_held = porthole_pipewire_stream_max_held(handle);
  if (max_held == 0) {
    return false;
  }
  while (handle->held_count >= max_held) {
    porthole_pipewire_stream_force_oldest_handback(handle);
  }

  struct porthole_native_linux_pipewire_held_buffer *held = calloc(1, sizeof(*held));
  if (held == NULL) {
    /* Not a forced handback: the caller requeues this buffer immediately when
     * hold returns false, so leave forced_handback_count (which counts held
     * buffers stolen under saturation) unskewed. */
    return false;
  }
  held->slot_id = slot_id;
  held->buffer = buffer;
  if (handle->held_tail != NULL) {
    handle->held_tail->next = held;
  } else {
    handle->held_head = held;
  }
  handle->held_tail = held;
  handle->held_count++;
  return true;
}

static void porthole_pipewire_stream_process(void *data) {
  struct porthole_native_linux_pipewire_stream *handle = data;
  struct pw_buffer *buffer = NULL;
  while ((buffer = pw_stream_dequeue_buffer(handle->stream)) != NULL) {
    struct porthole_native_linux_pipewire_buffer_user_data *buffer_data = buffer->user_data;
    uint32_t slot_id = UINT32_MAX;
    if (buffer_data != NULL && handle->callbacks.frame_ready != NULL) {
      slot_id = buffer_data->slot_id;
      handle->callbacks.frame_ready(
        handle->callbacks.user_data,
        slot_id,
        buffer->time,
        buffer->buffer);
    }
    if (slot_id == UINT32_MAX || !porthole_pipewire_stream_hold_buffer(handle, slot_id, buffer)) {
      pw_stream_queue_buffer(handle->stream, buffer);
    }
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
  /* SPA_DATA_SyncObj is absent from older spa headers (ubuntu-24.04 ships
   * PipeWire 1.0) and, being an enumerator, can't be probed with #ifndef.
   * spa_data_type is a public append-only ABI enum, so the value is stable;
   * state it. An older PipeWire at runtime simply never produces it. */
  out->spa_data_syncobj = 5 /* SPA_DATA_SyncObj */;
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
  /* Drain the held list under the thread-loop lock: the loop thread may still
   * be running process/add_buffer/remove_buffer, which mutate the same list.
   * Every other mutator locks first (see _release_buffer); this one must too.
   * Release the lock before pw_thread_loop_stop, which reacquires it to join. */
  if (handle->loop != NULL) {
    pw_thread_loop_lock(handle->loop);
  }
  while (handle->held_head != NULL) {
    porthole_pipewire_stream_force_oldest_handback(handle);
  }
  if (handle->loop != NULL) {
    pw_thread_loop_unlock(handle->loop);
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

  uint8_t param_storage[3][1024];
  const struct spa_pod *params[3];
  struct spa_pod_builder buffers_builder = SPA_POD_BUILDER_INIT(param_storage[2], sizeof(param_storage[2]));
  params[0] = spa_pod_builder_add_object(
    &buffers_builder,
    SPA_TYPE_OBJECT_ParamBuffers,
    SPA_PARAM_Buffers,
    SPA_PARAM_BUFFERS_buffers,
    SPA_POD_Int(PORTHOLE_NATIVE_LINUX_PIPEWIRE_REQUESTED_BUFFERS),
    SPA_PARAM_BUFFERS_dataType,
    SPA_POD_CHOICE_FLAGS_Int(1 << SPA_DATA_DmaBuf));
  params[1] = porthole_pipewire_build_dmabuf_format(
    param_storage[0],
    sizeof(param_storage[0]),
    SPA_VIDEO_FORMAT_BGRA,
    desc->modifiers,
    desc->modifier_count);
  params[2] = porthole_pipewire_build_dmabuf_format(
    param_storage[1],
    sizeof(param_storage[1]),
    SPA_VIDEO_FORMAT_RGBA,
    desc->modifiers,
    desc->modifier_count);
  if (params[0] == NULL || params[1] == NULL || params[2] == NULL) {
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
    3);
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

int porthole_native_linux_pipewire_stream_release_buffer(struct porthole_native_linux_pipewire_stream *handle,
                                                        uint32_t slot_id) {
  if (handle == NULL) {
    return EINVAL;
  }
  pw_thread_loop_lock(handle->loop);
  struct porthole_native_linux_pipewire_held_buffer *held = porthole_pipewire_stream_detach_held(handle, slot_id);
  if (held != NULL) {
    porthole_pipewire_stream_queue_held(handle, held);
  }
  pw_thread_loop_unlock(handle->loop);
  return held == NULL ? ENOENT : 0;
}

void porthole_native_linux_pipewire_stream_close(struct porthole_native_linux_pipewire_stream *handle) {
  porthole_native_linux_pipewire_stream_free(handle);
  pw_deinit();
}
