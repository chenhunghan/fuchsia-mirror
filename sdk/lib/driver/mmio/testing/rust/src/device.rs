// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::atomic::AtomicMmio;
use crate::operand::MmioOperand;
use mmio::{IndexedRegister, Mmio, MmioExt, Register};

/// Provides a wrapper around `Mmio` and `AtomicMmio` implementations for
/// register access in tests from the perspective of a device.
///
/// This struct effectively sidesteps the `WritableRegister` and
/// `ReadableRegister` traits, given it's meant to be used by a faked peripheral
/// Mmio.
pub struct FakeMmioDevice<M>(M);

impl<M> FakeMmioDevice<M> {
    /// Creates a new [`FakeMmioDevice`] wrapping the MMIO implementation `m`.
    pub fn new(m: M) -> Self {
        Self(m)
    }
}

impl<M: Mmio> FakeMmioDevice<M> {
    /// Reads a register `R`.
    pub fn read<R: Register>(&self) -> R {
        R::from_raw(self.0.load::<R::Value>(R::OFFSET))
    }

    /// Writes a register `R` with value `v`.
    pub fn write<R: Register>(&mut self, v: R) {
        self.0.store::<R::Value>(R::OFFSET, v.to_raw())
    }

    /// Clears the specified bits in `v` for register `R`.
    pub fn clear_bits<R: Register>(&mut self, v: R::Value) {
        self.0.masked_modify(R::OFFSET, v, !v)
    }

    /// Sets the specified bits in `v` for register `R`.
    pub fn set_bits<R: Register>(&mut self, v: R::Value) {
        self.0.masked_modify(R::OFFSET, v, v)
    }

    /// Reads an indexed register `R` at `index`.
    pub fn read_indexed<R: IndexedRegister>(&self, index: usize) -> R {
        R::from_raw(self.0.load::<R::Value>(R::BASE_OFFSET + index * R::STRIDE))
    }

    /// Writes an indexed register `R` with value `v` at `index`.
    pub fn write_indexed<R: IndexedRegister>(&mut self, index: usize, v: R) {
        self.0.store::<R::Value>(R::BASE_OFFSET + index * R::STRIDE, v.to_raw())
    }
}

impl<M: AtomicMmio> FakeMmioDevice<M> {
    /// Atomically swaps register `R` with value `v` and returns the previous
    /// value.
    pub fn swap<R: Register<Value: MmioOperand>>(&self, v: R) -> R {
        R::from_raw(self.0.swap(R::OFFSET, v.to_raw()))
    }

    /// Atomically takes register `R`, replacing it with `R::default()`, and
    /// returns the previous value.
    pub fn take<R: Register<Value: MmioOperand> + Default>(&self) -> R {
        self.swap(R::default())
    }

    /// Atomically performs bitwise OR on register `R` with `v` and returns the
    /// previous value.
    pub fn fetch_or<R: Register<Value: MmioOperand>>(&self, v: R) -> R {
        R::from_raw(self.0.fetch_or(R::OFFSET, v.to_raw()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CachedVmoMemory;

    const VMO_SIZE: usize = 4096;

    mmio::register! {
        #[register(offset = 0x0, mode = RW)]
        pub struct TestReg32(u32);

        #[register(offset = 0x8, mode = RW)]
        pub struct TestReg8(u8);

        #[indexed_register(offset = 0x10, stride = 4, count = 4, mode = RW)]
        pub struct TestIndexedReg32(u32);
    }

    #[test]
    fn test_read_write() {
        let mmio = CachedVmoMemory::new_mapped(VMO_SIZE).expect("Failed to map VMO.");
        let mut dev = FakeMmioDevice::new(mmio);

        let reg32 = TestReg32::from_raw(0x1234_5678);
        dev.write(reg32);
        assert_eq!(dev.read::<TestReg32>(), reg32);

        let reg8 = TestReg8::from_raw(0xab);
        dev.write(reg8);
        assert_eq!(dev.read::<TestReg8>(), reg8);
    }

    #[test]
    fn test_set_and_clear_bits() {
        let mmio = CachedVmoMemory::new_mapped(VMO_SIZE).expect("Failed to map VMO.");
        let mut dev = FakeMmioDevice::new(mmio);

        dev.write(TestReg32::from_raw(0xf0f0_f0f0));

        dev.clear_bits::<TestReg32>(0xa0a0_a0a0);
        assert_eq!(dev.read::<TestReg32>(), TestReg32::from_raw(0x5050_5050));

        dev.set_bits::<TestReg32>(0x0505_0505);
        assert_eq!(dev.read::<TestReg32>(), TestReg32::from_raw(0x5555_5555));
    }

    #[test]
    fn test_indexed_read_write() {
        let mmio = CachedVmoMemory::new_mapped(VMO_SIZE).expect("Failed to map VMO.");
        let mut dev = FakeMmioDevice::new(mmio);

        let reg0 = TestIndexedReg32::from_raw(0x1111_1111);
        let reg1 = TestIndexedReg32::from_raw(0x2222_2222);
        let reg2 = TestIndexedReg32::from_raw(0x3333_3333);

        dev.write_indexed(0, reg0);
        dev.write_indexed(1, reg1);
        dev.write_indexed(2, reg2);

        assert_eq!(dev.read_indexed::<TestIndexedReg32>(0), reg0);
        assert_eq!(dev.read_indexed::<TestIndexedReg32>(1), reg1);
        assert_eq!(dev.read_indexed::<TestIndexedReg32>(2), reg2);
    }

    #[test]
    fn test_atomic_swap() {
        let mmio = CachedVmoMemory::new_mapped(VMO_SIZE).expect("Failed to map VMO.");
        let dev = FakeMmioDevice::new(mmio);

        let new_val = TestReg32::from_raw(0xdead_beef);
        let old = dev.swap(new_val);
        assert_eq!(old, TestReg32::from_raw(0));
        assert_eq!(dev.read::<TestReg32>(), new_val);
    }

    #[test]
    fn test_atomic_take() {
        let mmio = CachedVmoMemory::new_mapped(VMO_SIZE).expect("Failed to map VMO.");
        let dev = FakeMmioDevice::new(mmio);

        let val = TestReg32::from_raw(0xcafe_babe);
        let _ = dev.swap(val);
        let taken = dev.take::<TestReg32>();
        assert_eq!(taken, val);
        assert_eq!(dev.read::<TestReg32>(), TestReg32::default());
    }

    #[test]
    fn test_atomic_fetch_or() {
        let mmio = CachedVmoMemory::new_mapped(VMO_SIZE).expect("Failed to map VMO.");
        let dev = FakeMmioDevice::new(mmio);

        let initial = TestReg32::from_raw(0x0000_ffff);
        let _ = dev.swap(initial);
        let old = dev.fetch_or(TestReg32::from_raw(0xffff_0000));
        assert_eq!(old, initial);
        assert_eq!(dev.read::<TestReg32>(), TestReg32::from_raw(0xffff_ffff));
    }
}
