// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::{Iframe, SyscallRegs, exceptions};

unsafe extern "C" {
    /// Invokes the host external interrupt handler for `vector`
    /// (`x86_call_external_interrupt_handler` in `exceptions.S`).
    ///
    /// Constructs a synthetic kernel-mode [`Iframe`] on the current stack with `vector` and
    /// `err_code = 0`, saves general-purpose registers, and dispatches through `interrupt_common`
    /// into [`x86_exception_handler`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `vector` is a valid external interrupt vector (`X86_INT_PLATFORM_BASE.0 as u64 <= vector`
    ///   and `vector <= 255`; in particular never `X86_INT_NMI`, which assumes `%rsp` is on the
    ///   NMI IST stack).
    /// - CPL is 0, `RFLAGS.DF == 0`, `DR7.GD` is clear, hardware interrupts are disabled, and
    ///   `!blocking_disallowed()`.
    /// - `%gs.base` points to the calling CPU's `x86_percpu` with `PerCpu` initialized and
    ///   `%gs:current_thread` pointing to a valid, constructed `Thread` whose `KernelStack`
    ///   metadata is not concurrently modified.
    /// - The local APIC (`apic_vm_init` / `apic_local_init`) is initialized on the calling CPU,
    ///   and `TimerLock`, `IpiTaskLock`, and `INTERRUPT_MANAGER`'s spinlock are not held.
    /// - Whenever preemption/rescheduling is enabled on exit, the current thread is in
    ///   `THREAD_RUNNING` state in a context where rescheduling is permitted and no spinlocks,
    ///   thread lock, dispatcher locks, lockdep-tracked locks, or active `ChainLockTransaction`
    ///   are held.
    pub fn x86_call_external_interrupt_handler(vector: u64);
}

/// Wrapper for assembly caller (`x86_syscall_process_pending_signals`).
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
// SAFETY: The symbol name is unique and matches the assembly call in `syscall.S`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn x86_syscall_process_pending_signals(gregs: *mut SyscallRegs) {
    // SAFETY: Discharged by this function's `# Safety` contract, which matches
    // `exceptions::syscall_process_pending_signals` verbatim.
    unsafe { exceptions::syscall_process_pending_signals(gregs) }
}

/// Wrapper for assembly caller (`x86_exception_handler`).
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
// SAFETY: The symbol name is unique and matches the assembly call in `exceptions.S`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn x86_exception_handler(frame: *mut Iframe) {
    // SAFETY: Discharged by this function's `# Safety` contract, which matches
    // `exceptions::exception_handler` verbatim.
    unsafe { exceptions::exception_handler(frame) }
}

/// Wrapper for assembly caller (`x86_nmi_handler`).
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
// SAFETY: The symbol name is unique and matches the assembly call in `exceptions.S`.
#[sanitize(address = "off")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn x86_nmi_handler(frame: &Iframe) {
    // SAFETY: Discharged by this function's `# Safety` contract, which matches
    // `exceptions::nmi_handler` verbatim.
    unsafe { exceptions::nmi_handler(frame) }
}

/// Wrapper for C++ caller (`arch_iframe_process_pending_signals`).
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
// SAFETY: The symbol name is unique and matches the C FFI declaration in `<kernel/thread.h>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_iframe_process_pending_signals(iframe: *mut Iframe) {
    // SAFETY: Discharged by this function's `# Safety` contract, which matches
    // `exceptions::iframe_process_pending_signals` verbatim.
    unsafe { exceptions::iframe_process_pending_signals(iframe) }
}
