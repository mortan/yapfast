/* H264 and H265 decoded by VideoToolbox on macOS, where libwebrtc's FFmpeg
 * has neither decoder: for watched Go Live streams (decode.rs) and files
 * (video.c). */
#ifndef YAPFAST_VTDEC_H
#define YAPFAST_VTDEC_H

#include <stddef.h>
#include <stdint.h>

#include "picture.h"

enum { VTDEC_H264, VTDEC_HEVC };

/* A decoded picture: its I420 planes, valid until the next call on its
 * decoder, when it shows, and its colours. */
struct vtdec_picture {
  struct yapfast_picture planes;
  int64_t pts;
  /* 1 for BT.709 colours, 0 for BT.601, -1 when the stream doesn't say. */
  int bt709;
  int full_range;
};

struct vtdec;

/* With `reorder`, pictures come in presentation order, held back as long as
 * the stream says it reorders; without, each as soon as it's decoded. */
struct vtdec *vtdec_new(int codec, int reorder);
void vtdec_free(struct vtdec *decoder);
/* Takes a file's avcC record: its parameter sets and the size of its
 * samples' length prefixes. Without one, samples are Annex B. 0, -1 when
 * the record is damaged, or -2 for a stream VideoToolbox can't decode. */
int vtdec_configure(struct vtdec *decoder, const uint8_t *record, size_t size);
/* Decodes one access unit. 0, -1 when it couldn't be decoded, -2 for a
 * stream VideoToolbox can't decode or bigger than `max_side`, or -3 out of
 * memory. */
int vtdec_decode(struct vtdec *decoder, const uint8_t *data, size_t size, int64_t pts,
                 int max_side);
/* The next picture: 1 with one, 0 while none is due; `drain` at the end of
 * the stream gives those held back too. -3 out of memory. */
int vtdec_picture(struct vtdec *decoder, int drain, struct vtdec_picture *picture);
/* Lets go of the pictures held back, as when seeking. */
void vtdec_flush(struct vtdec *decoder);
/* A watched stream's frame, with decode.c's interface: a decoder made
 * without `reorder` takes a whole access unit and gives its picture. 1 with
 * one, 0 when it has none yet, or a negative number when the frame couldn't
 * be decoded. */
int yapfast_vtdec_decode(struct vtdec *decoder, const unsigned char *data, size_t size,
                         struct yapfast_picture *picture);

#endif
