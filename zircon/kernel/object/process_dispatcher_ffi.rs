// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::exceptionate::Exceptionate;
use super::handle::KernelHandle;
use super::handle_table::HandleTable;
use super::job_dispatcher::JobDispatcher;
use super::process_dispatcher::{ProcessDispatcher, ProcessDispatcherState, ShareableProcessState};
use super::thread_dispatcher::ThreadDispatcher;
use super::vm_address_region_dispatcher::VmAddressRegionDispatcher;
use crate::arch_rs::ArchExceptionContext;
use crate::vm::vm_aspace::{ShareOpt, VmAspace};
use core::mem::MaybeUninit;
use fbl::RefPtr;
use zx_status::Status;
use zx_types::{zx_koid_t, zx_rights_t, zx_status_t, zx_vaddr_t};

unsafe extern "C" {
    /// Allocates and constructs a `ProcessDispatcher`, initializing `out_handle` with a
    /// `KernelHandle` owning it.
    ///
    /// # Safety
    ///
    /// `shareable_state` and `job` must carry acquired reference counts that are transferred to
    /// the constructor. `name_ptr` must point to `name_len` readable bytes, and `out_handle`
    /// must point to valid uninitialized storage.
    pub(super) fn cpp_process_dispatcher_alloc(
        shareable_state: *mut ShareableProcessState,
        job: *mut JobDispatcher,
        name_ptr: *const core::ffi::c_char,
        name_len: usize,
        flags: u32,
        out_handle: *mut MaybeUninit<KernelHandle<ProcessDispatcher>>,
    ) -> zx_status_t;

    /// Allocates a new `ShareableProcessState`, returning an acquired reference or null on OOM.
    pub(super) fn cpp_shareable_process_state_create() -> *mut ShareableProcessState;

    /// Returns the `fbl::RefCounted` subobject of `state`.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid `ShareableProcessState`.
    pub(super) fn cpp_shareable_process_state_get_ref_counted(
        state: *mut ShareableProcessState,
    ) -> *mut ();

    /// Deletes `state` once its last reference is gone.
    ///
    /// # Safety
    ///
    /// `state` must point to a `ShareableProcessState` to which no reference remains.
    pub(super) fn cpp_shareable_process_state_free(state: *mut ShareableProcessState);

    /// Increments the share count of `state`; returns false if it was already destroyed.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid `ShareableProcessState`.
    pub(super) fn cpp_shareable_process_state_increment_share_count(
        state: *mut ShareableProcessState,
    ) -> bool;

    /// Decrements the share count of `state`, cleaning up the shared resources at zero.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid `ShareableProcessState`.
    pub(super) fn cpp_shareable_process_state_decrement_share_count(
        state: *mut ShareableProcessState,
    );

    /// Initializes the shared address space and futex context of `state`.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid, uninitialized `ShareableProcessState` (`share_count > 0`)
    /// to which the caller has exclusive access (no prior or concurrent calls to initialize or
    /// read `futex_context` / `aspace`), and `aspace_name` must point to a NUL-terminated C
    /// string.
    pub(super) fn cpp_shareable_process_state_initialize(
        state: *mut ShareableProcessState,
        aspace_base: zx_vaddr_t,
        aspace_size: usize,
        aspace_name: *const core::ffi::c_char,
        share_opt: ShareOpt,
    ) -> bool;

    /// Returns the number of processes sharing `state`.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid `ShareableProcessState`.
    pub(super) fn cpp_shareable_process_state_share_count(
        state: *const ShareableProcessState,
    ) -> u64;

    /// Returns a pointer to the handle table of `state`.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid `ShareableProcessState`.
    #[allow(improper_ctypes)]
    pub(super) fn cpp_shareable_process_state_handle_table(
        state: *const ShareableProcessState,
    ) -> *const HandleTable;

    /// Returns a pointer to the `FutexContext` storage of `state`.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid `ShareableProcessState` on which
    /// `cpp_shareable_process_state_initialize` has returned `true`.
    pub(super) fn cpp_shareable_process_state_futex_context(
        state: *const ShareableProcessState,
    ) -> *mut core::ffi::c_void;

    /// Returns a pointer to the address space of `state`.
    ///
    /// # Safety
    ///
    /// `state` must point to a valid `ShareableProcessState` on which
    /// `cpp_shareable_process_state_initialize` has returned `true`.
    pub(super) fn cpp_shareable_process_state_aspace(
        state: *const ShareableProcessState,
    ) -> *mut VmAspace;
}

// Exported FFI functions for C++ callers:

/// Initializes `ProcessDispatcherState` in-place.
///
/// # Safety
///
/// `state` must point to valid uninitialized memory for `ProcessDispatcherState`.
/// `shareable_state` and `job` must carry acquired reference counts, and `name_ptr` must point to
/// `name_len` readable bytes whenever `name_len > 0`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_process_dispatcher_state_init(
    state: *mut ProcessDispatcherState,
    disp: &ProcessDispatcher,
    shareable_state: *mut ShareableProcessState,
    job: *mut JobDispatcher,
    name_ptr: *const core::ffi::c_char,
    name_len: usize,
    flags: u32,
) {
    // SAFETY: The caller transferred reference counts on `shareable_state` and `job`, guarantees
    // `name_ptr` is readable for `name_len` bytes, and that `state` points to uninitialized
    // storage of the right size and alignment.
    unsafe {
        let shareable_state = RefPtr::from_raw(shareable_state);
        let job = RefPtr::from_raw(job);
        let name = zr::slice_from_raw_parts(name_ptr.cast(), name_len);
        let _ = pin_init::PinInit::__pinned_init(
            ProcessDispatcherState::init(disp, shareable_state, job, name, flags),
            state,
        );
    }
}

/// Creates a new `ProcessDispatcher` and its root VMAR dispatcher.
///
/// # Safety
///
/// `job` must carry an acquired reference count. `name_ptr` must point to `name_len` readable
/// bytes. All out pointers must point to valid uninitialized memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_process_dispatcher_create(
    job: *mut JobDispatcher,
    name_ptr: *const core::ffi::c_char,
    name_len: usize,
    flags: u32,
    out_handle: *mut MaybeUninit<KernelHandle<ProcessDispatcher>>,
    out_rights: *mut MaybeUninit<zx_rights_t>,
    out_vmar_handle: *mut MaybeUninit<KernelHandle<VmAddressRegionDispatcher>>,
    out_vmar_rights: *mut MaybeUninit<zx_rights_t>,
) -> zx_status_t {
    // SAFETY: The caller transferred a reference count on `job` and guarantees `name_ptr` is
    // readable for `name_len` bytes whenever `name_len > 0`.
    let (job, name) =
        unsafe { (RefPtr::from_raw(job), zr::slice_from_raw_parts(name_ptr.cast(), name_len)) };
    match ProcessDispatcher::create(job, name, flags) {
        Ok((handle, rights, vmar_handle, vmar_rights)) => {
            // SAFETY: The caller guarantees all out pointers are valid for writes.
            unsafe {
                (*out_handle).write(handle);
                (*out_rights).write(rights);
                (*out_vmar_handle).write(vmar_handle);
                (*out_vmar_rights).write(vmar_rights);
            }
            zx_types::ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}

/// Runs the shutdown sequence for a process whose last handle closed before it started.
#[unsafe(no_mangle)]
pub extern "C" fn rust_process_dispatcher_on_zero_handles(process: &ProcessDispatcher) {
    process.on_zero_handles();
}

/// Returns the koid of the process's job.
#[unsafe(no_mangle)]
pub extern "C" fn rust_process_dispatcher_get_related_koid(
    process: &ProcessDispatcher,
) -> zx_koid_t {
    process.get_related_koid()
}

/// Copies the process name into `out_name`.
///
/// # Safety
///
/// `out_name` must point to at least `ZX_MAX_NAME_LEN` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_process_dispatcher_get_name(
    process: &ProcessDispatcher,
    out_name: *mut core::ffi::c_char,
) -> zx_status_t {
    // SAFETY: The caller guarantees `out_name` points to `ZX_MAX_NAME_LEN` writable bytes.
    let name_buf = unsafe { &mut *out_name.cast::<[u8; zx_types::ZX_MAX_NAME_LEN]>() };
    Status::result_into_raw(process.get_name(name_buf))
}

/// Sets the process name.
///
/// # Safety
///
/// If `len > 0`, `name` must point to at least `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_process_dispatcher_set_name(
    process: &ProcessDispatcher,
    name: *const core::ffi::c_char,
    len: usize,
) -> zx_status_t {
    // SAFETY: The caller guarantees `name` is readable for `len` bytes whenever `len > 0`.
    let name = unsafe { zr::slice_from_raw_parts(name.cast(), len) };
    Status::result_into_raw(process.set_name(name))
}

/// Returns the process's job with an acquired reference count.
#[unsafe(no_mangle)]
pub extern "C" fn rust_process_dispatcher_job(process: &ProcessDispatcher) -> *mut JobDispatcher {
    RefPtr::into_raw(process.job().clone()).cast_mut()
}

/// Kills the process with `retcode`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_process_dispatcher_kill(process: &ProcessDispatcher, retcode: i64) {
    process.kill(retcode);
}

/// Returns the process's debug `Exceptionate`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_process_dispatcher_debug_exceptionate(
    process: &ProcessDispatcher,
) -> &Exceptionate {
    process.debug_exceptionate()
}

/// Returns a borrowed pointer to the process's `ShareableProcessState`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_process_dispatcher_shareable_state(
    process: &ProcessDispatcher,
) -> *mut ShareableProcessState {
    RefPtr::as_ptr(&process.state().shareable_state).cast_mut()
}

/// Looks up a process anywhere in the job tree by koid, returning an acquired reference or null.
#[unsafe(no_mangle)]
pub extern "C" fn rust_process_dispatcher_lookup_process_by_id(
    koid: zx_koid_t,
) -> *mut ProcessDispatcher {
    ProcessDispatcher::lookup_process_by_id(koid)
        .map_or(core::ptr::null_mut(), |proc| RefPtr::into_raw(proc).cast_mut())
}

/// Notifies attached job debuggers that `thread` is starting `process`.
///
/// # Safety
///
/// * `thread` must be the currently executing user thread (`thread.is_current()`) and belong to
///   `process`, in a blocking-permitted context with interrupts enabled (required by
///   [`ProcessDispatcher::on_process_start_for_job_debugger`]).
/// * `thread`'s dispatcher lock (`thread.state().lock`) must not be held, nor any thread lock,
///   spinlock, or active `ChainLockTransaction` (required by
///   [`ProcessDispatcher::on_process_start_for_job_debugger`]).
/// * `context` must describe `thread`, and if `context.frame` is non-null, it must point to a
///   valid, initialized, writable `Iframe` for the duration of the call without conflicting Rust
///   references (required by [`ProcessDispatcher::on_process_start_for_job_debugger`]).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_process_dispatcher_on_process_start_for_job_debugger(
    process: &ProcessDispatcher,
    thread: &ThreadDispatcher,
    context: &ArchExceptionContext,
) {
    // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
    unsafe { process.on_process_start_for_job_debugger(thread, context) };
}
