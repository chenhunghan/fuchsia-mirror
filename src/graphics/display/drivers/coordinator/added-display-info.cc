// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/coordinator/added-display-info.h"

#include <fidl/fuchsia.hardware.display.engine/cpp/driver/wire.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/zx/result.h>

#include <cstdint>
#include <span>
#include <utility>

#include <fbl/alloc_checker.h>
#include <fbl/vector.h>

#include "src/graphics/display/lib/api-types/cpp/display-id.h"
#include "src/graphics/display/lib/api-types/cpp/pixel-format.h"

namespace display_coordinator {

namespace {

constexpr int64_t kNanosecondsPerMillihertzPeriod = 1'000'000'000'000;

zx::result<fuchsia_hardware_display_types::VrrConfig> ParsePreferredModeVrrConfig(
    const fuchsia_hardware_display_types::wire::VrrConfig& fidl_vrr_config,
    const display::Mode& mode, size_t mode_index) {
  fuchsia_hardware_display_types::VrrConfig vrr_config = fidl::ToNatural(fidl_vrr_config);
  if (!mode.is_variable_refresh_rate()) {
    if (!vrr_config.IsEmpty()) {
      fdf::error("AddedDisplayInfo creation failed: non-empty VrrConfig on fixed-refresh mode {}",
                 mode_index);
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    return zx::ok(std::move(vrr_config));
  }

  if (vrr_config.vsync_period().has_value() && *vrr_config.vsync_period() <= 0) {
    fdf::error("AddedDisplayInfo creation failed: non-positive vsync_period {} on mode {}",
               *vrr_config.vsync_period(), mode_index);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  const int64_t nominal_vsync_period_ns =
      kNanosecondsPerMillihertzPeriod / mode.refresh_rate_millihertz();
  const int64_t effective_vsync_period_ns =
      vrr_config.vsync_period().value_or(nominal_vsync_period_ns);

  if (vrr_config.min_frame_interval().has_value() &&
      (*vrr_config.min_frame_interval() <= 0 ||
       *vrr_config.min_frame_interval() < effective_vsync_period_ns)) {
    fdf::error(
        "AddedDisplayInfo creation failed: invalid min_frame_interval {} (< vsync_period {}) "
        "on mode {}",
        *vrr_config.min_frame_interval(), effective_vsync_period_ns, mode_index);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  const int64_t effective_min_frame_interval_ns =
      vrr_config.min_frame_interval().value_or(nominal_vsync_period_ns);

  if (vrr_config.frame_interval_power_hints().has_value()) {
    if (vrr_config.frame_interval_power_hints()->size() >
        fuchsia_hardware_display_types::kMaxCountFrameIntervalPowerHints) {
      fdf::error("AddedDisplayInfo creation failed: too many power hints on mode {}", mode_index);
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    int64_t prev_frame_interval_ns = 0;
    for (const auto& hint : *vrr_config.frame_interval_power_hints()) {
      if (hint.frame_interval() < effective_min_frame_interval_ns ||
          hint.frame_interval() <= prev_frame_interval_ns || hint.average_refresh_period() <= 0 ||
          hint.average_refresh_period() >= hint.frame_interval()) {
        fdf::error(
            "AddedDisplayInfo creation failed: invalid power hint (frame_interval={}, "
            "average_refresh_period={}) on mode {}",
            hint.frame_interval(), hint.average_refresh_period(), mode_index);
        return zx::error(ZX_ERR_INVALID_ARGS);
      }
      prev_frame_interval_ns = hint.frame_interval();
    }
  }

  if (vrr_config.notify_expected_present_config().has_value()) {
    const auto& notify_config = *vrr_config.notify_expected_present_config();
    if (notify_config.heads_up() <= 0 || notify_config.timeout() < 0) {
      fdf::error(
          "AddedDisplayInfo creation failed: invalid notify_expected_present_config on mode {}",
          mode_index);
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
  }

  return zx::ok(std::move(vrr_config));
}

zx::result<fbl::Vector<fuchsia_hardware_display_types::VrrConfig>> ParsePreferredModeVrrConfigs(
    std::span<const fuchsia_hardware_display_types::wire::VrrConfig> fidl_vrr_configs,
    const fbl::Vector<display::Mode>& preferred_modes) {
  ZX_DEBUG_ASSERT(fidl_vrr_configs.empty() || fidl_vrr_configs.size() == preferred_modes.size());

  fbl::Vector<fuchsia_hardware_display_types::VrrConfig> preferred_mode_vrr_configs;
  fbl::AllocChecker alloc_checker;
  if (fidl_vrr_configs.empty()) {
    preferred_mode_vrr_configs.resize(preferred_modes.size(), &alloc_checker);
    if (!alloc_checker.check()) {
      fdf::error("AddedDisplayInfo creation failed: out of memory allocating VRR configs");
      return zx::error(ZX_ERR_NO_MEMORY);
    }
    return zx::ok(std::move(preferred_mode_vrr_configs));
  }

  preferred_mode_vrr_configs.reserve(preferred_modes.size(), &alloc_checker);
  if (!alloc_checker.check()) {
    fdf::error("AddedDisplayInfo creation failed: out of memory allocating VRR configs");
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  for (size_t i = 0; i < preferred_modes.size(); ++i) {
    zx::result<fuchsia_hardware_display_types::VrrConfig> vrr_config_result =
        ParsePreferredModeVrrConfig(fidl_vrr_configs[i], preferred_modes[i], i);
    if (vrr_config_result.is_error()) {
      return vrr_config_result.take_error();
    }
    ZX_DEBUG_ASSERT_MSG(preferred_mode_vrr_configs.size() < preferred_modes.size(),
                        "The push_back() below was not supposed to allocate memory, but it might");
    preferred_mode_vrr_configs.push_back(std::move(vrr_config_result).value(), &alloc_checker);
    ZX_DEBUG_ASSERT_MSG(alloc_checker.check(),
                        "The push_back() above failed to allocate memory; "
                        "it was not supposed to allocate at all");
  }
  return zx::ok(std::move(preferred_mode_vrr_configs));
}

}  // namespace

// static
zx::result<std::unique_ptr<AddedDisplayInfo>> AddedDisplayInfo::Create(
    const fuchsia_hardware_display_engine::wire::RawDisplayInfo& fidl_display_info) {
  const display::DisplayId display_id(fidl_display_info.display_id);
  if (display_id == display::kInvalidDisplayId) {
    fdf::error("AddedDisplayInfo creation failed: invalid display ID");
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  if (fidl_display_info.pixel_formats.size() == 0) {
    fdf::error("AddedDisplayInfo creation failed: empty pixel formats list");
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  if (fidl_display_info.preferred_modes.empty()) {
    fdf::error("AddedDisplayInfo creation failed: empty preferred modes list");
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  if (!fidl_display_info.preferred_mode_vrr_configs.empty() &&
      fidl_display_info.preferred_mode_vrr_configs.size() !=
          fidl_display_info.preferred_modes.size()) {
    fdf::error(
        "AddedDisplayInfo creation failed: preferred_mode_vrr_configs size ({}) does not match "
        "preferred_modes size ({})",
        fidl_display_info.preferred_mode_vrr_configs.size(),
        fidl_display_info.preferred_modes.size());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  fbl::Vector<display::PixelFormat> pixel_formats;
  fbl::AllocChecker alloc_checker;
  pixel_formats.reserve(fidl_display_info.pixel_formats.size(), &alloc_checker);
  if (!alloc_checker.check()) {
    fdf::error("AddedDisplayInfo creation failed: out of memory allocating pixel formats");
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  for (const fuchsia_images2::wire::PixelFormat& fidl_pixel_format :
       fidl_display_info.pixel_formats) {
    if (!display::PixelFormat::IsSupported(fidl_pixel_format)) {
      fdf::error("AddedDisplayInfo creation failed: unsupported pixel format {}",
                 static_cast<uint32_t>(fidl_pixel_format));
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    display::PixelFormat pixel_format(fidl_pixel_format);

    ZX_DEBUG_ASSERT_MSG(pixel_formats.size() < fidl_display_info.pixel_formats.size(),
                        "The push_back() below was not supposed to allocate memory, but it might");
    pixel_formats.push_back(pixel_format, &alloc_checker);
    ZX_DEBUG_ASSERT_MSG(alloc_checker.check(),
                        "The push_back() above failed to allocate memory; "
                        "it was not supposed to allocate at all");
  }

  fbl::Vector<display::Mode> preferred_modes;
  preferred_modes.reserve(fidl_display_info.preferred_modes.size(), &alloc_checker);
  if (!alloc_checker.check()) {
    fdf::error("AddedDisplayInfo creation failed: out of memory allocating display modes");
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  for (const fuchsia_hardware_display_types::wire::Mode& fidl_preferred_mode :
       fidl_display_info.preferred_modes) {
    ZX_DEBUG_ASSERT_MSG(preferred_modes.size() < fidl_display_info.preferred_modes.size(),
                        "The push_back() below was not supposed to allocate memory, but it might");
    if (!display::Mode::IsValid(fidl_preferred_mode)) {
      fdf::error("AddedDisplayInfo creation failed: invalid preferred mode for display ID {}",
                 display_id.value());
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    preferred_modes.push_back(display::Mode::From(fidl_preferred_mode), &alloc_checker);
    ZX_DEBUG_ASSERT_MSG(alloc_checker.check(),
                        "The push_back() above failed to allocate memory; "
                        "it was not supposed to allocate at all");
  }

  zx::result<fbl::Vector<fuchsia_hardware_display_types::VrrConfig>>
      preferred_mode_vrr_configs_result = ParsePreferredModeVrrConfigs(
          fidl_display_info.preferred_mode_vrr_configs.get(), preferred_modes);
  if (preferred_mode_vrr_configs_result.is_error()) {
    return preferred_mode_vrr_configs_result.take_error();
  }
  fbl::Vector<fuchsia_hardware_display_types::VrrConfig> preferred_mode_vrr_configs =
      std::move(preferred_mode_vrr_configs_result).value();

  auto display_info = fbl::make_unique_checked<AddedDisplayInfo>(&alloc_checker);
  if (!alloc_checker.check()) {
    fdf::error("AddedDisplayInfo creation failed: out of memory allocating instance");
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  *display_info = {
      .display_id = display_id,
      .pixel_formats = std::move(pixel_formats),
      .preferred_modes = std::move(preferred_modes),
      .preferred_mode_vrr_configs = std::move(preferred_mode_vrr_configs),
  };
  return zx::ok(std::move(display_info));
}

}  // namespace display_coordinator
