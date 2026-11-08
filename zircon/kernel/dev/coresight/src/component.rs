// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM CoreSight component identification registers and constants, mirroring
//! `<dev/coresight/component.h>`.

use core::ffi::c_char;
use core::fmt;

use bitrs::{bitfield_repr, layout};
use regio::{AccessRestrictsTo, Accessible, MmioBank, MmioPtr, Offset, Ro};
use zr::static_assert;

/// Typically components are 4KiB in size; the spec permits them to be larger.
pub const MINIMUM_COMPONENT_SIZE: usize = 4096;

/// Device architecture constants for ARM-designed components.
pub mod arm {
    /// `DEVARCH.ARCHITECT`.
    pub const ARCHITECT: u16 = 0x23b;

    /// `DEVARCH.ARCHID` values.
    pub mod archid {
        /// Cross-Trigger Interface (CTI).
        pub const CTI: u16 = 0x1a14;
        /// Embedded Trace Macrocell (ETM) v3.
        pub const ETM3: u16 = 0x3a13;
        /// Embedded Trace Macrocell (ETM) v4.
        pub const ETM4: u16 = 0x4a13;
        /// Performance Monitors Extension (PMU) v2.
        pub const PMU2: u16 = 0x1a16;
        /// Performance Monitors Extension (PMU) v3.
        pub const PMU3: u16 = 0x2a16;
        /// Class 0x9 ROM Table.
        pub const ROM_TABLE: u16 = 0x0af7;
        /// ARM v8.0-A Core Debug Interface.
        pub const CORE_DEBUG_INTERFACE_8_0A: u16 = 0x6a15;
        /// ARM v8.1-A Core Debug Interface.
        pub const CORE_DEBUG_INTERFACE_8_1A: u16 = 0x7a15;
        /// ARM v8.2-A Core Debug Interface.
        pub const CORE_DEBUG_INTERFACE_8_2A: u16 = 0x8a15;
    }

    /// Peripheral part ID (`PARTID`) values.
    pub mod partid {
        /// Cross-Trigger Interface (SoC400 generation).
        pub const CTI_400: u16 = 0x0906;
        /// Cross-Trigger Interface (SoC600 generation).
        pub const CTI_600: u16 = 0x09ed;
        /// Embedded Trace Buffer (ETB).
        pub const ETB: u16 = 0x0907;
        /// Timestamp Generator.
        pub const TIMESTAMP_GENERATOR: u16 = 0x0101;
        /// Trace Memory Controller (TMC).
        pub const TMC: u16 = 0x0961;
        /// Trace Port Interface Unit (TPIU).
        pub const TPIU: u16 = 0x0912;
        /// Trace Funnel.
        pub const TRACE_FUNNEL: u16 = 0x0908;
        /// Trace Replicator.
        pub const TRACE_REPLICATOR: u16 = 0x0909;
    }
}

/// Component class identifier encoded in `CIDR1.CLASS` (`ComponentIdRegister::Class`).
#[bitfield_repr(u8)]
#[derive(Copy, Clone)]
pub enum ClassId {
    /// Generic verification component (`0x0`).
    GenericVerification = 0x0,
    /// Class `0x1` ROM table (`0x1`).
    RomTable0x1 = 0x1,
    /// CoreSight component (`0x9`).
    CoreSight = 0x9,
    /// Peripheral test block (`0xb`).
    PeripheralTestBlock = 0xb,
    /// Generic IP component (`0xe`).
    GenericIp = 0xe,
    /// Non-standard component (`0xf`). For older components without standardized registers.
    NonStandard = 0xf,
}

impl ClassId {
    /// Returns the human-readable description of this component class.
    #[inline]
    pub const fn as_str(self) -> &'static str {
        class_id_to_string(self)
    }
}

impl fmt::Display for ClassId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

static_assert!(size_of::<ClassId>() == size_of::<u8>());
static_assert!(align_of::<ClassId>() == align_of::<u8>());

/// Returns the human-readable description of a [`ClassId`].
#[inline]
pub const fn class_id_to_string(classid: ClassId) -> &'static str {
    match classid {
        ClassId::GenericVerification => "generic verification",
        ClassId::RomTable0x1 => "0x1 ROM table",
        ClassId::CoreSight => "CoreSight",
        ClassId::PeripheralTestBlock => "peripheral test block",
        ClassId::GenericIp => "generic IP",
        ClassId::NonStandard => "non-standard",
    }
}

layout!({
    /// [CS] B2.2.1
    /// The first component identification register (`CIDR1`).
    pub struct ComponentIdRegister(u32);
    {
        let __ @ 31..8;
        /// Should conventionally be called `class` to match the spec, but that is a
        /// reserved keyword.
        let classid @ 7..4: ClassId;
        let prmbl_1 @ 3..0;
    }
});

/// Byte offset of [`ComponentIdRegister`] (`CIDR1`) within a component page.
pub const CIDR1: Offset<ComponentIdRegister, Ro> = Offset::new(0xff4);

layout!({
    /// [CS] B2.2.2
    /// Peripheral ID0 register (`PIDR0`).
    pub struct PeripheralId0Register(u32);
    {
        let __ @ 31..8;
        let part0 @ 7..0;
    }
});

/// Byte offset of [`PeripheralId0Register`] (`PIDR0`) within a component page.
pub const PIDR0: Offset<PeripheralId0Register, Ro> = Offset::new(0xfe0);

layout!({
    /// [CS] B2.2.2
    /// Peripheral ID1 register (`PIDR1`).
    pub struct PeripheralId1Register(u32);
    {
        let __ @ 31..8;
        let des0 @ 7..4;
        let part1 @ 3..0;
    }
});

/// Byte offset of [`PeripheralId1Register`] (`PIDR1`) within a component page.
pub const PIDR1: Offset<PeripheralId1Register, Ro> = Offset::new(0xfe4);

layout!({
    /// [CS] B2.2.2
    /// Peripheral ID2 register (`PIDR2`).
    pub struct PeripheralId2Register(u32);
    {
        let __ @ 31..8;
        let revision @ 7..4;
        let jedec @ 3;
        let des1 @ 2..0;
    }
});

/// Byte offset of [`PeripheralId2Register`] (`PIDR2`) within a component page.
pub const PIDR2: Offset<PeripheralId2Register, Ro> = Offset::new(0xfe8);

layout!({
    /// [CS] B2.2.2
    /// Peripheral ID4 register (`PIDR4`).
    pub struct PeripheralId4Register(u32);
    {
        let __ @ 31..8;
        let size @ 7..4;
        let des2 @ 3..0;
    }
});

/// Byte offset of [`PeripheralId4Register`] (`PIDR4`) within a component page.
pub const PIDR4: Offset<PeripheralId4Register, Ro> = Offset::new(0xfd0);

/// [CS] B2.2.2
/// JEDEC ID of the designer.
///
/// # Safety
///
/// `bank` must wrap a valid, mapped CoreSight component MMIO region of at least
/// [`MINIMUM_COMPONENT_SIZE`] bytes that is safe to read for the duration of the call.
#[inline]
pub unsafe fn get_designer<Access>(bank: &MmioBank<u32, Access>) -> u16
where
    Access: Accessible + AccessRestrictsTo<Ro>,
{
    // SAFETY: Caller guarantees `bank` covers a valid component MMIO region of at least
    // `MINIMUM_COMPONENT_SIZE` bytes, which includes `PIDR1`, `PIDR2`, and `PIDR4`.
    unsafe {
        let des0 = u16::from(bank.at(PIDR1).read().des0());
        let des1 = u16::from(bank.at(PIDR2).read().des1());
        let des2 = u16::from(bank.at(PIDR4).read().des2());
        (des2 << 7) | (des1 << 4) | des0
    }
}

/// [CS] B2.2.2
/// This number is an ID chosen by the designer.
///
/// # Safety
///
/// `bank` must wrap a valid, mapped CoreSight component MMIO region of at least
/// [`MINIMUM_COMPONENT_SIZE`] bytes that is safe to read for the duration of the call.
#[inline]
pub unsafe fn get_part_id<Access>(bank: &MmioBank<u32, Access>) -> u16
where
    Access: Accessible + AccessRestrictsTo<Ro>,
{
    // SAFETY: Caller guarantees `bank` covers a valid component MMIO region of at least
    // `MINIMUM_COMPONENT_SIZE` bytes, which includes `PIDR0` and `PIDR1`.
    unsafe {
        let part0 = u16::from(bank.at(PIDR0).read().part0());
        let part1 = u16::from(bank.at(PIDR1).read().part1());
        (part1 << 8) | part0
    }
}

layout!({
    /// [CS] B2.3.3
    /// Used to determine whether two components have an affinity with one another
    /// (e.g., if both correspond to the same CPU).
    ///
    /// This 64-bit register is actually an amalgamation of the two device affinity
    /// registers, `DEVAFF0` and `DEVAFF1`. We combine them as, in practice, the
    /// resulting value is typically that of the 64-bit `MPIDR` register of the
    /// associated CPU.
    pub struct DeviceAffinityRegister(u64);
    {}
});

/// Byte offset of [`DeviceAffinityRegister`] (`DEVAFF0`/`DEVAFF1`) within a component page.
pub const DEVAFF: Offset<DeviceAffinityRegister, Ro> = Offset::new(0xfa8);

layout!({
    /// [CS] B2.3.4
    /// Identifies the architect and architecture of a CoreSight component (`DEVARCH`).
    pub struct DeviceArchRegister(u32);
    {
        let architect @ 31..21;
        let present @ 20;
        let revision @ 19..16;
        let archid @ 15..0;
    }
});

/// Byte offset of [`DeviceArchRegister`] (`DEVARCH`) within a component page.
pub const DEVARCH: Offset<DeviceArchRegister, Ro> = Offset::new(0xfbc);

/// Major component type encoded in `DEVTYPE.MAJOR` (`DeviceTypeRegister::MajorType`).
#[bitfield_repr(u8)]
#[derive(Copy, Clone)]
pub enum MajorType {
    /// Miscellaneous component (`0x0`).
    Miscellaneous = 0x0,
    /// Trace sink (`0x1`).
    TraceSink = 0x1,
    /// Trace link (`0x2`).
    TraceLink = 0x2,
    /// Trace source (`0x3`).
    TraceSource = 0x3,
    /// Debug control (`0x4`).
    DebugControl = 0x4,
    /// Debug logic (`0x5`).
    DebugLogic = 0x5,
    /// Performance monitor (`0x6`).
    PerformanceMonitor = 0x6,
}

static_assert!(size_of::<MajorType>() == size_of::<u8>());
static_assert!(align_of::<MajorType>() == align_of::<u8>());

/// Combined CoreSight device type (`DeviceTypeRegister::Type`) decoded from `(major, sub)`.
#[bitfield_repr(u32)]
#[derive(Copy, Clone)]
pub enum DeviceType {
    /// Unknown or reserved `(major, sub)` combination.
    Unknown = 0,
    /// Validation component (`Miscellaneous`, `0x4`).
    ValidationComponent,
    /// Trace port (`TraceSink`, `0x1`).
    TracePort,
    /// Trace buffer (`TraceSink`, `0x2`).
    TraceBuffer,
    /// Trace router (`TraceSink`, `0x3`).
    TraceRouter,
    /// Trace funnel (`TraceLink`, `0x1`).
    TraceFunnel,
    /// Trace filter (`TraceLink`, `0x2`).
    TraceFilter,
    /// Trace FIFO (`TraceLink`, `0x3`).
    TraceFifo,
    /// CPU trace source (`TraceSource`, `0x1`).
    CpuTraceSource,
    /// DSP trace source (`TraceSource`, `0x2`).
    DspTraceSource,
    /// Data engine or coprocessor trace source (`TraceSource`, `0x3`).
    DataEngineTraceSource,
    /// Bus trace source (`TraceSource`, `0x4`).
    BusTraceSource,
    /// Software trace source (`TraceSource`, `0x6`).
    SoftwareTraceSource,
    /// Trigger matrix (`DebugControl`, `0x1`).
    TriggerMatrix,
    /// Debug authentication module (`DebugControl`, `0x2`).
    DebugAuthenticationModule,
    /// Power requestor (`DebugControl`, `0x3`).
    PowerRequestor,
    /// CPU debug logic (`DebugLogic`, `0x1`).
    CpuDebugLogic,
    /// DSP debug logic (`DebugLogic`, `0x2`).
    DspDebugLogic,
    /// Data engine or coprocessor debug logic (`DebugLogic`, `0x3`).
    DataEngineDebugLogic,
    /// Bus debug logic (`DebugLogic`, `0x4`).
    BusDebugLogic,
    /// Memory debug logic (`DebugLogic`, `0x5`).
    MemoryDebugLogic,
    /// CPU performance monitor (`PerformanceMonitor`, `0x1`).
    CpuPerformanceMonitor,
    /// DSP performance monitor (`PerformanceMonitor`, `0x2`).
    DspPerformanceMonitor,
    /// Data engine or coprocessor performance monitor (`PerformanceMonitor`, `0x3`).
    DataEnginePerformanceMonitor,
    /// Bus performance monitor (`PerformanceMonitor`, `0x4`).
    BusPerformanceMonitor,
    /// MMU performance monitor (`PerformanceMonitor`, `0x5`).
    MmuPerformanceMonitor,
}

impl DeviceType {
    /// Returns the human-readable description of this device type.
    #[inline]
    pub const fn as_str(self) -> &'static str {
        device_type_to_string(self)
    }
}

impl fmt::Display for DeviceType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

static_assert!(size_of::<DeviceType>() == size_of::<u32>());
static_assert!(align_of::<DeviceType>() == align_of::<u32>());

/// Returns the human-readable description of a [`DeviceType`].
#[inline]
pub const fn device_type_to_string(dev_type: DeviceType) -> &'static str {
    match dev_type {
        DeviceType::Unknown => "unknown",
        DeviceType::ValidationComponent => "validation component",
        DeviceType::TracePort => "trace port",
        DeviceType::TraceBuffer => "trace buffer",
        DeviceType::TraceRouter => "trace router",
        DeviceType::TraceFunnel => "trace funnel",
        DeviceType::TraceFilter => "trace filter",
        DeviceType::TraceFifo => "trace FIFO",
        DeviceType::CpuTraceSource => "CPU trace source",
        DeviceType::DspTraceSource => "DSP trace source",
        DeviceType::DataEngineTraceSource => "data engine or coprocessor trace source",
        DeviceType::BusTraceSource => "bus trace source",
        DeviceType::SoftwareTraceSource => "software trace source",
        DeviceType::TriggerMatrix => "trigger matrix",
        DeviceType::DebugAuthenticationModule => "debug authentication module",
        DeviceType::PowerRequestor => "power requestor",
        DeviceType::CpuDebugLogic => "CPU debug logic",
        DeviceType::DspDebugLogic => "DSP debug logic",
        DeviceType::DataEngineDebugLogic => "data engine or coprocessor debug logic",
        DeviceType::BusDebugLogic => "bus debug logic",
        DeviceType::MemoryDebugLogic => "memory debug logic",
        DeviceType::CpuPerformanceMonitor => "CPU performance monitor",
        DeviceType::DspPerformanceMonitor => "DSP performance monitor",
        DeviceType::DataEnginePerformanceMonitor => {
            "Data engine or coprocessor performance monitor"
        }
        DeviceType::BusPerformanceMonitor => "bus performance monitor",
        DeviceType::MmuPerformanceMonitor => "MMU performance monitor",
    }
}

layout!({
    /// [CS] B2.3.8
    /// Gives high-level information about the type of a CoreSight component (`DEVTYPE`).
    pub struct DeviceTypeRegister(u32);
    {
        let __ @ 31..8;
        /// Subtype.
        let sub @ 7..4;
        let major @ 3..0: MajorType;
    }
});

impl DeviceTypeRegister {
    /// The type encoded by `sub` and `major` fields.
    #[inline]
    pub fn device_type(self) -> DeviceType {
        // Allows us to switch across (major, sub-) type pairs.

        // [CS] Table B2-9.
        match (self.try_major(), self.sub()) {
            (Ok(MajorType::Miscellaneous), 0x4) => DeviceType::ValidationComponent,
            (Ok(MajorType::TraceSink), 0x1) => DeviceType::TracePort,
            (Ok(MajorType::TraceSink), 0x2) => DeviceType::TraceBuffer,
            (Ok(MajorType::TraceSink), 0x3) => DeviceType::TraceRouter,
            (Ok(MajorType::TraceLink), 0x1) => DeviceType::TraceFunnel,
            (Ok(MajorType::TraceLink), 0x2) => DeviceType::TraceFilter,
            (Ok(MajorType::TraceLink), 0x3) => DeviceType::TraceFifo,
            (Ok(MajorType::TraceSource), 0x1) => DeviceType::CpuTraceSource,
            (Ok(MajorType::TraceSource), 0x2) => DeviceType::DspTraceSource,
            (Ok(MajorType::TraceSource), 0x3) => DeviceType::DataEngineTraceSource,
            (Ok(MajorType::TraceSource), 0x4) => DeviceType::BusTraceSource,
            (Ok(MajorType::TraceSource), 0x6) => DeviceType::SoftwareTraceSource,
            (Ok(MajorType::DebugControl), 0x1) => DeviceType::TriggerMatrix,
            (Ok(MajorType::DebugControl), 0x2) => DeviceType::DebugAuthenticationModule,
            (Ok(MajorType::DebugControl), 0x3) => DeviceType::PowerRequestor,
            (Ok(MajorType::DebugLogic), 0x1) => DeviceType::CpuDebugLogic,
            (Ok(MajorType::DebugLogic), 0x2) => DeviceType::DspDebugLogic,
            (Ok(MajorType::DebugLogic), 0x3) => DeviceType::DataEngineDebugLogic,
            (Ok(MajorType::DebugLogic), 0x4) => DeviceType::BusDebugLogic,
            (Ok(MajorType::DebugLogic), 0x5) => DeviceType::MemoryDebugLogic,
            (Ok(MajorType::PerformanceMonitor), 0x1) => DeviceType::CpuPerformanceMonitor,
            (Ok(MajorType::PerformanceMonitor), 0x2) => DeviceType::DspPerformanceMonitor,
            (Ok(MajorType::PerformanceMonitor), 0x3) => DeviceType::DataEnginePerformanceMonitor,
            (Ok(MajorType::PerformanceMonitor), 0x4) => DeviceType::BusPerformanceMonitor,
            (Ok(MajorType::PerformanceMonitor), 0x5) => DeviceType::MmuPerformanceMonitor,
            _ => DeviceType::Unknown,
        }
    }
}

/// Byte offset of [`DeviceTypeRegister`] (`DEVTYPE`) within a component page.
pub const DEVTYPE: Offset<DeviceTypeRegister, Ro> = Offset::new(0xfcc);

/// FFI trampoline for `coresight::DeviceTypeRegister::type()`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_coresight_device_type_from_raw(reg_value: u32) -> u32 {
    DeviceTypeRegister::from(reg_value).device_type() as u32
}

/// FFI trampoline for `coresight::ToString(ComponentIdRegister::Class)`.
///
/// # Safety
///
/// `out_len` must be a valid, aligned pointer writable for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_coresight_class_id_to_string(
    raw_classid: u8,
    out_len: *mut usize,
) -> *const c_char {
    let s =
        zerocopy::TryFromBytes::try_read_from_bytes(zerocopy::IntoBytes::as_bytes(&raw_classid))
            .map_or("unknown", class_id_to_string);
    // SAFETY: Caller guarantees `out_len` is a valid, aligned, writable pointer.
    unsafe {
        out_len.write(s.len());
    }
    s.as_ptr().cast::<c_char>()
}

/// FFI trampoline for `coresight::ToString(DeviceTypeRegister::Type)`.
///
/// # Safety
///
/// `out_len` must be a valid, aligned pointer writable for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_coresight_device_type_to_string(
    raw_dev_type: u32,
    out_len: *mut usize,
) -> *const c_char {
    let dev_type =
        zerocopy::TryFromBytes::try_read_from_bytes(zerocopy::IntoBytes::as_bytes(&raw_dev_type))
            .unwrap_or(DeviceType::Unknown);
    let s = device_type_to_string(dev_type);
    // SAFETY: Caller guarantees `out_len` is a valid, aligned, writable pointer.
    unsafe {
        out_len.write(s.len());
    }
    s.as_ptr().cast::<c_char>()
}

/// FFI trampoline for `coresight::GetDesigner`.
///
/// # Safety
///
/// `base` must be a valid, 4-byte aligned pointer to a mapped CoreSight component MMIO region of
/// at least [`MINIMUM_COMPONENT_SIZE`] bytes that is safe to read for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_coresight_get_designer(base: *const u32) -> u16 {
    // SAFETY: Caller guarantees `base` is a valid, aligned pointer to a mapped component MMIO
    // region of at least `MINIMUM_COMPONENT_SIZE` bytes.
    unsafe {
        let ptr = MmioPtr::<u32, Ro>::new(base);
        let bank = MmioBank::new(ptr, MINIMUM_COMPONENT_SIZE);
        get_designer(&bank)
    }
}

/// FFI trampoline for `coresight::GetPartId`.
///
/// # Safety
///
/// `base` must be a valid, 4-byte aligned pointer to a mapped CoreSight component MMIO region of
/// at least [`MINIMUM_COMPONENT_SIZE`] bytes that is safe to read for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_coresight_get_part_id(base: *const u32) -> u16 {
    // SAFETY: Caller guarantees `base` is a valid, aligned pointer to a mapped component MMIO
    // region of at least `MINIMUM_COMPONENT_SIZE` bytes.
    unsafe {
        let ptr = MmioPtr::<u32, Ro>::new(base);
        let bank = MmioBank::new(ptr, MINIMUM_COMPONENT_SIZE);
        get_part_id(&bank)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C, align(8))]
    struct FakeComponentPage {
        words: [u32; MINIMUM_COMPONENT_SIZE / size_of::<u32>()],
    }

    impl FakeComponentPage {
        const fn new() -> Self {
            Self { words: [0; MINIMUM_COMPONENT_SIZE / size_of::<u32>()] }
        }

        fn write32(&mut self, offset: usize, val: u32) {
            self.words[offset / size_of::<u32>()] = val;
        }

        fn write64(&mut self, offset: usize, val: u64) {
            let idx = offset / size_of::<u32>();
            let bytes = val.to_ne_bytes();
            self.words[idx] = u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            self.words[idx + 1] = u32::from_ne_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        }

        fn bank(&self) -> MmioBank<u32, Ro> {
            // SAFETY: `self.words` is a valid, 8-byte aligned 4 KiB region alive for the borrow.
            let ptr = unsafe { MmioPtr::<u32, Ro>::new(self.words.as_ptr()) };
            MmioBank::new(ptr, MINIMUM_COMPONENT_SIZE)
        }

        fn bank64(&self) -> MmioBank<u64, Ro> {
            // SAFETY: `self.words` is a valid, 8-byte aligned 4 KiB region alive for the borrow.
            let ptr = unsafe { MmioPtr::<u64, Ro>::new(self.words.as_ptr().cast::<u64>()) };
            MmioBank::new(ptr, MINIMUM_COMPONENT_SIZE)
        }

        fn as_ptr(&self) -> *const u32 {
            self.words.as_ptr()
        }
    }

    #[test]
    fn device_type_decoding_and_strings() {
        let cases = [
            (
                (MajorType::Miscellaneous, 0x4),
                DeviceType::ValidationComponent,
                "validation component",
            ),
            ((MajorType::TraceSink, 0x1), DeviceType::TracePort, "trace port"),
            ((MajorType::TraceSink, 0x2), DeviceType::TraceBuffer, "trace buffer"),
            ((MajorType::TraceSink, 0x3), DeviceType::TraceRouter, "trace router"),
            ((MajorType::TraceLink, 0x1), DeviceType::TraceFunnel, "trace funnel"),
            ((MajorType::TraceLink, 0x2), DeviceType::TraceFilter, "trace filter"),
            ((MajorType::TraceLink, 0x3), DeviceType::TraceFifo, "trace FIFO"),
            ((MajorType::TraceSource, 0x1), DeviceType::CpuTraceSource, "CPU trace source"),
            ((MajorType::TraceSource, 0x2), DeviceType::DspTraceSource, "DSP trace source"),
            (
                (MajorType::TraceSource, 0x3),
                DeviceType::DataEngineTraceSource,
                "data engine or coprocessor trace source",
            ),
            ((MajorType::TraceSource, 0x4), DeviceType::BusTraceSource, "bus trace source"),
            (
                (MajorType::TraceSource, 0x6),
                DeviceType::SoftwareTraceSource,
                "software trace source",
            ),
            ((MajorType::DebugControl, 0x1), DeviceType::TriggerMatrix, "trigger matrix"),
            (
                (MajorType::DebugControl, 0x2),
                DeviceType::DebugAuthenticationModule,
                "debug authentication module",
            ),
            ((MajorType::DebugControl, 0x3), DeviceType::PowerRequestor, "power requestor"),
            ((MajorType::DebugLogic, 0x1), DeviceType::CpuDebugLogic, "CPU debug logic"),
            ((MajorType::DebugLogic, 0x2), DeviceType::DspDebugLogic, "DSP debug logic"),
            (
                (MajorType::DebugLogic, 0x3),
                DeviceType::DataEngineDebugLogic,
                "data engine or coprocessor debug logic",
            ),
            ((MajorType::DebugLogic, 0x4), DeviceType::BusDebugLogic, "bus debug logic"),
            ((MajorType::DebugLogic, 0x5), DeviceType::MemoryDebugLogic, "memory debug logic"),
            (
                (MajorType::PerformanceMonitor, 0x1),
                DeviceType::CpuPerformanceMonitor,
                "CPU performance monitor",
            ),
            (
                (MajorType::PerformanceMonitor, 0x2),
                DeviceType::DspPerformanceMonitor,
                "DSP performance monitor",
            ),
            (
                (MajorType::PerformanceMonitor, 0x3),
                DeviceType::DataEnginePerformanceMonitor,
                "Data engine or coprocessor performance monitor",
            ),
            (
                (MajorType::PerformanceMonitor, 0x4),
                DeviceType::BusPerformanceMonitor,
                "bus performance monitor",
            ),
            (
                (MajorType::PerformanceMonitor, 0x5),
                DeviceType::MmuPerformanceMonitor,
                "MMU performance monitor",
            ),
        ];

        for ((major, sub), expected_type, expected_str) in cases {
            let mut reg = DeviceTypeRegister::default();
            reg.set_major(major).set_sub(sub);
            assert_eq!(reg.device_type(), expected_type);
            assert_eq!(reg.device_type().as_str(), expected_str);
            assert_eq!(rust_coresight_device_type_from_raw(reg.bits()), expected_type as u32);
        }

        let unknown = DeviceTypeRegister::from(0xff);
        assert_eq!(unknown.device_type(), DeviceType::Unknown);
        assert_eq!(unknown.device_type().as_str(), "unknown");
    }

    #[test]
    fn class_id_decoding_and_strings() {
        assert_eq!(class_id_to_string(ClassId::GenericVerification), "generic verification");
        assert_eq!(class_id_to_string(ClassId::RomTable0x1), "0x1 ROM table");
        assert_eq!(class_id_to_string(ClassId::CoreSight), "CoreSight");
        assert_eq!(class_id_to_string(ClassId::PeripheralTestBlock), "peripheral test block");
        assert_eq!(class_id_to_string(ClassId::GenericIp), "generic IP");
        assert_eq!(class_id_to_string(ClassId::NonStandard), "non-standard");
    }

    #[test]
    fn designer_and_part_id() {
        let mut pidr0 = PeripheralId0Register::default();
        pidr0.set_part0(0x06);
        let mut pidr1 = PeripheralId1Register::default();
        pidr1.set_part1(0x09).set_des0(0xb);
        let mut pidr2 = PeripheralId2Register::default();
        pidr2.set_des1(0x3).set_jedec(true).set_revision(0x2);
        let mut pidr4 = PeripheralId4Register::default();
        pidr4.set_des2(0x4).set_size(0x0);

        let mut page = FakeComponentPage::new();
        page.write32(PIDR0.value, pidr0.bits());
        page.write32(PIDR1.value, pidr1.bits());
        page.write32(PIDR2.value, pidr2.bits());
        page.write32(PIDR4.value, pidr4.bits());
        page.write64(DEVAFF.value, 0x8000_0001);
        page.write32(CIDR1.value, 0x90);

        let bank = page.bank();
        let bank64 = page.bank64();
        // SAFETY: `page` is a valid, 8-byte aligned 4 KiB component buffer.
        unsafe {
            assert_eq!(get_designer(&bank), arm::ARCHITECT);
            assert_eq!(get_part_id(&bank), arm::partid::CTI_400);
            assert_eq!(rust_coresight_get_designer(page.as_ptr()), arm::ARCHITECT);
            assert_eq!(rust_coresight_get_part_id(page.as_ptr()), arm::partid::CTI_400);
            assert_eq!(bank64.at(DEVAFF).read().bits(), 0x8000_0001);
            assert_eq!(bank.at(CIDR1).read().try_classid().ok(), Some(ClassId::CoreSight));
        }
    }

    #[test]
    fn ffi_trampolines() {
        let mut len = 0usize;
        // SAFETY: `&mut len` is a valid, aligned pointer, and the FFI functions return valid
        // static UTF-8 slices of length `len`.
        unsafe {
            let ptr = rust_coresight_class_id_to_string(ClassId::CoreSight as u8, &mut len);
            let s =
                core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr.cast::<u8>(), len));
            assert_eq!(s, "CoreSight");

            let ptr = rust_coresight_class_id_to_string(0x5, &mut len);
            let s =
                core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr.cast::<u8>(), len));
            assert_eq!(s, "unknown");

            let ptr =
                rust_coresight_device_type_to_string(DeviceType::CpuDebugLogic as u32, &mut len);
            let s =
                core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr.cast::<u8>(), len));
            assert_eq!(s, "CPU debug logic");

            let ptr = rust_coresight_device_type_to_string(0xffff, &mut len);
            let s =
                core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr.cast::<u8>(), len));
            assert_eq!(s, "unknown");
        }
    }
}
