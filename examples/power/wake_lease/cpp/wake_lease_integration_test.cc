// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.power.broker/cpp/test_base.h>
#include <fidl/fuchsia.power.system/cpp/test_base.h>
#include <lib/async/cpp/executor.h>
#include <lib/async/dispatcher.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/driver/power/cpp/testing/fake_activity_governor.h>
#include <lib/driver/power/cpp/testing/fidl_bound_server.h>
#include <lib/fidl/cpp/client.h>
#include <lib/fpromise/result.h>
#include <lib/zx/eventpair.h>
#include <lib/zx/time.h>

#include <memory>
#include <optional>
#include <string>
#include <vector>

#include <gtest/gtest.h>
#include <src/lib/testing/loop_fixture/real_loop_fixture.h>
#include <src/lib/testing/predicates/status.h>

#include "examples/power/wake_lease/cpp/wake_lease.h"

namespace {

using fdf_power::testing::FakeSuspendBlocker;
using fdf_power::testing::FidlBoundServer;
using fuchsia_power_broker::DependencyToken;
using fuchsia_power_broker::LeaseDependency;
using fuchsia_power_broker::LeaseSchema;
using fuchsia_power_broker::Topology;
using fuchsia_power_system::ActivityGovernor;
using fuchsia_power_system::ApplicationActivityLevel;
using fuchsia_power_system::SuspendBlocker;

class WakeLeaseIntegrationTest : public gtest::RealLoopFixture {
 protected:
  template <typename Protocol>
  fidl::Client<Protocol> Connect() {
    zx::result<fidl::ClientEnd<Protocol>> result = component::Connect<Protocol>();
    EXPECT_OK(result);
    return fidl::Client(std::move(*result), dispatcher());
  }
};

// TODO(b/356953708): This test is currently broken on specific environments.
// Re-enable once fixed.
TEST_F(WakeLeaseIntegrationTest, DISABLED_WakeLeaseBlocksSuspend) {
  auto topology = Connect<Topology>();
  auto activity_governor = Connect<ActivityGovernor>();

  // Take an assertive lease on ApplicationActivity to indicate boot completion.
  // System Activity Governor waits for this signal before handling suspend or resume.
  zx::eventpair activity_lease;
  bool lease_completed = false;
  activity_governor->GetPowerElements().Then([&](auto& result) {
    ASSERT_OK(result);
    ASSERT_TRUE(result->application_activity().has_value());
    ASSERT_TRUE(result->application_activity()->assertive_dependency_token().has_value());
    DependencyToken activity_token =
        std::move(result->application_activity()->assertive_dependency_token().value());

    zx::eventpair lease_server;
    ASSERT_OK(zx::eventpair::create(0, &activity_lease, &lease_server));

    LeaseSchema schema;
    schema.lease_token(std::move(lease_server));
    schema.lease_name("boot-complete-lease");
    schema.dependencies().emplace().push_back(LeaseDependency{{
        .requires_token = std::move(activity_token),
        .requires_level = fidl::ToUnderlying(ApplicationActivityLevel::kActive),
    }});

    topology->Lease(std::move(schema)).Then([&](auto& lease_result) {
      ASSERT_OK(lease_result);
      lease_completed = true;
    });
  });
  RunLoopUntil([&] { return lease_completed || HasFatalFailure(); });
  ASSERT_TRUE(lease_completed);

  // Register a suspend blocker on System Activity Governor to check for suspend callbacks.
  auto endpoints = fidl::CreateEndpoints<SuspendBlocker>().value();
  FidlBoundServer<FakeSuspendBlocker> suspend_blocker(dispatcher(), std::move(endpoints.server));
  bool register_blocker_completed = false;
  activity_governor
      ->RegisterSuspendBlocker(
          {{.suspend_blocker = std::make_optional(std::move(endpoints.client))}})
      .Then([&register_blocker_completed](auto& result) { register_blocker_completed = true; });
  RunLoopUntil([&register_blocker_completed]() { return register_blocker_completed; });
  EXPECT_TRUE(register_blocker_completed);
  ASSERT_FALSE(suspend_blocker.SuspendStarted());

  // Take a wake lease and check that BeforeSuspend doesn't get called.
  std::unique_ptr<examples::power::WakeLease> wake_lease;
  bool take_wake_lease_completed = false;
  async::Executor executor(dispatcher());
  executor.schedule_task(
      examples::power::WakeLease::Take(activity_governor, "test-wake-lease")
          .then([&](fpromise::result<examples::power::WakeLease, examples::power::Error>& result) {
            EXPECT_FALSE(result.is_error()) << result.error();
            ASSERT_FALSE(suspend_blocker.SuspendStarted());

            wake_lease = std::make_unique<examples::power::WakeLease>(result.take_value());
            take_wake_lease_completed = true;
          }));
  RunLoopUntil([&take_wake_lease_completed]() { return take_wake_lease_completed; });
  EXPECT_TRUE(take_wake_lease_completed);
  EXPECT_TRUE(wake_lease);

  // Dropping the ApplicationActivity lease shouldn't suspend the system as long as the wake lease
  // is active.
  activity_lease.reset();
  RunLoopUntilIdle();
  ASSERT_FALSE(suspend_blocker.SuspendStarted());

  // Drop the wake lease and observe OnSuspend callback.
  wake_lease.reset();
  EXPECT_FALSE(wake_lease);
  RunLoopUntil([&suspend_blocker]() { return suspend_blocker.SuspendStarted(); });
  ASSERT_TRUE(suspend_blocker.SuspendStarted());
}

}  // namespace
