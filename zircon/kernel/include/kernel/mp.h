// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_MP_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_MP_H_

#include <stdint.h>
#include <zircon/types.h>

#include <kernel/cpu.h>

using mp_sync_task_t = void (*)(void* context);

enum class mp_ipi : uint8_t { GENERIC, RESCHEDULE, INTERRUPT, HALT };

// When sending inter processor interrupts (IPIs), apis will take a combination of
// this enum and a bitmask. If mp_ipi_target::MASK is used, the mask argument will
// contain a bitmap of every cpu that should receive the IPI. The other targets
// serve as shortcuts and potentially optimizations in the lower layers.
enum class mp_ipi_target : uint8_t { MASK, ALL, ALL_BUT_LOCAL };

// Trigger an interrupt on another cpu without a corresponding reschedule.
// Used by the hypervisor to trigger a vmexit.
void mp_interrupt(mp_ipi_target, cpu_mask_t mask);

// Make a cross cpu call to one or more cpus. Waits for all of the calls
// to complete before returning.
void mp_sync_exec(mp_ipi_target, cpu_mask_t mask, mp_sync_task_t task, void* context);

zx_status_t mp_hotplug_cpu_mask(cpu_mask_t mask);

// Unplug the cpu specified by |mask|, waiting, up to |deadline| for its "shutdown" thread to
// complete.
zx_status_t mp_unplug_cpu_mask(cpu_mask_t mask, zx_instant_mono_t deadline);

cpu_mask_t mp_get_online_mask();

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_MP_H_
