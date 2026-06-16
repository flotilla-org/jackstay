// Metal present path for the native capture viewer (#85). See metal_present.h.
//
// The IOSurface latch is zero-copy (newTextureWithDescriptor:iosurface:) and
// cached per surface — pool slots are stable for an attach. Ordering is the
// point: each present command buffer encodes encodeWaitForEvent:value: before
// the blit, so the drawable is composited only from a surface whose producer
// staging blit has completed on the shared timeline. No CPU fence wait.

#import <Metal/Metal.h>
#import <QuartzCore/CAMetalLayer.h>
#import <IOSurface/IOSurface.h>
#import <IOSurface/IOSurfaceObjC.h>

#include <stdio.h>

#include "metal_present.h"

struct mp_presenter {
  void *retained; // PortholeMetalPresenter*, +1
};

@interface PortholeMetalPresenter : NSObject
@property(nonatomic, strong) id<MTLDevice> device;
@property(nonatomic, strong) id<MTLCommandQueue> queue;
@property(nonatomic, strong) CAMetalLayer *layer;
@property(nonatomic, strong) id<MTLSharedEvent> event;
@property(nonatomic, strong) NSMutableDictionary<NSValue *, id<MTLTexture>> *textureCache;
@end

@implementation PortholeMetalPresenter
- (id<MTLTexture>)textureForSurface:(IOSurfaceRef)surface width:(uint32_t)width height:(uint32_t)height {
  NSValue *key = [NSValue valueWithPointer:surface];
  id<MTLTexture> cached = self.textureCache[key];
  if (cached != nil) {
    return cached;
  }
  MTLTextureDescriptor *descriptor =
      [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm width:width height:height mipmapped:NO];
  descriptor.usage = MTLTextureUsageShaderRead;
  descriptor.storageMode = MTLStorageModeShared;
  id<MTLTexture> texture = [self.device newTextureWithDescriptor:descriptor iosurface:surface plane:0];
  if (texture != nil) {
    self.textureCache[key] = texture;
  }
  return texture;
}
@end

mp_presenter *mp_create(void *ca_metal_layer, void *sync_handle) {
  CAMetalLayer *layer = (__bridge CAMetalLayer *)ca_metal_layer;
  id<MTLDevice> device = layer.device ?: MTLCreateSystemDefaultDevice();
  if (device == nil) {
    fprintf(stderr, "mp_create: no Metal device\n");
    return NULL;
  }
  id<MTLCommandQueue> queue = [device newCommandQueue];
  if (queue == nil) {
    fprintf(stderr, "mp_create: failed to create Metal command queue\n");
    return NULL;
  }
  MTLSharedEventHandle *handle = (__bridge MTLSharedEventHandle *)sync_handle;
  id<MTLSharedEvent> event = [device newSharedEventWithHandle:handle];
  if (event == nil) {
    fprintf(stderr, "mp_create: failed to resolve MTLSharedEvent from grant handle\n");
    return NULL;
  }

  PortholeMetalPresenter *presenter = [[PortholeMetalPresenter alloc] init];
  presenter.device = device;
  presenter.queue = queue;
  presenter.layer = layer;
  presenter.layer.device = device;
  presenter.layer.pixelFormat = MTLPixelFormatBGRA8Unorm;
  presenter.layer.framebufferOnly = NO; // we blit into the drawable
  presenter.event = event;
  presenter.textureCache = [NSMutableDictionary dictionary];

  mp_presenter *out = malloc(sizeof(mp_presenter));
  if (out == NULL) {
    return NULL;
  }
  out->retained = (__bridge_retained void *)presenter;
  return out;
}

int mp_present(mp_presenter *presenter, void *io_surface, uint64_t fence_value, uint32_t width, uint32_t height) {
  if (presenter == NULL) {
    return 1;
  }
  PortholeMetalPresenter *p = (__bridge PortholeMetalPresenter *)presenter->retained;
  IOSurfaceRef surface = (IOSurfaceRef)io_surface;

  // Match the drawable to the frame so the blit is a 1:1 copy.
  p.layer.drawableSize = CGSizeMake(width, height);
  id<MTLTexture> srcTexture = [p textureForSurface:surface width:width height:height];
  if (srcTexture == nil) {
    return 1;
  }
  id<CAMetalDrawable> drawable = [p.layer nextDrawable];
  if (drawable == nil) {
    return 1;
  }

  id<MTLCommandBuffer> commandBuffer = [p.queue commandBuffer];
  // GPU-wait: nothing below samples the surface until the producer's timeline
  // reaches fence_value.
  [commandBuffer encodeWaitForEvent:p.event value:fence_value];
  id<MTLBlitCommandEncoder> blit = [commandBuffer blitCommandEncoder];
  [blit copyFromTexture:srcTexture
            sourceSlice:0
            sourceLevel:0
           sourceOrigin:MTLOriginMake(0, 0, 0)
             sourceSize:MTLSizeMake(width, height, 1)
              toTexture:drawable.texture
       destinationSlice:0
       destinationLevel:0
      destinationOrigin:MTLOriginMake(0, 0, 0)];
  [blit endEncoding];
  [commandBuffer presentDrawable:drawable];
  [commandBuffer commit];
  return 0;
}

void mp_present_placeholder(mp_presenter *presenter) {
  if (presenter == NULL) {
    return;
  }
  PortholeMetalPresenter *p = (__bridge PortholeMetalPresenter *)presenter->retained;
  id<CAMetalDrawable> drawable = [p.layer nextDrawable];
  if (drawable == nil) {
    return;
  }
  MTLRenderPassDescriptor *pass = [MTLRenderPassDescriptor renderPassDescriptor];
  pass.colorAttachments[0].texture = drawable.texture;
  pass.colorAttachments[0].loadAction = MTLLoadActionClear;
  pass.colorAttachments[0].storeAction = MTLStoreActionStore;
  pass.colorAttachments[0].clearColor = MTLClearColorMake(0.1, 0.1, 0.12, 1.0);
  id<MTLCommandBuffer> commandBuffer = [p.queue commandBuffer];
  id<MTLRenderCommandEncoder> encoder = [commandBuffer renderCommandEncoderWithDescriptor:pass];
  [encoder endEncoding];
  [commandBuffer presentDrawable:drawable];
  [commandBuffer commit];
}

void mp_destroy(mp_presenter *presenter) {
  if (presenter == NULL) {
    return;
  }
  (void)(__bridge_transfer PortholeMetalPresenter *)presenter->retained;
  free(presenter);
}
