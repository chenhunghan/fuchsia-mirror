// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_ARM64_FFI_H_
#define ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_ARM64_FFI_H_

#include <stdint.h>

struct iframe_t;

extern "C" {

// Implemented in Rust (arch.rs); backs ArchIdlePowerThread::EnterIdleState().
void arch_enter_idle_state();

void cpp_arm64_get_tpidr_regs(uint64_t* tpidr_el0, uint64_t* tpidrro_el0);
void cpp_arm64_set_tpidr_regs(uint64_t tpidr_el0, uint64_t tpidrro_el0);
void cpp_arm64_enter_restricted_tpidr(uint64_t tpidr_el0, bool is_arm32);
uint64_t cpp_arm64_get_tpidr_el0();
void cpp_arm64_print_frame(const iframe_t* frame);
void cpp_arm64_set_crashlog_regs(iframe_t* iframe, uint32_t esr, uint64_t far, uint64_t tpidr_el0,
                                 uint64_t tpidr_el1, uint64_t tpidrro_el0);

}  // extern "C"

#endif  // ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_ARM64_FFI_H_
