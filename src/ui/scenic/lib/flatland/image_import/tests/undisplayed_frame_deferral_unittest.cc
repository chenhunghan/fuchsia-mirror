// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/image_import/undisplayed_frame_deferral.h"

#include <gtest/gtest.h>

#include "src/lib/testing/loop_fixture/test_loop_fixture.h"

namespace flatland::test {

using UndisplayedFrameDeferralTest = gtest::TestLoopFixture;

TEST_F(UndisplayedFrameDeferralTest, BasicDeferralAndDisplay) {
  bool deadline_reached = false;
  UndisplayedFrameDeferral deferral(dispatcher(), [&] { deadline_reached = true; });

  zx::time_monotonic now = Now();
  zx::time_monotonic deadline = now + zx::sec(1);

  deferral.OnFrameCommitted({.value = 10}, deadline);
  EXPECT_TRUE(deferral.IsDeferred(Now()));

  EXPECT_FALSE(deferral.OnStampDisplayed(
      {.value = fuchsia_hardware_display::wire::kInvalidConfigStampValue}));
  EXPECT_TRUE(deferral.IsDeferred(Now()));

  EXPECT_FALSE(deferral.OnStampDisplayed({.value = 9}));
  EXPECT_TRUE(deferral.IsDeferred(Now()));

  EXPECT_TRUE(deferral.OnStampDisplayed({.value = 10}));
  EXPECT_FALSE(deferral.IsDeferred(Now()));

  EXPECT_FALSE(deferral.OnStampDisplayed({.value = 11}));
  EXPECT_FALSE(deferral.OnStampDisplayed(
      {.value = fuchsia_hardware_display::wire::kInvalidConfigStampValue}));
}

TEST_F(UndisplayedFrameDeferralTest, FrameSkipped) {
  UndisplayedFrameDeferral deferral(dispatcher(), [] {});

  deferral.OnFrameCommitted({.value = 10}, Now() + zx::sec(1));
  EXPECT_TRUE(deferral.IsDeferred(Now()));

  deferral.OnFrameSkipped();
  EXPECT_FALSE(deferral.IsDeferred(Now()));
}

TEST_F(UndisplayedFrameDeferralTest, DeadlineExpired) {
  bool deadline_reached = false;
  UndisplayedFrameDeferral deferral(dispatcher(), [&] { deadline_reached = true; });

  zx::time_monotonic deadline = Now() + zx::sec(1);
  deferral.OnFrameCommitted({.value = 10}, deadline);
  EXPECT_TRUE(deferral.IsDeferred(Now()));

  RunLoopUntil(deadline);
  EXPECT_FALSE(deferral.IsDeferred(Now()));
  // IsDeferred clears it lazily, it does not invoke the timeout handler.
  EXPECT_FALSE(deadline_reached);
}

TEST_F(UndisplayedFrameDeferralTest, TimeoutHandler) {
  int timeout_count = 0;
  UndisplayedFrameDeferral deferral(dispatcher(), [&] { timeout_count++; });

  zx::time_monotonic deadline = Now() + zx::sec(1);
  deferral.OnFrameCommitted({.value = 10}, deadline);

  deferral.ArmTimeout();
  deferral.ArmTimeout();  // Calling twice should only arm once.

  RunLoopUntil(deadline);
  EXPECT_EQ(timeout_count, 1);
  EXPECT_FALSE(deferral.IsDeferred(Now()));
}

TEST_F(UndisplayedFrameDeferralTest, ReplaceFrame) {
  int timeout_count = 0;
  UndisplayedFrameDeferral deferral(dispatcher(), [&] { timeout_count++; });

  zx::time_monotonic deadline1 = Now() + zx::sec(1);
  deferral.OnFrameCommitted({.value = 10}, deadline1);
  deferral.ArmTimeout();

  zx::time_monotonic deadline2 = Now() + zx::sec(2);
  deferral.OnFrameCommitted({.value = 20}, deadline2);

  // Stamp 10 no longer ends it.
  EXPECT_FALSE(deferral.OnStampDisplayed({.value = 10}));
  EXPECT_TRUE(deferral.IsDeferred(Now()));

  RunLoopUntil(deadline1);
  // Old timeout was cancelled.
  EXPECT_EQ(timeout_count, 0);
  EXPECT_TRUE(deferral.IsDeferred(Now()));
}

TEST_F(UndisplayedFrameDeferralTest, Stop) {
  int timeout_count = 0;
  UndisplayedFrameDeferral deferral(dispatcher(), [&] { timeout_count++; });

  zx::time_monotonic deadline = Now() + zx::sec(1);
  deferral.OnFrameCommitted({.value = 10}, deadline);
  deferral.ArmTimeout();

  deferral.Stop();
  EXPECT_FALSE(deferral.IsDeferred(Now()));

  RunLoopUntil(deadline);
  EXPECT_EQ(timeout_count, 0);
}

}  // namespace flatland::test
