// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_INCLUDE_DEV_ARM_SMMU_INIT_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_INCLUDE_DEV_ARM_SMMU_INIT_H_

#include <lib/zbi-format/driver-config.h>
#include <stddef.h>

// Keep the layout of the configuration structs shared with Rust in sync; the
// Rust side asserts the same values via zr::static_assert!.
static_assert(sizeof(zbi_dcfg_arm_smmu_irq_t) == 8);
static_assert(alignof(zbi_dcfg_arm_smmu_irq_t) == 4);
static_assert(sizeof(zbi_dcfg_arm_smmu_v2_driver_t) == 1120);
static_assert(alignof(zbi_dcfg_arm_smmu_v2_driver_t) == 8);

// Entry point of the Rust ARM SMMU driver (//zircon/kernel/dev/iommu/arm_smmu/arm_smmu_ffi.rs)
// which platform initialization calls.
extern "C" {

// Validates `kernel.arm-smmu-mode` and creates, initializes, and registers all SMMU
// instances described by `(v2_configs, v2_count)`.
void rust_arm_smmu_init(const zbi_dcfg_arm_smmu_v2_driver_t* v2_configs, size_t v2_count);

}  // extern "C"

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_INCLUDE_DEV_ARM_SMMU_INIT_H_
