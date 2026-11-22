// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>

#include "thermal_test_control.h"

int main(int argc, const char** argv) {
  fuchsia_logging::LogSettingsBuilder builder;
  builder.WithTags({"thermal_test_control"}).BuildAndInitialize();

  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  ThermalTestControl app(loop.dispatcher());

  component::OutgoingDirectory outgoing(loop.dispatcher());
  if (zx::result<> result = outgoing.AddUnmanagedProtocol<fuchsia_thermal::ClientStateConnector>(
          app.connector_handler());
      result.is_error()) {
    FX_LOGS(ERROR) << "Failed to publish fuchsia.thermal.ClientStateConnector: "
                   << result.status_string();
    return -1;
  }
  if (zx::result<> result =
          outgoing.AddUnmanagedProtocol<test_thermal::ClientStateControl>(app.control_handler());
      result.is_error()) {
    FX_LOGS(ERROR) << "Failed to publish test.thermal.ClientStateControl: "
                   << result.status_string();
    return -1;
  }
  if (zx::result<> result = outgoing.ServeFromStartupInfo(); result.is_error()) {
    FX_LOGS(ERROR) << "Failed to serve outgoing directory: " << result.status_string();
    return -1;
  }

  FX_LOGS(INFO) << "Thermal test control is now running";

  return loop.Run();
}
