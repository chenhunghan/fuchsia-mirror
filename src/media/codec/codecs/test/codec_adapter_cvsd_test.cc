// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.media/cpp/fidl.h>

#include <mutex>
#include <string>
#include <utility>

#include <gtest/gtest.h>

#include "src/media/codec/codecs/sw/cvsd/codec_adapter_cvsd_encoder.h"
#include "src/media/codec/codecs/test/test_fake_codec_adapter_events.h"

namespace {

constexpr char kIncompletePcmFormatMessage[] = "CVSD Encoder received incomplete PcmFormat2.";
constexpr char kUnsupportedSampleFormatMessage[] =
    "CVSD Encoder only encodes mono audio with signed 16 bit linear samples.";

class TestCodecAdapterCvsdEncoder : public CodecAdapterCvsdEncoder {
 public:
  TestCodecAdapterCvsdEncoder(std::mutex& lock, CodecAdapterEvents* events)
      : CodecAdapterCvsdEncoder(lock, events) {}

  using CodecAdapterCvsdEncoder::InputLoopStatus;
  using CodecAdapterCvsdEncoder::ProcessFormatDetails;
};

fuchsia_media::FormatDetails MakeValidCvsdEncoderFormatDetails() {
  fuchsia_media::PcmFormat2 pcm;
  pcm.pcm_mode() = fuchsia_media::AudioPcmMode2::kLinear;
  pcm.bits_per_sample() = 16;
  pcm.frames_per_second() = kExpectedSamplingFreq;
  pcm.channel_map() = {fuchsia_media::AudioChannelId2::kFl};

  fuchsia_media::FormatDetails format_details;
  format_details.pcm_format() = std::move(pcm);
  format_details.encoder_settings() =
      fuchsia_media::EncoderSettings::WithCvsd(fuchsia_media::CvsdEncoderSettings{});
  return format_details;
}

void ExpectCvsdEncoderFormatRejected(const fuchsia_media::FormatDetails& format_details,
                                     const std::string& expected_message) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterCvsdEncoder encoder(lock, &events);

  EXPECT_EQ(encoder.ProcessFormatDetails(format_details),
            TestCodecAdapterCvsdEncoder::kShouldTerminate);
  EXPECT_EQ(events.fail_codec_count(), 1u);
  EXPECT_EQ(events.last_fail_message(), expected_message);
}

TEST(CodecAdapterCvsdEncoderTest, ValidFormatDetailsSucceeds) {
  std::mutex lock;
  FakeCodecAdapterEvents events;
  TestCodecAdapterCvsdEncoder encoder(lock, &events);

  EXPECT_EQ(encoder.ProcessFormatDetails(MakeValidCvsdEncoderFormatDetails()),
            TestCodecAdapterCvsdEncoder::kOk);
  EXPECT_EQ(events.fail_codec_count(), 0u);
}

TEST(CodecAdapterCvsdEncoderTest, IncompletePcmFormatRejected) {
  {
    auto format_details = MakeValidCvsdEncoderFormatDetails();
    format_details.pcm_format()->pcm_mode().reset();
    ExpectCvsdEncoderFormatRejected(format_details, kIncompletePcmFormatMessage);
  }
  {
    auto format_details = MakeValidCvsdEncoderFormatDetails();
    format_details.pcm_format()->bits_per_sample().reset();
    ExpectCvsdEncoderFormatRejected(format_details, kIncompletePcmFormatMessage);
  }
  {
    auto format_details = MakeValidCvsdEncoderFormatDetails();
    format_details.pcm_format()->frames_per_second().reset();
    ExpectCvsdEncoderFormatRejected(format_details, kIncompletePcmFormatMessage);
  }
  {
    auto format_details = MakeValidCvsdEncoderFormatDetails();
    format_details.pcm_format()->channel_map().reset();
    ExpectCvsdEncoderFormatRejected(format_details, kIncompletePcmFormatMessage);
  }
}

TEST(CodecAdapterCvsdEncoderTest, UnsupportedSampleFormatRejected) {
  {
    auto format_details = MakeValidCvsdEncoderFormatDetails();
    format_details.pcm_format()->pcm_mode() = fuchsia_media::AudioPcmMode2::kFloat;
    ExpectCvsdEncoderFormatRejected(format_details, kUnsupportedSampleFormatMessage);
  }
  {
    auto format_details = MakeValidCvsdEncoderFormatDetails();
    format_details.pcm_format()->bits_per_sample() = 8;
    ExpectCvsdEncoderFormatRejected(format_details, kUnsupportedSampleFormatMessage);
  }
  {
    auto format_details = MakeValidCvsdEncoderFormatDetails();
    format_details.pcm_format()->channel_map().emplace();
    ExpectCvsdEncoderFormatRejected(format_details, kUnsupportedSampleFormatMessage);
  }
  {
    auto format_details = MakeValidCvsdEncoderFormatDetails();
    format_details.pcm_format()->channel_map() = {fuchsia_media::AudioChannelId2::kFl,
                                                  fuchsia_media::AudioChannelId2::kFr};
    ExpectCvsdEncoderFormatRejected(format_details, kUnsupportedSampleFormatMessage);
  }
  {
    auto format_details = MakeValidCvsdEncoderFormatDetails();
    format_details.pcm_format()->bytes_per_sample() = 4;
    ExpectCvsdEncoderFormatRejected(format_details, kUnsupportedSampleFormatMessage);
  }
}

}  // namespace
