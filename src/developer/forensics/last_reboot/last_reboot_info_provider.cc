// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/last_reboot/last_reboot_info_provider.h"

#include <fidl/fuchsia.feedback/cpp/fidl.h>
#include <fidl/fuchsia.feedback/cpp/hlcpp_conversion.h>

#include "src/developer/forensics/feedback/reboot_log/final_shutdown_info.h"

namespace forensics {
namespace last_reboot {

LastRebootInfoProvider::LastRebootInfoProvider(
    const feedback::FinalShutdownInfo& final_shutdown_info) {
  if (final_shutdown_info.Uptime().has_value()) {
    last_reboot_.uptime(final_shutdown_info.Uptime()->get());
  }

  if (final_shutdown_info.Runtime().has_value()) {
    last_reboot_.runtime(final_shutdown_info.Runtime()->get());
  }

  if (const std::optional<bool> graceful = final_shutdown_info.OptionallyGraceful();
      graceful.has_value()) {
    last_reboot_.graceful(graceful.value());
  }

  if (const std::optional<bool> planned = final_shutdown_info.OptionallyPlanned();
      planned.has_value()) {
    last_reboot_.planned(planned.value());
  }

  if (std::optional<::fuchsia::feedback::RebootReason> fidl_reboot_reason =
          final_shutdown_info.ToFidlRebootReason();
      fidl_reboot_reason.has_value()) {
    last_reboot_.reason(fidl::HLCPPToNatural(fidl_reboot_reason.value()));
  }
}

void LastRebootInfoProvider::Get(GetCompleter::Sync& completer) { completer.Reply(last_reboot_); }

}  // namespace last_reboot
}  // namespace forensics
