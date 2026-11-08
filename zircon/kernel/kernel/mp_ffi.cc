// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>
#include <zircon/types.h>

#include <arch/mp.h>
#include <arch/mp_unplug_event.h>
#include <kernel/cpu.h>
#include <kernel/deadline.h>
#include <kernel/ffi.h>
#include <kernel/mp.h>

static_assert(sizeof(mp_ipi_target) == 1, "mp_ipi_target size mismatch");
static_assert(sizeof(mp_ipi) == 1, "mp_ipi size mismatch");
static_assert(static_cast<uint8_t>(mp_ipi_target::MASK) == 0);
static_assert(static_cast<uint8_t>(mp_ipi_target::ALL) == 1);
static_assert(static_cast<uint8_t>(mp_ipi_target::ALL_BUT_LOCAL) == 2);
static_assert(static_cast<uint8_t>(mp_ipi::GENERIC) == 0);
static_assert(static_cast<uint8_t>(mp_ipi::RESCHEDULE) == 1);
static_assert(static_cast<uint8_t>(mp_ipi::INTERRUPT) == 2);
static_assert(static_cast<uint8_t>(mp_ipi::HALT) == 3);

extern "C" {

void cpp_mp_unplug_event_signal(MpUnplugEvent* event);
zx_status_t cpp_mp_unplug_event_unsignal(MpUnplugEvent* event);
zx_status_t cpp_mp_unplug_event_wait_deadline(MpUnplugEvent* event, zx_instant_mono_t deadline,
                                              Interruptible interruptible);

void rust_mp_init();
void rust_mp_reschedule(cpu_mask_t mask, uint32_t flags);
void rust_mp_reschedule_self();
void rust_mp_interrupt(uint8_t target, cpu_mask_t mask);
void rust_mp_sync_exec(uint8_t target, cpu_mask_t mask, mp_sync_task_t task, void* context);
zx_status_t rust_mp_hotplug_cpu_mask(cpu_mask_t mask);
__NO_RETURN void rust_mp_unplug_current_cpu();
zx_status_t rust_mp_unplug_cpu_mask(cpu_mask_t mask, zx_instant_mono_t deadline);
void rust_mp_mbx_reschedule_irq();
void rust_mp_mbx_generic_irq();
void rust_mp_mbx_interrupt_irq();
void rust_mp_set_cpu_online(cpu_num_t cpu, bool online);
cpu_mask_t rust_mp_get_online_mask();
bool rust_mp_is_cpu_online(cpu_num_t cpu);
void rust_mp_signal_curr_cpu_ready();
zx_status_t rust_mp_wait_for_all_cpus_ready(const Deadline* deadline);
zx_status_t rust_platform_mp_cpu_hotplug(cpu_num_t cpu_id);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_mp_unplug_event_signal(MpUnplugEvent* event) {
  DEBUG_ASSERT(event != nullptr);
  event->Signal();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_mp_unplug_event_unsignal(MpUnplugEvent* event) {
  DEBUG_ASSERT(event != nullptr);
  return event->Unsignal();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_mp_unplug_event_wait_deadline(MpUnplugEvent* event,
                                                                zx_instant_mono_t deadline,
                                                                Interruptible interruptible) {
  DEBUG_ASSERT(event != nullptr);
  return event->WaitDeadline(deadline, interruptible);
}

zx_status_t platform_mp_cpu_hotplug(cpu_num_t cpu_id) {
  return rust_platform_mp_cpu_hotplug(cpu_id);
}

}  // extern "C"

void mp_init() { rust_mp_init(); }

void mp_reschedule(cpu_mask_t mask, uint flags) { rust_mp_reschedule(mask, flags); }

void mp_reschedule_self() { rust_mp_reschedule_self(); }

void mp_interrupt(mp_ipi_target target, cpu_mask_t mask) {
  rust_mp_interrupt(static_cast<uint8_t>(target), mask);
}

void mp_sync_exec(mp_ipi_target target, cpu_mask_t mask, mp_sync_task_t task, void* context) {
  DEBUG_ASSERT(task != nullptr);
  rust_mp_sync_exec(static_cast<uint8_t>(target), mask, task, context);
}

zx_status_t mp_hotplug_cpu_mask(cpu_mask_t mask) { return rust_mp_hotplug_cpu_mask(mask); }

__NO_RETURN void mp_unplug_current_cpu() { rust_mp_unplug_current_cpu(); }

zx_status_t mp_unplug_cpu_mask(cpu_mask_t mask, zx_instant_mono_t deadline) {
  return rust_mp_unplug_cpu_mask(mask, deadline);
}

void mp_mbx_reschedule_irq() { rust_mp_mbx_reschedule_irq(); }

void mp_mbx_generic_irq() { rust_mp_mbx_generic_irq(); }

void mp_mbx_interrupt_irq() { rust_mp_mbx_interrupt_irq(); }

void mp_set_cpu_online(cpu_num_t cpu, bool online) { rust_mp_set_cpu_online(cpu, online); }

cpu_mask_t mp_get_online_mask() { return rust_mp_get_online_mask(); }

bool mp_is_cpu_online(cpu_num_t cpu) { return rust_mp_is_cpu_online(cpu); }

void mp_signal_curr_cpu_ready() { rust_mp_signal_curr_cpu_ready(); }

zx_status_t mp_wait_for_all_cpus_ready(Deadline deadline) {
  return rust_mp_wait_for_all_cpus_ready(&deadline);
}
