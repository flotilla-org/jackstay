#include <errno.h>
#include <stdint.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#include <drm/drm.h>
#include <linux/dma-heap.h>

static int porthole_native_linux_errno(void) {
  return errno == 0 ? EIO : errno;
}

int porthole_native_linux_drm_get_cap(int drm_fd, uint64_t capability, uint64_t *out_value) {
  struct drm_get_cap request;
  memset(&request, 0, sizeof(request));
  request.capability = capability;
  if (ioctl(drm_fd, DRM_IOCTL_GET_CAP, &request) != 0) {
    return porthole_native_linux_errno();
  }
  *out_value = request.value;
  return 0;
}

int porthole_native_linux_syncobj_create(int drm_fd, uint32_t flags, uint32_t *out_handle) {
  struct drm_syncobj_create request;
  memset(&request, 0, sizeof(request));
  request.flags = flags;
  if (ioctl(drm_fd, DRM_IOCTL_SYNCOBJ_CREATE, &request) != 0) {
    return porthole_native_linux_errno();
  }
  *out_handle = request.handle;
  return 0;
}

int porthole_native_linux_syncobj_destroy(int drm_fd, uint32_t handle) {
  struct drm_syncobj_destroy request;
  memset(&request, 0, sizeof(request));
  request.handle = handle;
  if (ioctl(drm_fd, DRM_IOCTL_SYNCOBJ_DESTROY, &request) != 0) {
    return porthole_native_linux_errno();
  }
  return 0;
}

int porthole_native_linux_syncobj_export_timeline_fd(int drm_fd, uint32_t handle, int *out_fd) {
  struct drm_syncobj_handle request;
  memset(&request, 0, sizeof(request));
  request.handle = handle;
  request.flags = DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_TIMELINE;
  request.fd = -1;
  if (ioctl(drm_fd, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &request) != 0) {
    return porthole_native_linux_errno();
  }
  *out_fd = request.fd;
  return 0;
}

int porthole_native_linux_syncobj_import_timeline_fd(int drm_fd, int fd, uint32_t *out_handle) {
  struct drm_syncobj_handle request;
  memset(&request, 0, sizeof(request));
  request.flags = DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE;
  request.fd = fd;
  if (ioctl(drm_fd, DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &request) != 0) {
    return porthole_native_linux_errno();
  }
  *out_handle = request.handle;
  return 0;
}

int porthole_native_linux_syncobj_timeline_signal(int drm_fd, uint32_t handle, uint64_t point) {
  uint32_t handles[1] = {handle};
  uint64_t points[1] = {point};
  struct drm_syncobj_timeline_array request;
  memset(&request, 0, sizeof(request));
  request.handles = (uintptr_t)handles;
  request.points = (uintptr_t)points;
  request.count_handles = 1;
  if (ioctl(drm_fd, DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, &request) != 0) {
    return porthole_native_linux_errno();
  }
  return 0;
}

int porthole_native_linux_syncobj_timeline_query(int drm_fd, uint32_t handle, uint32_t flags, uint64_t *out_point) {
  uint32_t handles[1] = {handle};
  uint64_t points[1] = {0};
  struct drm_syncobj_timeline_array request;
  memset(&request, 0, sizeof(request));
  request.handles = (uintptr_t)handles;
  request.points = (uintptr_t)points;
  request.count_handles = 1;
  request.flags = flags;
  if (ioctl(drm_fd, DRM_IOCTL_SYNCOBJ_QUERY, &request) != 0) {
    return porthole_native_linux_errno();
  }
  *out_point = points[0];
  return 0;
}

static int64_t porthole_native_linux_now_monotonic_ns(void) {
  struct timespec time;
  if (clock_gettime(CLOCK_MONOTONIC, &time) != 0) {
    return -1;
  }
  return ((int64_t)time.tv_sec * 1000000000) + time.tv_nsec;
}

int porthole_native_linux_syncobj_timeline_wait(int drm_fd, uint32_t handle, uint64_t point, uint64_t timeout_ns, uint32_t flags) {
  uint32_t handles[1] = {handle};
  uint64_t points[1] = {point};
  int64_t now = porthole_native_linux_now_monotonic_ns();
  if (now < 0) {
    return porthole_native_linux_errno();
  }
  struct drm_syncobj_timeline_wait request;
  memset(&request, 0, sizeof(request));
  request.handles = (uintptr_t)handles;
  request.points = (uintptr_t)points;
  request.count_handles = 1;
  request.flags = flags;
  if (timeout_ns == UINT64_MAX || timeout_ns > (uint64_t)(INT64_MAX - now)) {
    request.timeout_nsec = INT64_MAX;
  } else {
    request.timeout_nsec = now + (int64_t)timeout_ns;
  }
  if (ioctl(drm_fd, DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &request) != 0) {
    return porthole_native_linux_errno();
  }
  return 0;
}

int porthole_native_linux_dma_heap_alloc(int heap_fd, uint64_t len, uint32_t fd_flags, uint64_t heap_flags, int *out_fd) {
  struct dma_heap_allocation_data request;
  memset(&request, 0, sizeof(request));
  request.len = len;
  request.fd_flags = fd_flags;
  request.heap_flags = heap_flags;
  if (ioctl(heap_fd, DMA_HEAP_IOCTL_ALLOC, &request) != 0) {
    return porthole_native_linux_errno();
  }
  *out_fd = (int)request.fd;
  return 0;
}
