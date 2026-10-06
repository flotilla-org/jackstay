/* Observe before SDL2/sdl2-compat can drop zero-delta terminals. The monitor
 * runs on the AppKit event thread, is scoped to the viewer window, and consumes
 * each native wheel exactly once after queueing its value-only replacement. */
#import <AppKit/AppKit.h>
#include <SDL_syswm.h>
#include "viewer_input.h"
_Static_assert(NSEventPhaseNone == 0 && NSEventPhaseBegan == 1 &&
  NSEventPhaseStationary == 2 && NSEventPhaseChanged == 4 && NSEventPhaseEnded == 8 &&
  NSEventPhaseCancelled == 16 && NSEventPhaseMayBegin == 32, "portable NSEvent phase mapping");
void *viewer_native_scroll_install(viewer_input *input, SDL_Window *window) {
  SDL_SysWMinfo info; SDL_VERSION(&info.version);
  if (!SDL_GetWindowWMInfo(window, &info) || info.subsystem != SDL_SYSWM_COCOA) return NULL;
  NSWindow *nativeWindow = info.info.cocoa.window;
  id monitor = [NSEvent addLocalMonitorForEventsMatchingMask:NSEventMaskScrollWheel handler:^NSEvent *(NSEvent *event) {
    if (event.window != nativeWindow) return event;
    NSPoint point = [nativeWindow.contentView convertPoint:event.locationInWindow fromView:nil];
    double y = nativeWindow.contentView.isFlipped ? point.y : nativeWindow.contentView.bounds.size.height - point.y;
    viewer_input_capture_scroll(input, window, -event.scrollingDeltaX, -event.scrollingDeltaY,
      event.hasPreciseScrollingDeltas ? FT_INPUT_SCROLL_PIXEL : FT_INPUT_SCROLL_LINE,
      event.phase, event.momentumPhase, event.isDirectionInvertedFromDevice, point.x, y);
    /* Suppress duplicates even while resetting/failed: falling back to SDL
     * would strip metadata and could admit the discarded gesture tail. */
    return nil;
  }];
  if (!monitor) input->failed = 1;
  return (__bridge_retained void *)monitor;
}
void viewer_native_scroll_remove(void *monitor) {
  [NSEvent removeMonitor:(__bridge_transfer id)monitor];
}
