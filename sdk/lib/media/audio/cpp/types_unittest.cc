// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "lib/media/audio/cpp/types.h"

#include <gtest/gtest.h>

namespace media {
namespace {

TEST(TypesTest, BytesPerSampleHlcpp) {
  EXPECT_EQ(1u, BytesPerSample(fuchsia::media::AudioSampleFormat::UNSIGNED_8));
  EXPECT_EQ(2u, BytesPerSample(fuchsia::media::AudioSampleFormat::SIGNED_16));
  EXPECT_EQ(4u, BytesPerSample(fuchsia::media::AudioSampleFormat::SIGNED_24_IN_32));
  EXPECT_EQ(4u, BytesPerSample(fuchsia::media::AudioSampleFormat::FLOAT));
}

TEST(TypesTest, BytesPerSampleNatural) {
  EXPECT_EQ(1u, BytesPerSample(fuchsia_media::AudioSampleFormat::kUnsigned8));
  EXPECT_EQ(2u, BytesPerSample(fuchsia_media::AudioSampleFormat::kSigned16));
  EXPECT_EQ(4u, BytesPerSample(fuchsia_media::AudioSampleFormat::kSigned24In32));
  EXPECT_EQ(4u, BytesPerSample(fuchsia_media::AudioSampleFormat::kFloat));
}

// The HLCPP and natural overloads must agree for every sample format.
TEST(TypesTest, BytesPerSampleOverloadsAgree) {
  const std::pair<fuchsia::media::AudioSampleFormat, fuchsia_media::AudioSampleFormat> formats[] = {
      {fuchsia::media::AudioSampleFormat::UNSIGNED_8, fuchsia_media::AudioSampleFormat::kUnsigned8},
      {fuchsia::media::AudioSampleFormat::SIGNED_16, fuchsia_media::AudioSampleFormat::kSigned16},
      {fuchsia::media::AudioSampleFormat::SIGNED_24_IN_32,
       fuchsia_media::AudioSampleFormat::kSigned24In32},
      {fuchsia::media::AudioSampleFormat::FLOAT, fuchsia_media::AudioSampleFormat::kFloat},
  };
  for (const auto& [hlcpp_format, natural_format] : formats) {
    EXPECT_EQ(static_cast<uint32_t>(hlcpp_format), static_cast<uint32_t>(natural_format));
    EXPECT_EQ(BytesPerSample(hlcpp_format), BytesPerSample(natural_format));
  }
}

TEST(TypesTest, CreateAudioStreamTypeHlcpp) {
  const fuchsia::media::AudioStreamType stream_type =
      CreateAudioStreamType(fuchsia::media::AudioSampleFormat::SIGNED_16, 2, 48000);
  EXPECT_EQ(fuchsia::media::AudioSampleFormat::SIGNED_16, stream_type.sample_format);
  EXPECT_EQ(2u, stream_type.channels);
  EXPECT_EQ(48000u, stream_type.frames_per_second);
}

TEST(TypesTest, CreateAudioStreamTypeNatural) {
  const fuchsia_media::AudioStreamType stream_type =
      CreateAudioStreamType(fuchsia_media::AudioSampleFormat::kSigned16, 2, 48000);
  EXPECT_EQ(fuchsia_media::AudioSampleFormat::kSigned16, stream_type.sample_format());
  EXPECT_EQ(2u, stream_type.channels());
  EXPECT_EQ(48000u, stream_type.frames_per_second());

  const fuchsia_media::AudioStreamType expected{{
      .sample_format = fuchsia_media::AudioSampleFormat::kSigned16,
      .channels = 2,
      .frames_per_second = 48000,
  }};
  EXPECT_EQ(expected, stream_type);
}

}  // namespace
}  // namespace media
