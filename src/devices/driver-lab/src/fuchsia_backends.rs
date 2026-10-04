// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Real hardware backends for Fuchsia devices.

use crate::platform_provider::MappedMmio;
use fidl_fuchsia_hardware_clock as fclock;
use fidl_fuchsia_hardware_gpio as fgpio;
use fidl_fuchsia_hardware_i2c as fi2c;
use fidl_fuchsia_hardware_reset as freset;
use fidl_fuchsia_hardware_serial as fserial;
use fidl_fuchsia_hardware_spi as fspi;
use lab_proxy_core::hardware_backend::BackendError;
use lab_proxy_core::protocol_resource_adapter::{
    ClockBackend, GpioBackend, I2cBackend, ResetBackend, ResourceBackend, SerialBackend, SpiBackend,
};
use lab_proxy_core::state_bank::StateBank;

/// Volatile GPIO pin backend using synchronous FIDL proxy.
pub struct FuchsiaGpio {
    proxy: fgpio::GpioSynchronousProxy,
}

impl FuchsiaGpio {
    pub fn new(proxy: fgpio::GpioSynchronousProxy) -> Self {
        Self { proxy }
    }
}

impl std::fmt::Debug for FuchsiaGpio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuchsiaGpio").finish()
    }
}

impl GpioBackend for FuchsiaGpio {
    fn read(&mut self) -> Result<bool, BackendError> {
        let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(1));
        match self.proxy.read(deadline) {
            Ok(Ok(val)) => Ok(val),
            Ok(Err(status)) => {
                log::warn!("Gpio.read error: {status}");
                Err(BackendError::Fault)
            }
            Err(e) => {
                log::warn!("Gpio.read fidl error: {e:?}");
                Err(BackendError::Fault)
            }
        }
    }

    fn write(&mut self, value: bool) -> Result<(), BackendError> {
        let mode = if value { fgpio::BufferMode::OutputHigh } else { fgpio::BufferMode::OutputLow };
        let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(1));
        match self.proxy.set_buffer_mode(mode, deadline) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(status)) => {
                log::warn!("Gpio.set_buffer_mode error: {status}");
                Err(BackendError::Fault)
            }
            Err(e) => {
                log::warn!("Gpio.set_buffer_mode fidl error: {e:?}");
                Err(BackendError::Fault)
            }
        }
    }
}

impl ResourceBackend for FuchsiaGpio {
    fn gpio_read(&mut self) -> Result<bool, BackendError> {
        self.read()
    }

    fn gpio_write(&mut self, value: bool) -> Result<(), BackendError> {
        self.write(value)
    }
}

/// Volatile I2C device backend using synchronous FIDL proxy.
pub struct FuchsiaI2c {
    proxy: fi2c::DeviceSynchronousProxy,
}

impl FuchsiaI2c {
    pub fn new(proxy: fi2c::DeviceSynchronousProxy) -> Self {
        Self { proxy }
    }
}

impl std::fmt::Debug for FuchsiaI2c {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuchsiaI2c").finish()
    }
}

impl I2cBackend for FuchsiaI2c {
    fn transfer(&mut self, write_data: &[u8], read_length: usize) -> Result<Vec<u8>, BackendError> {
        let mut transactions = Vec::new();
        if !write_data.is_empty() {
            transactions.push(fi2c::Transaction {
                data_transfer: Some(fi2c::DataTransfer::WriteData(write_data.to_vec())),
                stop: Some(read_length == 0),
                ..Default::default()
            });
        }
        if read_length > 0 {
            transactions.push(fi2c::Transaction {
                data_transfer: Some(fi2c::DataTransfer::ReadSize(read_length as u32)),
                stop: Some(true),
                ..Default::default()
            });
        }
        if transactions.is_empty() {
            return Ok(Vec::new());
        }
        let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(1));
        match self.proxy.transfer(&transactions, deadline) {
            Ok(Ok(mut read_data)) => {
                if read_length > 0 {
                    if let Some(data) = read_data.pop() {
                        return Ok(data);
                    }
                    Ok(Vec::new())
                } else {
                    Ok(Vec::new())
                }
            }
            Ok(Err(status)) => {
                log::warn!("I2c.transfer error: {status}");
                Err(BackendError::Fault)
            }
            Err(e) => {
                log::warn!("I2c.transfer fidl error: {e:?}");
                Err(BackendError::Fault)
            }
        }
    }
}

impl ResourceBackend for FuchsiaI2c {
    fn i2c_transfer(
        &mut self,
        write_data: &[u8],
        read_length: usize,
    ) -> Result<Vec<u8>, BackendError> {
        self.transfer(write_data, read_length)
    }
}

/// Volatile SPI device backend using synchronous FIDL proxy.
pub struct FuchsiaSpi {
    proxy: fspi::DeviceSynchronousProxy,
}

impl FuchsiaSpi {
    pub fn new(proxy: fspi::DeviceSynchronousProxy) -> Self {
        Self { proxy }
    }
}

impl std::fmt::Debug for FuchsiaSpi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuchsiaSpi").finish()
    }
}

impl SpiBackend for FuchsiaSpi {
    fn transmit(&mut self, tx_data: &[u8]) -> Result<Vec<u8>, BackendError> {
        let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(1));
        match self.proxy.exchange_vector(tx_data, deadline) {
            Ok((status, rxdata)) => {
                if status == 0 {
                    Ok(rxdata)
                } else {
                    log::warn!("Spi.exchange_vector error: {status}");
                    Err(BackendError::Fault)
                }
            }
            Err(e) => {
                log::warn!("Spi.exchange_vector fidl error: {e:?}");
                Err(BackendError::Fault)
            }
        }
    }
}

impl ResourceBackend for FuchsiaSpi {
    fn spi_transmit(&mut self, tx_data: &[u8]) -> Result<Vec<u8>, BackendError> {
        self.transmit(tx_data)
    }
}

/// Volatile Clock backend using synchronous FIDL proxy.
pub struct FuchsiaClock {
    proxy: fclock::ClockSynchronousProxy,
}

impl FuchsiaClock {
    pub fn new(proxy: fclock::ClockSynchronousProxy) -> Self {
        Self { proxy }
    }
}

impl std::fmt::Debug for FuchsiaClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuchsiaClock").finish()
    }
}

impl ClockBackend for FuchsiaClock {
    fn enable(&mut self) -> Result<(), BackendError> {
        let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(1));
        match self.proxy.enable(deadline) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(status)) => {
                log::warn!("Clock.enable error: {status}");
                Err(BackendError::Fault)
            }
            Err(e) => {
                log::warn!("Clock.enable fidl error: {e:?}");
                Err(BackendError::Fault)
            }
        }
    }

    fn disable(&mut self) -> Result<(), BackendError> {
        let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(1));
        match self.proxy.disable(deadline) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(status)) => {
                log::warn!("Clock.disable error: {status}");
                Err(BackendError::Fault)
            }
            Err(e) => {
                log::warn!("Clock.disable fidl error: {e:?}");
                Err(BackendError::Fault)
            }
        }
    }

    fn is_enabled(&mut self) -> Result<bool, BackendError> {
        let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(1));
        match self.proxy.is_enabled(deadline) {
            Ok(Ok(val)) => Ok(val),
            Ok(Err(status)) => {
                log::warn!("Clock.is_enabled error: {status}");
                Err(BackendError::Fault)
            }
            Err(e) => {
                log::warn!("Clock.is_enabled fidl error: {e:?}");
                Err(BackendError::Fault)
            }
        }
    }

    fn set_rate(&mut self, hz: u64) -> Result<(), BackendError> {
        let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(1));
        match self.proxy.set_rate(hz, deadline) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(status)) => {
                log::warn!("Clock.set_rate error: {status}");
                Err(BackendError::Fault)
            }
            Err(e) => {
                log::warn!("Clock.set_rate fidl error: {e:?}");
                Err(BackendError::Fault)
            }
        }
    }

    fn query_rate(&mut self, hz_in: u64) -> Result<u64, BackendError> {
        let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(1));
        match self.proxy.query_supported_rate(hz_in, deadline) {
            Ok(Ok(val)) => Ok(val),
            Ok(Err(status)) => {
                log::warn!("Clock.query_supported_rate error: {status}");
                Err(BackendError::Fault)
            }
            Err(e) => {
                log::warn!("Clock.query_supported_rate fidl error: {e:?}");
                Err(BackendError::Fault)
            }
        }
    }

    fn get_rate(&mut self) -> Result<u64, BackendError> {
        let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(1));
        match self.proxy.get_rate(deadline) {
            Ok(Ok(val)) => Ok(val),
            Ok(Err(status)) => {
                log::warn!("Clock.get_rate error: {status}");
                Err(BackendError::Fault)
            }
            Err(e) => {
                log::warn!("Clock.get_rate fidl error: {e:?}");
                Err(BackendError::Fault)
            }
        }
    }
}

impl ResourceBackend for FuchsiaClock {
    fn clock_enable(&mut self) -> Result<(), BackendError> {
        self.enable()
    }
    fn clock_disable(&mut self) -> Result<(), BackendError> {
        self.disable()
    }
    fn clock_is_enabled(&mut self) -> Result<bool, BackendError> {
        self.is_enabled()
    }
    fn clock_set_rate(&mut self, hz: u64) -> Result<(), BackendError> {
        self.set_rate(hz)
    }
    fn clock_query_rate(&mut self, hz_in: u64) -> Result<u64, BackendError> {
        self.query_rate(hz_in)
    }
    fn clock_get_rate(&mut self) -> Result<u64, BackendError> {
        self.get_rate()
    }
}

/// Volatile Reset backend using synchronous FIDL proxy.
pub struct FuchsiaReset {
    proxy: freset::ResetSynchronousProxy,
}

impl FuchsiaReset {
    pub fn new(proxy: freset::ResetSynchronousProxy) -> Self {
        Self { proxy }
    }
}

impl std::fmt::Debug for FuchsiaReset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuchsiaReset").finish()
    }
}

impl ResetBackend for FuchsiaReset {
    fn assert(&mut self) -> Result<(), BackendError> {
        let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(1));
        match self.proxy.assert(deadline) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(status)) => {
                log::warn!("Reset.assert error: {status}");
                Err(BackendError::Fault)
            }
            Err(e) => {
                log::warn!("Reset.assert fidl error: {e:?}");
                Err(BackendError::Fault)
            }
        }
    }

    fn deassert(&mut self) -> Result<(), BackendError> {
        let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(1));
        match self.proxy.deassert(deadline) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(status)) => {
                log::warn!("Reset.deassert error: {status}");
                Err(BackendError::Fault)
            }
            Err(e) => {
                log::warn!("Reset.deassert fidl error: {e:?}");
                Err(BackendError::Fault)
            }
        }
    }

    fn toggle(&mut self) -> Result<(), BackendError> {
        let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(1));
        match self.proxy.toggle(deadline) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(status)) => {
                log::warn!("Reset.toggle error: {status}");
                Err(BackendError::Fault)
            }
            Err(e) => {
                log::warn!("Reset.toggle fidl error: {e:?}");
                Err(BackendError::Fault)
            }
        }
    }

    fn status(&mut self) -> Result<bool, BackendError> {
        let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(1));
        match self.proxy.status(deadline) {
            Ok(Ok(val)) => Ok(val),
            Ok(Err(status)) => {
                log::warn!("Reset.status error: {status}");
                Err(BackendError::Fault)
            }
            Err(e) => {
                log::warn!("Reset.status fidl error: {e:?}");
                Err(BackendError::Fault)
            }
        }
    }
}

impl ResourceBackend for FuchsiaReset {
    fn reset_assert(&mut self) -> Result<(), BackendError> {
        self.assert()
    }
    fn reset_deassert(&mut self) -> Result<(), BackendError> {
        self.deassert()
    }
    fn reset_toggle(&mut self) -> Result<(), BackendError> {
        self.toggle()
    }
    fn reset_status(&mut self) -> Result<bool, BackendError> {
        self.status()
    }
}

/// Volatile Serial backend using synchronous FIDL proxy.
pub struct FuchsiaSerial {
    proxy: fserial::DeviceSynchronousProxy,
}

impl FuchsiaSerial {
    pub fn new(proxy: fserial::DeviceSynchronousProxy) -> Self {
        Self { proxy }
    }
}

impl std::fmt::Debug for FuchsiaSerial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FuchsiaSerial").finish()
    }
}

impl SerialBackend for FuchsiaSerial {
    fn read(&mut self) -> Result<Vec<u8>, BackendError> {
        let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(1));
        match self.proxy.read(deadline) {
            Ok(Ok(val)) => Ok(val),
            Ok(Err(status)) => {
                log::warn!("Serial.read error: {status}");
                Err(BackendError::Fault)
            }
            Err(e) => {
                log::warn!("Serial.read fidl error: {e:?}");
                Err(BackendError::Fault)
            }
        }
    }

    fn write(&mut self, data: &[u8]) -> Result<(), BackendError> {
        let deadline = zx::MonotonicInstant::after(zx::Duration::from_seconds(1));
        match self.proxy.write(data, deadline) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(status)) => {
                log::warn!("Serial.write error: {status}");
                Err(BackendError::Fault)
            }
            Err(e) => {
                log::warn!("Serial.write fidl error: {e:?}");
                Err(BackendError::Fault)
            }
        }
    }
}

impl ResourceBackend for FuchsiaSerial {
    fn serial_read(&mut self) -> Result<Vec<u8>, BackendError> {
        self.read()
    }
    fn serial_write(&mut self, data: &[u8]) -> Result<(), BackendError> {
        self.write(data)
    }
}

/// Heterogeneous live hardware backend covering MMIO, software state banks, GPIO, I2C, SPI, Clock, Reset, Serial, and Interrupts.
#[derive(Debug)]
pub enum LiveBackend {
    Mmio(MappedMmio),
    State(StateBank),
    Gpio(FuchsiaGpio),
    I2c(FuchsiaI2c),
    Spi(FuchsiaSpi),
    Clock(FuchsiaClock),
    Reset(FuchsiaReset),
    Serial(FuchsiaSerial),
    Interrupt,
}

impl ResourceBackend for LiveBackend {
    fn read32(&mut self, offset: u64) -> Result<u32, BackendError> {
        match self {
            Self::Mmio(m) => m.read32(offset),
            Self::State(s) => s.read32(offset),
            _ => Err(BackendError::Fault),
        }
    }

    fn write32(&mut self, offset: u64, value: u32) -> Result<(), BackendError> {
        match self {
            Self::Mmio(m) => m.write32(offset, value),
            Self::State(s) => s.write32(offset, value),
            _ => Err(BackendError::Fault),
        }
    }

    fn barrier(&mut self) {
        match self {
            Self::Mmio(m) => m.barrier(),
            Self::State(s) => s.barrier(),
            _ => {}
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
            Self::Serial(s) => s.serial_read(),
            _ => Err(BackendError::Fault),
        }
    }

    fn serial_write(&mut self, data: &[u8]) -> Result<(), BackendError> {
        match self {
            Self::Serial(s) => s.serial_write(data),
            _ => Err(BackendError::Fault),
        }
    }
}
