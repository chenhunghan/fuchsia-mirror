// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_H_
#define ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_H_

#ifndef __ASSEMBLER__

#include <lib/arch/intrin.h>
#include <zircon/types.h>

struct iframe_t;

struct arch_exception_context {
  struct iframe_t* frame;
  uint64_t far;
  uint32_t esr;
  // The |user_synth_code| and |user_synth_data| fields have different values depending on the
  // exception type.
  //
  // 1) For ZX_EXCP_POLICY_ERROR, |user_synth_code| contains the type of the policy error (a
  // ZX_EXCP_POLICY_CODE_* value), and |user_synth_data| contains additional information relevant to
  // the policy error (e.g. the syscall number for ZX_EXCP_POLICY_CODE_BAD_SYSCALL).
  //
  // 2) For ZX_EXCP_FATAL_PAGE_FAULT, |user_synth_code| contains the |zx_status_t| error code
  // returned by the page fault handler, typecast to |uint32_t|. |user_synth_data| is 0.
  //
  // 3) For all other exception types, |user_synth_code| and |user_synth_data| are both set to 0.
  uint32_t user_synth_code;
  uint32_t user_synth_data;
};

inline uint64_t arm64_get_boot_el() { return __arm_rsr64("CurrentEL") >> 2; }

#endif  // __ASSEMBLER__

// Used in the exception_flags arguments of arm64_sync_exception, arm64_irq and
// arm64_serror_exception.
#define ARM64_EXCEPTION_FLAG_LOWER_EL (1 << 0)

// Used in the which argument of arm64_unrestricted_arm32_exception.
#define ARM64_DISALLOWED_ARM32_SYSCALL (1 << 0)
#define ARM64_DISALLOWED_ARM32_SYNC_EXCEPTION (1 << 1)
#define ARM64_DISALLOWED_ARM32_ASYNC_EXCEPTION (1 << 2)

#endif  // ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ARM64_H_
