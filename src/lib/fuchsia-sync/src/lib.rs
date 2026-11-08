// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fuchsia-native synchronization primitives.

#[cfg(target_os = "fuchsia")]
mod completion;
#[cfg(target_os = "fuchsia")]
pub use completion::*;
mod condvar;
pub use condvar::*;

#[cfg(target_os = "fuchsia")]
mod mutex;
#[cfg(target_os = "fuchsia")]
mod rwlock;

#[cfg(target_os = "fuchsia")]
pub use mutex::RawSyncMutex as RawMutex;
#[cfg(not(target_os = "fuchsia"))]
pub use parking_lot::RawMutex;

#[cfg(not(target_os = "fuchsia"))]
pub use parking_lot::RawRwLock;
#[cfg(target_os = "fuchsia")]
pub use rwlock::RawSyncRwLock as RawRwLock;

#[cfg(not(detect_lock_cycles))]
type RawMutexInner = RawMutex;
#[cfg(detect_lock_cycles)]
type RawMutexInner = tracing_mutex::lockapi::TracingWrapper<RawMutex>;

#[cfg(not(detect_lock_cycles))]
type RawRwLockInner = RawRwLock;
#[cfg(detect_lock_cycles)]
type RawRwLockInner = tracing_mutex::lockapi::TracingWrapper<RawRwLock>;

#[doc(hidden)]
#[repr(transparent)]
pub struct RawMutexImpl(RawMutexInner);

// SAFETY: `RawMutexImpl` is a transparent wrapper around `RawMutexInner`, which implements
// `lock_api::RawMutex`.
unsafe impl lock_api::RawMutex for RawMutexImpl {
    const INIT: Self = Self(<RawMutexInner as lock_api::RawMutex>::INIT);
    type GuardMarker = <RawMutexInner as lock_api::RawMutex>::GuardMarker;

    #[inline(always)]
    fn lock(&self) {
        self.0.lock();
    }

    #[inline(always)]
    fn try_lock(&self) -> bool {
        self.0.try_lock()
    }

    #[inline(always)]
    unsafe fn unlock(&self) {
        // SAFETY: Caller upholds `lock_api::RawMutex::unlock` invariants.
        unsafe {
            self.0.unlock();
        }
    }

    #[inline(always)]
    fn is_locked(&self) -> bool {
        self.0.is_locked()
    }
}

#[doc(hidden)]
#[repr(transparent)]
pub struct RawRwLockImpl(RawRwLockInner);

// SAFETY: `RawRwLockImpl` is a transparent wrapper around `RawRwLockInner`, which implements
// `lock_api::RawRwLock`.
unsafe impl lock_api::RawRwLock for RawRwLockImpl {
    const INIT: Self = Self(<RawRwLockInner as lock_api::RawRwLock>::INIT);
    type GuardMarker = <RawRwLockInner as lock_api::RawRwLock>::GuardMarker;

    #[inline(always)]
    fn lock_shared(&self) {
        self.0.lock_shared();
    }

    #[inline(always)]
    fn try_lock_shared(&self) -> bool {
        self.0.try_lock_shared()
    }

    #[inline(always)]
    unsafe fn unlock_shared(&self) {
        // SAFETY: Caller upholds `lock_api::RawRwLock::unlock_shared` invariants.
        unsafe {
            self.0.unlock_shared();
        }
    }

    #[inline(always)]
    fn lock_exclusive(&self) {
        self.0.lock_exclusive();
    }

    #[inline(always)]
    fn try_lock_exclusive(&self) -> bool {
        self.0.try_lock_exclusive()
    }

    #[inline(always)]
    unsafe fn unlock_exclusive(&self) {
        // SAFETY: Caller upholds `lock_api::RawRwLock::unlock_exclusive` invariants.
        unsafe {
            self.0.unlock_exclusive();
        }
    }

    #[inline(always)]
    fn is_locked(&self) -> bool {
        self.0.is_locked()
    }

    #[inline(always)]
    fn is_locked_exclusive(&self) -> bool {
        self.0.is_locked_exclusive()
    }
}

// SAFETY: `RawRwLockImpl` is a transparent wrapper around `RawRwLockInner`, which implements
// `lock_api::RawRwLockDowngrade`.
unsafe impl lock_api::RawRwLockDowngrade for RawRwLockImpl {
    #[inline(always)]
    unsafe fn downgrade(&self) {
        // SAFETY: Caller upholds `lock_api::RawRwLockDowngrade::downgrade` invariants.
        unsafe {
            self.0.downgrade();
        }
    }
}

pub type Mutex<T> = lock_api::Mutex<RawMutexImpl, T>;
pub type MutexGuard<'a, T> = lock_api::MutexGuard<'a, RawMutexImpl, T>;
pub type MappedMutexGuard<'a, T> = lock_api::MappedMutexGuard<'a, RawMutexImpl, T>;

pub type RwLock<T> = lock_api::RwLock<RawRwLockImpl, T>;
pub type RwLockReadGuard<'a, T> = lock_api::RwLockReadGuard<'a, RawRwLockImpl, T>;
pub type RwLockWriteGuard<'a, T> = lock_api::RwLockWriteGuard<'a, RawRwLockImpl, T>;
pub type MappedRwLockReadGuard<'a, T> = lock_api::MappedRwLockReadGuard<'a, RawRwLockImpl, T>;
pub type MappedRwLockWriteGuard<'a, T> = lock_api::MappedRwLockWriteGuard<'a, RawRwLockImpl, T>;

// SAFETY: Without `cfg(detect_lock_cycles)`, `RawMutexImpl` wraps a raw futex/atomic mutex state
// with no drop glue. With `cfg(detect_lock_cycles)`, `tracing_mutex::lockapi::TracingWrapper`
// removes its lock ID from the cycle-detection graph on drop without external blocking or
// thread-local affinity.
unsafe impl fuchsia_rcu_traits::RcuDroppable for RawMutexImpl {}

// SAFETY: Without `cfg(detect_lock_cycles)`, `RawRwLockImpl` wraps a raw futex/atomic rwlock state
// with no drop glue. With `cfg(detect_lock_cycles)`, `tracing_mutex::lockapi::TracingWrapper`
// removes its lock ID from the cycle-detection graph on drop without external blocking or
// thread-local affinity.
unsafe impl fuchsia_rcu_traits::RcuDroppable for RawRwLockImpl {}

// SAFETY: `RawSyncMutex` wraps `zx_futex_t` (`AtomicI32`) with no custom drop logic.
#[cfg(target_os = "fuchsia")]
unsafe impl fuchsia_rcu_traits::RcuDroppable for RawMutex {}

// SAFETY: `RawSyncRwLock` wraps `zx::Futex` (`AtomicI32`) with no custom drop logic.
#[cfg(target_os = "fuchsia")]
unsafe impl fuchsia_rcu_traits::RcuDroppable for RawRwLock {}

// SAFETY: `Condvar` has no custom drop logic and does not block on drop.
unsafe impl fuchsia_rcu_traits::RcuDroppable for Condvar {}

// SAFETY: `Completion` has no custom drop logic and does not block on drop.
#[cfg(target_os = "fuchsia")]
unsafe impl fuchsia_rcu_traits::RcuDroppable for Completion {}

/// Prevent potential deadlocks from panicking when lock cycle detection is enabled. This will
/// cause them to print instead of exiting the process.
pub fn suppress_lock_cycle_panics() {
    #[cfg(detect_lock_cycles)]
    tracing_mutex::util::set_cycle_handler(|message| eprintln!("{message}"));
}

/// A trait for locks whose dynamic dependency tracking graph can be reset.
///
/// This should only be called when we need to change a previous lock ordering.
pub trait ResetDependencies {
    /// Resets the lock dependency graph for this lock.
    ///
    /// # Safety
    ///
    /// It is the responsibility of the caller to ensure changing this lock ordering is safe.
    unsafe fn reset_dependencies(&self);
}

impl<T> ResetDependencies for RwLock<T> {
    #[inline(always)]
    unsafe fn reset_dependencies(&self) {
        #[cfg(detect_lock_cycles)]
        // SAFETY: The caller guarantees they are enforcing a sound locking order
        // and that resetting the graph will not mask a real deadlock.
        unsafe {
            tracing_mutex::util::reset_dependencies(&lock_api::RwLock::raw(self).0);
        }
    }
}

impl<T> ResetDependencies for Mutex<T> {
    #[inline(always)]
    unsafe fn reset_dependencies(&self) {
        #[cfg(detect_lock_cycles)]
        // SAFETY: The caller guarantees they are enforcing a sound locking order
        // and that resetting the graph will not mask a real deadlock.
        unsafe {
            tracing_mutex::util::reset_dependencies(&lock_api::Mutex::raw(self).0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_rcu_traits::RcuDroppable;

    fn assert_rcu_droppable<T: RcuDroppable>() {}

    #[test]
    fn test_rcu_droppable() {
        assert_rcu_droppable::<RawMutexImpl>();
        assert_rcu_droppable::<RawRwLockImpl>();
        assert_rcu_droppable::<Mutex<Condvar>>();
        assert_rcu_droppable::<RwLock<Condvar>>();
        assert_rcu_droppable::<Condvar>();
        #[cfg(target_os = "fuchsia")]
        {
            assert_rcu_droppable::<RawMutex>();
            assert_rcu_droppable::<RawRwLock>();
            assert_rcu_droppable::<Completion>();
        }
    }
}
