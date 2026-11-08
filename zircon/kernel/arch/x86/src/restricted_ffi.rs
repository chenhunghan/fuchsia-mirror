// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use zx_types::{zx_status_t, zx_thread_state_general_regs_t};

use crate::kernel::thread::Thread;

unsafe extern "C" {
    /// Reads the saved user-mode general registers for `thread` into `out`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `thread` points to a valid `Thread` whose user register state
    /// is saved, and `out` is valid for writes.
    pub(super) fn arch_get_general_regs(
        thread: *mut Thread,
        out: *mut zx_thread_state_general_regs_t,
    ) -> zx_status_t;

    /// Writes `in_` into the saved user-mode general registers for `thread`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `thread` points to a valid `Thread` whose user register state
    /// is saved, and `in_` points to a valid `zx_thread_state_general_regs_t`.
    pub(super) fn arch_set_general_regs(
        thread: *mut Thread,
        in_: *const zx_thread_state_general_regs_t,
    ) -> zx_status_t;
}
