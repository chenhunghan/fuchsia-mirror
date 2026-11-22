// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/testing/log_message.h"

#include <lib/zx/time.h>

#include <cstdint>

namespace forensics {

fuchsia_logger::LogMessage BuildLogMessage(const int32_t severity, const std::string& text,
                                           const zx::duration timestamp_offset,
                                           const std::vector<std::string>& tags) {
  fuchsia_logger::LogMessage msg{};
  msg.time(kLogMessageBaseTimestamp + timestamp_offset);
  msg.pid(kLogMessageProcessId);
  msg.tid(kLogMessageThreadId);
  msg.tags(tags);
  msg.severity(severity);
  msg.msg(text);
  return msg;
}

}  // namespace forensics
