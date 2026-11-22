// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2009 Corey Tabaka
// Copyright (c) 2015 Intel Corporation
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::crashlog::CrashlogRegs;
use super::descriptor::selector_pl;
pub use super::exceptions_ffi::x86_call_external_interrupt_handler;
use super::interrupts::{
    X86_INT_ALIGNMENT_CHECK, X86_INT_APIC_ERROR, X86_INT_APIC_SPURIOUS, X86_INT_APIC_TIMER,
    X86_INT_BOUND_RANGE, X86_INT_BREAKPOINT, X86_INT_DEBUG, X86_INT_DEVICE_NA, X86_INT_DIVIDE_0,
    X86_INT_DOUBLE_FAULT, X86_INT_FPU_FP_ERROR, X86_INT_GP_FAULT, X86_INT_INVALID_OP,
    X86_INT_IPI_GENERIC, X86_INT_IPI_HALT, X86_INT_IPI_INTERRUPT, X86_INT_IPI_RESCHEDULE,
    X86_INT_NMI, X86_INT_OVERFLOW, X86_INT_PAGE_FAULT, X86_INT_PLATFORM_BASE, X86_INT_PLATFORM_MAX,
    X86_INT_SEGMENT_NOT_PRESENT, X86_INT_SIMD_FP_ERROR, X86_INT_STACK_FAULT,
};
use super::registers::{
    self, X86_DR6_BD, X86_DR6_BS, X86_DR6_BT, X86_DR6_MASK, X86_DR6_USER_MASK, X86_FLAGS_AC,
    X86_MSR_IA32_FS_BASE, X86_MSR_IA32_GS_BASE, X86_MSR_IA32_KERNEL_GS_BASE,
};
use super::thread::X86_PFR_RUN_FAULT_HANDLER_BIT;
use super::x86::{read_msr, x86_get_cr2};
use super::{CODE_64_SELECTOR, GeneralRegsSource, Iframe, SyscallRegs, feature, lapic, mp, vm};
use crate::arch_rs::{
    arch_copy_from_user, blocking_disallowed, disable_ints, enable_ints, ints_disabled,
    set_blocking_disallowed,
};
use crate::backtrace::Backtrace;
use crate::counters::define_kcounter;
use crate::kernel::interrupt::{IntHandlerSavedState, int_handler_finish, int_handler_start};
use crate::kernel::mp::{mbx_generic_irq, mbx_interrupt_irq, mbx_reschedule_irq};
use crate::kernel::stats::inc_page_faults;
use crate::kernel::thread::{self, Thread};
use crate::object::{dispatch_user_exception, dump_common_exception_context};
use crate::platform_pc::interrupts::platform_irq;
use crate::platform_rs::power::{
    PlatformHaltAction, ZirconCrashReason, platform_halt, platform_panic_start,
};
use crate::vm::fault::flag as pf_flag;
use arch_x86_aspace_bindings::{PFEX_I, PFEX_P, PFEX_RSV, PFEX_U, PFEX_W};
use core::ffi::c_void;
use core::mem::{align_of, size_of};
use debug::dprintf;
use debug::dprintf::KernelConsoleWriter;
use kprint::kprintln;
use zx_status::Status;
use zx_types::{
    ZX_EXCP_FATAL_PAGE_FAULT, ZX_EXCP_GENERAL, ZX_EXCP_HW_BREAKPOINT, ZX_EXCP_POLICY_ERROR,
    ZX_EXCP_SW_BREAKPOINT, ZX_EXCP_UNDEFINED_INSTRUCTION, zx_exception_report_t,
};

/// Formats a `u64` like C `printf`'s `%#<width>lx`, omitting the `0x` prefix when the value is `0`
/// while preserving right-aligned field width padding.
struct CAltHex(u64);

impl core::fmt::LowerHex for CAltHex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let width = f.width().unwrap_or(0);
        if self.0 == 0 {
            write!(f, "{:width$}", 0, width = width)
        } else {
            write!(f, "{:#width$x}", self.0, width = width)
        }
    }
}

/// 32-bit mask (8 hexadecimal digits) used when formatting segment selectors on page fault dumps.
const X86_8BYTE_MASK: u64 = 0xffff_ffff;

/// Architecture-specific exception context passed to user exception dispatch
/// (`struct arch_exception_context`).
pub type ArchExceptionContext = x86_registers_bindings::arch_exception_context;

zr::static_assert!(size_of::<ArchExceptionContext>() == 32);
zr::static_assert!(align_of::<ArchExceptionContext>() == 8);

define_kcounter!(EXCEPTIONS_DEBUG, "exceptions.debug", Sum);
define_kcounter!(EXCEPTIONS_NMI, "exceptions.nmi", Sum);
define_kcounter!(EXCEPTIONS_BRKPT, "exceptions.breakpoint", Sum);
define_kcounter!(EXCEPTIONS_INVOP, "exceptions.inv_opcode", Sum);
define_kcounter!(EXCEPTIONS_DEV_NA, "exceptions.dev_na", Sum);
define_kcounter!(EXCEPTIONS_DFAULT, "exceptions.double_fault", Sum);
define_kcounter!(EXCEPTIONS_FPU, "exceptions.fpu", Sum);
define_kcounter!(EXCEPTIONS_SIMD, "exceptions.simd", Sum);
define_kcounter!(EXCEPTIONS_GPF, "exceptions.gpf", Sum);
define_kcounter!(EXCEPTIONS_PAGE, "exceptions.page_fault", Sum);
define_kcounter!(EXCEPTIONS_APIC_ERR, "exceptions.apic_error", Sum);
define_kcounter!(EXCEPTIONS_APIC_TIMER, "exceptions.apic_timer", Sum);
define_kcounter!(EXCEPTIONS_IRQ, "exceptions.irq", Sum);
define_kcounter!(EXCEPTIONS_UNHANDLED, "exceptions.unhandled", Sum);
define_kcounter!(EXCEPTIONS_USER, "exceptions.user", Sum);

/// Returns whether the register state indicates that the CPU was executing userland code.
///
/// # Safety
///
/// The caller must ensure that `frame` is non-null, aligned, and points to a valid, initialized
/// [`Iframe`] with no conflicting mutable references for the duration of the call.
#[inline]
unsafe fn is_from_user(frame: *const Iframe) -> bool {
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    unsafe { selector_pl((*frame).cs as u16) != 0 }
}

/// Copies `[addr, addr + len)` from the kernel stack in 128-byte chunks via `rep movsb` and
/// hex-dumps it to the kernel console without triggering Rust typed-load UB on uninitialized stack
/// slots.
///
/// # Safety
///
/// The caller must ensure that:
/// - `RFLAGS.DF == 0` (cleared on exception entry by `cld` in `exceptions.S` and required by the
///   x86-64 System V ABI).
/// - `[addr, addr + len)` is mapped, readable kernel stack memory with exposed provenance for the
///   duration of this call, with no concurrent writes or conflicting active `&mut` borrows if this
///   call returns.
#[sanitize(address = "off")]
#[cold]
#[inline(never)]
unsafe fn hexdump_kernel_stack(mut addr: usize, len: usize) {
    let end = addr.saturating_add(len);
    let mut writer = KernelConsoleWriter;
    let mut buf = core::mem::MaybeUninit::<[u8; 128]>::uninit();
    while addr < end {
        let chunk_size = core::cmp::min(128, end - addr);
        // SAFETY: `[addr, addr + chunk_size)` is mapped, readable kernel stack memory with exposed
        // provenance and no concurrent writes, `buf` is a 128-byte local buffer
        // (`chunk_size <= 128`), and the caller guarantees `RFLAGS.DF == 0` as required by
        // `rep movsb`. `rep movsb` copies raw bytes in assembly without typed Rust loads of
        // potentially uninitialized stack bytes.
        unsafe {
            core::arch::asm!(
                "rep movsb",
                inout("rsi") core::ptr::with_exposed_provenance::<u8>(addr) => _,
                inout("rdi") buf.as_mut_ptr().cast::<u8>() => _,
                inout("rcx") chunk_size => _,
                options(preserves_flags, nostack),
            );
        }
        // SAFETY: `buf[..chunk_size]` was initialized by `rep movsb` above.
        let _ = unsafe {
            pretty::hexdump_very_ex_raw(
                &mut writer,
                buf.as_ptr().cast::<u8>(),
                chunk_size,
                addr as u64,
            )
        };
        addr += chunk_size;
    }
}

/// Dumps the initial faulting thread's kernel stack and backtrace during a `#DF` panic.
///
/// # Safety
///
/// The caller must ensure that:
/// - The calling CPU is executing at CPL 0 on the single-panic `#DF` IST path after
///   `platform_panic_start()`, with `RFLAGS.DF == 0` and `%gs.base` pointing to the calling CPU's
///   `x86_percpu`.
/// - `%gs:current_thread` is either null or points to a valid, constructed `Thread` whose
///   `KernelStack` is not concurrently initialized, mutated, or torn down.
#[sanitize(address = "off")]
#[cold]
#[inline(never)]
unsafe fn dump_double_fault() {
    kprintln!("\ndouble fault, dumping stack of initial faulting thread: ");
    let current_thread = thread::current_get();
    if current_thread.is_null() {
        kprintln!("no current thread");
        return;
    }
    // SAFETY: `current_thread` is non-null, and by `# Safety` points to a valid, constructed
    // `Thread` whose stack metadata is not concurrently modified.
    let main_stack_base = unsafe { thread::get_stack_base(current_thread) };
    // SAFETY: As above.
    let main_stack_top = unsafe { thread::get_stack_top(current_thread) };
    let main_stack_size = main_stack_top.saturating_sub(main_stack_base);
    kprintln!("kernel stack base: 0x{:x} size: {} ", main_stack_base, main_stack_size);

    // SAFETY: Discharged by `# Safety`: `RFLAGS.DF == 0`, and `[main_stack_base, main_stack_top)`
    // is the interrupted thread's mapped kernel stack on the non-returning `#DF` IST panic path.
    unsafe { hexdump_kernel_stack(main_stack_base, main_stack_size) };

    #[cfg(sanitize = "safestack")]
    {
        // SAFETY: `current_thread` is non-null and valid as above.
        let unsafe_stack_base = unsafe { thread::get_unsafe_stack_base(current_thread) };
        // SAFETY: `current_thread` is non-null and valid as above.
        let unsafe_stack_top = unsafe { thread::get_unsafe_stack_top(current_thread) };
        kprintln!(
            "\nunsafe stack enabled. base: {:x} size: {}\n",
            unsafe_stack_base,
            unsafe_stack_top.saturating_sub(unsafe_stack_base)
        );
    }
    #[cfg(not(sanitize = "safestack"))]
    {
        kprintln!("no unsafe stack.");
    }
    kprintln!("attempting to backtrace initially faulting thread:");
    let mut bt = Backtrace::default();
    // SAFETY: `%gs.base` and `current_thread` are valid, `ReadStack` rejects out-of-bounds or
    // uninitialized `KernelStack` bounds, and this runs on the non-returning `#DF` panic path where
    // stack reads cannot alias live `&mut` borrows.
    unsafe { thread::current_get_backtrace_with_fp(main_stack_base, &mut bt) };
    // SAFETY: `stdout()` is non-null and aligned, pointing to `gStdout`, which has a thread-safe
    // write callback and is not concurrently mutated on the halted panic path.
    unsafe { bt.print_without_version(libc::stdio::stdout()) };
}

/// Prints `frame` to the kernel console.
///
/// # Safety
///
/// The caller must ensure that CPL == 0 (for `%cr2` access) and that `frame` is non-null, aligned,
/// and points to a valid, initialized [`Iframe`] with no conflicting mutable references for the
/// duration of the call.
#[sanitize(address = "off")]
unsafe fn print_frame(frame: *const Iframe) {
    // SAFETY: Discharged by this function's `# Safety` contract: CPL == 0 and `frame` points to a
    // valid, readable `Iframe`.
    unsafe { super::registers::print_frame(frame, &mut KernelConsoleWriter) };
}

/// Dumps `frame`, user segment base MSRs, and up to 256 bytes of the kernel stack at `frame`.
///
/// # Safety
///
/// The caller must ensure that:
/// - CPL == 0, `RFLAGS.DF == 0`, and `%gs.base` points to the calling CPU's `x86_percpu`, with
///   `%gs:current_thread` either null or pointing to a valid, constructed `Thread` whose
///   `KernelStack` metadata is not concurrently modified.
/// - `frame` is non-null, aligned, and points to a valid, initialized [`Iframe`] with no
///   conflicting mutable references for the duration of the call.
/// - If `(*frame).cs == CODE_64_SELECTOR` and `frame` lies within the current thread's kernel
///   stack, `[frame, min(frame + 256, stack_top))` is mapped with no conflicting active `&mut`
///   borrows across this call.
#[sanitize(address = "off")]
#[cold]
#[inline(never)]
unsafe fn dump_fault_frame(frame: *const Iframe) {
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    unsafe { print_frame(frame) };
    // SAFETY: `IA32_FS_BASE` and `IA32_KERNEL_GS_BASE` are architectural MSRs readable at CPL 0.
    unsafe {
        let mut lw = KernelConsoleWriter;
        let _ = core::fmt::write(
            &mut lw,
            format_args!(
                "  user FS.BASE: {:32x}   user GS.BASE: {:31x}\n",
                CAltHex(read_msr(X86_MSR_IA32_FS_BASE)),
                CAltHex(read_msr(X86_MSR_IA32_KERNEL_GS_BASE)),
            ),
        );
    }
    // dump the bottom of the current stack
    // SAFETY: Caller guarantees `frame` points to a valid, initialized `Iframe`.
    if unsafe { (*frame).cs } == u64::from(CODE_64_SELECTOR) {
        let stack_addr = frame.expose_provenance();
        kprintln!("bottom of kernel stack at {:p}:", frame);

        // Clamp the dump to the current thread's kernel stack top when `frame` resides on the
        // main kernel stack, and fall back to `size_of::<Iframe>()` (176 bytes) on IST stacks or
        // early boot so `rep movsb` never reads past stack bounds into an unmapped guard page.
        let current_thread = thread::current_get();
        let mut max_bytes = 256;
        let mut clamped = false;
        if !current_thread.is_null() {
            // SAFETY: By `# Safety`, a non-null `current_thread` points to a valid, constructed
            // `Thread`.
            let kernel_base = unsafe { thread::get_stack_base(current_thread) };
            // SAFETY: As above.
            let kernel_top = unsafe { thread::get_stack_top(current_thread) };
            if kernel_base <= stack_addr && stack_addr < kernel_top {
                max_bytes = core::cmp::min(max_bytes, kernel_top - stack_addr);
                clamped = true;
            }
        }
        if !clamped {
            max_bytes = size_of::<Iframe>();
        }

        // SAFETY: `RFLAGS.DF == 0` by `# Safety`, `stack_addr` has exposed provenance, and
        // `[stack_addr, stack_addr + max_bytes)` lies either within `[kernel_base, kernel_top)`
        // (`clamped`) or strictly within the valid 176-byte `Iframe` (`!clamped`), with no
        // conflicting active `&mut` borrows.
        unsafe { hexdump_kernel_stack(stack_addr, max_bytes) };
    }
}

/// Records crashlog registers, dumps the fault frame (and double-fault stack if `#DF`), and halts.
///
/// # Safety
///
/// The caller must ensure that:
/// - CPL == 0, `RFLAGS.DF == 0`, and `%gs.base` points to the calling CPU's `x86_percpu`, with
///   `%gs:current_thread` either null or pointing to a valid, constructed `Thread` whose
///   `KernelStack` metadata is not concurrently modified.
/// - `frame` is non-null, aligned, and points to a valid, initialized [`Iframe`] on the current
///   stack (with `[frame, min(frame + 256, stack_top))` mapped if on the current kernel stack) that
///   remains live and unmutated because this function never returns.
#[sanitize(address = "off")]
#[cold]
#[inline(never)]
unsafe fn exception_die(frame: *mut Iframe, msg: &str) -> ! {
    platform_panic_start();
    // No need to try to optimize with rdfsbase and rdgsbase here.
    // SAFETY: CPL == 0 for `CR2` and MSR reads, and `frame` points to a valid, aligned `Iframe` on
    // this non-returning panic handler's stack so it remains live and unmutated for the remainder
    // of the kernel's execution as required by `set_crashlog_regs`.
    unsafe {
        crate::crashlog::set_crashlog_regs(CrashlogRegs {
            iframe: frame.cast_const(),
            cr2: x86_get_cr2(),
            fsbase: read_msr(X86_MSR_IA32_FS_BASE),
            gsbase: read_msr(X86_MSR_IA32_GS_BASE),
            swapgs_gsbase: read_msr(X86_MSR_IA32_KERNEL_GS_BASE),
        });
    }
    // SAFETY: Caller guarantees `frame` points to a valid, initialized `Iframe`.
    let vector = unsafe { (*frame).vector };
    // Note: We intentionally omit dumping the user stack via `arch_copy_from_user` here because
    // blocking is disallowed in `exception_die`, matching arm64 and riscv64. User stack dumping
    // when blocking is permitted is handled in `arch_dump_exception_context`.
    kprintln!("vector {}", vector);
    dprintf!(CRITICAL, "{}", msg);
    // SAFETY: Discharged by this function's `# Safety` contract (and peer CPUs were halted by
    // `platform_panic_start()` above).
    unsafe { dump_fault_frame(frame) };
    // Attempt to dump additional debugging info in the event of a double fault. Update crash logs
    // first in case we trigger an additional fault.
    if vector == u64::from(X86_INT_DOUBLE_FAULT.0) {
        // SAFETY: `platform_panic_start()` has completed at CPL 0 with `RFLAGS.DF == 0` and valid
        // `%gs.base` / `current_thread` per `# Safety`.
        unsafe { dump_double_fault() };
    }
    platform_halt(PlatformHaltAction::Halt, ZirconCrashReason::Panic)
}

/// Attempts to dispatch `exception_type` to user-space exception handlers if `frame` originated in
/// user mode, returning `true` if handled.
///
/// # Safety
///
/// The caller must ensure that:
/// - CPL == 0, `RFLAGS.DF == 0`, interrupts are disabled, `int_handler_start` has incremented
///   `preempt_disable` with no `PreemptDisabledToken` live across this call, and `%gs.base` points
///   to the calling CPU's `x86_percpu` with a constructed `current_thread`.
/// - `frame` is non-null, aligned, and points to a valid, initialized, writable [`Iframe`] with no
///   conflicting Rust references for the duration of the call.
/// - If `is_from_user(frame)` holds, the current thread is in `THREAD_RUNNING` state in a context
///   where blocking is permitted once interrupts are enabled, and no spinlocks, thread lock,
///   dispatcher locks, lockdep-tracked locks, or active `ChainLockTransaction` are held.
#[cold]
#[inline(never)]
unsafe fn try_dispatch_user_exception(frame: *mut Iframe, exception_type: u32) -> bool {
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    if unsafe { is_from_user(frame) } {
        let context = ArchExceptionContext { frame: frame.cast(), ..Default::default() };
        set_blocking_disallowed(false);
        enable_ints();
        // SAFETY: Pairs with the `int_handler_start` `preempt_disable` on exception entry; no
        // `PreemptDisabledToken` is live across user exception dispatch, and `preempt_disable`
        // below restores the count before `int_handler_finish`.
        unsafe { thread::preempt_reenable() };
        // SAFETY: Discharged by `# Safety`: CPL == 0, `RFLAGS.DF == 0`, `%gs.base` and
        // `current_thread` (`THREAD_RUNNING`) are initialized, interrupts are enabled, blocking is
        // permitted, no spinlocks, thread/dispatcher/lockdep-tracked locks, or active
        // `ChainLockTransaction` are held, and `context` references a valid, writable user `frame`
        // with no conflicting Rust references.
        let handled = unsafe { dispatch_user_exception(exception_type, &context) }.is_ok();
        thread::preempt_disable();
        disable_ints();
        set_blocking_disallowed(true);
        if handled {
            return true;
        }
    }
    false
}

/// Handles a `#DB` debug exception.
///
/// # Safety
///
/// The caller must ensure all preconditions of [`try_dispatch_user_exception`] and
/// [`exception_die`], and that `DR7.GD` is clear so `%dr6`/`%dr7` accesses at CPL 0 do not trap.
#[cold]
unsafe fn x86_debug_handler(frame: *mut Iframe) {
    // DR6 is the status register that explains what exception happened (single step, hardware
    // breakpoint, etc.).
    //
    // We only need to keep track of DR6 because the other state doesn't change and the only way
    // to actually change the debug registers for a thread is through the thread_write_state
    // syscall.
    let thread = thread::current_get();
    // We save the current state so that exception handlers can check what kind of exception it was.
    // SAFETY: By `# Safety`, CPL == 0 and `DR7.GD` is clear on kernel threads/user debug state.
    let dr6 = unsafe { registers::read_debug_status() };
    if !thread.is_null() {
        // SAFETY: `thread` is the current running thread on this CPU with interrupts disabled and
        // no conflicting references to `debug_state.dr6`.
        unsafe {
            let dr6_ptr =
                core::ptr::addr_of_mut!((*super::thread::thread_arch(thread)).debug_state.dr6);
            dr6_ptr.write(dr6);
        }
    }
    // TODO(https://fxbug.dev/453746441): Due to CVE-2025-21839 when receiving a debug exception
    // the value in DR6 could be an older now stale value. There is nothing we can do to recover it
    // and if this was a trap exception (which we would know by inspecting DR6) we cannot even
    // return to the thread to regenerate it.
    // Once no longer running on impacted hosts this check can either be removed or upgraded to an
    // OOPS / assertion.
    if (dr6 & X86_DR6_USER_MASK) == 0 {
        core::hint::cold_path();
        let thread_dr7 = if !thread.is_null() {
            // SAFETY: `thread` is the current running thread on this CPU and is non-null.
            unsafe { (*super::thread::thread_arch(thread)).debug_state.dr7 }
        } else {
            0
        };
        // SAFETY: By `# Safety`, CPL is 0 and `DR7.GD` is clear.
        let (raw_dr6, regs) = unsafe {
            let raw_dr6 = registers::read_raw_debug_status();
            let regs = registers::read_hw_debug_regs();
            (raw_dr6, regs)
        };
        kprintln!(
            "Unexpected spurious #DB with DR6: 0x{:x} DR7(thread): 0x{:x} DR7(hw): 0x{:x}",
            raw_dr6,
            thread_dr7,
            regs.dr7
        );
    }
    // The value in the thread's debug_state struct is authoritative. Reset the hardware state of
    // DR6 to ensure that if we context switch to another thread that thread won't see the DR6
    // meant for this thread.
    // SAFETY: By `# Safety`, CPL is 0 and `DR7.GD` is clear.
    unsafe { registers::write_debug_status(X86_DR6_MASK) };
    // NOTE: a HW breakpoint exception can also represent a single step.
    // TODO(https://fxbug.dev/42108005): Is it worth separating this into two separate exceptions?
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    if unsafe { try_dispatch_user_exception(frame, ZX_EXCP_HW_BREAKPOINT) } {
        // If the exception was successfully handled, we mask the debug single step bit, as the
        // cpu doesn't automatically do it.
        //
        // After this point, any exception handler that reads DR6 won't see the single step bit
        // active.
        if !thread.is_null() {
            // SAFETY: Interrupts are disabled again on the current thread (`thread`) with no
            // conflicting references to `debug_state.dr6`.
            unsafe {
                let dr6_ptr =
                    core::ptr::addr_of_mut!((*super::thread::thread_arch(thread)).debug_state.dr6);
                let current_dr6 = dr6_ptr.read();
                dr6_ptr.write(current_dr6 & !(X86_DR6_BD | X86_DR6_BS | X86_DR6_BT));
            }
        }
        return;
    }
    // If the breakpoint couldn't be handled check if it's because it happened in kernel mode. This
    // can occur if a watchpoint is set on a memory address that the kernel accesses via a user
    // copy.
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    if unsafe { !is_from_user(frame) } {
        // The only way the kernel should have taken a breakpoint is specifically for a memory
        // access, it should not have been possible to make it single step or perform any other
        // action. Unfortunately we cannot check for this since although DR6 is updated by the
        // hardware, it does not clear bits for actions that have not happened, and so the user
        // could have (since the last handled debug exception) manually set bits in DR6.
        return;
    }
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    unsafe { exception_die(frame, "unhandled hw breakpoint, halting\n") };
}

/// NMI handler (`x86_nmi_handler`).
///
/// Separate from [`exception_handler`] because we must take care to avoid calling *any*
/// non-reentrant-safe code that may have been interrupted by the NMI. In particular, it's crucial
/// that we don't acquire any spinlocks in the NMI handler because the NMI could have interrupted
/// the thread while it was holding the spinlock we would then attempt to (re)acquire.
///
/// # Safety
///
/// The caller must ensure that:
/// - CPL is 0, interrupts are disabled in NMI context, and `%gs.base` points to the current CPU's
///   `x86_percpu` (whose `high_level_percpu` is either null during early boot/AP bringup or points
///   to the CPU's initialized `PerCpu`).
/// - `%gs:current_thread` is either null or points to a valid, constructed `Thread` whose
///   `KernelStack` metadata is not concurrently modified; whenever `frame.rbp` falls within
///   `[stack.base(), stack.top())`, `frame.rbp` is a valid, initialized frame pointer on the
///   thread's stack with no conflicting live `&mut` stack borrows.
///
/// Note: `#[sanitize(address = "off")]` is applied on the sole caller
/// `x86_nmi_handler` in `exceptions_ffi.rs`, which `#[inline(always)]` inlines this into.
#[inline(always)]
pub(super) unsafe fn nmi_handler(frame: &Iframe) {
    // Generally speaking, NMIs don't "stack".  That is, further NMIs are disabled
    // until the execution of the next IRET instruction so to prevent reentrancy
    // we must take care to not execute an IRET until the NMI handler is complete.
    //
    // Keeping interrupts disabled and avoiding faults is critical because the
    // *next* IRET to execute will enable further NMIs.  Consider what might
    // happen if we enabled interrupts here.  If interrupts were enabled, a timer
    // interrupt might fire and stack the timer interrupt handler on top of this
    // NMI handler.  When the timer interrupt handler completes, and issues an
    // IRET, NMIs would be re-enabled even though this handler is still on the
    // stack.  We'd be open to unexpected reentrancy.
    debug_assert!(ints_disabled());
    // SAFETY: `%gs.base` points to this CPU's `x86_percpu` per `# Safety`; `high_level_percpu`
    // remains null during early boot and early AP bringup until `PerCpu` is initialized.
    if unsafe { !(*mp::get_percpu()).high_level_percpu.is_null() } {
        EXCEPTIONS_NMI.add(1);
    }
    // SAFETY: Discharged by this function's `# Safety` contract: CPL == 0, interrupts are disabled
    // in NMI context with `%gs.base` and `current_thread` stack preconditions satisfied.
    unsafe {
        crate::backtrace::GLOBAL_CPU_CONTEXT_EXCHANGE.handle_request(frame.rbp as usize, frame);
    }
    debug_assert!(ints_disabled());
}

/// Handles a `#BP` software breakpoint exception.
///
/// # Safety
///
/// The caller must ensure all preconditions of [`try_dispatch_user_exception`] and
/// [`exception_die`].
#[cold]
unsafe fn x86_breakpoint_handler(frame: *mut Iframe) {
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    if unsafe { try_dispatch_user_exception(frame, ZX_EXCP_SW_BREAKPOINT) } {
        return;
    }
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    unsafe { exception_die(frame, "unhandled sw breakpoint, halting\n") };
}

/// Handles a `#GP` general protection fault.
///
/// # Safety
///
/// The caller must ensure all preconditions of [`try_dispatch_user_exception`] and
/// [`exception_die`].
#[cold]
unsafe fn x86_gpf_handler(frame: *mut Iframe) {
    debug_assert!(ints_disabled());
    // Check if we were doing a GPF test, e.g. to check if an MSR exists.
    // SAFETY: Interrupts are disabled, `%gs.base` points to this CPU's `x86_percpu`, and `frame` is
    // a valid, exclusively writable `Iframe`.
    unsafe {
        let percpu = mp::get_percpu();
        let target = (*percpu).gpf_return_target;
        if target != 0 {
            assert!(!is_from_user(frame));
            // Set up return to new address
            (*frame).ip = target as u64;
            (*percpu).gpf_return_target = 0;
            return;
        }
    }
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    if unsafe { try_dispatch_user_exception(frame, ZX_EXCP_GENERAL) } {
        return;
    }
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    unsafe { exception_die(frame, "unhandled gpf, halting\n") };
}

/// Handles a `#UD` invalid opcode exception.
///
/// # Safety
///
/// The caller must ensure all preconditions of [`try_dispatch_user_exception`] and
/// [`exception_die`].
#[cold]
unsafe fn x86_invop_handler(frame: *mut Iframe) {
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    if unsafe { try_dispatch_user_exception(frame, ZX_EXCP_UNDEFINED_INSTRUCTION) } {
        return;
    }
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    unsafe { exception_die(frame, "invalid opcode, halting\n") };
}

/// Handles an unrecoverable `#DF` double fault.
///
/// # Safety
///
/// The caller must ensure all preconditions of [`exception_die`].
#[cold]
#[inline(never)]
unsafe fn x86_df_handler(frame: *mut Iframe) -> ! {
    // Do not give the user exception handler the opportunity to handle double
    // faults, since they indicate an unexpected system state and cannot be
    // recovered from.
    EXCEPTIONS_DFAULT.add(1);
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    unsafe { exception_die(frame, "double fault, halting\n") }
}

/// Handles synchronous architectural exceptions routed to `ZX_EXCP_GENERAL`.
///
/// # Safety
///
/// The caller must ensure all preconditions of [`try_dispatch_user_exception`] and
/// [`exception_die`].
#[cold]
#[inline(never)]
unsafe fn x86_unhandled_exception(frame: *mut Iframe) {
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    if unsafe { try_dispatch_user_exception(frame, ZX_EXCP_GENERAL) } {
        return;
    }
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    unsafe { exception_die(frame, "unhandled exception, halting\n") };
}

/// Prints the `<PAGE FAULT>` diagnostic banner for `frame` and faulting address `cr2`.
#[cold]
#[inline(never)]
#[sanitize(address = "off")]
fn x86_dump_pfe(frame: &Iframe, cr2: u64) {
    let error_code = frame.err_code;
    let v_addr = cr2;
    let ssp = frame.user_ss & X86_8BYTE_MASK;
    let sp = frame.user_sp;
    let cs = frame.cs & X86_8BYTE_MASK;
    let ip = frame.ip;
    dprintf!(CRITICAL, "<PAGE FAULT> Instruction Pointer   = {:#x}:{:#x}\n", cs, ip);
    dprintf!(CRITICAL, "<PAGE FAULT> Stack Pointer         = {:#x}:{:#x}\n", ssp, sp);
    dprintf!(CRITICAL, "<PAGE FAULT> Fault Linear Address  = {:#x}\n", v_addr);
    dprintf!(CRITICAL, "<PAGE FAULT> Error Code Value      = {:#x}\n", error_code);
    dprintf!(
        CRITICAL,
        "<PAGE FAULT> Error Code Type       = {} {} {}{}, {}\n",
        if (error_code & u64::from(PFEX_U)) != 0 { "user" } else { "supervisor" },
        if (error_code & u64::from(PFEX_W)) != 0 { "write" } else { "read" },
        if (error_code & u64::from(PFEX_I)) != 0 { "instruction" } else { "data" },
        if (error_code & u64::from(PFEX_RSV)) != 0 { " rsv" } else { "" },
        if (error_code & u64::from(PFEX_P)) != 0 {
            "protection violation"
        } else {
            "page not present"
        },
    );
}

/// Dumps fatal page-fault state, current thread diagnostics, and halts.
///
/// # Safety
///
/// The caller must ensure all preconditions of [`exception_die`].
#[sanitize(address = "off")]
#[cold]
#[inline(never)]
unsafe fn x86_fatal_pfe_handler(frame: *mut Iframe, cr2: u64) -> ! {
    // Start panic mode and halt peer CPUs before dumping unlocked current-thread fields.
    platform_panic_start();
    // SAFETY: `frame` points to a valid, initialized `Iframe` on this non-returning panic path,
    // and `frame_ref` is only used for `x86_dump_pfe` and reading `err_code` before `exception_die`
    // uses the original raw `frame` pointer.
    let error_code = {
        let frame_ref = unsafe { &*frame };
        x86_dump_pfe(frame_ref, cr2);
        frame_ref.err_code
    };
    let current = thread::current_get();
    if !current.is_null() {
        // SAFETY: `%gs.base` is initialized, `current` is non-null (`ConstructFirstThread` has
        // completed), and `platform_panic_start()` above has halted peer CPUs so lock-skipping
        // thread state reads do not race with concurrent mutations.
        unsafe { thread::current_dump_during_panic(true) };
    }
    if (error_code & u64::from(PFEX_U)) != 0 {
        // User mode page fault
        if (error_code & !u64::from(PFEX_U | PFEX_W | PFEX_P)) == 0 {
            // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
            unsafe { exception_die(frame, "User Page Fault exception, halting\n") };
        }
    } else {
        // Supervisor mode page fault
        if (error_code & !u64::from(PFEX_W | PFEX_P)) == 0 {
            // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
            unsafe { exception_die(frame, "Supervisor Page Fault exception, halting\n") };
        }
    }
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    unsafe { exception_die(frame, "unhandled page fault, halting\n") }
}

/// Outlined cold path that constructs [`ArchExceptionContext`] and dispatches a fatal user-space
/// page fault, keeping `ArchExceptionContext`'s 32-byte SafeStack slot out of
/// [`exception_handler`]'s hot IRQ and resolved-page-fault prologue.
///
/// # Safety
///
/// The caller must ensure that CPL == 0, `RFLAGS.DF == 0`, `%gs.base` and `current_thread`
/// (`THREAD_RUNNING`, with `KernelStack` metadata not concurrently modified) are initialized,
/// interrupts are enabled, blocking is permitted, no spinlocks, thread lock, dispatcher locks,
/// lockdep-tracked locks, or active `ChainLockTransaction` are held, and `frame` points to a valid,
/// live, writable user-mode [`Iframe`] with no conflicting Rust references for the duration of the
/// call.
#[cold]
#[inline(never)]
unsafe fn dispatch_fatal_user_page_fault(
    frame: *mut Iframe,
    va: usize,
    pf_err: Status,
) -> Result<(), Status> {
    EXCEPTIONS_USER.add(1);
    let context = ArchExceptionContext {
        frame: frame.cast(),
        cr2: va as u64,
        user_synth_code: pf_err.into_raw() as u32,
        user_synth_data: 0,
        is_page_fault: true,
    };
    // SAFETY: Discharged by this function's `# Safety` contract: `context` is a valid
    // `ArchExceptionContext` referencing `frame`, interrupts are enabled, blocking is permitted,
    // and no conflicting locks or transactions are held.
    unsafe { dispatch_user_exception(ZX_EXCP_FATAL_PAGE_FAULT, &context) }
}

/// Handles a `#PF` page fault exception.
///
/// # Safety
///
/// The caller must ensure that:
/// - CPL == 0, `RFLAGS.DF == 0`, interrupts are disabled, `int_handler_start` has incremented
///   `preempt_disable` with no `PreemptDisabledToken` live across this call, and `%gs.base` points
///   to the calling CPU's `x86_percpu` with `%gs:current_thread` either null or pointing to a
///   valid, constructed `Thread` whose `KernelStack` metadata is not concurrently modified.
/// - `frame` is non-null, aligned, and points to a valid, initialized, writable [`Iframe`] with no
///   conflicting Rust references (and remains live on the current stack if `exception_die` runs).
/// - Whenever the fault originates from user mode (`PFEX_U != 0`) or kernel user-copy with
///   `X86_PFR_RUN_FAULT_HANDLER_BIT` set, the current thread is constructed and in `THREAD_RUNNING`
///   state (with an attached user `VmAspace` for user-accessible `cr2`) in a context where blocking
///   is permitted once interrupts are enabled, with no spinlocks, thread lock, dispatcher locks,
///   lockdep-tracked locks, conflicting VM locks, or active `ChainLockTransaction` held.
unsafe fn x86_pfe_handler(frame: *mut Iframe, cr2: u64) -> Result<(), Status> {
    // Handle a page fault exception.
    // SAFETY: `frame` is non-null and points to a valid, initialized `Iframe`.
    let (error_code, flags_val) = unsafe { ((*frame).err_code, (*frame).flags) };
    let va = cr2 as usize;
    let thread = thread::current_get();
    let mut pfr = if !thread.is_null() {
        // SAFETY: `thread` is verified non-null and points to the current CPU's valid `Thread`
        // with interrupts disabled.
        unsafe { (*super::thread::thread_arch(thread)).page_fault_resume }
    } else {
        core::hint::cold_path();
        0
    };
    if (error_code & u64::from(PFEX_U)) == 0 {
        core::hint::cold_path();
        // Any page fault in kernel mode that's not during user-copy is a bug.
        // Check for an SMAP violation.
        //
        // By policy, the kernel is not allowed to access user memory except when
        // performing a user_copy. SMAP is used to enforce the policy.
        if feature::x86_feature_has_smap() // CPU supports SMAP
            && (flags_val & X86_FLAGS_AC) == 0 // SMAP was enabled at time of fault
            && vm::is_user_accessible(va)
        // fault address is a user address
        {
            core::hint::cold_path();
            kprintln!(
                "x86_pfe_handler: potential SMAP failure, supervisor access at address {:#x}",
                va
            );
            pfr = 0;
        }
        if pfr == 0 {
            core::hint::cold_path();
            // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
            unsafe { exception_die(frame, "page fault in kernel mode\n") };
        }
    }
    // Check for flags we're not prepared to handle.
    if (error_code & !u64::from(PFEX_I | PFEX_U | PFEX_W | PFEX_P)) != 0 {
        core::hint::cold_path();
        kprintln!("x86_pfe_handler: unhandled error code bits set, error code {:#x}", error_code);
        return Err(Status::NOT_SUPPORTED);
    }
    // Convert the PF error codes to page fault flags.
    let mut flags: u32 = 0;
    if (error_code & u64::from(PFEX_W)) != 0 {
        flags |= pf_flag::WRITE;
    }
    if (error_code & u64::from(PFEX_U)) != 0 {
        flags |= pf_flag::USER;
    }
    if (error_code & u64::from(PFEX_I)) != 0 {
        flags |= pf_flag::INSTRUCTION;
    }
    if (error_code & u64::from(PFEX_P)) == 0 {
        flags |= pf_flag::NOT_PRESENT;
    }
    // Check if the page fault handler should be skipped. It is skipped if there's a
    // page_fault_resume address and the highest bit is 0.
    if pfr != 0 && (pfr & (1u64 << X86_PFR_RUN_FAULT_HANDLER_BIT)) == 0 {
        core::hint::cold_path();
        // Need to reconstruct the canonical resume address by ensuring it is correctly sign
        // extended. Double check the bit before X86_PFR_RUN_FAULT_HANDLER_BIT was set (indicating
        // kernel address) and fill it in.
        debug_assert!((pfr & (1u64 << (X86_PFR_RUN_FAULT_HANDLER_BIT - 1))) != 0);
        // SAFETY: `frame` is non-null and valid for exclusive writes.
        unsafe {
            (*frame).ip = pfr | (1u64 << X86_PFR_RUN_FAULT_HANDLER_BIT);
            (*frame).rdx = va as u64;
            (*frame).rcx = u64::from(flags);
        }
        return Ok(());
    }
    // Now that we've checked whether we're capturing faults (we're not), we must
    // therefore be in a context where it's safe for the thread to block so we can
    // re-enable preemption and interrupts at this point.
    debug_assert!(crate::arch_rs::ops::num_spinlocks_held() == 0);
    set_blocking_disallowed(false);
    enable_ints();
    // SAFETY: Pairs with the `int_handler_start` `preempt_disable` on exception entry; no
    // `PreemptDisabledToken` is live across page fault handling, and `_cleanup` restores
    // `preempt_disable` before `int_handler_finish`.
    unsafe { thread::preempt_reenable() };
    // Make sure we put interrupts back as we exit.
    let _cleanup = zr::defer(|| {
        thread::preempt_disable();
        disable_ints();
        set_blocking_disallowed(true);
    });
    // Call the high level page fault handler.
    // SAFETY: By `# Safety` and the state transitions above, the current CPU has a running
    // `Thread` (with an attached user `VmAspace` for user addresses), interrupts are enabled,
    // blocking is permitted, and no spinlocks, lockdep-tracked locks, conflicting VM locks, or
    // active `ChainLockTransaction` are held.
    let Err(pf_err) = (unsafe { crate::vm::page_fault_handler(va, flags) }) else {
        return Ok(());
    };
    core::hint::cold_path();
    // If the high level page fault handler can't deal with it,
    // resort to trying to recover first, before bailing.

    // Check if a resume address is specified, and just return to it if so.
    if pfr != 0 {
        core::hint::cold_path();
        // Having the X86_PFR_RUN_FAULT_HANDLER_BIT set should have already resulted in a valid
        // sign extended canonical address. Double check the bit before, which should be a one.
        debug_assert!((pfr & (1u64 << (X86_PFR_RUN_FAULT_HANDLER_BIT - 1))) != 0);
        // SAFETY: `frame` is non-null and valid for exclusive writes.
        unsafe { (*frame).ip = pfr };
        return Ok(());
    }
    // Let high level code deal with this.
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    if unsafe { is_from_user(frame) } {
        // SAFETY: Interrupts are enabled, blocking is permitted, no spinlocks, thread lock,
        // dispatcher locks, lockdep-tracked locks, or active `ChainLockTransaction` are held, and
        // `frame` is valid and exclusively writable.
        return unsafe { dispatch_fatal_user_page_fault(frame, va, pf_err) };
    }
    // fall through to fatal path
    Err(Status::NOT_SUPPORTED)
}

/// Dispatches exception or interrupt `vector` to its corresponding architectural or IRQ handler.
///
/// Marked `#[inline(always)]` with `vector` passed from [`exception_handler`] so release builds do
/// not emit a second stack frame on every IRQ and jump-thread `INT_PAGE_FAULT` without reloading
/// `(*frame).vector`.
///
/// # Safety
///
/// The caller must ensure `PerCpu` is initialized on the calling CPU, all preconditions of
/// [`x86_debug_handler`], [`x86_pfe_handler`], and [`exception_die`] hold, and when `vector` is a
/// local APIC, IPI, or platform IRQ vector, that the local APIC (`apic_vm_init` /
/// `apic_local_init`) is initialized on the calling CPU with `TimerLock`, `IpiTaskLock`, and
/// `INTERRUPT_MANAGER`'s spinlock not held.
#[inline(always)]
unsafe fn handle_exception_types(frame: *mut Iframe, vector: u64) {
    const INT_DEBUG: u64 = X86_INT_DEBUG.0 as u64;
    const INT_BREAKPOINT: u64 = X86_INT_BREAKPOINT.0 as u64;
    const INT_INVALID_OP: u64 = X86_INT_INVALID_OP.0 as u64;
    const INT_DEVICE_NA: u64 = X86_INT_DEVICE_NA.0 as u64;
    const INT_DOUBLE_FAULT: u64 = X86_INT_DOUBLE_FAULT.0 as u64;
    const INT_FPU_FP_ERROR: u64 = X86_INT_FPU_FP_ERROR.0 as u64;
    const INT_SIMD_FP_ERROR: u64 = X86_INT_SIMD_FP_ERROR.0 as u64;
    const INT_GP_FAULT: u64 = X86_INT_GP_FAULT.0 as u64;
    const INT_PAGE_FAULT: u64 = X86_INT_PAGE_FAULT.0 as u64;
    const INT_APIC_SPURIOUS: u64 = X86_INT_APIC_SPURIOUS.0 as u64;
    const INT_APIC_ERROR: u64 = X86_INT_APIC_ERROR.0 as u64;
    const INT_APIC_TIMER: u64 = X86_INT_APIC_TIMER.0 as u64;
    const INT_IPI_GENERIC: u64 = X86_INT_IPI_GENERIC.0 as u64;
    const INT_IPI_RESCHEDULE: u64 = X86_INT_IPI_RESCHEDULE.0 as u64;
    const INT_IPI_INTERRUPT: u64 = X86_INT_IPI_INTERRUPT.0 as u64;
    const INT_IPI_HALT: u64 = X86_INT_IPI_HALT.0 as u64;
    const INT_PLATFORM_BASE: u64 = X86_INT_PLATFORM_BASE.0 as u64;
    const INT_PLATFORM_MAX: u64 = X86_INT_PLATFORM_MAX.0 as u64;
    const INT_DIVIDE_0: u64 = X86_INT_DIVIDE_0.0 as u64;
    const INT_OVERFLOW: u64 = X86_INT_OVERFLOW.0 as u64;
    const INT_BOUND_RANGE: u64 = X86_INT_BOUND_RANGE.0 as u64;
    const INT_SEGMENT_NOT_PRESENT: u64 = X86_INT_SEGMENT_NOT_PRESENT.0 as u64;
    const INT_STACK_FAULT: u64 = X86_INT_STACK_FAULT.0 as u64;
    const INT_ALIGNMENT_CHECK: u64 = X86_INT_ALIGNMENT_CHECK.0 as u64;

    match vector {
        INT_DEBUG => {
            EXCEPTIONS_DEBUG.add(1);
            // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
            unsafe { x86_debug_handler(frame) };
        }
        INT_BREAKPOINT => {
            EXCEPTIONS_BRKPT.add(1);
            // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
            unsafe { x86_breakpoint_handler(frame) };
        }
        INT_INVALID_OP => {
            EXCEPTIONS_INVOP.add(1);
            // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
            unsafe { x86_invop_handler(frame) };
        }
        INT_DEVICE_NA => {
            EXCEPTIONS_DEV_NA.add(1);
            // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
            unsafe { exception_die(frame, "device na fault\n") };
        }
        INT_DOUBLE_FAULT => {
            // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
            unsafe { x86_df_handler(frame) };
        }
        INT_FPU_FP_ERROR => {
            EXCEPTIONS_FPU.add(1);
            // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
            unsafe { x86_unhandled_exception(frame) };
        }
        INT_SIMD_FP_ERROR => {
            EXCEPTIONS_SIMD.add(1);
            // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
            unsafe { x86_unhandled_exception(frame) };
        }
        INT_GP_FAULT => {
            EXCEPTIONS_GPF.add(1);
            // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
            unsafe { x86_gpf_handler(frame) };
        }
        INT_PAGE_FAULT => {
            EXCEPTIONS_PAGE.add(1);
            inc_page_faults();
            // SAFETY: Reading `CR2` at CPL 0 has no side effects.
            let cr2 = unsafe { x86_get_cr2() };
            // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
            if unsafe { x86_pfe_handler(frame, cr2) }.is_err() {
                core::hint::cold_path();
                // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
                unsafe { x86_fatal_pfe_handler(frame, cr2) };
            }
        }
        // Ignore spurious APIC irqs.
        INT_APIC_SPURIOUS => {}
        INT_APIC_ERROR => {
            EXCEPTIONS_APIC_ERR.add(1);
            // SAFETY: Discharged by `# Safety` in hard-IRQ context with CPL == 0, interrupts
            // disabled, and `%gs.base` and the local APIC initialized.
            unsafe {
                lapic::apic_error_interrupt_handler();
                lapic::apic_issue_eoi();
            }
        }
        INT_APIC_TIMER => {
            EXCEPTIONS_APIC_TIMER.add(1);
            // SAFETY: Discharged by `# Safety` in hard-IRQ context with CPL == 0, interrupts
            // disabled, `%gs.base`, `PerCpu`, and the local APIC initialized, and `TimerLock` not
            // held.
            unsafe {
                lapic::apic_timer_interrupt_handler();
                lapic::apic_issue_eoi();
            }
        }
        INT_IPI_GENERIC => {
            // SAFETY: Discharged by `# Safety` in hard-IRQ context with CPL == 0, interrupts and
            // preemption disabled, `%gs.base`, `PerCpu`, and the local APIC initialized, and
            // `IpiTaskLock` not held.
            unsafe {
                mbx_generic_irq();
                lapic::apic_issue_eoi();
            }
        }
        INT_IPI_RESCHEDULE => {
            // SAFETY: Discharged by `# Safety` in hard-IRQ context with CPL == 0, interrupts and
            // preemption disabled, and `%gs.base`, `PerCpu`, and the local APIC initialized.
            unsafe {
                mbx_reschedule_irq();
                lapic::apic_issue_eoi();
            }
        }
        INT_IPI_INTERRUPT => {
            mbx_interrupt_irq();
            // SAFETY: Discharged by `# Safety` in hard-IRQ context with CPL == 0, interrupts
            // disabled, and `%gs.base` and the local APIC initialized.
            unsafe { lapic::apic_issue_eoi() };
        }
        INT_IPI_HALT => {
            // SAFETY: Interrupts are disabled and CPL is 0 in interrupt context.
            unsafe { mp::ipi_halt_handler() };
            // no return
        }
        // Pass all other non-Intel defined irq vectors to the platform.
        INT_PLATFORM_BASE..=INT_PLATFORM_MAX => {
            EXCEPTIONS_IRQ.add(1);
            // SAFETY: Discharged by `# Safety` in hard-IRQ context with CPL == 0, interrupts
            // disabled, `%gs.base`, `PerCpu`, and the local APIC initialized, `INTERRUPT_MANAGER`'s
            // spinlock not held, and `frame` valid and non-aliasing.
            unsafe { platform_irq(frame) };
        }
        // Integer division-by-zero
        INT_DIVIDE_0
        // Overflow for INTO instruction (should be x86-32-only)
        | INT_OVERFLOW
        // Bound range exceeded for BOUND instruction (should be x86-32-only)
        | INT_BOUND_RANGE
        // Loading segment with "not present" bit set
        | INT_SEGMENT_NOT_PRESENT
        // Stack segment fault (should be x86-32-only)
        | INT_STACK_FAULT
        // Misaligned memory access when AC=1 in flags
        | INT_ALIGNMENT_CHECK => {
            EXCEPTIONS_UNHANDLED.add(1);
            // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
            unsafe { x86_unhandled_exception(frame) };
        }
        _ => {
            // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
            unsafe { exception_die(frame, "unhandled exception type, halting\n") };
        }
    }
}

/// Top-level x86 exception handler for most exceptions and IRQs (`x86_exception_handler`).
///
/// # Safety
///
/// The caller must ensure that:
/// - CPL is 0, `RFLAGS.DF == 0` (`cld` on entry), `DR7.GD` is clear, interrupts are disabled, and
///   `%gs.base` points to the calling CPU's `x86_percpu` with `PerCpu` initialized and
///   `%gs:current_thread` pointing to a valid, constructed `Thread` whose `KernelStack` metadata is
///   not concurrently modified.
/// - `frame` is non-null, aligned, and points to a valid, live, writable [`Iframe`] on the current
///   stack constructed by the exception entry assembly, with no conflicting Rust references for
///   the duration of the call (and `[frame, min(frame + 256, stack_top))` mapped if on the current
///   kernel stack).
/// - When `(*frame).vector` is a local APIC, IPI, or platform IRQ vector, the local APIC
///   (`apic_vm_init` / `apic_local_init`) is initialized on the calling CPU, and `TimerLock`,
///   `IpiTaskLock`, and `INTERRUPT_MANAGER`'s spinlock are not held.
/// - Whenever the exception originated from user space, is a kernel-mode user-copy page fault with
///   `X86_PFR_RUN_FAULT_HANDLER_BIT` set, or occurs with preemption/rescheduling enabled on exit,
///   the current thread is in `THREAD_RUNNING` state (with an attached user `VmAspace` for user
///   page-fault addresses) in a context where blocking/rescheduling is permitted (with no
///   `PreemptDisabledToken` live across blocking user/user-copy faults) and no spinlocks, thread
///   lock, dispatcher locks, lockdep-tracked locks, conflicting VM locks, or active
///   `ChainLockTransaction` are held once interrupts are re-enabled or pending signals run.
#[inline(always)]
pub(super) unsafe fn exception_handler(frame: *mut Iframe) {
    // SAFETY: `frame` is non-null and points to a valid, initialized `Iframe`.
    let entry_vector = unsafe { (*frame).vector };
    // NMIs should be handled in a different handler.
    debug_assert!(entry_vector != u64::from(X86_INT_NMI.0));
    // Are we recursing?
    if blocking_disallowed() {
        core::hint::cold_path();
        // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
        unsafe { exception_die(frame, "recursion in interrupt handler\n") };
    }
    let mut state = IntHandlerSavedState::default();
    // SAFETY: Interrupts are disabled and `%gs:current_thread` is a valid `Thread` (caller's
    // contract), and `int_handler_finish` is called below with the same `state` on the current
    // thread. The paths in `handle_exception_types` that enable interrupts, re-enable preemption,
    // and may block (user exception dispatch and page faults) disable preemption, set
    // blocking-disallowed, and disable interrupts again before returning.
    unsafe { int_handler_start(&mut state) };
    // Did we come from user or kernel space?
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    let from_user = unsafe { is_from_user(frame) };
    // Deliver the interrupt.
    {
        let _trace = if entry_vector != u64::from(X86_INT_PAGE_FAULT.0) {
            crate::ktrace_rs::cpu_begin_scope!("kernel:irq", "irq", "irq #" => entry_vector)
        } else {
            None
        };
        // SAFETY: `int_handler_start` disabled preemption, interrupts are disabled, `entry_vector`
        // was read from `(*frame).vector`, and all remaining preconditions hold by `# Safety`.
        unsafe { handle_exception_types(frame, entry_vector) };
    }

    // SAFETY: Pairs with the `int_handler_start` above on the current thread, which may now be on
    // another CPU if the handler was preempted or blocked; any path that enabled interrupts
    // restored the preempt disable count, blocking-disallowed, and disabled interrupts (see
    // above), so interrupts are disabled, and no `PreemptDisabledToken` outlives the handler.
    let do_preempt = unsafe { int_handler_finish(&mut state) };
    if do_preempt {
        core::hint::cold_path();
        thread::preempt();
    }
    // If we came from user space, check to see if we have any signals to handle.
    if from_user {
        core::hint::cold_path();
        // SAFETY: Interrupts are disabled, `%gs.base` and `current_thread` (`THREAD_RUNNING`) are
        // valid on return to user space with blocking permitted and no spinlocks, thread lock,
        // dispatcher locks, or active `ChainLockTransaction` held, and `frame` is valid and
        // exclusively writable.
        unsafe { iframe_process_pending_signals(frame) };
    }
    debug_assert!(
        ints_disabled(),
        "ints disabled on way out of exception, vector {} IP {:#x}",
        entry_vector,
        // SAFETY: `frame` points to a valid, initialized `Iframe`.
        unsafe { (*frame).ip }
    );
}

/// Processes pending thread signals on the current thread at syscall exit
/// (`x86_syscall_process_pending_signals`).
///
/// # Safety
///
/// The caller must ensure that:
/// - `%gs.base` points to the current CPU's `x86_percpu` and `ConstructFirstThread` has completed,
///   with the current thread in the `THREAD_RUNNING` state.
/// - Interrupts are disabled on entry.
/// - `gregs` points to a valid, live, writable [`SyscallRegs`] frame on the current thread's
///   kernel stack with no conflicting Rust references for the duration of the call.
/// - Blocking is permitted, and no spinlocks, thread lock, dispatcher locks, lockdep-tracked
///   locks, or active `ChainLockTransaction` are held.
#[inline(always)]
pub(super) unsafe fn syscall_process_pending_signals(gregs: *mut SyscallRegs) {
    debug_assert!(!gregs.is_null());
    // SAFETY: Discharged by this function's `# Safety` contract.
    unsafe {
        thread::process_pending_signals(GeneralRegsSource::Syscall, gregs.cast::<c_void>());
    }
}

/// Processes pending thread signals on the current thread at interrupt/exception exit
/// (`arch_iframe_process_pending_signals`).
///
/// # Safety
///
/// The caller must ensure that:
/// - `%gs.base` points to the current CPU's `x86_percpu` and `ConstructFirstThread` has completed,
///   with the current thread in the `THREAD_RUNNING` state.
/// - Interrupts are disabled on entry.
/// - `iframe` points to a valid, live, writable [`Iframe`] on the current thread's kernel stack
///   with no conflicting Rust references for the duration of the call.
/// - Blocking is permitted, and no spinlocks, thread lock, dispatcher locks, lockdep-tracked
///   locks, or active `ChainLockTransaction` are held.
#[inline(always)]
pub(super) unsafe fn iframe_process_pending_signals(iframe: *mut Iframe) {
    debug_assert!(!iframe.is_null());
    // SAFETY: Discharged by this function's `# Safety` contract.
    unsafe {
        thread::process_pending_signals(GeneralRegsSource::Iframe, iframe.cast::<c_void>());
    }
}

/// Dumps architecture-specific state to the console. `context` typically comes from a call to
/// `dispatch_user_exception`. Implemented by arch code. The context is always describing an
/// exception on the current thread.
///
/// # Safety
///
/// The caller must ensure that:
/// - CPL is 0, `RFLAGS.DF == 0`, and `%gs.base` points to the current CPU's `x86_percpu`, with
///   `%gs:current_thread` either null or pointing to a valid, constructed `Thread` whose
///   `KernelStack` metadata is not concurrently modified.
/// - If `context.frame` is non-null, it points to a valid, aligned, initialized, live [`Iframe`]
///   with no conflicting mutable references:
///   - If `(*context.frame).cs == CODE_64_SELECTOR` and the frame lies within the current thread's
///     kernel stack, `[frame, min(frame + 256, stack_top))` is mapped with no conflicting active
///     `&mut` borrows.
///   - If `(*context.frame).cs != CODE_64_SELECTOR` (any non-kernel / user-space code segment),
///     the calling thread is in a context where `arch_copy_from_user` (blocking) is permitted, with
///     no spinlocks or lockdep-tracked locks held.
#[cold]
#[inline(never)]
pub unsafe fn arch_dump_exception_context(context: &ArchExceptionContext) {
    // Print the common fields first.
    dump_common_exception_context(context);

    // If we don't have a frame, there's nothing more we can print.
    if context.frame.is_null() {
        kprintln!("no frame to dump");
        return;
    }
    let frame_ptr = context.frame.cast::<Iframe>();

    if context.is_page_fault {
        // SAFETY: Caller guarantees `frame_ptr` points to a valid, initialized `Iframe` when
        // non-null; `x86_dump_pfe` only borrows the 176-byte `Iframe`, while `dump_fault_frame`
        // below receives the original `frame_ptr` preserving full stack provenance.
        x86_dump_pfe(unsafe { &*frame_ptr }, context.cr2);
    }
    // SAFETY: Discharged by this function's `# Safety` contract.
    unsafe { dump_fault_frame(frame_ptr) };
    // try to dump the user stack
    // SAFETY: Caller guarantees `frame_ptr` points to a valid, initialized `Iframe` with no
    // conflicting mutable references.
    let (cs, user_sp) = unsafe { ((*frame_ptr).cs, (*frame_ptr).user_sp) };
    if cs != u64::from(CODE_64_SELECTOR) && vm::is_user_accessible(user_sp as usize) {
        let mut buf = core::mem::MaybeUninit::<[u8; 256]>::uninit();
        // SAFETY: `buf` is a 256-byte stack buffer, `user_sp` is passed with exposed provenance,
        // and by `# Safety` blocking is permitted with no spinlocks or lockdep locks held.
        let res = unsafe {
            arch_copy_from_user(
                buf.as_mut_ptr().cast::<c_void>(),
                core::ptr::with_exposed_provenance::<c_void>(user_sp as usize),
                size_of::<[u8; 256]>(),
            )
        };
        if res.is_ok() {
            // SAFETY: `arch_copy_from_user` returned `Ok(())`, initializing all 256 bytes of `buf`.
            let buf = unsafe { buf.assume_init_ref() };
            kprintln!("bottom of user stack at 0x{:x}:", user_sp as usize);
            let mut writer = KernelConsoleWriter;
            let _ = pretty::hexdump_very_ex_rs(&mut writer, buf, user_sp);
        }
    }
}

/// Sets `report` using architecture-specific information from `arch_context`. Implemented by arch
/// code.
///
/// # Safety
///
/// The caller must ensure that if `arch_context.frame` is non-null, it points to a valid, aligned,
/// initialized, live [`Iframe`] with no conflicting mutable references for the duration of the
/// call.
pub unsafe fn arch_fill_in_exception_context(
    arch_context: &ArchExceptionContext,
    report: &mut zx_exception_report_t,
) {
    report.context.synth_code = arch_context.user_synth_code;
    report.context.synth_data = arch_context.user_synth_data;

    // TODO(https://fxbug.dev/42105394): `frame` will be null for synthetic exceptions that
    // don't provide general register values yet.
    let frame_ptr = arch_context.frame.cast::<Iframe>();
    let (vector, err_code) = if !frame_ptr.is_null() {
        // SAFETY: Caller guarantees a non-null `arch_context.frame` points to a valid, initialized
        // `Iframe` with no conflicting mutable references.
        unsafe { ((*frame_ptr).vector, (*frame_ptr).err_code) }
    } else {
        (0, 0)
    };
    report.context.arch.x86_64.vector = vector;
    report.context.arch.x86_64.err_code = err_code;
    report.context.arch.x86_64.cr2 = arch_context.cr2;
}

/// Dispatches an exception that was raised by a syscall using `thread::signal_policy_exception`
/// (`arch_dispatch_user_policy_exception`), storing `policy_exception_code` and
/// `policy_exception_data` into the resulting `zx_exception_report_t`.
///
/// # Safety
///
/// The caller must ensure that CPL == 0, `RFLAGS.DF == 0`, `%gs.base` points to the current CPU's
/// `x86_percpu`, and `ConstructFirstThread` has completed, with the current thread in the
/// `THREAD_RUNNING` state (whose `KernelStack` metadata is not concurrently modified) in a context
/// where interrupts are enabled, blocking is permitted, and no spinlocks, thread lock, dispatcher
/// locks, lockdep-tracked locks, or active `ChainLockTransaction` are held. Any pre-installed
/// `suspended_general_regs` on the current thread must point to a valid, writable, unaliased
/// general-register frame.
pub unsafe fn dispatch_user_policy_exception(
    policy_exception_code: u32,
    policy_exception_data: u32,
) -> Result<(), Status> {
    let context = ArchExceptionContext {
        user_synth_code: policy_exception_code,
        user_synth_data: policy_exception_data,
        ..Default::default()
    };
    // SAFETY: `context` is a valid `ArchExceptionContext` with a null `frame`, and by `# Safety`
    // CPL == 0, `RFLAGS.DF == 0`, `%gs.base` and `current_thread` (`THREAD_RUNNING`) are
    // initialized, interrupts are enabled, blocking is permitted, and no spinlocks,
    // thread/dispatcher/lockdep-tracked locks, or active `ChainLockTransaction` are held.
    unsafe { dispatch_user_exception(ZX_EXCP_POLICY_ERROR, &context) }
}

/// Records registers in `context` as available to `zx_thread_read_state` / `zx_thread_write_state`
/// prior to `thread` stopping in an exception (`arch_install_exception_context`).
///
/// Returns `true` if register state was installed on `thread` (in which case the caller *must*
/// call [`remove_exception_context`] before `thread` resumes execution), or `false` if no
/// registers were recorded (e.g., synthetic exceptions with a null `frame`).
///
/// # Safety
///
/// The caller must ensure that:
/// - `thread` points to a valid, live (not freed) [`Thread`] with exclusive access to its
///   `arch.general_regs_source` and `arch.suspended_general_regs` fields.
/// - If `context.frame` is non-null, it points to a valid, writable [`Iframe`] that outlives the
///   installed exception context until [`remove_exception_context`] is called on `thread`.
#[must_use]
pub unsafe fn install_exception_context(
    thread: *mut Thread,
    context: &ArchExceptionContext,
) -> bool {
    let frame = context.frame;
    if frame.is_null() {
        // TODO(https://fxbug.dev/42105394): Must be a synthetic exception as they don't (yet)
        // provide the registers.
        return false;
    }
    // SAFETY: Discharged by `# Safety`: `thread` is valid with exclusive access to
    // `general_regs_source` and `suspended_general_regs`, and `frame` is non-null and outlives the
    // installed context.
    unsafe {
        super::thread::set_suspended_general_regs(
            thread,
            GeneralRegsSource::Iframe,
            frame.cast::<c_void>(),
        );
    }
    true
}

/// Undoes a previous call to [`install_exception_context`] (`arch_remove_exception_context`).
///
/// # Safety
///
/// The caller must ensure that `thread` points to a valid, live (not freed) [`Thread`] with
/// exclusive access to its `arch.general_regs_source` and `arch.suspended_general_regs` fields.
pub unsafe fn remove_exception_context(thread: *mut Thread) {
    // SAFETY: Discharged by `# Safety`: `thread` is valid and live with exclusive access to
    // `general_regs_source` and `suspended_general_regs`.
    unsafe {
        super::thread::reset_suspended_general_regs(thread);
    }
}

/// Unit tests for x86 exception formatting and report population.
#[cfg(ktest)]
#[unittest::suite(name = "x86_exceptions")]
mod tests {
    use super::{ArchExceptionContext, CAltHex, Iframe, arch_fill_in_exception_context};
    use core::fmt::{self, Write};
    use unittest::{expect_eq, expect_true};
    use zerocopy::FromZeros;
    use zx_types::zx_exception_report_t;

    struct SliceWriter<'a> {
        buf: &'a mut [u8],
        pos: usize,
    }

    impl Write for SliceWriter<'_> {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            let bytes = s.as_bytes();
            let end = self.pos.checked_add(bytes.len()).ok_or(fmt::Error)?;
            let dst = self.buf.get_mut(self.pos..end).ok_or(fmt::Error)?;
            dst.copy_from_slice(bytes);
            self.pos = end;
            Ok(())
        }
    }

    /// Verifies `CAltHex` matches C `%#<width>lx` formatting for zero and non-zero values.
    #[test]
    fn test_c_alt_hex() {
        let mut raw = [0u8; 32];
        let mut w = SliceWriter { buf: &mut raw, pos: 0 };
        expect_true!(write!(w, "{:8x}|{:8x}", CAltHex(0), CAltHex(0x2a)).is_ok());
        let len = w.pos;
        expect_true!(&raw[..len] == b"       0|    0x2a");
    }

    /// Verifies `arch_fill_in_exception_context` for hardware and synthetic frames.
    #[test]
    fn test_fill_in_exception_context() {
        let mut report = zx_exception_report_t::new_zeroed();

        let mut frame = Iframe { vector: 14, err_code: 0x6, ..Default::default() };
        let hw_ctx = ArchExceptionContext {
            frame: (&raw mut frame).cast(),
            cr2: 0xdead_beef,
            user_synth_code: 3,
            user_synth_data: 4,
            is_page_fault: true,
        };
        // SAFETY: `hw_ctx`, `frame`, and `report` are valid, unaliased stack locals.
        unsafe { arch_fill_in_exception_context(&hw_ctx, &mut report) };
        expect_eq!(report.context.synth_code, 3);
        expect_eq!(report.context.synth_data, 4);
        // SAFETY: `arch_fill_in_exception_context` initialized the `x86_64` union variant above.
        unsafe {
            expect_eq!(report.context.arch.x86_64.vector, 14);
            expect_eq!(report.context.arch.x86_64.err_code, 0x6);
            expect_eq!(report.context.arch.x86_64.cr2, 0xdead_beef);
        }

        // Populate `report` again with a null `frame` to verify `vector` and `err_code` are reset
        // to 0 even when `report` previously held non-zero hardware fault fields.
        let synth_ctx = ArchExceptionContext {
            frame: core::ptr::null_mut(),
            cr2: 0x1234_5678,
            user_synth_code: 11,
            user_synth_data: 22,
            is_page_fault: false,
        };
        // SAFETY: `synth_ctx` and `report` are valid, unaliased stack locals with null `frame`.
        unsafe { arch_fill_in_exception_context(&synth_ctx, &mut report) };
        expect_eq!(report.context.synth_code, 11);
        expect_eq!(report.context.synth_data, 22);
        // SAFETY: `arch_fill_in_exception_context` initialized the `x86_64` union variant above.
        unsafe {
            expect_eq!(report.context.arch.x86_64.vector, 0);
            expect_eq!(report.context.arch.x86_64.err_code, 0);
            expect_eq!(report.context.arch.x86_64.cr2, 0x1234_5678);
        }
    }
}
