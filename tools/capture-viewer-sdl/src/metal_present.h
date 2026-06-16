/* Metal present shim for the native capture path (#85).
 *
 * The viewer's per-frame GPU work — latch the received IOSurface into an
 * MTLTexture, GPU-wait on the frame's MTLSharedEvent value, blit, present —
 * is ObjC/Metal and lives here so main.c stays C11. The C API below is all
 * main.c calls; pointers cross as void* (CAMetalLayer*, MTLSharedEventHandle*,
 * IOSurfaceRef) and are bridged inside metal_present.m.
 */
#ifndef METAL_PRESENT_H
#define METAL_PRESENT_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct mp_presenter mp_presenter;

/* ca_metal_layer: a CAMetalLayer* (SDL_Metal_GetLayer). sync_handle: the
 * grant's MTLSharedEventHandle, resolved into an MTLSharedEvent on the
 * presenter's own device. Returns NULL on failure. */
mp_presenter *mp_create(void *ca_metal_layer, void *sync_handle);

/* GPU-wait fence_value on the shared event, then blit io_surface into the
 * drawable and present. The wait is encoded on the command buffer before the
 * blit, so the drawable never samples a surface the producer has not finished.
 * Returns 0 on success, non-zero on failure (e.g. no drawable). */
int mp_present(mp_presenter *presenter, void *io_surface, uint64_t fence_value, uint32_t width, uint32_t height);

/* Present a solid placeholder (producer gone or no frame yet). */
void mp_present_placeholder(mp_presenter *presenter);

void mp_destroy(mp_presenter *presenter);

#ifdef __cplusplus
}
#endif

#endif
