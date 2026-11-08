// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "avcodec_context.h"

#include <lib/media/codec_impl/codec_buffer.h>

#include <cstring>
#include <limits>
#include <map>
#include <string>

namespace {

// TODO(turnage): Add VP9, and more.
static const std::map<std::string, AVCodecID> codec_ids = {{"video/h264", AV_CODEC_ID_H264}};

}  // namespace

std::optional<std::unique_ptr<AvCodecContext>> AvCodecContext::CreateDecoder(
    const fuchsia::media::FormatDetails& format_details, GetBufferCallback get_buffer_callback) {
  if (!format_details.has_mime_type()) {
    return std::nullopt;
  }
  auto codec_id = codec_ids.find(format_details.mime_type());
  if (codec_id == codec_ids.end()) {
    return std::nullopt;
  }

  const AVCodec* codec = avcodec_find_decoder(codec_id->second);
  ZX_DEBUG_ASSERT(codec);
  ZX_DEBUG_ASSERT(av_codec_is_decoder(codec));
  auto avcodec_context = std::unique_ptr<AVCodecContext, fit::function<void(AVCodecContext*)>>(
      avcodec_alloc_context3(codec),
      [](AVCodecContext* avcodec_context) { avcodec_free_context(&avcodec_context); });
  ZX_ASSERT(avcodec_context);

  // This flag must be set in case our packets come on NAL boundaries
  // and not just frame boundaries.
  avcodec_context->flags2 |= AV_CODEC_FLAG2_CHUNKS;

  // This flag is required to override get_buffer2.
  ZX_ASSERT(avcodec_context->codec->capabilities & AV_CODEC_CAP_DR1);

  avcodec_context->get_format = AvCodecContext::GetFormatCallback;
  avcodec_context->get_buffer2 = AvCodecContext::GetBufferCallbackRouter;

  std::unique_ptr<AvCodecContext> decoder(
      new AvCodecContext(std::move(avcodec_context), std::move(get_buffer_callback)));

  if (format_details.has_oob_bytes() && !format_details.oob_bytes().empty()) {
    // Freed in AVCodecContext deleter in avcodec_free.
    const std::vector<uint8_t>& oob = format_details.oob_bytes();
    ZX_ASSERT(oob.size() <=
              static_cast<size_t>(std::numeric_limits<int>::max()) - AV_INPUT_BUFFER_PADDING_SIZE);
    auto* extradata =
        reinterpret_cast<uint8_t*>(av_malloc(oob.size() + AV_INPUT_BUFFER_PADDING_SIZE));
    ZX_ASSERT(extradata);
    std::memcpy(extradata, oob.data(), oob.size());
    std::memset(extradata + oob.size(), 0, AV_INPUT_BUFFER_PADDING_SIZE);
    decoder->avcodec_context_->extradata = extradata;
    decoder->avcodec_context_->extradata_size = static_cast<int>(oob.size());
  }

  int open_error = avcodec_open2(decoder->avcodec_context_.get(), codec, nullptr);
  ZX_ASSERT(!open_error);
  ZX_DEBUG_ASSERT(avcodec_is_open(decoder->avcodec_context_.get()));

  return decoder;
}

int AvCodecContext::SendPacket(const CodecPacket* codec_packet) {
  ZX_DEBUG_ASSERT(codec_packet);
  ZX_DEBUG_ASSERT(avcodec_context_);
  ZX_DEBUG_ASSERT(avcodec_is_open(avcodec_context_.get()));
  ZX_DEBUG_ASSERT(av_codec_is_decoder(avcodec_context_->codec));
  ZX_DEBUG_ASSERT(codec_packet->has_start_offset());
  ZX_DEBUG_ASSERT(codec_packet->has_valid_length_bytes());
  ZX_DEBUG_ASSERT(codec_packet->buffer());

  AVPacket* packet = av_packet_alloc();
  ZX_ASSERT(packet);

  // Because packet->buf is left as nullptr (av_packet_alloc() zero-initializes the struct),
  // avcodec_send_packet() -> av_packet_ref() allocates a new FFmpeg heap buffer of size
  // packet->size + AV_INPUT_BUFFER_PADDING_SIZE, zeroes the trailing AV_INPUT_BUFFER_PADDING_SIZE
  // bytes, and copies packet->size bytes from packet->data via a single memcpy() before any
  // bitstream filtering or decoding occurs. This ensures both the required FFmpeg input padding
  // and protection against TOCTOU if the client concurrently modifies the shared input VMO.
  ZX_DEBUG_ASSERT(!packet->buf);
  packet->data = codec_packet->buffer()->base() + codec_packet->start_offset();
  packet->size = codec_packet->valid_length_bytes();

  if (codec_packet->has_timestamp_ish()) {
    packet->pts = codec_packet->timestamp_ish();
  }

  auto result = avcodec_send_packet(avcodec_context_.get(), packet);

  av_packet_free(&packet);

  return result;
}

std::pair<int, AvCodecContext::AVFramePtr> AvCodecContext::ReceiveFrame() {
  ZX_DEBUG_ASSERT(avcodec_context_);
  ZX_DEBUG_ASSERT(avcodec_is_open(avcodec_context_.get()));
  ZX_DEBUG_ASSERT(av_codec_is_decoder(avcodec_context_->codec));

  AVFramePtr frame(av_frame_alloc(), [](AVFrame* frame) { av_frame_free(&frame); });
  // If we can't allocate a frame, abort this isolate process.
  ZX_ASSERT(frame);

  int result_code = avcodec_receive_frame(avcodec_context_.get(), frame.get());
  if (result_code < 0) {
    return {result_code, nullptr};
  }

  return {result_code, std::move(frame)};
}

int AvCodecContext::EndStream() {
  ZX_DEBUG_ASSERT(avcodec_context_);
  ZX_DEBUG_ASSERT(avcodec_is_open(avcodec_context_.get()));
  ZX_DEBUG_ASSERT(av_codec_is_decoder(avcodec_context_->codec));
  return avcodec_send_packet(avcodec_context_.get(), nullptr);
}

AvCodecContext::FrameBufferRequest AvCodecContext::frame_buffer_request(AVFrame* frame) const {
  ZX_DEBUG_ASSERT(avcodec_context_);
  ZX_DEBUG_ASSERT(avcodec_is_open(avcodec_context_.get()));
  ZX_DEBUG_ASSERT(av_codec_is_decoder(avcodec_context_->codec));
  // TODO(turnage): Accept 10 bit YUV formats.
  ZX_ASSERT(frame->format == AV_PIX_FMT_YUV420P);
  // We only implement right and bottom crops, not left or top crops.
  ZX_ASSERT(frame->crop_left == 0);
  ZX_ASSERT(frame->crop_top == 0);

  int linesizes[4];
  av_image_fill_linesizes(linesizes, static_cast<AVPixelFormat>(frame->format), frame->width);

  fuchsia::images2::ImageFormat image_format;
  image_format.set_pixel_format(fuchsia::images2::PixelFormat::YV12);
  image_format.set_size(fuchsia::math::SizeU{
      .width = static_cast<uint32_t>(frame->width),
      .height = static_cast<uint32_t>(frame->height),
  });
  image_format.set_bytes_per_row(static_cast<uint32_t>(linesizes[0]));
  image_format.set_display_rect(fuchsia::math::RectU{
      .x = 0,
      .y = 0,
      .width = static_cast<uint32_t>(frame->width - frame->crop_right),
      .height = static_cast<uint32_t>(frame->height - frame->crop_bottom),
  });
  image_format.set_color_space(fuchsia::images2::ColorSpace::REC709);

  if (frame->sample_aspect_ratio.num) {
    image_format.set_pixel_aspect_ratio(fuchsia::math::SizeU{
        .width = static_cast<uint32_t>(frame->sample_aspect_ratio.num),
        .height = static_cast<uint32_t>(frame->sample_aspect_ratio.den),
    });
  }

  int buffer_bytes_needed_int = av_image_get_buffer_size(static_cast<AVPixelFormat>(frame->format),
                                                         frame->width, frame->height,
                                                         /*align=*/1);
  ZX_ASSERT(buffer_bytes_needed_int >= 0);
  size_t buffer_bytes_needed = static_cast<size_t>(buffer_bytes_needed_int);

  return {.format = std::move(image_format), .buffer_bytes_needed = buffer_bytes_needed};
}

// static
AVPixelFormat AvCodecContext::GetFormatCallback(AVCodecContext* /*avcodec_context*/,
                                                const AVPixelFormat* pix_fmts) {
  for (const AVPixelFormat* p = pix_fmts; *p != AV_PIX_FMT_NONE; ++p) {
    if (*p == AV_PIX_FMT_YUV420P) {
      return AV_PIX_FMT_YUV420P;
    }
  }
  return AV_PIX_FMT_NONE;
}

// static
int AvCodecContext::GetBufferCallbackRouter(AVCodecContext* avcodec_context, AVFrame* frame,
                                            int flags) {
  auto instance = reinterpret_cast<AvCodecContext*>(avcodec_context->opaque);
  ZX_DEBUG_ASSERT(instance);
  return instance->GetBufferHandler(avcodec_context, frame, flags);
}

int AvCodecContext::GetBufferHandler(AVCodecContext* avcodec_context, AVFrame* frame, int flags) {
  ZX_DEBUG_ASSERT(avcodec_context_);
  ZX_DEBUG_ASSERT(get_buffer_callback_);
  ZX_DEBUG_ASSERT(frame->width);

  return get_buffer_callback_(frame_buffer_request(frame), avcodec_context, frame, flags);
}

AvCodecContext::AvCodecContext(
    std::unique_ptr<AVCodecContext, fit::function<void(AVCodecContext*)>> avcodec_context,
    GetBufferCallback get_buffer_callback)
    : avcodec_context_(std::move(avcodec_context)),
      get_buffer_callback_(std::move(get_buffer_callback)) {
  ZX_DEBUG_ASSERT(avcodec_context_);
  ZX_DEBUG_ASSERT(get_buffer_callback_);

  avcodec_context_->opaque = this;
}
