// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async/cpp/task.h>
#include <lib/driver/testing/cpp/internal/driver_lifecycle.h>
#include <lib/fdf/env.h>

namespace fdf_testing::internal {

namespace {

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
struct SuspendCompleter : public fdf_env_suspend_completer_t {
  explicit SuspendCompleter(fit::closure callback)
      : fdf_env_suspend_completer_t{.handler = &SuspendCompleter::CallHandler},
        callback(std::move(callback)) {}

  static void CallHandler(fdf_env_suspend_completer_t* completer) {
    auto* self = static_cast<SuspendCompleter*>(completer);
    auto cb = std::move(self->callback);
    delete self;
    cb();
  }

  fit::closure callback;
};
#endif

}  // namespace

DriverUnderTestBase::DriverUnderTestBase(DriverRegistration driver_registration_symbol)
    : driver_dispatcher_(fdf::Dispatcher::GetCurrent()->get()),
      driver_owner_(fdf_env_get_current_driver()),
      checker_(fdf_dispatcher_get_async_dispatcher(driver_dispatcher_)),
      driver_registration_symbol_(driver_registration_symbol) {
  auto [client_end, server_end] = fdf::Endpoints<fuchsia_driver_framework::Driver>::Create();
  void* token = driver_registration_symbol_.v1.initialize(server_end.TakeHandle().release());
#if FUCHSIA_API_LEVEL_AT_LEAST(32)
  fdf_dispatcher_t* client_dispatcher = fdf_dispatcher_get_always_on_dispatcher(driver_dispatcher_);
#else
  fdf_dispatcher_t* client_dispatcher = driver_dispatcher_;
#endif
  driver_client_.Bind(std::move(client_end), client_dispatcher, this);
  token_.emplace(token);
}

DriverUnderTestBase::~DriverUnderTestBase() {
  if (token_.has_value()) {
    driver_registration_symbol_.v1.destroy(token_.value());
  }
}

void DriverUnderTestBase::OnHealthAlert(
    fidl::WireEvent<fuchsia_driver_framework::Driver::OnHealthAlert>* event) {
  std::scoped_lock lock(health_alerts_mutex_);
  health_alerts_.push_back(fidl::ToNatural(event->alert));
}

void DriverUnderTestBase::on_fidl_error(fidl::UnbindInfo error) {
  std::lock_guard guard(checker_);
  if (stop_completer_.has_value()) {
    auto completer = std::move(stop_completer_.value());
    stop_completer_.reset();
    completer.complete_ok(zx::ok());
  }
}

void DriverUnderTestBase::handle_unknown_event(
    fidl::UnknownEventMetadata<fuchsia_driver_framework::Driver> metadata) {}

DriverRuntime::AsyncTask<zx::result<>> DriverUnderTestBase::Start(
    fuchsia_driver_framework::DriverStartArgs start_args) {
  std::lock_guard guard(checker_);
  fdf::Arena arena('STRT');
  fpromise::bridge<zx::result<>> bridge;
  driver_client_.buffer(arena)
      ->Start(fidl::ToWire(arena, std::move(start_args)))
      .Then([completer = bridge.completer.bind()](
                fdf::WireUnownedResult<fuchsia_driver_framework::Driver::Start>& result) mutable {
        if (!result.ok()) {
          completer(zx::make_result(result.error().status()));
          return;
        }

        if (result.value().is_error()) {
          completer(result->take_error());
          return;
        }

        completer(zx::ok());
      });

  return DriverRuntime::AsyncTask<zx::result<>>(bridge.consumer.promise());
}

DriverRuntime::AsyncTask<zx::result<>> DriverUnderTestBase::PrepareStop() {
  std::lock_guard guard(checker_);
#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
  // Ensure the driver's dispatchers are resumed before calling Stop in case a previous suspend or
  // resume transition failed and left them suspended.
  fdf_env_driver_resume(driver_owner_);
#endif
  fpromise::bridge<zx::result<>> bridge;
  stop_completer_.emplace(std::move(bridge.completer));
  fdf::Arena arena('STOP');
  fidl::OneWayStatus status = driver_client_.buffer(arena)->Stop();
  ZX_ASSERT_MSG(status.ok(), "Failed to send Stop request.");
  return DriverRuntime::AsyncTask<zx::result<>>(bridge.consumer.promise());
}

zx::result<> DriverUnderTestBase::Stop() {
  ZX_ASSERT(token_.has_value());
  driver_registration_symbol_.v1.destroy(token_.value());
  token_.reset();
  return zx::ok();
}

DriverRuntime::AsyncTask<zx::result<DriverUnderTestBase::HealthCheckResult>>
DriverUnderTestBase::CheckHealth(fuchsia_driver_framework::HealthCheckRequest request) {
  std::scoped_lock guard(checker_);
  fdf::Arena arena('HLTH');
  fpromise::bridge<zx::result<HealthCheckResult>> bridge;
  driver_client_.buffer(arena)->HealthCheck(request).Then(
      [completer = bridge.completer.bind()](
          fdf::WireUnownedResult<fuchsia_driver_framework::Driver::HealthCheck>& result) mutable {
        if (!result.ok()) {
          completer(zx::error(result.error().status()));
          return;
        }

        if (result->is_error()) {
          completer(zx::ok(fit::error(result->error_value())));
          return;
        }

        completer(zx::ok(fit::ok(fidl::ToNatural(*result->value()))));
      });

  return DriverRuntime::AsyncTask<zx::result<HealthCheckResult>>(bridge.consumer.promise());
}

std::vector<fuchsia_driver_framework::HealthAlert> DriverUnderTestBase::TakeHealthAlerts() {
  std::scoped_lock lock(health_alerts_mutex_);
  std::vector<fuchsia_driver_framework::HealthAlert> alerts = std::move(health_alerts_);
  health_alerts_.clear();
  return alerts;
}

size_t DriverUnderTestBase::PendingHealthAlertCount() const {
  std::scoped_lock lock(health_alerts_mutex_);
  return health_alerts_.size();
}

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
DriverRuntime::AsyncTask<zx::result<>> DriverUnderTestBase::Suspend() {
  std::lock_guard guard(checker_);
  fpromise::bridge<zx::result<>> bridge;
  fdf_dispatcher_t* always_on_dispatcher =
      fdf_dispatcher_get_always_on_dispatcher(driver_dispatcher_);
  auto* env_completer = new SuspendCompleter(
      [this, always_on_dispatcher, completer = bridge.completer.bind()]() mutable {
        async::PostTask(fdf_dispatcher_get_async_dispatcher(always_on_dispatcher),
                        [this, completer = std::move(completer)]() mutable {
                          std::lock_guard guard(checker_);
                          fdf::Arena arena('SUSP');
                          driver_client_.buffer(arena)->Suspend().Then(
                              [completer = std::move(completer)](
                                  fdf::WireUnownedResult<fuchsia_driver_framework::Driver::Suspend>&
                                      result) mutable {
                                if (!result.ok()) {
                                  completer(zx::make_result(result.error().status()));
                                  return;
                                }

                                if (result.value().is_error()) {
                                  completer(result->take_error());
                                  return;
                                }

                                completer(zx::ok());
                              });
                        });
      });
  fdf_env_driver_suspend(driver_owner_, env_completer);
  return DriverRuntime::AsyncTask<zx::result<>>(bridge.consumer.promise());
}

DriverRuntime::AsyncTask<zx::result<>> DriverUnderTestBase::Resume(
    std::optional<fuchsia_power_broker::LeaseToken> power_element_lease) {
  std::lock_guard guard(checker_);
  fdf::Arena arena('RESM');
  fpromise::bridge<zx::result<>> bridge;
  zx::eventpair lease =
      power_element_lease.has_value() ? std::move(*power_element_lease) : zx::eventpair{};
  driver_client_.buffer(arena)
      ->Resume(std::move(lease))
      .Then([this, completer = bridge.completer.bind()](
                fdf::WireUnownedResult<fuchsia_driver_framework::Driver::Resume>& result) mutable {
        if (!result.ok()) {
          completer(zx::make_result(result.error().status()));
          return;
        }

        if (result.value().is_error()) {
          completer(result->take_error());
          return;
        }

        fdf_env_driver_resume(driver_owner_);
        completer(zx::ok());
      });

  return DriverRuntime::AsyncTask<zx::result<>>(bridge.consumer.promise());
}
#endif

}  // namespace fdf_testing::internal
