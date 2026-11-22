// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Restricted mode kernel control flow implementation.

use core::ptr::NonNull;
use debug::ltracef;
use kalloc::Box;
use zx_status::Status;
use zx_types::{
    zx_exception_report_t, zx_restricted_exception_t, zx_restricted_reason_t, zx_restricted_state_t,
};

use crate::arch_rs::{Iframe, SyscallRegs};
use crate::kernel::restricted_state::RestrictedState;
use crate::ktrace_rs;
use crate::object::ProcessDispatcher;
use crate::user_copy::UserOutPtr;
use crate::vm::vmm;

const LOCAL_TRACE: u32 = 0;

/// Leave restricted mode on the current thread and return to normal mode.
///
/// Disables restricted mode flags, saves register state from `restricted_state_source` using `save_fn`,
/// switches the address space back to normal mode, and vectors into normal user space via `enter_full`.
///
/// # Safety
///
/// * The calling thread must be an initialized user thread currently executing in restricted mode
///   (`in_restricted() == true`) with a bound [`RestrictedState`] and exclusive access to it.
/// * Interrupts must be disabled (`crate::arch_rs::ints_disabled()`).
pub unsafe fn restricted_leave<T>(
    restricted_state_source: &T,
    save_fn: fn(&mut zx_restricted_state_t, &T),
    reason: zx_restricted_reason_t,
) -> ! {
    ltracef!("regs {:p}\n", restricted_state_source as *const T);

    debug_assert!(crate::arch_rs::ints_disabled());

    // SAFETY: Caller guarantees `restricted_leave` is called on an initialized user thread.
    let rs_ptr = unsafe { crate::kernel::thread::current_restricted_state() };
    let mut rs_ptr =
        NonNull::new(rs_ptr).expect("restricted_leave called without bound RestrictedState");

    let state_ptr = {
        // SAFETY: Caller guarantees `rs_ptr` is the current thread's exclusively owned
        // `RestrictedState`. This mutable borrow ends before `save_fn` is invoked so no
        // `&mut RestrictedState` is held live across the callback.
        let rs = unsafe { rs_ptr.as_mut() };

        debug_assert!(rs.in_restricted());
        debug_assert!(crate::arch_rs::vm::is_user_accessible(rs.vector_ptr()));

        rs.set_in_restricted(false);
        crate::arch_rs::set_restricted_flag(false);

        rs.state_ptr()
    };

    // SAFETY: By `RestrictedState` invariant, `state_ptr` points to the current thread's valid,
    // exclusively accessed mapped `zx_restricted_state_t` buffer.
    save_fn(unsafe { &mut *state_ptr }, restricted_state_source);

    // SAFETY: Caller guarantees `rs_ptr` is the current thread's bound `RestrictedState` and
    // `save_fn` has completed.
    let rs = unsafe { rs_ptr.as_ref() };

    ltracef!(
        "returning to normal mode at vector {:#x}, context {:#x}\n",
        rs.vector_ptr(),
        rs.context()
    );

    let up = ProcessDispatcher::get_current();
    // SAFETY: The process normal aspace remains valid for the current thread.
    unsafe { vmm::set_active_aspace(Some(up.normal_aspace_ptr())) };

    ktrace_rs::duration_end!("kernel:restricted", "restricted mode", "reason" => reason);

    crate::arch_rs::enter_full(rs.arch_normal_state(), rs.vector_ptr(), rs.context(), reason);
}

/// Leave restricted mode from an interrupt or exception frame (`Iframe`).
///
/// # Safety
///
/// * The calling thread must be an initialized user thread currently executing in restricted mode
///   with a bound [`RestrictedState`] and exclusive access to it.
/// * Interrupts must be disabled (`crate::arch_rs::ints_disabled()`).
pub unsafe fn restricted_leave_iframe(iframe: &Iframe, reason: zx_restricted_reason_t) -> ! {
    // SAFETY: Upheld by the caller.
    unsafe { restricted_leave(iframe, crate::arch_rs::save_restricted_iframe_state, reason) }
}

/// Leave restricted mode from a syscall register frame (`SyscallRegs`).
///
/// # Safety
///
/// * The calling thread must be an initialized user thread currently executing in restricted mode
///   with a bound [`RestrictedState`] and exclusive access to it.
/// * Interrupts must be disabled (`crate::arch_rs::ints_disabled()`).
pub unsafe fn restricted_leave_syscall(regs: &SyscallRegs, reason: zx_restricted_reason_t) -> ! {
    // SAFETY: Upheld by the caller.
    unsafe { restricted_leave(regs, crate::arch_rs::save_restricted_syscall_state, reason) }
}

/// Enter restricted mode on the current thread using the specified vector table pointer and context.
///
/// # Errors
/// - `Status::INVALID_ARGS`: `vector_table_ptr` is not a valid user-accessible address.
/// - `Status::BAD_STATE`: No `RestrictedState` is bound to the current thread, or state is invalid.
/// - `Status::INTERRUPTED_RETRY`: Current thread has pending signals that must be processed.
///
/// # Safety
///
/// * The calling thread must be an initialized thread not currently in restricted mode, with
///   exclusive access to its bound [`RestrictedState`] (if any) and no live references to it.
/// * If a [`RestrictedState`] is bound, the calling thread must belong to a user process
///   ([`ProcessDispatcher::get_current`]).
pub unsafe fn restricted_enter(vector_table_ptr: usize, context: usize) -> Result<(), Status> {
    ltracef!("vector {:#x} context {:#x}\n", vector_table_ptr, context);

    if !crate::arch_rs::vm::is_user_accessible(vector_table_ptr) {
        return Err(Status::INVALID_ARGS);
    }

    // SAFETY: Caller guarantees `restricted_enter` is called on an initialized thread.
    let rs_ptr = unsafe { crate::kernel::thread::current_restricted_state() };
    if rs_ptr.is_null() {
        return Err(Status::BAD_STATE);
    }
    let state_buffer = {
        // SAFETY: `rs_ptr` is non-null and owned by the current thread with no live mutable
        // references. This shared borrow is confined to this block before `disable_ints()` so no
        // reference to `RestrictedState` is held across a preemption point (where context switch
        // inspects `in_restricted`).
        let rs = unsafe { &*rs_ptr };
        debug_assert!(!rs.in_restricted());
        rs.state_ptr()
    };
    if state_buffer.is_null() {
        return Err(Status::BAD_STATE);
    }

    // Copy the state out of the state buffer and into an automatic variable.
    // User mode may have a mapping of the state buffer so it's critical that we make a copy
    // and then validate the copy before using the copy to avoid a ToCToU vulnerability.
    // Use a compiler barrier to ensure the copy actually happens.
    // SAFETY: state_buffer points to a valid mapped zx_restricted_state_t.
    let state: zx_restricted_state_t = unsafe { *state_buffer };
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);

    if LOCAL_TRACE != 0 {
        crate::arch_rs::dump(&state);
    }

    crate::arch_rs::validate_state_pre_restricted_entry(&state)?;

    crate::arch_rs::disable_ints();
    if crate::kernel::thread::current_is_signaled() {
        crate::arch_rs::enable_ints();
        return Err(Status::INTERRUPTED_RETRY);
    }

    // SAFETY: Interrupts are now disabled so the thread cannot be preempted, and the caller
    // guarantees exclusive access to the current thread's `RestrictedState`.
    let rs = unsafe { &mut *rs_ptr };
    crate::arch_rs::save_state_pre_restricted_entry(rs.arch_normal_state_mut());

    rs.set_vector_ptr(vector_table_ptr);
    rs.set_context(context);

    // SAFETY: `ConstructFirstThread` has completed on the calling CPU and interrupts were disabled
    // above.
    if unsafe { crate::kernel::thread::current_check_for_restricted_kick() } {
        ktrace_rs::duration_end!("kernel:syscall", "restricted_enter");
        crate::arch_rs::enter_full(
            rs.arch_normal_state(),
            vector_table_ptr,
            context,
            zx_types::ZX_RESTRICTED_REASON_KICK,
        );
    }

    let up = ProcessDispatcher::get_current();
    // This check can be removed once the restricted mode tests can and do run with a restricted
    // aspace.
    if let Some(restricted_aspace) = up.restricted_aspace_ptr() {
        // SAFETY: The restricted aspace remains valid while active.
        unsafe { vmm::set_active_aspace(Some(restricted_aspace)) };
    }
    rs.set_in_restricted(true);
    crate::arch_rs::set_restricted_flag(true);

    ktrace_rs::duration_end!("kernel:syscall", "restricted_enter");
    ktrace_rs::duration_begin!("kernel:restricted", "restricted mode");

    crate::arch_rs::enter_restricted(&state);
}

/// Sets or clears the current thread's restricted mode state.
///
/// # Safety
///
/// The calling thread must be an initialized thread not currently in restricted mode, with no
/// live references to its current [`RestrictedState`] (if any).
pub unsafe fn thread_current_set_restricted_state(state: Option<Box<RestrictedState>>) {
    let raw_ptr = match state {
        Some(s) => Box::into_raw(s),
        None => core::ptr::null_mut(),
    };
    // SAFETY: Caller guarantees the current thread is initialized and no references to the
    // previous `RestrictedState` are live; `current_set_restricted_state` takes ownership of
    // `raw_ptr`.
    unsafe { crate::kernel::thread::current_set_restricted_state(raw_ptr) };
}

/// Redirect a synchronous exception raised while in restricted mode to normal mode handling.
///
/// Called as part of a synchronous exception that is to be handled by the normal mode in-thread
/// exception handler. Specifically this function will:
/// 1. Copy the exception report into either the restricted mode state VMO or to the `user_ptr`
///    provided during the `bind_state` call.
/// 2. Update the exception `iframe_t` on the stack such that the thread will return from this
///    exception to normal mode instead of restricted mode. The restricted `reason` code will be
///    set to `ZX_RESTRICTED_REASON_EXCEPTION` or, in the case that a user-copy was attempted and
///    failed, `ZX_RESTRICTED_REASON_EXCEPTION_LOST`.
///
/// # Safety
///
/// * The calling thread must be a user thread currently in restricted mode
///   (`rs_ptr.as_ref().in_restricted()`), and `rs_ptr` must point to the calling thread's bound
///   [`RestrictedState`] with exclusive access (no live references to it).
/// * The calling thread must have an installed exception context
///   ([`crate::kernel::thread::with_exception_context`]) with saved user state (and any non-null
///   `suspended_general_regs` frame valid, writable, and unaliased for the duration of the call).
/// * No thread lock, spinlock, or active `ChainLockTransaction` may be held on the calling CPU.
pub unsafe fn redirect_restricted_exception_to_normal_mode(
    mut rs_ptr: NonNull<RestrictedState>,
    report: &zx_exception_report_t,
) {
    let exception_report_ptr = {
        // SAFETY: Caller guarantees `rs_ptr` is the current thread's exclusively owned
        // `RestrictedState`. This mutable borrow ends before `copy_slice_to_user` below so that a
        // re-entrant page fault on the user exception report buffer can safely inspect
        // `in_restricted` via `RestrictedState::in_restricted_ptr` without aliasing a live
        // `&mut RestrictedState`.
        let rs = unsafe { rs_ptr.as_mut() };
        assert!(rs.in_restricted());
        let state = rs.state_mut();

        ktrace_rs::duration_end!("kernel:restricted", "restricted mode", "reason" => zx_types::ZX_RESTRICTED_REASON_EXCEPTION);

        // SAFETY: Caller guarantees the current user thread is stopped inside
        // `with_exception_context` with saved user state and no active chain lock transaction.
        unsafe { crate::arch_rs::save_restricted_exception_state(state) };

        rs.set_in_restricted(false);
        crate::arch_rs::set_restricted_flag(false);

        rs.exception_report_ptr()
    };

    let up = ProcessDispatcher::get_current();
    // SAFETY: The process normal aspace remains valid for the current thread.
    unsafe { vmm::set_active_aspace(Some(up.normal_aspace_ptr())) };

    let mut reason = zx_types::ZX_RESTRICTED_REASON_EXCEPTION;
    if let Some(report_ptr) = exception_report_ptr {
        let user_out = UserOutPtr::<u8>::new(report_ptr.as_ptr().cast::<u8>());
        // SAFETY: report points to a valid zx_exception_report_t instance of size_of::<zx_exception_report_t>() bytes.
        let report_bytes = unsafe {
            core::slice::from_raw_parts(
                (report as *const zx_exception_report_t).cast::<u8>(),
                core::mem::size_of::<zx_exception_report_t>(),
            )
        };
        if user_out.copy_slice_to_user(report_bytes).is_err() {
            core::hint::cold_path();
            reason = zx_types::ZX_RESTRICTED_REASON_EXCEPTION_LOST;
        }
    } else {
        // SAFETY: `rs_ptr` is the current thread's bound `RestrictedState` and no user copy is
        // performed in this branch. Casts mapped state buffer to `zx_restricted_exception_t` and
        // writes `report`.
        let restricted_exception = unsafe {
            rs_ptr
                .as_mut()
                .state_ptr_as::<zx_restricted_exception_t>()
                .as_mut()
                .expect("state_ptr must be non-null")
        };
        restricted_exception.exception = *report;
    }

    // SAFETY: `rs_ptr` is the current thread's bound `RestrictedState` and any user copy (and
    // potential page fault) has completed.
    let rs = unsafe { rs_ptr.as_ref() };
    // SAFETY: Caller guarantees the current user thread stays stopped in a synchronous exception
    // with saved user state, and no chain lock transaction is active on this CPU.
    unsafe {
        crate::arch_rs::redirect_restricted_exception_to_normal(
            rs.arch_normal_state(),
            rs.vector_ptr(),
            rs.context(),
            reason,
        );
    }
}

// FFI Export Wrappers

/// Entry point called directly from hardware exception / syscall assembly vectors.
///
/// # Safety
/// * `regs` must be a valid pointer to saved syscall registers.
/// * The calling thread must be an initialized user thread currently executing in restricted mode
///   with interrupts disabled and exclusive access to its bound [`RestrictedState`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn syscall_from_restricted(regs: *const SyscallRegs) -> ! {
    // SAFETY: Caller guarantees `regs` is non-null and valid, and the current user thread is in
    // restricted mode with interrupts disabled and exclusive access to its `RestrictedState`.
    unsafe {
        let regs_ref = &*regs;
        restricted_leave_syscall(regs_ref, zx_types::ZX_RESTRICTED_REASON_SYSCALL)
    }
}

/// Restricted mode kernel unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "restricted_tests")]
mod tests {
    use super::restricted_enter;
    use zx_status::Status;

    /// Verifies that restricted_enter returns INVALID_ARGS for non-user-accessible vector table addresses.
    #[test]
    fn test_restricted_enter_invalid_args() {
        let invalid_vector = 0xffff_8000_0000_0000usize;
        // SAFETY: Unit tests run on an initialized kernel thread not in restricted mode.
        let res = unsafe { restricted_enter(invalid_vector, 0) };
        unittest::expect_true!(res.unwrap_err() == Status::INVALID_ARGS);
    }

    /// Verifies that restricted_enter returns BAD_STATE when no RestrictedState is bound to the thread.
    #[test]
    fn test_restricted_enter_bad_state_no_restricted_state() {
        // Ensure no RestrictedState is bound to the current thread during this test.
        // SAFETY: Unit tests run on an initialized kernel thread not in restricted mode.
        let prev_rs_ptr = unsafe { crate::kernel::thread::current_restricted_state() };
        unsafe { crate::kernel::thread::current_set_restricted_state(core::ptr::null_mut()) };

        // SAFETY: Unit tests run on an initialized kernel thread with no `RestrictedState` bound.
        let res = unsafe { restricted_enter(0x1000, 0) };
        unittest::expect_true!(res.unwrap_err() == Status::BAD_STATE);

        // Restore previous RestrictedState pointer.
        unsafe { crate::kernel::thread::current_set_restricted_state(prev_rs_ptr) };
    }
}
