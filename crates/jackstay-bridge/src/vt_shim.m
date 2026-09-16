// VideoToolbox glue for the bridge halves.
//
// Same conventions as the transport core's macos_shim.m: every fallible call
// returns a malloc'd UTF-8 error string, or NULL on success, freed with
// jsb_string_free. Objects cross the boundary as opaque void pointers. Output
// callbacks are plain C function pointers with a refcon, invoked on
// VideoToolbox's own threads; the Rust side does the marshalling.
//
// Encoder output is Annex B (4-byte start codes) without parameter sets; the
// parameter sets are read from the format description separately so the wire
// can carry them once per keyframe. Decoder input is Annex B and is repacked
// to AVCC lengths in one block buffer per access unit.

#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <Foundation/Foundation.h>
#import <IOSurface/IOSurface.h>
#import <VideoToolbox/VideoToolbox.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

// ---- errors -----------------------------------------------------------------

static char *jsb_copy_error(NSString *message) {
  const char *utf8 = message.UTF8String ?: "unknown error";
  size_t len = strlen(utf8) + 1;
  char *out = malloc(len);
  if (out) memcpy(out, utf8, len);
  return out;
}

static char *jsb_status_error(const char *what, OSStatus status) {
  return jsb_copy_error([NSString stringWithFormat:@"%s failed: %d", what, (int)status]);
}

void jsb_string_free(char *s) { free(s); }
void jsb_bytes_free(uint8_t *b) { free(b); }

// ---- shared helpers -----------------------------------------------------------

static CMVideoCodecType jsb_codec_type(int32_t codec) {
  return codec == 2 ? kCMVideoCodecType_H264 : kCMVideoCodecType_HEVC;
}

static OSStatus jsb_parameter_set(CMFormatDescriptionRef desc, int32_t codec, size_t index, const uint8_t **ptr,
                                  size_t *size, size_t *count, int *nalLen) {
  if (codec == 2) {
    return CMVideoFormatDescriptionGetH264ParameterSetAtIndex(desc, index, ptr, size, count, nalLen);
  }
  return CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(desc, index, ptr, size, count, nalLen);
}

// Rewrites length-prefixed NAL units to 4-byte start codes. Returns malloc'd
// bytes; the caller frees with jsb_bytes_free.
static uint8_t *jsb_avcc_to_annexb(const uint8_t *src, size_t len, int nalLen, size_t *outLen) {
  uint8_t *out = malloc(len + 16);
  if (!out) return NULL;
  size_t at = 0, o = 0;
  while (at + (size_t)nalLen <= len) {
    uint32_t n = 0;
    for (int i = 0; i < nalLen; i++) n = (n << 8) | src[at + (size_t)i];
    at += (size_t)nalLen;
    if (n == 0 || at + n > len) break;
    if (o + 4 + n > len + 16) {
      uint8_t *grown = realloc(out, o + 4 + n + 16);
      if (!grown) { free(out); return NULL; }
      out = grown;
    }
    out[o++] = 0; out[o++] = 0; out[o++] = 0; out[o++] = 1;
    memcpy(out + o, src + at, n);
    o += n;
    at += n;
  }
  *outLen = o;
  return out;
}

// Rewrites Annex B (3- or 4-byte start codes) to 4-byte big-endian lengths.
static uint8_t *jsb_annexb_to_avcc(const uint8_t *src, size_t len, size_t *outLen) {
  uint8_t *out = malloc(len + 8);
  if (!out) return NULL;
  size_t o = 0;
  size_t i = 0;
  // find first start code
  size_t start = SIZE_MAX;
  while (i + 3 <= len) {
    if (src[i] == 0 && src[i + 1] == 0 && src[i + 2] == 1) { start = i + 3; i += 3; break; }
    i++;
  }
  while (start != SIZE_MAX) {
    size_t j = start;
    size_t next = SIZE_MAX, nalEnd = len;
    while (j + 3 <= len) {
      if (src[j] == 0 && src[j + 1] == 0 && src[j + 2] == 1) {
        nalEnd = (j > 0 && src[j - 1] == 0) ? j - 1 : j;
        next = j + 3;
        break;
      }
      j++;
    }
    size_t n = nalEnd - start;
    if (n > 0) {
      out[o++] = (uint8_t)(n >> 24); out[o++] = (uint8_t)(n >> 16); out[o++] = (uint8_t)(n >> 8); out[o++] = (uint8_t)n;
      memcpy(out + o, src + start, n);
      o += n;
    }
    start = next;
  }
  *outLen = o;
  return out;
}

// ---- encoder ------------------------------------------------------------------

typedef void (*jsb_encoder_output_fn)(void *refcon, void *frameRefcon, int32_t status, int32_t keyframe,
                                      int32_t dropped, const uint8_t *annexb, size_t annexbLen, int64_t ptsNs);

@interface JsbEncoder : NSObject
@property(nonatomic) VTCompressionSessionRef session;
@property(nonatomic) int32_t codec;
@property(nonatomic) jsb_encoder_output_fn output;
@property(nonatomic) void *refcon;
@property(nonatomic) CMFormatDescriptionRef format; // last output format, retained
@property(nonatomic) NSLock *lock;
@end
@implementation JsbEncoder
- (void)dealloc {
  if (_session) {
    VTCompressionSessionInvalidate(_session);
    CFRelease(_session);
  }
  if (_format) CFRelease(_format);
}
@end

static void jsb_encoder_callback(void *refcon, void *frameRefcon, OSStatus status, VTEncodeInfoFlags flags,
                                 CMSampleBufferRef sample) {
  JsbEncoder *enc = (__bridge JsbEncoder *)refcon;
  int32_t dropped = (flags & kVTEncodeInfo_FrameDropped) ? 1 : 0;
  if (status != noErr || sample == NULL) {
    enc.output(enc.refcon, frameRefcon, (int32_t)status, 0, dropped, NULL, 0, 0);
    return;
  }
  CMFormatDescriptionRef desc = CMSampleBufferGetFormatDescription(sample);
  [enc.lock lock];
  if (desc && desc != enc.format) {
    if (enc.format) CFRelease(enc.format);
    enc.format = (CMFormatDescriptionRef)CFRetain(desc);
  }
  [enc.lock unlock];
  int32_t keyframe = 1;
  CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
  if (attachments && CFArrayGetCount(attachments) > 0) {
    CFDictionaryRef a = CFArrayGetValueAtIndex(attachments, 0);
    CFBooleanRef notSync = CFDictionaryGetValue(a, kCMSampleAttachmentKey_NotSync);
    if (notSync && CFBooleanGetValue(notSync)) keyframe = 0;
  }
  int nalLen = 4;
  const uint8_t *ps = NULL; size_t psSize = 0, psCount = 0;
  if (desc) jsb_parameter_set(desc, enc.codec, 0, &ps, &psSize, &psCount, &nalLen);
  CMBlockBufferRef block = CMSampleBufferGetDataBuffer(sample);
  size_t total = block ? CMBlockBufferGetDataLength(block) : 0;
  uint8_t *contiguous = malloc(total ? total : 1);
  if (!contiguous || (total && CMBlockBufferCopyDataBytes(block, 0, total, contiguous) != kCMBlockBufferNoErr)) {
    free(contiguous);
    enc.output(enc.refcon, frameRefcon, -1, keyframe, dropped, NULL, 0, 0);
    return;
  }
  size_t annexbLen = 0;
  uint8_t *annexb = jsb_avcc_to_annexb(contiguous, total, nalLen, &annexbLen);
  free(contiguous);
  CMTime pts = CMSampleBufferGetPresentationTimeStamp(sample);
  int64_t ptsNs = (pts.timescale > 0) ? (int64_t)((double)pts.value * 1e9 / (double)pts.timescale) : 0;
  enc.output(enc.refcon, frameRefcon, 0, keyframe, dropped, annexb, annexbLen, ptsNs);
  free(annexb);
}

char *jsb_encoder_create(uint32_t width, uint32_t height, int32_t codec, const char *profile, int32_t lowLatency,
                         uint32_t bitrateBps, uint32_t fps, int32_t colourTags, jsb_encoder_output_fn output,
                         void *refcon, void **outEncoder) {
  @autoreleasepool {
    JsbEncoder *enc = [JsbEncoder new];
    enc.codec = codec;
    enc.output = output;
    enc.refcon = refcon;
    enc.lock = [NSLock new];
    NSMutableDictionary *spec = [NSMutableDictionary new];
    spec[(__bridge NSString *)kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder] = @YES;
    if (lowLatency) {
      if (@available(macOS 11.3, *)) {
        spec[(__bridge NSString *)kVTVideoEncoderSpecification_EnableLowLatencyRateControl] = @YES;
      }
    }
    NSDictionary *source = @{
      (__bridge NSString *)kCVPixelBufferPixelFormatTypeKey : @(kCVPixelFormatType_32BGRA),
      (__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey : @{},
    };
    VTCompressionSessionRef session = NULL;
    OSStatus st = VTCompressionSessionCreate(kCFAllocatorDefault, (int32_t)width, (int32_t)height, jsb_codec_type(codec),
                                             (__bridge CFDictionaryRef)spec, (__bridge CFDictionaryRef)source, NULL,
                                             jsb_encoder_callback, (__bridge void *)enc, &session);
    if (st != noErr || session == NULL) return jsb_status_error("VTCompressionSessionCreate", st);
    enc.session = session;
    if (profile && profile[0]) {
      CFStringRef p = CFStringCreateWithCString(kCFAllocatorDefault, profile, kCFStringEncodingUTF8);
      st = VTSessionSetProperty(session, kVTCompressionPropertyKey_ProfileLevel, p);
      CFRelease(p);
      if (st != noErr) return jsb_status_error("set ProfileLevel", st);
    }
    VTSessionSetProperty(session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue);
    VTSessionSetProperty(session, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);
    VTSessionSetProperty(session, kVTCompressionPropertyKey_AverageBitRate, (__bridge CFNumberRef)@(bitrateBps));
    VTSessionSetProperty(session, kVTCompressionPropertyKey_ExpectedFrameRate, (__bridge CFNumberRef)@(fps ? fps : 60));
    VTSessionSetProperty(session, kVTCompressionPropertyKey_MaxKeyFrameInterval, (__bridge CFNumberRef)@(1000000));
    VTSessionSetProperty(session, kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kCFBooleanTrue);
    if (colourTags) {
      VTSessionSetProperty(session, kVTCompressionPropertyKey_ColorPrimaries, kCMFormatDescriptionColorPrimaries_ITU_R_709_2);
      VTSessionSetProperty(session, kVTCompressionPropertyKey_TransferFunction, kCMFormatDescriptionTransferFunction_sRGB);
      VTSessionSetProperty(session, kVTCompressionPropertyKey_YCbCrMatrix, kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2);
    }
    VTCompressionSessionPrepareToEncodeFrames(session);
    *outEncoder = (__bridge_retained void *)enc;
    return NULL;
  }
}

void jsb_encoder_destroy(void *raw) {
  if (raw) {
    JsbEncoder *enc = (__bridge_transfer JsbEncoder *)raw;
    (void)enc;
  }
}

int32_t jsb_encoder_using_hardware(void *raw) {
  JsbEncoder *enc = (__bridge JsbEncoder *)raw;
  CFTypeRef value = NULL;
  if (VTSessionCopyProperty(enc.session, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder, kCFAllocatorDefault,
                            &value) != noErr || value == NULL) {
    return -1;
  }
  int32_t result = CFBooleanGetValue(value) ? 1 : 0;
  CFRelease(value);
  return result;
}

char *jsb_encoder_id(void *raw) {
  JsbEncoder *enc = (__bridge JsbEncoder *)raw;
  CFTypeRef value = NULL;
  if (VTSessionCopyProperty(enc.session, kVTCompressionPropertyKey_EncoderID, kCFAllocatorDefault, &value) != noErr || !value) {
    return jsb_copy_error(@"unknown");
  }
  char *out = jsb_copy_error((__bridge NSString *)value);
  CFRelease(value);
  return out;
}

char *jsb_encoder_encode(void *raw, void *surface, int64_t ptsNs, int32_t forceKeyframe, void *frameRefcon) {
  @autoreleasepool {
    JsbEncoder *enc = (__bridge JsbEncoder *)raw;
    CVPixelBufferRef pixels = NULL;
    CVReturn cv = CVPixelBufferCreateWithIOSurface(kCFAllocatorDefault, (IOSurfaceRef)surface, NULL, &pixels);
    if (cv != kCVReturnSuccess || pixels == NULL) return jsb_status_error("CVPixelBufferCreateWithIOSurface", cv);
    NSDictionary *props = forceKeyframe ? @{(__bridge NSString *)kVTEncodeFrameOptionKey_ForceKeyFrame : @YES} : nil;
    CMTime pts = CMTimeMake(ptsNs, 1000000000);
    OSStatus st = VTCompressionSessionEncodeFrame(enc.session, pixels, pts, kCMTimeInvalid, (__bridge CFDictionaryRef)props,
                                                  frameRefcon, NULL);
    CFRelease(pixels);
    if (st != noErr) return jsb_status_error("VTCompressionSessionEncodeFrame", st);
    return NULL;
  }
}

char *jsb_encoder_flush(void *raw) {
  JsbEncoder *enc = (__bridge JsbEncoder *)raw;
  OSStatus st = VTCompressionSessionCompleteFrames(enc.session, kCMTimeInvalid);
  return st == noErr ? NULL : jsb_status_error("VTCompressionSessionCompleteFrames", st);
}

typedef struct jsb_stream_info {
  uint32_t profile_idc;   // H.264 profile_idc, or HEVC general_profile_idc
  uint32_t full_range;
  uint32_t primaries;     // H.273 codes, 2 = unspecified
  uint32_t transfer;
  uint32_t matrix;
  uint32_t parameter_set_count;
} jsb_stream_info;

static uint32_t jsb_h273_code(CFTypeRef value, CFStringRef const *names, const uint32_t *codes, size_t n) {
  if (!value) return 2;
  for (size_t i = 0; i < n; i++) {
    if (CFEqual(value, names[i])) return codes[i];
  }
  return 2;
}

// Facts about the stream as it is being produced; valid after the first output.
char *jsb_encoder_stream_info(void *raw, jsb_stream_info *out) {
  JsbEncoder *enc = (__bridge JsbEncoder *)raw;
  [enc.lock lock];
  CMFormatDescriptionRef desc = enc.format ? (CMFormatDescriptionRef)CFRetain(enc.format) : NULL;
  [enc.lock unlock];
  if (!desc) return jsb_copy_error(@"no output yet");
  memset(out, 0, sizeof(*out));
  const uint8_t *ps = NULL; size_t size = 0, count = 0; int nalLen = 4;
  size_t spsIndex = enc.codec == 2 ? 0 : 1;
  if (jsb_parameter_set(desc, enc.codec, spsIndex, &ps, &size, &count, &nalLen) == noErr && ps) {
    out->parameter_set_count = (uint32_t)count;
    if (enc.codec == 2 && size > 1) out->profile_idc = ps[1];
    if (enc.codec != 2 && size > 3) out->profile_idc = ps[3] & 0x1f;
  }
  CFTypeRef fr = CMFormatDescriptionGetExtension(desc, kCMFormatDescriptionExtension_FullRangeVideo);
  out->full_range = (fr && CFGetTypeID(fr) == CFBooleanGetTypeID() && CFBooleanGetValue(fr)) ? 1 : 0;
  CFStringRef primariesNames[] = {kCMFormatDescriptionColorPrimaries_ITU_R_709_2, kCMFormatDescriptionColorPrimaries_ITU_R_2020,
                                  kCMFormatDescriptionColorPrimaries_P3_D65};
  uint32_t primariesCodes[] = {1, 9, 12};
  CFStringRef transferNames[] = {kCMFormatDescriptionTransferFunction_ITU_R_709_2, kCMFormatDescriptionTransferFunction_sRGB,
                                 kCMFormatDescriptionTransferFunction_ITU_R_2020};
  uint32_t transferCodes[] = {1, 13, 14};
  CFStringRef matrixNames[] = {kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2, kCMFormatDescriptionYCbCrMatrix_ITU_R_601_4,
                               kCMFormatDescriptionYCbCrMatrix_ITU_R_2020};
  uint32_t matrixCodes[] = {1, 6, 9};
  out->primaries = jsb_h273_code(CMFormatDescriptionGetExtension(desc, kCMFormatDescriptionExtension_ColorPrimaries), primariesNames,
                                 primariesCodes, 3);
  out->transfer = jsb_h273_code(CMFormatDescriptionGetExtension(desc, kCMFormatDescriptionExtension_TransferFunction), transferNames,
                                transferCodes, 3);
  out->matrix = jsb_h273_code(CMFormatDescriptionGetExtension(desc, kCMFormatDescriptionExtension_YCbCrMatrix), matrixNames,
                              matrixCodes, 3);
  CFRelease(desc);
  return NULL;
}

// Copies parameter set `index` (raw NAL unit, no start code). Freed with jsb_bytes_free.
char *jsb_encoder_copy_parameter_set(void *raw, uint32_t index, uint8_t **outBytes, size_t *outLen) {
  JsbEncoder *enc = (__bridge JsbEncoder *)raw;
  [enc.lock lock];
  CMFormatDescriptionRef desc = enc.format ? (CMFormatDescriptionRef)CFRetain(enc.format) : NULL;
  [enc.lock unlock];
  if (!desc) return jsb_copy_error(@"no output yet");
  const uint8_t *ps = NULL; size_t size = 0, count = 0; int nalLen = 4;
  OSStatus st = jsb_parameter_set(desc, enc.codec, index, &ps, &size, &count, &nalLen);
  if (st != noErr || !ps) {
    CFRelease(desc);
    return jsb_status_error("parameter set", st);
  }
  uint8_t *copy = malloc(size ? size : 1);
  if (!copy) { CFRelease(desc); return jsb_copy_error(@"out of memory"); }
  memcpy(copy, ps, size);
  CFRelease(desc);
  *outBytes = copy;
  *outLen = size;
  return NULL;
}

// ---- decoder ------------------------------------------------------------------

typedef void (*jsb_decoder_output_fn)(void *refcon, void *frameRefcon, int32_t status, void *pixelBuffer, int64_t ptsNs);

@interface JsbDecoder : NSObject
@property(nonatomic) VTDecompressionSessionRef session;
@property(nonatomic) CMFormatDescriptionRef format;
@property(nonatomic) int32_t codec;
@property(nonatomic) OSType destination;
@property(nonatomic) jsb_decoder_output_fn output;
@property(nonatomic) void *refcon;
@end
@implementation JsbDecoder
- (void)dealloc {
  if (_session) {
    VTDecompressionSessionWaitForAsynchronousFrames(_session);
    VTDecompressionSessionInvalidate(_session);
    CFRelease(_session);
  }
  if (_format) CFRelease(_format);
}
@end

static void jsb_decoder_callback(void *refcon, void *frameRefcon, OSStatus status, VTDecodeInfoFlags flags,
                                 CVImageBufferRef image, CMTime pts, CMTime duration) {
  (void)flags; (void)duration;
  JsbDecoder *dec = (__bridge JsbDecoder *)refcon;
  int64_t ptsNs = (pts.timescale > 0) ? (int64_t)((double)pts.value * 1e9 / (double)pts.timescale) : 0;
  if (status != noErr || image == NULL) {
    dec.output(dec.refcon, frameRefcon, (int32_t)status, NULL, ptsNs);
    return;
  }
  dec.output(dec.refcon, frameRefcon, 0, (void *)CFRetain(image), ptsNs);
}

char *jsb_decoder_create(int32_t codec, jsb_decoder_output_fn output, void *refcon, void **outDecoder) {
  JsbDecoder *dec = [JsbDecoder new];
  dec.codec = codec;
  dec.output = output;
  dec.refcon = refcon;
  *outDecoder = (__bridge_retained void *)dec;
  return NULL;
}

void jsb_decoder_destroy(void *raw) {
  if (raw) {
    JsbDecoder *dec = (__bridge_transfer JsbDecoder *)raw;
    (void)dec;
  }
}

static char *jsb_decoder_open_session(JsbDecoder *dec) {
  if (dec.session) {
    VTDecompressionSessionWaitForAsynchronousFrames(dec.session);
    VTDecompressionSessionInvalidate(dec.session);
    CFRelease(dec.session);
    dec.session = NULL;
  }
  NSDictionary *spec = @{(__bridge NSString *)kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder : @YES};
  NSDictionary *dest = @{
    (__bridge NSString *)kCVPixelBufferPixelFormatTypeKey : @(dec.destination),
    (__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey : @{},
    (__bridge NSString *)kCVPixelBufferMetalCompatibilityKey : @YES,
  };
  VTDecompressionOutputCallbackRecord record = {jsb_decoder_callback, (__bridge void *)dec};
  VTDecompressionSessionRef session = NULL;
  OSStatus st = VTDecompressionSessionCreate(kCFAllocatorDefault, dec.format, (__bridge CFDictionaryRef)spec,
                                             (__bridge CFDictionaryRef)dest, &record, &session);
  if (st != noErr || !session) return jsb_status_error("VTDecompressionSessionCreate", st);
  dec.session = session;
  return NULL;
}

// Installs parameter sets (raw NAL units) and opens or reopens the session.
// destination is the CoreVideo pixel format to decode into ('444v', '444f',
// '420v', '420f'); match its range to the stream or VideoToolbox keeps a
// private pool and copies every frame.
char *jsb_decoder_configure(void *raw, const uint8_t *const *sets, const size_t *lens, uint32_t count, uint32_t destination) {
  @autoreleasepool {
    JsbDecoder *dec = (__bridge JsbDecoder *)raw;
    CMFormatDescriptionRef desc = NULL;
    OSStatus st;
    if (dec.codec == 2) {
      st = CMVideoFormatDescriptionCreateFromH264ParameterSets(kCFAllocatorDefault, count, sets, lens, 4, &desc);
    } else {
      st = CMVideoFormatDescriptionCreateFromHEVCParameterSets(kCFAllocatorDefault, count, sets, lens, 4, NULL, &desc);
    }
    if (st != noErr || !desc) return jsb_status_error("format description from parameter sets", st);
    bool reopen = true;
    if (dec.session && dec.destination == destination && dec.format &&
        VTDecompressionSessionCanAcceptFormatDescription(dec.session, desc)) {
      reopen = false;
    }
    if (dec.format) CFRelease(dec.format);
    dec.format = desc;
    dec.destination = destination;
    if (!reopen) return NULL;
    return jsb_decoder_open_session(dec);
  }
}

int32_t jsb_decoder_using_hardware(void *raw) {
  JsbDecoder *dec = (__bridge JsbDecoder *)raw;
  if (!dec.session) return -1;
  CFTypeRef value = NULL;
  if (VTSessionCopyProperty(dec.session, kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder, kCFAllocatorDefault,
                            &value) != noErr || !value) {
    return -1;
  }
  int32_t r = CFBooleanGetValue(value) ? 1 : 0;
  CFRelease(value);
  return r;
}

int32_t jsb_decoder_pool_shared(void *raw) {
  JsbDecoder *dec = (__bridge JsbDecoder *)raw;
  if (!dec.session) return -1;
  CFTypeRef value = NULL;
  if (VTSessionCopyProperty(dec.session, kVTDecompressionPropertyKey_PixelBufferPoolIsShared, kCFAllocatorDefault, &value) !=
          noErr || !value) {
    return -1;
  }
  int32_t r = CFBooleanGetValue(value) ? 1 : 0;
  CFRelease(value);
  return r;
}

char *jsb_decoder_decode(void *raw, const uint8_t *annexb, size_t len, int64_t ptsNs, void *frameRefcon) {
  @autoreleasepool {
    JsbDecoder *dec = (__bridge JsbDecoder *)raw;
    if (!dec.session || !dec.format) return jsb_copy_error(@"decoder is not configured");
    size_t avccLen = 0;
    uint8_t *avcc = jsb_annexb_to_avcc(annexb, len, &avccLen);
    if (!avcc) return jsb_copy_error(@"out of memory");
    CMBlockBufferRef block = NULL;
    OSStatus st = CMBlockBufferCreateWithMemoryBlock(kCFAllocatorDefault, NULL, avccLen, kCFAllocatorDefault, NULL, 0, avccLen, 0, &block);
    if (st == kCMBlockBufferNoErr) st = CMBlockBufferAssureBlockMemory(block);
    if (st == kCMBlockBufferNoErr) st = CMBlockBufferReplaceDataBytes(avcc, block, 0, avccLen);
    free(avcc);
    if (st != kCMBlockBufferNoErr) {
      if (block) CFRelease(block);
      return jsb_status_error("CMBlockBuffer", st);
    }
    CMSampleTimingInfo timing = {kCMTimeInvalid, CMTimeMake(ptsNs, 1000000000), kCMTimeInvalid};
    CMSampleBufferRef sample = NULL;
    st = CMSampleBufferCreateReady(kCFAllocatorDefault, block, dec.format, 1, 1, &timing, 1, &avccLen, &sample);
    CFRelease(block);
    if (st != noErr || !sample) return jsb_status_error("CMSampleBufferCreateReady", st);
    st = VTDecompressionSessionDecodeFrame(dec.session, sample, kVTDecodeFrame_EnableAsynchronousDecompression, frameRefcon, NULL);
    CFRelease(sample);
    if (st != noErr) return jsb_status_error("VTDecompressionSessionDecodeFrame", st);
    return NULL;
  }
}

char *jsb_decoder_flush(void *raw) {
  JsbDecoder *dec = (__bridge JsbDecoder *)raw;
  if (!dec.session) return NULL;
  OSStatus st = VTDecompressionSessionWaitForAsynchronousFrames(dec.session);
  return st == noErr ? NULL : jsb_status_error("VTDecompressionSessionWaitForAsynchronousFrames", st);
}

// ---- decoded pixel buffers -------------------------------------------------------

void jsb_pixel_buffer_release(void *pb) {
  if (pb) CFRelease((CVPixelBufferRef)pb);
}
uint32_t jsb_pixel_buffer_format(void *pb) { return (uint32_t)CVPixelBufferGetPixelFormatType((CVPixelBufferRef)pb); }
uint32_t jsb_pixel_buffer_width(void *pb) { return (uint32_t)CVPixelBufferGetWidth((CVPixelBufferRef)pb); }
uint32_t jsb_pixel_buffer_height(void *pb) { return (uint32_t)CVPixelBufferGetHeight((CVPixelBufferRef)pb); }

char *jsb_transfer_create(void **out) {
  VTPixelTransferSessionRef session = NULL;
  OSStatus st = VTPixelTransferSessionCreate(kCFAllocatorDefault, &session);
  if (st != noErr || !session) return jsb_status_error("VTPixelTransferSessionCreate", st);
  *out = session;
  return NULL;
}

void jsb_transfer_destroy(void *raw) {
  if (raw) {
    VTPixelTransferSessionInvalidate((VTPixelTransferSessionRef)raw);
    CFRelease(raw);
  }
}

// Converts a decoded YCbCr pixel buffer into a BGRA IOSurface the caller owns.
// Synchronous: the surface is complete when this returns.
char *jsb_transfer_to_surface(void *raw, void *pb, void *surface) {
  CVPixelBufferRef dst = NULL;
  CVReturn cv = CVPixelBufferCreateWithIOSurface(kCFAllocatorDefault, (IOSurfaceRef)surface, NULL, &dst);
  if (cv != kCVReturnSuccess || !dst) return jsb_status_error("CVPixelBufferCreateWithIOSurface(dst)", cv);
  OSStatus st = VTPixelTransferSessionTransferImage((VTPixelTransferSessionRef)raw, (CVPixelBufferRef)pb, dst);
  CFRelease(dst);
  return st == noErr ? NULL : jsb_status_error("VTPixelTransferSessionTransferImage", st);
}

// ---- capability probe --------------------------------------------------------------

typedef struct jsb_probe_ctx {
  int32_t outputs;
  int32_t keyframes;
  CMSampleBufferRef samples[4];
} jsb_probe_ctx;

static void jsb_probe_encode_cb(void *refcon, void *frameRefcon, OSStatus status, VTEncodeInfoFlags flags, CMSampleBufferRef sample) {
  (void)frameRefcon; (void)flags;
  jsb_probe_ctx *ctx = refcon;
  if (status == noErr && sample && ctx->outputs < 4) {
    ctx->samples[ctx->outputs] = (CMSampleBufferRef)CFRetain(sample);
    ctx->outputs++;
  }
}

static void jsb_probe_decode_cb(void *refcon, void *frameRefcon, OSStatus status, VTDecodeInfoFlags flags, CVImageBufferRef image,
                                CMTime pts, CMTime duration) {
  (void)frameRefcon; (void)flags; (void)pts; (void)duration;
  int32_t *decoded = refcon;
  if (status == noErr && image) (*decoded)++;
}

// Creates a hardware encoder for (codec, profile), encodes two 64x64 frames,
// then decodes them with hardware required. 1 when both succeed, else 0.
static int32_t jsb_probe_path(int32_t codec, const char *profile, OSType destination) {
  @autoreleasepool {
    int32_t result = 0;
    jsb_probe_ctx ctx = {0, 0, {NULL, NULL, NULL, NULL}};
    NSDictionary *spec = @{(__bridge NSString *)kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder : @YES};
    NSDictionary *source = @{(__bridge NSString *)kCVPixelBufferPixelFormatTypeKey : @(kCVPixelFormatType_32BGRA)};
    VTCompressionSessionRef enc = NULL;
    if (VTCompressionSessionCreate(kCFAllocatorDefault, 64, 64, jsb_codec_type(codec), (__bridge CFDictionaryRef)spec,
                                   (__bridge CFDictionaryRef)source, NULL, jsb_probe_encode_cb, &ctx, &enc) != noErr) {
      return 0;
    }
    CFStringRef p = CFStringCreateWithCString(kCFAllocatorDefault, profile, kCFStringEncodingUTF8);
    OSStatus st = VTSessionSetProperty(enc, kVTCompressionPropertyKey_ProfileLevel, p);
    CFRelease(p);
    VTSessionSetProperty(enc, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue);
    VTSessionSetProperty(enc, kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse);
    if (st == noErr) {
      for (int i = 0; i < 2; i++) {
        CVPixelBufferRef pixels = NULL;
        if (CVPixelBufferCreate(kCFAllocatorDefault, 64, 64, kCVPixelFormatType_32BGRA, NULL, &pixels) != kCVReturnSuccess) break;
        CVPixelBufferLockBaseAddress(pixels, 0);
        uint8_t *base = CVPixelBufferGetBaseAddress(pixels);
        size_t rows = CVPixelBufferGetBytesPerRow(pixels) * 64;
        for (size_t k = 0; k < rows; k++) base[k] = (uint8_t)((k * 7 + (size_t)i * 13) & 0xff);
        CVPixelBufferUnlockBaseAddress(pixels, 0);
        VTCompressionSessionEncodeFrame(enc, pixels, CMTimeMake(i, 30), kCMTimeInvalid, NULL, NULL, NULL);
        CFRelease(pixels);
      }
      VTCompressionSessionCompleteFrames(enc, kCMTimeInvalid);
    }
    // the SPS must say what we asked for: profile 244 / RExt (4) for 4:4:4
    bool profileOk = false;
    if (ctx.outputs > 0) {
      CMFormatDescriptionRef desc = CMSampleBufferGetFormatDescription(ctx.samples[0]);
      const uint8_t *ps = NULL; size_t size = 0, count = 0; int nalLen = 4;
      size_t spsIndex = codec == 2 ? 0 : 1;
      if (desc && jsb_parameter_set(desc, codec, spsIndex, &ps, &size, &count, &nalLen) == noErr && ps) {
        bool wants444 = strstr(profile, "444") != NULL;
        if (codec == 2 && size > 1) profileOk = wants444 ? ps[1] == 244 : ps[1] != 244;
        if (codec != 2 && size > 3) profileOk = wants444 ? (ps[3] & 0x1f) == 4 : (ps[3] & 0x1f) != 4;
      }
      if (profileOk) {
        NSDictionary *dspec = @{(__bridge NSString *)kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder : @YES};
        NSDictionary *dest = @{
          (__bridge NSString *)kCVPixelBufferPixelFormatTypeKey : @(destination),
          (__bridge NSString *)kCVPixelBufferIOSurfacePropertiesKey : @{},
        };
        int32_t decoded = 0;
        VTDecompressionOutputCallbackRecord record = {jsb_probe_decode_cb, &decoded};
        VTDecompressionSessionRef dec = NULL;
        if (VTDecompressionSessionCreate(kCFAllocatorDefault, desc, (__bridge CFDictionaryRef)dspec, (__bridge CFDictionaryRef)dest,
                                         &record, &dec) == noErr) {
          for (int32_t i = 0; i < ctx.outputs; i++) {
            VTDecompressionSessionDecodeFrame(dec, ctx.samples[i], 0, NULL, NULL);
          }
          VTDecompressionSessionWaitForAsynchronousFrames(dec);
          VTDecompressionSessionInvalidate(dec);
          CFRelease(dec);
          result = decoded == ctx.outputs ? 1 : 0;
        }
      }
    }
    for (int32_t i = 0; i < ctx.outputs; i++) CFRelease(ctx.samples[i]);
    VTCompressionSessionInvalidate(enc);
    CFRelease(enc);
    return result;
  }
}

typedef struct jsb_caps {
  int32_t hevc_444;
  int32_t hevc_420;
  int32_t h264_444;
  int32_t h264_420;
} jsb_caps;

void jsb_probe_capabilities(jsb_caps *out) {
  out->hevc_444 = jsb_probe_path(1, "HEVC_Main444_AutoLevel", kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange);
  out->hevc_420 = jsb_probe_path(1, "HEVC_Main_AutoLevel", kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
  out->h264_444 = jsb_probe_path(2, "H264_High444Predictive_AutoLevel", kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange);
  out->h264_420 = jsb_probe_path(2, "H264_High_AutoLevel", kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
}
