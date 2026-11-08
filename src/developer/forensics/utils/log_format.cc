// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/utils/log_format.h"

#include <fuchsia/logger/cpp/fidl.h>
#include <lib/fit/function.h>
#include <lib/syslog/cpp/log_level.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>

#include <cctype>
#include <cinttypes>
#include <cstddef>
#include <cstdint>
#include <optional>
#include <string_view>

#include "src/lib/fxl/strings/join_strings.h"
#include "src/lib/fxl/strings/string_number_conversions.h"
#include "src/lib/fxl/strings/string_printf.h"

namespace forensics {
namespace {

constexpr char kDroppedOnceFormatStr[] = "!!! DROPPED 1 LOG !!!";
constexpr char kDroppedFormatStr[] = "!!! DROPPED %u LOGS !!!";

std::string SeverityToString(const int32_t severity) {
  if (severity == FUCHSIA_LOG_TRACE) {
    return "TRACE";
  } else if (severity == FUCHSIA_LOG_DEBUG) {
    return "DEBUG";
  } else if (severity > FUCHSIA_LOG_DEBUG && severity < FUCHSIA_LOG_INFO) {
    return fxl::StringPrintf("VLOG(%d)", FUCHSIA_LOG_INFO - severity);
  } else if (severity == FUCHSIA_LOG_INFO) {
    return "INFO";
  } else if (severity == FUCHSIA_LOG_WARNING) {
    return "WARN";
  } else if (severity == FUCHSIA_LOG_ERROR) {
    return "ERROR";
  } else if (severity == FUCHSIA_LOG_FATAL) {
    return "FATAL";
  }
  return "INVALID";
}

fit::function<std::string(int32_t, const std::string&)> FormatFn(
    const fuchsia::logger::LogMessage& message) {
  return [&message](int32_t severity, const std::string& msg) {
    return fxl::StringPrintf("[%05d.%03d][%05" PRIu64 "][%05" PRIu64 "][%s] %s: %s\n",
                             static_cast<int>(message.time.get() / 1000000000ULL),
                             static_cast<int>((message.time.get() / 1000000ULL) % 1000ULL),
                             message.pid, message.tid, fxl::JoinStrings(message.tags, ", ").c_str(),
                             SeverityToString(severity).c_str(), msg.c_str());
  };
}

}  // namespace

std::string Format(const fuchsia::logger::LogMessage& message) {
  fit::function<std::string(int32_t, const std::string&)> format = FormatFn(message);

  std::string log;
  if (message.dropped_logs == 1) {
    log += format(FUCHSIA_LOG_WARNING, kDroppedOnceFormatStr);
  } else if (message.dropped_logs > 1) {
    log += format(FUCHSIA_LOG_WARNING, fxl::StringPrintf(kDroppedFormatStr, message.dropped_logs));
  }

  log += format(message.severity, message.msg);

  return log;
}

std::optional<zx::time_boot> ParseLogHeader(std::string_view str) {
  auto ConsumeChar = [&str](const char c) {
    if (!str.starts_with(c)) {
      return false;
    }

    str.remove_prefix(1);
    return true;
  };

  // On success, consumes a non-empty sequence of digits and parses them into |value|. Fails if
  // |str| doesn't start with digits, the number doesn't fit in |value|, or |expected_digits| is set
  // and doesn't match the number of digits.
  auto ConsumeNumber = [&str](auto* value,
                              const std::optional<size_t> expected_digits = std::nullopt) {
    size_t n = 0;
    while (n < str.size() && std::isdigit(static_cast<unsigned char>(str[n]))) {
      ++n;
    }

    if (n == 0 || (expected_digits.has_value() && n != *expected_digits) ||
        !fxl::StringToNumberWithError(str.substr(0, n), value)) {
      return false;
    }

    str.remove_prefix(n);
    return true;
  };

  uint32_t secs = 0;
  uint32_t msecs = 0;
  uint64_t pid = 0;
  uint64_t tid = 0;
  const bool matches =
      // The timestamp is like [$secs.$msecs]. Format() always writes 3 digits for $msecs, so e.g.
      // "1.5" isn't a valid header (it would otherwise be read as 1.005s).
      ConsumeChar('[') && ConsumeNumber(&secs) && ConsumeChar('.') &&
      ConsumeNumber(&msecs, /*expected_digits=*/3) && ConsumeChar(']') &&
      // The process koid is like [$pid].
      ConsumeChar('[') && ConsumeNumber(&pid) && ConsumeChar(']') &&
      // The thread koid is like [$tid].
      ConsumeChar('[') && ConsumeNumber(&tid) && ConsumeChar(']');

  if (!matches) {
    return std::nullopt;
  }

  return zx::time_boot((zx::sec(secs) + zx::msec(msecs)).get());
}

}  // namespace forensics
