// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::ops::Range;

use crate::inject::{
    GenericVmoMemoryHandler, MaybeVmoMemoryHandler, VmoMemoryHandler, VmoOpHelper,
};
use crate::operand::MmioOperand;

/// An MMIO handler that applies an offset to operations before dispatching the
/// call to the next handler.
///
/// Use this combinator so that the inner handlers can operate in relative
/// address spaces. For example, if a register bank is defined in an offset in
/// conjunction with [`mmio::MmioSplit`], it is desirable to reuse the same
/// [`mmio::Register`] implementations in the fakes.
pub struct Offset<H> {
    inner: H,
    offset: usize,
}

impl<H> Offset<H> {
    /// Returns a new [`Offset`] that dispatches calls into `inner` after
    /// applying `offset` to [`VmoOpHelper`] in both loads and stores.
    pub fn new(inner: H, offset: usize) -> Self {
        Self { inner, offset }
    }
}

impl<H: VmoMemoryHandler> GenericVmoMemoryHandler for Offset<H> {
    fn load<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>) -> T {
        T::load(&self.inner, op.apply_offset(self.offset))
    }

    fn store<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>, value: T) {
        T::store(&self.inner, op.apply_offset(self.offset), value)
    }
}

impl<H: MaybeVmoMemoryHandler> MaybeVmoMemoryHandler for Offset<H> {
    fn get_handler(&self, range: Range<usize>) -> Option<&(impl VmoMemoryHandler + ?Sized)> {
        let Some(start) = range.start.checked_sub(self.offset) else {
            panic!("{range:?} before offset {}", self.offset);
        };
        self.inner.get_handler(start..start + range.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CachedVmoMemory;
    use crate::inject::dispatch::DispatchBuilder;
    use crate::inject::passthrough::Passthrough;
    use crate::inject::{MockMaybeVmoMemoryHandler, MockVmoMemoryHandler, VmoMemory};
    use crate::operand::tests::OperandCallable;
    use mmio::MmioExt as _;
    use mmio::region::MmioRegion;

    #[test]
    fn test_offset_passthrough() {
        const BASE_OFFSET: usize = 0x100;

        struct TestRunner<'a>(&'a mut MmioRegion<VmoMemory<Offset<Passthrough>>>);

        impl OperandCallable for TestRunner<'_> {
            fn call<T: MmioOperand>(&mut self) {
                let val = T::try_from(0x42).unwrap();
                self.0.store::<T>(BASE_OFFSET, val);
                assert_eq!(self.0.load::<T>(BASE_OFFSET), val);
            }
        }

        let handler = Offset::new(Passthrough, BASE_OFFSET);
        let mut mmio = CachedVmoMemory::new_mapped(4096)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(handler, vmo));

        TestRunner(&mut mmio).call_all();
    }

    #[test]
    fn test_offset_op_helper_attributes() {
        const BASE_OFFSET: usize = 0x200;
        let expected_val = 0x1234_5678_u32;

        let mut mock = MockVmoMemoryHandler::new();
        let _ = mock
            .expect_load32()
            .once()
            .withf(|op| {
                op.offset() == 0
                    && op.range() == (0..4)
                    && op.vmo_offset() == BASE_OFFSET
                    && op.vmo_range() == (BASE_OFFSET..BASE_OFFSET + 4)
            })
            .return_const(expected_val);

        let val_to_store = 0x8765_4321_u32;
        let _ = mock
            .expect_store32()
            .once()
            .withf(move |op, val| {
                op.offset() == 4
                    && op.range() == (4..8)
                    && op.vmo_offset() == BASE_OFFSET + 4
                    && op.vmo_range() == (BASE_OFFSET + 4..BASE_OFFSET + 8)
                    && *val == val_to_store
            })
            .return_const(());

        let handler = Offset::new(mock, BASE_OFFSET);
        let mut mmio = CachedVmoMemory::new_mapped(4096)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(handler, vmo));

        assert_eq!(mmio.load::<u32>(BASE_OFFSET), expected_val);
        mmio.store::<u32>(BASE_OFFSET + 4, val_to_store);
    }

    #[test]
    fn test_offset_with_dispatch() {
        const BASE_OFFSET: usize = 0x100;
        let dispatch = DispatchBuilder::new().insert_node(0..4, Passthrough).build();
        let handler = Offset::new(dispatch, BASE_OFFSET);

        let mut mmio = CachedVmoMemory::new_mapped(4096)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(handler, vmo));

        let val = 0xab_cd_ef_01_u32;
        mmio.store::<u32>(BASE_OFFSET, val);
        assert_eq!(mmio.load::<u32>(BASE_OFFSET), val);
    }

    #[test]
    fn test_maybe_handler() {
        const BASE_OFFSET: usize = 0x100;
        let mut mock = MockMaybeVmoMemoryHandler::<Passthrough>::new();
        let _ = mock
            .expect_get_handler()
            .once()
            .withf(|range| range == &(0usize..4))
            .return_const(None);

        let handler = Offset::new(mock, BASE_OFFSET);
        assert!(handler.get_handler(BASE_OFFSET..BASE_OFFSET + 4).is_none());
    }

    #[fuchsia::test(logging = false)]
    #[should_panic(expected = "too big for current offset")]
    fn test_offset_underflow_panics() {
        const BASE_OFFSET: usize = 0x100;
        let handler = Offset::new(Passthrough, BASE_OFFSET);

        let mmio = CachedVmoMemory::new_mapped(4096)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(handler, vmo));

        // Offset 0x50 is smaller than BASE_OFFSET 0x100, which causes underflow.
        let _ = mmio.load::<u32>(0x50);
    }
}
