// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback_data/log_source.h"

#include <fidl/fuchsia.logger/cpp/fidl.h>
#include <lib/fpromise/result.h>
#include <lib/syslog/cpp/log_level.h>
#include <lib/zx/time.h>

#include <format>
#include <memory>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/testing/log_message.h"
#include "src/developer/forensics/testing/stubs/diagnostics_archive.h"
#include "src/developer/forensics/testing/stubs/diagnostics_batch_iterator.h"
#include "src/developer/forensics/testing/unit_test_fixture.h"

namespace forensics::feedback_data {
namespace {

using ::testing::ElementsAreArray;

constexpr zx::duration kTimestamp = zx::sec(1234);

LogSink::MessageOr BuildOutputLogMessage(const std::string& text) {
  return ::fpromise::ok(BuildLogMessage(FUCHSIA_LOG_INFO, text, kTimestamp, {}));
}

std::string BuildInputLogMessage(const std::string& text) {
  const zx::time_boot timestamp = kLogMessageBaseTimestamp + kTimestamp;
  return std::format(R"JSON(
[
  {{
    "metadata": {{
      "timestamp": {},
      "severity": "INFO",
      "pid": {},
      "tid": {}
    }},
    "payload": {{
      "root": {{
      "message": {{
        "value": "{}"
        }}
      }}
    }}
  }}
]
)JSON",
                     timestamp.get(), kLogMessageProcessId, kLogMessageThreadId, text);
}

bool operator==(const std::vector<LogSink::MessageOr>& lhs,
                const std::vector<LogSink::MessageOr>& rhs) {
  if (lhs.size() != rhs.size()) {
    return false;
  }

  for (size_t i = 0; i < lhs.size(); ++i) {
    if (lhs[i].is_ok() != rhs[i].is_ok()) {
      return false;
    }

    if (lhs[i].is_error()) {
      if (lhs[i].error() != rhs[i].error()) {
        return false;
      }
      continue;
    }

    if (lhs[i].value() != rhs[i].value()) {
      return false;
    }
  }

  return true;
}

class SimpleLogSink : public LogSink {
 public:
  bool Add(LogSink::MessageOr message) override {
    messages_.push_back(std::move(message));
    return true;
  }

  const std::vector<LogSink::MessageOr>& Messages() const { return messages_; }

 private:
  std::vector<LogSink::MessageOr> messages_;
};

using LogSourceTest = UnitTestFixture;

TEST_F(LogSourceTest, WritesToSink) {
  const zx::duration kTimeWaitForLimitedLogs = zx::sec(60);
  const zx::duration kArchivePeriod = zx::msec(750);

  SimpleLogSink sink;
  LogSource source(dispatcher(), services(), &sink);

  const std::vector<std::vector<std::string>> batches({
      {"line 0", "line 1", "line 2", "line 3"},
      {"line 4", "line 5", "line 6", "line 7"},
      {"line 8"},
      {"line 9"},
      {"line A"},
      {"line B"},
      {"line C"},
      {"line D"},
      {},
  });

  std::vector<std::vector<std::string>> json_batches;
  for (const std::vector<std::string>& batch : batches) {
    std::vector<std::string>& json_batch = json_batches.emplace_back();
    for (const std::string& line : batch) {
      json_batch.push_back(BuildInputLogMessage(line));
    }
  }

  stubs::DiagnosticsArchive archive(
      dispatcher(), std::make_unique<stubs::DiagnosticsBatchIteratorDelayedBatches>(
                        dispatcher(), json_batches, kTimeWaitForLimitedLogs, kArchivePeriod));

  InjectServiceProvider(&archive, "fuchsia.diagnostics.ArchiveAccessor.logs");

  source.Start();
  RunLoopFor(kTimeWaitForLimitedLogs);

  std::vector<LogSink::MessageOr> expected;
  for (const std::vector<std::string>& batch : batches) {
    // Break to prevent |source| from fetching more messages.
    if (batch.empty()) {
      break;
    }

    for (const std::string& msg : batch) {
      expected.push_back(BuildOutputLogMessage(msg));
    }
    EXPECT_TRUE(sink.Messages() == expected);
    RunLoopFor(kArchivePeriod);
  }

  EXPECT_TRUE(archive.IsBound());
}

TEST_F(LogSourceTest, UnbindsOnIteratorClose) {
  SimpleLogSink sink;
  LogSource source(dispatcher(), services(), &sink);

  stubs::DiagnosticsArchiveClosesIteratorConnection archive;

  InjectServiceProvider(&archive, "fuchsia.diagnostics.ArchiveAccessor.logs");

  source.Start();
  RunLoopUntilIdle();

  EXPECT_FALSE(archive.IsBound());
}

}  // namespace
}  // namespace forensics::feedback_data
