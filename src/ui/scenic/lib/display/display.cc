// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/display/display.h"

#include <fidl/fuchsia.hardware.display.types/cpp/fidl.h>
#include <fidl/fuchsia.hardware.display/cpp/fidl.h>
#include <fidl/fuchsia.images2/cpp/fidl.h>
#include <fidl/fuchsia.ui.input.internal/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>
#include <zircon/syscalls.h>

#include <algorithm>

#include "src/ui/scenic/lib/utils/logging.h"

namespace display {

namespace {

// These constants are defined as raw hex in the FIDL file, so we confirm here that they are the
// same values as the expected constants in the ZX headers.
static_assert(
    static_cast<uint32_t>(fuchsia_ui_input_internal::InputOwnershipSignal::kDisplayUnowned) ==
        ZX_USER_SIGNAL_0,
    "Bad constant");
static_assert(
    static_cast<uint32_t>(fuchsia_ui_input_internal::InputOwnershipSignal::kDisplayPlatformOwned) ==
        ZX_USER_SIGNAL_1,
    "Bad constant");
static_assert(
    static_cast<uint32_t>(fuchsia_ui_input_internal::InputOwnershipSignal::kInputClientOwned) ==
        ZX_USER_SIGNAL_3,
    "Bad constant");
static_assert(
    static_cast<uint32_t>(fuchsia_ui_input_internal::InputOwnershipSignal::kInputPlatformOwned) ==
        ZX_USER_SIGNAL_4,
    "Bad constant");
static_assert(fuchsia_ui_display_singleton::kMaxCountFrameIntervalPowerHints ==
                  fuchsia_hardware_display_types::kMaxCountFrameIntervalPowerHints,
              "Bad constant");

constexpr int64_t kMillihertzPerHertz = 1'000;

zx::duration NominalRefreshPeriod(const WireDisplayMode& mode) {
  if (mode.refresh_rate_millihertz == 0) {
    return zx::duration(0);
  }
  return zx::nsec((zx::sec(1).to_nsecs() * kMillihertzPerHertz) / mode.refresh_rate_millihertz);
}

zx::duration ComputeVsyncPeriod(
    const WireDisplayMode& mode,
    const std::optional<fuchsia_hardware_display_types::VrrConfig>& vrr_config) {
  if (vrr_config.has_value() && vrr_config->vsync_period().has_value() &&
      *vrr_config->vsync_period() > 0) {
    return zx::duration(*vrr_config->vsync_period());
  }
  return NominalRefreshPeriod(mode);
}

std::optional<fuchsia_ui_display_singleton::VrrConfig> ComputeSingletonVrrConfig(
    const WireDisplayMode& mode,
    const std::optional<fuchsia_hardware_display_types::VrrConfig>& vrr_config) {
  if (!static_cast<bool>(mode.flags &
                         fuchsia_hardware_display_types::wire::ModeFlags::kVariableRefreshRate)) {
    return std::nullopt;
  }

  fuchsia_ui_display_singleton::VrrConfig out;
  if (vrr_config.has_value() && vrr_config->min_frame_interval().has_value() &&
      *vrr_config->min_frame_interval() > 0) {
    out.min_frame_interval(*vrr_config->min_frame_interval());
  } else {
    out.min_frame_interval(NominalRefreshPeriod(mode).get());
  }

  if (vrr_config.has_value() && vrr_config->frame_interval_power_hints().has_value() &&
      !vrr_config->frame_interval_power_hints()->empty()) {
    const auto& src_hints = *vrr_config->frame_interval_power_hints();
    std::vector<fuchsia_ui_display_singleton::FrameIntervalPowerHint> hints;
    hints.reserve(src_hints.size());
    for (const auto& hint : src_hints) {
      hints.emplace_back(hint.frame_interval(), hint.average_refresh_period());
    }
    out.frame_interval_power_hints(std::move(hints));
  }

  if (vrr_config.has_value() && vrr_config->notify_expected_present_config().has_value()) {
    const auto& cfg = *vrr_config->notify_expected_present_config();
    out.notify_expected_present_config(
        fuchsia_ui_display_singleton::NotifyExpectedPresentConfig(cfg.heads_up(), cfg.timeout()));
  }

  return out;
}

}  // namespace

Display::Display(WireDisplayId id, const WireDisplayMode& mode, uint32_t width_in_mm,
                 uint32_t height_in_mm, uint32_t max_layer_count,
                 std::vector<fuchsia_images2::PixelFormat> pixel_formats,
                 std::optional<fuchsia_hardware_display_types::VrrConfig> vrr_config)
    : vsync_timing_(std::make_shared<scheduling::VsyncTiming>()),
      display_id_(id),
      mode_(mode),
      width_in_mm_(width_in_mm),
      height_in_mm_(height_in_mm),
      max_layer_count_(max_layer_count),
      vsync_period_(ComputeVsyncPeriod(mode, vrr_config)),
      vrr_config_(ComputeSingletonVrrConfig(mode, vrr_config)),
      pixel_formats_(std::move(pixel_formats)) {
  zx_status_t status = zx::event::create(0, &ownership_event_);
  FX_DCHECK(status == ZX_OK);
  // Seed the ownership event with initial default signals: the display starts unowned by Scenic
  // (`kDisplayUnowned`), and input ownership defaults to the platform (`kInputPlatformOwned`)
  // before DisplayManager and ViewTree updates run.
  ownership_event_.signal(
      0,
      static_cast<uint32_t>(fuchsia_ui_input_internal::InputOwnershipSignal::kDisplayUnowned |
                            fuchsia_ui_input_internal::InputOwnershipSignal::kInputPlatformOwned));
  device_pixel_ratio_.store({1.f, 1.f});

  if (vsync_period_ > zx::duration(0)) {
    vsync_timing_->set_vsync_interval(vsync_period_);
  }
}

Display::Display(WireDisplayId id, uint32_t width_in_px, uint32_t height_in_px,
                 uint32_t max_layer_count)
    : Display(id,
              WireDisplayMode{.active_area = {.width = width_in_px, .height = height_in_px},
                              .refresh_rate_millihertz = 60'000},
              0, 0, max_layer_count, {fuchsia_images2::PixelFormat::kB8G8R8A8}) {}

void Display::Claim() {
  FX_DCHECK(!claimed_);
  claimed_ = true;
}

void Display::Unclaim() {
  FX_DCHECK(claimed_);
  claimed_ = false;
}

Display::VsyncCallbackId Display::AddVsyncCallback(VsyncCallback callback) {
  const VsyncCallbackId id = ++next_vsync_callback_id_;
  FX_DCHECK(!vsync_callbacks_.contains(id));
  vsync_callbacks_[id] = std::move(callback);
  return id;
}

void Display::RemoveVsyncCallback(VsyncCallbackId id) {
  if (!vsync_callbacks_.erase(id)) {
    FX_LOGS(ERROR) << "Removing an unregistered vsync callback.";
  }
}

void Display::OnVsync(zx::time_monotonic timestamp, WireConfigStamp displayed_config_stamp) {
  if (!vrr_config_.has_value() && vsync_timing_->last_vsync_time() > zx::time(0)) {
    // Fixed-rate and emulator displays may report an uncalibrated or placeholder refresh rate, or
    // experience single-IRQ delivery jitter, so smooth empirical inter-VSync deltas via
    // `MedianDurationPredictor`. Variable refresh rate displays may skip VSync events during
    // self-refresh while keeping their hardware TE period constant, so they stay locked to the
    // authoritative `vsync_period_` seeded at display attach.
    const zx::duration time_since_last_vsync = timestamp - vsync_timing_->last_vsync_time();
    if (time_since_last_vsync >= zx::duration(0) && time_since_last_vsync < kMaximumVsyncInterval) {
      vsync_timing_->AddVsyncInterval(std::max(kMinimumVsyncInterval, time_since_last_vsync));
    }
  }

  vsync_timing_->set_last_vsync_time(timestamp);

  TRACE_INSTANT("gfx", "Display::OnVsync", TRACE_SCOPE_PROCESS, "Timestamp", timestamp.get(),
                "Vsync interval", vsync_timing_->vsync_interval().get());

  for (const auto& [id, callback] : vsync_callbacks_) {
    FLATLAND_VERBOSE_LOG << "Display::OnVsync(): display_id=" << display_id_.value()
                         << "  callback_id=" << id << "  timestamp=" << timestamp.get()
                         << "  displayed_config_stamp=" << displayed_config_stamp.value
                         << "  ... invoking vsync callback";
    callback(display_id_, timestamp, displayed_config_stamp);
  }
}

}  // namespace display
