// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/media/cpp/fidl.h>
#include <lib/media/codec_impl/codec_impl.h>

#include <algorithm>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <mutex>
#include <numbers>
#include <string>
#include <utility>
#include <vector>

#include <gtest/gtest.h>

#include "src/media/codec/codecs/sw/lc3/codec_adapter_lc3_decoder.h"
#include "src/media/codec/codecs/sw/lc3/codec_adapter_lc3_encoder.h"
#include "src/media/codec/codecs/test/test_codec_packets.h"
#include "src/media/codec/codecs/test/test_fake_codec_adapter_events.h"

namespace {

class TestCodecAdapterLc3Decoder : public CodecAdapterLc3Decoder {
 public:
  TestCodecAdapterLc3Decoder(std::mutex& lock, CodecAdapterEvents* events)
      : CodecAdapterLc3Decoder(lock, events) {}

  using CodecAdapterLc3Decoder::CleanUpAfterStream;
  using CodecAdapterLc3Decoder::InputChunkSize;
  using CodecAdapterLc3Decoder::InputLoopStatus;
  using CodecAdapterLc3Decoder::MinOutputBufferSize;
  using CodecAdapterLc3Decoder::OutputFormatDetails;
  using CodecAdapterLc3Decoder::ProcessFormatDetails;
  using CodecAdapterLc3Decoder::ProcessInputChunkData;
};

class TestCodecAdapterLc3Encoder : public CodecAdapterLc3Encoder {
 public:
  TestCodecAdapterLc3Encoder(std::mutex& lock, CodecAdapterEvents* events)
      : CodecAdapterLc3Encoder(lock, events) {}

  using CodecAdapterLc3Encoder::CleanUpAfterStream;
  using CodecAdapterLc3Encoder::CreateTimestampExtrapolator;
  using CodecAdapterLc3Encoder::InputChunkSize;
  using CodecAdapterLc3Encoder::InputLoopStatus;
  using CodecAdapterLc3Encoder::MinOutputBufferSize;
  using CodecAdapterLc3Encoder::ProcessFormatDetails;
  using CodecAdapterLc3Encoder::ProcessInputChunkData;
};

fuchsia::media::FormatDetails MakeLc3FormatDetails(std::vector<uint8_t> oob_bytes) {
  fuchsia::media::FormatDetails format_details;
  format_details.set_mime_type(kLc3MimeType);
  format_details.set_oob_bytes(std::move(oob_bytes));
  return format_details;
}

fuchsia::media::FormatDetails MakeValidLc3DecoderFormatDetails() {
  // Valid 16-byte LTV configuration:
  // Sampling_Frequency: 48 kHz (0x08)
  // Frame_Duration: 10 ms (0x01)
  // Audio_Channel_Allocation: LF (0x00000001)
  // Octets_Per_Codec_Frame: 40 (0x0028)
  std::vector<uint8_t> oob_bytes = {
      0x02, 0x01, 0x08,                    // Sampling_Frequency: 48 kHz
      0x02, 0x02, 0x01,                    // Frame_Duration: 10 ms
      0x05, 0x03, 0x00, 0x00, 0x00, 0x01,  // Audio_Channel_Allocation: LF
      0x03, 0x04, 0x00, 0x28               // Octets_Per_Codec_Frame: 40
  };
  return MakeLc3FormatDetails(std::move(oob_bytes));
}

fuchsia::media::FormatDetails MakeValidLc3EncoderFormatDetails() {
  fuchsia::media::PcmFormat2 pcm;
  pcm.set_pcm_mode(fuchsia::media::AudioPcmMode2::LINEAR);
  pcm.set_bits_per_sample(16);
  pcm.set_frames_per_second(48000);
  pcm.set_channel_map({fuchsia::media::AudioChannelId2::FL});

  fuchsia::media::Lc3EncoderSettings lc3_settings;
  lc3_settings.set_nbytes(40);
  lc3_settings.set_frame_duration(fuchsia::media::Lc3FrameDuration::D10_MS);

  fuchsia::media::EncoderSettings encoder_settings;
  encoder_settings.set_lc3(std::move(lc3_settings));

  fuchsia::media::FormatDetails format_details;
  format_details.set_pcm_format(std::move(pcm));
  format_details.set_encoder_settings(std::move(encoder_settings));
  return format_details;
}

TEST(CodecAdapterLc3DecoderTest, ValidOobBytesSucceeds) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  auto status = decoder.ProcessFormatDetails(MakeValidLc3DecoderFormatDetails());
  EXPECT_EQ(status, TestCodecAdapterLc3Decoder::kOk);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(decoder.InputChunkSize(), 40u);
  EXPECT_EQ(decoder.MinOutputBufferSize(), 960u);
}

TEST(CodecAdapterLc3DecoderTest, MissingMimeTypeOrOobBytesRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  fuchsia::media::FormatDetails missing_mime;
  missing_mime.set_oob_bytes(std::vector<uint8_t>(16, 0));
  EXPECT_EQ(decoder.ProcessFormatDetails(missing_mime),
            TestCodecAdapterLc3Decoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);

  fuchsia::media::FormatDetails short_oob;
  short_oob.set_mime_type(kLc3MimeType);
  short_oob.set_oob_bytes(std::vector<uint8_t>(15, 0));
  EXPECT_EQ(decoder.ProcessFormatDetails(short_oob), TestCodecAdapterLc3Decoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 2u);
}

TEST(CodecAdapterLc3DecoderTest, OutOfRangeOctetsPerCodecFrameRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  // Octets_Per_Codec_Frame = 19 (< kMinExternalByteCount = 20).
  std::vector<uint8_t> oob_bytes = {
      0x02, 0x01, 0x08,                    // Sampling_Frequency
      0x02, 0x02, 0x01,                    // Frame_Duration
      0x05, 0x03, 0x00, 0x00, 0x00, 0x01,  // Audio_Channel_Allocation
      0x03, 0x04, 0x00, 0x13               // Octets_Per_Codec_Frame = 19
  };

  auto status = decoder.ProcessFormatDetails(MakeLc3FormatDetails(std::move(oob_bytes)));
  EXPECT_EQ(status, TestCodecAdapterLc3Decoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("Octets_Per_Codec_Frame"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3DecoderTest, ValidOobBytesWithUnknownLtvParamSucceeds) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  // Valid 16-byte LTV configuration plus optional/unknown LTV parameter (e.g.
  // Codec_Frame_Blocks_Per_SDU = 0x05).
  std::vector<uint8_t> oob_bytes = {
      0x02, 0x01, 0x08,                    // Sampling_Frequency
      0x02, 0x02, 0x01,                    // Frame_Duration
      0x05, 0x03, 0x00, 0x00, 0x00, 0x01,  // Audio_Channel_Allocation
      0x03, 0x04, 0x00, 0x28,              // Octets_Per_Codec_Frame
      0x02, 0x05, 0x01                     // Optional Codec_Frame_Blocks_Per_SDU
  };

  auto status = decoder.ProcessFormatDetails(MakeLc3FormatDetails(std::move(oob_bytes)));
  EXPECT_EQ(status, TestCodecAdapterLc3Decoder::kOk);
  EXPECT_EQ(events.fail_codec_count(), 0u);
}

TEST(CodecAdapterLc3DecoderTest, TruncatedAudioChannelAllocationRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  // Audio_Channel_Allocation with LTV length 2 (1 byte value instead of 4).
  std::vector<uint8_t> oob_bytes = {
      0x02, 0x01, 0x08,             // Sampling_Frequency
      0x02, 0x02, 0x01,             // Frame_Duration
      0x02, 0x03, 0x00,             // Truncated Audio_Channel_Allocation (len=2)
      0x03, 0x04, 0x00, 0x28,       // Valid Octets_Per_Codec_Frame
      0x04, 0x99, 0x00, 0x00, 0x00  // Unknown LTV param to pad to >= 16B
  };

  auto status = decoder.ProcessFormatDetails(MakeLc3FormatDetails(std::move(oob_bytes)));
  EXPECT_EQ(status, TestCodecAdapterLc3Decoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("Audio_Channel_Allocation"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3DecoderTest, TruncatedOctetsPerCodecFrameRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  // Octets_Per_Codec_Frame with LTV length 2 (1 byte value instead of 2).
  std::vector<uint8_t> oob_bytes = {
      0x02, 0x01, 0x08,                    // Sampling_Frequency
      0x02, 0x02, 0x01,                    // Frame_Duration
      0x05, 0x03, 0x00, 0x00, 0x00, 0x01,  // Audio_Channel_Allocation
      0x02, 0x04, 0x28,                    // Truncated Octets_Per_Codec_Frame (len=2)
      0x01, 0x99                           // Unknown param to pad to 16B
  };

  auto status = decoder.ProcessFormatDetails(MakeLc3FormatDetails(std::move(oob_bytes)));
  EXPECT_EQ(status, TestCodecAdapterLc3Decoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("Octets_Per_Codec_Frame"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3DecoderTest, TruncatedSamplingFrequencyRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  // Sampling_Frequency with LTV length 1 (0 value bytes).
  std::vector<uint8_t> oob_bytes = {
      0x01, 0x01,                          // Truncated Sampling_Frequency (len=1)
      0x02, 0x02, 0x01,                    // Frame_Duration
      0x05, 0x03, 0x00, 0x00, 0x00, 0x01,  // Audio_Channel_Allocation
      0x03, 0x04, 0x00, 0x28,              // Octets_Per_Codec_Frame
      0x01, 0x99                           // Padding to >= 16B
  };

  auto status = decoder.ProcessFormatDetails(MakeLc3FormatDetails(std::move(oob_bytes)));
  EXPECT_EQ(status, TestCodecAdapterLc3Decoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("Sampling_Frequency"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3DecoderTest, TruncatedFrameDurationRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  // Frame_Duration with LTV length 1 (0 value bytes).
  std::vector<uint8_t> oob_bytes = {
      0x02, 0x01, 0x08,                    // Sampling_Frequency
      0x01, 0x02,                          // Truncated Frame_Duration (len=1)
      0x05, 0x03, 0x00, 0x00, 0x00, 0x01,  // Audio_Channel_Allocation
      0x03, 0x04, 0x00, 0x28,              // Octets_Per_Codec_Frame
      0x01, 0x99                           // Padding to >= 16B
  };

  auto status = decoder.ProcessFormatDetails(MakeLc3FormatDetails(std::move(oob_bytes)));
  EXPECT_EQ(status, TestCodecAdapterLc3Decoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("Frame_Duration"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3DecoderTest, ZeroLengthLtvEntryAtEndRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  // Valid LTVs followed by a trailing 0x00 length byte at the very end of oob_bytes.
  std::vector<uint8_t> oob_bytes = {
      0x02, 0x01, 0x08,                    // Sampling_Frequency
      0x02, 0x02, 0x01,                    // Frame_Duration
      0x05, 0x03, 0x00, 0x00, 0x00, 0x01,  // Audio_Channel_Allocation
      0x03, 0x04, 0x00, 0x28,              // Octets_Per_Codec_Frame
      0x00                                 // len == 0 at final byte
  };

  auto status = decoder.ProcessFormatDetails(MakeLc3FormatDetails(std::move(oob_bytes)));
  EXPECT_EQ(status, TestCodecAdapterLc3Decoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("invalid LTV length"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3DecoderTest, LtvLengthExceedsRemainingBufferRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  // Final LTV claims length 10 when only 3 bytes remain.
  std::vector<uint8_t> oob_bytes = {
      0x02, 0x01, 0x08,                    // Sampling_Frequency
      0x02, 0x02, 0x01,                    // Frame_Duration
      0x05, 0x03, 0x00, 0x00, 0x00, 0x01,  // Audio_Channel_Allocation
      0x0a, 0x04, 0x00, 0x28               // len=10 exceeds buffer
  };

  auto status = decoder.ProcessFormatDetails(MakeLc3FormatDetails(std::move(oob_bytes)));
  EXPECT_EQ(status, TestCodecAdapterLc3Decoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("invalid LTV length"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3DecoderTest, DuplicateLtvParameterKeyRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  // Duplicate Sampling_Frequency (key 0x01) entries.
  std::vector<uint8_t> oob_bytes = {
      0x02, 0x01, 0x08,                    // Sampling_Frequency #1
      0x02, 0x01, 0x08,                    // Sampling_Frequency #2 (duplicate)
      0x02, 0x02, 0x01,                    // Frame_Duration
      0x05, 0x03, 0x00, 0x00, 0x00, 0x01,  // Audio_Channel_Allocation
      0x03, 0x04, 0x00, 0x28               // Octets_Per_Codec_Frame
  };

  auto status = decoder.ProcessFormatDetails(MakeLc3FormatDetails(std::move(oob_bytes)));
  EXPECT_EQ(status, TestCodecAdapterLc3Decoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("duplicate LTV parameter key"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3DecoderTest, ZeroAudioChannelAllocationRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  // Audio_Channel_Allocation = 0x00000000 (0 channels).
  std::vector<uint8_t> oob_bytes = {
      0x02, 0x01, 0x08,                    // Sampling_Frequency
      0x02, 0x02, 0x01,                    // Frame_Duration
      0x05, 0x03, 0x00, 0x00, 0x00, 0x00,  // Audio_Channel_Allocation = 0
      0x03, 0x04, 0x00, 0x28               // Octets_Per_Codec_Frame
  };

  auto status = decoder.ProcessFormatDetails(MakeLc3FormatDetails(std::move(oob_bytes)));
  EXPECT_EQ(status, TestCodecAdapterLc3Decoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("Audio_Channel_Allocation"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3DecoderTest, IncompleteRequiredLtvParamsRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  // oob_bytes >= 16 bytes (padded with unknown LTV key 0x05), missing Octets_Per_Codec_Frame
  // (0x04).
  std::vector<uint8_t> oob_bytes = {
      0x02, 0x01, 0x08,                    // Sampling_Frequency
      0x02, 0x02, 0x01,                    // Frame_Duration
      0x05, 0x03, 0x00, 0x00, 0x00, 0x01,  // Audio_Channel_Allocation
      0x04, 0x05, 0x01, 0x00, 0x00         // Unknown param to pad to >= 16B
  };

  auto status = decoder.ProcessFormatDetails(MakeLc3FormatDetails(std::move(oob_bytes)));
  EXPECT_EQ(status, TestCodecAdapterLc3Decoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("incomplete"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3DecoderTest, MidstreamFormatChangeRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  ASSERT_EQ(decoder.ProcessFormatDetails(MakeValidLc3DecoderFormatDetails()),
            TestCodecAdapterLc3Decoder::kOk);
  EXPECT_EQ(events.fail_codec_count(), 0u);

  // A second ProcessFormatDetails call on an active stream should fail the codec.
  EXPECT_EQ(decoder.ProcessFormatDetails(MakeValidLc3DecoderFormatDetails()),
            TestCodecAdapterLc3Decoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("Midstream input format change"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3DecoderTest, ProcessInputChunkDataBufferSizeValidation) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  ASSERT_EQ(decoder.ProcessFormatDetails(MakeValidLc3DecoderFormatDetails()),
            TestCodecAdapterLc3Decoder::kOk);

  const size_t input_size = decoder.InputChunkSize();
  const size_t min_output_size = decoder.MinOutputBufferSize();
  std::vector<uint8_t> input(input_size, 0);
  std::vector<uint8_t> output(min_output_size, 0);

  // Mismatched input_data_size should return -1.
  EXPECT_EQ(
      decoder.ProcessInputChunkData(input.data(), input_size - 1, output.data(), output.size()),
      -1);
  // Undersized output_buffer_size should return -1.
  EXPECT_EQ(
      decoder.ProcessInputChunkData(input.data(), input_size, output.data(), min_output_size - 1),
      -1);
  // Zeroed input triggers liblc3 Packet Loss Concealment (return code 1) and succeeds.
  EXPECT_EQ(decoder.ProcessInputChunkData(input.data(), input_size, output.data(), output.size()),
            static_cast<int>(min_output_size));
}

TEST(CodecAdapterLc3EncoderTest, ValidFormatDetailsSucceeds) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Encoder encoder(lock, &events);

  auto status = encoder.ProcessFormatDetails(MakeValidLc3EncoderFormatDetails());
  EXPECT_EQ(status, TestCodecAdapterLc3Encoder::kOk);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(encoder.InputChunkSize(), 960u);
  EXPECT_EQ(encoder.MinOutputBufferSize(), 40u);
}

TEST(CodecAdapterLc3EncoderTest, MissingPcmFormatOrEncoderSettingsRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Encoder encoder(lock, &events);

  fuchsia::media::FormatDetails missing_pcm_format = MakeValidLc3EncoderFormatDetails();
  missing_pcm_format.clear_pcm_format();
  EXPECT_EQ(encoder.ProcessFormatDetails(missing_pcm_format),
            TestCodecAdapterLc3Encoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);

  fuchsia::media::FormatDetails missing_settings = MakeValidLc3EncoderFormatDetails();
  missing_settings.clear_encoder_settings();
  EXPECT_EQ(encoder.ProcessFormatDetails(missing_settings),
            TestCodecAdapterLc3Encoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 2u);
}

TEST(CodecAdapterLc3EncoderTest, OutOfRangeEncoderNbytesRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Encoder encoder(lock, &events);

  auto format_details = MakeValidLc3EncoderFormatDetails();
  format_details.mutable_encoder_settings()->lc3().set_nbytes(401);

  auto status = encoder.ProcessFormatDetails(format_details);
  EXPECT_EQ(status, TestCodecAdapterLc3Encoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("Byte count"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3EncoderTest, EmptyChannelMapRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Encoder encoder(lock, &events);

  auto format_details = MakeValidLc3EncoderFormatDetails();
  format_details.mutable_pcm_format()->mutable_channel_map()->clear();

  auto status = encoder.ProcessFormatDetails(format_details);
  EXPECT_EQ(status, TestCodecAdapterLc3Encoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("Unsupported channel count"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3EncoderTest, ExcessiveChannelMapRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Encoder encoder(lock, &events);

  auto format_details = MakeValidLc3EncoderFormatDetails();
  format_details.mutable_pcm_format()->mutable_channel_map()->assign(
      kMaxChannelCount + 1, fuchsia::media::AudioChannelId2::FL);

  auto status = encoder.ProcessFormatDetails(format_details);
  EXPECT_EQ(status, TestCodecAdapterLc3Encoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("Unsupported channel count"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3EncoderTest, MissingNbytesRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Encoder encoder(lock, &events);

  auto format_details = MakeValidLc3EncoderFormatDetails();
  format_details.mutable_encoder_settings()->lc3().clear_nbytes();

  auto status = encoder.ProcessFormatDetails(format_details);
  EXPECT_EQ(status, TestCodecAdapterLc3Encoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("Byte count"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3EncoderTest, MissingFrameDurationRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Encoder encoder(lock, &events);

  auto format_details = MakeValidLc3EncoderFormatDetails();
  format_details.mutable_encoder_settings()->lc3().clear_frame_duration();

  auto status = encoder.ProcessFormatDetails(format_details);
  EXPECT_EQ(status, TestCodecAdapterLc3Encoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("frame duration"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3EncoderTest, InvalidFrameDurationRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Encoder encoder(lock, &events);

  auto format_details = MakeValidLc3EncoderFormatDetails();
  format_details.mutable_encoder_settings()->lc3().set_frame_duration(
      static_cast<fuchsia::media::Lc3FrameDuration>(99));

  auto status = encoder.ProcessFormatDetails(format_details);
  EXPECT_EQ(status, TestCodecAdapterLc3Encoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("frame duration"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3EncoderTest, MidstreamFormatChangeRejected) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Encoder encoder(lock, &events);

  ASSERT_EQ(encoder.ProcessFormatDetails(MakeValidLc3EncoderFormatDetails()),
            TestCodecAdapterLc3Encoder::kOk);
  EXPECT_EQ(events.fail_codec_count(), 0u);

  EXPECT_EQ(encoder.ProcessFormatDetails(MakeValidLc3EncoderFormatDetails()),
            TestCodecAdapterLc3Encoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_NE(events.last_fail_message().find("Midstream input format change"), std::string::npos)
      << "Actual message: " << events.last_fail_message();
}

TEST(CodecAdapterLc3EncoderTest, ProcessInputChunkDataValidationAndUnalignedInput) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Encoder encoder(lock, &events);

  auto format_details = MakeValidLc3EncoderFormatDetails();
  format_details.mutable_pcm_format()->set_bits_per_sample(24);
  format_details.mutable_pcm_format()->set_bytes_per_sample(4);
  ASSERT_EQ(encoder.ProcessFormatDetails(format_details), TestCodecAdapterLc3Encoder::kOk);

  const size_t input_size = encoder.InputChunkSize();
  const size_t min_output_size = encoder.MinOutputBufferSize();

  // Allocate extra padding so input_storage.data() + 1 is guaranteed misaligned for int32_t.
  std::vector<uint8_t> input_storage(input_size + 4, 0);
  const uint8_t* unaligned_input = input_storage.data() + 1;
  std::vector<uint8_t> output(min_output_size, 0);

  // Mismatched input_data_size should return -1.
  EXPECT_EQ(
      encoder.ProcessInputChunkData(unaligned_input, input_size - 1, output.data(), output.size()),
      -1);
  // Undersized output_buffer_size should return -1.
  EXPECT_EQ(encoder.ProcessInputChunkData(unaligned_input, input_size, output.data(),
                                          min_output_size - 1),
            -1);
  // Valid unaligned input chunk should succeed via aligned scratch buffer copy.
  EXPECT_EQ(
      encoder.ProcessInputChunkData(unaligned_input, input_size, output.data(), output.size()),
      static_cast<int>(min_output_size));
}

TEST(CodecAdapterLc3DecoderTest, SamplingFrequency44100HzPreservedInOutputFormat) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  // Sampling_Frequency 0x07 (44.1 kHz), Frame_Duration 0x01 (10 ms), 1 ch, 40 bytes.
  std::vector<uint8_t> oob_bytes = {
      0x02, 0x01, 0x07,                    // Sampling_Frequency: 44.1 kHz
      0x02, 0x02, 0x01,                    // Frame_Duration: 10 ms
      0x05, 0x03, 0x00, 0x00, 0x00, 0x01,  // Audio_Channel_Allocation: LF
      0x03, 0x04, 0x00, 0x28               // Octets_Per_Codec_Frame: 40
  };

  ASSERT_EQ(decoder.ProcessFormatDetails(MakeLc3FormatDetails(std::move(oob_bytes))),
            TestCodecAdapterLc3Decoder::kOk);
  EXPECT_EQ(events.fail_codec_count(), 0u);

  // liblc3 uses 480 samples per 10ms frame for 44.1 kHz, so MinOutputBufferSize is 480 * 2 = 960 B.
  EXPECT_EQ(decoder.MinOutputBufferSize(), 960u);
  // Output format details must report the actual PCM sample rate (44100 Hz), not coerced 48000 Hz.
  auto [out_format, min_size] = decoder.OutputFormatDetails();
  EXPECT_EQ(out_format.pcm_format().frames_per_second(), 44100u);
}

TEST(CodecAdapterLc3EncoderTest, TimestampExtrapolatorUsesTrueSampleRateFor44100Hz) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Encoder encoder(lock, &events);

  auto format_details = MakeValidLc3EncoderFormatDetails();
  format_details.mutable_pcm_format()->set_frames_per_second(44100);
  format_details.set_timebase(1'000'000'000ull);  // 1 second in ns
  ASSERT_EQ(encoder.ProcessFormatDetails(format_details), TestCodecAdapterLc3Encoder::kOk);

  // For 44.1 kHz 10ms nominal frame, liblc3 encodes 480 samples per frame (InputChunkSize = 960 B).
  // At 44,100 samples/sec, 480 samples lasts 480 / 44100 s = 10,884,353 ns (~10.884 ms).
  const size_t chunk_size = encoder.InputChunkSize();
  EXPECT_EQ(chunk_size, 960u);

  auto extrapolator = encoder.CreateTimestampExtrapolator(format_details);
  extrapolator.Inform(0, 0);
  auto extrapolated = extrapolator.Extrapolate(chunk_size);
  ASSERT_TRUE(extrapolated.has_value());
  EXPECT_EQ(*extrapolated, 480ull * 1'000'000'000ull / 44100ull);
}

TEST(CodecAdapterLc3EncoderTest, TimestampExtrapolatorUsesFourBytesPerSampleFor24BitAudio) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Encoder encoder(lock, &events);

  auto format_details = MakeValidLc3EncoderFormatDetails();
  format_details.mutable_pcm_format()->set_bits_per_sample(24);
  format_details.mutable_pcm_format()->set_bytes_per_sample(4);
  format_details.set_timebase(1'000'000'000ull);  // 1 second in ns
  ASSERT_EQ(encoder.ProcessFormatDetails(format_details), TestCodecAdapterLc3Encoder::kOk);

  // For 48 kHz 10ms frame, liblc3 encodes 480 samples per frame. Since 24-bit PCM
  // (LC3_PCM_FORMAT_S24) uses 4-byte (int32_t) words in memory, InputChunkSize is 480 * 4 = 1920 B.
  const size_t chunk_size = encoder.InputChunkSize();
  EXPECT_EQ(chunk_size, 1920u);

  // One 1920-byte chunk at 48 kHz (480 samples) must advance the timestamp by 10 ms (10,000,000
  // ns), not 13.333 ms (which would happen if bits_per_sample / 8 == 3 bytes/sample were used).
  auto extrapolator = encoder.CreateTimestampExtrapolator(format_details);
  extrapolator.Inform(0, 0);
  auto extrapolated = extrapolator.Extrapolate(chunk_size);
  ASSERT_TRUE(extrapolated.has_value());
  EXPECT_EQ(*extrapolated, 10'000'000ull);
}

TEST(CodecAdapterLc3EncoderTest, BytesPerSampleValidation) {
  {
    // Omitted bytes_per_sample on 24-bit PcmFormat2 defaults to packed 3 bytes per sample
    // (LC3_PCM_FORMAT_S24_3LE) and succeeds with 480 * 3 = 1440-byte chunks and 10 ms timestamps.
    std::mutex lock;
    FakeCodecAdapterEvents events;
    TestCodecAdapterLc3Encoder encoder(lock, &events);

    auto format_details = MakeValidLc3EncoderFormatDetails();
    format_details.mutable_pcm_format()->set_bits_per_sample(24);
    format_details.set_timebase(1'000'000'000ull);
    ASSERT_EQ(encoder.ProcessFormatDetails(format_details), TestCodecAdapterLc3Encoder::kOk);
    EXPECT_EQ(events.fail_codec_count(), 0u);
    EXPECT_EQ(encoder.InputChunkSize(), 1440u);

    auto extrapolator = encoder.CreateTimestampExtrapolator(format_details);
    extrapolator.Inform(0, 0);
    auto extrapolated = extrapolator.Extrapolate(1440u);
    ASSERT_TRUE(extrapolated.has_value());
    EXPECT_EQ(*extrapolated, 10'000'000ull);
  }

  {
    // Explicit packed 3-byte container for 24-bit PCM succeeds (LC3_PCM_FORMAT_S24_3LE).
    std::mutex lock;
    FakeCodecAdapterEvents events;
    TestCodecAdapterLc3Encoder encoder(lock, &events);

    auto format_details = MakeValidLc3EncoderFormatDetails();
    format_details.mutable_pcm_format()->set_bits_per_sample(24);
    format_details.mutable_pcm_format()->set_bytes_per_sample(3);
    EXPECT_EQ(encoder.ProcessFormatDetails(format_details), TestCodecAdapterLc3Encoder::kOk);
    EXPECT_EQ(events.fail_codec_count(), 0u);
    EXPECT_EQ(encoder.InputChunkSize(), 1440u);
  }

  {
    // Undersized 2-byte container for 24-bit PCM is rejected.
    std::mutex lock;
    FakeCodecAdapterEvents events;
    TestCodecAdapterLc3Encoder encoder(lock, &events);

    auto format_details = MakeValidLc3EncoderFormatDetails();
    format_details.mutable_pcm_format()->set_bits_per_sample(24);
    format_details.mutable_pcm_format()->set_bytes_per_sample(2);
    EXPECT_EQ(encoder.ProcessFormatDetails(format_details),
              TestCodecAdapterLc3Encoder::kShouldTerminate);
    EXPECT_EQ(events.fail_codec_count(), 1u);
    EXPECT_NE(events.last_fail_message().find("bytes_per_sample 2"), std::string::npos)
        << "Actual message: " << events.last_fail_message();
  }

  {
    // Unsupported 8-byte container for 24-bit PCM is rejected.
    std::mutex lock;
    FakeCodecAdapterEvents events;
    TestCodecAdapterLc3Encoder encoder(lock, &events);

    auto format_details = MakeValidLc3EncoderFormatDetails();
    format_details.mutable_pcm_format()->set_bits_per_sample(24);
    format_details.mutable_pcm_format()->set_bytes_per_sample(8);
    EXPECT_EQ(encoder.ProcessFormatDetails(format_details),
              TestCodecAdapterLc3Encoder::kShouldTerminate);
    EXPECT_EQ(events.fail_codec_count(), 1u);
    EXPECT_NE(events.last_fail_message().find("bytes_per_sample 8"), std::string::npos)
        << "Actual message: " << events.last_fail_message();
  }

  {
    // Mismatched 4-byte container for 16-bit PCM is rejected.
    std::mutex lock;
    FakeCodecAdapterEvents events;
    TestCodecAdapterLc3Encoder encoder(lock, &events);

    auto format_details = MakeValidLc3EncoderFormatDetails();
    format_details.mutable_pcm_format()->set_bits_per_sample(16);
    format_details.mutable_pcm_format()->set_bytes_per_sample(4);
    EXPECT_EQ(encoder.ProcessFormatDetails(format_details),
              TestCodecAdapterLc3Encoder::kShouldTerminate);
    EXPECT_EQ(events.fail_codec_count(), 1u);
    EXPECT_NE(events.last_fail_message().find("bytes_per_sample"), std::string::npos)
        << "Actual message: " << events.last_fail_message();
  }

  {
    // Explicit 4-byte container for 24-bit PCM succeeds.
    std::mutex lock;
    FakeCodecAdapterEvents events;
    TestCodecAdapterLc3Encoder encoder(lock, &events);

    auto format_details = MakeValidLc3EncoderFormatDetails();
    format_details.mutable_pcm_format()->set_bits_per_sample(24);
    format_details.mutable_pcm_format()->set_bytes_per_sample(4);
    EXPECT_EQ(encoder.ProcessFormatDetails(format_details), TestCodecAdapterLc3Encoder::kOk);
    EXPECT_EQ(events.fail_codec_count(), 0u);
    EXPECT_EQ(encoder.InputChunkSize(), 1920u);
  }

  {
    // Explicit 2-byte container for 16-bit PCM succeeds.
    std::mutex lock;
    FakeCodecAdapterEvents events;
    TestCodecAdapterLc3Encoder encoder(lock, &events);

    auto format_details = MakeValidLc3EncoderFormatDetails();
    format_details.mutable_pcm_format()->set_bits_per_sample(16);
    format_details.mutable_pcm_format()->set_bytes_per_sample(2);
    EXPECT_EQ(encoder.ProcessFormatDetails(format_details), TestCodecAdapterLc3Encoder::kOk);
    EXPECT_EQ(events.fail_codec_count(), 0u);
    EXPECT_EQ(encoder.InputChunkSize(), 960u);
  }
}

TEST(CodecAdapterLc3DecoderTest, OutputBufferConstraintsAvailableAfterCleanUpAfterStream) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  ASSERT_EQ(decoder.ProcessFormatDetails(MakeValidLc3DecoderFormatDetails()),
            TestCodecAdapterLc3Decoder::kOk);

  // Stop/clean up the stream (which resets codec_params_ and chunk_input_stream_) and verify
  // CoreCodecGetBufferCollectionConstraints2(kOutputPort) still succeeds using cached size.
  decoder.CleanUpAfterStream();

  fuchsia::media::StreamBufferConstraints stream_constraints;
  fuchsia::media::StreamBufferPartialSettings partial_settings;
  auto constraints = decoder.CoreCodecGetBufferCollectionConstraints2(
      kOutputPort, stream_constraints, partial_settings);
  ASSERT_TRUE(constraints.buffer_memory_constraints().has_value());
  EXPECT_EQ(constraints.buffer_memory_constraints()->min_size_bytes(), 960u);
}

TEST(CodecAdapterLc3EncoderTest, OutputBufferConstraintsAvailableAfterCleanUpAfterStream) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Encoder encoder(lock, &events);

  ASSERT_EQ(encoder.ProcessFormatDetails(MakeValidLc3EncoderFormatDetails()),
            TestCodecAdapterLc3Encoder::kOk);

  encoder.CleanUpAfterStream();

  fuchsia::media::StreamBufferConstraints stream_constraints;
  fuchsia::media::StreamBufferPartialSettings partial_settings;
  auto constraints = encoder.CoreCodecGetBufferCollectionConstraints2(
      kOutputPort, stream_constraints, partial_settings);
  ASSERT_TRUE(constraints.buffer_memory_constraints().has_value());
  EXPECT_EQ(constraints.buffer_memory_constraints()->min_size_bytes(), 40u);
}

TEST(CodecAdapterLc3DecoderTest, EmptyStreamEndOfStreamAndStaleChunkInputStreamCleanedUp) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterLc3Decoder decoder(lock, &events);

  auto format_details = MakeValidLc3DecoderFormatDetails();
  format_details.set_format_details_version_ordinal(0);

  decoder.CoreCodecInit(format_details);
  ASSERT_EQ(events.fail_codec_count(), 0u);

  // Stream 1: Queue EndOfStream immediately without any FormatDetails or Packet.
  decoder.CoreCodecStartStream();
  decoder.CoreCodecQueueInputEndOfStream();
  events.WaitForOutputEosCount(1);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(events.output_eos_count(), 1u);
  decoder.CoreCodecStopStream();

  // Stream 2: Queue FormatDetails and a partial input packet (10 bytes < 40-byte frame size)
  // so chunk_input_stream_ buffers partial bytes in scratch_block_, then stop stream without EOS.
  decoder.CoreCodecStartStream();
  decoder.CoreCodecQueueInputFormatDetails(format_details);
  events.WaitForOutputConstraintsChangeCount(1);
  decoder.CoreCodecMidStreamOutputBufferReConfigFinish();

  auto input_buffers = Buffers({4096});
  auto input_packets = Packets(1);
  CodecPacket* input_packet = input_packets.ptr(0);
  input_packet->SetBuffer(input_buffers.ptr(0));
  input_packet->SetStartOffset(0);
  input_packet->SetValidLengthBytes(10);  // Partial frame (< 40 bytes)
  decoder.CoreCodecQueueInputPacket(input_packet);
  events.WaitForInputPacketDoneCount(1);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  decoder.CoreCodecStopStream();

  // Between streams: CoreCodecGetBufferCollectionConstraints2(kOutputPort) must succeed.
  fuchsia::media::StreamBufferConstraints stream_constraints;
  fuchsia::media::StreamBufferPartialSettings partial_settings;
  auto constraints = decoder.CoreCodecGetBufferCollectionConstraints2(
      kOutputPort, stream_constraints, partial_settings);
  ASSERT_TRUE(constraints.buffer_memory_constraints().has_value());
  EXPECT_EQ(constraints.buffer_memory_constraints()->min_size_bytes(), 960u);

  // Stream 3: Queue EndOfStream without FormatDetails or Packet; must not flush stale partial
  // bytes from Stream 2 or dereference null codec_params_.
  decoder.CoreCodecStartStream();
  decoder.CoreCodecQueueInputEndOfStream();
  events.WaitForOutputEosCount(2);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(events.output_eos_count(), 2u);
  EXPECT_EQ(events.output_packet_count(), 0u);
  decoder.CoreCodecStopStream();
}

TEST(CodecAdapterLc3EncoderTest, S16And24In32HighOrderAnd24PackedProduceIdenticalFrames) {
  constexpr size_t kNumFrames = 3;
  constexpr size_t kFramesPerChunk = 480;  // 48 kHz, 10 ms
  constexpr size_t kChannels = 2;
  constexpr size_t kTotalSamples = kNumFrames * kFramesPerChunk * kChannels;

  std::vector<int16_t> s16_samples(kTotalSamples);
  for (size_t frame = 0; frame < kNumFrames * kFramesPerChunk; ++frame) {
    const double t = static_cast<double>(frame) / 48000.0;
    const double l = 0.55 * std::sin(2.0 * std::numbers::pi * 997.0 * t) +
                     0.20 * std::sin(2.0 * std::numbers::pi * 3511.0 * t);
    const double r = 0.55 * std::cos(2.0 * std::numbers::pi * 997.0 * t) -
                     0.20 * std::sin(2.0 * std::numbers::pi * 2311.0 * t);
    s16_samples[frame * kChannels] = static_cast<int16_t>(std::lround(l * 32767.0));
    s16_samples[frame * kChannels + 1] = static_cast<int16_t>(std::lround(r * 32767.0));
  }

  auto make_stereo_format = [](uint32_t bits_per_sample, std::optional<uint32_t> bytes_per_sample) {
    auto format_details = MakeValidLc3EncoderFormatDetails();
    format_details.mutable_pcm_format()->set_bits_per_sample(bits_per_sample);
    if (bytes_per_sample.has_value()) {
      format_details.mutable_pcm_format()->set_bytes_per_sample(*bytes_per_sample);
    } else {
      format_details.mutable_pcm_format()->clear_bytes_per_sample();
    }
    format_details.mutable_pcm_format()->set_channel_map({
        fuchsia::media::AudioChannelId2::FL,
        fuchsia::media::AudioChannelId2::FR,
    });
    return format_details;
  };

  auto encode_all_chunks = [](const fuchsia::media::FormatDetails& format_details,
                              const std::vector<uint8_t>& raw_pcm) {
    std::mutex lock;
    FakeCodecAdapterEvents events;
    TestCodecAdapterLc3Encoder encoder(lock, &events);
    EXPECT_EQ(encoder.ProcessFormatDetails(format_details), TestCodecAdapterLc3Encoder::kOk);
    EXPECT_EQ(events.fail_codec_count(), 0u);

    const size_t chunk_bytes = encoder.InputChunkSize();
    const size_t out_chunk_bytes = encoder.MinOutputBufferSize();
    EXPECT_EQ(raw_pcm.size(), kNumFrames * chunk_bytes);

    // Offset input buffer by +1 byte to exercise unaligned packet offsets across all formats.
    std::vector<uint8_t> unaligned_storage(raw_pcm.size() + 4, 0);
    std::memcpy(unaligned_storage.data() + 1, raw_pcm.data(), raw_pcm.size());

    std::vector<uint8_t> encoded(kNumFrames * out_chunk_bytes, 0);
    for (size_t f = 0; f < kNumFrames; ++f) {
      int produced =
          encoder.ProcessInputChunkData(unaligned_storage.data() + 1 + f * chunk_bytes, chunk_bytes,
                                        encoded.data() + f * out_chunk_bytes, out_chunk_bytes);
      EXPECT_EQ(produced, static_cast<int>(out_chunk_bytes));
    }
    return encoded;
  };

  // (a) 16-bit signed PCM (`bytes_per_sample = 2`).
  std::vector<uint8_t> pcm_s16(kTotalSamples * sizeof(int16_t));
  std::memcpy(pcm_s16.data(), s16_samples.data(), pcm_s16.size());

  // (b) 24-in-32 signed PCM (`bytes_per_sample = 4`): valid bits in `31..8` (`s16 << 16`), with
  // arbitrary non-zero garbage in low-order padding bits `7..0` that must be ignored.
  std::vector<uint8_t> pcm_s24_in_32(kTotalSamples * sizeof(int32_t));
  for (size_t i = 0; i < kTotalSamples; ++i) {
    const uint32_t low_byte_padding = static_cast<uint32_t>((i * 73u + 0xA5u) & 0xFFu);
    const uint32_t word =
        (static_cast<uint32_t>(static_cast<uint16_t>(s16_samples[i])) << 16) | low_byte_padding;
    std::memcpy(pcm_s24_in_32.data() + i * sizeof(int32_t), &word, sizeof(uint32_t));
  }

  // (c) Packed 24-bit signed little-endian PCM (`bytes_per_sample = 3` or omitted): `[0x00, lo,
  // hi]`.
  std::vector<uint8_t> pcm_s24_packed(kTotalSamples * 3);
  for (size_t i = 0; i < kTotalSamples; ++i) {
    const auto u16 = static_cast<uint16_t>(s16_samples[i]);
    pcm_s24_packed[i * 3] = 0x00;
    pcm_s24_packed[i * 3 + 1] = static_cast<uint8_t>(u16 & 0xFFu);
    pcm_s24_packed[i * 3 + 2] = static_cast<uint8_t>((u16 >> 8) & 0xFFu);
  }

  const auto encoded_s16 = encode_all_chunks(make_stereo_format(16, 2), pcm_s16);
  const auto encoded_s16_silence =
      encode_all_chunks(make_stereo_format(16, 2), std::vector<uint8_t>(pcm_s16.size(), 0));
  ASSERT_NE(encoded_s16, encoded_s16_silence);

  const auto encoded_s24_in_32 = encode_all_chunks(make_stereo_format(24, 4), pcm_s24_in_32);
  const auto encoded_s24_packed_explicit =
      encode_all_chunks(make_stereo_format(24, 3), pcm_s24_packed);
  const auto encoded_s24_packed_implicit =
      encode_all_chunks(make_stereo_format(24, std::nullopt), pcm_s24_packed);

  EXPECT_EQ(encoded_s24_in_32, encoded_s16);
  EXPECT_EQ(encoded_s24_packed_explicit, encoded_s16);
  EXPECT_EQ(encoded_s24_packed_implicit, encoded_s16);
}

TEST(CodecAdapterLc3EncoderTest, NearFullScale24In32EncodesAndDecodesWithoutOverflow) {
  constexpr size_t kNumFrames = 4;
  constexpr size_t kFramesPerChunk = 480;  // 48 kHz, 10 ms
  constexpr uint16_t kNbytes = 120;

  std::mutex enc_lock;
  FakeCodecAdapterEvents enc_events;
  TestCodecAdapterLc3Encoder encoder(enc_lock, &enc_events);

  auto enc_format = MakeValidLc3EncoderFormatDetails();
  enc_format.mutable_pcm_format()->set_bits_per_sample(24);
  enc_format.mutable_pcm_format()->set_bytes_per_sample(4);
  enc_format.mutable_encoder_settings()->lc3().set_nbytes(kNbytes);
  ASSERT_EQ(encoder.ProcessFormatDetails(enc_format), TestCodecAdapterLc3Encoder::kOk);

  std::mutex dec_lock;
  FakeCodecAdapterEvents dec_events;
  TestCodecAdapterLc3Decoder decoder(dec_lock, &dec_events);

  std::vector<uint8_t> oob_bytes = {
      0x02, 0x01, 0x08,                    // Sampling_Frequency: 48 kHz
      0x02, 0x02, 0x01,                    // Frame_Duration: 10 ms
      0x05, 0x03, 0x00, 0x00, 0x00, 0x01,  // Audio_Channel_Allocation: LF
      0x03, 0x04, 0x00, 0x78               // Octets_Per_Codec_Frame: 120
  };
  ASSERT_EQ(decoder.ProcessFormatDetails(MakeLc3FormatDetails(std::move(oob_bytes))),
            TestCodecAdapterLc3Decoder::kOk);

  // 85% full-scale 1 kHz sine wave in left-justified 24-in-32 (`0x7FFFFF00` full scale).
  // Without shifting right by 8 before `LC3_PCM_FORMAT_S24`, samples exceed `2^23 - 1` by ~217x,
  // overflowing `liblc3`'s `int16_t` resampler input and clipping to full-scale distortion.
  constexpr double kAmplitude24 = 0.85 * static_cast<double>(0x7FFFFF);
  std::vector<int16_t> decoded_steady_state;
  for (size_t f = 0; f < kNumFrames; ++f) {
    std::vector<uint8_t> pcm_chunk(kFramesPerChunk * sizeof(int32_t));
    for (size_t i = 0; i < kFramesPerChunk; ++i) {
      const double t = static_cast<double>(f * kFramesPerChunk + i) / 48000.0;
      const auto s24 = static_cast<int32_t>(
          std::lround(kAmplitude24 * std::sin(2.0 * std::numbers::pi * 1000.0 * t)));
      const int32_t s24_in_32 = static_cast<int32_t>(static_cast<uint32_t>(s24) << 8);
      std::memcpy(pcm_chunk.data() + i * sizeof(int32_t), &s24_in_32, sizeof(int32_t));
    }

    std::vector<uint8_t> encoded_frame(kNbytes);
    ASSERT_EQ(encoder.ProcessInputChunkData(pcm_chunk.data(), pcm_chunk.size(),
                                            encoded_frame.data(), encoded_frame.size()),
              static_cast<int>(kNbytes));

    std::vector<uint8_t> decoded_bytes(decoder.MinOutputBufferSize());
    ASSERT_EQ(decoder.ProcessInputChunkData(encoded_frame.data(), encoded_frame.size(),
                                            decoded_bytes.data(), decoded_bytes.size()),
              static_cast<int>(decoded_bytes.size()));

    if (f >= 2) {
      const size_t prev = decoded_steady_state.size();
      decoded_steady_state.resize(prev + kFramesPerChunk);
      std::memcpy(decoded_steady_state.data() + prev, decoded_bytes.data(), decoded_bytes.size());
    }
  }

  int32_t max_abs = 0;
  double sum_sq = 0.0;
  for (int16_t s : decoded_steady_state) {
    max_abs = std::max(max_abs, std::abs(static_cast<int32_t>(s)));
    sum_sq += static_cast<double>(s) * static_cast<double>(s);
  }
  const double rms = std::sqrt(sum_sq / static_cast<double>(decoded_steady_state.size()));

  // Expected 16-bit peak is ~0.85 * 32767 = 27852; expected RMS is ~27852 / sqrt(2) = 19694.
  EXPECT_GT(max_abs, 25000);
  EXPECT_LT(max_abs, 30500);
  EXPECT_GT(rms, 18000.0);
  EXPECT_LT(rms, 21500.0);
}

TEST(CodecAdapterLc3EncoderTest, LegacyDomain24BitInputUsesHighOrderBits) {
  constexpr size_t kNumFrames = 3;
  constexpr size_t kFramesPerChunk = 480;  // 48 kHz, 10 ms
  constexpr size_t kTotalSamples = kNumFrames * kFramesPerChunk;

  // Bipolar (both negative and positive) 16-bit signal so sign-extension and shift behavior are
  // both exercised.
  std::vector<int16_t> s16_samples(kTotalSamples);
  for (size_t i = 0; i < kTotalSamples; ++i) {
    const double t = static_cast<double>(i) / 48000.0;
    const double v = 0.55 * std::sin(2.0 * std::numbers::pi * 997.0 * t) +
                     0.20 * std::sin(2.0 * std::numbers::pi * 3511.0 * t);
    s16_samples[i] = static_cast<int16_t>(std::lround(v * 32767.0));
  }

  auto encode_mono_chunks = [](const fuchsia::media::FormatDetails& format_details,
                               const std::vector<uint8_t>& raw_pcm) {
    std::mutex lock;
    FakeCodecAdapterEvents events;
    TestCodecAdapterLc3Encoder encoder(lock, &events);
    EXPECT_EQ(encoder.ProcessFormatDetails(format_details), TestCodecAdapterLc3Encoder::kOk);
    EXPECT_EQ(events.fail_codec_count(), 0u);

    const size_t chunk_bytes = encoder.InputChunkSize();
    const size_t out_chunk_bytes = encoder.MinOutputBufferSize();
    EXPECT_EQ(raw_pcm.size(), kNumFrames * chunk_bytes);

    std::vector<uint8_t> encoded(kNumFrames * out_chunk_bytes, 0);
    for (size_t f = 0; f < kNumFrames; ++f) {
      int produced =
          encoder.ProcessInputChunkData(raw_pcm.data() + f * chunk_bytes, chunk_bytes,
                                        encoded.data() + f * out_chunk_bytes, out_chunk_bytes);
      EXPECT_EQ(produced, static_cast<int>(out_chunk_bytes));
    }
    return encoded;
  };

  auto v2_s16_format = MakeValidLc3EncoderFormatDetails();
  std::vector<uint8_t> pcm_s16(kTotalSamples * sizeof(int16_t));
  std::memcpy(pcm_s16.data(), s16_samples.data(), pcm_s16.size());
  const auto encoded_s16 = encode_mono_chunks(v2_s16_format, pcm_s16);
  const auto encoded_silence =
      encode_mono_chunks(v2_s16_format, std::vector<uint8_t>(pcm_s16.size(), 0));
  ASSERT_NE(encoded_s16, encoded_silence);

  // Construct legacy V1 `domain.audio.uncompressed.pcm` 24-bit input while keeping `lc3`
  // `encoder_settings`, then normalize via `CodecImpl` just as `CodecImpl` does on input
  // `FormatDetails` before calling the adapter.
  auto legacy_format = MakeValidLc3EncoderFormatDetails();
  legacy_format.clear_pcm_format();
  fuchsia::media::PcmFormat pcm_v1;
  pcm_v1.pcm_mode = fuchsia::media::AudioPcmMode::LINEAR;
  pcm_v1.bits_per_sample = 24;
  pcm_v1.frames_per_second = 48000;
  pcm_v1.channel_map = {fuchsia::media::AudioChannelId::LF};
  legacy_format.mutable_domain()->audio().uncompressed().set_pcm(std::move(pcm_v1));

  CodecImpl::EnsureInputFormatDetailsNormalizedForTesting(&legacy_format);
  ASSERT_TRUE(legacy_format.has_pcm_format());
  EXPECT_EQ(legacy_format.pcm_format().bits_per_sample(), 24u);
  ASSERT_TRUE(legacy_format.pcm_format().has_bytes_per_sample());
  EXPECT_EQ(legacy_format.pcm_format().bytes_per_sample(), 4u);

  // Left-justified 24-in-32 (`s16 << 16` in bits `31..8`, non-zero padding in bits `7..0`) must
  // encode identically to 16-bit `PcmFormat2`.
  std::vector<uint8_t> pcm_legacy_24_in_32(kTotalSamples * sizeof(int32_t));
  for (size_t i = 0; i < kTotalSamples; ++i) {
    const uint32_t low_byte_padding = static_cast<uint32_t>((i * 73u + 0xA5u) & 0xFFu);
    const uint32_t word =
        (static_cast<uint32_t>(static_cast<uint16_t>(s16_samples[i])) << 16) | low_byte_padding;
    std::memcpy(pcm_legacy_24_in_32.data() + i * sizeof(int32_t), &word, sizeof(uint32_t));
  }

  const auto encoded_legacy_24_in_32 = encode_mono_chunks(legacy_format, pcm_legacy_24_in_32);
  EXPECT_EQ(encoded_legacy_24_in_32, encoded_s16);
}

}  // namespace
