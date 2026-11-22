// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Fine-grained native Rust (`Rust -> Rust`) unit tests for the ARM SMMU driver components which
//! do not depend on the architecture version. Those of the SMMUv2 driver proper are in
//! `v2/unit_tests.rs`.
//!
//! This module is shared between the standalone host test binary (`fx test --host`) and the
//! in-kernel `#[cfg(ktest)]` test suite, so assertions return `bool` via [`check!`] and
//! [`check_eq!`] rather than panicking or depending on `std`.
//!
//! Covers:
//! 1. `smmu_mode` boot option string validation and parsing.
//! 2. `vmsav8_64` stage-1 page/table descriptor encoding, permissions, and atomic PTE stores.
//! 3. `Bitmask128` 128-bit resource mask operations and set-bit iteration.
//! 4. `PageCache` LIFO allocation, zero-filling, FIFO tail trimming, and OOM handling.
//! 5. `TranslationTableHelper` 4-level cursor walks, PoC cache cleaning, and table reclamation.
//! 6. `DeviceAspace` map/unmap, specific-location placement, straddling levels, OOM rollback,
//!    shared table levels, and recursive 4-level page-table verification.
//! 7. The `EscapedName` formatter used when printing object names in diagnostics.

use core::ffi::c_char;
use core::fmt::Write as _;
use core::sync::atomic::Ordering;

use debug::dprintf;
use region_alloc::{AllowOverlap, RegionSpan, TestRegionSet};
use zx_status::Status;

use super::common::translation_table::{LEVELS, TranslationTableHelper, table_slice};
use super::common::vmsav8_64::{
    ENTRIES_PER_PAGE, PAGE_ENTRY_ACCESS_FLAG_BIT, PAGE_ENTRY_BIT, PERM_LUT, TABLE_ENTRY_BIT,
    VALID_ADDR_MASK, VALID_ENTRY_BIT, get_page_entry_perms, get_table_entry_paddr, is_block_entry,
    is_table_entry, is_valid_addr, is_valid_entry, make_page_entry, make_perms, make_table_entry,
    write_pte,
};
use super::common::{
    ALL_PERM_FLAGS, ArmSmmuMode, DeviceAspace, EscapedName, IOMMU_FLAG_PERM_EXECUTE,
    IOMMU_FLAG_PERM_READ, IOMMU_FLAG_PERM_WRITE, PAGE_MASK, PAGE_SHIFT, PAGE_SIZE, PAGE_SIZE_USIZE,
    PageCache, PinnedVmoSource, SmmuEnv, SmmuPage, TlbInvalidator, get_smmu_mode,
    get_smmu_mode_bytes, get_smmu_mode_cstr, validate_smmu_mode_cstr, validate_smmu_mode_string,
};
use super::v2::Bitmask128;
use super::v2::smmu::TagBuf;

type RegionAllocator = region_alloc::RegionAllocator<super::common::SmmuRawMutex>;

// Lightweight boolean assertion macros for the in-kernel `#[ktest]` harness, where panicking on failure is undesirable.
macro_rules! check {
    ($cond:expr) => {
        if !($cond) {
            dprintf!(INFO, "UNIT TEST FAILURE at {}:{}: {}\n", file!(), line!(), stringify!($cond));
            return false;
        }
    };
}

macro_rules! check_eq {
    ($left:expr, $right:expr) => {
        match (&($left), &($right)) {
            (left_val, right_val) => {
                if !(*left_val == *right_val) {
                    dprintf!(
                        INFO,
                        "UNIT TEST FAILURE at {}:{}: {} != {}\n",
                        file!(),
                        line!(),
                        stringify!($left),
                        stringify!($right)
                    );
                    return false;
                }
            }
        }
    };
}

// ============================================================================
// 1. `smmu_mode` Unit Tests
// ============================================================================

/// Tests default SMMU mode strings (`"disabled"`, `"passthru"`, `"enforced"`) and invalid default
/// tokens (`""`, `"pass thru"`, `"pass-thru"`, `"asdfjkldsf"`).
pub fn test_smmu_mode_defaults() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] SmmuModeDefaults\n");

    let cases: &[(&str, &[u8], ArmSmmuMode)] = &[
        ("disabled", b"disabled\0", ArmSmmuMode::Disabled),
        ("passthru", b"passthru\0", ArmSmmuMode::Passthru),
        ("enforced", b"enforced\0", ArmSmmuMode::Enforced),
    ];

    for &(mode_str, cstr_bytes, expected) in cases {
        check!(validate_smmu_mode_string(mode_str));
        check_eq!(get_smmu_mode(mode_str, None), Some(expected));
        check_eq!(get_smmu_mode(mode_str, Some(0x1234)), Some(expected));
        check_eq!(get_smmu_mode_bytes(mode_str.as_bytes(), None), Some(expected));
        check_eq!(get_smmu_mode_bytes(mode_str.as_bytes(), Some(0x1234)), Some(expected));

        let c_ptr = cstr_bytes.as_ptr().cast::<c_char>();
        // SAFETY: `cstr_bytes` is a valid NUL-terminated byte slice.
        unsafe {
            check!(validate_smmu_mode_cstr(c_ptr));
            check_eq!(get_smmu_mode_cstr(c_ptr, None), Some(expected));
            check_eq!(get_smmu_mode_cstr(c_ptr, Some(0x1234)), Some(expected));
        }
    }

    // Invalid default mode strings must return `None`.
    for &invalid in &["", "pass thru", "pass-thru", "asdfjkldsf"] {
        check!(!validate_smmu_mode_string(invalid));
        check_eq!(get_smmu_mode(invalid, None), None);
        check_eq!(get_smmu_mode(invalid, Some(0x1234)), None);
    }

    // Null C-string pointers must be rejected cleanly without dereferencing.
    // SAFETY: Testing explicit null pointer handling in `validate_smmu_mode_cstr` and
    // `get_smmu_mode_cstr`.
    unsafe {
        check!(!validate_smmu_mode_cstr(core::ptr::null()));
        check_eq!(get_smmu_mode_cstr(core::ptr::null(), None), None);
    }

    true
}

/// Tests per-base-address overrides, first-match-wins semantics, radix parsing, and malformed
/// segment skipping in `get_smmu_mode`.
pub fn test_smmu_mode_advanced() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] SmmuModeAdvanced\n");

    // Base address matching and fallback to default.
    check_eq!(get_smmu_mode("disabled,0x1234=passthru", Some(0x1234)), Some(ArmSmmuMode::Passthru));
    check_eq!(get_smmu_mode("disabled,0x1234=passthru", Some(0x5678)), Some(ArmSmmuMode::Disabled));

    // Multiple entries.
    let mode_str = "disabled,0x1234=passthru,0x5678=enforced";
    check!(validate_smmu_mode_string(mode_str));
    check_eq!(get_smmu_mode(mode_str, None), Some(ArmSmmuMode::Disabled));
    check_eq!(get_smmu_mode(mode_str, Some(0x5678)), Some(ArmSmmuMode::Enforced));
    check_eq!(get_smmu_mode(mode_str, Some(0x1234)), Some(ArmSmmuMode::Passthru));

    // First match wins.
    check_eq!(
        get_smmu_mode("disabled,0x1234=passthru,0x1234=enforced", Some(0x1234)),
        Some(ArmSmmuMode::Passthru)
    );

    // Invalid entries: missing `=` is skipped and falls back to default; invalid mode for a
    // matched address returns `None`.
    check_eq!(get_smmu_mode("disabled,0x1234passthru", Some(0x1234)), Some(ArmSmmuMode::Disabled));
    check_eq!(get_smmu_mode("disabled,0x1234=invalid", Some(0x1234)), None);
    check_eq!(get_smmu_mode("disabled,0x1234=", Some(0x1234)), None);

    // Decimal, octal, and hex radix parsing.
    let radix_str = "disabled, 4660 = passthru, 01234 = enforced, 0xabcd = passthru";
    check!(validate_smmu_mode_string(radix_str));
    check_eq!(get_smmu_mode(radix_str, Some(0x1234)), Some(ArmSmmuMode::Passthru));
    check_eq!(get_smmu_mode(radix_str, Some(0o1234)), Some(ArmSmmuMode::Enforced));
    check_eq!(get_smmu_mode(radix_str, Some(0xabcd)), Some(ArmSmmuMode::Passthru));
    check_eq!(get_smmu_mode(radix_str, Some(0x9999)), Some(ArmSmmuMode::Disabled));

    // The `<base_addr>` grammar matches kernel `strtoul(..., 0)` semantics, quirks included.
    //
    // An uppercase `0X` prefix is not recognized, so the segment is malformed.
    check!(!validate_smmu_mode_string("disabled,0X1234=enforced"));
    check_eq!(get_smmu_mode("disabled,0X1234=enforced", Some(0x1234)), Some(ArmSmmuMode::Disabled));
    // Uppercase hex digits after a lowercase prefix are fine.
    check_eq!(get_smmu_mode("disabled,0xABCD=enforced", Some(0xabcd)), Some(ArmSmmuMode::Enforced));
    // Leading signs: `+` is ignored and `-` negates in two's complement.
    check_eq!(get_smmu_mode("disabled,+0x10=enforced", Some(0x10)), Some(ArmSmmuMode::Enforced));
    check!(validate_smmu_mode_string("disabled,-1=enforced"));
    check_eq!(get_smmu_mode("disabled,-1=enforced", Some(u64::MAX)), Some(ArmSmmuMode::Enforced));
    check_eq!(
        get_smmu_mode("disabled,-0x10=enforced", Some(0x10u64.wrapping_neg())),
        Some(ArmSmmuMode::Enforced)
    );
    // Overflow saturates to `u64::MAX` (and is then not negated).
    for overflow in [
        "disabled,0x1ffffffffffffffff=enforced",
        "disabled,99999999999999999999999=enforced",
        "disabled,-0x1ffffffffffffffff=enforced",
    ] {
        check!(validate_smmu_mode_string(overflow));
        check_eq!(get_smmu_mode(overflow, Some(u64::MAX)), Some(ArmSmmuMode::Enforced));
    }
    check_eq!(
        get_smmu_mode("disabled,0xffffffffffffffff=enforced", Some(u64::MAX)),
        Some(ArmSmmuMode::Enforced)
    );
    // A sign or prefix with no digits parses as zero.
    for zero in ["disabled,0x=enforced", "disabled,+=enforced", "disabled,-=enforced"] {
        check!(validate_smmu_mode_string(zero));
        check_eq!(get_smmu_mode(zero, Some(0)), Some(ArmSmmuMode::Enforced));
    }
    // An empty token, digits invalid for the radix, and trailing garbage are all rejected.
    for bad in [
        "disabled,=enforced",
        "disabled, =enforced",
        "disabled,08=enforced",
        "disabled,0xg=enforced",
        "disabled,0x 12=enforced",
        "disabled,12q=enforced",
        "disabled,--1=enforced",
    ] {
        check!(!validate_smmu_mode_string(bad));
        check_eq!(get_smmu_mode(bad, Some(0)), Some(ArmSmmuMode::Disabled));
    }

    // Skipping bad address segments to find a later match under `TolerateErrors`.
    check_eq!(
        get_smmu_mode("disabled,invalid_segment,0x1234=passthru", Some(0x1234)),
        Some(ArmSmmuMode::Passthru)
    );
    check_eq!(
        get_smmu_mode("disabled,0x1234passthru,0x5678=enforced", Some(0x5678)),
        Some(ArmSmmuMode::Enforced)
    );

    true
}

/// Tests ASCII case-insensitivity of SMMU mode tokens and hexadecimal base addresses.
pub fn test_smmu_mode_case_insensitive() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] SmmuModeCaseInsensitive\n");

    check_eq!(get_smmu_mode("DiSaBlEd", None), Some(ArmSmmuMode::Disabled));
    check_eq!(get_smmu_mode("PASSTHRU", None), Some(ArmSmmuMode::Passthru));
    check_eq!(get_smmu_mode("EnFoRcEd", None), Some(ArmSmmuMode::Enforced));

    let mode_str = "EnFoRcEd, 0x1234 = Disabled, 0x5678 = ENFORCED, 0x9ABC = pAsStHrU";
    check!(validate_smmu_mode_string(mode_str));
    check_eq!(get_smmu_mode(mode_str, None), Some(ArmSmmuMode::Enforced));
    check_eq!(get_smmu_mode(mode_str, Some(0x1234)), Some(ArmSmmuMode::Disabled));
    check_eq!(get_smmu_mode(mode_str, Some(0x5678)), Some(ArmSmmuMode::Enforced));
    check_eq!(get_smmu_mode(mode_str, Some(0x9abc)), Some(ArmSmmuMode::Passthru));
    check_eq!(get_smmu_mode(mode_str, Some(0x0001)), Some(ArmSmmuMode::Enforced));

    true
}

/// Tests whitespace trimming (`' '`, `'\t'`, `'\n'`, `'\r'`) around commas, `=`, and tokens.
pub fn test_smmu_mode_whitespace() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] SmmuModeWhitespace\n");

    check_eq!(get_smmu_mode("  passthru  ", None), Some(ArmSmmuMode::Passthru));
    check_eq!(get_smmu_mode(" disabled ", None), Some(ArmSmmuMode::Disabled));

    let mode_str = " \t\n\r passthru \t\n\r , \t\n\r 0x1234 \t\n\r = \t\n\r disabled \
                    \t\n\r , \t\n\r 0x5678=enforced";
    check!(validate_smmu_mode_string(mode_str));
    check_eq!(get_smmu_mode(mode_str, None), Some(ArmSmmuMode::Passthru));
    check_eq!(get_smmu_mode(mode_str, Some(0x1234)), Some(ArmSmmuMode::Disabled));
    check_eq!(get_smmu_mode(mode_str, Some(0x5678)), Some(ArmSmmuMode::Enforced));
    check_eq!(get_smmu_mode(mode_str, Some(0x0001)), Some(ArmSmmuMode::Passthru));

    check_eq!(
        get_smmu_mode(" \t disabled \t , \t 0x1234 \t = \t passthru \t ", Some(0x1234)),
        Some(ArmSmmuMode::Passthru)
    );

    // Whitespace-only string is invalid (equivalent to empty).
    check!(!validate_smmu_mode_string("   \t\n\r  "));
    check_eq!(get_smmu_mode("   \t\n\r  ", None), None);

    true
}

/// Tests validation of valid and malformed SMMU mode strings (including radix errors, integer
/// overflow, sign prefixes, and delimiter edge cases).
pub fn test_smmu_mode_validation() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] SmmuModeValidation\n");

    let valid_strings: &[&str] = &[
        "disabled",
        "passthru",
        "enforced",
        "  disabled  ",
        "disabled,0x1234=passthru",
        "disabled , 0x1234 = passthru",
        "disabled,0x1234=passthru,0x5678=enforced",
        "disabled, 1234 = passthru",
        "disabled, 01234 = passthru",
        "disabled, +1 = passthru",
        "disabled, 0 = passthru",
        "disabled, 0x0 = passthru",
        "disabled, 0xffffffffffffffff = passthru",
        // Quirks of kernel `strtoul(..., 0)` reproduced by the `<base_addr>` grammar: overflow
        // saturates, a leading `-` negates, and a sign or `0x` prefix with no digits parses as
        // zero.
        "disabled, 0x10000000000000000 = passthru",
        "disabled, -1 = passthru",
        "disabled, + = passthru",
        "disabled, 0x = passthru",
    ];
    for &s in valid_strings {
        check!(validate_smmu_mode_string(s));
        check!(get_smmu_mode(s, None).is_some());
    }

    let invalid_strings: &[&str] = &[
        "",
        "pass thru",
        "invalid",
        "disabled,",                              // Trailing comma
        "disabled,,",                             // Double comma
        ",disabled",                              // Leading comma
        "0x1234 = passthru",                      // Missing initial default mode
        "invalid_segment,0x1234=passthru",        // Bad segment first
        "disabled,0x1234passthru",                // Malformed address (missing '=')
        "disabled,0x1234=invalid",                // Invalid mode
        "disabled,0x1234=",                       // Empty mode
        "disabled, 0x1234",                       // Missing '=mode'
        "disabled, = passthru",                   // Empty address
        "disabled, 0x1234 = passthru = enforced", // Extra '='
        "disabled, not_a_number = passthru",      // Malformed address
        "disabled, 0x1234extra = passthru",       // Malformed address with trailing chars
        "disabled, 08 = passthru",
        "disabled, 0X1234 = passthru",
    ];
    for &s in invalid_strings {
        check!(!validate_smmu_mode_string(s));
    }

    true
}

// ============================================================================
// 2. `vmsav8_64` Unit Tests
// ============================================================================

/// Tests VMSAv8-64 stage-1 address validation, table/block/page descriptor encoding, permission
/// lookup table invariants (`AF == 1`, `DBM == 0`), and atomic PTE writes.
pub fn test_vmsav8_64_descriptors() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] Vmsav8_64Descriptors\n");

    check_eq!(ENTRIES_PER_PAGE, 512);
    check_eq!(VALID_ADDR_MASK, 0x0000_FFFF_FFFF_F000);

    // Address alignment and 48-bit range checks.
    check!(is_valid_addr(0));
    check!(is_valid_addr(PAGE_SIZE));
    check!(is_valid_addr(0x0000_FFFF_FFFF_F000));
    check!(!is_valid_addr(1));
    check!(!is_valid_addr(PAGE_MASK));
    check!(!is_valid_addr(PAGE_SIZE + 1));
    check!(!is_valid_addr(1u64 << 48));
    check!(!is_valid_addr(0xFFFF_0000_0000_0000));

    // Invalid, block, and table descriptor classification across levels 0..=3.
    check!(!is_valid_entry(0));
    check!(!is_table_entry(0));
    for level in 0..=3 {
        check!(!is_block_entry(0, level));
        check!(!is_block_entry(TABLE_ENTRY_BIT, level));
    }

    // A descriptor with bit 0 = 1 and bit 1 = 0 is a valid block entry at levels 0..=2, but never
    // at level 3.
    let block_desc = 0x0000_1234_0000_0000 | VALID_ENTRY_BIT;
    check!(is_valid_entry(block_desc));
    check!(!is_table_entry(block_desc));
    check!(is_block_entry(block_desc, 0));
    check!(is_block_entry(block_desc, 1));
    check!(is_block_entry(block_desc, 2));
    check!(!is_block_entry(block_desc, 3));

    // Table descriptor round-trip.
    let table_pa: u64 = 0x0000_ABCD_EF01_2000;
    let table_desc = make_table_entry(table_pa);
    check!(is_valid_entry(table_desc));
    check!(is_table_entry(table_desc));
    for level in 0..=3 {
        check!(!is_block_entry(table_desc, level));
    }
    check_eq!(get_table_entry_paddr(table_desc), table_pa);
    check_eq!(table_desc & 0x3, VALID_ENTRY_BIT | TABLE_ENTRY_BIT);

    // Page descriptor encoding and `PERM_LUT` invariants across all `0..=7` RWX combinations.
    const DBM_BIT: u64 = 1u64 << 51;
    const PERM_BITS_MASK: u64 = (1u64 << 54) | (1u64 << 53) | (0x3u64 << 6);
    let page_pa: u64 = 0x0000_fedc_ba98_7000;

    for perms in 0..=ALL_PERM_FLAGS {
        let entry = make_page_entry(page_pa, perms);
        check!(is_valid_entry(entry));
        check_eq!(entry & (VALID_ENTRY_BIT | PAGE_ENTRY_BIT), VALID_ENTRY_BIT | PAGE_ENTRY_BIT);
        check_eq!(get_table_entry_paddr(entry), page_pa);
        // Access Flag (bit 10) must always be 1, and Dirty Bit Modifier (bit 51) must be 0.
        check!((entry & PAGE_ENTRY_ACCESS_FLAG_BIT) != 0);
        check_eq!(entry & DBM_BIT, 0);
        // Not global (bit 11) must always be 1: the mapping belongs to one address space.
        check_eq!(entry & (1 << 11), 1 << 11);
        check_eq!(entry & PERM_BITS_MASK, get_page_entry_perms(perms));
        check_eq!(get_page_entry_perms(perms), PERM_LUT[perms as usize]);

        // Verify UXN/PXN and AP[2:1] semantics.
        let has_write = (perms & IOMMU_FLAG_PERM_WRITE) != 0;
        let has_read = (perms & IOMMU_FLAG_PERM_READ) != 0;
        let has_exec = (perms & IOMMU_FLAG_PERM_EXECUTE) != 0;
        let expected_xn = if has_exec { 0 } else { 1 };
        let expected_ap21 = if has_write {
            1 // Read/Write at EL0+
        } else if has_read {
            3 // Read-only at EL0+
        } else {
            2 // No access at EL0
        };
        check_eq!(get_page_entry_perms(perms), make_perms(expected_xn, expected_xn, expected_ap21));
    }

    // Exercise `write_pte` on an allocated physical page.
    let Ok(page) = SmmuEnv::alloc_page(0) else {
        return false;
    };
    let vaddr = SmmuEnv::paddr_to_physmap(page.paddr());
    // SAFETY: `vaddr` is the physmap virtual address of `page`, which is exclusively owned here.
    unsafe {
        core::ptr::write_bytes(
            core::ptr::with_exposed_provenance_mut::<u8>(vaddr),
            0,
            PAGE_SIZE_USIZE,
        );
    }
    // SAFETY: `vaddr` is the physmap virtual address of `page`, which is exclusively owned here.
    let table = unsafe { table_slice(vaddr) };
    write_pte(&table[0], table_desc, 0);
    check_eq!(table[0].load(Ordering::Relaxed), table_desc);
    let leaf_desc = make_page_entry(page_pa, IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE);
    write_pte(&table[511], leaf_desc, 3);
    check_eq!(table[511].load(Ordering::Relaxed), leaf_desc);
    write_pte(&table[0], 0, 0);
    write_pte(&table[511], 0, 3);
    // SAFETY: `vaddr` is the physmap address of `page`, which is owned by this test.
    unsafe {
        SmmuEnv::clean_dcache_range(vaddr, PAGE_SIZE_USIZE);
    }
    // SAFETY: `page` was allocated above via `SmmuEnv::alloc_page`, zeroed and cleaned to PoC, and
    // is unlinked.
    unsafe {
        SmmuEnv::free_page(page);
    }

    true
}

// ============================================================================
// 3. `Bitmask128` Unit Tests
// ============================================================================

/// Tests `Bitmask128` bit manipulation, boundary indices (`0`, `63`, `64`, `127`),
/// `set_lowest_n_bits`, `find_first_set_bit`, `find_first_clr_bit`, and `Bitmask128Iter`.
pub fn test_bitmask128() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] Bitmask128\n");

    let mut mask = Bitmask128::new();
    check_eq!(mask, Bitmask128::default());
    check_eq!(mask.raw(), 0);
    check_eq!(mask.num_set_bits(), 0);
    check_eq!(mask.find_first_set_bit(), None);
    check_eq!(mask.find_first_clr_bit(), Some(0));
    check_eq!(mask.iter_set_bits().next(), None);

    // Boundary bits: 0, 63, 64, 127.
    let boundary_bits: [u32; 4] = [0, 63, 64, 127];
    for &b in &boundary_bits {
        mask.set_bit(b);
        check!(mask.test_bit(b));
    }
    check_eq!(mask.num_set_bits(), 4);
    check_eq!(mask.find_first_set_bit(), Some(0));
    check_eq!(mask.find_first_clr_bit(), Some(1));

    // Verify `Bitmask128Iter` yields set bits in ascending order across low and high 64-bit halves.
    let mut iter_idx = 0usize;
    for b in &mask {
        check!(iter_idx < boundary_bits.len());
        check_eq!(b, boundary_bits[iter_idx]);
        iter_idx += 1;
    }
    check_eq!(iter_idx, boundary_bits.len());

    // `clear_bit` and `assign_bit`.
    mask.clear_bit(0);
    check!(!mask.test_bit(0));
    check_eq!(mask.find_first_set_bit(), Some(63));
    mask.clear_bit(63);
    check_eq!(mask.find_first_set_bit(), Some(64));
    mask.assign_bit(64, false);
    check_eq!(mask.find_first_set_bit(), Some(127));
    mask.assign_bit(10, true);
    check_eq!(mask.find_first_set_bit(), Some(10));

    // `clear_all` and `set_lowest_n_bits` across word boundaries (`0`, `1`, `63`, `64`, `65`,
    // `127`, `128`).
    for &n in &[0u32, 1, 63, 64, 65, 127, 128] {
        mask.clear_all();
        mask.set_lowest_n_bits(n);
        check_eq!(mask.num_set_bits(), n);
        if n == 0 {
            check_eq!(mask.find_first_set_bit(), None);
            check_eq!(mask.find_first_clr_bit(), Some(0));
        } else if n == 128 {
            check_eq!(mask.raw(), u128::MAX);
            check_eq!(mask.find_first_set_bit(), Some(0));
            check_eq!(mask.find_first_clr_bit(), None);
        } else {
            check_eq!(mask.find_first_set_bit(), Some(0));
            check_eq!(mask.find_first_clr_bit(), Some(n));
            check!(mask.test_bit(n - 1));
            check!(!mask.test_bit(n));
        }
    }

    // `set_all` and `from_raw`.
    mask.clear_all();
    mask.set_all();
    check_eq!(mask, Bitmask128::from_raw(u128::MAX));
    check_eq!(mask.num_set_bits(), 128);
    check_eq!(mask.find_first_clr_bit(), None);
    mask.clear_bit(99);
    check_eq!(mask.find_first_clr_bit(), Some(99));

    true
}

// ============================================================================
// 4. `PageCache` Unit Tests
// ============================================================================

/// Returns `true` if the 4 KiB page at `paddr` in the kernel/mock physmap is completely
/// zero-filled.
///
/// # Safety
///
/// `paddr` must be the physical address of an allocated 4 KiB page with no concurrent mutable
/// access during the call.
unsafe fn is_page_zeroed(paddr: u64) -> bool {
    let vaddr = SmmuEnv::paddr_to_physmap(paddr);
    // SAFETY: Caller guarantees `paddr` is an allocated page valid for shared read access.
    let bytes = unsafe { &*core::ptr::with_exposed_provenance::<[u8; PAGE_SIZE_USIZE]>(vaddr) };
    bytes.iter().all(|&b| b == 0)
}

/// Recursively frees all valid translation table pages reachable from `table_page` back into
/// `cache` during early-return test cleanup.
fn free_test_table_tree(table_page: SmmuPage, level: usize, cache: &mut PageCache) {
    let vaddr = SmmuEnv::paddr_to_physmap(table_page.paddr());
    // SAFETY: `table_page` is a valid translation table page allocated by the test.
    let entries = unsafe { table_slice(vaddr) };
    for entry_ref in entries.iter() {
        let entry = entry_ref.load(Ordering::Relaxed);
        if entry != 0 {
            if level + 1 < LEVELS
                && is_valid_entry(entry)
                && is_table_entry(entry)
                && let Some(child) = SmmuEnv::paddr_to_page(get_table_entry_paddr(entry))
            {
                free_test_table_tree(child, level + 1, cache);
            }
            entry_ref.store(0, Ordering::Relaxed);
        }
    }
    // SAFETY: `table_page` was checked out from `cache`, is unlinked, and all entries were zeroed
    // above.
    unsafe {
        cache.return_page(table_page);
    }
}

/// Test wrapper around [`PageCache`] that returns any in-flight pages and trims the cache to 0 on
/// early test failure return before [`PageCache::drop`] runs its teardown `debug_assert!`s.
struct TestPageCache {
    inner: PageCache,
    in_flight: [Option<SmmuPage>; 8],
    root_page: Option<SmmuPage>,
}

impl TestPageCache {
    const fn new() -> Self {
        Self { inner: PageCache::new(), in_flight: [None; 8], root_page: None }
    }

    fn get_page(&mut self) -> Result<SmmuPage, Status> {
        let page = self.inner.get_page()?;
        if let Some(slot) = self.in_flight.iter_mut().find(|s| s.is_none()) {
            *slot = Some(page);
        }
        Ok(page)
    }

    fn get_root_page(&mut self) -> Result<SmmuPage, Status> {
        let page = self.inner.get_page()?;
        self.root_page = Some(page);
        Ok(page)
    }

    fn return_page(&mut self, page: SmmuPage) {
        if self.root_page == Some(page) {
            self.root_page = None;
        }
        if let Some(slot) = self.in_flight.iter_mut().find(|s| **s == Some(page)) {
            *slot = None;
        }
        // SAFETY: Test callers only return unlinked pages checked out from `self.inner`.
        unsafe {
            self.inner.return_page(page);
        }
    }
}

impl core::ops::Deref for TestPageCache {
    type Target = PageCache;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl core::ops::DerefMut for TestPageCache {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Drop for TestPageCache {
    fn drop(&mut self) {
        if self.inner.in_flight_pages() != 0 || self.inner.cache_entries() != 0 {
            if let Some(root) = self.root_page.take() {
                free_test_table_tree(root, 0, &mut self.inner);
            }
            for slot in &mut self.in_flight {
                if let Some(page) = slot.take() {
                    // SAFETY: `page` was checked out from `self.inner` and is unlinked.
                    unsafe {
                        self.inner.return_page(page);
                    }
                }
            }
            self.inner.trim(0);
        }
    }
}

/// Tests `PageCache` allocation (`get_page`), zero-filling, `return_page`, and LIFO reuse
/// ordering.
pub fn test_page_cache_add_and_get() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] PageCacheAddAndGet\n");

    let mut cache = TestPageCache::new();
    check_eq!(cache.in_flight_pages(), 0);
    check_eq!(cache.cache_entries(), 0);

    let mut pages = [None; 4];
    for (i, slot) in pages.iter_mut().enumerate() {
        let Ok(page) = cache.get_page() else {
            return false;
        };
        check_eq!(cache.in_flight_pages(), (i + 1) as u32);
        check_eq!(cache.cache_entries(), 0);

        // Newly allocated pages from `PageCache::get_page` must be completely zero-filled and
        // cleaned to PoC DRAM.
        // SAFETY: `page` was just checked out from `cache` and is exclusively owned here.
        check!(unsafe { is_page_zeroed(page.paddr()) });

        *slot = Some(page);
    }

    // Return all 4 pages to the cache.
    for (i, slot) in pages.iter().enumerate() {
        cache.return_page(slot.unwrap());
        check_eq!(cache.in_flight_pages(), (3 - i) as u32);
        check_eq!(cache.cache_entries(), (i + 1) as u32);
    }

    // Pop all 4 pages again: must come from the cache in LIFO order (`pages[3..=0]`, stack
    // behavior, not queue behavior) and be zeroed.
    for i in 0..4 {
        let Ok(page) = cache.get_page() else {
            return false;
        };
        check_eq!(page, pages[3 - i].unwrap());
        check_eq!(cache.in_flight_pages(), (i + 1) as u32);
        check_eq!(cache.cache_entries(), (3 - i) as u32);

        // SAFETY: `page` was just checked out from `cache` and is exclusively owned here.
        check!(unsafe { is_page_zeroed(page.paddr()) });
    }

    // Return and trim to 0 before dropping `cache`.
    for slot in pages.iter().rev() {
        cache.return_page(slot.unwrap());
    }
    cache.trim(0);
    check_eq!(cache.in_flight_pages(), 0);
    check_eq!(cache.cache_entries(), 0);

    true
}

/// Tests `PageCache::trim` FIFO tail eviction and LIFO head retention.
pub fn test_page_cache_trim() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] PageCacheTrim\n");

    let mut cache = TestPageCache::new();
    let mut pages = [None; 4];
    for slot in &mut pages {
        let Ok(p) = cache.get_page() else {
            return false;
        };
        *slot = Some(p);
    }

    // Return `pages[0..4]` in order so `pages[3]` is at the MRU head and `pages[0]` is at the LRU
    // tail.
    for slot in &pages {
        cache.return_page(slot.unwrap());
    }
    check_eq!(cache.cache_entries(), 4);

    // Trimming to `>= 4` is a no-op.
    cache.trim(5);
    check_eq!(cache.cache_entries(), 4);
    cache.trim(4);
    check_eq!(cache.cache_entries(), 4);

    // Trimming to 3 must evict the oldest tail page (`pages[0]`) and retain `pages[3], pages[2],
    // pages[1]` from head to tail.
    cache.trim(3);
    check_eq!(cache.cache_entries(), 3);

    let Ok(pop3) = cache.get_page() else {
        return false;
    };
    let Ok(pop2) = cache.get_page() else {
        return false;
    };
    let Ok(pop1) = cache.get_page() else {
        return false;
    };
    check_eq!(pop3, pages[3].unwrap());
    check_eq!(pop2, pages[2].unwrap());
    check_eq!(pop1, pages[1].unwrap());
    check_eq!(cache.cache_entries(), 0);

    // Return `pop1`, `pop2`, `pop3` so `pop3` is head and `pop1` is tail, then step `trim` down
    // to 0.
    cache.return_page(pop1);
    cache.return_page(pop2);
    cache.return_page(pop3);
    check_eq!(cache.cache_entries(), 3);

    cache.trim(2);
    check_eq!(cache.cache_entries(), 2);

    cache.trim(1);
    check_eq!(cache.cache_entries(), 1);

    // The single remaining cached page must be the MRU head (`pop3`).
    let Ok(last_pop) = cache.get_page() else {
        return false;
    };
    check_eq!(last_pop, pop3);
    cache.return_page(last_pop);

    cache.trim(0);
    check_eq!(cache.cache_entries(), 0);
    check_eq!(cache.in_flight_pages(), 0);

    true
}

/// Tests `PageCache` OOM handling when the underlying PMM reaches its allocation limit.
pub fn test_page_cache_oom() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] PageCacheOom\n");

    // The budget belongs to this cache alone, so it cannot affect (or be consumed by) any
    // other address space, and it goes away with the cache.
    let mut cache = TestPageCache::new();
    cache.set_test_alloc_budget(Some(1));
    let Ok(p1) = cache.get_page() else {
        return false;
    };
    check_eq!(cache.in_flight_pages(), 1);

    // Second allocation from PMM must fail with `Status::NO_MEMORY`.
    check_eq!(cache.get_page(), Err(Status::NO_MEMORY));
    check_eq!(cache.in_flight_pages(), 1);

    // Returning `p1` to the cache allows `get_page()` to succeed from the cache without
    // allocating a new page from the PMM.
    cache.return_page(p1);
    check_eq!(cache.cache_entries(), 1);
    let Ok(reused) = cache.get_page() else {
        return false;
    };
    check_eq!(reused, p1);

    cache.return_page(reused);
    cache.trim(0);

    true
}

// ============================================================================
// 5. `TranslationTableHelper` Unit Tests
// ============================================================================

/// Tests `TranslationTableHelper` 4-level cursor walks across L1/L2/L3 boundaries, PoC dcache
/// flushing on `finish_operation`, and partial vs. full intermediate table reclamation on unmap.
pub fn test_translation_table_helper_walk_and_reclaim() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] TranslationTableHelperWalkAndReclaim\n");

    let aspace_start: u64 = 0x1000;
    let aspace_len: u64 = (1u64 << 39) - aspace_start;
    pin_init::stack_pin_init!(let avail_regions = RegionAllocator::init());
    let avail_regions = avail_regions.as_ref();
    check!(
        avail_regions
            .add_region(RegionSpan { base: aspace_start, size: aspace_len }, AllowOverlap::No)
            .is_ok()
    );

    let mut page_cache = TestPageCache::new();
    let Ok(root_page) = page_cache.get_root_page() else {
        return false;
    };
    check_eq!(page_cache.in_flight_pages(), 1);

    // Part A: Map 2 consecutive pages straddling the 1 GiB (L1[0] -> L1[1]) boundary:
    // Page 0 at `(1 << 30) - PAGE_SIZE` (`L0[0], L1[0], L2[511], L3[511]`)
    // Page 1 at `1 << 30` (`L0[0], L1[1], L2[0], L3[0]`)
    let straddle_va = (1u64 << 30) - PAGE_SIZE;
    let Ok(straddle_reg) =
        avail_regions.get_region_specific(RegionSpan { base: straddle_va, size: 2 * PAGE_SIZE })
    else {
        return false;
    };

    let mut helper = TranslationTableHelper::new();
    // SAFETY: `root_page` and all intermediate table pages are valid, live 4 KiB pages checked out
    // from `page_cache`.
    unsafe {
        check!(helper.initialize_for_map(straddle_va, root_page, &mut page_cache).is_ok());
        // `initialize_for_map` allocates L1[0], L2[511], and L3 (3 intermediate pages + 1 root =
        // 4).
        check_eq!(page_cache.in_flight_pages(), 4);
        check!(!helper.current_page_entry_valid());

        let pa0: u64 = 0x0000_2000_0000;
        let pa1: u64 = 0x0000_2000_1000;
        let pte0 = make_page_entry(pa0, IOMMU_FLAG_PERM_READ);
        let pte1 = make_page_entry(pa1, IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE);

        helper.assign_page_entry(pte0);
        check!(helper.current_page_entry_valid());

        // Advance across the 1 GiB boundary: finishes L3 and L2 under L1[0] and allocates new L2[0]
        // and L3[0] under L1[1] (+2 pages -> 6 total in flight).
        check!(helper.advance(aspace_start, aspace_len, &avail_regions, &mut page_cache).is_ok());
        check_eq!(page_cache.in_flight_pages(), 6);
        check!(!helper.current_page_entry_valid());

        helper.assign_page_entry(pte1);
        check!(helper.current_page_entry_valid());
        helper.finish_operation(aspace_start, aspace_len, &avail_regions, &mut page_cache);
    }

    // Unmap the 2 straddling pages after dropping `straddle_reg` (returning the VA range to
    // `avail_regions`), and verify all 5 intermediate tables (L1, two L2s, two L3s) are reclaimed!
    drop(straddle_reg);
    // SAFETY: `root_page` and all intermediate table pages are valid, live 4 KiB pages checked out
    // from `page_cache`.
    unsafe {
        check!(helper.initialize_for_unmap(straddle_va, root_page, &mut page_cache).is_ok());
        check!(helper.current_page_entry_valid());
        helper.assign_page_entry(0);
        check!(helper.advance(aspace_start, aspace_len, &avail_regions, &mut page_cache).is_ok());
        check!(helper.current_page_entry_valid());
        helper.assign_page_entry(0);
        helper.finish_operation(aspace_start, aspace_len, &avail_regions, &mut page_cache);
    }

    // Only the root L0 page should remain in flight; the 5 intermediate pages are back in cache.
    check_eq!(page_cache.in_flight_pages(), 1);
    check_eq!(page_cache.cache_entries(), 5);

    // Part B: Map two sibling pages in the same L3 table (`0x1000` and `0x2000`), then unmap one
    // at a time to verify partial unmap retains intermediate tables while final unmap reclaims
    // them.
    let Ok(reg_a) = avail_regions.get_region_specific(RegionSpan { base: 0x1000, size: PAGE_SIZE })
    else {
        page_cache.return_page(root_page);
        page_cache.trim(0);
        return false;
    };
    let Ok(reg_b) = avail_regions.get_region_specific(RegionSpan { base: 0x2000, size: PAGE_SIZE })
    else {
        page_cache.return_page(root_page);
        page_cache.trim(0);
        return false;
    };

    let pa0: u64 = 0x0000_2000_0000;
    let pa1: u64 = 0x0000_2000_1000;
    let pte0 = make_page_entry(pa0, IOMMU_FLAG_PERM_READ);
    let pte1 = make_page_entry(pa1, IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE);

    // SAFETY: `root_page` and all intermediate table pages are valid, live 4 KiB pages checked out
    // from `page_cache`.
    unsafe {
        check!(helper.initialize_for_map(0x1000, root_page, &mut page_cache).is_ok());
        helper.assign_page_entry(pte0);
        check!(helper.advance(aspace_start, aspace_len, &avail_regions, &mut page_cache).is_ok());
        helper.assign_page_entry(pte1);
        helper.finish_operation(aspace_start, aspace_len, &avail_regions, &mut page_cache);
    }
    // Root L0 + L1 + L2 + L3 = 4 pages in flight, 2 pages remaining in cache.
    check_eq!(page_cache.in_flight_pages(), 4);
    check_eq!(page_cache.cache_entries(), 2);

    // Unmap `0x1000` while `0x2000` (`reg_b`) is still allocated: L1/L2/L3 must NOT be reclaimed.
    drop(reg_a);
    // SAFETY: `root_page` and all intermediate table pages are valid, live 4 KiB pages checked out
    // from `page_cache`.
    unsafe {
        check!(helper.initialize_for_unmap(0x1000, root_page, &mut page_cache).is_ok());
        helper.assign_page_entry(0);
        helper.finish_operation(aspace_start, aspace_len, &avail_regions, &mut page_cache);
    }
    check_eq!(page_cache.in_flight_pages(), 4);
    check_eq!(page_cache.cache_entries(), 2);

    // Now drop `reg_b` and unmap `0x2000`: L3, L2, and L1 are now completely unused and must all
    // be reclaimed into `page_cache`.
    drop(reg_b);
    // SAFETY: `root_page` and all intermediate table pages are valid, live 4 KiB pages checked out
    // from `page_cache`.
    unsafe {
        check!(helper.initialize_for_unmap(0x2000, root_page, &mut page_cache).is_ok());
        helper.assign_page_entry(0);
        helper.finish_operation(aspace_start, aspace_len, &avail_regions, &mut page_cache);
    }
    check_eq!(page_cache.in_flight_pages(), 1);
    check_eq!(page_cache.cache_entries(), 5);

    page_cache.return_page(root_page);
    page_cache.trim(0);
    check_eq!(page_cache.in_flight_pages(), 0);
    check_eq!(page_cache.cache_entries(), 0);

    true
}

// ============================================================================
// 6. `DeviceAspace` Unit Tests
// ============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExpectedRegion {
    vaddr: u64,
    paddr: u64,
    len: u64,
    perms: u32,
}

/// Mock `PinnedVmoSource` backed by a slice of physical `(paddr, len)` segments.
struct MockPinnedVmo<'a> {
    segments: &'a [(u64, u64)],
    offset: u64,
    size: u64,
}

impl<'a> MockPinnedVmo<'a> {
    fn new(segments: &'a [(u64, u64)]) -> Self {
        let mut total = 0u64;
        for &(_pa, len) in segments {
            total += len;
        }
        Self { segments, offset: 0, size: total }
    }

    fn new_with_size(segments: &'a [(u64, u64)], size: u64) -> Self {
        Self { segments, offset: 0, size }
    }
}

impl PinnedVmoSource for MockPinnedVmo<'_> {
    fn offset(&self) -> u64 {
        self.offset
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn lookup_contiguous(&self, offset: u64, size: u64) -> Result<u64, Status> {
        if offset < self.offset || size == 0 || (offset - self.offset) + size > self.size {
            return Err(Status::INVALID_ARGS);
        }
        let mut cur_off = self.offset;
        for &(seg_pa, seg_len) in self.segments {
            if offset >= cur_off && offset < cur_off + seg_len {
                let rel = offset - cur_off;
                if rel + size > seg_len {
                    return Err(Status::NOT_FOUND);
                }
                return Ok(seg_pa + rel);
            }
            cur_off += seg_len;
        }
        Err(Status::NOT_FOUND)
    }
}

#[derive(Default)]
struct RecordingInvalidator {
    count: usize,
    last_base: u64,
    last_size: u64,
}

impl TlbInvalidator for RecordingInvalidator {
    fn invalidate(&mut self, base: u64, size: u64) {
        self.count += 1;
        self.last_base = base;
        self.last_size = size;
    }
}

/// Test guard that ensures `DeviceAspace::free_translation_tables` runs even if a `check!` returns
/// early.
struct TestAspace(kalloc::Box<DeviceAspace>);

impl core::ops::Deref for TestAspace {
    type Target = DeviceAspace;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl core::ops::DerefMut for TestAspace {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for TestAspace {
    fn drop(&mut self) {
        if self.0.page_cache().in_flight_pages() != 0 || self.0.page_cache().cache_entries() != 0 {
            let mut inval = RecordingInvalidator::default();
            self.0.free_translation_tables(&mut inval);
        }
    }
}

struct WalkVerificationState<'a> {
    expected: &'a [ExpectedRegion],
    region_idx: usize,
    region_offset: u64,
    table_pages: u32,
    aspace_start: u64,
    aspace_end: u64,
    avail_regions: &'a RegionAllocator,
}

/// Recursively walks levels `0..=3` of a VMSAv8-64 translation table rooted at `table_pa` and
/// verifies that the translation table entries accurately reflect the expected allocated and
/// available regions.
///
/// The invariants verified by this function are:
/// 1. Any valid leaf entry (`level == 3`) must be fully contained within the set of regions marked
///    as `Allocated` in `avail_regions`.
/// 2. Any valid leaf entry must map to the expected physical address and have the correct access
///    permissions (`make_page_entry(reg.paddr + state.region_offset, reg.perms)`).
/// 3. Any invalid entry (or portion of an invalid entry) that falls within the bounds of the
///    active address space (`[aspace_start, aspace_end)`) must be fully contained within the set of
///    regions marked as `Available` (free/unmapped space).
/// 4. There cannot be any Block entries (only Table entries at `level < 3` or Page entries at
///    `level == 3`).
/// 5. Invalid entries must be strictly zero (`entry == 0`), not just missing the valid bit.
/// 6. Intermediate tables (`level > 0`) are never empty (all 512 entries invalid), all entries
///    match PoC DRAM and the CPU cache on host, and the total count of visited translation table
///    pages (`state.table_pages`) equals `PageCache::in_flight_pages()`.
fn verify_table_level(
    table_pa: u64,
    level: usize,
    base_vaddr: u64,
    state: &mut WalkVerificationState<'_>,
) -> bool {
    state.table_pages += 1;
    let table_vaddr = SmmuEnv::paddr_to_physmap(table_pa);
    check!(table_vaddr != 0);
    // SAFETY: `table_vaddr` is the physmap virtual address of a valid 4 KiB translation table page.
    let table = unsafe { table_slice(table_vaddr) };

    let shift = PAGE_SHIFT + (9 * ((LEVELS - level - 1) as u32));
    let entry_span = 1u64 << shift;
    let mut non_empty = false;

    for (i, slot) in table.iter().enumerate() {
        let entry = slot.load(Ordering::Relaxed);
        let entry_vaddr = base_vaddr + (i as u64) * entry_span;
        let entry_end = entry_vaddr + entry_span;

        if !is_valid_entry(entry) {
            check_eq!(entry, 0);
            let check_start = entry_vaddr.max(state.aspace_start);
            let check_end = entry_end.min(state.aspace_end);
            if check_start < check_end {
                let contained = state.avail_regions.test_region_contained_by(
                    RegionSpan { base: check_start, size: check_end - check_start },
                    TestRegionSet::Available,
                );
                check_eq!(contained, Ok(true));
            }
            // If we were partway through matching an expected region, a gap inside that region is
            // an error.
            check_eq!(state.region_offset, 0);
            continue;
        }

        non_empty = true;
        if level < LEVELS - 1 {
            check!(is_table_entry(entry));
            check!(!is_block_entry(entry, level as u32));
            let next_pa = get_table_entry_paddr(entry);
            check!(is_valid_addr(next_pa));
            check!(verify_table_level(next_pa, level + 1, entry_vaddr, state));
        } else {
            // Level 3 leaf page descriptor.
            check!(!is_block_entry(entry, 3));
            let contained = state.avail_regions.test_region_contained_by(
                RegionSpan { base: entry_vaddr, size: entry_span },
                TestRegionSet::Allocated,
            );
            check_eq!(contained, Ok(true));

            // Walk `state.expected` (sorted in ascending virtual address order) in lockstep with
            // the depth-first page table traversal, verifying that each mapped leaf page matches
            // the expected region's virtual address, physical address, and permissions.
            check!(state.region_idx < state.expected.len());
            let reg = &state.expected[state.region_idx];
            check_eq!(entry_vaddr, reg.vaddr + state.region_offset);
            let expected_pte = make_page_entry(reg.paddr + state.region_offset, reg.perms);
            check_eq!(entry, expected_pte);

            state.region_offset += PAGE_SIZE;
            if state.region_offset == reg.len {
                state.region_idx += 1;
                state.region_offset = 0;
            }
        }
    }

    // No intermediate table (`level > 0`) is permitted to remain allocated if all 512 of its
    // entries are invalid (`0`).
    if level > 0 {
        check!(non_empty);
    }
    true
}

/// Helper to kick off the recursive verification of the page table against the region allocator's
/// state and `PageCache` in-flight page count. This checks that every mapped and unmapped region
/// is correctly accounted for in the 4-level page table structure.
fn verify_allocated_regions(aspace: &DeviceAspace, expected: &[ExpectedRegion]) -> bool {
    for reg in expected {
        let contained = aspace.avail_regions().test_region_contained_by(
            RegionSpan { base: reg.vaddr, size: reg.len },
            TestRegionSet::Allocated,
        );
        check_eq!(contained, Ok(true));
    }

    let root_pa = aspace.root_tt_paddr();
    check!(root_pa != 0);
    let aspace_start = aspace.first_valid_address();
    let aspace_end = aspace.last_valid_address() + 1;
    let mut state = WalkVerificationState {
        expected,
        region_idx: 0,
        region_offset: 0,
        table_pages: 0,
        aspace_start,
        aspace_end,
        avail_regions: aspace.avail_regions(),
    };
    check!(verify_table_level(root_pa, 0, 0, &mut state));
    check_eq!(state.region_idx, expected.len());
    check_eq!(state.region_offset, 0);
    check_eq!(state.table_pages, aspace.page_cache().in_flight_pages());
    true
}

/// Maps and unmaps a 3-segment scattered 6-page VMO (with both single-page and multi-page
/// contiguous segment runs) across all `0..=7` permission flag combinations, verifying the
/// 4-level page table tree at each step.
pub fn test_device_aspace_map_and_unmap() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] DeviceAspaceMapAndUnmap\n");

    // 48-bit address space, starting at 1 MiB.
    let aspace_start: u64 = 1u64 << 20;
    let aspace_end: u64 = 1u64 << 48;
    // Don't cache any pages (`max_cache_pages = 0`). This makes it easier to assert that we have
    // released the proper number of pages as we go through the tests.
    let Ok(aspace) = DeviceAspace::create(aspace_start, aspace_end - aspace_start, 0) else {
        return false;
    };
    let mut aspace = TestAspace(aspace);
    // We should have 1 page allocated at this point (for the root translation table page);
    // `verify_allocated_regions` checks this.
    check!(verify_allocated_regions(&aspace, &[]));

    // Create mock VMO with virtual offset to physical address segment mappings.
    let segments: [(u64, u64); 3] =
        [(0x0010_0000, 2 * PAGE_SIZE), (0x0020_0000, PAGE_SIZE), (0x0030_0000, 3 * PAGE_SIZE)];
    let vmo = MockPinnedVmo::new(&segments);

    // Now perform the mapping operation, then verify that the page tables match what we would
    // expect based on our region allocator and expected mappings state. Test all combinations of
    // the IOMMU permission flags.
    for perms in 0..=ALL_PERM_FLAGS {
        let mut inval = RecordingInvalidator::default();
        let Ok(alloc) = aspace.map(&vmo, perms, &mut inval, None) else {
            return false;
        };
        check_eq!(alloc.base(), aspace_start);
        check_eq!(alloc.size(), 6 * PAGE_SIZE);
        check_eq!(inval.count, 1);
        check_eq!(inval.last_base, alloc.base());
        check_eq!(inval.last_size, alloc.size());

        let expected = [
            ExpectedRegion { vaddr: alloc.base(), paddr: segments[0].0, len: segments[0].1, perms },
            ExpectedRegion {
                vaddr: alloc.base() + 2 * PAGE_SIZE,
                paddr: segments[1].0,
                len: segments[1].1,
                perms,
            },
            ExpectedRegion {
                vaddr: alloc.base() + 3 * PAGE_SIZE,
                paddr: segments[2].0,
                len: segments[2].1,
                perms,
            },
        ];
        check!(verify_allocated_regions(&aspace, &expected));

        aspace.unmap(alloc, &mut inval);
        check_eq!(inval.count, 2);
        check_eq!(inval.last_base, aspace_start);
        check_eq!(inval.last_size, 6 * PAGE_SIZE);
        check!(verify_allocated_regions(&aspace, &[]));
        check_eq!(aspace.page_cache().in_flight_pages(), 1);
        check_eq!(aspace.page_cache().cache_entries(), 0);
    }

    let mut inval = RecordingInvalidator::default();
    aspace.free_translation_tables(&mut inval);
    check_eq!(inval.count, 1);
    check_eq!(inval.last_base, 0);
    check_eq!(inval.last_size, 0);
    // The root table page has been returned and freed along with everything else. (Asking for
    // `root_tt_paddr()` at this point is a debug-asserted error.)
    check_eq!(aspace.page_cache().in_flight_pages(), 0);
    check_eq!(aspace.page_cache().cache_entries(), 0);

    true
}

/// Tests `DeviceAspace::map` at a caller-specified virtual address.
pub fn test_device_aspace_map_at_specific_location() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] DeviceAspaceMapAtSpecificLocation\n");

    // 48-bit address space, starting at 1 MiB.
    let aspace_start: u64 = 1u64 << 20;
    let aspace_end: u64 = 1u64 << 48;
    let Ok(aspace) = DeviceAspace::create(aspace_start, aspace_end - aspace_start, 0) else {
        return false;
    };
    let mut aspace = TestAspace(aspace);
    check!(verify_allocated_regions(&aspace, &[]));

    // Create mock VMO with virtual offset to physical address segment mappings.
    let segments = [(0x0040_0000, 4 * PAGE_SIZE)];
    let vmo = MockPinnedVmo::new(&segments);
    // Pick a page-aligned target location in the address space.
    let target_vaddr: u64 = 0x1234_5000;
    let perms = IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE;
    let mut inval = RecordingInvalidator::default();

    let Ok(alloc) = aspace.map(&vmo, perms, &mut inval, Some(target_vaddr)) else {
        return false;
    };
    check_eq!(alloc.base(), target_vaddr);
    check_eq!(alloc.size(), 4 * PAGE_SIZE);

    let expected =
        [ExpectedRegion { vaddr: target_vaddr, paddr: segments[0].0, len: segments[0].1, perms }];
    check!(verify_allocated_regions(&aspace, &expected));

    // Unmap and revalidate the page tables once again.
    aspace.unmap(alloc, &mut inval);
    check!(verify_allocated_regions(&aspace, &[]));
    aspace.free_translation_tables(&mut inval);
    true
}

/// Creates a 2-page mapping which straddles all of the translation table levels (`(1 << 39) -
/// PAGE_SIZE`). The first page requires an entry in the last slot of every level of the
/// translation tables, while the second page requires an entry in the first slot of every level of
/// the translation table.
pub fn test_device_aspace_map_straddling_table_levels() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] DeviceAspaceMapStraddlingTableLevels\n");

    // 48-bit address space, starting at 1 MiB.
    let aspace_start: u64 = 1u64 << 20;
    let aspace_end: u64 = 1u64 << 48;
    let Ok(aspace) = DeviceAspace::create(aspace_start, aspace_end - aspace_start, 0) else {
        return false;
    };
    let mut aspace = TestAspace(aspace);
    check!(verify_allocated_regions(&aspace, &[]));

    let segments = [(0x0050_0000, PAGE_SIZE), (0x0060_0000, PAGE_SIZE)];
    let vmo = MockPinnedVmo::new(&segments);
    // Pick a location which straddles all of the translation table levels. The L0/L1 boundary is
    // at 1 << 39.
    let target_vaddr: u64 = (1u64 << 39) - PAGE_SIZE;
    let perms = IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_EXECUTE;
    let mut inval = RecordingInvalidator::default();

    let Ok(alloc) = aspace.map(&vmo, perms, &mut inval, Some(target_vaddr)) else {
        return false;
    };
    check_eq!(alloc.base(), target_vaddr);
    check_eq!(alloc.size(), 2 * PAGE_SIZE);
    // Root L0 + two full L1/L2/L3 chains = 1 + 6 = 7 translation table pages in flight.
    check_eq!(aspace.page_cache().in_flight_pages(), 7);

    let expected = [
        ExpectedRegion { vaddr: target_vaddr, paddr: segments[0].0, len: PAGE_SIZE, perms },
        ExpectedRegion {
            vaddr: target_vaddr + PAGE_SIZE,
            paddr: segments[1].0,
            len: PAGE_SIZE,
            perms,
        },
    ];
    check!(verify_allocated_regions(&aspace, &expected));

    // Unmap and revalidate the page tables once again.
    aspace.unmap(alloc, &mut inval);
    check!(verify_allocated_regions(&aspace, &[]));
    check_eq!(aspace.page_cache().in_flight_pages(), 1);
    aspace.free_translation_tables(&mut inval);
    true
}

/// Tests `DeviceAspace::map` failure injection and rollback.
///
/// Part 1 maps 6 pages straddling the 512 GiB `L0[0] -> L0[1]` boundary (`(1 << 39) - 5 *
/// PAGE_SIZE`), where the first 5 pages reside in the last 5 slots of `L0[0]`'s L1/L2/L3 table
/// chain and the 6th page resides in the first slot of `L0[1]`'s L1/L2/L3 table chain. The full
/// mapping requires 6 new intermediate table allocations:
/// 1. `L0[0]`'s L1 table (during `initialize_for_map` for page 0)
/// 2. `L0[0]`'s L2 table (during `initialize_for_map` for page 0)
/// 3. `L0[0]`'s L3 table (during `initialize_for_map` for page 0)
/// 4. `L0[1]`'s L1 table (during `advance` from page 4 to page 5)
/// 5. `L0[1]`'s L2 table (during `advance` from page 4 to page 5)
/// 6. `L0[1]`'s L3 table (during `advance` from page 4 to page 5)
///
/// By setting the allocation budget to `limit` in `0..=5`, we inject an allocation failure at each
/// of those 6 table allocations in turn and verify that `map` fails with `ZX_ERR_NO_MEMORY`,
/// invalidates the TLB, and rolls back all partially-allocated tables and regions.
///
/// Part 2 maps `vmo1` at `0x1000` and then injects failure across `limit` in `0..=2` while mapping
/// `vmo2` across the 1 GiB `L1[0] -> L1[1]` boundary, verifying that rollback preserves `vmo1`'s
/// shared L0/L1/L2 tables.
pub fn test_device_aspace_map_failure_injection() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] DeviceAspaceMapFailureInjection\n");

    let aspace_start: u64 = 0x1000;
    let aspace_end: u64 = 1u64 << 48;
    let Ok(aspace) = DeviceAspace::create(aspace_start, aspace_end - aspace_start, 0) else {
        return false;
    };
    let mut aspace = TestAspace(aspace);
    check!(verify_allocated_regions(&aspace, &[]));

    let segments = [(0x0070_0000, 5 * PAGE_SIZE), (0x0080_0000, PAGE_SIZE)];
    let vmo = MockPinnedVmo::new(&segments);
    let straddle_vaddr: u64 = (1u64 << 39) - 5 * PAGE_SIZE;
    let perms = IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE;
    let mut inval = RecordingInvalidator::default();

    for limit in 0..=5u32 {
        inval = RecordingInvalidator::default();
        aspace.set_test_alloc_budget(Some(limit));
        let res = aspace.map(&vmo, perms, &mut inval, Some(straddle_vaddr));
        aspace.set_test_alloc_budget(None);
        check_eq!(res.err(), Some(Status::NO_MEMORY));
        // Since it failed, it should have unmapped and invalidated.
        check_eq!(inval.count, 1);
        check_eq!(inval.last_base, straddle_vaddr);
        check_eq!(inval.last_size, 6 * PAGE_SIZE);

        // Confirm that nothing remained allocated in the RegionAllocator and the page tables
        // were properly cleaned up.
        check!(verify_allocated_regions(&aspace, &[]));
        check_eq!(aspace.page_cache().in_flight_pages(), 1);
        check_eq!(aspace.page_cache().cache_entries(), 0);
    }

    // Now map `vmo1` at `0x1000`, then inject failure while mapping `vmo2` at `(1 << 30) -
    // PAGE_SIZE` (which shares L0[0] and L1[0] with `vmo1` for its first page, then fails when
    // allocating L1[1]/L2[0]/L3[0] for its second page). Verify `vmo1` remains intact!
    let vmo1_segs = [(0x0090_0000, PAGE_SIZE)];
    let vmo1 = MockPinnedVmo::new(&vmo1_segs);
    let Ok(alloc1) = aspace.map(&vmo1, perms, &mut inval, Some(0x1000)) else {
        return false;
    };
    let expected_vmo1 =
        [ExpectedRegion { vaddr: 0x1000, paddr: 0x0090_0000, len: PAGE_SIZE, perms }];
    check!(verify_allocated_regions(&aspace, &expected_vmo1));

    let vmo2_segs = [(0x0070_0000, PAGE_SIZE), (0x0080_0000, PAGE_SIZE)];
    let vmo2 = MockPinnedVmo::new(&vmo2_segs);
    let straddle_l1 = (1u64 << 30) - PAGE_SIZE;
    // Because `vmo1` at `0x1000` already allocated the L1 table under `L0[0]` and the L2 table
    // under `L1[0]`, `straddle_l1` needs 1 new page (`L3` under `L2[511]`) for page 0 and 2 new
    // pages (`L2` and `L3` under `L1[1]`) for page 1 (3 new pages total). Limit the countdown
    // to `0..=2` pages so it fails at each step and preserves `vmo1`'s shared L0/L1/L2 tables!
    for limit in 0..=2u32 {
        inval = RecordingInvalidator::default();
        aspace.set_test_alloc_budget(Some(limit));
        let res2 = aspace.map(&vmo2, perms, &mut inval, Some(straddle_l1));
        aspace.set_test_alloc_budget(None);
        check_eq!(res2.err(), Some(Status::NO_MEMORY));
        check_eq!(inval.count, 1);
        check_eq!(inval.last_base, straddle_l1);
        check_eq!(inval.last_size, 2 * PAGE_SIZE);

        // `vmo1` must still be mapped and intact, and all tables allocated for `res2` must be
        // reclaimed.
        check!(verify_allocated_regions(&aspace, &expected_vmo1));
        check_eq!(aspace.page_cache().in_flight_pages(), 4);
    }

    aspace.unmap(alloc1, &mut inval);
    check!(verify_allocated_regions(&aspace, &[]));
    aspace.free_translation_tables(&mut inval);

    true
}

/// Verifies reference-free reclamation of shared L0/L1/L2/L3 translation tables when 5 mappings
/// sharing progressively deeper table levels are mapped and then unmapped in reverse order.
pub fn test_device_aspace_shared_table_levels() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] DeviceAspaceSharedTableLevels\n");

    let aspace_start: u64 = 1u64 << 20;
    let aspace_end: u64 = 1u64 << 48;
    // Configure `max_cache_pages = 12` to retain all reclaimed intermediate table pages on unmap.
    let Ok(aspace) = DeviceAspace::create(aspace_start, aspace_end - aspace_start, 12) else {
        return false;
    };
    let mut aspace = TestAspace(aspace);
    check!(verify_allocated_regions(&aspace, &[]));

    let seg = [(0x0100_0000, PAGE_SIZE)];
    let vmo = MockPinnedVmo::new(&seg);
    let perms = IOMMU_FLAG_PERM_READ;
    let mut inval = RecordingInvalidator::default();

    // Define a set of virtual addresses to map. The first one is our "base". The subsequent ones
    // share L0, L1, L2, and L3 tables with the base respectively.
    let base_vaddr: u64 = 2u64 << 39;
    let vaddr_l0 = base_vaddr + (1u64 << 39); // Different L0 index (shares L0 root table)
    let vaddr_l1 = base_vaddr + (1u64 << 30); // Different L1 index (shares L0, L1)
    let vaddr_l2 = base_vaddr + (1u64 << 21); // Different L2 index (shares L0, L1, L2)
    let vaddr_l3 = base_vaddr + (1u64 << 12); // Different L3 index (shares L0, L1, L2, L3)

    let reg_base = ExpectedRegion { vaddr: base_vaddr, paddr: seg[0].0, len: PAGE_SIZE, perms };
    let reg_l3 = ExpectedRegion { vaddr: vaddr_l3, paddr: seg[0].0, len: PAGE_SIZE, perms };
    let reg_l2 = ExpectedRegion { vaddr: vaddr_l2, paddr: seg[0].0, len: PAGE_SIZE, perms };
    let reg_l1 = ExpectedRegion { vaddr: vaddr_l1, paddr: seg[0].0, len: PAGE_SIZE, perms };
    let reg_l0 = ExpectedRegion { vaddr: vaddr_l0, paddr: seg[0].0, len: PAGE_SIZE, perms };

    // 1. Map `base_vaddr` (`L0[2], L1[0], L2[0], L3[0]`): root + 3 new tables = 4 in flight.
    let Ok(alloc_base) = aspace.map(&vmo, perms, &mut inval, Some(base_vaddr)) else {
        return false;
    };
    check_eq!(aspace.page_cache().in_flight_pages(), 4);
    check!(verify_allocated_regions(&aspace, &[reg_base]));

    // 2. Map `vaddr_l0` (`L0[3], L1[0], L2[0], L3[0]`): +3 new tables = 7 in flight.
    let Ok(alloc_l0) = aspace.map(&vmo, perms, &mut inval, Some(vaddr_l0)) else {
        return false;
    };
    check_eq!(aspace.page_cache().in_flight_pages(), 7);
    check!(verify_allocated_regions(&aspace, &[reg_base, reg_l0]));

    // 3. Map `vaddr_l1` (`L0[2], L1[1], L2[0], L3[0]`): shares L1, +2 new tables = 9 in flight.
    let Ok(alloc_l1) = aspace.map(&vmo, perms, &mut inval, Some(vaddr_l1)) else {
        return false;
    };
    check_eq!(aspace.page_cache().in_flight_pages(), 9);
    check!(verify_allocated_regions(&aspace, &[reg_base, reg_l1, reg_l0]));

    // 4. Map `vaddr_l2` (`L0[2], L1[0], L2[1], L3[0]`): shares L1/L2, +1 new table = 10 in flight.
    let Ok(alloc_l2) = aspace.map(&vmo, perms, &mut inval, Some(vaddr_l2)) else {
        return false;
    };
    check_eq!(aspace.page_cache().in_flight_pages(), 10);
    check!(verify_allocated_regions(&aspace, &[reg_base, reg_l2, reg_l1, reg_l0]));

    // 5. Map `vaddr_l3` (`L0[2], L1[0], L2[0], L3[1]`): shares L1/L2/L3, +0 tables = 10 in flight.
    let Ok(alloc_l3) = aspace.map(&vmo, perms, &mut inval, Some(vaddr_l3)) else {
        return false;
    };
    check_eq!(aspace.page_cache().in_flight_pages(), 10);
    check!(verify_allocated_regions(&aspace, &[reg_base, reg_l3, reg_l2, reg_l1, reg_l0]));

    // Now unmap in reverse order (`alloc_l3`, `alloc_l2`, `alloc_l1`, `alloc_l0`, `alloc_base`):
    // Unmap `alloc_l3`: `alloc_base` still uses L3, so 0 pages reclaimed.
    aspace.unmap(alloc_l3, &mut inval);
    check_eq!(aspace.page_cache().in_flight_pages(), 10);
    check_eq!(aspace.page_cache().cache_entries(), 0);
    check!(verify_allocated_regions(&aspace, &[reg_base, reg_l2, reg_l1, reg_l0]));

    // Unmap `alloc_l2`: its L3 table is reclaimed (1 cached); L2 remains shared with `alloc_base`.
    aspace.unmap(alloc_l2, &mut inval);
    check_eq!(aspace.page_cache().in_flight_pages(), 9);
    check_eq!(aspace.page_cache().cache_entries(), 1);
    check!(verify_allocated_regions(&aspace, &[reg_base, reg_l1, reg_l0]));

    // Unmap `alloc_l1`: its L3 and L2 tables are reclaimed (3 cached); L1 remains shared with
    // `alloc_base`.
    aspace.unmap(alloc_l1, &mut inval);
    check_eq!(aspace.page_cache().in_flight_pages(), 7);
    check_eq!(aspace.page_cache().cache_entries(), 3);
    check!(verify_allocated_regions(&aspace, &[reg_base, reg_l0]));

    // Unmap `alloc_l0`: its L3, L2, and L1 tables under `L0[3]` are reclaimed (6 cached).
    aspace.unmap(alloc_l0, &mut inval);
    check_eq!(aspace.page_cache().in_flight_pages(), 4);
    check_eq!(aspace.page_cache().cache_entries(), 6);
    check!(verify_allocated_regions(&aspace, &[reg_base]));

    // Unmap `alloc_base`: its L3, L2, and L1 tables under `L0[2]` are reclaimed (9 cached).
    aspace.unmap(alloc_base, &mut inval);
    check_eq!(aspace.page_cache().in_flight_pages(), 1);
    check_eq!(aspace.page_cache().cache_entries(), 9);
    check!(verify_allocated_regions(&aspace, &[]));

    aspace.free_translation_tables(&mut inval);
    check_eq!(aspace.page_cache().in_flight_pages(), 0);
    check_eq!(aspace.page_cache().cache_entries(), 0);
    true
}

/// Verifies alignment, `u64` overflow, bounds checks, overlap rejection, and adjacent region
/// allocation.
pub fn test_device_aspace_map_at_location_edge_cases() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] DeviceAspaceMapAtLocationEdgeCases\n");

    let aspace_start: u64 = 1u64 << 20;
    let aspace_len: u64 = (1u64 << 40) - aspace_start;
    let aspace_end = aspace_start + aspace_len;
    let Ok(aspace) = DeviceAspace::create(aspace_start, aspace_len, 0) else {
        return false;
    };
    let mut aspace = TestAspace(aspace);
    check!(verify_allocated_regions(&aspace, &[]));

    let segs = [(0x0200_0000, 2 * PAGE_SIZE)];
    let vmo = MockPinnedVmo::new(&segs);
    let unaligned_vmo = MockPinnedVmo::new_with_size(&segs, 2 * PAGE_SIZE - 1);
    let perms = IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE;
    let mut inval = RecordingInvalidator::default();

    // 1. Unaligned location or unaligned VMO size -> `INVALID_ARGS` with no TLB invalidation.
    // A. Location not page aligned.
    check_eq!(
        aspace.map(&vmo, perms, &mut inval, Some(0x1000_0001)).err(),
        Some(Status::INVALID_ARGS)
    );
    check_eq!(inval.count, 0);

    // B. Size not page aligned.
    check_eq!(
        aspace.map(&unaligned_vmo, perms, &mut inval, Some(0x1000_0000)).err(),
        Some(Status::INVALID_ARGS)
    );
    check_eq!(inval.count, 0);

    // 2. `u64` addition overflow (`u64::MAX - PAGE_SIZE + 1 + 2 * PAGE_SIZE`) -> `INVALID_ARGS`.
    // Use a location close to `u64::MAX` such that `location + size` overflows.
    check_eq!(
        aspace.map(&vmo, perms, &mut inval, Some(u64::MAX - PAGE_SIZE + 1)).err(),
        Some(Status::INVALID_ARGS)
    );
    check_eq!(inval.count, 0);

    // 3. Below `aspace_start` or extending past `aspace_end` -> `INVALID_ARGS`.
    // A. Region starts before `first_valid_address()`.
    check_eq!(
        aspace.map(&vmo, perms, &mut inval, Some(aspace_start - PAGE_SIZE)).err(),
        Some(Status::INVALID_ARGS)
    );
    check_eq!(inval.count, 0);

    // B. Region ends after `last_valid_address()` (`aspace_end` is the exclusive end of the valid
    //    address range, i.e. the last valid address is `aspace_end - 1`).
    check_eq!(
        aspace.map(&vmo, perms, &mut inval, Some(aspace_end - PAGE_SIZE)).err(),
        Some(Status::INVALID_ARGS)
    );
    check_eq!(inval.count, 0);

    // 4. Map at `0x1000_0000` (`[0x1000_0000, 0x1000_2000)`), then test overlapping requests
    //    (`ALREADY_EXISTS` with no TLB invalidation) and adjacent requests (`Ok`).
    // Map first region successfully.
    let Ok(alloc_mid) = aspace.map(&vmo, perms, &mut inval, Some(0x1000_0000)) else {
        return false;
    };
    inval = RecordingInvalidator::default();
    // Attempt to map another region overlapping the first region:
    // A. Exactly the same location.
    check_eq!(
        aspace.map(&vmo, perms, &mut inval, Some(0x1000_0000)).err(),
        Some(Status::ALREADY_EXISTS)
    );
    check_eq!(inval.count, 0);
    // B. Partially overlapping location (1 page offset forward).
    check_eq!(
        aspace.map(&vmo, perms, &mut inval, Some(0x1000_1000)).err(),
        Some(Status::ALREADY_EXISTS)
    );
    check_eq!(inval.count, 0);
    // C. Partially overlapping location (1 page offset backward).
    check_eq!(
        aspace.map(&vmo, perms, &mut inval, Some(0x0fff_f000)).err(),
        Some(Status::ALREADY_EXISTS)
    );
    check_eq!(inval.count, 0);

    // Immediately before (`0x0fff_e000`) and immediately after (`0x1000_2000`) must succeed.
    let Ok(alloc_before) = aspace.map(&vmo, perms, &mut inval, Some(0x0fff_e000)) else {
        return false;
    };
    let Ok(alloc_after) = aspace.map(&vmo, perms, &mut inval, Some(0x1000_2000)) else {
        return false;
    };

    aspace.unmap(alloc_before, &mut inval);
    aspace.unmap(alloc_mid, &mut inval);
    aspace.unmap(alloc_after, &mut inval);
    check!(verify_allocated_regions(&aspace, &[]));
    aspace.free_translation_tables(&mut inval);
    true
}

// ============================================================================
// Non-MMIO Bitfield Macro & `dump_bitfields` Unit Tests
// ============================================================================

/// Tests the non-MMIO arm of `define_smmu_reg!` (`struct Name(u64) => { ... }`) across integer,
/// boolean, enum, `[rsvdz]`, and `__res` fields without `FieldDesc` in scope, and exercises
/// `dump_bitfields` directly.
pub fn test_non_mmio_bitfields() -> bool {
    use super::common::mmio::{define_smmu_reg, dump_bitfields};

    dprintf!(INFO, "[Backend: Rust->Rust] NonMmioBitfields\n");

    #[repr(u32)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum TestCfg {
        Abort = 0,
        Bypass = 4,
        Stage1 = 5,
        Reserved = 7,
    }

    impl TestCfg {
        const fn from_raw(raw: u32) -> Self {
            match raw {
                0 => Self::Abort,
                4 => Self::Bypass,
                5 => Self::Stage1,
                _ => Self::Reserved,
            }
        }
    }

    define_smmu_reg! {
        /// Synthetic 64-bit in-memory structure doubleword covering all supported field kinds.
        pub struct TestStructDw(u64) => {
            63:63 => valid, set_valid ["VALID"]: bool;
            62:60 => [rsvdz];
            59:56 => res0, set_res0 ["__res0"]: u64;
            55:12 => addr, set_addr ["ADDR"]: u64;
            11:4  => [rsvdz];
            3:1   => cfg, set_cfg ["CFG"]: enum TestCfg;
            0:0   => leaf, set_leaf ["LEAF"]: bool;
        }
    }

    let expected_rsvdz: u64 = (0b111u64 << 60) | (0xffu64 << 4);
    check_eq!(TestStructDw::RSVDZ_MASK, expected_rsvdz);
    check_eq!(TestStructDw::default().reg_value(), 0);

    let mut dw = TestStructDw::from(0u64);
    dw.set_valid(true)
        .set_res0(0xa)
        .set_addr(0x1_2345_6789)
        .set_cfg(TestCfg::Stage1)
        .set_leaf(true);
    check!(dw.valid());
    check_eq!(dw.res0(), 0xa);
    check_eq!(dw.addr(), 0x1_2345_6789);
    check_eq!(dw.cfg(), TestCfg::Stage1);
    check!(dw.leaf());

    let expected_raw: u64 =
        (1u64 << 63) | (0xau64 << 56) | (0x1_2345_6789u64 << 12) | (5u64 << 1) | 1u64;
    check_eq!(dw.reg_value(), expected_raw);
    check_eq!(u64::from(dw), expected_raw);

    // Clearing boolean fields and changing enum/integer fields leaves adjacent bits intact.
    dw.set_valid(false).set_cfg(TestCfg::Bypass).set_leaf(false);
    check!(!dw.valid());
    check_eq!(dw.cfg(), TestCfg::Bypass);
    check!(!dw.leaf());
    check_eq!(dw.addr(), 0x1_2345_6789);
    check_eq!(dw.res0(), 0xa);

    // Raw value injection with RSVDZ bits set: verify `RSVDZ_MASK` strips only `[rsvdz]` ranges.
    let with_rsvdz = TestStructDw::from_value(expected_raw | expected_rsvdz);
    check_eq!(with_rsvdz.reg_value() & !TestStructDw::RSVDZ_MASK, expected_raw);

    let mut reset_dw = with_rsvdz;
    reset_dw.set_reg_value(expected_raw);
    check_eq!(reset_dw, TestStructDw::from_value(expected_raw));

    // Verify `FIELDS` metadata and `FieldDesc::extract` values.
    check_eq!(TestStructDw::FIELDS.len(), 7);
    check_eq!(TestStructDw::FIELDS[0].name, "VALID");
    check_eq!(TestStructDw::FIELDS[0].extract(expected_raw), 1);
    check_eq!(TestStructDw::FIELDS[1].name, "");
    check_eq!(TestStructDw::FIELDS[2].name, "__res0");
    check_eq!(TestStructDw::FIELDS[2].extract(expected_raw), 0xa);
    check_eq!(TestStructDw::FIELDS[3].name, "ADDR");
    check_eq!(TestStructDw::FIELDS[3].extract(expected_raw), 0x1_2345_6789);
    check_eq!(TestStructDw::FIELDS[4].name, "");
    check_eq!(TestStructDw::FIELDS[5].name, "CFG");
    check_eq!(TestStructDw::FIELDS[5].extract(expected_raw), 5);
    check_eq!(TestStructDw::FIELDS[6].name, "LEAF");
    check_eq!(TestStructDw::FIELDS[6].extract(expected_raw), 1);

    // Exercise `dump_bitfields` directly on the 64-bit structure doubleword.
    dump_bitfields(
        "TEST_DW0",
        0x4000_1000,
        expected_raw,
        core::mem::size_of::<u64>(),
        TestStructDw::FIELDS,
        1,
    );

    true
}

// ============================================================================
// 7. `EscapedName` Unit Tests
// ============================================================================

/// Tests that `EscapedName` stops at the first NUL, prints valid UTF-8 unchanged, escapes bytes
/// which are not valid UTF-8 as `\xNN`, and substitutes `<unknown>` for empty names on request.
pub fn test_escaped_name() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] EscapedName\n");

    let cases: &[(&[u8], &str)] = &[
        (b"", ""),
        (b"\0ignored", ""),
        (b"usb-bti\0\0\0", "usb-bti"),
        (b"no-terminator", "no-terminator"),
        // Valid multi-byte UTF-8 is printed as is.
        ("caf\u{e9}".as_bytes(), "caf\u{e9}"),
        // Latin-1 and other invalid bytes are escaped, keeping the valid text around them.
        (b"caf\xe9\0", "caf\\xe9"),
        (b"\xff\xfename", "\\xff\\xfename"),
        // A multi-byte character split by truncation is escaped byte by byte.
        (b"name-\xe2\x82", "name-\\xe2\\x82"),
    ];
    for &(raw, expected) in cases {
        let mut buf = TagBuf::new();
        check!(write!(&mut buf, "{}", EscapedName::new(raw)).is_ok());
        check_eq!(buf.as_str(), expected);
        check_eq!(EscapedName::new(raw).is_empty(), expected.is_empty());
    }

    let mut buf = TagBuf::new();
    check!(write!(&mut buf, "{}", EscapedName::new_or_unknown(b"\0")).is_ok());
    check_eq!(buf.as_str(), "<unknown>");
    buf.clear();
    check!(write!(&mut buf, "{}", EscapedName::new_or_unknown(b"x\0")).is_ok());
    check_eq!(buf.as_str(), "x");

    true
}

/// In-kernel `k ut arm_smmu` test suite wrapping the 18 version-neutral unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "arm_smmu")]
mod ktest_suite {
    /// Tests default SMMU mode strings (`"disabled"`, `"passthru"`, `"enforced"`).
    #[test]
    fn smmu_mode_defaults() {
        unittest::assert_true!(super::test_smmu_mode_defaults());
    }

    /// Tests per-base-address overrides, radix parsing, and malformed segment skipping.
    #[test]
    fn smmu_mode_advanced() {
        unittest::assert_true!(super::test_smmu_mode_advanced());
    }

    /// Tests ASCII case-insensitivity of SMMU mode tokens and hexadecimal base addresses.
    #[test]
    fn smmu_mode_case_insensitive() {
        unittest::assert_true!(super::test_smmu_mode_case_insensitive());
    }

    /// Tests whitespace trimming around commas, `=`, and mode tokens.
    #[test]
    fn smmu_mode_whitespace() {
        unittest::assert_true!(super::test_smmu_mode_whitespace());
    }

    /// Tests validation of valid and malformed SMMU mode strings.
    #[test]
    fn smmu_mode_validation() {
        unittest::assert_true!(super::test_smmu_mode_validation());
    }

    /// Tests VMSAv8-64 stage-1 address validation, descriptor encoding, and atomic PTE writes.
    #[test]
    fn vmsav8_64_descriptors() {
        unittest::assert_true!(super::test_vmsav8_64_descriptors());
    }

    /// Tests `Bitmask128` bit manipulation, boundary indices, and `Bitmask128Iter`.
    #[test]
    fn bitmask128() {
        unittest::assert_true!(super::test_bitmask128());
    }

    /// Tests `PageCache` allocation (`get_page`), zero-filling, `return_page`, and LIFO reuse.
    #[test]
    fn page_cache_add_and_get() {
        unittest::assert_true!(super::test_page_cache_add_and_get());
    }

    /// Tests `PageCache::trim` FIFO tail eviction and LIFO head retention.
    #[test]
    fn page_cache_trim() {
        unittest::assert_true!(super::test_page_cache_trim());
    }

    /// Tests `PageCache` OOM handling when the underlying PMM reaches its allocation limit.
    #[test]
    fn page_cache_oom() {
        unittest::assert_true!(super::test_page_cache_oom());
    }

    /// Tests `TranslationTableHelper` 4-level cursor walks and intermediate table reclamation.
    #[test]
    fn translation_table_helper_walk_and_reclaim() {
        unittest::assert_true!(super::test_translation_table_helper_walk_and_reclaim());
    }

    /// Tests `DeviceAspace` map and unmap across all permission flag combinations.
    #[test]
    fn device_aspace_map_and_unmap() {
        unittest::assert_true!(super::test_device_aspace_map_and_unmap());
    }

    /// Tests `DeviceAspace::map` at a caller-specified virtual address.
    #[test]
    fn device_aspace_map_at_specific_location() {
        unittest::assert_true!(super::test_device_aspace_map_at_specific_location());
    }

    /// Tests `DeviceAspace::map` straddling the 512 GiB L0[0] -> L0[1] boundary.
    #[test]
    fn device_aspace_map_straddling_table_levels() {
        unittest::assert_true!(super::test_device_aspace_map_straddling_table_levels());
    }

    /// Tests `DeviceAspace::map` OOM rollback and preservation of pre-existing sibling mappings.
    #[test]
    fn device_aspace_map_failure_injection() {
        unittest::assert_true!(super::test_device_aspace_map_failure_injection());
    }

    /// Tests reference-free reclamation of shared L0/L1/L2/L3 translation tables on reverse unmap.
    #[test]
    fn device_aspace_shared_table_levels() {
        unittest::assert_true!(super::test_device_aspace_shared_table_levels());
    }

    /// Tests alignment, overflow, bounds checks, overlap rejection, and adjacent region placement.
    #[test]
    fn device_aspace_map_at_location_edge_cases() {
        unittest::assert_true!(super::test_device_aspace_map_at_location_edge_cases());
    }

    /// Tests the non-MMIO `define_smmu_reg!` arm and `dump_bitfields` helper on a `u64` value.
    #[test]
    fn non_mmio_bitfields() {
        unittest::assert_true!(super::test_non_mmio_bitfields());
    }

    /// Tests `EscapedName` NUL termination, UTF-8 passthrough, and invalid-byte escaping.
    #[test]
    fn escaped_name() {
        unittest::assert_true!(super::test_escaped_name());
    }
}
