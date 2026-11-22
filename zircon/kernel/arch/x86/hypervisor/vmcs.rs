// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! VMCS fields and controls, and access to a loaded VMCS.

use core::arch::asm;
use core::ptr;
use debug::dprintf;
use zx_status::Status;
use zx_types::zx_paddr_t;

use super::vmx_cpu_state::{
    X86_MSR_IA32_VMX_CR0_FIXED0, X86_MSR_IA32_VMX_CR0_FIXED1, cr_is_invalid,
};
use crate::arch_rs::x86::interrupts::{
    X86_INT_ALIGNMENT_CHECK, X86_INT_BREAKPOINT, X86_INT_DOUBLE_FAULT, X86_INT_GP_FAULT,
    X86_INT_INVALID_TSS, X86_INT_NMI, X86_INT_OVERFLOW, X86_INT_PAGE_FAULT,
    X86_INT_SEGMENT_NOT_PRESENT, X86_INT_STACK_FAULT, X86_INT_VIRT, X86InterruptVector,
};
use crate::arch_rs::x86::registers::{X86_CR0_PE, X86_CR0_PG};
use crate::arch_rs::{
    InterruptSavedState, arch_interrupt_restore, arch_interrupt_save, blocking_disallowed,
    ints_disabled, set_blocking_disallowed,
};
use crate::hypervisor::state_invalidator::Invalidate;
use crate::kernel::auto_preempt_disabler::AutoPreemptDisabler;
use crate::kernel::bits::{bits, bits_shift};
use crate::kernel::types::PAddr;

pub const X86_MSR_IA32_VMX_PINBASED_CTLS: u32 = 0x0481;
/// Primary processor-based VM-execution controls.
pub const X86_MSR_IA32_VMX_PROCBASED_CTLS: u32 = 0x0482;
pub const X86_MSR_IA32_VMX_EXIT_CTLS: u32 = 0x0483;
pub const X86_MSR_IA32_VMX_ENTRY_CTLS: u32 = 0x0484;
/// Secondary processor-based VM-execution controls.
pub const X86_MSR_IA32_VMX_PROCBASED_CTLS2: u32 = 0x048b;
pub const X86_MSR_IA32_VMX_TRUE_PINBASED_CTLS: u32 = 0x048d;
pub const X86_MSR_IA32_VMX_TRUE_PROCBASED_CTLS: u32 = 0x048e;
pub const X86_MSR_IA32_VMX_TRUE_EXIT_CTLS: u32 = 0x048f;
pub const X86_MSR_IA32_VMX_TRUE_ENTRY_CTLS: u32 = 0x0490;

// PROCBASED_CTLS2 flags.
pub const PROCBASED_CTLS2_EPT: u32 = 1 << 1;
pub const PROCBASED_CTLS2_RDTSCP: u32 = 1 << 3;
pub const PROCBASED_CTLS2_X2APIC: u32 = 1 << 4;
pub const PROCBASED_CTLS2_VPID: u32 = 1 << 5;
pub const PROCBASED_CTLS2_UNRESTRICTED_GUEST: u32 = 1 << 7;
pub const PROCBASED_CTLS2_PAUSE_LOOP_EXITING: u32 = 1 << 10;
pub const PROCBASED_CTLS2_INVPCID: u32 = 1 << 12;

// PROCBASED_CTLS flags.
pub const PROCBASED_CTLS_INT_WINDOW_EXITING: u32 = 1 << 2;
pub const PROCBASED_CTLS_HLT_EXITING: u32 = 1 << 7;
pub const PROCBASED_CTLS_CR3_LOAD_EXITING: u32 = 1 << 15;
pub const PROCBASED_CTLS_CR3_STORE_EXITING: u32 = 1 << 16;
pub const PROCBASED_CTLS_CR8_LOAD_EXITING: u32 = 1 << 19;
pub const PROCBASED_CTLS_CR8_STORE_EXITING: u32 = 1 << 20;
pub const PROCBASED_CTLS_TPR_SHADOW: u32 = 1 << 21;
pub const PROCBASED_CTLS_IO_EXITING: u32 = 1 << 24;
pub const PROCBASED_CTLS_MSR_BITMAPS: u32 = 1 << 28;
pub const PROCBASED_CTLS_PROCBASED_CTLS2: u32 = 1 << 31;

// PINBASED_CTLS flags.
pub const PINBASED_CTLS_EXT_INT_EXITING: u32 = 1 << 0;
pub const PINBASED_CTLS_NMI_EXITING: u32 = 1 << 3;

// EXIT_CTLS flags.
pub const EXIT_CTLS_64BIT_MODE: u32 = 1 << 9;
pub const EXIT_CTLS_ACK_INT_ON_EXIT: u32 = 1 << 15;
pub const EXIT_CTLS_SAVE_IA32_PAT: u32 = 1 << 18;
pub const EXIT_CTLS_LOAD_IA32_PAT: u32 = 1 << 19;
pub const EXIT_CTLS_SAVE_IA32_EFER: u32 = 1 << 20;
pub const EXIT_CTLS_LOAD_IA32_EFER: u32 = 1 << 21;

// ENTRY_CTLS flags.
pub const ENTRY_CTLS_64BIT_MODE: u32 = 1 << 9;
pub const ENTRY_CTLS_LOAD_IA32_PAT: u32 = 1 << 14;
pub const ENTRY_CTLS_LOAD_IA32_EFER: u32 = 1 << 15;

// LINK_POINTER values.
pub const LINK_POINTER_INVALIDATE: u64 = u64::MAX;

// GUEST_XX_ACCESS_RIGHTS flags.
pub const GUEST_XX_ACCESS_RIGHTS_UNUSABLE: u32 = 1 << 16;
// See Volume 3, Section 24.4.1 for access rights format.
pub const GUEST_XX_ACCESS_RIGHTS_TYPE_A: u32 = 1 << 0;
pub const GUEST_XX_ACCESS_RIGHTS_TYPE_W: u32 = 1 << 1;
pub const GUEST_XX_ACCESS_RIGHTS_TYPE_E: u32 = 1 << 2;
pub const GUEST_XX_ACCESS_RIGHTS_TYPE_CODE: u32 = 1 << 3;
// See Volume 3, Section 3.4.5.1 for valid non-system selector types.
pub const GUEST_XX_ACCESS_RIGHTS_S: u32 = 1 << 4;
pub const GUEST_XX_ACCESS_RIGHTS_DPL_USER: u32 = 3 << 5;
pub const GUEST_XX_ACCESS_RIGHTS_P: u32 = 1 << 7;
pub const GUEST_XX_ACCESS_RIGHTS_L: u32 = 1 << 13;
pub const GUEST_XX_ACCESS_RIGHTS_D: u32 = 1 << 14;
// See Volume 3, Section 3.5 for valid system selector types.
pub const GUEST_TR_ACCESS_RIGHTS_TSS_BUSY_16BIT: u32 = 3;
pub const GUEST_TR_ACCESS_RIGHTS_TSS_BUSY: u32 = 11;

pub const GUEST_XX_ACCESS_RIGHTS_DEFAULT: u32 = GUEST_XX_ACCESS_RIGHTS_TYPE_A
    | GUEST_XX_ACCESS_RIGHTS_TYPE_W
    | GUEST_XX_ACCESS_RIGHTS_S
    | GUEST_XX_ACCESS_RIGHTS_P;

// GUEST_INTERRUPTIBILITY_STATE flags.
pub const INTERRUPTIBILITY_STI_BLOCKING: u32 = 1 << 0;
pub const INTERRUPTIBILITY_MOV_SS_BLOCKING: u32 = 1 << 1;
pub const INTERRUPTIBILITY_NMI_BLOCKING: u32 = 1 << 3;

// VMCS fields.
/// 16-bit VMCS fields.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmcsField16(u64);

impl VmcsField16 {
    pub const VPID: Self = Self(0x0000);
    pub const GUEST_CS_SELECTOR: Self = Self(0x0802);
    pub const GUEST_TR_SELECTOR: Self = Self(0x080e);
    pub const HOST_ES_SELECTOR: Self = Self(0x0c00);
    pub const HOST_CS_SELECTOR: Self = Self(0x0c02);
    pub const HOST_SS_SELECTOR: Self = Self(0x0c04);
    pub const HOST_DS_SELECTOR: Self = Self(0x0c06);
    pub const HOST_FS_SELECTOR: Self = Self(0x0c08);
    pub const HOST_GS_SELECTOR: Self = Self(0x0c0a);
    pub const HOST_TR_SELECTOR: Self = Self(0x0c0c);
}

/// 64-bit VMCS fields.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmcsField64(u64);

impl VmcsField64 {
    pub const MSR_BITMAPS_ADDRESS: Self = Self(0x2004);
    pub const EXIT_MSR_STORE_ADDRESS: Self = Self(0x2006);
    pub const EXIT_MSR_LOAD_ADDRESS: Self = Self(0x2008);
    pub const ENTRY_MSR_LOAD_ADDRESS: Self = Self(0x200a);
    pub const EPT_POINTER: Self = Self(0x201a);
    pub const GUEST_PHYSICAL_ADDRESS: Self = Self(0x2400);
    pub const LINK_POINTER: Self = Self(0x2800);
    pub const GUEST_IA32_PAT: Self = Self(0x2804);
    pub const GUEST_IA32_EFER: Self = Self(0x2806);
    pub const HOST_IA32_PAT: Self = Self(0x2c00);
    pub const HOST_IA32_EFER: Self = Self(0x2c02);
}

/// 32-bit VMCS fields.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmcsField32(u64);

impl VmcsField32 {
    pub const PINBASED_CTLS: Self = Self(0x4000);
    pub const PROCBASED_CTLS: Self = Self(0x4002);
    pub const EXCEPTION_BITMAP: Self = Self(0x4004);
    pub const PAGEFAULT_ERRORCODE_MASK: Self = Self(0x4006);
    pub const PAGEFAULT_ERRORCODE_MATCH: Self = Self(0x4008);
    pub const EXIT_CTLS: Self = Self(0x400c);
    pub const EXIT_MSR_STORE_COUNT: Self = Self(0x400e);
    pub const EXIT_MSR_LOAD_COUNT: Self = Self(0x4010);
    pub const ENTRY_CTLS: Self = Self(0x4012);
    pub const ENTRY_MSR_LOAD_COUNT: Self = Self(0x4014);
    pub const ENTRY_INTERRUPTION_INFORMATION: Self = Self(0x4016);
    pub const ENTRY_EXCEPTION_ERROR_CODE: Self = Self(0x4018);
    pub const PROCBASED_CTLS2: Self = Self(0x401e);
    pub const PLE_GAP: Self = Self(0x4020);
    pub const PLE_WINDOW: Self = Self(0x4022);
    pub const INSTRUCTION_ERROR: Self = Self(0x4400);
    pub const EXIT_REASON: Self = Self(0x4402);
    pub const EXIT_INTERRUPTION_INFORMATION: Self = Self(0x4404);
    pub const EXIT_INTERRUPTION_ERROR_CODE: Self = Self(0x4406);
    pub const EXIT_INSTRUCTION_LENGTH: Self = Self(0x440c);
    pub const EXIT_INSTRUCTION_INFORMATION: Self = Self(0x440e);
    pub const HOST_IA32_SYSENTER_CS: Self = Self(0x4c00);

    pub const GUEST_ES_LIMIT: Self = Self(0x4800);
    pub const GUEST_CS_LIMIT: Self = Self(0x4802);
    pub const GUEST_SS_LIMIT: Self = Self(0x4804);
    pub const GUEST_DS_LIMIT: Self = Self(0x4806);
    pub const GUEST_FS_LIMIT: Self = Self(0x4808);
    pub const GUEST_GS_LIMIT: Self = Self(0x480a);
    pub const GUEST_LDTR_LIMIT: Self = Self(0x480c);
    pub const GUEST_TR_LIMIT: Self = Self(0x480e);

    pub const GUEST_GDTR_LIMIT: Self = Self(0x4810);
    pub const GUEST_IDTR_LIMIT: Self = Self(0x4812);
    pub const GUEST_CS_ACCESS_RIGHTS: Self = Self(0x4816);
    pub const GUEST_ES_ACCESS_RIGHTS: Self = Self(0x4814);
    pub const GUEST_SS_ACCESS_RIGHTS: Self = Self(0x4818);
    pub const GUEST_DS_ACCESS_RIGHTS: Self = Self(0x481a);
    pub const GUEST_FS_ACCESS_RIGHTS: Self = Self(0x481c);
    pub const GUEST_GS_ACCESS_RIGHTS: Self = Self(0x481e);
    pub const GUEST_LDTR_ACCESS_RIGHTS: Self = Self(0x4820);
    pub const GUEST_TR_ACCESS_RIGHTS: Self = Self(0x4822);
    pub const GUEST_INTERRUPTIBILITY_STATE: Self = Self(0x4824);
    pub const GUEST_ACTIVITY_STATE: Self = Self(0x4826);
    pub const GUEST_IA32_SYSENTER_CS: Self = Self(0x482a);
}

/// Natural-width VMCS fields.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmcsFieldXX(u64);

impl VmcsFieldXX {
    pub const CR0_GUEST_HOST_MASK: Self = Self(0x6000);
    pub const CR4_GUEST_HOST_MASK: Self = Self(0x6002);
    pub const CR0_READ_SHADOW: Self = Self(0x6004);
    pub const CR4_READ_SHADOW: Self = Self(0x6006);
    pub const EXIT_QUALIFICATION: Self = Self(0x6400);
    pub const GUEST_LINEAR_ADDRESS: Self = Self(0x640a);
    pub const GUEST_CR0: Self = Self(0x6800);
    pub const GUEST_CR3: Self = Self(0x6802);
    pub const GUEST_CR4: Self = Self(0x6804);

    pub const GUEST_ES_BASE: Self = Self(0x6806);
    pub const GUEST_CS_BASE: Self = Self(0x6808);
    pub const GUEST_SS_BASE: Self = Self(0x680a);
    pub const GUEST_DS_BASE: Self = Self(0x680c);
    pub const GUEST_FS_BASE: Self = Self(0x680e);
    pub const GUEST_GS_BASE: Self = Self(0x6810);
    pub const GUEST_TR_BASE: Self = Self(0x6814);

    pub const GUEST_GDTR_BASE: Self = Self(0x6816);
    pub const GUEST_IDTR_BASE: Self = Self(0x6818);
    pub const GUEST_RSP: Self = Self(0x681c);
    pub const GUEST_RIP: Self = Self(0x681e);
    pub const GUEST_RFLAGS: Self = Self(0x6820);
    pub const GUEST_PENDING_DEBUG_EXCEPTIONS: Self = Self(0x6822);
    pub const GUEST_IA32_SYSENTER_ESP: Self = Self(0x6824);
    pub const GUEST_IA32_SYSENTER_EIP: Self = Self(0x6826);
    pub const HOST_CR0: Self = Self(0x6c00);
    pub const HOST_CR3: Self = Self(0x6c02);
    pub const HOST_CR4: Self = Self(0x6c04);
    pub const HOST_FS_BASE: Self = Self(0x6c06);
    pub const HOST_GS_BASE: Self = Self(0x6c08);
    pub const HOST_TR_BASE: Self = Self(0x6c0a);
    pub const HOST_GDTR_BASE: Self = Self(0x6c0c);
    pub const HOST_IDTR_BASE: Self = Self(0x6c0e);
    pub const HOST_IA32_SYSENTER_ESP: Self = Self(0x6c10);
    pub const HOST_IA32_SYSENTER_EIP: Self = Self(0x6c12);
    pub const HOST_RSP: Self = Self(0x6c14);
    pub const HOST_RIP: Self = Self(0x6c16);
}

const INTERRUPT_INFO_VALID: u32 = 1 << 31;
const INTERRUPT_INFO_DELIVER_ERROR_CODE: u32 = 1 << 11;
const INTERRUPT_TYPE_NMI: u32 = 2 << 8;
const INTERRUPT_TYPE_HARDWARE_EXCEPTION: u32 = 3 << 8;
const INTERRUPT_TYPE_SOFTWARE_EXCEPTION: u32 = 6 << 8;

/// Makes the VMCS at `pa` the current VMCS.
///
/// # Safety
///
/// `pa` must be the non-zero, 4-KiB-aligned physical address of a VMCS region in write-back RAM
/// (not MMIO) whose first 4 bytes are initialized to the CPU's VMCS revision identifier, and whose
/// page remains allocated and is only accessed through VMX instructions while it is current or
/// active on any CPU. The VMCS must not be active on any other CPU, i.e. it must have been cleared
/// with `vmclear` on the CPU where it was last current. This replaces the current VMCS of this
/// CPU, so it must not be called while an [`AutoVmcs`] that has not been invalidated is live on
/// this CPU.
pub(super) unsafe fn vmptrld(pa: zx_paddr_t) {
    let err: u8;

    // SAFETY: VMPTRLD reads the 64-bit physical address from `pa`, which is a live local. The
    // caller guarantees that the VMCS region it points to may be used by the CPU.
    unsafe {
        asm!(
            "vmptrld qword ptr [{pa}]",
            // Set `err` on error (C or Z flag set)
            "setna {err}",
            pa = in(reg) ptr::from_ref(&pa),
            err = out(reg_byte) err,
            options(nostack),
        );
    }

    assert!(err == 0);
}

/// Clears the VMCS at `pa`, writing its cached state back to memory.
///
/// # Safety
///
/// `pa` must be the non-zero, 4-KiB-aligned physical address of a VMCS region in write-back RAM
/// (not MMIO) that remains allocated, and is only accessed through VMX instructions, during this
/// call and while it is current or active on any CPU. The VMCS must not be active on any other
/// CPU, i.e. this must be the CPU where it was last current, if any. It must not be the current
/// VMCS of an [`AutoVmcs`] on this CPU that has not been invalidated. Clearing a different VMCS
/// while such an `AutoVmcs` is live (e.g. from an IPI handler) is permitted, as it does not change
/// the current VMCS.
pub(super) unsafe fn vmclear(pa: zx_paddr_t) {
    let err: u8;

    // SAFETY: VMCLEAR reads the 64-bit physical address from `pa`, which is a live local, and
    // writes back to the VMCS region it points to, which the caller guarantees may be used by the
    // CPU.
    unsafe {
        asm!(
            "vmclear qword ptr [{pa}]",
            // Set `err` on error (C or Z flag set)
            "setna {err}",
            pa = in(reg) ptr::from_ref(&pa),
            err = out(reg_byte) err,
            options(nostack),
        );
    }

    assert!(err == 0);
}

/// Reads `field` of the current VMCS.
#[inline]
fn vmread(field: u64) -> u64 {
    let err: u8;
    let val: u64;

    // SAFETY: VMREAD copies a field of the current VMCS into a register. It does not access any
    // memory visible to Rust, as a VMCS region is only accessed through VMX instructions (see
    // `vmptrld`). If there is no current VMCS it fails, which is asserted below, and outside VMX
    // operation it raises #UD, which panics. Neither outcome accesses memory visible to Rust, so
    // this is sound in any state, and `vmread` is safe.
    unsafe {
        asm!(
            "vmread {val}, {field}",
            // Set `err` on error (C or Z flag set)
            "setna {err}",
            val = out(reg) val,
            field = in(reg) field,
            err = out(reg_byte) err,
            options(nomem, nostack),
        );
    }
    assert!(err == 0);
    val
}

/// Writes `val` to `field` of the current VMCS.
///
/// # Safety
///
/// A VMCS owned by the caller must be current on this CPU, and the CPU must not switch to another
/// VMCS (e.g. preemption must be disabled) until the caller is done with it. Writing `val` to
/// `field` must not allow any later VM entry or VM exit of that VMCS's guest to violate host
/// memory safety. In particular, host-state fields must describe the host state of the CPU and
/// thread that will next enter the guest, and fields holding physical addresses (e.g. the EPT
/// pointer, or the MSR bitmap, load and store addresses) must refer to memory owned by the guest
/// that remains allocated while the VMCS can be entered. See [`AutoVmcs::write`].
#[inline]
pub(super) unsafe fn vmwrite(field: u64, val: u64) {
    let err: u8;

    // SAFETY: VMWRITE copies a register to a field of the current VMCS; omitting `nomem` prevents
    // the compiler from reordering it across the caller's preemption or interrupt guards. The
    // caller guarantees the rest.
    unsafe {
        asm!(
            "vmwrite {field}, {val}",
            // Set `err` on error (C or Z flag set)
            "setna {err}",
            val = in(reg) val,
            field = in(reg) field,
            err = out(reg_byte) err,
            options(nostack),
        );
    }
    assert!(err == 0);
}

fn has_error_code(vector: u32) -> bool {
    let Ok(vector) = u8::try_from(vector) else {
        return false;
    };
    matches!(
        X86InterruptVector(vector),
        X86_INT_DOUBLE_FAULT
            | X86_INT_INVALID_TSS
            | X86_INT_SEGMENT_NOT_PRESENT
            | X86_INT_STACK_FAULT
            | X86_INT_GP_FAULT
            | X86_INT_PAGE_FAULT
            | X86_INT_ALIGNMENT_CHECK
    )
}

/// A VMCS field, whose type determines the width of its value.
pub trait VmcsField: Copy {
    /// The value of the field.
    type Value;

    /// Returns the encoding of the field.
    fn encoding(self) -> u64;

    /// Converts a value read with VMREAD to the width of the field.
    fn from_raw(raw: u64) -> Self::Value;

    /// Converts a value of the field to write with VMWRITE.
    fn into_raw(value: Self::Value) -> u64;
}

macro_rules! impl_vmcs_field {
    ($field:ty, $value:ty) => {
        impl VmcsField for $field {
            type Value = $value;

            #[inline]
            fn encoding(self) -> u64 {
                self.0
            }

            #[inline]
            fn from_raw(raw: u64) -> $value {
                raw as $value
            }

            #[inline]
            fn into_raw(value: $value) -> u64 {
                value as u64
            }
        }
    };
}

impl_vmcs_field!(VmcsField16, u16);
impl_vmcs_field!(VmcsField32, u32);
impl_vmcs_field!(VmcsField64, u64);
impl_vmcs_field!(VmcsFieldXX, u64);

/// A guard-like object that loads a VMCS within a given scope.
///
/// Until `invalidate` is called, the VMCS is current on this CPU, with interrupts and preemption
/// disabled. After `invalidate`, the VMCS must not be accessed through the `AutoVmcs`, which is
/// checked by an assertion on writes. Blocking while the VMCS is current, which could allow
/// another VMCS to become current on this CPU, is forbidden by the safety contract of
/// [`AutoVmcs::new`], and is only checked by debug assertions.
///
/// An `AutoVmcs` must stay on the thread that created it, so it is neither `Send` nor `Sync`, via
/// its `AutoPreemptDisabler`.
#[must_use]
pub struct AutoVmcs {
    apd: AutoPreemptDisabler,
    /// The physical address of the VMCS, or 0 once invalidated.
    ///
    /// Safety invariant: while this is non-zero, the VMCS at this address is current on this CPU,
    /// with interrupts and preemption disabled, as guaranteed by the caller of [`AutoVmcs::new`].
    vmcs_address: PAddr,
    int_state: InterruptSavedState,
}

impl AutoVmcs {
    /// Loads the VMCS at `vmcs_address`, clearing it first if `clear`.
    ///
    /// Constructing an AutoVmcs disables interrupts, disables preemption, and
    /// marks the calling thread as not being allowed to block (in order to catch
    /// programming errors). Calling `invalidate` restores the previous preemption
    /// and blocking states, and destroying restores all of those previous states.
    ///
    /// # Safety
    ///
    /// `vmcs_address` must be the non-zero, 4-KiB-aligned physical address of a VMCS region in
    /// write-back RAM (not MMIO) whose first 4 bytes are initialized to the CPU's VMCS revision
    /// identifier, and whose page remains allocated and is only accessed through VMX instructions
    /// during this call (if `clear` is set) and while it is current or active on any CPU. The VMCS
    /// must not be active on any other CPU, i.e. it must have been cleared on the CPU where it was
    /// last current (or `clear` must be set and this must be that CPU).
    ///
    /// On entry, interrupts must be enabled (`!ints_disabled()`) and blocking must be allowed
    /// (`!blocking_disallowed()`) on the calling thread (in particular, no other `AutoVmcs`, even
    /// an invalidated one, may still be live on the calling thread), and no spinlocks or active
    /// `ChainLockTransaction` may be held when the returned `AutoVmcs` is invalidated or dropped.
    ///
    /// Until the returned `AutoVmcs` is invalidated or dropped, the calling thread must not block,
    /// interrupts and preemption must remain disabled, and the VMCS must remain the current VMCS of
    /// this CPU: no other VMCS may be made current (by another `AutoVmcs` or `vmptrld`) and this
    /// VMCS must not be cleared (by `vmclear`) on this CPU. Clearing a different VMCS on this CPU
    /// (e.g. from an IPI handler) is permitted.
    ///
    /// The returned `AutoVmcs` must be dropped on the calling thread, rather than leaked, so that
    /// the interrupt, preemption and blocking states are restored.
    #[inline]
    pub unsafe fn new(vmcs_address: PAddr, clear: bool) -> Self {
        let mut apd = AutoPreemptDisabler::new_deferred();
        debug_assert!(vmcs_address.0 != 0);
        debug_assert!(!ints_disabled());
        debug_assert!(!blocking_disallowed());
        let int_state = arch_interrupt_save();
        set_blocking_disallowed(true);
        apd.disable();
        if clear {
            core::hint::cold_path();
            // SAFETY: The caller guarantees that `vmcs_address` is a VMCS region that may be
            // cleared on this CPU.
            unsafe { vmclear(vmcs_address.0) };
        }
        // SAFETY: The caller guarantees that `vmcs_address` is a VMCS region that may be made
        // current on this CPU. Because interrupts were enabled on entry, no other `AutoVmcs` is
        // live on this CPU (an `AutoVmcs` keeps interrupts disabled until it is dropped).
        unsafe { vmptrld(vmcs_address.0) };
        Self { apd, vmcs_address, int_state }
    }

    /// Enables or disables exiting when the guest is ready to receive interrupts.
    ///
    /// # Panics
    ///
    /// Panics if called after `invalidate`.
    pub fn interrupt_window_exiting(&mut self, enable: bool) {
        assert!(self.vmcs_address.0 != 0);
        let mut controls = self.read(VmcsField32::PROCBASED_CTLS);
        if enable {
            controls |= PROCBASED_CTLS_INT_WINDOW_EXITING;
        } else {
            controls &= !PROCBASED_CTLS_INT_WINDOW_EXITING;
        }
        // SAFETY: By the invariant on `vmcs_address`, which is non-zero as checked above, the read
        // above was of this VMCS, so only the interrupt-window exiting control is changed, which
        // affects when VM exits occur, not the host state they restore.
        unsafe { self.write(VmcsField32::PROCBASED_CTLS, controls) };
    }

    /// Injects `vector` into the guest on the next VM entry.
    ///
    /// # Panics
    ///
    /// Panics if called after `invalidate`.
    pub fn issue_interrupt(&mut self, vector: u32) {
        assert!(self.vmcs_address.0 != 0);
        let mut interrupt_info = INTERRUPT_INFO_VALID | (vector & u32::from(u8::MAX));
        if vector == u32::from(X86_INT_BREAKPOINT.0) || vector == u32::from(X86_INT_OVERFLOW.0) {
            // From Volume 3, Section 24.8.3. A VMM should use type hardware exception for all
            // exceptions other than breakpoints and overflows, which should be software exceptions.
            interrupt_info |= INTERRUPT_TYPE_SOFTWARE_EXCEPTION;
        } else if vector == u32::from(X86_INT_NMI.0) {
            interrupt_info |= INTERRUPT_TYPE_NMI;
        } else if vector <= u32::from(X86_INT_VIRT.0) {
            // From Volume 3, Section 6.15. All other vectors from 0 to X86_INT_VIRT are exceptions.
            interrupt_info |= INTERRUPT_TYPE_HARDWARE_EXCEPTION;
        }
        if has_error_code(vector) {
            interrupt_info |= INTERRUPT_INFO_DELIVER_ERROR_CODE;
            // SAFETY: The exception error code is delivered to the guest, not the host.
            unsafe { self.write(VmcsField32::ENTRY_EXCEPTION_ERROR_CODE, 0) };
        }

        debug_assert!(
            (self.read(VmcsField32::ENTRY_INTERRUPTION_INFORMATION) & INTERRUPT_INFO_VALID) == 0
        );
        // SAFETY: The interruption information describes an event injected into the guest, not
        // the host.
        unsafe { self.write(VmcsField32::ENTRY_INTERRUPTION_INFORMATION, interrupt_info) };
    }

    /// Reads `field` of the VMCS.
    ///
    /// This must not be called after `invalidate`, which is only checked by a debug assertion.
    /// Doing so is a logic error, as another VMCS (or none) may be current, but cannot violate
    /// memory safety, as `vmread` is sound in any state.
    #[inline]
    pub fn read<F: VmcsField>(&self, field: F) -> F::Value {
        debug_assert!(self.vmcs_address.0 != 0);
        F::from_raw(vmread(field.encoding()))
    }

    /// Writes `val` to `field` of the VMCS.
    ///
    /// # Panics
    ///
    /// Panics if called after `invalidate`.
    ///
    /// # Safety
    ///
    /// Writing `F::into_raw(val)` to the field encoded by `field.encoding()` must not allow the
    /// next VM entry or VM exit of the VMCS's guest to violate host memory safety. In particular,
    /// host-state fields must describe the host state to return to on VM exit, and fields holding
    /// physical addresses (e.g. the EPT pointer, or the MSR bitmap, load and store addresses) must
    /// refer to memory owned by the VMCS's guest.
    ///
    /// If this `AutoVmcs` has not been invalidated, this relies on the contract of
    /// [`AutoVmcs::new`]: since construction, the calling thread must not have blocked and no other
    /// VMCS may have been made current on this CPU.
    #[inline]
    pub unsafe fn write<F: VmcsField>(&mut self, field: F, val: F::Value) {
        // After `invalidate`, the thread may have blocked and another VMCS may be current on this
        // CPU, so this is checked in release builds as writing it would be unsound.
        assert!(self.vmcs_address.0 != 0);
        // SAFETY: By the invariant on `vmcs_address`, which is non-zero as checked above, the VMCS
        // of `self` is current on this CPU, and remains so for this call as interrupts and
        // preemption are disabled. The caller guarantees that writing `val` to `field` is safe.
        unsafe { vmwrite(field.encoding(), F::into_raw(val)) };
    }

    /// Sets the VMX controls `controls`, setting the bits in `set` and clearing the bits in
    /// `clear`, as allowed by `true_msr`, and using the defaults from `old_msr` for other bits.
    ///
    /// # Panics
    ///
    /// Panics if called after `invalidate`.
    ///
    /// # Safety
    ///
    /// See [`AutoVmcs::write`]. The resulting controls must not allow the VMCS's guest to violate
    /// host memory safety.
    pub unsafe fn set_control(
        &mut self,
        controls: VmcsField32,
        true_msr: u64,
        old_msr: u64,
        set: u32,
        clear: u32,
    ) -> Result<(), Status> {
        assert!(self.vmcs_address.0 != 0);
        let allowed_0 = bits(true_msr, 31, 0) as u32;
        let allowed_1 = bits_shift(true_msr, 63, 32) as u32;
        if (allowed_1 & set) != set {
            core::hint::cold_path();
            dprintf!(
                INFO,
                "Failed to set VMCS controls {:#x}, {:#x} != {:#x}\n",
                controls.0 as u32,
                allowed_1,
                set
            );
            return Err(Status::NOT_SUPPORTED);
        }
        if (!allowed_0 & clear) != clear {
            core::hint::cold_path();
            dprintf!(
                INFO,
                "Failed to clear VMCS controls {:#x}, {:#x} != {:#x}\n",
                controls.0 as u32,
                !allowed_0,
                clear
            );
            return Err(Status::NOT_SUPPORTED);
        }
        if (set & clear) != 0 {
            core::hint::cold_path();
            dprintf!(
                INFO,
                "Attempted to set and clear the same VMCS controls {:#x}\n",
                controls.0 as u32
            );
            return Err(Status::INVALID_ARGS);
        }

        // See Volume 3, Section 31.5.1, Algorithm 3, Part C. If the control can be
        // either 0 or 1 (flexible), and the control is unknown, then refer to the
        // old MSR to find the default value.
        let flexible = allowed_0 ^ allowed_1;
        let unknown = flexible & !(set | clear);
        let defaults = unknown & bits(old_msr, 31, 0) as u32;
        // SAFETY: The caller guarantees that the resulting controls are safe, as per `write`.
        unsafe { self.write(controls, allowed_0 | defaults | set) };
        Ok(())
    }
}

impl Invalidate for AutoVmcs {
    #[inline]
    fn invalidate(&mut self) {
        if self.vmcs_address.0 != 0 {
            self.vmcs_address = PAddr(0);
            set_blocking_disallowed(false);
            self.apd.enable();
        }
    }
}

impl Drop for AutoVmcs {
    #[inline]
    fn drop(&mut self) {
        debug_assert!(ints_disabled());
        if self.vmcs_address.0 != 0 {
            debug_assert!(blocking_disallowed());
            set_blocking_disallowed(false);
        }
        arch_interrupt_restore(self.int_state);
    }
}

/// Returns whether `cr0_value` is invalid for the guest of the VMCS.
pub fn cr0_is_invalid(vmcs: &AutoVmcs, cr0_value: u64) -> bool {
    let mut check_value = cr0_value;
    // From Volume 3, Section 26.3.1.1: PE and PG bits of CR0 are not checked when unrestricted
    // guest is enabled. Set both here to avoid clashing with X86_MSR_IA32_VMX_CR0_FIXED1.
    if vmcs.read(VmcsField32::PROCBASED_CTLS2) & PROCBASED_CTLS2_UNRESTRICTED_GUEST != 0 {
        check_value |= X86_CR0_PE | X86_CR0_PG;
    }
    // SAFETY: Constructing `vmcs` executed `VMPTRLD`, which only succeeds in VMX operation, and all
    // CPUs have the same features, so this CPU supports VMX (and has the CR0 fixed MSRs) even if
    // the thread migrated after `invalidate`.
    unsafe { cr_is_invalid(check_value, X86_MSR_IA32_VMX_CR0_FIXED0, X86_MSR_IA32_VMX_CR0_FIXED1) }
}

/// Unit tests for the VMCS helpers that do not require VMX operation.
#[cfg(ktest)]
#[unittest::suite(name = "x86_hypervisor_vmcs")]
mod tests {
    use unittest::{expect_eq, expect_false};

    /// Verifies that `has_error_code` is true only for the exceptions that push an error code.
    #[test]
    fn has_error_code_vectors() {
        let error_code_vectors = [
            X86_INT_DOUBLE_FAULT,
            X86_INT_INVALID_TSS,
            X86_INT_SEGMENT_NOT_PRESENT,
            X86_INT_STACK_FAULT,
            X86_INT_GP_FAULT,
            X86_INT_PAGE_FAULT,
            X86_INT_ALIGNMENT_CHECK,
        ];
        for v in 0..=u8::MAX {
            expect_eq!(
                has_error_code(u32::from(v)),
                error_code_vectors.contains(&X86InterruptVector(v))
            );
        }
        // Vectors that do not fit in a `u8` are not exceptions.
        expect_false!(has_error_code(0x100 | u32::from(X86_INT_GP_FAULT.0)));
    }
}
