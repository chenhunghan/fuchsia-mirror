// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! The synchronous exception, IRQ and SError handlers that `exceptions.S` calls, and the arch
//! side of user exception dispatch.
//!
//! arm64 takes every exception and IRQ on the current thread's kernel stack, so the frames of the
//! entry points stay live under any exception that nests inside them. The fatal paths are
//! `#[cold]` and `#[inline(never)]`, and they format their messages themselves from scalar
//! arguments, so that none of their stack lands in the frames of the handlers that call them.

use core::ffi::{CStr, c_void};
use core::hint::cold_path;
use core::mem::{align_of, size_of};
use core::ptr::with_exposed_provenance;

use arm64_mp_bindings::ARM64_EXCEPTION_FLAG_LOWER_EL;
use arm64_thread_bindings::{
    ARM64_DFR_RUN_ACCESS_FAULT_HANDLER_BIT, ARM64_DFR_RUN_FAULT_HANDLER_BIT,
};
use debug::ltrace::KernelConsoleWriter;
use debug::ltracef;
use kprint::kprint;
use libarch::arm64::{ExceptionClass, ExceptionSyndromeRegister};
use pretty::hexdump_very_ex_rs;
use regio::arm64::{SysReg, spec};
use zx_status::Status;
use zx_types::{
    ZX_EXCP_FATAL_PAGE_FAULT, ZX_EXCP_GENERAL, ZX_EXCP_HW_BREAKPOINT, ZX_EXCP_POLICY_ERROR,
    ZX_EXCP_SW_BREAKPOINT, ZX_EXCP_UNALIGNED_ACCESS, ZX_EXCP_UNDEFINED_INSTRUCTION,
    zx_exception_report_t, zx_excp_type_t,
};

use super::mp::read_percpu_field;
use super::registers::print_frame;
use super::thread::{
    TPIDR_EL0, TPIDRRO_EL0, arch_reset_suspended_general_regs, arch_set_suspended_general_regs,
    thread_arch,
};
use super::uarch::arm64_uarch_do_spectre_v2_mitigation;
use super::vm::{detag_ptr, is_user_accessible};
use super::{GeneralRegsSource, Iframe, arch_copy_from_user};
use crate::arch_rs::ops::num_spinlocks_held;
use crate::arch_rs::{disable_ints, enable_ints};
use crate::counters;
use crate::kernel::interrupt::{IntHandlerSavedState, int_handler_finish, int_handler_start};
use crate::kernel::stats::inc_page_faults;
use crate::kernel::thread::{current_get, preempt, process_pending_signals};
use crate::pdev_interrupt::platform_irq;
use crate::platform_rs::power::{
    PlatformHaltAction, ZirconCrashReason, platform_halt, platform_panic_start,
};
use crate::vm::fault::flag;

const LOCAL_TRACE: u32 = 0;

/// The arch-specific part of an exception's context (`struct arch_exception_context`).
pub type ArchExceptionContext = arm64_mp_bindings::arch_exception_context;

zr::static_assert!(size_of::<ArchExceptionContext>() == 32);
zr::static_assert!(align_of::<ArchExceptionContext>() == 8);

const DFSC_ALIGNMENT_FAULT: u32 = 0b100001;

const FAR_EL1: SysReg<spec::FAR_EL1, u64> = SysReg::new();
const TPIDR_EL1: SysReg<spec::TPIDR_EL1, u64> = SysReg::new();

/// Dispatches a user exception with `context` through the generic exception code.
///
/// # Safety
///
/// As for `crate::object::dispatch_user_exception`.
#[inline(always)]
unsafe fn dispatch_user_exception(
    exception_type: zx_excp_type_t,
    context: &ArchExceptionContext,
) -> Result<(), Status> {
    // SAFETY: The caller's guarantees are the callee's.
    unsafe { crate::object::dispatch_user_exception(exception_type, context) }
}

/// Hex dumps `buf` to the console, labeling it with addresses from `disp_addr`.
///
/// Must stay out of line: under full LTO the dump's formatting frame otherwise inlines through
/// `arch_dump_exception_context` into the `dump_context` lambda of `dispatch_user_exception`, on
/// the path that kills the thread, growing that frame past its C++ size.
// TODO(https://fxbug.dev/537458631): Revisit once the C++ forwarders lose FFI_ALWAYS_INLINE.
#[inline(never)]
fn hexdump_ex(buf: &[u8], disp_addr: u64) {
    let _ = hexdump_very_ex_rs(&mut KernelConsoleWriter, buf, disp_addr);
}

/// The spaces that right-align a zero in the widest hex field printed here (`%#35x`).
const ALT_HEX_SPACES: &CStr = c"                                  ";
/// The spaces and `0x` prefix ahead of a nonzero value's digits in that field.
const ALT_HEX_LEAD: &CStr = c"                                0x";

/// Returns what C's `%#x` prints ahead of `value`'s hex digits: `0x`, or nothing for zero.
///
/// `{:cs}{:x}` over this and `value` prints what `%#x` does, which kprint's `{:#x}` does not for
/// zero (`0x0`), as two printf arguments, with no buffer and no `core::fmt`.
pub fn alt_hex_prefix(value: impl Into<u64>) -> &'static CStr {
    if value.into() == 0 { c"" } else { c"0x" }
}

/// [`alt_hex_prefix`] for C's `%#<WIDTH>x`: the spaces that right-align the field come first.
pub(super) fn alt_hex_lead<const WIDTH: usize>(value: u64) -> &'static CStr {
    const {
        assert!(WIDTH <= ALT_HEX_SPACES.to_bytes().len() + 1);
        assert!(WIDTH <= ALT_HEX_LEAD.to_bytes().len() + 1);
    };
    let (lead, len) = if value == 0 {
        (ALT_HEX_SPACES, WIDTH.saturating_sub(1))
    } else {
        let digits = (u64::BITS - value.leading_zeros()).div_ceil(4) as usize;
        (ALT_HEX_LEAD, 2 + WIDTH.saturating_sub(2 + digits))
    };
    let bytes = lead.to_bytes_with_nul();
    // SAFETY: `len` is at most the length of `lead` (the assertions bound `WIDTH`), so this is a
    // suffix of `lead` that keeps its NUL, and `lead` has no interior NUL.
    unsafe { CStr::from_bytes_with_nul_unchecked(&bytes[bytes.len() - 1 - len..]) }
}

/// Returns the kernel address that a `data_fault_resume` value with its fault-handler bits
/// cleared stands for.
fn kernel_addr_from_dfr(dfr: u64) -> u64 {
    // Assert that the DFR is a valid kernel address by checking that the bit before the
    // ARM64_DFR_RUN_ACCESS_FAULT_HANDLER_BIT (which is before the ARM64_DFR_RUN_FAULT_HANDLER_BIT)
    // is a 1.
    debug_assert!(dfr & (1u64 << (ARM64_DFR_RUN_ACCESS_FAULT_HANDLER_BIT - 1)) != 0);
    dfr | (1u64 << ARM64_DFR_RUN_ACCESS_FAULT_HANDLER_BIT)
        | (1u64 << ARM64_DFR_RUN_FAULT_HANDLER_BIT)
}

/// Returns a description of a data fault status code.
fn dfsc_to_string(dfsc: u32) -> &'static str {
    match dfsc {
        0b000000 => "Address Size Fault, Level 0",
        0b000001 => "Address Size Fault, Level 1",
        0b000010 => "Address Size Fault, Level 2",
        0b000011 => "Address Size Fault, Level 3",
        0b000100 => "Translation Fault, Level 0",
        0b000101 => "Translation Fault, Level 1",
        0b000110 => "Translation Fault, Level 2",
        0b000111 => "Translation Fault, Level 3",
        0b001001 => "Access Flag Fault, Level 1",
        0b001010 => "Access Flag Fault, Level 2",
        0b001011 => "Access Flag Fault, Level 3",
        0b001101 => "Permission Fault, Level 1",
        0b001110 => "Permission Fault, Level 2",
        0b001111 => "Permission Fault, Level 3",
        0b010000 => "Synchronous External Abort",
        0b010001 => "Synchronous Tag Check Fail",
        0b010100 => "Synchronous External Abort, Level 0",
        0b010101 => "Synchronous External Abort, Level 1",
        0b010110 => "Synchronous External Abort, Level 2",
        0b010111 => "Synchronous External Abort, Level 3",
        0b011000 => "Synchronous Parity or ECC Abort",
        0b011100 => "Synchronous Parity or ECC Abort, Level 0",
        0b011101 => "Synchronous Parity or ECC Abort, Level 1",
        0b011110 => "Synchronous Parity or ECC Abort, Level 2",
        0b011111 => "Synchronous Parity or ECC Abort, Level 3",
        0b100001 => "Alignment Fault",
        0b110000 => "TLB Conflict Abort",
        0b110100 => "Implementation Defined, Lockdown",
        0b110101 => "Implementation Defined, Unsupported exclusive or atomic",
        0b111101 => "Section Domain Fault",
        0b111110 => "Page Domain Fault",
        _ => "Unknown",
    }
}

/// Faulting Virtual Address for synchronous exceptions taken to EL1. Exceptions that
/// set the FAR_EL1 are Instruction Aborts (EC 0x20 or 0x21), Data Aborts (EC 0x24 or
/// 0x25), PC alignment faults (EC 0x22), and Watchpoints (EC 0x34 or 0x35).
/// ESR_EL1.EC holds the EC value for the exception.
fn exception_sets_far(ec: ExceptionClass) -> bool {
    matches!(
        ec,
        ExceptionClass::InstructionAbortLowerEl
            | ExceptionClass::InstructionAbortSameEl
            | ExceptionClass::DataAbortLowerEl
            | ExceptionClass::DataAbortSameEl
            | ExceptionClass::PcAlignment
            | ExceptionClass::WatchpointLowerEl
            | ExceptionClass::WatchpointSameEl
    )
}

counters::define_kcounter!(EXCEPTIONS_BRKPT, "exceptions.breakpoint", Sum);
counters::define_kcounter!(EXCEPTIONS_HW_BRKPT, "exceptions.hw_breakpoint", Sum);
counters::define_kcounter!(EXCEPTIONS_HW_WP, "exceptions.hw_watchpoint", Sum);
counters::define_kcounter!(EXCEPTIONS_FPU, "exceptions.fpu", Sum);
counters::define_kcounter!(EXCEPTIONS_PAGE, "exceptions.page_fault", Sum);
counters::define_kcounter!(EXCEPTIONS_IRQ, "exceptions.irq", Sum);
counters::define_kcounter!(EXCEPTIONS_UNHANDLED, "exceptions.unhandled", Sum);
counters::define_kcounter!(EXCEPTIONS_USER, "exceptions.user", Sum);
counters::define_kcounter!(EXCEPTIONS_UNKNOWN, "exceptions.unknown", Sum);
counters::define_kcounter!(EXCEPTIONS_ACCESS, "exceptions.access_fault", Sum);
counters::define_kcounter!(EXCEPTIONS_SERROR, "exceptions.serror", Sum);
counters::define_kcounter!(EXCEPTIONS_MOPS, "exceptions.mops", Sum);

/// Dispatches a user exception that carries a fault address, with interrupts enabled for the
/// dispatch.
///
/// # Safety
///
/// `iframe` points to the writable, unaliased user exception frame that `exceptions.S` saved on
/// the current stack. Interrupts are disabled, and this returns with them disabled; once
/// interrupts are enabled, the current CPU has a `THREAD_RUNNING` thread (`ConstructFirstThread`
/// completed), blocking is permitted, and no spinlocks, thread/dispatcher/lockdep-tracked locks,
/// or active `ChainLockTransaction` are held.
unsafe fn try_dispatch_user_data_fault_exception(
    exception_type: zx_excp_type_t,
    iframe: *mut Iframe,
    esr: u32,
    far: u64,
    error_code: u32,
) -> Result<(), Status> {
    debug_assert!(!iframe.is_null());
    let context = ArchExceptionContext {
        frame: iframe.cast(),
        far,
        esr,
        user_synth_code: error_code,
        user_synth_data: 0,
    };

    enable_ints();
    // SAFETY: `context` is a live local whose frame the caller guarantees. The dispatcher reads it
    // only until it returns.
    let status = unsafe { dispatch_user_exception(exception_type, &context) };
    disable_ints();
    status
}

/// Dispatches a user exception, with the fault address if the exception class sets FAR_EL1.
///
/// Must be called with interrupts disabled from exception entry.
///
/// # Safety
///
/// As for [`try_dispatch_user_data_fault_exception`].
unsafe fn try_dispatch_user_exception(
    exception_type: zx_excp_type_t,
    iframe: *mut Iframe,
    esr: u32,
) -> Result<(), Status> {
    let esr_reg = ExceptionSyndromeRegister::from(u64::from(esr));
    let maybe_far = if esr_reg.try_ec().is_ok_and(exception_sets_far) { FAR_EL1.read() } else { 0 };
    // SAFETY: The caller's guarantees are this function's.
    unsafe { try_dispatch_user_data_fault_exception(exception_type, iframe, esr, maybe_far, 0) }
}

// The fatal paths below are `#[cold]` and `#[inline(never)]`, and each builds its own message,
// because they are reached from handlers that inline into the exception entry points: anything
// they allocated would otherwise sit in an entry frame on every exception, live under whatever
// exception nests inside it.

/// Prints exception details and then panics.
///
/// # Safety
///
/// `iframe` points to the exception frame that `exceptions.S` saved on the current stack.
#[cold]
#[inline(never)]
unsafe fn exception_die(iframe: *mut Iframe, esr: u32, far: u64, msg: &'static str) -> ! {
    platform_panic_start();

    kprint!("{:s}", msg);

    // SAFETY: The caller's guarantees are this function's.
    unsafe { exception_die_halt(iframe, esr, far) }
}

/// [`exception_die`] for an exception taken in the kernel, printing `what` and the PC.
///
/// # Safety
///
/// As for [`exception_die`].
#[cold]
#[inline(never)]
unsafe fn exception_die_at_pc(iframe: *mut Iframe, esr: u32, far: u64, what: &'static str) -> ! {
    platform_panic_start();

    // SAFETY: The caller guarantees `iframe`.
    let elr = unsafe { (*iframe).elr };
    kprint!("{:s}: PC at {:cs}{:x}\n", what, alt_hex_prefix(elr), elr);

    // SAFETY: The caller's guarantees are this function's.
    unsafe { exception_die_halt(iframe, esr, far) }
}

/// [`exception_die`] for an instruction abort that no handler resolved.
///
/// # Safety
///
/// As for [`exception_die`].
#[cold]
#[inline(never)]
unsafe fn instruction_abort_die(iframe: *mut Iframe, esr: u32, far: u64, is_user: bool) -> ! {
    platform_panic_start();

    // SAFETY: The caller guarantees `iframe`.
    let elr = unsafe { (*iframe).elr };
    kprint!(
        "instruction abort: PC at {:cs}{:x}, is_user {:u}, FAR {:x}\n",
        alt_hex_prefix(elr),
        elr,
        u32::from(is_user),
        far
    );

    // SAFETY: The caller's guarantees are this function's.
    unsafe { exception_die_halt(iframe, esr, far) }
}

/// [`exception_die`] for a data abort that no handler resolved.
///
/// # Safety
///
/// As for [`exception_die`].
#[cold]
#[inline(never)]
unsafe fn data_abort_die(
    iframe: *mut Iframe,
    esr: u32,
    far: u64,
    iss: u32,
    wnr: bool,
    cm: bool,
    dfsc: u32,
) -> ! {
    platform_panic_start();

    // SAFETY: The caller guarantees `iframe`.
    let elr = unsafe { (*iframe).elr };
    // One print per line keeps each call's arguments in registers, so they add no argument area
    // to this frame.
    kprint!(
        "data fault: PC at {:cs}{:x}, FAR {:cs}{:x}\n",
        alt_hex_prefix(elr),
        elr,
        alt_hex_prefix(far),
        far
    );
    kprint!(
        "ISS {:cs}{:x} (WnR {:u} CM {:u})\n",
        alt_hex_prefix(iss),
        iss,
        u32::from(wnr),
        u32::from(cm)
    );
    kprint!("DFSC {:cs}{:x} ({:s})\n", alt_hex_prefix(dfsc), dfsc, dfsc_to_string(dfsc));

    // SAFETY: The caller's guarantees are this function's.
    unsafe { exception_die_halt(iframe, esr, far) }
}

/// The rest of [`exception_die`] once the panic has started and the message is printed: prints
/// the syndrome, the fault address and the frame, records them in the crashlog and halts.
///
/// # Safety
///
/// As for [`exception_die`].
#[cold]
#[inline(never)]
unsafe fn exception_die_halt(iframe: *mut Iframe, esr: u32, far: u64) -> ! {
    let ec = (esr >> 26) & 0x3f;
    let il = esr & (1 << 25);
    let iss = esr & 0x1ff_ffff;

    // fatal exception, die here
    kprint!(
        "ESR {:cs}{:x}: ec {:cs}{:x}, il {:cs}{:x}, iss {:cs}{:x}\n",
        alt_hex_prefix(esr),
        esr,
        alt_hex_prefix(ec),
        ec,
        alt_hex_prefix(il),
        il,
        alt_hex_prefix(iss),
        iss
    );
    kprint!("FAR {:cs}{:x}\n", alt_hex_prefix(far), far);
    // SAFETY: The caller guarantees that `iframe` is the exception frame on the current stack. It
    // stays live, and nothing writes it, while this thread runs here.
    print_frame(unsafe { &*iframe });
    // SAFETY: The caller guarantees `iframe` is null or points to a valid `Iframe` on the current
    // stack, which stays live and unmutated because this function does not return.
    unsafe {
        crate::crashlog::set_crashlog_regs(crate::arch_rs::crashlog::CrashlogRegs {
            iframe: iframe.cast_const(),
            esr,
            far,
            tpidr_el0: TPIDR_EL0.read(),
            tpidr_el1: TPIDR_EL1.read(),
            tpidrro_el0: TPIDRRO_EL0.read(),
        });
    }

    platform_halt(PlatformHaltAction::Halt, ZirconCrashReason::Panic);
}

/// Handles an exception with an unknown reason.
///
/// # Safety
///
/// As for [`exception_die`].
unsafe fn arm64_unknown_handler(iframe: *mut Iframe, exception_flags: u32, esr: u32) {
    // this is for a lot of reasons, but most of them are undefined instructions
    if (exception_flags & ARM64_EXCEPTION_FLAG_LOWER_EL) == 0 {
        // trapped inside the kernel, this is bad
        // SAFETY: The caller guarantees `iframe`.
        unsafe { exception_die_at_pc(iframe, esr, FAR_EL1.read(), "unknown exception in kernel") };
    }
    // SAFETY: The caller guarantees `iframe`, and interrupts are still disabled from entry.
    let _ = unsafe { try_dispatch_user_exception(ZX_EXCP_UNDEFINED_INSTRUCTION, iframe, esr) };
}

/// Handles a FEAT_MOPS exception.
///
/// # Safety
///
/// As for [`exception_die`].
unsafe fn arm64_mops_handler(iframe: *mut Iframe, exception_flags: u32, esr: u32) {
    // This means the PC and the PSTATE don't agree.  It's not an undefined
    // instruction but it's an illegal instruction.
    if (exception_flags & ARM64_EXCEPTION_FLAG_LOWER_EL) == 0 {
        // trapped inside the kernel, this is bad
        // SAFETY: The caller guarantees `iframe`.
        unsafe {
            exception_die_at_pc(iframe, esr, FAR_EL1.read(), "FEAT_MOPS exception in kernel")
        };
    }
    // SAFETY: The caller guarantees `iframe`, and interrupts are still disabled from entry.
    let _ = unsafe { try_dispatch_user_exception(ZX_EXCP_UNDEFINED_INSTRUCTION, iframe, esr) };
}

/// Handles a BRK or BKPT instruction.
///
/// # Safety
///
/// As for [`exception_die`].
unsafe fn arm64_brk_handler(iframe: *mut Iframe, exception_flags: u32, esr: u32) {
    if (exception_flags & ARM64_EXCEPTION_FLAG_LOWER_EL) == 0 {
        // trapped inside the kernel, this is bad
        // SAFETY: The caller guarantees `iframe`.
        unsafe { exception_die_at_pc(iframe, esr, FAR_EL1.read(), "BRK in kernel") };
    }
    // Spectre V2: If we took a BRK exception from EL0, but the ELR address is not a user address,
    // invalidate the branch predictor. User code may be attempting to mistrain indirect branch
    // prediction structures.
    // SAFETY: The caller guarantees `iframe`.
    if !is_user_accessible(unsafe { (*iframe).elr } as usize)
        && read_percpu_field!(should_invalidate_bp_on_el0_exception)
    {
        cold_path();
        arm64_uarch_do_spectre_v2_mitigation();
    }
    // SAFETY: The caller guarantees `iframe`, and interrupts are still disabled from entry.
    let _ = unsafe { try_dispatch_user_exception(ZX_EXCP_SW_BREAKPOINT, iframe, esr) };
}

/// Handles a PC alignment fault.
///
/// # Safety
///
/// As for [`exception_die`].
unsafe fn arm64_pc_alignment_fault_handler(iframe: *mut Iframe, exception_flags: u32, esr: u32) {
    if (exception_flags & ARM64_EXCEPTION_FLAG_LOWER_EL) == 0 {
        // trapped inside the kernel, this is bad
        // SAFETY: The caller guarantees `iframe`.
        unsafe { exception_die_at_pc(iframe, esr, FAR_EL1.read(), "PC alignment fault in kernel") };
    }

    // SAFETY: The caller guarantees `iframe`, and interrupts are still disabled from entry.
    let _ = unsafe { try_dispatch_user_exception(ZX_EXCP_UNALIGNED_ACCESS, iframe, esr) };
}

/// Handles a hardware breakpoint.
///
/// # Safety
///
/// As for [`exception_die`].
unsafe fn arm64_hw_breakpoint_exception_handler(
    iframe: *mut Iframe,
    exception_flags: u32,
    esr: u32,
) {
    if (exception_flags & ARM64_EXCEPTION_FLAG_LOWER_EL) == 0 {
        // trapped inside the kernel, this is bad
        // SAFETY: The caller guarantees `iframe`.
        unsafe { exception_die_at_pc(iframe, esr, FAR_EL1.read(), "HW breakpoint in kernel") };
    }

    // We don't need to save the debug state because it doesn't change by an exception. The only
    // way to change the debug state is through the thread write syscall.

    // NOTE: ARM64 Doesn't provide a good way to comunicate exception status (without exposing ESR
    //       to userspace). This means a debugger will have to compare the registers with the PC
    //       on the exceptions to find out which breakpoint triggered the exception.
    // SAFETY: The caller guarantees `iframe`, and interrupts are still disabled from entry.
    let _ = unsafe { try_dispatch_user_exception(ZX_EXCP_HW_BREAKPOINT, iframe, esr) };
}

/// Handles a watchpoint.
///
/// # Safety
///
/// As for [`exception_die`].
unsafe fn arm64_watchpoint_exception_handler(iframe: *mut Iframe, exception_flags: u32, esr: u32) {
    // Arm64 uses the Fault Address Register to determine which watchpoint triggered the exception.
    let far = FAR_EL1.read();

    if (exception_flags & ARM64_EXCEPTION_FLAG_LOWER_EL) == 0 {
        // trapped inside the kernel, this is bad
        // SAFETY: The caller guarantees `iframe`.
        unsafe { exception_die_at_pc(iframe, esr, far, "Watchpoint in kernel") };
    }

    // We don't need to save the debug state because it doesn't change by an exception. The only
    // way to change the debug state is through the thread write syscall.

    // SAFETY: The caller guarantees `iframe`, and interrupts are still disabled from entry.
    let _ = unsafe {
        try_dispatch_user_data_fault_exception(ZX_EXCP_HW_BREAKPOINT, iframe, esr, far, 0)
    };
}

/// Handles a software step.
///
/// # Safety
///
/// As for [`exception_die`].
unsafe fn arm64_step_handler(iframe: *mut Iframe, exception_flags: u32, esr: u32) {
    if (exception_flags & ARM64_EXCEPTION_FLAG_LOWER_EL) == 0 {
        // trapped inside the kernel, this is bad
        // SAFETY: The caller guarantees `iframe`.
        unsafe { exception_die_at_pc(iframe, esr, FAR_EL1.read(), "software step in kernel") };
    }
    // TODO(https://fxbug.dev/42108005): Is it worth separating this into two separate exceptions?
    // SAFETY: The caller guarantees `iframe`, and interrupts are still disabled from entry.
    let _ = unsafe { try_dispatch_user_exception(ZX_EXCP_HW_BREAKPOINT, iframe, esr) };
}

/// Handles a trapped floating point or SIMD instruction, which is always fatal.
///
/// # Safety
///
/// As for [`exception_die`].
unsafe fn arm64_fpu_handler(iframe: *mut Iframe, exception_flags: u32, esr: u32) -> ! {
    if (exception_flags & ARM64_EXCEPTION_FLAG_LOWER_EL) == 0 {
        // we trapped a floating point instruction inside our own EL, this is bad
        // SAFETY: The caller guarantees `iframe`.
        unsafe { exception_die_at_pc(iframe, esr, FAR_EL1.read(), "invalid fpu use in kernel") };
    }
    // SAFETY: The caller guarantees `iframe`.
    unsafe { exception_die(iframe, esr, FAR_EL1.read(), "unhandled fpu exception\n") };
}

/// Handles an instruction abort.
///
/// # Safety
///
/// As for [`exception_die`] and [`arm64_sync_exception`]: interrupts are disabled, `iframe` is
/// writable and unaliased, and on EL0 aborts the current CPU has a `THREAD_RUNNING` user thread
/// with blocking permitted once interrupts are enabled and no spinlocks, lockdep-tracked locks,
/// conflicting VM locks, or active `ChainLockTransaction` held.
unsafe fn arm64_instruction_abort_handler(iframe: *mut Iframe, _exception_flags: u32, esr: u32) {
    let mut far = FAR_EL1.read();
    let ec = (esr >> 26) & 0x3f;
    let iss = esr & 0x1ff_ffff;
    let dfsc = iss & 0x3f;
    let is_user = (ec & 1) == 0;
    // 0b0010XX is access faults.
    let is_access = (dfsc & 0b111100) == 0b001000;
    // 0b0001XX is translation faults.
    let is_translation = (dfsc & 0b111100) == 0b000100;

    if !is_user {
        // Any instruction page fault in kernel mode is a bug.
        // SAFETY: The caller guarantees `iframe`.
        unsafe { exception_die(iframe, esr, far, "instruction abort in kernel mode\n") };
    }

    // Strip any tag that might be present on the FAR.
    far = detag_ptr(far as usize) as u64;

    // Spectre V2: If we took an instruction abort in EL0 but the faulting address is not a user
    // address, invalidate the branch predictor. The $PC may have been updated before the abort is
    // delivered, user code may be attempting to mistrain indirect branch prediction structures.
    if is_user
        && !is_user_accessible(far as usize)
        && read_percpu_field!(should_invalidate_bp_on_el0_exception)
    {
        cold_path();
        arm64_uarch_do_spectre_v2_mitigation();
    }

    let mut pf_flags = flag::INSTRUCTION;
    pf_flags |= if is_user { flag::USER } else { 0 };
    pf_flags |= if is_translation { flag::NOT_PRESENT } else { 0 };
    pf_flags |= if is_access { flag::ACCESS } else { 0 };

    ltracef!(
        "instruction abort: PC at {:#x}, is_user {}, FAR {:x}, esr {:#x}, iss {:#x}\n",
        // SAFETY: The caller guarantees `iframe`.
        unsafe { (*iframe).elr },
        u32::from(is_user),
        far,
        esr,
        iss
    );

    debug_assert!(num_spinlocks_held() == 0);
    enable_ints();
    // Check for accessed fault and update the counters accordingly.
    if is_access {
        debug_assert!((pf_flags & flag::ACCESS) != 0);
        EXCEPTIONS_ACCESS.add(1);
    } else {
        EXCEPTIONS_PAGE.add(1);
    }
    // SAFETY: On an EL0 instruction abort, the current CPU has a running user `Thread` (with an
    // attached user `VmAspace`), interrupts are enabled, blocking is permitted, and no spinlocks,
    // disallowed lockdep-tracked locks, conflicting VM locks, or active `ChainLockTransaction` are
    // held.
    let err = unsafe { crate::vm::page_fault_handler(far as usize, pf_flags) };
    disable_ints();
    if err.is_ok() {
        return;
    }

    // If this is from user space, let the user exception handler
    // get a shot at it.
    if is_user {
        EXCEPTIONS_USER.add(1);
        // SAFETY: The caller guarantees `iframe`, and interrupts are disabled again.
        if unsafe {
            try_dispatch_user_data_fault_exception(
                ZX_EXCP_FATAL_PAGE_FAULT,
                iframe,
                esr,
                far,
                Status::result_into_raw(err) as u32,
            )
        }
        .is_ok()
        {
            return;
        }
    }

    // SAFETY: The caller guarantees `iframe`.
    unsafe { instruction_abort_die(iframe, esr, far, is_user) };
}

/// Handles a data abort.
///
/// # Safety
///
/// As for [`exception_die`] and [`arm64_sync_exception`]: interrupts are disabled, `iframe` is
/// writable and unaliased, and whenever `invoke_page_fault_handler` or EL0 exception dispatch runs,
/// the current CPU has a `THREAD_RUNNING` thread (with an attached user `VmAspace` for user
/// addresses), blocking is permitted once interrupts are enabled, and no spinlocks, disallowed
/// lockdep-tracked locks, conflicting VM locks, or active `ChainLockTransaction` are held.
unsafe fn arm64_data_abort_handler(iframe: *mut Iframe, _exception_flags: u32, esr: u32) {
    // read the FAR register
    let far = FAR_EL1.read();
    let ec = (esr >> 26) & 0x3f;
    let iss = esr & 0x1ff_ffff;
    let dfsc = iss & 0x3f;
    let is_user = (ec & 1) == 0;
    // 0b0010XX is access faults.
    let is_access = (dfsc & 0b111100) == 0b001000;
    // 0b0001XX is translation faults.
    let is_translation = (dfsc & 0b111100) == 0b000100;
    let wnr = (iss & (1 << 6)) != 0; // Write not Read
    let cm = (iss & (1 << 8)) != 0; // cache maintenance op

    let mut pf_flags = 0;
    // if it was marked Write but the cache maintenance bit was set, treat it as read
    pf_flags |= if wnr && !cm { flag::WRITE } else { 0 };
    pf_flags |= if is_user { flag::USER } else { 0 };
    pf_flags |= if is_translation { flag::NOT_PRESENT } else { 0 };
    pf_flags |= if is_access { flag::ACCESS } else { 0 };

    ltracef!(
        "data fault: PC at {:#x}, is_user {}, FAR {:#x}, esr {:#x}, iss {:#x}\n",
        // SAFETY: The caller guarantees `iframe`.
        unsafe { (*iframe).elr },
        u32::from(is_user),
        far,
        esr,
        iss
    );

    // SAFETY: `current_get()` is the running thread, a live `Thread` whose arch state only this
    // CPU writes while it runs, and `thread_arch` only offsets the pointer.
    let dfr = unsafe { (*thread_arch(current_get())).data_fault_resume };
    if !is_user && dfr == 0 {
        // Any page fault in kernel mode that's not during user-copy is a bug.
        // SAFETY: The caller guarantees `iframe`.
        unsafe { exception_die(iframe, esr, far, "data abort in kernel mode\n") };
    }

    // Check if we want to capture this fault.
    let mut capture_fault = false;
    if dfr != 0 {
        cold_path();
        if dfr & (1u64 << ARM64_DFR_RUN_ACCESS_FAULT_HANDLER_BIT) == 0 {
            // This function does not support capturing only access faults. In other words, if the
            // caller wants to capture access faults, they must also capture page faults, so we
            // assert that here.
            debug_assert!(dfr & (1u64 << ARM64_DFR_RUN_FAULT_HANDLER_BIT) == 0);
            capture_fault = true;
        } else if dfr & (1u64 << ARM64_DFR_RUN_FAULT_HANDLER_BIT) == 0 {
            // If the RUN_FAULT_HANDLER_BIT is not set, then we only want to capture this fault if
            // it is _not_ an access fault.
            capture_fault = !is_access;
        }
    }

    // Only invoke the page fault handler for access, translation, and permission faults. Any other
    // kind of fault cannot be resolved by the handler.
    // 0b0010XX is access faults.
    // 0b0001XX is translation faults.
    // 0b0011XX is permission faults.
    let invoke_page_fault_handler = (dfsc & 0b001100) != 0 && (dfsc & 0b110000) == 0;

    if capture_fault {
        // SAFETY: The caller guarantees `iframe`. The user copy that set `data_fault_resume`
        // resumes at the address written here.
        unsafe { (*iframe).elr = kernel_addr_from_dfr(dfr) };
        // Need to inform the user_copy routine whether this is a fault that can be handled by the
        // page fault handler or not, which is done by encoding a different status code.
        if invoke_page_fault_handler {
            // The user_copy routine expects the status and the flags combined into the low and
            // high parts of x0 respectively, and since status values are negative we need to
            // construct this carefully to control the sign extensions.
            // SAFETY: As above.
            unsafe {
                (*iframe).r[0] =
                    u64::from(Status::INVALID_ARGS.into_raw() as u32) | (u64::from(pf_flags) << 32)
            };
            // TODO(https://fxbug.dev/42175395): x1 is relayed back to user_copy where it will be
            // stored in page fault info. Currently, the only users of this page fault info is
            // VmAspace::SoftFault, but the kernel page fault handler shouldn't accept/work with
            // tags. To avoid architecture-specific tags reaching the VM layer at all, we can strip
            // it here so it never reaches user_copy page fault results.
            // SAFETY: As above.
            unsafe { (*iframe).r[1] = detag_ptr(far as usize) as u64 };
        } else {
            cold_path();
            // SAFETY: As above.
            unsafe { (*iframe).r[0] = Status::BAD_STATE.into_raw() as u64 };
        }
        return;
    }

    let mut err: Result<(), Status> = Ok(());
    if invoke_page_fault_handler {
        if is_access {
            debug_assert!((pf_flags & flag::ACCESS) != 0);
            EXCEPTIONS_ACCESS.add(1);
        } else {
            EXCEPTIONS_PAGE.add(1);
        }
        debug_assert!(num_spinlocks_held() == 0);
        enable_ints();
        // SAFETY: The current CPU has a running `Thread` (with an attached user `VmAspace` for
        // user addresses), interrupts are enabled, blocking is permitted, and the caller holds no
        // spinlocks, disallowed lockdep-tracked locks, conflicting VM locks, or active
        // `ChainLockTransaction`.
        err = unsafe { crate::vm::page_fault_handler(detag_ptr(far as usize), pf_flags) };
        disable_ints();
        if err.is_ok() {
            return;
        }
    }

    // Check if the current thread was expecting a data fault and
    // we should return to its handler.
    if dfr != 0 && is_user_accessible(far as usize) {
        // Having the ARM64_DFR_RUN_FAULT_HANDLER_BIT and ARM64_DFR_RUN_ACCESS_FAULT_HANDLER_BIT
        // bits set should have already resulted in a valid sign extended canonical address. Double
        // check the bit before, which should be a one.
        debug_assert!(dfr & (1u64 << (ARM64_DFR_RUN_ACCESS_FAULT_HANDLER_BIT - 1)) != 0);
        // SAFETY: The caller guarantees `iframe`. The user copy that set `data_fault_resume`
        // resumes at it.
        unsafe { (*iframe).elr = dfr };
        // Set the return status for the user copy routine.
        // SAFETY: As above.
        unsafe { (*iframe).r[0] = Status::INVALID_ARGS.into_raw() as u64 };
        return;
    }

    // If this is from user space, let the user exception handler
    // get a shot at it.
    if is_user {
        EXCEPTIONS_USER.add(1);
        let mut excp_type = ZX_EXCP_FATAL_PAGE_FAULT;
        if dfsc == DFSC_ALIGNMENT_FAULT {
            cold_path();
            excp_type = ZX_EXCP_UNALIGNED_ACCESS;
        }
        // SAFETY: The caller guarantees `iframe`, and interrupts are disabled.
        if unsafe {
            try_dispatch_user_data_fault_exception(
                excp_type,
                iframe,
                esr,
                far,
                Status::result_into_raw(err) as u32,
            )
        }
        .is_ok()
        {
            return;
        }
    }

    // Print the data fault and stop the kernel.
    // SAFETY: The caller guarantees `iframe`.
    unsafe { data_abort_die(iframe, esr, far, iss, wnr, cm, dfsc) };
}

/// Handles a synchronous exception.
///
/// Called from assembly.
///
/// # Safety
///
/// `iframe` points to the writable, unaliased exception frame that `exceptions.S` saved on the
/// current stack, `esr` is ESR_EL1 for the exception, and `exception_flags` has
/// `ARM64_EXCEPTION_FLAG_LOWER_EL` set if it was taken from EL0. Interrupts are disabled at EL1
/// with per-CPU state initialized.
///
/// Whenever the exception came from EL0 or reaches the page fault handler via an EL1 user copy
/// (`data_fault_resume` set with `ARM64_DFR_RUN_FAULT_HANDLER_BIT` or access fault), the current
/// CPU has a `THREAD_RUNNING` thread (`ConstructFirstThread` completed, with an attached user
/// `VmAspace` for user addresses), blocking is permitted once interrupts are enabled, and no
/// spinlocks, thread/dispatcher locks, disallowed lockdep-tracked locks, conflicting VM locks, or
/// active `ChainLockTransaction` are held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arm64_sync_exception(iframe: *mut Iframe, exception_flags: u32, esr: u32) {
    let esr_reg = ExceptionSyndromeRegister::from(u64::from(esr));

    // SAFETY for every handler call: the caller's guarantees are the handlers'.
    match esr_reg.try_ec() {
        Ok(ExceptionClass::Unknown) => {
            EXCEPTIONS_UNKNOWN.add(1);
            // SAFETY: See above.
            unsafe { arm64_unknown_handler(iframe, exception_flags, esr) };
        }
        Ok(ExceptionClass::Fp) => {
            EXCEPTIONS_FPU.add(1);
            // SAFETY: See above.
            unsafe { arm64_fpu_handler(iframe, exception_flags, esr) };
        }
        Ok(ExceptionClass::Svc32 | ExceptionClass::Svc64) => {
            // SAFETY: See above.
            unsafe {
                exception_die(
                    iframe,
                    esr,
                    FAR_EL1.read(),
                    "syscalls should be handled in assembly\n",
                )
            };
        }
        Ok(ExceptionClass::InstructionAbortLowerEl | ExceptionClass::InstructionAbortSameEl) => {
            inc_page_faults();
            // SAFETY: See above.
            unsafe { arm64_instruction_abort_handler(iframe, exception_flags, esr) };
        }
        Ok(ExceptionClass::DataAbortLowerEl | ExceptionClass::DataAbortSameEl) => {
            inc_page_faults();
            // SAFETY: See above.
            unsafe { arm64_data_abort_handler(iframe, exception_flags, esr) };
        }
        Ok(ExceptionClass::BreakpointLowerEl | ExceptionClass::BreakpointSameEl) => {
            EXCEPTIONS_HW_BRKPT.add(1);
            // SAFETY: See above.
            unsafe { arm64_hw_breakpoint_exception_handler(iframe, exception_flags, esr) };
        }
        Ok(ExceptionClass::StepLowerEl | ExceptionClass::StepSameEl) => {
            // SAFETY: See above.
            unsafe { arm64_step_handler(iframe, exception_flags, esr) };
        }
        Ok(ExceptionClass::WatchpointLowerEl | ExceptionClass::WatchpointSameEl) => {
            EXCEPTIONS_HW_WP.add(1);
            // SAFETY: See above.
            unsafe { arm64_watchpoint_exception_handler(iframe, exception_flags, esr) };
        }
        Ok(ExceptionClass::Bkpt | ExceptionClass::Brk) => {
            EXCEPTIONS_BRKPT.add(1);
            // SAFETY: See above.
            unsafe { arm64_brk_handler(iframe, exception_flags, esr) };
        }
        Ok(ExceptionClass::PcAlignment) => {
            // SAFETY: See above.
            unsafe { arm64_pc_alignment_fault_handler(iframe, exception_flags, esr) };
        }
        Ok(ExceptionClass::Mops) => {
            EXCEPTIONS_MOPS.add(1);
            // SAFETY: See above.
            unsafe { arm64_mops_handler(iframe, exception_flags, esr) };
        }
        _ => {
            // TODO: properly decode more of these
            if (exception_flags & ARM64_EXCEPTION_FLAG_LOWER_EL) == 0 {
                // trapped inside the kernel, this is bad
                // SAFETY: See above.
                unsafe {
                    exception_die_at_pc(
                        iframe,
                        esr,
                        FAR_EL1.read(),
                        "unhandled exception in kernel",
                    )
                };
            }
            // let the user exception handler get a shot at it
            EXCEPTIONS_UNHANDLED.add(1);
            // SAFETY: See above.
            if unsafe { try_dispatch_user_exception(ZX_EXCP_GENERAL, iframe, esr) }.is_err() {
                // SAFETY: See above.
                unsafe {
                    exception_die(iframe, esr, FAR_EL1.read(), "unhandled synchronous exception\n")
                };
            }
        }
    }

    // if we came from user space, check to see if we have any signals to handle
    if (exception_flags & ARM64_EXCEPTION_FLAG_LOWER_EL) != 0 {
        // in the case of receiving a kill signal, this function may not return,
        // but the scheduler would have been invoked so it's fine.
        // SAFETY: The caller guarantees `iframe`, which holds the user registers since the
        // exception came from EL0, and interrupts are disabled.
        unsafe { iframe_process_pending_signals(iframe) };
    }
}

/// Handles an IRQ.
///
/// Called from assembly.
///
/// # Safety
///
/// `iframe` points to the exception frame that `exceptions.S` saved on the current stack, and
/// `exception_flags` has `ARM64_EXCEPTION_FLAG_LOWER_EL` set if the IRQ was taken from EL0.
/// Interrupts are disabled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arm64_irq(iframe: *mut Iframe, exception_flags: u32) {
    ltracef!("iframe {:p}, flags {:#x}\n", iframe, exception_flags);
    let is_user = (exception_flags & ARM64_EXCEPTION_FLAG_LOWER_EL) != 0;

    // Spectre V2: If we took an interrupt while in EL0 but $PC was not a user address, invalidate
    // the branch predictor. User code may be attempting to mistrain an indirect branch predictor.
    // SAFETY: The caller guarantees `iframe`.
    if is_user
        && !is_user_accessible(unsafe { (*iframe).elr } as usize)
        && read_percpu_field!(should_invalidate_bp_on_el0_exception)
    {
        cold_path();
        arm64_uarch_do_spectre_v2_mitigation();
    }

    let mut state = IntHandlerSavedState::default();
    // SAFETY: This is the IRQ handler, running on the current thread with interrupts disabled,
    // and `int_handler_finish` is called below with the same `state`.
    unsafe { int_handler_start(&mut state) };

    EXCEPTIONS_IRQ.add(1);
    // SAFETY: The caller guarantees `iframe`, which the interrupt controller only passes to the
    // handler it dispatches. The GIC drivers register the interrupt ops from
    // `driver_handoff_post_vm` in `lk_main`, before `Thread::Current::BecomeIdle` first unmasks
    // IRQs.
    unsafe { platform_irq(iframe) };

    // SAFETY: Pairs with the `int_handler_start` above; interrupts are still disabled, and no
    // `PreemptDisabledToken` outlives the handler.
    let do_preempt = unsafe { int_handler_finish(&mut state) };

    // preempt the thread if the interrupt has signaled it
    if do_preempt {
        preempt();
    }

    // if we came from user space, check to see if we have any signals to handle
    if is_user {
        cold_path();
        // SAFETY: The caller guarantees `iframe`, which holds the user registers since the IRQ
        // came from EL0, and interrupts are disabled.
        unsafe { iframe_process_pending_signals(iframe) };
    }
}

/// Handles an SError.
///
/// Called from assembly.
#[unsafe(no_mangle)]
pub extern "C" fn arm64_serror_exception(_iframe: *mut Iframe, _exception_flags: u32) {
    // SError is largely implementation defined and may or may not be fatal. For now, just count the
    // occurrences and add a tracer to help analyze possible causes.
    let _trace = crate::ktrace_rs::cpu_begin_scope!("kernel:irq", "irq", "irq #" => "SError");
    EXCEPTIONS_SERROR.add(1);
}

/// Panics on an exception from a vector that should never be taken.
///
/// Called from assembly.
///
/// # Safety
///
/// `iframe` points to the exception frame that `exceptions.S` saved on the current stack.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arm64_invalid_exception(iframe: *mut Iframe, which: u32) -> ! {
    platform_panic_start();

    kprint!("invalid exception, which {:cs}{:x}\n", alt_hex_prefix(which), which);
    // SAFETY: The caller guarantees that `iframe` is the exception frame on the current stack. It
    // stays live, and nothing writes it, while this thread runs here.
    print_frame(unsafe { &*iframe });

    platform_halt(PlatformHaltAction::Halt, ZirconCrashReason::Panic);
}

/// Panics on an exception from AArch32 EL0 outside restricted mode.
///
/// Called from assembly.
///
/// # Safety
///
/// `iframe` points to the exception frame that `exceptions.S` saved on the current stack.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arm64_unrestricted_arm32_exception(
    iframe: *mut Iframe,
    which: u32,
    esr: u32,
) -> ! {
    platform_panic_start();

    // See include/arch/arm64.h to decode the which value
    kprint!(
        "unrestricted exception from arm32, esr {:cs}{:x} which {:cs}{:x}\n",
        alt_hex_prefix(esr),
        esr,
        alt_hex_prefix(which),
        which
    );
    // SAFETY: The caller guarantees that `iframe` is the exception frame on the current stack. It
    // stays live, and nothing writes it, while this thread runs here.
    print_frame(unsafe { &*iframe });

    platform_halt(PlatformHaltAction::Halt, ZirconCrashReason::Panic);
}

/// Processes the current thread's pending signals on its way back to user mode.
///
/// # Safety
///
/// The caller must ensure all preconditions of [`process_pending_signals`] for
/// [`GeneralRegsSource::Iframe`] and `iframe` (EL1, per-CPU state initialized,
/// `ConstructFirstThread` completed with a `THREAD_RUNNING` thread, interrupts disabled, `iframe`
/// valid/aligned/writable/unaliased on the current stack, blocking permitted, and no spinlocks,
/// thread/dispatcher/lockdep-tracked locks, or active `ChainLockTransaction` held).
#[inline(always)]
unsafe fn iframe_process_pending_signals(iframe: *mut Iframe) {
    debug_assert!(!iframe.is_null());
    // SAFETY: Discharged by this function's `# Safety` contract.
    unsafe { process_pending_signals(GeneralRegsSource::Iframe, iframe.cast()) };
}

/// Processes the current thread's pending signals on its way back to user mode.
///
/// Called from assembly.
///
/// # Safety
///
/// Same preconditions as [`iframe_process_pending_signals`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_iframe_process_pending_signals(iframe: *mut Iframe) {
    // SAFETY: Discharged by this function's `# Safety` contract.
    unsafe { iframe_process_pending_signals(iframe) };
}

/// Dumps `context` to the console, with the bottom of the user stack if it is readable.
///
/// # Safety
///
/// The caller must ensure that:
/// - The calling CPU is executing at EL1 with a constructed current `Thread`.
/// - `context.frame` is null or points to the current thread's valid, aligned, initialized
///   [`Iframe`] with no conflicting mutable references.
/// - Blocking is permitted and no spinlocks or lockdep-tracked locks are held, as
///   `arch_copy_from_user` requires.
#[cold]
#[inline(never)]
pub unsafe fn arch_dump_exception_context(context: &ArchExceptionContext) {
    // Print the common fields first.
    crate::object::dump_common_exception_context(context);

    // If we don't have a frame, there's nothing more we can print.
    if context.frame.is_null() {
        kprint!("no frame to dump\n");
        return;
    }
    // SAFETY: The frame is non-null, so the caller guarantees it is the current thread's exception
    // frame, which nothing writes while the thread runs this function.
    let frame = unsafe { &*context.frame.cast::<Iframe>() };

    let esr_raw = context.esr;
    let far = context.far;
    let esr = ExceptionSyndromeRegister::from(u64::from(esr_raw));
    let ec = esr.try_ec();
    let iss = esr.iss();

    match ec {
        Ok(
            ec @ (ExceptionClass::InstructionAbortLowerEl | ExceptionClass::InstructionAbortSameEl),
        ) => {
            kprint!(
                "instruction abort: PC at {:cs}{:x}, address {:cs}{:x} IFSC {:cs}{:x} {:cs}\n",
                alt_hex_prefix(frame.elr),
                frame.elr,
                alt_hex_prefix(far),
                far,
                alt_hex_prefix(esr_raw & 0x3f),
                esr_raw & 0x3f,
                if (ec as u32) & 1 != 0 { c"" } else { c"user " }
            );
        }
        Ok(ec @ (ExceptionClass::DataAbortLowerEl | ExceptionClass::DataAbortSameEl)) => {
            kprint!(
                "data abort: PC at {:cs}{:x}, address {:cs}{:x} {:cs}{:cs}\n",
                alt_hex_prefix(frame.elr),
                frame.elr,
                alt_hex_prefix(far),
                far,
                if (ec as u32) & 1 != 0 { c"" } else { c"user " },
                if iss & (1 << 6) != 0 { c"write" } else { c"read" }
            );
        }
        _ => {}
    }

    print_frame(frame);
    let tpidr_el0 = TPIDR_EL0.read();
    let tpidrro_el0 = TPIDRRO_EL0.read();
    kprint!(
        "tpidr_el0 {:cs}{:x}         tpidrro_el0 {:cs}{:x}\n",
        alt_hex_lead::<35>(tpidr_el0),
        tpidr_el0,
        alt_hex_lead::<25>(tpidrro_el0),
        tpidrro_el0
    );

    // try to dump the user stack
    if is_user_accessible(frame.usp as usize) {
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is a live local of the length passed. `arch_copy_from_user` checks that
        // the source is a user range and catches a fault in it. The caller guarantees the context
        // a fault there may block in.
        if unsafe {
            arch_copy_from_user(
                buf.as_mut_ptr().cast(),
                with_exposed_provenance::<c_void>(frame.usp as usize),
                buf.len(),
            )
        }
        .is_ok()
        {
            kprint!("bottom of user stack at {:cs}{:x}:\n", alt_hex_prefix(frame.usp), frame.usp);
            hexdump_ex(&buf, frame.usp);
        }
    }
}

/// Fills in the arch-specific part of `report` from `arch_context`.
///
/// # Safety
///
/// Unused on ARM64; marked `unsafe` to match [`crate::arch_rs::arch_fill_in_exception_context`].
pub unsafe fn arch_fill_in_exception_context(
    arch_context: &ArchExceptionContext,
    report: &mut zx_exception_report_t,
) {
    let zx_context = &mut report.context;
    zx_context.synth_code = arch_context.user_synth_code;
    zx_context.synth_data = arch_context.user_synth_data;
    zx_context.arch.arm_64.esr = arch_context.esr;
    zx_context.arch.arm_64.far = arch_context.far;
}

/// Dispatches a policy exception that a syscall raised.
///
/// # Safety
///
/// The caller must ensure that the calling CPU is executing at EL1 with per-CPU state initialized
/// and `ConstructFirstThread` completed (`THREAD_RUNNING` current thread), interrupts enabled,
/// blocking permitted, and no spinlocks, thread lock, dispatcher locks, lockdep-tracked locks, or
/// active `ChainLockTransaction` held. Any pre-installed `suspended_general_regs` on the current
/// thread must point to a valid, writable, unaliased general-register frame.
pub unsafe fn arch_dispatch_user_policy_exception(
    policy_exception_code: u32,
    policy_exception_data: u32,
) -> Result<(), Status> {
    let context = ArchExceptionContext {
        frame: core::ptr::null_mut(),
        far: 0,
        esr: 0,
        user_synth_code: policy_exception_code,
        user_synth_data: policy_exception_data,
    };
    // SAFETY: `context` is a live local with a null `frame`, and the caller upholds all
    // thread, interrupt, blocking, and lock preconditions of `dispatch_user_exception`.
    unsafe { dispatch_user_exception(ZX_EXCP_POLICY_ERROR, &context) }
}

/// Records `context`'s registers as `thread`'s, for the debugger while `thread` is stopped in the
/// exception. Returns false for a synthetic exception, which has no registers.
///
/// # Safety
///
/// As for [`arch_set_suspended_general_regs`], and `thread`'s debug state is the caller's too.
/// `context.frame` is null or stays valid until [`arch_remove_exception_context`].
#[must_use]
pub unsafe fn arch_install_exception_context(
    thread: *mut c_void,
    context: &ArchExceptionContext,
) -> bool {
    if context.frame.is_null() {
        // TODO(https://fxbug.dev/42105394): Must be a synthetic exception as they don't (yet)
        // provide the registers.
        return false;
    }

    // SAFETY: The caller guarantees `thread` and the frame, which is non-null.
    unsafe {
        arch_set_suspended_general_regs(thread, GeneralRegsSource::Iframe, context.frame.cast())
    };
    // SAFETY: The caller guarantees `thread` and access to its debug state, and `thread_arch`
    // only offsets the pointer.
    let arch = unsafe { thread_arch(thread.cast()) };
    // SAFETY: As above.
    unsafe { (*arch).debug_state.esr = context.esr };
    // SAFETY: As above.
    unsafe { (*arch).debug_state.far = context.far };
    true
}

/// Undoes [`arch_install_exception_context`].
///
/// # Safety
///
/// As for [`arch_reset_suspended_general_regs`].
pub unsafe fn arch_remove_exception_context(thread: *mut c_void) {
    // SAFETY: The caller's guarantees are the callee's.
    unsafe { arch_reset_suspended_general_regs(thread) };
}

#[cfg(ktest)]
/// Unit tests for the exception dump helpers.
#[unittest::suite(name = "arm64_exceptions")]
mod arm64_exceptions_tests {
    use super::{
        ARM64_DFR_RUN_ACCESS_FAULT_HANDLER_BIT, ARM64_DFR_RUN_FAULT_HANDLER_BIT, alt_hex_lead,
        alt_hex_prefix, dfsc_to_string, kernel_addr_from_dfr,
    };
    use core::ffi::CStr;
    use unittest::{assert_eq, assert_true};

    /// Whether `lead` is `spaces` spaces, followed by `0x` if `prefix`.
    fn is_lead(lead: &CStr, spaces: usize, prefix: bool) -> bool {
        let bytes = lead.to_bytes();
        let tail: &[u8] = if prefix { b"0x" } else { b"" };
        bytes.len() == spaces + tail.len()
            && bytes[..spaces].iter().all(|&b| b == b' ')
            && &bytes[spaces..] == tail
    }

    /// Tests `alt_hex_prefix`.
    #[test]
    fn test_alt_hex_prefix() {
        assert_true!(alt_hex_prefix(0u32).is_empty());
        assert_true!(alt_hex_prefix(1u32) == c"0x");
        assert_true!(alt_hex_prefix(u64::MAX) == c"0x");
    }

    /// Tests `alt_hex_lead` against what C's `%#25x` and `%#35x` print ahead of the digits.
    #[test]
    fn test_alt_hex_lead() {
        // Zero prints as a lone `0`.
        assert_true!(is_lead(alt_hex_lead::<25>(0), 24, false));
        assert_true!(is_lead(alt_hex_lead::<35>(0), 34, false));
        // One digit.
        assert_true!(is_lead(alt_hex_lead::<25>(0x5), 22, true));
        assert_true!(is_lead(alt_hex_lead::<35>(0xf), 32, true));
        // Sixteen digits.
        assert_true!(is_lead(alt_hex_lead::<25>(u64::MAX), 7, true));
        assert_true!(is_lead(alt_hex_lead::<35>(1 << 63), 17, true));
        // The digits and prefix fill or overflow the field.
        assert_true!(is_lead(alt_hex_lead::<18>(u64::MAX), 0, true));
        assert_true!(is_lead(alt_hex_lead::<10>(u64::MAX), 0, true));
        assert_true!(is_lead(alt_hex_lead::<1>(0), 0, false));
    }

    /// Tests `dfsc_to_string`.
    #[test]
    fn test_dfsc_to_string() {
        assert_true!(dfsc_to_string(0b000100) == "Translation Fault, Level 0");
        assert_true!(dfsc_to_string(0b100001) == "Alignment Fault");
        assert_true!(dfsc_to_string(0b111111) == "Unknown");
    }

    /// Tests `kernel_addr_from_dfr`.
    #[test]
    fn test_kernel_addr_from_dfr() {
        let addr: u64 = 0xffff_ffff_0012_3450;
        let dfr = addr
            & !(1u64 << ARM64_DFR_RUN_ACCESS_FAULT_HANDLER_BIT)
            & !(1u64 << ARM64_DFR_RUN_FAULT_HANDLER_BIT);
        assert_eq!(kernel_addr_from_dfr(dfr), addr);
    }
}
