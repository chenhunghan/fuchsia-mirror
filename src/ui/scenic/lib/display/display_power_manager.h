// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_DISPLAY_DISPLAY_POWER_MANAGER_H_
#define SRC_UI_SCENIC_LIB_DISPLAY_DISPLAY_POWER_MANAGER_H_

#include <fidl/fuchsia.hardware.display.types/cpp/fidl.h>
#include <fidl/fuchsia.ui.display.singleton/cpp/fidl.h>
#include <lib/async/default.h>
#include <lib/fit/function.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/types.h>

#include "src/lib/fxl/macros.h"

namespace display {

// True for every `PowerMode` in which the display emits VSync events: `kOn`, `kDoze`,
// `kDozeSuspend`. Only `kOff` stops VSync events (see `fuchsia.hardware.display.types/PowerMode`).
bool PowerModeGeneratesVsyncs(fuchsia_ui_display_singleton::PowerMode mode);

// Implements the `fuchsia::ui::display::singleton::DisplayPower` protocol,
// Internal protocol clients are able to control the power of all available
// display devices through this protocol.
class DisplayPowerManager : public fidl::Server<fuchsia_ui_display_singleton::DisplayPower> {
 public:
  // Applies `mode` to the display hardware, synchronously, and returns `ZX_OK` or the
  // status to report to the protocol client.
  using SetDisplayPowerModeFn =
      fit::function<zx_status_t(fuchsia_hardware_display_types::PowerMode mode)>;

  DisplayPowerManager(inspect::Node& parent_node, SetDisplayPowerModeFn set_display_power_mode);

  // `fuchsia::ui::display::singleton::DisplayPower`
  void SetPowerMode(SetPowerModeRequest& request, SetPowerModeCompleter::Sync& completer) override;
  void SetPowerMode(fuchsia_ui_display_singleton::PowerMode power_mode,
                    fit::function<void(fit::result<zx_status_t>)> completer);

  fuchsia_ui_display_singleton::PowerMode current_power_mode() const { return current_power_mode_; }
  zx::time_monotonic last_power_change_time() const { return last_power_change_time_; }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_ui_display_singleton::DisplayPower> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {
    FX_LOGS(WARNING) << "Received an unknown method with ordinal " << metadata.method_ordinal;
  }

  fidl::ProtocolHandler<fuchsia_ui_display_singleton::DisplayPower> GetHandler() {
    return bindings_.CreateHandler(this, async_get_default_dispatcher(),
                                   fidl::kIgnoreBindingClosure);
  }

 private:
  void AddSetPowerModeInspectValues(fuchsia_ui_display_singleton::PowerMode, zx_status_t status);

  inspect::BoundedListNode inspect_display_power_events_;
  fidl::ServerBindingGroup<fuchsia_ui_display_singleton::DisplayPower> bindings_;
  fuchsia_ui_display_singleton::PowerMode current_power_mode_ =
      fuchsia_ui_display_singleton::PowerMode::kOn;
  zx::time_monotonic last_power_change_time_ = zx::time_monotonic::infinite_past();
  SetDisplayPowerModeFn set_display_power_mode_;

  FXL_DISALLOW_COPY_AND_ASSIGN(DisplayPowerManager);
};

}  // namespace display

#endif  // SRC_UI_SCENIC_LIB_DISPLAY_DISPLAY_POWER_MANAGER_H_
