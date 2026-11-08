// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! VMX TLB invalidation instructions (INVVPID and INVEPT).

use core::arch::asm;
use zx_types::{zx_paddr_t, zx_vaddr_t};

/// Write back memory type, as reported in `X86_MSR_IA32_VMX_BASIC` and used in the EPTP.
///
/// Must match `VMX_MEMORY_TYPE_WRITE_BACK` in `vmx_cpu_state_priv.h`.
const VMX_MEMORY_TYPE_WRITE_BACK: u64 = 0x06;

/// INVVPID invalidation types.
///
/// From Volume 3, Section 30.3: There are four INVVPID types currently defined:
/// * Individual-address invalidation: If the INVVPID type is 0, the logical
///   processor invalidates mappings for the linear address and VPID specified in
///   the INVVPID descriptor. In some cases, it may invalidate mappings for other
///   linear addresses (or other VPIDs) as well.
/// * Single-context invalidation: If the INVVPID type is 1, the logical
///   processor invalidates all mappings tagged with the VPID specified in the
///   INVVPID descriptor. In some cases, it may invalidate mappings for other
///   VPIDs as well.
/// * All-contexts invalidation: If the INVVPID type is 2, the logical processor
///   invalidates all mappings tagged with all VPIDs except VPID 0000H. In some
///   cases, it may invalidate translations with VPID 0000H as well.
/// * Single-context invalidation, retaining global translations: If the INVVPID
///   type is 3, the logical processor invalidates all mappings tagged with the
///   VPID specified in the INVVPID descriptor except global translations. In
///   some cases, it may invalidate global translations (and mappings with other
///   VPIDs) as well.
///
/// Must match the C++ `InvVpid` in `<arch/x86/hypervisor/invalidate.h>`, as it is passed across
/// FFI.
#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvVpid {
    IndividualAddress = 0,
    SingleContext = 1,
    AllContexts = 2,
    SingleContextRetainGlobals = 3,
}

/// Invalidates TLB mappings tagged with a VPID, as selected by `invalidation`.
///
/// # Panics
///
/// Panics if the instruction fails (VMfailInvalid or VMfailValid). Executing outside VMX root
/// operation faults.
pub fn invvpid(invalidation: InvVpid, vpid: u16, address: zx_vaddr_t) {
    let err: u8;
    let descriptor: [u64; 2] = [u64::from(vpid), address as u64];

    // SAFETY: INVVPID only reads the 128-bit descriptor, which is a live local, and invalidates
    // cached translations, which does not affect memory safety. The descriptor's reserved bits
    // (63:16 of the first quadword) are zero as `vpid` is a `u16`. Invalid operands are reported
    // through CF/ZF without writing memory and are caught by the `assert!` below, and executing
    // outside VMX root operation raises a fault rather than continuing.
    unsafe {
        asm!(
            "invvpid {invalidation}, xmmword ptr [{descriptor}]",
            // Set `err` on error (C or Z flag set)
            "setna {err}",
            invalidation = in(reg) invalidation as u64,
            descriptor = in(reg) descriptor.as_ptr(),
            err = out(reg_byte) err,
            options(nostack),
        );
    }

    assert!(err == 0);
}

/// INVEPT invalidation types.
///
/// From Volume 3, Section 30.3: There are two INVEPT types currently defined:
/// * Single-context invalidation. If the INVEPT type is 1, the logical
///   processor invalidates all mappings associated with bits 51:12 of the EPT
///   pointer (EPTP) specified in the INVEPT descriptor. It may invalidate other
///   mappings as well.
/// * Global invalidation. If the INVEPT type is 2, the logical processor
///   invalidates mappings associated with all EPTPs.
///
/// Must match the C++ `InvEpt` in `<arch/x86/hypervisor/invalidate.h>`, as it is passed across
/// FFI.
#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvEpt {
    SingleContext = 1,
    Global = 2,
}

/// Invalidates TLB mappings derived from EPT, as selected by `invalidation`.
///
/// # Panics
///
/// Panics if the instruction fails (VMfailInvalid or VMfailValid). Executing outside VMX root
/// operation faults.
pub fn invept(invalidation: InvEpt, eptp: u64) {
    let err: u8;
    let descriptor: [u64; 2] = [eptp, 0];

    // SAFETY: INVEPT only reads the 128-bit descriptor, which is a live local, and invalidates
    // cached translations, which does not affect memory safety. The descriptor's reserved second
    // quadword is zero. Invalid operands are reported through CF/ZF without writing memory and are
    // caught by the `assert!` below, and executing outside VMX root operation raises a fault rather
    // than continuing.
    unsafe {
        asm!(
            "invept {invalidation}, xmmword ptr [{descriptor}]",
            // Set `err` on error (C or Z flag set)
            "setna {err}",
            invalidation = in(reg) invalidation as u64,
            descriptor = in(reg) descriptor.as_ptr(),
            err = out(reg_byte) err,
            options(nostack),
        );
    }

    assert!(err == 0);
}

/// Returns an EPT pointer based on an EPT PML4 address.
pub fn ept_pointer_from_pml4(ept_pml4: zx_paddr_t) -> u64 {
    // Physical address of the PML4 page, page aligned.
    ept_pml4 as u64
        // Use write-back memory type for paging structures.
        | VMX_MEMORY_TYPE_WRITE_BACK
        // Page walk length of 4 (defined as N minus 1).
        | 3u64 << 3
}

/// Unit tests for the VMX invalidation helpers.
#[cfg(ktest)]
#[unittest::suite(name = "x86_hypervisor_invalidate")]
mod tests {
    use unittest::expect_eq;

    /// Verifies that `ept_pointer_from_pml4` sets the memory type and page walk length.
    #[test]
    fn ept_pointer_from_pml4_fields() {
        // Bits 2:0 are the memory type (6, write-back) and bits 5:3 the page walk length minus 1
        // (3).
        expect_eq!(0x1e, super::ept_pointer_from_pml4(0));
        expect_eq!(0x1234_5678_901e, super::ept_pointer_from_pml4(0x1234_5678_9000));
    }
}
