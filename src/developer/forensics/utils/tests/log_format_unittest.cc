// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/utils/log_format.h"

#include <fuchsia/logger/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>

#include <cstdint>
#include <limits>
#include <optional>
#include <string>
#include <vector>

#include <gtest/gtest.h>

#include "src/developer/forensics/testing/gpretty_printers.h"

namespace forensics {
namespace {

using ::testing::TestParamInfo;
using ::testing::TestWithParam;
using ::testing::ValuesIn;

constexpr zx::duration kLogMessageBaseTimestamp = zx::sec(15604);
constexpr uint64_t kLogMessageProcessId = 7559;
constexpr uint64_t kLogMessageThreadId = 7687;

fuchsia::logger::LogMessage BuildLogMessage(const int32_t severity, const std::string& text,
                                            const zx::duration timestamp_offset = zx::nsec(0),
                                            const std::vector<std::string>& tags = {}) {
  fuchsia::logger::LogMessage msg{};
  msg.time = zx::time_boot((kLogMessageBaseTimestamp + timestamp_offset).get());
  msg.pid = kLogMessageProcessId;
  msg.tid = kLogMessageThreadId;
  msg.tags = tags;
  msg.severity = severity;
  msg.msg = text;
  return msg;
}

TEST(LogFormatTest, Check_CorrectSeverity) {
  std::string log_message;

  log_message = Format(BuildLogMessage(FUCHSIA_LOG_INFO, "line 1"));
  EXPECT_EQ(log_message, "[15604.000][07559][07687][] INFO: line 1\n");

  log_message = Format(BuildLogMessage(FUCHSIA_LOG_WARNING, "line 2"));
  EXPECT_EQ(log_message, "[15604.000][07559][07687][] WARN: line 2\n");

  log_message = Format(BuildLogMessage(FUCHSIA_LOG_ERROR, "line 3"));
  EXPECT_EQ(log_message, "[15604.000][07559][07687][] ERROR: line 3\n");

  log_message = Format(BuildLogMessage(FUCHSIA_LOG_FATAL, "line 4"));
  EXPECT_EQ(log_message, "[15604.000][07559][07687][] FATAL: line 4\n");

  log_message = Format(BuildLogMessage(
      FUCHSIA_LOG_INFO + FUCHSIA_LOG_WARNING + FUCHSIA_LOG_ERROR + FUCHSIA_LOG_FATAL, "line 5"));
  EXPECT_EQ(log_message, "[15604.000][07559][07687][] INVALID: line 5\n");

  log_message = Format(BuildLogMessage(FUCHSIA_LOG_TRACE, "line 6"));
  EXPECT_EQ(log_message, "[15604.000][07559][07687][] TRACE: line 6\n");

  log_message = Format(BuildLogMessage(FUCHSIA_LOG_DEBUG, "line 7"));
  EXPECT_EQ(log_message, "[15604.000][07559][07687][] DEBUG: line 7\n");

  log_message = Format(BuildLogMessage(FUCHSIA_LOG_INFO - 1, "line 8"));
  EXPECT_EQ(log_message, "[15604.000][07559][07687][] VLOG(1): line 8\n");

  log_message = Format(BuildLogMessage(FUCHSIA_LOG_INFO - 12, "line 9"));
  EXPECT_EQ(log_message, "[15604.000][07559][07687][] VLOG(12): line 9\n");
}

TEST(LogFormatTest, Check_CorrectTime) {
  std::string log_message = Format(BuildLogMessage(FUCHSIA_LOG_WARNING, "line 1", zx::msec(1)));
  EXPECT_EQ(log_message, "[15604.001][07559][07687][] WARN: line 1\n");
}

TEST(LogFormatTest, Check_CorrectTags) {
  std::string log_message = Format(BuildLogMessage(FUCHSIA_LOG_INFO, "line 1", zx::msec(1),
                                                   /*tags=*/{"foo", "bar"}));
  EXPECT_EQ(log_message, "[15604.001][07559][07687][foo, bar] INFO: line 1\n");
}

TEST(LogFormatTest, Check_InsertsDroppedLogMessageSingular) {
  fuchsia::logger::LogMessage message = BuildLogMessage(FUCHSIA_LOG_INFO, "line 1", zx::msec(1),
                                                        /*tags=*/{"foo", "bar"});
  message.dropped_logs = 1;

  EXPECT_EQ(
      Format(message),
      "[15604.001][07559][07687][foo, bar] WARN: !!! DROPPED 1 LOG !!!\n[15604.001][07559][07687][foo, bar] INFO: line 1\n");
}

TEST(LogFormatTest, Check_InsertsDroppedLogMessageMultiple) {
  fuchsia::logger::LogMessage message = BuildLogMessage(FUCHSIA_LOG_INFO, "line 1", zx::msec(1),
                                                        /*tags=*/{"foo", "bar"});
  message.dropped_logs = 2;

  EXPECT_EQ(
      Format(message),
      "[15604.001][07559][07687][foo, bar] WARN: !!! DROPPED 2 LOGS !!!\n[15604.001][07559][07687][foo, bar] INFO: line 1\n");
}

struct ParseLogHeaderTestCase {
  std::string test_name;
  std::string input;
  std::optional<zx::time_boot> expected;
};

using ParseLogHeaderTest = TestWithParam<ParseLogHeaderTestCase>;

TEST_P(ParseLogHeaderTest, ParsesLogHeader) {
  const ParseLogHeaderTestCase& test_case = GetParam();
  EXPECT_EQ(ParseLogHeader(test_case.input), test_case.expected);
}

INSTANTIATE_TEST_SUITE_P(
    WithVariousInputs, ParseLogHeaderTest,
    ValuesIn(std::vector<ParseLogHeaderTestCase>({
        {
            .test_name = "HeaderOnly",
            .input = "[15604.000][07559][07687]",
            .expected = zx::time_boot(zx::sec(15604).get()),
        },
        {
            .test_name = "NonZeroMilliseconds",
            .input = "[15604.025][07559][07687]",
            .expected = zx::time_boot((zx::sec(15604) + zx::msec(25)).get()),
        },
        {
            .test_name = "LargeValues",
            .input = "[4294967295.999][18446744073709551615][18446744073709551614]",
            .expected = zx::time_boot(
                (zx::sec(std::numeric_limits<uint32_t>::max()) + zx::msec(999)).get()),
        },
        {
            .test_name = "EmptyString",
            .input = "",
            .expected = std::nullopt,
        },
        {
            .test_name = "CannotDecodeMessage",
            .input = "!!! CANNOT DECODE!!!\n",
            .expected = std::nullopt,
        },
        {
            .test_name = "MissingSeconds",
            .input = "[.000][07559][07687]",
            .expected = std::nullopt,
        },
        {
            .test_name = "NonDigitInSeconds",
            .input = "[156a4.000][07559][07687]",
            .expected = std::nullopt,
        },
        {
            .test_name = "MissingTimestampDot",
            .input = "[15604][07559][07687]",
            .expected = std::nullopt,
        },
        {
            .test_name = "MissingMilliseconds",
            .input = "[15604.][07559][07687]",
            .expected = std::nullopt,
        },
        {
            .test_name = "TwoDigitMilliseconds",
            .input = "[15604.00][07559][07687]",
            .expected = std::nullopt,
        },
        {
            .test_name = "FourDigitMilliseconds",
            .input = "[15604.0000][07559][07687]",
            .expected = std::nullopt,
        },
        {
            .test_name = "SecondsOverflow",
            .input = "[4294967296.000][07559][07687]",
            .expected = std::nullopt,
        },
        {
            .test_name = "NonDigitInMilliseconds",
            .input = "[15604.0a0][07559][07687]",
            .expected = std::nullopt,
        },
        {
            .test_name = "MissingProcessKoidOpenBracket",
            .input = "[15604.000]07559][07687]",
            .expected = std::nullopt,
        },
        {
            .test_name = "MissingProcessKoid",
            .input = "[15604.000][][07687]",
            .expected = std::nullopt,
        },
        {
            .test_name = "NonDigitInProcessKoid",
            .input = "[15604.000][075a9][07687]",
            .expected = std::nullopt,
        },
        {
            .test_name = "ProcessKoidOverflow",
            .input = "[15604.000][18446744073709551616][07687]",
            .expected = std::nullopt,
        },
        {
            .test_name = "MissingThreadKoidOpenBracket",
            .input = "[15604.000][07559]07687]",
            .expected = std::nullopt,
        },
        {
            .test_name = "MissingThreadKoid",
            .input = "[15604.000][07559][]",
            .expected = std::nullopt,
        },
        {
            .test_name = "NonDigitInThreadKoid",
            .input = "[15604.000][07559][076a7]",
            .expected = std::nullopt,
        },
        {
            .test_name = "ThreadKoidOverflow",
            .input = "[15604.000][07559][18446744073709551616]",
            .expected = std::nullopt,
        },
        {
            // Non-ASCII bytes are negative when char is signed.
            .test_name = "NonAsciiSeconds",
            .input = "[\xd9\xa1.000][07559][07687]",
            .expected = std::nullopt,
        },
        {
            .test_name = "EndsInSeconds",
            .input = "[15604",
            .expected = std::nullopt,
        },
        {
            .test_name = "EndsInMilliseconds",
            .input = "[15604.000",
            .expected = std::nullopt,
        },
        {
            .test_name = "EndsInProcessKoid",
            .input = "[15604.000][07559",
            .expected = std::nullopt,
        },
        {
            .test_name = "EndsInThreadKoid",
            .input = "[15604.000][07559][07687",
            .expected = std::nullopt,
        },
    })),
    [](const TestParamInfo<ParseLogHeaderTestCase>& info) { return info.param.test_name; });

struct FormatRoundTripTestCase {
  std::string test_name;
  zx::time_boot timestamp;
  uint64_t pid;
  uint64_t tid;
  uint32_t dropped_logs;
  std::string text;
  zx::time_boot expected_timestamp;
};

using FormatRoundTripTest = TestWithParam<FormatRoundTripTestCase>;

TEST_P(FormatRoundTripTest, FormatAndParseLogHeaderRoundTrip) {
  const FormatRoundTripTestCase& test_case = GetParam();
  fuchsia::logger::LogMessage message =
      BuildLogMessage(FUCHSIA_LOG_INFO, test_case.text,
                      /*timestamp_offset=*/zx::nsec(0), /*tags=*/{"foo", "bar"});
  message.time = test_case.timestamp;
  message.pid = test_case.pid;
  message.tid = test_case.tid;
  message.dropped_logs = test_case.dropped_logs;

  const std::string formatted = Format(message);
  EXPECT_EQ(ParseLogHeader(formatted), test_case.expected_timestamp);
}

INSTANTIATE_TEST_SUITE_P(
    WithVariousMessages, FormatRoundTripTest,
    ValuesIn(std::vector<FormatRoundTripTestCase>({
        {
            .test_name = "ZeroTimestamp",
            .timestamp = zx::time_boot(0),
            .pid = kLogMessageProcessId,
            .tid = kLogMessageThreadId,
            .dropped_logs = 0,
            .text = "line 1",
            .expected_timestamp = zx::time_boot(0),
        },
        {
            .test_name = "MillisecondPrecision",
            .timestamp = zx::time_boot((zx::sec(15604) + zx::msec(25)).get()),
            .pid = kLogMessageProcessId,
            .tid = kLogMessageThreadId,
            .dropped_logs = 0,
            .text = "line 1",
            .expected_timestamp = zx::time_boot((zx::sec(15604) + zx::msec(25)).get()),
        },
        {
            .test_name = "SubMillisecondTruncated",
            .timestamp = zx::time_boot((zx::sec(15604) + zx::msec(25) + zx::usec(999)).get()),
            .pid = kLogMessageProcessId,
            .tid = kLogMessageThreadId,
            .dropped_logs = 0,
            .text = "line 1",
            .expected_timestamp = zx::time_boot((zx::sec(15604) + zx::msec(25)).get()),
        },
        {
            .test_name = "LargeTimestampOverFiveDigits",
            .timestamp = zx::time_boot((zx::sec(100000) + zx::msec(999)).get()),
            .pid = kLogMessageProcessId,
            .tid = kLogMessageThreadId,
            .dropped_logs = 0,
            .text = "line 1",
            .expected_timestamp = zx::time_boot((zx::sec(100000) + zx::msec(999)).get()),
        },
        {
            .test_name = "ZeroKoids",
            .timestamp = zx::time_boot((zx::sec(15604) + zx::msec(1)).get()),
            .pid = 0,
            .tid = 0,
            .dropped_logs = 0,
            .text = "line 1",
            .expected_timestamp = zx::time_boot((zx::sec(15604) + zx::msec(1)).get()),
        },
        {
            .test_name = "LargeKoids",
            .timestamp = zx::time_boot((zx::sec(15604) + zx::msec(1)).get()),
            .pid = std::numeric_limits<uint64_t>::max(),
            .tid = std::numeric_limits<uint64_t>::max() - 1,
            .dropped_logs = 0,
            .text = "line 1",
            .expected_timestamp = zx::time_boot((zx::sec(15604) + zx::msec(1)).get()),
        },
        {
            .test_name = "WithDroppedLogs",
            .timestamp = zx::time_boot((zx::sec(15604) + zx::msec(1)).get()),
            .pid = kLogMessageProcessId,
            .tid = kLogMessageThreadId,
            .dropped_logs = 2,
            .text = "line 1",
            .expected_timestamp = zx::time_boot((zx::sec(15604) + zx::msec(1)).get()),
        },
        {
            .test_name = "MultilineMessage",
            .timestamp = zx::time_boot((zx::sec(15604) + zx::msec(4)).get()),
            .pid = kLogMessageProcessId,
            .tid = kLogMessageThreadId,
            .dropped_logs = 0,
            .text = "multi\nline\nmessage",
            .expected_timestamp = zx::time_boot((zx::sec(15604) + zx::msec(4)).get()),
        },
    })),
    [](const TestParamInfo<FormatRoundTripTestCase>& info) { return info.param.test_name; });

}  // namespace
}  // namespace forensics
