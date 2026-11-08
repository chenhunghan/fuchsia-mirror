// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/component/cpp/health_alert_latch.h>
#include <lib/driver/testing/cpp/driver_runtime.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/driver/testing/cpp/minimal_compat_environment.h>
#include <lib/inspect/cpp/reader.h>

#include <algorithm>
#include <string>
#include <thread>
#include <vector>

#include <gtest/gtest.h>

namespace {

using fuchsia_driver_framework::HealthAlert;
using fuchsia_driver_framework::HealthCheckRequest;
using fuchsia_driver_framework::HealthCheckResponse;
using fuchsia_driver_framework::HealthCondition;
using fuchsia_driver_framework::HealthDiagnosticItem;
using fuchsia_driver_framework::HealthDiagnosticValue;
using fuchsia_driver_framework::HealthStatus;

class LatchTestDriver : public fdf::DriverBase2 {
 public:
  using Server = fdf_internal::DriverServer2<LatchTestDriver>;

  LatchTestDriver() : fdf::DriverBase2("latch_test_driver") {}

  static DriverRegistration GetDriverRegistration() {
    return FUCHSIA_DRIVER_REGISTRATION_V1(Server::initialize, Server::destroy);
  }

  zx::result<> Start(fdf::DriverContext context) override {
    component_inspector_.emplace(context.CreateInspector(this, inspector_));
    return zx::ok();
  }

  // NOLINTNEXTLINE(readability-convert-member-functions-to-static)
  void HealthCheck(HealthCheckRequest request, HealthCheckCompleter completer) override {
    if (request == HealthCheckRequest::kPing) {
      completer(fit::ok(health_alerts_.ToResponse()));
      return;
    }
    completer(fit::error(fuchsia_driver_framework::HealthCheckError::kNotSupported));
  }

  fdf::HealthAlertLatch& latch() { return health_alerts_; }
  const inspect::Inspector& inspector() const { return inspector_; }

 private:
  inspect::Inspector inspector_;
  std::optional<inspect::ComponentInspector> component_inspector_;
  fdf::HealthAlertLatch health_alerts_{*this};
};

class AlertCollector : public fdf::WireAsyncEventHandler<fuchsia_driver_framework::Driver> {
 public:
  void OnHealthAlert(
      fidl::WireEvent<fuchsia_driver_framework::Driver::OnHealthAlert>* event) override {
    alerts.push_back(fidl::ToNatural(event->alert));
  }

  void handle_unknown_event(
      fidl::UnknownEventMetadata<fuchsia_driver_framework::Driver> metadata) override {}

  std::vector<HealthAlert> alerts;
};

struct RawLatchHarness {
  fdf_testing::DriverRuntime runtime;
  void* token = nullptr;
  LatchTestDriver* driver = nullptr;
  AlertCollector collector;
  fdf::WireClient<fuchsia_driver_framework::Driver> client;

  RawLatchHarness() {
    auto [client_end, server_end] = fdf::Endpoints<fuchsia_driver_framework::Driver>::Create();
    token = LatchTestDriver::Server::initialize(server_end.TakeChannel().release());
    auto* server = static_cast<LatchTestDriver::Server*>(token);
    driver = static_cast<LatchTestDriver*>(server->GetDriverBaseImpl());
    client.Bind(std::move(client_end), fdf::Dispatcher::GetCurrent()->get(), &collector);
  }

  ~RawLatchHarness() {
    client = {};
    runtime.RunUntilIdle();
    if (token != nullptr) {
      LatchTestDriver::Server::destroy(token);
    }
  }
};

TEST(HealthAlertLatchTest, DeduplicatesRepeatedRaisesAndTracksCounts) {
  RawLatchHarness h;
  EXPECT_EQ(HealthStatus::kOk, h.driver->latch().status());
  EXPECT_TRUE(h.driver->latch().Snapshot().empty());

  // Clearing an inactive reason code or empty latch emits nothing.
  h.driver->latch().Clear("NOT_ACTIVE");
  h.driver->latch().ClearAll();
  h.runtime.RunUntilIdle();
  EXPECT_TRUE(h.collector.alerts.empty());

  // First raise emits one SelfFault and populates timestamps + occurrence_count = 1.
  h.driver->latch().Raise(HealthCondition{{
      .status = HealthStatus::kDegraded,
      .reason_code = "BUS_RETRY",
      .message = "retry #1",
      .first_observed = zx::time_boot(100),
      .last_observed = zx::time_boot(100),
  }});
  h.runtime.RunUntilIdle();
  ASSERT_EQ(1u, h.collector.alerts.size());
  EXPECT_EQ(HealthStatus::kDegraded, h.driver->latch().status());

  const auto& first_alert = h.collector.alerts[0].self_fault();
  ASSERT_TRUE(first_alert.has_value());
  ASSERT_TRUE(first_alert->conditions().has_value());
  ASSERT_EQ(1u, first_alert->conditions()->size());
  EXPECT_EQ("BUS_RETRY", first_alert->conditions()->at(0).reason_code());
  EXPECT_EQ(HealthStatus::kDegraded, first_alert->conditions()->at(0).status());
  EXPECT_EQ(1u, first_alert->conditions()->at(0).occurrence_count());

  // Repeated raise at the same status updates last_observed, occurrence_count, and message in
  // memory without emitting another OnHealthAlert.
  h.driver->latch().Raise(HealthCondition{{
      .status = HealthStatus::kDegraded,
      .reason_code = "BUS_RETRY",
      .message = "retry #2",
      .first_observed = zx::time_boot(999),  // ignored: original first_observed is kept
      .last_observed = zx::time_boot(250),
  }});
  h.runtime.RunUntilIdle();
  EXPECT_EQ(1u, h.collector.alerts.size());

  auto snap = h.driver->latch().Snapshot();
  ASSERT_EQ(1u, snap.size());
  EXPECT_EQ(zx::time_boot(100), snap[0].first_observed());
  EXPECT_EQ(zx::time_boot(250), snap[0].last_observed());
  EXPECT_EQ(2u, snap[0].occurrence_count());
  EXPECT_EQ("retry #2", snap[0].message());
}

TEST(HealthAlertLatchTest, EscalationPartialRecoveryAndFullRecoveryTransitions) {
  RawLatchHarness h;
  EXPECT_FALSE(h.driver->latch().ToResponse().conditions().has_value());

  // 1. OK -> DEGRADED ("WARN_A")
  h.driver->latch().RaiseDegraded("WARN_A", "degraded A");
  // 2. Second DEGRADED condition ("WARN_B") emits updated active set of size 2.
  h.driver->latch().RaiseDegraded("WARN_B");
  ASSERT_TRUE(h.driver->latch().ToResponse().conditions().has_value());
  EXPECT_EQ(2u, h.driver->latch().ToResponse().conditions()->size());
  // 3. Escalating "WARN_A" DEGRADED -> UNHEALTHY emits alert.
  h.driver->latch().RaiseUnhealthy("WARN_A", "unhealthy A");
  // 4. Downgrading "WARN_A" UNHEALTHY -> DEGRADED emits partial-recovery alert.
  h.driver->latch().RaiseDegraded("WARN_A", "degraded A again");
  // 5. Clearing "WARN_A" while "WARN_B" remains emits reduced active set ("WARN_B").
  h.driver->latch().Clear("WARN_A");
  // 6. Clearing "WARN_B" emits empty conditions (OK recovery).
  h.driver->latch().Clear("WARN_B");

  h.runtime.RunUntilIdle();
  ASSERT_EQ(6u, h.collector.alerts.size());

  EXPECT_EQ(1u, h.collector.alerts[0].self_fault()->conditions()->size());
  EXPECT_EQ("degraded A", h.collector.alerts[0].self_fault()->conditions()->at(0).message());
  EXPECT_EQ(2u, h.collector.alerts[1].self_fault()->conditions()->size());
  EXPECT_FALSE(h.collector.alerts[1].self_fault()->conditions()->at(1).message().has_value());
  EXPECT_EQ(HealthStatus::kUnhealthy,
            h.collector.alerts[2].self_fault()->conditions()->at(0).status());
  EXPECT_EQ("unhealthy A", h.collector.alerts[2].self_fault()->conditions()->at(0).message());
  EXPECT_EQ(HealthStatus::kDegraded,
            h.collector.alerts[3].self_fault()->conditions()->at(0).status());
  ASSERT_EQ(1u, h.collector.alerts[4].self_fault()->conditions()->size());
  EXPECT_EQ("WARN_B", h.collector.alerts[4].self_fault()->conditions()->at(0).reason_code());
  EXPECT_FALSE(h.collector.alerts[5].self_fault()->conditions().has_value());
  EXPECT_FALSE(h.driver->latch().ToResponse().conditions().has_value());
  EXPECT_EQ(HealthStatus::kOk, h.driver->latch().status());
}

TEST(HealthAlertLatchTest, OmittedStatusDefaultsToUnhealthyAndOkClears) {
  RawLatchHarness h;

  // Omitted status normalizes to kUnhealthy.
  h.driver->latch().Raise(HealthCondition{{
      .reason_code = "WEDGED_RING",
  }});
  h.runtime.RunUntilIdle();
  ASSERT_EQ(1u, h.collector.alerts.size());
  EXPECT_EQ(HealthStatus::kUnhealthy, h.driver->latch().status());
  EXPECT_EQ(HealthStatus::kUnhealthy,
            h.collector.alerts[0].self_fault()->conditions()->at(0).status());

  // Passing HealthStatus::kOk clears the condition.
  h.driver->latch().Raise(HealthCondition{{
      .status = HealthStatus::kOk,
      .reason_code = "WEDGED_RING",
  }});
  h.runtime.RunUntilIdle();
  ASSERT_EQ(2u, h.collector.alerts.size());
  EXPECT_FALSE(h.collector.alerts[1].self_fault()->conditions().has_value());
  EXPECT_EQ(HealthStatus::kOk, h.driver->latch().status());

  // Omitted reason_code normalizes to "UNKNOWN", and Clear("") clears "UNKNOWN".
  h.driver->latch().Raise(HealthCondition{});
  h.runtime.RunUntilIdle();
  ASSERT_EQ(3u, h.collector.alerts.size());
  EXPECT_EQ("UNKNOWN", h.collector.alerts[2].self_fault()->conditions()->at(0).reason_code());

  h.driver->latch().Clear("");
  h.runtime.RunUntilIdle();
  ASSERT_EQ(4u, h.collector.alerts.size());
  EXPECT_FALSE(h.collector.alerts[3].self_fault()->conditions().has_value());
}

TEST(HealthAlertLatchTest, ClearAllEmitsSingleRecoveryAndResetsEpisodeCount) {
  RawLatchHarness h;

  h.driver->latch().Raise(HealthCondition{{
      .status = HealthStatus::kDegraded,
      .reason_code = "FAULT_1",
  }});
  h.driver->latch().Raise(HealthCondition{{
      .status = HealthStatus::kUnhealthy,
      .reason_code = "FAULT_2",
  }});
  h.driver->latch().Raise(HealthCondition{{
      .status = HealthStatus::kUnhealthy,
      .reason_code = "FAULT_2",
  }});
  ASSERT_EQ(2u, h.driver->latch().Snapshot()[1].occurrence_count());

  h.driver->latch().ClearAll();
  h.runtime.RunUntilIdle();
  ASSERT_EQ(3u, h.collector.alerts.size());
  EXPECT_FALSE(h.collector.alerts[2].self_fault()->conditions().has_value());

  // Re-raising after ClearAll starts a fresh episode with occurrence_count = 1.
  h.driver->latch().Raise(HealthCondition{{
      .status = HealthStatus::kUnhealthy,
      .reason_code = "FAULT_2",
  }});
  ASSERT_EQ(1u, h.driver->latch().Snapshot().size());
  EXPECT_EQ(1u, h.driver->latch().Snapshot()[0].occurrence_count());
}

TEST(HealthAlertLatchTest, MergeIntoDeduplicatesAndRespectsMaxConditions) {
  RawLatchHarness h;

  for (size_t i = 0; i < fdf::HealthAlertLatch::kMaxConditions; ++i) {
    h.driver->latch().Raise(HealthCondition{{
        .status = HealthStatus::kDegraded,
        .reason_code = "DEG_" + std::to_string(i),
    }});
  }
  EXPECT_EQ(fdf::HealthAlertLatch::kMaxConditions, h.driver->latch().Snapshot().size());

  // Raising a 17th DEGRADED condition is ignored when at capacity.
  h.driver->latch().Raise(HealthCondition{{
      .status = HealthStatus::kDegraded,
      .reason_code = "DEG_OVERFLOW",
  }});
  EXPECT_EQ(fdf::HealthAlertLatch::kMaxConditions, h.driver->latch().Snapshot().size());
  EXPECT_EQ(HealthStatus::kDegraded, h.driver->latch().status());

  // Raising an UNHEALTHY condition when full of DEGRADED conditions evicts one DEGRADED entry so
  // the critical fault is never dropped.
  h.driver->latch().Raise(HealthCondition{{
      .status = HealthStatus::kUnhealthy,
      .reason_code = "CRITICAL_FAULT",
  }});
  EXPECT_EQ(fdf::HealthAlertLatch::kMaxConditions, h.driver->latch().Snapshot().size());
  EXPECT_EQ(HealthStatus::kUnhealthy, h.driver->latch().status());

  // MergeInto keeps existing condition with matching reason_code and caps total at 16.
  HealthCheckResponse resp{{
      .conditions =
          std::vector<HealthCondition>{
              {{
                  .status = HealthStatus::kUnhealthy,
                  .reason_code = "CRITICAL_FAULT",
                  .message = "live FAST measurement",
              }},
              {{
                  .status = HealthStatus::kDegraded,
                  .reason_code = "LIVE_ONLY",
              }},
          },
  }};
  h.driver->latch().MergeInto(resp);
  ASSERT_TRUE(resp.conditions().has_value());
  EXPECT_EQ(fdf::HealthAlertLatch::kMaxConditions, resp.conditions()->size());
  EXPECT_EQ("live FAST measurement", resp.conditions()->at(0).message());

  // When the response already holds a live DEGRADED condition, the 15 latched DEGRADED entries
  // fill it before the latched UNHEALTHY entry (last in insertion order) is reached. MergeInto
  // must replace a DEGRADED entry with the UNHEALTHY one rather than drop it.
  HealthCheckResponse live_only_resp{{
      .conditions = std::vector<HealthCondition>{{{
          .status = HealthStatus::kDegraded,
          .reason_code = "LIVE_ONLY",
      }}},
  }};
  h.driver->latch().MergeInto(live_only_resp);
  ASSERT_TRUE(live_only_resp.conditions().has_value());
  EXPECT_EQ(fdf::HealthAlertLatch::kMaxConditions, live_only_resp.conditions()->size());
  EXPECT_TRUE(std::any_of(live_only_resp.conditions()->begin(), live_only_resp.conditions()->end(),
                          [](const HealthCondition& c) {
                            return c.reason_code() == "CRITICAL_FAULT" &&
                                   c.status() == HealthStatus::kUnhealthy;
                          }));
  EXPECT_TRUE(std::any_of(live_only_resp.conditions()->begin(), live_only_resp.conditions()->end(),
                          [](const HealthCondition& c) { return c.reason_code() == "LIVE_ONLY"; }));

  // Fill with kUnhealthy conditions.
  h.driver->latch().ClearAll();
  for (size_t i = 0; i < fdf::HealthAlertLatch::kMaxConditions; ++i) {
    h.driver->latch().Raise(HealthCondition{{
        .status = HealthStatus::kUnhealthy,
        .reason_code = "UNHEALTHY_" + std::to_string(i),
    }});
  }
  // Raising a 17th kUnhealthy condition when full of kUnhealthy is ignored safely.
  h.driver->latch().Raise(HealthCondition{{
      .status = HealthStatus::kUnhealthy,
      .reason_code = "UNHEALTHY_OVERFLOW",
  }});
  EXPECT_EQ(fdf::HealthAlertLatch::kMaxConditions, h.driver->latch().Snapshot().size());
}

struct InspectLatchFixtureConfig final {
  using DriverType = LatchTestDriver;
  using EnvironmentType = fdf_testing::MinimalCompatEnvironment;
};

TEST(HealthAlertLatchTest, ConcurrentCrossThreadUpdatesStayConsistentWithInspect) {
  fdf_testing::ForegroundDriverTest<InspectLatchFixtureConfig> driver_test;
  ASSERT_EQ(ZX_OK, driver_test.StartDriver().status_value());

  auto* driver = driver_test.driver();
  std::thread t1([driver] {
    for (int i = 0; i < 100; ++i) {
      driver->latch().Raise(HealthCondition{{
          .status = (i % 2 == 0) ? HealthStatus::kDegraded : HealthStatus::kUnhealthy,
          .reason_code = "THREAD_1_FAULT",
      }});
      if (i % 3 == 0) {
        driver->latch().Clear("THREAD_1_FAULT");
      }
    }
    driver->latch().Raise(HealthCondition{{
        .status = HealthStatus::kDegraded,
        .reason_code = "THREAD_1_FAULT",
        .message = "final t1",
    }});
  });

  std::thread t2([driver] {
    for (int i = 0; i < 100; ++i) {
      driver->latch().Raise(HealthCondition{{
          .status = HealthStatus::kUnhealthy,
          .reason_code = "THREAD_2_FAULT",
      }});
      driver->latch().Clear("THREAD_2_FAULT");
    }
  });

  t1.join();
  t2.join();
  driver_test.runtime().RunUntilIdle();

  auto snap = driver->latch().Snapshot();
  ASSERT_EQ(1u, snap.size());
  EXPECT_EQ("THREAD_1_FAULT", snap[0].reason_code());
  EXPECT_EQ(HealthStatus::kDegraded, driver->latch().status());

  // Verify the mirrored Inspect state matches the final active latch state.
  auto hierarchy = inspect::ReadFromVmo(driver->inspector().DuplicateVmo());
  ASSERT_TRUE(hierarchy.is_ok());
  const auto* hc_node = hierarchy.value().GetByPath({"driver_health_check"});
  ASSERT_NE(nullptr, hc_node);
  EXPECT_EQ("DEGRADED",
            hc_node->node().get_property<inspect::StringPropertyValue>("status")->value());
  EXPECT_NE(nullptr,
            hierarchy.value().GetByPath({"driver_health_check", "conditions", "THREAD_1_FAULT"}));
  EXPECT_EQ(nullptr,
            hierarchy.value().GetByPath({"driver_health_check", "conditions", "THREAD_2_FAULT"}));

  ASSERT_EQ(ZX_OK, driver_test.StopDriver().status_value());
}

TEST(HealthAlertLatchTest, ConcurrentCrossThreadUpdatesAlertSequence) {
  RawLatchHarness h;
  auto* driver = h.driver;

  std::thread t1([driver] {
    for (int i = 0; i < 100; ++i) {
      driver->latch().Raise(HealthCondition{{
          .status = (i % 2 == 0) ? HealthStatus::kDegraded : HealthStatus::kUnhealthy,
          .reason_code = "THREAD_1_FAULT",
      }});
      if (i % 3 == 0) {
        driver->latch().Clear("THREAD_1_FAULT");
      }
    }
    driver->latch().Raise(HealthCondition{{
        .status = HealthStatus::kDegraded,
        .reason_code = "THREAD_1_FAULT",
        .message = "final t1",
    }});
  });

  std::thread t2([driver] {
    for (int i = 0; i < 100; ++i) {
      driver->latch().Raise(HealthCondition{{
          .status = HealthStatus::kUnhealthy,
          .reason_code = "THREAD_2_FAULT",
      }});
      driver->latch().Clear("THREAD_2_FAULT");
    }
  });

  t1.join();
  t2.join();

  h.runtime.RunUntilIdle();

  auto snap = driver->latch().Snapshot();
  auto status = driver->latch().status();

  ASSERT_EQ(1u, snap.size());
  EXPECT_EQ("THREAD_1_FAULT", snap[0].reason_code());
  EXPECT_EQ(HealthStatus::kDegraded, status);

  ASSERT_GE(h.collector.alerts.size(), 200u);
  for (const auto& alert : h.collector.alerts) {
    if (!alert.self_fault().has_value() || !alert.self_fault()->conditions().has_value())
      continue;
    for (const auto& cond : *alert.self_fault()->conditions()) {
      if (cond.reason_code() == "THREAD_2_FAULT") {
        EXPECT_EQ(HealthStatus::kUnhealthy, cond.status());
      }
    }
  }
}
TEST(HealthAlertLatchTest, SafeNoOpAfterStop) {
  RawLatchHarness h;

  // Destroying a standalone latch drops pending tasks before execution.
  std::optional<fdf::HealthAlertLatch> temp_latch;
  temp_latch.emplace(*h.driver);
  temp_latch->Raise(HealthCondition{{.reason_code = "TEMP_FAULT"}});
  temp_latch.reset();
  h.runtime.RunUntilIdle();
  EXPECT_TRUE(h.collector.alerts.empty());

  fdf::Arena arena('STOP');
  ASSERT_TRUE(h.client.buffer(arena)->Stop().ok());
  h.runtime.RunUntilIdle();

  h.driver->latch().Raise(HealthCondition{{
      .status = HealthStatus::kUnhealthy,
      .reason_code = "AFTER_STOP",
  }});
  h.runtime.RunUntilIdle();
  EXPECT_TRUE(h.collector.alerts.empty());
}

}  // namespace
