// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;

use super::process_dispatcher::ProcessDispatcher;
use super::thread_dispatcher::ThreadDispatcher;

unsafe extern "C" {
    /// Initializes a C++ `RootJobSignalObserver` in inline `storage` attached to `rust_ctx`.
    ///
    /// # Safety
    ///
    /// `storage` must point to valid memory of at least `kRootJobSignalObserverSize` bytes
    /// aligned to `kRootJobSignalObserverAlign`. `rust_ctx` must point to a live `RootJobObserver`
    /// or be null.
    pub(crate) fn cpp_root_job_signal_observer_init(storage: *mut c_void, rust_ctx: *mut c_void);

    /// Destroys a C++ `RootJobSignalObserver`.
    ///
    /// # Safety
    ///
    /// `observer` must point to an initialized `RootJobSignalObserver`.
    pub(crate) fn cpp_root_job_signal_observer_destroy(observer: *mut c_void);

    /// Checks if a process is in DEAD state.
    ///
    /// # Safety
    ///
    /// `process` must point to an initialized `ProcessDispatcher`.
    pub(crate) fn cpp_test_process_is_dead(process: *const ProcessDispatcher) -> bool;

    /// Checks if a thread is dying or dead.
    ///
    /// # Safety
    ///
    /// `thread` must point to an initialized `ThreadDispatcher`.
    pub(crate) fn cpp_test_thread_is_dying_or_dead(thread: *const ThreadDispatcher) -> bool;

    /// Checks if a process is in RUNNING state.
    ///
    /// # Safety
    ///
    /// `process` must point to an initialized `ProcessDispatcher`.
    pub(crate) fn cpp_test_process_is_running(process: *const ProcessDispatcher) -> bool;
}
