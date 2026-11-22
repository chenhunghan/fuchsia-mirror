// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/lib/effects_loader/effects_module.h"

#include <gtest/gtest.h>

#include "src/media/audio/lib/effects_loader/testing/effects_loader_v1_test_base.h"

namespace media::audio {
namespace {

TEST(EffectsModuleTest, OpenModule) {
  auto m = EffectsModuleV1::Open(testing::kTestEffectsModuleName);
  ASSERT_TRUE(m);
  ASSERT_EQ(0u, m->num_effects);
}

TEST(EffectsModuleTest, MoveModule) {
  auto m1 = EffectsModuleV1::Open(testing::kTestEffectsModuleName);
  ASSERT_TRUE(m1);

  auto m2 = std::move(m1);
  EXPECT_FALSE(m1);
  EXPECT_TRUE(m2);
}

TEST(EffectsModuleTest, MaxEffectsAndInvalidEffectIdBounds) {
  auto m = EffectsModuleV1::Open(testing::kTestEffectsModuleName);
  ASSERT_TRUE(m);
  auto ext = testing::TestEffectsV1Module::Open();
  ASSERT_EQ(0u, m->num_effects);

  // Creating an effect when num_effects == 0 must fail.
  EXPECT_EQ(FUCHSIA_AUDIO_EFFECTS_INVALID_HANDLE, m->create_effect(0, 48000, 2, 2, nullptr, 0));

  constexpr uint32_t kMaxEffects = 255;
  for (uint32_t i = 0; i < kMaxEffects; ++i) {
    ASSERT_EQ(ZX_OK, ext.AddEffect("effect").Build());
  }
  EXPECT_EQ(kMaxEffects, m->num_effects);

  // Adding one more effect beyond TEST_EFFECTS_MAX must return ZX_ERR_OUT_OF_RANGE.
  EXPECT_EQ(ZX_ERR_OUT_OF_RANGE, ext.AddEffect("overflow").Build());
  EXPECT_EQ(kMaxEffects, m->num_effects);

  // Creating an effect at the last valid index must succeed.
  fuchsia_audio_effects_handle_t handle =
      m->create_effect(kMaxEffects - 1, 48000, 2, 2, nullptr, 0);
  ASSERT_NE(FUCHSIA_AUDIO_EFFECTS_INVALID_HANDLE, handle);
  EXPECT_TRUE(m->delete_effect(handle));

  // Creating an effect at index kMaxEffects (one past the last valid index) must fail.
  EXPECT_EQ(FUCHSIA_AUDIO_EFFECTS_INVALID_HANDLE,
            m->create_effect(kMaxEffects, 48000, 2, 2, nullptr, 0));
}

}  // namespace
}  // namespace media::audio
