// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_BIN_UFSUTIL_ERROR_HISTORY_H_
#define SRC_DEVICES_BLOCK_BIN_UFSUTIL_ERROR_HISTORY_H_

#include <fidl/fuchsia.hardware.ufs/cpp/wire.h>
#include <lib/zx/result.h>

#include <array>
#include <cstdint>
#include <ostream>
#include <unordered_map>

#include "src/devices/block/bin/ufsutil/common.h"

namespace ufsutil {
struct ErrorDirectoryEntry {
  uint8_t buffer_id;
  std::array<uint8_t, 3> reserved;
  uint32_t length;
};
static_assert(sizeof(ErrorDirectoryEntry) == 8, "ErrorDirectoryEntry must be 8 bytes");

struct ErrorDirectoryHeader {
  std::array<uint8_t, 8> vendor_id;
  uint8_t version;
  uint8_t reserved1;
  std::array<uint8_t, 20> reserved2;
  uint16_t directory_length;
};
static_assert(sizeof(ErrorDirectoryHeader) == 32, "ErrorDirectoryHeader must be 32 bytes");

zx::result<> HandleErrorHistory(const fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>& client,
                                const std::unordered_map<uint32_t, OptionValue>& options,
                                std::ostream& out, std::ostream& err);
}  // namespace ufsutil

#endif  // SRC_DEVICES_BLOCK_BIN_UFSUTIL_ERROR_HISTORY_H_
