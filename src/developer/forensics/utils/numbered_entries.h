// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_UTILS_NUMBERED_ENTRIES_H_
#define SRC_DEVELOPER_FORENSICS_UTILS_NUMBERED_ENTRIES_H_

#include <cstdint>
#include <string>
#include <vector>

namespace forensics {

enum class EntryType : std::uint8_t {
  kFile,
  kDirectory,
};

// Returns the numbers of the entries of |type| in |dir| that are named after a non-negative
// integer, e.g. 12 for "12", sorted in ascending order. Names with leading zeros, like "012", are
// skipped, so callers can rebuild an entry's path from its number with std::to_string().
//
// Returns an empty vector if |dir| can't be read.
std::vector<uint64_t> GetNumberedEntries(const std::string& dir, EntryType type);

}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_UTILS_NUMBERED_ENTRIES_H_
