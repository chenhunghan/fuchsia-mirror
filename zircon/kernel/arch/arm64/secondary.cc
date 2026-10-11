// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014-2016 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>
#include <stddef.h>
#include <stdint.h>
#include <zircon/errors.h>
#include <zircon/tls.h>
#include <zircon/types.h>

#include <arch/arm64.h>
#include <arch/arm64/feature.h>
#include <arch/mp.h>
#include <arch/ops.h>
#include <kernel/cpu.h>
#include <kernel/thread.h>
#include <lk/init.h>
#include <lk/main.h>
#include <vm/kstack.h>

#include <ktl/enforce.h>

namespace {

// one for each secondary CPU, indexed by (cpu_num - 1).
Thread _init_thread[SMP_MAX_CPUS - 1];

}  // anonymous namespace

// Structure used to pass information to a newly booted secondary cpu.
//
// SP will be set to the bottom of this structure.
struct arm64_sp_info {
  uintptr_t* shadow_call_sp;  // SCS pointer points to array of addresses.
  uint64_t pad;               // Pad so that its size is a multiple of 16.

  // This part of the struct itself will serve temporarily as the
  // fake arch_thread in the thread pointer, so that safe-stack
  // and stack-protector code can work early.  The thread pointer
  // (TPIDR_EL1) points just past arm64_sp_info_t.
  uintptr_t stack_guard;
  void* unsafe_sp = nullptr;  // Never actually used in the kernel.
};

static_assert(sizeof(arm64_sp_info) == 32, "check arm64_secondary_start assembly");
static_assert(offsetof(arm64_sp_info, shadow_call_sp) == 0, "check arm64_secondary_start assembly");
static_assert(sizeof(arm64_sp_info) % 16 == 0);

#define TP_OFFSET(field) ((int)offsetof(arm64_sp_info, field) - (int)sizeof(arm64_sp_info))
static_assert(TP_OFFSET(stack_guard) == ZX_TLS_STACK_GUARD_OFFSET);
static_assert(TP_OFFSET(unsafe_sp) == ZX_TLS_UNSAFE_SP_OFFSET);
#undef TP_OFFSET

zx_status_t arm64_create_secondary_stack(cpu_num_t cpu_num, uintptr_t* out) {
  // Allocate a stack for the init thread of this particular secondary cpu.
  DEBUG_ASSERT_MSG(cpu_num > 0 && cpu_num < SMP_MAX_CPUS, "cpu_num: %u", cpu_num);
  KernelStack* stack = &_init_thread[cpu_num - 1].stack();
  DEBUG_ASSERT(stack->base() == 0);
  zx_status_t status = stack->Init();
  if (status != ZX_OK) {
    return status;
  }

  // Get the stack pointers.
  uintptr_t sp = static_cast<uintptr_t>(stack->top());
  DEBUG_ASSERT(sp % 16 == 0);
  uintptr_t* shadow_call_sp = nullptr;
#if __has_feature(shadow_call_stack)
  DEBUG_ASSERT(stack->shadow_call_base() != 0);
  // The shadow call stack grows up.
  shadow_call_sp = reinterpret_cast<uintptr_t*>(stack->shadow_call_base());
#endif

  // Place the secondary bootstrap structure at the top of the stack.
  arm64_sp_info* cpu = reinterpret_cast<arm64_sp_info*>(sp);
  cpu--;

  // Store the necessary cpu boot info.
  cpu->stack_guard = Thread::Current::Get()->arch().stack_guard;
  cpu->shadow_call_sp = shadow_call_sp;

  *out = reinterpret_cast<uintptr_t>(cpu);
  return ZX_OK;
}

zx_status_t arm64_free_secondary_stack(cpu_num_t cpu_num) {
  DEBUG_ASSERT(cpu_num > 0 && cpu_num < SMP_MAX_CPUS);
  return _init_thread[cpu_num - 1].stack().Teardown();
}

// called from assembly.
extern "C" void arm64_secondary_entry();

extern "C" void arm64_secondary_entry() {
  arm64_cpu_early_init();

  cpu_num_t cpu = arch_curr_cpu_num();
  _init_thread[cpu - 1].SecondaryCpuInitEarly();
  // Run early secondary cpu init routines up to the threading level.
  lk_init_level(LK_INIT_FLAG_SECONDARY_CPUS, LK_INIT_LEVEL_EARLIEST, LK_INIT_LEVEL_THREADING - 1);

  arch_mp_init_percpu();

  const bool full_dump = arm64_feature_current_is_first_in_cluster();
  arm64_feature_debug(full_dump);

  lk_secondary_cpu_entry();
}
