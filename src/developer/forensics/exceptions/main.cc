// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#include <fidl/fuchsia.exception/cpp/fidl.h>
#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>

#include "src/developer/forensics/exceptions/constants.h"
#include "src/developer/forensics/exceptions/exception_broker.h"
#include "src/developer/forensics/exceptions_config.h"
#include "src/developer/forensics/utils/component/component.h"
#include "src/lib/fxl/strings/join_strings.h"

namespace forensics {
namespace exceptions {
namespace {

void LogProcessLimboStatus(const ::forensics::exceptions::ProcessLimboManager& limbo) {
  if (!limbo.active()) {
    return;
  }

  const std::string filters = fxl::JoinStrings(limbo.filters(), ", ");
  FX_LOGS(INFO) << "Process limbo is active at startup with the following filters: " << filters;
}

}  // namespace

int main() {
  forensics::component::Component component;
  fuchsia_logging::LogSettingsBuilder builder;
  builder.WithTags({"forensics", "exception"}).BuildAndInitialize();

  const exceptions_config::Config config = exceptions_config::Config::TakeFromStartupHandle();
  std::unique_ptr<ExceptionBroker> broker =
      ExceptionBroker::Create(component.Dispatcher(), component.InspectRoot(),
                              kMaxNumExceptionHandlers, kExceptionTtl, config.suspend_enabled());

  if (!broker)
    return EXIT_FAILURE;

  // Create the bindings for the protocols.
  fidl::ServerBindingGroup<fuchsia_exception::Handler> handler_bindings;
  component.AddPublicService(handler_bindings.CreateHandler(broker.get(), component.Dispatcher(),
                                                            fidl::kIgnoreBindingClosure));

  ProcessLimboManager& limbo_manager = broker->limbo_manager();

  // Everytime a new request comes for this service, we create a new handler. This permits us to
  // track per-connection state.
  component.AddPublicService<fuchsia_exception::ProcessLimbo>(
      [dispatcher = component.Dispatcher(),
       &limbo_manager](fidl::ServerEnd<fuchsia_exception::ProcessLimbo> server_end) {
        // Create a new handler exclusive to this connection.
        auto handler = std::make_unique<ProcessLimboHandler>(limbo_manager.GetWeakPtr());

        // Track this handler in the limbo manager, so it can be notified about events.
        limbo_manager.AddHandler(handler->GetWeakPtr());

        // Bind the handler to the channel. The binding owns the handler and destroys it when the
        // connection closes, which invalidates the weak pointer held by the limbo manager.
        fidl::BindServer(dispatcher, std::move(server_end), std::move(handler));
      });

  LogProcessLimboStatus(broker->limbo_manager());

  component.RunLoop();

  return EXIT_SUCCESS;
}

}  // namespace exceptions
}  // namespace forensics
