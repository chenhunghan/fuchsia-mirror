// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/display/display_power_manager.h"

#include <fidl/fuchsia.hardware.display.types/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/clock.h>
#include <zircon/errors.h>
#include <zircon/status.h>

namespace display {

namespace {

using PowerMode = fuchsia_ui_display_singleton::PowerMode;

constexpr char kDisplayPowerEvents[] = "display_power_events";
constexpr uint64_t kInspectHistorySize = 64;

std::string ToString(const PowerMode& power_mode) {
  switch (power_mode) {
    case PowerMode::kOff:
      return "off";
    case PowerMode::kOn:
      return "on";
    case PowerMode::kDoze:
      return "doze";
    case PowerMode::kDozeSuspend:
      return "doze_suspend";
    default:
      return "unknown";
  }
}

fuchsia_hardware_display_types::PowerMode ToDisplayPowerMode(const PowerMode& power_mode) {
  switch (power_mode) {
    case PowerMode::kOff:
      return fuchsia_hardware_display_types::PowerMode::kOff;
    case PowerMode::kOn:
      return fuchsia_hardware_display_types::PowerMode::kOn;
    case PowerMode::kDoze:
      return fuchsia_hardware_display_types::PowerMode::kDoze;
    case PowerMode::kDozeSuspend:
      return fuchsia_hardware_display_types::PowerMode::kDozeSuspend;
    default:
      FX_LOGS(ERROR) << "Unexpected power mode: " << ToString(power_mode) << "; defaulting to ON";
      return fuchsia_hardware_display_types::PowerMode::kOn;
  }
}

}  // namespace

bool PowerModeGeneratesVsyncs(fuchsia_ui_display_singleton::PowerMode mode) {
  switch (mode) {
    case fuchsia_ui_display_singleton::PowerMode::kOn:
    case fuchsia_ui_display_singleton::PowerMode::kDoze:
    case fuchsia_ui_display_singleton::PowerMode::kDozeSuspend:
      return true;
    case fuchsia_ui_display_singleton::PowerMode::kOff:
      return false;
    default:
      // `ToDisplayPowerMode()` forwards unknown modes to the coordinator as `kOn`,
      // so treat them as generating vsyncs here too.
      return true;
  }
}

DisplayPowerManager::DisplayPowerManager(inspect::Node& parent_node,
                                         SetDisplayPowerModeFn set_display_power_mode)
    : inspect_display_power_events_(parent_node.CreateChild(kDisplayPowerEvents),
                                    kInspectHistorySize),
      set_display_power_mode_(std::move(set_display_power_mode)) {}

void DisplayPowerManager::SetPowerMode(SetPowerModeRequest& request,
                                       SetPowerModeCompleter::Sync& completer) {
  SetPowerMode(request.power_mode(),
               [completer = completer.ToAsync()](auto result) mutable { completer.Reply(result); });
}

void DisplayPowerManager::SetPowerMode(PowerMode power_mode,
                                       fit::function<void(fit::result<zx_status_t>)> completer) {
  const zx_status_t status = set_display_power_mode_(ToDisplayPowerMode(power_mode));
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "DisplayPowerManager.SetPowerMode() FAILED to set value: "
                   << ToString(power_mode) << ": " << zx_status_get_string(status);
    AddSetPowerModeInspectValues(power_mode, status);
    completer(fit::error(status));
    return;
  }

  FX_LOGS(INFO) << "Successfully set display power mode: " << ToString(power_mode);
  current_power_mode_ = power_mode;
  last_power_change_time_ = zx::clock::get_monotonic();

  AddSetPowerModeInspectValues(power_mode, ZX_OK);
  completer(fit::ok());
}

void DisplayPowerManager::AddSetPowerModeInspectValues(PowerMode power_mode, zx_status_t status) {
  const auto boot_now = zx::clock::get_boot();
  const auto mono_now = zx::clock::get_monotonic();

  inspect_display_power_events_.CreateEntry(
      [power_mode, status, boot_now, mono_now](inspect::Node& n) {
        std::string power_mode_str = ToString(power_mode);
        if (status != ZX_OK) {
          power_mode_str = power_mode_str + "_ERROR_" + zx_status_get_string(status);
        }

        // TODO(b/475953032): Remove unsuffixed `power_mode_str` when it is no longer used directly.
        n.RecordInt(power_mode_str, mono_now.get());
        // This is used in system health analyses. Without it, the fallback is `mono_now` above,
        // which is wrong.
        n.RecordInt(std::format("{}_boot_ns", power_mode_str), boot_now.get());
        n.RecordInt(std::format("{}_mono_ns", power_mode_str), mono_now.get());

        // Detect potential inaccuracy in the monotonic clock reading.  This instructs the
        // consumer to take this timestamp with a grain of salt.
        const zx::duration boot_diff = zx::clock::get_boot() - boot_now;
        constexpr auto kBootDiffThreshold = zx::usec(100);
        if (boot_diff >= kBootDiffThreshold) {
          n.RecordInt("timestamp_inaccuracy_range_us", boot_diff.to_usecs());
        }
      });
}

}  // namespace display
