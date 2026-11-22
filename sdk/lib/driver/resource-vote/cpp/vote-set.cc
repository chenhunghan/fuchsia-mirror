// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/resource-vote/cpp/vote-set.h>

#include <atomic>
#include <cstdio>

namespace fdf_resource_vote {

std::string_view EntryKindToString(EntryKind kind) {
  switch (kind) {
    case EntryKind::kLive:
      return "live";
    case EntryKind::kOrphan:
      return "orphan";
    case EntryKind::kHold:
      return "hold";
  }
  return "unknown";
}

namespace internal {

uint64_t NextId() {
  static std::atomic<uint64_t> next_id{1};
  return next_id.fetch_add(1, std::memory_order_relaxed);
}

void DefaultLog(LogSeverity severity, const std::string& message) {
  if (!fdf::Logger::HasGlobalInstance()) {
    fprintf(stderr, "[resource-vote] %s\n", message.c_str());
    return;
  }
  switch (severity) {
    case LogSeverity::kWarning:
      fdf::warn("{}", message);
      break;
    case LogSeverity::kError:
      fdf::error("{}", message);
      break;
  }
}

}  // namespace internal

std::optional<bool> MergeAnyTrue(const std::vector<bool>& requests) {
  for (bool request : requests) {
    if (request) {
      return true;
    }
  }
  return std::nullopt;
}

BoolVoteSet::BoolVoteSet(std::string resource_name)
    : VoteSet<bool>(std::move(resource_name), MergeAnyTrue,
                    [](const bool& request) { return std::string(request ? "on" : "off"); }) {}

}  // namespace fdf_resource_vote
