// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_VM_H_
#define ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_VM_H_

#include <stdint.h>
#include <sys/types.h>

#include <arch/kernel_aspace.h>

static inline bool is_kernel_address(vaddr_t va) {
  return (va >= (vaddr_t)KERNEL_ASPACE_BASE &&
          va - (vaddr_t)KERNEL_ASPACE_BASE < (vaddr_t)KERNEL_ASPACE_SIZE);
}

static inline constexpr uint8_t kHighVABit = 55;
static inline constexpr uint64_t kUserBitMask = UINT64_C(1) << kHighVABit;
static inline constexpr uint64_t kTbiBit = 56;
static inline constexpr uint64_t kTbiMask = ~((UINT64_C(1) << kTbiBit) - 1);

// This address refers to userspace if bit 55 is zero.
static inline bool is_user_accessible(vaddr_t va) { return (va & kUserBitMask) == 0; }

#endif  // ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_VM_H_
