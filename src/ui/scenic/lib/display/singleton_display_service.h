// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_DISPLAY_SINGLETON_DISPLAY_SERVICE_H_
#define SRC_UI_SCENIC_LIB_DISPLAY_SINGLETON_DISPLAY_SERVICE_H_

#include <fidl/fuchsia.ui.composition.internal/cpp/fidl.h>
#include <fidl/fuchsia.ui.display.singleton/cpp/fidl.h>
#include <fidl/fuchsia.ui.input.internal/cpp/fidl.h>
#include <lib/fit/function.h>
#include <lib/sys/cpp/outgoing_directory.h>

#include <memory>
#include <optional>

#include "src/ui/scenic/lib/display/display.h"

namespace display {

// Implements the `fuchsia.ui.display.singleton.Info`,
// `fuchsia.ui.composition.internal.DisplayOwnership`, and
// `fuchsia.ui.input.internal.InputOwnership` FIDL services.
//
// Architectural note on `InputOwnership` colocation:
// `InputOwnership` is implemented here alongside `DisplayOwnership` because both protocols
// distribute duplicates of the same underlying display kernel event (`Display::ownership_event()`),
// which multiplexes orthogonal signal bits for both hardware display ownership (controlled by
// `DisplayManager`) and direct input ownership (controlled by the `ViewTree` evaluator).
class SingletonDisplayService
    : public fidl::Server<fuchsia_ui_display_singleton::Info>,
      public fidl::Server<fuchsia_ui_composition_internal::DisplayOwnership>,
      public fidl::Server<fuchsia_ui_input_internal::InputOwnership> {
 public:
  using ViewRefRegisteredCallback = fit::function<void(zx_koid_t)>;

  explicit SingletonDisplayService(std::shared_ptr<display::Display> display);

  // `fuchsia_ui_display_singleton::Info`
  void GetMetrics(GetMetricsCompleter::Sync& completer) override;
  void GetMetrics(
      fit::function<void(fuchsia_ui_display_singleton::InfoGetMetricsResponse)> callback);

  // `fuchsia_ui_composition_internal::DisplayOwnership`
  void GetEvent(
      fidl::Server<fuchsia_ui_composition_internal::DisplayOwnership>::GetEventCompleter::Sync&
          completer) override;
  void GetEvent(
      fit::function<void(fuchsia_ui_composition_internal::DisplayOwnershipGetEventResponse)>
          callback);

  // `fuchsia_ui_input_internal::InputOwnership`
  void GetEvent(fuchsia_ui_input_internal::InputOwnershipGetEventRequest& request,
                fidl::Server<fuchsia_ui_input_internal::InputOwnership>::GetEventCompleter::Sync&
                    completer) override;
  void GetEvent(
      fuchsia_ui_input_internal::InputOwnershipGetEventRequest request,
      fit::function<void(
          zx_status_t status,
          std::optional<fit::result<fuchsia_ui_input_internal::InputOwnershipError,
                                    fuchsia_ui_input_internal::InputOwnershipGetEventResponse>>)>
          callback);

  // Registers a callback invoked whenever an input client requests `GetEvent()` with a
  // `kViewRef` target after handle duplication has succeeded.
  //
  // Threading contract: Must be called on Scenic's main thread during initialization before
  // `AddPublicService()` starts serving incoming FIDL requests.
  //
  // Lifecycle & direct-client model: In accordance with the system-wide singleton direct-client
  // model, registering a new client ViewRef replaces any prior registration in Scenic's
  // `InputOwnershipEvaluator`. The evaluator monitors when this registered view satisfies
  // single-view and full-screen criteria, while the duplicated kernel event broadcasts ownership
  // signal state changes.
  void SetOnViewRefRegisteredCallback(ViewRefRegisteredCallback callback) {
    on_view_ref_registered_ = std::move(callback);
  }

  // Registers this service impl in `outgoing_directory`.  This service impl object must then live
  // for as long as it is possible for any service requests to be made.
  void AddPublicService(sys::OutgoingDirectory* outgoing_directory);

 private:
  const std::shared_ptr<display::Display> display_ = nullptr;
  ViewRefRegisteredCallback on_view_ref_registered_;
  fidl::ServerBindingGroup<fuchsia_ui_display_singleton::Info> info_bindings_;
  fidl::ServerBindingGroup<fuchsia_ui_composition_internal::DisplayOwnership> ownership_bindings_;
  fidl::ServerBindingGroup<fuchsia_ui_input_internal::InputOwnership> input_ownership_bindings_;
};

}  // namespace display

#endif  // SRC_UI_SCENIC_LIB_DISPLAY_SINGLETON_DISPLAY_SERVICE_H_
