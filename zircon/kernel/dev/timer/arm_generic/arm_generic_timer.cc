// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2013, Google Inc. All rights reserved.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/zbi-format/driver-config.h>

#include <dev/timer/arm_generic.h>

#include "arm_generic_timer_ffi.h"

static_assert(sizeof(zbi_dcfg_arm_generic_timer_driver_t) == 16 &&
              alignof(zbi_dcfg_arm_generic_timer_driver_t) == 4);

void ArmGenericTimerInit(const zbi_dcfg_arm_generic_timer_driver_t& config) {
  rust_arm_generic_timer_init(config);
}

void ArmGenericTimerInitPostVm(const zbi_dcfg_arm_generic_timer_driver_t& config) {
  rust_arm_generic_timer_init_post_vm(config);
}

bool ArmUsePhysTimerInVdso() { return rust_arm_use_phys_timer_in_vdso(); }
