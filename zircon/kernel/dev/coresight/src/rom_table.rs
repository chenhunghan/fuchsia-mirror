// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM CoreSight ROM table registers and tree walker.

use core::mem::{align_of, size_of};

use bitrs::{bitfield_repr, layout};
use regio::{AccessRestrictsTo, Accessible, MmioBank, MmioPtr, Offset, Ro};
use zr::static_assert;

use crate::component::{CIDR1, ClassId, DEVARCH, MINIMUM_COMPONENT_SIZE, arm};

layout!({
    /// [CS] D6.4.4
    /// Class `0x1` ROM table entry register.
    pub struct Class0x1RomEntry(u32);
    {
        let offset @ 31..12;
        let __ @ 11..9;
        let powerid @ 8..4;
        let __ @ 3;
        let powerid_valid @ 2;
        let format @ 1;
        let present @ 0;
    }
});

impl Class0x1RomEntry {
    /// Returns the byte offset of entry `n` within a Class `0x1` ROM table.
    #[inline]
    pub const fn offset_of(n: u32) -> Offset<Self, Ro> {
        Offset::new((n as usize) * size_of::<u32>())
    }
}

layout!({
    /// [CS] D7.5.17
    /// Class `0x9` 32-bit ROM table entry register.
    pub struct Class0x9Rom32BitEntry(u32);
    {
        let offset @ 31..12;
        let __ @ 11..9;
        let powerid @ 8..4;
        let __ @ 3;
        let powerid_valid @ 2;
        let present @ 1..0;
    }
});

impl Class0x9Rom32BitEntry {
    /// Returns the byte offset of entry `n` within a Class `0x9` 32-bit ROM table.
    #[inline]
    pub const fn offset_of(n: u32) -> Offset<Self, Ro> {
        Offset::new((n as usize) * size_of::<u32>())
    }
}

layout!({
    /// [CS] D7.5.17
    /// Class `0x9` 64-bit ROM table entry register.
    pub struct Class0x9Rom64BitEntry(u64);
    {
        let offset @ 63..12;
        let __ @ 11..9;
        let powerid @ 8..4;
        let __ @ 3;
        let powerid_valid @ 2;
        let present @ 1..0;
    }
});

impl Class0x9Rom64BitEntry {
    /// Returns the byte offset of entry `n` within a Class `0x9` 64-bit ROM table.
    #[inline]
    pub const fn offset_of(n: u32) -> Offset<Self, Ro> {
        Offset::new((n as usize) * size_of::<u64>())
    }
}

/// Entry format encoded in `DEVID.FORMAT`.
#[bitfield_repr(u8)]
#[derive(Copy, Clone)]
pub enum Class0x9RomEntryFormat {
    /// 32-bit ROM table entries (`0`).
    Bits32 = 0,
    /// 64-bit ROM table entries (`1`).
    Bits64 = 1,
}

static_assert!(size_of::<Class0x9RomEntryFormat>() == size_of::<u8>());
static_assert!(align_of::<Class0x9RomEntryFormat>() == align_of::<u8>());

layout!({
    /// [CS] D7.5.10
    /// Class `0x9` ROM table device ID register (`DEVID`).
    pub struct Class0x9RomDeviceIdRegister(u32);
    {
        let __ @ 31..6;
        let prr @ 5;
        let sysmem @ 4;
        let format @ 3..0: Class0x9RomEntryFormat;
    }
});

/// Byte offset of [`Class0x9RomDeviceIdRegister`] (`DEVID`) within a component page.
pub const DEVID: Offset<Class0x9RomDeviceIdRegister, Ro> = Offset::new(0xfc8);

/// Represents an error that occurred while walking the table.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct WalkError {
    /// Human-readable description of the walk error.
    pub reason: &'static str,
    /// Byte offset from the base ROM table at which the error occurred.
    pub offset: u32,
}

/// There are several types of ROM table entry registers; this struct serves
/// as unified front-end for accessing their contents.
struct EntryContents {
    value: u64,
    offset: u32,
    present: bool,
}

/// [CS] D5
/// A ROM table is a basic CoreSight component that provides pointers to other
/// components (including other ROM tables) in its lower registers via offsets
/// from its base address. It is an organizational structure that can be used to
/// find all CoreSight components - possibly as well as legacy or
/// vendor-specific ones - on an SoC. Thought of as a tree, the leaves are the
/// system's CoreSight components and the root is typically referred to as the
/// "base ROM table" (or, more plainly, "the ROM table").
pub struct RomTable;

impl RomTable {
    // [CS] D6.2.1, D7.2.1
    // The maximum number of ROM table entries, for various types.
    /// Maximum number of entries in a Class `0x1` ROM table.
    const MAX_0X1_ROM_ENTRIES: u32 = 960;
    /// Maximum number of entries in a Class `0x9` 32-bit ROM table.
    const MAX_0X9_ROM_32BIT_ENTRIES: u32 = 512;
    /// Maximum number of entries in a Class `0x9` 64-bit ROM table.
    const MAX_0X9_ROM_64BIT_ENTRIES: u32 = 256;

    /// The most tables [`Self::walk`] goes through to reach a component. CoreSight hierarchies are
    /// only a few tables deep.
    const MAX_TABLE_DEPTH: u32 = 8;

    /// Walks the underlying tree of components with no dynamic allocation,
    /// calling `callback` on the offset from the table's base address (implicitly
    /// encoded in `base`) of each component found. The (`base`, `max_offset`)
    /// together implicitly give the aperture to walk.
    ///
    /// The walk will visit and access the first page of memory of each found
    /// component. Unfortunately, however, there is no canonical means to
    /// determine how large a region of memory this entails. The determination of
    /// the maximum visited offset - or at least something deemed large enough -
    /// is left to the caller. The offset must at least be
    /// `MINIMUM_COMPONENT_SIZE`, which is the size of the base table proper.
    ///
    /// `callback` is only called with offsets that are multiples of `MINIMUM_COMPONENT_SIZE`
    /// and satisfy `offset as usize + MINIMUM_COMPONENT_SIZE <= max_offset as usize`.
    ///
    /// A component reached through more than 8 tables (`MAX_TABLE_DEPTH`), as a cycle of entries
    /// leads to, ends the walk with an error at the table that points at it.
    ///
    /// # Safety
    ///
    /// `base` must point to a valid, mapped MMIO aperture of at least `max_offset` bytes that
    /// remains mapped and safe to read for the duration of the call (with 8-byte alignment if
    /// 64-bit Class `0x9` ROM tables are present in the aperture).
    pub unsafe fn walk<Access, F>(
        base: MmioPtr<u32, Access>,
        max_offset: u32,
        mut callback: F,
    ) -> Result<(), WalkError>
    where
        Access: Accessible + AccessRestrictsTo<Ro>,
        F: FnMut(u32),
    {
        const { assert!(size_of::<u32>() == 4) };
        assert!((max_offset as usize) >= MINIMUM_COMPONENT_SIZE);
        let base = base.restrict::<Ro>();
        // SAFETY: Caller guarantees `base` points to a mapped MMIO aperture of at least
        // `max_offset` bytes.
        unsafe { Self::walk_from(base, max_offset, &mut callback, 0, 0) }
    }

    #[inline]
    fn is_table(classid: Option<ClassId>, architect: u16, archid: u16) -> bool {
        if classid == Some(ClassId::RomTable0x1) {
            return true;
        }
        if classid == Some(ClassId::CoreSight)
            && architect == arm::ARCHITECT
            && archid == arm::archid::ROM_TABLE
        {
            return true;
        }
        false
    }

    /// Walks the component at `offset`, reached through `depth` tables.
    ///
    /// # Safety
    ///
    /// `base` must point to a valid, mapped MMIO aperture of at least `max_offset` bytes.
    unsafe fn walk_from<F>(
        base: MmioPtr<u32, Ro>,
        max_offset: u32,
        callback: &mut F,
        offset: u32,
        depth: u32,
    ) -> Result<(), WalkError>
    where
        F: FnMut(u32),
    {
        if max_offset - (MINIMUM_COMPONENT_SIZE as u32) < offset {
            return Err(WalkError { reason: "component exceeds aperture", offset });
        }

        // Treats I/O as rooted at the current offset (instead of rooted at the base ROM table,
        // which would complicate register access).
        // SAFETY: The check above guarantees `[offset, offset + MINIMUM_COMPONENT_SIZE)` lies
        // within the `max_offset`-byte aperture starting at `base`, `offset` is a multiple of
        // `MINIMUM_COMPONENT_SIZE` (4096) so alignment is preserved, and `CIDR1` and `DEVARCH`
        // lie within the 4 KiB `bank32` component region.
        let (comp_ptr, bank32, cidr1, arch_reg) = unsafe {
            let comp_ptr = MmioPtr::<u32, Ro>::new(base.as_ptr().byte_add(offset as usize));
            let bank32 = MmioBank::new(comp_ptr, MINIMUM_COMPONENT_SIZE);
            let cidr1 = bank32.at(CIDR1).read();
            let arch_reg = bank32.at(DEVARCH).read();
            (comp_ptr, bank32, cidr1, arch_reg)
        };
        let classid = cidr1.try_classid().ok();
        let architect = arch_reg.architect();
        let archid = arch_reg.archid();
        if Self::is_table(classid, architect, archid) {
            let classid = classid.unwrap();
            let max_entries: u32;
            let mut format: Option<Class0x9RomEntryFormat> = None;
            if classid == ClassId::RomTable0x1 {
                max_entries = Self::MAX_0X1_ROM_ENTRIES;
            } else {
                // If not a class 0x1 table, then a class 0x9.
                debug_assert_eq!(classid, ClassId::CoreSight);
                // SAFETY: `DEVID` lies within the 4 KiB `bank32` component region.
                let devid = unsafe { bank32.at(DEVID) }.read();
                match devid.try_format() {
                    Ok(fmt @ Class0x9RomEntryFormat::Bits32) => {
                        format = Some(fmt);
                        max_entries = Self::MAX_0X9_ROM_32BIT_ENTRIES;
                    }
                    Ok(fmt @ Class0x9RomEntryFormat::Bits64) => {
                        format = Some(fmt);
                        max_entries = Self::MAX_0X9_ROM_64BIT_ENTRIES;
                    }
                    Err(_) => {
                        return Err(WalkError { reason: "bad format value", offset });
                    }
                }
            }

            for i in 0..max_entries {
                // SAFETY: `comp_ptr` covers the current 4 KiB component page, and
                // `i < max_entries`.
                let contents = unsafe { Self::read_entry(comp_ptr, i, classid, format) }
                    .map_err(|reason| WalkError { reason, offset })?;
                if contents.value == 0 {
                    break; // Signals that the walk is over if identically zero.
                }
                if !contents.present {
                    continue;
                }
                if depth == Self::MAX_TABLE_DEPTH {
                    return Err(WalkError { reason: "ROM tables nested too deeply", offset });
                }
                // [CS] D5.4
                // the offset provided by the ROM table entry requires a shift of 12 bits.
                let new_offset = offset.wrapping_add(contents.offset.wrapping_shl(12));
                // SAFETY: `base` covers `max_offset` bytes, and `walk_from` validates bounds for
                // `new_offset` on entry.
                unsafe { Self::walk_from(base, max_offset, callback, new_offset, depth + 1) }?;
            }
            return Ok(());
        }

        // There should be a ROM table at offset zero.
        if offset == 0 {
            return Err(WalkError { reason: "not a ROM table", offset: 0 });
        }

        callback(offset);
        Ok(())
    }

    /// # Safety
    ///
    /// `comp_ptr` must cover a valid 4 KiB component MMIO region (8-byte aligned if `format` is
    /// `Some(Class0x9RomEntryFormat::Bits64)`), and `n` must be less than the table's maximum
    /// entry count.
    unsafe fn read_entry(
        comp_ptr: MmioPtr<u32, Ro>,
        n: u32,
        classid: ClassId,
        format: Option<Class0x9RomEntryFormat>,
    ) -> Result<EntryContents, &'static str> {
        let bank32 = MmioBank::new(comp_ptr, MINIMUM_COMPONENT_SIZE);
        if classid == ClassId::RomTable0x1 {
            // SAFETY: `n < MAX_0X1_ROM_ENTRIES` (960), so `n * 4 < MINIMUM_COMPONENT_SIZE` (4096),
            // which lies within `bank32`.
            let entry = unsafe { bank32.at(Class0x1RomEntry::offset_of(n)) }.read();
            return Ok(EntryContents {
                value: u64::from(entry.bits()),
                offset: entry.offset(),
                present: entry.present(),
            });
        }

        // If not a class 0x1 table, then a class 0x9.
        debug_assert_eq!(classid, ClassId::CoreSight);
        debug_assert!(format.is_some());

        match format {
            Some(Class0x9RomEntryFormat::Bits32) => {
                // SAFETY: `n < MAX_0X9_ROM_32BIT_ENTRIES` (512), so
                // `n * 4 < MINIMUM_COMPONENT_SIZE` (4096), which lies within `bank32`.
                let entry = unsafe { bank32.at(Class0x9Rom32BitEntry::offset_of(n)) }.read();
                Ok(EntryContents {
                    value: u64::from(entry.bits()),
                    offset: entry.offset(),
                    // [CS] D7.5.17: only a value of 0b11 for present() signifies presence.
                    present: (entry.present() & 0b11) != 0,
                })
            }
            Some(Class0x9RomEntryFormat::Bits64) => {
                // SAFETY: Caller guarantees the component MMIO region is 8-byte aligned when
                // 64-bit ROM tables are present and covers `MINIMUM_COMPONENT_SIZE` bytes;
                // `n < MAX_0X9_ROM_64BIT_ENTRIES` (256), so `n * 8 < MINIMUM_COMPONENT_SIZE`.
                let entry = unsafe {
                    let comp_ptr64 = MmioPtr::<u64, Ro>::new(comp_ptr.as_ptr().cast::<u64>());
                    let bank64 = MmioBank::new(comp_ptr64, MINIMUM_COMPONENT_SIZE);
                    bank64.at(Class0x9Rom64BitEntry::offset_of(n))
                }
                .read();
                // `walk_from` shifts the offset field left by 12 into a `u32`, which keeps only
                // its low 20 bits.
                if entry.offset() > u64::from(u32::MAX >> 12) {
                    return Err(
                        "a simplifying assumption is made that a ROM table entry's offset only \
                         contains 32 bits of information. If this is no longer true, please file \
                         a bug.",
                    );
                }
                Ok(EntryContents {
                    value: entry.bits(),
                    offset: entry.offset() as u32,
                    present: (entry.present() & 0b11) != 0,
                })
            }
            None => Err("bad format value"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLASS_0X1_ROM_COMPONENT_ID_REG: u32 = 0x0000_0010;
    const CLASS_0X9_ROM_COMPONENT_ID_REG: u32 = 0x0000_0090;

    const DEV_ID_REG: u32 = 0x0000_0000;

    const EMPTY_DEV_ARCH_REG: u32 = 0x0000_0000;
    const CLASS_0X9_ROM_TABLE_DEV_ARCH_REG: u32 = 0x4760_0af7;

    const EMPTY_ROM_ENTRY_REG: u32 = 0x0000_0000;
    const OFFSET_0X0000_CLASS_0X1_ROM_ENTRY_REG: u32 = 0x0000_0001;
    const OFFSET_0X1000_CLASS_0X1_ROM_ENTRY_REG: u32 = 0x0000_1001;
    const OFFSET_0X2000_CLASS_0X9_ROM_ENTRY_REG: u32 = 0x0000_2011;
    const OFFSET_0X2000_NOT_PRESENT_CLASS_0X1_ROM_ENTRY_REG: u32 = 0x0000_2000;
    const OFFSET_0X3000_CLASS_0X1_ROM_ENTRY_REG: u32 = 0x0000_3001;
    const OFFSET_0X5000_CLASS_0X1_ROM_ENTRY_REG: u32 = 0x0000_5001;
    const OFFSET_0X6000_CLASS_0X1_ROM_ENTRY_REG: u32 = 0x0000_6001;
    const OFFSET_0XA000_CLASS_0X1_ROM_ENTRY_REG: u32 = 0x0000_a001;
    const OFFSET_0XFFFFF000_CLASS_0X1_ROM_ENTRY_REG: u32 = 0xffff_f001;

    const MAX_APERTURE_BYTES: usize = 0xb000;

    #[repr(C, align(8))]
    struct FakeAperture {
        words: [u32; MAX_APERTURE_BYTES / size_of::<u32>()],
    }

    impl FakeAperture {
        const fn new() -> Self {
            Self { words: [0; MAX_APERTURE_BYTES / size_of::<u32>()] }
        }

        fn clear(&mut self) {
            self.words.fill(0);
        }

        fn write32(&mut self, value: u32, offset: u32) -> &mut Self {
            let idx = (offset as usize) / size_of::<u32>();
            self.words[idx] = value;
            self
        }

        fn write64(&mut self, value: u64, offset: u32) -> &mut Self {
            let idx = (offset as usize) / size_of::<u32>();
            let bytes = value.to_ne_bytes();
            self.words[idx] = u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            self.words[idx + 1] = u32::from_ne_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
            self
        }

        fn walk<F>(&self, max_offset: u32, callback: F) -> Result<(), WalkError>
        where
            F: FnMut(u32),
        {
            assert!((max_offset as usize) <= MAX_APERTURE_BYTES);
            // SAFETY: `self.words` is a valid, 8-byte aligned aperture of `MAX_APERTURE_BYTES`
            // (>= `max_offset`) bytes alive for the duration of the borrow.
            unsafe {
                let ptr = MmioPtr::<u32, Ro>::new(self.words.as_ptr());
                RomTable::walk(ptr, max_offset, callback)
            }
        }
    }

    struct VisitedList {
        offsets: [u32; 8],
        len: usize,
    }

    impl VisitedList {
        const fn new() -> Self {
            Self { offsets: [0; 8], len: 0 }
        }

        fn push(&mut self, offset: u32) {
            self.offsets[self.len] = offset;
            self.len += 1;
        }

        fn as_slice(&self) -> &[u32] {
            &self.offsets[..self.len]
        }
    }

    #[test]
    fn empty_0x1_table() {
        let end_offset = 0x0000 + (MINIMUM_COMPONENT_SIZE as u32);
        let mut aperture = FakeAperture::new();
        aperture
            // Visit: Table (class 0x1)
            .write32(CLASS_0X1_ROM_COMPONENT_ID_REG, 0x0000 + 0xff4)
            .write32(EMPTY_DEV_ARCH_REG, 0x0000 + 0xfbc)
            // Read: Entry0 of Table (empty and last)
            .write32(EMPTY_ROM_ENTRY_REG, 0);

        let result = aperture.walk(end_offset, |offset| {
            panic!("unexpected component found at offset {offset:#x}");
        });
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn empty_0x9_table() {
        let end_offset = 0x0000 + (MINIMUM_COMPONENT_SIZE as u32);
        let mut aperture = FakeAperture::new();
        aperture
            // Visit: Table (class 0x9)
            .write32(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x0000 + 0xff4)
            .write32(CLASS_0X9_ROM_TABLE_DEV_ARCH_REG, 0x0000 + 0xfbc)
            .write32(DEV_ID_REG, 0x0000 + 0xfc8)
            // Read: Entry0 of Table (empty and last)
            .write32(EMPTY_ROM_ENTRY_REG, 0);

        let result = aperture.walk(end_offset, |offset| {
            panic!("unexpected component found at offset {offset:#x}");
        });
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn depth_one_references() {
        let mut aperture = FakeAperture::new();
        aperture
            // Visit: Table (class 0x1)
            .write32(CLASS_0X1_ROM_COMPONENT_ID_REG, 0x0000 + 0xff4)
            .write32(EMPTY_DEV_ARCH_REG, 0x0000 + 0xfbc)
            // Read: Entry0 of Table -> Component0
            .write32(OFFSET_0X1000_CLASS_0X1_ROM_ENTRY_REG, 0x0000)
            // Visit: Component0
            .write32(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x1000 + 0xff4)
            .write32(EMPTY_DEV_ARCH_REG, 0x1000 + 0xfbc)
            // Read: Entry1 of Table (not present)
            .write32(
                OFFSET_0X2000_NOT_PRESENT_CLASS_0X1_ROM_ENTRY_REG,
                0x0000 + (size_of::<u32>() as u32),
            )
            // Read: Entry2 of Table -> Component2
            .write32(OFFSET_0X3000_CLASS_0X1_ROM_ENTRY_REG, 0x0000 + 2 * (size_of::<u32>() as u32))
            // Visit: Component2
            .write32(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x3000 + 0xff4)
            .write32(EMPTY_DEV_ARCH_REG, 0x3000 + 0xfbc)
            // Read: Entry3 of Table (empty and last).
            .write32(EMPTY_ROM_ENTRY_REG, 0x0000 + 3 * (size_of::<u32>() as u32));

        let end_offset = 0x3000 + (MINIMUM_COMPONENT_SIZE as u32);
        let mut visited = VisitedList::new();
        let result = aperture.walk(end_offset, |offset| visited.push(offset));
        assert_eq!(result, Ok(()));
        assert_eq!(visited.as_slice(), &[0x1000, 0x3000]);
    }

    #[test]
    fn depth_two_references() {
        let u32_size = size_of::<u32>() as u32;
        let mut aperture = FakeAperture::new();
        aperture
            // Visit: Table (class 0x1)
            .write32(CLASS_0X1_ROM_COMPONENT_ID_REG, 0x0000 + 0xff4)
            .write32(EMPTY_DEV_ARCH_REG, 0x0000 + 0xfbc)
            // Read: Entry0 of Table -> Subtable0
            .write32(OFFSET_0X1000_CLASS_0X1_ROM_ENTRY_REG, 0x0000)
            // Visit: Subtable0 (class 0x1)
            .write32(CLASS_0X1_ROM_COMPONENT_ID_REG, 0x1000 + 0xff4)
            .write32(EMPTY_DEV_ARCH_REG, 0x1000 + 0xfbc)
            // Read: Entry0 of Subtable0 -> Component00
            .write32(OFFSET_0X1000_CLASS_0X1_ROM_ENTRY_REG, 0x1000)
            // Visit: Component00
            .write32(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x1000 + 0x1000 + 0xff4)
            .write32(EMPTY_DEV_ARCH_REG, 0x1000 + 0x1000 + 0xfbc)
            // Read: Entry1 of Subtable0 (not present)
            .write32(OFFSET_0X2000_NOT_PRESENT_CLASS_0X1_ROM_ENTRY_REG, 0x1000 + u32_size)
            // Read: Entry2 of Subtable0 -> Component02
            .write32(OFFSET_0X3000_CLASS_0X1_ROM_ENTRY_REG, 0x1000 + 2 * u32_size)
            // Visit: Component02
            .write32(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x1000 + 0x3000 + 0xff4)
            .write32(EMPTY_DEV_ARCH_REG, 0x1000 + 0x3000 + 0xfbc)
            // Read: Entry3 of Subtable0 (empty and last).
            .write32(EMPTY_ROM_ENTRY_REG, 0x1000 + 3 * u32_size)
            // Read: Entry1 of Table -> Component1
            .write32(OFFSET_0X5000_CLASS_0X1_ROM_ENTRY_REG, 0x0000 + u32_size)
            // Visit: Component1
            .write32(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x5000 + 0xff4)
            .write32(EMPTY_DEV_ARCH_REG, 0x5000 + 0xfbc)
            // Read: Entry2 of Table -> Subtable2
            .write32(OFFSET_0X6000_CLASS_0X1_ROM_ENTRY_REG, 0x0000 + 2 * u32_size)
            // Visit: Subtable2 (class 0x9)
            .write32(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x6000 + 0xff4)
            .write32(CLASS_0X9_ROM_TABLE_DEV_ARCH_REG, 0x6000 + 0xfbc)
            .write32(DEV_ID_REG, 0x6000 + 0xfc8)
            // Read: Entry0 of Subtable2 -> Component20
            .write32(OFFSET_0X2000_CLASS_0X9_ROM_ENTRY_REG, 0x6000)
            // Visit: Component20
            .write32(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x6000 + 0x2000 + 0xff4)
            .write32(EMPTY_DEV_ARCH_REG, 0x6000 + 0x2000 + 0xfbc)
            // Read: Entry1 of Subtable2 (empty and last)
            .write32(EMPTY_ROM_ENTRY_REG, 0x6000 + u32_size)
            // Read: Entry3 of Table (empty and last).
            .write32(EMPTY_ROM_ENTRY_REG, 3 * u32_size);

        let end_offset = 0x8000 + (MINIMUM_COMPONENT_SIZE as u32);
        let mut visited = VisitedList::new();
        let result = aperture.walk(end_offset, |offset| visited.push(offset));
        assert_eq!(result, Ok(()));
        assert_eq!(visited.as_slice(), &[0x2000, 0x4000, 0x5000, 0x8000]);
    }

    #[test]
    fn negative_offset() {
        // An encoded -4096 in two's complement form.
        // const COMP00_ENCODED_OFFSET: u32 = !1u32 + 1;

        let u32_size = size_of::<u32>() as u32;
        let mut aperture = FakeAperture::new();
        aperture
            // Visit: Table (class 0x1)
            .write32(CLASS_0X1_ROM_COMPONENT_ID_REG, 0x0000 + 0xff4)
            .write32(EMPTY_DEV_ARCH_REG, 0x0000 + 0xfbc)
            // Read: Entry0 of Table -> Subtable0
            .write32(OFFSET_0XA000_CLASS_0X1_ROM_ENTRY_REG, 0x0000)
            // Visit: Subtable0 (class 0x1)
            .write32(CLASS_0X1_ROM_COMPONENT_ID_REG, 0xa000 + 0xff4)
            .write32(EMPTY_DEV_ARCH_REG, 0xa000 + 0xfbc)
            // Read: Entry0 of Subtable0 -> Component00
            .write32(OFFSET_0XFFFFF000_CLASS_0X1_ROM_ENTRY_REG, 0xa000)
            // Visit: Component00
            .write32(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x9000 + 0xff4)
            .write32(EMPTY_DEV_ARCH_REG, 0x9000 + 0xfbc)
            // Read: Entry1 of Subtable0 (empty and last).
            .write32(EMPTY_ROM_ENTRY_REG, 0xa000 + u32_size)
            // Read: Entry1 of Table (empty and last).
            .write32(EMPTY_ROM_ENTRY_REG, u32_size);

        let end_offset = 0xa000 + (MINIMUM_COMPONENT_SIZE as u32);
        let mut visited = VisitedList::new();
        let result = aperture.walk(end_offset, |offset| visited.push(offset));
        assert_eq!(result, Ok(()));
        assert_eq!(visited.as_slice(), &[0x9000]);
    }

    #[test]
    fn class_0x9_64bit_entries_and_errors() {
        // 64-bit Class 0x9 table with one valid entry at offset 0x1000.
        let mut aperture = FakeAperture::new();
        aperture
            .write32(CLASS_0X9_ROM_COMPONENT_ID_REG, 0xff4)
            .write32(CLASS_0X9_ROM_TABLE_DEV_ARCH_REG, 0xfbc)
            .write32(0x0000_0001, 0xfc8) // Format::Bits64
            .write64(0x0000_0000_0000_1003, 0)
            .write32(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x1000 + 0xff4)
            .write32(EMPTY_DEV_ARCH_REG, 0x1000 + 0xfbc)
            .write64(0, 8);

        let mut visited = VisitedList::new();
        assert_eq!(
            aperture.walk(0x1000 + (MINIMUM_COMPONENT_SIZE as u32), |offset| visited.push(offset)),
            Ok(())
        );
        assert_eq!(visited.as_slice(), &[0x1000]);

        // Not a ROM table at offset 0.
        aperture.clear();
        aperture.write32(CLASS_0X9_ROM_COMPONENT_ID_REG, 0xff4).write32(EMPTY_DEV_ARCH_REG, 0xfbc);
        assert_eq!(
            aperture.walk(MINIMUM_COMPONENT_SIZE as u32, |_| {}),
            Err(WalkError { reason: "not a ROM table", offset: 0 })
        );
    }

    #[test]
    fn class_0x9_64bit_offset_over_32_bits() {
        // 64-bit Class 0x9 table whose one entry is at offset 0x1_0000_1000. Kept to 32 bits, that
        // offset would alias the component at 0x1000.
        let mut aperture = FakeAperture::new();
        aperture
            .write32(CLASS_0X9_ROM_COMPONENT_ID_REG, 0xff4)
            .write32(CLASS_0X9_ROM_TABLE_DEV_ARCH_REG, 0xfbc)
            .write32(0x0000_0001, 0xfc8) // Format::Bits64
            .write64(0x0000_0001_0000_1003, 0)
            .write32(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x1000 + 0xff4)
            .write32(EMPTY_DEV_ARCH_REG, 0x1000 + 0xfbc)
            .write64(0, 8);

        let mut visited = VisitedList::new();
        let result =
            aperture.walk(0x1000 + (MINIMUM_COMPONENT_SIZE as u32), |offset| visited.push(offset));
        let error = result.unwrap_err();
        assert!(error.reason.starts_with("a simplifying assumption"));
        assert_eq!(error.offset, 0);
        assert!(visited.as_slice().is_empty());
    }

    #[test]
    fn self_referencing_table() {
        let mut aperture = FakeAperture::new();
        aperture
            // Visit: Table (class 0x1)
            .write32(CLASS_0X1_ROM_COMPONENT_ID_REG, 0x0000 + 0xff4)
            .write32(EMPTY_DEV_ARCH_REG, 0x0000 + 0xfbc)
            // Read: Entry0 of Table -> Table
            .write32(OFFSET_0X0000_CLASS_0X1_ROM_ENTRY_REG, 0x0000);

        let result = aperture.walk(MINIMUM_COMPONENT_SIZE as u32, |offset| {
            panic!("unexpected component found at offset {offset:#x}");
        });
        assert_eq!(result, Err(WalkError { reason: "ROM tables nested too deeply", offset: 0 }));
    }

    #[test]
    fn nesting_depth_limit() {
        /// Writes a chain of `tables` class 0x1 tables, one per page from offset 0, each pointing
        /// at the next page, and a component on the page after the last.
        fn write_chain(aperture: &mut FakeAperture, tables: u32) {
            aperture.clear();
            for i in 0..tables {
                aperture
                    .write32(CLASS_0X1_ROM_COMPONENT_ID_REG, i * 0x1000 + 0xff4)
                    .write32(EMPTY_DEV_ARCH_REG, i * 0x1000 + 0xfbc)
                    .write32(OFFSET_0X1000_CLASS_0X1_ROM_ENTRY_REG, i * 0x1000);
            }
            aperture
                .write32(CLASS_0X9_ROM_COMPONENT_ID_REG, tables * 0x1000 + 0xff4)
                .write32(EMPTY_DEV_ARCH_REG, tables * 0x1000 + 0xfbc);
        }

        let depth = RomTable::MAX_TABLE_DEPTH;
        let mut aperture = FakeAperture::new();

        // The deepest component the walk reaches.
        write_chain(&mut aperture, depth);
        let mut visited = VisitedList::new();
        let end_offset = depth * 0x1000 + (MINIMUM_COMPONENT_SIZE as u32);
        assert_eq!(aperture.walk(end_offset, |offset| visited.push(offset)), Ok(()));
        assert_eq!(visited.as_slice(), &[depth * 0x1000]);

        // One table more: the error names the table that points past the limit.
        write_chain(&mut aperture, depth + 1);
        let end_offset = (depth + 1) * 0x1000 + (MINIMUM_COMPONENT_SIZE as u32);
        let result = aperture.walk(end_offset, |offset| {
            panic!("unexpected component found at offset {offset:#x}");
        });
        assert_eq!(
            result,
            Err(WalkError { reason: "ROM tables nested too deeply", offset: depth * 0x1000 })
        );
    }

    #[test]
    fn is_table_and_register_setters() {
        assert!(RomTable::is_table(Some(ClassId::RomTable0x1), 0, 0));
        assert!(RomTable::is_table(
            Some(ClassId::CoreSight),
            arm::ARCHITECT,
            arm::archid::ROM_TABLE,
        ));
        assert!(!RomTable::is_table(Some(ClassId::CoreSight), arm::ARCHITECT, arm::archid::ETM4));
        assert!(!RomTable::is_table(None, arm::ARCHITECT, arm::archid::ROM_TABLE));

        let mut e0x1 = Class0x1RomEntry::default();
        e0x1.set_offset(1)
            .set_powerid(0)
            .set_powerid_valid(false)
            .set_format(false)
            .set_present(true);
        assert_eq!(e0x1.bits(), OFFSET_0X1000_CLASS_0X1_ROM_ENTRY_REG);

        let mut e0x9_32 = Class0x9Rom32BitEntry::default();
        e0x9_32.set_offset(2).set_powerid(1).set_powerid_valid(false).set_present(1);
        assert_eq!(e0x9_32.bits(), OFFSET_0X2000_CLASS_0X9_ROM_ENTRY_REG);

        let mut e0x9_64 = Class0x9Rom64BitEntry::default();
        e0x9_64.set_offset(1).set_powerid(0).set_powerid_valid(false).set_present(0b11);
        assert_eq!(e0x9_64.bits(), 0x0000_0000_0000_1003);

        let mut devid = Class0x9RomDeviceIdRegister::default();
        devid.set_prr(true).set_sysmem(true).set_format(Class0x9RomEntryFormat::Bits64);
        assert!(devid.prr());
        assert!(devid.sysmem());
        assert_eq!(devid.try_format().ok(), Some(Class0x9RomEntryFormat::Bits64));
    }
}
