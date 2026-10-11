// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-options/boot-options.h>
#include <sys/types.h>
#include <zircon/types.h>

#include <arch/mp.h>
#include <arch/regs.h>
#include <arch/riscv64.h>
#include <arch/riscv64/riscv64_ffi.h>
#include <dev/interrupt.h>
#include <kernel/ffi.h>
#include <kernel/thread.h>

extern "C" {

zx_status_t cpp_interrupt_send_ipi(cpu_mask_t cpu_mask, uint8_t ipi);
void cpp_interrupt_init_percpu();

FFI_ALWAYS_INLINE zx_status_t cpp_interrupt_send_ipi(cpu_mask_t cpu_mask, uint8_t ipi) {
  return interrupt_send_ipi(cpu_mask, static_cast<mp_ipi>(ipi));
}

FFI_ALWAYS_INLINE void cpp_interrupt_init_percpu() { interrupt_init_percpu(); }

void cpp_print_current_thread_backtrace() {
  Backtrace bt;
  Thread::Current::GetBacktrace(bt);
  bt.Print();
}
}  // extern "C"

void ArchIdlePowerThread::EnterIdleState() { arch_enter_idle_state(); }
