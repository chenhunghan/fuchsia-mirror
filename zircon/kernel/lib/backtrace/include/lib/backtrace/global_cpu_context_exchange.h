// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_BACKTRACE_INCLUDE_LIB_BACKTRACE_GLOBAL_CPU_CONTEXT_EXCHANGE_H_
#define ZIRCON_KERNEL_LIB_BACKTRACE_INCLUDE_LIB_BACKTRACE_GLOBAL_CPU_CONTEXT_EXCHANGE_H_

#include <lib/backtrace/cpu_context_exchange.h>
#include <stdint.h>
#include <zircon/types.h>

#include <arch/x86/apic.h>
#include <arch/x86/interrupts.h>
#include <kernel/cpu.h>

#if defined __x86_64__

// TODO(maniscalco): Find a new home for the declaration and definition. Needs to be visible to
// both backtrace_ffi.cc and cpu_context_exchange_tests.cc.

static void SendNmiIpi(cpu_num_t target_cpu) {
  DEBUG_ASSERT(is_valid_cpu_num(target_cpu));
  apic_send_mask_ipi(X86_INT_NMI, cpu_num_to_mask(target_cpu), DELIVERY_MODE_NMI);
}
inline CpuContextExchange g_cpu_context_exchange(&SendNmiIpi);

extern "C" {
zx_status_t cpp_global_cpu_context_exchange_request_context(cpu_num_t target_cpu,
                                                            zx_duration_mono_t timeout,
                                                            CpuContext* context);
void cpp_global_cpu_context_exchange_handle_request(uintptr_t fp, const struct iframe_t* frame);
}  // extern "C"

#endif  // __x86_64__

#endif  // ZIRCON_KERNEL_LIB_BACKTRACE_INCLUDE_LIB_BACKTRACE_GLOBAL_CPU_CONTEXT_EXCHANGE_H_
