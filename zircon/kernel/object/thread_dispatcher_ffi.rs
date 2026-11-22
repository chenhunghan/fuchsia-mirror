// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_char;
use core::mem::MaybeUninit;

use fbl::RefPtr;
use zx_status::Status;
use zx_types::{ZX_MAX_NAME_LEN, zx_excp_type_t, zx_koid_t, zx_status_t};

use super::exceptionate::Exceptionate;
use super::handle::KernelHandle;
use super::process_dispatcher::ProcessDispatcher;
use super::thread_dispatcher::{
    Blocked, CoreThreadObservation, ThreadDispatcher, ThreadDispatcherState,
};
use crate::arch_rs::{ArchExceptionContext, UserEntryState};
use crate::kernel::thread::Thread;

unsafe extern "C" {
    /// Acquires `thread`'s critical mutex and returns whether the timeslice extension should be
    /// cleared on release.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`.
    pub(crate) fn cpp_thread_dispatcher_lock_acquire(thread: *mut ThreadDispatcher) -> bool;

    /// Releases `thread`'s critical mutex.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher` whose lock was acquired by the caller.
    pub(crate) fn cpp_thread_dispatcher_lock_release(
        thread: *mut ThreadDispatcher,
        should_clear: bool,
    );

    /// Releases a `CoreThreadObservation` via `CoreThreadObservation::Release()`.
    ///
    /// # Safety
    ///
    /// `obs` must point to a valid `CoreThreadObservation`.
    pub(crate) fn cpp_thread_dispatcher_release_core_thread_observation(
        obs: *mut CoreThreadObservation,
    );

    /// Allocates and constructs a ThreadDispatcher.
    ///
    /// # Safety
    ///
    /// `process` must point to a valid `ProcessDispatcher` whose reference count was transferred.
    /// `out_handle` must point to valid uninitialized memory for a
    /// `KernelHandle<ThreadDispatcher>`.
    pub(crate) fn cpp_thread_dispatcher_alloc(
        process: *mut ProcessDispatcher,
        out_handle: *mut MaybeUninit<KernelHandle<ThreadDispatcher>>,
    ) -> zx_status_t;

    /// Stores the lower level kernel thread on a ThreadDispatcher.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`, and `core_thread` must point to a
    /// valid kernel `Thread`.
    pub(crate) fn cpp_thread_dispatcher_set_core_thread(
        thread: *mut ThreadDispatcher,
        core_thread: *mut Thread,
    );

    /// Loads the lower level kernel thread from a ThreadDispatcher, which is null once
    /// `ThreadDispatcher::exiting_current()` has cleared it.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`, and the caller must hold that
    /// dispatcher's state lock: `core_thread_` is `TA_GUARDED(get_lock())` on the C++ side.
    pub(crate) fn cpp_thread_dispatcher_get_core_thread(
        thread: *const ThreadDispatcher,
    ) -> *mut Thread;

    /// Stores the register values userspace will start executing with.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher` and `entry` to a valid
    /// `UserEntryState`.
    pub(crate) fn cpp_thread_dispatcher_set_user_entry(
        thread: *mut ThreadDispatcher,
        entry: *const UserEntryState,
    );

    /// Enters userspace on a newly created thread. Does not return.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`.
    pub(crate) fn cpp_thread_start_routine_entry(thread: *mut core::ffi::c_void) -> i32;

    /// Takes the core thread spinlock and clears the linked core thread.
    ///
    /// TRANSITIONAL: while `core_thread_` remains a C++ field.
    ///
    /// # Safety
    ///
    /// `thread` must be a valid `ThreadDispatcher` pointer, and the caller must hold the
    /// dispatcher's state lock (`ThreadDispatcherState::lock`).
    pub(crate) fn cpp_thread_dispatcher_reset_core_thread(thread: *mut ThreadDispatcher);

    /// Sets whether `thread` is the initial thread of its process.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`.
    pub(super) fn cpp_thread_dispatcher_set_is_initial_thread(
        thread: *mut ThreadDispatcher,
        is_initial_thread: bool,
    );
}

// Exported FFI functions for C++ callers:

/// Initializes `ThreadDispatcherState` in-place.
///
/// # Safety
///
/// `state` must point to valid uninitialized memory for `ThreadDispatcherState`.
/// `process` must point to a valid `ProcessDispatcher` carrying an acquired refcount.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_dispatcher_state_init(
    state: *mut ThreadDispatcherState,
    disp: &ThreadDispatcher,
    process: *mut ProcessDispatcher,
) {
    // SAFETY: The caller transferred a reference count on `process` and guarantees `state`
    // points to uninitialized storage of the right size and alignment.
    unsafe {
        let process_ref = RefPtr::from_raw(process);
        let _ =
            pin_init::PinInit::__pinned_init(ThreadDispatcherState::init(disp, process_ref), state);
    }
}

/// Returns the related process koid for this thread.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_get_related_koid(thread: &ThreadDispatcher) -> zx_koid_t {
    thread.get_related_koid()
}

/// Returns the parent process of this thread.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_get_process(
    thread: &ThreadDispatcher,
) -> *mut ProcessDispatcher {
    thread.assert_canary();
    RefPtr::as_ptr(&thread.state().process).cast_mut()
}

/// Asserts that the thread dispatcher's canary is valid.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_assert_canary(thread: &ThreadDispatcher) {
    thread.assert_canary();
}

/// Kills this thread.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_kill(thread: &ThreadDispatcher) {
    thread.kill();
}

/// Suspends this thread.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_suspend(thread: &ThreadDispatcher) -> zx_status_t {
    Status::result_into_raw(thread.suspend())
}

/// Resumes this thread.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_resume(thread: &ThreadDispatcher) {
    thread.resume();
}

/// Sets the blocked reason of the current thread and returns the previous one.
///
/// # Safety
///
/// The calling thread must be a user thread, so that `Thread::Current::Get()->user_thread()` is
/// its live `ThreadDispatcher`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_dispatcher_set_blocked_reason(reason: u32) -> u32 {
    let reason = Blocked::from_raw(reason).expect("invalid ThreadDispatcher::Blocked");
    ThreadDispatcher::set_blocked_reason(reason).0
}

/// Returns a pointer to the thread's core thread spinlock, as a C++ `Lock<SpinLock>`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_get_core_thread_lock(
    thread: &ThreadDispatcher,
) -> *mut core::ffi::c_void {
    zr::ToMutPtr::to_mut_ptr(thread.state().core_thread_lock.raw_mutex()).cast()
}

/// Sends a single-shot exception over `exceptionate` and waits for the handler response.
///
/// # Safety
///
/// * `thread` must be the currently executing user thread (`thread.is_current()`).
/// * `thread`'s dispatcher lock must not be held (`TA_EXCL(get_lock())`), nor any thread lock,
///   spinlock, or active `ChainLockTransaction`.
/// * `arch_context` must describe the current thread, and if `arch_context.frame` is non-null, it
///   must point to a valid, initialized, writable `Iframe` for the duration of the call without
///   conflicting Rust references.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_dispatcher_handle_single_shot_exception(
    thread: &ThreadDispatcher,
    exceptionate: &Exceptionate,
    exception_type: zx_excp_type_t,
    arch_context: &ArchExceptionContext,
) -> bool {
    // SAFETY: Caller upholds all preconditions of `handle_single_shot_exception`.
    unsafe { thread.handle_single_shot_exception(exceptionate, exception_type, arch_context) }
}

/// Sets the thread's name from C++.
///
/// # Safety
///
/// `name` must point to `len` initialized bytes whenever `len > 0`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_dispatcher_set_name(
    thread: &ThreadDispatcher,
    name: *const c_char,
    len: usize,
) -> Result<(), Status> {
    // SAFETY: Caller guarantees `name` points to `len` initialized bytes whenever `len > 0`.
    let name_bytes = unsafe { zr::slice_from_raw_parts(name.cast(), len) };
    thread.set_name(name_bytes)
}

/// Gets the thread's name for C++.
#[unsafe(no_mangle)]
pub extern "C" fn rust_thread_dispatcher_get_name(
    thread: &ThreadDispatcher,
    out_name: &mut [u8; ZX_MAX_NAME_LEN],
) -> Result<(), Status> {
    thread.get_name(out_name)
}
