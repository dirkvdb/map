#pragma once
#include "maplibre_custom_layer.h"
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif
struct map_compositor;
struct map_surface {
  uint32_t width, height;
  const uint8_t *color;
  const float *depth;
  bool extruded;
};
struct map_compositor *map_compositor_new(void);
int map_compositor_prepare(struct map_compositor *,
                           const mln_custom_frame_parameters *,
                           const struct map_surface *, char *, size_t);
int map_compositor_render(struct map_compositor *,
                          const mln_custom_frame_parameters *, char *, size_t);
int map_compositor_deinitialize(struct map_compositor *, char *, size_t);
void map_compositor_context_lost(struct map_compositor *);
void map_compositor_delete(struct map_compositor *);
#ifdef __cplusplus
}
#endif
