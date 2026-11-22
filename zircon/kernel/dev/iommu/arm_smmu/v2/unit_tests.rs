// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Native Rust unit tests for the ARM SMMUv2 driver components.
//!
//! Like `arm_smmu/unit_tests.rs`, this module is shared between the standalone host test binary
//! and the in-kernel `#[cfg(ktest)]` suite, so tests return `bool` rather than panicking.
//!
//! Covers:
//! 1. `SmrValue` stream-matching construction, intersection, containment, and iteration.
//! 2. `StreamMatchRegGroup` lockdown, adoption, context-bank binding, invalidation, and teardown.
//! 3. `ContextBank` lockdown, mode transitions, adoption (32-bit, Ext32Bit, 64-bit), fault IRQ
//!    toggling, and ASID/VA TLB invalidation.
//! 4. `define_smmu_reg!` 32-bit/64-bit/indexed register bitfields, `RSVDZ_MASK`, and metadata.
//! 5. `SmmuV2Bti` orphan and quarantine end-of-life transitions and warning emission.

use debug::dprintf;
#[cfg(ktest)]
use fbl::RefPtr;
use zx_status::Status;

use super::context_bank::{INVALIDATE_ALL_PAGE_COUNT_THRESHOLD, UNUSED_ASID};
use super::registers::{
    Cba2rVa64, CbarType, ClientPd, ExidEnable, FieldDesc, S2crPrivCfg, S2crType, SmmuMmio, Stalld,
    TcrIrg0, TcrOrg0, TcrSh0, TcrTg0, TcrTg1, gr0, gr1, s1cbr, s2cbr,
};
use super::{
    AddrMode, Bitmask128, BtiMode, ContextBank, SmmuController, SmmuParams, SmrValue,
    StreamMatchRegGroup, addr_mode_to_string, arm_cbar_type_to_string, arm_s2cr_type_to_string,
    arm_smmu_mode_to_string, bti_mode_to_string, smr_intersects,
};
#[cfg(ktest)]
use super::{SmmuV2, SmmuV2Bti};
use crate::arm_smmu::common::{
    ArmSmmuMode, DEFAULT_TCR_T0SZ, DeviceAspace, PAGE_SIZE, TlbInvalidator,
};
#[cfg(ktest)]
use crate::arm_smmu::common::{
    BtiPageLeakReason, IOMMU_FLAG_PERM_READ, IOMMU_FLAG_PERM_WRITE, RequireContiguousMapping,
    SmmuPinnedVmo, ZX_MAX_NAME_LEN,
};
#[cfg(ktest)]
use crate::iommu::{
    CapturedQuarantineWarning, start_capturing_quarantine_warnings_for_test,
    take_captured_quarantine_warning_for_test,
};
#[cfg(ktest)]
use crate::vm::pinned_vm_object::PinnedVmObject;
#[cfg(ktest)]
use crate::vm::vm_object::VmObject;
#[cfg(ktest)]
use crate::vm::vm_object_paged::VmObjectPaged;

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
// 1. `SmrValue` and `SmrValueIter` Unit Tests
// ============================================================================

/// Tests `SmrValue` construction, raw value preservation, boundary Stream IDs (`0x7fff` vs
/// `0x8000`), intersection, containment, stream counting, and `SmrValueIter` iteration.
pub fn test_smr_value_and_iter() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] SmrValueAndIter\n");

    // Default invalid sentinel (`0x7fff_ffff`): extracting mask/id.
    let def = SmrValue::default_invalid();
    check_eq!(def, SmrValue::default());
    check_eq!(def.value(), 0x7fff_ffff);
    check_eq!(def.id(), 0x7fff);
    check_eq!(def.mask(), 0x7fff);

    // The raw value is preserved: bits in `id` that are masked by `mask` are kept as given (so
    // `value()` round-trips a `bti_id` or `SMR` register), and are only cleared when computing the
    // first matched Stream ID / iterating.
    let raw = SmrValue::new(0x00ff, 0x000f);
    check_eq!(raw.id(), 0x00ff);
    check_eq!(raw.mask(), 0x000f);
    check_eq!(raw.num_streams(), 16);
    check_eq!(raw.value(), (0x000fu32 << 16) | 0x00ffu32);
    check_eq!(raw.first_stream_id(), 0x00f0);
    check_eq!(
        SmrValue::from_bti_id(0x000f_00ff, SmrValue::FULL_VALID_MASK).map(|v| v.value()),
        Ok(0x000f_00ff)
    );
    // Values which differ only in masked `id` bits match the same streams.
    check!(raw.intersects(SmrValue::new(0x00f0, 0x000f)));
    check!(raw.contains(0x00f0) && raw.contains(0x00ff) && !raw.contains(0x0100));
    let mut raw_count = 0u16;
    for sid in raw {
        check_eq!(sid, 0x00f0 + raw_count);
        raw_count += 1;
    }
    check_eq!(raw_count, 16);
    // The default sentinel wildcards every bit, and so matches every 15-bit Stream ID.
    check_eq!(def.first_stream_id(), 0);
    check_eq!(def.iter().count(), 0x8000);
    check_eq!(def.iter().last(), Some(0x7fff));

    // `from_bti_id` boundary checks against `FULL_VALID_MASK` (`0x7fff_7fff`) and narrower masks.
    check!(SmrValue::from_bti_id(0x7fff, SmrValue::FULL_VALID_MASK).is_ok());
    check_eq!(SmrValue::from_bti_id(0x8000, SmrValue::FULL_VALID_MASK), Err(Status::INVALID_ARGS));
    check_eq!(
        SmrValue::from_bti_id(0x8000_0000, SmrValue::FULL_VALID_MASK),
        Err(Status::INVALID_ARGS)
    );
    check_eq!(
        SmrValue::from_bti_id(1u64 << 32, SmrValue::FULL_VALID_MASK),
        Err(Status::INVALID_ARGS)
    );
    // Narrower hardware mask (e.g. 10-bit stream IDs `0x03ff_03ff`):
    let mask_10bit: u32 = 0x03ff_03ff;
    check!(SmrValue::from_bti_id(0x03ff, mask_10bit).is_ok());
    check_eq!(SmrValue::from_bti_id(0x0400, mask_10bit), Err(Status::INVALID_ARGS));

    // `from_smr_reg` construction (the `VALID` bit is dropped, masked `id` bits are preserved).
    let mut smr_reg = gr0::Smr::from_value(0);
    smr_reg.set_valid(1).set_id(0x0123).set_mask(0x0003);
    let from_reg = SmrValue::from_smr_reg(smr_reg, SmrValue::FULL_VALID_MASK);
    check_eq!(from_reg.id(), 0x0123);
    check_eq!(from_reg.first_stream_id(), 0x0120);
    check_eq!(from_reg.mask(), 0x0003);
    check_eq!(from_reg.num_streams(), 4);

    // Intersection and containment.
    let exact_10 = SmrValue::new(0x0010, 0x0000);
    let exact_11 = SmrValue::new(0x0011, 0x0000);
    let masked_10_13 = SmrValue::new(0x0010, 0x0003); // {0x10, 0x11, 0x12, 0x13}
    let masked_10_12 = SmrValue::new(0x0010, 0x0002); // {0x10, 0x12}
    let masked_10_11 = SmrValue::new(0x0010, 0x0001); // {0x10, 0x11}
    let masked_20_23 = SmrValue::new(0x0020, 0x0003); // {0x20, 0x21, 0x22, 0x23}

    check!(!exact_10.intersects(exact_11));
    check!(!smr_intersects(exact_10.value(), exact_11.value()));
    check!(exact_10.contains(0x0010));
    check!(!exact_10.contains(0x0011));

    check!(masked_10_13.intersects(exact_11));
    check!(exact_11.intersects(masked_10_13));
    check!(smr_intersects(masked_10_13.value(), exact_11.value()));
    check!(masked_10_13.contains(0x0010));
    check!(masked_10_13.contains(0x0011));
    check!(masked_10_13.contains(0x0012));
    check!(masked_10_13.contains(0x0013));
    check!(!masked_10_13.contains(0x0014));

    // Overlapping masks with non-subset relationship (`{0x10, 0x11}` vs `{0x10, 0x12}` intersect
    // at `0x10`).
    check!(masked_10_11.intersects(masked_10_12));
    check!(!masked_10_13.intersects(masked_20_23));

    // `SmrValueIter` over an exact Stream ID (`mask == 0`).
    let mut iter_exact = exact_10.iter();
    check_eq!(iter_exact.next(), Some(0x0010));
    check_eq!(iter_exact.next(), None);

    // `SmrValueIter` over a non-contiguous mask (`id = 0x0200, mask = 0x0105`, wildcard bits 0, 2,
    // and 8 -> 8 stream IDs in strictly ascending order).
    let non_contig = SmrValue::new(0x0205, 0x0105);
    check_eq!(non_contig.id(), 0x0205);
    check_eq!(non_contig.first_stream_id(), 0x0200);
    check_eq!(non_contig.num_streams(), 8);
    let expected_sids: [u16; 8] = [0x0200, 0x0201, 0x0204, 0x0205, 0x0300, 0x0301, 0x0304, 0x0305];
    let mut count = 0usize;
    for sid in non_contig {
        check!(count < expected_sids.len());
        check_eq!(sid, expected_sids[count]);
        check!(non_contig.contains(sid));
        count += 1;
    }
    check_eq!(count, expected_sids.len());

    // `SmrValueIter` at the upper 15-bit boundary (`0x7ffe..=0x7fff`) must terminate cleanly
    // without wrapping around.
    let top_pair = SmrValue::new(0x7ffe, 0x0001);
    let mut top_iter = top_pair.iter();
    check_eq!(top_iter.next(), Some(0x7ffe));
    check_eq!(top_iter.next(), Some(0x7fff));
    check_eq!(top_iter.next(), None);

    let max_single = SmrValue::new(0x7fff, 0x0000);
    let mut max_iter = max_single.iter();
    check_eq!(max_iter.next(), Some(0x7fff));
    check_eq!(max_iter.next(), None);

    true
}

// ============================================================================
// 2. `StreamMatchRegGroup` Unit Tests
// ============================================================================

/// Test guard that ensures `StreamMatchRegGroup::disable` runs even if a `check!` returns early.
struct TestSmrg(fbl::UniquePtr<StreamMatchRegGroup>);

impl core::ops::Deref for TestSmrg {
    type Target = StreamMatchRegGroup;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl core::ops::DerefMut for TestSmrg {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for TestSmrg {
    fn drop(&mut self) {
        if !self.0.gr0_base().is_null() {
            self.0.disable();
        }
    }
}

/// Tests `StreamMatchRegGroup` creation, lockdown, adoption (valid `Translation`/`Bypass`/`Fault`
/// vs. invalid `S2crType::Invalid` and out-of-range `cbndx`), `enable_for_context_bank`,
/// `invalidate`, `revalidate`, `disable`, and `disable_at`.
pub fn test_stream_match_reg_group() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] StreamMatchRegGroup\n");

    let Ok(mut fake_gr0) = fbl::Array::<u64>::try_new(512) else {
        return false;
    };
    // SAFETY: `fake_gr0` is a 4096-byte (512 * 8-byte) aligned heap buffer with exposed provenance
    // valid for the duration of this test, and writes to this RAM-backed test buffer cannot cause
    // DMA or hardware UB.
    let gr0_base = unsafe { SmmuMmio::with_size(fake_gr0.as_mut_ptr().expose_provenance(), 4096) };
    let mut avail_smrgs = Bitmask128::new();
    avail_smrgs.set_lowest_n_bits(4);
    avail_smrgs.clear_bit(2); // Mark SMRG #2 unavailable (`ALREADY_BOUND`).
    let mut avail_cbs = Bitmask128::new();
    avail_cbs.set_lowest_n_bits(4);

    let smmu = SmmuParams {
        name: "SMMU@smrg_test",
        op_mode: ArmSmmuMode::Enforced,
        num_smrgs: 4,
        num_cbs: 4,
        available_smrgs: &avail_smrgs,
        available_cbs: &avail_cbs,
        gr0_base,
        gr1_base: SmmuMmio::null(),
        cb0_base: SmmuMmio::null(),
        reg_page_size: 4096,
    };

    let sid = SmrValue::new(0x0123, 0x0003);
    // Index validation: `>= num_smrgs` -> `INVALID_ARGS`, unavailable bit -> `ALREADY_BOUND`.
    check_eq!(
        StreamMatchRegGroup::create_and_lockdown(&smmu, 4, sid).err(),
        Some(Status::INVALID_ARGS)
    );
    check_eq!(
        StreamMatchRegGroup::create_and_lockdown(&smmu, 2, sid).err(),
        Some(Status::ALREADY_BOUND)
    );

    // `create_and_lockdown` disables SMR[0] (`valid = 0, id = 0, mask = 0`) and sets S2CR[0] to
    // `S2crType::Fault`.
    gr0::Smr::from_value(0xffff_ffff).write_at(&gr0_base, 0);
    let Ok(smrg) = StreamMatchRegGroup::create_and_lockdown(&smmu, 0, sid) else {
        return false;
    };
    let mut smrg = TestSmrg(smrg);
    check_eq!(smrg.smrg_ndx(), 0);
    check_eq!(smrg.stream_ids(), sid);
    check_eq!(smrg.mode(), S2crType::Invalid);
    check_eq!(smrg.cb_ndx(), StreamMatchRegGroup::INVALID_CB_NDX);
    check_eq!(gr0::Smr::read_at(&gr0_base, 0).valid(), 0);
    check_eq!(gr0::S2cr::read_at(&gr0_base, 0).s2cr_type(), S2crType::Fault);

    // `enable_for_context_bank(1)` programs S2CR[0] (`Translation`, `cbndx = 1`, `privcfg = 2`)
    // and SMR[0] (`valid = 1`, `id = 0x0123`, written exactly as given, `mask = 0x0003`).
    smrg.enable_for_context_bank(1);
    check_eq!(smrg.mode(), S2crType::Translation);
    check_eq!(smrg.cb_ndx(), 1);
    let s2cr0 = gr0::S2crType0::from_value(gr0::S2cr::read_at(&gr0_base, 0).reg_value());
    check_eq!(s2cr0.s2cr_type(), S2crType::Translation);
    check_eq!(s2cr0.cbndx(), 1);
    check_eq!(s2cr0.privcfg(), S2crPrivCfg::Unprivileged);
    let smr0 = gr0::Smr::read_at(&gr0_base, 0);
    check_eq!(smr0.valid(), 1);
    check_eq!(smr0.id(), 0x0123);
    check_eq!(smr0.mask(), 0x0003);

    // `invalidate` and `revalidate` toggle `SMR.VALID` while preserving `ID` and `MASK`.
    smrg.invalidate();
    check_eq!(gr0::Smr::read_at(&gr0_base, 0).valid(), 0);
    check_eq!(gr0::Smr::read_at(&gr0_base, 0).id(), 0x0123);
    smrg.revalidate();
    check_eq!(gr0::Smr::read_at(&gr0_base, 0).valid(), 1);

    // `disable` resets registers and internal fields so `Drop` succeeds cleanly.
    smrg.disable();
    check_eq!(smrg.mode(), S2crType::Invalid);
    check_eq!(smrg.cb_ndx(), StreamMatchRegGroup::INVALID_CB_NDX);
    check!(smrg.gr0_base().is_null());
    drop(smrg);

    // `create_and_adopt` tests:
    // 1. Adopting `S2crType::Invalid` -> `Err(Status::BAD_STATE)` with clean Drop.
    gr0::S2cr::from_value(0).set_s2cr_type(S2crType::Invalid).write_at(&gr0_base, 1);
    check_eq!(StreamMatchRegGroup::create_and_adopt(&smmu, 1, sid).err(), Some(Status::BAD_STATE));

    // 2. Adopting `S2crType::Translation` with out-of-range `cbndx = 7 >= num_cbs` ->
    //    `Err(Status::OUT_OF_RANGE)` with clean Drop.
    gr0::S2crType0::from_value(0)
        .set_s2cr_type(S2crType::Translation)
        .set_cbndx(7)
        .write_at(&gr0_base, 1);
    check_eq!(
        StreamMatchRegGroup::create_and_adopt(&smmu, 1, sid).err(),
        Some(Status::OUT_OF_RANGE)
    );

    // 3. Adopting valid `S2crType::Translation` (`cbndx = 2`) and `S2crType::Bypass`.
    gr0::S2crType0::from_value(0)
        .set_s2cr_type(S2crType::Translation)
        .set_cbndx(2)
        .write_at(&gr0_base, 1);
    let Ok(adopted_trans) = StreamMatchRegGroup::create_and_adopt(&smmu, 1, sid) else {
        return false;
    };
    let mut adopted_trans = TestSmrg(adopted_trans);
    check_eq!(adopted_trans.mode(), S2crType::Translation);
    check_eq!(adopted_trans.cb_ndx(), 2);
    adopted_trans.disable();
    drop(adopted_trans);

    gr0::S2cr::from_value(0).set_s2cr_type(S2crType::Bypass).write_at(&gr0_base, 3);
    let Ok(adopted_bypass) = StreamMatchRegGroup::create_and_adopt(&smmu, 3, sid) else {
        return false;
    };
    let mut adopted_bypass = TestSmrg(adopted_bypass);
    check_eq!(adopted_bypass.mode(), S2crType::Bypass);
    check_eq!(adopted_bypass.cb_ndx(), StreamMatchRegGroup::INVALID_CB_NDX);
    adopted_bypass.disable();
    drop(adopted_bypass);

    // `disable_at` static helper.
    gr0::Smr::from_value(0).set_valid(1).set_id(0x55).write_at(&gr0_base, 3);
    StreamMatchRegGroup::disable_at(&smmu, 3);
    check_eq!(gr0::Smr::read_at(&gr0_base, 3).valid(), 0);
    check_eq!(gr0::S2cr::read_at(&gr0_base, 3).s2cr_type(), S2crType::Fault);

    true
}

// ============================================================================
// 3. `ContextBank` Unit Tests
// ============================================================================

/// Test guard that ensures `ContextBank` transitions to `BtiMode::Shutdown` even if a `check!`
/// returns early.
struct TestCb<'a>(fbl::UniquePtr<ContextBank>, &'a str);

impl core::ops::Deref for TestCb<'_> {
    type Target = ContextBank;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl core::ops::DerefMut for TestCb<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for TestCb<'_> {
    fn drop(&mut self) {
        if self.0.mode() != BtiMode::Shutdown {
            if self.0.mode() == BtiMode::Adopted {
                self.0.force_mode_for_test(BtiMode::Fault);
            }
            self.0.set_mode(self.1, BtiMode::Shutdown);
        }
    }
}

/// No-op `TlbInvalidator` used when tearing down a `DeviceAspace` on early test failure.
struct NoopInvalidator;

impl TlbInvalidator for NoopInvalidator {
    fn invalidate(&mut self, _vaddr: u64, _size: u64) {}
}

/// Test guard that ensures `DeviceAspace::free_translation_tables` runs if a `check!` returns
/// early.
struct TestCbAspace(kalloc::Box<DeviceAspace>);

impl core::ops::Deref for TestCbAspace {
    type Target = DeviceAspace;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl core::ops::DerefMut for TestCbAspace {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for TestCbAspace {
    fn drop(&mut self) {
        if self.0.page_cache().in_flight_pages() != 0 || self.0.page_cache().cache_entries() != 0 {
            let mut inval = NoopInvalidator;
            self.0.free_translation_tables(&mut inval);
        }
    }
}

/// Tests `ContextBank` lockdown, mode transitions (`Fault`, `Bypass`, `Translation`, `Shutdown`),
/// fault interrupt toggling, ASID and region TLB invalidations
/// (`INVALIDATE_ALL_PAGE_COUNT_THRESHOLD`), and hardware state adoption across 64-bit, Ext32Bit,
/// and 32-bit addressing modes.
pub fn test_context_bank() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] ContextBank\n");

    let Ok(mut fake_regs) = fbl::Array::<u64>::try_new(512 * 6) else {
        return false;
    };
    let base_vaddr = fake_regs.as_mut_ptr().expose_provenance();
    // SAFETY: `fake_regs` is a 24 KiB (512 * 6 * 8-byte) aligned heap buffer with exposed
    // provenance valid for the duration of this test; `gr0_base` and `gr1_base` are 4 KiB each,
    // `cb0_base` covers the remaining 4 * 4 KiB context bank pages (`0..num_cbs`), and writes to
    // this RAM-backed test buffer cannot cause DMA or hardware UB.
    let gr0_base = unsafe { SmmuMmio::with_size(base_vaddr, 4096) };
    let gr1_base = unsafe { SmmuMmio::with_size(base_vaddr + 4096, 4096) };
    let cb0_base = unsafe { SmmuMmio::with_size(base_vaddr + 8192, 4 * 4096) };

    let mut avail_smrgs = Bitmask128::new();
    avail_smrgs.set_lowest_n_bits(4);
    let mut avail_cbs = Bitmask128::new();
    avail_cbs.set_lowest_n_bits(4);
    avail_cbs.clear_bit(3); // Mark CB #3 unavailable.

    let smmu = SmmuParams {
        name: "SMMU@cb_test",
        op_mode: ArmSmmuMode::Disabled,
        num_smrgs: 4,
        num_cbs: 4,
        available_smrgs: &avail_smrgs,
        available_cbs: &avail_cbs,
        gr0_base,
        gr1_base,
        cb0_base,
        reg_page_size: 4096,
    };

    // Index validation: `>= num_cbs` -> `INVALID_ARGS`, unavailable bit -> `ALREADY_BOUND`.
    check_eq!(ContextBank::create_and_lockdown(&smmu, 4).err(), Some(Status::INVALID_ARGS));
    check_eq!(ContextBank::create_and_lockdown(&smmu, 3).err(), Some(Status::ALREADY_BOUND));

    let Ok(cb) = ContextBank::create_and_lockdown(&smmu, 1) else {
        return false;
    };
    let mut cb = TestCb(cb, smmu.name);
    let cb1_base = smmu.get_cb_base(1);
    check_eq!(cb.cb_ndx(), 1);
    check_eq!(cb.asid(), ContextBank::UPPER_ASID_BITS | 1);
    check_eq!(s1cbr::Ttbr0_64Bit::read_from(&cb1_base).asid(), UNUSED_ASID as u64);
    check_eq!(s1cbr::Ttbr1_64Bit::read_from(&cb1_base).asid(), UNUSED_ASID as u64);

    // Transition to `BtiMode::Fault`, then `BtiMode::Bypass`, then back to `BtiMode::Fault`.
    cb.set_mode(smmu.name, BtiMode::Fault);
    check_eq!(cb.mode(), BtiMode::Fault);
    check_eq!(cb.addr_mode(), AddrMode::Mode64Bit);
    check_eq!(cb.aspace_size(), 0);
    check_eq!(s1cbr::Sctlr::read_from(&cb1_base).m(), 1);
    check_eq!(s1cbr::Tcr64Bit::read_from(&cb1_base).epd0(), 1);
    check_eq!(s1cbr::Tcr64Bit::read_from(&cb1_base).epd1(), 1);

    cb.set_mode(smmu.name, BtiMode::Bypass);
    check_eq!(cb.mode(), BtiMode::Bypass);
    check_eq!(cb.aspace_size(), 1u64 << 48);
    check_eq!(gr1::Cbar::read_at(&gr1_base, 1).cbar_type(), CbarType::S1TS2Bypass);
    check_eq!(s1cbr::Sctlr::read_from(&cb1_base).m(), 0);

    cb.set_mode(smmu.name, BtiMode::Fault);
    check_eq!(cb.mode(), BtiMode::Fault);

    // Attach a `DeviceAspace` and transition to `BtiMode::Translation`.
    let Ok(aspace) = DeviceAspace::create(0x1000, (1u64 << 48) - 0x1000, 0) else {
        return false;
    };
    let mut aspace = TestCbAspace(aspace);
    cb.attach_aspace(&aspace);
    cb.set_mode(smmu.name, BtiMode::Translation);
    check_eq!(cb.mode(), BtiMode::Translation);
    check_eq!(cb.aspace_size(), 1u64 << (64 - DEFAULT_TCR_T0SZ));
    check!(cb.ttbrs()[0].enabled);
    check_eq!(cb.ttbrs()[0].ttbr_paddr, aspace.root_tt_paddr());
    check_eq!(s1cbr::Ttbr0_64Bit::read_from(&cb1_base).base_addr_value(), aspace.root_tt_paddr());
    check_eq!(s1cbr::Ttbr0_64Bit::read_from(&cb1_base).asid(), cb.asid() as u64);
    check_eq!(s1cbr::Tcr64Bit::read_from(&cb1_base).epd0(), 0);
    check_eq!(s1cbr::Tcr64Bit::read_from(&cb1_base).epd1(), 1);
    check_eq!(s1cbr::Sctlr::read_from(&cb1_base).m(), 1);

    // Fault interrupt toggling (`SCTLR.CFIE`).
    cb.enable_fault_interrupt();
    check_eq!(s1cbr::Sctlr::read_from(&cb1_base).cfie(), 1);
    cb.disable_fault_interrupt();
    check_eq!(s1cbr::Sctlr::read_from(&cb1_base).cfie(), 0);

    // TLB invalidation by region (below threshold `<= 256` pages uses `TLBIVA`; above threshold
    // `> 256` pages falls back to `TLBIASID`).
    s1cbr::Tlbiasid::from_value(0).write_to(&cb1_base);
    s1cbr::TlbivaAArch64::from_value(0).write_to(&cb1_base);
    let mut inval_op = cb.make_inval_op();
    inval_op.invalidate(0x1234_5000, 2 * PAGE_SIZE);
    let tlbiva = s1cbr::TlbivaAArch64::read_from(&cb1_base);
    check_eq!(tlbiva.asid(), cb.asid() as u64);
    check_eq!(tlbiva.address(), 0x1234_6000);
    check_eq!(s1cbr::Tlbiasid::read_from(&cb1_base).asid(), 0);

    // Region larger than `INVALIDATE_ALL_PAGE_COUNT_THRESHOLD` (257 pages) must issue `TLBIASID`.
    inval_op.invalidate(0x1000_0000, (INVALIDATE_ALL_PAGE_COUNT_THRESHOLD + 1) * PAGE_SIZE);
    check_eq!(s1cbr::Tlbiasid::read_from(&cb1_base).asid(), cb.asid() as u32);

    // Zero-size invalidation via `ContextBankInvalidator` also issues `TLBIASID`.
    s1cbr::Tlbiasid::from_value(0).write_to(&cb1_base);
    aspace.free_translation_tables(&mut inval_op);
    check_eq!(s1cbr::Tlbiasid::read_from(&cb1_base).asid(), cb.asid() as u32);

    // Shut down `cb` cleanly.
    cb.set_mode(smmu.name, BtiMode::Shutdown);
    check_eq!(cb.mode(), BtiMode::Shutdown);
    check!(cb.gr1_base().is_null());
    check!(cb.cb_base().is_null());
    drop(cb);

    // Test `ContextBank::create_and_adopt` across:
    // (a) Stage-1 64-bit translation (`VA64 = 1`, `SCTLR.M = 1`, `TCR.T0SZ = 16`, `EPD0 = 0`,
    //     `EPD1 = 1`).
    let cb2_base = smmu.get_cb_base(2);
    gr1::Cbar::from_value(0).set_cbar_type(CbarType::S1TS2Bypass).write_at(&gr1_base, 2);
    gr1::Cba2r::from_value(0).set_va64(Cba2rVa64::Va64).write_at(&gr1_base, 2);
    s1cbr::Tcr64Bit::from_value(0)
        .set_t0sz(16)
        .set_t1sz(24)
        .set_tg0(TcrTg0::Granule4KB)
        .set_epd0(0)
        .set_epd1(1)
        .write_to(&cb2_base);
    s1cbr::Ttbr0_64Bit::from_value(0).set_base_addr_from_value(0xabcd_0000).write_to(&cb2_base);
    s1cbr::Sctlr::from_value(0).set_m(1).write_to(&cb2_base);

    let Ok(adopted_64) = ContextBank::create_and_adopt(&smmu, 2) else {
        return false;
    };
    let mut adopted_64 = TestCb(adopted_64, smmu.name);
    check_eq!(adopted_64.mode(), BtiMode::Adopted);
    check_eq!(adopted_64.addr_mode(), AddrMode::Mode64Bit);
    check!(adopted_64.ttbrs()[0].enabled);
    check!(!adopted_64.ttbrs()[1].enabled);
    check_eq!(adopted_64.ttbrs()[0].granule_size_bits, 12);
    check_eq!(adopted_64.ttbrs()[0].first_valid_addr, 0);
    check_eq!(adopted_64.ttbrs()[0].last_valid_addr, (1u64 << 48) - 1);
    check_eq!(adopted_64.ttbrs()[0].ttbr_paddr, 0xabcd_0000);
    adopted_64.force_mode_for_test(BtiMode::Fault);
    adopted_64.set_mode(smmu.name, BtiMode::Shutdown);
    drop(adopted_64);

    // (b) Stage-1 Ext32Bit translation (`VA64 = 0`, `TCR.EAE = 1`, `T0SZ = 2`, `T1SZ = 0`).
    gr1::Cba2r::from_value(0).set_va64(Cba2rVa64::Va32).write_at(&gr1_base, 2);
    s1cbr::TcrExt32Bit::from_value(0)
        .set_eae(1)
        .set_t0sz(2)
        .set_t1sz(0)
        .set_epd0(0)
        .set_epd1(1)
        .write_to(&cb2_base);
    s1cbr::Ttbr0Ext32Bit::from_value(0).set_base_addr_from_value(0x1234_0000).write_to(&cb2_base);
    s1cbr::Sctlr::from_value(0).set_m(1).write_to(&cb2_base);

    let Ok(adopted_ext32) = ContextBank::create_and_adopt(&smmu, 2) else {
        return false;
    };
    let mut adopted_ext32 = TestCb(adopted_ext32, smmu.name);
    check_eq!(adopted_ext32.mode(), BtiMode::Adopted);
    check_eq!(adopted_ext32.addr_mode(), AddrMode::Ext32Bit);
    check_eq!(adopted_ext32.ttbrs()[0].last_valid_addr, (1u64 << 30) - 1);
    check_eq!(adopted_ext32.ttbrs()[0].ttbr_paddr, 0x1234_0000);
    adopted_ext32.force_mode_for_test(BtiMode::Fault);
    adopted_ext32.set_mode(smmu.name, BtiMode::Shutdown);
    drop(adopted_ext32);

    // (c) Stage-1 32-bit short-descriptor translation (`VA64 = 0`, `TCR.EAE = 0`, `T0SZ = 1`).
    gr1::Cba2r::from_value(0).set_va64(Cba2rVa64::Va32).write_at(&gr1_base, 2);
    s1cbr::Tcr32Bit::from_value(0).set_eae(0).set_t0sz(1).set_pd0(0).set_pd1(1).write_to(&cb2_base);
    s1cbr::Ttbr0_32Bit::from_value(0).set_base_addr_from_value(0x5678_0000).write_to(&cb2_base);
    s1cbr::Sctlr::from_value(0).set_m(1).write_to(&cb2_base);

    let Ok(adopted_32) = ContextBank::create_and_adopt(&smmu, 2) else {
        return false;
    };
    let mut adopted_32 = TestCb(adopted_32, smmu.name);
    check_eq!(adopted_32.mode(), BtiMode::Adopted);
    check_eq!(adopted_32.addr_mode(), AddrMode::Mode32Bit);
    check_eq!(adopted_32.ttbrs()[0].last_valid_addr, (1u64 << 31) - 1);
    check_eq!(adopted_32.ttbrs()[1].first_valid_addr, 1u64 << 31);
    check_eq!(adopted_32.ttbrs()[1].last_valid_addr, 0xFFFF_FFFF);
    check_eq!(adopted_32.ttbrs()[0].ttbr_paddr, 0x5678_0000);
    adopted_32.force_mode_for_test(BtiMode::Fault);
    adopted_32.set_mode(smmu.name, BtiMode::Shutdown);
    drop(adopted_32);

    // `ContextBank::disable_at` static helper.
    ContextBank::disable_at(&smmu, 0);
    check_eq!(s1cbr::Ttbr0_64Bit::read_from(&cb0_base).asid(), UNUSED_ASID as u64);

    true
}

// ============================================================================
// 4. `define_smmu_reg!` Formatting & Bitfields Unit Tests
// ============================================================================

/// Tests `define_smmu_reg!` 32-bit, 64-bit, and indexed register bitfields, `RSVDZ_MASK` write
/// masking, enum conversions, `FieldDesc::extract`, and string formatting helpers.
pub fn test_registers_and_bitfields() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] RegistersAndBitfields\n");

    let Ok(mut fake_mmio) = fbl::Array::<u64>::try_new(512) else {
        return false;
    };
    // SAFETY: `fake_mmio` is a 4096-byte (512 * 8-byte) aligned heap buffer with exposed
    // provenance valid for the duration of this test, and writes to this RAM-backed test buffer
    // cannot cause DMA or hardware UB.
    let mmio = unsafe { SmmuMmio::with_size(fake_mmio.as_mut_ptr().expose_provenance(), 4096) };

    // 1. 32-bit fixed-offset register (`gr0::Cr0`) round-trip and `RSVDZ_MASK` stripping on write.
    let mut cr0 = gr0::Cr0::from_value(u32::MAX);
    check_eq!(cr0.clientpd(), ClientPd::Disabled);
    check_eq!(cr0.stalld(), Stalld::Disabled);
    check_eq!(cr0.exidenable(), ExidEnable::Enabled);
    cr0.write_to(&mmio);
    let cr0_readback = gr0::Cr0::read_from(&mmio);
    check_eq!(cr0_readback.reg_value() & gr0::Cr0::RSVDZ_MASK, 0);
    check_eq!(cr0_readback.reg_value(), !gr0::Cr0::RSVDZ_MASK);

    cr0.set_clientpd(ClientPd::Enabled)
        .set_stalld(Stalld::Enabled)
        .set_exidenable(ExidEnable::Disabled)
        .set_usfcfg(1)
        .set_gfre(1)
        .set_gfie(1)
        .write_to(&mmio);
    let cr0_mod = gr0::Cr0::read_from(&mmio);
    check_eq!(cr0_mod.clientpd(), ClientPd::Enabled);
    check_eq!(cr0_mod.stalld(), Stalld::Enabled);
    check_eq!(cr0_mod.exidenable(), ExidEnable::Disabled);
    check_eq!(cr0_mod.usfcfg(), 1);
    check_eq!(cr0_mod.gfre(), 1);
    check_eq!(cr0_mod.gfie(), 1);

    // 2. Bitfields in `gr0::Idr0`.
    let mut idr0 = gr0::Idr0::from_value(0);
    idr0.set_s1ts(1)
        .set_s2ts(0)
        .set_nts(1)
        .set_sms(1)
        .set_cttw(1)
        .set_numirpt(16)
        .set_numsmrg(64)
        .write_to(&mmio);
    let idr0_rb = gr0::Idr0::read_from(&mmio);
    check_eq!(idr0_rb.s1ts(), 1);
    check_eq!(idr0_rb.s2ts(), 0);
    check_eq!(idr0_rb.nts(), 1);
    check_eq!(idr0_rb.sms(), 1);
    check_eq!(idr0_rb.cttw(), 1);
    check_eq!(idr0_rb.numirpt(), 16);
    check_eq!(idr0_rb.numsmrg(), 64);
    check_eq!(idr0_rb.reg_value() & gr0::Idr0::RSVDZ_MASK, 0);

    // 3. 64-bit register (`s1cbr::Ttbr0_64Bit`, `s2cbr::Ttbr0`, `s1cbr::Far`) round-trip and
    //    `RSVDZ_MASK` enforcement.
    let mut ttbr0 = s1cbr::Ttbr0_64Bit::from_value(u64::MAX);
    ttbr0.write_to(&mmio);
    let ttbr0_rb = s1cbr::Ttbr0_64Bit::read_from(&mmio);
    check_eq!(ttbr0_rb.reg_value() & s1cbr::Ttbr0_64Bit::RSVDZ_MASK, 0);

    let test_pa: u64 = 0x0000_DEAD_BEEF_F000;
    s1cbr::Ttbr0_64Bit::from_value(0)
        .set_asid(0x5A03)
        .set_base_addr_from_value(test_pa)
        .write_to(&mmio);
    let ttbr0_pa = s1cbr::Ttbr0_64Bit::read_from(&mmio);
    check_eq!(ttbr0_pa.asid(), 0x5A03);
    check_eq!(ttbr0_pa.base_addr_value(), test_pa);

    s2cbr::Ttbr0::from_value(u64::MAX).write_to(&mmio);
    check_eq!(s2cbr::Ttbr0::read_from(&mmio).reg_value() & s2cbr::Ttbr0::RSVDZ_MASK, 0);
    s2cbr::Ttbr0::from_value(0).set_base_addr_from_value(test_pa).write_to(&mmio);
    check_eq!(s2cbr::Ttbr0::read_from(&mmio).base_addr_value(), test_pa);

    s1cbr::Far::from_value(0).set_faddr(0xCAFE_BABE_1234_5678).write_to(&mmio);
    check_eq!(s1cbr::Far::read_from(&mmio).faddr(), 0xCAFE_BABE_1234_5678);

    // 4. Indexed register arrays (`gr0::Smr`, `gr0::S2cr`, `gr1::Cbar`, `gr1::Cba2r`,
    //    `gr1::Cbfrsynra`) across indices `0..4`.
    for ndx in 0..4u32 {
        check_eq!(gr0::Smr::offset(ndx), 0x800 + (ndx << 2));
        check_eq!(gr0::S2cr::offset(ndx), 0xc00 + (ndx << 2));
        check_eq!(gr1::Cbar::offset(ndx), ndx << 2);
        check_eq!(gr1::Cbfrsynra::offset(ndx), 0x400 + (ndx << 2));
        check_eq!(gr1::Cba2r::offset(ndx), 0x800 + (ndx << 2));

        gr0::Smr::from_value(u32::MAX).write_at(&mmio, ndx);
        check_eq!(gr0::Smr::read_at(&mmio, ndx).reg_value() & gr0::Smr::RSVDZ_MASK, 0);

        gr1::Cba2r::from_value(u32::MAX).write_at(&mmio, ndx);
        check_eq!(gr1::Cba2r::read_at(&mmio, ndx).reg_value() & gr1::Cba2r::RSVDZ_MASK, 0);
    }

    // 5. Enum `from_raw` and string helper coverage.
    check_eq!(S2crType::from_raw(0), S2crType::Translation);
    check_eq!(S2crType::from_raw(1), S2crType::Bypass);
    check_eq!(S2crType::from_raw(2), S2crType::Fault);
    check_eq!(S2crType::from_raw(3), S2crType::Invalid);

    check_eq!(S2crPrivCfg::from_raw(0), S2crPrivCfg::Default);
    check_eq!(S2crPrivCfg::from_raw(1), S2crPrivCfg::Reserved1);
    check_eq!(S2crPrivCfg::from_raw(2), S2crPrivCfg::Unprivileged);
    check_eq!(S2crPrivCfg::from_raw(3), S2crPrivCfg::Privileged);

    check_eq!(CbarType::from_raw(0), CbarType::S2Translation);
    check_eq!(CbarType::from_raw(1), CbarType::S1TS2Bypass);
    check_eq!(CbarType::from_raw(2), CbarType::S1TS2Fault);
    check_eq!(CbarType::from_raw(3), CbarType::S1TS2Translate);

    check_eq!(Cba2rVa64::from_raw(0), Cba2rVa64::Va32);
    check_eq!(Cba2rVa64::from_raw(1), Cba2rVa64::Va64);
    check_eq!(TcrTg0::from_raw(0), TcrTg0::Granule4KB);
    check_eq!(TcrTg0::from_raw(1), TcrTg0::Granule64KB);
    check_eq!(TcrTg0::from_raw(2), TcrTg0::Granule16KB);
    check_eq!(TcrTg0::from_raw(3), TcrTg0::Reserved);
    check_eq!(TcrTg1::from_raw(0), TcrTg1::Reserved0);
    check_eq!(TcrTg1::from_raw(1), TcrTg1::Granule16KB);
    check_eq!(TcrTg1::from_raw(2), TcrTg1::Granule4KB);
    check_eq!(TcrTg1::from_raw(3), TcrTg1::Granule64KB);
    check_eq!(TcrSh0::from_raw(3), TcrSh0::InnerShareable);
    check_eq!(TcrOrg0::from_raw(1), TcrOrg0::WriteBackWriteAllocate);
    check_eq!(TcrIrg0::from_raw(1), TcrIrg0::WriteBackWriteAllocate);

    check_eq!(arm_smmu_mode_to_string(ArmSmmuMode::Disabled), "Disabled");
    check_eq!(arm_smmu_mode_to_string(ArmSmmuMode::Passthru), "Passthru");
    check_eq!(arm_smmu_mode_to_string(ArmSmmuMode::Enforced), "Enforced");
    check_eq!(arm_s2cr_type_to_string(S2crType::Translation), "Translation");
    check_eq!(
        arm_cbar_type_to_string(CbarType::S1TS2Bypass),
        "Translate: Stage 1 Context with Stage 2 Bypass"
    );
    check_eq!(addr_mode_to_string(AddrMode::Mode64Bit), "64Bit");
    check_eq!(bti_mode_to_string(BtiMode::Translation), "Translation");

    // 6. `FieldDesc::extract` and generated `FIELDS` metadata tables.
    let fd_single = FieldDesc { name: "BIT5", msb: 5, lsb: 5 };
    check_eq!(fd_single.extract(1u64 << 5), 1);
    check_eq!(fd_single.extract(!(1u64 << 5)), 0);

    let fd_full = FieldDesc { name: "ALL64", msb: 63, lsb: 0 };
    check_eq!(fd_full.extract(0xDEAD_BEEF_CAFE_BABE), 0xDEAD_BEEF_CAFE_BABE);

    check!(!gr0::Cr0::FIELDS.is_empty());
    check!(!gr0::Cr2::FIELDS.is_empty());
    check!(!gr0::Smr::FIELDS.is_empty());
    check!(!s1cbr::Ttbr0_64Bit::FIELDS.is_empty());
    // Verify that `[rsvdz]` fields in `gr0::Cr2::FIELDS` have `name == ""` while named fields in
    // `gr0::Smr::FIELDS` have non-empty names.
    let has_rsvdz_field = gr0::Cr2::FIELDS.iter().any(|f| f.name.is_empty());
    let has_valid_field =
        gr0::Smr::FIELDS.iter().any(|f| f.name == "VALID" && f.msb == 31 && f.lsb == 31);
    check!(has_rsvdz_field);
    check!(has_valid_field);

    // Exercise `dump_reg` formatting for both 32-bit (`Cr0`) and 64-bit (`Ttbr0_64Bit`) registers.
    super::console::dump_reg(
        "GR0.CR0",
        cr0_mod.reg_value() as u64,
        4,
        gr0::Cr0::OFFSET,
        gr0::Cr0::FIELDS,
        &mmio,
        1,
    );
    super::console::dump_reg(
        "CB0.TTBR0",
        ttbr0_pa.reg_value(),
        8,
        s1cbr::Ttbr0_64Bit::OFFSET,
        s1cbr::Ttbr0_64Bit::FIELDS,
        &mmio,
        1,
    );

    true
}

// ============================================================================
// 5. `SmmuV2Bti` Orphan and Quarantine End-of-Life Unit Tests
// ============================================================================

/// RAII wrapper around an unregistered unit-test [`SmmuV2`] that dismantles any remaining orphaned
/// BTIs on drop (including on early [`check!`] failure).
#[cfg(ktest)]
struct TestUnitSmmu(Option<RefPtr<SmmuV2>>);

#[cfg(ktest)]
impl core::ops::Deref for TestUnitSmmu {
    type Target = SmmuV2;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("TestUnitSmmu must hold an SmmuV2 while live")
    }
}

#[cfg(ktest)]
impl Drop for TestUnitSmmu {
    fn drop(&mut self) {
        if let Some(smmu) = self.0.take() {
            SmmuV2::shutdown_for_unit_test(smmu);
        }
    }
}

/// Allocates and pins a `pages`-page paged VMO for BTI mapping unit tests.
#[cfg(ktest)]
fn pin_test_vmo(pages: u64) -> Option<SmmuPinnedVmo> {
    let size = pages * PAGE_SIZE;
    let vmo_paged = VmObjectPaged::create(0, 0, size).ok()?;
    let vmo: RefPtr<VmObject> = RefPtr::from_ref(&vmo_paged);
    PinnedVmObject::create(vmo, 0, size, true).ok().map(SmmuPinnedVmo::from_kernel_pinned_vmo)
}

/// Builds an expected [`CapturedQuarantineWarning`] with `name` NUL-padded to `ZX_MAX_NAME_LEN`.
#[cfg(ktest)]
fn make_expected_warning(
    bti_id: u64,
    name: &[u8],
    reason: BtiPageLeakReason,
    pages: u64,
    vmos: usize,
) -> CapturedQuarantineWarning {
    let mut bti_name = [0u8; ZX_MAX_NAME_LEN];
    bti_name[..name.len()].copy_from_slice(name);
    CapturedQuarantineWarning { bti_id, bti_name, reason, pages, vmos }
}

/// Verifies that no quarantine warnings have been emitted since the previous check.
#[cfg(ktest)]
fn check_no_quarantine_warnings() -> bool {
    check_eq!(take_captured_quarantine_warning_for_test(), None);
    true
}

/// Verifies that exactly one quarantine warning matching `(bti_id, name, reason, pages, vmos)` was
/// emitted since the previous check.
#[cfg(ktest)]
fn check_single_quarantine_warning(
    bti_id: u64,
    name: &[u8],
    reason: BtiPageLeakReason,
    pages: u64,
    vmos: usize,
) -> bool {
    check_eq!(
        take_captured_quarantine_warning_for_test(),
        Some(make_expected_warning(bti_id, name, reason, pages, vmos))
    );
    check_eq!(take_captured_quarantine_warning_for_test(), None);
    true
}

/// Verifies post-end-of-life state for a BTI with unreleased quarantine: `Enforced` shuts down and
/// frees `sid` for reuse, whereas `Passthru` remains orphaned in `Fault` mode blocking `sid`.
#[cfg(ktest)]
fn check_quarantined_bti_eol(smmu: &SmmuV2, bti: &SmmuV2Bti, sid: u64, enforced: bool) -> bool {
    if enforced {
        check!(!bti.in_container());
        let Ok(bti_again) = smmu.create_bti(sid) else {
            return false;
        };
        bti_again.on_dispatcher_zero_handles();
        drop(bti_again);
    } else {
        check!(bti.in_container());
        check!(bti.in_fault_state());
        let cb_base = smmu.get_cb_base(bti.cb_ndx());
        check_eq!(s1cbr::Sctlr::read_from(&cb_base).m(), 1);
        check_eq!(s1cbr::Tcr64Bit::read_from(&cb_base).epd0(), 1);
        check_eq!(smmu.create_bti(sid).err(), Some(Status::ALREADY_EXISTS));
    }
    true
}

/// Tests `SmmuV2Bti` end-of-life, orphaning, and quarantine warning emission across both `Enforced`
/// and `Passthru` modes:
/// 1. Closing a BTI with only pinned PMTs emits no warning and frees the stream once unpinned.
/// 2. Leaking a PMT and then closing the BTI logs `BtiOrphanedWithQuarantinedPmts` in both modes,
///    shuts down cleanly in `Enforced` mode without tripping `SmmuV2Bti::shutdown` assertions, and
///    keeps the `Passthru` BTI alive in `Fault` mode blocking `create_bti` (`ALREADY_EXISTS`).
/// 3. An orphaned BTI with one leaked PMT and one cleanly unpinned PMT (in either order) counts
///    earlier quarantine totals when the final PMT closes: `Enforced` shuts down cleanly, whereas
///    `Passthru` remains orphaned in `Fault` mode.
/// 4. An orphaned BTI whose last two PMTs are both unpinned before either reaches zero handles
///    calls `on_end_of_life` once rather than panicking on the second PMT close.
/// 5. Closing a BTI with both a quarantined PMT and an active PMT logs
///    `BtiOrphanedWithQuarantinedPmts` immediately while remaining orphaned until the active PMT
///    closes.
/// 6. Unpinning a PMT before BTI close and closing the PMT handle after BTI end-of-life handles
///    `op_mode == None` cleanly with zero warnings.
#[cfg(ktest)]
pub fn test_smmu_bti_orphan_and_quarantine_end_of_life() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] SmmuBtiOrphanAndQuarantineEndOfLife\n");

    const BASE_SID: u64 = 0x0061;
    let perms = IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE;

    let _capture_guard = start_capturing_quarantine_warnings_for_test();

    for op_mode in [ArmSmmuMode::Enforced, ArmSmmuMode::Passthru] {
        let enforced = op_mode == ArmSmmuMode::Enforced;
        let Ok(smmu) = SmmuV2::create_for_unit_test(op_mode) else {
            return false;
        };
        let smmu = TestUnitSmmu(Some(smmu));
        while take_captured_quarantine_warning_for_test().is_some() {}

        // Case 1: BTI closed while a PMT is still pinned, then PMT cleanly unpinned and closed.
        // Nothing was leaked, so zero warnings must be emitted, and the stream becomes free once
        // the PMT closes.
        {
            let Ok(bti) = smmu.create_bti(BASE_SID) else {
                return false;
            };
            bti.set_name(b"OrphanCase1");
            let Some(pin) = pin_test_vmo(2) else {
                return false;
            };
            let Ok(pmt) = bti.map(pin, perms, RequireContiguousMapping::No) else {
                return false;
            };

            bti.on_dispatcher_zero_handles();
            check!(check_no_quarantine_warnings());
            check_eq!(smmu.create_bti(BASE_SID).err(), Some(Status::ALREADY_EXISTS));

            pmt.release_pinned_memory();
            pmt.on_dispatcher_zero_handles();
            drop(pmt);
            drop(bti);

            check!(check_no_quarantine_warnings());
        }

        // Case 2: PMT leaked first, then BTI closed with quarantine unreleased. Both modes must
        // emit `PmtQuarantined` followed by `BtiOrphanedWithQuarantinedPmts`; `Enforced` shuts
        // down cleanly while `Passthru` remains orphaned in `Fault` mode holding its stream.
        {
            let Ok(bti) = smmu.create_bti(BASE_SID) else {
                return false;
            };
            bti.set_name(b"OrphanCase2");
            let Some(pin) = pin_test_vmo(2) else {
                return false;
            };
            let Ok(pmt) = bti.map(pin, perms, RequireContiguousMapping::No) else {
                return false;
            };

            pmt.on_dispatcher_zero_handles();
            drop(pmt);
            check!(check_single_quarantine_warning(
                BASE_SID,
                b"OrphanCase2",
                BtiPageLeakReason::PmtQuarantined,
                2,
                1,
            ));
            check!(bti.in_fault_state());

            bti.on_dispatcher_zero_handles();
            check!(check_single_quarantine_warning(
                BASE_SID,
                b"OrphanCase2",
                BtiPageLeakReason::BtiOrphanedWithQuarantinedPmts,
                2,
                1,
            ));
            check!(check_quarantined_bti_eol(&smmu, &bti, BASE_SID, enforced));
            drop(bti);
        }
        let sid = if enforced { BASE_SID } else { BASE_SID + 1 };

        // Case 3a: Orphaned BTI has `pmt_a` leaked first, then `pmt_b` cleanly unpinned and
        // closed. `on_pmt_zero_handles(&pmt_b)` must still account for `pmt_a`'s quarantine.
        {
            let Ok(bti) = smmu.create_bti(sid) else {
                return false;
            };
            bti.set_name(b"OrphanCase3a");
            let (Some(pin_a), Some(pin_b)) = (pin_test_vmo(1), pin_test_vmo(1)) else {
                return false;
            };
            let (Ok(pmt_a), Ok(pmt_b)) = (
                bti.map(pin_a, perms, RequireContiguousMapping::No),
                bti.map(pin_b, perms, RequireContiguousMapping::No),
            ) else {
                return false;
            };

            bti.on_dispatcher_zero_handles();
            check!(check_no_quarantine_warnings());

            pmt_a.on_dispatcher_zero_handles();
            drop(pmt_a);
            check!(check_single_quarantine_warning(
                sid,
                b"OrphanCase3a",
                BtiPageLeakReason::PmtQuarantinedWhenBtiOrphaned,
                1,
                1,
            ));
            check_eq!(smmu.create_bti(sid).err(), Some(Status::ALREADY_EXISTS));

            pmt_b.release_pinned_memory();
            pmt_b.on_dispatcher_zero_handles();
            drop(pmt_b);
            check!(check_no_quarantine_warnings());
            check!(check_quarantined_bti_eol(&smmu, &bti, sid, enforced));
            drop(bti);
        }
        let sid = if enforced { sid } else { sid + 1 };

        // Case 3b: Reverse order on an orphaned BTI (`pmt_b` cleanly unpinned and closed first,
        // then final active `pmt_a` leaked).
        {
            let Ok(bti) = smmu.create_bti(sid) else {
                return false;
            };
            bti.set_name(b"OrphanCase3b");
            let (Some(pin_a), Some(pin_b)) = (pin_test_vmo(2), pin_test_vmo(1)) else {
                return false;
            };
            let (Ok(pmt_a), Ok(pmt_b)) = (
                bti.map(pin_a, perms, RequireContiguousMapping::No),
                bti.map(pin_b, perms, RequireContiguousMapping::No),
            ) else {
                return false;
            };

            bti.on_dispatcher_zero_handles();
            check!(check_no_quarantine_warnings());

            pmt_b.release_pinned_memory();
            pmt_b.on_dispatcher_zero_handles();
            drop(pmt_b);
            check!(check_no_quarantine_warnings());
            check_eq!(smmu.create_bti(sid).err(), Some(Status::ALREADY_EXISTS));

            pmt_a.on_dispatcher_zero_handles();
            drop(pmt_a);
            check!(check_single_quarantine_warning(
                sid,
                b"OrphanCase3b",
                BtiPageLeakReason::PmtQuarantinedWhenBtiOrphaned,
                2,
                1,
            ));
            check!(check_quarantined_bti_eol(&smmu, &bti, sid, enforced));
            drop(bti);
        }
        let sid = if enforced { sid } else { sid + 1 };

        // Case 4: Orphaned BTI's last two PMTs are both unpinned before either handle closes (as
        // two threads in `zx_pmt_unpin` can do). The first handle close ends the BTI's life; the
        // second must not call `on_end_of_life` again.
        {
            let Ok(bti) = smmu.create_bti(sid) else {
                return false;
            };
            let (Some(pin_a), Some(pin_b)) = (pin_test_vmo(1), pin_test_vmo(1)) else {
                return false;
            };
            let (Ok(pmt_a), Ok(pmt_b)) = (
                bti.map(pin_a, perms, RequireContiguousMapping::No),
                bti.map(pin_b, perms, RequireContiguousMapping::No),
            ) else {
                return false;
            };

            bti.on_dispatcher_zero_handles();
            pmt_a.release_pinned_memory();
            pmt_b.release_pinned_memory();
            check_eq!(smmu.create_bti(sid).err(), Some(Status::ALREADY_EXISTS));

            pmt_a.on_dispatcher_zero_handles();
            drop(pmt_a);
            check!(!bti.in_container());

            pmt_b.on_dispatcher_zero_handles();
            drop(pmt_b);
            drop(bti);

            check!(check_no_quarantine_warnings());
            let Ok(bti_again) = smmu.create_bti(sid) else {
                return false;
            };
            bti_again.on_dispatcher_zero_handles();
            drop(bti_again);
        }

        // Case 5: BTI closed while both a quarantined PMT (`pmt_a`) and a still-pinned PMT
        // (`pmt_b`) exist. Both modes must emit `BtiOrphanedWithQuarantinedPmts` on BTI close
        // while staying orphaned until `pmt_b` closes.
        {
            let Ok(bti) = smmu.create_bti(sid) else {
                return false;
            };
            bti.set_name(b"OrphanCase5");
            let (Some(pin_a), Some(pin_b)) = (pin_test_vmo(2), pin_test_vmo(1)) else {
                return false;
            };
            let (Ok(pmt_a), Ok(pmt_b)) = (
                bti.map(pin_a, perms, RequireContiguousMapping::No),
                bti.map(pin_b, perms, RequireContiguousMapping::No),
            ) else {
                return false;
            };

            pmt_a.on_dispatcher_zero_handles();
            drop(pmt_a);
            check!(check_single_quarantine_warning(
                sid,
                b"OrphanCase5",
                BtiPageLeakReason::PmtQuarantined,
                2,
                1,
            ));

            bti.on_dispatcher_zero_handles();
            check!(check_single_quarantine_warning(
                sid,
                b"OrphanCase5",
                BtiPageLeakReason::BtiOrphanedWithQuarantinedPmts,
                2,
                1,
            ));
            check!(bti.in_container());
            check_eq!(smmu.create_bti(sid).err(), Some(Status::ALREADY_EXISTS));

            pmt_b.release_pinned_memory();
            pmt_b.on_dispatcher_zero_handles();
            drop(pmt_b);
            check!(check_no_quarantine_warnings());
            check!(check_quarantined_bti_eol(&smmu, &bti, sid, enforced));
            drop(bti);
        }
        let sid = if enforced { sid } else { sid + 1 };

        // Case 6: PMT is unpinned before BTI handle closes, then PMT handle closes after BTI
        // `on_end_of_life` has already cleared `bti.smmu` (`op_mode == None`).
        {
            let Ok(bti) = smmu.create_bti(sid) else {
                return false;
            };
            bti.set_name(b"OrphanCase6");
            let Some(pin) = pin_test_vmo(1) else {
                return false;
            };
            let Ok(pmt) = bti.map(pin, perms, RequireContiguousMapping::No) else {
                return false;
            };

            pmt.release_pinned_memory();
            bti.on_dispatcher_zero_handles();
            check!(!bti.in_container());
            check!(check_no_quarantine_warnings());

            pmt.on_dispatcher_zero_handles();
            drop(pmt);
            drop(bti);
            check!(check_no_quarantine_warnings());
        }
    }

    true
}

/// In-kernel `k ut arm_smmu_v2` suite of the SMMUv2 component unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "arm_smmu_v2")]
mod ktest_suite {
    /// Tests `SmrValue` and `SmrValueIter`.
    #[test]
    fn smr_value_and_iter() {
        unittest::assert_true!(super::test_smr_value_and_iter());
    }

    /// Tests `StreamMatchRegGroup`.
    #[test]
    fn stream_match_reg_group() {
        unittest::assert_true!(super::test_stream_match_reg_group());
    }

    /// Tests `ContextBank`.
    #[test]
    fn context_bank() {
        unittest::assert_true!(super::test_context_bank());
    }

    /// Tests register definitions and bitfields.
    #[test]
    fn registers_and_bitfields() {
        unittest::assert_true!(super::test_registers_and_bitfields());
    }

    /// Tests `SmmuV2Bti` end-of-life, orphaning, and quarantine warning emission.
    #[test]
    fn smmu_bti_orphan_and_quarantine_end_of_life() {
        unittest::assert_true!(super::test_smmu_bti_orphan_and_quarantine_end_of_life());
    }
}
