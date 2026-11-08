// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>

#include <cstdlib>

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/ot_rcp_fuchsia_config.h"
#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/spinel_device_server.h"

int main() {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  async_dispatcher_t* dispatcher = loop.dispatcher();
  fuchsia_logging::LogSettingsBuilder().WithDispatcher(dispatcher).BuildAndInitialize();

  // Values of the config capabilities the manifest uses (`node_id`).
  auto config = ot_rcp_fuchsia_config::Config::TakeFromStartupHandle();

  ot_rcp::SpinelDeviceServer server(dispatcher, config.node_id());

  component::OutgoingDirectory outgoing(dispatcher);
  zx::result result = outgoing.AddService<fuchsia_lowpan_spinel::Service>(
      server.GetInstanceHandler(), component::OutgoingDirectory::kDefaultServiceInstance);
  if (result.is_error()) {
    FX_LOGS(ERROR) << "Failed to add fuchsia.lowpan.spinel.Service: " << result.status_string();
    return EXIT_FAILURE;
  }
  result = outgoing.ServeFromStartupInfo();
  if (result.is_error()) {
    FX_LOGS(ERROR) << "Failed to serve outgoing directory: " << result.status_string();
    return EXIT_FAILURE;
  }

  loop.Run();
  return EXIT_SUCCESS;
}
