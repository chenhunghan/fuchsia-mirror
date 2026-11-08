// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/exceptions/handler/main.h"

#include <fidl/fuchsia.exception.internal/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/sys/cpp/service_directory.h>
#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>
#include <zircon/errors.h>
#include <zircon/processargs.h>

#include <memory>
#include <string>

#include "src/developer/forensics/exceptions/constants.h"
#include "src/developer/forensics/exceptions/handler/crash_reporter.h"
#include "src/lib/fxl/strings/substitute.h"

namespace forensics {
namespace exceptions {
namespace handler {
namespace {

std::string ExtractHandlerIndex(const std::string& process_name) {
  // The process name should be of the form "exception_handler_001".
  size_t first_num = process_name.find_last_of('_');
  FX_CHECK(first_num != std::string::npos);
  FX_CHECK((++first_num) != std::string::npos);
  return process_name.substr(first_num);
}

// Returns nullptr if there's an error connecting to required protocols.
std::unique_ptr<WakeLease> CreateWakeLease(async_dispatcher_t* dispatcher,
                                           const std::string& handler_index) {
  zx::result sag_client_end = component::Connect<fuchsia_power_system::ActivityGovernor>();
  if (!sag_client_end.is_ok()) {
    FX_LOGS(ERROR)
        << "Synchronous error when connecting to the |fuchsia_power_system::ActivityGovernor| protocol: "
        << sag_client_end.status_string();
    return nullptr;
  }

  const std::string lease_name = fxl::Substitute("exceptions-element-$0", handler_index);
  return std::make_unique<WakeLease>(dispatcher, lease_name, std::move(sag_client_end).value());
}

}  // namespace

int main(const std::string& process_name, const std::string& suspend_enabled_flag) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  using forensics::exceptions::kComponentLookupTimeout;

  const std::string handler_index = ExtractHandlerIndex(process_name);

  fuchsia_logging::LogSettingsBuilder()
      .WithTags({"forensics", "exception", handler_index})
      .BuildAndInitialize();

  // We receive a channel that we interpret as a fuchsia.exception.internal.CrashReporter
  // connection.
  zx::channel channel(zx_take_startup_handle(PA_HND(PA_USER0, 0)));
  if (!channel.is_valid()) {
    FX_LOGS(FATAL) << "Received invalid channel";
    return EXIT_FAILURE;
  }

  fidl::ClientEnd<fuchsia_driver_crash::CrashIntrospect> driver_crash_introspect_client_end;
  {
    zx::result driver_crash_introspect_result =
        component::Connect<fuchsia_driver_crash::CrashIntrospect>();
    if (driver_crash_introspect_result.is_error()) {
      FX_LOGS(WARNING) << "Failed to connect to driver crash introspect channel.";
    } else {
      driver_crash_introspect_client_end = std::move(driver_crash_introspect_result).value();
    }
  }

  std::unique_ptr<WakeLease> wake_lease = suspend_enabled_flag == kSuspendEnabledFlag
                                              ? CreateWakeLease(loop.dispatcher(), handler_index)
                                              : nullptr;

  CrashReporter crash_reporter(loop.dispatcher(), sys::ServiceDirectory::CreateFromNamespace(),
                               kComponentLookupTimeout, std::move(wake_lease),
                               std::move(driver_crash_introspect_client_end));

  fidl::ServerBinding<fuchsia_exception_internal::CrashReporter> crash_reporter_binding(
      loop.dispatcher(),
      fidl::ServerEnd<fuchsia_exception_internal::CrashReporter>(std::move(channel)),
      &crash_reporter, [&loop](fidl::UnbindInfo info) {
        FX_LOGS(WARNING) << "Lost connection to client: " << info;
        loop.Shutdown();
      });

  loop.Run();

  return EXIT_SUCCESS;
}

}  // namespace handler
}  // namespace exceptions
}  // namespace forensics
