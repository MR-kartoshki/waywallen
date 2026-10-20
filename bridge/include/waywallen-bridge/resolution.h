/* waywallen-bridge — resolution enum shared between the daemon's
 * renderer-manifest schema and every renderer subprocess. Values are
 * wire-stable: the daemon validates and forwards them as the kv string
 * "resolution"=<N> on `Init.settings` (and on hot-reload, though the
 * value is identity=true so a reload triggers respawn). Each renderer
 * applies it as a SHORT-EDGE cap on top of the already-resolved
 * extent — no upscaling.
 */
#ifndef WAYWALLEN_BRIDGE_RESOLUTION_H
#define WAYWALLEN_BRIDGE_RESOLUTION_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef enum ww_resolution
{
    WW_RESOLUTION_AUTO   = -2, /* daemon-coordinated render target */
    WW_RESOLUTION_CUSTOM = -1, /* renderer-specific custom short-edge extent */
    WW_RESOLUTION_ORIGIN = 0,  /* no cap — use renderer's native size */
    WW_RESOLUTION_720P   = 1,
    WW_RESOLUTION_1080P  = 2,
    WW_RESOLUTION_1440P  = 3,
    WW_RESOLUTION_2160P  = 4,
} ww_resolution_t;

/* Per-renderer policy passed to `ww_resolution_apply_cap`. Defaults to
 * downscale-only so image/video (which have a real native size) never
 * waste bandwidth blowing up small sources. Renderers without a fixed
 * native (wescene — every scene is designed to scale) pass
 * `ALLOW_UPSCALE` so the user's choice is honoured exactly. */
typedef enum ww_resolution_cap_option
{
    WW_RESOLUTION_CAP_DEFAULT       = 0, /* downscale only; below cap is left as-is */
    WW_RESOLUTION_CAP_ALLOW_UPSCALE = 1, /* always scale to match the cap */
} ww_resolution_cap_option_t;

/* Short-edge cap (in pixels) implied by the enum.
 * `WW_RESOLUTION_CUSTOM`, `WW_RESOLUTION_ORIGIN`, and out-of-range
 * values return 0 (= no cap). */
static inline uint32_t ww_resolution_short_edge(int32_t r) {
    switch (r) {
    case WW_RESOLUTION_720P: return 720u;
    case WW_RESOLUTION_1080P: return 1080u;
    case WW_RESOLUTION_1440P: return 1440u;
    case WW_RESOLUTION_2160P: return 2160u;
    default: return 0u;
    }
}

/* Apply an explicit short-edge cap to an already-resolved (w, h).
 * Aspect ratio is preserved. A zero cap is a no-op.
 *
 * `option == WW_RESOLUTION_CAP_DEFAULT` only shrinks: when the shorter
 *   input edge already fits within the cap, the extent is left
 *   untouched. Use for renderers with a real native size (image/video)
 *   so a small source is never blown up.
 * `option == WW_RESOLUTION_CAP_ALLOW_UPSCALE` always rescales so the
 *   short edge matches the cap exactly — upward when the input is
 *   smaller, downward when larger. Use for renderers whose content
 *   has no fixed native (wescene). */
static inline void ww_resolution_apply_short_edge(uint32_t cap, uint32_t option, uint32_t* w,
                                                  uint32_t* h) {
    if (cap == 0 || w == 0 || h == 0 || *w == 0 || *h == 0) return;
    uint32_t short_edge = (*w < *h) ? *w : *h;
    if (short_edge == cap) return;
    if (short_edge < cap && option != (uint32_t)WW_RESOLUTION_CAP_ALLOW_UPSCALE) {
        return;
    }
    if (*w < *h) {
        *h = (uint32_t)((uint64_t)(*h) * cap / short_edge);
        *w = cap;
    } else {
        *w = (uint32_t)((uint64_t)(*w) * cap / short_edge);
        *h = cap;
    }
    if (*w == 0) *w = 1u;
    if (*h == 0) *h = 1u;
}

/* Apply the enum's short-edge cap to an already-resolved (w, h).
 * Aspect ratio is preserved. CUSTOM / ORIGIN / unknown resolutions are
 * no-ops; CUSTOM callers should pass their explicit extent through
 * `ww_resolution_apply_short_edge`. */
static inline void ww_resolution_apply_cap(int32_t resolution, uint32_t option, uint32_t* w,
                                           uint32_t* h) {
    ww_resolution_apply_short_edge(ww_resolution_short_edge(resolution), option, w, h);
}

/* Coerce a raw wire value into a usable resolution. Anything outside
 * `{AUTO, CUSTOM, ORIGIN .. 2160P}` falls back to `WW_RESOLUTION_1080P`.
 * AUTO, CUSTOM and ORIGIN stay unchanged — callers that disallow them must
 * filter them themselves. */
static inline int32_t ww_resolution_sanitize(int32_t raw) {
    if (raw == (int32_t)WW_RESOLUTION_AUTO || raw == (int32_t)WW_RESOLUTION_CUSTOM) return raw;
    if (raw < (int32_t)WW_RESOLUTION_ORIGIN || raw > (int32_t)WW_RESOLUTION_2160P)
        return (int32_t)WW_RESOLUTION_1080P;
    return raw;
}

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* WAYWALLEN_BRIDGE_RESOLUTION_H */
