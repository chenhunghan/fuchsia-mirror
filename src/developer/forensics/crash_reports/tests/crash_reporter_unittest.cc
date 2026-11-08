// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/crash_reports/crash_reporter.h"

#include <fidl/fuchsia.feedback/cpp/fidl.h>
#include <fidl/fuchsia.mem/cpp/fidl.h>
#include <fuchsia/time/cpp/fidl.h>
#include <lib/fpromise/result.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/clock.h>
#include <lib/zx/time.h>
#include <zircon/errors.h>
#include <zircon/types.h>
#include <zircon/utc.h>

#include <algorithm>
#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <utility>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/crash_reports/constants.h"
#include "src/developer/forensics/crash_reports/info/info_context.h"
#include "src/developer/forensics/crash_reports/tests/scoped_test_report_store.h"
#include "src/developer/forensics/crash_reports/tests/stub_crash_server.h"
#include "src/developer/forensics/feedback/annotations/annotation_manager.h"
#include "src/developer/forensics/feedback/annotations/constants.h"
#include "src/developer/forensics/feedback/config.h"
#include "src/developer/forensics/feedback/constants.h"
#include "src/developer/forensics/feedback_data/constants.h"
#include "src/developer/forensics/testing/fakes/privacy_settings.h"
#include "src/developer/forensics/testing/stubs/cobalt_logger_factory.h"
#include "src/developer/forensics/testing/stubs/data_provider.h"
#include "src/developer/forensics/testing/unit_test_fixture.h"
#include "src/developer/forensics/utils/cobalt/event.h"
#include "src/developer/forensics/utils/cobalt/metrics.h"
#include "src/developer/forensics/utils/vmo.h"
#include "src/lib/files/path.h"
#include "src/lib/files/scoped_temp_dir.h"
#include "src/lib/fxl/strings/substitute.h"
#include "src/lib/timekeeper/test_clock.h"

namespace forensics {
namespace crash_reports {
namespace {

using fuchsia_feedback::Annotation;
using fuchsia_feedback::Attachment;
using fuchsia_feedback::CrashReport;
using fuchsia_feedback::FilingError;
using fuchsia_feedback::FilingSuccess;
using fuchsia_feedback::NativeCrashReport;
using fuchsia_feedback::RuntimeCrashReport;
using fuchsia_feedback::SpecificCrashReport;
using fuchsia_feedback::TextBacktraceCrashReport;
using inspect::testing::ChildrenMatch;
using inspect::testing::NameMatches;
using inspect::testing::NodeMatches;
using inspect::testing::PropertyList;
using inspect::testing::StringIs;
using inspect::testing::UintIs;
using testing::ByRef;
using testing::Contains;
using testing::ElementsAre;
using testing::HasSubstr;
using testing::IsEmpty;
using testing::IsSupersetOf;
using testing::Not;
using testing::UnorderedElementsAre;
using testing::UnorderedElementsAreArray;

constexpr CrashServer::UploadStatus kUploadSuccessful = CrashServer::UploadStatus::kSuccess;
constexpr CrashServer::UploadStatus kUploadFailed = CrashServer::UploadStatus::kFailure;
constexpr CrashServer::UploadStatus kUploadThrottled = CrashServer::UploadStatus::kThrottled;
constexpr CrashServer::UploadStatus kUploadTimeout = CrashServer::UploadStatus::kTimedOut;

constexpr char kProgramName[] = "crashing_program";

constexpr char kBuildVersion[] = "some-version";
constexpr char kBuildPlatformVersion[] = "some-platform-version";
constexpr char kBuildProductVersion[] = "some-product-version";
const std::string kProductVersion =
    fxl::Substitute("$0--$1", kBuildProductVersion, kBuildPlatformVersion);
constexpr char kBuildBoard[] = "some-board";
constexpr char kBuildProduct[] = "some-product";
constexpr char kBuildLatestCommitDate[] = "some-date";
constexpr char kDefaultChannel[] = "some-channel";
constexpr char kDefaultDeviceId[] = "some-device-id";

constexpr char kSingleAttachmentKey[] = "attachment.key";
constexpr char kSingleAttachmentValue[] = "attachment.value";

constexpr bool kUserOptInDataSharing = true;
constexpr bool kUserOptOutDataSharing = false;

constexpr size_t kDailyPerProductQuota = 100u;
constexpr zx::duration kResetOffset = zx::min(20);

const std::map<std::string, std::string> kFeedbackAnnotations = {
    {feedback::kBuildVersionKey, kBuildVersion},
    {feedback::kBuildBoardKey, kBuildBoard},
    {feedback::kBuildPlatformVersionKey, kBuildPlatformVersion},
    {feedback::kBuildProductVersionKey, kBuildProductVersion},
    {feedback::kBuildProductKey, kBuildProduct},
    {feedback::kBuildLatestCommitDateKey, kBuildLatestCommitDate},
    {feedback::kDeviceFeedbackIdKey, kDefaultDeviceId},
    {feedback::kSystemUpdateChannelCurrentKey, kDefaultChannel},
};

constexpr char kDefaultAttachmentBundleKey[] = "feedback.attachment.bundle.key";
constexpr char kEmptyAttachmentBundleKey[] = "empty.attachment.key";

constexpr zx::duration kSnapshotSharedRequestWindow = zx::sec(5);

fuchsia_mem::Buffer BuildBuffer(const std::string& value) {
  return BufferFromString(value).value();
}

Attachment BuildAttachment(const std::string& key, const std::string& value) {
  return Attachment({
      .key = key,
      .value = BuildBuffer(value),
  });
}

template <typename K, typename V>
std::vector<testing::internal::PairMatcher<K, V>> Linearize(const std::map<K, V>& annotations) {
  std::vector<testing::internal::PairMatcher<K, V>> linearized;
  for (const auto& [k, v] : annotations) {
    linearized.push_back(testing::Pair(k, v));
  }
  return linearized;
}

// Matches a fuchsia.feedback.CrashReporter/FileReport result that failed with the FilingError
// |expected|.
MATCHER_P(IsFilingError, expected, "") {
  if (arg.is_ok()) {
    *result_listener << "succeeded";
    return false;
  }
  if (!arg.error_value().is_domain_error()) {
    *result_listener << "failed with framework error "
                     << arg.error_value().framework_error().FormatDescription();
    return false;
  }
  *result_listener << "failed with FilingError "
                   << static_cast<uint32_t>(arg.error_value().domain_error());
  return arg.error_value().domain_error() == expected;
}

// Unit-tests the implementation of the fuchsia.feedback.CrashReporter FIDL interface.
//
// This does not test the environment service. It directly instantiates the class and serves it
// over a local channel via fidl::BindServer.
class CrashReporterTest : public UnitTestFixture {
 public:
  void SetUp() override {
    clock_.SetMonotonic(zx::time_monotonic(0u));
    info_context_ =
        std::make_shared<InfoContext>(&InspectRoot(), &clock_, dispatcher(), services());
    crash_register_ = std::make_unique<CrashRegister>(info_context_, RegisterJsonPath());

    SetUpCobaltServer(std::make_unique<stubs::CobaltLoggerFactory>(dispatcher()));
    RunLoopUntilIdle();
  }

  void TearDown() override {
    ASSERT_TRUE(files::DeletePath(feedback::kProductQuotasPath, /*recursive=*/true));
  }

 protected:
  // Sets up the underlying crash reporter using the given |config| and |crash_server|.
  void SetUpCrashReporter(const feedback::CrashReportUploadPolicy crash_report_upload_policy,
                          const std::optional<uint64_t> daily_per_product_quota,
                          const bool enable_hourly_snapshots,
                          const std::vector<CrashServer::UploadStatus>& upload_attempt_results = {},
                          StorageSize max_reports_cache_size = StorageSize::Megabytes(1)) {
    FX_CHECK(data_provider_server_);
    annotation_manager_ = std::make_unique<feedback::AnnotationManager>(
        dispatcher(),
        std::set<std::string>{
            feedback::kBuildVersionKey,
            feedback::kBuildBoardKey,
            feedback::kBuildPlatformVersionKey,
            feedback::kBuildProductVersionKey,
            feedback::kBuildProductKey,
            feedback::kBuildLatestCommitDateKey,
            feedback::kDeviceFeedbackIdKey,
            feedback::kSystemUpdateChannelCurrentKey,
        },
        feedback::Annotations{
            {feedback::kBuildVersionKey, ErrorOrString(kBuildVersion)},
            {feedback::kBuildBoardKey, ErrorOrString(kBuildBoard)},
            {feedback::kBuildPlatformVersionKey, ErrorOrString(kBuildPlatformVersion)},
            {feedback::kBuildProductVersionKey, ErrorOrString(kBuildProductVersion)},
            {feedback::kBuildProductKey, ErrorOrString(kBuildProduct)},
            {feedback::kBuildLatestCommitDateKey, ErrorOrString(kBuildLatestCommitDate)},
            {feedback::kDeviceFeedbackIdKey, ErrorOrString(kDefaultDeviceId)},
            {feedback::kSystemUpdateChannelCurrentKey, ErrorOrString(kDefaultChannel)},
        });
    report_store_ = std::make_unique<ScopedTestReportStore>(
        annotation_manager_.get(), info_context_,
        /*max_reports_tmp_size=*/StorageSize::Megabytes(1), max_reports_cache_size);

    crash_server_ = std::make_unique<StubCrashServer>(
        dispatcher(), services(), annotation_manager_.get(), upload_attempt_results, &clock_);

    const feedback::BuildTypeConfig build_type_config{
        .crash_report_upload_policy = crash_report_upload_policy,
        .daily_per_product_crash_report_quota = daily_per_product_quota,
        .enable_hourly_snapshots = enable_hourly_snapshots,
    };

    zx_clock_create_args_v1_t clock_args{.backstop_time = 0};
    FX_CHECK(zx::clock::create(0u, &clock_args, &clock_handle_) == ZX_OK);

    if (const zx_status_t status =
            clock_handle_.signal(/*clear_mask=*/0,
                                 /*set_mask=*/fuchsia::time::SIGNAL_UTC_CLOCK_LOGGING_QUALITY);
        status != ZX_OK) {
      FX_PLOGS(FATAL, status) << "Failed to sync clock";
    }

    crash_reporter_ = std::make_unique<CrashReporter>(
        dispatcher(), services(), &clock_, zx::unowned_clock(clock_handle_.get_handle()),
        info_context_, build_type_config, crash_register_.get(), &tags_, crash_server_.get(),
        &report_store_->GetReportStore(), data_provider_server_.get(), kSnapshotSharedRequestWindow,
        kResetOffset);
    FX_CHECK(crash_reporter_);

    auto [client_end, server_end] = fidl::Endpoints<fuchsia_feedback::CrashReporter>::Create();
    fidl::BindServer(dispatcher(), std::move(server_end), crash_reporter_.get());
    crash_reporter_client_.Bind(std::move(client_end), dispatcher());
  }

  // Sets up the underlying crash reporter using a default config.
  void SetUpCrashReporterDefaultConfig(
      const std::vector<CrashServer::UploadStatus>& upload_attempt_results = {}) {
    SetUpCrashReporter(
        /*crash_report_upload_policy=*/feedback::CrashReportUploadPolicy::kEnabled,
        /*daily_per_product_quota=*/kDailyPerProductQuota,
        /*enable_hourly_snapshots=*/true, upload_attempt_results);
  }

  void SetUpDataProviderServer(std::unique_ptr<stubs::DataProviderBase> server) {
    data_provider_server_ = std::move(server);
  }

  void SetUpPrivacySettingsServer(std::unique_ptr<fakes::PrivacySettings> server) {
    privacy_settings_server_ = std::move(server);
    if (privacy_settings_server_) {
      InjectServiceProvider(privacy_settings_server_.get());
    }
  }

  std::string RegisterJsonPath() { return files::JoinPath(tmp_dir_.path(), "register.json"); }

  // Checks that on the crash server the annotations received match the concatenation of:
  //   * |expected_extra_annotations|
  //   * default annotations
  //
  // In case of duplicate keys, the value from |expected_extra_annotations| is picked.
  void CheckAnnotationsOnServer(
      const std::map<std::string, std::string>& expected_extra_annotations = {}) {
    ASSERT_TRUE(crash_server_);

    std::map<std::string, testing::Matcher<std::string>> expected_annotations = {
        {"product", "Fuchsia"},
        {"version", kProductVersion},
        {"program", testing::StartsWith("crashing_program")},
        {"ptype", testing::StartsWith("crashing_program")},
        {feedback::kOSNameKey, "Fuchsia"},
        {feedback::kOSVersionKey, kBuildPlatformVersion},
        {feedback::kOSChannelKey, kDefaultChannel},
        {feedback::kBuildVersionKey, kBuildVersion},
        {feedback::kBuildBoardKey, kBuildBoard},
        {feedback::kBuildPlatformVersionKey, kBuildPlatformVersion},
        {feedback::kBuildProductVersionKey, kBuildProductVersion},
        {feedback::kBuildProductKey, kBuildProduct},
        {feedback::kBuildLatestCommitDateKey, kBuildLatestCommitDate},
        {feedback::kDeviceFeedbackIdKey, kDefaultDeviceId},
        {feedback::kSystemUpdateChannelCurrentKey, kDefaultChannel},
        {"reportTimeMillis", Not(IsEmpty())},
        {"guid", kDefaultDeviceId},
        {"channel", kDefaultChannel},
        {"debug.snapshot.shared-request.num-clients", Not(IsEmpty())},
        {feedback::kSnapshotUuid, Not(IsEmpty())},
        {feedback::kDebugReportUuid, Not(IsEmpty())},
    };
    for (const auto& [key, value] : expected_extra_annotations) {
      expected_annotations[key] = value;
    }

    EXPECT_THAT(crash_server_->latest_annotations().Raw(),
                testing::UnorderedElementsAreArray(Linearize(expected_annotations)));
  }

  // Checks that on the crash server the keys for the attachments received match the
  // concatenation of:
  //   * |expected_extra_attachment_keys|
  //   * data_provider_->attachment_bundle_key()
  void CheckAttachmentsOnServer(
      const std::vector<std::string>& expected_extra_attachment_keys = {}) {
    ASSERT_TRUE(crash_server_);

    std::vector<std::string> expected_attachment_keys = expected_extra_attachment_keys;

    EXPECT_THAT(crash_server_->latest_attachment_keys(),
                testing::UnorderedElementsAreArray(expected_attachment_keys));
  }

  // Checks that the crash server is still expecting at least one more request.
  //
  // This is useful to check that an upload request hasn't been made as we are using a strict stub.
  void CheckServerStillExpectRequests() {
    ASSERT_TRUE(crash_server_);
    EXPECT_TRUE(crash_server_->ExpectRequest());
  }

  // Files one crash report.
  fidl::Result<fuchsia_feedback::CrashReporter::FileReport> FileOneCrashReport(CrashReport report) {
    // Run loop to start the clock.
    RunLoopUntilIdle();
    FX_CHECK(crash_reporter_ != nullptr)
        << "crash_reporter_ is nullptr. Call SetUpCrashReporter() or one of its variants "
           "at the beginning of a test case.";
    std::optional<fidl::Result<fuchsia_feedback::CrashReporter::FileReport>> out_result{
        std::nullopt};
    crash_reporter_client_->FileReport(std::move(report))
        .Then([&out_result](fidl::Result<fuchsia_feedback::CrashReporter::FileReport>& result) {
          out_result = std::move(result);
        });
    RunLoopFor(kSnapshotSharedRequestWindow);
    FX_CHECK(out_result.has_value());
    return std::move(*out_result);
  }

  // Files one crash report.
  fidl::Result<fuchsia_feedback::CrashReporter::FileReport> FileOneCrashReport(
      const std::vector<Annotation>& annotations = {}, std::vector<Attachment> attachments = {}) {
    CrashReport report({
        .program_name = kProgramName,
    });
    if (!annotations.empty()) {
      report.annotations(annotations);
    }
    if (!attachments.empty()) {
      report.attachments(std::move(attachments));
    }
    return FileOneCrashReport(std::move(report));
  }

  // Files one crash report.
  //
  // |attachment| is useful to control the lower bound of the size of the report by controlling the
  // size of some of the attachment(s).
  fidl::Result<fuchsia_feedback::CrashReporter::FileReport> FileOneCrashReportWithSingleAttachment(
      const std::string& attachment = kSingleAttachmentValue) {
    std::vector<Attachment> attachments;
    attachments.emplace_back(BuildAttachment(kSingleAttachmentKey, attachment));
    return FileOneCrashReport(/*annotations=*/{},
                              /*attachments=*/std::move(attachments));
  }

  // Files one native crash report.
  fidl::Result<fuchsia_feedback::CrashReporter::FileReport> FileOneNativeCrashReport(
      std::optional<fuchsia_mem::Buffer> minidump,
      const std::optional<std::string>& crash_signature) {
    NativeCrashReport native_report({
        .minidump = std::move(minidump),
        .process_name = "crashing_process",
        .process_koid = 123u,
        .thread_name = "crashing_thread",
        .thread_koid = 1234u,
    });

    CrashReport report({
        .program_name = "crashing_program_native",
        .specific_report = SpecificCrashReport::WithNative(std::move(native_report)),
        .crash_signature = crash_signature,
    });

    return FileOneCrashReport(std::move(report));
  }

  // Files one Dart crash report.
  fidl::Result<fuchsia_feedback::CrashReporter::FileReport> FileOneDartCrashReport(
      const std::optional<std::string>& exception_type,
      const std::optional<std::string>& exception_message,
      std::optional<fuchsia_mem::Buffer> exception_stack_trace) {
    RuntimeCrashReport dart_report({
        .exception_type = exception_type,
        .exception_message = exception_message,
        .exception_stack_trace = std::move(exception_stack_trace),
    });

    CrashReport report({
        .program_name = "crashing_program_dart",
        .specific_report = SpecificCrashReport::WithDart(std::move(dart_report)),
    });

    return FileOneCrashReport(std::move(report));
  }

  // Files one Fuchsia text backtrace crash report.
  fidl::Result<fuchsia_feedback::CrashReporter::FileReport> FileOneFuchsiaTextBacktraceCrashReport(
      std::optional<fuchsia_mem::Buffer> backtrace,
      const std::optional<std::string>& crash_signature) {
    TextBacktraceCrashReport text_backtrace_report({
        .fuchsia_backtrace = std::move(backtrace),
        .process_name = "crashing_process",
        .process_koid = 123u,
        .thread_name = "crashing_thread",
        .thread_koid = 1234u,
    });

    CrashReport report({
        .program_name = "crashing_program_text_backtrace",
        .specific_report = SpecificCrashReport::WithTextBacktrace(std::move(text_backtrace_report)),
        .crash_signature = crash_signature,
    });

    return FileOneCrashReport(std::move(report));
  }

  // Files one empty crash report.
  fidl::Result<fuchsia_feedback::CrashReporter::FileReport> FileOneEmptyCrashReport() {
    CrashReport report;
    return FileOneCrashReport(std::move(report));
  }

  // Files one crash report with the provided crash signature.
  fidl::Result<fuchsia_feedback::CrashReporter::FileReport> FileOneCrashReportWithSignature(
      const std::string& signature) {
    CrashReport report({
        .program_name = "crashing_program_generic",
        .crash_signature = signature,
    });

    return FileOneCrashReport(std::move(report));
  }

  // Files one crash report with the provided is fatal value.
  fidl::Result<fuchsia_feedback::CrashReporter::FileReport> FileOneCrashReportWithIsFatal(
      const bool is_fatal) {
    CrashReport report({
        .program_name = "crashing_program_generic",
        .is_fatal = is_fatal,
    });

    return FileOneCrashReport(std::move(report));
  }

  void SetPrivacySettings(std::optional<bool> user_data_sharing_consent) {
    FX_CHECK(privacy_settings_server_);
    privacy_settings_server_->SetUserDataSharingConsent(user_data_sharing_consent);
  }

 private:
  files::ScopedTempDir tmp_dir_;

  LogTags tags_;

  // Stubs and fake servers.
  std::unique_ptr<stubs::DataProviderBase> data_provider_server_;
  std::unique_ptr<fakes::PrivacySettings> privacy_settings_server_;

 protected:
  std::unique_ptr<StubCrashServer> crash_server_;
  timekeeper::TestClock clock_;

 private:
  std::shared_ptr<InfoContext> info_context_;
  std::unique_ptr<ScopedTestReportStore> report_store_;
  zx::clock clock_handle_;
  fidl::Client<fuchsia_feedback::CrashReporter> crash_reporter_client_;

 protected:
  std::unique_ptr<feedback::AnnotationManager> annotation_manager_;
  std::unique_ptr<CrashRegister> crash_register_;
  std::unique_ptr<CrashReporter> crash_reporter_;
};

TEST_F(CrashReporterTest, Succeed_OnInputCrashReport) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful});

  const fidl::Result<fuchsia_feedback::CrashReporter::FileReport> result = FileOneCrashReport();
  ASSERT_TRUE(result.is_ok());
  EXPECT_EQ(result->results().result(), FilingSuccess::kReportUploaded);

  CheckAnnotationsOnServer();
  CheckAttachmentsOnServer({kDefaultAttachmentBundleKey});
}

TEST_F(CrashReporterTest, EnforcesQuota) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporter(
      /*crash_report_upload_policy=*/feedback::CrashReportUploadPolicy::kEnabled,
      /*daily_per_product_quota=*/kDailyPerProductQuota,
      /*enable_hourly_snapshots=*/false,
      std::vector<CrashServer::UploadStatus>(kDailyPerProductQuota, kUploadSuccessful));

  for (size_t i = 0; i < kDailyPerProductQuota; ++i) {
    ASSERT_TRUE(FileOneCrashReport().is_ok());
  }

  EXPECT_THAT(FileOneCrashReport(), IsFilingError(FilingError::kQuotaReachedError));
}

TEST_F(CrashReporterTest, ResetsQuota) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporter(
      /*crash_report_upload_policy=*/feedback::CrashReportUploadPolicy::kEnabled,
      /*daily_per_product_quota=*/kDailyPerProductQuota,
      /*enable_hourly_snapshots=*/false,
      std::vector<CrashServer::UploadStatus>(kDailyPerProductQuota * 2, kUploadSuccessful));

  for (size_t i = 0; i < kDailyPerProductQuota; ++i) {
    ASSERT_TRUE(FileOneCrashReport().is_ok());
  }

  timekeeper::time_utc utc_time;
  ASSERT_EQ(clock_.UtcNow(&utc_time), ZX_OK);
  clock_.SetUtc(utc_time + zx::hour(24) + kResetOffset);

  for (size_t i = 0; i < kDailyPerProductQuota; ++i) {
    ASSERT_TRUE(FileOneCrashReport().is_ok());
  }
}

TEST_F(CrashReporterTest, NoQuota) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporter(
      /*crash_report_upload_policy=*/feedback::CrashReportUploadPolicy::kEnabled,
      /*daily_per_product_quota=*/std::nullopt,
      /*enable_hourly_snapshots=*/true,
      std::vector<CrashServer::UploadStatus>(kDailyPerProductQuota + 1 /*first hourly snapshot*/,
                                             kUploadSuccessful));

  for (size_t i = 0; i < kDailyPerProductQuota; ++i) {
    ASSERT_TRUE(FileOneCrashReport().is_ok());
  }
}

TEST_F(CrashReporterTest, Check_RegisteredProduct) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful});

  auto [client_end, server_end] =
      fidl::Endpoints<fuchsia_feedback::CrashReportingProductRegister>::Create();
  fidl::BindServer(dispatcher(), std::move(server_end), crash_register_.get());
  fidl::Client crash_register_client(std::move(client_end), dispatcher());

  fuchsia_feedback::CrashReportingProduct product({
      .name = "some name",
      .version = "some version",
      .channel = "some channel",
  });
  ASSERT_TRUE(crash_register_client
                  ->Upsert({{
                      .component_url = kProgramName,
                      .product = std::move(product),
                  }})
                  .is_ok());

  ASSERT_TRUE(FileOneCrashReport().is_ok());

  ASSERT_TRUE(crash_server_->latest_annotations().Contains("product"));
  EXPECT_EQ(crash_server_->latest_annotations().Get("product"), "some name");
  ASSERT_TRUE(crash_server_->latest_annotations().Contains("version"));
  EXPECT_EQ(crash_server_->latest_annotations().Get("version"), "some version");
  ASSERT_TRUE(crash_server_->latest_annotations().Contains("channel"));
  EXPECT_EQ(crash_server_->latest_annotations().Get("channel"), "some channel");
}

TEST_F(CrashReporterTest, Succeed_OnInputCrashReportWithAdditionalData) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kEmptyAttachmentBundleKey));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful});

  std::vector<Attachment> attachments;
  attachments.emplace_back(BuildAttachment(kSingleAttachmentKey, kSingleAttachmentValue));

  ASSERT_TRUE(FileOneCrashReport(
                  /*annotations=*/
                  {
                      {{.key = "annotation.key", .value = "annotation.value"}},
                  },
                  /*attachments=*/std::move(attachments))
                  .is_ok());
  CheckAnnotationsOnServer({
      {"annotation.key", "annotation.value"},
  });
  CheckAttachmentsOnServer({kSingleAttachmentKey, kEmptyAttachmentBundleKey});
}

TEST_F(CrashReporterTest, Succeed_OnInputCrashReportWithEventId) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful});

  CrashReport report({
      .program_name = kProgramName,
      .event_id = "some-event-id",
  });

  ASSERT_TRUE(FileOneCrashReport(std::move(report)).is_ok());
  CheckAnnotationsOnServer({
      {"comments", "some-event-id"},
  });
  CheckAttachmentsOnServer({kDefaultAttachmentBundleKey});
}

TEST_F(CrashReporterTest, Succeed_OnInputCrashReportWithProgramUptime) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful});

  const zx::duration uptime =
      zx::hour(3) * 24 + zx::hour(15) + zx::min(33) + zx::sec(17) + zx::msec(54);
  CrashReport report({
      .program_name = kProgramName,
      .program_uptime = uptime.get(),
  });

  ASSERT_TRUE(FileOneCrashReport(std::move(report)).is_ok());
  CheckAnnotationsOnServer({
      {"ptime", std::to_string(uptime.to_msecs())},
  });
  CheckAttachmentsOnServer({kDefaultAttachmentBundleKey});
}

TEST_F(CrashReporterTest, Succeed_OnNativeInputCrashReport) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful});

  ASSERT_TRUE(
      FileOneNativeCrashReport(BuildBuffer("minidump"), /*crash_signature=*/std::nullopt).is_ok());
  CheckAnnotationsOnServer({
      {"crash.process.name", "crashing_process"},
      {"crash.process.koid", "123"},
      {"crash.thread.name", "crashing_thread"},
      {"crash.thread.koid", "1234"},
  });
  CheckAttachmentsOnServer({"uploadFileMinidump", kDefaultAttachmentBundleKey});
}

TEST_F(CrashReporterTest, Succeed_OnNativeInputCrashReportWithoutMinidump) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful});

  ASSERT_TRUE(FileOneNativeCrashReport(/*minidump=*/std::nullopt, /*crash_signature=*/std::nullopt)
                  .is_ok());
  CheckAnnotationsOnServer({
      {"crash.process.name", "crashing_process"},
      {"crash.process.koid", "123"},
      {"crash.thread.name", "crashing_thread"},
      {"crash.thread.koid", "1234"},
      {"signature", "fuchsia-no-minidump"},
  });
  CheckAttachmentsOnServer({kDefaultAttachmentBundleKey});
}

TEST_F(CrashReporterTest, Succeed_OnNativeInputCrashReportWithoutMinidumpButCrashSignature) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful});

  ASSERT_TRUE(FileOneNativeCrashReport(/*minidump=*/std::nullopt, "some-signature").is_ok());
  CheckAnnotationsOnServer({
      {"crash.process.name", "crashing_process"},
      {"crash.process.koid", "123"},
      {"crash.thread.name", "crashing_thread"},
      {"crash.thread.koid", "1234"},
      {"signature", "some-signature"},
  });
  CheckAttachmentsOnServer({kDefaultAttachmentBundleKey});
}

TEST_F(CrashReporterTest, Succeed_OnDartInputCrashReport) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kEmptyAttachmentBundleKey));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful});

  ASSERT_TRUE(
      FileOneDartCrashReport("FileSystemException", "cannot open file", BuildBuffer("#0")).is_ok());
  CheckAnnotationsOnServer({
      {"error_runtime_type", "FileSystemException"},
      {"error_message", "cannot open file"},
      {"type", "DartError"},
  });
  CheckAttachmentsOnServer({"DartError", kEmptyAttachmentBundleKey});
}

TEST_F(CrashReporterTest, Succeed_OnDartInputCrashReportWithoutExceptionData) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful});

  ASSERT_TRUE(FileOneDartCrashReport(/*exception_type=*/std::nullopt,
                                     /*exception_message=*/std::nullopt,
                                     /*exception_stack_trace=*/std::nullopt)
                  .is_ok());
  CheckAnnotationsOnServer({
      {"type", "DartError"},
      {"signature", "fuchsia-no-dart-stack-trace"},
  });
  CheckAttachmentsOnServer({kDefaultAttachmentBundleKey});
}

TEST_F(CrashReporterTest, Succeed_OnFuchsiaTextBacktraceInputCrashReport) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kEmptyAttachmentBundleKey));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful});

  ASSERT_TRUE(FileOneFuchsiaTextBacktraceCrashReport(BuildBuffer("#0"),
                                                     /*crash_signature=*/std::nullopt)
                  .is_ok());
  CheckAnnotationsOnServer({
      {"crash.process.name", "crashing_process"},
      {"crash.process.koid", "123"},
      {"crash.thread.name", "crashing_thread"},
      {"crash.thread.koid", "1234"},
  });
  CheckAttachmentsOnServer({"backtrace.txt", kEmptyAttachmentBundleKey});
}

TEST_F(CrashReporterTest, Succeed_OnFuchsiaTextBacktraceInputCrashReportWithoutBacktrace) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful});

  ASSERT_TRUE(FileOneFuchsiaTextBacktraceCrashReport(/*backtrace=*/std::nullopt,
                                                     /*crash_signature=*/std::nullopt)
                  .is_ok());
  CheckAnnotationsOnServer({
      {"crash.process.name", "crashing_process"},
      {"crash.process.koid", "123"},
      {"crash.thread.name", "crashing_thread"},
      {"crash.thread.koid", "1234"},
      {"signature", "fuchsia-no-fuchsia-text-backtrace"},
  });
  CheckAttachmentsOnServer({kDefaultAttachmentBundleKey});
}

TEST_F(CrashReporterTest, Succeed_OnInputCrashReportWithSignature) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful});

  ASSERT_TRUE(FileOneCrashReportWithSignature("some-signature").is_ok());
  CheckAnnotationsOnServer({
      {"signature", "some-signature"},
  });
  CheckAttachmentsOnServer({kDefaultAttachmentBundleKey});
}

TEST_F(CrashReporterTest, FailOnInvalidInputNoProgramName) {
  SetUpDataProviderServer(std::make_unique<stubs::DataProviderReturnsEmptySnapshot>());
  SetUpCrashReporterDefaultConfig();

  EXPECT_THAT(FileOneEmptyCrashReport(), IsFilingError(FilingError::kInvalidArgsError));
}

TEST_F(CrashReporterTest, FailOnInvalidInputEmptyShortenedProgramName) {
  SetUpDataProviderServer(std::make_unique<stubs::DataProviderReturnsEmptySnapshot>());
  SetUpCrashReporterDefaultConfig();

  CrashReport report({
      .program_name = "fuchsia-pkg://",
  });
  EXPECT_THAT(FileOneCrashReport(std::move(report)), IsFilingError(FilingError::kInvalidArgsError));
}

TEST_F(CrashReporterTest, FailOnInvalidInputZeroWeight) {
  SetUpDataProviderServer(std::make_unique<stubs::DataProviderReturnsEmptySnapshot>());
  SetUpCrashReporterDefaultConfig();

  CrashReport report({
      .program_name = "crashing_program_generic",
      .weight = 0,
  });
  EXPECT_THAT(FileOneCrashReport(std::move(report)), IsFilingError(FilingError::kInvalidArgsError));
}

TEST_F(CrashReporterTest, FailOnInvalidInputAttachmentKeyIsEmpty) {
  SetUpDataProviderServer(std::make_unique<stubs::DataProviderReturnsEmptySnapshot>());
  SetUpCrashReporterDefaultConfig();

  std::vector<Attachment> attachments;
  attachments.emplace_back(BuildAttachment("", "content"));
  CrashReport report({
      .program_name = "crashing_program_generic",
      .attachments = std::move(attachments),
  });

  EXPECT_THAT(FileOneCrashReport(std::move(report)), IsFilingError(FilingError::kInvalidArgsError));
}

TEST_F(CrashReporterTest, FailOnInvalidInputAttachmentKeyContainsSlash) {
  SetUpDataProviderServer(std::make_unique<stubs::DataProviderReturnsEmptySnapshot>());
  SetUpCrashReporterDefaultConfig();

  std::vector<Attachment> attachments;
  attachments.emplace_back(BuildAttachment("my/attachment.key", "content"));
  CrashReport report({
      .program_name = "crashing_program_generic",
      .attachments = std::move(attachments),
  });

  EXPECT_THAT(FileOneCrashReport(std::move(report)), IsFilingError(FilingError::kInvalidArgsError));
}

TEST_F(CrashReporterTest, FailOnInvalidInputAttachmentKeyContainsSpace) {
  SetUpDataProviderServer(std::make_unique<stubs::DataProviderReturnsEmptySnapshot>());
  SetUpCrashReporterDefaultConfig();

  std::vector<Attachment> attachments;
  attachments.emplace_back(BuildAttachment("my attachment.key", "content"));
  CrashReport report({
      .program_name = "crashing_program_generic",
      .attachments = std::move(attachments),
  });

  EXPECT_THAT(FileOneCrashReport(std::move(report)), IsFilingError(FilingError::kInvalidArgsError));
}

TEST_F(CrashReporterTest, FailOnInvalidInputAttachmentKeyContainsNonPrintableAscii) {
  SetUpDataProviderServer(std::make_unique<stubs::DataProviderReturnsEmptySnapshot>());
  SetUpCrashReporterDefaultConfig();

  std::vector<Attachment> attachments;
  attachments.emplace_back(BuildAttachment(std::string("key\0with_null", 13), "content"));
  CrashReport report({
      .program_name = "crashing_program_generic",
      .attachments = std::move(attachments),
  });

  EXPECT_THAT(FileOneCrashReport(std::move(report)), IsFilingError(FilingError::kInvalidArgsError));
}

TEST_F(CrashReporterTest, FailOnInvalidInputAttachmentKeyIsDot) {
  SetUpDataProviderServer(std::make_unique<stubs::DataProviderReturnsEmptySnapshot>());
  SetUpCrashReporterDefaultConfig();

  std::vector<Attachment> attachments;
  attachments.emplace_back(BuildAttachment(".", "content"));
  CrashReport report({
      .program_name = "crashing_program_generic",
      .attachments = std::move(attachments),
  });

  EXPECT_THAT(FileOneCrashReport(std::move(report)), IsFilingError(FilingError::kInvalidArgsError));
}

TEST_F(CrashReporterTest, FailOnInvalidInputAttachmentKeyIsDotDot) {
  SetUpDataProviderServer(std::make_unique<stubs::DataProviderReturnsEmptySnapshot>());
  SetUpCrashReporterDefaultConfig();

  std::vector<Attachment> attachments;
  attachments.emplace_back(BuildAttachment("..", "content"));
  CrashReport report({
      .program_name = "crashing_program_generic",
      .attachments = std::move(attachments),
  });

  EXPECT_THAT(FileOneCrashReport(std::move(report)), IsFilingError(FilingError::kInvalidArgsError));
}

struct ReservedAttachmentKeyTestParam {
  std::string test_name;
  std::string key;
};

class ReservedAttachmentKeyTest
    : public CrashReporterTest,
      public testing::WithParamInterface<ReservedAttachmentKeyTestParam> {};

INSTANTIATE_TEST_SUITE_P(WithVariousShutdownActions, ReservedAttachmentKeyTest,
                         ::testing::ValuesIn(std::vector<ReservedAttachmentKeyTestParam>({
                             {
                                 "AnnotationsJson",
                                 feedback_data::kAttachmentAnnotations,
                             },
                             {
                                 "MinidumpDmp",
                                 feedback_data::kMinidumpFilename,
                             },
                             {
                                 "SnapshotUuidTxt",
                                 feedback_data::kSnapshotUuidFilename,
                             },
                             {
                                 "SnapshotZip",
                                 feedback_data::kSnapshotFilename,
                             },
                             {
                                 "UploadFileMinidump",
                                 feedback_data::kMinidumpUploadKey,
                             },
                         })),
                         [](const testing::TestParamInfo<ReservedAttachmentKeyTestParam>& info) {
                           return info.param.test_name;
                         });

TEST_P(ReservedAttachmentKeyTest, FailOnInvalidInputAttachmentKeyIsReserved) {
  const ReservedAttachmentKeyTestParam& param = GetParam();

  SetUpDataProviderServer(std::make_unique<stubs::DataProviderReturnsEmptySnapshot>());
  SetUpCrashReporterDefaultConfig();

  std::vector<Attachment> attachments;
  attachments.emplace_back(BuildAttachment(param.key, "content"));
  CrashReport report({
      .program_name = "crashing_program_generic",
      .attachments = std::move(attachments),
  });

  EXPECT_THAT(FileOneCrashReport(std::move(report)), IsFilingError(FilingError::kInvalidArgsError));
}

TEST_F(CrashReporterTest, Succeed_OnInputCrashReportWithIsFatalTrue) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful});

  ASSERT_TRUE(FileOneCrashReportWithIsFatal(/*is_fatal=*/true).is_ok());
  CheckAnnotationsOnServer({
      {"isFatal", "true"},
  });
  CheckAttachmentsOnServer({kDefaultAttachmentBundleKey});
}

TEST_F(CrashReporterTest, Succeed_OnInputCrashReportWithIsFatalFalse) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful});

  ASSERT_TRUE(FileOneCrashReportWithIsFatal(/*is_fatal=*/false).is_ok());
  CheckAnnotationsOnServer({
      {"isFatal", "false"},
  });
  CheckAttachmentsOnServer({kDefaultAttachmentBundleKey});
}

TEST_F(CrashReporterTest, Upload_OnUserAlreadyOptedInDataSharing) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporter(
      /*crash_report_upload_policy=*/feedback::CrashReportUploadPolicy::kReadFromPrivacySettings,
      /*daily_per_product_quota=*/kDailyPerProductQuota,
      /*enable_hourly_snapshots=*/true, std::vector({kUploadSuccessful}));
  SetUpPrivacySettingsServer(std::make_unique<fakes::PrivacySettings>());
  SetPrivacySettings(kUserOptInDataSharing);

  ASSERT_TRUE(FileOneCrashReport().is_ok());
  CheckAnnotationsOnServer();
  CheckAttachmentsOnServer({kDefaultAttachmentBundleKey});
}

TEST_F(CrashReporterTest, Archive_OnUserAlreadyOptedOutDataSharing) {
  SetUpDataProviderServer(std::make_unique<stubs::DataProviderTracksNumCalls>(0u));
  SetUpCrashReporter(
      /*crash_report_upload_policy=*/feedback::CrashReportUploadPolicy::kReadFromPrivacySettings,
      /*daily_per_product_quota=*/kDailyPerProductQuota,
      /*enable_hourly_snapshots=*/true);
  SetUpPrivacySettingsServer(std::make_unique<fakes::PrivacySettings>());
  SetPrivacySettings(kUserOptOutDataSharing);
  RunLoopUntilIdle();

  const fidl::Result<fuchsia_feedback::CrashReporter::FileReport> result = FileOneCrashReport();
  ASSERT_TRUE(result.is_ok());
  EXPECT_EQ(result->results().result(), FilingSuccess::kReportNotFiledUserOptedOut);
}

TEST_F(CrashReporterTest, Upload_OnceUserOptInDataSharing) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporter(
      /*crash_report_upload_policy=*/feedback::CrashReportUploadPolicy::kReadFromPrivacySettings,
      /*daily_per_product_quota=*/kDailyPerProductQuota,
      /*enable_hourly_snapshots=*/true, std::vector({kUploadSuccessful}));
  SetUpPrivacySettingsServer(std::make_unique<fakes::PrivacySettings>());

  ASSERT_TRUE(FileOneCrashReport().is_ok());
  CheckServerStillExpectRequests();

  SetPrivacySettings(kUserOptInDataSharing);
  ASSERT_TRUE(RunLoopUntilIdle());

  CheckAnnotationsOnServer();
  CheckAttachmentsOnServer({kDefaultAttachmentBundleKey});
}

TEST_F(CrashReporterTest, Succeed_OnFailedUploadPutIntoDisk) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kEmptyAttachmentBundleKey));
  SetUpCrashReporter(
      /*crash_report_upload_policy=*/feedback::CrashReportUploadPolicy::kEnabled,
      /*daily_per_product_quota=*/kDailyPerProductQuota,
      /*enable_hourly_snapshots=*/true, std::vector({kUploadFailed}));

  const fidl::Result<fuchsia_feedback::CrashReporter::FileReport> result = FileOneCrashReport();
  ASSERT_TRUE(result.is_ok());
  EXPECT_EQ(result->results().result(), FilingSuccess::kReportOnDisk);
}

TEST_F(CrashReporterTest, Succeed_OnFailedUploadPutIntoMemory) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kEmptyAttachmentBundleKey));
  SetUpCrashReporter(
      /*crash_report_upload_policy=*/feedback::CrashReportUploadPolicy::kEnabled,
      /*daily_per_product_quota=*/kDailyPerProductQuota,
      /*enable_hourly_snapshots=*/true, std::vector({kUploadFailed}),
      /*max_reports_cache_size=*/StorageSize::Bytes(0));

  const fidl::Result<fuchsia_feedback::CrashReporter::FileReport> result = FileOneCrashReport();
  ASSERT_TRUE(result.is_ok());
  EXPECT_EQ(result->results().result(), FilingSuccess::kReportInMemory);
}

TEST_F(CrashReporterTest, Fail_OnThrottledUpload) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kEmptyAttachmentBundleKey));
  SetUpCrashReporter(
      /*crash_report_upload_policy=*/feedback::CrashReportUploadPolicy::kEnabled,
      /*daily_per_product_quota=*/kDailyPerProductQuota,
      /*enable_hourly_snapshots=*/true, std::vector({kUploadThrottled}));

  EXPECT_THAT(FileOneCrashReport(), IsFilingError(FilingError::kServerError));
}

TEST_F(CrashReporterTest, Fail_OnUploadTimeout) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kEmptyAttachmentBundleKey));
  SetUpCrashReporter(
      /*crash_report_upload_policy=*/feedback::CrashReportUploadPolicy::kEnabled,
      /*daily_per_product_quota=*/kDailyPerProductQuota,
      /*enable_hourly_snapshots=*/true, std::vector({kUploadTimeout}));

  EXPECT_THAT(FileOneCrashReport(), IsFilingError(FilingError::kServerError));
}

TEST_F(CrashReporterTest, Succeed_OnDisabledUpload) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kEmptyAttachmentBundleKey));
  SetUpCrashReporter(
      /*crash_report_upload_policy=*/feedback::CrashReportUploadPolicy::kDisabled,
      /*daily_per_product_quota=*/std::nullopt,
      /*enable_hourly_snapshots=*/true);

  const fidl::Result<fuchsia_feedback::CrashReporter::FileReport> result = FileOneCrashReport();
  ASSERT_TRUE(result.is_ok());
  EXPECT_EQ(result->results().result(), FilingSuccess::kReportOnDisk);
}

TEST_F(CrashReporterTest, Succeed_OnNoFeedbackAttachments) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProviderReturnsNoAttachment>(kFeedbackAnnotations));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful});

  EXPECT_TRUE(FileOneCrashReportWithSingleAttachment().is_ok());
  CheckAnnotationsOnServer({{feedback::kDebugSnapshotPresentKey, "false"}});
  CheckAttachmentsOnServer({kSingleAttachmentKey});
}

TEST_F(CrashReporterTest, Upload_HourlySnapshot) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful, kUploadSuccessful});

  RunLoopFor(zx::min(5));
  EXPECT_THAT(crash_server_->latest_annotations().Raw(),
              IsSupersetOf(Linearize(std::map<std::string, testing::Matcher<std::string>>({
                  {"ptime", Not(IsEmpty())},
                  {"signature", kHourlySnapshotSignature},
              }))));
  CheckAttachmentsOnServer({kDefaultAttachmentBundleKey});

  RunLoopFor(zx::hour(1));
  EXPECT_THAT(crash_server_->latest_annotations().Raw(),
              IsSupersetOf(Linearize(std::map<std::string, testing::Matcher<std::string>>({
                  {"ptime", Not(IsEmpty())},
                  {"signature", kHourlySnapshotSignature},
              }))));
  CheckAttachmentsOnServer({kDefaultAttachmentBundleKey});
}

TEST_F(CrashReporterTest, Skip_HourlySnapshotIfNegativeConsent) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kDefaultAttachmentBundleKey));
  SetUpCrashReporter(
      /*crash_report_upload_policy=*/feedback::CrashReportUploadPolicy::kReadFromPrivacySettings,
      /*daily_per_product_quota=*/kDailyPerProductQuota,
      /*enable_hourly_snapshots=*/true, std::vector<CrashServer::UploadStatus>({}));
  SetUpPrivacySettingsServer(std::make_unique<fakes::PrivacySettings>());
  SetPrivacySettings(kUserOptOutDataSharing);

  RunLoopFor(zx::min(5));
}

TEST_F(CrashReporterTest, Check_CobaltAfterSuccessfulUpload) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kEmptyAttachmentBundleKey));
  SetUpCrashReporterDefaultConfig({kUploadSuccessful});

  EXPECT_TRUE(FileOneCrashReport().is_ok());

  EXPECT_THAT(ReceivedCobaltEvents(),
              UnorderedElementsAreArray({
                  cobalt::Event(cobalt::CrashState::kFiled),
                  cobalt::Event(cobalt::CrashState::kUploaded),
                  cobalt::Event(cobalt::UploadAttemptState::kUploadAttempt, 1u),
                  cobalt::Event(cobalt::UploadAttemptState::kUploaded, 1u),
              }));
}

TEST_F(CrashReporterTest, Check_CobaltAfterQuotaReached) {
  SetUpDataProviderServer(
      std::make_unique<stubs::DataProvider>(kFeedbackAnnotations, kEmptyAttachmentBundleKey));
  SetUpCrashReporter(
      /*crash_report_upload_policy=*/feedback::CrashReportUploadPolicy::kEnabled,
      /*daily_per_product_quota=*/0u,
      /*enable_hourly_snapshots=*/true);

  EXPECT_THAT(FileOneCrashReport(), IsFilingError(FilingError::kQuotaReachedError));
  EXPECT_THAT(ReceivedCobaltEvents(), UnorderedElementsAreArray({
                                          cobalt::Event(cobalt::CrashState::kOnDeviceQuotaReached),
                                      }));
}

TEST_F(CrashReporterTest, CheckCobaltAfterInvalidInputNoProgramName) {
  SetUpDataProviderServer(std::make_unique<stubs::DataProviderReturnsEmptySnapshot>());
  SetUpCrashReporterDefaultConfig();

  EXPECT_THAT(FileOneEmptyCrashReport(), IsFilingError(FilingError::kInvalidArgsError));
  EXPECT_THAT(ReceivedCobaltEvents(), UnorderedElementsAreArray({
                                          cobalt::Event(cobalt::CrashState::kDropped),
                                      }));
}

TEST_F(CrashReporterTest, CheckCobaltAfterInvalidInputZeroWeight) {
  SetUpDataProviderServer(std::make_unique<stubs::DataProviderReturnsEmptySnapshot>());
  SetUpCrashReporterDefaultConfig();

  CrashReport report({
      .program_name = "crashing_program_generic",
      .weight = 0,
  });
  EXPECT_THAT(FileOneCrashReport(std::move(report)), IsFilingError(FilingError::kInvalidArgsError));
  EXPECT_THAT(ReceivedCobaltEvents(), UnorderedElementsAreArray({
                                          cobalt::Event(cobalt::CrashState::kDropped),
                                      }));
}

TEST_F(CrashReporterTest, Crash_UploadDisabledWithQuota) {
  ASSERT_DEATH(
      {
        SetUpDataProviderServer(std::make_unique<stubs::DataProvider>(kFeedbackAnnotations,
                                                                      kDefaultAttachmentBundleKey));
        SetUpCrashReporter(
            /*crash_report_upload_policy=*/feedback::CrashReportUploadPolicy::kDisabled,
            /*daily_per_product_quota=*/kDailyPerProductQuota,
            /*enable_hourly_snapshots=*/true);
      },
      HasSubstr("quota is 100"));
}

}  // namespace
}  // namespace crash_reports
}  // namespace forensics
