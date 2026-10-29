/* A picture as I420 planes, as the decoders, the cameras and picture.c hand
 * it over and Rust's RawPicture (src/picture.rs) reads it: C and C++. */
#ifndef YAPFAST_PICTURE_H
#define YAPFAST_PICTURE_H

/* The widest or tallest picture Yapfast decodes: a 4K one fits, and a stream
 * or a file can't make Yapfast take more memory than that by claiming a
 * bigger one. */
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

#endif
