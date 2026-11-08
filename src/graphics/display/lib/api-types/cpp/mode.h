// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_LIB_API_TYPES_CPP_MODE_H_
#define SRC_GRAPHICS_DISPLAY_LIB_API_TYPES_CPP_MODE_H_

#include <fidl/fuchsia.hardware.display.types/cpp/wire.h>
#include <zircon/assert.h>

#include <cstdint>

#include "src/graphics/display/lib/api-types/cpp/dimensions.h"

#if __cplusplus >= 202002L
#include <format>
#include <string_view>
#endif

namespace display {

// Equivalent to the FIDL type [`fuchsia.hardware.display.types/Mode`].
//
// Instances are guaranteed to represent display modes supported by the display
// stack.
//
// This is a value type. Instances can be stored in containers. Copying, moving
// and destruction are trivial.
class Mode {
 private:
  // Enables creating instances using the designated initializer syntax.
  struct ConstructorArgs;

 public:
  // The maximum refresh rate supported by the display stack.
  static constexpr int32_t kMaxRefreshRateMillihertz = 1'000'000;  // 1,000 Hz

  // True iff `fidl_mode` is convertible to a valid Mode.
  [[nodiscard]] static constexpr bool IsValid(
      const fuchsia_hardware_display_types::wire::Mode& fidl_mode);

  // `fidl_mode` must be convertible to a valid Mode.
  //
  // This is not a constructor to allow designated initializer syntax. Making
  // this a constructor would introduce ambiguity when designated initializer
  // syntax is used, because `fuchsia.hardware.display.types/Mode` has the same
  // field names as our supported designated initializer syntax.
  [[nodiscard]] static constexpr Mode From(
      const fuchsia_hardware_display_types::wire::Mode& fidl_mode);

  // Constructor that enables the designated initializer syntax with containers.
  //
  // NOLINTNEXTLINE(google-explicit-constructor)
  constexpr Mode(const Mode::ConstructorArgs& args);

  constexpr Mode(const Mode&) noexcept = default;
  constexpr Mode(Mode&&) noexcept = default;
  constexpr Mode& operator=(const Mode&) noexcept = default;
  constexpr Mode& operator=(Mode&&) noexcept = default;
  ~Mode() = default;

  friend constexpr bool operator==(const Mode& lhs, const Mode& rhs);
  friend constexpr bool operator!=(const Mode& lhs, const Mode& rhs);

  constexpr fuchsia_hardware_display_types::wire::Mode ToFidl() const;

  // Guaranteed to meet the requirements in the FIDL documentation.
  constexpr Dimensions active_area() const { return active_area_; }

  // Guaranteed to meet the requirements in the FIDL documentation.
  //
  // This is the maximum refresh rate when `is_variable_refresh_rate()` is true.
  constexpr int32_t refresh_rate_millihertz() const { return refresh_rate_millihertz_; }

  // Guaranteed to meet the requirements in the FIDL documentation.
  constexpr fuchsia_hardware_display_types::wire::ModeFlags flags() const { return flags_; }

  // True iff the display may refresh less often than `refresh_rate_millihertz()`.
  constexpr bool is_variable_refresh_rate() const {
    return static_cast<bool>(flags_ &
                             fuchsia_hardware_display_types::wire::ModeFlags::kVariableRefreshRate);
  }

 private:
  struct ConstructorArgs {
    int32_t active_width;
    int32_t active_height;
    int32_t refresh_rate_millihertz;
    fuchsia_hardware_display_types::wire::ModeFlags flags = {};
  };

  // In debug mode, asserts that IsValid() would return true.
  //
  // IsValid() variant with developer-friendly debug assertions.
  static constexpr void DebugAssertIsValid(const Mode::ConstructorArgs& args);
  static constexpr void DebugAssertIsValid(
      const fuchsia_hardware_display_types::wire::Mode& fidl_mode);

  Dimensions active_area_;
  int32_t refresh_rate_millihertz_;
  fuchsia_hardware_display_types::wire::ModeFlags flags_;
};

// static
constexpr bool Mode::IsValid(const fuchsia_hardware_display_types::wire::Mode& fidl_mode) {
  if (!Dimensions::IsValid((fidl_mode.active_area))) {
    return false;
  }

  const Dimensions active_area = Dimensions::From(fidl_mode.active_area);
  if (active_area.IsEmpty()) {
    return false;
  }

  if (fidl_mode.refresh_rate_millihertz <= 0) {
    return false;
  }
  if (fidl_mode.refresh_rate_millihertz > kMaxRefreshRateMillihertz) {
    return false;
  }

  if (fidl_mode.flags.has_unknown_bits()) {
    return false;
  }

  return true;
}

constexpr Mode::Mode(const Mode::ConstructorArgs& args)
    : active_area_({.width = args.active_width, .height = args.active_height}),
      refresh_rate_millihertz_(args.refresh_rate_millihertz),
      flags_(args.flags) {
  DebugAssertIsValid(args);
}

// static
constexpr Mode Mode::From(const fuchsia_hardware_display_types::wire::Mode& fidl_mode) {
  DebugAssertIsValid(fidl_mode);
  const Dimensions active_area = Dimensions::From(fidl_mode.active_area);

  return Mode({
      .active_width = active_area.width(),
      .active_height = active_area.height(),
      .refresh_rate_millihertz = static_cast<int32_t>(fidl_mode.refresh_rate_millihertz),
      .flags = fidl_mode.flags,
  });
}

constexpr bool operator==(const Mode& lhs, const Mode& rhs) {
  return lhs.active_area_ == rhs.active_area_ &&
         lhs.refresh_rate_millihertz_ == rhs.refresh_rate_millihertz_ && lhs.flags_ == rhs.flags_;
}

constexpr bool operator!=(const Mode& lhs, const Mode& rhs) { return !(lhs == rhs); }

constexpr fuchsia_hardware_display_types::wire::Mode Mode::ToFidl() const {
  return fuchsia_hardware_display_types::wire::Mode{
      .active_area = active_area_.ToFidl(),

      // This cast does not cause UB because the refresh rate must be
      // non-negative.
      .refresh_rate_millihertz = static_cast<uint32_t>(refresh_rate_millihertz_),
      .flags = flags_,
  };
}

// static
constexpr void Mode::DebugAssertIsValid(const Mode::ConstructorArgs& args) {
  const Dimensions active_area({.width = args.active_width, .height = args.active_height});
  ZX_DEBUG_ASSERT(!active_area.IsEmpty());

  ZX_DEBUG_ASSERT(args.refresh_rate_millihertz > 0);
  ZX_DEBUG_ASSERT(args.refresh_rate_millihertz <= kMaxRefreshRateMillihertz);

  ZX_DEBUG_ASSERT(!args.flags.has_unknown_bits());
}

// static
constexpr void Mode::DebugAssertIsValid(
    const fuchsia_hardware_display_types::wire::Mode& fidl_mode) {
  ZX_DEBUG_ASSERT(Dimensions::IsValid(fidl_mode.active_area));
  const Dimensions active_area = Dimensions::From(fidl_mode.active_area);
  ZX_DEBUG_ASSERT(!active_area.IsEmpty());

  ZX_DEBUG_ASSERT(fidl_mode.refresh_rate_millihertz > 0);
  ZX_DEBUG_ASSERT(fidl_mode.refresh_rate_millihertz <= kMaxRefreshRateMillihertz);

  ZX_DEBUG_ASSERT(!fidl_mode.flags.has_unknown_bits());
}

}  // namespace display

#if __cplusplus >= 202002L
template <>
struct std::formatter<display::Mode> {
  constexpr auto parse(std::format_parse_context& ctx) { return ctx.begin(); }
  auto format(const display::Mode& mode, std::format_context& ctx) const {
    auto out = std::format_to(ctx.out(), "{}x{} @ {}.{:03} Hz", mode.active_area().width(),
                              mode.active_area().height(), mode.refresh_rate_millihertz() / 1000,
                              mode.refresh_rate_millihertz() % 1000);
    if (mode.is_variable_refresh_rate()) {
      out = std::format_to(out, " (variable refresh rate)");
    }
    return out;
  }
};
#endif

#endif  // SRC_GRAPHICS_DISPLAY_LIB_API_TYPES_CPP_MODE_H_
