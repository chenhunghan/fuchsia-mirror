// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_TIMER_ARM_GENERIC_ARM_GENERIC_TIMER_FFI_H_
#define ZIRCON_KERNEL_DEV_TIMER_ARM_GENERIC_ARM_GENERIC_TIMER_FFI_H_

#include <lib/zbi-format/driver-config.h>
#include <zircon/compiler.h>

__BEGIN_CDECLS

void rust_arm_generic_timer_init(const zbi_dcfg_arm_generic_timer_driver_t& config);
void rust_arm_generic_timer_init_post_vm(const zbi_dcfg_arm_generic_timer_driver_t& config);
bool rust_arm_use_phys_timer_in_vdso();

__END_CDECLS

#endif  // ZIRCON_KERNEL_DEV_TIMER_ARM_GENERIC_ARM_GENERIC_TIMER_FFI_H_
