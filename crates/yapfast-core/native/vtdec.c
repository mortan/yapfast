/* H264 and H265 decoded by VideoToolbox, for macOS: the FFmpeg in LiveKit's
 * macOS libwebrtc archive has no H264 or HEVC decoder, since WebRTC there
 * decodes with VideoToolbox too. Access units come as Annex B (Go Live) or
 * length-prefixed with an avcC record (files); either way they're rewritten
 * with 4-byte lengths and their parameter sets taken out. Those are kept by
 * kind and id: one that changed opens another session with all of them, and
 * one with a new id goes to the open session, when it takes it, so that the
 * session keeps its reference pictures. Decoding is synchronous, and pictures
 * leave as
 * I420 converted from VideoToolbox's NV12 into one reused buffer. */
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

/* The bytes of a parameter set read for its id, or an SPS's for its colours
 * and reordering; past them, it reads as zeros, as if it said nothing more. */
enum { SET_BYTES = 256 };

/* The most parameter sets kept; past them, the one seen longest ago makes
 * way, as for an encoder that numbers each resolution's sets anew. */
enum { SETS = 32 };

struct parameter_set {
  /* H264's SPS or PPS, or H265's VPS, SPS or PPS, as set_kind numbers
   * them. */
  int kind;
  unsigned id;
  uint8_t *data;
  size_t size;
  size_t capacity;
  /* When it last came, on the decoder's clock. */
  uint64_t seen;
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
  /* The last parameter set of each kind and id. */
  struct parameter_set sets[SETS];
  int set_count;
  uint64_t clock;
  /* A kept set's bytes changed, or a set with a new id came. */
  int changed;
  int added;
  CMVideoFormatDescriptionRef format;
  VTDecompressionSessionRef session;
  /* Pictures the stream may reorder, from its SPSs, and its colours, from
   * the SPS it names now. */
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

static int kinds(const struct vtdec *decoder) {
  return decoder->codec == VTDEC_HEVC ? 3 : 2;
}

/* Which kind of parameter set a NAL unit is, -1 for none, or -2 for one left
 * out. */
static int set_kind(const struct vtdec *decoder, const uint8_t *nal) {
  if (decoder->codec == VTDEC_HEVC) {
    int type = (nal[0] >> 1) & 0x3f;
    /* VPS, SPS, PPS; then the access unit delimiter. */
    return type >= 32 && type <= 34 ? type - 32 : type == 35 ? -2 : -1;
  }
  int type = nal[0] & 0x1f;
  return type == 7 ? 0 : type == 8 ? 1 : type == 9 ? -2 : -1;
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

/* A parameter set's bits, read past the emulation prevention bytes. */
struct bits {
  uint8_t data[SET_BYTES];
  size_t size;
  size_t at;
};

/* Reads the bits of `nal` after its `header` bytes. */
static void read_payload(struct bits *bits, const uint8_t *nal, size_t size, size_t header) {
  bits->size = 0;
  bits->at = 0;
  int zeros = 0;
  for (size_t index = header; index < size && bits->size < SET_BYTES; index++) {
    if (zeros >= 2 && nal[index] == 3) {
      zeros = 0;
      continue;
    }
    zeros = nal[index] ? 0 : zeros + 1;
    bits->data[bits->size++] = nal[index];
  }
}

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

/* What VideoToolbox doesn't tell from an H264 SPS. */
struct sps {
  /* Its pictures' size, in pixels. */
  unsigned width;
  unsigned height;
  int delay;
  int bt709;
  int full_range;
};

/* Reads what VideoToolbox doesn't tell from an H264 SPS: whether its
 * pictures can be decoded, their size, how many it reorders, and its
 * colours. 0, or -2 for 4:2:2, 4:4:4 or more than 8 bits, which VideoToolbox
 * doesn't decode. */
static int read_sps(const uint8_t *nal, size_t size, struct sps *sps) {
  struct bits bits;
  read_payload(&bits, nal, size, 1);
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
  sps->width = width * 16;
  sps->height = height * 16;
  /* Baseline has no B frames; otherwise, without its own count, a stream
   * may reorder as many pictures as its level holds. */
  sps->delay = profile == 66 ? 0 : max_dpb_frames(level, width * height);
  sps->bt709 = -1;
  sps->full_range = 0;
  if (!read_bit(&bits)) return 0;
  if (read_bit(&bits) && read_bits(&bits, 8) == 255) read_bits(&bits, 32);
  if (read_bit(&bits)) read_bit(&bits);
  if (read_bit(&bits)) {
    read_bits(&bits, 3);
    sps->full_range = (int)read_bit(&bits);
    if (read_bit(&bits)) {
      read_bits(&bits, 16);
      unsigned matrix = read_bits(&bits, 8);
      sps->bt709 = matrix == 1 ? 1 : matrix == 5 || matrix == 6 ? 0 : -1;
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
    if (bits.at <= bits.size * 8 && reorder <= 16) sps->delay = (int)reorder;
  }
  return 0;
}

/* Skips H265's profile_tier_level, with `sublayers` past the first. */
static void skip_profile_tier_level(struct bits *bits, unsigned sublayers) {
  /* The general profile, its flags and its level. */
  bits->at += 96;
  unsigned present = read_bits(bits, 2 * (int)sublayers);
  if (sublayers > 0) bits->at += 2 * (8 - sublayers);
  for (unsigned layer = 0; layer < sublayers; layer++) {
    unsigned flags = present >> (2 * (sublayers - 1 - layer)) & 3;
    /* A sublayer's profile, then its level. */
    if (flags & 2) bits->at += 88;
    if (flags & 1) bits->at += 8;
  }
}

/* A parameter set's id, or -1 for a damaged one. */
static int set_id(const struct vtdec *decoder, int kind, const uint8_t *nal, size_t size) {
  struct bits bits;
  unsigned id, ids;
  if (decoder->codec == VTDEC_H264) {
    read_payload(&bits, nal, size, 1);
    /* An SPS's follows its profile, constraints and level. */
    if (kind == 0) bits.at += 24;
    id = read_ue(&bits);
    ids = kind == 0 ? 32 : 256;
  } else {
    read_payload(&bits, nal, size, 2);
    if (kind == 0) {
      id = read_bits(&bits, 4);
      ids = 16;
    } else if (kind == 1) {
      /* An SPS's follows its VPS's, its sublayers and their profiles. */
      bits.at += 4;
      unsigned sublayers = read_bits(&bits, 3);
      bits.at += 1;
      skip_profile_tier_level(&bits, sublayers);
      id = read_ue(&bits);
      ids = 16;
    } else {
      id = read_ue(&bits);
      ids = 64;
    }
  }
  return id < ids && bits.at <= bits.size * 8 ? (int)id : -1;
}

/* The first parameter set of `kind` kept, or NULL. */
static const struct parameter_set *first_set(const struct vtdec *decoder, int kind) {
  for (int index = 0; index < decoder->set_count; index++) {
    const struct parameter_set *set = &decoder->sets[index];
    if (set->kind == kind && set->size) return set;
  }
  return NULL;
}

/* The SPS that came last, which the stream's pictures name now, or NULL. */
static const struct parameter_set *current_sps(const struct vtdec *decoder) {
  int kind = decoder->codec == VTDEC_HEVC ? 1 : 0;
  const struct parameter_set *current = NULL;
  for (int index = 0; index < decoder->set_count; index++) {
    const struct parameter_set *set = &decoder->sets[index];
    if (set->kind == kind && set->size && (!current || set->seen > current->seen)) current = set;
  }
  return current;
}

/* Keeps a parameter set in place of the last of its kind and id, noting
 * whether that changes what's kept; with every slot taken, it replaces the
 * set seen longest ago. 0, -1 for a damaged one, or -3 out of memory. */
static int keep_set(struct vtdec *decoder, int kind, const uint8_t *nal, size_t size) {
  int id = set_id(decoder, kind, nal, size);
  if (id < 0) return -1;
  struct parameter_set *set = NULL;
  for (int index = 0; index < decoder->set_count && !set; index++) {
    struct parameter_set *kept = &decoder->sets[index];
    if (kept->kind == kind && kept->id == (unsigned)id) set = kept;
  }
  int *noted = &decoder->changed;
  if (!set) {
    if (decoder->set_count < SETS) {
      set = &decoder->sets[decoder->set_count++];
    } else {
      /* Never the last of its kind, as an avcC record's only SPS. */
      int of_kind[3] = {0};
      for (int index = 0; index < SETS; index++) of_kind[decoder->sets[index].kind]++;
      for (int index = 0; index < SETS; index++) {
        struct parameter_set *kept = &decoder->sets[index];
        if (of_kind[kept->kind] > 1 && (!set || kept->seen < set->seen)) set = kept;
      }
    }
    set->kind = kind;
    set->id = (unsigned)id;
    set->size = 0;
    noted = &decoder->added;
  }
  set->seen = ++decoder->clock;
  if (set->size == size && memcmp(set->data, nal, size) == 0) return 0;
  if (set->capacity < size) {
    uint8_t *grown = realloc(set->data, size);
    if (!grown) return -3;
    set->data = grown;
    set->capacity = size;
  }
  memcpy(set->data, nal, size);
  set->size = size;
  *noted = 1;
  return 0;
}

/* Reads what VideoToolbox doesn't tell from the kept SPSs: whether the
 * current one's pictures can be decoded, and its colours, and the most
 * pictures any of them reorders. An SPS the stream no longer names may be one
 * VideoToolbox can't decode. 0, or -2 for a current SPS VideoToolbox can't
 * decode or any bigger than `max_side`. */
static int read_sets(struct vtdec *decoder, int max_side) {
  decoder->delay = 0;
  decoder->bt709 = -1;
  decoder->full_range = 0;
  if (decoder->codec != VTDEC_H264) return 0;
  const struct parameter_set *current = current_sps(decoder);
  for (int index = 0; index < decoder->set_count; index++) {
    const struct parameter_set *set = &decoder->sets[index];
    if (set->kind != 0 || !set->size) continue;
    struct sps sps;
    int read = read_sps(set->data, set->size, &sps);
    if (read < 0) {
      if (set == current) return read;
      continue;
    }
    if (sps.width > (unsigned)max_side || sps.height > (unsigned)max_side) return -2;
    if (sps.delay > decoder->delay) decoder->delay = sps.delay;
    if (set == current) {
      decoder->bt709 = sps.bt709;
      decoder->full_range = sps.full_range;
    }
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

/* A session for the parameter sets kept, every one of them. The session
 * open stays when only sets with new ids came and it takes them, as an
 * encoder's new PPS in the middle of a group of pictures, so that it keeps
 * the pictures the next ones refer to. */
static int open_session(struct vtdec *decoder, int max_side) {
  const uint8_t *sets[SETS];
  size_t sizes[SETS];
  size_t count = 0;
  /* By kind: H265's VPSs, then the SPSs, the current one first, as the
   * format's size is its, then the PPSs. */
  const struct parameter_set *current = current_sps(decoder);
  for (int kind = 0; kind < kinds(decoder); kind++) {
    if (current && current->kind == kind) {
      sets[count] = current->data;
      sizes[count++] = current->size;
    }
    for (int index = 0; index < decoder->set_count; index++) {
      const struct parameter_set *set = &decoder->sets[index];
      if (set->kind != kind || !set->size || set == current) continue;
      sets[count] = set->data;
      sizes[count++] = set->size;
    }
  }
  CMVideoFormatDescriptionRef format = NULL;
  OSStatus status =
      decoder->codec == VTDEC_HEVC
          ? CMVideoFormatDescriptionCreateFromHEVCParameterSets(NULL, count, sets, sizes, 4, NULL,
                                                                &format)
          : CMVideoFormatDescriptionCreateFromH264ParameterSets(NULL, count, sets, sizes, 4,
                                                                &format);
  if (status != noErr) return -1;
  CMVideoDimensions size = CMVideoFormatDescriptionGetDimensions(format);
  int range = decoder->full_range;
  int read = size.width <= 0 || size.height <= 0 || size.width > max_side || size.height > max_side
                 ? -2
                 : read_sets(decoder, max_side);
  if (read < 0) {
    CFRelease(format);
    return read;
  }
  if (decoder->session && !decoder->changed && decoder->full_range == range &&
      VTDecompressionSessionCanAcceptFormatDescription(decoder->session, format)) {
    CFRelease(decoder->format);
    decoder->format = format;
    decoder->added = 0;
    return 0;
  }
  drop_session(decoder);
  decoder->format = format;
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
  decoder->added = 0;
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
  for (int index = 0; index < decoder->set_count; index++) free(decoder->sets[index].data);
  free(decoder->sample);
  free(decoder->i420);
  free(decoder);
}

int vtdec_configure(struct vtdec *decoder, const uint8_t *record, size_t size) {
  /* avcC: version 1, profile, compatibility, level, length size, then the
   * SPSs and the PPSs, each with its size. An avc3 file's may have none,
   * and its samples carry them. */
  if (decoder->codec != VTDEC_H264 || size < 7 || record[0] != 1) return -1;
  int length_size = (record[4] & 3) + 1;
  if (length_size == 3) return -1;
  size_t at = 5;
  for (int kind = 0; kind < 2; kind++) {
    if (at >= size) return -1;
    int count = kind == 0 ? record[at] & 0x1f : record[at];
    at++;
    for (int set = 0; set < count; set++) {
      if (size - at < 2) return -1;
      size_t length = (size_t)record[at] << 8 | record[at + 1];
      at += 2;
      if (length == 0 || length > size - at) return -1;
      int kept = keep_set(decoder, kind, record + at, length);
      if (kept < 0) return kept;
      at += length;
    }
  }
  decoder->length_size = length_size;
  /* The size is checked as each access unit is decoded. */
  return read_sets(decoder, INT_MAX);
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
    int kind = set_kind(decoder, nal);
    int result = kind >= 0    ? keep_set(decoder, kind, nal, nal_size)
                 : kind == -1 ? append(decoder, &sample_size, nal, nal_size)
                              : 0;
    if (result < 0) return result;
  }
  if (got < 0) return -1;
  if (decoder->changed || decoder->added || !decoder->session) {
    for (int kind = 0; kind < kinds(decoder); kind++) {
      /* Pictures before their parameter sets can't be decoded. */
      if (!first_set(decoder, kind)) return sample_size ? -1 : 0;
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
        .planes =
            {
                .width = width,
                .height = height,
                .y = y,
                .u = u,
                .v = v,
                .y_stride = width,
                .u_stride = chroma_width,
                .v_stride = chroma_width,
            },
        .pts = held.pts,
        .bt709 = decoder->bt709,
        .full_range = pixels == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    };
  }
  CVPixelBufferUnlockBaseAddress(buffer, kCVPixelBufferLock_ReadOnly);
  CVPixelBufferRelease(buffer);
  return result;
}

int yapfast_vtdec_decode(struct vtdec *decoder, const unsigned char *data, size_t size,
                         struct yapfast_picture *picture) {
  int decoded = vtdec_decode(decoder, data, size, 0, MAX_SIDE);
  if (decoded < 0) return decoded;
  struct vtdec_picture got;
  int result = vtdec_picture(decoder, 0, &got);
  if (result <= 0) return result;
  *picture = got.planes;
  return 1;
}
