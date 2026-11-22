// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Shared Stage-1 VMSAv8-64 translation table, page cache, device address space,
//! mode-string parser, and environment abstractions used by both SMMUv2 and SMMUv3.

pub(crate) mod constants;
pub(crate) mod device_aspace;
pub(crate) mod env;
pub(crate) mod irq;
pub(crate) mod mmio;
pub(crate) mod page_cache;
pub(crate) mod smmu_mode;
pub(crate) mod translation_table;
pub(crate) mod utils;
pub(crate) mod vmsav8_64;

#[cfg(is_kernel)]
pub(crate) use crate::iommu::EscapedName;
pub(crate) use constants::{
    ALL_PERM_FLAGS, ArmSmmuMode, BtiPageLeakReason, DEFAULT_TCR_T0SZ, DEFAULT_TTBR0_ADDR_SPACE_SZ,
    INVALID_PADDR, InterruptPolarity, InterruptTriggerMode, PAGE_MASK, PAGE_SHIFT, PAGE_SIZE,
    QueryAddressResult, RequireContiguousMapping, ZX_MAX_NAME_LEN, ZX_RSRC_KIND_MMIO,
    is_page_rounded, round_up_page_size,
};
#[cfg(any(ktest, not(is_kernel)))]
pub(crate) use constants::{
    IOMMU_FLAG_PERM_EXECUTE, IOMMU_FLAG_PERM_READ, IOMMU_FLAG_PERM_WRITE, PAGE_SIZE_USIZE,
};
pub(crate) use device_aspace::{
    DEFAULT_MAX_PAGE_CACHE_ENTRIES, DeviceAspace, MappedPin, MappingAllocation, TlbInvalidator,
};
#[cfg(any(ktest, not(is_kernel)))]
pub(crate) use env::SmmuPage;
pub(crate) use env::{
    DRIVER_OOPS_BANNER, PinnedVmoSource, SmmuEnv, SmmuNoIrqSavePolicy, SmmuPinnedVmo, SmmuRawMutex,
    SmmuRawSpinLock,
};
#[cfg(any(ktest, not(is_kernel)))]
pub(crate) use page_cache::PageCache;
pub(crate) use smmu_mode::warn_on_invalid_boot_options;
#[cfg(any(ktest, not(is_kernel)))]
pub(crate) use smmu_mode::{
    get_smmu_mode, get_smmu_mode_bytes, get_smmu_mode_cstr, validate_smmu_mode_cstr,
    validate_smmu_mode_string,
};
