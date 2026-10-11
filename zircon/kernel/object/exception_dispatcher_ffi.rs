// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::exception_dispatcher::{
    ArchExceptionContext, ExceptionDispatcher, ExceptionDispatcherState,
};
use super::thread_dispatcher::ThreadDispatcher;
use fbl::RefPtr;
use zx_status::Status;
use zx_types::{zx_exception_report_t, zx_excp_type_t, zx_rights_t, zx_status_t};

unsafe extern "C" {
    /// Calls into C++ to allocate and initialize an `ExceptionDispatcher`.
    ///
    /// # Safety
    ///
    /// `thread` must be a valid owning raw pointer exported from `fbl::RefPtr<ThreadDispatcher>`,
    /// and `report` and `arch_context` must remain valid until `clear()` is called.
    pub(crate) fn cpp_exception_dispatcher_create(
        thread: *mut ThreadDispatcher,
        exception_type: zx_excp_type_t,
        report: *const zx_exception_report_t,
        arch_context: *const ArchExceptionContext,
    ) -> *mut ExceptionDispatcher;
}

crate::object::dispatcher::impl_dispatcher_state_init!(
    ExceptionDispatcher,
    ExceptionDispatcherState,
    thread: RefPtr<ThreadDispatcher>,
    exception_type: zx_excp_type_t,
    report: *const zx_exception_report_t,
    arch_context: *const ArchExceptionContext,
);

/// Creates an `ExceptionDispatcher` from C++ and returns an exported raw pointer (or null).
///
/// # Safety
///
/// `thread_raw` must be a valid raw pointer exported from `fbl::RefPtr<ThreadDispatcher>`.
/// `report` and `arch_context` must remain valid until `rust_exception_dispatcher_clear` is
/// called.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_exception_dispatcher_create(
    thread_raw: *mut ThreadDispatcher,
    exception_type: zx_excp_type_t,
    report: *const zx_exception_report_t,
    arch_context: *const ArchExceptionContext,
) -> *mut ExceptionDispatcher {
    debug_assert!(!thread_raw.is_null());
    // SAFETY: `thread_raw` was exported from `fbl::RefPtr<ThreadDispatcher>`.
    let thread = unsafe { RefPtr::from_raw(thread_raw) };
    // SAFETY: Caller upholds lifetime invariants for `report` and `arch_context`.
    match unsafe { ExceptionDispatcher::create(thread, exception_type, report, arch_context) } {
        Some(disp) => RefPtr::into_raw(disp).cast_mut(),
        None => core::ptr::null_mut(),
    }
}

/// Returns a pointer to the `RefPtr<ThreadDispatcher>` stored in `disp`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_exception_dispatcher_get_thread(
    disp: &ExceptionDispatcher,
) -> *const RefPtr<ThreadDispatcher> {
    disp.thread()
}

/// Returns the exception type of `disp`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_exception_dispatcher_get_exception_type(
    disp: &ExceptionDispatcher,
) -> zx_excp_type_t {
    disp.exception_type()
}

/// Invokes `on_zero_handles` on `disp`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_exception_dispatcher_on_zero_handles(disp: &ExceptionDispatcher) {
    disp.on_zero_handles();
}

/// Copies the exception report from `disp` into `report`.
///
/// # Safety
///
/// `report` must point to writable memory for a `zx_exception_report_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_exception_dispatcher_fill_report(
    disp: &ExceptionDispatcher,
    report: *mut zx_exception_report_t,
) -> bool {
    if let Some(r) = disp.fill_report() {
        // SAFETY: Caller guarantees `report` points to valid writable memory.
        unsafe { report.write(r) };
        true
    } else {
        false
    }
}

/// Sets the task rights on `disp`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_exception_dispatcher_set_task_rights(
    disp: &ExceptionDispatcher,
    thread_rights: zx_rights_t,
    process_rights: zx_rights_t,
) {
    disp.set_task_rights(thread_rights, process_rights);
}

/// Returns whether `disp` is configured for second-chance debugger handling.
#[unsafe(no_mangle)]
pub extern "C" fn rust_exception_dispatcher_is_second_chance(disp: &ExceptionDispatcher) -> bool {
    disp.is_second_chance()
}

/// Waits for the exception handle to close and returns the resulting status.
#[unsafe(no_mangle)]
pub extern "C" fn rust_exception_dispatcher_wait_for_handle_close(
    disp: &ExceptionDispatcher,
) -> zx_status_t {
    Status::result_into_raw(disp.wait_for_handle_close())
}

/// Resets the exception state after a failed send.
#[unsafe(no_mangle)]
pub extern "C" fn rust_exception_dispatcher_discard_handle_close(disp: &ExceptionDispatcher) {
    disp.discard_handle_close();
}

/// Clears the exception report and arch context pointers on `disp`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_exception_dispatcher_clear(disp: &ExceptionDispatcher) {
    disp.clear();
}
