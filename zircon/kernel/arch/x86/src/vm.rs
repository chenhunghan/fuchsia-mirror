// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::{KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE};
use arch_x86_vm_bindings as vm_bindings;

/// The definition of an x86/64 "canonical" address depends on the number of
/// total meaningful virtual address bits. The canonical address range is
/// divided into two regions; the first where all of the high address bits are 0,
/// and the second where all of the high address bits are 1. IOW - if the number
/// of meaningful address bits is N, then the bits [N - 1, 63] must all be either
/// 1 or 0. The lower N - 1 bits may be whatever they want to be.
///
/// Precompute the mask for the [N - 1, 63] range so we can easily test to see
/// if an address might be a user-mode address
/// (`(addr & X86_CANONICAL_ADDRESS_MASK) == 0`), or might be a kernel address
/// (`(addr & X86_CANONICAL_ADDRESS_MASK) == X86_CANONICAL_ADDRESS_MASK`).
pub(crate) const X86_CANONICAL_ADDRESS_MASK: usize = 0xffff_8000_0000_0000;
zr::static_assert!(X86_CANONICAL_ADDRESS_MASK == vm_bindings::kX86CanonicalAddressMask as usize);

/// Checks if a virtual address is accessible to user mode on x86_64.
///
/// This address refers to userspace if it is in the lower half of the
/// canonical addresses (i.e., if all of the bits in the canonical address
/// mask are zero).
#[inline]
pub const fn is_user_accessible(va: usize) -> bool {
    // See [intel/vol1]: 3.3.7.1 Canonical Addressing, and
    // [amd/vol1]: 2.1.3 Canonical Address Form.
    (va & X86_CANONICAL_ADDRESS_MASK) == 0
}

/// Check that the continuous range of addresses in `[va, va+len)` are all
/// accessible to the user.
#[inline]
pub const fn is_user_accessible_range(va: usize, len: usize) -> bool {
    // Check for normal overflow which implies the range is not continuous.
    let Some(end) = va.checked_add(len) else {
        return false;
    };

    is_user_accessible(va) && (len == 0 || is_user_accessible(end - 1))
}

/// Checks if a virtual address is in canonical form on x86_64.
///
/// An address is canonical if bits [N - 1, 63] are all either 0 (the low half of
/// canonical addresses) or all 1 (the high half of canonical addresses).
#[inline]
pub const fn is_vaddr_canonical(va: usize) -> bool {
    // See [intel/vol1]: 3.3.7.1 Canonical Addressing, and
    // [amd/vol1]: 2.1.3 Canonical Address Form.
    //
    // If N is the number of address bits in use for a virtual address, then the
    // address is canonical if bits [N - 1, 63] are all either 0 (the low half of
    // the valid addresses) or 1 (the high half).
    ((va & X86_CANONICAL_ADDRESS_MASK) == 0)
        || ((va & X86_CANONICAL_ADDRESS_MASK) == X86_CANONICAL_ADDRESS_MASK)
}

/// Returns whether `va` is within the kernel address space.
#[inline]
pub const fn is_kernel_address(va: usize) -> bool {
    va >= KERNEL_ASPACE_BASE && va.wrapping_sub(KERNEL_ASPACE_BASE) < KERNEL_ASPACE_SIZE
}

/// Userspace threads can only set an entry point to userspace addresses, or
/// the null pointer (for testing a thread that will always fail).
///
/// See docs/concepts/kernel/sysret_problem.md for more details.
#[inline]
pub const fn is_valid_user_pc(pc: usize) -> bool {
    (pc == 0) || (is_user_accessible(pc) && is_vaddr_canonical(pc))
}
