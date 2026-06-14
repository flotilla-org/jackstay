// macOS native backend shim: IOSurface pools, Metal blit staging, and
// MTLSharedEvent timeline fences behind a C ABI (see src/native/macos.rs).
//
// Conventions match sck_capture_shim.m: functions return a malloc'd error
// string (NULL on success) and results land in out-params. Objects handed
// to Rust are +1 retained; Rust returns ownership through the matching
// release function.

#import <Foundation/Foundation.h>
#import <IOSurface/IOSurface.h>
#import <Metal/Metal.h>

#include <stdint.h>
#include <string.h>

static char *porthole_native_copy_error(NSString *message) {
  const char *utf8 = message.UTF8String;
  if (utf8 == NULL) {
    utf8 = "unknown native backend error";
  }
  char *copy = malloc(strlen(utf8) + 1);
  if (copy == NULL) {
    abort();
  }
  strcpy(copy, utf8);
  return copy;
}

void porthole_native_string_free(char *message) {
  free(message);
}

// ---- Metal context (device + queue) ---------------------------------------

@interface PortholeNativeMetal : NSObject
@property(nonatomic, strong) id<MTLDevice> device;
@property(nonatomic, strong) id<MTLCommandQueue> queue;
@end
@implementation PortholeNativeMetal
@end

char *porthole_native_metal_create(void **outMetal) {
  *outMetal = NULL;
  id<MTLDevice> device = MTLCreateSystemDefaultDevice();
  if (device == nil) {
    return porthole_native_copy_error(@"no Metal device available");
  }
  id<MTLCommandQueue> queue = [device newCommandQueue];
  if (queue == nil) {
    return porthole_native_copy_error(@"failed to create Metal command queue");
  }
  PortholeNativeMetal *metal = [[PortholeNativeMetal alloc] init];
  metal.device = device;
  metal.queue = queue;
  *outMetal = (__bridge_retained void *)metal;
  return NULL;
}

void porthole_native_metal_destroy(void *metalPtr) {
  (void)(__bridge_transfer PortholeNativeMetal *)metalPtr;
}

// ---- IOSurface utilities ---------------------------------------------------

static IOSurfaceRef porthole_native_create_surface(uint32_t width, uint32_t height, uint32_t fourcc,
                                                   uint32_t bytesPerElement) {
  NSDictionary *properties = @{
    (__bridge NSString *)kIOSurfaceWidth : @(width),
    (__bridge NSString *)kIOSurfaceHeight : @(height),
    (__bridge NSString *)kIOSurfacePixelFormat : @(fourcc),
    (__bridge NSString *)kIOSurfaceBytesPerElement : @(bytesPerElement),
  };
  return IOSurfaceCreate((__bridge CFDictionaryRef)properties);
}

char *porthole_native_surface_create(uint32_t width, uint32_t height, uint32_t fourcc,
                                     uint32_t bytesPerElement, void **outSurface) {
  *outSurface = NULL;
  IOSurfaceRef surface = porthole_native_create_surface(width, height, fourcc, bytesPerElement);
  if (surface == NULL) {
    return porthole_native_copy_error(@"IOSurfaceCreate failed");
  }
  *outSurface = surface; // +1 from create
  return NULL;
}

void porthole_native_surface_retain(void *surface) {
  CFRetain((IOSurfaceRef)surface);
}

void porthole_native_surface_release(void *surface) {
  CFRelease((IOSurfaceRef)surface);
}

void porthole_native_surface_hold(void *surface) {
  IOSurfaceIncrementUseCount((IOSurfaceRef)surface);
}

void porthole_native_surface_unhold(void *surface) {
  IOSurfaceDecrementUseCount((IOSurfaceRef)surface);
}

int32_t porthole_native_surface_in_use(void *surface) {
  return IOSurfaceIsInUse((IOSurfaceRef)surface) ? 1 : 0;
}

uint32_t porthole_native_surface_width(void *surface) {
  return (uint32_t)IOSurfaceGetWidth((IOSurfaceRef)surface);
}

uint32_t porthole_native_surface_height(void *surface) {
  return (uint32_t)IOSurfaceGetHeight((IOSurfaceRef)surface);
}

// Copies tightly-packed pixels (len == width * height * bytesPerElement)
// into / out of a surface, handling the surface's row stride.
static char *porthole_native_surface_copy(void *surfacePtr, uint8_t *pixels, size_t len, bool toSurface) {
  IOSurfaceRef surface = (IOSurfaceRef)surfacePtr;
  size_t height = IOSurfaceGetHeight(surface);
  size_t rowBytes = IOSurfaceGetWidth(surface) * IOSurfaceGetBytesPerElement(surface);
  if (height == 0 || len != rowBytes * height) {
    return porthole_native_copy_error([NSString
        stringWithFormat:@"pixel buffer is %zu bytes; surface wants %zu", len, rowBytes * height]);
  }
  uint32_t options = toSurface ? 0 : kIOSurfaceLockReadOnly;
  if (IOSurfaceLock(surface, options, NULL) != kIOReturnSuccess) {
    return porthole_native_copy_error(@"IOSurfaceLock failed");
  }
  uint8_t *base = IOSurfaceGetBaseAddress(surface);
  size_t stride = IOSurfaceGetBytesPerRow(surface);
  for (size_t row = 0; row < height; row++) {
    if (toSurface) {
      memcpy(base + row * stride, pixels + row * rowBytes, rowBytes);
    } else {
      memcpy(pixels + row * rowBytes, base + row * stride, rowBytes);
    }
  }
  IOSurfaceUnlock(surface, options, NULL);
  return NULL;
}

char *porthole_native_surface_write(void *surface, const uint8_t *pixels, size_t len) {
  return porthole_native_surface_copy(surface, (uint8_t *)pixels, len, true);
}

char *porthole_native_surface_read(void *surface, uint8_t *pixels, size_t len) {
  return porthole_native_surface_copy(surface, pixels, len, false);
}

// ---- Surface pool ----------------------------------------------------------

@interface PortholeNativePool : NSObject
@property(nonatomic, strong) NSMutableArray<id<MTLTexture>> *textures;
@property(nonatomic, assign) CFMutableArrayRef surfaces; // IOSurfaceRefs, retained by the array
@property(nonatomic, assign) uint32_t fourcc;
@end
@implementation PortholeNativePool
- (void)dealloc {
  if (_surfaces != NULL) {
    CFRelease(_surfaces);
  }
}
@end

static IOSurfaceRef porthole_native_pool_surface(PortholeNativePool *pool, uint32_t slotId) {
  return (IOSurfaceRef)CFArrayGetValueAtIndex(pool.surfaces, slotId);
}

char *porthole_native_pool_create(void *metalPtr, uint32_t width, uint32_t height, uint32_t fourcc,
                                  uint32_t bytesPerElement, uint64_t mtlPixelFormat,
                                  uint32_t slotCount, void **outPool) {
  *outPool = NULL;
  PortholeNativeMetal *metal = (__bridge PortholeNativeMetal *)metalPtr;
  PortholeNativePool *pool = [[PortholeNativePool alloc] init];
  pool.textures = [NSMutableArray arrayWithCapacity:slotCount];
  pool.surfaces = CFArrayCreateMutable(NULL, slotCount, &kCFTypeArrayCallBacks);
  pool.fourcc = fourcc;

  MTLTextureDescriptor *descriptor =
      [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:(MTLPixelFormat)mtlPixelFormat
                                                         width:width
                                                        height:height
                                                     mipmapped:NO];
  descriptor.usage = MTLTextureUsageShaderRead;
  descriptor.storageMode = MTLStorageModeShared;

  for (uint32_t slot = 0; slot < slotCount; slot++) {
    IOSurfaceRef surface = porthole_native_create_surface(width, height, fourcc, bytesPerElement);
    if (surface == NULL) {
      return porthole_native_copy_error(
          [NSString stringWithFormat:@"IOSurfaceCreate failed for pool slot %u", slot]);
    }
    CFArrayAppendValue(pool.surfaces, surface);
    CFRelease(surface); // the array holds the reference now
    id<MTLTexture> texture = [metal.device newTextureWithDescriptor:descriptor
                                                           iosurface:surface
                                                               plane:0];
    if (texture == nil) {
      return porthole_native_copy_error(
          [NSString stringWithFormat:@"Metal texture creation failed for pool slot %u", slot]);
    }
    [pool.textures addObject:texture];
  }
  *outPool = (__bridge_retained void *)pool;
  return NULL;
}

void porthole_native_pool_destroy(void *poolPtr) {
  (void)(__bridge_transfer PortholeNativePool *)poolPtr;
}

int32_t porthole_native_pool_surface_in_use(void *poolPtr, uint32_t slotId) {
  PortholeNativePool *pool = (__bridge PortholeNativePool *)poolPtr;
  return IOSurfaceIsInUse(porthole_native_pool_surface(pool, slotId)) ? 1 : 0;
}

// Returns the slot's IOSurface at +1 for setup-channel export.
void *porthole_native_pool_copy_surface(void *poolPtr, uint32_t slotId) {
  PortholeNativePool *pool = (__bridge PortholeNativePool *)poolPtr;
  IOSurfaceRef surface = porthole_native_pool_surface(pool, slotId);
  CFRetain(surface);
  return surface;
}

// ---- Shared event fence ----------------------------------------------------

char *porthole_native_event_create(void *metalPtr, void **outEvent) {
  *outEvent = NULL;
  PortholeNativeMetal *metal = (__bridge PortholeNativeMetal *)metalPtr;
  id<MTLSharedEvent> event = [metal.device newSharedEvent];
  if (event == nil) {
    return porthole_native_copy_error(@"failed to create MTLSharedEvent");
  }
  *outEvent = (__bridge_retained void *)event;
  return NULL;
}

void porthole_native_event_destroy(void *eventPtr) {
  (void)(__bridge_transfer id<MTLSharedEvent>)eventPtr;
}

// Returns a +1 MTLSharedEventHandle. It can cross a process boundary only
// inside an NSXPCCoder message (ADR-0007); in-process it resolves directly.
void *porthole_native_event_copy_handle(void *eventPtr) {
  id<MTLSharedEvent> event = (__bridge id<MTLSharedEvent>)eventPtr;
  MTLSharedEventHandle *handle = [event newSharedEventHandle];
  return (__bridge_retained void *)handle;
}

void porthole_native_object_release(void *object) {
  (void)(__bridge_transfer id)object;
}

char *porthole_native_event_from_handle(void *metalPtr, void *handlePtr, void **outEvent) {
  *outEvent = NULL;
  PortholeNativeMetal *metal = (__bridge PortholeNativeMetal *)metalPtr;
  MTLSharedEventHandle *handle = (__bridge MTLSharedEventHandle *)handlePtr;
  id<MTLSharedEvent> event = [metal.device newSharedEventWithHandle:handle];
  if (event == nil) {
    return porthole_native_copy_error(@"failed to resolve MTLSharedEvent from handle");
  }
  *outEvent = (__bridge_retained void *)event;
  return NULL;
}

uint64_t porthole_native_event_signaled_value(void *eventPtr) {
  id<MTLSharedEvent> event = (__bridge id<MTLSharedEvent>)eventPtr;
  return event.signaledValue;
}

int32_t porthole_native_event_wait(void *eventPtr, uint64_t value, uint64_t timeoutMs) {
  id<MTLSharedEvent> event = (__bridge id<MTLSharedEvent>)eventPtr;
  return [event waitUntilSignaledValue:value timeoutMS:timeoutMs] ? 1 : 0;
}

// ---- Staging (blit + signal on one command buffer) --------------------------

@interface PortholeNativeStage : NSObject
@property(nonatomic, strong) id<MTLCommandBuffer> commandBuffer;
@end
@implementation PortholeNativeStage
@end

// Encodes a full-texture copy from the captured surface into the pool slot.
// The command buffer is NOT committed: porthole_native_stage_commit encodes
// the fence signal on the same buffer and commits, so pixels-complete and
// timeline-signal are inseparable on the GPU timeline.
char *porthole_native_stage_blit(void *metalPtr, void *poolPtr, uint32_t slotId, void *srcSurfacePtr,
                                 void **outStage) {
  *outStage = NULL;
  PortholeNativeMetal *metal = (__bridge PortholeNativeMetal *)metalPtr;
  PortholeNativePool *pool = (__bridge PortholeNativePool *)poolPtr;
  IOSurfaceRef src = (IOSurfaceRef)srcSurfacePtr;
  id<MTLTexture> dst = pool.textures[slotId];

  if (IOSurfaceGetPixelFormat(src) != pool.fourcc) {
    return porthole_native_copy_error([NSString
        stringWithFormat:@"captured surface pixel format %08x does not match pool format %08x",
                         IOSurfaceGetPixelFormat(src), pool.fourcc]);
  }
  if (IOSurfaceGetWidth(src) != dst.width || IOSurfaceGetHeight(src) != dst.height) {
    return porthole_native_copy_error([NSString
        stringWithFormat:@"captured surface is %zux%zu; pool is %lux%lu (a resize is a new pool)",
                         IOSurfaceGetWidth(src), IOSurfaceGetHeight(src), (unsigned long)dst.width,
                         (unsigned long)dst.height]);
  }

  MTLTextureDescriptor *descriptor =
      [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:dst.pixelFormat
                                                         width:dst.width
                                                        height:dst.height
                                                     mipmapped:NO];
  descriptor.usage = MTLTextureUsageShaderRead;
  descriptor.storageMode = MTLStorageModeShared;
  id<MTLTexture> srcTexture = [metal.device newTextureWithDescriptor:descriptor
                                                            iosurface:src
                                                                plane:0];
  if (srcTexture == nil) {
    return porthole_native_copy_error(@"Metal texture creation failed for captured surface");
  }

  id<MTLCommandBuffer> commandBuffer = [metal.queue commandBuffer];
  if (commandBuffer == nil) {
    return porthole_native_copy_error(@"failed to create Metal command buffer");
  }
  id<MTLBlitCommandEncoder> blit = [commandBuffer blitCommandEncoder];
  [blit copyFromTexture:srcTexture toTexture:dst];
  [blit endEncoding];

  PortholeNativeStage *stage = [[PortholeNativeStage alloc] init];
  stage.commandBuffer = commandBuffer;
  *outStage = (__bridge_retained void *)stage;
  return NULL;
}

// Consumes the stage: encodes the timeline signal after the blit and commits.
char *porthole_native_stage_commit(void *stagePtr, void *eventPtr, uint64_t value) {
  PortholeNativeStage *stage = (__bridge_transfer PortholeNativeStage *)stagePtr;
  id<MTLSharedEvent> event = (__bridge id<MTLSharedEvent>)eventPtr;
  [stage.commandBuffer encodeSignalEvent:event value:value];
  [stage.commandBuffer commit];
  return NULL;
}

// Releases a stage that will never be committed (an error path between
// stage_frame and signal_fence). The encoded work is discarded.
void porthole_native_stage_destroy(void *stagePtr) {
  (void)(__bridge_transfer PortholeNativeStage *)stagePtr;
}
