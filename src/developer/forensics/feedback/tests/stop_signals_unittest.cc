// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/stop_signals.h"

#include <fidl/fuchsia.hardware.power.statecontrol/cpp/fidl.h>
#include <fidl/fuchsia.process.lifecycle/cpp/fidl.h>
#include <lib/async/cpp/executor.h>
#include <lib/fit/function.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/testing/gpretty_printers.h"  // IWYU pragma: keep
#include "src/developer/forensics/testing/unit_test_fixture.h"

namespace forensics::feedback {
namespace {

using WaitForLifecycleStopTest = UnitTestFixture;
using ::fuchsia_hardware_power_statecontrol::ShutdownAction;
using ::fuchsia_hardware_power_statecontrol::ShutdownOptions;
using ::fuchsia_hardware_power_statecontrol::ShutdownReason;
using ::fuchsia_hardware_power_statecontrol::ShutdownWatcher;
using ::fuchsia_process_lifecycle::Lifecycle;

class LifecycleEventHandler : public fidl::AsyncEventHandler<Lifecycle> {
 public:
  LifecycleEventHandler() : is_bound_(true) {}

  void on_fidl_error(const fidl::UnbindInfo info) override { is_bound_ = false; }
  bool is_bound() const { return is_bound_; }

 private:
  bool is_bound_;
};

class ShutdownWatcherEventHandler : public fidl::AsyncEventHandler<ShutdownWatcher> {
 public:
  ShutdownWatcherEventHandler() : is_bound_(true) {}

  void on_fidl_error(const fidl::UnbindInfo info) override { is_bound_ = false; }
  void handle_unknown_event(const fidl::UnknownEventMetadata<ShutdownWatcher> metadata) override {}
  bool is_bound() const { return is_bound_; }

 private:
  bool is_bound_;
};

TEST_F(WaitForLifecycleStopTest, BadChannel) {
  async::Executor executor(dispatcher());

  std::optional<Error> error;
  fidl::ServerEnd<Lifecycle> server_end;
  executor.schedule_task(
      WaitForLifecycleStop(dispatcher(), std::move(server_end)).or_else([&error](const Error& e) {
        error = e;
        return fpromise::error();
      }));

  RunLoopUntilIdle();
  EXPECT_EQ(error, Error::kBadValue);
}

TEST_F(WaitForLifecycleStopTest, ClientDisconnects) {
  async::Executor executor(dispatcher());

  std::optional<Error> error;
  auto [client_end, server_end] = fidl::Endpoints<Lifecycle>::Create();
  executor.schedule_task(
      WaitForLifecycleStop(dispatcher(), std::move(server_end)).or_else([&error](const Error& e) {
        error = e;
        return fpromise::error();
      }));

  client_end.reset();
  RunLoopUntilIdle();
  EXPECT_EQ(error, Error::kConnectionError);
}

TEST_F(WaitForLifecycleStopTest, ServerDisconnectsOnCallbackExecution) {
  async::Executor executor(dispatcher());

  std::optional<LifecycleStopSignal> signal;
  auto [client_end, server_end] = fidl::Endpoints<Lifecycle>::Create();
  LifecycleEventHandler event_handler;
  const fidl::Client client(std::move(client_end), dispatcher(), &event_handler);
  executor.schedule_task(WaitForLifecycleStop(dispatcher(), std::move(server_end))
                             .and_then([&signal](LifecycleStopSignal& s) {
                               signal = std::move(s);
                               return fpromise::ok();
                             }));

  ASSERT_TRUE(client->Stop().is_ok());
  RunLoopUntilIdle();
  EXPECT_TRUE(event_handler.is_bound());
  ASSERT_NE(signal, std::nullopt);

  signal->Respond();
  RunLoopUntilIdle();
  EXPECT_FALSE(event_handler.is_bound());
}

TEST_F(WaitForLifecycleStopTest, ServerDisconnectsOnCallbackDeletion) {
  async::Executor executor(dispatcher());

  std::optional<LifecycleStopSignal> signal;
  auto [client_end, server_end] = fidl::Endpoints<Lifecycle>::Create();
  LifecycleEventHandler event_handler;
  const fidl::Client client(std::move(client_end), dispatcher(), &event_handler);
  executor.schedule_task(WaitForLifecycleStop(dispatcher(), std::move(server_end))
                             .and_then([&signal](LifecycleStopSignal& s) {
                               signal = std::move(s);
                               return fpromise::ok();
                             }));

  ASSERT_TRUE(client->Stop().is_ok());
  RunLoopUntilIdle();
  EXPECT_TRUE(event_handler.is_bound());
  ASSERT_NE(signal, std::nullopt);

  signal = std::nullopt;
  RunLoopUntilIdle();
  EXPECT_FALSE(event_handler.is_bound());
}

using WaitForRebootReasonTest = UnitTestFixture;

TEST_F(WaitForRebootReasonTest, BadChannel) {
  async::Executor executor(dispatcher());

  std::optional<Error> error;
  fidl::ServerEnd<ShutdownWatcher> server_end;
  executor.schedule_task(
      WaitForShutdownInfo(dispatcher(), std::move(server_end)).or_else([&error](const Error& e) {
        error = e;
        return fpromise::error();
      }));

  RunLoopUntilIdle();
  EXPECT_EQ(error, Error::kBadValue);
}

TEST_F(WaitForRebootReasonTest, ClientDisconnects) {
  async::Executor executor(dispatcher());

  std::optional<Error> error;
  auto [client_end, server_end] = fidl::Endpoints<ShutdownWatcher>::Create();
  executor.schedule_task(
      WaitForShutdownInfo(dispatcher(), std::move(server_end)).or_else([&error](const Error& e) {
        error = e;
        return fpromise::error();
      }));

  client_end.reset();
  RunLoopUntilIdle();
  EXPECT_EQ(error, Error::kConnectionError);
}

TEST_F(WaitForRebootReasonTest, ServerDisconnectsOnCallbackExecution) {
  async::Executor executor(dispatcher());

  std::optional<GracefulShutdownInfoSignal> signal;
  auto [client_end, server_end] = fidl::Endpoints<ShutdownWatcher>::Create();
  ShutdownWatcherEventHandler event_handler;
  const fidl::Client client(std::move(client_end), dispatcher(), &event_handler);
  executor.schedule_task(WaitForShutdownInfo(dispatcher(), std::move(server_end))
                             .and_then([&signal](GracefulShutdownInfoSignal& s) {
                               signal = std::move(s);
                               return fpromise::ok();
                             }));

  bool called{false};

  ShutdownOptions options;
  std::vector<ShutdownReason> reasons = {ShutdownReason::kUserRequest};
  options.reasons(reasons);
  options.action(ShutdownAction::kReboot);
  client->OnShutdown({std::move(options)})
      .Then([&called](const fidl::Result<ShutdownWatcher::OnShutdown>& result) {
        EXPECT_TRUE(result.is_ok());
        called = true;
      });

  RunLoopUntilIdle();
  EXPECT_TRUE(event_handler.is_bound());
  ASSERT_NE(signal, std::nullopt);
  EXPECT_THAT(signal->Reasons(), testing::ElementsAre(GracefulShutdownReason::kUserRequest));

  signal->Respond();
  RunLoopUntilIdle();
  EXPECT_TRUE(called);
  EXPECT_FALSE(event_handler.is_bound());
}

TEST_F(WaitForRebootReasonTest, ServerDisconnectsOnCallbackDeletion) {
  async::Executor executor(dispatcher());

  std::optional<GracefulShutdownInfoSignal> signal;
  auto [client_end, server_end] = fidl::Endpoints<ShutdownWatcher>::Create();
  ShutdownWatcherEventHandler event_handler;
  const fidl::Client client(std::move(client_end), dispatcher(), &event_handler);
  executor.schedule_task(WaitForShutdownInfo(dispatcher(), std::move(server_end))
                             .and_then([&signal](GracefulShutdownInfoSignal& s) {
                               signal = std::move(s);
                               return fpromise::ok();
                             }));

  bool called{false};

  ShutdownOptions options;
  std::vector<ShutdownReason> reasons = {ShutdownReason::kUserRequest};
  options.reasons(reasons);
  options.action(ShutdownAction::kReboot);
  client->OnShutdown({std::move(options)})
      .Then([&called](const fidl::Result<ShutdownWatcher::OnShutdown>& result) {
        EXPECT_TRUE(result.is_ok());
        called = true;
      });

  RunLoopUntilIdle();
  ASSERT_NE(signal, std::nullopt);
  EXPECT_THAT(signal->Reasons(), testing::ElementsAre(GracefulShutdownReason::kUserRequest));

  signal = std::nullopt;
  RunLoopUntilIdle();
  EXPECT_TRUE(called);
  EXPECT_FALSE(event_handler.is_bound());
}

TEST_F(WaitForRebootReasonTest, NoCompletionOnNoAction) {
  async::Executor executor(dispatcher());

  std::optional<GracefulShutdownInfoSignal> signal;
  auto [client_end, server_end] = fidl::Endpoints<ShutdownWatcher>::Create();
  const fidl::Client client(std::move(client_end), dispatcher());
  executor.schedule_task(WaitForShutdownInfo(dispatcher(), std::move(server_end))
                             .and_then([&signal](GracefulShutdownInfoSignal& s) {
                               signal = std::move(s);
                               return fpromise::ok();
                             }));

  bool called{false};

  ShutdownOptions options;
  std::vector<ShutdownReason> reasons = {ShutdownReason::kUserRequest};
  options.reasons(reasons);
  client->OnShutdown({std::move(options)})
      .Then([&called](const fidl::Result<ShutdownWatcher::OnShutdown>& result) {
        EXPECT_TRUE(result.is_ok());
        called = true;
      });

  RunLoopUntilIdle();
  EXPECT_EQ(signal, std::nullopt);
  EXPECT_TRUE(called);
}

struct TestParam {
  std::string test_name;
  ShutdownAction action;
  GracefulShutdownAction expected_action;
};

class WaitForRebootReasonParameterizedTest : public UnitTestFixture,
                                             public testing::WithParamInterface<TestParam> {};

INSTANTIATE_TEST_SUITE_P(WithVariousShutdownActions, WaitForRebootReasonParameterizedTest,
                         ::testing::ValuesIn(std::vector<TestParam>({
                             {
                                 "Poweroff",
                                 ShutdownAction::kPoweroff,
                                 GracefulShutdownAction::kPoweroff,
                             },
                             {
                                 "Reboot",
                                 ShutdownAction::kReboot,
                                 GracefulShutdownAction::kReboot,
                             },
                             {
                                 "RebootToBootloader",
                                 ShutdownAction::kRebootToBootloader,
                                 GracefulShutdownAction::kRebootToBootloader,
                             },
                             {
                                 "RebootToRecovery",
                                 ShutdownAction::kRebootToRecovery,
                                 GracefulShutdownAction::kRebootToRecovery,
                             },
                         })),
                         [](const testing::TestParamInfo<TestParam>& info) {
                           return info.param.test_name;
                         });

TEST_P(WaitForRebootReasonParameterizedTest, CompletionWithExpectedAction) {
  const TestParam& param = GetParam();
  async::Executor executor(dispatcher());

  std::optional<GracefulShutdownInfoSignal> signal;
  auto [client_end, server_end] = fidl::Endpoints<ShutdownWatcher>::Create();
  const fidl::Client client(std::move(client_end), dispatcher());
  executor.schedule_task(WaitForShutdownInfo(dispatcher(), std::move(server_end))
                             .and_then([&signal](GracefulShutdownInfoSignal& s) {
                               signal = std::move(s);
                               return fpromise::ok();
                             }));

  bool called{false};

  ShutdownOptions options;
  std::vector<ShutdownReason> reasons = {ShutdownReason::kUserRequest};
  options.reasons(reasons);
  options.action(param.action);
  client->OnShutdown({std::move(options)})
      .Then([&called](const fidl::Result<ShutdownWatcher::OnShutdown>& result) {
        EXPECT_TRUE(result.is_ok());
        called = true;
      });

  RunLoopUntilIdle();
  ASSERT_TRUE(signal.has_value());
  EXPECT_EQ(signal->Action(), param.expected_action);
  EXPECT_THAT(signal->Reasons(), testing::ElementsAre(GracefulShutdownReason::kUserRequest));
}

}  // namespace
}  // namespace forensics::feedback
