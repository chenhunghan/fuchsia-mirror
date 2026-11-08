// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Timers that charge elapsed time to a thread's task runtime statistics.

use core::marker::PhantomData;

use crate::kernel::thread::{Thread, ThreadPtr};
use crate::platform_rs::timer::{InstantMonoTicks, timer_current_mono_ticks};

/// Charges the time between `start_ticks` (passed at construction) and its drop to the lock
/// contention runtime statistics of the given thread's `ThreadDispatcher`, if it has one.
///
/// This guard is `!Send` and `!Sync` because it must be dropped on `current_thread`.
pub struct ContentionTimer<'a> {
    current_thread: &'a Thread,
    start_ticks: InstantMonoTicks,
    _marker: PhantomData<*mut ()>,
}

impl<'a> ContentionTimer<'a> {
    /// Starts timing contention for `current_thread` from `start_ticks`.
    ///
    /// # Safety
    ///
    /// `current_thread` must be the current thread, and the timer must be dropped on it.
    pub unsafe fn new(current_thread: &'a Thread, start_ticks: InstantMonoTicks) -> Self {
        Self { current_thread, start_ticks, _marker: PhantomData }
    }
}

impl Drop for ContentionTimer<'_> {
    fn drop(&mut self) {
        // SAFETY: `current_thread` is a live `Thread`, borrowed for the lifetime of `self`, and
        // is the current thread (guaranteed by the caller of `new`), so its `user_thread` is not
        // changing concurrently.
        let user_thread = unsafe { ThreadPtr::from(self.current_thread).user_thread_raw() };
        // SAFETY: A non-null `user_thread` is the `ThreadDispatcher` of the current thread, which
        // outlives the `Thread` it is bound to.
        if let Some(user_thread) = unsafe { user_thread.as_ref() } {
            user_thread
                .add_lock_contention_ticks(timer_current_mono_ticks().0 - self.start_ticks.0);
        } else {
            // Contention is expected to be charged to user threads.
            core::hint::cold_path();
        }
    }
}
