// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::feature::x86_get_microarch_config;

/// Returns the raw Local APIC timer reference clock frequency if it can be
/// determined from the CPU alone (i.e. without calibration), or returns 0 if
/// not.
///
/// Depending on the microarchitecture, `config.get_apic_freq` returns the
/// `CPUID.15H` Core Crystal Clock on Skylake+ Intel or the 100 MHz processor
/// bus/reference clock (`BCLK`/`CLKIN`) on Sandy Bridge-Broadwell and AMD
/// Bulldozer (AMD Zen returns 0 as it does not implement `CPUID.15H`).
#[inline]
pub fn x86_lookup_core_crystal_freq() -> u64 {
    let config = x86_get_microarch_config().expect("x86_microarch_config not initialized");
    let func = config.get_apic_freq.expect("get_apic_freq not initialized");
    // SAFETY: Per `x86_get_microarch_config`'s safety-usable invariant, `get_apic_freq` has no
    // preconditions.
    unsafe { func() }
}

/// Returns the TSC frequency if it can be found from the CPU alone (i.e. without
/// calibration), or returns 0 if not.
#[inline]
pub fn x86_lookup_tsc_freq() -> u64 {
    let config = x86_get_microarch_config().expect("x86_microarch_config not initialized");
    let func = config.get_tsc_freq.expect("get_tsc_freq not initialized");
    // SAFETY: Per `x86_get_microarch_config`'s safety-usable invariant, `get_tsc_freq` has no
    // preconditions beyond running in the kernel (CPL 0) with `%gs.base` initialized, which
    // holds for all kernel threads, and is safe to call from any thread context (AMD Zen's
    // `read_msr_safe` preconditions are validated at runtime).
    unsafe { func() }
}
