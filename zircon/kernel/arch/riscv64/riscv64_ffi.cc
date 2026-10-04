// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-options/boot-options.h>
#include <sys/types.h>
#include <zircon/types.h>

#include <arch/debugger.h>
#include <arch/mp.h>
#include <arch/regs.h>
#include <arch/riscv64.h>
#include <arch/riscv64/riscv64_ffi.h>
#include <dev/interrupt.h>
#include <kernel/ffi.h>
#include <kernel/interrupt.h>
#include <kernel/thread.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove when FFI inlining is resolved.
FFI_ALWAYS_INLINE zx_status_t cpp_riscv64_get_general_regs(zx_thread_state_general_regs_t* regs) {
  return arch_get_general_regs(Thread::Current::Get(), regs);
}

// TODO(https://fxbug.dev/537458631): Remove when FFI inlining is resolved.
FFI_ALWAYS_INLINE zx_status_t
cpp_riscv64_set_general_regs(const zx_thread_state_general_regs_t* regs) {
  return arch_set_general_regs(Thread::Current::Get(), regs);
}

zx_status_t cpp_interrupt_send_ipi(cpu_mask_t cpu_mask, uint8_t ipi);
void cpp_interrupt_init_percpu();
void cpp_int_handler_start(uint64_t* state);
uint32_t cpp_int_handler_finish(uint64_t* state);

FFI_ALWAYS_INLINE zx_status_t cpp_interrupt_send_ipi(cpu_mask_t cpu_mask, uint8_t ipi) {
  return interrupt_send_ipi(cpu_mask, static_cast<mp_ipi>(ipi));
}

FFI_ALWAYS_INLINE void cpp_interrupt_init_percpu() { interrupt_init_percpu(); }

FFI_ALWAYS_INLINE void cpp_int_handler_start(uint64_t* state) {
  int_handler_start(reinterpret_cast<int_handler_saved_state_t*>(state));
}

FFI_ALWAYS_INLINE uint32_t cpp_int_handler_finish(uint64_t* state) {
  return int_handler_finish(reinterpret_cast<int_handler_saved_state_t*>(state)) ? 1 : 0;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_riscv64_cpu_mask_to_hart_mask(uint32_t cmask) {
  return riscv64_cpu_mask_to_hart_mask(cmask);
}

void cpp_print_current_thread_backtrace() {
  Backtrace bt;
  Thread::Current::GetBacktrace(bt);
  bt.Print();
}
}  // extern "C"

void ArchIdlePowerThread::EnterIdleState() { arch_enter_idle_state(); }
