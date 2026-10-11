// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/display/singleton_display_service.h"

#include <fidl/fuchsia.math/cpp/fidl.h>
#include <lib/async/default.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/status.h>

#include "src/lib/fsl/handles/object_info.h"

namespace display {

SingletonDisplayService::SingletonDisplayService(std::shared_ptr<display::Display> display)
    : display_(std::move(display)) {}

void SingletonDisplayService::GetMetrics(GetMetricsCompleter::Sync& completer) {
  GetMetrics([completer = completer.ToAsync()](auto result) mutable {
    completer.Reply(std::move(result));
  });
}

void SingletonDisplayService::GetMetrics(
    fit::function<void(fuchsia_ui_display_singleton::InfoGetMetricsResponse)> callback) {
  const glm::vec2 dpr = display_->device_pixel_ratio();
  if (dpr.x != dpr.y) {
    FX_LOGS(WARNING) << "SingletonDisplayService::GetMetrics(): x/y display pixel ratio mismatch ("
                     << dpr.x << " vs. " << dpr.y << ")";
  }

  auto metrics = fuchsia_ui_display_singleton::Metrics();
  metrics.extent_in_px(fuchsia_math::SizeU{display_->width_in_px(), display_->height_in_px()});
  metrics.extent_in_mm(fuchsia_math::SizeU{display_->width_in_mm(), display_->height_in_mm()});
  metrics.recommended_device_pixel_ratio(fuchsia_math::VecF{dpr.x, dpr.y});
  metrics.maximum_refresh_rate_in_millihertz(display_->maximum_refresh_rate_in_millihertz());

  callback(std::move(metrics));
}

void SingletonDisplayService::GetEvent(
    fidl::Server<fuchsia_ui_composition_internal::DisplayOwnership>::GetEventCompleter::Sync&
        completer) {
  GetEvent([completer = completer.ToAsync()](auto result) mutable {
    completer.Reply(std::move(result));
  });
}

void SingletonDisplayService::GetEvent(
    fit::function<void(fuchsia_ui_composition_internal::DisplayOwnershipGetEventResponse)>
        callback) {
  // These constants are defined as raw hex in the FIDL file, so we confirm here that they are the
  // same values as the expected constants in the ZX headers.
  static_assert(fuchsia_ui_composition_internal::kSignalDisplayNotOwned == ZX_USER_SIGNAL_0,
                "Bad constant");
  static_assert(fuchsia_ui_composition_internal::kSignalDisplayOwned == ZX_USER_SIGNAL_1,
                "Bad constant");

  zx::event dup;
  if (display_->ownership_event().duplicate(ZX_RIGHTS_BASIC, &dup) != ZX_OK) {
    FX_LOGS(ERROR) << "Display ownership event duplication error.";
    callback(zx::event());
  } else {
    callback(std::move(dup));
  }
}

void SingletonDisplayService::GetEvent(
    fuchsia_ui_input_internal::InputOwnershipGetEventRequest& request,
    fidl::Server<fuchsia_ui_input_internal::InputOwnership>::GetEventCompleter::Sync& completer) {
  GetEvent(std::move(request),
           [completer = completer.ToAsync()](zx_status_t status, auto result) mutable {
             if (status != ZX_OK) {
               completer.Close(status);
               return;
             }
             FX_DCHECK(result.has_value());
             completer.Reply(std::move(*result));
           });
}

void SingletonDisplayService::GetEvent(
    fuchsia_ui_input_internal::InputOwnershipGetEventRequest request,
    fit::function<
        void(zx_status_t status,
             std::optional<fit::result<fuchsia_ui_input_internal::InputOwnershipError,
                                       fuchsia_ui_input_internal::InputOwnershipGetEventResponse>>)>
        callback) {
  // These constants are defined as raw hex in the FIDL file, so we confirm here that they are the
  // same values as the expected constants in the ZX headers.
  static_assert(
      static_cast<uint32_t>(fuchsia_ui_input_internal::InputOwnershipSignal::kDisplayUnowned) ==
          ZX_USER_SIGNAL_0,
      "Bad constant");
  static_assert(static_cast<uint32_t>(
                    fuchsia_ui_input_internal::InputOwnershipSignal::kDisplayPlatformOwned) ==
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

  std::optional<zx_koid_t> client_koid;
  switch (request.target().Which()) {
    case fuchsia_ui_input_internal::InputOwnershipTarget::Tag::kVirtcon:
    case fuchsia_ui_input_internal::InputOwnershipTarget::Tag::kPlatform:
      // Virtcon and SceneManager represent well-known platform system components that do
      // not require view registration or dynamic validation. They are granted a duplicate
      // handle to the shared ownership event with read-only rights.
      break;
    case fuchsia_ui_input_internal::InputOwnershipTarget::Tag::kViewRef: {
      const auto& view_ref = request.target().view_ref().value();
      zx_signals_t observed = 0;
      if (view_ref.reference().is_valid() &&
          view_ref.reference().wait_one(ZX_EVENTPAIR_PEER_CLOSED, zx::time(0), &observed) ==
              ZX_OK &&
          (observed & ZX_EVENTPAIR_PEER_CLOSED)) {
        callback(ZX_OK, fit::error(fuchsia_ui_input_internal::InputOwnershipError::kUnknownView));
        return;
      }
      const zx_koid_t koid = fsl::GetKoid(view_ref.reference().get());
      if (koid == ZX_KOID_INVALID) {
        callback(ZX_OK, fit::error(fuchsia_ui_input_internal::InputOwnershipError::kUnknownView));
        return;
      }
      client_koid = koid;
      break;
    }
    default:
      callback(ZX_OK, fit::error(fuchsia_ui_input_internal::InputOwnershipError::kInvalidTarget));
      return;
  }

  if (!display_) {
    FX_LOGS(ERROR) << "Failed to duplicate ownership event: display is null";
    callback(ZX_ERR_INTERNAL, std::nullopt);
    return;
  }

  // ZX_RIGHTS_BASIC grants read-only access (TRANSFER, DUPLICATE, WAIT, INSPECT)
  // while intentionally omitting ZX_RIGHT_SIGNAL so clients cannot signal ownership themselves.
  zx::event dup;
  if (zx_status_t status = display_->ownership_event().duplicate(ZX_RIGHTS_BASIC, &dup);
      status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to duplicate ownership event: " << zx_status_get_string(status);
    callback(status, std::nullopt);
    return;
  }

  // Register client ViewRef only after handle duplication succeeds to prevent state leakage
  // on resource exhaustion. In accordance with the system-wide singleton direct-client model,
  // registering this ViewRef replaces any prior direct client registration.
  if (client_koid.has_value() && on_view_ref_registered_) {
    on_view_ref_registered_(*client_koid);
  }

  callback(ZX_OK,
           fit::ok(fuchsia_ui_input_internal::InputOwnershipGetEventResponse(std::move(dup))));
}

void SingletonDisplayService::AddPublicService(sys::OutgoingDirectory* outgoing_directory) {
  FX_DCHECK(outgoing_directory);
  outgoing_directory->AddProtocol<fuchsia_ui_display_singleton::Info>(info_bindings_.CreateHandler(
      this, async_get_default_dispatcher(), fidl::kIgnoreBindingClosure));
  outgoing_directory->AddProtocol<fuchsia_ui_composition_internal::DisplayOwnership>(
      ownership_bindings_.CreateHandler(this, async_get_default_dispatcher(),
                                        fidl::kIgnoreBindingClosure));
  outgoing_directory->AddProtocol<fuchsia_ui_input_internal::InputOwnership>(
      input_ownership_bindings_.CreateHandler(this, async_get_default_dispatcher(),
                                              fidl::kIgnoreBindingClosure));
}

}  // namespace display
