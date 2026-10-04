// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Protocol resource adapters for GPIO, I2C, and SPI.
//!
//! Provides representative hardware backends and fakes for protocol-backed
//! resources (Spec 8.1, 9.5, 11.6; Milestone P4). Protocol resources preserve
//! their production request/response semantics rather than being flattened into
//! generic byte writes or MMIO offsets.

use crate::hardware_backend::{BackendError, FakeMmio, MmioBackend};
use std::collections::VecDeque;

/// Hardware access abstraction for a GPIO pin.
pub trait GpioBackend: Send {
    /// Reads the current voltage level (high = true, low = false).
    fn read(&mut self) -> Result<bool, BackendError>;

    /// Sets the output voltage level.
    fn write(&mut self, value: bool) -> Result<(), BackendError>;
}

/// Hardware access abstraction for an I2C bus device.
pub trait I2cBackend: Send {
    /// Executes an I2C transfer with optional write and read phases.
    fn transfer(&mut self, write_data: &[u8], read_length: usize) -> Result<Vec<u8>, BackendError>;
}

/// Hardware access abstraction for a SPI bus device.
pub trait SpiBackend: Send {
    /// Transmits data to a SPI device, returning received data of matching or specified length.
    fn transmit(&mut self, tx_data: &[u8]) -> Result<Vec<u8>, BackendError>;
}

/// Hardware access abstraction for a Clock device.
pub trait ClockBackend: Send {
    fn enable(&mut self) -> Result<(), BackendError>;
    fn disable(&mut self) -> Result<(), BackendError>;
    fn is_enabled(&mut self) -> Result<bool, BackendError>;
    fn set_rate(&mut self, hz: u64) -> Result<(), BackendError>;
    fn query_rate(&mut self, hz_in: u64) -> Result<u64, BackendError>;
    fn get_rate(&mut self) -> Result<u64, BackendError>;
}

/// Hardware access abstraction for a Reset device.
pub trait ResetBackend: Send {
    fn assert(&mut self) -> Result<(), BackendError>;
    fn deassert(&mut self) -> Result<(), BackendError>;
    fn toggle(&mut self) -> Result<(), BackendError>;
    fn status(&mut self) -> Result<bool, BackendError>;
}

/// Hardware access abstraction for a Serial device.
pub trait SerialBackend: Send {
    fn read(&mut self) -> Result<Vec<u8>, BackendError>;
    fn write(&mut self, data: &[u8]) -> Result<(), BackendError>;
}

/// Unified hardware access trait covering MMIO and protocol resources.
pub trait ResourceBackend: Send {
    fn read32(&mut self, _offset: u64) -> Result<u32, BackendError> {
        Err(BackendError::Fault)
    }
    fn write32(&mut self, _offset: u64, _value: u32) -> Result<(), BackendError> {
        Err(BackendError::Fault)
    }
    fn barrier(&mut self) {}
    fn gpio_read(&mut self) -> Result<bool, BackendError> {
        Err(BackendError::Fault)
    }
    fn gpio_write(&mut self, _value: bool) -> Result<(), BackendError> {
        Err(BackendError::Fault)
    }
    fn i2c_transfer(
        &mut self,
        _write_data: &[u8],
        _read_length: usize,
    ) -> Result<Vec<u8>, BackendError> {
        Err(BackendError::Fault)
    }
    fn spi_transmit(&mut self, _tx_data: &[u8]) -> Result<Vec<u8>, BackendError> {
        Err(BackendError::Fault)
    }
    fn clock_enable(&mut self) -> Result<(), BackendError> {
        Err(BackendError::Fault)
    }
    fn clock_disable(&mut self) -> Result<(), BackendError> {
        Err(BackendError::Fault)
    }
    fn clock_is_enabled(&mut self) -> Result<bool, BackendError> {
        Err(BackendError::Fault)
    }
    fn clock_set_rate(&mut self, _hz: u64) -> Result<(), BackendError> {
        Err(BackendError::Fault)
    }
    fn clock_query_rate(&mut self, _hz_in: u64) -> Result<u64, BackendError> {
        Err(BackendError::Fault)
    }
    fn clock_get_rate(&mut self) -> Result<u64, BackendError> {
        Err(BackendError::Fault)
    }
    fn reset_assert(&mut self) -> Result<(), BackendError> {
        Err(BackendError::Fault)
    }
    fn reset_deassert(&mut self) -> Result<(), BackendError> {
        Err(BackendError::Fault)
    }
    fn reset_toggle(&mut self) -> Result<(), BackendError> {
        Err(BackendError::Fault)
    }
    fn reset_status(&mut self) -> Result<bool, BackendError> {
        Err(BackendError::Fault)
    }
    fn serial_read(&mut self) -> Result<Vec<u8>, BackendError> {
        Err(BackendError::Fault)
    }
    fn serial_write(&mut self, _data: &[u8]) -> Result<(), BackendError> {
        Err(BackendError::Fault)
    }
}

impl ResourceBackend for FakeMmio {
    fn read32(&mut self, offset: u64) -> Result<u32, BackendError> {
        MmioBackend::read32(self, offset)
    }
    fn write32(&mut self, offset: u64, value: u32) -> Result<(), BackendError> {
        MmioBackend::write32(self, offset, value)
    }
    fn barrier(&mut self) {
        MmioBackend::barrier(self);
    }
}

/// In-memory fake GPIO pin for tests.
#[derive(Clone, Debug, Default)]
pub struct FakeGpio {
    pub value: bool,
    pub reads: usize,
    pub writes: Vec<bool>,
    pub fault: bool,
}

impl FakeGpio {
    pub fn new(initial: bool) -> Self {
        Self { value: initial, reads: 0, writes: Vec::new(), fault: false }
    }

    pub fn set_state(&mut self, value: bool) {
        self.value = value;
    }

    pub fn fail(&mut self, fault: bool) {
        self.fault = fault;
    }
}

impl GpioBackend for FakeGpio {
    fn read(&mut self) -> Result<bool, BackendError> {
        if self.fault {
            return Err(BackendError::Fault);
        }
        self.reads += 1;
        Ok(self.value)
    }

    fn write(&mut self, value: bool) -> Result<(), BackendError> {
        if self.fault {
            return Err(BackendError::Fault);
        }
        self.value = value;
        self.writes.push(value);
        Ok(())
    }
}

impl ResourceBackend for FakeGpio {
    fn gpio_read(&mut self) -> Result<bool, BackendError> {
        GpioBackend::read(self)
    }
    fn gpio_write(&mut self, value: bool) -> Result<(), BackendError> {
        GpioBackend::write(self, value)
    }
}

/// In-memory fake I2C device for tests.
#[derive(Clone, Debug, Default)]
pub struct FakeI2c {
    pub responses: VecDeque<Vec<u8>>,
    pub recorded_transfers: Vec<(Vec<u8>, usize)>,
    pub fault: bool,
}

impl FakeI2c {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_response(&mut self, response: Vec<u8>) {
        self.responses.push_back(response);
    }

    pub fn set_read_response(&mut self, response: Vec<u8>) {
        self.push_response(response);
    }

    pub fn fail(&mut self, fault: bool) {
        self.fault = fault;
    }
}

impl I2cBackend for FakeI2c {
    fn transfer(&mut self, write_data: &[u8], read_length: usize) -> Result<Vec<u8>, BackendError> {
        if self.fault {
            return Err(BackendError::Fault);
        }
        self.recorded_transfers.push((write_data.to_vec(), read_length));
        if let Some(resp) = self.responses.pop_front() {
            let mut out = resp;
            out.resize(read_length, 0);
            return Ok(out);
        }
        Ok(vec![0u8; read_length])
    }
}

impl ResourceBackend for FakeI2c {
    fn i2c_transfer(
        &mut self,
        write_data: &[u8],
        read_length: usize,
    ) -> Result<Vec<u8>, BackendError> {
        I2cBackend::transfer(self, write_data, read_length)
    }
}

/// In-memory fake SPI device for tests.
#[derive(Clone, Debug, Default)]
pub struct FakeSpi {
    pub responses: VecDeque<Vec<u8>>,
    pub recorded_transmits: Vec<Vec<u8>>,
    pub loopback: bool,
    pub fault: bool,
}

impl FakeSpi {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_loopback(mut self) -> Self {
        self.loopback = true;
        self
    }

    pub fn push_response(&mut self, response: Vec<u8>) {
        self.responses.push_back(response);
    }

    pub fn set_rx_response(&mut self, response: Vec<u8>) {
        self.push_response(response);
    }

    pub fn fail(&mut self, fault: bool) {
        self.fault = fault;
    }
}

impl SpiBackend for FakeSpi {
    fn transmit(&mut self, tx_data: &[u8]) -> Result<Vec<u8>, BackendError> {
        if self.fault {
            return Err(BackendError::Fault);
        }
        self.recorded_transmits.push(tx_data.to_vec());
        if self.loopback {
            return Ok(tx_data.to_vec());
        }
        if let Some(resp) = self.responses.pop_front() {
            return Ok(resp);
        }
        Ok(vec![0u8; tx_data.len()])
    }
}

impl ResourceBackend for FakeSpi {
    fn spi_transmit(&mut self, tx_data: &[u8]) -> Result<Vec<u8>, BackendError> {
        SpiBackend::transmit(self, tx_data)
    }
}

/// In-memory fake Clock for tests.
#[derive(Clone, Debug, Default)]
pub struct FakeClock {
    pub enabled: bool,
    pub rate: u64,
    pub fault: bool,
}

impl FakeClock {
    pub fn new(enabled: bool, rate: u64) -> Self {
        Self { enabled, rate, fault: false }
    }

    pub fn fail(&mut self, fault: bool) {
        self.fault = fault;
    }
}

impl ClockBackend for FakeClock {
    fn enable(&mut self) -> Result<(), BackendError> {
        if self.fault {
            return Err(BackendError::Fault);
        }
        self.enabled = true;
        Ok(())
    }

    fn disable(&mut self) -> Result<(), BackendError> {
        if self.fault {
            return Err(BackendError::Fault);
        }
        self.enabled = false;
        Ok(())
    }

    fn is_enabled(&mut self) -> Result<bool, BackendError> {
        if self.fault {
            return Err(BackendError::Fault);
        }
        Ok(self.enabled)
    }

    fn set_rate(&mut self, hz: u64) -> Result<(), BackendError> {
        if self.fault {
            return Err(BackendError::Fault);
        }
        self.rate = hz;
        Ok(())
    }

    fn query_rate(&mut self, hz_in: u64) -> Result<u64, BackendError> {
        if self.fault {
            return Err(BackendError::Fault);
        }
        Ok(hz_in)
    }

    fn get_rate(&mut self) -> Result<u64, BackendError> {
        if self.fault {
            return Err(BackendError::Fault);
        }
        Ok(self.rate)
    }
}

impl ResourceBackend for FakeClock {
    fn clock_enable(&mut self) -> Result<(), BackendError> {
        ClockBackend::enable(self)
    }
    fn clock_disable(&mut self) -> Result<(), BackendError> {
        ClockBackend::disable(self)
    }
    fn clock_is_enabled(&mut self) -> Result<bool, BackendError> {
        ClockBackend::is_enabled(self)
    }
    fn clock_set_rate(&mut self, hz: u64) -> Result<(), BackendError> {
        ClockBackend::set_rate(self, hz)
    }
    fn clock_query_rate(&mut self, hz_in: u64) -> Result<u64, BackendError> {
        ClockBackend::query_rate(self, hz_in)
    }
    fn clock_get_rate(&mut self) -> Result<u64, BackendError> {
        ClockBackend::get_rate(self)
    }
}

/// In-memory fake Reset for tests.
#[derive(Clone, Debug, Default)]
pub struct FakeReset {
    pub asserted: bool,
    pub fault: bool,
}

impl FakeReset {
    pub fn new(asserted: bool) -> Self {
        Self { asserted, fault: false }
    }

    pub fn fail(&mut self, fault: bool) {
        self.fault = fault;
    }
}

impl ResetBackend for FakeReset {
    fn assert(&mut self) -> Result<(), BackendError> {
        if self.fault {
            return Err(BackendError::Fault);
        }
        self.asserted = true;
        Ok(())
    }

    fn deassert(&mut self) -> Result<(), BackendError> {
        if self.fault {
            return Err(BackendError::Fault);
        }
        self.asserted = false;
        Ok(())
    }

    fn toggle(&mut self) -> Result<(), BackendError> {
        if self.fault {
            return Err(BackendError::Fault);
        }
        self.asserted = !self.asserted;
        Ok(())
    }

    fn status(&mut self) -> Result<bool, BackendError> {
        if self.fault {
            return Err(BackendError::Fault);
        }
        Ok(self.asserted)
    }
}

impl ResourceBackend for FakeReset {
    fn reset_assert(&mut self) -> Result<(), BackendError> {
        ResetBackend::assert(self)
    }
    fn reset_deassert(&mut self) -> Result<(), BackendError> {
        ResetBackend::deassert(self)
    }
    fn reset_toggle(&mut self) -> Result<(), BackendError> {
        ResetBackend::toggle(self)
    }
    fn reset_status(&mut self) -> Result<bool, BackendError> {
        ResetBackend::status(self)
    }
}

/// In-memory fake Serial for tests.
#[derive(Clone, Debug, Default)]
pub struct FakeSerial {
    pub rx_queue: VecDeque<u8>,
    pub tx_log: Vec<Vec<u8>>,
    pub loopback: bool,
    pub fault: bool,
}

impl FakeSerial {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_loopback(mut self) -> Self {
        self.loopback = true;
        self
    }

    pub fn push_rx(&mut self, data: &[u8]) {
        self.rx_queue.extend(data);
    }

    pub fn fail(&mut self, fault: bool) {
        self.fault = fault;
    }
}

impl SerialBackend for FakeSerial {
    fn read(&mut self) -> Result<Vec<u8>, BackendError> {
        if self.fault {
            return Err(BackendError::Fault);
        }
        let data: Vec<u8> = self.rx_queue.drain(..).collect();
        Ok(data)
    }

    fn write(&mut self, data: &[u8]) -> Result<(), BackendError> {
        if self.fault {
            return Err(BackendError::Fault);
        }
        self.tx_log.push(data.to_vec());
        if self.loopback {
            self.rx_queue.extend(data);
        }
        Ok(())
    }
}

impl ResourceBackend for FakeSerial {
    fn serial_read(&mut self) -> Result<Vec<u8>, BackendError> {
        SerialBackend::read(self)
    }
    fn serial_write(&mut self, data: &[u8]) -> Result<(), BackendError> {
        SerialBackend::write(self, data)
    }
}

/// Unified enum backend allowing heterogeneous resources to coexist in one proxy instance.
#[derive(Clone, Debug)]
pub enum DeviceBackend<
    M = FakeMmio,
    G = FakeGpio,
    I = FakeI2c,
    S = FakeSpi,
    C = FakeClock,
    R = FakeReset,
    U = FakeSerial,
> {
    Mmio(M),
    Gpio(G),
    I2c(I),
    Spi(S),
    Clock(C),
    Reset(R),
    Serial(U),
}

impl<M, G, I, S, C, R, U> DeviceBackend<M, G, I, S, C, R, U> {
    pub fn new_mmio(mmio: M) -> Self {
        Self::Mmio(mmio)
    }

    pub fn new_gpio(gpio: G) -> Self {
        Self::Gpio(gpio)
    }

    pub fn new_i2c(i2c: I) -> Self {
        Self::I2c(i2c)
    }

    pub fn new_spi(spi: S) -> Self {
        Self::Spi(spi)
    }

    pub fn new_clock(clock: C) -> Self {
        Self::Clock(clock)
    }

    pub fn new_reset(reset: R) -> Self {
        Self::Reset(reset)
    }

    pub fn new_serial(serial: U) -> Self {
        Self::Serial(serial)
    }
}

impl<
    M: ResourceBackend,
    G: ResourceBackend,
    I: ResourceBackend,
    S: ResourceBackend,
    C: ResourceBackend,
    R: ResourceBackend,
    U: ResourceBackend,
> ResourceBackend for DeviceBackend<M, G, I, S, C, R, U>
{
    fn read32(&mut self, offset: u64) -> Result<u32, BackendError> {
        match self {
            Self::Mmio(m) => m.read32(offset),
            _ => Err(BackendError::Fault),
        }
    }

    fn write32(&mut self, offset: u64, value: u32) -> Result<(), BackendError> {
        match self {
            Self::Mmio(m) => m.write32(offset, value),
            _ => Err(BackendError::Fault),
        }
    }

    fn barrier(&mut self) {
        if let Self::Mmio(m) = self {
            m.barrier();
        }
    }

    fn gpio_read(&mut self) -> Result<bool, BackendError> {
        match self {
            Self::Gpio(g) => g.gpio_read(),
            _ => Err(BackendError::Fault),
        }
    }

    fn gpio_write(&mut self, value: bool) -> Result<(), BackendError> {
        match self {
            Self::Gpio(g) => g.gpio_write(value),
            _ => Err(BackendError::Fault),
        }
    }

    fn i2c_transfer(
        &mut self,
        write_data: &[u8],
        read_length: usize,
    ) -> Result<Vec<u8>, BackendError> {
        match self {
            Self::I2c(i) => i.i2c_transfer(write_data, read_length),
            _ => Err(BackendError::Fault),
        }
    }

    fn spi_transmit(&mut self, tx_data: &[u8]) -> Result<Vec<u8>, BackendError> {
        match self {
            Self::Spi(s) => s.spi_transmit(tx_data),
            _ => Err(BackendError::Fault),
        }
    }

    fn clock_enable(&mut self) -> Result<(), BackendError> {
        match self {
            Self::Clock(c) => c.clock_enable(),
            _ => Err(BackendError::Fault),
        }
    }

    fn clock_disable(&mut self) -> Result<(), BackendError> {
        match self {
            Self::Clock(c) => c.clock_disable(),
            _ => Err(BackendError::Fault),
        }
    }

    fn clock_is_enabled(&mut self) -> Result<bool, BackendError> {
        match self {
            Self::Clock(c) => c.clock_is_enabled(),
            _ => Err(BackendError::Fault),
        }
    }

    fn clock_set_rate(&mut self, hz: u64) -> Result<(), BackendError> {
        match self {
            Self::Clock(c) => c.clock_set_rate(hz),
            _ => Err(BackendError::Fault),
        }
    }

    fn clock_query_rate(&mut self, hz_in: u64) -> Result<u64, BackendError> {
        match self {
            Self::Clock(c) => c.clock_query_rate(hz_in),
            _ => Err(BackendError::Fault),
        }
    }

    fn clock_get_rate(&mut self) -> Result<u64, BackendError> {
        match self {
            Self::Clock(c) => c.clock_get_rate(),
            _ => Err(BackendError::Fault),
        }
    }

    fn reset_assert(&mut self) -> Result<(), BackendError> {
        match self {
            Self::Reset(r) => r.reset_assert(),
            _ => Err(BackendError::Fault),
        }
    }

    fn reset_deassert(&mut self) -> Result<(), BackendError> {
        match self {
            Self::Reset(r) => r.reset_deassert(),
            _ => Err(BackendError::Fault),
        }
    }

    fn reset_toggle(&mut self) -> Result<(), BackendError> {
        match self {
            Self::Reset(r) => r.reset_toggle(),
            _ => Err(BackendError::Fault),
        }
    }

    fn reset_status(&mut self) -> Result<bool, BackendError> {
        match self {
            Self::Reset(r) => r.reset_status(),
            _ => Err(BackendError::Fault),
        }
    }

    fn serial_read(&mut self) -> Result<Vec<u8>, BackendError> {
        match self {
            Self::Serial(u) => u.serial_read(),
            _ => Err(BackendError::Fault),
        }
    }

    fn serial_write(&mut self, data: &[u8]) -> Result<(), BackendError> {
        match self {
            Self::Serial(u) => u.serial_write(data),
            _ => Err(BackendError::Fault),
        }
    }
}
