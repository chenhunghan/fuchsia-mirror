// Copyright 2021 The Fuchsia Authors.All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/main_service.h"

#include <fidl/fuchsia.hardware.power.statecontrol/cpp/fidl.h>
#include <fidl/fuchsia.process.lifecycle/cpp/fidl.h>
#include <fuchsia/feedback/cpp/fidl.h>
#include <lib/async-loop/default.h>
#include <lib/fidl/cpp/interface_request.h>
#include <lib/fit/defer.h>
#include <lib/fit/function.h>
#include <lib/syslog/cpp/macros.h>

#include <memory>
#include <optional>
#include <utility>

#include "src/developer/forensics/feedback/annotations/device_id_provider.h"
#include "src/developer/forensics/feedback/annotations/provider.h"
#include "src/developer/forensics/feedback/constants.h"
#include "src/developer/forensics/feedback/reboot_log/graceful_shutdown_info.h"
#include "src/developer/forensics/feedback/redactor_factory.h"
#include "src/developer/forensics/feedback/stop_signals.h"
#include "src/developer/forensics/utils/cobalt/logger.h"
#include "src/lib/backoff/exponential_backoff.h"

namespace forensics::feedback {
namespace {

std::unique_ptr<CachedAsyncAnnotationProvider> MakeDeviceIdProvider(
    const std::optional<std::string>& local_device_id_path, async_dispatcher_t* dispatcher,
    const std::shared_ptr<sys::ServiceDirectory>& services) {
  if (local_device_id_path.has_value()) {
    FX_LOGS(INFO) << "Using local device id provider";
    return std::make_unique<LocalDeviceIdProvider>(local_device_id_path.value());
  }

  FX_LOGS(INFO) << "Using remote device id provider";
  return std::make_unique<RemoteDeviceIdProvider>(dispatcher, services,
                                                  AnnotationProviders::AnnotationProviderBackoff());
}

std::set<std::string> RemoveExcludedAnnotations(const std::set<std::string>& default_annotations,
                                                const std::set<std::string>& excluded_annotations) {
  std::set<std::string> difference;
  std::set_difference(default_annotations.begin(), default_annotations.end(),
                      excluded_annotations.begin(), excluded_annotations.end(),
                      std::inserter(difference, difference.begin()));
  return difference;
}

}  // namespace

MainService::MainService(async_dispatcher_t* dispatcher,
                         const std::shared_ptr<sys::ServiceDirectory> services,
                         timekeeper::Clock* clock, inspect::Node* inspect_root,
                         cobalt::Logger* cobalt, const Annotations& startup_annotations,
                         fidl::ServerEnd<fuchsia_process_lifecycle::Lifecycle> lifecycle_channel,
                         std::unique_ptr<RedactorBase> redactor, Options options)
    : dispatcher_(dispatcher),
      executor_(dispatcher),
      services_(services),
      clock_(clock),
      inspect_root_(inspect_root),
      cobalt_(cobalt),
      redactor_(std::move(redactor)),
      inspect_node_manager_(inspect_root),
      annotations_(
          dispatcher_, services_,
          /*allowlist=*/
          RemoveExcludedAnnotations(
              options.feedback_data_options.snapshot_config.default_annotations,
              options.feedback_data_options.snapshot_exclusion_config.excluded_annotations),
          options.feedback_data_options.snapshot_exclusion_config.excluded_annotations,
          startup_annotations,
          MakeDeviceIdProvider(options.local_device_id_path, dispatcher_, services_)),
      network_watcher_(dispatcher, services,
                       std::make_unique<backoff::ExponentialBackoff>(
                           /*initial_delay=*/zx::min(1), /*retry_factor=*/2u,
                           /*max_delay=*/zx::hour(1))),
      feedback_data_(dispatcher_, services_, clock_, inspect_root_, cobalt_, redactor_.get(),
                     annotations_.GetAnnotationManager(), std::move(options.feedback_data_options)),
      crash_reports_(dispatcher_, services_, clock_, inspect_root_,
                     annotations_.GetAnnotationManager(), feedback_data_.DataProvider(),
                     options.crash_reports_options),
      last_reboot_(dispatcher_, cobalt_, redactor_.get(), crash_reports_.CrashReporter(),
                   std::move(options.last_reboot_options)),
      system_time_tracker_loop_(&kAsyncLoopConfigNoAttachToCurrentThread),
      system_time_tracker_(system_time_tracker_loop_.dispatcher(), clock_,
                           kSystemTimeTrackerWritePeriod,
                           std::move(options.current_system_time_write_path)),
      component_data_register_bindings_(dispatcher_, annotations_.ComponentDataRegister(),
                                        &inspect_node_manager_,
                                        "/fidl/fuchsia.feedback.ComponentDataRegister"),
      crash_reporter_bindings_(dispatcher_, crash_reports_.CrashReporter(), &inspect_node_manager_,
                               "/fidl/fuchsia.feedback.CrashReporter"),
      crash_register_bindings_(dispatcher_, crash_reports_.CrashRegister(), &inspect_node_manager_,
                               "/fidl/fuchsia.feedback.CrashReportingProductRegister"),
      data_provider_bindings_(dispatcher_, feedback_data_.DataProvider(), &inspect_node_manager_,
                              "/fidl/fuchsia.feedback.DataProvider"),
      last_reboot_info_bindings_(dispatcher_, last_reboot_.LastRebootInfoProvider(),
                                 &inspect_node_manager_,
                                 "/fidl/fuchsia.feedback.LastRebootInfoProvider") {
  executor_.schedule_task(
      WaitForLifecycleStop(dispatcher_, std::move(lifecycle_channel))
          .and_then([this](LifecycleStopSignal& signal) {
            FX_LOGS(INFO) << "Received stop signal; stopping upload, but not exiting "
                             "to continue persisting new reports and logs";

            crash_reports_.ShutdownImminent();
            feedback_data_.ShutdownImminent(
                fit::defer_callback([s = std::move(signal)]() mutable { s.Respond(); }));
          })
          .or_else([](const Error& error) {
            FX_LOGS(ERROR) << "Won't receive lifecycle signal: " << ToString(error);
          }));

  auto [watcher_client, watcher_server] =
      fidl::Endpoints<fuchsia_hardware_power_statecontrol::ShutdownWatcher>::Create();
  executor_.schedule_task(
      WaitForShutdownInfo(dispatcher_, std::move(watcher_server))
          .and_then([this, path = options.graceful_shutdown_info_write_path](
                        GracefulShutdownInfoSignal& signal) {
            const GracefulShutdownAction action = signal.Action();
            const std::vector<GracefulShutdownReason> reasons = signal.Reasons();
            FX_LOGS(INFO) << std::format("Received shutdown action '{}' with reasons '{}'",
                                         ToString(action), ToRawStrings(reasons));

            WriteGracefulShutdownInfo(action, reasons, path);
            system_time_tracker_.RecordSystemShutdownSignal();
            signal.Respond();
          })
          .or_else([](const Error& error) {
            FX_LOGS(ERROR) << "Won't receive shutdown options: " << ToString(error);
          }));

  // We register ourselves with ShutdownWatcher using a fire-and-forget request that gives
  // an endpoint to a long-lived connection we maintain.
  //
  // The ack response is ignored because the failures aren't expected unless the system is in a dire
  // state.
  auto [register_client, register_server] =
      fidl::Endpoints<fuchsia_hardware_power_statecontrol::ShutdownWatcherRegister>::Create();
  services->Connect(
      fidl::DiscoverableProtocolName<fuchsia_hardware_power_statecontrol::ShutdownWatcherRegister>,
      register_server.TakeChannel());
  const fidl::Client client(std::move(register_client), dispatcher_);
  client->RegisterWatcher({std::move(watcher_client)})
      .Then([](const fidl::Result<
                fuchsia_hardware_power_statecontrol::ShutdownWatcherRegister::RegisterWatcher>&
                   result) {});

  network_watcher_.Register(
      fit::bind_member(&crash_reports_, &CrashReports::SetNetworkIsReachable));

  // Perform periodic writes on a separate thread so that Feedback can still function if the
  // filesystem gets stuck.
  if (const zx_status_t status = system_time_tracker_loop_.StartThread("system-time-tracker");
      status != ZX_OK) {
    FX_PLOGS(FATAL, status) << "Failed to start writer thread for SystemTimeTracker";
  }

  if (options.enable_periodic_system_time_writes) {
    system_time_tracker_.Start();
  }
}

MainService::~MainService() { system_time_tracker_loop_.Shutdown(); }

template <>
::fidl::ProtocolHandler<fuchsia_feedback::LastRebootInfoProvider>
MainService::GetHandler<fuchsia_feedback::LastRebootInfoProvider>() {
  return [this](::fidl::ServerEnd<fuchsia_feedback::LastRebootInfoProvider> server_end) {
    last_reboot_info_bindings_.AddBinding(std::move(server_end));
  };
}

template <>
::fidl::ProtocolHandler<fuchsia_feedback::CrashReporter>
MainService::GetHandler<fuchsia_feedback::CrashReporter>() {
  return [this](::fidl::ServerEnd<fuchsia_feedback::CrashReporter> server_end) {
    crash_reporter_bindings_.AddBinding(std::move(server_end));
  };
}

template <>
::fidl::ProtocolHandler<fuchsia_feedback::CrashReportingProductRegister>
MainService::GetHandler<fuchsia_feedback::CrashReportingProductRegister>() {
  return [this](::fidl::ServerEnd<fuchsia_feedback::CrashReportingProductRegister> server_end) {
    crash_register_bindings_.AddBinding(std::move(server_end));
  };
}

template <>
::fidl::ProtocolHandler<fuchsia_feedback::ComponentDataRegister>
MainService::GetHandler<fuchsia_feedback::ComponentDataRegister>() {
  return [this](::fidl::ServerEnd<fuchsia_feedback::ComponentDataRegister> server_end) {
    component_data_register_bindings_.AddBinding(std::move(server_end));
  };
}

template <>
::fidl::ProtocolHandler<fuchsia_feedback::DataProvider>
MainService::GetHandler<fuchsia_feedback::DataProvider>() {
  return [this](::fidl::ServerEnd<fuchsia_feedback::DataProvider> server_end) {
    data_provider_bindings_.AddBinding(std::move(server_end));
  };
}

}  // namespace forensics::feedback
