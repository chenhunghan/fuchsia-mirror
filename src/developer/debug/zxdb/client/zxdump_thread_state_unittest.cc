// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/zxdump_thread_state.h"

#include <cstring>
#include <span>
#include <utility>
#include <vector>

#include <gtest/gtest.h>

#include "src/developer/debug/shared/register_id.h"

namespace zxdb {

namespace {

using debug::RegisterID;

// TODO(https://fxbug.dev/586369747): Replace this linear scan with a shared RegisterSet lookup.
const debug::RegisterValue* Find(const std::vector<debug::RegisterValue>& regs, RegisterID id) {
  for (const auto& reg : regs) {
    if (reg.id == id) {
      return &reg;
    }
  }
  return nullptr;
}

uint64_t Value(const std::vector<debug::RegisterValue>& regs, RegisterID id) {
  const debug::RegisterValue* reg = Find(regs, id);
  EXPECT_NE(nullptr, reg) << std::to_underlying(id);
  return reg ? static_cast<uint64_t>(reg->GetValue()) : 0;
}

TEST(ZxdumpThreadState, Arm64General) {
  zx_arm64_thread_state_general_regs_t regs = {};
  for (size_t i = 0; i < std::size(regs.r); i++) {
    regs.r[i] = 0x100 + i;
  }
  regs.lr = 0x1111;
  regs.sp = 0x2222;
  regs.pc = 0x3333;
  regs.cpsr = 0x4444;
  regs.tpidr = 0x5555;

  std::vector<debug::RegisterValue> out;
  AppendArm64GeneralRegisters(regs, &out);
  EXPECT_EQ(35u, out.size());
  EXPECT_EQ(0x100u, Value(out, RegisterID::kARMv8_x0));
  EXPECT_EQ(0x100u + 29, Value(out, RegisterID::kARMv8_x29));
  EXPECT_EQ(0x1111u, Value(out, RegisterID::kARMv8_lr));
  EXPECT_EQ(0x2222u, Value(out, RegisterID::kARMv8_sp));
  EXPECT_EQ(0x3333u, Value(out, RegisterID::kARMv8_pc));
  EXPECT_EQ(0x4444u, Value(out, RegisterID::kARMv8_cpsr));
  EXPECT_EQ(0x5555u, Value(out, RegisterID::kARMv8_tpidr));
}

TEST(ZxdumpThreadState, Arm64Vector) {
  zx_arm64_thread_state_vector_regs_t regs = {};
  regs.fpcr = 0x12;
  regs.fpsr = 0x34;
  regs.v[31].low = 0x1122334455667788;
  regs.v[31].high = 0x99aabbccddeeff00;

  std::vector<debug::RegisterValue> out;
  AppendArm64VectorRegisters(regs, &out);
  EXPECT_EQ(34u, out.size());
  EXPECT_EQ(0x12u, Value(out, RegisterID::kARMv8_fpcr));
  EXPECT_EQ(0x34u, Value(out, RegisterID::kARMv8_fpsr));
  const debug::RegisterValue* v31 = Find(out, RegisterID::kARMv8_v31);
  ASSERT_NE(nullptr, v31);
  ASSERT_EQ(16u, v31->data.size());
  uint64_t halves[2];
  memcpy(halves, v31->data.data(), sizeof(halves));
  EXPECT_EQ(regs.v[31].low, halves[0]);
  EXPECT_EQ(regs.v[31].high, halves[1]);
}

TEST(ZxdumpThreadState, Arm64VectorStateSizes) {
  zx_arm64_thread_state_vector_regs_t regs = {};
  regs.fpcr = 0x12;
  regs.v[31].high = 0x99aabbccddeeff00;
  // The 40 bytes after the NEON layout stand for the SVE p, ffr, and padding.
  std::vector<std::byte> bytes(sizeof(regs) + 40, std::byte{0xff});
  memcpy(bytes.data(), &regs, sizeof(regs));
  std::span<const std::byte> all(bytes);

  auto neon = DecodeArm64VectorRegs(all.first(sizeof(regs)));
  ASSERT_TRUE(neon);
  EXPECT_EQ(0x12u, neon->fpcr);
  EXPECT_EQ(regs.v[31].high, neon->v[31].high);

  auto with_sve = DecodeArm64VectorRegs(all);
  ASSERT_TRUE(with_sve);
  EXPECT_EQ(0x12u, with_sve->fpcr);
  EXPECT_EQ(regs.v[31].high, with_sve->v[31].high);

  EXPECT_FALSE(DecodeArm64VectorRegs(all.first(sizeof(regs) - 1)));
  EXPECT_FALSE(DecodeArm64VectorRegs(all.first(sizeof(regs) + 34)));
}

TEST(ZxdumpThreadState, Arm64Debug) {
  zx_arm64_thread_state_debug_regs_t regs = {};
  regs.hw_bps_count = 2;
  regs.hw_wps_count = 16;
  regs.hw_bps[0].dbgbcr = 0x5;
  regs.hw_bps[0].dbgbvr = 0x1000;
  regs.hw_bps[2].dbgbcr = 0x9;
  regs.hw_wps[15].dbgwcr = 0x7;
  regs.hw_wps[15].dbgwvr = 0x2000;

  std::vector<debug::RegisterValue> out;
  AppendArm64DebugRegisters(regs, &out);
  EXPECT_EQ(36u, out.size());
  EXPECT_EQ(0x5u, Value(out, RegisterID::kARMv8_dbgbcr0_el1));
  EXPECT_EQ(0x1000u, Value(out, RegisterID::kARMv8_dbgbvr0_el1));
  EXPECT_EQ(nullptr, Find(out, RegisterID::kARMv8_dbgbcr2_el1));
  EXPECT_EQ(0x7u, Value(out, RegisterID::kARMv8_dbgwcr15_el1));
  EXPECT_EQ(0x2000u, Value(out, RegisterID::kARMv8_dbgwvr15_el1));
}

TEST(ZxdumpThreadState, Arm64UnwinderRegisters) {
  zx_arm64_thread_state_general_regs_t regs = {};
  regs.r[29] = 0xf0;
  regs.lr = 0x10;
  regs.sp = 0x20;
  regs.pc = 0x30;

  unwinder::Registers out = Arm64UnwinderRegisters(regs);
  uint64_t value = 0;
  ASSERT_TRUE(out.Get(unwinder::RegisterID::kArm64_x29, value).ok());
  EXPECT_EQ(0xf0u, value);
  ASSERT_TRUE(out.Get(unwinder::RegisterID::kArm64_lr, value).ok());
  EXPECT_EQ(0x10u, value);
  ASSERT_TRUE(out.GetSP(value).ok());
  EXPECT_EQ(0x20u, value);
  ASSERT_TRUE(out.GetPC(value).ok());
  EXPECT_EQ(0x30u, value);
}

TEST(ZxdumpThreadState, Arm64ExceptionReport) {
  zx_exception_report_t report = {};
  report.header.size = sizeof(report);
  report.header.type = ZX_EXCP_FATAL_PAGE_FAULT;
  report.context.arch.u.arm_64.esr = 0x92000046;
  report.context.arch.u.arm_64.far = 0xdead0000;

  debug_ipc::NotifyException exception = DecodeArm64ExceptionReport(report);
  EXPECT_EQ(debug_ipc::ExceptionType::kPageFault, exception.type);
  EXPECT_TRUE(exception.exception.valid);
  EXPECT_EQ(0x92000046u, exception.exception.arch.arm64.esr);
  EXPECT_EQ(0xdead0000u, exception.exception.arch.arm64.far);

  report.header.type = ZX_EXCP_SW_BREAKPOINT;
  EXPECT_EQ(debug_ipc::ExceptionType::kSoftwareBreakpoint, DecodeArm64ExceptionReport(report).type);

  report.header.type = ZX_EXCP_HW_BREAKPOINT;
  // EC 0b110000: Hardware breakpoint from a lower exception level.
  report.context.arch.u.arm_64.esr = 0b110000u << 26u;
  EXPECT_EQ(debug_ipc::ExceptionType::kHardwareBreakpoint, DecodeArm64ExceptionReport(report).type);

  // EC 0b110010: Software step from a lower exception level.
  report.context.arch.u.arm_64.esr = 0b110010u << 26u;
  EXPECT_EQ(debug_ipc::ExceptionType::kSingleStep, DecodeArm64ExceptionReport(report).type);

  // EC 0b110100: Watchpoint from a lower exception level.
  report.context.arch.u.arm_64.esr = 0b110100u << 26u;
  EXPECT_EQ(debug_ipc::ExceptionType::kWatchpoint, DecodeArm64ExceptionReport(report).type);
}

}  // namespace
}  // namespace zxdb
