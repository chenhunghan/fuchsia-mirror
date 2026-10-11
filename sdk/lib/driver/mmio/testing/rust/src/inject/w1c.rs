// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::marker::PhantomData;

use crate::atomic::AtomicMmio;
use crate::inject::{GenericVmoMemoryHandler, VmoOpHelper};
use crate::operand::MmioOperand;

/// A handler implementation that fakes write-1-to-clear registers of width `T`.
///
/// `WriteOneToClear` replaces every `store` of `v` with a `fetch_and(!v)`, which
/// has the final effect of faking registers that clear the set bits on write.
///
/// The handler panics on both loads and stores if the operand width does not
/// match `T`.
pub struct WriteOneToClear<T>(PhantomData<T>);

impl<T> WriteOneToClear<T> {
    /// Creates a new write-1-to-clear handler.
    pub fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> Default for WriteOneToClear<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> GenericVmoMemoryHandler for WriteOneToClear<T> {
    fn load<X: MmioOperand>(&self, op: VmoOpHelper<'_, X>) -> X {
        assert_eq!(std::mem::size_of::<X>(), std::mem::size_of::<T>(), "incorrect width");
        op.load()
    }

    fn store<X: MmioOperand>(&self, op: VmoOpHelper<'_, X>, value: X) {
        assert_eq!(std::mem::size_of::<X>(), std::mem::size_of::<T>(), "incorrect width");
        let _: X = op.borrow_region().fetch_and(0, !value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CachedVmoMemory;
    use crate::inject::VmoMemory;
    use crate::operand::MmioOperand;
    use crate::operand::tests::OperandCallable;
    use mmio::MmioExt as _;

    const VMO_SIZE: usize = 4096;

    #[test]
    fn test_w1c_all_operand_types() {
        struct W1CTester;

        impl OperandCallable for W1CTester {
            fn call<T: MmioOperand>(&mut self) {
                let handler = WriteOneToClear::<T>::default();
                let mut mmio = CachedVmoMemory::new_mapped(VMO_SIZE).expect("Failed to map VMO.");

                // Set initial bits in memory using mmio.store on the unmapped region.
                let initial_val = T::try_from(0xff).unwrap();
                mmio.store::<T>(0, initial_val);

                let mut mmio = mmio.map(|vmo| VmoMemory::new(handler, vmo));
                assert_eq!(mmio.load::<T>(0), initial_val);

                // Write 1 to clear bit 0x0f (bits 0..4).
                let clear_bits = T::try_from(0x0f).unwrap();
                mmio.store::<T>(0, clear_bits);

                // Remaining value should be 0xf0.
                let expected_val = T::try_from(0xf0).unwrap();
                assert_eq!(mmio.load::<T>(0), expected_val);
            }
        }

        W1CTester.call_all();
    }

    #[fuchsia::test(logging = false)]
    #[should_panic(expected = "incorrect width")]
    fn test_w1c_load_incorrect_width_panics() {
        let handler = WriteOneToClear::<u32>::new();
        let mmio = CachedVmoMemory::new_mapped(VMO_SIZE)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(handler, vmo));

        let _ = mmio.load::<u8>(0);
    }

    #[fuchsia::test(logging = false)]
    #[should_panic(expected = "incorrect width")]
    fn test_w1c_store_incorrect_width_panics() {
        let mut mmio = CachedVmoMemory::new_mapped(VMO_SIZE)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(WriteOneToClear::<u32>::new(), vmo));

        mmio.store::<u8>(0, 0x12);
    }
}
