// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/media/cpp/fidl.h>

#include <mutex>
#include <utility>

#include <gtest/gtest.h>

#include "src/media/codec/codecs/sw/aac/codec_adapter_aac_encoder.h"
#include "src/media/codec/codecs/test/test_codec_packets.h"
#include "src/media/codec/codecs/test/test_fake_codec_adapter_events.h"

namespace {

fuchsia::media::FormatDetails MakeValidAacEncoderFormatDetails() {
  fuchsia::media::PcmFormat pcm;
  pcm.pcm_mode = fuchsia::media::AudioPcmMode::LINEAR;
  pcm.bits_per_sample = 16;
  pcm.frames_per_second = 48000;
  pcm.channel_map = {fuchsia::media::AudioChannelId::LF};

  fuchsia::media::AudioUncompressedFormat uncompressed;
  uncompressed.set_pcm(std::move(pcm));

  fuchsia::media::AudioFormat audio;
  audio.set_uncompressed(std::move(uncompressed));

  fuchsia::media::DomainFormat domain;
  domain.set_audio(std::move(audio));

  fuchsia::media::AacEncoderSettings aac_settings;
  aac_settings.aot = fuchsia::media::AacAudioObjectType::MPEG4_AAC_LC;
  aac_settings.channel_mode = fuchsia::media::AacChannelMode::MONO;
  aac_settings.bit_rate = fuchsia::media::AacBitRate::WithConstant(
      fuchsia::media::AacConstantBitRate{.bit_rate = 64000});
  aac_settings.transport = fuchsia::media::AacTransport::WithRaw(fuchsia::media::AacTransportRaw{});

  fuchsia::media::EncoderSettings encoder_settings;
  encoder_settings.set_aac(std::move(aac_settings));

  fuchsia::media::FormatDetails format_details;
  format_details.set_format_details_version_ordinal(0);
  format_details.set_domain(std::move(domain));
  format_details.set_encoder_settings(std::move(encoder_settings));
  return format_details;
}

TEST(CodecAdapterAacEncoderTest, EmptyStreamEndOfStreamAndPostStopOutputConstraints) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  CodecAdapterAacEncoder encoder(lock, &events);

  auto format_details = MakeValidAacEncoderFormatDetails();
  encoder.CoreCodecInit(format_details);
  ASSERT_EQ(events.fail_codec_count(), 0u);

  // Stream 1: Queue EndOfStream immediately without any FormatDetails or Packet.
  encoder.CoreCodecStartStream();
  encoder.CoreCodecQueueInputEndOfStream();
  events.WaitForOutputEosCount(1);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(events.output_eos_count(), 1u);
  encoder.CoreCodecStopStream();

  // Stream 2: Queue FormatDetails and a partial input packet (16 bytes < 2048-byte AAC chunk size)
  // so stream_->chunk_input_stream buffers partial bytes, then stop stream without EOS.
  encoder.CoreCodecStartStream();
  encoder.CoreCodecQueueInputFormatDetails(format_details);
  events.WaitForOutputConstraintsChangeCount(1);
  encoder.CoreCodecMidStreamOutputBufferReConfigFinish();

  auto input_buffers = Buffers({4096});
  auto input_packets = Packets(1);
  CodecPacket* input_packet = input_packets.ptr(0);
  input_packet->SetBuffer(input_buffers.ptr(0));
  input_packet->SetStartOffset(0);
  input_packet->SetValidLengthBytes(16);  // Partial AAC frame (< 2048 bytes)
  encoder.CoreCodecQueueInputPacket(input_packet);
  events.WaitForInputPacketDoneCount(1);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  encoder.CoreCodecStopStream();

  // Between streams: CoreCodecGetBufferCollectionConstraints2(kOutputPort) must succeed using
  // format_configuration_->recommended_output_buffer_size even after stream_ was reset.
  fuchsia::media::StreamBufferConstraints stream_constraints;
  fuchsia::media::StreamBufferPartialSettings partial_settings;
  auto constraints = encoder.CoreCodecGetBufferCollectionConstraints2(
      kOutputPort, stream_constraints, partial_settings);
  ASSERT_TRUE(constraints.buffer_memory_constraints().has_value());
  // kFdkMaxOutBytesPerChannel (6144 / 8 = 768) * 1 channel = 768 bytes.
  EXPECT_EQ(constraints.buffer_memory_constraints()->min_size_bytes(), 768u);

  // Stream 3: Queue EndOfStream without FormatDetails or Packet; must not flush stale partial
  // bytes from Stream 2 or dereference null stream_.
  encoder.CoreCodecStartStream();
  encoder.CoreCodecQueueInputEndOfStream();
  events.WaitForOutputEosCount(2);
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(events.output_eos_count(), 2u);
  EXPECT_EQ(events.output_packet_count(), 0u);
  encoder.CoreCodecStopStream();
}

TEST(CodecAdapterAacEncoderTest, QueuedPacketAndEosIgnoredAfterFormatDetailsFailure) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  CodecAdapterAacEncoder encoder(lock, &events);

  auto valid_format_details = MakeValidAacEncoderFormatDetails();
  encoder.CoreCodecInit(valid_format_details);
  ASSERT_EQ(events.fail_codec_count(), 0u);

  encoder.CoreCodecStartStream();

  // Queue invalid FormatDetails (missing encoder_settings) followed immediately by an input packet,
  // EndOfStream, and a trailing input packet on input_processing_loop_ before CoreCodecStopStream()
  // is called.
  fuchsia::media::FormatDetails invalid_format_details = MakeValidAacEncoderFormatDetails();
  invalid_format_details.clear_encoder_settings();
  encoder.CoreCodecQueueInputFormatDetails(invalid_format_details);

  auto input_buffers = Buffers({4096, 4096});
  auto input_packets = Packets(2);
  CodecPacket* input_packet_0 = input_packets.ptr(0);
  input_packet_0->SetBuffer(input_buffers.ptr(0));
  input_packet_0->SetStartOffset(0);
  input_packet_0->SetValidLengthBytes(16);
  CodecPacket* input_packet_1 = input_packets.ptr(1);
  input_packet_1->SetBuffer(input_buffers.ptr(1));
  input_packet_1->SetStartOffset(0);
  input_packet_1->SetValidLengthBytes(16);

  encoder.CoreCodecQueueInputPacket(input_packet_0);
  encoder.CoreCodecQueueInputEndOfStream();
  encoder.CoreCodecQueueInputPacket(input_packet_1);

  // Wait for both input packets (including the one queued after EndOfStream) to be returned on
  // input_processing_loop_ before calling CoreCodecStopStream(), verifying that ProcessInput itself
  // deactivated the stream upon FormatDetails failure.
  events.WaitForInputPacketDoneCount(2, /*stop_on_fail_codec=*/false);

  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_EQ(events.input_packet_done_count(), 2u);
  EXPECT_EQ(events.output_eos_count(), 0u);

  encoder.CoreCodecStopStream();
}

TEST(CodecAdapterAacEncoderTest, StopAndMidStreamFormatFailureWhileOutputReconfigPending) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  CodecAdapterAacEncoder encoder(lock, &events);

  auto format_details_v0 = MakeValidAacEncoderFormatDetails();
  encoder.CoreCodecInit(format_details_v0);
  ASSERT_EQ(events.fail_codec_count(), 0u);

  auto input_buffers = Buffers({4096, 4096});
  auto input_packets = Packets(2);
  CodecPacket* input_packet_0 = input_packets.ptr(0);
  input_packet_0->SetBuffer(input_buffers.ptr(0));
  input_packet_0->SetStartOffset(0);
  input_packet_0->SetValidLengthBytes(16);
  CodecPacket* input_packet_1 = input_packets.ptr(1);
  input_packet_1->SetBuffer(input_buffers.ptr(1));
  input_packet_1->SetStartOffset(0);
  input_packet_1->SetValidLengthBytes(16);

  // Stream 1: Queue FormatDetails (setting output_reconfig_pending_ = true) and a Packet + EOS
  // without calling CoreCodecMidStreamOutputBufferReConfigFinish(), then stop the stream.
  // CoreCodecStopStream() must unblock the waiting ProcessInput() call without deadlocking.
  encoder.CoreCodecStartStream();
  encoder.CoreCodecQueueInputFormatDetails(format_details_v0);
  events.WaitForOutputConstraintsChangeCount(1);
  encoder.CoreCodecQueueInputPacket(input_packet_0);
  encoder.CoreCodecQueueInputEndOfStream();
  encoder.CoreCodecStopStream();
  EXPECT_EQ(events.fail_codec_count(), 0u);
  EXPECT_EQ(events.input_packet_done_count(), 1u);
  EXPECT_EQ(events.output_eos_count(), 0u);

  // Stream 2: Queue FormatDetails(v0) (setting output_reconfig_pending_ = true), followed by a
  // midstream FormatDetails(v1) (which fails before ReConfigFinish) and a Packet + EOS.
  // ProcessInput() must reset output_reconfig_pending_ on failure so the trailing Packet + EOS
  // do not hang waiting on reconfig_cond_.
  encoder.CoreCodecStartStream();
  encoder.CoreCodecQueueInputFormatDetails(format_details_v0);
  events.WaitForOutputConstraintsChangeCount(2);

  auto format_details_v1 = MakeValidAacEncoderFormatDetails();
  format_details_v1.set_format_details_version_ordinal(1);
  encoder.CoreCodecQueueInputFormatDetails(format_details_v1);
  encoder.CoreCodecQueueInputPacket(input_packet_1);
  encoder.CoreCodecQueueInputEndOfStream();

  events.WaitForInputPacketDoneCount(2, /*stop_on_fail_codec=*/false);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_EQ(events.input_packet_done_count(), 2u);
  EXPECT_EQ(events.output_eos_count(), 0u);

  encoder.CoreCodecStopStream();
}

}  // namespace
