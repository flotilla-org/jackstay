#import <QuartzCore/CAMetalLayer.h>
#include <stdio.h>
#include "metal_present.h"

// Validates actual Metal compilation/setup without a desktop capture or event.
// Run explicitly: cmake --build build/viewer --target metal-presenter-smoke
//                ./build/viewer/metal-presenter-smoke
int main(void) {
  @autoreleasepool {
    CAMetalLayer *layer = [CAMetalLayer layer];
    mp_presenter *presenter = mp_create((__bridge void *)layer);
    if (presenter == NULL) return 1;
    int failed = mp_failed(presenter) || mp_completed_frames(presenter) != 0 || mp_drain(presenter, 0);
    mp_destroy(presenter);
    if (failed) return 1;
    puts("metal_presenter_initialized=1");
    return 0;
  }
}
