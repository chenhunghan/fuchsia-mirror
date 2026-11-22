// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Cross-architecture user exception context types and helpers (`<arch/exception.h>`).

use zx_types::zx_exception_report_t;

/// Architectural exception context (`arch_exception_context_t`).
pub type ArchExceptionContext = crate::arch_rs::exceptions::ArchExceptionContext;

/// Dumps architecture-specific state to the console. `context` typically comes from a call to
/// `dispatch_user_exception`. Implemented by arch code. The context is always describing an
/// exception on the current thread.
///
/// # Safety
///
/// The caller must ensure that:
/// - The calling CPU is executing at kernel privilege level (`CPL == 0` with `RFLAGS.DF == 0` on
///   x86-64, `EL1` on ARM64, `S-mode` on RISC-V) with its per-CPU state initialized (`%gs.base`
///   and either null or a valid constructed `current_thread` whose `KernelStack` metadata is not
///   concurrently modified on x86-64; `ConstructFirstThread` completed on ARM64 and RISC-V).
/// - If `context.frame` is non-null, it points to a valid, aligned, initialized, live `Iframe`
///   with no conflicting mutable references (and on x86-64, if it is a kernel-mode frame within the
///   current thread's kernel stack, `[frame, min(frame + 256, stack_top))` is mapped with no
///   conflicting active `&mut` borrows).
/// - Blocking is permitted and no spinlocks or lockdep-tracked locks are held when user stack
///   dumping (`arch_copy_from_user`) runs (unconditionally required on ARM64).
#[inline]
pub unsafe fn arch_dump_exception_context(context: &ArchExceptionContext) {
    // SAFETY: Discharged by this function's `# Safety` contract.
    unsafe {
        crate::arch_rs::exceptions::arch_dump_exception_context(context);
    }
}

/// Sets `report` using architecture-specific information from `context`. Implemented by arch code.
///
/// # Safety
///
/// The caller must ensure that if `context.frame` is non-null, it points to a valid, aligned,
/// initialized, live `Iframe` with no conflicting mutable references for the duration of the call.
#[inline]
pub unsafe fn arch_fill_in_exception_context(
    context: &ArchExceptionContext,
    report: &mut zx_exception_report_t,
) {
    // SAFETY: Discharged by this function's `# Safety` contract.
    unsafe {
        crate::arch_rs::exceptions::arch_fill_in_exception_context(context, report);
    }
}

/// Records `context`'s registers as `thread`'s suspended general/debug registers while `thread` is
/// stopped in an exception (`arch_install_exception_context`).
///
/// Returns `true` if the exception context was installed and must later be removed with
/// [`arch_remove_exception_context`], or `false` if `context` has no frame (e.g., a synthetic
/// exception).
///
/// # Safety
///
/// - `thread` must point to the valid, currently executing user
///   [`Thread`](crate::kernel::thread::Thread) with its lock held (`thread.lock()`).
/// - `context` must be a valid, initialized `arch_exception_context_t` for `thread` whose `frame`
///   (if non-null) points to a valid, writable `iframe_t` that remains valid with no conflicting
///   references until [`arch_remove_exception_context`] is called.
#[inline]
#[must_use]
pub unsafe fn arch_install_exception_context(
    thread: *mut crate::kernel::thread::Thread,
    context: &ArchExceptionContext,
) -> bool {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller guarantees `thread` is valid with its lock held, `context` is a shared
        // reference to a valid, initialized `ArchExceptionContext`, and any non-null
        // `context.frame` remains valid with no conflicting references while installed.
        unsafe { super::arm64::arch_install_exception_context(thread.cast(), context) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller guarantees `thread` and `context` (and any non-null `context.frame`) are
        // valid and `thread`'s lock is held.
        unsafe {
            super::riscv64::exceptions::arch_install_exception_context(thread.cast(), context)
        }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller guarantees `thread` and `context` (and any non-null `context.frame`) are
        // valid and `thread`'s lock is held.
        unsafe { super::x86::exceptions::install_exception_context(thread, context) }
    }
}

/// Undoes a prior call to [`arch_install_exception_context`] that returned `true`
/// (`arch_remove_exception_context`).
///
/// # Safety
///
/// `thread` must point to a valid [`Thread`](crate::kernel::thread::Thread) with its lock held
/// (`thread.lock()`) whose exception context was previously installed via
/// [`arch_install_exception_context`].
#[inline]
pub unsafe fn arch_remove_exception_context(thread: *mut crate::kernel::thread::Thread) {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller guarantees `thread` is valid with its lock held.
        unsafe { super::arm64::arch_remove_exception_context(thread.cast()) }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller guarantees `thread` is valid with its lock held.
        unsafe { super::riscv64::exceptions::arch_remove_exception_context(thread.cast()) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller guarantees `thread` is valid with its lock held.
        unsafe { super::x86::exceptions::remove_exception_context(thread) }
    }
}

/// Dispatches a user-mode policy exception (`ZX_EXCP_POLICY_ERROR`) on the current thread
/// (`arch_dispatch_user_policy_exception`).
///
/// # Safety
///
/// Must be called in normal user thread context with interrupts enabled and no spinlocks, thread
/// locks, or active `ChainLockTransaction` held. Any pre-installed `suspended_general_regs` on the
/// current thread must point to a valid, writable, unaliased general-register frame (because the
/// exception context's `frame` pointer is null, debuggers read and write `suspended_general_regs`
/// while the thread is stopped).
pub unsafe fn dispatch_user_policy_exception(
    policy_exception_code: u32,
    policy_exception_data: u32,
) -> Result<(), zx_status::Status> {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: Caller guarantees normal user thread context with interrupts enabled and no
        // locks held.
        unsafe {
            super::arm64::arch_dispatch_user_policy_exception(
                policy_exception_code,
                policy_exception_data,
            )
        }
    }
    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: Caller guarantees normal user thread context with interrupts enabled and no
        // locks held.
        unsafe {
            super::riscv64::exceptions::arch_dispatch_user_policy_exception(
                policy_exception_code,
                policy_exception_data,
            )
        }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: Caller guarantees normal user thread context with interrupts enabled and no
        // locks held.
        unsafe {
            super::x86::exceptions::dispatch_user_policy_exception(
                policy_exception_code,
                policy_exception_data,
            )
        }
    }
}
