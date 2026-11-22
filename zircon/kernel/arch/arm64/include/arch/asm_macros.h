// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ASM_MACROS_H_
#define ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ASM_MACROS_H_

// clang-format off

// GAS doesn't support xzr in .cfi directives.

.macro push_regs ra, rb
stp \ra, \rb, [sp,#-16]!
.cfi_adjust_cfa_offset 16
.ifnes "\ra", "xzr"
.cfi_rel_offset \ra, 0
.endif
.ifnes "\rb", "xzr"
.cfi_rel_offset \rb, 8
.endif
.endm

.macro pop_regs ra, rb
ldp \ra, \rb, [sp], #16
.cfi_adjust_cfa_offset -16
.ifnes "\ra", "xzr"
.cfi_same_value \ra
.endif
.ifnes "\rb", "xzr"
.cfi_same_value \rb
.endif
.endm

.macro sub_from_sp value
sub sp, sp, #\value
.cfi_adjust_cfa_offset \value
.endm

.macro add_to_sp value
add sp, sp, #\value
.cfi_adjust_cfa_offset -\value
.endm

#define SPECULATION_POSTFENCE \
    dsb nsh; \
    isb

#endif  // ZIRCON_KERNEL_ARCH_ARM64_INCLUDE_ARCH_ASM_MACROS_H_
