// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "kernel/scheduler.h"

#include <assert.h>
#include <debug.h>
#include <inttypes.h>
#include <lib/zircon-internal/macros.h>
#include <platform.h>
#include <stdio.h>
#include <string.h>
#include <zircon/errors.h>
#include <zircon/time.h>
#include <zircon/types.h>

#include <arch/interrupt.h>
#include <arch/ops.h>
#include <kernel/cpu.h>
#include <kernel/ffi.h>
#include <kernel/lockdep.h>
#include <kernel/mp.h>
#include <kernel/percpu.h>
#include <kernel/scheduler_state.h>
#include <kernel/thread.h>
#include <ktl/utility.h>

#include <ktl/enforce.h>

Scheduler* Scheduler::Get() { return Get(arch_curr_cpu_num()); }

Scheduler* Scheduler::Get(cpu_num_t cpu) { return &percpu::Get(cpu).scheduler; }

void Scheduler::SetPowerDomainSet(PowerDomainSet power_domain_set) {
  percpu::ForEach([&power_domain_set](cpu_num_t cpu_num, percpu* percpu) {
    percpu->scheduler.ExchangePowerDomainSet(power_domain_set);
  });
}

zx::result<cpu_mask_t> Scheduler::UpdatePowerLevel(uint32_t domain_id, uint64_t controller_id,
                                                   power_management::ControlInterface interface,
                                                   uint64_t arg) {
  // Get a ref to the power domain using the power domain set for the current
  // CPU.
  fbl::RefPtr<PowerDomain> domain;
  {
    Scheduler* scheduler = Get();
    Guard<MonitoredSpinLock, IrqSave> guard{&scheduler->queue_lock_, SOURCE_TAG};
    const PowerDomainSet& power_domain_set = scheduler->rust_tail_.power_state.power_domain_set();
    domain = fbl::RefPtr{power_domain_set.FindByDomainId(domain_id)};
  }

  if (!domain) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  if (domain->controller()->id() != controller_id ||
      domain->controller()->control_interface() != interface) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  ktl::optional<uint8_t> power_level = domain->model().FindActivePowerLevel(interface, arg);
  if (!power_level.has_value()) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  cpu_mask_t domain_cpus = domain->cpus();
  cpu_mask_t cpus_to_reschedule_mask = 0;
  while (domain_cpus != 0) {
    const cpu_num_t cpu_num = remove_cpu_from_mask(domain_cpus);
    Scheduler* scheduler = Get(cpu_num);
    if (scheduler->UpdateActivePowerLevel(*power_level).is_ok()) {
      cpus_to_reschedule_mask |= cpu_num_to_mask(cpu_num);
    }
  }

  return zx::ok(cpus_to_reschedule_mask);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_lock_queue(Scheduler* sched) TA_NO_THREAD_SAFETY_ANALYSIS {
  sched->queue_lock_.lock().Acquire(SOURCE_TAG);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_unlock_queue(Scheduler* sched) TA_NO_THREAD_SAFETY_ANALYSIS {
  sched->queue_lock_.lock().Release();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_scheduler_lock_queue_irq_save(Scheduler* sched)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  interrupt_saved_state_t state;
  sched->queue_lock_.lock().AcquireIrqSave(state, SOURCE_TAG);
  return static_cast<uint64_t>(state);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_scheduler_unlock_queue_irq_restore(Scheduler* sched, uint64_t state)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  sched->queue_lock_.lock().ReleaseIrqRestore(static_cast<interrupt_saved_state_t>(state));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
// Posts |req| to the power domain controller of |sched|, temporarily releasing
// queue_lock_ around the post (which may acquire other CPUs' queue locks).
// Must be called with queue_lock_ held and interrupts disabled; queue_lock_ is
// re-acquired before returning.
FFI_ALWAYS_INLINE cpu_mask_t cpp_scheduler_power_state_post_update_request(
    Scheduler* sched,
    const power_management::PowerLevelUpdateRequest* req) TA_NO_THREAD_SAFETY_ANALYSIS {
  // Latch a reference to the domain's controller while queue_lock_ is still
  // held. power_state is guarded by queue_lock_, and a concurrent
  // ExchangePowerDomainSet on another CPU could otherwise replace or clear the
  // domain while the lock is dropped below. Only the controller is needed to
  // post the request.
  fbl::RefPtr<power_management::PowerLevelController> controller =
      sched->rust_tail_.power_state.domain()->controller();
  DEBUG_ASSERT(controller != nullptr);

  cpu_mask_t cpus_to_reschedule_mask = 0;
  sched->queue_lock_.lock().Release();
  {
    const zx::result<cpu_mask_t> result = controller->Post(*req);
    if (result.is_ok()) {
      cpus_to_reschedule_mask = result.value();
    } else {
      KERNEL_OOPS("Failed to set OPP: domain_id=%" PRIu32 " control_argument=%" PRIu64 ": %d\n",
                  req->domain_id, req->control_argument, result.status_value());
    }
    // Drop the reference before re-acquiring queue_lock_: if this is the last
    // reference, destruction may take the heap lock, which must not happen
    // while holding a spinlock (queue_lock_ is a leaf lock). This also keeps
    // the atomic refcount decrement out of the critical section.
    controller.reset();
  }
  sched->queue_lock_.lock().Acquire(SOURCE_TAG);
  return cpus_to_reschedule_mask;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_current_mono_time_and_boot_ticks(
    Scheduler::SchedMonoTimeAndBootTicks* out) {
  *out = Scheduler::CurrentMonoTimeAndBootTicks();
}
