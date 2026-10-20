#pragma once

#include <cstdint>
#include <string>
#include <vector>
#include <memory>

namespace ww_image
{

// Tightly-packed RGBA8 (R,G,B,A in memory order).
struct RgbaBuf {
    std::vector<uint8_t> data;
    uint32_t             width { 0 };
    uint32_t             height { 0 };
    uint32_t             stride { 0 }; // bytes per row; == width * 4 (no padding)
};

struct DecodeError {
    std::string message;
};

class ImageSession {
public:
    static std::unique_ptr<ImageSession> open(const std::string& path, DecodeError* err);
    ~ImageSession();
    uint32_t width() const;
    uint32_t height() const;
    RgbaBuf  decode(uint32_t width, uint32_t height, DecodeError* err);

private:
    struct Impl;
    explicit ImageSession(std::unique_ptr<Impl>);
    std::unique_ptr<Impl> impl_;
};

// Decode `path` (any container/codec FFmpeg understands) at the file's
// native size, then apply the short-edge cap implied by `resolution`
// (see `<waywallen-bridge/resolution.h>`) and scale the first frame
// to that extent in RGBA8 with SWS_BICUBIC. ORIGIN (0) and unknown
// values keep the native size unchanged; numeric presets only
// downscale (never upscale a smaller source). The returned
// `width`/`height` reflect the final scaled size. Populates
// `err->message` and returns an empty buffer on failure.
RgbaBuf decode_to_rgba(const std::string& path, int32_t resolution, DecodeError* err);

} // namespace ww_image
