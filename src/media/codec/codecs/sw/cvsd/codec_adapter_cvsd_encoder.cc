// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "codec_adapter_cvsd_encoder.h"

#include <lib/media/codec_impl/codec_port.h>
#include <lib/media/codec_impl/log.h>

#include <safemath/safe_math.h>

namespace {

// The implementations are based off of the CVSD codec algorithm defined in
// Bluetooth Core Spec v5.3, Vol 2, Part B Sec 9.2.
uint8_t SingleEncode(CvsdParams* params, const int16_t& input_sample) {
  const int16_t x = input_sample;

  // Determine output value (EQ 15).
  const uint8_t bit = (x >= params->accumulator) ? 0 : 1;

  UpdateCvsdParams(params, bit);

  return bit;
}

}  // namespace

CodecAdapterCvsdEncoder::CodecAdapterCvsdEncoder(std::mutex& lock,
                                                 CodecAdapterEvents* codec_adapter_events)
    : CodecAdapterSWImpl(lock, codec_adapter_events) {}

std::pair<fuchsia_media::FormatDetails, size_t> CodecAdapterCvsdEncoder::OutputFormatDetails() {
  std::lock_guard<std::mutex> lock(lock_);
  ZX_DEBUG_ASSERT(codec_params_);

  fuchsia_media::FormatDetails format_details;
  format_details.mime_type() = kCvsdMimeType;

  // The bytes needed to store each output packet. Since we're doing 16-1
  // compression for mono audio, where 2 byte input = 1 bit output. bytes needed
  // to store each output packet is 1.
  return {std::move(format_details), kOutputFrameSize};
}

CodecAdapterCvsdEncoder::InputLoopStatus CodecAdapterCvsdEncoder::ProcessFormatDetails(
    const fuchsia_media::FormatDetails& format_details) {
  if (!format_details.pcm_format().has_value()) {
    events_->onCoreCodecFailCodec(
        "CVSD Encoder received input that was not uncompressed pcm audio.");
    return kShouldTerminate;
  }
  if (!format_details.encoder_settings().has_value() ||
      format_details.encoder_settings()->Which() != fuchsia_media::EncoderSettings::Tag::kCvsd) {
    events_->onCoreCodecFailCodec("CVSD Encoder did not receive encoder settings.");
    return kShouldTerminate;
  }
  const auto& input_format = *format_details.pcm_format();
  if (!input_format.pcm_mode().has_value() || !input_format.bits_per_sample().has_value() ||
      !input_format.frames_per_second().has_value() || !input_format.channel_map().has_value()) {
    events_->onCoreCodecFailCodec("CVSD Encoder received incomplete PcmFormat2.");
    return kShouldTerminate;
  }

  if (*input_format.pcm_mode() != fuchsia_media::AudioPcmMode2::kLinear ||
      *input_format.bits_per_sample() != 16 ||
      (input_format.bytes_per_sample().has_value() &&
       *input_format.bytes_per_sample() != sizeof(int16_t)) ||
      input_format.channel_map()->size() != 1) {
    events_->onCoreCodecFailCodec(
        "CVSD Encoder only encodes mono audio with signed 16 bit linear samples.");
    return kShouldTerminate;
  }

  if (*input_format.frames_per_second() != kExpectedSamplingFreq) {
    LOG(WARNING, "Expected sampling frequency %u got %u", kExpectedSamplingFreq,
        *input_format.frames_per_second());
  }

  {
    std::lock_guard<std::mutex> lock(lock_);
    InitCvsdParams(codec_params_);
  }
  InitChunkInputStream(format_details);

  return kOk;
}

// Encode input buffer of 16 byte size to produce one byte of output data.
int CodecAdapterCvsdEncoder::ProcessInputChunkData(const uint8_t* input_data,
                                                   size_t input_data_size, uint8_t* output_buffer,
                                                   size_t output_buffer_size) {
  uint8_t output_byte = 0x00;
  const int16_t* pcm = reinterpret_cast<const int16_t*>(input_data);
  for (int idx = 0; idx < static_cast<int>(input_data_size / 2); ++idx, ++pcm) {
    output_byte = static_cast<uint8_t>(output_byte << 1);
    output_byte |= SingleEncode(&(*codec_params_), *pcm);
  }
  *output_buffer = output_byte;
  return 1;
}

fuchsia_sysmem2::BufferCollectionConstraints CodecAdapterCvsdEncoder::BufferCollectionConstraints(
    CodecPort port) {
  fuchsia_sysmem2::BufferCollectionConstraints c;
  c.min_buffer_count_for_camping() = kMinBufferCountForCamping;

  // Actual minimum buffer size is 16 bytes for input and 1 byte for output since we're doing
  // 16:1 compression for encoding. However, sysmem will basically allocate at least a
  // 4KiB page per buffer even if the client is also ok with less, so we default to
  // system page size for now.
  auto& bmc = c.buffer_memory_constraints().emplace();
  if (port == kInputPort) {
    // TODO(dayeonglee): consider requiring a larger buffer, based on what
    // seems like a minimum reasonable time duration per buffer, to keep the
    // buffers per second <= ~120.
    bmc.min_size_bytes() = std::max(zx_system_get_page_size(), kInputFrameSize);
    bmc.max_size_bytes() = kInputPerPacketBufferBytesMax;
  } else {
    bmc.min_size_bytes() = static_cast<uint32_t>(MinOutputBufferSize());
    // Set to some arbitrary value.
    bmc.max_size_bytes() = 0xFFFFFFFF;
  }

  return c;
}

TimestampExtrapolator CodecAdapterCvsdEncoder::CreateTimestampExtrapolator(
    const fuchsia_media::FormatDetails& format_details) {
  if (format_details.timebase().has_value()) {
    const auto& input_format = *format_details.pcm_format();
    // Bytes per second is sampling frequency * number_of_channels * bytes_per_sample.
    const size_t bytes_per_second = input_format.channel_map()->size() *
                                    *input_format.frames_per_second() *
                                    (*input_format.bits_per_sample() / 8ull);
    return TimestampExtrapolator(*format_details.timebase(), bytes_per_second);
  }
  return TimestampExtrapolator();
}
