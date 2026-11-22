// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_ZXDUMP_THREAD_STATE_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_ZXDUMP_THREAD_STATE_H_

#include <zircon/syscalls/debug.h>
#include <zircon/syscalls/exception.h>

#include <cstddef>
#include <optional>
#include <span>
#include <vector>

#include "src/developer/debug/ipc/protocol.h"
#include "src/developer/debug/shared/register_value.h"
#include "src/lib/unwinder/registers.h"

namespace zxdb {

// Converts arm64 thread state from a dump to debug register values. These functions append to
// |out|.
void AppendArm64GeneralRegisters(const zx_arm64_thread_state_general_regs_t& regs,
                                 std::vector<debug::RegisterValue>* out);
void AppendArm64VectorRegisters(const zx_arm64_thread_state_vector_regs_t& regs,
                                std::vector<debug::RegisterValue>* out);
// Decodes ZX_THREAD_STATE_VECTOR_REGS bytes from a dump. Some kernels wrote a 560-byte state:
// the 520-byte NEON layout followed by the SVE p and ffr registers and padding. Both sizes start
// with the same NEON layout, so both decode here. Other sizes return std::nullopt.
std::optional<zx_arm64_thread_state_vector_regs_t> DecodeArm64VectorRegs(
    std::span<const std::byte> bytes);
void AppendArm64DebugRegisters(const zx_arm64_thread_state_debug_regs_t& regs,
                               std::vector<debug::RegisterValue>* out);

// Converts arm64 general registers to the unwinder register set.
unwinder::Registers Arm64UnwinderRegisters(const zx_arm64_thread_state_general_regs_t& regs);

// Decodes an arm64 ZX_INFO_THREAD_EXCEPTION_REPORT into a NotifyException with |type| and
// |exception| populated.
debug_ipc::NotifyException DecodeArm64ExceptionReport(const zx_exception_report_t& report);

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_ZXDUMP_THREAD_STATE_H_
