// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_POWER_TESTING_THERMAL_TEST_CONTROL_THERMAL_TEST_CONTROL_H_
#define SRC_POWER_TESTING_THERMAL_TEST_CONTROL_THERMAL_TEST_CONTROL_H_

#include <fidl/fuchsia.thermal/cpp/fidl.h>
#include <fidl/test.thermal/cpp/fidl.h>
#include <lib/async/dispatcher.h>
#include <lib/fit/function.h>

#include <map>
#include <memory>
#include <optional>
#include <string>

class ClientStateWatcher : public fidl::Server<fuchsia_thermal::ClientStateWatcher> {
 public:
  ClientStateWatcher(async_dispatcher_t* dispatcher,
                     fidl::ServerEnd<fuchsia_thermal::ClientStateWatcher> watcher,
                     fit::function<void(fidl::UnbindInfo)> close_handler);

 private:
  friend class ThermalTestControl;

  void Watch(WatchCompleter::Sync& completer) override;
  void SetThermalState(uint64_t thermal_state);
  void MaybeSendThermalState();

  std::optional<WatchCompleter::Async> hanging_get_;
  std::optional<uint64_t> client_thermal_state_;
  std::optional<uint64_t> pending_client_state_;

  // Declared last so that the connection is torn down before the pending request it may reply to.
  fidl::ServerBinding<fuchsia_thermal::ClientStateWatcher> watcher_binding_;
};

class ThermalTestControl : public fidl::Server<fuchsia_thermal::ClientStateConnector>,
                           public fidl::Server<test_thermal::ClientStateControl> {
 public:
  explicit ThermalTestControl(async_dispatcher_t* dispatcher);

  // Handlers that bind connections to this instance on the dispatcher given to the constructor.
  // The component publishes them in its outgoing directory; tests may publish them anywhere.
  fidl::ProtocolHandler<fuchsia_thermal::ClientStateConnector> connector_handler();
  fidl::ProtocolHandler<test_thermal::ClientStateControl> control_handler();

 private:
  // fuchsia.thermal.ClientStateConnector impl
  void Connect(fidl::Server<fuchsia_thermal::ClientStateConnector>::ConnectRequest& request,
               fidl::Server<fuchsia_thermal::ClientStateConnector>::ConnectCompleter::Sync&
                   completer) override;

  // test.thermal.ClientStateControl impl
  void IsClientTypeConnected(
      fidl::Server<test_thermal::ClientStateControl>::IsClientTypeConnectedRequest& request,
      fidl::Server<test_thermal::ClientStateControl>::IsClientTypeConnectedCompleter::Sync&
          completer) override;
  void SetThermalState(
      fidl::Server<test_thermal::ClientStateControl>::SetThermalStateRequest& request,
      fidl::Server<test_thermal::ClientStateControl>::SetThermalStateCompleter::Sync& completer)
      override;

  bool IsClientTypeConnectedInternal(const std::string& client_type) const;

  async_dispatcher_t* dispatcher_;

  fidl::ServerBindingGroup<fuchsia_thermal::ClientStateConnector> client_state_connector_bindings_;
  fidl::ServerBindingGroup<test_thermal::ClientStateControl> test_controller_bindings_;
  std::map<std::string, std::unique_ptr<ClientStateWatcher>> watchers_;
};

#endif  // SRC_POWER_TESTING_THERMAL_TEST_CONTROL_THERMAL_TEST_CONTROL_H_
