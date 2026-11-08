// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/unwinder/tests/unwind_local.h"

#include <link.h>

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <memory>
#include <vector>

#include "src/lib/unwinder/memory.h"
#include "src/lib/unwinder/registers.h"
#include "src/lib/unwinder/unwind.h"

namespace unwinder {

namespace {

[[gnu::always_inline]] inline Registers GetContext() {
#ifdef __x86_64__
  Registers res(Registers::Arch::kX64);
  uint64_t regs[8] = {};
  uint64_t scratch;
  asm volatile(
      "1:\n"
      "movq %%rbx, 0(%[base])\n"
      "movq %%rbp, 8(%[base])\n"
      "movq %%rsp, 16(%[base])\n"
      "movq %%r12, 24(%[base])\n"
      "movq %%r13, 32(%[base])\n"
      "movq %%r14, 40(%[base])\n"
      "movq %%r15, 48(%[base])\n"
      "leaq 1b(%%rip), %[scratch]\n"
      "movq %[scratch], 56(%[base])\n"
      : [scratch] "=&a"(scratch)
      : [base] "D"(regs)
      : "memory");
  res.Set(RegisterID::kX64_rbx, regs[0]);
  res.Set(RegisterID::kX64_rbp, regs[1]);
  res.Set(RegisterID::kX64_rsp, regs[2]);
  res.Set(RegisterID::kX64_r12, regs[3]);
  res.Set(RegisterID::kX64_r13, regs[4]);
  res.Set(RegisterID::kX64_r14, regs[5]);
  res.Set(RegisterID::kX64_r15, regs[6]);
  res.Set(RegisterID::kX64_rip, regs[7]);
  return res;
#elifdef __aarch64__
  Registers res(Registers::Arch::kArm64);
  uint64_t regs[15] = {};
  register uint64_t* base asm("x0") = regs;
  asm volatile(
      "1:\n"
      "stp x18, x19, [%[base], #0]\n"
      "stp x20, x21, [%[base], #16]\n"
      "stp x22, x23, [%[base], #32]\n"
      "stp x24, x25, [%[base], #48]\n"
      "stp x26, x27, [%[base], #64]\n"
      "stp x28, x29, [%[base], #80]\n"
      "mov x1, sp\n"
      "stp x30, x1, [%[base], #96]\n"
      "adr x1, 1b\n"
      "str x1, [%[base], #112]\n"
      :
      : [base] "r"(base)
      : "x1", "memory");
  for (uint8_t i = 0; i < 13; i++) {
    res.Set(static_cast<RegisterID>(static_cast<uint8_t>(RegisterID::kArm64_x18) + i), regs[i]);
  }
  res.SetSP(regs[13]);
  res.SetPC(regs[14]);
  return res;
#elifdef __riscv
  Registers res(Registers::Arch::kRiscv64);
  uint64_t regs[17] = {};
  register uint64_t* base asm("a0") = regs;
  asm volatile(
      "1:\n"
      "sd ra, 0(%[base])\n"
      "sd sp, 8(%[base])\n"
      "sd gp, 16(%[base])\n"
      "sd tp, 24(%[base])\n"
      "sd s0, 32(%[base])\n"
      "sd s1, 40(%[base])\n"
      "sd s2, 48(%[base])\n"
      "sd s3, 56(%[base])\n"
      "sd s4, 64(%[base])\n"
      "sd s5, 72(%[base])\n"
      "sd s6, 80(%[base])\n"
      "sd s7, 88(%[base])\n"
      "sd s8, 96(%[base])\n"
      "sd s9, 104(%[base])\n"
      "sd s10, 112(%[base])\n"
      "sd s11, 120(%[base])\n"
      "la t0, 1b\n"
      "sd t0, 128(%[base])\n"
      :
      : [base] "r"(base)
      : "t0", "memory");
  res.Set(RegisterID::kRiscv64_ra, regs[0]);
  res.Set(RegisterID::kRiscv64_sp, regs[1]);
  res.Set(RegisterID::kRiscv64_gp, regs[2]);
  res.Set(RegisterID::kRiscv64_tp, regs[3]);
  res.Set(RegisterID::kRiscv64_s0, regs[4]);
  res.Set(RegisterID::kRiscv64_s1, regs[5]);
  for (uint8_t i = 0; i < 10; i++) {
    res.Set(static_cast<RegisterID>(static_cast<uint8_t>(RegisterID::kRiscv64_s2) + i),
            regs[6 + i]);
  }
  res.SetPC(regs[16]);
  return res;
#else
#error Unsupported platform
#endif
}

std::vector<uint64_t> GetLocalModules() {
  std::vector<uint64_t> modules;

  // Find all the modules in the current process.
  using CallbackType = int (*)(dl_phdr_info* info, size_t size, void* modules);
  CallbackType dl_iterate_phdr_callback = [](dl_phdr_info* info, size_t, void* p_modules) {
    auto modules = reinterpret_cast<std::vector<uint64_t>*>(p_modules);
    modules->push_back(info->dlpi_addr);
    return 0;
  };
  dl_iterate_phdr(dl_iterate_phdr_callback, &modules);

  return modules;
}

}  // namespace

std::vector<Frame> UnwindLocal() {
  auto modules = GetLocalModules();
  LocalMemory mem;
  auto frames = Unwind(&mem, modules, GetContext());

  if (frames.empty()) {
    return {};
  }
  // Drop the first frame.
  return {frames.begin() + 1, frames.end()};
}

void UnwindLocalAsync(Memory* local_memory, AsyncMemory::Delegate* delegate,
                      fit::callback<void(std::vector<Frame>)> on_done) {
  auto load_addrs = GetLocalModules();
  std::vector<Module> modules;
  modules.reserve(load_addrs.size());

  for (const auto& addr : load_addrs) {
    modules.emplace_back(addr, local_memory, Module::AddressMode::kProcess);
  }

  constexpr size_t kMaxDepth = 255;

  auto unwinder = std::make_unique<AsyncUnwinder>(delegate, modules);
  unwinder->Unwind(GetContext(), kMaxDepth,
                   [unwinder = std::move(unwinder),
                    on_done = std::move(on_done)](std::vector<Frame> frames) mutable {
                     if (frames.size() <= 2) {
                       on_done({});
                       return;
                     }

                     on_done({frames.begin() + 2, frames.end()});
                   });
}

}  // namespace unwinder
