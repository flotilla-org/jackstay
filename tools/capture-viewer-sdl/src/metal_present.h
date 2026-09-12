/* Metal present shim for the native capture path (#85).
 *
 * The viewer's per-frame GPU work — latch the received IOSurface into an
 * MTLTexture, GPU-wait on the frame's MTLSharedEvent value, render, present —
 * is ObjC/Metal and lives here so main.c stays C11. The C API below is all
 * main.c calls. The presenter accepts a layer and acquired frame ownership;
 * native resource imports stay within the frame's lease.
 */
#ifndef METAL_PRESENT_H
#define METAL_PRESENT_H

#include <stdint.h>
#include "capture_transfer.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct mp_presenter mp_presenter;

/* ca_metal_layer is a CAMetalLayer* from SDL_Metal_GetLayer. */
mp_presenter *mp_create(void *ca_metal_layer);

/* On success transfers *frame to a completion owner and clears the handle.
 * Waits for producer readiness on the GPU, presents, then releases the frame
 * after all GPU use and temporary native imports have finished. Failure leaves
 * the frame owned by the caller and submits no GPU work. */
int mp_present(mp_presenter *presenter, ft_acquired_frame **frame);

int mp_failed(const mp_presenter *presenter);
uint64_t mp_completed_frames(const mp_presenter *presenter);
/* Wait efficiently for completion owners. Timeout/failure returns nonzero and
 * never discards still-owned frames. Call before destroying a healthy viewer. */
int mp_drain(mp_presenter *presenter, uint64_t timeout_ns);

/* Outstanding completion owners remain valid independently of this API handle. */
void mp_destroy(mp_presenter *presenter);

#ifdef __cplusplus
}
#endif

#endif
