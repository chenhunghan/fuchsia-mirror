// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/crash_reports/filing_result.h"

#include <fidl/fuchsia.feedback/cpp/fidl.h>

#include <gtest/gtest.h>

namespace forensics::crash_reports {
namespace {

using fuchsia_feedback::FilingError;
using fuchsia_feedback::FilingSuccess;

TEST(FilingResult, ReportUploaded) {
  const auto fidl_result = ToFidlFilingResult(FilingResult::kReportUploaded);
  ASSERT_TRUE(fidl_result.is_ok());
  EXPECT_EQ(fidl_result.value(), FilingSuccess::kReportUploaded);
}

TEST(FilingResult, ReportOnDisk) {
  const auto fidl_result = ToFidlFilingResult(FilingResult::kReportOnDisk);
  ASSERT_TRUE(fidl_result.is_ok());
  EXPECT_EQ(fidl_result.value(), FilingSuccess::kReportOnDisk);
}

TEST(FilingResult, ReportInMemory) {
  const auto fidl_result = ToFidlFilingResult(FilingResult::kReportInMemory);
  ASSERT_TRUE(fidl_result.is_ok());
  EXPECT_EQ(fidl_result.value(), FilingSuccess::kReportInMemory);
}

TEST(FilingResult, ReportNotFiledUserOptedOut) {
  const auto fidl_result = ToFidlFilingResult(FilingResult::kReportNotFiledUserOptedOut);
  ASSERT_TRUE(fidl_result.is_ok());
  EXPECT_EQ(fidl_result.value(), FilingSuccess::kReportNotFiledUserOptedOut);
}

TEST(FilingResult, InvalidArgsError) {
  const auto fidl_result = ToFidlFilingResult(FilingResult::kInvalidArgsError);
  ASSERT_TRUE(fidl_result.is_error());
  EXPECT_EQ(fidl_result.error(), FilingError::kInvalidArgsError);
}

TEST(FilingResult, ServerError) {
  const auto fidl_result = ToFidlFilingResult(FilingResult::kServerError);
  ASSERT_TRUE(fidl_result.is_error());
  EXPECT_EQ(fidl_result.error(), FilingError::kServerError);
}

TEST(FilingResult, PersistenceError) {
  const auto fidl_result = ToFidlFilingResult(FilingResult::kPersistenceError);
  ASSERT_TRUE(fidl_result.is_error());
  EXPECT_EQ(fidl_result.error(), FilingError::kPersistenceError);
}

TEST(FilingResult, QuotaReachedError) {
  const auto fidl_result = ToFidlFilingResult(FilingResult::kQuotaReachedError);
  ASSERT_TRUE(fidl_result.is_error());
  EXPECT_EQ(fidl_result.error(), FilingError::kQuotaReachedError);
}

}  // namespace
}  // namespace forensics::crash_reports
