#include "av_image.hpp"

#include "waywallen-bridge/resolution.h"

extern "C" {
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/imgutils.h>
#include <libavutil/pixdesc.h>
#include <libswscale/swscale.h>
}

#include <memory>

namespace ww_image
{

namespace
{

struct FmtCtxDeleter {
    void operator()(AVFormatContext* p) const noexcept {
        if (p) avformat_close_input(&p);
    }
};
struct CodecCtxDeleter {
    void operator()(AVCodecContext* p) const noexcept {
        if (p) avcodec_free_context(&p);
    }
};
struct FrameDeleter {
    void operator()(AVFrame* p) const noexcept {
        if (p) av_frame_free(&p);
    }
};
struct PacketDeleter {
    void operator()(AVPacket* p) const noexcept {
        if (p) av_packet_free(&p);
    }
};
struct SwsDeleter {
    void operator()(SwsContext* p) const noexcept {
        if (p) sws_freeContext(p);
    }
};

using FmtCtxPtr   = std::unique_ptr<AVFormatContext, FmtCtxDeleter>;
using CodecCtxPtr = std::unique_ptr<AVCodecContext, CodecCtxDeleter>;
using FramePtr    = std::unique_ptr<AVFrame, FrameDeleter>;
using PacketPtr   = std::unique_ptr<AVPacket, PacketDeleter>;
using SwsPtr      = std::unique_ptr<SwsContext, SwsDeleter>;

bool fail(DecodeError* err, std::string m) {
    if (err) err->message = std::move(m);
    return false;
}

std::string av_err_str(int rc) {
    char buf[AV_ERROR_MAX_STRING_SIZE] = {};
    av_strerror(rc, buf, sizeof(buf));
    return buf;
}

} // namespace

struct ImageSession::Impl {
    std::string path;
    FmtCtxPtr   fmt;
    int         video_index;
    uint32_t    width;
    uint32_t    height;
};

ImageSession::ImageSession(std::unique_ptr<Impl> impl): impl_(std::move(impl)) {}
ImageSession::~ImageSession() = default;
uint32_t ImageSession::width() const { return impl_->width; }
uint32_t ImageSession::height() const { return impl_->height; }

std::unique_ptr<ImageSession> ImageSession::open(const std::string& path, DecodeError* err) {
    AVFormatContext* raw_fmt = nullptr;
    if (int rc = avformat_open_input(&raw_fmt, path.c_str(), nullptr, nullptr); rc < 0) {
        fail(err, "avformat_open_input: " + av_err_str(rc));
        return nullptr;
    }
    FmtCtxPtr fmt(raw_fmt);

    if (int rc = avformat_find_stream_info(fmt.get(), nullptr); rc < 0) {
        fail(err, "avformat_find_stream_info: " + av_err_str(rc));
        return nullptr;
    }

    int video_idx = -1;
    for (unsigned i = 0; i < fmt->nb_streams; ++i) {
        if (fmt->streams[i]->codecpar->codec_type == AVMEDIA_TYPE_VIDEO) {
            video_idx = static_cast<int>(i);
            break;
        }
    }
    if (video_idx < 0) {
        fail(err, "no video/image stream in file");
        return nullptr;
    }

    const auto* par = fmt->streams[video_idx]->codecpar;
    if (par->width <= 0 || par->height <= 0) {
        fail(err, "invalid image dimensions");
        return nullptr;
    }
    auto impl         = std::make_unique<Impl>();
    impl->path        = path;
    impl->video_index = video_idx;
    impl->width       = static_cast<uint32_t>(par->width);
    impl->height      = static_cast<uint32_t>(par->height);
    impl->fmt         = std::move(fmt);
    return std::unique_ptr<ImageSession>(new ImageSession(std::move(impl)));
}

RgbaBuf ImageSession::decode(uint32_t target_w, uint32_t target_h, DecodeError* err) {
    RgbaBuf out;
    if (! target_w || ! target_h || target_w > width() || target_h > height()) {
        fail(err, "invalid image target extent");
        return out;
    }
    if (! impl_->fmt) {
        auto reopened = open(impl_->path, err);
        if (! reopened) return out;
        if (reopened->width() != width() || reopened->height() != height()) {
            fail(err, "image dimensions changed");
            return out;
        }
        impl_ = std::move(reopened->impl_);
    }
    auto               fmt       = std::move(impl_->fmt);
    int                video_idx = impl_->video_index;
    AVStream*          st        = fmt->streams[video_idx];
    AVCodecParameters* par       = st->codecpar;
    const AVCodec*     dec       = avcodec_find_decoder(par->codec_id);
    if (! dec) {
        fail(err, std::string("no decoder for codec ") + avcodec_get_name(par->codec_id));
        return out;
    }

    CodecCtxPtr cctx(avcodec_alloc_context3(dec));
    if (! cctx) {
        fail(err, "avcodec_alloc_context3 failed");
        return out;
    }
    if (int rc = avcodec_parameters_to_context(cctx.get(), par); rc < 0) {
        fail(err, "avcodec_parameters_to_context: " + av_err_str(rc));
        return out;
    }
    if (int rc = avcodec_open2(cctx.get(), dec, nullptr); rc < 0) {
        fail(err, "avcodec_open2: " + av_err_str(rc));
        return out;
    }

    PacketPtr pkt(av_packet_alloc());
    FramePtr  src_frame(av_frame_alloc());
    if (! pkt || ! src_frame) {
        fail(err, "av_packet_alloc / av_frame_alloc failed");
        return out;
    }

    // Pull packets until we get one decoded frame (first frame = the image
    // for stills; for multi-frame inputs this is frame 0, which is what M1
    // promises — animation pacing lands in M5).
    bool got_frame = false;
    while (! got_frame) {
        int rc = av_read_frame(fmt.get(), pkt.get());
        if (rc == AVERROR_EOF) {
            // Flush the decoder.
            avcodec_send_packet(cctx.get(), nullptr);
        } else if (rc < 0) {
            fail(err, "av_read_frame: " + av_err_str(rc));
            return out;
        } else if (pkt->stream_index != video_idx) {
            av_packet_unref(pkt.get());
            continue;
        } else {
            rc = avcodec_send_packet(cctx.get(), pkt.get());
            av_packet_unref(pkt.get());
            if (rc < 0 && rc != AVERROR(EAGAIN)) {
                fail(err, "avcodec_send_packet: " + av_err_str(rc));
                return out;
            }
        }

        while (true) {
            rc = avcodec_receive_frame(cctx.get(), src_frame.get());
            if (rc == AVERROR(EAGAIN)) break;
            if (rc == AVERROR_EOF) {
                fail(err, "decoder flushed without producing a frame");
                return out;
            }
            if (rc < 0) {
                fail(err, "avcodec_receive_frame: " + av_err_str(rc));
                return out;
            }
            got_frame = true;
            break;
        }
    }

    const auto src_fmt = static_cast<AVPixelFormat>(src_frame->format);
    const int  src_w   = src_frame->width;
    const int  src_h   = src_frame->height;
    if (src_w <= 0 || src_h <= 0 || src_fmt == AV_PIX_FMT_NONE) {
        fail(err, "decoded frame has invalid dimensions/format");
        return out;
    }

    SwsPtr sws(sws_getContext(src_w,
                              src_h,
                              src_fmt,
                              static_cast<int>(target_w),
                              static_cast<int>(target_h),
                              AV_PIX_FMT_RGBA,
                              SWS_BICUBIC,
                              nullptr,
                              nullptr,
                              nullptr));
    if (! sws) {
        fail(err, std::string("sws_getContext failed (src=") + av_get_pix_fmt_name(src_fmt) + ")");
        return out;
    }

    const uint32_t stride = target_w * 4;
    const size_t   bytes  = static_cast<size_t>(stride) * target_h;
    out.data.assign(bytes + AV_INPUT_BUFFER_PADDING_SIZE, 0);

    uint8_t* dst_planes[4]  = { out.data.data(), nullptr, nullptr, nullptr };
    int      dst_strides[4] = { static_cast<int>(stride), 0, 0, 0 };

    int scaled = sws_scale(
        sws.get(), src_frame->data, src_frame->linesize, 0, src_h, dst_planes, dst_strides);
    if (scaled <= 0) {
        fail(err, "sws_scale produced no rows");
        out.data.clear();
        return out;
    }

    out.width = target_w;
    out.data.resize(bytes);
    out.height = target_h;
    out.stride = stride;
    return out;
}

RgbaBuf decode_to_rgba(const std::string& path, int32_t resolution, DecodeError* err) {
    auto session = ImageSession::open(path, err);
    if (! session) return {};
    uint32_t width = session->width(), height = session->height();
    ww_resolution_apply_cap(resolution, WW_RESOLUTION_CAP_DEFAULT, &width, &height);
    return session->decode(width, height, err);
}

} // namespace ww_image
