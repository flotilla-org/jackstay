#ifndef SYNTHETIC_H
#define SYNTHETIC_H
#include <stdint.h>
#include <stddef.h>
enum { WIDTH = 320, HEIGHT = 180, STRIDE = WIDTH * 4 };

static inline void fill_frame_sized(uint8_t *pixels, uint64_t sequence, uint32_t width,
                                    uint32_t height, uint32_t stride) {
  for (uint32_t y = 0; y < height; y++) {
    for (uint32_t x = 0; x < width; x++) {
      size_t offset = (size_t)y * stride + (size_t)x * 4;
      pixels[offset + 0] = (uint8_t)((x + sequence * 3) % 256);
      pixels[offset + 1] = (uint8_t)((y + sequence * 5) % 256);
      pixels[offset + 2] = (uint8_t)((x + y + sequence * 7) % 256);
      pixels[offset + 3] = 255;
    }
  }
}

static inline void fill_frame(uint8_t *pixels, uint64_t sequence) {
  fill_frame_sized(pixels, sequence, WIDTH, HEIGHT, STRIDE);
}

#endif
