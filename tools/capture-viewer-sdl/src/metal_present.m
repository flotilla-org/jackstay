// Frame ownership follows GPU completion. No publication-ring or pool-cache
// reference participates in presentation lifetime.
#import <Metal/Metal.h>
#import <QuartzCore/CAMetalLayer.h>
#import <IOSurface/IOSurface.h>
#import <dispatch/dispatch.h>

#include <stdatomic.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>

#include "metal_present.h"

struct mp_presenter { void *retained; };

@interface JackstayFrameUse : NSObject {
@public
  ft_acquired_frame *frame;
}
@property(nonatomic, strong) id<MTLTexture> texture;
@property(nonatomic, strong) id<MTLSharedEvent> readiness;
@property(nonatomic, strong) id<CAMetalDrawable> drawable;
@end
@implementation JackstayFrameUse
@end

@interface JackstayMetalPresenter : NSObject {
@public
  atomic_bool failed;
  atomic_uint_fast64_t completed;
}
@property(nonatomic, strong) id<MTLDevice> device;
@property(nonatomic, strong) id<MTLCommandQueue> queue;
@property(nonatomic, strong) id<MTLRenderPipelineState> pipeline;
@property(nonatomic, strong) CAMetalLayer *layer;
@property(nonatomic, strong) dispatch_group_t pending;
@end
@implementation JackstayMetalPresenter
@end

static NSString *const shader_source =
  @"#include <metal_stdlib>\n"
   "using namespace metal;\n"
   "struct Vertex { float4 position [[position]]; float2 uv; };\n"
   "vertex Vertex fullscreen(uint i [[vertex_id]]) {\n"
   "  const float2 p[3] = {float2(-1,-1), float2(3,-1), float2(-1,3)};\n"
   "  Vertex out; out.position = float4(p[i],0,1);\n"
   "  out.uv = float2((p[i].x+1)*0.5, 1-(p[i].y+1)*0.5); return out;\n"
   "}\n"
   "fragment float4 display(Vertex in [[stage_in]], texture2d<float> frame [[texture(0)]]) {\n"
   "  constexpr sampler nearest(coord::normalized, address::clamp_to_edge, filter::nearest);\n"
   "  return frame.sample(nearest, in.uv);\n"
   "}\n";

mp_presenter *mp_create(void *ca_metal_layer) {
  if (ca_metal_layer == NULL) return NULL;
  @autoreleasepool {
    CAMetalLayer *layer = (__bridge CAMetalLayer *)ca_metal_layer;
    id<MTLDevice> device = layer.device ?: MTLCreateSystemDefaultDevice();
    if (device == nil) { fprintf(stderr, "mp_create: no Metal device\n"); return NULL; }
    id<MTLCommandQueue> queue = [device newCommandQueue];
    if (queue == nil) { fprintf(stderr, "mp_create: no command queue\n"); return NULL; }
    NSError *error = nil;
    id<MTLLibrary> library = [device newLibraryWithSource:shader_source options:nil error:&error];
    MTLRenderPipelineDescriptor *pipeline = [[MTLRenderPipelineDescriptor alloc] init];
    pipeline.vertexFunction = [library newFunctionWithName:@"fullscreen"];
    pipeline.fragmentFunction = [library newFunctionWithName:@"display"];
    pipeline.colorAttachments[0].pixelFormat = MTLPixelFormatBGRA8Unorm;
    if (library == nil || pipeline.vertexFunction == nil || pipeline.fragmentFunction == nil) {
      fprintf(stderr, "mp_create: presentation shader unavailable: %s\n", error ? error.localizedDescription.UTF8String : "missing function");
      return NULL;
    }
    id<MTLRenderPipelineState> state = [device newRenderPipelineStateWithDescriptor:pipeline error:&error];
    if (state == nil) {
      fprintf(stderr, "mp_create: presentation pipeline failed: %s\n", error ? error.localizedDescription.UTF8String : "unknown error");
      return NULL;
    }
    JackstayMetalPresenter *p = [[JackstayMetalPresenter alloc] init];
    atomic_init(&p->failed, false);
    atomic_init(&p->completed, 0);
    p.device = device;
    p.queue = queue;
    p.pipeline = state;
    p.layer = layer;
    p.layer.device = device;
    p.layer.pixelFormat = MTLPixelFormatBGRA8Unorm;
    p.layer.framebufferOnly = YES;
    p.pending = dispatch_group_create();
    mp_presenter *out = malloc(sizeof(*out));
    if (out == NULL) return NULL;
    out->retained = (__bridge_retained void *)p;
    return out;
  }
}

// Drain temporary encoder/descriptor/handle references before commit can make
// the completion callback run. The returned command buffer retains no resources;
// `use` and `p` explicitly own everything it needs until that callback.
static id<MTLCommandBuffer> prepare(JackstayMetalPresenter *p, JackstayFrameUse *use,
                                    const ft_acquired_frame_descriptor *descriptor,
                                    void *surface, void *readiness) {
  @autoreleasepool {
    MTLPixelFormat format;
    switch (descriptor->pixel_format) {
      case FT_PIXEL_FORMAT_BGRA8_UNORM: format = MTLPixelFormatBGRA8Unorm; break;
      case FT_PIXEL_FORMAT_RGBA8_UNORM: format = MTLPixelFormatRGBA8Unorm; break;
      default: fprintf(stderr, "mp_present: unsupported pixel format %u\n", descriptor->pixel_format); return nil;
    }
    if (descriptor->width == 0 || descriptor->height == 0 || descriptor->sync_kind != FT_FRAME_SYNC_NATIVE_TIMELINE) return nil;
    MTLTextureDescriptor *texture = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:format
        width:descriptor->width height:descriptor->height mipmapped:NO];
    texture.usage = MTLTextureUsageShaderRead;
    texture.storageMode = MTLStorageModeShared;
    use.texture = [p.device newTextureWithDescriptor:texture iosurface:(IOSurfaceRef)surface plane:0];
    use.readiness = [p.device newSharedEventWithHandle:(__bridge MTLSharedEventHandle *)readiness];
    if (use.texture == nil || use.readiness == nil) return nil;
    p.layer.drawableSize = CGSizeMake(descriptor->width, descriptor->height);
    use.drawable = [p.layer nextDrawable];
    if (use.drawable == nil) return nil;
    id<MTLCommandBuffer> command = [p.queue commandBufferWithUnretainedReferences];
    if (command == nil) return nil;
    [command encodeWaitForEvent:use.readiness value:descriptor->fence_value];
    MTLRenderPassDescriptor *pass = [MTLRenderPassDescriptor renderPassDescriptor];
    pass.colorAttachments[0].texture = use.drawable.texture;
    pass.colorAttachments[0].loadAction = MTLLoadActionDontCare;
    pass.colorAttachments[0].storeAction = MTLStoreActionStore;
    id<MTLRenderCommandEncoder> encoder = [command renderCommandEncoderWithDescriptor:pass];
    if (encoder == nil) return nil;
    [encoder setRenderPipelineState:p.pipeline];
    [encoder setFragmentTexture:use.texture atIndex:0];
    [encoder drawPrimitives:MTLPrimitiveTypeTriangle vertexStart:0 vertexCount:3];
    [encoder endEncoding];
    [command presentDrawable:use.drawable];
    return command;
  }
}

int mp_present(mp_presenter *presenter, ft_acquired_frame **frame) {
  if (presenter == NULL || frame == NULL || *frame == NULL) return 1;
  @autoreleasepool {
    JackstayMetalPresenter *p = (__bridge JackstayMetalPresenter *)presenter->retained;
    if (atomic_load(&p->failed)) return 1;
    ft_acquired_frame_descriptor descriptor = {0};
    void *surface = NULL;
    void *readiness = NULL;
    if (ft_acquired_frame_describe(*frame, &descriptor) != FT_STATUS_OK ||
        ft_acquired_frame_macos_resources(*frame, &surface, &readiness) != FT_STATUS_OK) return 1;
    JackstayFrameUse *use = [[JackstayFrameUse alloc] init];
    id<MTLCommandBuffer> command = prepare(p, use, &descriptor, surface, readiness);
    if (command == nil) {
      fprintf(stderr, "mp_present: frame preparation failed before submission\n");
      return 1;
    }
    use->frame = *frame;
    *frame = NULL;
    dispatch_group_enter(p.pending);
    [command addCompletedHandler:^(id<MTLCommandBuffer> completed) {
      @autoreleasepool {
        // Metal calls this after GPU execution has finished. Explicitly discard
        // imported references before returning the lease's storage/credit.
        // The unretained command buffer cannot keep an old pool alive afterward.
        use.texture = nil;
        use.readiness = nil;
        use.drawable = nil;
        ft_status released = ft_acquired_frame_release(&use->frame);
        if (completed.status != MTLCommandBufferStatusCompleted || released != FT_STATUS_OK) {
          atomic_store(&p->failed, true);
          fprintf(stderr, "mp_present: GPU completion failed (status %lu, release %d)\n",
                  (unsigned long)completed.status, released);
        } else {
          atomic_fetch_add(&p->completed, 1);
        }
        dispatch_group_leave(p.pending);
      }
    }];
    [command commit];
    return 0;
  }
}

int mp_failed(const mp_presenter *presenter) {
  if (presenter == NULL) return 1;
  JackstayMetalPresenter *p = (__bridge JackstayMetalPresenter *)presenter->retained;
  return atomic_load(&p->failed);
}

uint64_t mp_completed_frames(const mp_presenter *presenter) {
  if (presenter == NULL) return 0;
  JackstayMetalPresenter *p = (__bridge JackstayMetalPresenter *)presenter->retained;
  return atomic_load(&p->completed);
}

int mp_drain(mp_presenter *presenter, uint64_t timeout_ns) {
  if (presenter == NULL || timeout_ns > INT64_MAX) return 1;
  JackstayMetalPresenter *p = (__bridge JackstayMetalPresenter *)presenter->retained;
  if (dispatch_group_wait(p.pending, dispatch_time(DISPATCH_TIME_NOW, (int64_t)timeout_ns)) != 0) {
    fprintf(stderr, "mp_drain: GPU completion timed out; outstanding frame leases remain held\n");
    return 1;
  }
  return atomic_load(&p->failed);
}

void mp_destroy(mp_presenter *presenter) {
  if (presenter == NULL) return;
  (void)(__bridge_transfer JackstayMetalPresenter *)presenter->retained;
  free(presenter);
}
