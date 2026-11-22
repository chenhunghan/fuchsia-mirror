// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.power.broker/cpp/fidl.h>
#include <fidl/fuchsia.power.system/cpp/fidl.h>
#include <lib/async/cpp/executor.h>
#include <lib/async/dispatcher.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/component/incoming/cpp/service.h>
#include <lib/fidl/cpp/channel.h>
#include <lib/fidl/cpp/client.h>
#include <lib/fpromise/promise.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/eventpair.h>

#include <memory>
#include <string>
#include <vector>

#include <gtest/gtest.h>

#include "src/developer/forensics/exceptions/constants.h"
#include "src/developer/forensics/exceptions/handler/wake_lease.h"
#include "src/lib/testing/loop_fixture/real_loop_fixture.h"

namespace forensics::exceptions {
namespace {

using ::fidl::ClientEnd;
using ::fidl::SyncClient;
using ::forensics::exceptions::handler::WakeLease;
using ::fuchsia_power_broker::ElementInfoProvider;
using ::fuchsia_power_broker::ElementInfoProviderGetStatusEndpointsResponse;
using ::fuchsia_power_broker::ElementInfoProviderService;
using ::fuchsia_power_broker::ElementStatusEndpoint;
using ::fuchsia_power_broker::LeaseDependency;
using ::fuchsia_power_broker::LeaseSchema;
using ::fuchsia_power_broker::Status;
using ::fuchsia_power_broker::StatusWatchPowerLevelResponse;
using ::fuchsia_power_broker::Topology;
using ::fuchsia_power_system::ActivityGovernor;
using ::fuchsia_power_system::ApplicationActivityLevel;
using ::fuchsia_power_system::BootControl;
using ::fuchsia_power_system::ExecutionStateLevel;
using ::fuchsia_power_system::LeaseToken;
using ::fuchsia_power_system::PowerElements;

constexpr char kApplicationActivity[] = "application_activity";
constexpr char kExecutionState[] = "execution_state";
constexpr char kBootCompleteIndicator[] = "boot-complete-indicator";

constexpr uint8_t ToUint(const ApplicationActivityLevel value) {
  return static_cast<uint8_t>(value);
}
constexpr uint8_t ToUint(const ExecutionStateLevel value) { return static_cast<uint8_t>(value); }

// Evaluates |rexpr|. On error, this function check-fails and prints the error. On success, sets the
// result of |rexpr| to |lhs|. This function exists to check invariants.
#define ASSIGN_OR_CHECK(lhs, rexpr) \
  ASSIGN_OR_CHECK_IMPL(RESULT_MACROS_CONCAT_NAME(result, __LINE__), lhs, rexpr)

// Same as |ASSIGN_OR_CHECK|, but doesn't attempt to set the result value of |expr| to a variable.
// Useful for checking a FIDL call that doesn't return a value but can still return an error.
#define CHECK_RESULT(expr) CHECK_RESULT_IMPL(RESULT_MACROS_CONCAT_NAME(result, __LINE__), expr)

#define ASSIGN_OR_CHECK_IMPL(result, lhs, rexpr) \
  CHECK_RESULT_IMPL(result, rexpr);              \
  lhs = std::move(result).value()

#define CHECK_RESULT_IMPL(result, rexpr) \
  auto(result) = (rexpr);                \
  FX_CHECK((result).is_ok()) << "Error: " << (result).error_value()

#define RESULT_MACROS_CONCAT_NAME(x, y) RESULT_MACROS_CONCAT_IMPL(x, y)
#define RESULT_MACROS_CONCAT_IMPL(x, y) x##y

template <typename Protocol>
SyncClient<Protocol> Connect() {
  zx::result client_end = component::Connect<Protocol>();
  FX_CHECK(client_end.is_ok());
  return SyncClient(std::move(client_end).value());
}

bool SetBootComplete() {
  zx::result boot_control_client = component::Connect<BootControl>();
  if (!boot_control_client.is_ok()) {
    FX_LOGS(ERROR) << "Synchronous error when connecting to the fuchsia.power.system/BootControl"
                   << " protocol: " << boot_control_client.status_string();
    return false;
  }
  auto status = fidl::WireCall(boot_control_client.value())->SetBootComplete();
  return status.ok();
}

// Returns nullptr if there's an error connecting to required protocols.
std::unique_ptr<WakeLease> CreateWakeLease(async_dispatcher_t* dispatcher) {
  zx::result sag_client_end = component::Connect<ActivityGovernor>();
  if (!sag_client_end.is_ok()) {
    FX_LOGS(ERROR)
        << "Synchronous error when connecting to the fuchsia.power.system/ActivityGovernor"
        << " protocol: " << sag_client_end.status_string();
    return nullptr;
  }

  return std::make_unique<WakeLease>(dispatcher, /*lease_name=*/"exceptions-element-001",
                                     std::move(sag_client_end).value());
}

// Acquires a direct lease on ApplicationActivity at kActive.
zx::eventpair AcquireApplicationActivityLease() {
  ASSIGN_OR_CHECK(PowerElements power_elements, Connect<ActivityGovernor>()->GetPowerElements());
  FX_CHECK(power_elements.application_activity().has_value());
  auto& application_activity = power_elements.application_activity().value();
  FX_CHECK(application_activity.assertive_dependency_token().has_value());

  zx::event aa_token = std::move(application_activity.assertive_dependency_token()).value();

  zx::eventpair lease_client;
  zx::eventpair lease_server;
  FX_CHECK(zx::eventpair::create(0, &lease_client, &lease_server) == ZX_OK);

  LeaseSchema schema;
  schema.lease_token(std::move(lease_server));
  schema.lease_name(kBootCompleteIndicator);
  schema.dependencies().emplace().push_back(LeaseDependency{{
      .requires_token = std::move(aa_token),
      .requires_level = ToUint(ApplicationActivityLevel::kActive),
  }});

  CHECK_RESULT(Connect<Topology>()->Lease(std::move(schema)));

  return lease_client;
}

std::vector<ElementStatusEndpoint> GetStatusEndpoints() {
  ASSIGN_OR_CHECK(ElementInfoProviderService::ServiceClient element_info_service,
                  component::OpenService<ElementInfoProviderService>("system_activity_governor"));

  ASSIGN_OR_CHECK(ClientEnd<ElementInfoProvider> element_info,
                  element_info_service.connect_status_provider());

  SyncClient<ElementInfoProvider> element_info_client(std::move(element_info));
  ASSIGN_OR_CHECK(ElementInfoProviderGetStatusEndpointsResponse status_endpoints,
                  element_info_client->GetStatusEndpoints());

  return std::move(status_endpoints.endpoints());
}

uint8_t GetCurrentLevel(const std::string& element) {
  std::vector<ElementStatusEndpoint> status_endpoints = GetStatusEndpoints();
  auto status = std::find_if(status_endpoints.begin(), status_endpoints.end(),
                             [&element](const ElementStatusEndpoint& endpoint) {
                               return endpoint.identifier() == element;
                             });
  FX_CHECK(status != status_endpoints.end());

  SyncClient<Status> status_client(std::move(*status->status()));
  ASSIGN_OR_CHECK(StatusWatchPowerLevelResponse result, status_client->WatchPowerLevel());

  return result.current_level();
}

// Uses |status_client| to retrieve the current power level. This function will block until the
// power level changes if the power level has not changed since the last time |status_client| was
// used to retrieve the power level.
uint8_t GetCurrentLevel(const SyncClient<Status>& status_client) {
  ASSIGN_OR_CHECK(StatusWatchPowerLevelResponse result, status_client->WatchPowerLevel());
  return result.current_level();
}

using WakeLeaseIntegrationTest = gtest::RealLoopFixture;

TEST_F(WakeLeaseIntegrationTest, AcquiresLease) {
  // Take an assertive dependency on ApplicationActivity to indicate boot complete, allowing SAG to
  // suspend if it deems appropriate. After we've acquired our wake lease using the WakeLease class,
  // we'll drop the lease on ApplicationActivity and check that ExecutionState was held at the
  // kSuspending level (the level that WakeLease has an assertive dependency on).
  zx::eventpair aa_lease = AcquireApplicationActivityLease();
  ASSERT_TRUE(SetBootComplete());
  ASSERT_EQ(GetCurrentLevel(kApplicationActivity), ToUint(ApplicationActivityLevel::kActive));

  std::vector<ElementStatusEndpoint> status_endpoints = GetStatusEndpoints();
  auto es_status_endpoint = std::find_if(status_endpoints.begin(), status_endpoints.end(),
                                         [](const ElementStatusEndpoint& endpoint) {
                                           return endpoint.identifier() == kExecutionState;
                                         });
  ASSERT_NE(es_status_endpoint, status_endpoints.end());

  SyncClient<Status> es_status_client(std::move(*es_status_endpoint->status()));

  // Subsequent calls to GetCurrentLevel using |es_status_client| will block until the level has
  // changed, so we'll reuse |es_status_client| when we want to wait until the power level has
  // changed.
  ASSERT_EQ(GetCurrentLevel(es_status_client), ToUint(ExecutionStateLevel::kActive));

  std::optional<LeaseToken> lease;
  std::unique_ptr<WakeLease> wake_lease = CreateWakeLease(dispatcher());

  fpromise::promise<void, Error> lease_promise =
      wake_lease->Acquire(kWakeLeaseAcquisitionTimeout)
          .or_else([](const Error& error) {
            FX_LOGS(FATAL) << "Wake lease not acquired: " << ToString(error);
            return fpromise::make_result_promise<LeaseToken, Error>(
                fpromise::error(Error::kBadValue));
          })
          .and_then(
              [&lease](LeaseToken& acquired_lease) mutable { lease = std::move(acquired_lease); });

  async::Executor executor(dispatcher());
  executor.schedule_task(std::move(lease_promise));
  RunLoopWithTimeoutOrUntil([&lease]() { return lease.has_value(); },
                            kWakeLeaseAcquisitionTimeout + zx::sec(1));

  // |aa_lease| is still valid, so ApplicationActivity should still be holding ExecutionState at
  // kActive.
  ASSERT_TRUE(lease.has_value());
  EXPECT_TRUE(lease->is_valid());
  ASSERT_EQ(GetCurrentLevel(kExecutionState), ToUint(ExecutionStateLevel::kActive));

  // Drop the ApplicationActivity lease while leaving |lease| intact.
  aa_lease.reset();

  RunLoopUntilIdle();
  ASSERT_EQ(GetCurrentLevel(es_status_client), ToUint(ExecutionStateLevel::kSuspending));

  // Drop |lease| so nothing is holding the system awake anymore.
  lease.reset();
  RunLoopUntilIdle();
  EXPECT_EQ(GetCurrentLevel(es_status_client), ToUint(ExecutionStateLevel::kInactive));
}

}  // namespace
}  // namespace forensics::exceptions
