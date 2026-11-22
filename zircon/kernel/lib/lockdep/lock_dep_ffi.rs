// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::{
    AcquiredLockEntry, LockResult, MAX_LOCK_DEPENDENCIES, ThreadLockState, ValidatorLockClassState,
};
use crate::platform_rs::timer::InstantMono;
use core::ffi::{c_char, c_void};
use core::mem::MaybeUninit;
use core::sync::atomic::AtomicPtr;
use lockdep::LockFlags;
use zx_status::Status;
use zx_types::{zx_instant_mono_t, zx_status_t};

unsafe extern "C" {
    /// Returns the first `ValidatorLockClassState` in the global lock class list, or null.
    ///
    /// # Safety
    ///
    /// Always safe to call; returns the head of the static lock class list (or null).
    pub(super) fn cpp_lockdep_iter_begin() -> *mut ValidatorLockClassState;

    /// Returns the next `ValidatorLockClassState` after `state` in the global lock class list, or
    /// null.
    ///
    /// # Safety
    ///
    /// `state` must be a non-null pointer previously returned by `cpp_lockdep_iter_begin` or
    /// `cpp_lockdep_iter_next`.
    pub(super) fn cpp_lockdep_iter_next(
        state: *mut ValidatorLockClassState,
    ) -> *mut ValidatorLockClassState;

    /// Returns the static null-terminated name string for `state`.
    ///
    /// # Safety
    ///
    /// `state` must reference a valid `ValidatorLockClassState` (or `MetadataLockClassState`) with
    /// lock metadata enabled.
    pub(super) fn cpp_lockdep_state_name(state: &ValidatorLockClassState) -> *const c_char;

    /// Returns a reference to `state`'s `LockDependencySet` array.
    ///
    /// # Safety
    ///
    /// `state` must reference a valid `ValidatorLockClassState`.
    pub(super) fn cpp_lockdep_state_dependency_set(
        state: &ValidatorLockClassState,
    ) -> &[AtomicPtr<ValidatorLockClassState>; MAX_LOCK_DEPENDENCIES];

    /// Returns the root of the connected set containing `state`.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid `ValidatorLockClassState`.
    pub(super) fn cpp_lockdep_state_connected_set(
        state: *mut ValidatorLockClassState,
    ) -> *mut ValidatorLockClassState;

    /// Initializes an uninitialized `ThreadLockState` in place.
    ///
    /// # Safety
    ///
    /// `out` must be a non-null, aligned pointer to writable storage for `ThreadLockState`.
    pub(super) fn cpp_lockdep_thread_lock_state_init(out: *mut MaybeUninit<ThreadLockState>);

    /// Destroys an initialized `ThreadLockState` in place (`~ThreadLockState()`).
    ///
    /// # Safety
    ///
    /// `state` must be a non-null, aligned pointer to an initialized `ThreadLockState` that is not
    /// used afterwards and has no concurrent readers or writers.
    pub(super) fn cpp_lockdep_thread_lock_state_destroy(state: *mut ThreadLockState);

    /// Returns the lock class ID stored in `entry`.
    ///
    /// # Safety
    ///
    /// `entry` must have a non-null lock class ID pointing to a valid `ValidatorLockClassState`.
    pub(super) fn cpp_lockdep_entry_id(entry: &AcquiredLockEntry) -> &ValidatorLockClassState;

    /// Returns the acquisition order stored in `entry`.
    ///
    /// # Safety
    ///
    /// `entry` must reference a valid `AcquiredLockEntry`.
    pub(super) fn cpp_lockdep_entry_order(entry: &AcquiredLockEntry) -> usize;

    /// Returns the static null-terminated string description for `result`.
    ///
    /// # Safety
    ///
    /// Always safe to call; `lockdep::ToString` handles all `uint8_t` values.
    pub(super) fn cpp_lockdep_result_to_string(result: LockResult) -> *const c_char;

    /// Runs a single Tarjan SCC loop detection pass over the lock dependency graph.
    ///
    /// # Safety
    ///
    /// Must only be called non-reentrantly from the dedicated loop detection worker thread.
    pub(super) fn cpp_lockdep_loop_detection_pass();
}

/// FFI entry point for `TriggerAndWaitForLoopDetection`.
///
/// # Safety
///
/// Must only be called in blockable thread context after `LK_INIT_LEVEL_EARLIEST`
/// (`lock_dep_event_init`) and threading initialization have completed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_lockdep_trigger_and_wait_for_loop_detection(
    deadline: zx_instant_mono_t,
) -> zx_status_t {
    // SAFETY: Upheld by the caller per this function's `# Safety` contract.
    Status::result_into_raw(unsafe {
        super::trigger_and_wait_for_loop_detection(InstantMono(deadline))
    })
}

/// FFI entry point for `lockdep::SystemLockValidationError`.
///
/// # Safety
///
/// - `lock_entry` and `conflicting_entry` must be valid references to initialized
///   `AcquiredLockEntry` instances whose lock class IDs point to valid `ValidatorLockClassState`
///   instances.
/// - `state` must be a valid reference to an initialized `ThreadLockState`.
/// - The calling CPU must have a valid, initialized current `Thread`.
/// - `caller_frame` must either lie outside the calling thread's stack bounds (such as null) or be
///   a valid, aligned, initialized frame pointer on the calling thread's stack.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_lockdep_system_lock_validation_error(
    lock_entry: &AcquiredLockEntry,
    conflicting_entry: &AcquiredLockEntry,
    state: &ThreadLockState,
    caller_address: *const c_void,
    caller_frame: *const c_void,
    result: LockResult,
) {
    // SAFETY: Upheld by the caller per this function's `# Safety` contract.
    unsafe {
        super::system_lock_validation_error(
            lock_entry,
            conflicting_entry,
            state,
            caller_address,
            caller_frame,
            result,
        );
    }
}

/// FFI entry point for `lockdep::SystemLockValidationFatal`.
///
/// # Safety
///
/// - `lock_entry` must be a valid reference to an initialized `AcquiredLockEntry` whose lock class
///   ID points to a valid `ValidatorLockClassState` instance.
/// - `state` must be a valid reference to an initialized `ThreadLockState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_lockdep_system_lock_validation_fatal(
    lock_entry: &AcquiredLockEntry,
    state: &ThreadLockState,
    caller_address: *const c_void,
    caller_frame: *const c_void,
    result: LockResult,
) -> ! {
    // SAFETY: Upheld by the caller per this function's `# Safety` contract.
    unsafe {
        super::system_lock_validation_fatal(lock_entry, state, caller_address, caller_frame, result)
    }
}

/// FFI entry point for `lockdep::SystemCircularLockDependencyDetected`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_lockdep_system_circular_lock_dependency_detected(
    connected_set_root: &ValidatorLockClassState,
) {
    super::system_circular_lock_dependency_detected(connected_set_root);
}

/// FFI entry point for `lockdep::SystemGetThreadLockState`.
///
/// # Safety
///
/// - When `(lock_flags & LOCK_FLAGS_IRQ_SAFE) != 0`, the current CPU's `PerCpu` pointer must be
///   initialized.
/// - Otherwise, the calling CPU must have a valid, initialized current `Thread`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_lockdep_system_get_thread_lock_state(
    lock_flags: LockFlags,
) -> *mut ThreadLockState {
    // SAFETY: Upheld by the caller per this function's `# Safety` contract.
    unsafe { super::system_get_thread_lock_state(lock_flags) }
}

/// FFI entry point for `lockdep::SystemInitThreadLockState`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_lockdep_system_init_thread_lock_state(state: &ThreadLockState) {
    super::system_init_thread_lock_state(state);
}

/// FFI entry point for `lockdep::SystemTriggerLoopDetection`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_lockdep_system_trigger_loop_detection() {
    super::system_trigger_loop_detection();
}
