// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/coordinator/added-display-info.h"

#include <fidl/fuchsia.hardware.display.engine/cpp/driver/wire.h>
#include <fidl/fuchsia.hardware.display.types/cpp/wire.h>
#include <fidl/fuchsia.images2/cpp/wire.h>
#include <lib/driver/testing/cpp/scoped_global_logger.h>
#include <lib/zx/result.h>
#include <zircon/errors.h>

#include <array>
#include <memory>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/mode.h"
#include "src/graphics/display/lib/api-types/cpp/pixel-format.h"
#include "src/lib/testing/predicates/status.h"

namespace display_coordinator {

namespace {

class AddedDisplayInfoTest : public ::testing::Test {
 private:
  fdf_testing::ScopedGlobalLogger logger_;
};

TEST_F(AddedDisplayInfoTest, CreateFromFidlTranscribesDisplayId) {
  static constexpr display::DisplayId kDisplayId(42);
  static constexpr std::array<display::Mode, 1> kModes = {
      display::Mode({
          .active_width = 1024,
          .active_height = 768,
          .refresh_rate_millihertz = 60'000,
      }),
  };
  static constexpr std::array<display::PixelFormat, 1> kPixelFormats = {
      display::PixelFormat::kR8G8B8A8};

  std::array<fuchsia_hardware_display_types::wire::Mode, 1> fidl_modes = {kModes[0].ToFidl()};
  std::array<fuchsia_images2::wire::PixelFormat, 1> fidl_pixel_formats = {
      kPixelFormats[0].ToFidl()};

  const fuchsia_hardware_display_engine::wire::RawDisplayInfo fidl_display_info = {
      .display_id = kDisplayId.ToFidl(),
      .preferred_modes =
          fidl::VectorView<fuchsia_hardware_display_types::wire::Mode>::FromExternal(fidl_modes),
      .pixel_formats =
          fidl::VectorView<fuchsia_images2::wire::PixelFormat>::FromExternal(fidl_pixel_formats),
  };

  zx::result<std::unique_ptr<AddedDisplayInfo>> added_display_info_result =
      AddedDisplayInfo::Create(fidl_display_info);
  ASSERT_OK(added_display_info_result);
  std::unique_ptr<AddedDisplayInfo> added_display_info =
      std::move(added_display_info_result).value();

  EXPECT_EQ(kDisplayId, added_display_info->display_id);
  EXPECT_THAT(added_display_info->preferred_modes, ::testing::SizeIs(1));
  EXPECT_THAT(added_display_info->pixel_formats, ::testing::SizeIs(1));
}

TEST_F(AddedDisplayInfoTest, CreateFromFidlTranscribesPixelFormats) {
  static constexpr display::DisplayId kDisplayId(42);
  static constexpr std::array<display::Mode, 1> kModes = {
      display::Mode({
          .active_width = 1024,
          .active_height = 768,
          .refresh_rate_millihertz = 60'000,
      }),
  };
  static constexpr std::array<display::PixelFormat, 2> kPixelFormats = {
      display::PixelFormat::kR8G8B8A8, display::PixelFormat::kB8G8R8A8};

  std::array<fuchsia_hardware_display_types::wire::Mode, 1> fidl_modes = {kModes[0].ToFidl()};
  std::array<fuchsia_images2::wire::PixelFormat, 2> fidl_pixel_formats = {
      kPixelFormats[0].ToFidl(), kPixelFormats[1].ToFidl()};

  const fuchsia_hardware_display_engine::wire::RawDisplayInfo fidl_display_info = {
      .display_id = kDisplayId.ToFidl(),
      .preferred_modes =
          fidl::VectorView<fuchsia_hardware_display_types::wire::Mode>::FromExternal(fidl_modes),
      .pixel_formats =
          fidl::VectorView<fuchsia_images2::wire::PixelFormat>::FromExternal(fidl_pixel_formats),
  };

  zx::result<std::unique_ptr<AddedDisplayInfo>> added_display_info_result =
      AddedDisplayInfo::Create(fidl_display_info);
  ASSERT_OK(added_display_info_result);
  std::unique_ptr<AddedDisplayInfo> added_display_info =
      std::move(added_display_info_result).value();

  EXPECT_THAT(added_display_info->pixel_formats, ::testing::ElementsAreArray(kPixelFormats));

  EXPECT_EQ(kDisplayId, added_display_info->display_id);
  EXPECT_THAT(added_display_info->preferred_modes, ::testing::SizeIs(1));
}

TEST_F(AddedDisplayInfoTest, CreateFromFidlCopiesDisplayModes) {
  static constexpr display::DisplayId kDisplayId(42);
  static constexpr std::array<display::PixelFormat, 1> kPixelFormats = {
      display::PixelFormat::kR8G8B8A8};

  std::array<fuchsia_images2::wire::PixelFormat, 1> fidl_pixel_formats = {
      kPixelFormats[0].ToFidl()};
  std::array<fuchsia_hardware_display_types::wire::Mode, 2> fidl_display_modes = {{
      {
          .active_area = {.width = 640, .height = 480},
          .refresh_rate_millihertz = 60'000,
      },
      {
          .active_area = {.width = 1024, .height = 768},
          .refresh_rate_millihertz = 75'000,
      },
  }};

  const fuchsia_hardware_display_engine::wire::RawDisplayInfo fidl_display_info = {
      .display_id = kDisplayId.ToFidl(),
      .preferred_modes = fidl::VectorView<fuchsia_hardware_display_types::wire::Mode>::FromExternal(
          fidl_display_modes),
      .pixel_formats =
          fidl::VectorView<fuchsia_images2::wire::PixelFormat>::FromExternal(fidl_pixel_formats),
  };

  zx::result<std::unique_ptr<AddedDisplayInfo>> added_display_info_result =
      AddedDisplayInfo::Create(fidl_display_info);
  ASSERT_OK(added_display_info_result);
  std::unique_ptr<AddedDisplayInfo> added_display_info =
      std::move(added_display_info_result).value();

  ASSERT_THAT(added_display_info->preferred_modes, ::testing::SizeIs(2));

  EXPECT_EQ(640, added_display_info->preferred_modes[0].active_area().width());
  EXPECT_EQ(480, added_display_info->preferred_modes[0].active_area().height());
  EXPECT_EQ(60'000, added_display_info->preferred_modes[0].refresh_rate_millihertz());

  EXPECT_EQ(1024, added_display_info->preferred_modes[1].active_area().width());
  EXPECT_EQ(768, added_display_info->preferred_modes[1].active_area().height());
  EXPECT_EQ(75'000, added_display_info->preferred_modes[1].refresh_rate_millihertz());

  EXPECT_EQ(kDisplayId, added_display_info->display_id);
  EXPECT_THAT(added_display_info->pixel_formats, ::testing::SizeIs(1));
}

TEST_F(AddedDisplayInfoTest, CreateFromFidlKeepsModeFlags) {
  static constexpr display::DisplayId kDisplayId(42);
  static constexpr std::array<display::PixelFormat, 1> kPixelFormats = {
      display::PixelFormat::kR8G8B8A8};

  std::array<fuchsia_images2::wire::PixelFormat, 1> fidl_pixel_formats = {
      kPixelFormats[0].ToFidl()};
  std::array<fuchsia_hardware_display_types::wire::Mode, 2> fidl_display_modes = {{
      {
          .active_area = {.width = 1280, .height = 2856},
          .refresh_rate_millihertz = 117'794,
          .flags = fuchsia_hardware_display_types::wire::ModeFlags::kVariableRefreshRate,
      },
      {
          .active_area = {.width = 1280, .height = 2856},
          .refresh_rate_millihertz = 117'794,
          .flags = fuchsia_hardware_display_types::wire::ModeFlags(),
      },
  }};

  const fuchsia_hardware_display_engine::wire::RawDisplayInfo fidl_display_info = {
      .display_id = kDisplayId.ToFidl(),
      .preferred_modes = fidl::VectorView<fuchsia_hardware_display_types::wire::Mode>::FromExternal(
          fidl_display_modes),
      .pixel_formats =
          fidl::VectorView<fuchsia_images2::wire::PixelFormat>::FromExternal(fidl_pixel_formats),
  };

  zx::result<std::unique_ptr<AddedDisplayInfo>> added_display_info_result =
      AddedDisplayInfo::Create(fidl_display_info);
  ASSERT_OK(added_display_info_result);
  std::unique_ptr<AddedDisplayInfo> added_display_info =
      std::move(added_display_info_result).value();

  ASSERT_THAT(added_display_info->preferred_modes, ::testing::SizeIs(2));

  EXPECT_EQ(117'794, added_display_info->preferred_modes[0].refresh_rate_millihertz());
  EXPECT_EQ(fuchsia_hardware_display_types::wire::ModeFlags::kVariableRefreshRate,
            added_display_info->preferred_modes[0].flags());
  EXPECT_TRUE(added_display_info->preferred_modes[0].is_variable_refresh_rate());

  EXPECT_EQ(117'794, added_display_info->preferred_modes[1].refresh_rate_millihertz());
  EXPECT_EQ(fuchsia_hardware_display_types::wire::ModeFlags(),
            added_display_info->preferred_modes[1].flags());
  EXPECT_FALSE(added_display_info->preferred_modes[1].is_variable_refresh_rate());

  EXPECT_NE(added_display_info->preferred_modes[0], added_display_info->preferred_modes[1]);
}

TEST_F(AddedDisplayInfoTest, CreateFromFidlRejectsUnknownModeFlags) {
  static constexpr display::DisplayId kDisplayId(42);
  static constexpr std::array<display::PixelFormat, 1> kPixelFormats = {
      display::PixelFormat::kR8G8B8A8};

  std::array<fuchsia_images2::wire::PixelFormat, 1> fidl_pixel_formats = {
      kPixelFormats[0].ToFidl()};
  std::array<fuchsia_hardware_display_types::wire::Mode, 1> fidl_display_modes = {{
      {
          .active_area = {.width = 1280, .height = 2856},
          .refresh_rate_millihertz = 117'794,
          .flags = static_cast<fuchsia_hardware_display_types::wire::ModeFlags>(0x2),
      },
  }};

  const fuchsia_hardware_display_engine::wire::RawDisplayInfo fidl_display_info = {
      .display_id = kDisplayId.ToFidl(),
      .preferred_modes = fidl::VectorView<fuchsia_hardware_display_types::wire::Mode>::FromExternal(
          fidl_display_modes),
      .pixel_formats =
          fidl::VectorView<fuchsia_images2::wire::PixelFormat>::FromExternal(fidl_pixel_formats),
  };

  zx::result<std::unique_ptr<AddedDisplayInfo>> added_display_info_result =
      AddedDisplayInfo::Create(fidl_display_info);
  EXPECT_STATUS(added_display_info_result, ZX_ERR_INVALID_ARGS);
}

TEST_F(AddedDisplayInfoTest, CreateFromFidlCopiesVrrConfigs) {
  static constexpr display::DisplayId kDisplayId(42);
  static constexpr std::array<display::PixelFormat, 1> kPixelFormats = {
      display::PixelFormat::kR8G8B8A8};

  std::array<fuchsia_images2::wire::PixelFormat, 1> fidl_pixel_formats = {
      kPixelFormats[0].ToFidl()};
  std::array<fuchsia_hardware_display_types::wire::Mode, 2> fidl_display_modes = {{
      {
          .active_area = {.width = 1080, .height = 2410},
          .refresh_rate_millihertz = 120'000,
          .flags = fuchsia_hardware_display_types::wire::ModeFlags::kVariableRefreshRate,
      },
      {
          .active_area = {.width = 1080, .height = 2410},
          .refresh_rate_millihertz = 60'000,
          .flags = fuchsia_hardware_display_types::wire::ModeFlags(),
      },
  }};

  fidl::Arena arena;
  std::array<fuchsia_hardware_display_types::wire::FrameIntervalPowerHint, 1> power_hints = {{
      {
          .frame_interval = 16'666'666,
          .average_refresh_period = 8'333'333,
      },
  }};
  std::array<fuchsia_hardware_display_types::wire::VrrConfig, 2> fidl_vrr_configs = {
      fuchsia_hardware_display_types::wire::VrrConfig::Builder(arena)
          .vsync_period(4'166'666)
          .min_frame_interval(8'333'333)
          .frame_interval_power_hints(
              fidl::VectorView<fuchsia_hardware_display_types::wire::FrameIntervalPowerHint>::
                  FromExternal(power_hints))
          .notify_expected_present_config({
              .heads_up = 30'000'000,
              .timeout = 500'000'000,
          })
          .Build(),
      fuchsia_hardware_display_types::wire::VrrConfig::Builder(arena).Build(),
  };

  const fuchsia_hardware_display_engine::wire::RawDisplayInfo fidl_display_info = {
      .display_id = kDisplayId.ToFidl(),
      .preferred_modes = fidl::VectorView<fuchsia_hardware_display_types::wire::Mode>::FromExternal(
          fidl_display_modes),
      .preferred_mode_vrr_configs =
          fidl::VectorView<fuchsia_hardware_display_types::wire::VrrConfig>::FromExternal(
              fidl_vrr_configs),
      .pixel_formats =
          fidl::VectorView<fuchsia_images2::wire::PixelFormat>::FromExternal(fidl_pixel_formats),
  };

  zx::result<std::unique_ptr<AddedDisplayInfo>> added_display_info_result =
      AddedDisplayInfo::Create(fidl_display_info);
  ASSERT_OK(added_display_info_result);
  std::unique_ptr<AddedDisplayInfo> added_display_info =
      std::move(added_display_info_result).value();

  ASSERT_THAT(added_display_info->preferred_mode_vrr_configs, ::testing::SizeIs(2));
  EXPECT_EQ(added_display_info->preferred_mode_vrr_configs[0].vsync_period(), 4'166'666);
  EXPECT_EQ(added_display_info->preferred_mode_vrr_configs[0].min_frame_interval(), 8'333'333);
  ASSERT_TRUE(
      added_display_info->preferred_mode_vrr_configs[0].frame_interval_power_hints().has_value());
  ASSERT_EQ(added_display_info->preferred_mode_vrr_configs[0].frame_interval_power_hints()->size(),
            1u);
  EXPECT_EQ(added_display_info->preferred_mode_vrr_configs[0]
                .frame_interval_power_hints()
                ->at(0)
                .frame_interval(),
            16'666'666);
  ASSERT_TRUE(added_display_info->preferred_mode_vrr_configs[0]
                  .notify_expected_present_config()
                  .has_value());
  EXPECT_EQ(added_display_info->preferred_mode_vrr_configs[0]
                .notify_expected_present_config()
                ->heads_up(),
            30'000'000);
  EXPECT_EQ(
      added_display_info->preferred_mode_vrr_configs[0].notify_expected_present_config()->timeout(),
      500'000'000);
  EXPECT_TRUE(added_display_info->preferred_mode_vrr_configs[1].IsEmpty());
}

TEST_F(AddedDisplayInfoTest, CreateFromFidlRejectsVrrConfigOnFixedRefreshMode) {
  static constexpr display::DisplayId kDisplayId(42);
  static constexpr std::array<display::PixelFormat, 1> kPixelFormats = {
      display::PixelFormat::kR8G8B8A8};

  std::array<fuchsia_images2::wire::PixelFormat, 1> fidl_pixel_formats = {
      kPixelFormats[0].ToFidl()};
  std::array<fuchsia_hardware_display_types::wire::Mode, 1> fidl_display_modes = {{
      {
          .active_area = {.width = 1080, .height = 2410},
          .refresh_rate_millihertz = 60'000,
          .flags = fuchsia_hardware_display_types::wire::ModeFlags(),
      },
  }};

  fidl::Arena arena;
  std::array<fuchsia_hardware_display_types::wire::VrrConfig, 1> fidl_vrr_configs = {
      fuchsia_hardware_display_types::wire::VrrConfig::Builder(arena)
          .vsync_period(16'666'666)
          .Build(),
  };

  const fuchsia_hardware_display_engine::wire::RawDisplayInfo fidl_display_info = {
      .display_id = kDisplayId.ToFidl(),
      .preferred_modes = fidl::VectorView<fuchsia_hardware_display_types::wire::Mode>::FromExternal(
          fidl_display_modes),
      .preferred_mode_vrr_configs =
          fidl::VectorView<fuchsia_hardware_display_types::wire::VrrConfig>::FromExternal(
              fidl_vrr_configs),
      .pixel_formats =
          fidl::VectorView<fuchsia_images2::wire::PixelFormat>::FromExternal(fidl_pixel_formats),
  };

  EXPECT_STATUS(AddedDisplayInfo::Create(fidl_display_info), ZX_ERR_INVALID_ARGS);
}

TEST_F(AddedDisplayInfoTest, CreateFromFidlRejectsMismatchedVrrConfigsCount) {
  static constexpr display::DisplayId kDisplayId(42);
  static constexpr std::array<display::PixelFormat, 1> kPixelFormats = {
      display::PixelFormat::kR8G8B8A8};

  std::array<fuchsia_images2::wire::PixelFormat, 1> fidl_pixel_formats = {
      kPixelFormats[0].ToFidl()};
  std::array<fuchsia_hardware_display_types::wire::Mode, 2> fidl_display_modes = {{
      {
          .active_area = {.width = 1080, .height = 2410},
          .refresh_rate_millihertz = 120'000,
          .flags = fuchsia_hardware_display_types::wire::ModeFlags::kVariableRefreshRate,
      },
      {
          .active_area = {.width = 1080, .height = 2410},
          .refresh_rate_millihertz = 60'000,
      },
  }};

  fidl::Arena arena;
  std::array<fuchsia_hardware_display_types::wire::VrrConfig, 1> fidl_vrr_configs = {
      fuchsia_hardware_display_types::wire::VrrConfig::Builder(arena).Build(),
  };

  const fuchsia_hardware_display_engine::wire::RawDisplayInfo fidl_display_info = {
      .display_id = kDisplayId.ToFidl(),
      .preferred_modes = fidl::VectorView<fuchsia_hardware_display_types::wire::Mode>::FromExternal(
          fidl_display_modes),
      .preferred_mode_vrr_configs =
          fidl::VectorView<fuchsia_hardware_display_types::wire::VrrConfig>::FromExternal(
              fidl_vrr_configs),
      .pixel_formats =
          fidl::VectorView<fuchsia_images2::wire::PixelFormat>::FromExternal(fidl_pixel_formats),
  };

  EXPECT_STATUS(AddedDisplayInfo::Create(fidl_display_info), ZX_ERR_INVALID_ARGS);
}

TEST_F(AddedDisplayInfoTest, CreateFromFidlRejectsInvalidPowerHints) {
  static constexpr display::DisplayId kDisplayId(42);
  static constexpr std::array<display::PixelFormat, 1> kPixelFormats = {
      display::PixelFormat::kR8G8B8A8};

  std::array<fuchsia_images2::wire::PixelFormat, 1> fidl_pixel_formats = {
      kPixelFormats[0].ToFidl()};
  std::array<fuchsia_hardware_display_types::wire::Mode, 1> fidl_display_modes = {{
      {
          .active_area = {.width = 1080, .height = 2410},
          .refresh_rate_millihertz = 120'000,
          .flags = fuchsia_hardware_display_types::wire::ModeFlags::kVariableRefreshRate,
      },
  }};

  const auto make_raw_info =
      [&](fidl::ArenaBase& arena,
          std::span<fuchsia_hardware_display_types::wire::FrameIntervalPowerHint> hints,
          std::array<fuchsia_hardware_display_types::wire::VrrConfig, 1>& out_configs) {
        out_configs[0] =
            fuchsia_hardware_display_types::wire::VrrConfig::Builder(arena)
                .vsync_period(4'166'666)
                .min_frame_interval(8'333'333)
                .frame_interval_power_hints(
                    fidl::VectorView<fuchsia_hardware_display_types::wire::FrameIntervalPowerHint>::
                        FromExternal(hints.data(), hints.size()))
                .Build();
        return fuchsia_hardware_display_engine::wire::RawDisplayInfo{
            .display_id = kDisplayId.ToFidl(),
            .preferred_modes =
                fidl::VectorView<fuchsia_hardware_display_types::wire::Mode>::FromExternal(
                    fidl_display_modes),
            .preferred_mode_vrr_configs =
                fidl::VectorView<fuchsia_hardware_display_types::wire::VrrConfig>::FromExternal(
                    out_configs),
            .pixel_formats = fidl::VectorView<fuchsia_images2::wire::PixelFormat>::FromExternal(
                fidl_pixel_formats),
        };
      };

  fidl::Arena arena;
  std::array<fuchsia_hardware_display_types::wire::VrrConfig, 1> vrr_configs;

  // `average_refresh_period == frame_interval` (no self-refresh overhead) is invalid.
  std::array<fuchsia_hardware_display_types::wire::FrameIntervalPowerHint, 1> equal_period = {{{
      .frame_interval = 16'666'666,
      .average_refresh_period = 16'666'666,
  }}};
  EXPECT_STATUS(AddedDisplayInfo::Create(make_raw_info(arena, equal_period, vrr_configs)),
                ZX_ERR_INVALID_ARGS);

  // `frame_interval < min_frame_interval` is invalid.
  std::array<fuchsia_hardware_display_types::wire::FrameIntervalPowerHint, 1> below_min = {{{
      .frame_interval = 4'166'666,
      .average_refresh_period = 2'083'333,
  }}};
  EXPECT_STATUS(AddedDisplayInfo::Create(make_raw_info(arena, below_min, vrr_configs)),
                ZX_ERR_INVALID_ARGS);

  // Non-ascending `frame_interval` order is invalid.
  std::array<fuchsia_hardware_display_types::wire::FrameIntervalPowerHint, 2> unsorted = {{
      {.frame_interval = 33'333'333, .average_refresh_period = 16'666'666},
      {.frame_interval = 16'666'666, .average_refresh_period = 8'333'333},
  }};
  EXPECT_STATUS(AddedDisplayInfo::Create(make_raw_info(arena, unsorted, vrr_configs)),
                ZX_ERR_INVALID_ARGS);
}

}  // namespace

}  // namespace display_coordinator
