// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/last_reboot.h"

namespace forensics::feedback {

LastReboot::LastReboot(async_dispatcher_t* dispatcher, cobalt::Logger* cobalt,
                       RedactorBase* redactor, crash_reports::CrashReporterInternal* crash_reporter,
                       const Options options)
    : reporter_(dispatcher, cobalt, redactor, crash_reporter),
      last_reboot_info_provider_(options.reboot_log.GetFinalShutdownInfo()) {
  if (options.is_first_instance) {
    const zx::duration delay = (options.reboot_log.GetFinalShutdownInfo().IsOom())
                                   ? options.oom_crash_reporting_delay
                                   : zx::sec(0);
    reporter_.ReportOn(options.reboot_log, delay, options.spontaneous_reboot_reason);
  }
}

fidl::Server<fuchsia_feedback::LastRebootInfoProvider>* LastReboot::LastRebootInfoProvider() {
  return &last_reboot_info_provider_;
}

}  // namespace forensics::feedback
