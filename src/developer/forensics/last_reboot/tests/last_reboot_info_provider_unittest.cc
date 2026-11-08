// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/last_reboot/last_reboot_info_provider.h"

#include <fidl/fuchsia.feedback/cpp/fidl.h>

#include <optional>
#include <utility>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/feedback/reboot_log/final_shutdown_info.h"
#include "src/developer/forensics/testing/gpretty_printers.h"  // IWYU pragma: keep
#include "src/developer/forensics/testing/unit_test_fixture.h"

namespace forensics {
namespace last_reboot {
namespace {

using feedback::FinalShutdownReason;
using fuchsia_feedback::LastReboot;

class LastRebootInfoProviderTest : public UnitTestFixture {
 protected:
  LastReboot MakeLastReboot(const FinalShutdownReason reason,
                            const std::optional<zx::duration> uptime = std::nullopt,
                            const std::optional<zx::duration> runtime = std::nullopt) {
    const feedback::FinalShutdownInfo final_shutdown_info(reason, uptime, runtime,
                                                          /*critical_process=*/std::nullopt);

    LastRebootInfoProvider last_reboot_info_provider(final_shutdown_info);

    auto [client_end, server_end] =
        fidl::Endpoints<fuchsia_feedback::LastRebootInfoProvider>::Create();

    fidl::ServerBinding<fuchsia_feedback::LastRebootInfoProvider> binding(
        dispatcher(), std::move(server_end), &last_reboot_info_provider,
        fidl::kIgnoreBindingClosure);
    fidl::Client client(std::move(client_end), dispatcher());

    std::optional<LastReboot> out_last_reboot;
    client->Get().Then(
        [&out_last_reboot](fidl::Result<fuchsia_feedback::LastRebootInfoProvider::Get>& result) {
          ASSERT_TRUE(result.is_ok()) << result.error_value();
          out_last_reboot = std::move(result->last_reboot());
        });

    RunLoopUntilIdle();
    EXPECT_TRUE(out_last_reboot.has_value());

    return std::move(out_last_reboot).value_or(LastReboot());
  }
};

TEST_F(LastRebootInfoProviderTest, Succeed_Graceful) {
  const LastReboot last_reboot = MakeLastReboot(FinalShutdownReason::kGenericGraceful);

  ASSERT_TRUE(last_reboot.graceful().has_value());
  EXPECT_TRUE(last_reboot.graceful().value());

  EXPECT_FALSE(last_reboot.reason().has_value());
}

TEST_F(LastRebootInfoProviderTest, Succeed_NotGraceful) {
  const LastReboot last_reboot = MakeLastReboot(FinalShutdownReason::kKernelPanic);

  ASSERT_TRUE(last_reboot.graceful().has_value());
  EXPECT_FALSE(last_reboot.graceful().value());

  ASSERT_TRUE(last_reboot.reason().has_value());
  EXPECT_EQ(last_reboot.reason().value(), fuchsia_feedback::RebootReason::kKernelPanic);
}

TEST_F(LastRebootInfoProviderTest, Succeed_Planned) {
  const LastReboot last_reboot = MakeLastReboot(FinalShutdownReason::kSystemUpdate);

  ASSERT_TRUE(last_reboot.planned().has_value());
  EXPECT_TRUE(last_reboot.planned().value());

  ASSERT_TRUE(last_reboot.reason().has_value());
  EXPECT_EQ(last_reboot.reason().value(), fuchsia_feedback::RebootReason::kSystemUpdate);
}

TEST_F(LastRebootInfoProviderTest, Succeed_NotPlanned) {
  const LastReboot last_reboot = MakeLastReboot(FinalShutdownReason::kUserRequest);

  ASSERT_TRUE(last_reboot.planned().has_value());
  EXPECT_FALSE(last_reboot.planned().value());

  ASSERT_TRUE(last_reboot.reason().has_value());
  EXPECT_EQ(last_reboot.reason().value(), fuchsia_feedback::RebootReason::kUserRequest);
}

TEST_F(LastRebootInfoProviderTest, Succeed_HasUptime) {
  const zx::duration uptime = zx::msec(100);

  const LastReboot last_reboot = MakeLastReboot(FinalShutdownReason::kUserRequest, uptime);

  ASSERT_TRUE(last_reboot.uptime().has_value());
  EXPECT_EQ(last_reboot.uptime().value(), uptime.to_nsecs());
}

TEST_F(LastRebootInfoProviderTest, Succeed_DoesNotHaveUptime) {
  const LastReboot last_reboot = MakeLastReboot(FinalShutdownReason::kUserRequest,
                                                /*uptime=*/std::nullopt);

  EXPECT_FALSE(last_reboot.uptime().has_value());
}

TEST_F(LastRebootInfoProviderTest, Succeed_HasRuntime) {
  const zx::duration runtime = zx::msec(78);

  const LastReboot last_reboot = MakeLastReboot(FinalShutdownReason::kUserRequest,
                                                /*uptime=*/std::nullopt, runtime);

  ASSERT_TRUE(last_reboot.runtime().has_value());
  EXPECT_EQ(last_reboot.runtime().value(), runtime.to_nsecs());
}

TEST_F(LastRebootInfoProviderTest, Succeed_DoesNotHaveRuntime) {
  const LastReboot last_reboot = MakeLastReboot(FinalShutdownReason::kUserRequest,
                                                /*uptime=*/std::nullopt,
                                                /*runtime=*/std::nullopt);

  EXPECT_FALSE(last_reboot.runtime().has_value());
}

TEST_F(LastRebootInfoProviderTest, Succeed_NotParseable) {
  const LastReboot last_reboot = MakeLastReboot(FinalShutdownReason::kNotParseable);

  EXPECT_FALSE(last_reboot.graceful().has_value());
  EXPECT_FALSE(last_reboot.reason().has_value());
}

}  // namespace
}  // namespace last_reboot
}  // namespace forensics
