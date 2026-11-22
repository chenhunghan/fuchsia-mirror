// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM SMMUv2 register blocks, stream matching, context banks, SMMU controller, BTI, and PMT.
//!
//! SMMUv2-specific hardware management lives in `v2/`, while hardware-independent page-table and
//! address-space infrastructure lives in `common/` for future reuse by SMMUv3. Submodule items are
//! re-exported here to preserve a flat namespace surface within `v2`.

pub(crate) mod console;
pub(crate) mod context_bank;
pub(crate) mod registers;
pub(crate) mod smmu;
pub(crate) mod smmu_bti;
pub(crate) mod smmu_pmt;
pub(crate) mod stream_match_reg;
#[cfg(ktest)]
pub(crate) mod unit_tests;
pub(crate) mod utils;

#[cfg(any(ktest, not(is_kernel)))]
pub(crate) use crate::arm_smmu::common::utils::{Bitmask128, BtiMode, bti_mode_to_string};
#[cfg(any(ktest, not(is_kernel)))]
pub(crate) use context_bank::ContextBank;
pub(crate) use smmu::{SmmuV2, ZbiDcfgArmSmmuV2Driver};
pub(crate) use smmu_bti::SmmuV2Bti;
pub(crate) use smmu_pmt::SmmuV2Pmt;
#[cfg(any(ktest, not(is_kernel)))]
pub(crate) use stream_match_reg::StreamMatchRegGroup;
#[cfg(any(ktest, not(is_kernel)))]
pub(crate) use utils::{
    AddrMode, SmrValue, addr_mode_to_string, arm_cbar_type_to_string, arm_s2cr_type_to_string,
    arm_smmu_mode_to_string, smr_intersects,
};
#[cfg(any(ktest, not(is_kernel)))]
pub(in crate::arm_smmu) use utils::{SmmuController, SmmuParams};
