// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Exception handling, trap causes, and exception context helpers for RISC-V 64.

use super::arch::{
    RISCV64_CSR_SSTATUS_SPIE, RISCV64_CSR_SSTATUS_SPP, RISCV64_CSR_STVAL, riscv64_csr_read,
};
use super::crashlog::CrashlogRegs;
use super::restricted::Iframe;
use super::thread::GeneralRegsSource;
use super::user_copy::{RISCV_CAPTURE_USER_COPY_FAULTS_BIT, arch_copy_from_user};
use super::vm::is_user_accessible;
use crate::counters;
use crate::kernel::interrupt::{IntHandlerSavedState, int_handler_finish, int_handler_start};
use crate::syscalls_rs::SyscallResult;
use core::fmt::Write;
use debug::ltrace::KernelConsoleWriter;
use debug::{dprintf, ltracef};
use pretty;
use zx_status::Status;
use zx_types::{
    ZX_EXCP_FATAL_PAGE_FAULT, ZX_EXCP_POLICY_ERROR, ZX_EXCP_SW_BREAKPOINT,
    ZX_EXCP_UNALIGNED_ACCESS, ZX_EXCP_UNDEFINED_INSTRUCTION, zx_exception_header_arch_t,
    zx_exception_report_t, zx_riscv64_exc_data_t,
};

#[allow(dead_code)]
const LOCAL_TRACE: u32 = 0;

// RISC-V interrupt cause codes (top bit set).
const RISCV64_INTERRUPT_SSWI: i64 = 1;
const RISCV64_INTERRUPT_STIM: i64 = 5;
const RISCV64_INTERRUPT_SEXT: i64 = 9;

// RISC-V synchronous exception cause codes.
const RISCV64_EXCEPTION_IADDR_MISALIGN: i64 = 0;
const RISCV64_EXCEPTION_IACCESS_FAULT: i64 = 1;
const RISCV64_EXCEPTION_ILLEGAL_INS: i64 = 2;
const RISCV64_EXCEPTION_BREAKPOINT: i64 = 3;
const RISCV64_EXCEPTION_LOAD_ADDR_MISALIGN: i64 = 4;
const RISCV64_EXCEPTION_LOAD_ACCESS_FAULT: i64 = 5;
const RISCV64_EXCEPTION_STORE_ADDR_MISALIGN: i64 = 6;
const RISCV64_EXCEPTION_STORE_ACCESS_FAULT: i64 = 7;
const RISCV64_EXCEPTION_ENV_CALL_U_MODE: i64 = 8;
const RISCV64_EXCEPTION_ENV_CALL_S_MODE: i64 = 9;
const RISCV64_EXCEPTION_ENV_CALL_M_MODE: i64 = 11;
const RISCV64_EXCEPTION_INS_PAGE_FAULT: i64 = 12;
const RISCV64_EXCEPTION_LOAD_PAGE_FAULT: i64 = 13;
const RISCV64_EXCEPTION_STORE_PAGE_FAULT: i64 = 15;

pub const VMM_PF_FLAG_NOT_PRESENT: u32 = 1 << 4;

/// Architectural exception context saved during exception handling.
pub type ArchExceptionContext = riscv64_regs_bindings::arch_exception_context;

const _: () = {
    assert!(core::mem::size_of::<ArchExceptionContext>() == 32);
    assert!(core::mem::align_of::<ArchExceptionContext>() == 8);
};

/// Flush a TLB entry for a single ASID.
#[inline(always)]
pub unsafe fn riscv64_tlb_flush_address_one_asid(va: usize, asid: u16) {
    // SAFETY: `sfence.vma` invalidates TLB entries for one virtual address in one
    // ASID. It changes no architectural state other than the TLB, and the TLB is
    // a cache of the page tables, so dropping an entry cannot make a mapping wrong.
    unsafe {
        core::arch::asm!("sfence.vma {}, {}", in(reg) va, in(reg) (asid as u64), options(nostack));
    }
}

/// Reads the current ASID configured in the SATP CSR on the local CPU.
#[inline(always)]
fn riscv64_current_asid() -> u16 {
    // SAFETY: Reads the SATP CSR on the current CPU, which has no side effects.
    let satp = unsafe { super::arch::riscv64_csr_read::<{ super::arch::RISCV64_CSR_SATP }>() };
    ((satp >> 44) & 0xffff) as u16
}

/// Convert an `scause` value to a human-readable string.
fn cause_to_string(cause: i64) -> &'static str {
    if cause < 0 {
        match cause & i64::MAX {
            RISCV64_INTERRUPT_SSWI => "Software interrupt",
            RISCV64_INTERRUPT_STIM => "Timer interrupt",
            RISCV64_INTERRUPT_SEXT => "External interrupt",
            _ => "Unknown interrupt",
        }
    } else {
        match cause {
            RISCV64_EXCEPTION_IADDR_MISALIGN => "Instruction address misaligned",
            RISCV64_EXCEPTION_IACCESS_FAULT => "Instruction access fault",
            RISCV64_EXCEPTION_ILLEGAL_INS => "Illegal instruction",
            RISCV64_EXCEPTION_BREAKPOINT => "Breakpoint",
            RISCV64_EXCEPTION_LOAD_ADDR_MISALIGN => "Load address misaligned",
            RISCV64_EXCEPTION_LOAD_ACCESS_FAULT => "Load access fault",
            RISCV64_EXCEPTION_STORE_ADDR_MISALIGN => "Store/AMO address misaligned",
            RISCV64_EXCEPTION_STORE_ACCESS_FAULT => "Store/AMO access fault",
            RISCV64_EXCEPTION_ENV_CALL_U_MODE => "Environment call from U-mode",
            RISCV64_EXCEPTION_ENV_CALL_S_MODE => "Environment call from S-mode",
            RISCV64_EXCEPTION_ENV_CALL_M_MODE => "Environment call from M-mode",
            RISCV64_EXCEPTION_INS_PAGE_FAULT => "Instruction page fault",
            RISCV64_EXCEPTION_LOAD_PAGE_FAULT => "Load page fault",
            RISCV64_EXCEPTION_STORE_PAGE_FAULT => "Store/AMO page fault",
            _ => "Unknown exception",
        }
    }
}

/// Format an interrupt/trap frame into a writer.
fn format_iframe<W: Write>(w: &mut W, frame: &Iframe) -> core::fmt::Result {
    writeln!(w, "iframe {:p}:", frame as *const _)?;
    writeln!(
        w,
        "epc    {:#18x} x1/ra  {:#18x} x2/sp   {:#18x} x3/gp   {:#18x}",
        frame.regs.pc, frame.regs.ra, frame.regs.sp, frame.regs.gp
    )?;
    writeln!(
        w,
        "x4/tp  {:#18x} x5/t0  {:#18x} x6/t1   {:#18x} x7/t2   {:#18x}",
        frame.regs.tp, frame.regs.t0, frame.regs.t1, frame.regs.t2
    )?;
    writeln!(
        w,
        "x8/s0  {:#18x} x9/s1  {:#18x} x10/a0  {:#18x} x11/a1  {:#18x}",
        frame.regs.s0, frame.regs.s1, frame.regs.a0, frame.regs.a1
    )?;
    writeln!(
        w,
        "x12/a2 {:#18x} x13/a3 {:#18x} x14/a4  {:#18x} x15/a5  {:#18x}",
        frame.regs.a2, frame.regs.a3, frame.regs.a4, frame.regs.a5
    )?;
    writeln!(
        w,
        "x16/a6 {:#18x} x17/a7 {:#18x} x18/s2  {:#18x} x19/s3  {:#18x}",
        frame.regs.a6, frame.regs.a7, frame.regs.s2, frame.regs.s3
    )?;
    writeln!(
        w,
        "x20/s4 {:#18x} x21/s5 {:#18x} x22/s6  {:#18x} x23/s7  {:#18x}",
        frame.regs.s4, frame.regs.s5, frame.regs.s6, frame.regs.s7
    )?;
    writeln!(
        w,
        "x24/s8 {:#18x} x25/s9 {:#18x} x26/s10 {:#18x} x27/s11 {:#18x}",
        frame.regs.s8, frame.regs.s9, frame.regs.s10, frame.regs.s11
    )?;
    writeln!(
        w,
        "x28/t3 {:#18x} x29/t4 {:#18x} x30/t5  {:#18x} x31/t6  {:#18x}",
        frame.regs.t3, frame.regs.t4, frame.regs.t5, frame.regs.t6
    )?;
    writeln!(w, "status {:#18x}", frame.status)
}

counters::define_kcounter!(EXCEPTIONS_BREAKPOINT, "exceptions.breakpoint", Sum);
counters::define_kcounter!(EXCEPTIONS_ILLEGAL_INSTRUCTION, "exceptions.illegal_instruction", Sum);
counters::define_kcounter!(EXCEPTIONS_IPI, "exceptions.ipi", Sum);
counters::define_kcounter!(EXCEPTIONS_IRQ, "exceptions.irq", Sum);
counters::define_kcounter!(EXCEPTIONS_MISALIGNED, "exceptions.misaligned", Sum);
counters::define_kcounter!(EXCEPTIONS_PAGE_FAULT, "exceptions.page_fault", Sum);
counters::define_kcounter!(EXCEPTIONS_SYSCALL, "exceptions.syscall", Sum);
counters::define_kcounter!(EXCEPTIONS_TIMER, "exceptions.timer", Sum);
counters::define_kcounter!(EXCEPTIONS_USER, "exceptions.user", Sum);

/// Print an iframe to the kernel debug log.
fn print_frame(frame: &Iframe) {
    let mut w = KernelConsoleWriter;
    let _ = format_iframe(&mut w, frame);
}

/// # Safety
/// Caller guarantees valid callbacks and pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_print_frame(
    write_cb: unsafe extern "C" fn(*mut core::ffi::c_void, *const u8, usize),
    ctx: *mut core::ffi::c_void,
    iframe: *const Iframe,
) {
    if iframe.is_null() {
        return;
    }
    struct CbWriter {
        cb: unsafe extern "C" fn(*mut core::ffi::c_void, *const u8, usize),
        ctx: *mut core::ffi::c_void,
    }
    impl core::fmt::Write for CbWriter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            // SAFETY: Forwarding string slice to provided callback.
            unsafe { (self.cb)(self.ctx, s.as_ptr(), s.len()) };
            Ok(())
        }
    }
    // SAFETY: the caller guarantees `iframe` points at a live trap frame.
    let iframe = unsafe { &*iframe };
    let mut writer = CbWriter { cb: write_cb, ctx };
    let _ = format_iframe(&mut writer, iframe);
}

// C++ kernel runtime hooks called from Rust exception handlers.
unsafe extern "C" {
    // Defined in exceptions.S and already `extern "C"`; no shim needed.
    fn riscv64_syscall_dispatcher(frame: *mut Iframe) -> SyscallResult;
}

/// Records the faulting register state for the crashlog.
///
/// # Safety
///
/// The caller must ensure that `iframe` is either null or aligned and pointing to a valid,
/// initialized trap [`Iframe`] on the stack of a non-returning panic handler that remains live and
/// unmutated for the remainder of the kernel's execution.
unsafe fn set_crashlog_regs(iframe: *const Iframe, cause: i64, tval: u64) {
    // SAFETY: Discharged by this function's `# Safety` contract.
    unsafe { crate::crashlog::set_crashlog_regs(CrashlogRegs { iframe, cause, tval }) };
}

/// Print panic details, dump frame & stack, fill crashlog, and halt.
///
/// # Safety
///
/// If `iframe` is non-null, it must point to a valid trap `Iframe` on the current stack. This CPU
/// must have a constructed current `Thread` (`ConstructFirstThread` has completed on this CPU).
unsafe fn exception_die(iframe: *mut Iframe, cause: i64, tval: u64, msg: &str) -> ! {
    crate::platform_rs::power::platform_panic_start();
    dprintf!(0, "{}", msg);
    if !iframe.is_null() {
        // SAFETY: null was ruled out immediately above. The trap stubs in stvec.S build
        // the frame on the current stack before calling in, so it outlives this call.
        print_frame(unsafe { &*iframe });
    }
    dprintf!(0, "cause  {:18} {}\n", cause, cause_to_string(cause));
    dprintf!(0, "tval   {:#18x}\n", tval);
    // SAFETY: The caller guarantees this CPU's first thread has been constructed, and a running
    // thread's stack is not concurrently initialized or torn down.
    unsafe { crate::kernel::thread::dump_current_stack() };
    // SAFETY: A non-null `iframe` stays live and unmutated on the stack of this non-returning
    // panic handler.
    unsafe { set_crashlog_regs(iframe, cause, tval) };
    crate::platform_rs::power::platform_halt(
        crate::platform_rs::power::PlatformHaltAction::Halt,
        crate::platform_rs::power::ZirconCrashReason::Panic,
    );
}

/// Handle a fatal exception that cannot be recovered.
///
/// # Safety
///
/// If `frame` is non-null, it must point to a valid trap `Iframe` on the current stack. This CPU
/// must have a constructed current `Thread` (`ConstructFirstThread` has completed on this CPU).
unsafe fn fatal_exception(cause: i64, tval: u64, frame: *mut Iframe) -> ! {
    let cpu = super::mp::arch_curr_cpu_num();
    // SAFETY: the read only happens on the non-null branch, and a non-null frame
    // points at the trap frame the stub built on the current stack.
    let pc = if !frame.is_null() { unsafe { (*frame).regs.pc } } else { 0 };
    crate::platform_rs::power::platform_panic_start();
    if cause < 0 {
        dprintf!(
            0,
            "unhandled interrupt cause {:#x}, epc {:#x}, tval {:#x} cpu {}\n",
            cause,
            pc,
            tval,
            cpu
        );
    } else {
        dprintf!(
            0,
            "unhandled exception cause {:#x} ({}), epc {:#x}, tval {:#x}, cpu {}\n",
            cause,
            cause_to_string(cause),
            pc,
            tval,
            cpu
        );
    }
    if !frame.is_null() {
        // SAFETY: null was ruled out immediately above. The trap stubs in stvec.S build
        // the frame on the current stack before calling in, so it outlives this call.
        print_frame(unsafe { &*frame });
    }
    dprintf!(0, "cause  {:18} {}\n", cause, cause_to_string(cause));
    dprintf!(0, "tval   {:#18x}\n", tval);
    // SAFETY: The caller guarantees this CPU's first thread has been constructed, and a running
    // thread's stack is not concurrently initialized or torn down.
    unsafe { crate::kernel::thread::dump_current_stack() };
    // SAFETY: A non-null `frame` stays live and unmutated on the stack of this non-returning
    // panic handler.
    unsafe { set_crashlog_regs(frame, cause, tval) };
    crate::platform_rs::power::platform_halt(
        crate::platform_rs::power::PlatformHaltAction::Halt,
        crate::platform_rs::power::ZirconCrashReason::Panic,
    );
}

/// Attempt to dispatch an exception to userspace exception handlers.
///
/// # Safety
///
/// The caller must ensure that the calling CPU is in S-mode with a constructed `THREAD_RUNNING`
/// current `Thread`, blocking is permitted once interrupts are enabled, no thread lock, dispatcher
/// lock, spinlock, lockdep-tracked lock, or active `ChainLockTransaction` is held, and if `frame`
/// is non-null, it is aligned and points to a valid, initialized, writable trap [`Iframe`] on the
/// current thread's kernel stack that outlives this call without conflicting Rust references.
unsafe fn try_dispatch_user_exception(
    exception_type: u32,
    cause: i64,
    tval: u64,
    frame: *mut Iframe,
    error_code: u32,
) -> Result<(), Status> {
    debug_assert!(super::arch::arch_ints_disabled());
    EXCEPTIONS_USER.add(1);

    let context = ArchExceptionContext {
        frame,
        cause,
        tval,
        user_synth_code: error_code,
        user_synth_data: 0,
    };

    super::arch::arch_enable_ints();
    // SAFETY: `context` lives on this stack frame for the duration of the call;
    // `dispatch_user_exception` temporarily stores `core::ptr::from_ref(context)` in
    // `ExceptionDispatcher` and installs `context.frame` into the thread's `suspended_general_regs`
    // across blocking handler delivery, clearing both pointers before returning. The caller
    // guarantees `frame` (if non-null) points to a valid, writable trap `Iframe` without
    // conflicting references and that no thread lock, spinlock, or active `ChainLockTransaction` is
    // held.
    let status = unsafe { crate::object::dispatch_user_exception(exception_type, &context) };
    super::arch::arch_disable_ints();
    status
}

/// Page fault handler for instruction and data aborts.
///
/// # Safety
///
/// The caller must ensure that the calling CPU is in S-mode with interrupts disabled and
/// `ConstructFirstThread` completed (`THREAD_RUNNING` current thread, with an attached user
/// `VmAspace` for user addresses), `frame` is null (only on fatal kernel faults) or points to a
/// valid, writable, unaliased [`Iframe`] on the current stack, and whenever `user` or non-capturing
/// `data_fault_resume` holds, blocking is permitted once interrupts are enabled with no spinlocks,
/// thread/dispatcher/lockdep-tracked locks, conflicting VM locks, or active `ChainLockTransaction`
/// held.
unsafe fn riscv64_page_fault_handler(cause: i64, tval: u64, frame: *mut Iframe, user: bool) {
    let mut pf_flags = VMM_PF_FLAG_NOT_PRESENT;
    if cause == RISCV64_EXCEPTION_STORE_PAGE_FAULT {
        pf_flags |= crate::vm::fault::flag::WRITE;
    }
    if cause == RISCV64_EXCEPTION_INS_PAGE_FAULT {
        pf_flags |= crate::vm::fault::flag::INSTRUCTION;
    }
    if user {
        pf_flags |= crate::vm::fault::flag::USER;
    }

    ltracef!(
        "Page fault: {} {} {} address {:#x}\n",
        if (pf_flags & crate::vm::fault::flag::USER) != 0 { "user" } else { "kernel" },
        if (pf_flags & crate::vm::fault::flag::INSTRUCTION) != 0 { "instruction" } else { "data" },
        if (pf_flags & crate::vm::fault::flag::WRITE) != 0 { "write" } else { "read" },
        tval
    );

    // SAFETY: `current_get()` returns this CPU's running thread, whose `arch_thread`
    // is fully constructed by the time any fault can be taken.
    let dfr = unsafe {
        let thread = crate::kernel::thread::current_get();
        let arch = super::thread::thread_arch(thread.cast());
        (*arch).data_fault_resume
    };
    if !user && dfr == 0 {
        crate::platform_rs::power::platform_panic_start();
        dprintf!(
            0,
            "Page fault in kernel: {} {} address {:#x}\n",
            if (pf_flags & crate::vm::fault::flag::INSTRUCTION) != 0 {
                "instruction"
            } else {
                "data"
            },
            if (pf_flags & crate::vm::fault::flag::WRITE) != 0 { "write" } else { "read" },
            tval
        );
        if !frame.is_null() {
            // SAFETY: null was ruled out immediately above. The trap stubs in stvec.S build
            // the frame on the current stack before calling in, so it outlives this call.
            print_frame(unsafe { &*frame });
        }
        dprintf!(0, "cause  {:18} {}\n", cause, cause_to_string(cause));
        dprintf!(0, "tval   {:#18x}\n", tval);
        // SAFETY: `current_get()` returns this CPU's running thread (as above), and its stack is
        // not concurrently initialized or torn down.
        unsafe { crate::kernel::thread::dump_current_stack() };
        // SAFETY: A non-null `frame` stays live and unmutated on the stack of this non-returning
        // panic handler.
        unsafe { set_crashlog_regs(frame, cause, tval) };
        crate::platform_rs::power::platform_halt(
            crate::platform_rs::power::PlatformHaltAction::Halt,
            crate::platform_rs::power::ZirconCrashReason::Panic,
        );
    }

    // Locally sfence.vma the address in case this is a spurious page fault.
    if is_user_accessible(tval as usize) {
        // SAFETY: sfence.vma on a user address with the current ASID; architecturally
        // safe to issue at any time and only affects TLB caching, not correctness.
        unsafe { riscv64_tlb_flush_address_one_asid(tval as usize, riscv64_current_asid()) };
    }

    // Check if the thread was expecting a data fault and capture bit is set.
    if (dfr & (RISCV_CAPTURE_USER_COPY_FAULTS_BIT as u64)) != 0 {
        ltracef!("DFR is set with capture: pc {:#x} tval {:#x} flags {:#x}\n", dfr, tval, pf_flags);
        // SAFETY: this arm only runs for a kernel fault taken inside a user-copy
        // routine, where the trap stub supplied a non-null frame. Rewriting pc/a1/a2
        // resumes the copy at its fault handler with the fault described in a1/a2.
        unsafe {
            (*frame).regs.pc = dfr & !(RISCV_CAPTURE_USER_COPY_FAULTS_BIT as u64);
            (*frame).regs.a1 = tval;
            (*frame).regs.a2 = pf_flags as u64;
        }
        return;
    }

    debug_assert!(super::mp::num_spinlocks_held() == 0);
    super::arch::arch_enable_ints();
    crate::kernel::stats::inc_page_faults();
    // SAFETY: The current CPU has a running `Thread` (with an attached user `VmAspace` for user
    // addresses), interrupts are enabled, blocking is permitted, and no spinlocks, lockdep-tracked
    // locks, conflicting VM locks, or active `ChainLockTransaction` are held.
    let pf_status = unsafe { crate::vm::page_fault_handler(tval as usize, pf_flags) };
    super::arch::arch_disable_ints();
    let Err(pf_status) = pf_status else {
        return;
    };

    // Check data fault resume without capture bit.
    if dfr != 0 {
        ltracef!(
            "DFR is set without capture: pc {:#x} tval {:#x} flags {:#x}\n",
            dfr,
            tval,
            pf_flags
        );
        debug_assert!((dfr & (RISCV_CAPTURE_USER_COPY_FAULTS_BIT as u64)) == 0);
        // SAFETY: reached only when `data_fault_resume` was set, which the user-copy
        // routines only do while running on a frame the trap stub built. Redirecting
        // pc resumes them at their fault label.
        unsafe {
            (*frame).regs.pc = dfr;
        }
        return;
    }

    if user
        // SAFETY: `frame` points to the trap `Iframe` built on the current thread's stack by
        // `stvec.S` with no conflicting references, and no thread lock, spinlock, or
        // `ChainLockTransaction` is held.
        && unsafe {
            try_dispatch_user_exception(
                ZX_EXCP_FATAL_PAGE_FAULT,
                cause,
                tval,
                frame,
                pf_status.into_raw() as u32,
            )
        }
        .is_ok()
    {
        return;
    }

    crate::platform_rs::power::platform_panic_start();
    dprintf!(
        0,
        "Page fault: {} {} {} address {:#x}\n",
        if (pf_flags & crate::vm::fault::flag::USER) != 0 { "user" } else { "kernel" },
        if (pf_flags & crate::vm::fault::flag::INSTRUCTION) != 0 { "instruction" } else { "data" },
        if (pf_flags & crate::vm::fault::flag::WRITE) != 0 { "write" } else { "read" },
        tval
    );
    if !frame.is_null() {
        // SAFETY: null was ruled out immediately above. The trap stubs in stvec.S build
        // the frame on the current stack before calling in, so it outlives this call.
        print_frame(unsafe { &*frame });
    }
    dprintf!(0, "cause  {:18} {}\n", cause, cause_to_string(cause));
    dprintf!(0, "tval   {:#18x}\n", tval);
    // SAFETY: `current_get()` returns this CPU's running thread (as above), and its stack is
    // not concurrently initialized or torn down.
    unsafe { crate::kernel::thread::dump_current_stack() };
    // SAFETY: A non-null `frame` stays live and unmutated on the stack of this non-returning
    // panic handler.
    unsafe { set_crashlog_regs(frame, cause, tval) };
    crate::platform_rs::power::platform_halt(
        crate::platform_rs::power::PlatformHaltAction::Halt,
        crate::platform_rs::power::ZirconCrashReason::Panic,
    );
}

/// Misaligned memory access fault handler.
unsafe fn riscv64_misaligned_fault_handler(cause: i64, tval: u64, frame: *mut Iframe, user: bool) {
    if !user {
        // SAFETY: the read only happens on the non-null branch, and a non-null frame
        // points at the trap frame the stub built on the current stack.
        let pc = if !frame.is_null() { unsafe { (*frame).regs.pc } } else { 0 };
        crate::platform_rs::power::platform_panic_start();
        dprintf!(0, "misaligned exception in kernel: PC at {:#x}\n", pc);
        if !frame.is_null() {
            // SAFETY: null was ruled out immediately above. The trap stubs in stvec.S build
            // the frame on the current stack before calling in, so it outlives this call.
            print_frame(unsafe { &*frame });
        }
        dprintf!(0, "cause  {:18} {}\n", cause, cause_to_string(cause));
        dprintf!(0, "tval   {:#18x}\n", tval);
        // SAFETY: `current_get()` returns this CPU's running thread, which is constructed by the
        // time faults are handled, and its stack is not concurrently initialized or torn down.
        unsafe { crate::kernel::thread::dump_current_stack() };
        // SAFETY: A non-null `frame` stays live and unmutated on the stack of this non-returning
        // panic handler.
        unsafe { set_crashlog_regs(frame, cause, tval) };
        crate::platform_rs::power::platform_halt(
            crate::platform_rs::power::PlatformHaltAction::Halt,
            crate::platform_rs::power::ZirconCrashReason::Panic,
        );
    }
    // SAFETY: `frame` points to the trap `Iframe` built on the current thread's stack by `stvec.S`
    // with no conflicting references, and no thread lock, spinlock, or `ChainLockTransaction` is
    // held.
    let _ = unsafe { try_dispatch_user_exception(ZX_EXCP_UNALIGNED_ACCESS, cause, tval, frame, 0) };
}

/// Illegal instruction exception handler.
unsafe fn riscv64_illegal_instruction_handler(
    cause: i64,
    tval: u64,
    frame: *mut Iframe,
    user: bool,
) {
    if !user {
        // SAFETY: the read only happens on the non-null branch, and a non-null frame
        // points at the trap frame the stub built on the current stack.
        let pc = if !frame.is_null() { unsafe { (*frame).regs.pc } } else { 0 };
        crate::platform_rs::power::platform_panic_start();
        dprintf!(0, "illegal instruction exception in kernel: PC at {:#x}\n", pc);
        if !frame.is_null() {
            // SAFETY: null was ruled out immediately above. The trap stubs in stvec.S build
            // the frame on the current stack before calling in, so it outlives this call.
            print_frame(unsafe { &*frame });
        }
        dprintf!(0, "cause  {:18} {}\n", cause, cause_to_string(cause));
        dprintf!(0, "tval   {:#18x}\n", tval);
        // SAFETY: `current_get()` returns this CPU's running thread, which is constructed by the
        // time faults are handled, and its stack is not concurrently initialized or torn down.
        unsafe { crate::kernel::thread::dump_current_stack() };
        // SAFETY: A non-null `frame` stays live and unmutated on the stack of this non-returning
        // panic handler.
        unsafe { set_crashlog_regs(frame, cause, tval) };
        crate::platform_rs::power::platform_halt(
            crate::platform_rs::power::PlatformHaltAction::Halt,
            crate::platform_rs::power::ZirconCrashReason::Panic,
        );
    }
    // SAFETY: `frame` points to the trap `Iframe` built on the current thread's stack by `stvec.S`
    // with no conflicting references, and no thread lock, spinlock, or `ChainLockTransaction` is
    // held.
    let _ = unsafe {
        try_dispatch_user_exception(ZX_EXCP_UNDEFINED_INSTRUCTION, cause, tval, frame, 0)
    };
}

/// Software breakpoint (ebreak) handler.
unsafe fn riscv64_breakpoint_handler(cause: i64, tval: u64, frame: *mut Iframe, user: bool) {
    if !user {
        // SAFETY: the read only happens on the non-null branch, and a non-null frame
        // points at the trap frame the stub built on the current stack.
        let pc = if !frame.is_null() { unsafe { (*frame).regs.pc } } else { 0 };
        crate::platform_rs::power::platform_panic_start();
        dprintf!(0, "ebreak instruction exception in kernel: PC at {:#x}\n", pc);
        if !frame.is_null() {
            // SAFETY: null was ruled out immediately above. The trap stubs in stvec.S build
            // the frame on the current stack before calling in, so it outlives this call.
            print_frame(unsafe { &*frame });
        }
        dprintf!(0, "cause  {:18} {}\n", cause, cause_to_string(cause));
        dprintf!(0, "tval   {:#18x}\n", tval);
        // SAFETY: `current_get()` returns this CPU's running thread, which is constructed by the
        // time faults are handled, and its stack is not concurrently initialized or torn down.
        unsafe { crate::kernel::thread::dump_current_stack() };
        // SAFETY: A non-null `frame` stays live and unmutated on the stack of this non-returning
        // panic handler.
        unsafe { set_crashlog_regs(frame, cause, tval) };
        crate::platform_rs::power::platform_halt(
            crate::platform_rs::power::PlatformHaltAction::Halt,
            crate::platform_rs::power::ZirconCrashReason::Panic,
        );
    }
    // SAFETY: `frame` points to the trap `Iframe` built on the current thread's stack by `stvec.S`
    // with no conflicting references, and no thread lock, spinlock, or `ChainLockTransaction` is
    // held.
    let _ = unsafe { try_dispatch_user_exception(ZX_EXCP_SW_BREAKPOINT, cause, tval, frame, 0) };
}

/// System call trap handler.
///
/// # Safety
///
/// The caller must ensure that the calling CPU is in S-mode with interrupts disabled and
/// `ConstructFirstThread` completed (`THREAD_RUNNING` user thread), `frame` points to a valid,
/// aligned, writable, unaliased user [`Iframe`] on the current stack, blocking is permitted, and no
/// spinlocks, thread/dispatcher/lockdep-tracked locks, or active `ChainLockTransaction` are held.
unsafe fn riscv64_syscall_handler(frame: *mut Iframe) {
    // Advance PC over the ECALL instruction (32-bit).
    // SAFETY: the syscall path is only entered from the trap stub, which passes a
    // non-null frame. Stepping pc past the `ecall` is what makes the syscall return
    // to the instruction after it.
    unsafe {
        (*frame).regs.pc += 4;
    }
    // SAFETY: `riscv64_syscall_dispatcher` is the assembly entry in exceptions.S; it
    // reads the argument registers out of the frame and does not retain the pointer.
    let ret = unsafe { riscv64_syscall_dispatcher(frame) };
    // SAFETY: same frame as above; a0 carries the syscall result back to userspace.
    unsafe {
        (*frame).regs.a0 = ret.status;
    }
    if ret.is_signaled != 0 {
        core::hint::cold_path();
        // SAFETY: Discharged by this function's `# Safety` contract.
        unsafe {
            crate::kernel::thread::process_pending_signals(GeneralRegsSource::Iframe, frame.cast());
        }
    }
}

/// Main trap and exception dispatcher for RISC-V 64.
///
/// # Safety
///
/// The caller must ensure that the calling CPU is in S-mode with interrupts disabled and per-CPU
/// state initialized, `frame` points to a valid, aligned, writable, unaliased [`Iframe`] matching
/// `pc` and `status` on the current stack, and whenever `status` indicates a U-mode trap or
/// kernel user-copy fault (or rescheduling/pending-signal handling runs on exit),
/// `ConstructFirstThread` has completed (`THREAD_RUNNING` thread with an attached user `VmAspace`
/// for user addresses), blocking is permitted, and no spinlocks, thread/dispatcher/lockdep-tracked
/// locks, conflicting VM locks, or active `ChainLockTransaction` are held.
unsafe fn riscv64_exception_handler(frame: *mut Iframe, pc: u64, status: u64, cause: i64) {
    // SAFETY: the trap stub passes the frame it just built along with copies of pc
    // and status taken from the same trap; these assertions check they agree.
    debug_assert!(unsafe { (*frame).regs.pc } == pc);
    // SAFETY: as above.
    debug_assert!(unsafe { (*frame).status } == status);
    let user = (status & RISCV64_CSR_SSTATUS_SPP) == 0;

    let mut do_preempt = false;

    if cause < 0 {
        let mut state = IntHandlerSavedState::default();
        // SAFETY: This is an interrupt trap, handled on the current thread with interrupts
        // disabled, and `int_handler_finish` is called below with the same `state`.
        unsafe { int_handler_start(&mut state) };

        match cause & i64::MAX {
            RISCV64_INTERRUPT_SSWI => {
                EXCEPTIONS_IPI.add(1);
                super::mp::riscv64_software_exception();
            }
            RISCV64_INTERRUPT_STIM => {
                EXCEPTIONS_TIMER.add(1);
                super::timer::riscv64_timer_exception();
            }
            RISCV64_INTERRUPT_SEXT => {
                EXCEPTIONS_IRQ.add(1);
                // SAFETY: `frame` is the live trap frame; the IRQ dispatcher only
                // passes it through to the registered handler.
                unsafe {
                    crate::pdev_interrupt::platform_irq(frame);
                }
            }
            // SAFETY: `frame` is the live trap frame; `fatal_exception` only reads
            // from it before halting.
            _ => unsafe { fatal_exception(cause, 0, frame) },
        }

        // SAFETY: Pairs with the `int_handler_start` above; interrupts are still disabled, and no
        // `PreemptDisabledToken` outlives the handler.
        do_preempt = unsafe { int_handler_finish(&mut state) };
    } else {
        // SAFETY: reading `stval` has no side effects; it holds the faulting
        // address or instruction for the trap being handled.
        let tval = unsafe { riscv64_csr_read::<{ RISCV64_CSR_STVAL }>() };
        match cause {
            RISCV64_EXCEPTION_INS_PAGE_FAULT
            | RISCV64_EXCEPTION_LOAD_PAGE_FAULT
            | RISCV64_EXCEPTION_STORE_PAGE_FAULT => {
                EXCEPTIONS_PAGE_FAULT.add(1);
                // SAFETY: `frame` is the live trap frame the stub passed in, and `user`
                // records whether the trap came from user mode.
                unsafe { riscv64_page_fault_handler(cause, tval, frame, user) };
            }
            RISCV64_EXCEPTION_IADDR_MISALIGN
            | RISCV64_EXCEPTION_LOAD_ADDR_MISALIGN
            | RISCV64_EXCEPTION_STORE_ADDR_MISALIGN => {
                EXCEPTIONS_MISALIGNED.add(1);
                // SAFETY: `frame` is the live trap frame the stub passed in, and `user`
                // records whether the trap came from user mode.
                unsafe { riscv64_misaligned_fault_handler(cause, tval, frame, user) };
            }
            RISCV64_EXCEPTION_ILLEGAL_INS => {
                EXCEPTIONS_ILLEGAL_INSTRUCTION.add(1);
                // SAFETY: `frame` is the live trap frame the stub passed in, and `user`
                // records whether the trap came from user mode.
                unsafe { riscv64_illegal_instruction_handler(cause, tval, frame, user) };
            }
            RISCV64_EXCEPTION_BREAKPOINT => {
                EXCEPTIONS_BREAKPOINT.add(1);
                // SAFETY: `frame` is the live trap frame the stub passed in, and `user`
                // records whether the trap came from user mode.
                unsafe { riscv64_breakpoint_handler(cause, tval, frame, user) };
            }
            RISCV64_EXCEPTION_ENV_CALL_U_MODE => {
                if !user {
                    // SAFETY: `frame` is the live trap frame; `exception_die` only reads
                    // from it before halting.
                    unsafe { exception_die(frame, cause, tval, "syscall from supervisor mode\n") };
                }
                EXCEPTIONS_SYSCALL.add(1);
                // SAFETY: `frame` is the live trap frame the stub passed in.
                unsafe { riscv64_syscall_handler(frame) };
            }
            RISCV64_EXCEPTION_ENV_CALL_S_MODE => {
                // SAFETY: `frame` is the live trap frame; `exception_die` only reads
                // from it before halting.
                unsafe { exception_die(frame, cause, tval, "syscall from supervisor mode\n") };
            }
            // SAFETY: `frame` is the live trap frame; `fatal_exception` only reads
            // from it before halting.
            _ => unsafe { fatal_exception(cause, tval, frame) },
        }
    }

    if do_preempt {
        crate::kernel::thread::preempt();
    }

    if user {
        // SAFETY: `frame` is the live trap frame; signal processing may rewrite the
        // saved user registers in it before the return to userspace.
        unsafe { iframe_process_pending_signals(frame) };
    }
}

/// Entry point called from `stvec.S` on user-mode exception.
///
/// # Safety
///
/// The caller (`stvec.S`) must ensure all preconditions of [`riscv64_exception_handler`] for a
/// U-mode trap frame.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Riscv64UserException(
    iframe: *mut Iframe,
    pc: u64,
    status: u64,
    cause: i64,
) {
    debug_assert!(
        (status & RISCV64_CSR_SSTATUS_SPP) == 0 || (status & RISCV64_CSR_SSTATUS_SPIE) == 0
    );
    // SAFETY: Discharged by this function's `# Safety` contract.
    unsafe { riscv64_exception_handler(iframe, pc, status, cause) };
}

/// Entry point called from `stvec.S` on kernel-mode exception.
///
/// # Safety
///
/// The caller (`stvec.S`) must ensure all preconditions of [`riscv64_exception_handler`] for an
/// S-mode trap frame.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Riscv64KernelException(
    iframe: *mut Iframe,
    pc: u64,
    status: u64,
    cause: i64,
) {
    debug_assert!((status & RISCV64_CSR_SSTATUS_SPP) != 0);
    // SAFETY: Discharged by this function's `# Safety` contract.
    unsafe { riscv64_exception_handler(iframe, pc, status, cause) };
}

/// Entry point called from `stvec.S` on emergency / stack overflow exception.
/// # Safety
/// Emergency handler called on machine stack overflow.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_emergency_exception(
    iframe: *mut Iframe,
    _pc: u64,
    _status: u64,
    cause: i64,
) -> ! {
    // SAFETY: reading `stval` has no side effects.
    let tval = unsafe { riscv64_csr_read::<{ RISCV64_CSR_STVAL }>() };
    // SAFETY: `iframe` is the frame the emergency trap stub built; `exception_die`
    // only reads from it before halting.
    unsafe {
        exception_die(
            iframe,
            cause,
            tval,
            "Exception with bad sscratch value: KERNEL STACK OVERFLOW!\n",
        )
    };
}

/// Process pending signals for the current thread from an interrupt frame.
///
/// # Safety
///
/// The caller must ensure all preconditions of [`crate::kernel::thread::process_pending_signals`]
/// for [`GeneralRegsSource::Iframe`] and `iframe`.
#[inline(always)]
unsafe fn iframe_process_pending_signals(iframe: *mut Iframe) {
    debug_assert!(!iframe.is_null());
    // SAFETY: Discharged by this function's `# Safety` contract.
    unsafe {
        crate::kernel::thread::process_pending_signals(GeneralRegsSource::Iframe, iframe.cast());
    }
}

/// Process pending signals for the current thread from an interrupt frame.
///
/// # Safety
///
/// Same preconditions as [`iframe_process_pending_signals`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_iframe_process_pending_signals(iframe: *mut Iframe) {
    // SAFETY: Discharged by this function's `# Safety` contract.
    unsafe { iframe_process_pending_signals(iframe) };
}

/// Dumps the architectural exception context and user stack bottom.
///
/// # Safety
///
/// The caller must ensure that:
/// - The calling CPU is executing in S-mode with `ConstructFirstThread` completed.
/// - If `context.frame` is non-null, it is aligned and points to a valid, initialized [`Iframe`]
///   with no conflicting mutable references, and when `is_user_accessible(frame.regs.sp)` holds,
///   blocking is permitted with no spinlocks or lockdep-tracked locks held.
#[cold]
#[inline(never)]
pub unsafe fn arch_dump_exception_context(context: &ArchExceptionContext) {
    crate::object::dump_common_exception_context(context);

    if context.frame.is_null() {
        dprintf!(0, "no frame to dump\n");
        return;
    }

    // SAFETY: null was ruled out immediately above, and the context's frame points
    // at the trap frame captured for this exception.
    let frame = unsafe { &*context.frame };
    print_frame(frame);
    dprintf!(0, "cause  {:18} {}\n", context.cause, cause_to_string(context.cause));
    dprintf!(0, "tval   {:#18x}\n", context.tval);

    let usp = frame.regs.sp as usize;
    if is_user_accessible(usp) {
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is a live local sized for the read; `usp` is a user address
        // whose accessibility `arch_copy_from_user` checks, and a fault while reading
        // it is reported rather than trapping.
        if unsafe {
            arch_copy_from_user(
                buf.as_mut_ptr().cast::<core::ffi::c_void>(),
                core::ptr::with_exposed_provenance::<core::ffi::c_void>(usp),
                buf.len(),
            )
        }
        .is_ok()
        {
            dprintf!(0, "bottom of user stack at {:#x}:\n", usp);
            let mut writer = debug::dprintf::KernelConsoleWriter;
            let _ = pretty::hexdump_very_ex_rs(&mut writer, &buf, usp as u64);
        }
    }
}

/// Fills in a userspace exception report context from architectural state.
///
/// # Safety
///
/// Unused on RISC-V; marked `unsafe` to match [`crate::arch_rs::arch_fill_in_exception_context`].
pub unsafe fn arch_fill_in_exception_context(
    arch_context: &ArchExceptionContext,
    report: &mut zx_exception_report_t,
) {
    report.context.synth_code = arch_context.user_synth_code;
    report.context.synth_data = arch_context.user_synth_data;
    let mut riscv_64 = zx_riscv64_exc_data_t::default();
    riscv_64.cause = arch_context.cause as u64;
    riscv_64.tval = arch_context.tval;
    report.context.arch = zx_exception_header_arch_t { riscv_64 };
}

/// Dispatch a user policy error exception.
///
/// # Safety
///
/// The caller must ensure that the calling CPU is executing in S-mode with per-CPU state
/// initialized and `ConstructFirstThread` completed (`THREAD_RUNNING` current thread), interrupts
/// enabled, blocking permitted, and no spinlocks, thread lock, dispatcher locks, lockdep-tracked
/// locks, or active `ChainLockTransaction` held. Any pre-installed `suspended_general_regs` on the
/// current thread must point to a valid, writable, unaliased general-register frame.
pub unsafe fn arch_dispatch_user_policy_exception(
    policy_exception_code: u32,
    policy_exception_data: u32,
) -> Result<(), Status> {
    let context = ArchExceptionContext {
        frame: core::ptr::null_mut(),
        cause: 0,
        tval: 0,
        user_synth_code: policy_exception_code,
        user_synth_data: policy_exception_data,
    };
    // SAFETY: `context` is a live local with a null `frame`, and the caller upholds all thread,
    // interrupt, blocking, and lock preconditions of `dispatch_user_exception`.
    unsafe { crate::object::dispatch_user_exception(ZX_EXCP_POLICY_ERROR, &context) }
}

/// Install suspended register context on a thread.
///
/// # Safety
///
/// The caller must ensure that `thread` points to a valid, live `Thread` with exclusive access to
/// its `suspended_general_regs` state, and that any non-null `context.frame` points to a valid,
/// writable [`Iframe`] that outlives the installed context until [`arch_remove_exception_context`].
#[must_use]
pub unsafe fn arch_install_exception_context(
    thread: *mut core::ffi::c_void,
    context: &ArchExceptionContext,
) -> bool {
    if context.frame.is_null() {
        return false;
    }
    // SAFETY: `thread` comes from the caller under this function's contract, and
    // `context.frame` was null-checked above.
    unsafe {
        super::thread::arch_set_suspended_general_regs(
            thread,
            GeneralRegsSource::Iframe,
            context.frame.cast(),
        );
    }
    true
}

/// Remove suspended register context from a thread.
///
/// # Safety
///
/// The caller must ensure that `thread` points to a valid, live `Thread` with exclusive access to
/// its `suspended_general_regs` state.
pub unsafe fn arch_remove_exception_context(thread: *mut core::ffi::c_void) {
    // SAFETY: `thread` comes from the caller under this function`s contract.
    unsafe { super::thread::arch_reset_suspended_general_regs(thread) };
}

#[cfg(ktest)]
/// Unit tests for exception cause decoding and frame formatting.
#[unittest::suite(name = "riscv64_exceptions")]
mod tests {
    use super::{
        ArchExceptionContext, Iframe, arch_fill_in_exception_context, cause_to_string,
        format_iframe,
    };
    use unittest::{assert_eq, assert_true};
    use zerocopy::FromZeros;
    use zx_types::zx_exception_report_t;

    struct TestBuffer {
        written: usize,
    }

    impl core::fmt::Write for TestBuffer {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            self.written += s.len();
            Ok(())
        }
    }

    /// Test scause code decoding to descriptive strings.
    #[test]
    fn test_cause_to_string() {
        assert_true!(cause_to_string(0) == "Instruction address misaligned");
        assert_true!(cause_to_string(2) == "Illegal instruction");
        assert_true!(cause_to_string(3) == "Breakpoint");
        assert_true!(cause_to_string(8) == "Environment call from U-mode");
        assert_true!(cause_to_string(12) == "Instruction page fault");
        assert_true!(cause_to_string(13) == "Load page fault");
        assert_true!(cause_to_string(15) == "Store/AMO page fault");
        assert_true!(cause_to_string(i64::MIN | 1) == "Software interrupt");
        assert_true!(cause_to_string(i64::MIN | 5) == "Timer interrupt");
        assert_true!(cause_to_string(i64::MIN | 9) == "External interrupt");
    }

    /// Test iframe formatting.
    #[test]
    fn test_format_iframe() {
        let frame = Iframe::default();
        let mut buf = TestBuffer { written: 0 };
        assert_true!(format_iframe(&mut buf, &frame).is_ok());
        assert_true!(buf.written > 100);
    }

    /// Test exception context filling into exception report.
    #[test]
    fn test_fill_in_exception_context() {
        let ctx = ArchExceptionContext {
            frame: core::ptr::null_mut(),
            cause: 13,
            tval: 0xdeadbeef,
            user_synth_code: 42,
            user_synth_data: 99,
        };
        let mut report = zx_exception_report_t::new_zeroed();
        // SAFETY: `ctx.frame` is null.
        unsafe { arch_fill_in_exception_context(&ctx, &mut report) };
        assert_eq!(report.context.synth_code, 42);
        assert_eq!(report.context.synth_data, 99);
        // SAFETY: `arch` is a union; the call above filled in the `riscv_64` variant,
        // which is the only one this architecture writes.
        assert_eq!(unsafe { report.context.arch.riscv_64.cause }, 13);
        // SAFETY: as above.
        assert_eq!(unsafe { report.context.arch.riscv_64.tval }, 0xdeadbeef);
    }
}
