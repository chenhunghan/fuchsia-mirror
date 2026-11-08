// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/stop_signals.h"

#include <fidl/fuchsia.hardware.power.statecontrol/cpp/fidl.h>
#include <fidl/fuchsia.hardware.power.statecontrol/cpp/hlcpp_conversion.h>
#include <fidl/fuchsia.process.lifecycle/cpp/fidl.h>
#include <lib/fit/defer.h>
#include <lib/fit/function.h>
#include <lib/fpromise/bridge.h>
#include <lib/fpromise/promise.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/channel.h>

#include <memory>

#include "src/developer/forensics/utils/errors.h"

namespace forensics::feedback {
namespace {

// Handles receiving the signal the component should stop and converting into a
// `LifecycleStopSignal`.
class LifecycleServer : public fidl::Server<fuchsia_process_lifecycle::Lifecycle> {
 public:
  LifecycleServer(async_dispatcher_t* dispatcher,
                  fidl::ServerEnd<fuchsia_process_lifecycle::Lifecycle> server_end,
                  fpromise::completer<LifecycleStopSignal, Error> completer);

  void Stop(StopCompleter::Sync& completer) override;

 private:
  std::unique_ptr<fidl::ServerBinding<fuchsia_process_lifecycle::Lifecycle>> binding_;
  fpromise::completer<LifecycleStopSignal, Error> completer_;
};

LifecycleServer::LifecycleServer(async_dispatcher_t* dispatcher,
                                 fidl::ServerEnd<fuchsia_process_lifecycle::Lifecycle> server_end,
                                 fpromise::completer<LifecycleStopSignal, Error> completer)
    : binding_(std::make_unique<fidl::ServerBinding<fuchsia_process_lifecycle::Lifecycle>>(
          dispatcher, std::move(server_end), this,
          [this](const fidl::UnbindInfo info) {
            if (!completer_) {
              return;
            }

            FX_PLOGS(WARNING, info.status()) << "Lost connection to lifecycle client";
            completer_.complete_error(Error::kConnectionError);
          })),
      completer_(std::move(completer)) {}

void LifecycleServer::Stop(StopCompleter::Sync& completer) {
  if (completer_) {
    // Move `binding_` to avoid breaking references to internal member variables.
    auto cb = fit::defer([b = std::move(binding_)]() mutable { b.reset(); });
    completer_.complete_ok(LifecycleStopSignal([cb = std::move(cb)]() mutable { cb.call(); }));
  }
}

}  // namespace

LifecycleStopSignal::LifecycleStopSignal(fit::callback<void(void)> callback)
    : callback_(std::move(callback)) {
  FX_CHECK(callback_ != nullptr);
}

fpromise::promise<LifecycleStopSignal, Error> WaitForLifecycleStop(
    async_dispatcher_t* dispatcher,
    fidl::ServerEnd<fuchsia_process_lifecycle::Lifecycle> server_end) {
  if (!server_end.is_valid()) {
    return fpromise::make_result_promise<LifecycleStopSignal, Error>(
        fpromise::error(Error::kBadValue));
  }

  fpromise::bridge<LifecycleStopSignal, Error> bridge;
  auto lifecycle = std::make_unique<LifecycleServer>(dispatcher, std::move(server_end),
                                                     std::move(bridge.completer));
  return bridge.consumer.promise_or(fpromise::error(Error::kLogicError))
      .then([l = std::move(lifecycle)](fpromise::result<LifecycleStopSignal, Error>& result) {
        return std::move(result);
      });
}

namespace {

// Handles receiving the reason a shutdown is expected to occur and converting into a
// `GracefulShutdownInfoSignal`.
class ShutdownWatcherServer
    : public fidl::Server<fuchsia_hardware_power_statecontrol::ShutdownWatcher> {
 public:
  ShutdownWatcherServer(
      async_dispatcher_t* dispatcher,
      fidl::ServerEnd<fuchsia_hardware_power_statecontrol::ShutdownWatcher> server_end,
      fpromise::completer<GracefulShutdownInfoSignal, Error> completer);

  void OnShutdown(OnShutdownRequest& request, OnShutdownCompleter::Sync& completer) override;

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_power_statecontrol::ShutdownWatcher> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {
    FX_LOGS(WARNING) << "Received an unknown method with ordinal: " << metadata.method_ordinal;
  }

 private:
  std::unique_ptr<fidl::ServerBinding<fuchsia_hardware_power_statecontrol::ShutdownWatcher>>
      binding_;
  fpromise::completer<GracefulShutdownInfoSignal, Error> completer_;
};

ShutdownWatcherServer::ShutdownWatcherServer(
    async_dispatcher_t* dispatcher,
    fidl::ServerEnd<fuchsia_hardware_power_statecontrol::ShutdownWatcher> server_end,
    fpromise::completer<GracefulShutdownInfoSignal, Error> completer)
    : binding_(std::make_unique<
               fidl::ServerBinding<fuchsia_hardware_power_statecontrol::ShutdownWatcher>>(
          dispatcher, std::move(server_end), this,
          [this](const fidl::UnbindInfo info) {
            if (!completer_) {
              return;
            }

            FX_PLOGS(WARNING, info.status())
                << "Lost connection to shutdown watcher client, won't reconnect";
            completer_.complete_error(Error::kConnectionError);
          })),
      completer_(std::move(completer)) {}

void ShutdownWatcherServer::OnShutdown(OnShutdownRequest& request,
                                       OnShutdownCompleter::Sync& completer) {
  if (!completer_) {
    completer.Reply();
    return;
  }

  if (!request.options().action().has_value()) {
    FX_LOGS(WARNING) << "Received shutdown signal without an action";
    completer.Reply();
    return;
  }

  // Move `binding_` to avoid breaking references to internal member variables.
  auto cb = fit::defer([completer = completer.ToAsync(), b = std::move(binding_)]() mutable {
    completer.Reply();
    b.reset();
  });
  const fuchsia::hardware::power::statecontrol::ShutdownOptions hlcpp_options =
      fidl::NaturalToHLCPP(request.options());
  completer_.complete_ok(GracefulShutdownInfoSignal(ToGracefulShutdownAction(hlcpp_options),
                                                    ToGracefulShutdownReasons(hlcpp_options),
                                                    [cb = std::move(cb)]() mutable { cb.call(); }));
}

}  // namespace

GracefulShutdownInfoSignal::GracefulShutdownInfoSignal(GracefulShutdownAction action,
                                                       std::vector<GracefulShutdownReason> reasons,
                                                       fit::callback<void(void)> callback)
    : action_(action), reasons_(std::move(reasons)), callback_(std::move(callback)) {
  FX_CHECK(callback_ != nullptr);
}

fpromise::promise<GracefulShutdownInfoSignal, Error> WaitForShutdownInfo(
    async_dispatcher_t* dispatcher,
    fidl::ServerEnd<fuchsia_hardware_power_statecontrol::ShutdownWatcher> server_end) {
  if (!server_end.is_valid()) {
    return fpromise::make_result_promise<GracefulShutdownInfoSignal, Error>(
        fpromise::error(Error::kBadValue));
  }

  fpromise::bridge<GracefulShutdownInfoSignal, Error> bridge;
  auto watcher = std::make_unique<ShutdownWatcherServer>(dispatcher, std::move(server_end),
                                                         std::move(bridge.completer));
  return bridge.consumer.promise_or(fpromise::error(Error::kLogicError))
      .then([w = std::move(watcher)](fpromise::result<GracefulShutdownInfoSignal, Error>& result) {
        return std::move(result);
      });
}

}  // namespace forensics::feedback
