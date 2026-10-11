// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/media/cpp/fidl.h>

#include <cstdint>
#include <cstring>
#include <mutex>
#include <utility>
#include <vector>

#include <gtest/gtest.h>

#include "src/media/codec/codecs/sw/sbc/codec_adapter_sbc_decoder.h"
#include "src/media/codec/codecs/sw/sbc/codec_adapter_sbc_encoder.h"
#include "src/media/codec/codecs/test/test_codec_packets.h"
#include "src/media/codec/codecs/test/test_fake_codec_adapter_events.h"

namespace {

fuchsia::media::FormatDetails MakeValidSbcDecoderFormatDetails() {
  SbcCodecInfo codec_info = {
      .sampling_frequency = kSbcSamplingFrequency44100Hz,
      .channel_mode = kSbcChannelModeJointStereo,
      .block_length = 0b0001,     // 16 blocks
      .subbands = 0b01,           // 8 subbands
      .allocation_method = 0b01,  // Loudness
      .min_bitpool_value = 2,
      .max_bitpool_value = 53,
  };
  std::vector<uint8_t> oob_bytes(sizeof(SbcCodecInfo));
  std::memcpy(oob_bytes.data(), &codec_info, sizeof(SbcCodecInfo));

  fuchsia::media::FormatDetails format_details;
  format_details.set_format_details_version_ordinal(0);
  format_details.set_mime_type("audio/sbc");
  format_details.set_oob_bytes(std::move(oob_bytes));
  return format_details;
}

fuchsia::media::FormatDetails MakeValidSbcEncoderFormatDetails() {
  fuchsia::media::PcmFormat pcm;
  pcm.pcm_mode = fuchsia::media::AudioPcmMode::LINEAR;
  pcm.bits_per_sample = 16;
  pcm.frames_per_second = 44100;
  pcm.channel_map = {fuchsia::media::AudioChannelId::LF, fuchsia::media::AudioChannelId::RF};

  fuchsia::media::AudioUncompressedFormat uncompressed;
  uncompressed.set_pcm(std::move(pcm));

  fuchsia::media::AudioFormat audio;
  audio.set_uncompressed(std::move(uncompressed));

  fuchsia::media::DomainFormat domain;
  domain.set_audio(std::move(audio));

  fuchsia::media::SbcEncoderSettings sbc_settings;
  sbc_settings.sub_bands = static_cast<fuchsia::media::SbcSubBands>(SUB_BANDS_8);
  sbc_settings.block_count = fuchsia::media::SbcBlockCount::BLOCK_COUNT_16;
  sbc_settings.allocation = fuchsia::media::SbcAllocation::ALLOC_LOUDNESS;
  sbc_settings.channel_mode = fuchsia::media::SbcChannelMode::JOINT_STEREO;
  sbc_settings.bit_pool = 53;

  fuchsia::media::EncoderSettings encoder_settings;
  encoder_settings.set_sbc(sbc_settings);

  fuchsia::media::FormatDetails format_details;
  format_details.set_format_details_version_ordinal(0);
  format_details.set_domain(std::move(domain));
  format_details.set_encoder_settings(std::move(encoder_settings));
  return format_details;
}

TEST(CodecAdapterSbcDecoderTest, EmptyStreamEndOfStreamAndPostStopOutputConstraints) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  CodecAdapterSbcDecoder decoder(lock, &events);

  auto format_details = MakeValidSbcDecoderFormatDetails();
  decoder.CoreCodecInit(format_details);
  ASSERT_EQ(events.fail_codec_count(), 0u);

  // Stream 1: Queue EndOfStream immediately without any FormatDetails or Packet.
  decoder.CoreCodecStartStream();
  decoder.CoreCodecQueueInputEndOfStream();
  events.WaitForOutputEosCount(1);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(events.output_eos_count(), 1u);
  decoder.CoreCodecStopStream();

  // Stream 2: Queue FormatDetails so CreateContext caches min_output_buffer_size_, then stop
  // stream (which resets context_ in CleanUpAfterStream()).
  decoder.CoreCodecStartStream();
  decoder.CoreCodecQueueInputFormatDetails(format_details);
  events.WaitForOutputConstraintsChangeCount(1);
  decoder.CoreCodecMidStreamOutputBufferReConfigFinish();
  EXPECT_EQ(events.fail_codec_count(), 0u);
  decoder.CoreCodecStopStream();

  // Between streams: CoreCodecGetBufferCollectionConstraints2(kOutputPort) must succeed even
  // though context_ was reset by CleanUpAfterStream().
  fuchsia::media::StreamBufferConstraints stream_constraints;
  fuchsia::media::StreamBufferPartialSettings partial_settings;
  auto constraints = decoder.CoreCodecGetBufferCollectionConstraints2(
      kOutputPort, stream_constraints, partial_settings);
  ASSERT_TRUE(constraints.buffer_memory_constraints().has_value());
  // (16 bits / 8) * SBC_MAX_SAMPLES_PER_FRAME (128) * SBC_MAX_CHANNELS (2) = 512 bytes.
  EXPECT_EQ(constraints.buffer_memory_constraints()->min_size_bytes(), 512u);

  // Stream 3: Queue EndOfStream without FormatDetails or Packet after a configured stream.
  decoder.CoreCodecStartStream();
  decoder.CoreCodecQueueInputEndOfStream();
  events.WaitForOutputEosCount(2);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(events.output_eos_count(), 2u);
  EXPECT_EQ(events.output_packet_count(), 0u);
  decoder.CoreCodecStopStream();
}

TEST(CodecAdapterSbcEncoderTest, EmptyStreamEndOfStreamAndPostStopOutputConstraints) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  CodecAdapterSbcEncoder encoder(lock, &events);

  auto format_details = MakeValidSbcEncoderFormatDetails();
  encoder.CoreCodecInit(format_details);
  ASSERT_EQ(events.fail_codec_count(), 0u);

  // Stream 1: Queue EndOfStream immediately without any FormatDetails or Packet.
  encoder.CoreCodecStartStream();
  encoder.CoreCodecQueueInputEndOfStream();
  events.WaitForOutputEosCount(1);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(events.output_eos_count(), 1u);
  encoder.CoreCodecStopStream();

  // Stream 2: Queue FormatDetails and a partial input packet (16 bytes < 512-byte PCM batch size)
  // so chunk_input_stream_ buffers partial bytes in scratch_block_, then stop stream without EOS.
  encoder.CoreCodecStartStream();
  encoder.CoreCodecQueueInputFormatDetails(format_details);
  events.WaitForOutputConstraintsChangeCount(1);
  encoder.CoreCodecMidStreamOutputBufferReConfigFinish();

  auto input_buffers = Buffers({4096});
  auto input_packets = Packets(1);
  CodecPacket* input_packet = input_packets.ptr(0);
  input_packet->SetBuffer(input_buffers.ptr(0));
  input_packet->SetStartOffset(0);
  input_packet->SetValidLengthBytes(16);  // Partial PCM batch (< 512 bytes)
  encoder.CoreCodecQueueInputPacket(input_packet);
  events.WaitForInputPacketDoneCount(1);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  encoder.CoreCodecStopStream();

  // Between streams: CoreCodecGetBufferCollectionConstraints2(kOutputPort) must succeed using
  // cached min_output_buffer_size_.
  fuchsia::media::StreamBufferConstraints stream_constraints;
  fuchsia::media::StreamBufferPartialSettings partial_settings;
  auto constraints = encoder.CoreCodecGetBufferCollectionConstraints2(
      kOutputPort, stream_constraints, partial_settings);
  ASSERT_TRUE(constraints.buffer_memory_constraints().has_value());
  // For 8 subbands, 16 blocks, JOINT_STEREO, bitpool 53: 4 + (8 * 2 / 2) + ceil((8 + 16 * 53) / 8)
  // = 12 + 107 = 119 bytes.
  EXPECT_EQ(constraints.buffer_memory_constraints()->min_size_bytes(), 119u);

  // Stream 3: Queue EndOfStream without FormatDetails or Packet; must not flush stale partial
  // bytes from Stream 2 or dereference null context_.
  encoder.CoreCodecStartStream();
  encoder.CoreCodecQueueInputEndOfStream();
  events.WaitForOutputEosCount(2);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(events.output_eos_count(), 2u);
  EXPECT_EQ(events.output_packet_count(), 0u);
  encoder.CoreCodecStopStream();
}

}  // namespace
