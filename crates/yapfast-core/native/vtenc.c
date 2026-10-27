/* Hardware encoding for Go Live on macOS, through VideoToolbox: hwenc.c's
 * interface, in its place, since the FFmpeg in vendor/ffmpeg-src isn't built
 * there. I420 frames in, Annex B access units out, each NAL unit after a
 * start code, the parameter sets before each IDR.
 *
 * Settings, as hwenc.c's (docs/discord/go-live.md): real time, no
 * reordering, a frame out for each frame in, H264's or H265's Main profile,
 * a keyframe every 3 seconds and when asked, and the bitrate capped over half
 * a second. Encoding is synchronous: each frame is complete before
 * yapfast_hwenc_encode returns. */
#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>
#include <VideoToolbox/VideoToolbox.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "libyuv/convert_from.h"

/* The codecs, as Rust numbers them. */
enum { CODEC_H264, CODEC_HEVC, CODEC_AV1 };

struct yapfast_hwenc {
  VTCompressionSessionRef session;
  int codec;
  int width, height;
  int64_t last_pts;
  /* The options of a frame that must be a keyframe, and the colours each
   * picture is tagged with. */
  CFDictionaryRef force_key;
  CFDictionaryRef colours;
  /* Where the output callback writes the frame being encoded, and what it
   * found. */
  uint8_t *out;
  size_t capacity;
  size_t size;
  int is_key;
  int failed;
};

static CFNumberRef number(int32_t value) {
  return CFNumberCreate(NULL, kCFNumberSInt32Type, &value);
}

static OSStatus set_int(VTCompressionSessionRef session, CFStringRef key, int32_t value) {
  CFNumberRef boxed = number(value);
  OSStatus status = VTSessionSetProperty(session, key, boxed);
  CFRelease(boxed);
  return status;
}

/* Aims for `kbps`, never above it over half a second, as h264.c's VBV. */
static OSStatus set_rate(VTCompressionSessionRef session, unsigned kbps) {
  int64_t bps = (int64_t)kbps * 1000;
  OSStatus status = set_int(session, kVTCompressionPropertyKey_AverageBitRate,
                            bps > INT32_MAX ? INT32_MAX : (int32_t)bps);
  if (status != noErr) return status;
  double bytes = kbps * 125.0 * 0.5, seconds = 0.5;
  CFNumberRef limit[2] = {CFNumberCreate(NULL, kCFNumberDoubleType, &bytes),
                          CFNumberCreate(NULL, kCFNumberDoubleType, &seconds)};
  CFArrayRef limits = CFArrayCreate(NULL, (const void **)limit, 2, &kCFTypeArrayCallBacks);
  status = VTSessionSetProperty(session, kVTCompressionPropertyKey_DataRateLimits, limits);
  CFRelease(limits);
  CFRelease(limit[0]);
  CFRelease(limit[1]);
  return status;
}

/* Appends `size` bytes of `block` at `offset` to the output, after a start
 * code. */
static int append_nal(struct yapfast_hwenc *h, CMBlockBufferRef block, size_t offset,
                      const uint8_t *bytes, size_t size) {
  if (h->size + 4 + size > h->capacity) return -2;
  memcpy(h->out + h->size, "\0\0\0\1", 4);
  h->size += 4;
  if (block) {
    if (CMBlockBufferCopyDataBytes(block, offset, size, h->out + h->size) != kCMBlockBufferNoErr)
      return -1;
  } else {
    memcpy(h->out + h->size, bytes, size);
  }
  h->size += size;
  return 0;
}

/* The format's parameter sets, VPS, SPS and PPS for H265, SPS and PPS for
 * H264, each after a start code. */
static int append_parameter_sets(struct yapfast_hwenc *h, CMFormatDescriptionRef format) {
  for (size_t index = 0;; index++) {
    const uint8_t *set = NULL;
    size_t size = 0, count = 0;
    OSStatus status =
        h->codec == CODEC_HEVC
            ? CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format, index, &set, &size,
                                                                 &count, NULL)
            : CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, index, &set, &size,
                                                                 &count, NULL);
    if (status != noErr) return -1;
    int ret = append_nal(h, NULL, 0, set, size);
    if (ret < 0) return ret;
    if (index + 1 >= count) return 0;
  }
}

/* Writes an encoded sample as Annex B: its NAL units come with 4-byte
 * lengths, as the format description says. */
static int write_sample(struct yapfast_hwenc *h, CMSampleBufferRef sample) {
  CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
  int key = 1;
  if (attachments && CFArrayGetCount(attachments) > 0) {
    CFDictionaryRef first = CFArrayGetValueAtIndex(attachments, 0);
    CFBooleanRef not_sync = CFDictionaryGetValue(first, kCMSampleAttachmentKey_NotSync);
    key = !(not_sync && CFBooleanGetValue(not_sync));
  }
  CMFormatDescriptionRef format = CMSampleBufferGetFormatDescription(sample);
  if (!format) return -1;
  int length_size = 4;
  OSStatus status =
      h->codec == CODEC_HEVC
          ? CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format, 0, NULL, NULL, NULL,
                                                               &length_size)
          : CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format, 0, NULL, NULL, NULL,
                                                               &length_size);
  if (status != noErr || length_size < 1 || length_size > 4) return -1;
  if (key) {
    int ret = append_parameter_sets(h, format);
    if (ret < 0) return ret;
  }
  CMBlockBufferRef block = CMSampleBufferGetDataBuffer(sample);
  if (!block) return -1;
  size_t total = CMBlockBufferGetDataLength(block);
  for (size_t at = 0; at < total;) {
    uint8_t prefix[4];
    if (total - at < (size_t)length_size ||
        CMBlockBufferCopyDataBytes(block, at, (size_t)length_size, prefix) != kCMBlockBufferNoErr)
      return -1;
    size_t length = 0;
    for (int byte = 0; byte < length_size; byte++) length = length << 8 | prefix[byte];
    at += (size_t)length_size;
    if (length > total - at) return -1;
    int ret = append_nal(h, block, at, NULL, length);
    if (ret < 0) return ret;
    at += length;
  }
  h->is_key |= key;
  return 0;
}

/* Runs before VTCompressionSessionCompleteFrames returns. */
static void output(void *opaque, void *frame, OSStatus status, VTEncodeInfoFlags flags,
                   CMSampleBufferRef sample) {
  (void)frame;
  struct yapfast_hwenc *h = opaque;
  if (status != noErr) {
    h->failed = -1;
    return;
  }
  /* A frame the encoder dropped gives no sample. */
  if (!sample || (flags & kVTEncodeInfo_FrameDropped) || h->failed) return;
  int ret = write_sample(h, sample);
  if (ret < 0) h->failed = ret;
}

void yapfast_hwenc_free(struct yapfast_hwenc *h) {
  if (!h) return;
  if (h->session) {
    VTCompressionSessionInvalidate(h->session);
    CFRelease(h->session);
  }
  if (h->force_key) CFRelease(h->force_key);
  if (h->colours) CFRelease(h->colours);
  free(h);
}

static CFDictionaryRef dictionary(const void **keys, const void **values, CFIndex count) {
  return CFDictionaryCreate(NULL, keys, values, count, &kCFTypeDictionaryKeyCallBacks,
                            &kCFTypeDictionaryValueCallBacks);
}

/* Opens and configures the session, or says which step failed. */
static OSStatus open_session(struct yapfast_hwenc *h, unsigned fps, unsigned kbps,
                             const char **step) {
  const void *spec_keys[] = {kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder};
  const void *spec_values[] = {kCFBooleanTrue};
  CFDictionaryRef specification = dictionary(spec_keys, spec_values, 1);
  /* The pool VideoToolbox makes holds NV12 in limited range, which it reads
   * without a copy. */
  CFNumberRef pixel_format = number(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
  CFNumberRef width = number(h->width), height = number(h->height);
  CFDictionaryRef surface = dictionary(NULL, NULL, 0);
  const void *source_keys[] = {kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
                               kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey};
  const void *source_values[] = {pixel_format, width, height, surface};
  CFDictionaryRef source = dictionary(source_keys, source_values, 4);
  *step = "creating the session";
  OSStatus status = VTCompressionSessionCreate(
      NULL, h->width, h->height,
      h->codec == CODEC_HEVC ? kCMVideoCodecType_HEVC : kCMVideoCodecType_H264, specification,
      source, NULL, output, h, &h->session);
  CFRelease(specification);
  CFRelease(source);
  CFRelease(surface);
  CFRelease(pixel_format);
  CFRelease(width);
  CFRelease(height);
  if (status != noErr) return status;
  VTCompressionSessionRef session = h->session;
  struct {
    CFStringRef key;
    CFTypeRef value;
  } properties[] = {
      {kVTCompressionPropertyKey_RealTime, kCFBooleanTrue},
      {kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse},
      {kVTCompressionPropertyKey_ProfileLevel, h->codec == CODEC_HEVC
                                                   ? kVTProfileLevel_HEVC_Main_AutoLevel
                                                   : kVTProfileLevel_H264_Main_AutoLevel},
      /* libyuv converts captured pixels with BT.601's limited range;
       * SMPTE 170M's transfer is BT.709's curve. */
      {kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_SMPTE_C},
      {kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2},
      {kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_601_4},
  };
  for (size_t index = 0; index < sizeof properties / sizeof *properties; index++) {
    *step = "setting a property";
    status = VTSessionSetProperty(session, properties[index].key, properties[index].value);
    if (status != noErr) return status;
  }
  *step = "setting the keyframe interval";
  /* A new viewer waits at most this long for a picture. */
  status = set_int(session, kVTCompressionPropertyKey_MaxKeyFrameInterval, (int32_t)fps * 3);
  if (status == noErr) {
    CFNumberRef seconds = number(3);
    status = VTSessionSetProperty(session, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                                  seconds);
    CFRelease(seconds);
  }
  if (status != noErr) return status;
  *step = "setting the frame rate";
  status = set_int(session, kVTCompressionPropertyKey_ExpectedFrameRate, (int32_t)fps);
  if (status != noErr) return status;
  *step = "setting the bitrate";
  status = set_rate(session, kbps);
  if (status != noErr) return status;
  *step = "preparing to encode";
  return VTCompressionSessionPrepareToEncodeFrames(session);
}

/* Opens VideoToolbox's hardware encoder for `codec_id` and writes its name
 * to `report`, or why it didn't open. AV1 has none. */
struct yapfast_hwenc *yapfast_hwenc_new(unsigned codec_id, unsigned width, unsigned height,
                                        unsigned fps, unsigned kbps, char *report,
                                        size_t report_size) {
  if (report_size) report[0] = 0;
  if (codec_id > CODEC_AV1) return NULL;
  const char *label = codec_id == CODEC_HEVC ? "VideoToolbox H265" : "VideoToolbox H264";
  if (codec_id == CODEC_AV1) {
    if (report_size) snprintf(report, report_size, "VideoToolbox: no AV1 encoder");
    return NULL;
  }
  struct yapfast_hwenc *h = calloc(1, sizeof *h);
  if (!h) return NULL;
  h->codec = (int)codec_id;
  h->width = (int)width;
  h->height = (int)height;
  h->last_pts = INT64_MIN;
  const void *key_keys[] = {kVTEncodeFrameOptionKey_ForceKeyFrame};
  const void *key_values[] = {kCFBooleanTrue};
  h->force_key = dictionary(key_keys, key_values, 1);
  const void *colour_keys[] = {kCVImageBufferColorPrimariesKey, kCVImageBufferTransferFunctionKey,
                               kCVImageBufferYCbCrMatrixKey};
  const void *colour_values[] = {kCVImageBufferColorPrimaries_SMPTE_C,
                                 kCVImageBufferTransferFunction_ITU_R_709_2,
                                 kCVImageBufferYCbCrMatrix_ITU_R_601_4};
  h->colours = dictionary(colour_keys, colour_values, 3);
  const char *step = "starting";
  OSStatus status = h->force_key && h->colours ? open_session(h, fps ? fps : 30, kbps, &step) : -1;
  if (status == noErr) {
    if (report_size) snprintf(report, report_size, "%s", label);
    return h;
  }
  if (report_size)
    snprintf(report, report_size, "%s: %s failed (OSStatus %d)", label, step, (int)status);
  yapfast_hwenc_free(h);
  return NULL;
}

/* Encodes one I420 frame into `out`. Returns the encoded size, 0 when the
 * encoder dropped the frame, -2 when it doesn't fit, or -1 on failure. */
long yapfast_hwenc_encode(struct yapfast_hwenc *h, const unsigned char *y, int y_stride,
                          const unsigned char *u, int u_stride, const unsigned char *v,
                          int v_stride, long long pts, unsigned long duration, int keyframe,
                          unsigned char *out, size_t capacity, int *is_key) {
  *is_key = 0;
  CVPixelBufferPoolRef pool = VTCompressionSessionGetPixelBufferPool(h->session);
  CVPixelBufferRef picture = NULL;
  if (!pool || CVPixelBufferPoolCreatePixelBuffer(NULL, pool, &picture) != kCVReturnSuccess)
    return -1;
  CVBufferSetAttachments(picture, h->colours, kCVAttachmentMode_ShouldPropagate);
  if (CVPixelBufferLockBaseAddress(picture, 0) != kCVReturnSuccess) {
    CVPixelBufferRelease(picture);
    return -1;
  }
  int converted = I420ToNV12(
      y, y_stride, u, u_stride, v, v_stride, CVPixelBufferGetBaseAddressOfPlane(picture, 0),
      (int)CVPixelBufferGetBytesPerRowOfPlane(picture, 0),
      CVPixelBufferGetBaseAddressOfPlane(picture, 1),
      (int)CVPixelBufferGetBytesPerRowOfPlane(picture, 1), h->width, h->height);
  CVPixelBufferUnlockBaseAddress(picture, 0);
  if (converted != 0) {
    CVPixelBufferRelease(picture);
    return -1;
  }
  /* RTP's clock times the frames, which must go forward. */
  if (pts <= h->last_pts) pts = h->last_pts + (long long)(duration ? duration : 1);
  h->last_pts = pts;
  CMTime time = CMTimeMake(pts, 90000);
  h->out = out;
  h->capacity = capacity;
  h->size = 0;
  h->is_key = 0;
  h->failed = 0;
  OSStatus status = VTCompressionSessionEncodeFrame(
      h->session, picture, time, CMTimeMake((int64_t)duration, 90000),
      keyframe ? h->force_key : NULL, NULL, NULL);
  CVPixelBufferRelease(picture);
  if (status == noErr) status = VTCompressionSessionCompleteFrames(h->session, time);
  h->out = NULL;
  if (status != noErr) return -1;
  if (h->failed) return h->failed;
  *is_key = h->is_key;
  return (long)h->size;
}

/* Changes the bitrate aimed for, from the next frame on, without a
 * keyframe. 0, or -1 when VideoToolbox refused it. */
int yapfast_hwenc_set_bitrate(struct yapfast_hwenc *h, unsigned kbps) {
  return set_rate(h->session, kbps) == noErr ? 0 : -1;
}
