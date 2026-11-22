// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::thread;
use crate::arch_rs;

/// State saved by [`int_handler_start`] for [`int_handler_finish`] to restore.
#[derive(Debug, Default)]
pub struct IntHandlerSavedState {
    blocking_disallowed: bool,
}

/// Start the main part of handling an interrupt in which preemption and blocking are disabled.
/// This must be matched by a later call to [`int_handler_finish`].
///
/// # Safety
///
/// - The calling CPU must have a valid current [`Thread`](crate::kernel::thread::Thread), and
///   interrupts must be disabled.
/// - The caller must call [`int_handler_finish`] with the same `state` exactly once on the
///   current thread before the interrupt handler returns.
/// - Code between the two calls may enable interrupts, re-enable preemption, and be preempted or
///   block (and so resume on another CPU) only if it restores the preempt disable count added
///   here, sets blocking-disallowed, and disables interrupts again before [`int_handler_finish`].
#[inline(always)]
pub unsafe fn int_handler_start(state: &mut IntHandlerSavedState) {
    // Save the current blocking_disallowed value so that we can restore it during
    // int_handler_finish.
    state.blocking_disallowed = arch_rs::blocking_disallowed();
    arch_rs::set_blocking_disallowed(true);

    // Look up the current thread's preemption state once for both updates.
    // SAFETY: The caller guarantees a valid current `Thread`, which outlives this call, and
    // `PreemptionState` consists solely of relaxed atomics, so a shared reference is sound while
    // interrupt handlers and other code update it.
    let preemption_state = unsafe { &*thread::current_preemption_state() };

    // Disable all reschedules at least until the interrupt is finishing up.
    // Although eager resched disable implies preempt disable, the nesting here is
    // not redundant and is necessary to defer the local preemption to the safe
    // point in the calling frame.
    preemption_state.preempt_disable();
    preemption_state.eager_resched_disable();
}

/// Leave the main part of handling an interrupt, following a call to [`int_handler_start`].
///
/// If this function returns true, it means that there was a local preempt pending at the time the
/// exception handler finished, and that the current thread does not have preemption disabled. In
/// this case, callers *must* arrange to have preemption take place (typically via
/// [`crate::kernel::thread::preempt`]) _before_ completely unwinding from the exception.
///
/// # Safety
///
/// - The call must pair 1:1 with a prior [`int_handler_start`] by the current thread in the same
///   interrupt handler, with the same `state`. It may run on a different CPU than
///   [`int_handler_start`] if code between the two was preempted or blocked, as long as that code
///   restored the state listed in [`int_handler_start`]'s `# Safety`.
/// - Interrupts must still be disabled.
/// - No [`PreemptDisabledToken`](crate::kernel::thread::PreemptDisabledToken) created during
///   the handler may outlive this call.
#[must_use]
#[inline(always)]
pub unsafe fn int_handler_finish(state: &mut IntHandlerSavedState) -> bool {
    // Look up the current thread's preemption state once for both updates.
    // SAFETY: As in `int_handler_start`: the current `Thread` is valid (it called
    // `int_handler_start`) and `PreemptionState` consists solely of relaxed atomics.
    let preemption_state = unsafe { &*thread::current_preemption_state() };

    // Flush any pending remote preemptions if eager reschedules are enabled.
    // SAFETY: `preemption_state` is the current thread's, and the caller guarantees that this
    // pairs with the `eager_resched_disable` in `int_handler_start` on the current thread and that
    // no `PreemptDisabledToken` from the handler outlives it. The preempt disable count from
    // `int_handler_start` is still held, so this only flushes remote preemptions and cannot
    // reschedule the current CPU.
    unsafe { preemption_state.eager_resched_reenable() };

    // Drop the preempt disable count that we added at the start of the interrupt
    // handler, but do not trigger any local preemption if the disabled count has
    // hit zero, and there is a local preempt pending.
    //
    // Instead, if a local preemption became pending during the interrupt handler and
    // preemption is now enabled, indicate that the caller should perform the
    // preemption.
    // SAFETY: `preemption_state` is the current thread's, and this pairs with the
    // `preempt_disable` in `int_handler_start`, as above. Interrupts are disabled (caller's
    // contract) and blocking is disallowed (set by `int_handler_start`).
    let do_preempt = unsafe { preemption_state.preempt_reenable_delay_flush() };

    // We can't blindly set blocking_disallowed to false because it's possible
    // this interrupt handler interrupted a context where blocking_disallowed was
    // true.  Instead, simply restore the value we saved during int_handler_start.
    arch_rs::set_blocking_disallowed(state.blocking_disallowed);

    do_preempt
}
