// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_REGISTERS_H_
#define ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_REGISTERS_H_

#include <stdint.h>
#include <zircon/hw/debug/arm64.h>

// MDSCR_EL1
// Monitor Debug System Control Register. It's the main control register fot the debug
// implementation.

// Initial value for MSDCR_EL1 when starting userspace, which disables all debug exceptions.
// Instruction Breakpoint Exceptions (software breakpoints) cannot be disabled and MDSCR does not
// affect single-step behaviour.
inline constexpr uint32_t MSDCR_EL1_INITIAL_VALUE = 0;

/* Kernel tracking of the current state of the debug registers for a particular thread.
 * ARMv8 can have from 2 to 16 HW breakpoints and 2 to 16 HW watchpoints.
 *
 * This struct can potentially hold all of them. If the platform has fewer of those
 * breakpoints available, it will fill from the lower index up to correct amount.
 * The other indices should never be accessed. */
typedef struct arm64_debug_state {
  struct {
    uint32_t dbgbcr;
    uint64_t dbgbvr;
  } hw_bps[ARM64_MAX_HW_BREAKPOINTS];
  struct {
    uint32_t dbgwcr;
    uint64_t dbgwvr;
  } hw_wps[ARM64_MAX_HW_WATCHPOINTS];
  uint64_t far;
  uint32_t esr;
} arm64_debug_state_t;

#endif  // ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_REGISTERS_H_
