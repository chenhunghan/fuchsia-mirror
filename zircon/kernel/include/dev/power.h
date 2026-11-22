// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_DEV_POWER_H_
#define ZIRCON_KERNEL_INCLUDE_DEV_POWER_H_

#include <stdint.h>
#include <sys/types.h>
#include <zircon/types.h>

enum class power_reboot_flags {
  REBOOT_NORMAL = 0,
  REBOOT_BOOTLOADER = 1,
  REBOOT_RECOVERY = 2,
  REBOOT_PANIC = 3,
};

// power_cpu_state represents the state a CPU in, and contains a union of the states found in all
// architectures.
enum class power_cpu_state {
  ON,
  OFF,
  ON_PENDING,
  STARTED,
  STOPPED,
  START_PENDING,
  STOP_PENDING,
  SUSPENDED,
  SUSPEND_PENDING,
  RESUME_PENDING,
};

static_assert(sizeof(power_reboot_flags) == 4, "power_reboot_flags size mismatch");
static_assert(alignof(power_reboot_flags) == 4, "power_reboot_flags align mismatch");
static_assert(sizeof(power_cpu_state) == 4, "power_cpu_state size mismatch");
static_assert(alignof(power_cpu_state) == 4, "power_cpu_state align mismatch");

// Initiates the power on sequence for the CPU with the given hardware CPU ID. Note that a hardware
// CPU ID is architecture specific (e.g. MPID on ARM, hart ID on RISC-V, etc.) and not equivalent to
// a cpu_num_t. This function does not block/wait on the CPU actually coming online.
//
// On some architectures, the context argument is passed to the newly started CPU.
zx_status_t power_cpu_on(uint64_t hw_cpu_id, paddr_t entry, uint64_t context);

#endif  // ZIRCON_KERNEL_INCLUDE_DEV_POWER_H_
