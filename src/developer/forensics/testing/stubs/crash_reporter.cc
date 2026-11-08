// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/testing/stubs/crash_reporter.h"

#include <fidl/fuchsia.feedback/cpp/fidl.h>
#include <lib/fit/result.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>

#include <optional>
#include <utility>

#include "src/lib/fsl/vmo/sized_vmo.h"
#include "src/lib/fsl/vmo/strings.h"
#include "src/lib/fxl/strings/string_printf.h"

namespace forensics {
namespace stubs {
namespace {

std::string ToString(const std::optional<zx::duration>& uptime) {
  if (!uptime.has_value()) {
    return "none";
  }

  return fxl::StringPrintf("%lu nanoseconds", uptime.value().to_nsecs());
}

std::string ToString(const std::optional<bool>& is_fatal) {
  if (!is_fatal.has_value()) {
    return "none";
  }

  return (is_fatal.value()) ? "true" : "false";
}

std::string ErrorMessage(const std::string& name, const std::string& received,
                         const std::string& expected) {
  return fxl::StringPrintf("Error with %s\nReceived: %s\n, Expected: %s", name.c_str(),
                           received.c_str(), expected.c_str());
}

}  // namespace

CrashReporter::~CrashReporter() {
  FX_CHECK(expectations_.crash_signature == crash_signature_)
      << ErrorMessage("crash signature", crash_signature_, expectations_.crash_signature);
  FX_CHECK(expectations_.reboot_log == reboot_log_)
      << ErrorMessage("reboot log", reboot_log_, expectations_.reboot_log);
  FX_CHECK(expectations_.uptime == uptime_)
      << ErrorMessage("uptime", ToString(uptime_), ToString(expectations_.uptime));
  FX_CHECK(expectations_.is_fatal == is_fatal_)
      << ErrorMessage("is fatal", ToString(is_fatal_), ToString(expectations_.is_fatal));
}

void CrashReporter::FileReportInternal(fuchsia_feedback::CrashReport report,
                                       FileReportInternalCallback callback) {
  FX_CHECK(report.crash_signature().has_value());
  FX_CHECK(report.attachments().has_value());
  FX_CHECK(report.attachments()->size() == 1u);

  crash_signature_ = *report.crash_signature();

  fuchsia_feedback::Attachment& attachment = report.attachments()->at(0);
  if (!fsl::StringFromVmo(
          fsl::SizedVmo(std::move(attachment.value().vmo()), attachment.value().size()),
          &reboot_log_)) {
    FX_LOGS(ERROR) << "error parsing feedback log VMO as string";
    callback(fit::error(fuchsia_feedback::FilingError::kInvalidArgsError));
    return;
  }

  if (report.program_uptime().has_value()) {
    uptime_ = zx::duration(*report.program_uptime());
  } else {
    uptime_ = std::nullopt;
  }

  if (report.is_fatal().has_value()) {
    is_fatal_ = *report.is_fatal();
  } else {
    is_fatal_ = std::nullopt;
  }

  fuchsia_feedback::FileReportResults results;
  results.result(fuchsia_feedback::FilingSuccess::kReportUploaded);
  results.report_id("01234567890abcdef");

  callback(fit::ok(std::move(results)));
}

void CrashReporterAlwaysReturnsError::FileReportInternal(fuchsia_feedback::CrashReport report,
                                                         FileReportInternalCallback callback) {
  callback(fit::error(fuchsia_feedback::FilingError::kInvalidArgsError));
}

void CrashReporterNoFileExpected::FileReportInternal(fuchsia_feedback::CrashReport report,
                                                     FileReportInternalCallback callback) {
  FX_CHECK(false) << "No call to FileReportInternal() expected";
}

}  // namespace stubs
}  // namespace forensics
