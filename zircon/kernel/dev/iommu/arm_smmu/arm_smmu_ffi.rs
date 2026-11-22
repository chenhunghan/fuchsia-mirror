// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! C FFI entrypoint for ARM SMMU initialization and boot option validation.

use super::{common, v2};

/// Validates `"kernel.arm-smmu-mode"` and creates, initializes, and registers all
/// [`v2::SmmuV2`] instances described by `(v2_configs, v2_count)`.
///
/// # Safety
///
/// - [`boot_options::BootOptions::get()`] must have been initialized by the kernel boot path
///   before calling this function.
/// - If `v2_count > 0`, `v2_configs` must be non-null, properly aligned, and point to
///   `v2_count` valid, initialized [`v2::ZbiDcfgArmSmmuV2Driver`] elements for the duration
///   of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arm_smmu_init(
    v2_configs: *const v2::ZbiDcfgArmSmmuV2Driver,
    v2_count: usize,
) {
    assert!(v2_count == 0 || (!v2_configs.is_null() && v2_configs.is_aligned()));
    // SAFETY: Validated non-null and aligned above when `v2_count > 0`; caller guarantees the
    // pointed-to array contains `v2_count` valid, initialized elements for the call duration.
    let v2_slice = unsafe { zr::slice_from_raw_parts(v2_configs, v2_count) };
    if v2_slice.is_empty() {
        return;
    }

    common::warn_on_invalid_boot_options();

    for config in v2_slice {
        if let Err(status) = v2::SmmuV2::create(config) {
            debug::dprintf!(
                INFO,
                "WARNING - Failed to construct ARM SMMU @ {:#010x} result {}\n",
                config.mmio_phys,
                status.into_raw()
            );
        }
    }
}
