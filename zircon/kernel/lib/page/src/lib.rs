// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#![no_std]
#![forbid(unsafe_code)]

// TODO(https://fxbug.dev/576429543): Derive these constants from `libarch::paging` once
// <lib/arch/paging.h> and <lib/arch/paging-traits.h> are ported to Rust.
#[derive(Clone, Copy)]
struct PagingConfiguration {
    shift: usize,
    table_level_shift: usize,
    num_table_levels: usize,
}

impl PagingConfiguration {
    // LINT.IfChange
    const fn from_str(name: &str) -> Self {
        match name.as_bytes() {
            // x86 4-level paging: 4 KiB pages, 512 entries/table, 4 levels (48-bit VA).
            b"4level" => Self { shift: 12, table_level_shift: 9, num_table_levels: 4 },
            // ARM64 4 KiB granule, 48-bit VA: 4 KiB pages, 512 entries/table, 4 levels.
            b"4k-48bit" => Self { shift: 12, table_level_shift: 9, num_table_levels: 4 },
            // RISC-V Sv39: 4 KiB pages, 512 entries/table, 3 levels (39-bit VA).
            b"sv39" => Self { shift: 12, table_level_shift: 9, num_table_levels: 3 },
            _ => panic!("unsupported LIB_ARCH_PAGING_CONFIGURATION"),
        }
    }
    // LINT.ThenChange(//zircon/kernel/lib/page/BUILD.gn)

    const fn page_size(self) -> usize {
        1usize << self.shift
    }

    const fn page_mask(self) -> usize {
        self.page_size() - 1
    }

    const fn num_table_entries(self) -> usize {
        1usize << self.table_level_shift
    }

    const fn table_size(self) -> usize {
        core::mem::size_of::<u64>() * self.num_table_entries()
    }

    const fn virtual_address_size(self) -> usize {
        self.shift + self.num_table_levels * self.table_level_shift
    }

    const fn virtual_address_mask(self) -> usize {
        (1usize << self.virtual_address_size()) - 1
    }
}

const CONFIGURATION: PagingConfiguration =
    PagingConfiguration::from_str(env!("LIB_ARCH_PAGING_CONFIGURATION"));

/// The selected page size.
pub const SIZE: usize = CONFIGURATION.page_size();

/// The shift of the first level of virtual address mask used for page table walking.
pub const SHIFT: usize = CONFIGURATION.shift;

/// Mask that will extract the offset into a page of an address.
pub const MASK: usize = CONFIGURATION.page_mask();

/// The size in bytes of a page table.
pub const TABLE_SIZE: usize = CONFIGURATION.table_size();

/// The number of page table levels.
pub const NUM_TABLE_LEVELS: usize = CONFIGURATION.num_table_levels;

/// The number of entries within a page table.
pub const NUM_TABLE_ENTRIES: usize = CONFIGURATION.num_table_entries();

/// The additional bit shift of the virtual address mask used for page table walking.
pub const TABLE_LEVEL_SHIFT: usize = CONFIGURATION.table_level_shift;

/// The number of addressable bits in a virtual address.
pub const VIRTUAL_ADDRESS_SIZE: usize = CONFIGURATION.virtual_address_size();

/// A mask for the addressable bits of a virtual address.
pub const VIRTUAL_ADDRESS_MASK: usize = CONFIGURATION.virtual_address_mask();

const _: () = {
    #[cfg(target_arch = "x86_64")]
    assert!(matches!(env!("LIB_ARCH_PAGING_CONFIGURATION").as_bytes(), b"4level"));
    #[cfg(target_arch = "aarch64")]
    assert!(matches!(env!("LIB_ARCH_PAGING_CONFIGURATION").as_bytes(), b"4k-48bit"));
    #[cfg(target_arch = "riscv64")]
    assert!(matches!(env!("LIB_ARCH_PAGING_CONFIGURATION").as_bytes(), b"sv39"));

    assert!(SIZE.is_power_of_two());
    assert!(SIZE == 1usize << SHIFT);
    assert!(MASK == SIZE - 1);
    assert!(NUM_TABLE_LEVELS > 0);
    assert!(NUM_TABLE_ENTRIES == 1usize << TABLE_LEVEL_SHIFT);
    assert!(TABLE_SIZE == core::mem::size_of::<u64>() * NUM_TABLE_ENTRIES);
    assert!(VIRTUAL_ADDRESS_SIZE == SHIFT + NUM_TABLE_LEVELS * TABLE_LEVEL_SHIFT);
    assert!(VIRTUAL_ADDRESS_MASK == (1usize << VIRTUAL_ADDRESS_SIZE) - 1);
};

/// Returns true if `val` is aligned to a page boundary.
#[inline]
pub const fn is_aligned(val: usize) -> bool {
    (val & MASK) == 0
}

/// Rounds `val` down to the nearest page boundary.
#[inline]
pub const fn round_down(val: usize) -> usize {
    val & !MASK
}

/// Rounds `val` up to the nearest page boundary, wrapping to `0` on overflow.
#[inline]
pub const fn round_up(val: usize) -> usize {
    val.wrapping_add(MASK) & !MASK
}

/// Returns true if `val` is aligned to a page boundary.
#[inline]
pub const fn is_page_rounded(val: usize) -> bool {
    is_aligned(val)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exported_constants() {
        #[cfg(target_arch = "x86_64")]
        let (expected_config, expected_levels, expected_va_size) = ("4level", 4, 48);
        #[cfg(target_arch = "aarch64")]
        let (expected_config, expected_levels, expected_va_size) = ("4k-48bit", 4, 48);
        #[cfg(target_arch = "riscv64")]
        let (expected_config, expected_levels, expected_va_size) = ("sv39", 3, 39);
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64")))]
        let (expected_config, expected_levels, expected_va_size) = ("4level", 4, 48);

        assert_eq!(env!("LIB_ARCH_PAGING_CONFIGURATION"), expected_config);
        assert_eq!(SIZE, 4096);
        assert_eq!(SHIFT, 12);
        assert_eq!(MASK, 0xfff);
        assert_eq!(TABLE_SIZE, 4096);
        assert_eq!(NUM_TABLE_LEVELS, expected_levels);
        assert_eq!(NUM_TABLE_ENTRIES, 512);
        assert_eq!(TABLE_LEVEL_SHIFT, 9);
        assert_eq!(VIRTUAL_ADDRESS_SIZE, expected_va_size);
        assert_eq!(VIRTUAL_ADDRESS_MASK, (1usize << expected_va_size) - 1);
    }

    #[test]
    fn test_paging_configurations() {
        let four_level = PagingConfiguration::from_str("4level");
        assert_eq!(four_level.page_size(), 4096);
        assert_eq!(four_level.shift, 12);
        assert_eq!(four_level.page_mask(), 0xfff);
        assert_eq!(four_level.table_size(), 4096);
        assert_eq!(four_level.num_table_levels, 4);
        assert_eq!(four_level.num_table_entries(), 512);
        assert_eq!(four_level.table_level_shift, 9);
        assert_eq!(four_level.virtual_address_size(), 48);
        assert_eq!(four_level.virtual_address_mask(), (1usize << 48) - 1);

        let arm_4k_48bit = PagingConfiguration::from_str("4k-48bit");
        assert_eq!(arm_4k_48bit.page_size(), 4096);
        assert_eq!(arm_4k_48bit.shift, 12);
        assert_eq!(arm_4k_48bit.page_mask(), 0xfff);
        assert_eq!(arm_4k_48bit.table_size(), 4096);
        assert_eq!(arm_4k_48bit.num_table_levels, 4);
        assert_eq!(arm_4k_48bit.num_table_entries(), 512);
        assert_eq!(arm_4k_48bit.table_level_shift, 9);
        assert_eq!(arm_4k_48bit.virtual_address_size(), 48);
        assert_eq!(arm_4k_48bit.virtual_address_mask(), (1usize << 48) - 1);

        let sv39 = PagingConfiguration::from_str("sv39");
        assert_eq!(sv39.page_size(), 4096);
        assert_eq!(sv39.shift, 12);
        assert_eq!(sv39.page_mask(), 0xfff);
        assert_eq!(sv39.table_size(), 4096);
        assert_eq!(sv39.num_table_levels, 3);
        assert_eq!(sv39.num_table_entries(), 512);
        assert_eq!(sv39.table_level_shift, 9);
        assert_eq!(sv39.virtual_address_size(), 39);
        assert_eq!(sv39.virtual_address_mask(), (1usize << 39) - 1);
    }

    #[test]
    #[should_panic(expected = "unsupported LIB_ARCH_PAGING_CONFIGURATION")]
    fn test_unsupported_paging_configuration() {
        let _ = PagingConfiguration::from_str("invalid");
    }

    #[test]
    fn test_page_rounding_helpers() {
        const {
            assert!(is_aligned(SIZE));
            assert!(is_page_rounded(SIZE));
            assert!(round_down(SIZE + 1) == SIZE);
            assert!(round_up(1) == SIZE);
        }

        let max_aligned = usize::MAX - MASK;

        assert!(is_aligned(0));
        assert!(is_page_rounded(0));
        assert!(!is_aligned(1));
        assert!(!is_page_rounded(1));
        assert!(!is_aligned(SIZE - 1));
        assert!(!is_page_rounded(SIZE - 1));
        assert!(is_aligned(SIZE));
        assert!(is_page_rounded(SIZE));
        assert!(is_aligned(max_aligned));
        assert!(is_page_rounded(max_aligned));
        assert!(!is_aligned(usize::MAX));
        assert!(!is_page_rounded(usize::MAX));

        assert_eq!(round_down(0), 0);
        assert_eq!(round_down(1), 0);
        assert_eq!(round_down(SIZE - 1), 0);
        assert_eq!(round_down(SIZE), SIZE);
        assert_eq!(round_down(SIZE + 1), SIZE);
        assert_eq!(round_down(max_aligned), max_aligned);
        assert_eq!(round_down(usize::MAX), max_aligned);

        assert_eq!(round_up(0), 0);
        assert_eq!(round_up(1), SIZE);
        assert_eq!(round_up(SIZE - 1), SIZE);
        assert_eq!(round_up(SIZE), SIZE);
        assert_eq!(round_up(SIZE + 1), 2 * SIZE);
        assert_eq!(round_up(max_aligned), max_aligned);
        assert_eq!(round_up(max_aligned + 1), 0);
        assert_eq!(round_up(usize::MAX), 0);
    }
}
