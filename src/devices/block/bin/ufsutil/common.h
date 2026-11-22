// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_BIN_UFSUTIL_COMMON_H_
#define SRC_DEVICES_BLOCK_BIN_UFSUTIL_COMMON_H_

#include <cstdint>
#include <cstdlib>
#include <format>
#include <optional>
#include <ostream>
#include <string>
#include <unordered_map>
#include <variant>

#include <safemath/safe_conversions.h>

namespace ufsutil {
using OptionValue = std::variant<uint32_t, std::string>;

// Maximum size of data that can be transferred in a single request.
// The device driver limits transfer size to 1MiB for performance reasons.
constexpr uint32_t kMaxTransferSize = 1024 * 1024;

template <typename T>
bool GetOptionAs(const std::unordered_map<uint32_t, OptionValue>& options, char opt, T& out,
                 std::ostream& err) {
  if (options.contains(opt)) {
    auto value = std::get<uint32_t>(options.at(opt));
    if (safemath::IsValueInRangeForNumericType<T>(value)) {
      out = static_cast<T>(value);
    } else {
      err << std::format("error: invalid argument for '-{:c}'\n", opt);
      return false;
    }
  }
  return true;
}

// Parses a decimal, octal (0 prefix) or hexadecimal (0x prefix) number that fits in 32 bits.
inline std::optional<uint32_t> ParseStrToUint32(const char* str) {
  char* endptr;
  uint64_t ret = std::strtoul(str, &endptr, 0);
  if (endptr == str || *endptr != '\0') {
    return std::nullopt;
  }
  if (ret > UINT32_MAX) {
    return std::nullopt;
  }
  return static_cast<uint32_t>(ret);
}

inline bool CheckRequiredOption(const std::unordered_map<uint32_t, OptionValue>& options, char opt,
                                const char* long_opt, std::ostream& err) {
  if (!options.contains(opt)) {
    err << std::format("error: missing required option -{:c}/--{}\n", opt, long_opt);
    return false;
  }
  return true;
}
}  // namespace ufsutil

#endif  // SRC_DEVICES_BLOCK_BIN_UFSUTIL_COMMON_H_
