// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/crash_reports/filing_result.h"

namespace forensics::crash_reports {

using fuchsia_feedback::FilingError;
using fuchsia_feedback::FilingSuccess;

fpromise::result<FilingSuccess, FilingError> ToFidlFilingResult(const FilingResult result) {
  switch (result) {
    case FilingResult::kReportUploaded:
      return fpromise::ok(FilingSuccess::kReportUploaded);
    case FilingResult::kReportOnDisk:
      return fpromise::ok(FilingSuccess::kReportOnDisk);
    case FilingResult::kReportInMemory:
      return fpromise::ok(FilingSuccess::kReportInMemory);
    case FilingResult::kReportNotFiledUserOptedOut:
      return fpromise::ok(FilingSuccess::kReportNotFiledUserOptedOut);
    case FilingResult::kInvalidArgsError:
      return fpromise::error(FilingError::kInvalidArgsError);
    case FilingResult::kServerError:
      return fpromise::error(FilingError::kServerError);
    case FilingResult::kPersistenceError:
      return fpromise::error(FilingError::kPersistenceError);
    case FilingResult::kQuotaReachedError:
      return fpromise::error(FilingError::kQuotaReachedError);
  }
}

}  // namespace forensics::crash_reports
