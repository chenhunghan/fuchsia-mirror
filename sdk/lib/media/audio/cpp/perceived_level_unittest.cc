// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "lib/media/audio/cpp/perceived_level.h"

#include <fidl/fuchsia.media.audio/cpp/fidl.h>

#include <gtest/gtest.h>

namespace media {
namespace {

TEST(PerceivedLevelTest, GainToLevel) {
  EXPECT_FLOAT_EQ(0.0f, PerceivedLevel::GainToLevel(fuchsia_media_audio::kMutedGainDb));
  EXPECT_FLOAT_EQ(0.0f, PerceivedLevel::GainToLevel(-60.0f));
  EXPECT_FLOAT_EQ(0.0f, PerceivedLevel::GainToLevel(-61.0f));
  EXPECT_FLOAT_EQ(0.5f, PerceivedLevel::GainToLevel(-30.0f));
  EXPECT_FLOAT_EQ(1.0f, PerceivedLevel::GainToLevel(0.0f));
  EXPECT_FLOAT_EQ(1.0f, PerceivedLevel::GainToLevel(10.0f));
}

TEST(PerceivedLevelTest, LevelToGain) {
  EXPECT_FLOAT_EQ(fuchsia_media_audio::kMutedGainDb, PerceivedLevel::LevelToGain(0.0f));
  EXPECT_FLOAT_EQ(fuchsia_media_audio::kMutedGainDb, PerceivedLevel::LevelToGain(-1.0f));
  EXPECT_FLOAT_EQ(-30.0f, PerceivedLevel::LevelToGain(0.5f));
  EXPECT_FLOAT_EQ(0.0f, PerceivedLevel::LevelToGain(1.0f));
  EXPECT_FLOAT_EQ(0.0f, PerceivedLevel::LevelToGain(2.0f));
}

TEST(PerceivedLevelTest, GainToLevelWithMaxLevel) {
  EXPECT_EQ(0, PerceivedLevel::GainToLevel(fuchsia_media_audio::kMutedGainDb, 10));
  EXPECT_EQ(0, PerceivedLevel::GainToLevel(-60.0f, 10));
  EXPECT_EQ(5, PerceivedLevel::GainToLevel(-30.0f, 10));
  EXPECT_EQ(10, PerceivedLevel::GainToLevel(0.0f, 10));
  EXPECT_EQ(10, PerceivedLevel::GainToLevel(10.0f, 10));
}

TEST(PerceivedLevelTest, LevelToGainWithMaxLevel) {
  EXPECT_FLOAT_EQ(fuchsia_media_audio::kMutedGainDb, PerceivedLevel::LevelToGain(0, 10));
  EXPECT_FLOAT_EQ(fuchsia_media_audio::kMutedGainDb, PerceivedLevel::LevelToGain(-1, 10));
  EXPECT_FLOAT_EQ(-54.0f, PerceivedLevel::LevelToGain(1, 10));
  EXPECT_FLOAT_EQ(-30.0f, PerceivedLevel::LevelToGain(5, 10));
  EXPECT_FLOAT_EQ(0.0f, PerceivedLevel::LevelToGain(10, 10));
  EXPECT_FLOAT_EQ(0.0f, PerceivedLevel::LevelToGain(11, 10));
}

TEST(PerceivedLevelTest, RoundTrip) {
  for (int level = 0; level <= 10; ++level) {
    EXPECT_EQ(level, PerceivedLevel::GainToLevel(PerceivedLevel::LevelToGain(level, 10), 10));
  }
}

}  // namespace
}  // namespace media
