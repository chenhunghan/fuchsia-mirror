// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Inline implementations of the lockup detector critical section entry and exit paths.

use core::ffi::{CStr, c_char};
use core::ptr::{self, NonNull};
use core::sync::atomic::{Ordering, compiler_fence};

use super::LOCKUP_CRITICAL_SECTION_ENABLED;
use super::state::LockupDetectorState;
use crate::arch_rs::curr_cpu_num;
use crate::kernel::types::cpu_num_t;

/// Enter a critical section.
///
/// Returns true if this is the outermost critical section.
///
/// Don't forget to call [`call_if_outer_and_leave`] (regardless of this function's return value).
#[inline]
pub(super) fn enter(state: &LockupDetectorState) -> bool {
    let cs_state = &state.critical_section;

    // We must maintain the invariant that if a call to `enter` increments the depth, the matching
    // call to `call_if_outer_and_leave` decrements it.  The most reliable way to accomplish that
    // is to always increment and always decrement.
    //
    // `depth` is only ever accessed by this CPU, so relaxed loads and stores are all the
    // atomicity it needs; the compiler fences below order it against the rest of the state.
    let depth = cs_state.depth.load(Ordering::Relaxed) + 1;
    cs_state.depth.store(depth, Ordering::Relaxed);
    if depth != 1 {
        core::hint::cold_path();
        return false;
    }

    // This is the outermost critical section.  However, we may be racing with an interrupt handler
    // that may call into `enter`.  Use a compiler fence to ensure that the compiler cannot
    // reorder upcoming stores to precede the depth=1 store above.  If the compiler were to make
    // such a reordering the `enter` call made by the interrupt handler may incorrectly believe
    // its critical section is the outermost critical section because it has not seen our depth=1
    // store.
    //
    // Note, there is a small gap here where the CriticalSectionLockupChecker may fail to notice a
    // lockup (though the HeartbeatLockupChecker may still detect it).  Consider the following:
    //
    //   1. We have stored depth=1, but not yet stored begin_ticks.
    //   2. An interrupt fires and the handler calls `enter`.
    //   3. The handler sees that depth is 1 so it does nothing.
    //   4. The CPU enters an infinite loop.
    //   5. CriticalSectionLockupChecker sees that begin_ticks has not been set so it assumes the
    //      CPU is not in a section.
    //
    // If interrupts are enabled at the time of the infinite loop in step 4, the system may never
    // detect the problem.  If instead of an infinite loop we just spend a lot of time in the
    // interrupt handler, we may never know it because the begin_ticks would only get set by the
    // outer critical section *after* returning from the handler.
    //
    // One way to close the gap would be to use begin_ticks rather than depth to determine if we're
    // already in a critical section.  Instead of storing begin_ticks when depth=1, we would store
    // begin_ticks only if it's currently unset (i.e. use an atomic compare and exchange with an
    // expected value of 0).  However, this would increase the cost of critical section
    // instrumentation.  Because the gap is small and we have heartbeats we have chosen to live
    // with it rather than pay the price of atomic compare and exchange.
    compiler_fence(Ordering::SeqCst);

    true
}

/// Call `func` if in the outermost critical section then leave the current critical section.
#[inline]
pub(super) fn call_if_outer_and_leave<F: FnOnce(&LockupDetectorState)>(
    state: &LockupDetectorState,
    func: F,
) {
    let cs_state = &state.critical_section;
    // See `enter` for why relaxed loads and stores of `depth` suffice.
    let depth = cs_state.depth.load(Ordering::Relaxed);
    if depth == 1 {
        // This is the outermost critical section.  However, we may be racing with an interrupt
        // handler that may call into `enter`.  Use compiler fences before and after `func` so
        // `func`'s stores cannot be compiler-reordered ahead of the `depth` load above, and the
        // `depth - 1` store below cannot be compiler-reordered ahead of `func` clearing critical
        // section state.
        compiler_fence(Ordering::SeqCst);
        func(state);
        compiler_fence(Ordering::SeqCst);
    } else {
        core::hint::cold_path();
    }
    debug_assert!(depth > 0);
    cs_state.depth.store(depth - 1, Ordering::Relaxed);
}

/// Used to indicate the CPU is entering a critical section with name `name` where it might appear
/// to be locked up.
///
/// # Safety
///
/// - Preemption or interrupts must be disabled on the calling CPU and remain disabled until the
///   matching [`lockup_end`] call on the same CPU.
/// - Any interrupt handler on the same CPU that enters a critical section while preemption is
///   disabled must balance its own `begin`/`end` calls before returning.
#[inline]
pub unsafe fn lockup_begin(name: &'static CStr) {
    // SAFETY: `curr_cpu_num()` is the calling CPU's valid index below `SMP_MAX_CPUS`, `name` is a
    // NUL-terminated C string with static lifetime, and the caller upholds the remaining
    // preconditions.
    unsafe { lockup_begin_raw_on_cpu(curr_cpu_num(), NonNull::from(name).cast()) };
}

/// [`lockup_begin`] when the caller has already loaded `curr_cpu_num()` into `cpu` and only has a
/// thin pointer to `name`.
///
/// # Safety
///
/// - `cpu` must equal `curr_cpu_num()` (`< SMP_MAX_CPUS`).
/// - `name` must point to a NUL-terminated C string with static lifetime.
/// - Preemption or interrupts must be disabled on the calling CPU and remain disabled until the
///   matching [`lockup_end`] call on the same CPU.
/// - Any interrupt handler on the same CPU that enters a critical section while preemption is
///   disabled must balance its own `begin`/`end` calls before returning.
#[inline]
pub unsafe fn lockup_begin_raw_on_cpu(cpu: cpu_num_t, name: NonNull<c_char>) {
    if !LOCKUP_CRITICAL_SECTION_ENABLED {
        return;
    }

    // SAFETY: The caller guarantees `cpu == curr_cpu_num()`, which is `< SMP_MAX_CPUS`.
    let state = unsafe { LockupDetectorState::get_unchecked(cpu) };
    let outermost = enter(state);
    if outermost {
        let cs_state = &state.critical_section;
        // We're using Ordering::Relaxed instead of Ordering::Release to
        // minimize performance impact.  As a result, HeartbeatLockupChecker and
        // CriticalSectionLockupChecker may see stale name values because there is
        // nothing for them to synchronize-with.
        cs_state.name.store(name.as_ptr(), Ordering::Relaxed);
    }
}

/// Used to indicate the CPU has left a critical section.
///
/// # Safety
///
/// Preemption or interrupts must be disabled on the calling CPU, and the call must balance an
/// earlier call to [`lockup_begin`] on the same CPU.
#[inline]
pub unsafe fn lockup_end() {
    if !LOCKUP_CRITICAL_SECTION_ENABLED {
        return;
    }

    // SAFETY: `curr_cpu_num()` is always `< SMP_MAX_CPUS`.
    let state = unsafe { LockupDetectorState::get_unchecked(curr_cpu_num()) };
    call_if_outer_and_leave(state, |state| {
        let cs_state = &state.critical_section;
        // See comment in `lockup_begin_raw_on_cpu` at the point where `name` is stored.
        cs_state.name.store(ptr::null_mut(), Ordering::Relaxed);
    });
}
