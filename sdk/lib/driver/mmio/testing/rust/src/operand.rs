// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::atomic::AtomicOperand;
use crate::inject::{VmoMemoryHandler, VmoOpHelper};

/// Trait for MMIO operand types that can be loaded or stored via a test
/// handler.
///
/// This trait is local version of [`mmio::MmioOperand`] with extensions
/// specifically tailored for testing.
pub trait MmioOperand:
    mmio::MmioOperand + AtomicOperand + sealed::MmioOperand + TryFrom<usize, Error: std::fmt::Debug>
{
    /// Dispatches a load operation of this operand type to `handler`.
    fn load<H: VmoMemoryHandler + ?Sized>(handler: &H, op: VmoOpHelper<'_, Self>) -> Self;

    /// Dispatches a store operation of this operand type to `handler`.
    fn store<H: VmoMemoryHandler + ?Sized>(handler: &H, op: VmoOpHelper<'_, Self>, value: Self);
}

impl MmioOperand for u8 {
    fn load<H: VmoMemoryHandler + ?Sized>(handler: &H, op: VmoOpHelper<'_, Self>) -> Self {
        handler.load8(op)
    }

    fn store<H: VmoMemoryHandler + ?Sized>(handler: &H, op: VmoOpHelper<'_, Self>, value: Self) {
        handler.store8(op, value);
    }
}

impl MmioOperand for u16 {
    fn load<H: VmoMemoryHandler + ?Sized>(handler: &H, op: VmoOpHelper<'_, Self>) -> Self {
        handler.load16(op)
    }

    fn store<H: VmoMemoryHandler + ?Sized>(handler: &H, op: VmoOpHelper<'_, Self>, value: Self) {
        handler.store16(op, value);
    }
}

impl MmioOperand for u32 {
    fn load<H: VmoMemoryHandler + ?Sized>(handler: &H, op: VmoOpHelper<'_, Self>) -> Self {
        handler.load32(op)
    }

    fn store<H: VmoMemoryHandler + ?Sized>(handler: &H, op: VmoOpHelper<'_, Self>, value: Self) {
        handler.store32(op, value);
    }
}

impl MmioOperand for u64 {
    fn load<H: VmoMemoryHandler + ?Sized>(handler: &H, op: VmoOpHelper<'_, Self>) -> Self {
        handler.load64(op)
    }

    fn store<H: VmoMemoryHandler + ?Sized>(handler: &H, op: VmoOpHelper<'_, Self>, value: Self) {
        handler.store64(op, value);
    }
}

mod sealed {

    /// Internal sealed MmioOperand trait as the types implementing this must match the types in
    /// the loadn/storen trait methods.
    pub trait MmioOperand {}

    impl MmioOperand for u8 {}
    impl MmioOperand for u16 {}
    impl MmioOperand for u32 {}
    impl MmioOperand for u64 {}
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A helper trait for executing unit test logic generically across all supported
    /// [`MmioOperand`] types (`u8`, `u16`, `u32`, and `u64`).
    ///
    /// This trait simplifies unit testing for MMIO handler implementations by avoiding
    /// copy-pasted test logic across different operand widths. Implementers define
    /// [`call<T>`](OperandCallable::call) for generic operand type `T`, and call
    /// [`call_all`](OperandCallable::call_all) to execute the test for `u8`, `u16`,
    /// `u32`, and `u64`.
    pub(crate) trait OperandCallable {
        /// Executes test logic for a specific operand type `T`.
        fn call<T: MmioOperand>(&mut self);

        /// Executes [`call`](OperandCallable::call) for `u8`, `u16`, `u32`, and `u64`.
        fn call_all(mut self)
        where
            Self: Sized,
        {
            self.call::<u8>();
            self.call::<u16>();
            self.call::<u32>();
            self.call::<u64>();
        }
    }
}
