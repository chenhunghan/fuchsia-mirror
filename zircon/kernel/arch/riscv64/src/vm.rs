// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::{KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE, USER_ASPACE_BASE, USER_ASPACE_SIZE};
use arch_riscv64_vm_bindings as vm_bindings;

/// Canonical address mask for RISC-V 64 Sv39 virtual addresses; a set bit above
/// bit 37 means the address is outside the user half of the address space.
///
/// [riscv/priv/v1.12]: Section 4.4.1 (Sv39: Page-Based 39-bit Virtual-Memory System)
///
/// Canonical addresses (to use an x86 term) are addresses where the top bits
/// from 63 down to (`RISCV64_VADDR_BITS`-1) are all either 0 or 1.
/// This means user area is [ 0 ... (1<<(`RISCV64_VADDR_BITS`-1)) )
pub(crate) const RISCV64_CANONICAL_ADDRESS_MASK: usize = 0xffff_ffc0_0000_0000;
zr::static_assert!(
    RISCV64_CANONICAL_ADDRESS_MASK == vm_bindings::kRiscv64CanonicalAddressMask as usize
);

// Assert that the kernel aspace #defines lines up with this notion, since it'll
// start at the first available address in the kernel half of the address space and
// extend to the highest 64bit value.
zr::static_assert!(RISCV64_CANONICAL_ADDRESS_MASK == KERNEL_ASPACE_BASE);
zr::static_assert!(!RISCV64_CANONICAL_ADDRESS_MASK == KERNEL_ASPACE_SIZE - 1);

/// Check if a virtual address is in the user-accessible address space.
#[inline]
pub const fn is_user_accessible(va: usize) -> bool {
    // This address refers to userspace if it is in the lower half of the
    // canonical addresses.  IOW - if all of the bits in the canonical address
    // mask are zero.
    (va & RISCV64_CANONICAL_ADDRESS_MASK) == 0
}

/// Check that the continuous range of addresses in `[va, va + len)` are all user-accessible.
#[inline]
pub const fn is_user_accessible_range(va: usize, len: usize) -> bool {
    // Check for normal overflow which implies the range is not continuous.
    let Some(end) = va.checked_add(len) else {
        return false;
    };

    is_user_accessible(va) && (len == 0 || is_user_accessible(end - 1))
}

// Assert that user space also lines up with the canonical mask calculation.
zr::static_assert!(is_user_accessible_range(USER_ASPACE_BASE, USER_ASPACE_SIZE));
zr::static_assert!(!is_user_accessible_range(USER_ASPACE_BASE, USER_ASPACE_SIZE + 1));

/// Returns whether `va` is within the kernel address space.
#[inline]
pub const fn is_kernel_address(va: usize) -> bool {
    va >= KERNEL_ASPACE_BASE && va.wrapping_sub(KERNEL_ASPACE_BASE) < KERNEL_ASPACE_SIZE
}

/// Userspace threads can only set an entry point to userspace addresses, or
/// the null pointer (for testing a thread that will always fail).
#[inline]
pub const fn is_valid_user_pc(pc: usize) -> bool {
    (pc == 0) || (is_user_accessible(pc) && !is_kernel_address(pc))
}
