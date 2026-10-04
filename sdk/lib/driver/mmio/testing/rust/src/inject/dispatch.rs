// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::ops::Range;
use std::sync::Arc;

use mmio::Register;

use crate::inject::{
    GenericVmoMemoryHandler, MaybeVmoMemoryHandler, VmoMemoryHandler, VmoOpHelper,
};
use crate::operand::MmioOperand;

/// Dispatches MMIO operations to specific handlers based on memory range.
pub struct OpDispatcher {
    ranges: BTreeMap<usize, (usize, DispatchNode)>,
}

impl OpDispatcher {
    fn get(&self, range: Range<usize>) -> Option<&DispatchNode> {
        let (start, (len, node)) = self.ranges.range(..=range.start).next_back()?;
        let end = start.checked_add(*len).expect("Overflow calculating range end");
        if range.start >= *start && range.start < end {
            assert!(
                range.end <= end,
                "Register access in {range:?} extends beyond range {start}..{end}"
            );
            Some(node)
        } else {
            None
        }
    }

    fn ensure(&self, range: Range<usize>) -> &DispatchNode {
        self.get(range.clone()).unwrap_or_else(|| panic!("{range:?} not installed"))
    }
}

impl GenericVmoMemoryHandler for OpDispatcher {
    fn load<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>) -> T {
        self.ensure(op.range()).load(op)
    }

    fn store<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>, value: T) {
        self.ensure(op.range()).store(op, value)
    }
}

/// A trait marking a [`VmoMemoryHandler`] as suitable for use within
/// [`Dispatch`].
pub trait DispatchHandler: VmoMemoryHandler + Send + Sync + 'static {}

impl<O: VmoMemoryHandler + Send + Sync + 'static> DispatchHandler for O {}

/// A reference-counted MMIO handler node used in dispatch trees.
#[derive(Clone)]
pub struct DispatchNode(Arc<dyn DispatchHandler>);

impl DispatchNode {
    /// Creates a new [`Node`] wrapping `handler`.
    pub fn new(handler: Arc<dyn DispatchHandler>) -> Self {
        Self(handler)
    }

    /// Takes this [`DispatchNode`] and returns a type that implements
    /// [`DispatchHandler`].
    ///
    /// This is useful to reuse `DispatchNode`s with other `inject` combinators.
    pub fn into_handler(self) -> DispatchNodeHandler {
        DispatchNodeHandler(self)
    }

    pub(crate) fn load<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>) -> T {
        T::load(&*self.0, op)
    }

    pub(crate) fn store<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>, value: T) {
        T::store(&*self.0, op, value)
    }
}

impl<T: DispatchHandler> From<T> for DispatchNode {
    fn from(value: T) -> Self {
        Self(Arc::new(value))
    }
}

/// A wrapper around [`DispatchNode`] that implements [`DispatchHandler`].
///
/// Use [`DispatchNodeHandler::into_node`] to retrieve a [`DispatchNode`] from
/// this. Note that there exists a blanket `From` impl that wraps any
/// [`DispatchHandler`] into an `Arc` for `DispatchNode`.
#[derive(Clone)]
pub struct DispatchNodeHandler(DispatchNode);

impl DispatchNodeHandler {
    /// Takes this [`DispatchNodeHandler`] transforming it back into a
    /// `DispatchNode` for installation in a `Dispatch`.
    ///
    /// See [`DispatchNode::into_handler`].
    pub fn into_node(self) -> DispatchNode {
        self.0
    }
}

impl GenericVmoMemoryHandler for DispatchNodeHandler {
    fn load<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>) -> T {
        self.0.load(op)
    }

    fn store<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>, value: T) {
        self.0.store(op, value)
    }
}

/// Builder for constructing a [`Dispatch`] handler.
///
/// See [`DispatchBuilder::add`] for how to construct dispatched ranges.
pub struct DispatchBuilder {
    inner: OpDispatcher,
}

impl Default for DispatchBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl DispatchBuilder {
    /// Creates a new empty [`DispatchBuilder`] with a no-op range.
    pub fn new() -> Self {
        Self { inner: OpDispatcher { ranges: Default::default() } }
    }

    /// Registers `entries` into this builder.
    pub fn insert<T: DispatchBuilderEntries>(mut self, entries: T) -> Self {
        for (range, node) in entries.into_entries() {
            assert!(range.start < range.end, "invalid or empty range: {range:?}");
            for (start, (len, _)) in self.inner.ranges.range(..range.end) {
                let existing = *start..(*start + *len);
                assert!(
                    !(range.start < existing.end && existing.start < range.end),
                    "{range:?} overlaps with {existing:?}"
                );
            }

            assert!(
                self.inner.ranges.insert(range.start, (range.end - range.start, node)).is_none()
            );
        }

        self
    }

    /// A shorthand for [`DispatchBuilder::insert`] with a tuple of `(range,
    /// node)`.
    pub fn insert_node<T: Into<DispatchNode>>(self, range: Range<usize>, node: T) -> Self {
        self.insert((range, node))
    }

    /// A shorthand for [`DispatchBuilder::insert`] with a tuple of
    /// `(RegisterRange, node)`.
    pub fn insert_reg<R: Register, T: Into<DispatchNode>>(self, node: T) -> Self {
        self.insert((RegisterRange::<R>::new(), node))
    }

    /// Consumes the builder and returns the configured [`Dispatch`].
    pub fn build(self) -> OpDispatcher {
        self.inner
    }
}

/// A trait abstractig addition of ranges to a [`DispatchBuilder`].
pub trait DispatchBuilderEntries {
    /// Consumes this type returning a series of `Range` and `DispatchNode` for
    /// insertion in a [`DispatchBuilder`].
    fn into_entries(self) -> impl Iterator<Item = (Range<usize>, DispatchNode)>;
}

impl<T: Into<DispatchNode>> DispatchBuilderEntries for (Range<usize>, T) {
    fn into_entries(self) -> impl Iterator<Item = (Range<usize>, DispatchNode)> {
        let (range, node) = self;
        std::iter::once((range, node.into()))
    }
}

/// A helper struct for producing `Range<usize>` from a [`Register`] definition.
#[derive(Debug, Copy, Clone)]
pub struct RegisterRange<R: Register>(PhantomData<R>);

impl<R: Register> RegisterRange<R> {
    /// Creates a new `RegisterRange`.
    pub fn new() -> Self {
        Self(PhantomData)
    }

    /// Returns the address range for register `R`.
    pub fn into_range(self) -> Range<usize> {
        R::OFFSET..(R::OFFSET + std::mem::size_of::<R::Value>())
    }
}

impl<R: Register> Default for RegisterRange<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: Register, T: Into<DispatchNode>> DispatchBuilderEntries for (RegisterRange<R>, T) {
    fn into_entries(self) -> impl Iterator<Item = (Range<usize>, DispatchNode)> {
        let (range, node) = self;
        std::iter::once((range.into_range(), node.into()))
    }
}

impl MaybeVmoMemoryHandler for OpDispatcher {
    fn get_handler(&self, range: Range<usize>) -> Option<&(impl VmoMemoryHandler + ?Sized)> {
        self.get(range).map(|node| &*node.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CachedVmoMemory;
    use crate::inject::VmoMemory;
    use crate::inject::passthrough::Passthrough;
    use mmio::MmioExt as _;

    struct TestReg;
    impl Register for TestReg {
        type Value = u32;
        const OFFSET: usize = 0x10;
        fn from_raw(_value: u32) -> Self {
            Self
        }
        fn to_raw(&self) -> u32 {
            0
        }
    }

    #[test]
    fn test_dispatch_builder_and_node() {
        let builder = DispatchBuilder::default();
        let node = DispatchNode::from(Passthrough);
        let handler = node.clone().into_handler();
        let node_back = handler.into_node();

        let reg_range = RegisterRange::<TestReg>::default();
        assert_eq!(reg_range.into_range(), 0x10..0x14);

        let dispatch =
            builder.insert_node(0..4, node_back).insert_reg::<TestReg, _>(Passthrough).build();

        let mut mmio = CachedVmoMemory::new_mapped(4096)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(dispatch, vmo));

        let val1 = 0x1122_3344_u32;
        mmio.store::<u32>(0, val1);
        assert_eq!(mmio.load::<u32>(0), val1);

        let val2 = 0x5566_7788_u32;
        mmio.store::<u32>(0x10, val2);
        assert_eq!(mmio.load::<u32>(0x10), val2);
    }

    #[fuchsia::test(logging = false)]
    #[should_panic(expected = "overlaps with")]
    fn test_dispatch_overlapping_ranges_panics() {
        let _ =
            DispatchBuilder::new().insert_node(0..10, Passthrough).insert_node(5..15, Passthrough);
    }

    #[fuchsia::test(logging = false)]
    #[should_panic(expected = "invalid or empty range")]
    #[allow(clippy::reversed_empty_ranges)]
    fn test_dispatch_invalid_range_panics() {
        let _ = DispatchBuilder::new().insert_node(10..5, Passthrough);
    }

    #[fuchsia::test(logging = false)]
    #[should_panic(expected = "invalid or empty range")]
    fn test_dispatch_empty_range_panics() {
        let _ = DispatchBuilder::new().insert_node(5..5, Passthrough);
    }

    #[fuchsia::test(logging = false)]
    #[should_panic(expected = "not installed")]
    fn test_dispatch_uninstalled_range_panics() {
        let dispatch = DispatchBuilder::new().insert_node(0..4, Passthrough).build();
        let mmio = CachedVmoMemory::new_mapped(4096)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(dispatch, vmo));

        let _ = mmio.load::<u32>(8);
    }

    #[fuchsia::test(logging = false)]
    #[should_panic(expected = "extends beyond range")]
    fn test_dispatch_extends_beyond_range_panics() {
        let dispatch = DispatchBuilder::new().insert_node(0..4, Passthrough).build();
        let mmio = CachedVmoMemory::new_mapped(4096)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(dispatch, vmo));

        let _ = mmio.load::<u64>(0);
    }
}
