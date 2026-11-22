// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/asm.h>

// This defines one of the helper functions that the compiler calls for atomic
// read-modify-write operations when building with -moutline-atomics (C/C++) or
// with the `outline-atomics` target feature (Rust):
//
//   iM __aarch64_casM_ORDER(iM expected, iM desired, iM* ptr)  M = 1, 2, 4, 8, 16
//   iN __aarch64_swpN_ORDER(iN value, iN* ptr)                 N = 1, 2, 4, 8
//   iN __aarch64_ldaddN_ORDER(iN value, iN* ptr)
//   iN __aarch64_ldclrN_ORDER(iN value, iN* ptr)
//   iN __aarch64_ldeorN_ORDER(iN value, iN* ptr)
//   iN __aarch64_ldsetN_ORDER(iN value, iN* ptr)
//
// ORDER is one of relax, acq, rel, or acq_rel.  Each function returns the
// value that *ptr held before the operation.  (GCC also calls `sync` versions
// for the __sync_* builtins, but Clang and Rust never do.)
//
// The ABI is the one that compiler-rt and libgcc implement: arguments and
// return values are in the normal argument registers (a 16-byte value takes an
// x0/x1 style pair), the arguments narrower than a register may have garbage
// in the high bits, and nothing is clobbered but the return registers, x15,
// x16, x17, and the condition flags.  (Compilers treat these as normal calls
// that clobber all the caller-saved registers.)
//
// OUTLINE_ATOMIC is defined as `family,size,order` to select the one function
// this file defines.  When compiled with FEAT_LSE enabled, it uses a single
// FEAT_LSE instruction; if OUTLINE_ATOMIC_LSE_A55 is also non-zero, it precedes
// the FEAT_LSE instruction with `prfm pstl1keep` on the target pointer (per the
// Arm Cortex-A55 Software Optimization Guide, section 5.6) to force a near
// atomic in L1 instead of a far atomic in L3 on an L1 miss or shared line.
// Otherwise, it uses a load-exclusive / store-exclusive (LL/SC) loop, which
// works on every ARMv8.0+ CPU.  All three versions are built as code-patching
// alternatives, and physboot patches in one of them.
//
// The LL/SC loops here never make another memory access between the
// load-exclusive and the store-exclusive, as the Arm ARM requires for the loop
// to be guaranteed to make forward progress.  Compiler-generated loops can't
// guarantee that: LLVM expands atomic operations to LL/SC loops in IR, before
// register allocation, so spills and reloads can land inside the loop.

// Define the function for \family (cas, swp, ldadd, ldclr, ldeor, ldset),
// \size in bytes, and \order.
.macro outline_atomic family, size, order
  .ifc \order, relax
    outline_atomic.size \family, \size, __aarch64_\family\size\()_\order
  .else
    .ifc \order, acq
      outline_atomic.size \family, \size, __aarch64_\family\size\()_\order, acq=a
    .else
      .ifc \order, rel
        outline_atomic.size \family, \size, __aarch64_\family\size\()_\order, rel=l
      .else
        .ifc \order, acq_rel
          outline_atomic.size \family, \size, __aarch64_\family\size\()_\order, acq=a, rel=l
        .else
          .error "unsupported order \order"
        .endif
      .endif
    .endif
  .endif
.endm

// Call outline_atomic.\family with the registers and suffixes for \size.
.macro outline_atomic.size family, size, name, acq=, rel=
  .if \size == 1
    outline_atomic.\family \name, w, uxtb, acq=\acq, rel=\rel, sfx=b
  .elseif \size == 2
    outline_atomic.\family \name, w, uxth, acq=\acq, rel=\rel, sfx=h
  .elseif \size == 4
    outline_atomic.\family \name, w, mov, acq=\acq, rel=\rel
  .elseif \size == 8
    outline_atomic.\family \name, x, mov, acq=\acq, rel=\rel
  .elseif \size == 16
    .ifnc \family, cas
      .error "only cas has a 16-byte form"
    .endif
    outline_atomic.casp \name, x, mov, acq=\acq, rel=\rel
  .else
    .error "unsupported size \size"
  .endif
.endm

.macro outline_atomic.return
  ret
  speculation_postfence
.endm

.macro outline_atomic.prefetch reg
#if OUTLINE_ATOMIC_LSE_A55
  prfm pstl1keep, [\reg]
#endif
.endm

// The rest of these macros define the function \name using \r registers (w
// or x), \uxt to zero-extend, \sfx for the b or h mnemonic suffix, and the
// \acq and \rel infixes: `a` for acquire and `l` for release semantics.

// iN cas(iN expected, iN desired, iN* ptr)
.macro outline_atomic.cas name, r, uxt, acq=, rel=, sfx=
  .function \name, global
#if __ARM_FEATURE_ATOMICS
    outline_atomic.prefetch x2
    cas\acq\rel\sfx \r\()0, \r\()1, [x2]
#else
    // The high bits of expected may be garbage.
    \uxt \r\()16, \r\()0
  1:
    ld\acq\()xr\sfx \r\()0, [x2]
    cmp \r\()0, \r\()16
    b.ne 2f
    st\rel\()xr\sfx w17, \r\()1, [x2]
    cbnz w17, 1b
    outline_atomic.return
  2:
    clrex
#endif
    outline_atomic.return
  .end_function
.endm

// i128 cas(i128 expected, i128 desired, i128* ptr)
.macro outline_atomic.casp name, r, uxt, acq=, rel=, sfx=
  .function \name, global
#if __ARM_FEATURE_ATOMICS
    outline_atomic.prefetch x4
    casp\acq\rel x0, x1, x2, x3, [x4]
#else
    mov x16, x0
    mov x17, x1
  1:
    ld\acq\()xp x0, x1, [x4]
    cmp x0, x16
    ccmp x1, x17, #0, eq
    b.ne 2f
    st\rel\()xp w15, x2, x3, [x4]
    cbnz w15, 1b
    outline_atomic.return
  2:
    // A 16-byte load-exclusive is only single-copy atomic when it's paired
    // with a successful store-exclusive.  So store back the value just loaded
    // so the value returned is known to be one that was really in memory.
    // LLVM does the same in its own expansion of a 16-byte compare-and-swap.
    st\rel\()xp w15, x0, x1, [x4]
    cbnz w15, 1b
#endif
    outline_atomic.return
  .end_function
.endm

// iN swp(iN value, iN* ptr)
.macro outline_atomic.swp name, r, uxt, acq=, rel=, sfx=
  .function \name, global
#if __ARM_FEATURE_ATOMICS
    outline_atomic.prefetch x1
    swp\acq\rel\sfx \r\()0, \r\()0, [x1]
#else
    mov \r\()16, \r\()0
  1:
    ld\acq\()xr\sfx \r\()0, [x1]
    st\rel\()xr\sfx w17, \r\()16, [x1]
    cbnz w17, 1b
#endif
    outline_atomic.return
  .end_function
.endm

// iN op(iN value, iN* ptr), using the \lse_op instruction or \alu_op in a loop.
.macro outline_atomic.ldop lse_op, alu_op, name, r, uxt, acq=, rel=, sfx=
  .function \name, global
#if __ARM_FEATURE_ATOMICS
    outline_atomic.prefetch x1
    \lse_op\acq\rel\sfx \r\()0, \r\()0, [x1]
#else
    mov \r\()16, \r\()0
  1:
    ld\acq\()xr\sfx \r\()0, [x1]
    \alu_op \r\()17, \r\()0, \r\()16
    st\rel\()xr\sfx w15, \r\()17, [x1]
    cbnz w15, 1b
#endif
    outline_atomic.return
  .end_function
.endm

.macro outline_atomic.ldadd args:vararg
  outline_atomic.ldop ldadd, add, \args
.endm

// ldclr clears the bits set in value: *ptr & ~value.
.macro outline_atomic.ldclr args:vararg
  outline_atomic.ldop ldclr, bic, \args
.endm

.macro outline_atomic.ldeor args:vararg
  outline_atomic.ldop ldeor, eor, \args
.endm

.macro outline_atomic.ldset args:vararg
  outline_atomic.ldop ldset, orr, \args
.endm

outline_atomic OUTLINE_ATOMIC
