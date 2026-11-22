// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_BIN_UFSUTIL_QUERY_H_
#define SRC_DEVICES_BLOCK_BIN_UFSUTIL_QUERY_H_

#include <fidl/fuchsia.hardware.ufs/cpp/wire.h>
#include <lib/zx/result.h>

#include <cstdint>
#include <ostream>
#include <unordered_map>

#include "common.h"

namespace ufsutil {
zx::result<> HandleReadDescriptor(const fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>& client,
                                  const std::unordered_map<uint32_t, OptionValue>& options,
                                  std::ostream& out, std::ostream& err);

zx::result<> HandleWriteDescriptor(const fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>& client,
                                   const std::unordered_map<uint32_t, OptionValue>& options,
                                   std::ostream& out, std::ostream& err);

zx::result<> HandleSetFlag(const fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>& client,
                           const std::unordered_map<uint32_t, OptionValue>& options,
                           std::ostream& out, std::ostream& err);

zx::result<> HandleReadFlag(const fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>& client,
                            const std::unordered_map<uint32_t, OptionValue>& options,
                            std::ostream& out, std::ostream& err);

zx::result<> HandleToggleFlag(const fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>& client,
                              const std::unordered_map<uint32_t, OptionValue>& options,
                              std::ostream& out, std::ostream& err);

zx::result<> HandleClearFlag(const fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>& client,
                             const std::unordered_map<uint32_t, OptionValue>& options,
                             std::ostream& out, std::ostream& err);

zx::result<> HandleReadAttribute(const fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>& client,
                                 const std::unordered_map<uint32_t, OptionValue>& options,
                                 std::ostream& out, std::ostream& err);

zx::result<> HandleWriteAttribute(const fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>& client,
                                  const std::unordered_map<uint32_t, OptionValue>& options,
                                  std::ostream& out, std::ostream& err);
}  // namespace ufsutil

#endif  // SRC_DEVICES_BLOCK_BIN_UFSUTIL_QUERY_H_
