// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/lib/api-types/cpp/mode.h"

#include <fidl/fuchsia.hardware.display.types/cpp/wire.h>

#include <gtest/gtest.h>

namespace display {

namespace {

constexpr Mode kVga60Fps({
    .active_width = 640,
    .active_height = 480,
    .refresh_rate_millihertz = 60'000,
});

constexpr Mode kVga60Fps2({
    .active_width = 640,
    .active_height = 480,
    .refresh_rate_millihertz = 60'000,
});

constexpr Mode kQvga30Fps({
    .active_width = 240,
    .active_height = 320,
    .refresh_rate_millihertz = 30'000,
});

constexpr Mode kPhoneVariable118Fps({
    .active_width = 1280,
    .active_height = 2856,
    .refresh_rate_millihertz = 117'794,
    .flags = fuchsia_hardware_display_types::wire::ModeFlags::kVariableRefreshRate,
});

TEST(ModeTest, EqualityIsReflexive) {
  EXPECT_EQ(kVga60Fps, kVga60Fps);
  EXPECT_EQ(kVga60Fps2, kVga60Fps2);
  EXPECT_EQ(kQvga30Fps, kQvga30Fps);
  EXPECT_EQ(kPhoneVariable118Fps, kPhoneVariable118Fps);
}

TEST(ModeTest, EqualityIsSymmetric) {
  EXPECT_EQ(kVga60Fps, kVga60Fps2);
  EXPECT_EQ(kVga60Fps2, kVga60Fps);
}

TEST(ModeTest, EqualityForDifferentWidths) {
  static constexpr Mode kSmallSquare30Fps({
      .active_width = 240,
      .active_height = 240,
      .refresh_rate_millihertz = 30'000,
  });
  EXPECT_NE(kQvga30Fps, kSmallSquare30Fps);
  EXPECT_NE(kSmallSquare30Fps, kQvga30Fps);
}

TEST(ModeTest, EqualityForDifferentHeights) {
  static constexpr Mode kLargeSquare30Fps({
      .active_width = 320,
      .active_height = 320,
      .refresh_rate_millihertz = 30'000,
  });
  EXPECT_NE(kQvga30Fps, kLargeSquare30Fps);
  EXPECT_NE(kLargeSquare30Fps, kQvga30Fps);
}

TEST(ModeTest, EqualityForDifferentRefreshRates) {
  static constexpr Mode kQvga60Fps({
      .active_width = 240,
      .active_height = 320,
      .refresh_rate_millihertz = 60'000,
  });
  EXPECT_NE(kQvga30Fps, kQvga60Fps);
  EXPECT_NE(kQvga60Fps, kQvga30Fps);
}

TEST(ModeTest, EqualityForDifferentFlags) {
  static constexpr Mode kPhoneFixed118Fps({
      .active_width = 1280,
      .active_height = 2856,
      .refresh_rate_millihertz = 117'794,
  });
  EXPECT_NE(kPhoneVariable118Fps, kPhoneFixed118Fps);
  EXPECT_NE(kPhoneFixed118Fps, kPhoneVariable118Fps);
}

TEST(ModeTest, FromDesignatedInitializer) {
  static constexpr Mode mode({
      .active_width = 640,
      .active_height = 480,
      .refresh_rate_millihertz = 60'000,
  });
  EXPECT_EQ(640, mode.active_area().width());
  EXPECT_EQ(480, mode.active_area().height());
  EXPECT_EQ(Dimensions({.width = 640, .height = 480}), mode.active_area());
  EXPECT_EQ(60'000, mode.refresh_rate_millihertz());
  EXPECT_EQ(fuchsia_hardware_display_types::wire::ModeFlags(), mode.flags());
  EXPECT_FALSE(mode.is_variable_refresh_rate());
}

TEST(ModeTest, FromDesignatedInitializerWithFlags) {
  EXPECT_EQ(1280, kPhoneVariable118Fps.active_area().width());
  EXPECT_EQ(2856, kPhoneVariable118Fps.active_area().height());
  EXPECT_EQ(117'794, kPhoneVariable118Fps.refresh_rate_millihertz());
  EXPECT_EQ(fuchsia_hardware_display_types::wire::ModeFlags::kVariableRefreshRate,
            kPhoneVariable118Fps.flags());
  EXPECT_TRUE(kPhoneVariable118Fps.is_variable_refresh_rate());
}

TEST(ModeTest, FromFidlMode) {
  static constexpr fuchsia_hardware_display_types::wire::Mode fidl_mode = {
      .active_area = {.width = 640, .height = 480},
      .refresh_rate_millihertz = 60'000,
  };

  static constexpr Mode mode = Mode::From(fidl_mode);
  EXPECT_EQ(640, mode.active_area().width());
  EXPECT_EQ(480, mode.active_area().height());
  EXPECT_EQ(Dimensions({.width = 640, .height = 480}), mode.active_area());
  EXPECT_EQ(60'000, mode.refresh_rate_millihertz());
  EXPECT_EQ(fuchsia_hardware_display_types::wire::ModeFlags(), mode.flags());
  EXPECT_FALSE(mode.is_variable_refresh_rate());
}

TEST(ModeTest, FromFidlModeWithFlags) {
  static constexpr fuchsia_hardware_display_types::wire::Mode fidl_mode = {
      .active_area = {.width = 1280, .height = 2856},
      .refresh_rate_millihertz = 117'794,
      .flags = fuchsia_hardware_display_types::wire::ModeFlags::kVariableRefreshRate,
  };

  static constexpr Mode mode = Mode::From(fidl_mode);
  EXPECT_EQ(1280, mode.active_area().width());
  EXPECT_EQ(2856, mode.active_area().height());
  EXPECT_EQ(117'794, mode.refresh_rate_millihertz());
  EXPECT_EQ(fuchsia_hardware_display_types::wire::ModeFlags::kVariableRefreshRate, mode.flags());
  EXPECT_TRUE(mode.is_variable_refresh_rate());
}

TEST(ModeTest, ToFidlMode) {
  static constexpr Mode mode({
      .active_width = 640,
      .active_height = 480,
      .refresh_rate_millihertz = 60'000,
  });

  static constexpr fuchsia_hardware_display_types::wire::Mode fidl_mode = mode.ToFidl();
  EXPECT_EQ(640u, fidl_mode.active_area.width);
  EXPECT_EQ(480u, fidl_mode.active_area.height);
  EXPECT_EQ(60'000u, fidl_mode.refresh_rate_millihertz);
  EXPECT_EQ(fuchsia_hardware_display_types::wire::ModeFlags(), fidl_mode.flags);

  static constexpr fuchsia_hardware_display_types::wire::Mode variable_fidl_mode =
      kPhoneVariable118Fps.ToFidl();
  EXPECT_EQ(1280u, variable_fidl_mode.active_area.width);
  EXPECT_EQ(2856u, variable_fidl_mode.active_area.height);
  EXPECT_EQ(117'794u, variable_fidl_mode.refresh_rate_millihertz);
  EXPECT_EQ(fuchsia_hardware_display_types::wire::ModeFlags::kVariableRefreshRate,
            variable_fidl_mode.flags);
}

TEST(ModeTest, IsValidFidlVga60Fps) {
  EXPECT_TRUE(Mode::IsValid(fuchsia_hardware_display_types::wire::Mode{
      .active_area = {.width = 640, .height = 480},
      .refresh_rate_millihertz = 60'000,
      .flags = fuchsia_hardware_display_types::wire::ModeFlags(),
  }));
}

TEST(ModeTest, IsValidFidlLargeWidth) {
  EXPECT_FALSE(Mode::IsValid(fuchsia_hardware_display_types::wire::Mode{
      .active_area = {.width = 1'000'000, .height = 480},
      .refresh_rate_millihertz = 60'000,
      .flags = fuchsia_hardware_display_types::wire::ModeFlags(),
  }));
}

TEST(ModeTest, IsValidFidlLargeHeight) {
  EXPECT_FALSE(Mode::IsValid(fuchsia_hardware_display_types::wire::Mode{
      .active_area = {.width = 640, .height = 1'000'000},
      .refresh_rate_millihertz = 60'000,
      .flags = fuchsia_hardware_display_types::wire::ModeFlags(),
  }));
}

TEST(ModeTest, IsValidFidlLargeRefreshRate) {
  EXPECT_FALSE(Mode::IsValid(fuchsia_hardware_display_types::wire::Mode{
      .active_area = {.width = 640, .height = 480},
      .refresh_rate_millihertz = 10'000'000,
      .flags = fuchsia_hardware_display_types::wire::ModeFlags(),
  }));
}

TEST(ModeTest, IsValidFidlUnknownFlags) {
  EXPECT_FALSE(Mode::IsValid(fuchsia_hardware_display_types::wire::Mode{
      .active_area = {.width = 640, .height = 480},
      .refresh_rate_millihertz = 60'000,
      .flags = static_cast<fuchsia_hardware_display_types::wire::ModeFlags>(0x2),
  }));
}

TEST(ModeTest, IsValidFidlKnownAndUnknownFlags) {
  EXPECT_FALSE(Mode::IsValid(fuchsia_hardware_display_types::wire::Mode{
      .active_area = {.width = 640, .height = 480},
      .refresh_rate_millihertz = 60'000,
      .flags = static_cast<fuchsia_hardware_display_types::wire::ModeFlags>(0x3),
  }));
}

TEST(ModeTest, IsValidFidlVariableRefreshRate) {
  EXPECT_TRUE(Mode::IsValid(fuchsia_hardware_display_types::wire::Mode{
      .active_area = {.width = 640, .height = 480},
      .refresh_rate_millihertz = 60'000,
      .flags = fuchsia_hardware_display_types::wire::ModeFlags::kVariableRefreshRate,
  }));
}

TEST(ModeTest, Format) {
#if __cplusplus >= 202002L
  EXPECT_EQ(std::format("{}", kVga60Fps), "640x480 @ 60.000 Hz");
  EXPECT_EQ(std::format("{}", kQvga30Fps), "240x320 @ 30.000 Hz");
  EXPECT_EQ(std::format("{}", kPhoneVariable118Fps),
            "1280x2856 @ 117.794 Hz (variable refresh rate)");
#endif
}

}  // namespace

}  // namespace display
