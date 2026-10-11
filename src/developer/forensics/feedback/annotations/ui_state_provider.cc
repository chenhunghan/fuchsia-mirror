// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/annotations/ui_state_provider.h"

#include <lib/async/cpp/task.h>
#include <lib/fit/function.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>

#include "src/developer/forensics/feedback/annotations/constants.h"
#include "src/developer/forensics/feedback/annotations/fidl_provider.h"
#include "src/developer/forensics/utils/errors.h"
#include "src/developer/forensics/utils/time.h"

namespace forensics::feedback {
namespace {

// fuchsia.ui.activity.Listener isn't @discoverable, so its name isn't available via
// fidl::DiscoverableProtocolName.
constexpr std::string_view kListenerProtocolName = "fuchsia.ui.activity.Listener";

std::string GetUIStateString(fuchsia_ui_activity::State state) {
  switch (state) {
    case fuchsia_ui_activity::State::kUnknown:
      return "unknown";
    case fuchsia_ui_activity::State::kIdle:
      return "idle";
    case fuchsia_ui_activity::State::kActive:
      return "active";
  }
}

}  // namespace

UIStateProvider::UIStateProvider(async_dispatcher_t* dispatcher,
                                 std::shared_ptr<sys::ServiceDirectory> services,
                                 std::unique_ptr<timekeeper::Clock> clock,
                                 std::unique_ptr<backoff::Backoff> backoff)
    : dispatcher_(dispatcher),
      services_(std::move(services)),
      clock_(std::move(clock)),
      backoff_(std::move(backoff)) {
  StartListening();
}

void UIStateProvider::StartListening() {
  zx::result provider_endpoints = fidl::CreateEndpoints<fuchsia_ui_activity::Provider>();
  if (provider_endpoints.is_error()) {
    FX_LOGS(ERROR) << "Failed to create endpoints for "
                   << fidl::DiscoverableProtocolName<fuchsia_ui_activity::Provider> << ": "
                   << provider_endpoints.status_string();
    return;
  }

  zx::result listener_endpoints = fidl::CreateEndpoints<fuchsia_ui_activity::Listener>();
  if (listener_endpoints.is_error()) {
    FX_LOGS(ERROR) << "Failed to create endpoints for " << kListenerProtocolName << ": "
                   << listener_endpoints.status_string();
    return;
  }

  services_->Connect(fidl::DiscoverableProtocolName<fuchsia_ui_activity::Provider>,
                     provider_endpoints->server.TakeChannel());
  provider_ = fidl::Client<fuchsia_ui_activity::Provider>(std::move(provider_endpoints->client),
                                                          dispatcher_, this);

  binding_.emplace(dispatcher_, std::move(listener_endpoints->server), this,
                   [this](fidl::UnbindInfo info) { OnListenerClosed(info); });

  const ::fit::result<::fidl::OneWayError> result = provider_->WatchState({{
      .listener = std::move(listener_endpoints->client),
  }});

  if (result.is_error()) {
    FX_LOGS(WARNING) << "Failed to watch activity state: " << result.error_value();
  }
}

void UIStateProvider::on_fidl_error(fidl::UnbindInfo error) {
  // The provider client and listener binding connections are not expected to close. A provider
  // error tears down both so that StartListening can recreate them together.
  provider_ = fidl::Client<fuchsia_ui_activity::Provider>();
  binding_.reset();

  OnDisconnect(error.status(), fidl::DiscoverableProtocolName<fuchsia_ui_activity::Provider>);
}

void UIStateProvider::OnListenerClosed(const fidl::UnbindInfo info) {
  // Intentionally leave |provider_| bound. If fuchsia.ui.activity.Provider isn't available, its
  // channel is closed with ZX_ERR_NOT_FOUND, which also drops the listener client end sent in
  // WatchState. Both closures can be observed in either order, so let the provider's error handler
  // decide whether to stop reconnecting. |provider_| is replaced when StartListening reconnects.
  binding_.reset();

  OnDisconnect(info.status(), kListenerProtocolName);
}

void UIStateProvider::OnDisconnect(const zx_status_t status,
                                   const std::string_view interface_name) {
  const internal::DisconnectResponse disconnect =
      internal::DisconnectResponse::BuildFrom(status, interface_name);

  current_state_ = ErrorOrString(disconnect.error);
  last_transition_time_ = disconnect.error;

  if (on_update_) {
    on_update_({{kSystemUserActivityCurrentStateKey, *current_state_}});
  }

  if (!disconnect.should_reconnect) {
    reconnect_task_.Cancel();
    FX_LOGS(WARNING) << disconnect.log_message;
    return;
  }

  // Both connections can close for the same underlying reason; only schedule one reconnect.
  if (reconnect_task_.is_pending()) {
    return;
  }

  FX_PLOGS(WARNING, status) << disconnect.log_message;
  reconnect_task_.PostDelayed(dispatcher_, backoff_->GetNext());
}

std::set<std::string> UIStateProvider::GetAnnotationKeys() {
  return {
      kSystemUserActivityCurrentStateKey,
      kSystemUserActivityCurrentDurationKey,
  };
}

std::set<std::string> UIStateProvider::GetKeys() const {
  return UIStateProvider::GetAnnotationKeys();
}

void UIStateProvider::OnStateChanged(OnStateChangedRequest& request,
                                     OnStateChangedCompleter::Sync& completer) {
  backoff_->Reset();
  current_state_ = ErrorOrString(GetUIStateString(request.state()));
  last_transition_time_ = zx::time_monotonic(request.transition_time());
  completer.Reply();

  if (on_update_) {
    on_update_({{kSystemUserActivityCurrentStateKey, *current_state_}});
  }
}

Annotations UIStateProvider::Get() {
  if (std::holds_alternative<std::monostate>(last_transition_time_)) {
    return {};
  }
  if (std::holds_alternative<Error>(last_transition_time_)) {
    return {{kSystemUserActivityCurrentDurationKey,
             ErrorOrString(std::get<Error>(last_transition_time_))}};
  }

  const auto& time = std::get<zx::time_monotonic>(last_transition_time_);
  const std::optional<std::string> formatted_duration =
      FormatDuration(clock_->MonotonicNow() - time);

  // FormatDuration returns std::nullopt if duration was negative- if so, send Error::kBadValue as
  // annotation value
  const ErrorOrString duration = formatted_duration.has_value()
                                     ? ErrorOrString(formatted_duration.value())
                                     : ErrorOrString(Error::kBadValue);

  return {{kSystemUserActivityCurrentDurationKey, duration}};
}

void UIStateProvider::GetOnUpdate(::fit::function<void(Annotations)> callback) {
  on_update_ = std::move(callback);

  if (current_state_.has_value()) {
    on_update_({{kSystemUserActivityCurrentStateKey, *current_state_}});
  }
}

}  // namespace forensics::feedback
