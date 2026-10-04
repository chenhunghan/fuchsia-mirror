// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//! Generic atomic abstractions and implementations.
//!
//! This module defines traits for atomic operations, abstracting over both
//! standard hardware atomics ([`StdAtomics`]) and mutex-emulated atomics ([`LockedAtomics`]).
//!
//! # Atomic Families
//!
//! An [`AtomicFamily`] maps a primitive or payload type `T` to a concrete atomic type
//! via the [`AtomicOf<F, T>`] associated type helper:
//! - [`StdAtomics`]: Backed by the standard library's [`core::sync::atomic`] types for scalar
//!   numeric types and booleans.
//! - [`LockedAtomics`]: Backed by [`LockedAtomic`], wrapping any [`RawMutex`](crate::mutex::raw::RawMutex).
//!   For single-threaded contexts, [`SingleThreadAtomics`] provides zero-cost atomic emulation.

use core::sync::atomic::Ordering;

pub mod locked;
pub use locked::{LockedAtomic, LockedAtomics};

/// Type alias for single-threaded atomics backed by [`SingleThreadMutex`](crate::mutex::raw::SingleThreadMutex).
pub type SingleThreadAtomics = LockedAtomics<crate::mutex::raw::SingleThreadMutex>;

pub trait AtomicFamily<T> {
    type Atomic: AtomicWraps<Item = T>;
}

pub struct StdAtomics;
pub type AtomicOf<F, T> = <F as AtomicFamily<T>>::Atomic;

pub trait AtomicWraps {
    type Item;

    fn get_mut(&mut self) -> &mut Self::Item;
    fn as_ptr(&self) -> *mut Self::Item;
}

pub trait AtomicAdd: AtomicWraps {
    /// Adds `val` to the current value, returning the previous value.
    ///
    /// Behaves the same as [`core::sync::atomic::AtomicUsize::fetch_add()`].  Notably
    /// this means that it has wrapping semantics.
    fn fetch_add(&self, val: Self::Item, order: Ordering) -> Self::Item;
}

pub trait AtomicSub: AtomicWraps {
    /// Subtracts `val` from the current value, returning the previous value.
    ///
    /// Behaves the same as [`core::sync::atomic::AtomicUsize::fetch_sub()`].  Notably
    /// this means that it has wrapping semantics.
    fn fetch_sub(&self, val: Self::Item, order: Ordering) -> Self::Item;
}

pub trait AtomicOr: AtomicWraps {
    fn fetch_or(&self, val: Self::Item, order: Ordering) -> Self::Item;
}

pub trait AtomicAnd: AtomicWraps {
    fn fetch_and(&self, val: Self::Item, order: Ordering) -> Self::Item;
}

pub trait AtomicNand: AtomicWraps {
    fn fetch_nand(&self, val: Self::Item, order: Ordering) -> Self::Item;
}

pub trait AtomicXor: AtomicWraps {
    fn fetch_xor(&self, val: Self::Item, order: Ordering) -> Self::Item;
}

pub trait AtomicNot: AtomicWraps {
    fn fetch_not(&self, order: Ordering) -> Self::Item;
}

pub trait AtomicLoad: AtomicWraps {
    /// Loads and returns the current value.
    ///
    /// Behaves the same as [`core::sync::atomic::AtomicUsize::load()`].
    fn load(&self, order: Ordering) -> Self::Item;
}

pub trait AtomicStore: AtomicWraps {
    /// Stores a value in the atomic.
    ///
    /// Behaves the same as [`core::sync::atomic::AtomicUsize::store()`].
    fn store(&self, val: Self::Item, order: Ordering);
}

pub trait AtomicCompareExchange: AtomicLoad + AtomicStore {
    /// Stores a value into the atomic if the current value is the same as the `current` value.
    ///
    /// Behaves the same as [`core::sync::atomic::AtomicUsize::compare_exchange()`].
    fn compare_exchange(
        &self,
        current: Self::Item,
        new: Self::Item,
        success: Ordering,
        failure: Ordering,
    ) -> Result<Self::Item, Self::Item>;

    fn compare_exchange_weak(
        &self,
        current: Self::Item,
        new: Self::Item,
        success: Ordering,
        failure: Ordering,
    ) -> Result<Self::Item, Self::Item> {
        self.compare_exchange(current, new, success, failure)
    }

    fn fetch_update<F: FnMut(Self::Item) -> Option<Self::Item>>(
        &self,
        set_order: Ordering,
        fetch_order: Ordering,
        mut f: F,
    ) -> Result<Self::Item, Self::Item>
    where
        Self::Item: Clone,
    {
        let mut prev = self.load(fetch_order);
        while let Some(next) = f(prev.clone()) {
            match self.compare_exchange_weak(prev, next, set_order, fetch_order) {
                x @ Ok(_) => return x,
                Err(next_prev) => prev = next_prev,
            }
        }
        Err(prev)
    }
}

pub trait AtomicNew: AtomicWraps {
    /// Returns a new atomic with `val`
    fn new(val: Self::Item) -> Self;
}

/// An atomic that can be constructed with a value of zero in a const context.
pub trait AtomicZero: AtomicWraps {
    /// An atomic holding zero.
    const ZERO: Self;
}

/// An atomic boolean that can be constructed in a const context.
pub trait AtomicFalse: AtomicWraps<Item = bool> {
    /// An atomic holding `false`.
    const FALSE: Self;
    /// An atomic holding `true`.
    const TRUE: Self;
}

pub trait AtomicBool:
    AtomicMinimal<Item = bool> + AtomicAnd + AtomicNand + AtomicNot + AtomicOr + AtomicXor
{
}
impl<T> AtomicBool for T where
    T: AtomicMinimal<Item = bool> + AtomicAnd + AtomicNand + AtomicNot + AtomicOr + AtomicXor
{
}

pub trait AtomicMinimal: AtomicNew + AtomicLoad + AtomicStore + AtomicCompareExchange {}
impl<T> AtomicMinimal for T where T: AtomicNew + AtomicLoad + AtomicStore + AtomicCompareExchange {}

pub trait AtomicNum:
    AtomicMinimal + AtomicAdd + AtomicSub + AtomicAnd + AtomicNand + AtomicOr + AtomicXor
{
}

impl<T> AtomicNum for T where
    T: AtomicMinimal + AtomicAdd + AtomicSub + AtomicAnd + AtomicNand + AtomicOr + AtomicXor
{
}

mod builtin_impls {
    use super::*;
    macro_rules! impl_for_scalar {
        ($atomic_type:ty, $primitive_type:ty) => {
            impl AtomicWraps for $atomic_type {
                type Item = $primitive_type;

                fn get_mut(&mut self) -> &mut Self::Item {
                    self.get_mut()
                }

                fn as_ptr(&self) -> *mut Self::Item {
                    self.as_ptr()
                }
            }

            impl AtomicLoad for $atomic_type {
                fn load(&self, ordering: Ordering) -> $primitive_type {
                    self.load(ordering)
                }
            }

            impl AtomicStore for $atomic_type {
                fn store(&self, val: $primitive_type, ordering: Ordering) {
                    self.store(val, ordering)
                }
            }

            impl AtomicCompareExchange for $atomic_type {
                fn compare_exchange(
                    &self,
                    current: $primitive_type,
                    new: $primitive_type,
                    success: Ordering,
                    failure: Ordering,
                ) -> Result<$primitive_type, $primitive_type> {
                    self.compare_exchange(current, new, success, failure)
                }

                fn compare_exchange_weak(
                    &self,
                    current: $primitive_type,
                    new: $primitive_type,
                    success: Ordering,
                    failure: Ordering,
                ) -> Result<$primitive_type, $primitive_type> {
                    self.compare_exchange_weak(current, new, success, failure)
                }

                fn fetch_update<F: FnMut($primitive_type) -> Option<$primitive_type>>(
                    &self,
                    set_order: Ordering,
                    fetch_order: Ordering,
                    f: F,
                ) -> Result<$primitive_type, $primitive_type> {
                    self.fetch_update(set_order, fetch_order, f)
                }
            }

            impl AtomicNew for $atomic_type {
                fn new(val: $primitive_type) -> Self {
                    Self::new(val)
                }
            }
            impl AtomicFamily<$primitive_type> for StdAtomics {
                type Atomic = $atomic_type;
            }
        };
    }

    macro_rules! impl_for_numeric {
        ($atomic_type:ty, $primitive_type:ty) => {
            impl_for_scalar!($atomic_type, $primitive_type);

            impl AtomicAdd for $atomic_type {
                fn fetch_add(&self, val: $primitive_type, ordering: Ordering) -> $primitive_type {
                    self.fetch_add(val, ordering)
                }
            }

            impl AtomicSub for $atomic_type {
                fn fetch_sub(&self, val: $primitive_type, ordering: Ordering) -> $primitive_type {
                    self.fetch_sub(val, ordering)
                }
            }

            impl AtomicZero for $atomic_type {
                const ZERO: Self = Self::new(0);
            }

            impl AtomicAnd for $atomic_type {
                fn fetch_and(&self, val: Self::Item, order: Ordering) -> Self::Item {
                    self.fetch_and(val, order)
                }
            }
            impl AtomicNand for $atomic_type {
                fn fetch_nand(&self, val: Self::Item, order: Ordering) -> Self::Item {
                    self.fetch_nand(val, order)
                }
            }
            impl AtomicOr for $atomic_type {
                fn fetch_or(&self, val: Self::Item, order: Ordering) -> Self::Item {
                    self.fetch_or(val, order)
                }
            }
            impl AtomicXor for $atomic_type {
                fn fetch_xor(&self, val: Self::Item, order: Ordering) -> Self::Item {
                    self.fetch_xor(val, order)
                }
            }
        };
    }

    impl AtomicFalse for core::sync::atomic::AtomicBool {
        const FALSE: Self = Self::new(false);
        const TRUE: Self = Self::new(true);
    }
    impl AtomicAnd for core::sync::atomic::AtomicBool {
        fn fetch_and(&self, val: Self::Item, order: Ordering) -> Self::Item {
            self.fetch_and(val, order)
        }
    }
    impl AtomicNand for core::sync::atomic::AtomicBool {
        fn fetch_nand(&self, val: Self::Item, order: Ordering) -> Self::Item {
            self.fetch_nand(val, order)
        }
    }
    impl AtomicNot for core::sync::atomic::AtomicBool {
        fn fetch_not(&self, order: Ordering) -> Self::Item {
            self.fetch_not(order)
        }
    }
    impl AtomicOr for core::sync::atomic::AtomicBool {
        fn fetch_or(&self, val: Self::Item, order: Ordering) -> Self::Item {
            self.fetch_or(val, order)
        }
    }
    impl AtomicXor for core::sync::atomic::AtomicBool {
        fn fetch_xor(&self, val: Self::Item, order: Ordering) -> Self::Item {
            self.fetch_xor(val, order)
        }
    }
    impl_for_scalar!(core::sync::atomic::AtomicBool, bool);

    impl_for_numeric!(core::sync::atomic::AtomicUsize, usize);
    impl_for_numeric!(core::sync::atomic::AtomicIsize, isize);
    impl_for_numeric!(core::sync::atomic::AtomicU8, u8);
    impl_for_numeric!(core::sync::atomic::AtomicU16, u16);
    impl_for_numeric!(core::sync::atomic::AtomicU32, u32);
    impl_for_numeric!(core::sync::atomic::AtomicU64, u64);
    impl_for_numeric!(core::sync::atomic::AtomicI8, i8);
    impl_for_numeric!(core::sync::atomic::AtomicI16, i16);
    impl_for_numeric!(core::sync::atomic::AtomicI32, i32);
    impl_for_numeric!(core::sync::atomic::AtomicI64, i64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutex::raw::SpinMutex;

    type SpinAtomics = LockedAtomics<SpinMutex>;

    /// Exercises the numeric atomic traits. Calling them on a generic type ensures that the trait
    /// methods run rather than the inherent methods of the `core::sync::atomic` types.
    fn check_num<A: AtomicNum<Item = u32> + AtomicZero>() {
        let mut atomic = A::new(10);
        assert_eq!(atomic.load(Ordering::Relaxed), 10);
        atomic.store(20, Ordering::Relaxed);
        assert_eq!(atomic.load(Ordering::Relaxed), 20);
        assert_eq!(atomic.fetch_add(5, Ordering::Relaxed), 20);
        assert_eq!(atomic.fetch_sub(3, Ordering::Relaxed), 25);
        assert_eq!(atomic.load(Ordering::Relaxed), 22);

        assert_eq!(atomic.compare_exchange(22, 30, Ordering::SeqCst, Ordering::Relaxed), Ok(22));
        assert_eq!(atomic.compare_exchange(22, 40, Ordering::SeqCst, Ordering::Relaxed), Err(30));

        // `compare_exchange_weak` may fail spuriously even when the value matches.
        loop {
            match atomic.compare_exchange_weak(30, 35, Ordering::SeqCst, Ordering::Relaxed) {
                Ok(prev) => {
                    assert_eq!(prev, 30);
                    break;
                }
                Err(actual) => assert_eq!(actual, 30),
            }
        }
        assert_eq!(
            atomic.compare_exchange_weak(30, 40, Ordering::SeqCst, Ordering::Relaxed),
            Err(35)
        );

        assert_eq!(
            atomic.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some(v + 5)),
            Ok(35)
        );
        assert_eq!(atomic.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |_| None), Err(40));
        assert_eq!(atomic.load(Ordering::Relaxed), 40);

        atomic.store(0b1100, Ordering::Relaxed);
        assert_eq!(atomic.fetch_and(0b1010, Ordering::Relaxed), 0b1100);
        assert_eq!(atomic.fetch_or(0b0011, Ordering::Relaxed), 0b1000);
        assert_eq!(atomic.fetch_xor(0b0110, Ordering::Relaxed), 0b1011);
        assert_eq!(atomic.fetch_nand(0b1111, Ordering::Relaxed), 0b1101);
        assert_eq!(atomic.load(Ordering::Relaxed), !0b1101);

        atomic.store(u32::MAX, Ordering::Relaxed);
        assert_eq!(atomic.fetch_add(1, Ordering::Relaxed), u32::MAX);
        assert_eq!(atomic.load(Ordering::Relaxed), 0);
        assert_eq!(atomic.fetch_sub(1, Ordering::Relaxed), 0);
        assert_eq!(atomic.load(Ordering::Relaxed), u32::MAX);

        *atomic.get_mut() = 7;
        assert_eq!(atomic.load(Ordering::Relaxed), 7);
        // SAFETY: `atomic` is alive and not accessed by anything else during the read.
        assert_eq!(unsafe { *atomic.as_ptr() }, 7);

        let zero = A::ZERO;
        assert_eq!(zero.load(Ordering::Relaxed), 0);
    }

    /// Exercises the boolean atomic traits. See [`check_num`] for why this is generic.
    fn check_bool<A: AtomicBool + AtomicFalse>() {
        let mut atomic = A::new(false);
        assert_eq!(atomic.load(Ordering::Relaxed), false);
        atomic.store(true, Ordering::Relaxed);
        assert_eq!(atomic.load(Ordering::Relaxed), true);

        assert_eq!(atomic.fetch_and(false, Ordering::Relaxed), true);
        assert_eq!(atomic.fetch_or(true, Ordering::Relaxed), false);
        assert_eq!(atomic.fetch_xor(true, Ordering::Relaxed), true);
        assert_eq!(atomic.fetch_not(Ordering::Relaxed), false);
        assert_eq!(atomic.fetch_nand(true, Ordering::Relaxed), true);
        assert_eq!(atomic.fetch_nand(true, Ordering::Relaxed), false);
        assert_eq!(atomic.load(Ordering::Relaxed), true);

        assert_eq!(
            atomic.compare_exchange(true, false, Ordering::SeqCst, Ordering::Relaxed),
            Ok(true)
        );
        assert_eq!(
            atomic.compare_exchange(true, false, Ordering::SeqCst, Ordering::Relaxed),
            Err(false)
        );
        assert_eq!(
            atomic.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some(!v)),
            Ok(false)
        );
        assert_eq!(atomic.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |_| None), Err(true));

        *atomic.get_mut() = false;
        // SAFETY: `atomic` is alive and not accessed by anything else during the read.
        assert_eq!(unsafe { *atomic.as_ptr() }, false);

        let false_val = A::FALSE;
        assert_eq!(false_val.load(Ordering::Relaxed), false);
        let true_val = A::TRUE;
        assert_eq!(true_val.load(Ordering::Relaxed), true);
    }

    #[test]
    fn test_std_atomics_num() {
        check_num::<AtomicOf<StdAtomics, u32>>();
    }

    #[test]
    fn test_single_thread_atomics_num() {
        check_num::<AtomicOf<SingleThreadAtomics, u32>>();
    }

    #[test]
    fn test_spin_atomics_num() {
        check_num::<AtomicOf<SpinAtomics, u32>>();
    }

    #[test]
    fn test_std_atomics_bool() {
        check_bool::<AtomicOf<StdAtomics, bool>>();
    }

    #[test]
    fn test_single_thread_atomics_bool() {
        check_bool::<AtomicOf<SingleThreadAtomics, bool>>();
    }

    #[test]
    fn test_spin_atomics_bool() {
        check_bool::<AtomicOf<SpinAtomics, bool>>();
    }
}
