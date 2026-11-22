// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_TIMER_ARMV7_MMIO_TIMER_INCLUDE_DEV_TIMER_ARMV7_MMIO_TIMER_H_
#define ZIRCON_KERNEL_DEV_TIMER_ARMV7_MMIO_TIMER_INCLUDE_DEV_TIMER_ARMV7_MMIO_TIMER_H_

#include <lib/zbi-format/driver-config.h>
#include <zircon/compiler.h>

__BEGIN_CDECLS

// Initializes all ARMv7 MMIO timers described by `config`.
void Armv7MmioTimerInit(const zbi_dcfg_arm_generic_timer_mmio_driver_t& config);

__END_CDECLS

#endif  // ZIRCON_KERNEL_DEV_TIMER_ARMV7_MMIO_TIMER_INCLUDE_DEV_TIMER_ARMV7_MMIO_TIMER_H_
