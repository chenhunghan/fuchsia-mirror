// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>

#include <cstdlib>
#include <memory>
#include <optional>
#include <utility>

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/ot_rcp_fuchsia_config.h"
#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/rfsim_client.h"
#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/spinel_device_server.h"

int main() {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  async_dispatcher_t* dispatcher = loop.dispatcher();
  fuchsia_logging::LogSettingsBuilder().WithDispatcher(dispatcher).BuildAndInitialize();

  // Values of the config capabilities the manifest uses (`node_id`).
  auto config = ot_rcp_fuchsia_config::Config::TakeFromStartupHandle();

  // The RCP is served only once its node is attached, and owns the attached medium connection.
  std::optional<ot_rcp::SpinelDeviceServer> server;
  component::OutgoingDirectory outgoing(dispatcher);
  ot_rcp::RfsimClient::Attach(
      dispatcher, config.node_id(), [&](std::unique_ptr<ot_rcp::RfsimClient> rfsim) {
        server.emplace(dispatcher, config.node_id(), std::move(rfsim));
        zx::result result = outgoing.AddService<fuchsia_lowpan_spinel::Service>(
            server->GetInstanceHandler(), component::OutgoingDirectory::kDefaultServiceInstance);
        FX_CHECK(result.is_ok()) << "Failed to add fuchsia.lowpan.spinel.Service: "
                                 << result.status_string();
        result = outgoing.ServeFromStartupInfo();
        FX_CHECK(result.is_ok()) << "Failed to serve outgoing directory: "
                                 << result.status_string();
      });

  loop.Run();
  return EXIT_SUCCESS;
}
