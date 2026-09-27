// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/asm.h>
#include <arch/riscv64.h>
#include <zircon/errors.h>

// Riscv64UserCopyRet _riscv64_user_copy(void *dst, const void *src, size_t len, uint64_t *fault_return, uint64_t capture_faults_mask)
// Align hot code to cache line.
.function _riscv64_user_copy, global, align=64
  .llvm.stack_size 0

  // Allow supervisor accesses to user memory by setting the SUM bit
  li a5, RISCV64_CSR_SSTATUS_SUM
  csrs sstatus, a5

  // Set *fault_return to fault_from_user orred with capture_faults_mask
  // to signal to the exception handler when to run the routine
  lla a5, .Lfault_from_user
  or a5, a5, a4
  sd a5, (a3)

  // Short-circuit for zero-length.  TODO: impossible at higher level?
  beqz a2, .Luser_copy_done

  // If len < 8, skip straight to the byte-by-byte tail loop.
  li a4, 8
  bltu a2, a4, .Luser_copy_tail

  // If the dst and src alignments aren't congruent modulo word size, then just
  // copy byte-by-byte rather than doing aligned word stores after unaligned
  // word loads or vice versa.
  xor a4, a0, a1
  and a4, a4, 7
  bnez a4, .Luser_copy_tail

  // Copy single bytes until dst is word-aligned.
.Luser_copy_leading_bytes:
  and a4, a0, 7
  beqz a4, .Luser_copy_words
  lbu a4, (a1)
  sb a4, (a0)
  add a0, a0, 1
  add a1, a1, 1
  add a2, a2, -1
  // a2 can never reach zero from >= 8 before a0 reaches 0 mod 8.
  j .Luser_copy_leading_bytes

  // Copy aligned whole words.
.Luser_copy_words:
  // Bulk copy multiples of 4 words with the unrolled loop.
  li t0, 4*8
  bltu a2, t0, .Luser_copy_single_words

.Luser_copy_4words:
  ld a4, 0*8(a1)
  ld a5, 1*8(a1)
  sd a4, 0*8(a0)
  sd a5, 1*8(a0)
  ld a4, 2*8(a1)
  ld a5, 3*8(a1)
  sd a4, 2*8(a0)
  sd a5, 3*8(a0)

  add a1, a1, 4*8
  add a0, a0, 4*8
  add a2, a2, -4*8
  bgeu a2, t0, .Luser_copy_4words

  // Copy remaining whole words individually.
.Luser_copy_single_words:
  li a4, 8
  bltu a2, a4, .Luser_copy_tail
0:ld a5, (a1)
  add a2, a2, -8
  add a1, a1, 8
  sd a5, (a0)
  add a0, a0, 8
  bgeu a2, a4, 0b

  // Copy remaining bytes individually.
.Luser_copy_tail:
  beqz a2, .Luser_copy_done
0:lbu a4, (a1)
  add a2, a2, -1
  add a1, a1, 1
  sb a4, (a0)
  add a0, a0, 1
  bnez a2, 0b

.Luser_copy_done:
  // Store a successful status for the return.
  // Zero out the top word of the structure to be clean about it.
  li a0, ZX_OK
  li a1, 0

.Luser_copy_return:
  // Restore *fault_return.
  sd zero, (a3)

  // Disable supervisor accesses to user memory
  li a5, RISCV64_CSR_SSTATUS_SUM
  csrc sstatus, a5

  ret

// If we are capturing faults the exception handler will have placed the
// faulting virtual address for us in a1 and the flags in a2. We do not touch
// a1 and rely on the caller to know if the value is meaningful based on
// whether it specified fault capture or not, we just need to construct a valid
// a0 before jmping to user_copy_return.
.Lfault_from_user:
  li a0, (ZX_ERR_INVALID_ARGS & 0xffffffff)

  // If we are capturing faults the flags will have been placed in a2 and we
  // want them placed in the high bits of a0. If not capturing faults then we
  // will copy some garbage bits which will be ignored by the caller.
  slli a2, a2, 32
  or a0, a0, a2
  j .Luser_copy_return

.end_function
