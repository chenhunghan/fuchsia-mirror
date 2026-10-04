// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A fixed-capacity, lock-based pool object allocator.
//!
//! This module provides a low-level, thin [`UnsafePool`] abstraction that manages fixed-capacity
//! contiguous storage backed by an intrusive free list. It handles allocation mechanics
//! (claiming slot indices and returning them to the free list) using a configurable
//! [`RawMutex`](sapphire_sync::mutex::raw::RawMutex) and [`StorageFamily`].
//!
//! # Submodules
//!
//! High-level lifecycle abstractions are built on top of this thin pool:
//! - [`guarded`]: Provides unique, single-ownership [`ObjectGuard`](guarded::ObjectGuard) handles
//!   that automatically unclaim the slot upon [`Drop`].
//! - [`refcounted`]: Provides multi-owner reference-counted [`RoSlotGuard`](refcounted::RoSlotGuard)
//!   and [`RwSlotGuard`](refcounted::RwSlotGuard) handles that track references atomically and
//!   unclaim the slot when the reference count drops to 0.
//!
//! # Architecture
//!
//! - **Contiguous Storage**: All slots are stored contiguously in a [`Vec`] parameterizable
//!   over a [`StorageFamily`]. The underlying storage is initialized once during [`UnsafePool::new`]
//!   and is never reallocated or moved.
//! - **Intrusive Free List**: Unallocated slots form an intrusive singly-linked list
//!   threaded through the `next_free` indices of free slots, terminating at [`END`].
//! - **Fine-Grained Synchronization**: The pool's mutex protects only the `next_free`
//!   head index and the linking metadata. Claiming pops a slot index from the head, and
//!   unclaiming pushes the slot index back onto the head.
//! - **Semaphore Tracking**: Available slots are tracked by an asynchronous [`Semaphore`], allowing
//!   callers to asynchronously await an available slot via [`claim`](UnsafePool::claim) or
//!   synchronously poll with [`try_claim`](UnsafePool::try_claim).
//! - **Interior Mutability**: Each slot is wrapped in an [`UnsafeCell`], allowing safe,
//!   lock-free mutable access to claimed slots without holding the pool lock.

use core::cell::UnsafeCell;

use sapphire_collections::AllocError;
use sapphire_collections::storage::StorageFamily;
use sapphire_collections::vec::Vec;

use crate::semaphore::Semaphore;
use sapphire_sync::mutex::Mutex;
use sapphire_sync::mutex::raw::RawMutex;

pub mod guarded;
pub mod refcounted;

pub use guarded::{ObjectGuard, Pool as GuardedPool};
pub use refcounted::{Pool as RcPool, RcPoolCfg, RoSlotGuard, RwSlotGuard};

/// Configuration trait for customizing a [`Pool`]'s storage and synchronization strategy.
pub trait PoolCfg {
    /// The storage family used to back the pool's contiguous buffer (e.g. heap-allocated
    /// [`Global`](sapphire_collections::storage::Global) or stack-allocated inline arrays).
    type Storage: StorageFamily;

    /// The raw mutual exclusion primitive used to serialize access to the free list
    /// (e.g. `SpinMutex`, `SingleThreadMutex`, or parking_lot's `RawMutex`).
    type Mutex: RawMutex;
}

/// The internal state of a single slot in the pool.
struct PoolSlotInner<T> {
    /// The payload object.
    object: T,
    /// The index of the next free slot in the intrusive free list chain, or [`END`].
    next_free: usize,
}

/// A wrapper providing interior mutability for a pool slot.
struct PoolSlot<T>(UnsafeCell<PoolSlotInner<T>>);

/// Sentinel value indicating the end of the free list chain.
const END: usize = usize::MAX;

impl<T> PoolSlot<T> {
    /// Creates a new slot with the given initial object and next-free pointer.
    pub const fn new(object: T, next_free: usize) -> Self {
        Self(UnsafeCell::new(PoolSlotInner { object, next_free }))
    }
}

/// A low-level, fixed-capacity pool object allocator.
///
/// Pre-allocates `count` slots of type `T` in contiguous storage. Slots can be claimed
/// asynchronously by calling [`claim`](UnsafePool::claim), which waits until a slot is
/// available, or synchronously by calling [`try_claim`](UnsafePool::try_claim), which
/// returns immediately. When the caller is finished with the slot, it must be returned
/// via [`unclaim`](UnsafePool::unclaim).
///
/// This data structure isn't safe to use as it requires custom life-cycle object management with
/// index tracking and the `unsafe` `fn unclaim()`. Prefer to use the [Guarded Pool](guarded::Pool) which
/// provides single-owner semantics or the [Reference Counted Pool](refcounted::Pool) which provides
/// reference-counted shared-ownership semantics.
pub struct UnsafePool<T, Cfg: PoolCfg> {
    /// Contiguous storage for all pool slots. Immutable in size and location after creation.
    objects: Vec<PoolSlot<T>, Cfg::Storage>,
    /// Index of the head of the free list, synchronized by a [`Mutex`].
    next_free: Mutex<Cfg::Mutex, usize>,
    /// Remaining free objects in the pool, tracked by an asynchronous [`Semaphore`].
    free_objects: Semaphore<Cfg::Mutex>,
}

// SAFETY: Access to Pool is synchronized: allocations and deallocations are serialized
// by `next_free` (protected by a RawMutex), and active slots are exclusively
// owned by the caller who claimed them.
unsafe impl<T, Cfg: PoolCfg> Sync for UnsafePool<T, Cfg>
where
    Cfg::Mutex: Sync,
    Cfg::Storage: Sync,
    T: Send,
{
}

// SAFETY: Pool uniquely owns its backing storage and all contained elements.
// Sending Pool to another thread transfers ownership of T and the backing storage,
// which is sound as long as T: Send, Cfg::Storage: Send, and Cfg::Mutex: Send.
unsafe impl<T, Cfg: PoolCfg> Send for UnsafePool<T, Cfg>
where
    Cfg::Mutex: Send,
    Cfg::Storage: Send,
    T: Send,
{
}

impl<T, Cfg: PoolCfg> UnsafePool<T, Cfg> {
    /// Creates a new object pool with capacity for `count` elements.
    ///
    /// Each slot is populated with `T::default()`. The initial free list is linked
    /// such that slots are handed out starting from the last index down to 0.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError`] if the backing storage fails to allocate the requested
    /// capacity.
    pub fn new(count: usize) -> Result<Self, AllocError>
    where
        Vec<PoolSlot<T>, Cfg::Storage>: Default,
        T: Default,
    {
        let mut objects = Vec::new();
        let mut next_free = END;
        objects.try_resize_with(count, |i| {
            let slot = PoolSlot::new(T::default(), next_free);
            next_free = i;
            slot
        })?;

        Ok(Self { objects, next_free: Mutex::new(next_free), free_objects: Semaphore::new(count) })
    }

    /// Asynchronously claims an object slot from the pool.
    ///
    /// Returns the `index` of the object slot that was claimed. This method will asynchronously wait
    /// until a slot is available.
    pub async fn claim(&self) -> usize {
        self.free_objects.down().await;
        // SAFETY: semaphore down was successful
        unsafe { self.do_claim() }
    }

    /// Attempts to claim an available slot from the pool synchronously.
    ///
    /// Returns `Some(index)` if a slot was available, or `None` if the pool is exhausted.
    /// The pool mutex is acquired only briefly to pop the slot from the free list
    /// and is released before this method returns.
    pub fn try_claim(&self) -> Option<usize> {
        self.free_objects.try_down().ok()?;
        // SAFETY: semaphore down was successful
        Some(unsafe { self.do_claim() })
    }

    /// Force claims the next object in the pool
    ///
    /// # Safety
    ///
    /// This doesn't update the semaphore nor waits for an object to actually be available
    /// in the pool. Callers must have called down on the semaphore protecting the objects.
    ///
    /// # Postconditions
    ///
    /// - Returns a valid slot index, i.e. less than [`len`](UnsafePool::len).
    /// - The returned index grants exclusive, unaliased ownership of the corresponding slot and
    ///   its object until it is returned via [`unclaim`](UnsafePool::unclaim): no other claim
    ///   returns the same index in the meantime.
    unsafe fn do_claim(&self) -> usize {
        let mut head = self.next_free.lock();
        let next = *head;
        debug_assert!(next != END, "Semaphore down guarantees object list is not empty");
        let object = self.objects.get(next).expect("Index should be in bounds");
        // SAFETY: This data structure guarantees that this slot cannot be used anywhere else
        // so long as we update the next_free head before dropping the mutex guard.
        let object = unsafe { &mut *object.0.get() };
        *head = object.next_free;
        drop(head);
        // The object is now properly unlinked.
        object.next_free = END;
        next
    }

    /// Drops an existing claim without an RAII guard.
    ///
    /// # Safety
    ///
    /// The slot at `index` must have been previously claimed from this pool and not yet unclaimed.
    /// No guards or active references to the slot's object may exist when this is called.
    pub unsafe fn unclaim(&self, index: usize) {
        let slot = &self.objects[index];
        {
            let mut head = self.next_free.lock();
            // SAFETY: The caller guarantees exclusive ownership of the unclaim operation for this slot.
            unsafe { (*slot.0.get()).next_free = *head };
            *head = index;
        }
        // Only release the permit once the slot is back on the free list, so that a concurrent
        // claim that acquires this permit is guaranteed to find a free slot.
        self.free_objects.up();
    }

    /// Returns a raw pointer to the object at the provided index.
    ///
    /// # Safe Usage
    ///
    /// The method is safe but returns a raw pointer which makes any operations
    /// on the returned value unsafe. In order to use this method safely, the caller
    /// must have claimed the object with [`claim`](UnsafePool::claim) or
    /// [`try_claim`](UnsafePool::try_claim), which returns the object's index.
    /// Upon claiming the object, the `Pool` data structure guarantees that no other
    /// claim calls will return the same index until the caller calls `unclaim` with the index.
    pub fn get_raw(&self, index: usize) -> *mut T {
        // SAFETY: The pointer is created by UnsafeCell and guaranteed to be valid. This expression
        // does not create any ephemeral references.
        unsafe { &raw mut (*self.objects[index].0.get()).object }
    }

    /// Returns the total capacity of the pool.
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    /// Returns `true` if the pool has zero capacity.
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::BoundedExecutor;
    use crate::testing::TestExecutor;
    use sapphire_collections::storage::Global;
    use sapphire_sync::mutex::raw::SingleThreadMutex;

    struct TestCfg;
    impl PoolCfg for TestCfg {
        type Storage = Global;
        type Mutex = SingleThreadMutex;
    }

    #[test]
    fn test_thin_pool_basic() {
        let pool = UnsafePool::<i32, TestCfg>::new(3).expect("pool creation succeeds");
        assert_eq!(pool.len(), 3);
        assert!(!pool.is_empty());

        let idx0 = pool.try_claim().expect("claim 1 succeeds");
        let idx1 = pool.try_claim().expect("claim 2 succeeds");
        let idx2 = pool.try_claim().expect("claim 3 succeeds");
        assert!(pool.try_claim().is_none(), "pool must be exhausted after 3 claims");

        // Write values using get_raw
        unsafe {
            *pool.get_raw(idx0) = 10;
            *pool.get_raw(idx1) = 20;
            *pool.get_raw(idx2) = 30;

            assert_eq!(*pool.get_raw(idx0), 10);
            assert_eq!(*pool.get_raw(idx1), 20);
            assert_eq!(*pool.get_raw(idx2), 30);
        }

        // Unclaim idx1 and re-claim
        unsafe { pool.unclaim(idx1) };
        let re_idx = pool.try_claim().expect("re-claim succeeds");
        assert_eq!(re_idx, idx1);
        unsafe {
            assert_eq!(*pool.get_raw(re_idx), 20, "retains previous value");
        }
        assert!(pool.try_claim().is_none());

        // Clean up remaining claims
        unsafe {
            pool.unclaim(idx0);
            pool.unclaim(idx2);
            pool.unclaim(re_idx);
        }

        // All 3 should be claimable again
        assert!(pool.try_claim().is_some());
        assert!(pool.try_claim().is_some());
        assert!(pool.try_claim().is_some());
        assert!(pool.try_claim().is_none());
    }

    #[test]
    fn test_thin_pool_zero_capacity() {
        let pool = UnsafePool::<i32, TestCfg>::new(0).expect("zero-capacity pool creation");
        assert_eq!(pool.len(), 0);
        assert!(pool.is_empty());
        assert!(pool.try_claim().is_none());
    }

    #[test]
    fn test_thin_pool_reclaim_all() {
        let pool = UnsafePool::<u32, TestCfg>::new(5).expect("pool creation");
        let mut claimed = std::vec::Vec::new();
        for _ in 0..5 {
            claimed.push(pool.try_claim().expect("claim succeeds"));
        }
        assert!(pool.try_claim().is_none());

        // Unclaim in reverse order
        for idx in claimed.into_iter().rev() {
            unsafe { pool.unclaim(idx) };
        }

        // Should be able to claim all 5 again
        for _ in 0..5 {
            assert!(pool.try_claim().is_some());
        }
        assert!(pool.try_claim().is_none());
    }

    #[test]
    fn test_thin_pool_async_claim_immediate() {
        let pool = UnsafePool::<i32, TestCfg>::new(2).expect("pool creation succeeds");
        BoundedExecutor::new(TestExecutor::new(), |s| {
            s.block_on(async {
                let idx0 = pool.claim().await;
                let idx1 = pool.claim().await;
                assert_ne!(idx0, idx1);
                unsafe {
                    *pool.get_raw(idx0) = 100;
                    *pool.get_raw(idx1) = 200;
                    assert_eq!(*pool.get_raw(idx0), 100);
                    assert_eq!(*pool.get_raw(idx1), 200);
                    pool.unclaim(idx0);
                    pool.unclaim(idx1);
                }
            });
        });
    }

    #[test]
    fn test_thin_pool_async_claim_blocks_until_unclaim() {
        let pool = UnsafePool::<i32, TestCfg>::new(1).expect("pool creation succeeds");
        let idx0 = pool.try_claim().expect("claim succeeds");
        assert!(pool.try_claim().is_none());

        BoundedExecutor::new(TestExecutor::new(), |s| {
            let mut handle = s.spawn(async { pool.claim().await });

            s.run_until_stalled();
            assert!(!handle.is_finished(), "claim task must stall when pool is empty");

            // Unclaim idx0; this should wake the waiting task.
            unsafe { pool.unclaim(idx0) };

            s.run_until_stalled();
            assert!(handle.is_finished(), "claim task must finish after unclaim");
            let claimed_idx = handle.get().expect("slot claimed");
            assert_eq!(claimed_idx, idx0);

            unsafe { pool.unclaim(claimed_idx) };
        });
    }

    #[test]
    fn test_thin_pool_async_multiple_waiters() {
        let pool = UnsafePool::<usize, TestCfg>::new(1).expect("pool creation succeeds");
        let initial_idx = pool.try_claim().expect("initial claim");

        BoundedExecutor::new(TestExecutor::new(), |s| {
            let mut h1 = s.spawn(async { pool.claim().await });
            let mut h2 = s.spawn(async { pool.claim().await });

            s.run_until_stalled();
            assert!(!h1.is_finished());
            assert!(!h2.is_finished());

            // Unclaim initial slot -> first waiter wakes
            unsafe { pool.unclaim(initial_idx) };
            s.run_until_stalled();
            assert!(h1.is_finished());
            assert!(!h2.is_finished());
            let idx1 = h1.get().unwrap();

            // Unclaim idx1 -> second waiter wakes
            unsafe { pool.unclaim(idx1) };
            s.run_until_stalled();
            assert!(h2.is_finished());
            let idx2 = h2.get().unwrap();

            unsafe { pool.unclaim(idx2) };
        });
    }

    #[test]
    fn test_thin_pool_async_claim_cancellation() {
        use futures::future::FutureExt;
        let pool = UnsafePool::<i32, TestCfg>::new(1).expect("pool creation succeeds");
        let idx = pool.try_claim().expect("claim succeeds");
        let cancel_notif = crate::notification::Notification::<SingleThreadMutex>::new();

        BoundedExecutor::new(TestExecutor::new(), |s| {
            let mut h1 = s.spawn(async {
                let claim_fut = pool.claim().fuse();
                let cancel_fut = cancel_notif.wait().fuse();
                futures::pin_mut!(claim_fut);
                futures::pin_mut!(cancel_fut);
                futures::select_biased! {
                    _ = cancel_fut => None,
                    slot = claim_fut => Some(slot),
                }
            });

            let mut h2 = s.spawn(async { pool.claim().await });

            s.run_until_stalled();
            assert!(!h1.is_finished());
            assert!(!h2.is_finished());

            // Cancel h1 and unclaim idx
            cancel_notif.notify_one();
            unsafe { pool.unclaim(idx) };

            s.run_until_stalled();
            assert!(h1.is_finished());
            assert_eq!(h1.get().unwrap(), None);

            // h2 should receive the unclaimed slot despite h1 being cancelled
            assert!(h2.is_finished());
            let claimed = h2.get().unwrap();
            assert_eq!(claimed, idx);

            unsafe { pool.unclaim(claimed) };
        });
    }
}
