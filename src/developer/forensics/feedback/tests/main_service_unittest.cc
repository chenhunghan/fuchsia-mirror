// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/main_service.h"

#include <fidl/fuchsia.feedback/cpp/fidl.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/testing/unit_test_fixture.h"
#include "src/developer/forensics/utils/redact/redactor.h"
#include "src/lib/fxl/strings/substitute.h"
#include "src/lib/timekeeper/async_test_clock.h"

namespace forensics::feedback {
namespace {

using inspect::testing::ChildrenMatch;
using inspect::testing::NameMatches;
using inspect::testing::NodeMatches;
using inspect::testing::PropertyList;
using inspect::testing::UintIs;
using ::testing::IsSupersetOf;
using ::testing::UnorderedElementsAreArray;

constexpr bool kIsFirstInstance = true;

class MainServiceTest : public UnitTestFixture {
 public:
  MainServiceTest()
      : clock_(dispatcher()),
        cobalt_(dispatcher(), services(), &clock_),
        redactor_(std::make_unique<forensics::IdentityRedactor>(
            InspectRoot().CreateBool("redaction_enabled", true))),
        main_service_(dispatcher(), services(), &clock_, &InspectRoot(), &cobalt_,
                      /*startup_annotations=*/{}, {}, std::move(redactor_),
                      MainService::Options{
                          "",
                          "",
                          "",
                          /*enable_periodic_system_time_writes=*/false,
                          LastReboot::Options{
                              .is_first_instance = kIsFirstInstance,
                              .reboot_log = RebootLog(FinalShutdownInfo(FinalShutdownReason::kCold,
                                                                        /*uptime=*/zx::sec(100),
                                                                        /*runtime=*/zx::sec(90)),
                                                      "reboot log"),
                              .oom_crash_reporting_delay = zx::sec(1),
                          },
                          CrashReports::Options{
                              .build_type_config = BuildTypeConfig{},
                              .snapshot_store_max_archives_size = StorageSize::Bytes(0),
                              .snapshot_collector_window_duration = zx::sec(0),
                          },
                          FeedbackData::Options{
                              .snapshot_config{},
                              .snapshot_exclusion_config{},
                              .is_first_instance = kIsFirstInstance,
                              .limit_inspect_data = false,
                              .delete_previous_boot_logs_time = std::nullopt,
                          },
                      }) {
    AddHandler(main_service_.GetHandler<fuchsia_feedback::LastRebootInfoProvider>());
    AddHandler(main_service_.GetHandler<fuchsia_feedback::CrashReporter>());
    AddHandler(main_service_.GetHandler<fuchsia_feedback::CrashReportingProductRegister>());
    AddHandler(main_service_.GetHandler<fuchsia_feedback::ComponentDataRegister>());
    AddHandler(main_service_.GetHandler<fuchsia_feedback::DataProvider>());
  }

 private:
  timekeeper::AsyncTestClock clock_;
  cobalt::Logger cobalt_;
  std::unique_ptr<RedactorBase> redactor_;
  MainService main_service_;
};

auto ProtocolMatcher(const std::string& feedback_protocol, const size_t total_num_connections,
                     const size_t current_num_connections) {
  return NodeMatches(AllOf(NameMatches(fxl::Substitute("fuchsia.feedback.$0", feedback_protocol)),
                           PropertyList(UnorderedElementsAreArray({
                               UintIs("total_num_connections", total_num_connections),
                               UintIs("current_num_connections", current_num_connections),
                           }))));
}

TEST_F(MainServiceTest, LastReboot) {
  zx::result endpoints = fidl::CreateEndpoints<fuchsia_feedback::LastRebootInfoProvider>();
  ASSERT_TRUE(endpoints.is_ok());
  services()->Connect(fidl::DiscoverableProtocolName<fuchsia_feedback::LastRebootInfoProvider>,
                      endpoints->server.TakeChannel());
  fidl::Client last_reboot_info_provider_ptr(std::move(endpoints->client), dispatcher());

  bool called = false;
  last_reboot_info_provider_ptr->Get().Then(
      [&](fidl::Result<fuchsia_feedback::LastRebootInfoProvider::Get>&) { called = true; });

  RunLoopUntilIdle();

  EXPECT_TRUE(called);
  EXPECT_THAT(InspectTree(), ChildrenMatch(IsSupersetOf({
                                 AllOf(NodeMatches(NameMatches("fidl")),
                                       ChildrenMatch(IsSupersetOf({
                                           ProtocolMatcher("LastRebootInfoProvider", 1u, 1u),
                                           ProtocolMatcher("CrashReporter", 0u, 0u),
                                           ProtocolMatcher("CrashReportingProductRegister", 0u, 0u),
                                           ProtocolMatcher("ComponentDataRegister", 0u, 0u),
                                           ProtocolMatcher("DataProvider", 0u, 0u),
                                       }))),
                             })));

  last_reboot_info_provider_ptr = {};
  RunLoopUntilIdle();
  EXPECT_THAT(InspectTree(), ChildrenMatch(IsSupersetOf({
                                 AllOf(NodeMatches(NameMatches("fidl")),
                                       ChildrenMatch(IsSupersetOf({
                                           ProtocolMatcher("LastRebootInfoProvider", 1u, 0u),
                                           ProtocolMatcher("CrashReporter", 0u, 0u),
                                           ProtocolMatcher("CrashReportingProductRegister", 0u, 0u),
                                           ProtocolMatcher("ComponentDataRegister", 0u, 0u),
                                           ProtocolMatcher("DataProvider", 0u, 0u),
                                       }))),
                             })));
}

TEST_F(MainServiceTest, CrashReports) {
  auto [crash_reporter_client_end, crash_reporter_server_end] =
      fidl::Endpoints<fuchsia_feedback::CrashReporter>::Create();
  services()->Connect(fidl::DiscoverableProtocolName<fuchsia_feedback::CrashReporter>,
                      crash_reporter_server_end.TakeChannel());
  fidl::Client crash_reporter(std::move(crash_reporter_client_end), dispatcher());

  zx::result crash_register_endpoints =
      fidl::CreateEndpoints<fuchsia_feedback::CrashReportingProductRegister>();
  ASSERT_TRUE(crash_register_endpoints.is_ok());
  services()->Connect(
      fidl::DiscoverableProtocolName<fuchsia_feedback::CrashReportingProductRegister>,
      crash_register_endpoints->server.TakeChannel());
  fidl::Client crash_reporting_product_register_ptr(std::move(crash_register_endpoints->client),
                                                    dispatcher());

  bool crash_reporter_called = false;
  crash_reporter
      ->FileReport(fuchsia_feedback::CrashReport({
          .program_name = "program_name",
      }))
      .Then([&](fidl::Result<fuchsia_feedback::CrashReporter::FileReport>&) {
        crash_reporter_called = true;
      });

  bool crash_reporting_product_register_called = false;
  crash_reporting_product_register_ptr
      ->UpsertWithAck({{
          .component_url = "component_url",
          .product = fuchsia_feedback::CrashReportingProduct({
              .name = "product_name",
              .version = "product_version",
          }),
      }})
      .Then([&](fidl::Result<fuchsia_feedback::CrashReportingProductRegister::UpsertWithAck>&
                    result) {
        FX_CHECK(result.is_ok());
        crash_reporting_product_register_called = true;
      });

  RunLoopUntilIdle();
  EXPECT_TRUE(crash_reporter_called);
  EXPECT_TRUE(crash_reporting_product_register_called);
  EXPECT_THAT(InspectTree(), ChildrenMatch(IsSupersetOf({
                                 AllOf(NodeMatches(NameMatches("fidl")),
                                       ChildrenMatch(IsSupersetOf({
                                           ProtocolMatcher("LastRebootInfoProvider", 0u, 0u),
                                           ProtocolMatcher("CrashReporter", 1u, 1u),
                                           ProtocolMatcher("CrashReportingProductRegister", 1u, 1u),
                                           ProtocolMatcher("ComponentDataRegister", 0u, 0u),
                                           ProtocolMatcher("DataProvider", 0u, 0u),
                                       }))),
                             })));

  crash_reporter = {};
  crash_reporting_product_register_ptr = {};
  RunLoopUntilIdle();
  EXPECT_THAT(InspectTree(), ChildrenMatch(IsSupersetOf({
                                 AllOf(NodeMatches(NameMatches("fidl")),
                                       ChildrenMatch(IsSupersetOf({
                                           ProtocolMatcher("LastRebootInfoProvider", 0u, 0u),
                                           ProtocolMatcher("CrashReporter", 1u, 0u),
                                           ProtocolMatcher("CrashReportingProductRegister", 1u, 0u),
                                           ProtocolMatcher("ComponentDataRegister", 0u, 0u),
                                           ProtocolMatcher("DataProvider", 0u, 0u),
                                       }))),
                             })));
}

TEST_F(MainServiceTest, FeedbackData) {
  zx::result endpoints = fidl::CreateEndpoints<fuchsia_feedback::ComponentDataRegister>();
  ASSERT_TRUE(endpoints.is_ok());
  services()->Connect(fidl::DiscoverableProtocolName<fuchsia_feedback::ComponentDataRegister>,
                      endpoints->server.TakeChannel());
  fidl::Client component_data_client(std::move(endpoints->client), dispatcher());

  zx::result data_provider_endpoints = fidl::CreateEndpoints<fuchsia_feedback::DataProvider>();
  ASSERT_TRUE(data_provider_endpoints.is_ok());
  services()->Connect(fidl::DiscoverableProtocolName<fuchsia_feedback::DataProvider>,
                      data_provider_endpoints->server.TakeChannel());
  fidl::Client data_provider_client(std::move(data_provider_endpoints->client), dispatcher());

  bool component_data_called = false;
  component_data_client->Upsert(fuchsia_feedback::ComponentData{})
      .Then([&component_data_called](
                fidl::Result<fuchsia_feedback::ComponentDataRegister::Upsert>& result) {
        FX_CHECK(result.is_ok());
        component_data_called = true;
      });

  fuchsia_feedback::GetSnapshotParameters snapshot_params;
  snapshot_params.collection_timeout_per_data(0);
  bool data_provider_called = false;
  data_provider_client->GetSnapshot(std::move(snapshot_params))
      .Then([&data_provider_called](fidl::Result<fuchsia_feedback::DataProvider::GetSnapshot>&) {
        data_provider_called = true;
      });

  RunLoopUntilIdle();
  EXPECT_TRUE(component_data_called);
  EXPECT_TRUE(data_provider_called);
  EXPECT_THAT(InspectTree(), ChildrenMatch(IsSupersetOf({
                                 AllOf(NodeMatches(NameMatches("fidl")),
                                       ChildrenMatch(IsSupersetOf({
                                           ProtocolMatcher("LastRebootInfoProvider", 0u, 0u),
                                           ProtocolMatcher("CrashReporter", 0u, 0u),
                                           ProtocolMatcher("CrashReportingProductRegister", 0u, 0u),
                                           ProtocolMatcher("ComponentDataRegister", 1u, 1u),
                                           ProtocolMatcher("DataProvider", 1u, 1u),
                                       }))),
                             })));

  component_data_client = {};
  data_provider_client = {};

  RunLoopUntilIdle();
  EXPECT_THAT(InspectTree(), ChildrenMatch(IsSupersetOf({
                                 AllOf(NodeMatches(NameMatches("fidl")),
                                       ChildrenMatch(IsSupersetOf({
                                           ProtocolMatcher("LastRebootInfoProvider", 0u, 0u),
                                           ProtocolMatcher("CrashReporter", 0u, 0u),
                                           ProtocolMatcher("CrashReportingProductRegister", 0u, 0u),
                                           ProtocolMatcher("ComponentDataRegister", 1u, 0u),
                                           ProtocolMatcher("DataProvider", 1u, 0u),
                                       }))),
                             })));
}

}  // namespace
}  // namespace forensics::feedback
