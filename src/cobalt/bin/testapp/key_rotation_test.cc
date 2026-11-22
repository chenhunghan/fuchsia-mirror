// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.net.reachability/cpp/fidl.h>
#include <fuchsia/cobalt/cpp/fidl.h>
#include <fuchsia/component/cpp/fidl.h>
#include <fuchsia/component/decl/cpp/fidl.h>
#include <fuchsia/diagnostics/cpp/fidl.h>
#include <fuchsia/io/cpp/fidl.h>
#include <fuchsia/mem/cpp/fidl.h>
#include <fuchsia/metrics/cpp/fidl.h>
#include <fuchsia/net/http/cpp/fidl.h>
#include <fuchsia/time/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/fdio/directory.h>
#include <lib/fidl/cpp/binding_set.h>
#include <lib/fidl/cpp/synchronous_interface_ptr.h>
#include <lib/sys/component/cpp/testing/realm_builder.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/vfs/cpp/pseudo_dir.h>
#include <lib/vfs/cpp/remote_dir.h>
#include <lib/zx/channel.h>
#include <lib/zx/clock.h>
#include <lib/zx/socket.h>
#include <lib/zx/time.h>

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <deque>
#include <future>
#include <map>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include <google/protobuf/io/zero_copy_stream_impl_lite.h>
#include <google/protobuf/util/delimited_message_util.h>
#include <gtest/gtest.h>
#include <rapidjson/document.h>
#include <rapidjson/pointer.h>

#include "src/cobalt/bin/testapp/cobalt_testapp_logger.h"
#include "src/cobalt/bin/testapp/test_constants.h"
#include "src/cobalt/bin/testapp/testapp_metrics_registry.cb.h"
#include "src/cobalt/bin/utils/error_utils.h"
#include "src/lib/files/directory.h"
#include "src/lib/files/file.h"
#include "src/lib/files/path.h"
#include "src/lib/fsl/vmo/strings.h"
#include "third_party/cobalt/src/lib/clearcut/clearcut.pb.h"
#include "third_party/cobalt/src/lib/util/datetime_util.h"
#include "third_party/cobalt/src/observation_store/observation_store_internal.pb.h"
#include "third_party/cobalt/src/pb/cobalt_public_keys.pb.h"
#include "third_party/cobalt/src/pb/encrypted_message.pb.h"
#include "third_party/cobalt/src/pb/key.pb.h"

namespace cobalt::testapp {
namespace {

constexpr char kDefaultPublicKeysUrl[] = "https://www.gstatic.com/cobalt/public_keys.pb";
constexpr char kKeyRotationDataDir[] = "/tmp/key_rotation_test_data";
constexpr char kKeyRotationDataCapability[] = "cobalt-data";
constexpr char kKeyRotationDataSourcePath[] = "/cobalt-data";
constexpr char kFakeServicesChild[] = "fake_services";
constexpr char kCobaltUnderTestChild[] = "cobalt_under_test";
constexpr char kCobaltNoEventAggregatorWorker[] = "#meta/cobalt_no_event_aggregator_worker.cm";
// TODO(https://fxbug.dev/539564173): Replace reading the bundled devel public
// key file with an embedded test Tink public keyset once legacy bundled static
// keys are removed from the package.
constexpr char kBundledDevelShufflerKeyPath[] = "/pkg/data/keys/shuffler_devel_public";
// Fixed initial fake UTC timestamp: 2023-11-15T12:00:00Z (noon UTC, 12 hours
// away from any UTC day boundary).
constexpr int64_t kInitialFakeTimeSecs = 1700049600;
constexpr int64_t kBootstrapExpiredOffsetSecs = 60;
constexpr uint32_t kBootstrapExpiredShufflerKeyIndex = 10;
constexpr uint32_t kBootstrapExpiredAnalyzerKeyIndex = 11;
constexpr uint32_t kKeyAShufflerIndex = 100;
constexpr uint32_t kKeyAAnalyzerIndex = 101;
constexpr uint32_t kKeyBShufflerIndex = 200;
constexpr uint32_t kKeyBAnalyzerIndex = 201;
constexpr int64_t kKeyALifetimeSecs = 600;
constexpr int64_t kStandardKeyLifetimeSecs = 3600;
constexpr auto kInspectLogInterval = std::chrono::seconds(5);

// Summary of observation records currently stored on disk in
// `/data/observation_store`.
struct OnDiskObservationCounts {
  // Number of unencrypted observation records for the target `project_id` in
  // finalized store files.
  size_t project_unencrypted_count = 0;

  // Total number of encrypted observation records across finalized store files.
  size_t total_encrypted_count = 0;

  // Whether an unfinalized `in_progress.data` file is present in the store.
  bool has_in_progress_file = false;
};

class FakeKeyRotationHttpLoader : public fuchsia::net::http::Loader {
 public:
  void Fetch(fuchsia::net::http::Request request, FetchCallback callback) override {
    std::lock_guard<std::mutex> lock(mutex_);
    const std::string method = request.has_method() ? request.method() : "GET";
    if (method == "GET") {
      callback(HandleGet(request));
      return;
    }
    if (method == "POST") {
      callback(HandlePost(request));
      return;
    }

    fuchsia::net::http::Response response;
    response.set_status_code(400);
    callback(std::move(response));
  }

  void Start(fuchsia::net::http::Request /*request*/,
             fidl::InterfaceHandle<fuchsia::net::http::LoaderClient> /*client*/) override {
    FX_NOTIMPLEMENTED();
  }

  fidl::InterfaceRequestHandler<fuchsia::net::http::Loader> GetHandler(
      async_dispatcher_t* dispatcher) {
    return bindings_.GetHandler(this, dispatcher);
  }

  void SetNetworkConnected(bool connected) {
    std::lock_guard<std::mutex> lock(mutex_);
    network_connected_ = connected;
  }

  void SetPublicKeysBytes(std::string bytes) {
    std::lock_guard<std::mutex> lock(mutex_);
    public_keys_bytes_ = std::move(bytes);
  }

  uint32_t PostRequestCount() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return post_request_count_;
  }

  std::vector<uint32_t> TakeUploadedShufflerKeyIndices() {
    std::lock_guard<std::mutex> lock(mutex_);
    std::vector<uint32_t> result = std::move(uploaded_shuffler_key_indices_);
    uploaded_shuffler_key_indices_.clear();
    return result;
  }

 private:
  fuchsia::net::http::Response HandleGet(const fuchsia::net::http::Request& request) const {
    fuchsia::net::http::Response response;
    if (!network_connected_) {
      response.set_error(fuchsia::net::http::Error::CONNECT);
      return response;
    }
    if (!request.has_url() || request.url() != kDefaultPublicKeysUrl) {
      response.set_status_code(404);
      return response;
    }
    response.set_status_code(200);
    response.set_body(CreateSocketWithBytes(public_keys_bytes_));
    return response;
  }

  fuchsia::net::http::Response HandlePost(const fuchsia::net::http::Request& request) {
    ++post_request_count_;
    fuchsia::net::http::Response response;
    if (!network_connected_) {
      response.set_error(fuchsia::net::http::Error::CONNECT);
      return response;
    }
    if (request.has_body() && request.body().is_buffer()) {
      RecordUploadedKeyIndices(request.body().buffer());
    }
    response.set_status_code(200);
    return response;
  }

  static zx::socket CreateSocketWithBytes(const std::string& bytes) {
    zx::socket tx, rx;
    FX_CHECK(zx::socket::create(ZX_SOCKET_STREAM, &tx, &rx) == ZX_OK);
    size_t offset = 0;
    while (offset < bytes.size()) {
      size_t actual = 0;
      const zx_status_t status = tx.write(0, bytes.data() + offset, bytes.size() - offset, &actual);
      FX_CHECK(status == ZX_OK) << "zx::socket::write failed: " << status;
      offset += actual;
    }
    tx.reset();
    return rx;
  }

  void RecordUploadedKeyIndices(const fuchsia::mem::Buffer& buffer) {
    std::string body_bytes;
    if (!fsl::StringFromVmo(buffer, &body_bytes)) {
      return;
    }
    clearcut_protos::LogRequest log_request;
    if (!log_request.ParseFromString(body_bytes)) {
      return;
    }
    for (const auto& event : log_request.log_event()) {
      if (!event.has_source_extension()) {
        continue;
      }
      EncryptedMessage encrypted_envelope;
      if (encrypted_envelope.ParseFromString(event.source_extension()) &&
          !encrypted_envelope.ciphertext().empty()) {
        uploaded_shuffler_key_indices_.push_back(encrypted_envelope.key_index());
      }
    }
  }

  mutable std::mutex mutex_;
  fidl::BindingSet<fuchsia::net::http::Loader> bindings_;
  bool network_connected_ = false;
  std::string public_keys_bytes_;
  uint32_t post_request_count_ = 0;
  std::vector<uint32_t> uploaded_shuffler_key_indices_;
};

class FakeKeyRotationReachabilityMonitor : public fidl::Server<fuchsia_net_reachability::Monitor> {
 public:
  void SetOptions(SetOptionsRequest& /*request*/,
                  SetOptionsCompleter::Sync& /*completer*/) override {}

  void Watch(WatchCompleter::Sync& completer) override {
    if (!queued_snapshots_.empty()) {
      fuchsia_net_reachability::Snapshot snapshot = std::move(queued_snapshots_.front());
      queued_snapshots_.pop_front();
      completer.Reply(
          fidl::Response<fuchsia_net_reachability::Monitor::Watch>(std::move(snapshot)));
      return;
    }
    pending_completer_ = completer.ToAsync();
  }

  void Bind(async_dispatcher_t* dispatcher,
            fidl::ServerEnd<fuchsia_net_reachability::Monitor> server_end) {
    pending_completer_.reset();
    binding_ = fidl::BindServer(dispatcher, std::move(server_end), this);
  }

  void SetHttpActive(bool http_active) {
    fuchsia_net_reachability::Snapshot snapshot;
    snapshot.http_active(http_active);
    if (pending_completer_.has_value()) {
      WatchCompleter::Async completer = std::move(*pending_completer_);
      pending_completer_.reset();
      completer.Reply(
          fidl::Response<fuchsia_net_reachability::Monitor::Watch>(std::move(snapshot)));
      return;
    }
    queued_snapshots_.push_back(std::move(snapshot));
  }

 private:
  std::optional<fidl::ServerBindingRef<fuchsia_net_reachability::Monitor>> binding_;
  std::optional<WatchCompleter::Async> pending_completer_;
  std::deque<fuchsia_net_reachability::Snapshot> queued_snapshots_;
};

class FakeKeyRotationTimeMaintenance : public fuchsia::time::Maintenance {
 public:
  explicit FakeKeyRotationTimeMaintenance(const zx::clock* utc_clock) : utc_clock_(utc_clock) {}

  void GetWritableUtcClock(GetWritableUtcClockCallback callback) override {
    zx::clock duplicate;
    FX_CHECK(utc_clock_->duplicate(ZX_RIGHT_SAME_RIGHTS, &duplicate) == ZX_OK);
    callback(std::move(duplicate));
  }

  fidl::InterfaceRequestHandler<fuchsia::time::Maintenance> GetHandler(
      async_dispatcher_t* dispatcher) {
    return bindings_.GetHandler(this, dispatcher);
  }

 private:
  const zx::clock* const utc_clock_;
  fidl::BindingSet<fuchsia::time::Maintenance> bindings_;
};

class KeyRotationFakeServicesComponent : public component_testing::LocalComponentImpl {
 public:
  KeyRotationFakeServicesComponent(async_dispatcher_t* dispatcher,
                                   FakeKeyRotationHttpLoader* http_loader,
                                   FakeKeyRotationReachabilityMonitor* reachability_monitor,
                                   FakeKeyRotationTimeMaintenance* time_maintenance,
                                   std::string data_dir_path)
      : dispatcher_(dispatcher),
        http_loader_(http_loader),
        reachability_monitor_(reachability_monitor),
        time_maintenance_(time_maintenance),
        data_dir_path_(std::move(data_dir_path)) {}

  void OnStart() override {
    FX_CHECK(outgoing()->AddPublicService(http_loader_->GetHandler(dispatcher_)) == ZX_OK);
    FX_CHECK(outgoing()->AddPublicService(time_maintenance_->GetHandler(dispatcher_)) == ZX_OK);
    FX_CHECK(outgoing()->AddProtocol<fuchsia_net_reachability::Monitor>(
                 [this](fidl::ServerEnd<fuchsia_net_reachability::Monitor> server_end) {
                   reachability_monitor_->Bind(dispatcher_, std::move(server_end));
                 }) == ZX_OK);

    zx::channel local_dir, remote_dir;
    FX_CHECK(zx::channel::create(0, &local_dir, &remote_dir) == ZX_OK);
    const zx_status_t open_status =
        fdio_open3(data_dir_path_.c_str(),
                   static_cast<uint64_t>(fuchsia::io::PERM_READABLE | fuchsia::io::PERM_WRITABLE |
                                         fuchsia::io::Flags::PROTOCOL_DIRECTORY),
                   remote_dir.release());
    FX_CHECK(open_status == ZX_OK)
        << "fdio_open3 failed for " << data_dir_path_ << ": " << open_status;
    FX_CHECK(outgoing()->root_dir()->AddEntry(
                 kKeyRotationDataCapability,
                 std::make_unique<vfs::RemoteDir>(std::move(local_dir))) == ZX_OK);
  }

 private:
  async_dispatcher_t* const dispatcher_;
  FakeKeyRotationHttpLoader* const http_loader_;
  FakeKeyRotationReachabilityMonitor* const reachability_monitor_;
  FakeKeyRotationTimeMaintenance* const time_maintenance_;
  const std::string data_dir_path_;
};

[[nodiscard]] std::string LoadTestTinkPublicKeysetBytes() {
  std::string key_bytes;
  FX_CHECK(files::ReadFileToString(kBundledDevelShufflerKeyPath, &key_bytes));
  CobaltEncryptionKey bundled_key;
  FX_CHECK(bundled_key.ParseFromString(key_bytes));
  FX_CHECK(!bundled_key.serialized_key().empty());
  return bundled_key.serialized_key();
}

[[nodiscard]] std::string SerializePublicKeys(const std::string& tink_keyset_bytes,
                                              uint32_t shuffler_key_index,
                                              uint32_t analyzer_key_index,
                                              int64_t expiration_timestamp_secs) {
  CobaltPublicKeys keys;
  keys.mutable_dev_shuffler_key()->set_public_tink_keyset(tink_keyset_bytes);
  keys.mutable_dev_shuffler_key()->set_key_index(shuffler_key_index);
  keys.mutable_dev_shuffler_key()->set_expiration_timestamp_secs(expiration_timestamp_secs);

  keys.mutable_dev_analyzer_key()->set_public_tink_keyset(tink_keyset_bytes);
  keys.mutable_dev_analyzer_key()->set_key_index(analyzer_key_index);
  keys.mutable_dev_analyzer_key()->set_expiration_timestamp_secs(expiration_timestamp_secs);

  keys.mutable_prod_shuffler_key()->set_public_tink_keyset(tink_keyset_bytes);
  keys.mutable_prod_shuffler_key()->set_key_index(shuffler_key_index);
  keys.mutable_prod_shuffler_key()->set_expiration_timestamp_secs(expiration_timestamp_secs);

  keys.mutable_prod_analyzer_key()->set_public_tink_keyset(tink_keyset_bytes);
  keys.mutable_prod_analyzer_key()->set_key_index(analyzer_key_index);
  keys.mutable_prod_analyzer_key()->set_expiration_timestamp_secs(expiration_timestamp_secs);

  std::string serialized;
  FX_CHECK(keys.SerializeToString(&serialized));
  return serialized;
}

class CobaltKeyRotationTest : public ::testing::Test {
 protected:
  CobaltKeyRotationTest()
      : context_(sys::ComponentContext::CreateAndServeOutgoingDirectory()),
        time_maintenance_(&fake_utc_clock_),
        fake_services_loop_(&kAsyncLoopConfigNoAttachToCurrentThread),
        logger_(/*use_network=*/false, &cobalt_controller_, &inspect_archive_) {}

  void SetUp() override {
    tink_keyset_bytes_ = LoadTestTinkPublicKeysetBytes();
    ASSERT_EQ(ZX_OK, context_->svc()->Connect(inspect_archive_.NewRequest()));

    const zx::time initial_time(kInitialFakeTimeSecs * ZX_SEC(1));
    zx_clock_create_args_v1_t clock_args{.backstop_time = initial_time.get()};
    ASSERT_EQ(ZX_OK, zx::clock::create(0u, &clock_args, &fake_utc_clock_));
    ASSERT_EQ(ZX_OK, fake_utc_clock_.update(zx::clock::update_args().set_value(initial_time)));
    day_index_ =
        util::TimeToDayIndex(static_cast<time_t>(kInitialFakeTimeSecs), MetricDefinition::UTC);

    ASSERT_TRUE(files::DeletePath(kKeyRotationDataDir, /*recursive=*/true));
    const std::string keys_dir = files::JoinPath(kKeyRotationDataDir, "keys");
    ASSERT_TRUE(files::CreateDirectory(keys_dir));

    // TODO(https://fxbug.dev/539564173): Until legacy bundled static fallback
    // keys are removed from CobaltApp, pre-seed `/data/keys/public_keys.pb`
    // with an already-expired dynamic keyset so `KeyFetcher::LoadKeysFromDiskCache()`
    // replaces the non-expiring bundled fallback keys and boots in the
    // fail-closed state without valid active keys.
    const std::string bootstrap_expired_keys =
        BuildPublicKeysBytes(kBootstrapExpiredShufflerKeyIndex, kBootstrapExpiredAnalyzerKeyIndex,
                             CurrentFakeTimeSecs() - kBootstrapExpiredOffsetSecs);
    const std::string cached_keys_path = files::JoinPath(keys_dir, "public_keys.pb");
    ASSERT_TRUE(files::WriteFile(cached_keys_path, bootstrap_expired_keys));

    ASSERT_EQ(ZX_OK, fake_services_loop_.StartThread("key_rotation_fake_services_loop"));

    auto realm_builder = component_testing::RealmBuilder::Create(context_->svc());
    realm_builder.AddChild(kCobaltUnderTestChild, kCobaltNoEventAggregatorWorker);
    realm_builder.AddLocalChild(kFakeServicesChild, [this]() {
      return std::make_unique<KeyRotationFakeServicesComponent>(
          fake_services_loop_.dispatcher(), &http_loader_, &reachability_monitor_,
          &time_maintenance_, kKeyRotationDataDir);
    });

    fuchsia::component::decl::Component cobalt_decl =
        realm_builder.GetComponentDecl(kCobaltUnderTestChild);
    bool replaced_data_use = false;
    for (auto& use_entry : *cobalt_decl.mutable_uses()) {
      if (use_entry.is_storage() && use_entry.storage().has_target_path() &&
          use_entry.storage().target_path() == "/data") {
        fuchsia::component::decl::UseDirectory use_data_dir;
        use_data_dir.set_source(
            fuchsia::component::decl::Ref::WithParent(fuchsia::component::decl::ParentRef{}));
        use_data_dir.set_source_name(kKeyRotationDataCapability);
        use_data_dir.set_target_path("/data");
        use_data_dir.set_rights(fuchsia::io::RW_STAR_DIR);
        use_data_dir.set_dependency_type(fuchsia::component::decl::DependencyType::STRONG);
        use_entry = fuchsia::component::decl::Use::WithDirectory(std::move(use_data_dir));
        replaced_data_use = true;
        break;
      }
    }
    ASSERT_TRUE(replaced_data_use) << "Failed to find /data storage use in cobalt_under_test decl";

    fuchsia::component::decl::UseProtocol use_time_maintenance;
    use_time_maintenance.set_source(
        fuchsia::component::decl::Ref::WithParent(fuchsia::component::decl::ParentRef{}));
    use_time_maintenance.set_source_name(fuchsia::time::Maintenance::Name_);
    use_time_maintenance.set_target_path(std::string("/svc/") + fuchsia::time::Maintenance::Name_);
    use_time_maintenance.set_dependency_type(fuchsia::component::decl::DependencyType::STRONG);
    use_time_maintenance.set_availability(fuchsia::component::decl::Availability::REQUIRED);
    cobalt_decl.mutable_uses()->push_back(
        fuchsia::component::decl::Use::WithProtocol(std::move(use_time_maintenance)));

    realm_builder.ReplaceComponentDecl(kCobaltUnderTestChild, std::move(cobalt_decl));

    realm_builder.AddRoute(component_testing::Route{
        .capabilities =
            {
                component_testing::Protocol{"fuchsia.net.http.Loader"},
                component_testing::Protocol{"fuchsia.net.reachability.Monitor"},
                component_testing::Protocol{fuchsia::time::Maintenance::Name_},
                component_testing::Directory{
                    .name = kKeyRotationDataCapability,
                    .rights = fuchsia::io::RW_STAR_DIR,
                    .path = kKeyRotationDataSourcePath,
                },
            },
        .source = component_testing::ChildRef{kFakeServicesChild},
        .targets = {component_testing::ChildRef{kCobaltUnderTestChild}},
    });
    realm_builder.AddRoute(component_testing::Route{
        .capabilities =
            {
                component_testing::Directory{
                    .name = "config-data",
                    .rights = fuchsia::io::R_STAR_DIR,
                    .path = "/config/data",
                },
                component_testing::Protocol{"fuchsia.scheduler.RoleManager"},
                component_testing::Protocol{"fuchsia.sysinfo.SysInfo"},
                component_testing::Protocol{"fuchsia.tracing.provider.Registry"},
                component_testing::Protocol{"fuchsia.update.channel.Provider"},
            },
        .source = component_testing::ParentRef{},
        .targets = {component_testing::ChildRef{kCobaltUnderTestChild}},
    });
    realm_builder.AddRoute(component_testing::Route{
        .capabilities =
            {
                component_testing::Protocol{"fuchsia.cobalt.Controller"},
                component_testing::Protocol{"fuchsia.metrics.MetricEventLoggerFactory"},
            },
        .source = component_testing::ChildRef{kCobaltUnderTestChild},
        .targets = {component_testing::ParentRef{}},
    });

    realm_root_.emplace(realm_builder.Build(fake_services_loop_.dispatcher()));
    logger_.SetCobaltUnderTestMoniker("realm_builder\\:" + realm_root_->component().GetChildName() +
                                      "/" + kCobaltUnderTestChild);

    fuchsia::metrics::MetricEventLoggerFactorySyncPtr metric_event_logger_factory =
        realm_root_->component().ConnectSync<fuchsia::metrics::MetricEventLoggerFactory>();

    fuchsia::metrics::MetricEventLoggerFactory_CreateMetricEventLogger_Result result;
    fuchsia::metrics::ProjectSpec project;
    project.set_customer_id(1);
    project.set_project_id(project_id_);
    ASSERT_EQ(ZX_OK, metric_event_logger_factory->CreateMetricEventLogger(
                         std::move(project), logger_.metric_event_logger_.NewRequest(), &result));
    ASSERT_FALSE(result.is_err()) << "CreateMetricEventLogger() => " << ErrorToString(result.err());

    cobalt_controller_ = realm_root_->component().ConnectSync<fuchsia::cobalt::Controller>();
    ASSERT_EQ(ZX_OK, cobalt_controller_->ListenForInitialized());
  }

  void TearDown() override {
    if (realm_root_.has_value()) {
      std::promise<void> teardown_done;
      async::PostTask(fake_services_loop_.dispatcher(), [this, &teardown_done]() {
        realm_root_->Teardown([&teardown_done](fit::result<fuchsia::component::Error> /*result*/) {
          teardown_done.set_value();
        });
      });
      teardown_done.get_future().wait();
    }
    fake_services_loop_.Shutdown();
    realm_root_.reset();
    EXPECT_TRUE(files::DeletePath(kKeyRotationDataDir, /*recursive=*/true));
  }

  void SetNetworkConnected(bool connected) {
    http_loader_.SetNetworkConnected(connected);
    std::promise<void> completion;
    async::PostTask(fake_services_loop_.dispatcher(), [this, connected, &completion]() {
      reachability_monitor_.SetHttpActive(connected);
      completion.set_value();
    });
    completion.get_future().wait();
  }

  void SetPublicKeysBytes(std::string bytes) { http_loader_.SetPublicKeysBytes(std::move(bytes)); }

  [[nodiscard]] std::string BuildPublicKeysBytes(uint32_t shuffler_key_index,
                                                 uint32_t analyzer_key_index,
                                                 int64_t expiration_timestamp_secs) const {
    return SerializePublicKeys(tink_keyset_bytes_, shuffler_key_index, analyzer_key_index,
                               expiration_timestamp_secs);
  }

  [[nodiscard]] bool GenerateObsAndCheckCount(
      const std::map<std::pair<uint32_t, uint32_t>, uint64_t>& expected_num_obs) {
    std::vector<fuchsia::cobalt::ReportSpec> report_specs;
    std::vector<std::pair<uint32_t, uint32_t>> report_ids;
    for (const auto& [ids, expected] : expected_num_obs) {
      const auto& [metric_id, report_id] = ids;
      fuchsia::cobalt::ReportSpec spec = std::move(fuchsia::cobalt::ReportSpec()
                                                       .set_customer_id(1)
                                                       .set_project_id(project_id_)
                                                       .set_metric_id(metric_id)
                                                       .set_report_id(report_id));
      report_specs.push_back(std::move(spec));
      report_ids.push_back(ids);
    }
    std::vector<uint64_t> num_obs;
    if (cobalt_controller_->GenerateAggregatedObservations(day_index_, std::move(report_specs),
                                                           &num_obs) != ZX_OK) {
      return false;
    }
    if (num_obs.size() != report_ids.size()) {
      return false;
    }
    for (size_t i = 0; i < report_ids.size(); ++i) {
      if (num_obs[i] != expected_num_obs.at(report_ids[i])) {
        return false;
      }
    }
    return true;
  }

  void WaitForKeyRotationInspect(const std::string& expected_shuffler_state,
                                 const std::string& expected_analyzer_state,
                                 int64_t expected_shuffler_index, int64_t expected_analyzer_index,
                                 const std::string& expected_fetch_status,
                                 int64_t expected_consecutive_failures) {
    auto next_log_time = std::chrono::steady_clock::now() + kInspectLogInterval;
    while (true) {
      const std::string inspect_json = logger_.GetInspectJson();
      rapidjson::Document inspect;
      inspect.Parse(inspect_json);

      const std::string shuffler_state =
          rapidjson::GetValueByPointerWithDefault(
              inspect,
              rapidjson::Pointer("/payload/root/cobalt_app/core/key_rotation/shuffler_key_state"),
              "")
              .GetString();
      const std::string analyzer_state =
          rapidjson::GetValueByPointerWithDefault(
              inspect,
              rapidjson::Pointer("/payload/root/cobalt_app/core/key_rotation/analyzer_key_state"),
              "")
              .GetString();
      const int64_t shuffler_index =
          rapidjson::GetValueByPointerWithDefault(
              inspect,
              rapidjson::Pointer("/payload/root/cobalt_app/core/key_rotation/shuffler_key_index"),
              -1)
              .GetInt64();
      const int64_t analyzer_index =
          rapidjson::GetValueByPointerWithDefault(
              inspect,
              rapidjson::Pointer("/payload/root/cobalt_app/core/key_rotation/analyzer_key_index"),
              -1)
              .GetInt64();
      const std::string fetch_status =
          rapidjson::GetValueByPointerWithDefault(
              inspect,
              rapidjson::Pointer("/payload/root/cobalt_app/core/key_rotation/last_fetch_status"),
              "")
              .GetString();
      const int64_t consecutive_failures =
          rapidjson::GetValueByPointerWithDefault(
              inspect,
              rapidjson::Pointer(
                  "/payload/root/cobalt_app/core/key_rotation/consecutive_fetch_failures"),
              -1)
              .GetInt64();

      if (shuffler_state == expected_shuffler_state && analyzer_state == expected_analyzer_state &&
          shuffler_index == expected_shuffler_index && analyzer_index == expected_analyzer_index &&
          fetch_status == expected_fetch_status &&
          consecutive_failures == expected_consecutive_failures) {
        return;
      }
      if (std::chrono::steady_clock::now() >= next_log_time) {
        FX_LOGS(WARNING)
            << "Still waiting for key_rotation Inspect state (expected shuffler_state='"
            << expected_shuffler_state << "', analyzer_state='" << expected_analyzer_state
            << "', shuffler_index=" << expected_shuffler_index
            << ", analyzer_index=" << expected_analyzer_index << ", last_fetch_status='"
            << expected_fetch_status
            << "', consecutive_fetch_failures=" << expected_consecutive_failures
            << "; actual shuffler_state='" << shuffler_state << "', analyzer_state='"
            << analyzer_state << "', shuffler_index=" << shuffler_index
            << ", analyzer_index=" << analyzer_index << ", last_fetch_status='" << fetch_status
            << "', consecutive_fetch_failures=" << consecutive_failures
            << ").\nFull inspect data: " << inspect_json;
        next_log_time = std::chrono::steady_clock::now() + kInspectLogInterval;
      }
      std::this_thread::sleep_for(std::chrono::milliseconds(20));
    }
  }

  [[nodiscard]] bool VerifyCachedPublicKeysOnDisk(uint32_t expected_shuffler_index,
                                                  uint32_t expected_analyzer_index) const {
    const std::string cache_path = files::JoinPath(kKeyRotationDataDir, "keys/public_keys.pb");
    std::string pb_bytes;
    if (!files::ReadFileToString(cache_path, &pb_bytes) || pb_bytes.empty()) {
      return false;
    }
    CobaltPublicKeys keys;
    if (!keys.ParseFromString(pb_bytes)) {
      return false;
    }
    return keys.dev_shuffler_key().key_index() == expected_shuffler_index &&
           keys.dev_analyzer_key().key_index() == expected_analyzer_index;
  }

  [[nodiscard]] OnDiskObservationCounts CountOnDiskObservations() const {
    OnDiskObservationCounts counts;
    const std::string store_dir = files::JoinPath(kKeyRotationDataDir, "observation_store");
    std::vector<std::string> dir_entries;
    if (!files::ReadDirContents(store_dir, &dir_entries)) {
      return counts;
    }

    for (const std::string& entry : dir_entries) {
      if (entry == "." || entry == "..") {
        continue;
      }
      if (entry == "in_progress.data") {
        counts.has_in_progress_file = true;
        continue;
      }
      const std::string full_path = files::JoinPath(store_dir, entry);
      std::string file_bytes;
      if (!files::ReadFileToString(full_path, &file_bytes)) {
        continue;
      }
      google::protobuf::io::ArrayInputStream iis(file_bytes.data(),
                                                 static_cast<int>(file_bytes.size()));
      uint32_t current_project_id = 0;
      observation_store::FileObservationStoreRecord stored;
      bool clean_eof = false;
      while (google::protobuf::util::ParseDelimitedFromZeroCopyStream(&stored, &iis, &clean_eof)) {
        if (stored.has_meta_data()) {
          current_project_id = stored.meta_data().project_id();
        } else if (stored.has_unencrypted_observation()) {
          if (current_project_id == project_id_) {
            ++counts.project_unencrypted_count;
          }
        } else if (stored.has_encrypted_observation()) {
          ++counts.total_encrypted_count;
        }
        stored.Clear();
      }
    }
    return counts;
  }

  [[nodiscard]] int64_t CurrentFakeTimeSecs() const {
    zx_time_t now_ns = 0;
    FX_CHECK(fake_utc_clock_.read(&now_ns) == ZX_OK);
    return now_ns / ZX_SEC(1);
  }

  void AdvanceFakeClockPast(int64_t expiration_timestamp_secs) {
    const zx::time target_time((expiration_timestamp_secs + 1) * ZX_SEC(1));
    ASSERT_EQ(ZX_OK, fake_utc_clock_.update(zx::clock::update_args().set_value(target_time)));
  }

  const uint32_t project_id_ = cobalt_registry::kProjectId;
  uint32_t day_index_ = 0;
  std::string tink_keyset_bytes_;
  std::unique_ptr<sys::ComponentContext> context_;
  zx::clock fake_utc_clock_;
  FakeKeyRotationHttpLoader http_loader_;
  FakeKeyRotationReachabilityMonitor reachability_monitor_;
  FakeKeyRotationTimeMaintenance time_maintenance_;
  std::optional<component_testing::RealmRoot> realm_root_;
  async::Loop fake_services_loop_;
  fuchsia::cobalt::ControllerSyncPtr cobalt_controller_;
  fuchsia::diagnostics::ArchiveAccessorSyncPtr inspect_archive_;
  CobaltTestAppLogger logger_;
};

TEST_F(CobaltKeyRotationTest, InitialBootWithoutValidKeysBuffersAndUploadsOnFetch) {
  // Verify initial fetch attempt failed while offline and active keys remain in
  // the fail-closed expired state.
  WaitForKeyRotationInspect("expired", "expired", kBootstrapExpiredShufflerKeyIndex,
                            kBootstrapExpiredAnalyzerKeyIndex, "NetworkError",
                            /*expected_consecutive_failures=*/1);

  ASSERT_TRUE(
      logger_.LogInteger(cobalt_registry::kStreamingTimeNewMetricId,
                         {kStreamingTimeNewTypeIndices[0], kStreamingTimeNewModuleNameIndices[0]},
                         kStreamingTimeNewValues[0]));
  ASSERT_TRUE(GenerateObsAndCheckCount(
      {{{cobalt_registry::kStreamingTimeNewMetricId,
         cobalt_registry::kStreamingTimeNewStreamingTimePerDeviceTotalReportId},
        1}}));

  bool send_success = true;
  ASSERT_EQ(ZX_OK, cobalt_controller_->RequestSendSoon(&send_success));
  EXPECT_FALSE(send_success);
  EXPECT_EQ(0u, http_loader_.PostRequestCount());

  const OnDiskObservationCounts initial_disk_counts = CountOnDiskObservations();
  EXPECT_GE(initial_disk_counts.project_unencrypted_count, 1u);
  EXPECT_EQ(0u, initial_disk_counts.total_encrypted_count);

  // Serve Key A with a standard 1-hour lifetime and bring the network online so
  // KeyFetcher fetches Key A.
  const int64_t key_a_expiration_secs = CurrentFakeTimeSecs() + kStandardKeyLifetimeSecs;
  SetPublicKeysBytes(
      BuildPublicKeysBytes(kKeyAShufflerIndex, kKeyAAnalyzerIndex, key_a_expiration_secs));
  SetNetworkConnected(/*connected=*/true);

  WaitForKeyRotationInspect("valid", "valid", kKeyAShufflerIndex, kKeyAAnalyzerIndex, "Success",
                            /*expected_consecutive_failures=*/0);
  EXPECT_TRUE(VerifyCachedPublicKeysOnDisk(kKeyAShufflerIndex, kKeyAAnalyzerIndex));

  send_success = false;
  ASSERT_EQ(ZX_OK, cobalt_controller_->RequestSendSoon(&send_success));
  EXPECT_TRUE(send_success);

  const std::vector<uint32_t> key_a_uploaded_indices =
      http_loader_.TakeUploadedShufflerKeyIndices();
  ASSERT_FALSE(key_a_uploaded_indices.empty());
  EXPECT_TRUE(std::all_of(key_a_uploaded_indices.begin(), key_a_uploaded_indices.end(),
                          [](uint32_t idx) { return idx == kKeyAShufflerIndex; }));

  const OnDiskObservationCounts post_key_a_disk_counts = CountOnDiskObservations();
  EXPECT_EQ(0u, post_key_a_disk_counts.project_unencrypted_count);
  EXPECT_FALSE(post_key_a_disk_counts.has_in_progress_file);
}

TEST_F(CobaltKeyRotationTest, LiveKeyRotationEncryptsWithNewKey) {
  // Serve Key A and bring the network online.
  const int64_t key_a_expiration_secs = CurrentFakeTimeSecs() + kKeyALifetimeSecs;
  SetPublicKeysBytes(
      BuildPublicKeysBytes(kKeyAShufflerIndex, kKeyAAnalyzerIndex, key_a_expiration_secs));
  SetNetworkConnected(/*connected=*/true);

  WaitForKeyRotationInspect("valid", "valid", kKeyAShufflerIndex, kKeyAAnalyzerIndex, "Success",
                            /*expected_consecutive_failures=*/0);

  // Generate and upload an observation under Key A.
  ASSERT_TRUE(
      logger_.LogInteger(cobalt_registry::kStreamingTimeNewMetricId,
                         {kStreamingTimeNewTypeIndices[0], kStreamingTimeNewModuleNameIndices[0]},
                         kStreamingTimeNewValues[0]));
  ASSERT_TRUE(GenerateObsAndCheckCount(
      {{{cobalt_registry::kStreamingTimeNewMetricId,
         cobalt_registry::kStreamingTimeNewStreamingTimePerDeviceTotalReportId},
        1}}));

  bool send_success = false;
  ASSERT_EQ(ZX_OK, cobalt_controller_->RequestSendSoon(&send_success));
  EXPECT_TRUE(send_success);

  const std::vector<uint32_t> key_a_uploaded_indices =
      http_loader_.TakeUploadedShufflerKeyIndices();
  ASSERT_FALSE(key_a_uploaded_indices.empty());
  EXPECT_TRUE(std::all_of(key_a_uploaded_indices.begin(), key_a_uploaded_indices.end(),
                          [](uint32_t idx) { return idx == kKeyAShufflerIndex; }));

  // Advance fake time past Key A's expiration and stage Key B on the HTTP
  // loader while keeping the network online.
  AdvanceFakeClockPast(key_a_expiration_secs);
  const int64_t key_b_expiration_secs = CurrentFakeTimeSecs() + kStandardKeyLifetimeSecs;
  SetPublicKeysBytes(
      BuildPublicKeysBytes(kKeyBShufflerIndex, kKeyBAnalyzerIndex, key_b_expiration_secs));

  // Log and generate an observation after Key A expired. The first
  // RequestSendSoon fails closed on expired Key A, buffering the observation
  // unencrypted on disk until a reachability reconnect triggers key renewal to
  // Key B.
  ASSERT_TRUE(logger_.LogOccurrence(cobalt_registry::kFeaturesActiveNewMetricId,
                                    {kFeaturesActiveNewSkillIndices[0]},
                                    kFeaturesActiveNewCounts[0]));
  ASSERT_TRUE(GenerateObsAndCheckCount(
      {{{cobalt_registry::kFeaturesActiveNewMetricId,
         cobalt_registry::kFeaturesActiveNewFeaturesActiveUniqueDevicesReportId},
        1}}));

  send_success = true;
  ASSERT_EQ(ZX_OK, cobalt_controller_->RequestSendSoon(&send_success));
  EXPECT_FALSE(send_success);

  // Trigger key renewal through the reachability watcher and verify active keys
  // rotate to Key B.
  SetNetworkConnected(/*connected=*/false);
  SetNetworkConnected(/*connected=*/true);
  WaitForKeyRotationInspect("valid", "valid", kKeyBShufflerIndex, kKeyBAnalyzerIndex, "Success",
                            /*expected_consecutive_failures=*/0);
  EXPECT_TRUE(VerifyCachedPublicKeysOnDisk(kKeyBShufflerIndex, kKeyBAnalyzerIndex));

  // Upload the observation buffered during the rotation trigger using Key B.
  send_success = false;
  ASSERT_EQ(ZX_OK, cobalt_controller_->RequestSendSoon(&send_success));
  EXPECT_TRUE(send_success);

  const std::vector<uint32_t> buffered_key_b_indices =
      http_loader_.TakeUploadedShufflerKeyIndices();
  ASSERT_FALSE(buffered_key_b_indices.empty());
  EXPECT_TRUE(std::all_of(buffered_key_b_indices.begin(), buffered_key_b_indices.end(),
                          [](uint32_t idx) { return idx == kKeyBShufflerIndex; }));

  // Log, generate, and upload a brand-new post-rotation observation while Key B
  // is active to verify post-rotation encryption.
  ASSERT_TRUE(logger_.LogString(cobalt_registry::kErrorOccurredComponentsMetricId,
                                {kErrorOccurredComponentsStatusIndices[0]},
                                kApplicationComponentNames[0]));
  ASSERT_TRUE(
      GenerateObsAndCheckCount({{{cobalt_registry::kErrorOccurredComponentsMetricId,
                                  cobalt_registry::kErrorOccurredComponentsErrorCountsReportId},
                                 1}}));

  send_success = false;
  ASSERT_EQ(ZX_OK, cobalt_controller_->RequestSendSoon(&send_success));
  EXPECT_TRUE(send_success);

  const std::vector<uint32_t> post_rotation_key_b_indices =
      http_loader_.TakeUploadedShufflerKeyIndices();
  ASSERT_FALSE(post_rotation_key_b_indices.empty());
  EXPECT_TRUE(std::all_of(post_rotation_key_b_indices.begin(), post_rotation_key_b_indices.end(),
                          [](uint32_t idx) { return idx == kKeyBShufflerIndex; }));

  const OnDiskObservationCounts post_key_b_disk_counts = CountOnDiskObservations();
  EXPECT_EQ(0u, post_key_b_disk_counts.project_unencrypted_count);
  EXPECT_FALSE(post_key_b_disk_counts.has_in_progress_file);
}

TEST_F(CobaltKeyRotationTest, OfflineKeyExpiryBuffersAndUploadsOnReconnect) {
  // Boot and fetch Key A while online.
  const int64_t key_a_expiration_secs = CurrentFakeTimeSecs() + kKeyALifetimeSecs;
  SetPublicKeysBytes(
      BuildPublicKeysBytes(kKeyAShufflerIndex, kKeyAAnalyzerIndex, key_a_expiration_secs));
  SetNetworkConnected(/*connected=*/true);

  WaitForKeyRotationInspect("valid", "valid", kKeyAShufflerIndex, kKeyAAnalyzerIndex, "Success",
                            /*expected_consecutive_failures=*/0);

  // Disconnect the network, advance fake time past Key A's expiration, and
  // verify unencrypted observations accumulate on disk across multiple batches
  // while upload attempts fail closed without dropping data.
  SetNetworkConnected(/*connected=*/false);
  AdvanceFakeClockPast(key_a_expiration_secs);

  const uint32_t uploads_before_offline_send = http_loader_.PostRequestCount();

  ASSERT_TRUE(logger_.LogOccurrence(cobalt_registry::kFeaturesActiveNewMetricId,
                                    {kFeaturesActiveNewSkillIndices[0]},
                                    kFeaturesActiveNewCounts[0]));
  ASSERT_TRUE(GenerateObsAndCheckCount(
      {{{cobalt_registry::kFeaturesActiveNewMetricId,
         cobalt_registry::kFeaturesActiveNewFeaturesActiveUniqueDevicesReportId},
        1}}));

  bool send_success = true;
  ASSERT_EQ(ZX_OK, cobalt_controller_->RequestSendSoon(&send_success));
  EXPECT_FALSE(send_success);
  EXPECT_EQ(uploads_before_offline_send, http_loader_.PostRequestCount());

  const OnDiskObservationCounts offline_batch1_counts = CountOnDiskObservations();
  EXPECT_GE(offline_batch1_counts.project_unencrypted_count, 1u);
  EXPECT_EQ(0u, offline_batch1_counts.total_encrypted_count);

  // Log and generate a second batch while still offline to verify accumulation.
  ASSERT_TRUE(logger_.LogString(cobalt_registry::kErrorOccurredComponentsMetricId,
                                {kErrorOccurredComponentsStatusIndices[0]},
                                kApplicationComponentNames[0]));
  ASSERT_TRUE(
      GenerateObsAndCheckCount({{{cobalt_registry::kErrorOccurredComponentsMetricId,
                                  cobalt_registry::kErrorOccurredComponentsErrorCountsReportId},
                                 1}}));

  send_success = true;
  ASSERT_EQ(ZX_OK, cobalt_controller_->RequestSendSoon(&send_success));
  EXPECT_FALSE(send_success);
  EXPECT_EQ(uploads_before_offline_send, http_loader_.PostRequestCount());

  const OnDiskObservationCounts offline_batch2_counts = CountOnDiskObservations();
  EXPECT_EQ(offline_batch1_counts.project_unencrypted_count,
            offline_batch2_counts.project_unencrypted_count);
  EXPECT_TRUE(offline_batch2_counts.has_in_progress_file);
  EXPECT_EQ(0u, offline_batch2_counts.total_encrypted_count);

  // Serve Key B, restore HTTP reachability via fuchsia.net.reachability.Monitor,
  // and verify KeyFetcher renews to Key B and uploads all accumulated
  // observations.
  const int64_t key_b_expiration_secs = CurrentFakeTimeSecs() + kStandardKeyLifetimeSecs;
  SetPublicKeysBytes(
      BuildPublicKeysBytes(kKeyBShufflerIndex, kKeyBAnalyzerIndex, key_b_expiration_secs));
  SetNetworkConnected(/*connected=*/true);

  WaitForKeyRotationInspect("valid", "valid", kKeyBShufflerIndex, kKeyBAnalyzerIndex, "Success",
                            /*expected_consecutive_failures=*/0);
  EXPECT_TRUE(VerifyCachedPublicKeysOnDisk(kKeyBShufflerIndex, kKeyBAnalyzerIndex));

  send_success = false;
  ASSERT_EQ(ZX_OK, cobalt_controller_->RequestSendSoon(&send_success));
  EXPECT_TRUE(send_success);

  const std::vector<uint32_t> key_b_uploaded_indices =
      http_loader_.TakeUploadedShufflerKeyIndices();
  ASSERT_GE(key_b_uploaded_indices.size(), 2u);
  EXPECT_TRUE(std::all_of(key_b_uploaded_indices.begin(), key_b_uploaded_indices.end(),
                          [](uint32_t idx) { return idx == kKeyBShufflerIndex; }));

  const OnDiskObservationCounts post_key_b_disk_counts = CountOnDiskObservations();
  EXPECT_EQ(0u, post_key_b_disk_counts.project_unencrypted_count);
  EXPECT_FALSE(post_key_b_disk_counts.has_in_progress_file);
}

}  // namespace
}  // namespace cobalt::testapp
