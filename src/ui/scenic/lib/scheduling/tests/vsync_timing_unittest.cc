// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/scheduling/vsync_timing.h"

#include <gtest/gtest.h>

namespace scheduling::test {
namespace {

TEST(VsyncTimingTest, DefaultsTo60HzIntervalAndZeroTimestamp) {
  VsyncTiming timing;
  EXPECT_EQ(timing.last_vsync_time(), zx::time(0));
  EXPECT_EQ(timing.vsync_interval(), zx::nsec(16'666'667));
}

TEST(VsyncTimingTest, UpdatesIntervalImmediatelyWithoutSmoothingLag) {
  VsyncTiming timing;

  timing.set_vsync_interval(zx::nsec(8'489'396));
  EXPECT_EQ(timing.vsync_interval(), zx::nsec(8'489'396));

  timing.set_vsync_interval(zx::nsec(4'244'698));
  EXPECT_EQ(timing.vsync_interval(), zx::nsec(4'244'698));

  timing.set_last_vsync_time(zx::time(500'000'000));
  EXPECT_EQ(timing.last_vsync_time(), zx::time(500'000'000));
  EXPECT_EQ(timing.vsync_interval(), zx::nsec(4'244'698));
}

TEST(VsyncTimingTest, MedianFiltersEmpiricalMeasurementsOnFixedRateDisplays) {
  VsyncTiming timing;
  constexpr zx::duration kTrueOscillatorPeriod = zx::nsec(16'980'000);
  timing.set_vsync_interval(zx::nsec(16'666'666));

  timing.AddVsyncInterval(kTrueOscillatorPeriod);
  timing.AddVsyncInterval(kTrueOscillatorPeriod);
  timing.AddVsyncInterval(kTrueOscillatorPeriod);
  EXPECT_EQ(timing.vsync_interval(), kTrueOscillatorPeriod);

  // Single-frame IRQ delivery jitter (+3ms followed by -3ms) is filtered out by the 5-frame
  // median window.
  timing.AddVsyncInterval(kTrueOscillatorPeriod + zx::msec(3));
  timing.AddVsyncInterval(kTrueOscillatorPeriod - zx::msec(3));
  EXPECT_EQ(timing.vsync_interval(), kTrueOscillatorPeriod);
}

}  // namespace
}  // namespace scheduling::test
