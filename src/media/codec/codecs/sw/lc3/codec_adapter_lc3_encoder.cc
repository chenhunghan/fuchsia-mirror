// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "codec_adapter_lc3_encoder.h"

#include <lib/media/codec_impl/codec_port.h>
#include <lib/media/codec_impl/log.h>

#include <bit>
#include <cstring>
#include <optional>
#include <unordered_set>
#include <utility>

#include <safemath/safe_math.h>

#include "codec_adapter_sw.h"
#include "lc3.h"

namespace {

static_assert(std::endian::native == std::endian::little,
              "LC3_PCM_FORMAT_S24_3LE and host-endian PcmFormat2 require little-endian host");

bool IsAcceptableSamplingFrequency(const uint32_t freq) {
  // See LC3 specification v1.0 section 2.2 and 2.4 for acceptable sampling
  // frequency values for the uncompressed audio.
  static const std::unordered_set<uint32_t> supported{8000, 16000, 24000, 32000, 44100, 48000};
  return supported.find(freq) != supported.end();
}

bool IsAcceptableBitsPerUncompAudioSample(const uint32_t bits_per_sample) {
  // See LC3 specification v1.0 section 2.2 and 2.4 for acceptable bits per
  // audio sample.
  // Fuchsia LC3 codec does not support 32 bit.
  static const std::unordered_set<uint32_t> supported{16, 24};
  return supported.find(bits_per_sample) != supported.end();
}

std::optional<std::pair<lc3_pcm_format, size_t>> ResolveLc3PcmFormat(
    const fuchsia::media::PcmFormat2& input_format) {
  const uint32_t bits_per_sample = input_format.bits_per_sample();
  const uint32_t effective_bytes_per_sample = input_format.has_bytes_per_sample()
                                                  ? input_format.bytes_per_sample()
                                                  : (bits_per_sample + 7) / 8;
  if (bits_per_sample == 16 && effective_bytes_per_sample == 2) {
    return std::make_pair(LC3_PCM_FORMAT_S16, size_t{2});
  }
  if (bits_per_sample == 24 && effective_bytes_per_sample == 3) {
    return std::make_pair(LC3_PCM_FORMAT_S24_3LE, size_t{3});
  }
  if (bits_per_sample == 24 && effective_bytes_per_sample == 4) {
    return std::make_pair(LC3_PCM_FORMAT_S24, size_t{4});
  }
  return std::nullopt;
}

Lc3EncoderParams CreateEncoderParams(const fuchsia::media::FormatDetails& format_details,
                                     lc3_pcm_format fmt, size_t bytes_per_sample) {
  const auto& input_format = format_details.pcm_format();
  const auto& settings = format_details.encoder_settings().lc3();

  auto num_channels = static_cast<int>(input_format.channel_map().size());
  std::vector<Lc3CodecContainer<lc3_encoder_t>> encoders;

  int frame_us =
      (settings.frame_duration() == fuchsia::media::Lc3FrameDuration::D10_MS) ? 10000 : 7500;

  // According to LC3 Specification v1.0 section 2.1, when the sampling frequency of the
  // input signal is 44.1 kHz, the same frame length is used as for 48 kHz.
  int sampling_freq = (input_format.frames_per_second() == 44100)
                          ? 48000
                          : static_cast<int>(input_format.frames_per_second());
  encoders.reserve(num_channels);
  auto encoder_size = lc3_encoder_size(frame_us, sampling_freq);

  // Set up an encoder for each channel to account for multi-channeled interleaved audio.
  for (int i = 0; i < num_channels; ++i) {
    encoders.emplace_back(
        [&frame_us, &sampling_freq](void* mem) {
          // Sets up and returns the pointer to the encoder struct. The pointer has the same value
          // as `mem`.
          return lc3_setup_encoder(frame_us, sampling_freq, 0, mem);
        },
        encoder_size);
  }

  return Lc3EncoderParams{
      .encoders = std::move(encoders),
      .num_channels = num_channels,
      .dt_us = frame_us,
      .sr_hz = sampling_freq,
      .nbytes = static_cast<int>(settings.nbytes()),
      .bytes_per_sample = bytes_per_sample,
      .fmt = fmt,
  };
}

}  // namespace

CodecAdapterLc3Encoder::CodecAdapterLc3Encoder(std::mutex& lock,
                                               CodecAdapterEvents* codec_adapter_events)
    : CodecAdapterSWImpl(lock, codec_adapter_events) {}

std::pair<fuchsia::media::FormatDetails, size_t> CodecAdapterLc3Encoder::OutputFormatDetails() {
  size_t min_output_buffer_size = 0;
  {
    std::lock_guard<std::mutex> lock(lock_);
    ZX_DEBUG_ASSERT(codec_params_);
    min_output_buffer_size = MinOutputBufferSize();
  }

  fuchsia::media::FormatDetails format_details;
  format_details.set_mime_type(kLc3MimeType);

  return {std::move(format_details), min_output_buffer_size};
}

CodecAdapterLc3Encoder::InputLoopStatus CodecAdapterLc3Encoder::ProcessFormatDetails(
    const fuchsia::media::FormatDetails& format_details) {
  if (codec_params_.has_value()) {
    events_->onCoreCodecFailCodec("LC3 Encoder: Midstream input format change is not supported.");
    return kShouldTerminate;
  }
  if (!format_details.has_pcm_format()) {
    events_->onCoreCodecFailCodec(
        "LC3 Encoder received input that was not uncompressed pcm audio.");
    return kShouldTerminate;
  }
  if (!format_details.has_encoder_settings() || !format_details.encoder_settings().is_lc3()) {
    events_->onCoreCodecFailCodec("LC3 Encoder did not receive encoder settings.");
    return kShouldTerminate;
  }
  const auto& input_format = format_details.pcm_format();
  const auto& settings = format_details.encoder_settings().lc3();

  if (!input_format.has_channel_map() || input_format.channel_map().empty() ||
      input_format.channel_map().size() > kMaxChannelCount) {
    events_->onCoreCodecFailCodec(
        "Unsupported channel count for LC3 Encoder: %zu",
        input_format.has_channel_map() ? input_format.channel_map().size() : 0);
    return kShouldTerminate;
  }

  if (!input_format.has_pcm_mode() ||
      input_format.pcm_mode() != fuchsia::media::AudioPcmMode2::LINEAR ||
      !input_format.has_bits_per_sample() ||
      !IsAcceptableBitsPerUncompAudioSample(input_format.bits_per_sample())) {
    events_->onCoreCodecFailCodec("Unsupported bits per sample LC3 Encoder.");
    return kShouldTerminate;
  }

  auto resolved_pcm_format = ResolveLc3PcmFormat(input_format);
  if (!resolved_pcm_format.has_value()) {
    const uint32_t effective_bytes_per_sample = input_format.has_bytes_per_sample()
                                                    ? input_format.bytes_per_sample()
                                                    : (input_format.bits_per_sample() + 7) / 8;
    events_->onCoreCodecFailCodec("Unsupported bytes_per_sample %u for %u-bit LC3 Encoder.",
                                  effective_bytes_per_sample, input_format.bits_per_sample());
    return kShouldTerminate;
  }

  if (!input_format.has_frames_per_second() ||
      !IsAcceptableSamplingFrequency(input_format.frames_per_second())) {
    events_->onCoreCodecFailCodec("Unsupported sampling frequency for LC3 Encoder.");
    return kShouldTerminate;
  }

  if (!settings.has_nbytes() || settings.nbytes() < kMinExternalByteCount ||
      settings.nbytes() > kMaxExternalByteCount) {
    events_->onCoreCodecFailCodec("Byte count should be between [20 ... 400] bytes per channel.");
    return kShouldTerminate;
  }

  if (!settings.has_frame_duration() ||
      (settings.frame_duration() != fuchsia::media::Lc3FrameDuration::D10_MS &&
       settings.frame_duration() != fuchsia::media::Lc3FrameDuration::D7P5_MS)) {
    events_->onCoreCodecFailCodec("Invalid or missing frame duration for LC3 Encoder.");
    return kShouldTerminate;
  }

  auto encoder_params =
      CreateEncoderParams(format_details, resolved_pcm_format->first, resolved_pcm_format->second);
  {
    std::lock_guard<std::mutex> lock(lock_);
    codec_params_.emplace(std::move(encoder_params));
  }
  InitChunkInputStream(format_details);
  return kOk;
}

// Encode one frame of interleaved PCM input samples across all channels into LC3 output bytes.
int CodecAdapterLc3Encoder::ProcessInputChunkData(const uint8_t* input_data, size_t input_data_size,
                                                  uint8_t* output_buffer,
                                                  size_t output_buffer_size) {
  ZX_DEBUG_ASSERT(codec_params_);
  if (input_data_size != InputChunkSize() || output_buffer_size < MinOutputBufferSize()) {
    return -1;
  }

  // Copy the entire input chunk from shared client VMO memory into codec-private
  // `aligned_input_scratch` in a single bulk `memcpy`. We copy unconditionally both to mitigate a
  // producer changing shared input VMO data out from under us while `liblc3` reads samples (note
  // `load_s16` and `load_s24` dereference `*pcm` twice per sample) and to guarantee
  // `int16_t`/`int32_t` alignment regardless of client packet `start_offset` or
  // `valid_length_bytes`.
  auto& scratch = codec_params_->aligned_input_scratch;
  const size_t num_words = (input_data_size + sizeof(int32_t) - 1) / sizeof(int32_t);
  if (scratch.size() < num_words) {
    scratch.resize(num_words);
  }
  std::memcpy(scratch.data(), input_data, input_data_size);

  const lc3_pcm_format pcm_fmt = codec_params_->fmt;
  if (pcm_fmt == LC3_PCM_FORMAT_S24) {
    // `PcmFormat2` places 24-in-32 valid audio in the high-order (most-significant /
    // left-justified) bits `31..8` with low-order padding bits `7..0` ignored by receivers, whereas
    // `liblc3`'s `LC3_PCM_FORMAT_S24` expects sign-extended 24-bit values in the low-order bits
    // `23..0` (`[-2^23, 2^23 - 1]`). Arithmetic right-shift by 8 converts in place and discards any
    // low-order padding bits.
    const size_t num_samples = input_data_size / sizeof(int32_t);
    for (size_t i = 0; i < num_samples; ++i) {
      scratch[i] >>= 8;
    }
  }

  int nch = codec_params_->num_channels;
  uint8_t* buffer = output_buffer;

  int bytes_produced = 0;
  for (int ich = 0; ich < nch; ich++) {
    // Ensure we have enough space.
    ZX_DEBUG_ASSERT(output_buffer_size >=
                    static_cast<size_t>(bytes_produced + codec_params_->nbytes));

    const void* pcm = nullptr;
    switch (pcm_fmt) {
      case LC3_PCM_FORMAT_S16:
        // Advance by `ich` 16-bit samples (`ich * sizeof(int16_t)` = `ich * 2` bytes) via
        // `int16_t*` pointer arithmetic to channel `ich`'s first interleaved sample (`lc3_encode`
        // then steps by `stride = nch` 16-bit samples per frame sample).
        pcm = reinterpret_cast<const int16_t*>(scratch.data()) + ich;
        break;
      case LC3_PCM_FORMAT_S24_3LE:
        // Advance by `ich` packed 24-bit samples (`ich * 3` bytes) via `uint8_t*` byte pointer
        // arithmetic to channel `ich`'s first interleaved sample (`lc3_encode` then steps by
        // `stride = nch` 3-byte samples per frame sample).
        pcm = reinterpret_cast<const uint8_t*>(scratch.data()) + static_cast<size_t>(ich) * 3;
        break;
      case LC3_PCM_FORMAT_S24:
        // `scratch` is `std::vector<int32_t>`, so `scratch.data() + ich` uses `int32_t*` pointer
        // arithmetic to advance by `ich` 24-in-32 samples (`ich * sizeof(int32_t)` = `ich * 4`
        // bytes) to channel `ich`'s first interleaved sample (`lc3_encode` then steps by
        // `stride = nch` `int32_t` samples per frame sample).
        pcm = scratch.data() + ich;
        break;
      default:
        ZX_PANIC("Unexpected lc3_pcm_format: %d", static_cast<int>(pcm_fmt));
    }
    if (lc3_encode(codec_params_->encoders[ich].GetCodec(), pcm_fmt, pcm, nch,
                   codec_params_->nbytes, buffer) != 0) {
      return -1;
    }
    bytes_produced += codec_params_->nbytes;
    buffer += codec_params_->nbytes;
  }
  return bytes_produced;
}

fuchsia::sysmem::BufferCollectionConstraints CodecAdapterLc3Encoder::BufferCollectionConstraints(
    CodecPort port) {
  fuchsia::sysmem::BufferCollectionConstraints c;
  if (port == kInputPort) {
    c.min_buffer_count_for_camping = kMinInputBufferCountForCamping;

    c.buffer_memory_constraints.min_size_bytes = zx_system_get_page_size();
    c.buffer_memory_constraints.max_size_bytes = kInputPerPacketBufferBytesMax;
  } else {
    c.min_buffer_count_for_camping = kMinOutputBufferCountForCamping;

    ZX_ASSERT(min_output_buffer_size_.has_value());
    c.buffer_memory_constraints.min_size_bytes = *min_output_buffer_size_;
    c.buffer_memory_constraints.max_size_bytes = 0xFFFFFFFF;  // arbitrary value.
  }

  return c;
}

size_t CodecAdapterLc3Encoder::InputChunkSize() {
  ZX_DEBUG_ASSERT(codec_params_);

  // LC3 Spec v1.0 section 2.2. Encoder Interfaces.
  // The total size of an input frame is specified by the session
  // configured number of channels, the frame size in samples, and the
  // configured sample container size (`bytes_per_sample`).
  int frame_samples = lc3_frame_samples(codec_params_->dt_us, codec_params_->sr_hz);
  ZX_DEBUG_ASSERT(frame_samples > 0);
  return static_cast<size_t>(codec_params_->num_channels) * static_cast<size_t>(frame_samples) *
         codec_params_->bytes_per_sample;
}

size_t CodecAdapterLc3Encoder::MinOutputBufferSize() {
  ZX_DEBUG_ASSERT(codec_params_);

  // LC3 Spec v1.0 section 2.2. Encoder Interfaces.
  // Expected output frame size is combined byte_count for all the channels.
  return static_cast<size_t>(codec_params_->nbytes) *
         static_cast<size_t>(codec_params_->num_channels);
}

TimestampExtrapolator CodecAdapterLc3Encoder::CreateTimestampExtrapolator(
    const fuchsia::media::FormatDetails& format_details) {
  ZX_DEBUG_ASSERT(codec_params_);

  if (format_details.has_timebase()) {
    const auto& input_format = format_details.pcm_format();
    // Use `codec_params_->bytes_per_sample` (2 for S16, 3 for packed S24_3LE, or 4 for 24-in-32
    // S24) rather than `bits_per_sample / 8`.
    // Bytes per second is input PCM sampling frequency * number_of_channels * bytes_per_sample.
    // Note that when the input signal is 44.1 kHz, liblc3 uses the 48 kHz frame sample count
    // (480 samples for 10 ms nominal interval, 360 samples for 7.5 ms nominal interval) via
    // codec_params_->sr_hz in InputChunkSize(). Dividing that 480-sample chunk size by
    // input_format.frames_per_second() (44.1 kHz) yields the actual frame duration of 10.884 ms
    // (or 8.16 ms for 7.5 ms interval) per LC3 specification v1.0 section 2.1.
    const size_t bytes_per_second = static_cast<size_t>(codec_params_->num_channels) *
                                    static_cast<size_t>(input_format.frames_per_second()) *
                                    codec_params_->bytes_per_sample;
    return TimestampExtrapolator(format_details.timebase(), bytes_per_second);
  }
  return TimestampExtrapolator();
}
