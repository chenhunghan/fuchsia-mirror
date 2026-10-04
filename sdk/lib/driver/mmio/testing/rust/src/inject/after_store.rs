// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::ops::Range;

use crate::inject::{
    GenericVmoMemoryHandler, MaybeVmoMemoryHandler, VmoMemoryHandler, VmoOpHelper,
};
use crate::operand::MmioOperand;

/// A handler wrapper that executes a callback after every store operation.
pub struct AfterStore<H, F> {
    inner: H,
    callback: F,
}

impl<H, F> AfterStore<H, F> {
    /// Creates a new [`AfterStore`] wrapping `inner` with `callback`.
    pub fn new(inner: H, callback: F) -> Self {
        Self { inner, callback }
    }
}

impl<H: VmoMemoryHandler, F: Fn(Range<usize>)> GenericVmoMemoryHandler for AfterStore<H, F> {
    fn load<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>) -> T {
        T::load(&self.inner, op)
    }

    fn store<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>, value: T) {
        let range = op.range();
        T::store(&self.inner, op, value);
        (self.callback)(range)
    }
}

impl<H: MaybeVmoMemoryHandler, F> MaybeVmoMemoryHandler for AfterStore<H, F> {
    fn get_handler(&self, range: Range<usize>) -> Option<&(impl VmoMemoryHandler + ?Sized)> {
        self.inner.get_handler(range)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CachedVmoMemory;
    use crate::inject::passthrough::Passthrough;
    use crate::inject::{DispatchNode, DispatchNodeHandler, VmoMemory};
    use crate::operand::MmioOperand;
    use crate::operand::tests::OperandCallable;
    use mmio::MmioExt as _;
    use mmio::region::MmioRegion;
    use std::sync::{Arc, Mutex};

    #[test]
    fn test_after_store() {
        let stores = Arc::new(Mutex::new(Vec::<Range<usize>>::new()));
        let stores_clone = stores.clone();
        let handler: DispatchNodeHandler =
            DispatchNode::from(AfterStore::new(Passthrough, move |range| {
                stores_clone.lock().unwrap().push(range);
            }))
            .into_handler();

        let mut mmio = CachedVmoMemory::new_mapped(4096)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(handler, vmo));

        // Load should not trigger the store callback.
        let _ = mmio.load::<u32>(0);
        assert!(stores.lock().unwrap().is_empty());

        struct StoreTester<'a>(&'a mut MmioRegion<VmoMemory<DispatchNodeHandler>>);

        impl OperandCallable for StoreTester<'_> {
            fn call<T: MmioOperand>(&mut self) {
                let val = T::try_from(0x12).unwrap();
                self.0.store::<T>(0, val);
                assert_eq!(self.0.load::<T>(0), val);
            }
        }

        StoreTester(&mut mmio).call_all();

        let recorded = stores.lock().unwrap().clone();
        assert_eq!(recorded, vec![0..1, 0..2, 0..4, 0..8]);
    }
}
