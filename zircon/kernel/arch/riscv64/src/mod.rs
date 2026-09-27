// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

pub mod arch;
pub mod cache;
pub mod crashlog;
pub mod debugger;
pub mod exceptions;
pub mod feature;
pub mod fpu;
pub mod mp;
pub mod restricted;
pub mod sbi;
pub mod spinlock;
pub mod thread;
pub mod timer;
pub mod user_copy;
pub mod vector;

use riscv64_aspace_bindings as aspace_bindings;

/// Virtual address where the kernel address space begins.
/// Below this is the user address space.
/// riscv64 with sv39 means a page-based 39-bit virtual memory space.  The
/// base kernel address is chosen so that kernel addresses have a 1 in the
/// most significant bit whereas user addresses have a 0.
pub const KERNEL_ASPACE_BASE: usize = 0xffffffc000000000;
zr::static_assert!(KERNEL_ASPACE_BASE == aspace_bindings::KERNEL_ASPACE_BASE as usize);

/// Virtual address where the kernel address space begins.
/// Below this is the user address space.
/// riscv64 with sv39 means a page-based 39-bit virtual memory space.  The
/// base kernel address is chosen so that kernel addresses have a 1 in the
/// most significant bit whereas user addresses have a 0.
pub const KERNEL_ASPACE_SIZE: usize = 1usize << 38;
zr::static_assert!(KERNEL_ASPACE_SIZE == aspace_bindings::KERNEL_ASPACE_SIZE as usize);

/// Virtual address where the user-accessible address space begins.
/// Below this is wholly inaccessible.
pub const USER_ASPACE_BASE: usize = 0x0000000000200000;
zr::static_assert!(USER_ASPACE_BASE == aspace_bindings::USER_ASPACE_BASE as usize);

/// Virtual address where the user-accessible address space begins.
/// Below this is wholly inaccessible.
pub const USER_ASPACE_SIZE: usize = (1usize << 38) - USER_ASPACE_BASE;
zr::static_assert!(USER_ASPACE_SIZE == aspace_bindings::USER_ASPACE_SIZE as usize);

/// Size of the restricted mode address space in unified address spaces.
/// We set the top of the restricted aspace to exactly halfway through the top
/// level page table.
pub const USER_RESTRICTED_ASPACE_SIZE: usize = (1usize << 37) - USER_ASPACE_BASE;
zr::static_assert!(
    USER_RESTRICTED_ASPACE_SIZE == aspace_bindings::USER_RESTRICTED_ASPACE_SIZE as usize
);

/// The dimensions of the paging are determined by libpage.
///
/// SvXXx4 for hypervisor guest translation
pub const MMU_GUEST_SIZE_SHIFT: usize = aspace_bindings::MMU_GUEST_SIZE_SHIFT;

/// Zic64b guarantees.
pub const MAX_CACHE_LINE: usize = 64;
#[repr(align(64))]
pub struct CpuAlignMarker;

/// Returns whether `va` is within the kernel address space.
#[inline]
pub fn is_kernel_address(va: usize) -> bool {
    va >= KERNEL_ASPACE_BASE && va.wrapping_sub(KERNEL_ASPACE_BASE) < KERNEL_ASPACE_SIZE
}

/// Userspace threads can only set an entry point to userspace addresses, or
/// the null pointer (for testing a thread that will always fail).
#[inline]
pub fn is_valid_user_pc(pc: usize) -> bool {
    (pc == 0) || (is_user_accessible(pc) && !is_kernel_address(pc))
}

#[cfg(ktest)]
/// Architecture unit tests for riscv64.
#[unittest::suite(name = "riscv64")]
mod riscv64_tests {
    use unittest::{assert_false, assert_true};

    /// Tests `is_kernel_address`.
    #[test]
    fn test_is_kernel_address() {
        assert_true!(is_kernel_address(KERNEL_ASPACE_BASE));
        assert_true!(is_kernel_address(KERNEL_ASPACE_BASE + 0x1000));
        assert_true!(is_kernel_address(KERNEL_ASPACE_BASE + (KERNEL_ASPACE_SIZE - 1)));
        assert_false!(is_kernel_address(0));
        assert_false!(is_kernel_address(0x1000));
        assert_false!(is_kernel_address(0x0000_003f_ffff_ffff));
        assert_false!(is_kernel_address(KERNEL_ASPACE_BASE - 1));
    }

    /// Tests `is_valid_user_pc`.
    #[test]
    fn test_is_valid_user_pc() {
        // Null pointer is valid (used for threads intended to fault).
        assert_true!(is_valid_user_pc(0));
        // Valid userspace addresses.
        assert_true!(is_valid_user_pc(0x1000));
        assert_true!(is_valid_user_pc(0x0000_003f_ffff_0000));
        // Inaccessible user address (bit 38 set).
        assert_false!(is_valid_user_pc(0x0000_0040_0000_0000));
        // Kernel address.
        assert_false!(is_valid_user_pc(KERNEL_ASPACE_BASE));
        assert_false!(is_valid_user_pc(0xffff_ffff_8000_0000));
    }
}

// Names the rest of the kernel resolves directly under `arch::riscv64`: the
// arch API contract checked by `assert_arch_signatures!` in
// //zircon/kernel/arch/src/api.rs, plus the few names other subsystems import
// by that path. Everything else stays behind its module, matching
// //zircon/kernel/arch/x86/src/mod.rs.
pub use arch::{
    arch_early_init, arch_enter_idle_state, arch_init, arch_late_init_percpu, arch_prevm_init,
};
pub use mp::arch_curr_cpu_num;
pub use restricted::{
    ArchSavedNormalState, Iframe, SyscallRegs, boot_hart_id, curr_hart_id, dump, enter_full,
    enter_restricted, redirect_restricted_exception_to_normal, save_restricted_exception_state,
    save_restricted_iframe_state, save_restricted_syscall_state, save_state_pre_restricted_entry,
    validate_state_pre_restricted_entry,
};
pub use thread::{
    arch_context_switch, arch_dump_thread, arch_enter_uspace, arch_prepare_uspace,
    arch_reset_suspended_general_regs, arch_restore_user_state, arch_save_user_state,
    arch_set_suspended_general_regs, arch_thread_construct_first, arch_thread_get_blocked_fp,
    arch_thread_initialize,
};
pub use user_copy::{
    arch_copy_from_user, arch_copy_from_user_capture_faults, arch_copy_to_user,
    arch_copy_to_user_capture_faults, is_user_accessible,
};
