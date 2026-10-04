// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Mutex-backed atomic implementations.
//!
//! This module provides [`LockedAtomics`] and [`LockedAtomic`], which emulate atomic
//! operations using a mutual exclusion primitive implementing [`RawMutex`].
//!
//! This generalizes mutex-based atomic simulation to any mutex kind:
//! - With [`SingleThreadMutex`](crate::mutex::raw::SingleThreadMutex), provides zero-overhead
//!   atomic emulation for single-threaded or thread-local contexts.
//! - With [`StdMutex`](crate::mutex::raw::StdMutex) or [`SpinMutex`](crate::mutex::raw::SpinMutex),
//!   provides thread-safe atomic emulation for multi-threaded contexts.
//!
//! # Examples
//!
//! Single-threaded atomics using [`SingleThreadMutex`](crate::mutex::raw::SingleThreadMutex):
//!
//! ```
//! use core::sync::atomic::Ordering;
//! use sapphire_sync::atomic::locked::LockedAtomic;
//! use sapphire_sync::atomic::{AtomicAdd, AtomicLoad};
//! use sapphire_sync::mutex::raw::SingleThreadMutex;
//!
//! let atomic = LockedAtomic::<SingleThreadMutex, usize>::new(10);
//! assert_eq!(atomic.fetch_add(5, Ordering::SeqCst), 10);
//! assert_eq!(atomic.load(Ordering::SeqCst), 15);
//! ```
//!
//! Multi-threaded atomics using [`StdMutex`](crate::mutex::raw::StdMutex):
//!
//! ```
//! use core::sync::atomic::Ordering;
//! use sapphire_sync::atomic::locked::LockedAtomic;
//! use sapphire_sync::atomic::{AtomicAdd, AtomicLoad};
//! use sapphire_sync::mutex::raw::StdMutex;
//! use std::sync::Arc;
//! use std::thread;
//!
//! let atomic = Arc::new(LockedAtomic::<StdMutex, usize>::new(0));
//! let a1 = Arc::clone(&atomic);
//! let t = thread::spawn(move || {
//!     a1.fetch_add(42, Ordering::SeqCst);
//! });
//! t.join().unwrap();
//! assert_eq!(atomic.load(Ordering::SeqCst), 42);
//! ```

use core::cmp::PartialEq;
use core::marker::PhantomData;
use core::ops;
use core::sync::atomic::Ordering;

use crate::atomic::{
    AtomicAdd, AtomicAnd, AtomicCompareExchange, AtomicFalse, AtomicFamily, AtomicLoad, AtomicNand,
    AtomicNew, AtomicNot, AtomicOr, AtomicStore, AtomicSub, AtomicWraps, AtomicXor, AtomicZero,
};
use crate::mutex::Mutex;
use crate::mutex::raw::{ConstInit, RawMutex};

/// Integer arithmetic that wraps around at the boundary of the type.
///
/// Used by [`LockedAtomic`] to give [`AtomicAdd`] and [`AtomicSub`] the same wrapping semantics
/// as the [`core::sync::atomic`] types.
pub trait WrappingOps: Sized {
    /// Returns `self + rhs`, wrapping around on overflow.
    fn wrapping_add(self, rhs: Self) -> Self;
    /// Returns `self - rhs`, wrapping around on underflow.
    fn wrapping_sub(self, rhs: Self) -> Self;
}

/// An [`AtomicFamily`] backed by a mutex of kind `Mtx`.
///
/// Implements [`AtomicFamily<T>`] for any type `T` by yielding a [`LockedAtomic<Mtx, T>`].
pub struct LockedAtomics<Mtx>(PhantomData<Mtx>);
impl<T, Mtx: RawMutex> AtomicFamily<T> for LockedAtomics<Mtx> {
    type Atomic = LockedAtomic<Mtx, T>;
}

/// An atomic wrapper protecting data of type `T` via a [`Mutex<Mtx, T>`].
///
/// Implements atomic traits such as [`AtomicLoad`], [`AtomicStore`], [`AtomicAdd`],
/// [`AtomicSub`], [`AtomicCompareExchange`], and bitwise traits by synchronizing access
/// through the underlying mutex.
pub struct LockedAtomic<Mtx, T> {
    payload: Mutex<Mtx, T>,
}

impl<Mtx: RawMutex, T> LockedAtomic<Mtx, T> {
    /// Constructs a LockedAtomic with an initial value of `payload`
    pub fn new(payload: T) -> Self {
        Self { payload: Mutex::new(payload) }
    }

    /// Constructs a LockedAtomic with an initial value of `payload` in a const context.
    pub const fn const_new(payload: T) -> Self
    where
        Mtx: ConstInit,
    {
        Self { payload: Mutex::const_new(payload) }
    }

    /// Performs a fetch-update operation on the underlying data
    pub fn fetch_do<F: FnOnce(T) -> Option<T>>(&self, f: F) -> Result<T, T>
    where
        T: Clone,
    {
        let mut guard = self.payload.lock();
        match f(guard.clone()) {
            Some(next) => Ok(core::mem::replace(&mut *guard, next)),
            None => Err(guard.clone()),
        }
    }

    /// Replaces the underlying data with `f(prev)`, returning `prev`.
    fn fetch_modify(&self, f: impl FnOnce(T) -> T) -> T
    where
        T: Clone,
    {
        let mut guard = self.payload.lock();
        let next = f(guard.clone());
        core::mem::replace(&mut *guard, next)
    }
}

macro_rules! impl_for_integer {
    ($($primitive_type:ty),*) => {$(
        impl WrappingOps for $primitive_type {
            fn wrapping_add(self, rhs: Self) -> Self {
                <$primitive_type>::wrapping_add(self, rhs)
            }
            fn wrapping_sub(self, rhs: Self) -> Self {
                <$primitive_type>::wrapping_sub(self, rhs)
            }
        }

        impl<Mtx: RawMutex + ConstInit> AtomicZero for LockedAtomic<Mtx, $primitive_type> {
            const ZERO: Self = Self::const_new(0);
        }
    )*};
}
impl_for_integer!(usize, isize, u8, u16, u32, u64, i8, i16, i32, i64);

impl<Mtx: RawMutex + ConstInit> AtomicFalse for LockedAtomic<Mtx, bool> {
    const FALSE: Self = Self::const_new(false);
    const TRUE: Self = Self::const_new(true);
}

impl<Mtx: RawMutex, T> AtomicWraps for LockedAtomic<Mtx, T> {
    type Item = T;

    fn get_mut(&mut self) -> &mut Self::Item {
        self.payload.get_mut()
    }

    fn as_ptr(&self) -> *mut Self::Item {
        self.payload.as_ptr()
    }
}

impl<Mtx: RawMutex, T> AtomicNew for LockedAtomic<Mtx, T> {
    fn new(val: T) -> Self {
        Self { payload: Mutex::new(val) }
    }
}
impl<Mtx: RawMutex, T> AtomicStore for LockedAtomic<Mtx, T> {
    fn store(&self, val: T, _order: Ordering) {
        *self.payload.lock() = val;
    }
}
impl<Mtx: RawMutex, T: Clone> AtomicLoad for LockedAtomic<Mtx, T> {
    fn load(&self, _order: Ordering) -> T {
        self.payload.lock().clone()
    }
}

impl<Mtx: RawMutex, T: WrappingOps + Clone> AtomicAdd for LockedAtomic<Mtx, T> {
    fn fetch_add(&self, val: T, _order: Ordering) -> T {
        self.fetch_modify(|prev| prev.wrapping_add(val))
    }
}
impl<Mtx: RawMutex, T: WrappingOps + Clone> AtomicSub for LockedAtomic<Mtx, T> {
    fn fetch_sub(&self, val: T, _order: Ordering) -> T {
        self.fetch_modify(|prev| prev.wrapping_sub(val))
    }
}

impl<Mtx: RawMutex, T: Clone + PartialEq> AtomicCompareExchange for LockedAtomic<Mtx, T> {
    fn compare_exchange(
        &self,
        current: T,
        new: T,
        _success: Ordering,
        _failure: Ordering,
    ) -> Result<T, T> {
        self.fetch_do(|prev| if prev == current { Some(new) } else { None })
    }

    fn fetch_update<F: FnMut(Self::Item) -> Option<Self::Item>>(
        &self,
        _set_order: Ordering,
        _fetch_order: Ordering,
        f: F,
    ) -> Result<Self::Item, Self::Item>
    where
        Self::Item: Clone,
    {
        self.fetch_do(f)
    }
}

impl<Mtx: RawMutex, T> AtomicAnd for LockedAtomic<Mtx, T>
where
    T: Clone + ops::BitAnd<Output = T>,
{
    fn fetch_and(&self, val: Self::Item, _order: Ordering) -> Self::Item {
        self.fetch_modify(|prev| prev & val)
    }
}
impl<Mtx: RawMutex, T> AtomicNand for LockedAtomic<Mtx, T>
where
    T: Clone + ops::BitAnd<Output = T> + ops::Not<Output = T>,
{
    fn fetch_nand(&self, val: Self::Item, _order: Ordering) -> Self::Item {
        self.fetch_modify(|prev| !(prev & val))
    }
}
impl<Mtx: RawMutex, T> AtomicOr for LockedAtomic<Mtx, T>
where
    T: Clone + ops::BitOr<Output = T>,
{
    fn fetch_or(&self, val: Self::Item, _order: Ordering) -> Self::Item {
        self.fetch_modify(|prev| prev | val)
    }
}
impl<Mtx: RawMutex, T> AtomicXor for LockedAtomic<Mtx, T>
where
    T: Clone + ops::BitXor<Output = T>,
{
    fn fetch_xor(&self, val: Self::Item, _order: Ordering) -> Self::Item {
        self.fetch_modify(|prev| prev ^ val)
    }
}
impl<Mtx: RawMutex, T> AtomicNot for LockedAtomic<Mtx, T>
where
    T: Clone + ops::Not<Output = T>,
{
    fn fetch_not(&self, _order: Ordering) -> Self::Item {
        self.fetch_modify(|prev| !prev)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atomic::AtomicOf;
    #[cfg(feature = "std")]
    use crate::mutex::raw::StdMutex;
    use crate::mutex::raw::{SingleThreadMutex, SpinMutex};
    use std::thread;

    #[test]
    fn test_locked_atomic_single_thread_basic() {
        let atomic = LockedAtomic::<SingleThreadMutex, usize>::new(42);
        assert_eq!(atomic.load(Ordering::Relaxed), 42);

        atomic.store(100, Ordering::Relaxed);
        assert_eq!(atomic.load(Ordering::Relaxed), 100);

        assert_eq!(atomic.fetch_add(5, Ordering::Relaxed), 100);
        assert_eq!(atomic.load(Ordering::Relaxed), 105);

        assert_eq!(atomic.fetch_sub(10, Ordering::Relaxed), 105);
        assert_eq!(atomic.load(Ordering::Relaxed), 95);

        assert_eq!(atomic.fetch_do(|v| Some(v * 2)), Ok(95));
        assert_eq!(atomic.fetch_do(|_| None), Err(190));
        assert_eq!(atomic.load(Ordering::Relaxed), 190);
    }

    #[test]
    fn test_locked_atomic_compare_exchange() {
        let atomic = LockedAtomic::<SingleThreadMutex, u32>::new(10);

        // compare_exchange success
        assert_eq!(atomic.compare_exchange(10, 20, Ordering::SeqCst, Ordering::Relaxed), Ok(10));
        assert_eq!(atomic.load(Ordering::Relaxed), 20);

        // compare_exchange failure
        assert_eq!(atomic.compare_exchange(10, 30, Ordering::SeqCst, Ordering::Relaxed), Err(20));
        assert_eq!(atomic.load(Ordering::Relaxed), 20);

        // compare_exchange_weak success & failure
        assert_eq!(
            atomic.compare_exchange_weak(20, 25, Ordering::SeqCst, Ordering::Relaxed),
            Ok(20)
        );
        assert_eq!(atomic.load(Ordering::Relaxed), 25);
        assert_eq!(
            atomic.compare_exchange_weak(20, 35, Ordering::SeqCst, Ordering::Relaxed),
            Err(25)
        );
        assert_eq!(atomic.load(Ordering::Relaxed), 25);

        // fetch_update success
        let res = atomic.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some(v + 5));
        assert_eq!(res, Ok(25));
        assert_eq!(atomic.load(Ordering::Relaxed), 30);

        // fetch_update failure (None returned)
        let res = atomic.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |_| None);
        assert_eq!(res, Err(30));
        assert_eq!(atomic.load(Ordering::Relaxed), 30);
    }

    #[test]
    fn test_locked_atomic_bitwise_ops() {
        let atomic = LockedAtomic::<SingleThreadMutex, u8>::new(0b1100);

        assert_eq!(atomic.fetch_and(0b1010, Ordering::Relaxed), 0b1100);
        assert_eq!(atomic.load(Ordering::Relaxed), 0b1000);

        assert_eq!(atomic.fetch_or(0b0011, Ordering::Relaxed), 0b1000);
        assert_eq!(atomic.load(Ordering::Relaxed), 0b1011);

        assert_eq!(atomic.fetch_xor(0b0110, Ordering::Relaxed), 0b1011);
        assert_eq!(atomic.load(Ordering::Relaxed), 0b1101);

        assert_eq!(atomic.fetch_nand(0b1111, Ordering::Relaxed), 0b1101);
        assert_eq!(atomic.load(Ordering::Relaxed), !0b1101);

        assert_eq!(atomic.fetch_not(Ordering::Relaxed), !0b1101);
        assert_eq!(atomic.load(Ordering::Relaxed), 0b1101);
    }

    #[test]
    fn test_locked_atomic_bool_ops() {
        let atomic = LockedAtomic::<SingleThreadMutex, bool>::new(false);

        assert_eq!(atomic.load(Ordering::Relaxed), false);
        atomic.store(true, Ordering::Relaxed);
        assert_eq!(atomic.load(Ordering::Relaxed), true);

        // Bitwise ops on bool
        assert_eq!(atomic.fetch_and(false, Ordering::Relaxed), true);
        assert_eq!(atomic.load(Ordering::Relaxed), false);

        assert_eq!(atomic.fetch_or(true, Ordering::Relaxed), false);
        assert_eq!(atomic.load(Ordering::Relaxed), true);

        assert_eq!(atomic.fetch_xor(true, Ordering::Relaxed), true);
        assert_eq!(atomic.load(Ordering::Relaxed), false);

        assert_eq!(atomic.fetch_not(Ordering::Relaxed), false);
        assert_eq!(atomic.load(Ordering::Relaxed), true);

        assert_eq!(atomic.fetch_nand(true, Ordering::Relaxed), true);
        assert_eq!(atomic.load(Ordering::Relaxed), false);
        assert_eq!(atomic.fetch_nand(true, Ordering::Relaxed), false);
        assert_eq!(atomic.load(Ordering::Relaxed), true);
    }

    #[test]
    fn test_locked_atomic_add_sub_wrap() {
        let atomic = LockedAtomic::<SingleThreadMutex, usize>::new(usize::MAX);
        assert_eq!(atomic.fetch_add(1, Ordering::Relaxed), usize::MAX);
        assert_eq!(atomic.load(Ordering::Relaxed), 0);
        assert_eq!(atomic.fetch_sub(1, Ordering::Relaxed), 0);
        assert_eq!(atomic.load(Ordering::Relaxed), usize::MAX);

        let atomic = LockedAtomic::<SingleThreadMutex, i8>::new(i8::MIN);
        assert_eq!(atomic.fetch_sub(1, Ordering::Relaxed), i8::MIN);
        assert_eq!(atomic.load(Ordering::Relaxed), i8::MAX);
    }

    #[test]
    fn test_locked_atomic_clone_payload() {
        let atomic = LockedAtomic::<SingleThreadMutex, std::string::String>::new("a".into());
        assert_eq!(atomic.load(Ordering::Relaxed), "a");
        assert_eq!(
            atomic.compare_exchange("a".into(), "b".into(), Ordering::SeqCst, Ordering::Relaxed),
            Ok("a".into())
        );
        assert_eq!(
            atomic.compare_exchange("a".into(), "c".into(), Ordering::SeqCst, Ordering::Relaxed),
            Err("b".into())
        );
        assert_eq!(atomic.load(Ordering::Relaxed), "b");
    }

    #[test]
    fn test_locked_atomic_const_init() {
        static COUNTER: LockedAtomic<SpinMutex, usize> = AtomicZero::ZERO;
        static FLAG: LockedAtomic<SpinMutex, bool> = AtomicFalse::FALSE;
        let set: LockedAtomic<SingleThreadMutex, bool> = AtomicFalse::TRUE;

        assert_eq!(COUNTER.fetch_add(1, Ordering::Relaxed), 0);
        assert_eq!(COUNTER.load(Ordering::Relaxed), 1);
        assert_eq!(FLAG.fetch_not(Ordering::Relaxed), false);
        assert_eq!(FLAG.load(Ordering::Relaxed), true);
        assert_eq!(set.load(Ordering::Relaxed), true);
    }

    #[test]
    fn test_locked_atomic_wraps() {
        let mut atomic = LockedAtomic::<SingleThreadMutex, i32>::new(50);
        *atomic.get_mut() += 10;
        assert_eq!(atomic.load(Ordering::Relaxed), 60);

        let ptr = atomic.as_ptr();
        // SAFETY: `ptr` points to the payload of `atomic`, which is alive and not locked or
        // otherwise borrowed for the duration of the write.
        unsafe {
            *ptr = 70;
        }
        assert_eq!(atomic.load(Ordering::Relaxed), 70);
    }

    #[test]
    #[should_panic(expected = "Attempting to lock single-thread mutex twice is a deadlock")]
    fn test_single_thread_reentrancy_panic() {
        let atomic = LockedAtomic::<SingleThreadMutex, usize>::new(10);
        atomic.fetch_do(|_| Some(atomic.load(Ordering::SeqCst)));
    }

    #[test]
    fn test_locked_atomics_family() {
        type STAtomics = LockedAtomics<SingleThreadMutex>;
        let atomic: AtomicOf<STAtomics, usize> = AtomicNew::new(123);
        assert_eq!(atomic.load(Ordering::Relaxed), 123);
        atomic.store(456, Ordering::Relaxed);
        assert_eq!(atomic.load(Ordering::Relaxed), 456);

        type SpinAtomics = LockedAtomics<SpinMutex>;
        let atomic_spin: AtomicOf<SpinAtomics, i32> = AtomicNew::new(-10);
        assert_eq!(atomic_spin.fetch_add(20, Ordering::Relaxed), -10);
        assert_eq!(atomic_spin.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn test_spin_mutex_atomic_multithreaded() {
        let atomic = LockedAtomic::<SpinMutex, usize>::new(0);

        thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..100 {
                        atomic.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });

        assert_eq!(atomic.load(Ordering::Relaxed), 800);
    }

    #[cfg(feature = "std")]
    #[test]
    fn test_std_mutex_atomic_multithreaded() {
        let atomic = LockedAtomic::<StdMutex, usize>::new(0);

        thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..100 {
                        atomic.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }
        });

        assert_eq!(atomic.load(Ordering::SeqCst), 800);

        // Test concurrent compare_exchange_weak / fetch_update
        let shared = LockedAtomic::<StdMutex, usize>::new(0);
        thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..50 {
                        let _ = shared
                            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some(v + 1));
                    }
                });
            }
        });

        assert_eq!(shared.load(Ordering::SeqCst), 200);
    }

    /// Differential tests that apply the same random sequence of operations to a [`LockedAtomic`]
    /// and to the equivalent `core::sync::atomic` type, and check that they agree at every step.
    mod proptests {
        use super::*;
        use crate::atomic::{AtomicBool, AtomicNum};
        use core::sync::atomic::{AtomicBool as CoreAtomicBool, AtomicU8};
        use proptest::prelude::*;

        #[derive(Debug, Clone)]
        enum NumOp {
            Load,
            Store(u8),
            FetchAdd(u8),
            FetchSub(u8),
            FetchAnd(u8),
            FetchNand(u8),
            FetchOr(u8),
            FetchXor(u8),
            /// `current: None` uses the actual current value so that the exchange succeeds.
            CompareExchange {
                current: Option<u8>,
                new: u8,
            },
            /// Adds `add` if the current value is at most `limit`, otherwise aborts the update.
            FetchUpdate {
                add: u8,
                limit: u8,
            },
        }

        fn num_op() -> impl Strategy<Value = NumOp> {
            prop_oneof![
                Just(NumOp::Load),
                any::<u8>().prop_map(NumOp::Store),
                any::<u8>().prop_map(NumOp::FetchAdd),
                any::<u8>().prop_map(NumOp::FetchSub),
                any::<u8>().prop_map(NumOp::FetchAnd),
                any::<u8>().prop_map(NumOp::FetchNand),
                any::<u8>().prop_map(NumOp::FetchOr),
                any::<u8>().prop_map(NumOp::FetchXor),
                (any::<Option<u8>>(), any::<u8>())
                    .prop_map(|(current, new)| NumOp::CompareExchange { current, new }),
                (any::<u8>(), any::<u8>())
                    .prop_map(|(add, limit)| NumOp::FetchUpdate { add, limit }),
            ]
        }

        /// Applies `op` to `atomic`, returning the operation's result, if it has one.
        fn apply_num<A: AtomicNum<Item = u8>>(atomic: &A, op: &NumOp) -> Option<Result<u8, u8>> {
            let o = Ordering::SeqCst;
            match *op {
                NumOp::Load => Some(Ok(atomic.load(o))),
                NumOp::Store(val) => {
                    atomic.store(val, o);
                    None
                }
                NumOp::FetchAdd(val) => Some(Ok(atomic.fetch_add(val, o))),
                NumOp::FetchSub(val) => Some(Ok(atomic.fetch_sub(val, o))),
                NumOp::FetchAnd(val) => Some(Ok(atomic.fetch_and(val, o))),
                NumOp::FetchNand(val) => Some(Ok(atomic.fetch_nand(val, o))),
                NumOp::FetchOr(val) => Some(Ok(atomic.fetch_or(val, o))),
                NumOp::FetchXor(val) => Some(Ok(atomic.fetch_xor(val, o))),
                NumOp::CompareExchange { current, new } => {
                    let current = current.unwrap_or_else(|| atomic.load(o));
                    Some(atomic.compare_exchange(current, new, o, o))
                }
                NumOp::FetchUpdate { add, limit } => {
                    Some(atomic.fetch_update(o, o, |v| (v <= limit).then(|| v.wrapping_add(add))))
                }
            }
        }

        #[derive(Debug, Clone)]
        enum BoolOp {
            Load,
            Store(bool),
            FetchAnd(bool),
            FetchNand(bool),
            FetchOr(bool),
            FetchXor(bool),
            FetchNot,
            CompareExchange {
                current: bool,
                new: bool,
            },
            /// Sets the value to `next`, or aborts the update if `None`.
            FetchUpdate(Option<bool>),
        }

        fn bool_op() -> impl Strategy<Value = BoolOp> {
            prop_oneof![
                Just(BoolOp::Load),
                any::<bool>().prop_map(BoolOp::Store),
                any::<bool>().prop_map(BoolOp::FetchAnd),
                any::<bool>().prop_map(BoolOp::FetchNand),
                any::<bool>().prop_map(BoolOp::FetchOr),
                any::<bool>().prop_map(BoolOp::FetchXor),
                Just(BoolOp::FetchNot),
                (any::<bool>(), any::<bool>())
                    .prop_map(|(current, new)| BoolOp::CompareExchange { current, new }),
                any::<Option<bool>>().prop_map(BoolOp::FetchUpdate),
            ]
        }

        /// Applies `op` to `atomic`, returning the operation's result, if it has one.
        fn apply_bool<A: AtomicBool>(atomic: &A, op: &BoolOp) -> Option<Result<bool, bool>> {
            let o = Ordering::SeqCst;
            match *op {
                BoolOp::Load => Some(Ok(atomic.load(o))),
                BoolOp::Store(val) => {
                    atomic.store(val, o);
                    None
                }
                BoolOp::FetchAnd(val) => Some(Ok(atomic.fetch_and(val, o))),
                BoolOp::FetchNand(val) => Some(Ok(atomic.fetch_nand(val, o))),
                BoolOp::FetchOr(val) => Some(Ok(atomic.fetch_or(val, o))),
                BoolOp::FetchXor(val) => Some(Ok(atomic.fetch_xor(val, o))),
                BoolOp::FetchNot => Some(Ok(atomic.fetch_not(o))),
                BoolOp::CompareExchange { current, new } => {
                    Some(atomic.compare_exchange(current, new, o, o))
                }
                BoolOp::FetchUpdate(next) => Some(atomic.fetch_update(o, o, |_| next)),
            }
        }

        proptest! {
            #[test]
            fn test_locked_atomic_matches_core_atomic_u8(
                initial in any::<u8>(),
                ops in prop::collection::vec(num_op(), 0..100),
            ) {
                let locked = LockedAtomic::<SingleThreadMutex, u8>::new(initial);
                let reference = AtomicU8::new(initial);
                for op in &ops {
                    prop_assert_eq!(apply_num(&locked, op), apply_num(&reference, op), "{:?}", op);
                    prop_assert_eq!(
                        locked.load(Ordering::SeqCst),
                        reference.load(Ordering::SeqCst),
                        "{:?}",
                        op
                    );
                }
            }

            #[test]
            fn test_locked_atomic_matches_core_atomic_bool(
                initial in any::<bool>(),
                ops in prop::collection::vec(bool_op(), 0..100),
            ) {
                let locked = LockedAtomic::<SingleThreadMutex, bool>::new(initial);
                let reference = CoreAtomicBool::new(initial);
                for op in &ops {
                    prop_assert_eq!(
                        apply_bool(&locked, op),
                        apply_bool(&reference, op),
                        "{:?}",
                        op
                    );
                    prop_assert_eq!(
                        locked.load(Ordering::SeqCst),
                        reference.load(Ordering::SeqCst),
                        "{:?}",
                        op
                    );
                }
            }
        }
    }
}
