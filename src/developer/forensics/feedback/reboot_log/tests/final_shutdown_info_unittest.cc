// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/reboot_log/final_shutdown_info.h"

#include <fidl/fuchsia.feedback/cpp/fidl.h>
#include <fidl/fuchsia.feedback/cpp/natural_ostream.h>

#include <limits>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/feedback/config.h"
#include "src/developer/forensics/feedback/reboot_log/hw_shutdown_reason.h"
#include "src/developer/forensics/testing/gpretty_printers.h"  // IWYU pragma: keep
#include "src/developer/forensics/utils/cobalt/metrics.h"

namespace forensics::feedback {
namespace {

TEST(FinalShutdownInfoTest, NotParseable) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kNotParseable);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(), cobalt::LastRebootReason::kUnknown);
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous),
            "fuchsia-reboot-log-not-parseable");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "reboot-log");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(), std::nullopt);
}

TEST(FinalShutdownInfoTest, Cold) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info = std::make_unique<FinalShutdownInfo>(
      FinalShutdownReason::kCold, GracefulShutdownAction::kPoweroff);

  EXPECT_FALSE(final_shutdown_info->IsCrash());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(), cobalt::LastRebootReason::kCold);
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(), fuchsia_feedback::RebootReason::kCold);
  EXPECT_EQ(final_shutdown_info->ToGracefulShutdownAction(), GracefulShutdownAction::kPoweroff);
}

TEST(FinalShutdownInfoTest, Spontaneous) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kSpontaneousReboot);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kBriefPowerLoss);
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous),
            "fuchsia-spontaneous-reboot");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "device");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia_feedback::RebootReason::kBriefPowerLoss);
}

TEST(FinalShutdownInfoTest, BriefPowerLoss) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kSpontaneousReboot);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kBriefPowerLoss);
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kBriefPowerLoss),
            "fuchsia-brief-power-loss");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "device");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia_feedback::RebootReason::kBriefPowerLoss);
}

TEST(FinalShutdownInfoTest, HardReset) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kSpontaneousReboot);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kBriefPowerLoss);
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kHardReset),
            "fuchsia-hard-reset");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "device");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia_feedback::RebootReason::kBriefPowerLoss);
}

TEST(FinalShutdownInfoTest, SpontaneousDuringShutdown) {
  const FinalShutdownInfo final_shutdown_info(FinalShutdownReason::kSpontaneousRebootDuringShutdown,
                                              GracefulShutdownAction::kRebootToBootloader);

  EXPECT_TRUE(final_shutdown_info.IsCrash());
  EXPECT_EQ(final_shutdown_info.OptionallyGraceful(), std::make_optional(false));
  EXPECT_EQ(final_shutdown_info.OptionallyPlanned(), std::make_optional(false));
  EXPECT_EQ(final_shutdown_info.ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kSpontaneousRebootDuringShutdown);
  EXPECT_EQ(final_shutdown_info.ToCrashSignature(SpontaneousRebootReason::kSpontaneous),
            "fuchsia-spontaneous-reboot-during-shutdown");
  EXPECT_EQ(final_shutdown_info.ToCrashSignature(SpontaneousRebootReason::kBriefPowerLoss),
            "fuchsia-brief-power-loss-during-shutdown");
  EXPECT_EQ(final_shutdown_info.ToCrashSignature(SpontaneousRebootReason::kHardReset),
            "fuchsia-hard-reset-during-shutdown");
  EXPECT_EQ(final_shutdown_info.ToCrashProgramName(), "device");
  EXPECT_EQ(final_shutdown_info.ToFidlRebootReason(),
            fuchsia_feedback::RebootReason::kSpontaneousRebootDuringShutdown);
  EXPECT_EQ(final_shutdown_info.ToGracefulShutdownAction(),
            GracefulShutdownAction::kRebootToBootloader);
  EXPECT_EQ(final_shutdown_info.ToSnapshotAnnotationGracefulAction(),
            ErrorOrString("reboot to bootloader"));
}

TEST(FinalShutdownInfoTest, KernelPanic) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kKernelPanic);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kKernelPanic);
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous),
            "fuchsia-kernel-panic");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "kernel");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia_feedback::RebootReason::kKernelPanic);
}

TEST(FinalShutdownInfoTest, OOM) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kOom);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kSystemOutOfMemory);
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous),
            "fuchsia-oom");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "system");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia_feedback::RebootReason::kSystemOutOfMemory);
}

TEST(FinalShutdownInfoTest, HardwareWatchdogTimeout) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kHwWatchdog);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kHardwareWatchdogTimeout);
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous),
            "fuchsia-hw-watchdog-timeout");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "device");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia_feedback::RebootReason::kHardwareWatchdogTimeout);
}

TEST(FinalShutdownInfoTest, UserHardReset) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kUserHardReset);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous),
            "fuchsia-hard-reset-user-requested");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "device");
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kUserHardReset);
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia_feedback::RebootReason::kUserHardReset);
}

TEST(FinalShutdownInfoTest, SoftwareWatchdogTimeout) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kSwWatchdog);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kSoftwareWatchdogTimeout);
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous),
            "fuchsia-sw-watchdog-timeout");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "system");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia_feedback::RebootReason::kSoftwareWatchdogTimeout);
}

TEST(FinalShutdownInfoTest, Brownout) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kBrownout);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(), cobalt::LastRebootReason::kBrownout);
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous),
            "fuchsia-brownout");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "device");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(), fuchsia_feedback::RebootReason::kBrownout);
}

TEST(FinalShutdownInfoTest, RootJobTerminationNoCriticalProcess) {
  FinalShutdownInfo final_shutdown_info(FinalShutdownReason::kRootJobTermination);

  EXPECT_TRUE(final_shutdown_info.IsCrash());
  EXPECT_EQ(final_shutdown_info.ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kRootJobTermination);
  EXPECT_EQ(final_shutdown_info.ToCrashSignature(SpontaneousRebootReason::kSpontaneous),
            "fuchsia-root-job-termination");
  EXPECT_EQ(final_shutdown_info.ToCrashProgramName(), "system");
  EXPECT_EQ(final_shutdown_info.ToFidlRebootReason(),
            fuchsia_feedback::RebootReason::kRootJobTermination);
}

TEST(FinalShutdownInfoTest, RootJobTerminationWithCriticalProcess) {
  const FinalShutdownInfo final_shutdown_info(FinalShutdownReason::kRootJobTermination,
                                              /*uptime=*/std::nullopt, /*runtime=*/std::nullopt,
                                              "critical_process");

  EXPECT_TRUE(final_shutdown_info.IsCrash());
  EXPECT_EQ(final_shutdown_info.ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kRootJobTermination);
  EXPECT_EQ(final_shutdown_info.ToCrashSignature(SpontaneousRebootReason::kSpontaneous),
            "fuchsia-reboot-critical_process-terminated");
  EXPECT_EQ(final_shutdown_info.ToCrashProgramName(), "system");
  EXPECT_EQ(final_shutdown_info.ToFidlRebootReason(),
            fuchsia_feedback::RebootReason::kRootJobTermination);
}

struct GracefulNoReportTestParams {
  std::string test_name;
  std::vector<GracefulShutdownReason> reasons;
  cobalt::LastRebootReason expected_cobalt_reason;
  fuchsia_feedback::RebootReason expected_fidl_reboot_reason;
};

class FinalShutdownInfoGracefulNoReportTest
    : public testing::TestWithParam<GracefulNoReportTestParams> {};

INSTANTIATE_TEST_SUITE_P(WithVariousReasons, FinalShutdownInfoGracefulNoReportTest,
                         ::testing::ValuesIn(std::vector<GracefulNoReportTestParams>({
                             {
                                 "SystemUpdateAndNetstackMigration",
                                 {
                                     GracefulShutdownReason::kSystemUpdate,
                                     GracefulShutdownReason::kNetstackMigration,
                                 },
                                 cobalt::LastRebootReason::kSystemUpdate,
                                 fuchsia_feedback::RebootReason::kSystemUpdate,
                             },
                             {
                                 "UserRequest",
                                 {GracefulShutdownReason::kUserRequest},
                                 cobalt::LastRebootReason::kUserRequest,
                                 fuchsia_feedback::RebootReason::kUserRequest,
                             },
                             {
                                 "SystemUpdate",
                                 {GracefulShutdownReason::kSystemUpdate},
                                 cobalt::LastRebootReason::kSystemUpdate,
                                 fuchsia_feedback::RebootReason::kSystemUpdate,
                             },
                             {
                                 "ZbiSwap",
                                 {GracefulShutdownReason::kZbiSwap},
                                 cobalt::LastRebootReason::kZbiSwap,
                                 fuchsia_feedback::RebootReason::kZbiSwap,
                             },
                             {
                                 "GracefulFdr",
                                 {GracefulShutdownReason::kFdr},
                                 cobalt::LastRebootReason::kFactoryDataReset,
                                 fuchsia_feedback::RebootReason::kFactoryDataReset,
                             },
                             {
                                 "NetstackMigration",
                                 {GracefulShutdownReason::kNetstackMigration},
                                 cobalt::LastRebootReason::kNetstackMigration,
                                 fuchsia_feedback::RebootReason::kNetstackMigration,
                             },
                             {
                                 "AndroidNoReason",
                                 {GracefulShutdownReason::kAndroidNoReason},
                                 cobalt::LastRebootReason::kAndroidNoReason,
                                 fuchsia_feedback::RebootReason::kAndroidNoReason,
                             },
                             {
                                 "BatteryDrained",
                                 {GracefulShutdownReason::kBatteryDrained},
                                 cobalt::LastRebootReason::kBatteryDrained,
                                 fuchsia_feedback::RebootReason::kBatteryDrained,
                             },
                         })),
                         [](const testing::TestParamInfo<GracefulNoReportTestParams>& info) {
                           return info.param.test_name;
                         });

TEST_P(FinalShutdownInfoGracefulNoReportTest, CheckProperties) {
  const GracefulNoReportTestParams& params = GetParam();
  const FinalShutdownInfo final_shutdown_info = FinalShutdownInfo::MakeFinalShutdownInfo(
      HwShutdownReason::kWarm, ZirconShutdownReason::kNoCrash,
      GracefulShutdownInfo{GracefulShutdownAction::kNotParseable, params.reasons},
      /*not_a_fdr=*/true, /*supports_user_initiated_poweroffs=*/false,
      /*uptime=*/std::nullopt, /*runtime=*/std::nullopt, /*critical_process=*/std::nullopt);

  EXPECT_FALSE(final_shutdown_info.IsCrash());
  EXPECT_EQ(final_shutdown_info.ToCobaltLastRebootReason(), params.expected_cobalt_reason);
  EXPECT_EQ(final_shutdown_info.ToFidlRebootReason(), params.expected_fidl_reboot_reason);
}

struct GracefulTestParams {
  std::string test_name;
  std::vector<GracefulShutdownReason> reasons;
  cobalt::LastRebootReason expected_cobalt_reason;
  std::optional<fuchsia_feedback::RebootReason> expected_fidl_reboot_reason;
  std::string expected_crash_signature;
  std::string expected_crash_program_name;
};

class FinalShutdownInfoGracefulTest : public testing::TestWithParam<GracefulTestParams> {};

INSTANTIATE_TEST_SUITE_P(WithVariousReasons, FinalShutdownInfoGracefulTest,
                         ::testing::ValuesIn(std::vector<GracefulTestParams>(
                             {{
                                  "GenericGraceful",
                                  /*reasons=*/{},
                                  cobalt::LastRebootReason::kGenericGraceful,
                                  /*expected_fidl_reboot_reason=*/std::nullopt,
                                  "fuchsia-shutdown-undetermined-userspace-reason",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "UnexpectedMultipleGraceful",
                                  {
                                      GracefulShutdownReason::kAndroidCriticalProcessFailure,
                                      GracefulShutdownReason::kSessionFailure,
                                  },
                                  cobalt::LastRebootReason::kUnexpectedReasonGraceful,
                                  /*expected_fidl_reboot_reason=*/std::nullopt,
                                  "fuchsia-shutdown-unexpected-userspace-reason",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "HighTemperature",
                                  {GracefulShutdownReason::kHighTemperature},
                                  cobalt::LastRebootReason::kHighTemperature,
                                  fuchsia_feedback::RebootReason::kHighTemperature,
                                  "fuchsia-shutdown-high-temperature",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "SessionFailure",
                                  {GracefulShutdownReason::kSessionFailure},
                                  cobalt::LastRebootReason::kSessionFailure,
                                  fuchsia_feedback::RebootReason::kSessionFailure,
                                  "fuchsia-session-failure",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "SysmgrFailure",
                                  {GracefulShutdownReason::kSysmgrFailure},
                                  cobalt::LastRebootReason::kSysmgrFailure,
                                  fuchsia_feedback::RebootReason::kSysmgrFailure,
                                  "fuchsia-sysmgr-failure",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "CriticalComponentFailure",
                                  {GracefulShutdownReason::kCriticalComponentFailure},
                                  cobalt::LastRebootReason::kCriticalComponentFailure,
                                  fuchsia_feedback::RebootReason::kCriticalComponentFailure,
                                  "fuchsia-critical-component-failure",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "CriticalDriverFailure",
                                  {GracefulShutdownReason::kCriticalDriverFailure},
                                  cobalt::LastRebootReason::kCriticalDriverFailure,
                                  fuchsia_feedback::RebootReason::kCriticalDriverFailure,
                                  "fuchsia-critical-driver-failure",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "RetrySystemUpdate",
                                  {GracefulShutdownReason::kRetrySystemUpdate},
                                  cobalt::LastRebootReason::kRetrySystemUpdate,
                                  fuchsia_feedback::RebootReason::kRetrySystemUpdate,
                                  "fuchsia-retry-system-update",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "OOM",
                                  {GracefulShutdownReason::kOutOfMemory},
                                  cobalt::LastRebootReason::kSystemOutOfMemory,
                                  fuchsia_feedback::RebootReason::kSystemOutOfMemory,
                                  "fuchsia-oom",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "AndroidUnexpectedReason",
                                  {GracefulShutdownReason::kAndroidUnexpectedReason},
                                  cobalt::LastRebootReason::kAndroidUnexpectedReason,
                                  fuchsia_feedback::RebootReason::kAndroidUnexpectedReason,
                                  "fuchsia-shutdown-android-unexpected-reason",
                                  /*expected_crash_program_name=*/"android",
                              },
                              {
                                  "AndroidRescueParty",
                                  {GracefulShutdownReason::kAndroidRescueParty},
                                  cobalt::LastRebootReason::kAndroidRescueParty,
                                  fuchsia_feedback::RebootReason::kAndroidRescueParty,
                                  "fuchsia-shutdown-android-rescue-party",
                                  /*expected_crash_program_name=*/"android",
                              },
                              {
                                  "AndroidCriticalProcessFailure",
                                  {GracefulShutdownReason::kAndroidCriticalProcessFailure},
                                  cobalt::LastRebootReason::kAndroidCriticalProcessFailure,
                                  fuchsia_feedback::RebootReason::kAndroidCriticalProcessFailure,
                                  "fuchsia-shutdown-android-critical-process-failure",
                                  /*expected_crash_program_name=*/"android",
                              },
                              {
                                  "UserRequestDeviceStuck",
                                  {GracefulShutdownReason::kUserRequestDeviceStuck},
                                  cobalt::LastRebootReason::kUserRequestDeviceStuck,
                                  fuchsia_feedback::RebootReason::kUserRequestDeviceStuck,
                                  "fuchsia-shutdown-user-request-device-stuck",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "SuspensionFailure",
                                  {GracefulShutdownReason::kSuspensionFailure},
                                  cobalt::LastRebootReason::kSuspensionFailure,
                                  fuchsia_feedback::RebootReason::kSuspensionFailure,
                                  "fuchsia-shutdown-suspension-failure",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "PowerLevelTransitionStuck",
                                  {GracefulShutdownReason::kPowerLevelTransitionStuck},
                                  cobalt::LastRebootReason::kPowerLevelTransitionStuck,
                                  fuchsia_feedback::RebootReason::kPowerLevelTransitionStuck,
                                  "fuchsia-shutdown-power-level-transition-stuck",
                                  /*expected_crash_program_name=*/"system",
                              }})),
                         [](const testing::TestParamInfo<GracefulTestParams>& info) {
                           return info.param.test_name;
                         });

TEST_P(FinalShutdownInfoGracefulTest, CheckProperties) {
  const GracefulTestParams& params = GetParam();
  const FinalShutdownInfo final_shutdown_info = FinalShutdownInfo::MakeFinalShutdownInfo(
      HwShutdownReason::kWarm, ZirconShutdownReason::kNoCrash,
      GracefulShutdownInfo{GracefulShutdownAction::kReboot, params.reasons},
      /*not_a_fdr=*/true, /*supports_user_initiated_poweroffs=*/false,
      /*uptime=*/std::nullopt, /*runtime=*/std::nullopt, /*critical_process=*/std::nullopt);

  EXPECT_TRUE(final_shutdown_info.IsCrash());
  EXPECT_EQ(final_shutdown_info.ToCobaltLastRebootReason(), params.expected_cobalt_reason);
  EXPECT_EQ(final_shutdown_info.ToFidlRebootReason(), params.expected_fidl_reboot_reason);

  EXPECT_EQ(final_shutdown_info.ToCrashSignature(SpontaneousRebootReason::kSpontaneous),
            params.expected_crash_signature);

  EXPECT_EQ(final_shutdown_info.ToCrashProgramName(), params.expected_crash_program_name);
}

TEST(FinalShutdownInfoGracefulTest, InferredFdr) {
  const FinalShutdownInfo final_shutdown_info = FinalShutdownInfo::MakeFinalShutdownInfo(
      HwShutdownReason::kWarm, ZirconShutdownReason::kNoCrash,
      /*graceful_shutdown_info=*/std::nullopt,
      /*not_a_fdr=*/false, /*supports_user_initiated_poweroffs=*/false,
      /*uptime=*/std::nullopt, /*runtime=*/std::nullopt, /*critical_process=*/std::nullopt);

  EXPECT_FALSE(final_shutdown_info.IsCrash());
  EXPECT_EQ(final_shutdown_info.ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kFactoryDataReset);
  EXPECT_EQ(final_shutdown_info.ToFidlRebootReason(),
            fuchsia_feedback::RebootReason::kFactoryDataReset);
}

TEST(FinalShutdownInfoTest, InferredFdr_Cold) {
  const FinalShutdownInfo final_shutdown_info = FinalShutdownInfo::MakeFinalShutdownInfo(
      HwShutdownReason::kCold, ZirconShutdownReason::kNotSet,
      /*graceful_shutdown_info=*/std::nullopt,
      /*not_a_fdr=*/false, /*supports_user_initiated_poweroffs=*/false,
      /*uptime=*/std::nullopt, /*runtime=*/std::nullopt, /*critical_process=*/std::nullopt);

  EXPECT_FALSE(final_shutdown_info.IsCrash());
  EXPECT_EQ(final_shutdown_info.ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kFactoryDataReset);
  EXPECT_EQ(final_shutdown_info.ToFidlRebootReason(),
            fuchsia_feedback::RebootReason::kFactoryDataReset);
}

TEST(FinalShutdownInfoTest, GracefulOnCold) {
  const FinalShutdownInfo final_shutdown_info = FinalShutdownInfo::MakeFinalShutdownInfo(
      HwShutdownReason::kCold, ZirconShutdownReason::kNotSet,
      GracefulShutdownInfo{
          .action = GracefulShutdownAction::kReboot,
          .reasons = {GracefulShutdownReason::kUserRequest},
      },
      /*not_a_fdr=*/true, /*supports_user_initiated_poweroffs=*/false,
      /*uptime=*/std::nullopt, /*runtime=*/std::nullopt, /*critical_process=*/std::nullopt);

  EXPECT_EQ(final_shutdown_info.ToGracefulShutdownAction(), GracefulShutdownAction::kReboot);
  EXPECT_EQ(final_shutdown_info.ToRebootReasonString(), "USER REQUEST");
}

TEST(FinalShutdownInfoTest, GracefulPoweroffOnCold) {
  const FinalShutdownInfo final_shutdown_info = FinalShutdownInfo::MakeFinalShutdownInfo(
      HwShutdownReason::kCold, ZirconShutdownReason::kNotSet,
      GracefulShutdownInfo{
          .action = GracefulShutdownAction::kPoweroff,
          .reasons = {GracefulShutdownReason::kUserRequest},
      },
      /*not_a_fdr=*/true, /*supports_user_initiated_poweroffs=*/false,
      /*uptime=*/std::nullopt, /*runtime=*/std::nullopt, /*critical_process=*/std::nullopt);

  EXPECT_EQ(final_shutdown_info.ToGracefulShutdownAction(), GracefulShutdownAction::kPoweroff);
  EXPECT_EQ(final_shutdown_info.ToRebootReasonString(), "USER REQUEST");
}

TEST(FinalShutdownInfoTest, EmptyGracefulReasonsOnCold) {
  const FinalShutdownInfo final_shutdown_info = FinalShutdownInfo::MakeFinalShutdownInfo(
      HwShutdownReason::kCold, ZirconShutdownReason::kNotSet,
      GracefulShutdownInfo{
          .action = GracefulShutdownAction::kReboot,
          .reasons = {},
      },
      /*not_a_fdr=*/true, /*supports_user_initiated_poweroffs=*/false,
      /*uptime=*/std::nullopt, /*runtime=*/std::nullopt, /*critical_process=*/std::nullopt);

  EXPECT_EQ(final_shutdown_info.ToGracefulShutdownAction(), GracefulShutdownAction::kReboot);
  EXPECT_EQ(final_shutdown_info.ToRebootReasonString(), "GENERIC GRACEFUL");
}

TEST(FinalShutdownInfoTest, NoGracefulInfoOnCold) {
  const FinalShutdownInfo final_shutdown_info = FinalShutdownInfo::MakeFinalShutdownInfo(
      HwShutdownReason::kCold, ZirconShutdownReason::kNotSet,
      /*graceful_shutdown_info=*/std::nullopt,
      /*not_a_fdr=*/true, /*supports_user_initiated_poweroffs=*/false,
      /*uptime=*/std::nullopt, /*runtime=*/std::nullopt, /*critical_process=*/std::nullopt);

  EXPECT_FALSE(final_shutdown_info.ToGracefulShutdownAction().has_value());
  EXPECT_EQ(final_shutdown_info.ToRebootReasonString(), "COLD");
}

TEST(FinalShutdownInfoTest, ZirconUnknownWithoutContextIsSpontaneous) {
  const FinalShutdownInfo final_shutdown_info = FinalShutdownInfo::MakeFinalShutdownInfo(
      HwShutdownReason::kUndefined, ZirconShutdownReason::kUnknown,
      /*graceful_shutdown_info=*/std::nullopt,
      /*not_a_fdr=*/true, /*supports_user_initiated_poweroffs=*/true,
      /*uptime=*/zx::msec(5000), /*runtime=*/zx::msec(4000), /*critical_process=*/std::nullopt);

  EXPECT_EQ(final_shutdown_info.ToRebootReasonString(), "SPONTANEOUS");
  EXPECT_EQ(final_shutdown_info.ToGracefulShutdownAction(), std::nullopt);
  EXPECT_EQ(final_shutdown_info.Uptime(), zx::msec(5000));
  EXPECT_EQ(final_shutdown_info.Runtime(), zx::msec(4000));
}

TEST(FinalShutdownInfoTest, ZirconUnknownWithGracefulShutdownInfoIsDuringShutdown) {
  for (const GracefulShutdownAction action : {
           GracefulShutdownAction::kPoweroff,
           GracefulShutdownAction::kReboot,
           GracefulShutdownAction::kRebootToRecovery,
           GracefulShutdownAction::kRebootToBootloader,
           GracefulShutdownAction::kNotSupported,
           GracefulShutdownAction::kNotParseable,
       }) {
    SCOPED_TRACE(ToString(action));
    const FinalShutdownInfo final_shutdown_info = FinalShutdownInfo::MakeFinalShutdownInfo(
        HwShutdownReason::kUndefined, ZirconShutdownReason::kUnknown,
        GracefulShutdownInfo{action, {GracefulShutdownReason::kSystemUpdate}},
        /*not_a_fdr=*/true, /*supports_user_initiated_poweroffs=*/true,
        /*uptime=*/zx::msec(5000), /*runtime=*/zx::msec(4000), /*critical_process=*/std::nullopt);

    EXPECT_TRUE(final_shutdown_info.IsCrash());
    EXPECT_EQ(final_shutdown_info.ToRebootReasonString(), "SPONTANEOUS DURING SHUTDOWN");
    EXPECT_EQ(final_shutdown_info.ToGracefulShutdownAction(), action);
    EXPECT_EQ(final_shutdown_info.Uptime(), zx::msec(5000));
    EXPECT_EQ(final_shutdown_info.Runtime(), zx::msec(4000));
  }
}

TEST(FinalShutdownInfoTest, ZirconUnknownAfterFdrIsDuringShutdown) {
  const FinalShutdownInfo final_shutdown_info = FinalShutdownInfo::MakeFinalShutdownInfo(
      HwShutdownReason::kUndefined, ZirconShutdownReason::kUnknown,
      /*graceful_shutdown_info=*/std::nullopt,
      /*not_a_fdr=*/false, /*supports_user_initiated_poweroffs=*/true,
      /*uptime=*/zx::msec(5000), /*runtime=*/zx::msec(4000), /*critical_process=*/std::nullopt);

  EXPECT_TRUE(final_shutdown_info.IsCrash());
  EXPECT_EQ(final_shutdown_info.ToRebootReasonString(), "SPONTANEOUS DURING SHUTDOWN");
  EXPECT_EQ(final_shutdown_info.ToGracefulShutdownAction(), std::nullopt);
  EXPECT_EQ(final_shutdown_info.Uptime(), zx::msec(5000));
  EXPECT_EQ(final_shutdown_info.Runtime(), zx::msec(4000));
}

TEST(FinalShutdownInfoTest, ZirconUnknownAfterFdrKeepsGracefulShutdownAction) {
  const FinalShutdownInfo final_shutdown_info = FinalShutdownInfo::MakeFinalShutdownInfo(
      HwShutdownReason::kUndefined, ZirconShutdownReason::kUnknown,
      GracefulShutdownInfo{GracefulShutdownAction::kReboot, {GracefulShutdownReason::kFdr}},
      /*not_a_fdr=*/false, /*supports_user_initiated_poweroffs=*/true,
      /*uptime=*/std::nullopt, /*runtime=*/std::nullopt, /*critical_process=*/std::nullopt);

  EXPECT_EQ(final_shutdown_info.ToRebootReasonString(), "SPONTANEOUS DURING SHUTDOWN");
  EXPECT_EQ(final_shutdown_info.ToGracefulShutdownAction(), GracefulShutdownAction::kReboot);
}

TEST(FinalShutdownInfoTest, HwRebootReasonTrumpsZirconUnknownDuringShutdown) {
  const GracefulShutdownInfo graceful_shutdown_info{GracefulShutdownAction::kReboot,
                                                    {GracefulShutdownReason::kSystemUpdate}};

  EXPECT_EQ(FinalShutdownInfo::MakeFinalShutdownInfo(
                HwShutdownReason::kUserHardReset, ZirconShutdownReason::kUnknown,
                graceful_shutdown_info, /*not_a_fdr=*/true,
                /*supports_user_initiated_poweroffs=*/true, /*uptime=*/std::nullopt,
                /*runtime=*/std::nullopt, /*critical_process=*/std::nullopt)
                .ToRebootReasonString(),
            "USER HARD RESET");
  EXPECT_EQ(FinalShutdownInfo::MakeFinalShutdownInfo(
                HwShutdownReason::kWatchdog, ZirconShutdownReason::kUnknown, graceful_shutdown_info,
                /*not_a_fdr=*/true,
                /*supports_user_initiated_poweroffs=*/true, /*uptime=*/std::nullopt,
                /*runtime=*/std::nullopt, /*critical_process=*/std::nullopt)
                .ToRebootReasonString(),
            "HARDWARE WATCHDOG TIMEOUT");
}

TEST(FinalShutdownInfoTest, ZirconCrashReasonTrumpsGracefulShutdownInfoAndFdr) {
  const FinalShutdownInfo final_shutdown_info = FinalShutdownInfo::MakeFinalShutdownInfo(
      HwShutdownReason::kUndefined, ZirconShutdownReason::kKernelPanic,
      GracefulShutdownInfo{GracefulShutdownAction::kReboot,
                           {GracefulShutdownReason::kSystemUpdate}},
      /*not_a_fdr=*/false, /*supports_user_initiated_poweroffs=*/true,
      /*uptime=*/std::nullopt, /*runtime=*/std::nullopt, /*critical_process=*/std::nullopt);

  EXPECT_EQ(final_shutdown_info.ToRebootReasonString(), "KERNEL PANIC");
}

TEST(FinalShutdownInfoTest, LastRebootReasonAnnotationSpontaneous) {
  const FinalShutdownInfo final_shutdown_info(FinalShutdownReason::kSpontaneousReboot);
  EXPECT_EQ(final_shutdown_info.ToSnapshotAnnotationReason(SpontaneousRebootReason::kSpontaneous),
            "spontaneous");
}

TEST(FinalShutdownInfoTest, LastRebootReasonAnnotationBriefPowerLoss) {
  const FinalShutdownInfo final_shutdown_info(FinalShutdownReason::kSpontaneousReboot);
  EXPECT_EQ(
      final_shutdown_info.ToSnapshotAnnotationReason(SpontaneousRebootReason::kBriefPowerLoss),
      "brief loss of power");
}

TEST(FinalShutdownInfoTest, LastRebootReasonAnnotationHardReset) {
  const FinalShutdownInfo final_shutdown_info(FinalShutdownReason::kSpontaneousReboot);
  EXPECT_EQ(final_shutdown_info.ToSnapshotAnnotationReason(SpontaneousRebootReason::kHardReset),
            "hard reset");
}

TEST(FinalShutdownInfoTest, LastRebootReasonAnnotationDuringShutdown) {
  const FinalShutdownInfo final_shutdown_info(
      FinalShutdownReason::kSpontaneousRebootDuringShutdown);
  EXPECT_EQ(final_shutdown_info.ToSnapshotAnnotationReason(SpontaneousRebootReason::kSpontaneous),
            "spontaneous during shutdown");
  EXPECT_EQ(
      final_shutdown_info.ToSnapshotAnnotationReason(SpontaneousRebootReason::kBriefPowerLoss),
      "brief loss of power during shutdown");
  EXPECT_EQ(final_shutdown_info.ToSnapshotAnnotationReason(SpontaneousRebootReason::kHardReset),
            "hard reset during shutdown");
}

TEST(FinalShutdownInfoTest, JsonSerializationWithCriticalProcess) {
  FinalShutdownInfo info(FinalShutdownReason::kRootJobTermination, zx::msec(1000), zx::msec(500),
                         "test-critical-process");
  const std::string json = info.ToJson();

  const std::optional<FinalShutdownInfo> parsed_info = FinalShutdownInfo::FromJson(json);
  ASSERT_TRUE(parsed_info.has_value());

  EXPECT_EQ(parsed_info->ToRebootReasonString(), info.ToRebootReasonString());
  EXPECT_EQ(parsed_info->Uptime(), info.Uptime());
  EXPECT_EQ(parsed_info->Runtime(), info.Runtime());
  EXPECT_EQ(parsed_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous),
            info.ToCrashSignature(SpontaneousRebootReason::kSpontaneous));
}

TEST(FinalShutdownInfoTest, JsonSerializationWithAction) {
  FinalShutdownInfo info(FinalShutdownReason::kUserRequest, GracefulShutdownAction::kReboot,
                         zx::msec(1000), zx::msec(500));
  const std::string json = info.ToJson();

  const std::optional<FinalShutdownInfo> parsed_info = FinalShutdownInfo::FromJson(json);
  ASSERT_TRUE(parsed_info.has_value());

  EXPECT_EQ(parsed_info->ToRebootReasonString(), info.ToRebootReasonString());
  EXPECT_EQ(parsed_info->Uptime(), info.Uptime());
  EXPECT_EQ(parsed_info->Runtime(), info.Runtime());
  EXPECT_EQ(parsed_info->ToGracefulShutdownAction(), info.ToGracefulShutdownAction());
}

TEST(FinalShutdownInfoTest, JsonSerializationSpontaneousDuringShutdown) {
  FinalShutdownInfo info(FinalShutdownReason::kSpontaneousRebootDuringShutdown,
                         GracefulShutdownAction::kRebootToBootloader, zx::msec(1000),
                         zx::msec(500));
  const std::string json = info.ToJson();

  const std::optional<FinalShutdownInfo> parsed_info = FinalShutdownInfo::FromJson(json);
  ASSERT_TRUE(parsed_info.has_value());

  EXPECT_EQ(parsed_info->ToRebootReasonString(), "SPONTANEOUS DURING SHUTDOWN");
  EXPECT_EQ(parsed_info->Uptime(), info.Uptime());
  EXPECT_EQ(parsed_info->Runtime(), info.Runtime());
  EXPECT_EQ(parsed_info->ToGracefulShutdownAction(), GracefulShutdownAction::kRebootToBootloader);
  EXPECT_EQ(parsed_info->ToCrashSignature(SpontaneousRebootReason::kHardReset),
            "fuchsia-hard-reset-during-shutdown");
}

TEST(FinalShutdownInfoTest, FromJsonFailsIfMissingReason) {
  const std::string json = R"({"uptime_ms": 1000})";
  EXPECT_EQ(FinalShutdownInfo::FromJson(json).ToRebootReasonString(), "NOT PARSEABLE");
}

TEST(FinalShutdownInfoTest, FromJsonFailsIfActionAndCriticalProcessPresent) {
  const std::string json = R"({
    "reason": "ROOT JOB TERMINATION",
    "graceful_action": "REBOOT",
    "critical_process": "test-critical-process"
  })";
  EXPECT_EQ(FinalShutdownInfo::FromJson(json).ToRebootReasonString(), "NOT PARSEABLE");
}

TEST(FinalShutdownInfoTest, AllReasonsParseable) {
  // Looping to max() over a switch statement without a default case will ensure that a developer
  // can't forget to test a new enum value. Values outside the range of the enum will safely fall
  // out of the switch statement.
  for (int i = 0; i <= std::numeric_limits<std::uint8_t>::max(); ++i) {
    const FinalShutdownReason reason = static_cast<FinalShutdownReason>(i);

    switch (reason) {
      case FinalShutdownReason::kNotParseable:
      case FinalShutdownReason::kCold:
      case FinalShutdownReason::kBrownout:
      case FinalShutdownReason::kHwWatchdog:
      case FinalShutdownReason::kUserHardReset:
      case FinalShutdownReason::kSpontaneousReboot:
      case FinalShutdownReason::kSpontaneousRebootDuringShutdown:
      case FinalShutdownReason::kKernelPanic:
      case FinalShutdownReason::kOom:
      case FinalShutdownReason::kSwWatchdog:
      case FinalShutdownReason::kRootJobTermination:
      case FinalShutdownReason::kGenericGraceful:
      case FinalShutdownReason::kUnexpectedReasonGraceful:
      case FinalShutdownReason::kUserRequest:
      case FinalShutdownReason::kSystemUpdate:
      case FinalShutdownReason::kRetrySystemUpdate:
      case FinalShutdownReason::kHighTemperature:
      case FinalShutdownReason::kSessionFailure:
      case FinalShutdownReason::kSysmgrFailure:
      case FinalShutdownReason::kCriticalComponentFailure:
      case FinalShutdownReason::kFdr:
      case FinalShutdownReason::kZbiSwap:
      case FinalShutdownReason::kNetstackMigration:
      case FinalShutdownReason::kAndroidUnexpectedReason:
      case FinalShutdownReason::kAndroidNoReason:
      case FinalShutdownReason::kAndroidRescueParty:
      case FinalShutdownReason::kAndroidCriticalProcessFailure:
      case FinalShutdownReason::kDeveloperRequest:
      case FinalShutdownReason::kUserRequestDeviceStuck:
      case FinalShutdownReason::kSuspensionFailure:
      case FinalShutdownReason::kBatteryDrained:
      case FinalShutdownReason::kCriticalDriverFailure:
      case FinalShutdownReason::kPowerLevelTransitionStuck:
        const FinalShutdownInfo info(reason);
        const std::string json = info.ToJson();
        EXPECT_EQ(info.ToRebootReasonString(),
                  FinalShutdownInfo::FromJson(json).ToRebootReasonString());
        break;
    }
  }
}

}  // namespace
}  // namespace forensics::feedback
