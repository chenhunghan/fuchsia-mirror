// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "thermal_test_control.h"

#include <fidl/fuchsia.thermal/cpp/fidl.h>
#include <fidl/test.thermal/cpp/fidl.h>
#include <lib/component/testing/cpp/service_directory_provider.h>
#include <lib/fidl/cpp/client.h>

#include <memory>
#include <optional>

#include <gtest/gtest.h>

#include "src/lib/testing/loop_fixture/test_loop_fixture.h"
#include "src/lib/testing/predicates/status.h"

class ThermalTestControlTest : public gtest::TestLoopFixture {
 protected:
  void SetUp() override {
    TestLoopFixture::SetUp();

    thermal_test_control_ = std::make_unique<ThermalTestControl>(dispatcher());
    ASSERT_OK(svc_provider_.AddProtocol<fuchsia_thermal::ClientStateConnector>(
        thermal_test_control_->connector_handler()));
    ASSERT_OK(svc_provider_.AddProtocol<test_thermal::ClientStateControl>(
        thermal_test_control_->control_handler()));

    zx::result connector = svc_provider_.Connect<fuchsia_thermal::ClientStateConnector>();
    ASSERT_OK(connector);
    connector_.Bind(std::move(connector.value()), dispatcher());

    zx::result client_state_control = svc_provider_.Connect<test_thermal::ClientStateControl>();
    ASSERT_OK(client_state_control);
    client_state_control_.Bind(std::move(client_state_control.value()), dispatcher());
  }

  void ConnectClient() {
    auto [client_end, server_end] = fidl::Endpoints<fuchsia_thermal::ClientStateWatcher>::Create();
    fit::result<fidl::OneWayError> result =
        connector_->Connect({{.client_type = kTestClientType, .watcher = std::move(server_end)}});
    ASSERT_TRUE(result.is_ok()) << result.error_value();
    watcher_.Bind(std::move(client_end), dispatcher());
    RunLoopUntilIdle();
  }

  void DisconnectClient() {
    watcher_ = {};
    // Unbinding completes asynchronously; let the server observe the closed channel.
    RunLoopUntilIdle();
  }

  // Sends a `Watch` request whose response, if any, is stored in `state_result`.
  void Watch(std::optional<uint64_t>& state_result) {
    watcher_->Watch().Then(
        [&state_result](fidl::Result<fuchsia_thermal::ClientStateWatcher::Watch>& result) {
          ASSERT_TRUE(result.is_ok()) << result.error_value();
          state_result = result->state();
        });
  }

  void SetThermalState(uint64_t state) {
    bool updated = false;
    client_state_control_->SetThermalState({{.client_type = kTestClientType, .state = state}})
        .Then([&updated](fidl::Result<test_thermal::ClientStateControl::SetThermalState>& result) {
          ASSERT_TRUE(result.is_ok()) << result.error_value();
          updated = true;
        });
    RunLoopUntilIdle();
    ASSERT_EQ(updated, true);
  }

  std::optional<bool> IsClientTypeConnected() {
    std::optional<bool> is_connected;
    client_state_control_->IsClientTypeConnected({{.client_type = kTestClientType}})
        .Then([&is_connected](
                  fidl::Result<test_thermal::ClientStateControl::IsClientTypeConnected>& result) {
          ASSERT_TRUE(result.is_ok()) << result.error_value();
          is_connected = result->connected();
        });
    RunLoopUntilIdle();
    return is_connected;
  }

 protected:
  static constexpr char kTestClientType[] = "test";

  std::unique_ptr<ThermalTestControl> thermal_test_control_;
  component::testing::ServiceDirectoryProvider svc_provider_{dispatcher()};
  fidl::Client<fuchsia_thermal::ClientStateConnector> connector_;
  fidl::Client<fuchsia_thermal::ClientStateWatcher> watcher_;
  fidl::Client<test_thermal::ClientStateControl> client_state_control_;
};

TEST_F(ThermalTestControlTest, PendingRequestCompletesForStateChange) {
  std::optional<uint64_t> state_result;

  ConnectClient();

  // Initial state should be 0
  state_result.reset();
  Watch(state_result);
  RunLoopUntilIdle();
  ASSERT_EQ(state_result, 0);

  // Another call to `Watch` should not complete since thermal state hasn't changed
  state_result.reset();
  Watch(state_result);
  RunLoopUntilIdle();
  ASSERT_EQ(state_result, std::nullopt);

  // Change thermal state to 1 and verify the `Watch` request completes successfully
  SetThermalState(1);
  RunLoopUntilIdle();
  ASSERT_EQ(state_result, 1);
}

TEST_F(ThermalTestControlTest, NewRequestAfterStateChange) {
  std::optional<uint64_t> state_result;

  ConnectClient();

  // Initial state should be 0
  state_result.reset();
  Watch(state_result);
  RunLoopUntilIdle();
  ASSERT_EQ(state_result, 0);

  // Another call to `Watch` should not complete since thermal state hasn't changed
  state_result.reset();
  Watch(state_result);
  RunLoopUntilIdle();
  ASSERT_EQ(state_result, std::nullopt);

  // Change thermal state to 1 and verify the `Watch` request completes successfully
  state_result.reset();
  SetThermalState(1);
  RunLoopUntilIdle();
  ASSERT_EQ(state_result, 1);

  // Change the state again, but without a pending request nothing should happen
  state_result.reset();
  SetThermalState(2);
  RunLoopUntilIdle();
  ASSERT_EQ(state_result, std::nullopt);

  // Send a new `Watch` request which should complete immediately with the latest state
  Watch(state_result);
  RunLoopUntilIdle();
  ASSERT_EQ(state_result, 2);
}

TEST_F(ThermalTestControlTest, IsClientConnected) {
  // Client is initially not connected
  ASSERT_EQ(IsClientTypeConnected(), false);

  // Connect the client
  ConnectClient();

  // Verify `IsClientTypeConnected` now reports true
  ASSERT_EQ(IsClientTypeConnected(), true);

  // Disconnect the client
  DisconnectClient();

  // Verify `IsClientTypeConnected` now reports false
  ASSERT_EQ(IsClientTypeConnected(), false);
}

TEST_F(ThermalTestControlTest, NoStateChange) {
  std::optional<uint64_t> state_result;

  ConnectClient();

  // Initial state should be 0
  state_result.reset();
  Watch(state_result);
  RunLoopUntilIdle();
  ASSERT_EQ(state_result, 0);

  // Another call to `Watch` should not complete since thermal state hasn't changed
  state_result.reset();
  Watch(state_result);
  RunLoopUntilIdle();
  ASSERT_EQ(state_result, std::nullopt);

  // No change to thermal state, then verify `Watch` still has no response
  SetThermalState(0);
  RunLoopUntilIdle();
  ASSERT_EQ(state_result, std::nullopt);
}

TEST_F(ThermalTestControlTest, SecondWatchWhilePendingClosesConnection) {
  ConnectClient();

  // Initial state should be 0.
  std::optional<uint64_t> first_result;
  Watch(first_result);
  RunLoopUntilIdle();
  ASSERT_EQ(first_result, 0);

  // Issue a second Watch call that hangs because thermal state hasn't changed.
  std::optional<uint64_t> pending_result;
  watcher_->Watch().Then(
      [&pending_result](fidl::Result<fuchsia_thermal::ClientStateWatcher::Watch>& result) {
        if (result.is_ok()) {
          pending_result = result->state();
        }
      });
  RunLoopUntilIdle();
  ASSERT_EQ(pending_result, std::nullopt);

  // Issue another Watch call while the second is still pending.
  // The server should close the watcher channel with ZX_ERR_BAD_STATE.
  std::optional<zx_status_t> epitaph;
  watcher_->Watch().Then(
      [&epitaph](fidl::Result<fuchsia_thermal::ClientStateWatcher::Watch>& result) {
        ASSERT_TRUE(result.is_error());
        ASSERT_TRUE(result.error_value().is_peer_closed());
        epitaph = result.error_value().status();
      });
  RunLoopUntilIdle();
  ASSERT_EQ(epitaph, ZX_ERR_BAD_STATE);

  // The watcher connection should be closed now.
  ASSERT_EQ(IsClientTypeConnected(), false);
}

TEST_F(ThermalTestControlTest, MultipleClientsSupported) {
  // Open a second ClientStateControl connection.
  zx::result client_state_control2 = svc_provider_.Connect<test_thermal::ClientStateControl>();
  ASSERT_OK(client_state_control2);
  fidl::Client<test_thermal::ClientStateControl> control2(std::move(client_state_control2.value()),
                                                          dispatcher());

  // Both connections should be active and able to query state concurrently.
  ASSERT_EQ(IsClientTypeConnected(), false);

  std::optional<bool> is_connected2;
  control2->IsClientTypeConnected({{.client_type = kTestClientType}})
      .Then([&is_connected2](
                fidl::Result<test_thermal::ClientStateControl::IsClientTypeConnected>& result) {
        ASSERT_TRUE(result.is_ok()) << result.error_value();
        is_connected2 = result->connected();
      });
  RunLoopUntilIdle();
  ASSERT_EQ(is_connected2, false);

  // Connect a watcher.
  ConnectClient();

  // Both the original and the second control connection observe the connection.
  ASSERT_EQ(IsClientTypeConnected(), true);

  is_connected2.reset();
  control2->IsClientTypeConnected({{.client_type = kTestClientType}})
      .Then([&is_connected2](
                fidl::Result<test_thermal::ClientStateControl::IsClientTypeConnected>& result) {
        ASSERT_TRUE(result.is_ok()) << result.error_value();
        is_connected2 = result->connected();
      });
  RunLoopUntilIdle();
  ASSERT_EQ(is_connected2, true);

  // Set state via the second control client; verify watcher receives it.
  bool updated = false;
  control2->SetThermalState({{.client_type = kTestClientType, .state = 42}})
      .Then([&updated](fidl::Result<test_thermal::ClientStateControl::SetThermalState>& result) {
        ASSERT_TRUE(result.is_ok()) << result.error_value();
        updated = true;
      });
  RunLoopUntilIdle();
  ASSERT_EQ(updated, true);

  std::optional<uint64_t> state_result;
  Watch(state_result);
  RunLoopUntilIdle();
  ASSERT_EQ(state_result, 42);
}
