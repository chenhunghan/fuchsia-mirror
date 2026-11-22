// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::job_dispatcher::{JobDispatcher, JobDispatcherState};
use super::job_policy::JobPolicy;
use super::process_dispatcher::ProcessDispatcher;
use fbl::RefPtr;
use zx_status::Status;
use zx_types::{zx_koid_t, zx_status_t};

unsafe extern "C" {
    /// Allocates and constructs a `JobDispatcher`, returning a pointer exported with an acquired
    /// reference count, or null if memory is exhausted.
    ///
    /// # Safety
    ///
    /// `parent` must be null or carry an acquired reference count that is transferred to this
    /// function, and `policy` must point to a valid `JobPolicy`.
    pub(super) fn cpp_job_dispatcher_alloc(
        parent: *mut JobDispatcher,
        policy: *const JobPolicy,
    ) -> *mut JobDispatcher;
}

// Exported FFI functions for C++ callers:

/// Initializes `JobDispatcherState` in-place.
///
/// # Safety
///
/// `state` must be a non-null, aligned pointer with exclusive write access to uninitialized memory
/// for `JobDispatcherState` that remains pinned until dropped in-place by
/// `rust_job_dispatcher_state_destroy` prior to deallocation. `parent` must be null (root job) or
/// a valid `JobDispatcher` pointer whose acquired reference count is transferred to this function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_job_dispatcher_state_init(
    state: *mut JobDispatcherState,
    parent: *mut JobDispatcher,
    policy: &JobPolicy,
) {
    // SAFETY: The caller guarantees `parent` is null or carries a transferred reference count.
    let parent = unsafe { RefPtr::try_from_raw(parent) };
    let init = JobDispatcherState::init(parent, *policy);

    // SAFETY: `__pinned_init` is used directly because `InPlaceWrite::write_pin_init` requires
    // `'static` storage. The caller guarantees `state` is non-null, aligned, exclusively writable,
    // and remains pinned until `rust_job_dispatcher_state_destroy` drops it prior to deallocation.
    let Ok(()) = unsafe { pin_init::PinInit::__pinned_init(init, state) };
}

/// Unlinks `job` from its parent (finishing the parent's dead transition if `job` was its last
/// child). Called by `~JobDispatcher()` before `rust_job_dispatcher_state_destroy`, while the job
/// is still reachable, as a shared object, from its parent's child list.
///
/// # Safety
///
/// Must only be called once, from `JobDispatcher::~JobDispatcher()` (`ref_count == 0`) prior to
/// `rust_job_dispatcher_state_destroy`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_job_dispatcher_leave_job_tree(job: &JobDispatcher) {
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    unsafe { job.leave_job_tree() };
}

/// Returns the koid of the job's parent, or 0 for the root job.
#[unsafe(no_mangle)]
pub extern "C" fn rust_job_dispatcher_get_related_koid(job: &JobDispatcher) -> zx_koid_t {
    job.get_related_koid()
}

/// Copies the job name into `out_name`.
///
/// # Safety
///
/// `out_name` must point to at least `ZX_MAX_NAME_LEN` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_job_dispatcher_get_name(
    job: &JobDispatcher,
    out_name: *mut core::ffi::c_char,
) -> zx_status_t {
    // SAFETY: The caller guarantees `out_name` points to `ZX_MAX_NAME_LEN` writable bytes. The
    // C++ callers pass uninitialized stack buffers, so the bytes are zeroed before a
    // `&mut [u8; ZX_MAX_NAME_LEN]` is formed over them.
    let name_buf = unsafe {
        out_name.write_bytes(0, zx_types::ZX_MAX_NAME_LEN);
        &mut *out_name.cast::<[u8; zx_types::ZX_MAX_NAME_LEN]>()
    };
    Status::result_into_raw(job.get_name(name_buf))
}

/// Sets the job name.
///
/// # Safety
///
/// If `len > 0`, `name` must point to at least `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_job_dispatcher_set_name(
    job: &JobDispatcher,
    name: *const core::ffi::c_char,
    len: usize,
) -> zx_status_t {
    // SAFETY: The caller guarantees `name` is readable for `len` bytes whenever `len > 0`.
    let name = unsafe { zr::slice_from_raw_parts(name.cast(), len) };
    Status::result_into_raw(job.set_name(name))
}

/// Removes `process` from `job`'s child process list.
///
/// # Safety
///
/// `process`'s parent job must be `job` (so that if `process` is linked into a child process list
/// it is `job`'s `procs` list). `process` may already have `ref_count == 0` when called from
/// `~ProcessDispatcher()`, so no un-upgraded `RefPtr` (`RefPtr::from_ref`) may be formed from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_job_dispatcher_remove_child_process(
    job: &JobDispatcher,
    process: &ProcessDispatcher,
) {
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    unsafe { job.remove_child_process(process) };
}
