// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A lightweight completion event primitive wrapping `libsync`'s `sync_completion_t`.

use std::time::Duration;
use zx::sys;

unsafe extern "C" {
    fn sync_completion_wait(
        completion: *const sys::zx_futex_t,
        timeout: sys::zx_duration_mono_t,
    ) -> sys::zx_status_t;
    fn sync_completion_wait_deadline(
        completion: *const sys::zx_futex_t,
        deadline: sys::zx_instant_mono_t,
    ) -> sys::zx_status_t;
    fn sync_completion_signal(completion: *const sys::zx_futex_t);
    fn sync_completion_reset(completion: *const sys::zx_futex_t);
    fn sync_completion_signaled(completion: *const sys::zx_futex_t) -> bool;
}

const SYNC_COMPLETION_INIT: i32 = 0;

/// A lightweight in-process completion event.
///
/// Conceptually, a completion has an internal state of either unsignaled or signaled. Threads
/// may wait on the completion until it achieves the signaled state, or alter its state.
///
/// On Fuchsia, this wraps `sync_completion_t` from `zircon/system/ulib/sync`.
#[repr(transparent)]
pub struct Completion(sys::zx_futex_t);

// SAFETY: `Completion` wraps `sys::zx_futex_t` (`AtomicI32`), which is safe to send
// across threads because futex operations operate on process-wide atomic memory.
unsafe impl Send for Completion {}

// SAFETY: All operations on `Completion` synchronize via atomic operations and
// kernel futexes with acquire/release semantics, making concurrent access sound.
unsafe impl Sync for Completion {}

impl Completion {
    /// Creates a new completion in the unsignaled state.
    pub const fn new() -> Self {
        Self(sys::zx_futex_t::new(SYNC_COMPLETION_INIT))
    }

    #[inline]
    fn as_futex_ptr(&self) -> *const sys::zx_futex_t {
        std::ptr::addr_of!(self.0)
    }

    /// Blocks the current thread until the completion is signaled.
    pub fn wait(&self) {
        let signaled = self.wait_until(zx::MonotonicInstant::INFINITE);
        assert!(signaled, "sync_completion_wait with infinite deadline failed");
    }

    /// Blocks the current thread until the completion is signaled or the timeout expires.
    /// Returns `true` if the completion was signaled, or `false` if it timed out.
    pub fn wait_for(&self, timeout: Duration) -> bool {
        let nanos = zx::MonotonicDuration::from(timeout).into_nanos();
        // SAFETY: `self.as_futex_ptr()` points to a valid, aligned `zx_futex_t`
        // whose memory remains valid for the duration of `&self`.
        let status = unsafe { sync_completion_wait(self.as_futex_ptr(), nanos) };
        status == sys::ZX_OK
    }

    /// Blocks the current thread until the completion is signaled or the deadline is reached.
    /// Returns `true` if the completion was signaled, or `false` if it timed out.
    pub fn wait_until(&self, deadline: zx::MonotonicInstant) -> bool {
        // SAFETY: `self.as_futex_ptr()` points to a valid, aligned `zx_futex_t`
        // whose memory remains valid for the duration of `&self`.
        let status =
            unsafe { sync_completion_wait_deadline(self.as_futex_ptr(), deadline.into_nanos()) };
        status == sys::ZX_OK
    }

    /// Signals the completion, awakening all waiting threads.
    pub fn signal(&self) {
        // SAFETY: `self.as_futex_ptr()` points to a valid, aligned `zx_futex_t`
        // whose memory remains valid for the duration of `&self`.
        unsafe {
            sync_completion_signal(self.as_futex_ptr());
        }
    }

    /// Resets the completion to the unsignaled state.
    pub fn reset(&self) {
        // SAFETY: `self.as_futex_ptr()` points to a valid, aligned `zx_futex_t`
        // whose memory remains valid for the duration of `&self`.
        unsafe {
            sync_completion_reset(self.as_futex_ptr());
        }
    }

    /// Returns `true` if the completion has been signaled.
    pub fn is_signaled(&self) -> bool {
        // SAFETY: `self.as_futex_ptr()` points to a valid, aligned `zx_futex_t`
        // whose memory remains valid for the duration of `&self`.
        unsafe { sync_completion_signaled(self.as_futex_ptr()) }
    }
}

impl Default for Completion {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn test_completion_signal_then_wait() {
        let c = Completion::new();
        assert!(!c.is_signaled());
        c.signal();
        assert!(c.is_signaled());
        c.wait();
        assert!(c.is_signaled());
    }

    #[test]
    fn test_completion_wait_then_signal() {
        let c = Arc::new(Completion::new());
        let c_clone = c.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            c_clone.signal();
        });
        c.wait();
        handle.join().unwrap();
        assert!(c.is_signaled());
    }

    #[test]
    fn test_completion_reset() {
        let c = Completion::new();
        c.signal();
        assert!(c.is_signaled());
        c.reset();
        assert!(!c.is_signaled());
    }

    #[test]
    fn test_completion_multiple_waiters() {
        let c = Arc::new(Completion::new());
        let mut handles = vec![];
        for _ in 0..5 {
            let c_clone = c.clone();
            handles.push(std::thread::spawn(move || {
                c_clone.wait();
            }));
        }
        std::thread::sleep(Duration::from_millis(10));
        c.signal();
        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn test_completion_wait_for() {
        let c = Completion::new();
        assert!(!c.wait_for(Duration::from_millis(10)));

        c.signal();
        assert!(c.wait_for(Duration::from_millis(10)));
    }

    #[test]
    fn test_completion_wait_for_signal() {
        let c = Arc::new(Completion::new());
        let c_clone = c.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            c_clone.signal();
        });
        assert!(c.wait_for(Duration::from_secs(5)));
        handle.join().unwrap();
        assert!(c.is_signaled());
    }

    #[test]
    fn test_completion_wait_until() {
        let c = Completion::new();
        assert!(!c.wait_until(zx::MonotonicInstant::after(zx::MonotonicDuration::from_millis(10))));

        c.signal();
        assert!(c.wait_until(zx::MonotonicInstant::from_nanos(0)));
    }

    #[test]
    fn test_completion_wait_until_signal() {
        let c = Arc::new(Completion::new());
        let c_clone = c.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            c_clone.signal();
        });
        assert!(c.wait_until(zx::MonotonicInstant::after(zx::MonotonicDuration::from_seconds(5))));
        handle.join().unwrap();
        assert!(c.is_signaled());
    }
}
