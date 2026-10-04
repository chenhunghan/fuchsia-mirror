// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::inject::{GenericVmoMemoryHandler, VmoOpHelper};
use crate::operand::MmioOperand;

/// An MMIO handler that directly passes loads and stores to the VMO memory.
#[derive(Default, Debug, Copy, Clone)]
pub struct Passthrough;

impl GenericVmoMemoryHandler for Passthrough {
    fn load<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>) -> T {
        op.load()
    }

    fn store<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>, value: T) {
        op.store(value)
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
    use mmio::region::MmioRegion;

    #[test]
    fn test_passthrough() {
        struct TestRunner<'a>(&'a mut MmioRegion<VmoMemory<Passthrough>>);

        impl OperandCallable for TestRunner<'_> {
            fn call<T: MmioOperand>(&mut self) {
                let offset = 0;
                let val = T::try_from(0x42).unwrap();
                self.0.store::<T>(offset, val);
                assert_eq!(self.0.load::<T>(offset), val);
            }
        }

        let mut mmio = CachedVmoMemory::new_mapped(4096)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(Passthrough, vmo));

        TestRunner(&mut mmio).call_all();
    }
}
