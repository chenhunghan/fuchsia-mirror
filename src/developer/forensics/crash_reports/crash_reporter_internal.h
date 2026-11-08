// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_CRASH_REPORTS_CRASH_REPORTER_INTERNAL_H_
#define SRC_DEVELOPER_FORENSICS_CRASH_REPORTS_CRASH_REPORTER_INTERNAL_H_

#include <fidl/fuchsia.feedback/cpp/fidl.h>
#include <lib/fit/function.h>

namespace forensics::crash_reports {

// Internal-only class for filing crash reports from within the Feedback component, without going
// through FIDL.
class CrashReporterInternal {
 public:
  virtual ~CrashReporterInternal() = default;

  using FileReportInternalCallback =
      fit::callback<void(fidl::Response<fuchsia_feedback::CrashReporter::FileReport>)>;

  // Files |report| and calls |callback| with the result that
  // fuchsia.feedback.CrashReporter/FileReport would have returned.
  virtual void FileReportInternal(fuchsia_feedback::CrashReport report,
                                  FileReportInternalCallback callback) = 0;
};

}  // namespace forensics::crash_reports

#endif  // SRC_DEVELOPER_FORENSICS_CRASH_REPORTS_CRASH_REPORTER_INTERNAL_H_
