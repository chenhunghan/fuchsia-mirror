// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/zxdump_thread_state.h"

#include <algorithm>
#include <cstring>
#include <utility>

#include "src/developer/debug/ipc/decode_exception.h"
#include "src/developer/debug/ipc/unwinder_support.h"
#include "src/developer/debug/shared/register_id.h"

namespace zxdb {

namespace {

using debug::RegisterID;

RegisterID Offset(RegisterID base, size_t index) {
  return static_cast<RegisterID>(std::to_underlying(base) + index);
}

}  // namespace

void AppendArm64GeneralRegisters(const zx_arm64_thread_state_general_regs_t& regs,
                                 std::vector<debug::RegisterValue>* out) {
  for (size_t i = 0; i < std::size(regs.r); i++) {
    out->emplace_back(Offset(RegisterID::kARMv8_x0, i), regs.r[i]);
  }
  out->emplace_back(RegisterID::kARMv8_lr, regs.lr);
  out->emplace_back(RegisterID::kARMv8_sp, regs.sp);
  out->emplace_back(RegisterID::kARMv8_pc, regs.pc);
  out->emplace_back(RegisterID::kARMv8_cpsr, regs.cpsr);
  out->emplace_back(RegisterID::kARMv8_tpidr, regs.tpidr);
}

void AppendArm64VectorRegisters(const zx_arm64_thread_state_vector_regs_t& regs,
                                std::vector<debug::RegisterValue>* out) {
  out->emplace_back(RegisterID::kARMv8_fpcr, regs.fpcr);
  out->emplace_back(RegisterID::kARMv8_fpsr, regs.fpsr);
  for (size_t i = 0; i < std::size(regs.v); i++) {
    out->emplace_back(Offset(RegisterID::kARMv8_v0, i), sizeof(regs.v[i]), &regs.v[i]);
  }
}

std::optional<zx_arm64_thread_state_vector_regs_t> DecodeArm64VectorRegs(
    std::span<const std::byte> bytes) {
  // The older state appended uint16_t p[16], uint16_t ffr, and 6 bytes of padding.
  constexpr size_t kSvePredicateTailSize = (16 * sizeof(uint16_t)) + sizeof(uint16_t) + 6;
  static_assert(kSvePredicateTailSize == 40);
  constexpr size_t kWithSveSize =
      sizeof(zx_arm64_thread_state_vector_regs_t) + kSvePredicateTailSize;
  if (bytes.size() != sizeof(zx_arm64_thread_state_vector_regs_t) && bytes.size() != kWithSveSize) {
    return std::nullopt;
  }
  zx_arm64_thread_state_vector_regs_t regs;
  memcpy(&regs, bytes.data(), sizeof(regs));
  return regs;
}

void AppendArm64DebugRegisters(const zx_arm64_thread_state_debug_regs_t& regs,
                               std::vector<debug::RegisterValue>* out) {
  const size_t bp_count = std::min<size_t>(regs.hw_bps_count, std::size(regs.hw_bps));
  for (size_t i = 0; i < bp_count; i++) {
    out->emplace_back(Offset(RegisterID::kARMv8_dbgbcr0_el1, i), regs.hw_bps[i].dbgbcr);
    out->emplace_back(Offset(RegisterID::kARMv8_dbgbvr0_el1, i), regs.hw_bps[i].dbgbvr);
  }
  const size_t wp_count = std::min<size_t>(regs.hw_wps_count, std::size(regs.hw_wps));
  for (size_t i = 0; i < wp_count; i++) {
    out->emplace_back(Offset(RegisterID::kARMv8_dbgwcr0_el1, i), regs.hw_wps[i].dbgwcr);
    out->emplace_back(Offset(RegisterID::kARMv8_dbgwvr0_el1, i), regs.hw_wps[i].dbgwvr);
  }
}

unwinder::Registers Arm64UnwinderRegisters(const zx_arm64_thread_state_general_regs_t& regs) {
  std::vector<debug::RegisterValue> general;
  AppendArm64GeneralRegisters(regs, &general);
  return debug_ipc::ConvertRegisters(debug::Arch::kArm64, general);
}

debug_ipc::NotifyException DecodeArm64ExceptionReport(const zx_exception_report_t& report) {
  debug_ipc::NotifyException result;
  const uint32_t esr = report.context.arch.u.arm_64.esr;
  result.type = debug_ipc::DecodeArm64Exception(report.header.type,
                                                [esr] -> std::optional<uint32_t> { return esr; });
  result.exception.valid = true;
  result.exception.arch.arm64.esr = esr;
  result.exception.arch.arm64.far = report.context.arch.u.arm_64.far;
  return result;
}

}  // namespace zxdb
