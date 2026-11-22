// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/utils/numbered_entries.h"

#include <algorithm>
#include <optional>

#include "src/lib/files/directory.h"
#include "src/lib/files/file.h"
#include "src/lib/files/path.h"
#include "src/lib/fxl/strings/string_number_conversions.h"

namespace forensics {
namespace {

// Returns the non-negative integer that |name| is named after, e.g. 12 for "12", or std::nullopt
// if |name| isn't a non-negative integer or has leading zeros, like "012".
std::optional<uint64_t> ParseEntryNumber(const std::string& name) {
  uint64_t number = 0;
  if (!fxl::StringToNumberWithError(name, &number) || std::to_string(number) != name) {
    return std::nullopt;
  }

  return number;
}

}  // namespace

std::vector<uint64_t> GetNumberedEntries(const std::string& dir, const EntryType type) {
  std::vector<std::string> names;
  if (!files::ReadDirContents(dir, &names)) {
    return {};
  }

  std::vector<uint64_t> numbers;
  for (const std::string& name : names) {
    const std::optional<uint64_t> number = ParseEntryNumber(name);
    if (!number.has_value()) {
      continue;
    }

    const std::string path = files::JoinPath(dir, name);
    const bool matches_type =
        (type == EntryType::kFile) ? files::IsFile(path) : files::IsDirectory(path);
    if (matches_type) {
      numbers.push_back(*number);
    }
  }

  std::ranges::sort(numbers);
  return numbers;
}

}  // namespace forensics
