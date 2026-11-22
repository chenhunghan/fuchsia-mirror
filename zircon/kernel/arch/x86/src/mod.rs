// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

mod amd;
pub mod apic;
pub mod arch;
mod arch_ffi;
pub mod bootstrap16;
mod bootstrap16_ffi;
pub mod cache;
mod cache_ffi;
pub mod cpuid;
pub mod crashlog;
pub mod debugger;
mod descriptor;
mod descriptor_ffi;
pub mod exceptions;
mod exceptions_ffi;
#[cfg(ktest)]
mod fake_msr_access;
pub mod feature;
mod feature_ffi;
pub mod handoff;
mod handoff_ffi;
mod hwp;
#[path = "../hypervisor/mod.rs"]
pub mod hypervisor;
mod idle_states;
mod idle_states_ffi;
mod idt;
mod idt_ffi;
mod intel;
pub mod interrupts;
pub mod ioapic;
mod ioport;
mod ioport_ffi;
pub mod lapic;
mod mmu_mem_types;
mod mmu_mem_types_ffi;
pub mod mp;
mod mp_ffi;
#[cfg(any(console_enabled, ktest))]
mod msr;
#[cfg(console_enabled)]
mod msr_bench;
mod mwait_monitor;
pub mod platform_access;
#[cfg(ktest)]
mod platform_tests;
pub mod pv;
pub mod registers;
mod registers_ffi;
pub mod restricted;
pub mod smp;
mod smp_ffi;
mod spinlock;
pub mod system_topology;
pub mod thread;
mod thread_ffi;
mod timer_freq;
mod user_copy;
mod user_copy_ffi;
pub mod vm;
pub mod x86;

pub use arch::{
    arch_enter_idle_state, arch_flush_state_and_halt, early_init as arch_early_init,
    init as arch_init, late_init_percpu as arch_late_init_percpu, prevm_init as arch_prevm_init,
};
pub use arch_ffi::{arch_enter_uspace, arch_prepare_uspace};
pub use debugger::{
    get_instruction_pointer as arch_get_instruction_pointer,
    set_return_instruction_pointer as arch_set_return_instruction_pointer,
};
pub use ioport::IoBitmap;
pub use mp::{arch_curr_cpu_num, arch_get_restricted_flag};
pub use restricted::{
    ArchSavedNormalState, dump, enter_full, enter_restricted,
    redirect_restricted_exception_to_normal, save_restricted_exception_state,
    save_restricted_iframe_state, save_restricted_syscall_state, save_state_pre_restricted_entry,
    validate_state_pre_restricted_entry,
};
pub use spinlock::{
    arch_spin_lock_non_instrumented, arch_spin_lock_trace_instrumented, arch_spin_trylock,
    arch_spin_unlock,
};
pub use thread::{
    context_switch as arch_context_switch, dump_thread as arch_dump_thread,
    thread_get_blocked_fp as arch_thread_get_blocked_fp,
};
pub use thread_ffi::{
    arch_reset_suspended_general_regs, arch_restore_user_state, arch_save_user_state,
    arch_set_suspended_general_regs, arch_thread_construct_first, arch_thread_initialize,
};
pub use user_copy::{
    arch_copy_from_user, arch_copy_from_user_capture_faults, arch_copy_to_user,
    arch_copy_to_user_capture_faults,
};

#[cfg(ktest)]
use zx_status::Status;

use arch_x86_aspace_bindings as aspace_bindings;

pub const MAX_CACHE_LINE: usize = 64;
#[repr(align(64))]
pub struct CpuAlignMarker;

/// Kernel 64-bit code segment selector (GDT index 2: `2 << 3 = 0x10`).
pub const CODE_64_SELECTOR: u16 = 0x10;

/// 32-bit compatibility user code selector (GDT index 4 with Requested Privilege Level 3:
/// `(4 << 3) | 3 = 0x23`), used as the `IA32_STAR[63:48]` base.
pub const USER_CODE_SELECTOR: u16 = 0x20 | 3;

/// User 64-bit code segment selector (GDT index 6 with Requested Privilege Level 3:
/// `(6 << 3) | 3 = 0x33`).
///
/// See [intel/vol3]: 3.4.2 Segment Selectors, and
/// [amd/vol2]: 4.5.1 Segment Selectors.
const USER_CODE_64_SELECTOR: u64 = 0x30 | 3;

/// User data segment selector (GDT index 5 with Requested Privilege Level 3:
/// `(5 << 3) | 3 = 0x2b`).
///
/// See [intel/vol3]: 3.4.2 Segment Selectors, and
/// [amd/vol2]: 4.5.1 Segment Selectors.
const USER_DATA_SELECTOR: u64 = 0x28 | 3;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Iframe {
    pub rdi: u64,
    pub rsi: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rax: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub vector: u64,
    pub err_code: u64,
    pub ip: u64,
    pub cs: u64,
    pub flags: u64,
    pub user_sp: u64,
    pub user_ss: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct SyscallRegs {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
    pub rsp: u64,
}

zr::static_assert!(core::mem::size_of::<Iframe>() == 176);
zr::static_assert!(core::mem::align_of::<Iframe>() == 8);
zr::static_assert!(core::mem::offset_of!(Iframe, rdi) == 0);
zr::static_assert!(core::mem::offset_of!(Iframe, rsi) == 8);
zr::static_assert!(core::mem::offset_of!(Iframe, rbp) == 16);
zr::static_assert!(core::mem::offset_of!(Iframe, rbx) == 24);
zr::static_assert!(core::mem::offset_of!(Iframe, rdx) == 32);
zr::static_assert!(core::mem::offset_of!(Iframe, rcx) == 40);
zr::static_assert!(core::mem::offset_of!(Iframe, rax) == 48);
zr::static_assert!(core::mem::offset_of!(Iframe, r8) == 56);
zr::static_assert!(core::mem::offset_of!(Iframe, r9) == 64);
zr::static_assert!(core::mem::offset_of!(Iframe, r10) == 72);
zr::static_assert!(core::mem::offset_of!(Iframe, r11) == 80);
zr::static_assert!(core::mem::offset_of!(Iframe, r12) == 88);
zr::static_assert!(core::mem::offset_of!(Iframe, r13) == 96);
zr::static_assert!(core::mem::offset_of!(Iframe, r14) == 104);
zr::static_assert!(core::mem::offset_of!(Iframe, r15) == 112);
zr::static_assert!(core::mem::offset_of!(Iframe, vector) == 120);
zr::static_assert!(core::mem::offset_of!(Iframe, err_code) == 128);
zr::static_assert!(core::mem::offset_of!(Iframe, ip) == 136);
zr::static_assert!(core::mem::offset_of!(Iframe, cs) == 144);
zr::static_assert!(core::mem::offset_of!(Iframe, flags) == 152);
zr::static_assert!(core::mem::offset_of!(Iframe, user_sp) == 160);
zr::static_assert!(core::mem::offset_of!(Iframe, user_ss) == 168);

zr::static_assert!(core::mem::size_of::<SyscallRegs>() == 144);
zr::static_assert!(core::mem::align_of::<SyscallRegs>() == 8);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rax) == 0);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rbx) == 8);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rcx) == 16);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rdx) == 24);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rsi) == 32);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rdi) == 40);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rbp) == 48);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, r8) == 56);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, r9) == 64);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, r10) == 72);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, r11) == 80);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, r12) == 88);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, r13) == 96);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, r14) == 104);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, r15) == 112);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rip) == 120);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rflags) == 128);
zr::static_assert!(core::mem::offset_of!(SyscallRegs, rsp) == 136);

/// Entry state for a thread.
#[allow(unused_imports)]
pub use arch_types_bindings::{GeneralRegsSource, UserEntryState};

/// Virtual address where the kernel address space begins.
/// Below this is the user address space.
pub const KERNEL_ASPACE_BASE: usize = 0xffffff8000000000; // -512GB
zr::static_assert!(KERNEL_ASPACE_BASE == aspace_bindings::KERNEL_ASPACE_BASE as usize);

/// Virtual address where the kernel address space begins.
/// Below this is the user address space.
pub const KERNEL_ASPACE_SIZE: usize = 0x0000008000000000;
zr::static_assert!(KERNEL_ASPACE_SIZE == aspace_bindings::KERNEL_ASPACE_SIZE as usize);

/// Virtual address where the user-accessible address space begins.
/// Below this is wholly inaccessible.
pub const USER_ASPACE_BASE: usize = 0x0000000000200000; // 2MB
zr::static_assert!(USER_ASPACE_BASE == aspace_bindings::USER_ASPACE_BASE as usize);

/// We set the top of user address space to be (1 << 47) - 4k.
/// See //docs/concepts/kernel/sysret_problem.md for why we subtract 4k here.
pub const USER_ASPACE_SIZE: usize = (1usize << 47) - 4096 - USER_ASPACE_BASE;
zr::static_assert!(USER_ASPACE_SIZE == aspace_bindings::USER_ASPACE_SIZE as usize);

/// Size of the restricted mode address space in unified address spaces.
/// We set the top of the restricted aspace to exactly halfway through the PML4.
pub const USER_RESTRICTED_ASPACE_SIZE: usize = (1usize << 46) - USER_ASPACE_BASE;
zr::static_assert!(
    USER_RESTRICTED_ASPACE_SIZE == aspace_bindings::USER_RESTRICTED_ASPACE_SIZE as usize
);

pub const MMU_GUEST_SIZE_SHIFT: usize = aspace_bindings::MMU_GUEST_SIZE_SHIFT as usize;

/// x86-64 page table constants and helpers shared with `<arch/x86/mmu.h>` and
/// `<page_tables/x86/constants.h>`.
pub mod mmu {
    use core::ptr;

    use crate::kernel::types::PAddr;
    use crate::vm::physmap::paddr_to_physmap;

    use super::aspace_bindings;

    pub const NO_OF_PT_ENTRIES: usize = aspace_bindings::NO_OF_PT_ENTRIES as usize;
    pub const PDP_SHIFT: usize = aspace_bindings::PDP_SHIFT as usize;
    pub const PD_SHIFT: usize = aspace_bindings::PD_SHIFT as usize;
    pub const PT_SHIFT: usize = aspace_bindings::PT_SHIFT as usize;
    pub const ADDR_OFFSET: usize = aspace_bindings::ADDR_OFFSET as usize;

    pub const X86_MMU_PG_P: u64 = aspace_bindings::X86_MMU_PG_P as u64;
    pub const X86_MMU_PG_RW: u64 = aspace_bindings::X86_MMU_PG_RW as u64;
    pub const X86_MMU_PG_G: u64 = aspace_bindings::X86_MMU_PG_G as u64;
    pub const X86_MMU_PG_NX: u64 = aspace_bindings::X86_MMU_PG_NX as u64;
    pub const X86_FLAGS_MASK: u64 = aspace_bindings::X86_FLAGS_MASK as u64;

    pub const X86_KERNEL_KASAN_RW_PT_FLAGS: u64 =
        aspace_bindings::X86_KERNEL_KASAN_RW_PT_FLAGS as u64;
    pub const X86_KERNEL_KASAN_PD_FLAGS: u64 = aspace_bindings::X86_KERNEL_KASAN_PD_FLAGS as u64;

    unsafe extern "C" {
        static upper_512gib_page_table_phys: usize;
    }

    /// Returns a pointer to the upper 512 GiB PDP table in the physmap.
    #[cfg_attr(sanitize = "address", sanitize(address = "off"))]
    pub fn x86_upper_512gib_page_table() -> *mut u64 {
        // SAFETY: `upper_512gib_page_table_phys` is initialized once in `start.S` before any C or
        // Rust code runs and never modified thereafter.
        let phys = unsafe { upper_512gib_page_table_phys };
        ptr::with_exposed_provenance_mut::<u64>(paddr_to_physmap(PAddr(phys)).0)
    }
}

#[cfg(ktest)]
/// Architecture unit tests for x86.
#[unittest::suite(name = "x86")]
mod x86_tests {
    use super::vm::{is_kernel_address, is_user_accessible, is_valid_user_pc};
    use unittest::{assert_err, assert_false, assert_ok, assert_true};
    use zx_types::zx_restricted_state_t;

    /// Tests `is_user_accessible`.
    #[test]
    fn test_is_user_accessible() {
        assert_true!(is_user_accessible(0x00000000_00100000));
        assert_true!(is_user_accessible(0x00007fff_ffffffff));
        assert_false!(is_user_accessible(0x00008000_00000000));
        assert_false!(is_user_accessible(0xffff8000_00000000));
    }

    /// Tests `validate_state_pre_restricted_entry`.
    #[test]
    fn test_validate_state_pre_restricted_entry() {
        let state = zx_restricted_state_t { ip: 0x1000, ..Default::default() };
        assert_ok!(validate_state_pre_restricted_entry(&state));

        let state_bad = zx_restricted_state_t { ip: 0xffff_8000_0000_0000, ..Default::default() };
        assert_err!(validate_state_pre_restricted_entry(&state_bad), Status::BAD_STATE);
    }

    /// Tests `dump`.
    #[test]
    fn test_dump() {
        let state = zx_restricted_state_t::default();
        dump(&state);
    }

    /// Tests `is_kernel_address`.
    #[test]
    fn test_is_kernel_address() {
        assert_true!(is_kernel_address(KERNEL_ASPACE_BASE));
        assert_true!(is_kernel_address(KERNEL_ASPACE_BASE + 0x1000));
        assert_true!(is_kernel_address(usize::MAX));
        assert_false!(is_kernel_address(0));
        assert_false!(is_kernel_address(0x1000));
        assert_false!(is_kernel_address(0x0000_7fff_ffff_ffff));
        assert_false!(is_kernel_address(KERNEL_ASPACE_BASE - 1));
    }

    /// Tests `is_valid_user_pc`.
    #[test]
    fn test_is_valid_user_pc() {
        // Null pointer is valid (used for threads intended to fault).
        assert_true!(is_valid_user_pc(0));
        // Valid userspace addresses.
        assert_true!(is_valid_user_pc(0x1000));
        assert_true!(is_valid_user_pc(0x0000_7fff_ffff_0000));
        // Non-canonical address.
        assert_false!(is_valid_user_pc(0x0000_8000_0000_0000));
        // Kernel address.
        assert_false!(is_valid_user_pc(KERNEL_ASPACE_BASE));
        assert_false!(is_valid_user_pc(0xffff_8000_0000_0000));
    }
}
