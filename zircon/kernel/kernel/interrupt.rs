// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

unsafe extern "C" {
    fn cpp_int_handler_start(state: *mut IntHandlerSavedState);
    fn cpp_int_handler_finish(state: *mut IntHandlerSavedState) -> bool;
}

// LINT.IfChange(IntHandlerSavedState)
/// State saved by [`int_handler_start`] for [`int_handler_finish`] to restore, matching C++
/// `int_handler_saved_state_t`.
#[repr(C)]
#[derive(Debug, Default)]
pub struct IntHandlerSavedState {
    blocking_disallowed: bool,
}
// LINT.ThenChange(//zircon/kernel/kernel/interrupt_ffi.cc:int_handler_saved_state_t)

zr::static_assert!(core::mem::size_of::<IntHandlerSavedState>() == 1);
zr::static_assert!(core::mem::align_of::<IntHandlerSavedState>() == 1);

/// Start the main part of handling an interrupt in which preemption and blocking are disabled.
/// This must be matched by a later call to [`int_handler_finish`].
#[inline(always)]
pub fn int_handler_start(state: &mut IntHandlerSavedState) {
    // SAFETY: `state` is a live, uniquely borrowed `int_handler_saved_state_t`.
    unsafe { cpp_int_handler_start(state) }
}

/// Leave the main part of handling an interrupt, following a call to [`int_handler_start`].
///
/// If this function returns true, it means that there was a local preempt pending at the time the
/// exception handler finished, and that the current thread does not have preemption disabled. In
/// this case, callers *must* arrange to have preemption take place (typically via
/// [`crate::kernel::thread::preempt`]) _before_ completely unwinding from the exception.
#[must_use]
#[inline(always)]
pub fn int_handler_finish(state: &mut IntHandlerSavedState) -> bool {
    // SAFETY: `state` is a live, uniquely borrowed `int_handler_saved_state_t`.
    unsafe { cpp_int_handler_finish(state) }
}
