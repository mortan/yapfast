/* AAC decoded by AudioToolbox on macOS, where libwebrtc's FFmpeg has no AAC
 * decoder: the sound of video files, for video.c. */
#ifndef YAPFAST_AACDEC_H
#define YAPFAST_AACDEC_H

#include <stddef.h>
#include <stdint.h>

struct aacdec;

/* Opens a decoder for the stream whose AudioSpecificConfig is `config`, or
 * returns NULL. HE-AAC decodes with its SBR and parametric stereo, at the
 * rate and channels they give. What it gives goes in `rate`, `channels`
 * and `frames`, the most frames a packet decodes to. More than two
 * channels are mixed down to two. */
struct aacdec *aacdec_new(const uint8_t *config, size_t size, int *rate, int *channels,
                          int *frames);
void aacdec_free(struct aacdec *decoder);
/* Decodes one packet into `samples`, interleaved floats with room for
 * aacdec_new's `frames`: how many frames, or -1 when it couldn't be
 * decoded. */
int aacdec_decode(struct aacdec *decoder, const uint8_t *data, size_t size, float *samples);
/* Forgets the packets before, as when seeking. */
void aacdec_flush(struct aacdec *decoder);

#endif
