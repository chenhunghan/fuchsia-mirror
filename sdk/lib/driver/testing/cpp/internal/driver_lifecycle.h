// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_TESTING_CPP_INTERNAL_DRIVER_LIFECYCLE_H_
#define LIB_DRIVER_TESTING_CPP_INTERNAL_DRIVER_LIFECYCLE_H_

#include <fidl/fuchsia.driver.framework/cpp/driver/wire.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/internal/driver_server2.h>
#include <lib/driver/symbols/symbols.h>
#include <lib/driver/testing/cpp/driver_runtime.h>

#include <mutex>
#include <vector>

// This is the exported driver registration symbol that the driver framework looks for.
// NOLINTNEXTLINE(bugprone-reserved-identifier)
extern "C" const DriverRegistration __fuchsia_driver_registration__;

namespace fdf_testing::internal {

using OpaqueDriverPtr = void*;

// The |DriverUnderTest| is a templated class so we pull out the non-template specifics into this
// base class so the implementation does not have to live in the header.
class DriverUnderTestBase : public fdf::WireAsyncEventHandler<fuchsia_driver_framework::Driver> {
 public:
  using HealthCheckResult = fit::result<fuchsia_driver_framework::HealthCheckError,
                                        fuchsia_driver_framework::HealthCheckResponse>;

  explicit DriverUnderTestBase(DriverRegistration driver_registration_symbol);

  virtual ~DriverUnderTestBase();

  // fdf::WireAsyncEventHandler<fuchsia_driver_framework::Driver>
  void OnHealthAlert(
      fidl::WireEvent<fuchsia_driver_framework::Driver::OnHealthAlert>* event) override;

  // fdf::WireAsyncEventHandler<fuchsia_driver_framework::Driver>
  void on_fidl_error(fidl::UnbindInfo error) override;

  // fdf::WireAsyncEventHandler<fuchsia_driver_framework::Driver>
  void handle_unknown_event(
      fidl::UnknownEventMetadata<fuchsia_driver_framework::Driver> metadata) override;

  // Start the driver. This is an asynchronous operation.
  // Use |DriverRuntime::RunToCompletion| to await the completion of the async task.
  // The resulting zx::result is the result of the start operation.
  DriverRuntime::AsyncTask<zx::result<>> Start(
      fuchsia_driver_framework::DriverStartArgs start_args);

  // PrepareStop the driver. This is an asynchronous operation.
  // Use |DriverRuntime::RunToCompletion| to await the completion of the async task.
  // The resulting zx::result is the result of the prepare stop operation.
  DriverRuntime::AsyncTask<zx::result<>> PrepareStop();

  // Stop the driver. The PrepareStop operation must have been completed before Stop is called.
  // Returns the result of the stop operation.
  zx::result<> Stop();

  // Sends a `HealthCheck` request over the `Driver` channel to the driver under test.
  // Use `DriverRuntime::RunToCompletion` to await completion of the async task.
  DriverRuntime::AsyncTask<zx::result<HealthCheckResult>> CheckHealth(
      fuchsia_driver_framework::HealthCheckRequest request =
          fuchsia_driver_framework::HealthCheckRequest::kPing);

  // Drains and returns all `OnHealthAlert` events received from the driver under test in FIFO
  // order.
  std::vector<fuchsia_driver_framework::HealthAlert> TakeHealthAlerts();

  // Returns the number of `OnHealthAlert` events currently queued without draining them.
  size_t PendingHealthAlertCount() const;

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
  // Suspend the driver. This is an asynchronous operation.
  // Use |DriverRuntime::RunToCompletion| to await the completion of the async task.
  // The resulting zx::result is the result of the suspend operation.
  DriverRuntime::AsyncTask<zx::result<>> Suspend();

  // Resume the driver. This is an asynchronous operation.
  // Use |DriverRuntime::RunToCompletion| to await the completion of the async task.
  // The resulting zx::result is the result of the resume operation.
  DriverRuntime::AsyncTask<zx::result<>> Resume(
      std::optional<fuchsia_power_broker::LeaseToken> power_element_lease = std::nullopt);
#endif

 protected:
  template <typename Driver>
#if __cplusplus >= 202002l
    requires std::is_same_v<decltype(&Driver::template GetInstanceFromTokenForTesting<Driver>),
                            Driver* (*)(void*)>
#endif
  Driver* GetDriver() {
    std::lock_guard guard(checker_);
    if (!token_.has_value()) {
      return nullptr;
    }

    return Driver::template GetInstanceFromTokenForTesting<Driver>(token_.value());
  }

 private:
  fdf_dispatcher_t* driver_dispatcher_;
  const void* driver_owner_;
  async::synchronization_checker checker_;
  DriverRegistration driver_registration_symbol_;
  std::optional<void*> token_;
  fdf::WireClient<fuchsia_driver_framework::Driver> driver_client_ __TA_GUARDED(checker_);
  std::optional<fpromise::completer<zx::result<>>> stop_completer_ __TA_GUARDED(checker_);
  mutable std::mutex health_alerts_mutex_;
  std::vector<fuchsia_driver_framework::HealthAlert> health_alerts_
      __TA_GUARDED(health_alerts_mutex_);
};

// This is a RAII wrapper over a driver under test. On construction it initializes the driver server
// and on destruction it destroys the driver server.
//
// The |Driver| type given in the template is used to provide pass-through `->` and `*` operators
// to the given driver type.
//
// To use this class, ensure that the driver has been exported into the
// __fuchsia_driver_registration__ symbol using the FUCHSIA_DRIVER macros. Otherwise pass the
// DriverRegistration manually into this class.
//
// # Thread safety
//
// This class is thread-unsafe. Instances must be managed and used from a synchronized dispatcher.
// See
// https://fuchsia.dev/fuchsia-src/development/languages/c-cpp/thread-safe-async#synchronized-dispatcher
//
// If the driver dispatcher is the foreground dispatcher, the DriverUnderTest does not need to be
// wrapped in a DispatcherBound.
//
// If the driver dispatcher is a background dispatcher, the suggestion is to
// wrap this inside of an |async_patterns::TestDispatcherBound|.
//
// The driver registration's initialize and destroy hooks are executed from the context of the
// dispatcher that this object lives on, therefore the driver's initial dispatcher will be this
// same dispatcher.
template <typename Driver = void>
class DriverUnderTest final : public DriverUnderTestBase {
 public:
  explicit DriverUnderTest(
      DriverRegistration driver_registration_symbol = __fuchsia_driver_registration__)
      : DriverUnderTestBase(driver_registration_symbol) {}

  Driver* operator->() { return static_cast<Driver*>(GetDriver<Driver>()); }
  Driver* operator*() { return static_cast<Driver*>(GetDriver<Driver>()); }
};

}  // namespace fdf_testing::internal

#endif  // LIB_DRIVER_TESTING_CPP_INTERNAL_DRIVER_LIFECYCLE_H_
