// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "thermal_test_control.h"

#include <lib/syslog/cpp/macros.h>
#include <zircon/assert.h>

#include <utility>

ClientStateWatcher::ClientStateWatcher(async_dispatcher_t* dispatcher,
                                       fidl::ServerEnd<fuchsia_thermal::ClientStateWatcher> watcher,
                                       fit::function<void(fidl::UnbindInfo)> close_handler)
    : pending_client_state_(0),
      watcher_binding_(dispatcher, std::move(watcher), this, std::move(close_handler)) {}

void ClientStateWatcher::Watch(WatchCompleter::Sync& completer) {
  if (hanging_get_.has_value()) {
    FX_LOGS(WARNING) << "Watch called while a Watch is already pending; closing watcher";
    completer.Close(ZX_ERR_BAD_STATE);
    return;
  }
  hanging_get_ = completer.ToAsync();
  MaybeSendThermalState();
}

void ClientStateWatcher::SetThermalState(uint64_t thermal_state) {
  if (thermal_state != client_thermal_state_) {
    pending_client_state_ = thermal_state;
    MaybeSendThermalState();
  }
}

void ClientStateWatcher::MaybeSendThermalState() {
  if (hanging_get_ && pending_client_state_) {
    hanging_get_->Reply(*pending_client_state_);
    hanging_get_.reset();

    client_thermal_state_ = *pending_client_state_;
    pending_client_state_.reset();
  }
}

ThermalTestControl::ThermalTestControl(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {
  FX_LOGS(INFO) << "Creating ThermalTestControl";
}

fidl::ProtocolHandler<fuchsia_thermal::ClientStateConnector>
ThermalTestControl::connector_handler() {
  return client_state_connector_bindings_.CreateHandler(this, dispatcher_,
                                                        fidl::kIgnoreBindingClosure);
}

fidl::ProtocolHandler<test_thermal::ClientStateControl> ThermalTestControl::control_handler() {
  return test_controller_bindings_.CreateHandler(this, dispatcher_, fidl::kIgnoreBindingClosure);
}

void ThermalTestControl::Connect(
    fidl::Server<fuchsia_thermal::ClientStateConnector>::ConnectRequest& request,
    fidl::Server<fuchsia_thermal::ClientStateConnector>::ConnectCompleter::Sync& completer) {
  const std::string& client_type = request.client_type();
  FX_LOGS(TRACE) << "client_type=" << client_type;

  ZX_ASSERT(!IsClientTypeConnectedInternal(client_type));

  watchers_[client_type] = std::make_unique<ClientStateWatcher>(
      dispatcher_, std::move(request.watcher()),
      [this, client_type](fidl::UnbindInfo info) { ZX_ASSERT(watchers_.erase(client_type) == 1); });
}

void ThermalTestControl::IsClientTypeConnected(
    fidl::Server<test_thermal::ClientStateControl>::IsClientTypeConnectedRequest& request,
    fidl::Server<test_thermal::ClientStateControl>::IsClientTypeConnectedCompleter::Sync&
        completer) {
  completer.Reply(IsClientTypeConnectedInternal(request.client_type()));
}

void ThermalTestControl::SetThermalState(
    fidl::Server<test_thermal::ClientStateControl>::SetThermalStateRequest& request,
    fidl::Server<test_thermal::ClientStateControl>::SetThermalStateCompleter::Sync& completer) {
  const std::string& client_type = request.client_type();
  FX_LOGS(TRACE) << "client_type=" << client_type << " state=" << request.state();

  ZX_ASSERT(IsClientTypeConnectedInternal(client_type));

  watchers_[client_type]->SetThermalState(request.state());
  completer.Reply();
}

bool ThermalTestControl::IsClientTypeConnectedInternal(const std::string& client_type) const {
  return watchers_.find(client_type) != watchers_.end();
}
