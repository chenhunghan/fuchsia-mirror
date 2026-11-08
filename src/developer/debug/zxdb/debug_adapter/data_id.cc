// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/data_id.h"

#include <inttypes.h>
#include <lib/syslog/cpp/macros.h>

#include <limits>
#include <string>
#include <string_view>

#include "src/developer/debug/shared/arch.h"
#include "src/developer/debug/zxdb/client/breakpoint_settings.h"
#include "src/developer/debug/zxdb/common/checked_math.h"
#include "src/developer/debug/zxdb/common/err_or.h"
#include "src/developer/debug/zxdb/expr/expr_value.h"
#include "src/lib/fxl/strings/split_string.h"
#include "src/lib/fxl/strings/string_number_conversions.h"
#include "src/lib/fxl/strings/string_printf.h"

namespace zxdb {

ErrOr<DataId> DataId::FromExprValue(debug::Arch arch, uint64_t process_koid, std::string_view name,
                                    const ExprValue& value) {
  FX_DCHECK(process_koid != 0);

  const ExprValueSource& source = value.source();
  if (source.type() != ExprValueSource::Type::kMemory) {
    return Err("Expression '" + std::string(name) + "' is stored in a " +
               ExprValueSource::TypeToString(source.type()) +
               " location. Only values stored in memory can be watched.");
  }

  if (source.is_bitfield()) {
    return Err("Bitfields cannot be watched.");
  }

  size_t raw_size = value.data().size();
  uint32_t size = raw_size > std::numeric_limits<uint32_t>::max()
                      ? std::numeric_limits<uint32_t>::max()
                      : static_cast<uint32_t>(raw_size);
  if (Err err = BreakpointSettings::ValidateSize(arch, BreakpointSettings::Type::kWrite, size);
      err.has_error()) {
    return err;
  }

  uint64_t address = source.address();
  if (!CheckedAdd(address, static_cast<uint64_t>(size)).has_value()) {
    return Err("Address range overflows 64-bit address space.");
  }

  return DataId(process_koid, address, size);
}

ErrOr<DataId> DataId::Parse(std::string_view data_id) {
  auto format_err = [&]() { return Err("Invalid dataId format: " + std::string(data_id)); };

  auto parts = fxl::SplitString(data_id, ":", fxl::kKeepWhitespace, fxl::kSplitWantAll);
  if (parts.size() != 3) {
    return format_err();
  }

  uint64_t koid = 0;
  if (!fxl::StringToNumberWithError<uint64_t>(parts[0], &koid, fxl::Base::k10) || koid == 0) {
    return format_err();
  }

  std::string_view addr_part = parts[1];
  std::string_view size_part = parts[2];

  if (addr_part.size() <= 2 || addr_part[0] != '0' ||
      (addr_part[1] != 'x' && addr_part[1] != 'X')) {
    return format_err();
  }

  uint64_t address = 0;
  uint32_t byte_size = 0;
  if (!fxl::StringToNumberWithError<uint64_t>(addr_part.substr(2), &address, fxl::Base::k16) ||
      !fxl::StringToNumberWithError<uint32_t>(size_part, &byte_size, fxl::Base::k10) ||
      byte_size == 0) {
    return format_err();
  }

  if (!CheckedAdd(address, static_cast<uint64_t>(byte_size)).has_value()) {
    return format_err();
  }

  return DataId(koid, address, byte_size);
}

ErrOr<BreakpointSettings::Type> DataId::ParseAccessType(std::string_view access_type) {
  if (access_type == "write") {
    return BreakpointSettings::Type::kWrite;
  }
  if (access_type == "readWrite") {
    return BreakpointSettings::Type::kReadWrite;
  }
  if (access_type == "read") {
    return Err("Read-only data breakpoints are not supported on this platform.");
  }
  return Err("Unknown accessType: " + std::string(access_type));
}

Err DataId::ValidateSize(debug::Arch arch, BreakpointSettings::Type type) const {
  return BreakpointSettings::ValidateSize(arch, type, byte_size_);
}

std::string DataId::ToString() const {
  return fxl::StringPrintf("%" PRIu64 ":0x%" PRIx64 ":%u", process_koid_, address_, byte_size_);
}

}  // namespace zxdb
