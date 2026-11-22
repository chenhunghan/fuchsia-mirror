// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_FEEDBACK_PREVIOUS_BOOT_LOGS_BOOT_INFO_H_
#define SRC_DEVELOPER_FORENSICS_FEEDBACK_PREVIOUS_BOOT_LOGS_BOOT_INFO_H_

#include <lib/zx/time.h>

#include <optional>
#include <string>
#include <string_view>

namespace forensics::feedback {

struct BootInfo {
  std::optional<std::string> boot_id;
  std::optional<zx::duration> uptime;
  std::optional<zx::duration> utc_boot_difference;

  bool operator==(const BootInfo& other) const = default;

  std::string ToJson() const;
  static std::optional<BootInfo> FromJson(std::string_view json);
};

}  // namespace forensics::feedback

#endif  // SRC_DEVELOPER_FORENSICS_FEEDBACK_PREVIOUS_BOOT_LOGS_BOOT_INFO_H_
