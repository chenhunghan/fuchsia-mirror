// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Virtual CPUs of the x86 hypervisor.

use boot_options::BootOptions;
use core::cell::{Cell, UnsafeCell};
use core::marker::{PhantomData, PhantomPinned};
use core::mem::{align_of, size_of};
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use debug::dprintf;
use fbl::{Recyclable, UniquePtr};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zr::static_assert;
use zx_status::Status;
use zx_types::{
    ZX_INFO_VCPU_FLAG_KICKED, zx_info_vcpu_t, zx_port_packet_t, zx_status_t, zx_vaddr_t,
    zx_vcpu_io_t, zx_vcpu_state_t,
};

use super::guest::Guest;
use super::invalidate::{InvVpid, ept_pointer_from_pml4, invvpid};
use super::pv::{PvClockState, pv_clock_update_system_time};
use super::vmcs::{
    AutoVmcs, ENTRY_CTLS_64BIT_MODE, ENTRY_CTLS_LOAD_IA32_EFER, ENTRY_CTLS_LOAD_IA32_PAT,
    EXIT_CTLS_64BIT_MODE, EXIT_CTLS_ACK_INT_ON_EXIT, EXIT_CTLS_LOAD_IA32_EFER,
    EXIT_CTLS_LOAD_IA32_PAT, EXIT_CTLS_SAVE_IA32_EFER, EXIT_CTLS_SAVE_IA32_PAT,
    GUEST_TR_ACCESS_RIGHTS_TSS_BUSY, GUEST_XX_ACCESS_RIGHTS_DEFAULT, GUEST_XX_ACCESS_RIGHTS_L,
    GUEST_XX_ACCESS_RIGHTS_P, GUEST_XX_ACCESS_RIGHTS_TYPE_CODE, GUEST_XX_ACCESS_RIGHTS_TYPE_E,
    GUEST_XX_ACCESS_RIGHTS_TYPE_W, INTERRUPTIBILITY_MOV_SS_BLOCKING, INTERRUPTIBILITY_NMI_BLOCKING,
    INTERRUPTIBILITY_STI_BLOCKING, LINK_POINTER_INVALIDATE, PINBASED_CTLS_EXT_INT_EXITING,
    PINBASED_CTLS_NMI_EXITING, PROCBASED_CTLS_CR3_LOAD_EXITING, PROCBASED_CTLS_CR3_STORE_EXITING,
    PROCBASED_CTLS_CR8_LOAD_EXITING, PROCBASED_CTLS_CR8_STORE_EXITING, PROCBASED_CTLS_HLT_EXITING,
    PROCBASED_CTLS_INT_WINDOW_EXITING, PROCBASED_CTLS_IO_EXITING, PROCBASED_CTLS_MSR_BITMAPS,
    PROCBASED_CTLS_PROCBASED_CTLS2, PROCBASED_CTLS_TPR_SHADOW, PROCBASED_CTLS2_EPT,
    PROCBASED_CTLS2_INVPCID, PROCBASED_CTLS2_PAUSE_LOOP_EXITING, PROCBASED_CTLS2_RDTSCP,
    PROCBASED_CTLS2_UNRESTRICTED_GUEST, PROCBASED_CTLS2_VPID, PROCBASED_CTLS2_X2APIC, VmcsField,
    VmcsField16, VmcsField32, VmcsField64, VmcsFieldXX, X86_MSR_IA32_VMX_ENTRY_CTLS,
    X86_MSR_IA32_VMX_EXIT_CTLS, X86_MSR_IA32_VMX_PINBASED_CTLS, X86_MSR_IA32_VMX_PROCBASED_CTLS,
    X86_MSR_IA32_VMX_PROCBASED_CTLS2, X86_MSR_IA32_VMX_TRUE_ENTRY_CTLS,
    X86_MSR_IA32_VMX_TRUE_EXIT_CTLS, X86_MSR_IA32_VMX_TRUE_PINBASED_CTLS,
    X86_MSR_IA32_VMX_TRUE_PROCBASED_CTLS, cr0_is_invalid, vmclear, vmptrld, vmwrite,
};
use super::vmexit::vmexit_handler;
use super::vmx_cpu_state::{
    VmxInfo, VmxPage, VmxRegion, X86_MSR_IA32_VMX_CR4_FIXED0, X86_MSR_IA32_VMX_CR4_FIXED1,
    cr_is_invalid,
};
use super::vmx_state::VmxState;
use crate::arch_rs::ops::num_spinlocks_held;
use crate::arch_rs::x86::descriptor::{
    CODE_64_SELECTOR, DATA_SELECTOR, clear_tss_busy, gdt_get, tss_selector,
};
use crate::arch_rs::x86::feature::{
    X86_FEATURE_INVAR_TSC, x86_cpu_has_ibpb, x86_cpu_ibpb, x86_cpu_should_l1d_flush_on_vmentry,
    x86_cpu_should_md_clear_on_user_return, x86_feature_test, x86_hypervisor_has_pv_clock,
};
use crate::arch_rs::x86::idt::idt_get_readonly;
use crate::arch_rs::x86::interrupts::{
    X86_INT_NMI, X86_INT_PLATFORM_BASE, X86_INT_VIRT, X86InterruptVector,
};
use crate::arch_rs::x86::lapic::LVT_MASKED;
use crate::arch_rs::x86::mp::{
    PERCPU_CPU_NUM_OFFSET, PERCPU_DEFAULT_TSS_OFFSET, PERCPU_DIRECT_OFFSET,
};
use crate::arch_rs::x86::pv::pv_clock_is_stable;
use crate::arch_rs::x86::registers::{
    X86_CR0_CD, X86_CR0_ET, X86_CR0_NE, X86_CR0_NW, X86_CR0_PE, X86_CR0_PG, X86_CR0_WP,
    X86_CR4_FSGSBASE, X86_CR4_LA57, X86_CR4_OSFXSR, X86_CR4_OSXSAVE, X86_CR4_PAE, X86_CR4_PGE,
    X86_CR4_VMXE, X86_EFER_LMA, X86_EFER_LME, X86_FLAGS_IF, X86_FLAGS_RESERVED_ONES,
    X86_FLAGS_USER, X86_MAX_EXTENDED_REGISTER_SIZE, X86_MSR_IA32_EFER, X86_MSR_IA32_FLUSH_CMD,
    X86_MSR_IA32_FMASK, X86_MSR_IA32_FS_BASE, X86_MSR_IA32_GS_BASE, X86_MSR_IA32_KERNEL_GS_BASE,
    X86_MSR_IA32_LSTAR, X86_MSR_IA32_PAT, X86_MSR_IA32_STAR, X86_MSR_IA32_TSC_AUX,
    X86_XSAVE_STATE_BIT_AVX, X86_XSAVE_STATE_BIT_SSE, X86_XSAVE_STATE_BIT_X87,
    extended_register_init_state, extended_register_init_state_from_bv,
    extended_register_restore_state, extended_register_save_state, xgetbv, xsave_supported, xsetbv,
};
use crate::arch_rs::x86::x86::{
    mds_buff_overwrite, read_msr, write_msr, x86_get_cr0, x86_get_cr3, x86_get_cr4, x86_ltr,
    x86_ras_fill, x86_read_gs_offset32, x86_read_gs_offset64, x86_thread_extended_register_buffer,
    x86_thread_fs_base,
};
use crate::arch_rs::{blocking_disallowed, curr_cpu_num, ints_disabled};
use crate::hypervisor::interrupt_tracker::InterruptTracker;
use crate::hypervisor::ktrace::{VcpuExit, ktrace_vcpu_exit};
use crate::kconcurrent::{
    ChainLockProof, NO_IRQ_SAVE_OPTION, clt_tag, single_chain_lock_guard_owned,
};
use crate::kernel::mp::{self, INVALID_CPU, MpIpiTarget, cpu_num_to_mask};
use crate::kernel::percpu::PerCpu;
use crate::kernel::scheduler_state::ThreadStateKind;
use crate::kernel::thread::{ContextSwitchFn, MigrateFn, MigrateStage, Thread, current_get};
use crate::kernel::timer::{Timer, ZX_CLOCK_MONOTONIC};
use crate::kernel::types::PAddr;
use crate::ktrace_rs::duration_begin;

/// Tracks the pending interrupts of a VCPU.
type VcpuInterruptTracker = InterruptTracker<
    { X86InterruptVector::COUNT },
    { X86InterruptVector::COUNT.div_ceil(usize::BITS as usize) },
>;

/// Stores the local APIC state for a virtual CPU.
///
/// The APIC timer callback accesses this concurrently with the VCPU thread, so it is only accessed
/// through shared references and the LVT registers are atomics. Relaxed accesses suffice: each LVT
/// register is an independent word through which no other memory is published, and the ordering
/// between the VCPU thread and the timer callback comes from `Timer::cancel` and
/// `Timer::set_oneshot`.
#[pin_data]
#[repr(C)]
pub struct LocalApicState {
    /// Timer for APIC timer.
    #[pin]
    pub timer: Timer,
    /// Tracks pending interrupts.
    #[pin]
    pub interrupt_tracker: VcpuInterruptTracker,
    /// LVT timer configuration. Initial state is masked (Vol 3 Section 10.12.5.1).
    pub lvt_timer: AtomicU32,
    /// LVT timer initial count.
    pub lvt_initial_count: AtomicU32,
    /// LVT timer divide configuration.
    pub lvt_divide_config: AtomicU32,
}

impl LocalApicState {
    /// Creates an in-place initializer for the local APIC state of a new VCPU.
    pub fn init() -> impl PinInit<Self> {
        pin_init!(Self {
            timer <- Timer::init(ZX_CLOCK_MONOTONIC),
            interrupt_tracker <- VcpuInterruptTracker::init(),
            // Initial state is masked (Vol 3 Section 10.12.5.1).
            lvt_timer: AtomicU32::new(LVT_MASKED),
            lvt_initial_count: AtomicU32::new(0),
            lvt_divide_config: AtomicU32::new(0),
        })
    }
}

/// Stores part of the MSR state for a virtual CPU.
#[derive(Default)]
struct MsrState {
    star: u64,
    lstar: u64,
    fmask: u64,
    tsc_aux: u64,
    kernel_gs_base: u64,
}

/// The XSAVE area of a VCPU.
#[repr(C, align(64))]
struct ExtendedRegisterState([u8; X86_MAX_EXTENDED_REGISTER_SIZE]);

/// Increments the guest statistic `$name` of the current CPU.
macro_rules! guest_stats_inc {
    ($name:ident) => {
        // Relaxed, as the statistics are independent counters that do not synchronize any other
        // memory.
        PerCpu::get_current().gstats.$name.fetch_add(1, Ordering::Relaxed)
    };
}

const BASE_PROCESSOR_VPID: u16 = 1;
const GUEST_XCR0_MASK: u64 =
    X86_XSAVE_STATE_BIT_X87 | X86_XSAVE_STATE_BIT_SSE | X86_XSAVE_STATE_BIT_AVX;

// Implemented in assembly.
unsafe extern "C" {
    /// Low-level functionality to save register and restore register state before/after entering
    /// a guest. Should only be called by `vmx_enter`.
    fn vmx_enter_asm(vmx_state: *mut VmxState) -> zx_status_t;

    /// The location where we jump to when a guest exits. An internal implementation detail of
    /// `vmx_enter_asm`.
    fn vmx_exit_asm();
}

/// Returns the CPU number and the address of the default TSS of the current CPU, from its
/// `x86_percpu`.
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized to the calling CPU's `x86_percpu` and
/// interrupts are disabled on the calling CPU (`ints_disabled()`) so the calling thread cannot
/// migrate between the two `%gs` loads.
unsafe fn percpu_cpu_num_and_default_tss() -> (u32, u64) {
    debug_assert!(ints_disabled());
    // SAFETY: The caller guarantees that `%gs.base` points to the current CPU's `x86_percpu` with
    // interrupts disabled, so both loads read from the same CPU's `x86_percpu`, and
    // `PERCPU_CPU_NUM_OFFSET` and `PERCPU_DIRECT_OFFSET` are the aligned offsets of its `cpu_num`
    // and `direct` fields.
    let (cpu_num, percpu) = unsafe {
        (
            x86_read_gs_offset32::<PERCPU_CPU_NUM_OFFSET>(),
            x86_read_gs_offset64::<PERCPU_DIRECT_OFFSET>(),
        )
    };
    (cpu_num, percpu + PERCPU_DEFAULT_TSS_OFFSET as u64)
}

macro_rules! register_copy {
    ($out:expr, $in:expr) => {{
        let out = $out;
        let r#in = $in;
        out.rax = r#in.rax;
        out.rcx = r#in.rcx;
        out.rdx = r#in.rdx;
        out.rbx = r#in.rbx;
        out.rbp = r#in.rbp;
        out.rsi = r#in.rsi;
        out.rdi = r#in.rdi;
        out.r8 = r#in.r8;
        out.r9 = r#in.r9;
        out.r10 = r#in.r10;
        out.r11 = r#in.r11;
        out.r12 = r#in.r12;
        out.r13 = r#in.r13;
        out.r14 = r#in.r14;
        out.r15 = r#in.r15;
    }};
}

/// # Safety
///
/// `vmcs` must not have been invalidated (so its VMCS is current on this CPU with interrupts
/// disabled), CPL must be 0, `%gs.base` must be initialized to the calling CPU's `x86_percpu`, and
/// `extended_register_init` must have completed on CPU 0. `vmx_state` and
/// `extended_register_state` (a 64-byte-aligned `X86_MAX_EXTENDED_REGISTER_SIZE` zeroed or
/// initialized XSAVE buffer) must be valid for writes and remain at the same address while the
/// VMCS of `vmcs` is in use, as the VMCS refers to `vmx_state`. `ept_pml4` and
/// `msr_bitmaps_address` must be the physical addresses of the guest's EPT PML4 and MSR bitmaps,
/// which must remain allocated while the VMCS of `vmcs` is in use.
unsafe fn vmcs_init(
    vmcs: &mut AutoVmcs,
    vpid: u16,
    entry: usize,
    msr_bitmaps_address: PAddr,
    ept_pml4: PAddr,
    vmx_state: *mut VmxState,
    extended_register_state: *mut u8,
) -> Result<(), Status> {
    // SAFETY: Reading the VMX capability MSRs does not access memory. They exist on every CPU that
    // supports VMX, which a guest requires.
    let msr = |msr_id| unsafe { read_msr(msr_id) };

    // Setup secondary processor-based VMCS controls.
    // SAFETY: Enabling EPT confines the guest to the memory mapped by its EPT, and the other
    // controls only affect the guest.
    unsafe {
        vmcs.set_control(
            VmcsField32::PROCBASED_CTLS2,
            msr(X86_MSR_IA32_VMX_PROCBASED_CTLS2),
            0,
            // Enable use of extended page tables.
            PROCBASED_CTLS2_EPT |
            // Enable use of RDTSCP instruction.
            PROCBASED_CTLS2_RDTSCP |
            // Enable X2APIC.
            PROCBASED_CTLS2_X2APIC |
            // Associate cached translations of linear
            // addresses with a virtual processor ID.
            PROCBASED_CTLS2_VPID |
            // Enable unrestricted guest.
            PROCBASED_CTLS2_UNRESTRICTED_GUEST,
            0,
        )
    }?;

    // Enable use of INVPCID instruction if available.
    // SAFETY: Enabling INVPCID only affects the guest's own translations.
    let _ = unsafe {
        vmcs.set_control(
            VmcsField32::PROCBASED_CTLS2,
            msr(X86_MSR_IA32_VMX_PROCBASED_CTLS2),
            u64::from(vmcs.read(VmcsField32::PROCBASED_CTLS2)),
            PROCBASED_CTLS2_INVPCID,
            0,
        )
    };

    // Setup pin-based VMCS controls.
    // SAFETY: These controls only select which guest events cause VM exits.
    unsafe {
        vmcs.set_control(
            VmcsField32::PINBASED_CTLS,
            msr(X86_MSR_IA32_VMX_TRUE_PINBASED_CTLS),
            msr(X86_MSR_IA32_VMX_PINBASED_CTLS),
            // External interrupts cause a VM exit.
            PINBASED_CTLS_EXT_INT_EXITING |
            // Non-maskable interrupts cause a VM exit.
            PINBASED_CTLS_NMI_EXITING,
            0,
        )
    }?;

    // Setup primary processor-based VMCS controls.
    // SAFETY: These controls only select which guest events cause VM exits, using the MSR
    // bitmaps set below.
    unsafe {
        vmcs.set_control(
            VmcsField32::PROCBASED_CTLS,
            msr(X86_MSR_IA32_VMX_TRUE_PROCBASED_CTLS),
            msr(X86_MSR_IA32_VMX_PROCBASED_CTLS),
            // Enable VM exit when interrupts are enabled.
            PROCBASED_CTLS_INT_WINDOW_EXITING |
            // Enable VM exit on HLT instruction.
            PROCBASED_CTLS_HLT_EXITING |
            // Enable TPR virtualization.
            PROCBASED_CTLS_TPR_SHADOW |
            // Enable VM exit on IO instructions.
            PROCBASED_CTLS_IO_EXITING |
            // Enable use of MSR bitmaps.
            PROCBASED_CTLS_MSR_BITMAPS |
            // Enable secondary processor-based controls.
            PROCBASED_CTLS_PROCBASED_CTLS2,
            // Disable VM exit on CR3 load.
            PROCBASED_CTLS_CR3_LOAD_EXITING |
            // Disable VM exit on CR3 store.
            PROCBASED_CTLS_CR3_STORE_EXITING |
            // Disable VM exit on CR8 load.
            PROCBASED_CTLS_CR8_LOAD_EXITING |
            // Disable VM exit on CR8 store.
            PROCBASED_CTLS_CR8_STORE_EXITING,
        )
    }?;

    // We only enable interrupt-window exiting above to ensure that the
    // processor supports it for later use. So disable it for now.
    vmcs.interrupt_window_exiting(false);

    // Setup VM-exit VMCS controls.
    // SAFETY: These controls return to the host in 64-bit mode, and save and load the guest and
    // host PAT and EFER MSRs, as set in the host state below.
    unsafe {
        vmcs.set_control(
            VmcsField32::EXIT_CTLS,
            msr(X86_MSR_IA32_VMX_TRUE_EXIT_CTLS),
            msr(X86_MSR_IA32_VMX_EXIT_CTLS),
            // Logical processor is in 64-bit mode after VM
            // exit. On VM exit CS.L, IA32_EFER.LME, and
            // IA32_EFER.LMA is set to true.
            EXIT_CTLS_64BIT_MODE |
            // Acknowledge external interrupt on exit.
            EXIT_CTLS_ACK_INT_ON_EXIT |
            // Save the guest IA32_PAT MSR on exit.
            EXIT_CTLS_SAVE_IA32_PAT |
            // Load the host IA32_PAT MSR on exit.
            EXIT_CTLS_LOAD_IA32_PAT |
            // Save the guest IA32_EFER MSR on exit.
            EXIT_CTLS_SAVE_IA32_EFER |
            // Load the host IA32_EFER MSR on exit.
            EXIT_CTLS_LOAD_IA32_EFER,
            0,
        )
    }?;

    // Whether we are configuring the base processor. The base processor starts in
    // 64-bit mode with all features enabled. For secondary processors, they must
    // be bootstrapped by the operating system.
    let is_base_processor = vpid == BASE_PROCESSOR_VPID;

    // Setup VM-entry VMCS controls.
    // Load the guest IA32_PAT MSR and IA32_EFER MSR on entry.
    let mut entry_ctls = ENTRY_CTLS_LOAD_IA32_PAT | ENTRY_CTLS_LOAD_IA32_EFER;
    if is_base_processor {
        // On the BSP, go straight to 64-bit mode on entry.
        entry_ctls |= ENTRY_CTLS_64BIT_MODE;
    }
    // SAFETY: These controls load the guest PAT and EFER MSRs, and select the guest's mode, on VM
    // entry, which only affects the guest.
    unsafe {
        vmcs.set_control(
            VmcsField32::ENTRY_CTLS,
            msr(X86_MSR_IA32_VMX_TRUE_ENTRY_CTLS),
            msr(X86_MSR_IA32_VMX_ENTRY_CTLS),
            entry_ctls,
            0,
        )
    }?;

    // Enable use of PAUSE-loop exiting if available.
    // SAFETY: PAUSE-loop exiting only selects when VM exits occur.
    let result = unsafe {
        vmcs.set_control(
            VmcsField32::PROCBASED_CTLS2,
            msr(X86_MSR_IA32_VMX_PROCBASED_CTLS2),
            u64::from(vmcs.read(VmcsField32::PROCBASED_CTLS2)),
            PROCBASED_CTLS2_PAUSE_LOOP_EXITING,
            0,
        )
    };
    if result.is_ok() {
        // From Volume 3, Section 25.1.3: The processor determines the amount of
        // time between this execution of PAUSE and the previous execution of PAUSE
        // at CPL 0. If this amount of time exceeds the value of the VM-execution
        // control field PLE_Gap, the processor considers this execution to be the
        // first execution of PAUSE in a loop. (It also does so for the first
        // execution of PAUSE at CPL 0 after VM entry.)
        //
        // Otherwise, the processor determines the amount of time since the most
        // recent execution of PAUSE that was considered to be the first in a loop.
        // If this amount of time exceeds the value of the VM-execution control
        // field PLE_Window, a VM exit occurs.
        //
        // For purposes of these computations, time is measured based on a counter
        // that runs at the same rate as the timestamp counter (TSC).
        //
        // NOTE: These values are based on KVM, which was based on empirical
        // analysis.
        // SAFETY: The PAUSE-loop exiting gap and window only affect when VM exits occur.
        unsafe {
            vmcs.write(VmcsField32::PLE_GAP, 1 << 7);
            vmcs.write(VmcsField32::PLE_WINDOW, 1 << 12);
        }
    }

    // From Volume 3, Section 24.6.3: The exception bitmap is a 32-bit field
    // that contains one bit for each exception. When an exception occurs,
    // its vector is used to select a bit in this field. If the bit is 1,
    // the exception causes a VM exit. If the bit is 0, the exception is
    // delivered normally through the IDT, using the descriptor
    // corresponding to the exception's vector.
    //
    // From Volume 3, Section 25.2: If software desires VM exits on all page
    // faults, it can set bit 14 in the exception bitmap to 1 and set the
    // page-fault error-code mask and match fields each to 00000000H.
    // SAFETY: The exception bitmap and page-fault error-code fields only affect when VM exits
    // occur.
    unsafe {
        vmcs.write(VmcsField32::EXCEPTION_BITMAP, 0);
        vmcs.write(VmcsField32::PAGEFAULT_ERRORCODE_MASK, 0);
        vmcs.write(VmcsField32::PAGEFAULT_ERRORCODE_MATCH, 0);
    }

    // From Volume 3, Section 28.1: Virtual-processor identifiers (VPIDs)
    // introduce to VMX operation a facility by which a logical processor may
    // cache information for multiple linear-address spaces. When VPIDs are
    // used, VMX transitions may retain cached information and the logical
    // processor switches to a different linear-address space.
    //
    // From Volume 3, Section 26.2.1.1: If the "enable VPID" VM-execution
    // control is 1, the value of the VPID VM-execution control field must not
    // be 0000H.
    //
    // From Volume 3, Section 28.3.3.3: If EPT is in use, the logical processor
    // associates all mappings it creates with the value of bits 51:12 of
    // current EPTP. If a VMM uses different EPTP values for different guests,
    // it may use the same VPID for those guests.
    //
    // From Volume 3, Section 28.3.3.1: Operations that architecturally
    // invalidate entries in the TLBs or paging-structure caches independent of
    // VMX operation (e.g., the INVLPG and INVPCID instructions) invalidate
    // linear mappings and combined mappings. They are required to do so only
    // for the current VPID (but, for combined mappings, all EP4TAs). Linear
    // mappings for the current VPID are invalidated even if EPT is in use.
    // Combined mappings for the current VPID are invalidated even if EPT is
    // not in use.
    // SAFETY: The VPID only tags the guest's cached translations, which are invalidated below.
    unsafe { vmcs.write(VmcsField16::VPID, vpid) };
    invvpid(InvVpid::SingleContext, vpid, 0);

    // From Volume 3, Section 28.2: The extended page-table mechanism (EPT) is a
    // feature that can be used to support the virtualization of physical
    // memory. When EPT is in use, certain addresses that would normally be
    // treated as physical addresses (and used to access memory) are instead
    // treated as guest-physical addresses. Guest-physical addresses are
    // translated by traversing a set of EPT paging structures to produce
    // physical addresses that are used to access memory.
    let eptp = ept_pointer_from_pml4(ept_pml4.0);
    // SAFETY: The caller guarantees that `ept_pml4` is the guest's EPT PML4.
    unsafe { vmcs.write(VmcsField64::EPT_POINTER, eptp) };

    // Setup MSR handling.
    // SAFETY: The caller guarantees that `msr_bitmaps_address` is the guest's MSR bitmaps.
    unsafe { vmcs.write(VmcsField64::MSR_BITMAPS_ADDRESS, msr_bitmaps_address.0 as u64) };

    // Setup VMCS host state.
    //
    // NOTE: We are pinned to a thread when executing this function, therefore
    // it is acceptable to use per-CPU state.
    // SAFETY: The caller guarantees `%gs.base` is initialized and `vmcs` has not been invalidated,
    // so interrupts are disabled on this CPU.
    let (cpu_num, default_tss) = unsafe { percpu_cpu_num_and_default_tss() };
    // SAFETY: Reading the control registers has no side effects at CPL 0.
    let (cr0, cr3, cr4) = unsafe { (x86_get_cr0(), x86_get_cr3(), x86_get_cr4()) };
    // SAFETY: The host state is that of this CPU, which the thread is pinned to, with `vmx_state`
    // as the stack and `vmx_exit_asm` as the entry point on VM exit. The caller guarantees that
    // `vmx_state` remains valid while the VMCS is in use.
    unsafe {
        vmcs.write(VmcsField32::HOST_IA32_SYSENTER_CS, 0);
        vmcs.write(VmcsFieldXX::HOST_IA32_SYSENTER_ESP, 0);
        vmcs.write(VmcsFieldXX::HOST_IA32_SYSENTER_EIP, 0);
        vmcs.write(VmcsField64::HOST_IA32_PAT, msr(X86_MSR_IA32_PAT));
        vmcs.write(VmcsField64::HOST_IA32_EFER, msr(X86_MSR_IA32_EFER));
        vmcs.write(VmcsFieldXX::HOST_CR0, cr0);
        vmcs.write(VmcsFieldXX::HOST_CR3, cr3);
        vmcs.write(VmcsFieldXX::HOST_CR4, cr4);
        vmcs.write(VmcsField16::HOST_ES_SELECTOR, 0);
        vmcs.write(VmcsField16::HOST_CS_SELECTOR, CODE_64_SELECTOR);
        vmcs.write(VmcsField16::HOST_SS_SELECTOR, DATA_SELECTOR);
        vmcs.write(VmcsField16::HOST_DS_SELECTOR, 0);
        vmcs.write(VmcsField16::HOST_FS_SELECTOR, 0);
        vmcs.write(VmcsField16::HOST_GS_SELECTOR, 0);
        vmcs.write(VmcsField16::HOST_TR_SELECTOR, tss_selector(cpu_num));
        vmcs.write(VmcsFieldXX::HOST_FS_BASE, msr(X86_MSR_IA32_FS_BASE));
        vmcs.write(VmcsFieldXX::HOST_GS_BASE, msr(X86_MSR_IA32_GS_BASE));
        vmcs.write(VmcsFieldXX::HOST_TR_BASE, default_tss);
        vmcs.write(VmcsFieldXX::HOST_GDTR_BASE, gdt_get() as u64);
        vmcs.write(VmcsFieldXX::HOST_IDTR_BASE, idt_get_readonly().expose_provenance() as u64);
        vmcs.write(VmcsFieldXX::HOST_RSP, vmx_state.expose_provenance() as u64);
        vmcs.write(VmcsFieldXX::HOST_RIP, vmx_exit_asm as unsafe extern "C" fn() as usize as u64);
    }

    // Setup VMCS guest state.
    let mut cr0 = X86_CR0_ET | // Enable extension type
                  X86_CR0_NE | // Enable internal x87 exception handling
                  X86_CR0_WP; // Enable supervisor write protect
    if is_base_processor {
        // Enable protected mode and paging on the primary VCPU.
        cr0 |= X86_CR0_PE | // Enable protected mode
               X86_CR0_PG; // Enable paging
    }
    if cr0_is_invalid(vmcs, cr0) {
        return Err(Status::BAD_STATE);
    }
    // SAFETY: GUEST_CR0 is guest state.
    unsafe { vmcs.write(VmcsFieldXX::GUEST_CR0, cr0) };

    // From Volume 3, Section 27.5.1: The following bits are not modified: For
    // CR0, ET, CD, NW; [the reserved bits], and any bits that are fixed in VMX
    // operation.
    //
    // Any bit that is not restored must be masked, or the guest will be able to
    // affect the host's cr0. However, we do not need to mask:
    //   * The reserved bits, which will generate GP faults;
    //   * ET, which is fixed to 1 (Volume 3 Section 2.5);
    //   * The bits that are fixed in VMX operation aside from PE and PG for
    //     unrestricted guests, which will generate GP faults (Volume 3 Section
    //     25.3);
    //
    // Additionally, NE is fixed in VMX operation but some guests will attempt to
    // clear it without handling the GP fault. So it should also be masked.
    // SAFETY: The mask covers the bits not restored on VM exit, as described above.
    unsafe {
        vmcs.write(
            VmcsFieldXX::CR0_GUEST_HOST_MASK,
            X86_CR0_ET | X86_CR0_NE | X86_CR0_NW | X86_CR0_CD,
        )
    };

    // From Volume 3, Section 9.1.1: Following power-up, The state of control
    // register CR0 is 60000010H (CD and ET are set.)
    // SAFETY: The CR0 read shadow is only observed by the guest.
    unsafe { vmcs.write(VmcsFieldXX::CR0_READ_SHADOW, X86_CR0_ET) };

    let xsave = xsave_supported();
    // Enable FXSAVE, VMX, FSGSBASE, and (if supported) XSAVE.
    let mut cr4 = X86_CR4_OSFXSR | X86_CR4_VMXE | X86_CR4_FSGSBASE;
    if xsave {
        cr4 |= X86_CR4_OSXSAVE;
    }
    if is_base_processor {
        // Enable PAE and PGE on the BSP.
        cr4 |= X86_CR4_PAE | X86_CR4_PGE;
    }
    // SAFETY: `vmcs` is a loaded VMCS, so the CPU supports VMX, and these are the CR4 fixed MSRs.
    if unsafe { cr_is_invalid(cr4, X86_MSR_IA32_VMX_CR4_FIXED0, X86_MSR_IA32_VMX_CR4_FIXED1) } {
        return Err(Status::BAD_STATE);
    }
    // SAFETY: The guest state, CR0/CR4 guest/host masks and read shadows only affect the guest,
    // with host state restored on VM exit.
    unsafe {
        vmcs.write(VmcsFieldXX::GUEST_CR4, cr4);

        // Mask access to CR4.
        vmcs.write(VmcsFieldXX::CR4_GUEST_HOST_MASK, X86_CR4_LA57 | X86_CR4_VMXE);
        vmcs.write(VmcsFieldXX::CR4_READ_SHADOW, 0);

        vmcs.write(VmcsField64::GUEST_IA32_PAT, msr(X86_MSR_IA32_PAT));

        let mut guest_efer = msr(X86_MSR_IA32_EFER);
        if !is_base_processor {
            // Disable LME and LMA on all but the BSP.
            guest_efer &= !(X86_EFER_LME | X86_EFER_LMA);
        }
        vmcs.write(VmcsField64::GUEST_IA32_EFER, guest_efer);

        let mut cs_access_rights = GUEST_XX_ACCESS_RIGHTS_DEFAULT
            | GUEST_XX_ACCESS_RIGHTS_TYPE_E
            | GUEST_XX_ACCESS_RIGHTS_TYPE_CODE;
        if is_base_processor {
            // Ensure that the BSP starts with a 64-bit code segment.
            cs_access_rights |= GUEST_XX_ACCESS_RIGHTS_L;
        }
        vmcs.write(VmcsField32::GUEST_CS_ACCESS_RIGHTS, cs_access_rights);

        vmcs.write(
            VmcsField32::GUEST_TR_ACCESS_RIGHTS,
            GUEST_TR_ACCESS_RIGHTS_TSS_BUSY | GUEST_XX_ACCESS_RIGHTS_P,
        );

        vmcs.write(VmcsField32::GUEST_SS_ACCESS_RIGHTS, GUEST_XX_ACCESS_RIGHTS_DEFAULT);
        vmcs.write(VmcsField32::GUEST_DS_ACCESS_RIGHTS, GUEST_XX_ACCESS_RIGHTS_DEFAULT);
        vmcs.write(VmcsField32::GUEST_ES_ACCESS_RIGHTS, GUEST_XX_ACCESS_RIGHTS_DEFAULT);
        vmcs.write(VmcsField32::GUEST_FS_ACCESS_RIGHTS, GUEST_XX_ACCESS_RIGHTS_DEFAULT);
        vmcs.write(VmcsField32::GUEST_GS_ACCESS_RIGHTS, GUEST_XX_ACCESS_RIGHTS_DEFAULT);

        vmcs.write(
            VmcsField32::GUEST_LDTR_ACCESS_RIGHTS,
            GUEST_XX_ACCESS_RIGHTS_TYPE_W | GUEST_XX_ACCESS_RIGHTS_P,
        );

        if is_base_processor {
            // Use GUEST_RIP to set the entry point on the BSP.
            vmcs.write(VmcsFieldXX::GUEST_CS_BASE, 0);
            vmcs.write(VmcsField16::GUEST_CS_SELECTOR, 0);
            vmcs.write(VmcsFieldXX::GUEST_RIP, entry as u64);
        } else {
            // Use CS to set the entry point on APs.
            vmcs.write(VmcsFieldXX::GUEST_CS_BASE, entry as u64);
            vmcs.write(VmcsField16::GUEST_CS_SELECTOR, (entry >> 4) as u16);
            vmcs.write(VmcsFieldXX::GUEST_RIP, 0);
        }
        vmcs.write(VmcsField32::GUEST_CS_LIMIT, 0xffff);
        vmcs.write(VmcsFieldXX::GUEST_TR_BASE, 0);
        vmcs.write(VmcsField16::GUEST_TR_SELECTOR, 0);
        vmcs.write(VmcsField32::GUEST_TR_LIMIT, 0xffff);
        vmcs.write(VmcsFieldXX::GUEST_DS_BASE, 0);
        vmcs.write(VmcsField32::GUEST_DS_LIMIT, 0xffff);
        vmcs.write(VmcsFieldXX::GUEST_SS_BASE, 0);
        vmcs.write(VmcsField32::GUEST_SS_LIMIT, 0xffff);
        vmcs.write(VmcsFieldXX::GUEST_ES_BASE, 0);
        vmcs.write(VmcsField32::GUEST_ES_LIMIT, 0xffff);
        vmcs.write(VmcsFieldXX::GUEST_FS_BASE, 0);
        vmcs.write(VmcsField32::GUEST_FS_LIMIT, 0xffff);
        vmcs.write(VmcsFieldXX::GUEST_GS_BASE, 0);
        vmcs.write(VmcsField32::GUEST_GS_LIMIT, 0xffff);
        vmcs.write(VmcsField32::GUEST_LDTR_LIMIT, 0xffff);
        vmcs.write(VmcsFieldXX::GUEST_GDTR_BASE, 0);
        vmcs.write(VmcsField32::GUEST_GDTR_LIMIT, 0xffff);
        vmcs.write(VmcsFieldXX::GUEST_IDTR_BASE, 0);
        vmcs.write(VmcsField32::GUEST_IDTR_LIMIT, 0xffff);

        // Set all reserved RFLAGS bits to their correct values
        vmcs.write(VmcsFieldXX::GUEST_RFLAGS, X86_FLAGS_RESERVED_ONES);

        vmcs.write(VmcsField32::GUEST_ACTIVITY_STATE, 0);
        vmcs.write(VmcsField32::GUEST_INTERRUPTIBILITY_STATE, 0);
        vmcs.write(VmcsFieldXX::GUEST_PENDING_DEBUG_EXCEPTIONS, 0);

        // From Volume 3, Section 26.3.1.1: The IA32_SYSENTER_ESP field and the
        // IA32_SYSENTER_EIP field must each contain a canonical address.
        vmcs.write(VmcsFieldXX::GUEST_IA32_SYSENTER_ESP, 0);
        vmcs.write(VmcsFieldXX::GUEST_IA32_SYSENTER_EIP, 0);
        vmcs.write(VmcsField32::GUEST_IA32_SYSENTER_CS, 0);

        vmcs.write(VmcsFieldXX::GUEST_CR3, 0);
        vmcs.write(VmcsFieldXX::GUEST_RSP, 0);

        // From Volume 3, Section 24.4.2: If the "VMCS shadowing" VM-execution
        // control is 1, the VMREAD and VMWRITE instructions access the VMCS
        // referenced by this pointer (see Section 24.10). Otherwise, software
        // should set this field to FFFFFFFF_FFFFFFFFH to avoid VM-entry
        // failures (see Section 26.3.1.5).
        vmcs.write(VmcsField64::LINK_POINTER, LINK_POINTER_INVALIDATE);
    }

    if xsave {
        // SAFETY: XSAVE is supported, and so enabled by the kernel (`CR4.OSXSAVE == 1`). The caller
        // guarantees that `extended_register_init` has completed on CPU 0 and `vmx_state` and
        // `extended_register_state`, which is a zeroed XSAVE area, are valid for writes.
        unsafe {
            // Enable x87, SSE, and (if enabled in host XCR0) AVX in initial guest XCR0.
            let host_xcr0 = xgetbv(0);
            (*vmx_state).host_state.xcr0 = host_xcr0;
            (*vmx_state).guest_state.xcr0 = host_xcr0 & GUEST_XCR0_MASK;
            extended_register_init_state_from_bv(
                extended_register_state,
                (*vmx_state).guest_state.xcr0,
            );
        }
    } else {
        // SAFETY: The caller guarantees `extended_register_init` has completed on CPU 0 and
        // `extended_register_state` is a 64-byte-aligned `X86_MAX_EXTENDED_REGISTER_SIZE` buffer
        // valid for writes.
        unsafe {
            extended_register_init_state(extended_register_state);
        }
    }

    Ok(())
}

#[cold]
#[inline(never)]
fn log_invalid_interrupt_vector(vector: u32) {
    dprintf!(INFO, "Invalid interrupt vector: {}\n", vector);
}

#[cold]
#[inline(never)]
fn log_vcpu_enter_failed(error: u32) {
    dprintf!(INFO, "VCPU enter failed: Instruction error {}\n", error);
}

/// Injects an interrupt into the guest, if there is one pending.
fn local_apic_maybe_interrupt(
    vmcs: &mut AutoVmcs,
    local_apic_state: &LocalApicState,
) -> Result<(), Status> {
    const NMI: u32 = X86_INT_NMI.0 as u32;
    const VIRT: u32 = X86_INT_VIRT.0 as u32;
    const PLATFORM_BASE: u32 = X86_INT_PLATFORM_BASE.0 as u32;

    // Since hardware generated exceptions are delivered to the guest directly,
    // the only exceptions we see here are those we generate in the VMM, e.g. GP
    // faults in vmexit handlers. Therefore we simplify interrupt priority to 1)
    // NMIs, 2) interrupts, and 3) generated exceptions. See Volume 3, Section
    // 6.9, Table 6-2.
    let vector;
    let pending = local_apic_state.interrupt_tracker.try_pop(NMI);
    if pending {
        vector = NMI;
    } else {
        // Pop scans vectors from highest to lowest, which will correctly pop
        // interrupts before exceptions. All vectors <= X86_INT_VIRT except the NMI
        // vector are exceptions.
        let Some(popped) = local_apic_state.interrupt_tracker.pop() else {
            return Ok(());
        };
        vector = popped;
        // If type isn't inactive, then Pop should have initialized vector to a
        // valid value.
        debug_assert!(vector != X86InterruptVector::COUNT as u32);
    }

    // NMI injection is blocked if an NMI is already being serviced (Volume 3,
    // Section 24.4.2, Table 24-3), and mov ss blocks *all* interrupts (Volume 2
    // Section 4.3 MOV-Move instruction). Note that the IF flag does not affect
    // NMIs (Volume 3, Section 6.8.1).
    let can_inject_nmi = |vmcs: &AutoVmcs| {
        (vmcs.read(VmcsField32::GUEST_INTERRUPTIBILITY_STATE)
            & (INTERRUPTIBILITY_NMI_BLOCKING | INTERRUPTIBILITY_MOV_SS_BLOCKING))
            == 0
    };
    // External interrupts can be blocked due to STI, move SS or the IF flag.
    let can_inject_external_int = |vmcs: &AutoVmcs| {
        (vmcs.read(VmcsFieldXX::GUEST_RFLAGS) & X86_FLAGS_IF) != 0
            && (vmcs.read(VmcsField32::GUEST_INTERRUPTIBILITY_STATE)
                & (INTERRUPTIBILITY_STI_BLOCKING | INTERRUPTIBILITY_MOV_SS_BLOCKING))
                == 0
    };

    if vector > VIRT && vector < PLATFORM_BASE {
        core::hint::cold_path();
        log_invalid_interrupt_vector(vector);
        return Err(Status::NOT_SUPPORTED);
    } else if (vector >= PLATFORM_BASE && !can_inject_external_int(vmcs))
        || (vector == NMI && !can_inject_nmi(vmcs))
    {
        let result = local_apic_state.interrupt_tracker.track(vector);
        if result.is_ok() {
            // If interrupts are disabled, we set VM exit on interrupt enable.
            vmcs.interrupt_window_exiting(true);
        } else {
            core::hint::cold_path();
        }
        return result;
    }

    // If the vector is non-maskable or interrupts are enabled, inject interrupt.
    vmcs.issue_interrupt(vector);

    // Volume 3, Section 6.9: Lower priority exceptions are discarded; lower
    // priority interrupts are held pending. Discarded exceptions are re-generated
    // when the interrupt handler returns execution to the point in the program or
    // task where the exceptions and/or interrupts occurred.
    local_apic_state.interrupt_tracker.clear(0, NMI);
    local_apic_state.interrupt_tracker.clear(NMI + 1, VIRT + 1);

    Ok(())
}

/// Launch/resume the guest, and return when the guest next exits.
///
/// If we return ZX_OK, the guest was successfully launched and has now
/// exited again. Otherwise, we failed to launch the guest.
///
/// # Safety
///
/// The caller must ensure that interrupts are disabled and `vmx_state` is valid for reads and
/// writes. If a VMCS is current on this CPU, its host state must have been set up by `vmcs_init`
/// (and updated by `Vcpu::migrate` after any CPU migration) for `vmx_state` and this CPU, and its
/// guest state must be the state of a guest of the hypervisor.
unsafe fn vmx_enter(vmx_state: *mut VmxState) -> Result<(), Status> {
    // Perform the low-level vmlaunch or vmresume, entering the guest,
    // and returning when the guest exits.
    // SAFETY: Guaranteed by the caller.
    let status = unsafe { vmx_enter_asm(vmx_state) };

    debug_assert!(ints_disabled());

    // Reload the task segment in order to restore its limit. VMX always
    // restores it with a limit of 0x67, which excludes the IO bitmap.
    let selector = tss_selector(curr_cpu_num());
    // SAFETY: `selector` selects the TSS of this CPU, which VMX marked busy on VM exit, and which
    // only this CPU uses. Interrupts are disabled, so nothing else accesses it meanwhile.
    unsafe {
        clear_tss_busy(selector);
        x86_ltr(selector);
    }

    Status::ok(status)
}

/// Represents a virtual CPU within a guest.
///
/// `#[repr(C)]` keeps the hot scalar, atomic, MSR, and `VmxState` fields within `+0x00..+0x7f`
/// (`disp8` addressing) ahead of the 64-byte-aligned 1024-byte `extended_register_state` at the
/// end of the struct.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct Vcpu {
    guest: NonNull<Guest>,
    vpid: u16,
    kicked: AtomicBool,
    entered: AtomicBool,
    in_use: AtomicBool,
    /// `last_cpu` contains the CPU dedicated to holding the guest's VMCS state,
    /// or INVALID_CPU if there is no such VCPU. If this Vcpu is actively running,
    /// then `last_cpu` will point to that CPU.
    ///
    /// The VMCS state of this Vcpu must not be loaded prior to `last_cpu` being
    /// set, nor must `last_cpu` be modified prior to the VMCS state being cleared.
    ///
    /// Guarded by the lock of `thread`. It is an atomic only so that it can be accessed through a
    /// shared reference; relaxed accesses suffice, as the lock orders them.
    last_cpu: AtomicU32,
    /// `thread` will be set to null when the thread exits during the Exiting
    /// stage of our migration function callback (or in `teardown_shared`).
    ///
    /// Guarded by `Thread::get_list_lock()`. See `thread_is_our_thread` for why it is an atomic.
    thread: AtomicPtr<Thread>,
    vmcs_page: VmxPage,
    msr_state: UnsafeCell<MsrState>,
    vmx_state: UnsafeCell<VmxState>,
    #[pin]
    local_apic_state: LocalApicState,
    pv_clock_state: UnsafeCell<PvClockState>,
    /// The guest may enable any state, so the XSAVE area is the maximum size.
    extended_register_state: UnsafeCell<ExtendedRegisterState>,
    /// `thread`'s migrate and context-switch callbacks hold raw pointers to this `Vcpu`.
    #[pin]
    _pin: PhantomData<PhantomPinned>,
}

// When lockdep is enabled, `LocalApicState`'s `TrackedSpinlock` pushes `extended_register_state`
// from offset 512 to the next 64-byte boundary (576).
const EXPECTED_VCPU_SIZE: usize = if cfg!(with_lock_dep) { 1600 } else { 1536 };

static_assert!(size_of::<Vcpu>() == EXPECTED_VCPU_SIZE);
static_assert!(align_of::<Vcpu>() == 64);

// SAFETY: `guest` outlives the VCPU (see `Vcpu::create`), and only shared references to the
// `Guest`, which is `Sync`, are created from it. `vmx_state`, `msr_state`, `pv_clock_state` and
// `extended_register_state` are only accessed by the thread of the VCPU in a non-reentrant normal
// thread context (enforced by `enter_our_thread` and `in_use`), or by its migrate and context
// switch functions, which run on behalf of that thread while it does not access them (see
// `enter`). The remaining fields are atomics or synchronize internally.
unsafe impl Send for Vcpu {}
// SAFETY: See `Send` above.
unsafe impl Sync for Vcpu {}

// SAFETY: `Vcpu` is allocated via `kalloc::Box` in `Vcpu::create` and destroyed below by
// unregistering concurrent callbacks and canceling `local_apic_state.timer` through `&Self` before
// reconstructing and dropping the `kalloc::Box`.
unsafe impl Recyclable for Vcpu {
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is the unique owning pointer to a live, pinned `Vcpu`
    /// allocated via `kalloc::Box` in `Vcpu::create` (whose `guest` is still alive), that no
    /// external references to `*ptr` exist (only `thread`'s callbacks and `local_apic_state.timer`
    /// may hold shared references until `teardown_shared` completes), that `ptr` is not used after
    /// this call, and that this is called in a normal thread context where blocking is permitted
    /// and no conflicting locks are held.
    unsafe fn recycle(ptr: NonNull<Self>) {
        // Unregister `thread`'s callbacks and cancel `local_apic_state.timer` through a shared
        // reference before `kalloc::Box::from_non_null` / `PinnedDrop::drop` forms an exclusive
        // `&mut Self` covering `Freeze` fields (which would race with `migrate_fn`,
        // `context_switch_fn`, or `deadline_callback` under Stacked/Tree Borrows and violate
        // `vmexit_handler`'s `# Safety` contract on `local_apic_state`).
        // SAFETY: `ptr` points to a valid `Vcpu` owned by the `UniquePtr` being dropped, and only
        // shared references (`&Vcpu`, `&LocalApicState`) can exist concurrently until
        // `teardown_shared` completes.
        unsafe { ptr.as_ref() }.teardown_shared();
        // SAFETY: `ptr` was allocated via `kalloc::Box` in `Vcpu::create`, and all concurrent
        // callbacks and timers have been unregistered and canceled above, so `ptr` is uniquely
        // owned.
        let _ = unsafe { kalloc::Box::from_non_null(ptr) };
    }
}

struct VcpuInUseGuard<'a>(&'a Vcpu);

impl Drop for VcpuInUseGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.0.in_use.store(false, Ordering::Release);
    }
}

impl Vcpu {
    /// Creates a new `Vcpu` within `guest`, which begins execution at `entry`, and runs on the
    /// calling thread.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `guest` outlives the returned `Vcpu`, and that this is called
    /// in a normal thread context (`!ints_disabled()`, `!blocking_disallowed()`, and no spinlocks
    /// or active `ChainLockTransaction` held on the calling thread).
    pub unsafe fn create(guest: &Guest, entry: zx_vaddr_t) -> Result<Pin<UniquePtr<Self>>, Status> {
        debug_assert!(!ints_disabled() && !blocking_disallowed() && num_spinlocks_held() == 0);
        let root_vmar = guest.root_vmar();
        if entry < root_vmar.base().0 || entry >= root_vmar.base().0 + root_vmar.size() {
            return Err(Status::INVALID_ARGS);
        }

        let thread = current_get();
        // SAFETY: `thread` is the live current thread, and this temporary `&Thread` is dropped
        // before any call that can reschedule or migrate `thread`.
        if unsafe { &*thread }.vcpu() {
            return Err(Status::BAD_STATE);
        }

        let vpid = guest.try_alloc_vpid()?;

        let mut vcpu = match kalloc::Box::try_pin_init(Self::init(guest, vpid, thread)) {
            Ok(vcpu) => vcpu,
            Err(_) => {
                let result = guest.free_vpid(vpid);
                debug_assert!(result.is_ok());
                return Err(Status::NO_MEMORY);
            }
        };
        // SAFETY: `thread` is the live current thread, and this temporary `&Thread` is dropped
        // before `vmcs_page.alloc` (which can reschedule or migrate `thread`).
        unsafe { &*thread }.set_vcpu(true);

        // SAFETY: `guest` exists, so `alloc_vmx_state` succeeded and the CPU supports VMX.
        let vmx_info = unsafe { VmxInfo::new() };
        {
            let this = vcpu.as_mut().project();

            // Setup PV clock state before converting to `UniquePtr` so no `&mut` projection
            // retags `Vcpu` after `vcpu_ptr` is derived.
            this.pv_clock_state.get_mut().set_is_stable(if x86_hypervisor_has_pv_clock() {
                pv_clock_is_stable()
            } else {
                x86_feature_test(X86_FEATURE_INVAR_TSC)
            });

            this.vmcs_page.alloc(&vmx_info, 0)?;

            let region = this.vmcs_page.virtual_address::<VmxRegion>();
            // SAFETY: `region` points to the start of the VMCS page allocated above, which nothing
            // else accesses yet.
            unsafe { (*region).revision_id = vmx_info.revision_id };
        }

        // SAFETY: Only the pointer to the VCPU moves, not the VCPU, and the `UniquePtr` is pinned
        // again below. `Recyclable for Vcpu` frees the VCPU as a `kalloc::Box`.
        let vcpu =
            unsafe { UniquePtr::from_raw(kalloc::Box::into_raw(Pin::into_inner_unchecked(vcpu))) };
        let vcpu_ptr = UniquePtr::as_ptr(&vcpu);
        // SAFETY: `UniquePtr` never moves its pointee, and the VCPU was pinned above.
        let vcpu = unsafe { Pin::new_unchecked(vcpu) };

        let ept_pml4 = guest.physical_aspace().arch_aspace().arch_table_phys();
        let vmcs_address = vcpu.vmcs_page.physical_address();
        // We create the `AutoVmcs` object here, so that we ensure that interrupts are
        // disabled from `vmcs_init` until `set_migrate_fn_locked`. This is important to ensure
        // that we do not migrate CPUs while setting up the VCPU.
        // SAFETY: `vmcs_address` is the VMCS page of the VCPU, which is only accessed through VMX
        // instructions, and remains allocated until the VCPU is dropped, which clears it. The
        // calling thread does not block, nor make another VMCS current, until `vmcs` is invalidated
        // or dropped.
        let mut vmcs = unsafe {
            AutoVmcs::new(vmcs_address, /*clear=*/ true)
        };
        vcpu.last_cpu.store(curr_cpu_num(), Ordering::Relaxed);
        // SAFETY: `vmcs` has not been invalidated (so interrupts are disabled and its VMCS is
        // current on this CPU), CPL is 0, `%gs.base` and extended registers are initialized, and
        // `vmx_state` and `extended_register_state` are fields of the pinned VCPU, which owns the
        // VMCS.
        unsafe {
            vmcs_init(
                &mut vmcs,
                vpid,
                entry,
                guest.msr_bitmaps_address(),
                ept_pml4,
                vcpu.vmx_state.get(),
                vcpu.extended_register_state.get().cast(),
            )
        }?;

        let vcpu_cb_ptr = VcpuCallbackPtr(vcpu_ptr);
        let migrate_fn = MigrateFn::new(move |thread: &Thread, stage: MigrateStage| {
            let vcpu_ptr = vcpu_cb_ptr;
            // SAFETY: `vcpu_ptr` points to the pinned `Vcpu`, which clears `thread`'s migrate
            // function in `teardown_shared` before `Vcpu` is dropped (unless `thread` has already
            // exited), and `Thread` invokes its migrate callback with `thread`'s lock held,
            // `%gs.base` initialized, and interrupts disabled (for `Save` and `Restore`).
            unsafe {
                (&*vcpu_ptr.0).migrate(thread, stage);
            }
        });
        let context_switch_fn = ContextSwitchFn::new(move || {
            let vcpu_ptr = vcpu_cb_ptr;
            // SAFETY: `vcpu_ptr` points to the pinned `Vcpu`, which clears `thread`'s context
            // switch function in `teardown_shared` before `Vcpu` is dropped (and in
            // `MigrateStage::Exiting` when `thread` exits).
            let vcpu = unsafe { &*vcpu_ptr.0 };
            if vcpu.entered.load(Ordering::SeqCst) {
                // `arch_context_switch()` saves and restores GS, so we can skip it.
                // SAFETY: This is the context switch function of the thread of the VCPU.
                unsafe {
                    vcpu.context_switch(/*include_gs=*/ false)
                };
            }
        });
        let (old_migrate, old_context_switch) = {
            ksync::lock!(let mut list_guard = Thread::get_list_lock().lock());
            // SAFETY: `thread` is the live current thread, and `vmcs` keeps interrupts and
            // preemption disabled so no context switch or migration can form `&mut` borrows on
            // `Thread` subfields while `thread_ref` is live.
            let thread_ref = unsafe { &*thread };
            single_chain_lock_guard_owned!(
                thread_guard,
                thread_ref,
                NO_IRQ_SAVE_OPTION,
                clt_tag!("Vcpu::Create")
            );

            // Only set the thread migrate function after we have initialized the VMCS.
            // Otherwise, the migrate function may interact with an uninitialized VMCS.
            // SAFETY: `thread_lists_init` has completed, `thread_ref` is the live current thread
            // (not erased from thread lists), and no scheduler `queue_lock_` is held.
            let old_migrate = unsafe {
                thread_guard
                    .proof()
                    .set_migrate_fn_locked(Some(migrate_fn), list_guard.as_mut().token_mut())
            };
            let old_context_switch =
                thread_guard.proof().set_context_switch_fn_locked(Some(context_switch_fn));
            (old_migrate, old_context_switch)
        };

        drop(vmcs);
        drop((old_migrate, old_context_switch));
        Ok(vcpu)
    }

    /// Creates an in-place initializer for a `Vcpu` of `guest` with VPID `vpid`, which runs on
    /// `thread`.
    fn init(guest: &Guest, vpid: u16, thread: *mut Thread) -> impl PinInit<Self> {
        pin_init!(Self {
            guest: NonNull::from(guest),
            vpid,
            kicked: AtomicBool::new(false),
            entered: AtomicBool::new(false),
            in_use: AtomicBool::new(false),
            last_cpu: AtomicU32::new(INVALID_CPU),
            thread: AtomicPtr::new(thread),
            vmcs_page: VmxPage::default(),
            msr_state: UnsafeCell::new(MsrState::default()),
            vmx_state: UnsafeCell::new(VmxState::default()),
            local_apic_state <- LocalApicState::init(),
            pv_clock_state: UnsafeCell::new(PvClockState::default()),
            extended_register_state: UnsafeCell::new(ExtendedRegisterState(
                [0; X86_MAX_EXTENDED_REGISTER_SIZE]
            )),
            _pin: PhantomData,
        })
    }

    fn guest(&self) -> &Guest {
        // SAFETY: The guest outlives the VCPU, as `create` requires.
        unsafe { self.guest.as_ref() }
    }

    /// Some explanation is required for why we use a relaxed atomic load here.
    /// `thread` is initially set during our construction in a context where the
    /// caller guarantees that `thread`'s instance cannot exit.  After this, there
    /// will only ever be one more assignment made to this variable, when the thread
    /// exits and calls the registered migration function.  This happens with the
    /// global Thread list_lock held, and is what is used to make sure that the
    /// thread cannot exit out from under us during operations like Kick and
    /// Interrupt.
    ///
    /// There are many other places in the code, however, where the code simply
    /// wants to check to see the calling thread is the same as `thread`.  There
    /// are only two options here.
    ///
    /// 1) The calling thread (A) is not the same as thread as our thread.  If our
    ///    initial thread was B, then thread can only ever have two values; either B
    ///    or nullptr.  A will never match, regardless of whether or not B has
    ///    exited or not yet.  We need the `thread` member to be atomic in order
    ///    to avoid a formal data race, but nothing more.
    /// 2) The calling thread (A) *is* the same as our thread.  This means that our
    ///    thread is currently running, and cannot exit out from under us.  We
    ///    don't even need the atomic load in this case, since it is A who will
    ///    eventually perform the mutation setting thread to nullptr, so no
    ///    concurrent write is possible.
    ///
    /// In either case, we don't actually need to hold the Thread::get_list_lock() in
    /// order to simply check to see if a calling thread is our thread.  All we
    /// need is a relaxed atomic load to deal with case 1 (above).
    fn thread_is_our_thread(&self, thread: *const Thread) -> bool {
        core::ptr::eq(thread, self.thread.load(Ordering::Relaxed))
    }

    /// # Safety
    ///
    /// `thread` must be the thread of the VCPU, and the caller must be its migrate function
    /// invoked with `thread`'s lock held, `%gs.base` initialized to the calling CPU's `x86_percpu`,
    /// and interrupts disabled (for `Save` and `Restore`).
    unsafe fn migrate(&self, thread: &Thread, stage: MigrateStage) {
        // The thread being passed to us must be _our_ thread, and its lock must
        // currently be held (a requirement for calling a Migration function).  Assert
        // this to make the lock analysis happy.
        debug_assert!(self.thread_is_our_thread(thread));
        // SAFETY: The caller guarantees that `thread` is our live thread, whose lock it holds for
        // the duration of this call.
        let thread_proof = unsafe { ChainLockProof::assume_held_unchecked(thread) };

        // Volume 3, Section 31.8.2: An MP-aware VMM is free to assign any logical
        // processor to a VM. But for performance considerations, moving a guest VMCS
        // to another logical processor is slower than resuming that guest VMCS on the
        // same logical processor. Certain VMX performance features (such as caching
        // of portions of the VMCS in the processor) are optimized for a guest VMCS
        // that runs on the same logical processor.
        //
        // If the VMCS regions are identical (same revision ID) the following sequence
        // can be used to move or copy the VMCS from one logical processor to another:
        match stage {
            // * Perform a VMCLEAR operation on the source logical processor. This
            //   ensures that all VMCS data that may be cached by the processor are
            //   flushed to memory.
            MigrateStage::Save => {
                // SAFETY: The VMCS page of the VCPU remains allocated until the VCPU is dropped,
                // which first clears the migrate function, and is only accessed through VMX
                // instructions.
                unsafe { vmclear(self.vmcs_page.physical_address().0) };
                // After VMCLEAR, `last_cpu` can be cleared to indicate this VCPU is both
                // not presently running, and its state is not loaded anywhere.
                self.last_cpu.store(INVALID_CPU, Ordering::Relaxed);
            }
            // * Copy the VMCS region from one memory location to another location. This
            //   is an optional step assuming the VMM wishes to relocate the VMCS or
            //   move the VMCS to another system.
            // * Perform a VMPTRLD of the physical address of VMCS region on the
            //   destination processor to establish its current VMCS pointer.
            MigrateStage::Restore => {
                // Volume 3, Section 31.8.2: To migrate a VMCS to another logical
                // processor, a VMM must use the sequence of VMCLEAR, VMPTRLD and
                // VMLAUNCH.
                //
                // We set `resume` to false so that `vmx_enter` will call VMLAUNCH when
                // entering the guest, instead of VMRESUME.
                // SAFETY: Only our thread, which is being migrated and so does not access
                // `vmx_state`, and its migrate function access `vmx_state`.
                unsafe { (*self.vmx_state.get()).resume = false };

                // Before performing the VMPTRLD, update the `last_cpu` for
                // `Vcpu::interrupt()` and `vmcs_page` state tracking. It is assumed that
                // the `MigrateStage::Save` stage already happened and that a
                // VMCLEAR has been performed on `last_cpu`, hence the previous value of
                debug_assert!(self.last_cpu.load(Ordering::Relaxed) == INVALID_CPU);
                self.last_cpu.store(thread_proof.scheduler_state().last_cpu(), Ordering::Relaxed);

                // Load the VMCS on the destination processor.
                // SAFETY: As for `vmclear` above. The VMCS was cleared by `MigrateStage::Save`, and
                // no `AutoVmcs` that has not been invalidated is live on this CPU, as the migrate
                // function runs while rescheduling, which a live `AutoVmcs` forbids.
                unsafe { vmptrld(self.vmcs_page.physical_address().0) };

                // Update the VMCS with the per-CPU variables of the destination
                // processor.
                // SAFETY: The caller guarantees `%gs.base` is initialized and interrupts are
                // disabled during `MigrateStage::Restore`.
                let (cpu_num, default_tss) = unsafe { percpu_cpu_num_and_default_tss() };
                // SAFETY: The VMCS of the VCPU was made current on this CPU above, and the migrate
                // function is called with interrupts disabled, so it remains current. `thread` is
                // our initialized `Thread` whose lock is held during `MigrateStage::Restore` while
                // `thread` is being migrated (and not running elsewhere), so `thread`'s
                // `arch.fs_base` is initialized and not concurrently mutated or `&mut`-borrowed.
                // These are the host state of this CPU, which the next VM exit restores.
                unsafe {
                    vmwrite(
                        VmcsField16::HOST_TR_SELECTOR.encoding(),
                        u64::from(tss_selector(cpu_num)),
                    );
                    vmwrite(
                        VmcsFieldXX::HOST_FS_BASE.encoding(),
                        x86_thread_fs_base(thread) as u64,
                    );
                    vmwrite(VmcsFieldXX::HOST_GS_BASE.encoding(), read_msr(X86_MSR_IA32_GS_BASE));
                    vmwrite(VmcsFieldXX::HOST_TR_BASE.encoding(), default_tss);
                }

                // Invalidate TLB mappings for the VPID.
                invvpid(InvVpid::SingleContext, self.vpid, 0);
            }
            MigrateStage::Exiting => {
                // Clear `context_switch_fn` while holding `thread`'s lock before nulling
                // `self.thread`: `Thread::current_exit` releases `Thread::get_list_lock()` after
                // `MigrateStage::Exiting` and only later performs the exiting thread's final
                // `Scheduler::context_switch`, so a concurrent `Vcpu::recycle` seeing
                // `self.thread == null` must not race with `context_switch_fn`.
                let _ = thread_proof.set_context_switch_fn_locked(None);
                // The `thread` is exiting and so we must clear our reference to it.
                // The thread list lock is held while the thread exits.
                self.thread.store(core::ptr::null_mut(), Ordering::Relaxed);
            }
        }
    }

    /// # Safety
    ///
    /// Must be called by the thread of the VCPU with interrupts disabled, or by its context switch
    /// function.
    unsafe fn context_switch(&self, include_gs: bool) {
        // SAFETY: Guaranteed by the caller, so nothing else accesses `msr_state` meanwhile.
        let msr_state = unsafe { &mut *self.msr_state.get() };
        // SAFETY: These MSRs hold the system call and TSC_AUX state of the thread running on this
        // CPU, which the guest state of the VCPU replaces while the VCPU is entered, and which is
        // swapped back before any other code depends on it.
        unsafe {
            let star = read_msr(X86_MSR_IA32_STAR);
            let lstar = read_msr(X86_MSR_IA32_LSTAR);
            let fmask = read_msr(X86_MSR_IA32_FMASK);
            let tsc_aux = read_msr(X86_MSR_IA32_TSC_AUX);

            write_msr(X86_MSR_IA32_STAR, msr_state.star);
            write_msr(X86_MSR_IA32_LSTAR, msr_state.lstar);
            write_msr(X86_MSR_IA32_FMASK, msr_state.fmask);
            write_msr(X86_MSR_IA32_TSC_AUX, msr_state.tsc_aux);

            msr_state.star = star;
            msr_state.lstar = lstar;
            msr_state.fmask = fmask;
            msr_state.tsc_aux = tsc_aux;

            if include_gs {
                let kernel_gs_base = read_msr(X86_MSR_IA32_KERNEL_GS_BASE);
                write_msr(X86_MSR_IA32_KERNEL_GS_BASE, msr_state.kernel_gs_base);
                msr_state.kernel_gs_base = kernel_gs_base;
            }
        }
    }

    /// Verifies that `current_thread` is `self.thread` in a normal, non-reentrant thread context,
    /// and claims exclusive access to the VCPU's thread-owned state for the lifetime of the
    /// returned guard.
    #[inline]
    fn enter_our_thread(
        &self,
        current_thread: *const Thread,
    ) -> Result<VcpuInUseGuard<'_>, Status> {
        assert!(!ints_disabled() && !blocking_disallowed() && num_spinlocks_held() == 0);
        if !self.thread_is_our_thread(current_thread) {
            core::hint::cold_path();
            return Err(Status::BAD_STATE);
        }
        if self.in_use.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err()
        {
            core::hint::cold_path();
            return Err(Status::BAD_STATE);
        }
        Ok(VcpuInUseGuard(self))
    }

    /// # Safety
    ///
    /// Must be called by the thread of the VCPU (holding `VcpuInUseGuard`) with interrupts
    /// disabled while `vmcs` is current, at CPL 0 with `CR0.EM == 0`, `CR0.TS == 0`, and
    /// `extended_register_init` completed on CPU 0.
    unsafe fn load_extended_registers(&self, vmcs: &AutoVmcs) {
        let thread = current_get();
        let save_host = xsave_supported();
        let load_guest = save_host && (vmcs.read(VmcsFieldXX::GUEST_CR4) & X86_CR4_OSXSAVE != 0);
        let vmx_state = self.vmx_state.get();
        // SAFETY: The buffers are the 64-byte-aligned XSAVE areas of the current thread and of the
        // VCPU, and only the current thread accesses them and `vmx_state` while interrupts are
        // disabled. Restoring `self.extended_register_state` before narrowing `XCR0` ensures
        // hardware `XCR0` is still `host_xcr0` (a superset of `xcomp_bv`), avoiding `#GP(0)` in
        // `xrstors`. When `load_guest` is true, `xsave_supported()` guarantees `CR4.OSXSAVE == 1`
        // and hardware support for x87 and SSE, and masking `guest_state.xcr0` with
        // `host_xcr0 & GUEST_XCR0_MASK` (preserving x87 and clearing AVX when SSE is clear)
        // guarantees a valid `XCR0` configuration supported by the CPU and fitting within
        // `X86_MAX_EXTENDED_REGISTER_SIZE`.
        unsafe {
            extended_register_save_state(x86_thread_extended_register_buffer(thread));
            let mut host_xcr0 = 0;
            if save_host {
                host_xcr0 = xgetbv(0);
                (*vmx_state).host_state.xcr0 = host_xcr0;
            }
            extended_register_restore_state(self.extended_register_state.get().cast());
            if load_guest {
                let mut guest_xcr0 = ((*vmx_state).guest_state.xcr0 & host_xcr0 & GUEST_XCR0_MASK)
                    | X86_XSAVE_STATE_BIT_X87;
                if guest_xcr0 & X86_XSAVE_STATE_BIT_SSE == 0 {
                    guest_xcr0 &= !X86_XSAVE_STATE_BIT_AVX;
                }
                xsetbv(0, guest_xcr0);
            }
        }
    }

    /// # Safety
    ///
    /// Must be called by the thread of the VCPU (holding `VcpuInUseGuard`) with interrupts
    /// disabled while `vmcs` is current, after a matching `load_extended_registers` on this entry,
    /// at CPL 0 with `CR0.EM == 0`, `CR0.TS == 0`, and `extended_register_init` completed on CPU 0.
    unsafe fn save_extended_registers(&self, vmcs: &AutoVmcs) {
        let thread = current_get();
        let load_host = xsave_supported();
        let save_guest = load_host && (vmcs.read(VmcsFieldXX::GUEST_CR4) & X86_CR4_OSXSAVE != 0);
        let vmx_state = self.vmx_state.get();
        // SAFETY: As in `load_extended_registers`. Widening `XCR0` back to `host_state.xcr0`
        // (saved by `load_extended_registers`) before restoring the host thread's XSAVE area
        // ensures hardware `XCR0` is a superset of the host buffer's `xcomp_bv`.
        unsafe {
            extended_register_save_state(self.extended_register_state.get().cast());
            if save_guest {
                (*vmx_state).guest_state.xcr0 = xgetbv(0);
            }
            if load_host {
                xsetbv(0, (*vmx_state).host_state.xcr0);
            }
            extended_register_restore_state(x86_thread_extended_register_buffer(thread));
        }
    }

    /// Enters the guest, returning once the guest traps, faults, or is kicked.
    ///
    /// While the thread of the VCPU runs this, `vmx_state`, `msr_state`, `pv_clock_state` and
    /// `extended_register_state` are only accessed with interrupts disabled (while an `AutoVmcs`
    /// is valid), except for `vmx_state.guest_state` and `pv_clock_state` while handling a VM exit,
    /// which the migrate and context switch functions do not access.
    pub fn enter(&self, packet: &mut zx_port_packet_t) -> Result<(), Status> {
        let current_thread = current_get();
        let _in_use = self.enter_our_thread(current_thread)?;

        let extended_registers_loaded = Cell::new(false);
        let _defer = zr::defer(|| {
            if extended_registers_loaded.get() {
                // SAFETY: As in `create`.
                let vmcs = unsafe { AutoVmcs::new(self.vmcs_page.physical_address(), false) };
                // SAFETY: This is the thread of the VCPU holding `_in_use`, and `vmcs` disables
                // interrupts.
                unsafe {
                    self.save_extended_registers(&vmcs);
                    self.context_switch(/*include_gs=*/ true);
                }
                self.entered.store(false, Ordering::SeqCst);
            }
            // Spectre V2: Ensure that code executed in the VM guest cannot influence
            // indirect branch prediction in the host.
            //
            // TODO(https://fxbug.dev/42108888): We may be able to avoid the IBPB here; the kernel
            // is either built with a retpoline or has Enhanced IBRS enabled. We
            // currently execute an IBPB on context-switch to a new aspace. The IBPB is
            // currently only here to protect hypervisor user threads.
            if !BootOptions::get().x86_disable_spec_mitigations && x86_cpu_has_ibpb() {
                // SAFETY: Kernel code runs at CPL 0 and `x86_cpu_has_ibpb()` checked that the CPU
                // supports IBPB.
                unsafe { x86_cpu_ibpb() };
            }
        });

        let mut result;
        loop {
            // If the thread was killed or suspended, then we should exit with an error.
            // SAFETY: `current_thread` is the live current thread, and this temporary `&Thread` is
            // dropped before `vmexit_handler` can block, reschedule, or migrate `current_thread`.
            if let Err(err) = unsafe { &*current_thread }.check_kill_or_suspend_signal() {
                core::hint::cold_path();
                return Err(err);
            }
            // SAFETY: As in `create`.
            let mut vmcs = unsafe { AutoVmcs::new(self.vmcs_page.physical_address(), false) };

            // We check whether a kick was requested before entering the guest so that:
            // 1. When we enter the syscall, we can return immediately without entering
            //    the guest.
            // 2. If we have already exited the guest to handle a packet, it allows us
            //    to return and gives user-space a chance to handle that packet, without
            //    the request to kick interfering with the packet in-flight.
            //
            // We also do this after we have disabled interrupts, so if an interrupt was
            // fired before we disabled interrupts, we have the opportunity to check
            // whether a kick was requested, but the interrupt was lost. If an interrupt
            // is fired after we have disabled interrupts, when we enter the guest we
            // will exit due to the interrupt, and run this check again.
            if self.kicked.swap(false, Ordering::SeqCst) {
                core::hint::cold_path();
                return Err(Status::CANCELED);
            }

            if let Err(err) = local_apic_maybe_interrupt(&mut vmcs, &self.local_apic_state) {
                core::hint::cold_path();
                return Err(err);
            }

            // Updates guest system time if the guest subscribed to updates.
            // SAFETY: Only this thread accesses `pv_clock_state`.
            pv_clock_update_system_time(unsafe { &mut *self.pv_clock_state.get() });

            if !extended_registers_loaded.get() {
                self.entered.store(true, Ordering::SeqCst);
                // SAFETY: This is the thread of the VCPU, and `vmcs` disables interrupts.
                unsafe {
                    self.context_switch(/*include_gs=*/ true);
                    self.load_extended_registers(&vmcs);
                }
                extended_registers_loaded.set(true);
            }

            if x86_cpu_should_l1d_flush_on_vmentry() {
                // L1TF: Flush L1D$ before entering vCPU. If the CPU is affected by MDS,
                // also flush microarchitectural buffers.
                // SAFETY: The CPU supports IA32_FLUSH_CMD, and flushing the L1D$ has no other
                // side effects.
                unsafe { write_msr(X86_MSR_IA32_FLUSH_CMD, 1) };
            } else if x86_cpu_should_md_clear_on_user_return() {
                // MDS: If the processor is not affected by L1TF but is affected by MDS or
                // TAA, flush microarchitectural buffers.
                mds_buff_overwrite();
            }

            duration_begin!("kernel:arch", "vcpu");

            guest_stats_inc!(vm_entries);
            // SAFETY: `vmcs` disables interrupts, and the VMCS of the VCPU, which `vmcs_init` and
            // `migrate` set up for `vmx_state` and this CPU, is current.
            result = unsafe { vmx_enter(self.vmx_state.get()) };
            guest_stats_inc!(vm_exits);

            if !BootOptions::get().x86_disable_spec_mitigations {
                // Spectre V2: Ensure that code executed in the VM guest cannot influence
                // return address prediction in the host.
                x86_ras_fill();
            }

            if result.is_ok() {
                let vmx_state = self.vmx_state.get();
                // SAFETY: Only this thread and its migrate function access `vmx_state`, and the
                // migrate function only accesses `resume`, which is written while interrupts are
                // disabled. Only this thread accesses `pv_clock_state` and
                // `local_apic_state.timer` (outside `deadline_callback`), and `local_apic_state` is
                // an initialized pinned field of `self` whose `timer` is canceled in
                // `Vcpu::recycle` before any `&mut` covers `self` or `local_apic_state`. `vmcs`
                // was created in this iteration and has not been invalidated, and the VM exit
                // entered CPL 0 with `RFLAGS.DF == 0`, `DR7.GD` clear, and `%gs.base` restored from
                // `HOST_GS_BASE` (which `vmcs_init` and `migrate` set to this CPU's initialized
                // `x86_percpu`) on this running VCPU thread with the local APIC initialized and no
                // locks held.
                unsafe {
                    (*vmx_state).resume = true;
                    result = vmexit_handler(
                        &mut vmcs,
                        &mut (*vmx_state).guest_state,
                        &self.local_apic_state,
                        &mut *self.pv_clock_state.get(),
                        self.guest().physical_aspace(),
                        self.guest().traps(),
                        packet,
                    );
                }
            } else {
                core::hint::cold_path();
                ktrace_vcpu_exit(VcpuExit::Failure, vmcs.read(VmcsFieldXX::GUEST_RIP));
                let error = vmcs.read(VmcsField32::INSTRUCTION_ERROR);
                log_vcpu_enter_failed(error);
            }
            if result.is_err() {
                core::hint::cold_path();
                break;
            }
        }
        if result == Err(Status::NEXT) { Ok(()) } else { result }
    }

    /// Kicks the VCPU out of the guest, causing a pending or future `enter` to return.
    pub fn kick(&self) {
        self.kicked.store(true, Ordering::SeqCst);
        // Cancel any pending or upcoming wait-for-interrupts.
        self.local_apic_state.interrupt_tracker.cancel();
        self.interrupt_cpu();
    }

    fn interrupt_cpu(&self) {
        // Enter the global thread list lock, allowing us to see if our thread still exists.
        ksync::lock!(let _guard = Thread::get_list_lock().lock());
        let thread = self.thread.load(Ordering::Relaxed);
        if !thread.is_null() {
            // If our thread is still around, grab its lock to prevent our last_cpu
            // bookkeeping from changing.
            //
            // TODO(johngro): Do we need any of this?  Can we just use the Thread's
            // last_cpu member instead?
            // SAFETY: `thread` is not null, so it has not exited, and cannot while the thread list
            // lock is held.
            let thread_ref = unsafe { &*thread };
            single_chain_lock_guard_owned!(
                thread_guard,
                thread_ref,
                NO_IRQ_SAVE_OPTION,
                clt_tag!("Vcpu::InterruptCpu")
            );

            // If the VCPU thread is still running, and we have a valid last_cpu, send
            // the thread's CPU an IPI.
            if thread_guard.proof().state() == ThreadStateKind::Running
                && self.last_cpu.load(Ordering::Relaxed) != INVALID_CPU
            {
                mp::interrupt(
                    MpIpiTarget::Mask,
                    cpu_num_to_mask(self.last_cpu.load(Ordering::Relaxed)),
                );
            }
        }
    }

    /// Raises an interrupt on the VCPU.
    pub fn interrupt(&self, vector: u32) -> Result<(), Status> {
        let result = self.local_apic_state.interrupt_tracker.interrupt(vector);
        if result.is_ok() {
            self.interrupt_cpu();
        }
        result
    }

    /// Reads the VCPU's register state.
    pub fn read_state(&self) -> Result<zx_vcpu_state_t, Status> {
        let _in_use = self.enter_our_thread(current_get())?;
        let mut vcpu_state = zx_vcpu_state_t::default();
        // SAFETY: `_in_use` guarantees this is the VCPU's thread in a non-reentrant context, so
        // nothing else accesses `vmx_state.guest_state`.
        register_copy!(&mut vcpu_state, unsafe { &(*self.vmx_state.get()).guest_state });
        // SAFETY: As in `create`; `_in_use` verified `!ints_disabled()`, `!blocking_disallowed()`,
        // and `num_spinlocks_held() == 0`.
        let vmcs = unsafe { AutoVmcs::new(self.vmcs_page.physical_address(), false) };
        vcpu_state.rsp = vmcs.read(VmcsFieldXX::GUEST_RSP);
        vcpu_state.rflags = vmcs.read(VmcsFieldXX::GUEST_RFLAGS) & X86_FLAGS_USER;
        Ok(vcpu_state)
    }

    /// Writes the VCPU's register state.
    pub fn write_state(&self, vcpu_state: &zx_vcpu_state_t) -> Result<(), Status> {
        let _in_use = self.enter_our_thread(current_get())?;
        // SAFETY: `_in_use` guarantees this is the VCPU's thread in a non-reentrant context, so
        // nothing else accesses `vmx_state.guest_state`.
        register_copy!(unsafe { &mut (*self.vmx_state.get()).guest_state }, vcpu_state);
        // SAFETY: As in `create`; `_in_use` verified `!ints_disabled()`, `!blocking_disallowed()`,
        // and `num_spinlocks_held() == 0`.
        let mut vmcs = unsafe { AutoVmcs::new(self.vmcs_page.physical_address(), false) };
        // SAFETY: The guest RSP is guest state, and does not affect the host.
        unsafe { vmcs.write(VmcsFieldXX::GUEST_RSP, vcpu_state.rsp) };
        if vcpu_state.rflags & X86_FLAGS_RESERVED_ONES != 0 {
            let rflags = vmcs.read(VmcsFieldXX::GUEST_RFLAGS);
            let user_flags = (rflags & !X86_FLAGS_USER) | (vcpu_state.rflags & X86_FLAGS_USER);
            // SAFETY: Only the user flags of the guest RFLAGS, which is guest state, are changed.
            unsafe { vmcs.write(VmcsFieldXX::GUEST_RFLAGS, user_flags) };
        }
        Ok(())
    }

    /// Writes the VCPU's pending I/O state.
    pub fn write_io_state(&self, io_state: &zx_vcpu_io_t) -> Result<(), Status> {
        let _in_use = self.enter_our_thread(current_get())?;
        if (io_state.access_size != 1) && (io_state.access_size != 2) && (io_state.access_size != 4)
        {
            return Err(Status::INVALID_ARGS);
        }
        static_assert!(size_of::<u64>() >= 4);
        // SAFETY: `_in_use` guarantees this is the VCPU's thread in a non-reentrant context, so
        // nothing else accesses `vmx_state.guest_state`.
        let rax = unsafe { &mut (*self.vmx_state.get()).guest_state.rax };
        let mut bytes = rax.to_ne_bytes();
        let access_size = usize::from(io_state.access_size);
        bytes[..access_size].copy_from_slice(&io_state.data[..access_size]);
        *rax = u64::from_ne_bytes(bytes);
        Ok(())
    }

    /// Returns information about the VCPU.
    pub fn get_info(&self) -> zx_info_vcpu_t {
        if self.kicked.load(Ordering::SeqCst) {
            return zx_info_vcpu_t { flags: ZX_INFO_VCPU_FLAG_KICKED };
        }
        zx_info_vcpu_t::default()
    }

    /// Unregisters `thread`'s migrate and context-switch callbacks (and clears its `vcpu` flag)
    /// and cancels `local_apic_state.timer` through `&self`.
    fn teardown_shared(&self) {
        let (old_migrate, old_context_switch) = {
            // Taking the Thread list lock guarantees that our thread cannot be in the
            // Exiting stage of our migration function as Thread::Exit holds the list
            // lock during the migration callback.  `thread` is only ever mutated
            // during this callback as the thread exits (or swapped to null here), so we
            // are guaranteed to see either our thread, or nullptr if the thread has
            // already exited.
            ksync::lock!(let mut list_guard = Thread::get_list_lock().lock());
            let thread = self.thread.swap(core::ptr::null_mut(), Ordering::Relaxed);
            if !thread.is_null() {
                // SAFETY: `thread` was non-null under the thread list lock, so it has not exited,
                // and cannot while `list_guard` is held.
                let thread_ref = unsafe { &*thread };
                single_chain_lock_guard_owned!(
                    thread_guard,
                    thread_ref,
                    NO_IRQ_SAVE_OPTION,
                    clt_tag!("Vcpu::~Vcpu")
                );
                thread_ref.set_vcpu(false);
                // Clear the migration function, so that `thread` does not reference
                // `self` after destruction of the VCPU.
                // SAFETY: `thread_lists_init` has completed, `thread_ref` has not exited (so it
                // has not been erased from thread lists), and no scheduler `queue_lock_` is held.
                let old_migrate = unsafe {
                    thread_guard
                        .proof()
                        .set_migrate_fn_locked(None, list_guard.as_mut().token_mut())
                };
                let old_context_switch = thread_guard.proof().set_context_switch_fn_locked(None);
                (old_migrate, old_context_switch)
            } else {
                (None, None)
            }
        };
        drop((old_migrate, old_context_switch));
        self.local_apic_state.timer.cancel();
    }
}

#[pinned_drop]
impl PinnedDrop for Vcpu {
    fn drop(mut self: Pin<&mut Self>) {
        // `Vcpu::recycle` already ran `teardown_shared` before forming `&mut Self`; only the
        // pre-`UniquePtr` error path in `Vcpu::create` (before callbacks or timers are armed)
        // reaches here with `self.thread` still non-null.
        if !(*self.as_mut().project().thread.get_mut()).is_null() {
            self.as_ref().get_ref().teardown_shared();
        }
        // last_cpu would normally be guarded by the lock of `thread`, but in the case where
        // `thread` has already exited and become nullptr we still need to clear the vmcs off the
        // last cpu the thread ran on. As we are in the destructor, and have already cleared out
        // any potential `thread` migration function, we are safe to just read last_cpu directly
        // here. Note that it is being in the destructor that causes the lock annotations to be
        // okay with this load, but it is being specifically after removal of any migrate method
        // that actually makes this safe.
        let cpu = self.last_cpu.load(Ordering::Relaxed);

        if self.vmcs_page.is_allocated() && cpu != INVALID_CPU {
            // Clear VMCS state from the CPU.
            //
            // The destructor may be called from a different thread, therefore we must
            // IPI the CPU that last run the thread.
            let paddr = self.vmcs_page.physical_address();
            // SAFETY: `MpIpiTarget::Mask` does not require interrupts to be disabled. The VMCS page
            // of the VCPU is still allocated, and is only accessed through VMX instructions.
            unsafe {
                mp::sync_exec(MpIpiTarget::Mask, cpu_num_to_mask(cpu), || {
                    vmclear(paddr.0);
                });
            }
        }

        let result = self.guest().free_vpid(self.vpid);
        debug_assert!(result.is_ok());
    }
}

/// Sendable raw pointer to a pinned [`Vcpu`] captured by `Thread` migrate and context-switch
/// callbacks.
#[derive(Clone, Copy)]
struct VcpuCallbackPtr(*const Vcpu);

// SAFETY: `Vcpu` is `Sync`, and `VcpuCallbackPtr` is only dereferenced as `&Vcpu` while the `Vcpu`
// remains pinned and alive (before `Vcpu::recycle` clears the callbacks on `thread`, or
// `MigrateStage::Exiting` clears `context_switch_fn` when `thread` exits).
unsafe impl Send for VcpuCallbackPtr {}

/// x86-specific VMX unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "x86_vmx")]
mod tests {
    use super::{VmxState, vmx_enter};
    use crate::arch_rs::x86::feature::{X86_FEATURE_VMX, x86_feature_test};
    use crate::arch_rs::x86::hypervisor::vmx_cpu_state::{alloc_vmx_state, free_vmx_state};
    use crate::arch_rs::{arch_interrupt_restore, arch_interrupt_save};
    use kprint::kprintln;
    use unittest::{assert_ok, expect_err};
    use zx_status::Status;

    /// Exercise the error path when a vmlaunch fails.
    #[test]
    fn vmlaunch_fail() {
        // Allocate the VMX state, as creating a guest does, which will both determine
        // if VMX is supported, and set up the CPU state correctly if so.
        let mut result = Err(Status::NOT_SUPPORTED);
        if x86_feature_test(X86_FEATURE_VMX) {
            // SAFETY: Kernel unit tests run after `LK_INIT_LEVEL_KERNEL` initialization has
            // completed, CPU hotplug does not run during this unit test, and `_free_vmx` below
            // calls `free_vmx_state` if this succeeds.
            result = unsafe { alloc_vmx_state() };
        }
        if result == Err(Status::NOT_SUPPORTED) {
            kprintln!("VMX not supported. Skipping test.");
            return true;
        }
        assert_ok!(result);
        // SAFETY: Kernel unit tests run after `LK_INIT_LEVEL_KERNEL` initialization has completed,
        // this matches the successful `alloc_vmx_state` call above, no VMCS was loaded, and CPU
        // hotplug does not run during this unit test.
        let _free_vmx = zr::defer(|| unsafe { free_vmx_state() });

        // Attempt to launch an empty VMCS state.
        //
        // The state is invalid and we haven't performed a "vmptrld" on it, so
        // the vmlaunch will fail. However, the pointers are all valid, so we
        // shouldn't fault, but gracefully return ZX_ERR_INTERNAL.
        let mut state = VmxState::default();
        let interrupt_state = arch_interrupt_save();
        // SAFETY: Interrupts are disabled, `state` is valid, and no VMCS is current on this CPU.
        let result = unsafe { vmx_enter(&mut state) };
        arch_interrupt_restore(interrupt_state);
        expect_err!(result, Status::INTERNAL);
    }
}
