#include <stddef.h>
#include <stdint.h>

// The caller retains the regular-file descriptor and an exclusive output buffer.
// Output is side*side*4 straight RGBA bytes; side is 128, 256 or 512.
// Returns 0 (rendered), 1 (no icon), or -1 (invalid input/I/O).
// dark is 0 (light) or 1 (dark); it only affects the SVG template.
int32_t pb_thumbnail_render(int32_t descriptor, uint32_t side, uint32_t dark,
    uint8_t *pixels, size_t length);
