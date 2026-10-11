// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::borrow::Borrow;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};

use mmio::MmioError;
use mmio::region::{MmioRegion, UnsafeMmio};

/// A type that can perform atomic operations given an [`AtomicMmioPtr`].
///
/// All atomic operations are performed with [`Ordering::SeqCst`].
pub trait AtomicOperand: sealed::AtomicOperand {
    /// The atomic value type.
    type Atomic;
    /// Returns a reference to the atomic value type from `ptr` at `offset`.
    fn get_atomic<P: AtomicMmioPtr>(ptr: &P, offset: usize) -> &Self::Atomic;
    /// Performs an atomic swap at `offset` within` ptr` with `val`.
    fn swap<P: AtomicMmioPtr>(ptr: &P, offset: usize, val: Self) -> Self;
    /// Performs an atomic fetch_and at `offset` within` ptr` with `val`.
    fn fetch_and<P: AtomicMmioPtr>(ptr: &P, offset: usize, val: Self) -> Self;
    /// Performs an atomic fetch_or at `offset` within` ptr` with `val`.
    fn fetch_or<P: AtomicMmioPtr>(ptr: &P, offset: usize, val: Self) -> Self;
}

// Ordering: We always pick the strongest ordering guarantee so AtomicOperand is
// closest to an atomic volatile backed by device memory.
const ORDERING: Ordering = Ordering::SeqCst;

macro_rules! impl_atomic_operand {
    ($t:ident, $a:ident) => {
        impl AtomicOperand for $t {
            type Atomic = $a;

            fn get_atomic<P: AtomicMmioPtr>(ptr: &P, offset: usize) -> &Self::Atomic {
                let ptr = ptr.ptr::<Self>(offset);
                // SAFETY: AtomicMmioPtr provides the guarantee that ptr is
                // valid for the lifetime of `&P` and is aligned to Self.
                unsafe { $a::from_ptr(ptr.as_ptr()) }
            }

            fn swap<P: AtomicMmioPtr>(ptr: &P, offset: usize, val: Self) -> Self {
                Self::get_atomic(ptr, offset).swap(val, ORDERING)
            }

            fn fetch_and<P: AtomicMmioPtr>(ptr: &P, offset: usize, val: Self) -> Self {
                // Ordering: We always pick the strongest ordering guarantee so
                // this is closest to an atomic volatile backed by device
                // memory.
                Self::get_atomic(ptr, offset).fetch_and(val, ORDERING)
            }

            fn fetch_or<P: AtomicMmioPtr>(ptr: &P, offset: usize, val: Self) -> Self {
                // Ordering: We always pick the strongest ordering guarantee so
                // this is closest to an atomic volatile backed by device
                // memory.
                Self::get_atomic(ptr, offset).fetch_or(val, ORDERING)
            }
        }
    };
}

impl_atomic_operand!(u8, AtomicU8);
impl_atomic_operand!(u16, AtomicU16);
impl_atomic_operand!(u32, AtomicU32);
impl_atomic_operand!(u64, AtomicU64);

mod sealed {
    pub trait AtomicOperand {}

    impl AtomicOperand for u8 {}
    impl AtomicOperand for u16 {}
    impl AtomicOperand for u32 {}
    impl AtomicOperand for u64 {}
}

/// A trait abstracting a type that can provide valid pointers for atomic MMIO
/// operations.
///
/// Notably implemented by [`crate::CachedVmoMemory`].
pub trait AtomicMmioPtr {
    /// Returns a valid pointer to T to perform atomic operations on.
    ///
    /// The returned pointer _must_ be properly aligned for `T` at `offset` and
    /// valid for atomic reads and writes for the lifetime of `Self`.
    ///
    /// # Panics
    ///
    /// Panics if a pointer with the conditions above can't be created at
    /// `offset`.
    fn ptr<T: AtomicOperand>(&self, offset: usize) -> NonNull<T>;
}

/// An extension trait applied to [`MmioRegion`] to support atomic operations.
///
/// All atomic operations are performed with [`Ordering::SeqCst`].
pub trait AtomicMmio {
    /// Atomic swap of `value` at `offset`.
    ///
    /// # Panics
    ///
    /// Panics if `offset` is invalid (unaligned or out of bounds for `T`).
    fn swap<T: AtomicOperand>(&self, offset: usize, value: T) -> T {
        self.try_swap(offset, value).unwrap()
    }

    /// Atomic fetch_and with `value` at `offset`.
    ///
    /// # Panics
    ///
    /// Panics if `offset` is invalid (unaligned or out of bounds for `T`).
    fn fetch_and<T: AtomicOperand>(&self, offset: usize, value: T) -> T {
        self.try_fetch_and(offset, value).unwrap()
    }

    /// Atomic fetch_or with `value` at `offset`.
    ///
    /// # Panics
    ///
    /// Panics if `offset` is invalid (unaligned or out of bounds for `T`).
    fn fetch_or<T: AtomicOperand>(&self, offset: usize, value: T) -> T {
        self.try_fetch_or(offset, value).unwrap()
    }

    /// Try atomic swap of `value` at `offset`.
    ///
    /// Returns an error if `offset` is invalid (unaligned or out of bounds for `T`).
    fn try_swap<T: AtomicOperand>(&self, offset: usize, value: T) -> Result<T, MmioError>;

    /// Try atomic fetch_and with `value` at `offset`.
    ///
    /// Returns an error if `offset` is invalid (unaligned or out of bounds for `T`).
    fn try_fetch_and<T: AtomicOperand>(&self, offset: usize, value: T) -> Result<T, MmioError>;

    /// Try atomic fetch_or with `value` at `offset`.
    ///
    /// Returns an error if `offset` is invalid (unaligned or out of bounds for `T`).
    fn try_fetch_or<T: AtomicOperand>(&self, offset: usize, value: T) -> Result<T, MmioError>;
}

impl<Impl: AtomicMmioPtr + UnsafeMmio, Owner: Borrow<Impl>> AtomicMmio for MmioRegion<Impl, Owner> {
    fn try_swap<T: AtomicOperand>(&self, offset: usize, value: T) -> Result<T, MmioError> {
        let offset = self.resolve_offset::<T>(offset)?;
        Ok(T::swap(self.unsafe_mmio(), offset, value))
    }

    fn try_fetch_and<T: AtomicOperand>(&self, offset: usize, value: T) -> Result<T, MmioError> {
        let offset = self.resolve_offset::<T>(offset)?;
        Ok(T::fetch_and(self.unsafe_mmio(), offset, value))
    }

    fn try_fetch_or<T: AtomicOperand>(&self, offset: usize, value: T) -> Result<T, MmioError> {
        let offset = self.resolve_offset::<T>(offset)?;
        Ok(T::fetch_or(self.unsafe_mmio(), offset, value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CachedVmoMemory;
    use crate::operand::MmioOperand;
    use crate::operand::tests::OperandCallable;
    use mmio::MmioExt as _;

    const VMO_SIZE: usize = 4096;

    #[test]
    fn test_atomic_swap() {
        struct SwapTester;

        impl OperandCallable for SwapTester {
            fn call<T: MmioOperand>(&mut self) {
                let mmio = CachedVmoMemory::new_mapped(VMO_SIZE).expect("Failed to map VMO.");
                let offset = 0;

                assert_eq!(mmio.load::<T>(offset), T::try_from(0).unwrap());

                let val_aa = T::try_from(0xaa).unwrap();
                let old = mmio.swap::<T>(offset, val_aa);
                assert_eq!(old, T::try_from(0).unwrap());
                assert_eq!(mmio.load::<T>(offset), val_aa);
            }
        }

        SwapTester.call_all();
    }

    #[test]
    fn test_atomic_fetch_and() {
        struct FetchAndTester;

        impl OperandCallable for FetchAndTester {
            fn call<T: MmioOperand>(&mut self) {
                let mmio = CachedVmoMemory::new_mapped(VMO_SIZE).expect("Failed to map VMO.");
                let offset = 0;

                let val_aa = T::try_from(0xaa).unwrap();
                let _ = mmio.swap::<T>(offset, val_aa);

                let mask = T::try_from(0x0f).unwrap();
                let old = mmio.fetch_and::<T>(offset, mask);
                assert_eq!(old, val_aa);
                assert_eq!(mmio.load::<T>(offset), T::try_from(0x0a).unwrap());
            }
        }

        FetchAndTester.call_all();
    }

    #[test]
    fn test_atomic_fetch_or() {
        struct FetchOrTester;

        impl OperandCallable for FetchOrTester {
            fn call<T: MmioOperand>(&mut self) {
                let mmio = CachedVmoMemory::new_mapped(VMO_SIZE).expect("Failed to map VMO.");
                let offset = 0;

                let val_0a = T::try_from(0x0a).unwrap();
                let _ = mmio.swap::<T>(offset, val_0a);

                let bits = T::try_from(0x50).unwrap();
                let old = mmio.fetch_or::<T>(offset, bits);
                assert_eq!(old, val_0a);
                assert_eq!(mmio.load::<T>(offset), T::try_from(0x5a).unwrap());
            }
        }

        FetchOrTester.call_all();
    }

    #[test]
    fn test_atomic_errors() {
        struct ErrorTester;

        impl OperandCallable for ErrorTester {
            fn call<T: MmioOperand>(&mut self) {
                let mmio = CachedVmoMemory::new_mapped(VMO_SIZE).expect("Failed to map VMO.");
                let val = T::try_from(0x12).unwrap();

                // Out of range.
                assert_eq!(mmio.try_swap::<T>(VMO_SIZE, val), Err(MmioError::OutOfRange));
                assert_eq!(mmio.try_fetch_and::<T>(VMO_SIZE, val), Err(MmioError::OutOfRange));
                assert_eq!(mmio.try_fetch_or::<T>(VMO_SIZE, val), Err(MmioError::OutOfRange));

                // Unaligned access for types with alignment > 1.
                if std::mem::align_of::<T>() > 1 {
                    assert_eq!(mmio.try_swap::<T>(1, val), Err(MmioError::Unaligned));
                    assert_eq!(mmio.try_fetch_and::<T>(1, val), Err(MmioError::Unaligned));
                    assert_eq!(mmio.try_fetch_or::<T>(1, val), Err(MmioError::Unaligned));
                }
            }
        }

        ErrorTester.call_all();
    }
}
