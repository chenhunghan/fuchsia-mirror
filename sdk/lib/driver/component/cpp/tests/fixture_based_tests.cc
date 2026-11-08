// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.driver.component.test/cpp/driver/wire.h>
#include <fidl/fuchsia.driver.component.test/cpp/wire.h>
#include <lib/async/cpp/task.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/component/cpp/tests/test_driver.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/driver/testing/cpp/minimal_compat_environment.h>

#include <algorithm>
#include <mutex>
#include <string>

#include <gtest/gtest.h>

class ZirconProtocolServer
    : public fidl::WireServer<fuchsia_driver_component_test::ZirconProtocol> {
 public:
  fuchsia_driver_component_test::ZirconService::InstanceHandler GetInstanceHandler() {
    return fuchsia_driver_component_test::ZirconService::InstanceHandler({
        .device = bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                          fidl::kIgnoreBindingClosure),
    });
  }

 private:
  void ZirconMethod(ZirconMethodCompleter::Sync& completer) override { completer.ReplySuccess(); }
  fidl::ServerBindingGroup<fuchsia_driver_component_test::ZirconProtocol> bindings_;
};

class DriverProtocolServer : public fdf::WireServer<fuchsia_driver_component_test::DriverProtocol> {
 public:
  fuchsia_driver_component_test::DriverService::InstanceHandler GetInstanceHandler() {
    return fuchsia_driver_component_test::DriverService::InstanceHandler({
        .device = bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->get(),
                                          fidl::kIgnoreBindingClosure),
    });
  }

 private:
  void DriverMethod(fdf::Arena& arena, DriverMethodCompleter::Sync& completer) override {
    fdf::Arena reply_arena('TEST');
    completer.buffer(reply_arena).ReplySuccess();
  }

  fdf::ServerBindingGroup<fuchsia_driver_component_test::DriverProtocol> bindings_;
};

class FixtureBasedTestEnvironment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    auto result = to_driver_vfs.AddService<fuchsia_driver_component_test::ZirconService>(
        zircon_proto_server_.GetInstanceHandler());
    EXPECT_EQ(ZX_OK, result.status_value());

    result = to_driver_vfs.AddService<fuchsia_driver_component_test::DriverService>(
        driver_proto_server_.GetInstanceHandler());
    EXPECT_EQ(ZX_OK, result.status_value());

    device_server_.Initialize(component::kDefaultInstance);
    return zx::make_result(
        device_server_.Serve(fdf::Dispatcher::GetCurrent()->async_dispatcher(), &to_driver_vfs));
  }

  std::string GetName() { return std::string(device_server_.name()); }

 private:
  ZirconProtocolServer zircon_proto_server_;
  DriverProtocolServer driver_proto_server_;
  compat::DeviceServer device_server_;
};

class FixtureConfig final {
 public:
  using DriverType = TestDriver;
  using EnvironmentType = FixtureBasedTestEnvironment;
};

// Demonstrates a test fixture that puts the driver on a background context. Using the driver
// requires going through |RunInDriverContext()| but sync client tasks can be ran directly on the
// main test thread.
class FixtureBasedTestBackground : public ::testing::Test {
 public:
  void SetUp() override {
    zx::result<> result = driver_test().StartDriver();
    ASSERT_EQ(ZX_OK, result.status_value());
  }
  void TearDown() override {
    zx::result<> result = driver_test().StopDriver();
    ASSERT_EQ(ZX_OK, result.status_value());
  }

  fdf_testing::BackgroundDriverTest<FixtureConfig>& driver_test() { return driver_test_; }

 private:
  fdf_testing::BackgroundDriverTest<FixtureConfig> driver_test_;
};

TEST_F(FixtureBasedTestBackground, GetNameFromEnv) {
  driver_test().RunInEnvironmentTypeContext([](FixtureBasedTestEnvironment& env) {
    env.GetName();
    ASSERT_EQ(component::kDefaultInstance, env.GetName());
  });
  auto name = driver_test().RunInEnvironmentTypeContext<std::string>(
      [](FixtureBasedTestEnvironment& env) { return env.GetName(); });
  ASSERT_EQ(component::kDefaultInstance, name);
}

TEST_F(FixtureBasedTestBackground, ValidateDriverIncomingServices) {
  driver_test().RunInDriverContext([](TestDriver& driver) {
    zx::result result = driver.ValidateIncomingDriverService();
    ASSERT_EQ(ZX_OK, result.status_value());
    result = driver.ValidateIncomingZirconService();
    ASSERT_EQ(ZX_OK, result.status_value());
  });
}

TEST_F(FixtureBasedTestBackground, ConnectWithDevfs) {
  driver_test().RunInDriverContext([](TestDriver& driver) {
    zx::result export_result = driver.ExportDevfsNodeSync();
    ASSERT_EQ(ZX_OK, export_result.status_value());
  });

  zx::result device_result =
      driver_test().ConnectThroughDevfs<fuchsia_driver_component_test::ZirconProtocol>(
          "devfs_node");
  ASSERT_EQ(ZX_OK, device_result.status_value());

  fidl::WireSyncClient zircon_proto_client(std::move(device_result.value()));
  fidl::WireResult result = zircon_proto_client->ZirconMethod();
  ASSERT_EQ(ZX_OK, result.status());
  ASSERT_EQ(true, result.value().is_ok());
}

TEST_F(FixtureBasedTestBackground, ConnectWithZirconService) {
  driver_test().RunInDriverContext([](TestDriver& driver) {
    zx::result serve_result = driver.ServeZirconService();
    ASSERT_EQ(ZX_OK, serve_result.status_value());
  });

  zx::result result = driver_test().Connect<fuchsia_driver_component_test::ZirconService::Device>();
  ASSERT_EQ(ZX_OK, result.status_value());

  fidl::WireResult wire_result = fidl::WireCall(result.value())->ZirconMethod();
  ASSERT_EQ(ZX_OK, wire_result.status());
  ASSERT_EQ(true, wire_result.value().is_ok());
}

TEST_F(FixtureBasedTestBackground, ConnectWithDriverService) {
  driver_test().RunInDriverContext([](TestDriver& driver) {
    zx::result serve_result = driver.ServeDriverService();
    ASSERT_EQ(ZX_OK, serve_result.status_value());
  });

  zx::result driver_connect_result =
      driver_test().Connect<fuchsia_driver_component_test::DriverService::Device>();
  ASSERT_EQ(ZX_OK, driver_connect_result.status_value());

  fdf::Arena arena('TEST');
  fdf::WireUnownedResult wire_result =
      fdf::WireCall(driver_connect_result.value()).buffer(arena)->DriverMethod();
  ASSERT_EQ(ZX_OK, wire_result.status());
  ASSERT_EQ(true, wire_result.value().is_ok());
}

// Demonstrates a test fixture that puts the driver on the foreground context. Using the driver
// is simply done using the |driver()| getter but sync client tasks must be ran in the background
// using |RunOnBackgroundDispatcherSync()|.
class FixtureBasedTest : public ::testing::Test {
 public:
  void SetUp() override {
    zx::result<> result = driver_test().StartDriver();
    ASSERT_EQ(ZX_OK, result.status_value());
  }
  void TearDown() override {
    zx::result<> result = driver_test().StopDriver();
    ASSERT_EQ(ZX_OK, result.status_value());
  }
  fdf_testing::ForegroundDriverTest<FixtureConfig>& driver_test() { return driver_test_; }

 private:
  fdf_testing::ForegroundDriverTest<FixtureConfig> driver_test_;
};

TEST_F(FixtureBasedTest, GetNameFromEnv) {
  driver_test().RunInEnvironmentTypeContext([](FixtureBasedTestEnvironment& env) {
    env.GetName();
    ASSERT_EQ(component::kDefaultInstance, env.GetName());
  });
  auto name = driver_test().RunInEnvironmentTypeContext<std::string>(
      [](FixtureBasedTestEnvironment& env) { return env.GetName(); });
  ASSERT_EQ(component::kDefaultInstance, name);
}

TEST_F(FixtureBasedTest, ValidateDriverIncomingServices) {
  zx::result result = driver_test().driver()->ValidateIncomingDriverService();
  ASSERT_EQ(ZX_OK, result.status_value());
  result = driver_test().driver()->ValidateIncomingZirconService();
  ASSERT_EQ(ZX_OK, result.status_value());

  // Check the bad instance paths.
  result = driver_test().driver()->ValidateIncomingDriverService("bad_instance");
  ASSERT_EQ(ZX_ERR_PEER_CLOSED, result.status_value());
  result = driver_test().driver()->ValidateIncomingZirconService("bad_instance");
  ASSERT_EQ(ZX_ERR_PEER_CLOSED, result.status_value());

  // Restart it to enable service validation.
  result = driver_test().StopDriver();
  ASSERT_EQ(ZX_OK, result.status_value());
  driver_test().ShutdownAndDestroyDriver();
  driver_test().SetServiceValidator(true);
  result = driver_test().StartDriver();
  ASSERT_EQ(ZX_OK, result.status_value());

  // Check the valid instances again.
  result = driver_test().driver()->ValidateIncomingDriverService();
  ASSERT_EQ(ZX_OK, result.status_value());
  result = driver_test().driver()->ValidateIncomingZirconService();
  ASSERT_EQ(ZX_OK, result.status_value());

  // Check the bad instance.
  result = driver_test().driver()->ValidateIncomingDriverService("bad_instance");
  ASSERT_EQ(ZX_ERR_NOT_FOUND, result.status_value());
  result = driver_test().driver()->ValidateIncomingZirconService("bad_instance");
  ASSERT_EQ(ZX_ERR_NOT_FOUND, result.status_value());
}

TEST_F(FixtureBasedTest, ConnectWithDevfs) {
  zx::result export_result = driver_test().driver()->ExportDevfsNodeSync();
  ASSERT_EQ(ZX_OK, export_result.status_value());

  zx::result device_result =
      driver_test().ConnectThroughDevfs<fuchsia_driver_component_test::ZirconProtocol>(
          "devfs_node");
  ASSERT_EQ(ZX_OK, device_result.status_value());

  zx::result run_result = driver_test().RunOnBackgroundDispatcherSync(
      [client_end = std::move(device_result.value())]() mutable {
        fidl::WireSyncClient zircon_proto_client(std::move(client_end));
        fidl::WireResult result = zircon_proto_client->ZirconMethod();
        ASSERT_EQ(ZX_OK, result.status());
        ASSERT_EQ(true, result.value().is_ok());
      });
  ASSERT_EQ(ZX_OK, run_result.status_value());
}

TEST_F(FixtureBasedTest, ConnectWithZirconService) {
  zx::result serve_result = driver_test().driver()->ServeZirconService();
  ASSERT_EQ(ZX_OK, serve_result.status_value());

  zx::result result = driver_test().Connect<fuchsia_driver_component_test::ZirconService::Device>();
  ASSERT_EQ(ZX_OK, result.status_value());

  zx::result run_result = driver_test().RunOnBackgroundDispatcherSync(
      [client_end = std::move(result.value())]() mutable {
        fidl::WireResult wire_result = fidl::WireCall(client_end)->ZirconMethod();
        ASSERT_EQ(ZX_OK, wire_result.status());
        ASSERT_EQ(true, wire_result.value().is_ok());
      });
  ASSERT_EQ(ZX_OK, run_result.status_value());
}

TEST_F(FixtureBasedTest, ConnectWithDriverService) {
  zx::result serve_result = driver_test().driver()->ServeDriverService();
  ASSERT_EQ(ZX_OK, serve_result.status_value());

  zx::result driver_connect_result =
      driver_test().Connect<fuchsia_driver_component_test::DriverService::Device>();
  ASSERT_EQ(ZX_OK, driver_connect_result.status_value());

  zx::result run_result = driver_test().RunOnBackgroundDispatcherSync(
      [client_end = std::move(driver_connect_result.value())]() mutable {
        fdf::Arena arena('TEST');
        fdf::WireUnownedResult wire_result =
            fdf::WireCall(client_end).buffer(arena)->DriverMethod();
        ASSERT_EQ(ZX_OK, wire_result.status());
        ASSERT_EQ(true, wire_result.value().is_ok());
      });
  ASSERT_EQ(ZX_OK, run_result.status_value());
}

// Demonstrates a test fixture that tests out the manual stop and shutdown feature. Validates by
// checking the a global that gets set in the driver header.
class FixtureBasedTestManualStop : public ::testing::Test {
 public:
  void SetUp() override {
    zx::result<> result = driver_test().StartDriver();
    ASSERT_EQ(ZX_OK, result.status_value());
  }
  fdf_testing::ForegroundDriverTest<FixtureConfig>& driver_test() { return driver_test_; }

 private:
  fdf_testing::ForegroundDriverTest<FixtureConfig> driver_test_;
};

TEST_F(FixtureBasedTestManualStop, ShutdownAndCheckLogger) {
  ASSERT_EQ(false, g_driver_stopped);
  ASSERT_EQ(ZX_OK, driver_test().StopDriver().status_value());
  ASSERT_EQ(false, g_driver_stopped);
  driver_test().ShutdownAndDestroyDriver();
  ASSERT_EQ(true, g_driver_stopped);
}

class AddChildFixtureConfig final {
 public:
  using DriverType = TestDriver;
  using EnvironmentType = fdf_testing::MinimalCompatEnvironment;
};

// Checks that adding a child and then managing it by the driver works.
class FixtureBasedTestAddChild : public ::testing::Test {
 public:
  void SetUp() override {
    zx::result<> result = driver_test().StartDriver();
    ASSERT_EQ(ZX_OK, result.status_value());
  }
  void TearDown() override {
    zx::result<> result = driver_test().StopDriver();
    ASSERT_EQ(ZX_OK, result.status_value());
  }

  fdf_testing::ForegroundDriverTest<AddChildFixtureConfig>& driver_test() { return driver_test_; }

 private:
  fdf_testing::ForegroundDriverTest<AddChildFixtureConfig> driver_test_;
};

TEST_F(FixtureBasedTestAddChild, AddChild) {
  EXPECT_EQ(ZX_OK, driver_test().driver()->InitSyncCompat().status_value());
  driver_test().driver()->CreateChildNodeSync();
  EXPECT_TRUE(driver_test().driver()->sync_added_child());
}

// Fixture config for testing start failure scenarios.
class FailStartFixtureConfig final {
 public:
  using DriverType = StartFailTestDriver;
  using EnvironmentType = fdf_testing::MinimalCompatEnvironment;
};

class FixtureBasedTestFailStart : public ::testing::Test {
 public:
  fdf_testing::ForegroundDriverTest<FailStartFixtureConfig>& driver_test() { return driver_test_; }

 private:
  fdf_testing::ForegroundDriverTest<FailStartFixtureConfig> driver_test_;
};

TEST_F(FixtureBasedTestFailStart, FailStart) {
  auto start_result = driver_test().StartDriver();
  EXPECT_EQ(ZX_ERR_INTERNAL, start_result.status_value());
  driver_test().ShutdownAndDestroyDriver();
  start_result = driver_test().StartDriver();
  EXPECT_EQ(ZX_ERR_INTERNAL, start_result.status_value());
  // calling StopDriver is optional and a no-op if start failed. Returns zx::ok in this case.
  auto stop_result = driver_test().StopDriver();
  EXPECT_EQ(ZX_OK, stop_result.status_value());
  driver_test().ShutdownAndDestroyDriver();
  start_result = driver_test().StartDriver();
  EXPECT_EQ(ZX_ERR_INTERNAL, start_result.status_value());
  driver_test().ShutdownAndDestroyDriver();
}

// Tests that require manual start and stop calls. With the driver on the background.
class FixtureBasedTestManualBackground : public ::testing::Test {
 public:
  fdf_testing::BackgroundDriverTest<FixtureConfig>& driver_test() { return driver_test_; }

 private:
  fdf_testing::BackgroundDriverTest<FixtureConfig> driver_test_;
};

TEST_F(FixtureBasedTestManualBackground, MultiStart) {
  auto start_result = driver_test().StartDriver();
  EXPECT_EQ(ZX_OK, start_result.status_value());
  auto stop_result = driver_test().StopDriver();
  EXPECT_EQ(ZX_OK, stop_result.status_value());
  driver_test().ShutdownAndDestroyDriver();
  start_result = driver_test().StartDriver();
  EXPECT_EQ(ZX_OK, start_result.status_value());
  stop_result = driver_test().StopDriver();
  EXPECT_EQ(ZX_OK, stop_result.status_value());
  driver_test().ShutdownAndDestroyDriver();
  start_result = driver_test().StartDriver();
  EXPECT_EQ(ZX_OK, start_result.status_value());
  stop_result = driver_test().StopDriver();
  EXPECT_EQ(ZX_OK, stop_result.status_value());
  driver_test().ShutdownAndDestroyDriver();
}

// Tests that require manual start and stop calls. With the driver on the foreground.
class FixtureBasedTestManualForeground : public ::testing::Test {
 public:
  fdf_testing::ForegroundDriverTest<FixtureConfig>& driver_test() { return driver_test_; }

 private:
  fdf_testing::ForegroundDriverTest<FixtureConfig> driver_test_;
};

TEST_F(FixtureBasedTestManualForeground, MultiStartAndValidateIncoming) {
  auto start_result = driver_test().StartDriver();
  EXPECT_EQ(ZX_OK, start_result.status_value());
  zx::result validate_result = driver_test().driver()->ValidateIncomingDriverService();
  ASSERT_EQ(ZX_OK, validate_result.status_value());
  auto stop_result = driver_test().StopDriver();
  EXPECT_EQ(ZX_OK, stop_result.status_value());
  driver_test().ShutdownAndDestroyDriver();
  start_result = driver_test().StartDriver();
  EXPECT_EQ(ZX_OK, start_result.status_value());
  validate_result = driver_test().driver()->ValidateIncomingZirconService();
  ASSERT_EQ(ZX_OK, validate_result.status_value());
  stop_result = driver_test().StopDriver();
  EXPECT_EQ(ZX_OK, stop_result.status_value());
  driver_test().ShutdownAndDestroyDriver();
  start_result = driver_test().StartDriver();
  EXPECT_EQ(ZX_OK, start_result.status_value());
  validate_result = driver_test().driver()->ValidateIncomingDriverService();
  ASSERT_EQ(ZX_OK, validate_result.status_value());
  stop_result = driver_test().StopDriver();
  EXPECT_EQ(ZX_OK, stop_result.status_value());
  driver_test().ShutdownAndDestroyDriver();
}

namespace {

using fuchsia_driver_framework::HealthAlert;
using fuchsia_driver_framework::HealthCheckError;
using fuchsia_driver_framework::HealthCheckRequest;
using fuchsia_driver_framework::HealthCheckResponse;
using fuchsia_driver_framework::HealthCondition;
using fuchsia_driver_framework::HealthDiagnosticItem;
using fuchsia_driver_framework::HealthDiagnosticValue;
using fuchsia_driver_framework::HealthStatus;

class HealthFixtureTestDriver;

class TestHealthLatch final {
 public:
  explicit TestHealthLatch(HealthFixtureTestDriver& driver) : driver_(driver) {}

  void RaiseDegraded(std::string reason_code, std::string message) {
    Raise(HealthStatus::kDegraded, std::move(reason_code), std::move(message));
  }

  void RaiseUnhealthy(std::string reason_code, std::string message) {
    Raise(HealthStatus::kUnhealthy, std::move(reason_code), std::move(message));
  }

  void Clear(std::string_view reason_code) {
    HealthCheckResponse resp;
    {
      std::scoped_lock lock(mutex_);
      std::erase_if(conditions_, [&](const HealthCondition& c) {
        return c.reason_code().has_value() && *c.reason_code() == reason_code;
      });
      resp.conditions(conditions_);
    }
    PostSelfFault(std::move(resp));
  }

  HealthCheckResponse ToResponse() const {
    std::scoped_lock lock(mutex_);
    return HealthCheckResponse{{.conditions = conditions_}};
  }

  void MergeInto(HealthCheckResponse& resp) const {
    std::scoped_lock lock(mutex_);
    if (!resp.conditions().has_value()) {
      resp.conditions(conditions_);
      return;
    }
    resp.conditions()->insert(resp.conditions()->end(), conditions_.begin(), conditions_.end());
  }

  // Posts a SelfFault alert carrying |resp| to the driver dispatcher.
  void PostSelfFault(HealthCheckResponse resp);

 private:
  void Raise(HealthStatus status, std::string reason_code, std::string message) {
    HealthCheckResponse resp;
    {
      std::scoped_lock lock(mutex_);
      conditions_.push_back(HealthCondition{{
          .status = status,
          .reason_code = std::move(reason_code),
          .message = std::move(message),
      }});
      resp.conditions(conditions_);
    }
    PostSelfFault(std::move(resp));
  }

  HealthFixtureTestDriver& driver_;
  mutable std::mutex mutex_;
  std::vector<HealthCondition> conditions_;
};

class HealthFixtureTestDriver final : public fdf::DriverBase2 {
 public:
  using fdf::DriverBase2::dispatcher;
  using Server = fdf_internal::DriverServer2<HealthFixtureTestDriver>;

  HealthFixtureTestDriver() : fdf::DriverBase2("health_fixture_test_driver") {}

  static DriverRegistration GetDriverRegistration() {
    return FUCHSIA_DRIVER_REGISTRATION_V1(Server::initialize, Server::destroy);
  }

  zx::result<> Start(fdf::DriverContext context) override { return zx::ok(); }

  void HealthCheck(HealthCheckRequest request, HealthCheckCompleter completer) override {
    if (request == HealthCheckRequest::kPing) {
      completer(fit::ok(latch_.ToResponse()));
      return;
    }
    if (request == HealthCheckRequest::kFast) {
      HealthCheckResponse resp{{
          .diagnostics =
              std::vector<HealthDiagnosticItem>{
                  {{.key = "link_up", .value = HealthDiagnosticValue::WithBoolValue(true)}},
                  {{.key = "rx_packets", .value = HealthDiagnosticValue::WithUintValue(42u)}},
                  {{.key = "temp_c", .value = HealthDiagnosticValue::WithIntValue(-7)}},
                  {{.key = "mode", .value = HealthDiagnosticValue::WithStringValue("hs-g4")}},
                  {{.key = "last_irq",
                    .value = HealthDiagnosticValue::WithTimestamp(zx::time_boot(987654))}},
              },
          .conditions =
              std::vector<HealthCondition>{
                  {{
                      .status = HealthStatus::kDegraded,
                      .reason_code = "LIVE_FIFO_WARN",
                      .message = "FIFO 75% full",
                      .diagnostics =
                          std::vector<HealthDiagnosticItem>{
                              {{.key = "depth",
                                .value = HealthDiagnosticValue::WithUintValue(12u)}},
                          },
                  }},
              },
      }};
      latch_.MergeInto(resp);
      completer(fit::ok(std::move(resp)));
      return;
    }
    completer(fit::error(HealthCheckError::kNotSupported));
  }

  TestHealthLatch& latch() { return latch_; }

 private:
  TestHealthLatch latch_{*this};
};

void TestHealthLatch::PostSelfFault(HealthCheckResponse resp) {
  async::PostTask(driver_.dispatcher(), [driver = &driver_, resp = std::move(resp)] mutable {
    driver->ReportHealthAlert(HealthAlert::WithSelfFault(std::move(resp)));
  });
}

struct HealthFixtureConfig final {
  using DriverType = HealthFixtureTestDriver;
  using EnvironmentType = fdf_testing::MinimalCompatEnvironment;
};

TEST(DriverTestHealthHelpers, ForegroundCheckHealthAndTakeHealthAlerts) {
  fdf_testing::ForegroundDriverTest<HealthFixtureConfig> driver_test;
  ASSERT_EQ(ZX_OK, driver_test.StartDriver().status_value());
  EXPECT_EQ(0u, driver_test.PendingHealthAlertCount());
  EXPECT_TRUE(driver_test.TakeHealthAlerts().empty());

  // 1. Default PING is OK with no conditions.
  auto ping_res = driver_test.CheckHealth();
  ASSERT_EQ(ZX_OK, ping_res.status_value());
  ASSERT_TRUE(ping_res.value().is_ok());
  EXPECT_EQ(HealthStatus::kOk, fdf_testing::OverallHealthStatus(ping_res.value().value()));
  EXPECT_EQ(nullptr, fdf_testing::FindCondition(ping_res.value().value(), "LIVE_FIFO_WARN"));

  // 2. Raising a degraded fault emits a SelfFault alert and reflects in PING/FAST.
  driver_test.driver()->latch().RaiseDegraded("BUS_RETRY", "retrying transfer");
  EXPECT_EQ(1u, driver_test.PendingHealthAlertCount());
  auto alerts = driver_test.TakeHealthAlerts();
  ASSERT_EQ(1u, alerts.size());
  EXPECT_EQ(0u, driver_test.PendingHealthAlertCount());
  ASSERT_TRUE(alerts[0].self_fault().has_value());
  EXPECT_EQ(HealthStatus::kDegraded,
            fdf_testing::OverallHealthStatus(alerts[0].self_fault().value()));
  const auto* retry_cond = fdf_testing::FindCondition(alerts[0].self_fault().value(), "BUS_RETRY");
  ASSERT_NE(nullptr, retry_cond);
  EXPECT_EQ("retrying transfer", retry_cond->message());

  // 3. FAST returns both live and latched conditions + typed diagnostics.
  auto fast_res = driver_test.CheckHealth(HealthCheckRequest::kFast);
  ASSERT_EQ(ZX_OK, fast_res.status_value());
  ASSERT_TRUE(fast_res.value().is_ok());
  const auto& fast_resp = fast_res.value().value();
  EXPECT_EQ(HealthStatus::kDegraded, fdf_testing::OverallHealthStatus(fast_resp));
  EXPECT_EQ(true, fdf_testing::GetDiagnosticBool(fast_resp, "link_up"));
  EXPECT_EQ(42u, fdf_testing::GetDiagnosticUint(fast_resp, "rx_packets"));
  EXPECT_EQ(-7, fdf_testing::GetDiagnosticInt(fast_resp, "temp_c"));
  EXPECT_EQ("hs-g4", fdf_testing::GetDiagnosticString(fast_resp, "mode"));
  EXPECT_EQ(zx::time_boot(987654), fdf_testing::GetDiagnosticTimestamp(fast_resp, "last_irq"));
  EXPECT_EQ(std::nullopt, fdf_testing::GetDiagnosticBool(fast_resp, "rx_packets"));
  EXPECT_EQ(std::nullopt, fdf_testing::GetDiagnosticUint(fast_resp, "missing_key"));

  const auto* fifo_cond = fdf_testing::FindCondition(fast_resp, "LIVE_FIFO_WARN");
  ASSERT_NE(nullptr, fifo_cond);
  EXPECT_EQ(12u, fdf_testing::GetDiagnosticUint(*fifo_cond, "depth"));
  EXPECT_NE(nullptr, fdf_testing::FindCondition(fast_resp, "BUS_RETRY"));

  // 4. DEEP returns kNotSupported.
  auto deep_res = driver_test.CheckHealth(HealthCheckRequest::kDeep);
  ASSERT_EQ(ZX_OK, deep_res.status_value());
  ASSERT_TRUE(deep_res.value().is_error());
  EXPECT_EQ(HealthCheckError::kNotSupported, deep_res.value().error_value());

  ASSERT_EQ(ZX_OK, driver_test.StopDriver().status_value());
}

TEST(DriverTestHealthHelpers, BackgroundCheckHealthAndTakeHealthAlerts) {
  fdf_testing::BackgroundDriverTest<HealthFixtureConfig> driver_test;
  ASSERT_EQ(ZX_OK, driver_test.StartDriver().status_value());

  driver_test.RunInDriverContext([](HealthFixtureTestDriver& driver) {
    driver.latch().RaiseUnhealthy("DMA_HUNG", "ring 0 wedged");
    driver.ReportHealthAlert(HealthAlert::WithUpstreamNeighborFault(HealthCheckResponse{{
        .conditions = std::vector<HealthCondition>{{{
            .status = HealthStatus::kUnhealthy,
            .reason_code = "PARENT_CLOCK_OFF",
        }}},
    }}));
  });

  EXPECT_EQ(2u, driver_test.PendingHealthAlertCount());
  auto alerts = driver_test.TakeHealthAlerts();
  ASSERT_EQ(2u, alerts.size());
  EXPECT_EQ(0u, driver_test.PendingHealthAlertCount());

  // Direct ReportHealthAlert executes synchronously inside RunInDriverContext (first on the wire),
  // whereas TestHealthLatch::RaiseUnhealthy posts a task to the driver dispatcher (second).
  ASSERT_TRUE(alerts[0].upstream_neighbor_fault().has_value());
  ASSERT_NE(nullptr, fdf_testing::FindCondition(alerts[0].upstream_neighbor_fault().value(),
                                                "PARENT_CLOCK_OFF"));

  ASSERT_TRUE(alerts[1].self_fault().has_value());
  EXPECT_EQ(HealthStatus::kUnhealthy,
            fdf_testing::OverallHealthStatus(alerts[1].self_fault().value()));
  ASSERT_NE(nullptr, fdf_testing::FindCondition(alerts[1].self_fault().value(), "DMA_HUNG"));

  auto ping_res = driver_test.CheckHealth(HealthCheckRequest::kPing);
  ASSERT_EQ(ZX_OK, ping_res.status_value());
  ASSERT_TRUE(ping_res.value().is_ok());
  EXPECT_EQ(HealthStatus::kUnhealthy, fdf_testing::OverallHealthStatus(ping_res.value().value()));

  driver_test.RunInDriverContext(
      [](HealthFixtureTestDriver& driver) { driver.latch().Clear("DMA_HUNG"); });
  auto recovery_alerts = driver_test.TakeHealthAlerts();
  ASSERT_EQ(1u, recovery_alerts.size());
  ASSERT_TRUE(recovery_alerts[0].self_fault().has_value());
  EXPECT_EQ(HealthStatus::kOk,
            fdf_testing::OverallHealthStatus(recovery_alerts[0].self_fault().value()));

  ASSERT_EQ(ZX_OK, driver_test.StopDriver().status_value());
}

// Several alerts posted asynchronously to the driver dispatcher are all drained in one call: the
// driver transport delivers one event per channel-read callback, so the harness has to keep
// pumping the dispatcher until no more events arrive rather than barrier once.
TEST(DriverTestHealthHelpers, BackgroundTakeHealthAlertsDrainsManyQueuedAlerts) {
  fdf_testing::BackgroundDriverTest<HealthFixtureConfig> driver_test;
  ASSERT_EQ(ZX_OK, driver_test.StartDriver().status_value());

  constexpr size_t kAlertCount = 5;
  driver_test.RunInDriverContext([](HealthFixtureTestDriver& driver) {
    for (size_t i = 0; i < kAlertCount; ++i) {
      // Each PostSelfFault posts a task; none of the events are on the wire yet when this returns.
      driver.latch().PostSelfFault(HealthCheckResponse{{
          .conditions = std::vector<HealthCondition>{{{
              .status = HealthStatus::kDegraded,
              .reason_code = "BURST_" + std::to_string(i),
          }}},
      }});
    }
  });

  EXPECT_EQ(kAlertCount, driver_test.PendingHealthAlertCount());
  auto alerts = driver_test.TakeHealthAlerts();
  ASSERT_EQ(kAlertCount, alerts.size());
  for (size_t i = 0; i < kAlertCount; ++i) {
    ASSERT_TRUE(alerts[i].self_fault().has_value());
    EXPECT_NE(nullptr, fdf_testing::FindCondition(alerts[i].self_fault().value(),
                                                  "BURST_" + std::to_string(i)));
  }
  EXPECT_EQ(0u, driver_test.PendingHealthAlertCount());
  EXPECT_TRUE(driver_test.TakeHealthAlerts().empty());

  ASSERT_EQ(ZX_OK, driver_test.StopDriver().status_value());
}

}  // namespace
