/* H264 and H265 decoded by VideoToolbox, for macOS: the FFmpeg in LiveKit's
 * macOS libwebrtc archive has no H264 or HEVC decoder, since WebRTC there
 * decodes with VideoToolbox too. Access units come as Annex B (Go Live) or
 * length-prefixed with an avcC record (files); either way they're rewritten
 * with 4-byte lengths and their parameter sets taken out, which re-create the
 * session when they change. Decoding is synchronous, and pictures leave as
 * I420 converted from VideoToolbox's NV12 into one reused buffer. Also gives
 * hevc.c's interface for watched H265 streams. */
#include "vtdec.h"

#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>
#include <VideoToolbox/VideoToolbox.h>
#include <limits.h>
#include <stdlib.h>
#include <string.h>

#include "libyuv/convert.h"

/* H264's most reference pictures: no stream holds back more than these. */
enum { HELD = 17 };

/* The SPS bytes read for its colours and reordering; past them, it reads as
 * zeros, as if it said nothing more. */
enum { SPS_BYTES = 256 };

struct parameter_set {
  uint8_t *data;
  size_t size;
  size_t capacity;
};

struct held_picture {
  CVPixelBufferRef buffer;
  int64_t pts;
};

struct vtdec {
  int codec;
  int reorder;
  /* The bytes of a sample's length prefixes, or 0 for Annex B. */
  int length_size;
  /* H264's SPS and PPS, or H265's VPS, SPS and PPS: the last of each. */
  struct parameter_set sets[3];
  int changed;
  CMVideoFormatDescriptionRef format;
  VTDecompressionSessionRef session;
  /* Pictures the stream may reorder, and its colours, from its SPS. */
  int delay;
  int bt709;
  int full_range;
  /* The access unit rewritten for VideoToolbox. */
  uint8_t *sample;
  size_t sample_capacity;
  /* What the output callback was given for the frame being decoded. */
  CVPixelBufferRef decoded;
  OSStatus status;
  struct held_picture held[HELD];
  int held_count;
  uint8_t *i420;
  size_t i420_size;
};

static int set_count(const struct vtdec *decoder) {
  return decoder->codec == VTDEC_HEVC ? 3 : 2;
}

/* Which of `sets` a NAL unit is, -1 for none, or -2 for one left out. */
static int set_index(const struct vtdec *decoder, const uint8_t *nal) {
  if (decoder->codec == VTDEC_HEVC) {
    int type = (nal[0] >> 1) & 0x3f;
    /* VPS, SPS, PPS; then the access unit delimiter. */
    return type >= 32 && type <= 34 ? type - 32 : type == 35 ? -2 : -1;
  }
  int type = nal[0] & 0x1f;
  return type == 7 ? 0 : type == 8 ? 1 : type == 9 ? -2 : -1;
}

/* Keeps a parameter set, noting whether it changed. */
static int keep_set(struct vtdec *decoder, int index, const uint8_t *nal, size_t size) {
  struct parameter_set *set = &decoder->sets[index];
  if (set->size == size && memcmp(set->data, nal, size) == 0) return 0;
  if (set->capacity < size) {
    uint8_t *grown = realloc(set->data, size);
    if (!grown) return -3;
    set->data = grown;
    set->capacity = size;
  }
  memcpy(set->data, nal, size);
  set->size = size;
  decoder->changed = 1;
  return 0;
}

/* The next NAL unit at `*at`: 1 with one, 0 at the end, -1 when a length
 * runs past the end. */
static int next_nal(const struct vtdec *decoder, const uint8_t *data, size_t size, size_t *at,
                    const uint8_t **nal, size_t *nal_size) {
  for (;;) {
    size_t start = *at;
    if (decoder->length_size) {
      if (start == size) return 0;
      if (size - start < (size_t)decoder->length_size) return -1;
      size_t length = 0;
      for (int byte = 0; byte < decoder->length_size; byte++) length = length << 8 | data[start++];
      if (length > size - start) return -1;
      *at = start + length;
      *nal = data + start;
      *nal_size = length;
    } else {
      while (start + 3 <= size && (data[start] || data[start + 1] || data[start + 2] != 1)) start++;
      if (start + 3 > size) return 0;
      start += 3;
      /* It ends where the next start code, or trailing zeros, begin. */
      size_t end = start;
      while (end + 3 <= size && (data[end] || data[end + 1] || data[end + 2] > 1)) end++;
      if (end + 3 > size) end = size;
      *at = end;
      *nal = data + start;
      *nal_size = end - start;
    }
    if (*nal_size > 0) return 1;
  }
}

/* An SPS's bits, read past the emulation prevention bytes. */
struct bits {
  uint8_t data[SPS_BYTES];
  size_t size;
  size_t at;
};

static unsigned read_bit(struct bits *bits) {
  size_t at = bits->at++;
  if (at >= bits->size * 8) return 0;
  return (bits->data[at >> 3] >> (7 - (at & 7))) & 1;
}

static unsigned read_bits(struct bits *bits, int count) {
  unsigned value = 0;
  while (count-- > 0) value = value << 1 | read_bit(bits);
  return value;
}

static unsigned read_ue(struct bits *bits) {
  int zeros = 0;
  while (!read_bit(bits)) {
    if (++zeros == 32) return UINT_MAX;
  }
  return ((1u << zeros) - 1) + read_bits(bits, zeros);
}

static int read_se(struct bits *bits) {
  unsigned value = read_ue(bits);
  return value & 1 ? (int)(value / 2 + 1) : -(int)(value / 2);
}

static void skip_scaling_list(struct bits *bits, int size) {
  int last = 8, next = 8;
  for (int index = 0; index < size; index++) {
    if (next) next = (last + read_se(bits) + 256) % 256;
    if (next) last = next;
  }
}

static void skip_hrd(struct bits *bits) {
  unsigned count = read_ue(bits) + 1;
  read_bits(bits, 8);
  for (unsigned index = 0; index < count && index < 32; index++) {
    read_ue(bits);
    read_ue(bits);
    read_bit(bits);
  }
  read_bits(bits, 20);
}

/* The most pictures an H264 decoder holds, for `level` and pictures of
 * `macroblocks`, as Annex A's MaxDpbMbs gives; at most 16. */
static int max_dpb_frames(int level, unsigned macroblocks) {
  static const struct {
    int level;
    unsigned mbs;
  } LIMITS[] = {{9, 396},     {10, 396},    {11, 900},    {12, 2376},   {13, 2376},
                {20, 2376},   {21, 4752},   {22, 8100},   {30, 8100},   {31, 18000},
                {32, 20480},  {40, 32768},  {41, 32768},  {42, 34816},  {50, 110400},
                {51, 184320}, {52, 184320}, {60, 696320}, {61, 696320}, {62, 696320}};
  unsigned limit = 696320;
  for (size_t index = 0; index < sizeof LIMITS / sizeof *LIMITS; index++) {
    if (LIMITS[index].level == level) limit = LIMITS[index].mbs;
  }
  unsigned frames = macroblocks ? limit / macroblocks : 16;
  return frames > 16 ? 16 : (int)frames;
}

/* Reads what VideoToolbox doesn't tell from an H264 SPS: whether its
 * pictures can be decoded, how many it reorders, and its colours. 0, or -2
 * for 4:2:2, 4:4:4 or more than 8 bits, which VideoToolbox doesn't decode. */
static int read_sps(struct vtdec *decoder, const uint8_t *nal, size_t size) {
  struct bits bits = {.size = 0};
  int zeros = 0;
  for (size_t index = 1; index < size && bits.size < SPS_BYTES; index++) {
    if (zeros >= 2 && nal[index] == 3) {
      zeros = 0;
      continue;
    }
    zeros = nal[index] ? 0 : zeros + 1;
    bits.data[bits.size++] = nal[index];
  }
  int profile = (int)read_bits(&bits, 8);
  read_bits(&bits, 8);
  int level = (int)read_bits(&bits, 8);
  read_ue(&bits);
  unsigned chroma = 1, luma_depth = 8, chroma_depth = 8;
  switch (profile) {
    case 100: case 110: case 122: case 244: case 44: case 83:
    case 86: case 118: case 128: case 138: case 139: case 134: case 135:
      chroma = read_ue(&bits);
      if (chroma == 3) read_bit(&bits);
      luma_depth = read_ue(&bits) + 8;
      chroma_depth = read_ue(&bits) + 8;
      read_bit(&bits);
      if (read_bit(&bits)) {
        for (int list = 0; list < (chroma == 3 ? 12 : 8); list++) {
          if (read_bit(&bits)) skip_scaling_list(&bits, list < 6 ? 16 : 64);
        }
      }
      break;
    default:
      break;
  }
  if (chroma != 1 || luma_depth != 8 || chroma_depth != 8) return -2;
  read_ue(&bits);
  unsigned order_type = read_ue(&bits);
  if (order_type == 0) {
    read_ue(&bits);
  } else if (order_type == 1) {
    read_bit(&bits);
    read_se(&bits);
    read_se(&bits);
    unsigned cycle = read_ue(&bits);
    for (unsigned index = 0; index < cycle && index < 256; index++) read_se(&bits);
  }
  read_ue(&bits);
  read_bit(&bits);
  unsigned width = read_ue(&bits) + 1;
  unsigned height = read_ue(&bits) + 1;
  if (!read_bit(&bits)) {
    height *= 2;
    read_bit(&bits);
  }
  read_bit(&bits);
  if (read_bit(&bits)) {
    for (int side = 0; side < 4; side++) read_ue(&bits);
  }
  /* Baseline has no B frames; otherwise, without its own count, a stream
   * may reorder as many pictures as its level holds. */
  decoder->delay = profile == 66 ? 0 : max_dpb_frames(level, width * height);
  decoder->bt709 = -1;
  decoder->full_range = 0;
  if (!read_bit(&bits)) return 0;
  if (read_bit(&bits) && read_bits(&bits, 8) == 255) read_bits(&bits, 32);
  if (read_bit(&bits)) read_bit(&bits);
  if (read_bit(&bits)) {
    read_bits(&bits, 3);
    decoder->full_range = (int)read_bit(&bits);
    if (read_bit(&bits)) {
      read_bits(&bits, 16);
      unsigned matrix = read_bits(&bits, 8);
      decoder->bt709 = matrix == 1 ? 1 : matrix == 5 || matrix == 6 ? 0 : -1;
    }
  }
  if (read_bit(&bits)) {
    read_ue(&bits);
    read_ue(&bits);
  }
  if (read_bit(&bits)) {
    read_bits(&bits, 32);
    read_bits(&bits, 32);
    read_bit(&bits);
  }
  int nal_hrd = (int)read_bit(&bits);
  if (nal_hrd) skip_hrd(&bits);
  int vcl_hrd = (int)read_bit(&bits);
  if (vcl_hrd) skip_hrd(&bits);
  if (nal_hrd || vcl_hrd) read_bit(&bits);
  read_bit(&bits);
  if (read_bit(&bits) && bits.at <= bits.size * 8) {
    read_bit(&bits);
    for (int field = 0; field < 4; field++) read_ue(&bits);
    unsigned reorder = read_ue(&bits);
    /* A damaged SPS keeps the level's count. */
    if (bits.at <= bits.size * 8 && reorder <= 16) decoder->delay = (int)reorder;
  }
  return 0;
}

static void drop_session(struct vtdec *decoder) {
  if (decoder->session) {
    VTDecompressionSessionInvalidate(decoder->session);
    CFRelease(decoder->session);
    decoder->session = NULL;
  }
  if (decoder->format) {
    CFRelease(decoder->format);
    decoder->format = NULL;
  }
}

/* Runs inside VTDecompressionSessionDecodeFrame, which decodes
 * synchronously. */
static void output(void *opaque, void *frame, OSStatus status, VTDecodeInfoFlags flags,
                   CVImageBufferRef image, CMTime pts, CMTime duration) {
  (void)frame, (void)flags, (void)pts, (void)duration;
  struct vtdec *decoder = opaque;
  decoder->status = status;
  if (status == noErr && image && !decoder->decoded) decoder->decoded = CVPixelBufferRetain(image);
}

/* A session for the parameter sets kept. */
static int open_session(struct vtdec *decoder, int max_side) {
  drop_session(decoder);
  const uint8_t *sets[3];
  size_t sizes[3];
  for (int index = 0; index < set_count(decoder); index++) {
    sets[index] = decoder->sets[index].data;
    sizes[index] = decoder->sets[index].size;
  }
  OSStatus status =
      decoder->codec == VTDEC_HEVC
          ? CMVideoFormatDescriptionCreateFromHEVCParameterSets(NULL, 3, sets, sizes, 4, NULL,
                                                                &decoder->format)
          : CMVideoFormatDescriptionCreateFromH264ParameterSets(NULL, 2, sets, sizes, 4,
                                                                &decoder->format);
  if (status != noErr) return -1;
  CMVideoDimensions size = CMVideoFormatDescriptionGetDimensions(decoder->format);
  if (size.width <= 0 || size.height <= 0 || size.width > max_side || size.height > max_side) {
    return -2;
  }
  if (decoder->codec == VTDEC_H264) {
    int read = read_sps(decoder, sets[0], sizes[0]);
    if (read < 0) return read;
  } else {
    decoder->delay = 0;
    decoder->bt709 = -1;
    decoder->full_range = 0;
  }
  /* In the stream's own range, so VideoToolbox doesn't convert it. */
  int32_t pixels = decoder->full_range ? kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
                                       : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
  CFNumberRef number = CFNumberCreate(NULL, kCFNumberSInt32Type, &pixels);
  if (!number) return -3;
  const void *keys[] = {kCVPixelBufferPixelFormatTypeKey};
  const void *values[] = {number};
  CFDictionaryRef attributes = CFDictionaryCreate(NULL, keys, values, 1,
                                                  &kCFTypeDictionaryKeyCallBacks,
                                                  &kCFTypeDictionaryValueCallBacks);
  CFRelease(number);
  if (!attributes) return -3;
  VTDecompressionOutputCallbackRecord callback = {output, decoder};
  status = VTDecompressionSessionCreate(NULL, decoder->format, NULL, attributes, &callback,
                                        &decoder->session);
  CFRelease(attributes);
  if (status != noErr) {
    decoder->session = NULL;
    return status == kVTCouldNotFindVideoDecoderErr ? -2 : -1;
  }
  decoder->changed = 0;
  return 0;
}

struct vtdec *vtdec_new(int codec, int reorder) {
  if (codec != VTDEC_H264 && codec != VTDEC_HEVC) return NULL;
  struct vtdec *decoder = calloc(1, sizeof *decoder);
  if (!decoder) return NULL;
  decoder->codec = codec;
  decoder->reorder = reorder;
  return decoder;
}

void vtdec_flush(struct vtdec *decoder) {
  for (int index = 0; index < decoder->held_count; index++) {
    CVPixelBufferRelease(decoder->held[index].buffer);
  }
  decoder->held_count = 0;
}

void vtdec_free(struct vtdec *decoder) {
  if (!decoder) return;
  vtdec_flush(decoder);
  drop_session(decoder);
  for (int index = 0; index < 3; index++) free(decoder->sets[index].data);
  free(decoder->sample);
  free(decoder->i420);
  free(decoder);
}

int vtdec_configure(struct vtdec *decoder, const uint8_t *record, size_t size) {
  /* avcC: version 1, profile, compatibility, level, length size, then the
   * SPSs and the PPSs, each with its size. The first of each is kept. */
  if (decoder->codec != VTDEC_H264 || size < 7 || record[0] != 1) return -1;
  int length_size = (record[4] & 3) + 1;
  if (length_size == 3) return -1;
  size_t at = 5;
  for (int index = 0; index < 2; index++) {
    if (at >= size) return -1;
    int count = index == 0 ? record[at] & 0x1f : record[at];
    at++;
    if (count == 0) return -1;
    for (int set = 0; set < count; set++) {
      if (size - at < 2) return -1;
      size_t length = (size_t)record[at] << 8 | record[at + 1];
      at += 2;
      if (length == 0 || length > size - at) return -1;
      if (set == 0 && keep_set(decoder, index, record + at, length) < 0) return -3;
      at += length;
    }
  }
  decoder->length_size = length_size;
  return read_sps(decoder, decoder->sets[0].data, decoder->sets[0].size);
}

/* Appends a NAL unit with a 4-byte length to the sample. */
static int append(struct vtdec *decoder, size_t *size, const uint8_t *nal, size_t nal_size) {
  if (nal_size > UINT32_MAX) return -1;
  size_t needed = *size + 4 + nal_size;
  if (decoder->sample_capacity < needed) {
    size_t capacity = needed + needed / 2;
    uint8_t *grown = realloc(decoder->sample, capacity);
    if (!grown) return -3;
    decoder->sample = grown;
    decoder->sample_capacity = capacity;
  }
  uint8_t *at = decoder->sample + *size;
  at[0] = (uint8_t)(nal_size >> 24);
  at[1] = (uint8_t)(nal_size >> 16);
  at[2] = (uint8_t)(nal_size >> 8);
  at[3] = (uint8_t)nal_size;
  memcpy(at + 4, nal, nal_size);
  *size = needed;
  return 0;
}

int vtdec_decode(struct vtdec *decoder, const uint8_t *data, size_t size, int64_t pts,
                 int max_side) {
  size_t at = 0, sample_size = 0;
  const uint8_t *nal;
  size_t nal_size;
  int got;
  while ((got = next_nal(decoder, data, size, &at, &nal, &nal_size)) > 0) {
    int index = set_index(decoder, nal);
    int result = index >= 0    ? keep_set(decoder, index, nal, nal_size)
                 : index == -1 ? append(decoder, &sample_size, nal, nal_size)
                               : 0;
    if (result < 0) return result;
  }
  if (got < 0) return -1;
  if (decoder->changed || !decoder->session) {
    for (int index = 0; index < set_count(decoder); index++) {
      /* Pictures before their parameter sets can't be decoded. */
      if (!decoder->sets[index].size) return sample_size ? -1 : 0;
    }
    int opened = open_session(decoder, max_side);
    if (opened < 0) {
      drop_session(decoder);
      return opened;
    }
  }
  if (!sample_size) return 0;
  if (decoder->held_count == HELD) return -1;
  CMBlockBufferRef block = NULL;
  CMSampleBufferRef sample = NULL;
  /* VideoToolbox reads the sample in place: kCFAllocatorNull frees nothing. */
  OSStatus status =
      CMBlockBufferCreateWithMemoryBlock(NULL, decoder->sample, sample_size, kCFAllocatorNull,
                                         NULL, 0, sample_size, 0, &block);
  if (status == noErr) {
    status = CMSampleBufferCreateReady(NULL, block, decoder->format, 1, 0, NULL, 1, &sample_size,
                                       &sample);
  }
  if (status != noErr) {
    if (block) CFRelease(block);
    return -3;
  }
  decoder->decoded = NULL;
  decoder->status = noErr;
  VTDecodeInfoFlags flags = 0;
  status = VTDecompressionSessionDecodeFrame(decoder->session, sample, 0, NULL, &flags);
  CFRelease(sample);
  CFRelease(block);
  CVPixelBufferRef decoded = decoder->decoded;
  decoder->decoded = NULL;
  /* macOS ends sessions across sleep; the next access unit opens another. */
  if (status == kVTInvalidSessionErr) drop_session(decoder);
  if (status != noErr || decoder->status != noErr) {
    CVPixelBufferRelease(decoded);
    return -1;
  }
  if (!decoded) return 0;
  decoder->held[decoder->held_count++] = (struct held_picture){decoded, pts};
  return 0;
}

int vtdec_picture(struct vtdec *decoder, int drain, struct vtdec_picture *picture) {
  int delay = decoder->reorder ? decoder->delay : 0;
  if (decoder->held_count == 0 || (!drain && decoder->held_count <= delay)) return 0;
  /* VideoToolbox gives pictures in decoding order. */
  int next = 0;
  for (int index = 1; decoder->reorder && index < decoder->held_count; index++) {
    if (decoder->held[index].pts < decoder->held[next].pts) next = index;
  }
  struct held_picture held = decoder->held[next];
  memmove(&decoder->held[next], &decoder->held[next + 1],
          (size_t)(decoder->held_count - next - 1) * sizeof *decoder->held);
  decoder->held_count--;
  CVPixelBufferRef buffer = held.buffer;
  OSType pixels = CVPixelBufferGetPixelFormatType(buffer);
  if ((pixels != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange &&
       pixels != kCVPixelFormatType_420YpCbCr8BiPlanarFullRange) ||
      CVPixelBufferGetPlaneCount(buffer) != 2 ||
      CVPixelBufferLockBaseAddress(buffer, kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess) {
    CVPixelBufferRelease(buffer);
    return -2;
  }
  int width = (int)CVPixelBufferGetWidth(buffer);
  int height = (int)CVPixelBufferGetHeight(buffer);
  int chroma_width = (width + 1) / 2, chroma_height = (height + 1) / 2;
  size_t luma = (size_t)width * (size_t)height;
  size_t chroma = (size_t)chroma_width * (size_t)chroma_height;
  int result = 1;
  if (decoder->i420_size < luma + 2 * chroma) {
    uint8_t *grown = realloc(decoder->i420, luma + 2 * chroma);
    if (grown) {
      decoder->i420 = grown;
      decoder->i420_size = luma + 2 * chroma;
    } else {
      result = -3;
    }
  }
  if (result == 1) {
    uint8_t *y = decoder->i420, *u = y + luma, *v = u + chroma;
    if (NV12ToI420(CVPixelBufferGetBaseAddressOfPlane(buffer, 0),
                   (int)CVPixelBufferGetBytesPerRowOfPlane(buffer, 0),
                   CVPixelBufferGetBaseAddressOfPlane(buffer, 1),
                   (int)CVPixelBufferGetBytesPerRowOfPlane(buffer, 1), y, width, u, chroma_width,
                   v, chroma_width, width, height)) {
      result = -1;
    }
    *picture = (struct vtdec_picture){
        .width = width,
        .height = height,
        .y = y,
        .u = u,
        .v = v,
        .y_stride = width,
        .u_stride = chroma_width,
        .v_stride = chroma_width,
        .pts = held.pts,
        .bt709 = decoder->bt709,
        .full_range = pixels == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    };
  }
  CVPixelBufferUnlockBaseAddress(buffer, kCVPixelBufferLock_ReadOnly);
  CVPixelBufferRelease(buffer);
  return result;
}

/* hevc.c's interface, for watched H265 streams. */

/* The widest or tallest picture a stream may send, as in decode.c. */
enum { MAX_SIDE = 8192 };

struct yapfast_picture {
  int width;
  int height;
  const unsigned char *y;
  const unsigned char *u;
  const unsigned char *v;
  int y_stride;
  int u_stride;
  int v_stride;
};

struct vtdec *yapfast_hevc_new(unsigned threads) {
  /* VideoToolbox picks its own threads, or the GPU's decoder. */
  (void)threads;
  return vtdec_new(VTDEC_HEVC, 0);
}

void yapfast_hevc_free(struct vtdec *decoder) {
  vtdec_free(decoder);
}

/* Decodes one frame. Returns 1 with a picture, 0 when the decoder has none
 * yet, or a negative number when the frame couldn't be decoded. */
int yapfast_hevc_decode(struct vtdec *decoder, const unsigned char *data, size_t size,
                        struct yapfast_picture *picture) {
  int decoded = vtdec_decode(decoder, data, size, 0, MAX_SIDE);
  if (decoded < 0) return decoded;
  struct vtdec_picture got;
  int result = vtdec_picture(decoder, 0, &got);
  if (result <= 0) return result;
  *picture = (struct yapfast_picture){got.width,    got.height,   got.y,       got.u, got.v,
                                      got.y_stride, got.u_stride, got.v_stride};
  return 1;
}
