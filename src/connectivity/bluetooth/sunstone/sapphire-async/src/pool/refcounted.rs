// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A reference-counted pool object allocator.
//!
//! This module provides a reference-counted [`Pool`], an object pool that manages slots with atomic
//! reference counts. When a slot is claimed, it is initially yielded as a [`RwSlotGuard`]
//! which provides exclusive access. To share the slot across multiple readers or threads, it can be downgraded
//! to a [`RoSlotGuard`] guard using [`share`](RwSlotGuard::share).
//!
//! [`RoSlotGuard`]s can be freely cloned, incrementing the slot's atomic reference
//! count. When all guards referencing a slot are dropped, the slot is automatically
//! returned to the pool's free list.
//!
//! # Examples
//!
//! Synchronous claiming via [`try_claim`](Pool::try_claim):
//!
//! ```
//! use sapphire_async::pool::PoolCfg;
//! use sapphire_async::pool::refcounted::{Pool, RcPoolCfg};
//! use sapphire_collections::storage::Global;
//! use sapphire_sync::atomic::StdAtomics;
//! use sapphire_sync::mutex::raw::SingleThreadMutex;
//!
//! struct Cfg;
//! impl PoolCfg for Cfg {
//!     type Storage = Global;
//!     type Mutex = SingleThreadMutex;
//! }
//! impl RcPoolCfg for Cfg {
//!     type Atomics = StdAtomics;
//! }
//!
//! let pool = Pool::<i32, Cfg>::new(1).unwrap();
//!
//! let rw = pool.try_claim().expect("slot claimed");
//! let ro1 = rw.share();
//! let ro2 = ro1.clone();
//!
//! assert_eq!(*ro1, 0);
//! assert_eq!(*ro2, 0);
//! assert_eq!(ro1.ref_count(), 2);
//!
//! drop(ro1);
//! // Pool is still exhausted because ro2 holds a reference.
//! assert!(pool.try_claim().is_none());
//!
//! drop(ro2);
//! // Last reference dropped; slot is now reclaimed.
//! assert!(pool.try_claim().is_some());
//! ```
//!
//! Asynchronous claiming via [`claim`](Pool::claim):
//!
//! ```
//! use sapphire_async::executor::BoundedExecutor;
//! use sapphire_async::pool::PoolCfg;
//! use sapphire_async::pool::refcounted::{Pool, RcPoolCfg};
//! use sapphire_async::testing::TestExecutor;
//! use sapphire_collections::storage::Global;
//! use sapphire_sync::atomic::StdAtomics;
//! use sapphire_sync::mutex::raw::SingleThreadMutex;
//!
//! struct Cfg;
//! impl PoolCfg for Cfg {
//!     type Storage = Global;
//!     type Mutex = SingleThreadMutex;
//! }
//! impl RcPoolCfg for Cfg {
//!     type Atomics = StdAtomics;
//! }
//!
//! let pool = Pool::<i32, Cfg>::new(1).unwrap();
//! BoundedExecutor::new(TestExecutor::new(), |s| {
//!     s.block_on(async {
//!         let mut rw = pool.claim().await;
//!         *rw = 42;
//!         assert_eq!(*rw, 42);
//!     });
//! });
//! ```

use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut, Drop};
use core::sync::atomic::Ordering;

use sapphire_collections::AllocError;
use sapphire_collections::vec::Vec;

use crate::pool::{PoolCfg, PoolSlot, UnsafePool};
use sapphire_sync::atomic::{
    AtomicAdd, AtomicFamily, AtomicLoad, AtomicNew, AtomicNum, AtomicOf, AtomicSub, AtomicWraps,
};

/// An internal slot node combining the payload object with an atomic reference count.
struct RcSlot<T, A: AtomicNum<Item = usize>> {
    payload: T,
    refs: A,
}

impl<T: Default, A: AtomicNum<Item = usize>> Default for RcSlot<T, A> {
    fn default() -> Self {
        Self { payload: T::default(), refs: AtomicNew::new(0) }
    }
}

/// Configuration trait for an [`Pool`], extending [`PoolCfg`] with atomic types.
pub trait RcPoolCfg: PoolCfg {
    /// The atomic family used for reference counters.
    type Atomics: AtomicFamily<usize, Atomic: AtomicNum>;
}

/// A reference-counted pool object allocator.
pub struct Pool<T, Cfg: RcPoolCfg> {
    inner: UnsafePool<RcSlot<T, AtomicOf<Cfg::Atomics, usize>>, Cfg>,
}

impl<T, Cfg: RcPoolCfg> Pool<T, Cfg> {
    /// Creates a new reference-counted pool with capacity for `count` elements.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError`] if the backing storage fails to allocate the requested capacity.
    pub fn new(count: usize) -> Result<Self, AllocError>
    where
        Vec<PoolSlot<RcSlot<T, AtomicOf<Cfg::Atomics, usize>>>, Cfg::Storage>: Default,
        T: Default,
    {
        Ok(Self { inner: UnsafePool::new(count)? })
    }

    /// Attempts to claim an available slot from the pool synchronously.
    ///
    /// Returns a [`RwSlotGuard`] initialized with a reference count of 1,
    /// or `None` if the pool is exhausted.
    pub fn try_claim(&self) -> Option<RwSlotGuard<'_, T, Cfg>> {
        let index = self.inner.try_claim()?;
        let mut guard = RwSlotGuard { pool: self, index, _slot: PhantomData };
        *guard.slot_mut().refs.get_mut() = 1;
        Some(guard)
    }

    /// Asynchronously claims an available slot from the pool.
    ///
    /// Blocks the calling task until a slot is available.
    /// Returns a [`RwSlotGuard`] initialized with a reference count of 1.
    pub async fn claim(&self) -> RwSlotGuard<'_, T, Cfg> {
        let index = self.inner.claim().await;
        let mut guard = RwSlotGuard { pool: self, index, _slot: PhantomData };
        *guard.slot_mut().refs.get_mut() = 1;
        guard
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

/// A read-write RAII guard representing exclusive access to a claimed object in an [`RcPool`].
pub struct RwSlotGuard<'a, T, Cfg: RcPoolCfg> {
    pool: &'a Pool<T, Cfg>,
    index: usize,
    // Auto-derives Send + Sync matching unique mutable access to the slot.
    _slot: PhantomData<&'a mut RcSlot<T, AtomicOf<Cfg::Atomics, usize>>>,
}

/// A read-only RAII guard representing shared reference-counted access to a claimed object in an [`RcPool`].
pub struct RoSlotGuard<'a, T, Cfg: RcPoolCfg> {
    pool: &'a Pool<T, Cfg>,
    index: usize,
    // Auto-derives Send + Sync matching shared immutable access to the slot.
    _slot: PhantomData<&'a RcSlot<T, AtomicOf<Cfg::Atomics, usize>>>,
}

impl<'a, T, Cfg: RcPoolCfg> RwSlotGuard<'a, T, Cfg> {
    /// Returns the 0-based slot index of the object within the pool.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Converts this guard into a read-only [`RoSlotGuard`].
    ///
    /// The reference count remains unchanged.
    pub fn share(self) -> RoSlotGuard<'a, T, Cfg> {
        let this = ManuallyDrop::new(self);
        RoSlotGuard { pool: this.pool, index: this.index, _slot: PhantomData }
    }

    fn slot(&self) -> &RcSlot<T, AtomicOf<Cfg::Atomics, usize>> {
        // SAFETY: The slot was claimed when this guard was created and is not unclaimed until
        // it is dropped, so the pointer is in bounds, aligned, and points to an initialized slot
        // (every slot is initialized when the pool is created). An `RwSlotGuard` is the only
        // guard for its slot, and the `&self` borrow prevents a `&mut` from `slot_mut` while the
        // returned reference is alive.
        unsafe { &(*self.pool.inner.get_raw(self.index)) }
    }

    fn slot_mut(&mut self) -> &mut RcSlot<T, AtomicOf<Cfg::Atomics, usize>> {
        // SAFETY: The slot was claimed when this guard was created and is not unclaimed until
        // it is dropped, so the pointer is in bounds, aligned, and points to an initialized slot
        // (every slot is initialized when the pool is created). An `RwSlotGuard` is the only
        // guard for its slot, and the `&mut self` borrow makes the returned reference the only
        // reference to the slot while it is alive.
        unsafe { &mut (*self.pool.inner.get_raw(self.index)) }
    }
}

impl<'a, T, Cfg: RcPoolCfg> RoSlotGuard<'a, T, Cfg> {
    /// Returns the 0-based slot index of the object within the pool.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Returns the current atomic reference count of the slot.
    pub fn ref_count(&self) -> usize {
        self.slot().refs.load(Ordering::Relaxed)
    }

    fn slot(&self) -> &RcSlot<T, AtomicOf<Cfg::Atomics, usize>> {
        // SAFETY: The slot was claimed when the originating `RwSlotGuard` was created, and this
        // guard holds one of its references, so the refcount stays positive and the slot is not
        // unclaimed while this guard is alive. The pointer is therefore in bounds, aligned, and
        // points to an initialized slot (every slot is initialized when the pool is created).
        // The `RwSlotGuard` was consumed by `share`, and `RoSlotGuard`s only create shared
        // references, so no `&mut` to the slot exists. The refcount is mutated only atomically.
        unsafe { &(*self.pool.inner.get_raw(self.index)) }
    }
}

impl<T, Cfg: RcPoolCfg> Deref for RoSlotGuard<'_, T, Cfg> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.slot().payload
    }
}

impl<T, Cfg: RcPoolCfg> Deref for RwSlotGuard<'_, T, Cfg> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.slot().payload
    }
}

impl<T, Cfg: RcPoolCfg> DerefMut for RwSlotGuard<'_, T, Cfg> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.slot_mut().payload
    }
}

impl<U: ?Sized, T: AsRef<U>, Cfg: RcPoolCfg> AsRef<U> for RoSlotGuard<'_, T, Cfg> {
    fn as_ref(&self) -> &U {
        self.deref().as_ref()
    }
}

impl<U: ?Sized, T: AsRef<U>, Cfg: RcPoolCfg> AsRef<U> for RwSlotGuard<'_, T, Cfg> {
    fn as_ref(&self) -> &U {
        self.deref().as_ref()
    }
}

impl<U: ?Sized, T: AsMut<U>, Cfg: RcPoolCfg> AsMut<U> for RwSlotGuard<'_, T, Cfg> {
    fn as_mut(&mut self) -> &mut U {
        self.deref_mut().as_mut()
    }
}

impl<'a, T, Cfg: RcPoolCfg> Clone for RoSlotGuard<'a, T, Cfg> {
    fn clone(&self) -> Self {
        // Relaxed is sufficient because the new reference is created from an existing one, so no
        // new data needs to be synchronized (same as `std::sync::Arc::clone`).
        self.slot().refs.fetch_add(1, Ordering::Relaxed);
        Self { pool: self.pool, index: self.index, _slot: PhantomData }
    }
}

impl<T, Cfg: RcPoolCfg> Drop for RoSlotGuard<'_, T, Cfg> {
    fn drop(&mut self) {
        let prev = self.slot().refs.fetch_sub(1, Ordering::AcqRel);
        match prev {
            0 => unreachable!("Arithmetic underflow on atomic reference count"),
            1 => {
                // The last reference was dropped; return the slot to the pool.
                // SAFETY: The slot was claimed from this pool, and this was the last guard
                // referencing it, so no other guards or references to the object exist.
                unsafe { self.pool.inner.unclaim(self.index) };
            }
            2.. => {}
        }
    }
}

impl<T, Cfg: RcPoolCfg> Drop for RwSlotGuard<'_, T, Cfg> {
    fn drop(&mut self) {
        let refs = self.slot_mut().refs.get_mut();
        assert_eq!(*refs, 1);
        *refs = 0;
        // SAFETY: The slot was claimed from this pool, and this guard has exclusive access to it,
        // so no other guards or references to the object exist.
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
    use sapphire_sync::atomic::{SingleThreadAtomics, StdAtomics};
    use sapphire_sync::mutex::raw::{SingleThreadMutex, StdMutex};
    use std::sync::Arc;
    use std::thread;

    struct TestCfg;
    impl PoolCfg for TestCfg {
        type Storage = Global;
        type Mutex = SingleThreadMutex;
    }
    impl RcPoolCfg for TestCfg {
        type Atomics = SingleThreadAtomics;
    }

    struct ThreadSafeCfg;
    impl PoolCfg for ThreadSafeCfg {
        type Storage = Global;
        type Mutex = StdMutex;
    }
    impl RcPoolCfg for ThreadSafeCfg {
        type Atomics = StdAtomics;
    }

    #[test]
    fn test_rc_zero_capacity() {
        let pool = Pool::<i32, TestCfg>::new(0).expect("zero-capacity rc pool creation succeeds");
        assert!(pool.try_claim().is_none());
        assert_eq!(pool.len(), 0);
        assert!(pool.is_empty());
    }

    #[test]
    fn test_rc_basic_lifecycle() {
        let pool = Pool::<i32, TestCfg>::new(2).expect("pool creation succeeds");

        // Claim as RwSlotGuard and mutate
        let mut rw = pool.try_claim().expect("claim 1 succeeds");
        *rw = 42;
        assert_eq!(*rw, 42);

        // Convert to RoSlotGuard
        let ro1 = rw.share();
        assert_eq!(*ro1, 42);
        assert_eq!(ro1.ref_count(), 1);

        // Clone RoSlotGuard
        let ro2 = ro1.clone();
        assert_eq!(*ro2, 42);
        assert_eq!(ro1.ref_count(), 2);
        assert_eq!(ro2.ref_count(), 2);

        // Claim the second slot
        let mut rw2 = pool.try_claim().expect("claim 2 succeeds");
        *rw2 = 99;
        assert!(pool.try_claim().is_none(), "pool should now be exhausted");

        // Drop ro1; ro2 still holds a reference, so the slot is not reclaimed yet
        drop(ro1);
        assert!(pool.try_claim().is_none(), "slot must not be reclaimed while ro2 is alive");
        assert_eq!(ro2.ref_count(), 1);

        // Drop ro2; the first slot should now be reclaimed
        drop(ro2);
        let mut reclaimed = pool.try_claim().expect("first slot reclaimed");
        assert_eq!(*reclaimed, 42, "retained previous value");
        *reclaimed = 123;

        drop(rw2);
        drop(reclaimed);

        // Both slots should be claimable again
        let a = pool.try_claim();
        let b = pool.try_claim();
        assert!(a.is_some() && b.is_some());
        assert!(pool.try_claim().is_none());
    }

    #[test]
    fn test_rc_multithreaded_cloning() {
        const CAPACITY: usize = 4;
        const NUM_THREADS: usize = 4;
        const NUM_CLONES_PER_THREAD: usize = 20;

        let pool = Arc::new(Pool::<u32, ThreadSafeCfg>::new(CAPACITY).expect("thread-safe pool"));

        let rw = pool.try_claim().expect("claim slot");
        let ro = rw.share();

        thread::scope(|s| {
            for _ in 0..NUM_THREADS {
                let ro_clone = ro.clone();
                s.spawn(move || {
                    for _ in 0..NUM_CLONES_PER_THREAD {
                        let temp = ro_clone.clone();
                        assert_eq!(*temp, 0);
                        thread::yield_now();
                        drop(temp);
                    }
                });
            }
        });

        // After all thread clones drop, only `ro` remains
        assert_eq!(ro.ref_count(), 1);
        drop(ro);

        // All CAPACITY slots must be available
        let mut guards = std::vec::Vec::new();
        for _ in 0..CAPACITY {
            guards.push(pool.try_claim().expect("all slots available"));
        }
        assert!(pool.try_claim().is_none());
    }

    #[test]
    fn test_async_rc_claim_immediate() {
        let pool = Pool::<i32, TestCfg>::new(2).expect("pool creation succeeds");
        BoundedExecutor::new(TestExecutor::new(), |s| {
            s.block_on(async {
                let mut rw0 = pool.claim().await;
                let mut rw1 = pool.claim().await;
                *rw0 = 11;
                *rw1 = 22;
                assert_eq!(*rw0, 11);
                assert_eq!(*rw1, 22);
                assert!(pool.try_claim().is_none());
            });
        });
        assert!(pool.try_claim().is_some());
    }

    #[test]
    fn test_async_rc_claim_blocks_until_ro_clones_drop() {
        let pool = Pool::<i32, TestCfg>::new(1).expect("pool creation succeeds");
        let rw = pool.try_claim().expect("initial claim");
        let ro1 = rw.share();
        let ro2 = ro1.clone();

        BoundedExecutor::new(TestExecutor::new(), |s| {
            let mut handle = s.spawn(async {
                let mut guard = pool.claim().await;
                *guard += 1;
                *guard
            });

            s.run_until_stalled();
            assert!(!handle.is_finished(), "must stall while ro guards exist");

            drop(ro1);
            s.run_until_stalled();
            assert!(!handle.is_finished(), "must stall while ro2 is still alive");

            drop(ro2);
            s.run_until_stalled();
            assert!(handle.is_finished(), "must complete once all ro guards dropped");
            assert_eq!(handle.get(), Some(1));
        });
    }

    #[test]
    fn test_async_rc_multiple_waiters() {
        let pool = Pool::<usize, TestCfg>::new(1).expect("pool creation succeeds");
        let rw = pool.try_claim().expect("initial claim");

        BoundedExecutor::new(TestExecutor::new(), |s| {
            let h1 = s.spawn(async {
                let mut g = pool.claim().await;
                *g = 1;
                let ro = g.share();
                let clone = ro.clone();
                drop(ro);
                drop(clone);
            });
            let h2 = s.spawn(async {
                let mut g = pool.claim().await;
                *g = 2;
                drop(g);
            });

            s.run_until_stalled();
            assert!(!h1.is_finished());
            assert!(!h2.is_finished());

            drop(rw);
            s.run_until_stalled();
            assert!(h1.is_finished());
            assert!(h2.is_finished());
        });

        assert!(pool.try_claim().is_some());
    }

    #[test]
    fn test_async_rc_cancellation() {
        use futures::future::FutureExt;
        let pool = Pool::<i32, TestCfg>::new(1).expect("pool creation succeeds");
        let rw = pool.try_claim().expect("initial claim");
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
            drop(rw);

            s.run_until_stalled();
            assert!(h1.is_finished());
            assert_eq!(h1.get().unwrap(), None);

            assert!(h2.is_finished());
            assert_eq!(h2.get(), Some(0));
        });
    }

    proptest! {
        /// Property 1 (Capacity Bound): Exactly N claims succeed on a pool of capacity N.
        #[test]
        fn prop_rc_capacity_exhaustion(capacity in 0usize..50) {
            let pool = Pool::<i32, TestCfg>::new(capacity).expect("valid pool creation");
            let mut guards = std::vec::Vec::new();
            for _ in 0..capacity {
                guards.push(pool.try_claim().expect("claim up to capacity"));
            }
            prop_assert!(pool.try_claim().is_none(), "claim beyond capacity must return None");
        }

        /// Property 2 (Ref Count Mechanics): Cloning C times yields ref_count == C + 1,
        /// and dropping C clones keeps the slot occupied until the last guard drops.
        #[test]
        fn prop_rc_ref_counting(clones in 1usize..30) {
            let pool = Pool::<u64, TestCfg>::new(1).expect("pool creation");
            let rw = pool.try_claim().expect("claim slot");
            let ro = rw.share();

            let mut clone_list = std::vec::Vec::new();
            for expected_count in 2..=(clones + 1) {
                clone_list.push(ro.clone());
                prop_assert_eq!(ro.ref_count(), expected_count);
            }

            // Pool is full while clones are alive
            prop_assert!(pool.try_claim().is_none());

            // Drop all clones
            drop(clone_list);
            prop_assert_eq!(ro.ref_count(), 1);
            prop_assert!(pool.try_claim().is_none(), "slot still occupied by ro");

            // Drop the original guard
            drop(ro);
            prop_assert!(pool.try_claim().is_some(), "slot reclaimed after last drop");
        }

        /// Property 3 (State-Machine Simulation): Interleaved claims, clones, and drops
        /// strictly respect capacity and reclaimability invariants.
        #[test]
        fn prop_rc_lifecycle_simulation(
            capacity in 1usize..20,
            ops in prop::collection::vec(
                prop_oneof![
                    Just(0usize), // Claim new slot
                    Just(1usize), // Clone an existing guard
                    any::<usize>(), // Drop a guard at index
                ],
                1..60,
            ),
        ) {
            let pool = Pool::<usize, TestCfg>::new(capacity).expect("valid pool creation");
            let mut active: std::vec::Vec<RoSlotGuard<'_, usize, TestCfg>> = std::vec::Vec::new();

            for op in ops {
                match op {
                    0 => {
                        // Claim a new slot
                        if let Some(rw) = pool.try_claim() {
                            active.push(rw.share());
                        }
                    }
                    1 => {
                        // Clone an existing guard
                        if !active.is_empty() {
                            let idx = active.len() - 1;
                            active.push(active[idx].clone());
                        }
                    }
                    drop_idx => {
                        // Drop a guard
                        if !active.is_empty() {
                            let idx = drop_idx % active.len();
                            active.swap_remove(idx);
                        }
                    }
                }
            }

            // Drop all remaining active guards
            drop(active);

            // Verify full recovery
            let mut recovered = std::vec::Vec::new();
            for _ in 0..capacity {
                recovered.push(pool.try_claim().expect("recovered full capacity"));
            }
            prop_assert!(pool.try_claim().is_none(), "pool should be saturated");
        }
    }
}
