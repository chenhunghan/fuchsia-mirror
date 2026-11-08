// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_TESTING_STUBS_CRASH_REPORTER_H_
#define SRC_DEVELOPER_FORENSICS_TESTING_STUBS_CRASH_REPORTER_H_

#include <fidl/fuchsia.feedback/cpp/fidl.h>
#include <lib/fit/function.h>
#include <lib/zx/time.h>

#include <optional>
#include <string>

#include "src/developer/forensics/crash_reports/crash_reporter_internal.h"

namespace forensics {
namespace stubs {

using CrashReporterBase = crash_reports::CrashReporterInternal;

class CrashReporter : public CrashReporterBase {
 public:
  struct Expectations {
    std::string crash_signature;
    std::string reboot_log;
    std::optional<zx::duration> uptime;
    std::optional<zx::duration> runtime;
    std::optional<bool> is_fatal;
  };

  CrashReporter(Expectations expectations) : expectations_(expectations) {}

  ~CrashReporter() override;

  // |crash_reports::CrashReporterInternal|
  void FileReportInternal(fuchsia_feedback::CrashReport report,
                          FileReportInternalCallback callback) override;

 private:
  const Expectations expectations_;

  std::string crash_signature_;
  std::string reboot_log_;
  std::optional<zx::duration> uptime_;
  std::optional<zx::duration> runtime_;
  std::optional<bool> is_fatal_;
};

class CrashReporterAlwaysReturnsError : public CrashReporterBase {
 public:
  // |crash_reports::CrashReporterInternal|
  void FileReportInternal(fuchsia_feedback::CrashReport report,
                          FileReportInternalCallback callback) override;
};

class CrashReporterNoFileExpected : public CrashReporterBase {
 public:
  // |crash_reports::CrashReporterInternal|
  void FileReportInternal(fuchsia_feedback::CrashReport report,
                          FileReportInternalCallback callback) override;
};

}  // namespace stubs
}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_TESTING_STUBS_CRASH_REPORTER_H_
