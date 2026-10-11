// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_RISCV64_INCLUDE_ARCH_RISCV64_RISCV64_FFI_H_
#define ZIRCON_KERNEL_ARCH_RISCV64_INCLUDE_ARCH_RISCV64_RISCV64_FFI_H_

#include <stdint.h>
#include <sys/types.h>

extern "C" {

// Implemented in Rust (arch.rs); backs ArchIdlePowerThread::EnterIdleState().
void arch_enter_idle_state();

void cpp_print_current_thread_backtrace();

}  // extern "C"

#endif  // ZIRCON_KERNEL_ARCH_RISCV64_INCLUDE_ARCH_RISCV64_RISCV64_FFI_H_
