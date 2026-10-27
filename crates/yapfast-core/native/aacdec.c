/* AAC decoded by AudioToolbox's AudioConverter, for macOS: the FFmpeg in
 * LiveKit's macOS libwebrtc archive has no AAC decoder. The converter takes
 * the stream's AudioSpecificConfig inside an ES descriptor, as an MP4's esds
 * box holds it, and picks the richest layer it describes, so HE-AAC comes at
 * the rate and channels SBR and parametric stereo give. Each packet decodes
 * at once into the caller's buffer, as interleaved floats. */
#include "aacdec.h"

#include <AudioToolbox/AudioToolbox.h>
#include <stdlib.h>
#include <string.h>

/* What the converter's input callback returns once it gave its packet: the
 * converter then returns what it decoded so far. */
enum { NO_MORE = 1 };

/* The most frames one packet decodes to, if the format doesn't say: HE-AAC's
 * 2048, with room to spare. */
enum { MOST_FRAMES = 4096 };

struct aacdec {
  AudioConverterRef converter;
  int channels;
  int frames;
  /* The packet the input callback gives, and its description. */
  const uint8_t *data;
  UInt32 size;
  AudioStreamPacketDescription description;
};

/* Writes a descriptor's tag and its size in four bytes of 7 bits. */
static uint8_t *descriptor(uint8_t *at, uint8_t tag, size_t size) {
  *at++ = tag;
  *at++ = (uint8_t)(0x80 | ((size >> 21) & 0x7f));
  *at++ = (uint8_t)(0x80 | ((size >> 14) & 0x7f));
  *at++ = (uint8_t)(0x80 | ((size >> 7) & 0x7f));
  *at++ = (uint8_t)(size & 0x7f);
  return at;
}

/* The ES descriptor around `config`, as an esds box holds it, into `cookie`,
 * which has room for config's size and 36 bytes: its size. */
static size_t es_descriptor(const uint8_t *config, size_t size, uint8_t *cookie) {
  uint8_t *at = descriptor(cookie, 0x03, 3 + 5 + 13 + 5 + size);
  /* ES_ID, then no flags. */
  *at++ = 0, *at++ = 0, *at++ = 0;
  at = descriptor(at, 0x04, 13 + 5 + size);
  /* MPEG-4 audio, an audio stream; then buffer size and bit rates, unknown. */
  *at++ = 0x40, *at++ = 0x15;
  memset(at, 0, 11);
  at += 11;
  at = descriptor(at, 0x05, size);
  memcpy(at, config, size);
  return (size_t)(at + size - cookie);
}

static OSStatus give_packet(AudioConverterRef converter, UInt32 *packets, AudioBufferList *data,
                            AudioStreamPacketDescription **descriptions, void *opaque) {
  (void)converter;
  struct aacdec *decoder = opaque;
  if (!decoder->data) {
    *packets = 0;
    return NO_MORE;
  }
  data->mNumberBuffers = 1;
  data->mBuffers[0].mNumberChannels = 0;
  data->mBuffers[0].mDataByteSize = decoder->size;
  data->mBuffers[0].mData = (void *)decoder->data;
  decoder->description =
      (AudioStreamPacketDescription){.mStartOffset = 0, .mDataByteSize = decoder->size};
  if (descriptions) *descriptions = &decoder->description;
  *packets = 1;
  decoder->data = NULL;
  return noErr;
}

struct aacdec *aacdec_new(const uint8_t *config, size_t size, int *rate, int *channels,
                          int *frames) {
  if (!config || size == 0 || size > 1 << 16) return NULL;
  uint8_t *cookie = malloc(size + 36);
  struct aacdec *decoder = calloc(1, sizeof *decoder);
  if (!cookie || !decoder) {
    free(cookie);
    free(decoder);
    return NULL;
  }
  UInt32 cookie_size = (UInt32)es_descriptor(config, size, cookie);
  /* The layers it describes, richest first: HE-AAC v2, HE-AAC, then AAC.
   * It wants room for more than it gives. */
  AudioFormatInfo info = {
      .mASBD = {.mFormatID = kAudioFormatMPEG4AAC},
      .mMagicCookie = cookie,
      .mMagicCookieSize = cookie_size,
  };
  AudioFormatListItem layers[8];
  UInt32 layers_size = sizeof layers;
  AudioStreamBasicDescription in;
  if (AudioFormatGetProperty(kAudioFormatProperty_FormatList, sizeof info, &info, &layers_size,
                             layers) == noErr &&
      layers_size >= sizeof layers[0]) {
    in = layers[0].mASBD;
  } else {
    in = info.mASBD;
    UInt32 in_size = sizeof in;
    if (AudioFormatGetProperty(kAudioFormatProperty_FormatInfo, cookie_size, cookie, &in_size,
                               &in) != noErr) {
      goto failed;
    }
  }
  if (in.mSampleRate <= 0 || in.mChannelsPerFrame == 0) goto failed;
  decoder->channels = in.mChannelsPerFrame > 2 ? 2 : (int)in.mChannelsPerFrame;
  decoder->frames = in.mFramesPerPacket > 0 && in.mFramesPerPacket <= MOST_FRAMES
                        ? (int)in.mFramesPerPacket
                        : MOST_FRAMES;
  AudioStreamBasicDescription out = {
      .mSampleRate = in.mSampleRate,
      .mFormatID = kAudioFormatLinearPCM,
      .mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
      .mBytesPerPacket = (UInt32)decoder->channels * sizeof(float),
      .mFramesPerPacket = 1,
      .mBytesPerFrame = (UInt32)decoder->channels * sizeof(float),
      .mChannelsPerFrame = (UInt32)decoder->channels,
      .mBitsPerChannel = 32,
  };
  if (AudioConverterNew(&in, &out, &decoder->converter) != noErr) goto failed;
  if (AudioConverterSetProperty(decoder->converter, kAudioConverterDecompressionMagicCookie,
                                cookie_size, cookie) != noErr) {
    goto failed;
  }
  /* Surround is mixed down by its layout, not cut to its first two. */
  if (in.mChannelsPerFrame > 2) {
    AudioChannelLayout stereo = {.mChannelLayoutTag = kAudioChannelLayoutTag_Stereo};
    AudioConverterSetProperty(decoder->converter, kAudioConverterOutputChannelLayout,
                              sizeof stereo, &stereo);
  }
  free(cookie);
  *rate = (int)in.mSampleRate;
  *channels = decoder->channels;
  *frames = decoder->frames;
  return decoder;
failed:
  free(cookie);
  aacdec_free(decoder);
  return NULL;
}

void aacdec_free(struct aacdec *decoder) {
  if (!decoder) return;
  if (decoder->converter) AudioConverterDispose(decoder->converter);
  free(decoder);
}

int aacdec_decode(struct aacdec *decoder, const uint8_t *data, size_t size, float *samples) {
  if (!data || size == 0 || size > UINT32_MAX) return -1;
  decoder->data = data;
  decoder->size = (UInt32)size;
  UInt32 frames = (UInt32)decoder->frames;
  AudioBufferList list = {
      .mNumberBuffers = 1,
      .mBuffers = {{
          .mNumberChannels = (UInt32)decoder->channels,
          .mDataByteSize = frames * (UInt32)decoder->channels * sizeof(float),
          .mData = samples,
      }},
  };
  OSStatus status = AudioConverterFillComplexBuffer(decoder->converter, give_packet, decoder,
                                                    &frames, &list, NULL);
  decoder->data = NULL;
  if (status != noErr && status != NO_MORE) {
    /* A damaged packet can leave the decoder in a state the next one
     * wouldn't decode from. */
    AudioConverterReset(decoder->converter);
    return -1;
  }
  return (int)frames;
}

void aacdec_flush(struct aacdec *decoder) {
  AudioConverterReset(decoder->converter);
}
