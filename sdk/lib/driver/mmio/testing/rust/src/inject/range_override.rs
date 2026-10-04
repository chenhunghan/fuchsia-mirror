// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::ops::Range;

use crate::inject::{GenericVmoMemoryHandler, VmoMemoryHandler, VmoOpHelper};
use crate::operand::MmioOperand;

/// A handler that delegates matching ranges to a [`MaybeVmoMemoryHandler`]
/// implementation and falls back to a parent handler for unmatched ranges.
pub struct RangeOverride<P, O> {
    maybe_override: O,
    parent: P,
}

impl<P, O> RangeOverride<P, O> {
    /// Creates a new [`RangeOverride`] with the given `parent` and
    /// `maybe_override`.
    pub fn new(parent: P, maybe_override: O) -> Self {
        Self { maybe_override, parent }
    }
}

impl<P: VmoMemoryHandler, O: MaybeVmoMemoryHandler> GenericVmoMemoryHandler
    for RangeOverride<P, O>
{
    fn load<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>) -> T {
        match self.maybe_override.get_handler(op.range()) {
            Some(handler) => T::load(handler, op),
            None => T::load(&self.parent, op),
        }
    }

    fn store<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>, value: T) {
        match self.maybe_override.get_handler(op.range()) {
            Some(handler) => T::store(handler, op, value),
            None => T::store(&self.parent, op, value),
        }
    }
}

/// A trait abstracting _optional_ handling of a VMO range.
pub trait MaybeVmoMemoryHandler {
    /// Returns a [`VmoMemoryHandler`] if `range` can be operated on by the
    /// implementer.
    fn get_handler(&self, range: Range<usize>) -> Option<&(impl VmoMemoryHandler + ?Sized)>;
}

/// Provides a mock for `MaybeVmoMemoryHandler` via `mockall`.
///
/// NOTE: We can't use `automock` here because `mockall` gets confused with the
/// lifetimes.
#[cfg(test)]
mod mock {
    use super::*;

    mockall::mock! {
        pub MaybeVmoMemoryHandler<T: VmoMemoryHandler + 'static> {
            pub fn get_handler<'a>(&'a self, range: Range<usize>) -> Option<&'a T>;
        }
    }

    impl<T: VmoMemoryHandler + 'static> MaybeVmoMemoryHandler for MockMaybeVmoMemoryHandler<T> {
        fn get_handler(&self, range: Range<usize>) -> Option<&(impl VmoMemoryHandler + ?Sized)> {
            self.get_handler(range)
        }
    }
}
#[cfg(test)]
pub use mock::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CachedVmoMemory;
    use crate::inject::dispatch::DispatchBuilder;
    use crate::inject::passthrough::Passthrough;
    use crate::inject::{MockVmoMemoryHandler, VmoMemory};
    use mmio::MmioExt as _;

    #[test]
    fn test_range_override() {
        let expected_val = 0xcafe_babe_u32;
        let mut mock = MockVmoMemoryHandler::new();
        let _ = mock
            .expect_load32()
            .once()
            .withf(|op| op.offset() == 0 && op.range() == (0..4))
            .return_const(expected_val);

        let dispatch = DispatchBuilder::new().insert_node(0..4, mock).build();
        let handler = RangeOverride::new(Passthrough, dispatch);

        let mut mmio = CachedVmoMemory::new_mapped(4096)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(handler, vmo));

        // Access in overridden range 0..4 returns the constant from MockVmoMemoryHandler.
        assert_eq!(mmio.load::<u32>(0), expected_val);

        // Access outside overridden range (offset 4) falls back to Passthrough.
        let val_outside = 0x1234_5678_u32;
        mmio.store::<u32>(4, val_outside);
        assert_eq!(mmio.load::<u32>(4), val_outside);
    }
}
