// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_UTILS_LOG_FORMAT_H_
#define SRC_DEVELOPER_FORENSICS_UTILS_LOG_FORMAT_H_

#include <fuchsia/logger/cpp/fidl.h>
#include <lib/zx/time.h>

#include <optional>
#include <string>
#include <string_view>

namespace forensics {

// Format a log message as a string. Inserts a "dropped log" message if |message.dropped_logs| > 0.
std::string Format(const fuchsia::logger::LogMessage& message);

// Parses the header at the start of |str| and returns its timestamp. Returns std::nullopt if |str|
// doesn't start with a header in the format that Format() writes, i.e.
// "[$secs.$msecs][$pid][$tid]".
std::optional<zx::time_boot> ParseLogHeader(std::string_view str);

}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_UTILS_LOG_FORMAT_H_
