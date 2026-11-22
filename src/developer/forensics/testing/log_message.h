// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_TESTING_LOG_MESSAGE_H_
#define SRC_DEVELOPER_FORENSICS_TESTING_LOG_MESSAGE_H_

#include <fidl/fuchsia.logger/cpp/fidl.h>
#include <lib/zx/time.h>

#include <cstdint>
#include <string>
#include <vector>

namespace forensics {

inline constexpr zx::time_boot kLogMessageBaseTimestamp = zx::time_boot(zx::sec(15604).to_nsecs());
inline constexpr uint64_t kLogMessageProcessId = 7559;
inline constexpr uint64_t kLogMessageThreadId = 7687;

// Returns a LogMessage with the given severity, message and optional tags.
//
// The process and thread ids are set to |kLogMessageProcessId| and |kLogMessageThreadId|. The
// timestamp is |kLogMessageBaseTimestamp| plus the optionally provided offset.
fuchsia_logger::LogMessage BuildLogMessage(const int32_t severity, const std::string& text,
                                           const zx::duration timestamp_offset = zx::duration(0),
                                           const std::vector<std::string>& tags = {});

}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_TESTING_LOG_MESSAGE_H_
