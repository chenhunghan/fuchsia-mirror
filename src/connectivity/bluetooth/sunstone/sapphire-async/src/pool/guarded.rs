// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A pool object allocator providing unique, exclusive ownership via [`ObjectGuard`].
//!
//! This module provides [`Pool`], which wraps the thin [`crate::pool::UnsafePool`] abstraction
//! and returns [`ObjectGuard`] instances on allocation. Each guard provides exclusive access
//! to its slot via [`Deref`] and [`DerefMut`]. Dropping the guard automatically relinks the
//! slot back into the pool's free list.
//!
//! # Examples
//!
//! Synchronous claiming via [`try_claim`](Pool::try_claim):
//!
//! ```
//! use sapphire_async::pool::guarded::Pool;
//! use sapphire_async::pool::PoolCfg;
//! use sapphire_collections::storage::Global;
//! use sapphire_sync::mutex::raw::SingleThreadMutex;
//!
//! struct Cfg;
//! impl PoolCfg for Cfg {
//!     type Storage = Global;
//!     type Mutex = SingleThreadMutex;
//! }
//!
//! let pool = Pool::<i32, Cfg>::new(2).unwrap();
//! {
//!     let mut g1 = pool.try_claim().unwrap();
//!     let mut g2 = pool.try_claim().unwrap();
//!     *g1 = 10;
//!     *g2 = 20;
//!     assert_eq!(*g1, 10);
//!     assert_eq!(*g2, 20);
//!     assert!(pool.try_claim().is_none());
//! }
//! assert!(pool.try_claim().is_some());
//! ```
//!
//! Asynchronous claiming via [`claim`](Pool::claim):
//!
//! ```
//! use sapphire_async::executor::BoundedExecutor;
//! use sapphire_async::pool::guarded::Pool;
//! use sapphire_async::pool::PoolCfg;
//! use sapphire_async::testing::TestExecutor;
//! use sapphire_collections::storage::Global;
//! use sapphire_sync::mutex::raw::SingleThreadMutex;
//!
//! struct Cfg;
//! impl PoolCfg for Cfg {
//!     type Storage = Global;
//!     type Mutex = SingleThreadMutex;
//! }
//!
//! let pool = Pool::<i32, Cfg>::new(1).unwrap();
//! BoundedExecutor::new(TestExecutor::new(), |s| {
//!     s.block_on(async {
//!         let mut guard = pool.claim().await;
//!         *guard = 42;
//!         assert_eq!(*guard, 42);
//!     });
//! });
//! ```

use core::marker::PhantomData;
use core::ops::{Deref, DerefMut, Drop};

use sapphire_collections::AllocError;
use sapphire_collections::vec::Vec;

use crate::pool::{PoolCfg, PoolSlot, UnsafePool};

/// An object pool providing RAII-guarded exclusive access to slots.
pub struct Pool<T, Cfg: PoolCfg> {
    inner: UnsafePool<T, Cfg>,
}

impl<T, Cfg: PoolCfg> Pool<T, Cfg> {
    /// Creates a new guarded pool with capacity for `count` elements.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError`] if the backing storage fails to allocate the requested capacity.
    pub fn new(count: usize) -> Result<Self, AllocError>
    where
        Vec<PoolSlot<T>, Cfg::Storage>: Default,
        T: Default,
    {
        Ok(Self { inner: UnsafePool::new(count)? })
    }

    /// Attempts to claim an available slot from the pool synchronously, returning an [`ObjectGuard`].
    ///
    /// Returns `None` if all slots in the pool are currently claimed.
    pub fn try_claim(&self) -> Option<ObjectGuard<'_, T, Cfg>> {
        let index = self.inner.try_claim()?;
        Some(ObjectGuard { pool: self, index, _slot: PhantomData })
    }

    /// Asynchronously claims an available slot from the pool, returning an [`ObjectGuard`].
    ///
    /// Blocks the calling task until a slot becomes available.
    pub async fn claim(&self) -> ObjectGuard<'_, T, Cfg> {
        let index = self.inner.claim().await;
        ObjectGuard { pool: self, index, _slot: PhantomData }
    }

    /// Returns the total capacity of the pool.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if the pool has zero capacity.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// An RAII guard representing exclusive access to a claimed object from a [`Pool`].
///
/// Implements [`Deref`] and [`DerefMut`] to allow reading and mutating the object.
/// When dropped, the slot is automatically returned to the pool's free list.
pub struct ObjectGuard<'a, T, Cfg: PoolCfg> {
    pool: &'a Pool<T, Cfg>,
    index: usize,
    // Auto-derives Send + Sync matching exclusive-ownership semantics of the underlying pool object.
    _slot: PhantomData<&'a mut T>,
}

impl<'a, T, Cfg: PoolCfg> ObjectGuard<'a, T, Cfg> {
    /// Returns the 0-based slot index of the object within the pool.
    pub fn index(&self) -> usize {
        self.index
    }
}

impl<T, Cfg: PoolCfg> Deref for ObjectGuard<'_, T, Cfg> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: `self.index` was returned by `UnsafePool::claim` or `try_claim` and is not
        // unclaimed until this guard is dropped, so the pointer is in bounds, aligned, and points
        // to an initialized object (every slot is initialized when the pool is created). This
        // guard is the only owner of the slot, and the `&self` borrow prevents a `&mut T` from
        // `deref_mut` while the returned reference is alive.
        unsafe { &*self.pool.inner.get_raw(self.index) }
    }
}

impl<T, Cfg: PoolCfg> DerefMut for ObjectGuard<'_, T, Cfg> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: `self.index` was returned by `UnsafePool::claim` or `try_claim` and is not
        // unclaimed until this guard is dropped, so the pointer is in bounds, aligned, and points
        // to an initialized object (every slot is initialized when the pool is created). This
        // guard is the only owner of the slot, and the `&mut self` borrow makes the returned
        // reference the only reference to the object while it is alive.
        unsafe { &mut *self.pool.inner.get_raw(self.index) }
    }
}

impl<U: ?Sized, T: AsRef<U>, Cfg: PoolCfg> AsRef<U> for ObjectGuard<'_, T, Cfg> {
    fn as_ref(&self) -> &U {
        self.deref().as_ref()
    }
}

impl<U: ?Sized, T: AsMut<U>, Cfg: PoolCfg> AsMut<U> for ObjectGuard<'_, T, Cfg> {
    fn as_mut(&mut self) -> &mut U {
        self.deref_mut().as_mut()
    }
}

impl<T, Cfg: PoolCfg> Drop for ObjectGuard<'_, T, Cfg> {
    fn drop(&mut self) {
        // SAFETY: The slot was claimed when this ObjectGuard was created and has not been unclaimed.
        unsafe { self.pool.inner.unclaim(self.index) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::BoundedExecutor;
    use crate::testing::TestExecutor;
    use proptest::prelude::*;
    use sapphire_collections::storage::Global;
    use sapphire_sync::mutex::raw::{SingleThreadMutex, StdMutex};
    use std::sync::Arc;
    use std::thread;

    struct TestCfg;
    impl PoolCfg for TestCfg {
        type Storage = Global;
        type Mutex = SingleThreadMutex;
    }

    struct ThreadSafeCfg;
    impl PoolCfg for ThreadSafeCfg {
        type Storage = Global;
        type Mutex = StdMutex;
    }

    #[test]
    fn test_zero_capacity() {
        let pool = Pool::<i32, TestCfg>::new(0).expect("zero-capacity pool creation succeeds");
        assert!(pool.try_claim().is_none(), "try_claim on zero-capacity pool must return None");
        assert_eq!(pool.len(), 0);
        assert!(pool.is_empty());
    }

    #[test]
    fn test_basic_lifecycle() {
        let pool = Pool::<i32, TestCfg>::new(3).expect("pool creation succeeds");

        let mut g0 = pool.try_claim().expect("first slot claimed");
        let mut g1 = pool.try_claim().expect("second slot claimed");
        let mut g2 = pool.try_claim().expect("third slot claimed");
        assert!(pool.try_claim().is_none(), "pool should be exhausted after 3 allocations");

        *g0 = 100;
        *g1 = 200;
        *g2 = 300;
        assert_eq!(*g0, 100);
        assert_eq!(*g1, 200);
        assert_eq!(*g2, 300);

        // Verify slot index accessor
        assert_eq!(g0.index(), 2);
        assert_eq!(g1.index(), 1);
        assert_eq!(g2.index(), 0);

        // Drop one guard and re-claim; verify reclaimed slot retains its value
        drop(g1);
        let g_realloc = pool.try_claim().expect("reclaiming dropped slot succeeds");
        assert_eq!(*g_realloc, 200, "reclaimed slot retains previous value");
        assert!(pool.try_claim().is_none(), "pool exhausted again");

        // Clean up all active guards
        drop(g0);
        drop(g2);
        drop(g_realloc);

        // All 3 slots should be allocatable again
        let a = pool.try_claim();
        let b = pool.try_claim();
        let c = pool.try_claim();
        assert!(a.is_some() && b.is_some() && c.is_some());
        assert!(pool.try_claim().is_none());
    }

    #[test]
    fn test_zst() {
        let pool = Pool::<(), TestCfg>::new(5).expect("zst pool creation succeeds");
        let mut guards = std::vec::Vec::new();
        for _ in 0..5 {
            guards.push(pool.try_claim().expect("zst allocation succeeds"));
        }
        assert!(pool.try_claim().is_none(), "zst pool should be exhausted");
        drop(guards);
        assert!(pool.try_claim().is_some(), "zst slot available after drop");
    }

    #[test]
    fn test_multithreaded_concurrency() {
        const CAPACITY: usize = 8;
        const NUM_THREADS: usize = 4;
        const ITERATIONS_PER_THREAD: usize = 100;

        let pool = Arc::new(Pool::<u32, ThreadSafeCfg>::new(CAPACITY).expect("thread-safe pool"));

        thread::scope(|s| {
            for _ in 0..NUM_THREADS {
                let pool_clone = Arc::clone(&pool);
                s.spawn(move || {
                    for _ in 0..ITERATIONS_PER_THREAD {
                        if let Some(mut guard) = pool_clone.try_claim() {
                            *guard = guard.wrapping_add(1);
                            thread::yield_now();
                            drop(guard);
                        }
                    }
                });
            }
        });

        // After all threads finish, exactly CAPACITY slots must be available
        let mut guards = std::vec::Vec::new();
        for _ in 0..CAPACITY {
            guards.push(pool.try_claim().expect("all slots recoverable after multithreaded test"));
        }
        assert!(pool.try_claim().is_none(), "pool fully saturated");
    }

    #[test]
    fn test_async_guarded_claim_immediate() {
        let pool = Pool::<i32, TestCfg>::new(2).expect("pool creation succeeds");
        BoundedExecutor::new(TestExecutor::new(), |s| {
            s.block_on(async {
                let mut g0 = pool.claim().await;
                let mut g1 = pool.claim().await;
                *g0 = 42;
                *g1 = 84;
                assert_eq!(*g0, 42);
                assert_eq!(*g1, 84);
                assert!(pool.try_claim().is_none());
            });
        });
        assert!(pool.try_claim().is_some());
    }

    #[test]
    fn test_async_guarded_claim_blocks_until_drop() {
        let pool = Pool::<i32, TestCfg>::new(1).expect("pool creation succeeds");
        let mut initial_guard = pool.try_claim().expect("initial claim");
        *initial_guard = 10;

        BoundedExecutor::new(TestExecutor::new(), |s| {
            let mut handle = s.spawn(async {
                let mut guard = pool.claim().await;
                assert_eq!(*guard, 10, "slot retains value from previous owner");
                *guard = 20;
                *guard
            });

            s.run_until_stalled();
            assert!(!handle.is_finished(), "claim must stall when pool is exhausted");

            drop(initial_guard);

            s.run_until_stalled();
            assert!(handle.is_finished(), "claim must proceed once guard is dropped");
            assert_eq!(handle.get(), Some(20));
        });
    }

    #[test]
    fn test_async_guarded_multiple_waiters() {
        let pool = Pool::<usize, TestCfg>::new(1).expect("pool creation succeeds");
        let g0 = pool.try_claim().expect("initial claim");

        BoundedExecutor::new(TestExecutor::new(), |s| {
            let h1 = s.spawn(async {
                let mut g = pool.claim().await;
                *g = 1;
                drop(g);
            });
            let h2 = s.spawn(async {
                let mut g = pool.claim().await;
                *g = 2;
                drop(g);
            });

            s.run_until_stalled();
            assert!(!h1.is_finished());
            assert!(!h2.is_finished());

            drop(g0);
            s.run_until_stalled();
            assert!(h1.is_finished());
            assert!(h2.is_finished());
        });

        assert!(pool.try_claim().is_some());
    }

    #[test]
    fn test_async_guarded_cancellation() {
        use futures::future::FutureExt;
        let pool = Pool::<i32, TestCfg>::new(1).expect("pool creation succeeds");
        let g0 = pool.try_claim().expect("claim succeeds");
        let cancel_notif = crate::notification::Notification::<SingleThreadMutex>::new();

        BoundedExecutor::new(TestExecutor::new(), |s| {
            let mut h1 = s.spawn(async {
                let claim_fut = pool.claim().fuse();
                let cancel_fut = cancel_notif.wait().fuse();
                futures::pin_mut!(claim_fut);
                futures::pin_mut!(cancel_fut);
                futures::select_biased! {
                    _ = cancel_fut => None,
                    guard = claim_fut => Some(guard.index()),
                }
            });

            let mut h2 = s.spawn(async {
                let g = pool.claim().await;
                g.index()
            });

            s.run_until_stalled();
            assert!(!h1.is_finished());
            assert!(!h2.is_finished());

            cancel_notif.notify_one();
            drop(g0);

            s.run_until_stalled();
            assert!(h1.is_finished());
            assert_eq!(h1.get().unwrap(), None);

            assert!(h2.is_finished());
            assert_eq!(h2.get(), Some(0));
        });
    }

    #[derive(Debug, Clone, Copy)]
    enum LifecycleOp {
        TryClaim,
        DropIndex(usize),
    }

    proptest! {
        /// Property 1 (Capacity Bound): A pool of capacity N allows exactly N concurrent allocations;
        /// any subsequent allocation returns None until an object is dropped.
        #[test]
        fn prop_capacity_exhaustion(capacity in 0usize..50) {
            let pool = Pool::<i32, TestCfg>::new(capacity).expect("valid pool creation");
            let mut guards = std::vec::Vec::new();
            for _ in 0..capacity {
                guards.push(pool.try_claim().expect("allocation up to capacity must succeed"));
            }
            prop_assert!(pool.try_claim().is_none(), "allocation beyond capacity must return None");
        }

        /// Property 2 (Slot Distinctness & Isolation): Every claimed slot is distinct and exclusive;
        /// mutating one guard never modifies or aliases another active guard.
        #[test]
        fn prop_slot_distinctness_and_isolation(
            capacity in 1usize..50,
            alloc_count in 1usize..=50,
        ) {
            let actual_k = alloc_count.min(capacity);
            let pool = Pool::<u64, TestCfg>::new(capacity).expect("valid pool creation");
            let mut guards = std::vec::Vec::new();
            for _ in 0..actual_k {
                guards.push(pool.try_claim().expect("allocation within capacity succeeds"));
            }

            // Write unique pseudo-random values to each guard
            for (i, guard) in guards.iter_mut().enumerate() {
                **guard = (i as u64 + 1) * 0xdead_beef;
            }

            // Verify every guard retained its own value and was not corrupted by other guards
            for (i, guard) in guards.iter().enumerate() {
                prop_assert_eq!(**guard, (i as u64 + 1) * 0xdead_beef);
            }
        }

        /// Property 3 (Reclaimability): Dropping all active guards completely restores the pool's
        /// capacity to its initial state, allowing exactly N allocations again.
        #[test]
        fn prop_reclaimability_after_batch_drop(
            capacity in 1usize..50,
            alloc_count in 0usize..=50,
        ) {
            let actual_k = alloc_count.min(capacity);
            let pool = Pool::<i32, TestCfg>::new(capacity).expect("valid pool creation");

            // Claim k guards and drop them all
            let guards: std::vec::Vec<_> = (0..actual_k).map(|_| pool.try_claim().unwrap()).collect();
            drop(guards);

            // Exactly capacity allocations must now succeed again
            let mut reclaimed = std::vec::Vec::new();
            for _ in 0..capacity {
                reclaimed.push(pool.try_claim().expect("reclaiming full capacity"));
            }
            prop_assert!(pool.try_claim().is_none(), "capacity fully saturated again");
        }

        /// Property 4 (Lifecycle State-Machine Simulation): Under arbitrary sequences of interleaved
        /// claims and drop operations, the pool invariant holds: allocations succeed if and only if
        /// active count < capacity. Once all remaining objects are dropped, full capacity is recovered.
        #[test]
        fn prop_lifecycle_simulation(
            capacity in 1usize..30,
            ops in prop::collection::vec(
                prop_oneof![
                    Just(LifecycleOp::TryClaim),
                    any::<usize>().prop_map(LifecycleOp::DropIndex),
                ],
                1..100,
            ),
        ) {
            let pool = Pool::<usize, TestCfg>::new(capacity).expect("valid pool creation");
            let mut active_guards = std::vec::Vec::new();

            for op in ops {
                match op {
                    LifecycleOp::TryClaim => {
                        if active_guards.len() < capacity {
                            let guard = pool.try_claim().expect("slot must be available");
                            active_guards.push(guard);
                        } else {
                            prop_assert!(pool.try_claim().is_none(), "must be exhausted when full");
                        }
                    }
                    LifecycleOp::DropIndex(idx) => {
                        if !active_guards.is_empty() {
                            let remove_idx = idx % active_guards.len();
                            active_guards.swap_remove(remove_idx);
                        }
                    }
                }
                prop_assert!(active_guards.len() <= capacity);
            }

            // Drop all remaining active guards
            drop(active_guards);

            // Verify full recovery
            let mut recovered = std::vec::Vec::new();
            for _ in 0..capacity {
                recovered.push(pool.try_claim().expect("recovered full capacity"));
            }
            prop_assert!(pool.try_claim().is_none(), "pool should be saturated");
        }
    }
}
