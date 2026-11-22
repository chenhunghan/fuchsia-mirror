// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use regio::traits::RwSafeReg;
use regio::x86::Cpuid;

use super::cpuid::{ADVANCED_POWER_FEATURE_FLAGS, THERMAL_AND_POWER_FEATURE_FLAGS_A, VendorString};
use super::{AmdHardwareConfigurationMsr, MiscFeaturesMsr};

/// Sets the "Turbo" state, which allows the processor to dynamically adjust and
/// control its operating frequency (`arch::SetX86CpuTurboState`).
///
/// Turbo here collectively refers to the analogous technologies of "Intel Turbo
/// Boost" and "AMD Turbo Core". Returns `false` if Turbo is unsupported; else
/// returns `true`.
///
/// `cpuid` must reflect the current state of the CPU whose MSRs are accessed,
/// since CPUID leaf 0x6 reports that CPU's live Turbo state.
///
/// For more detail, see:
/// [intel/vol3]: 14.3.3  Intel® Turbo Boost Technology.
/// [amd/vol2]: 17.2  Core Performance Boost.
pub fn set_x86_cpu_turbo_state(
    cpuid: &impl Cpuid,
    misc_enable_msr: &impl RwSafeReg<MiscFeaturesMsr>,
    amd_hw_cfg_msr: &impl RwSafeReg<AmdHardwareConfigurationMsr>,
    enable: bool,
) -> bool {
    // [intel/vol3]: 14.3.2.1  Discover Hardware Support and Enabling of Opportunistic Processor
    // Performance Operation.
    //
    // The Intel way, which Intel makes rather convoluted. Initially, when
    // powered on, IA32_MISC_ENABLE enumerates whether Turbo is supported: if
    // IDA_DISABLE is set, then Turbo is supported and is disabled by default;
    // else it is not supported. Moreover, unlike every other CPUID feature, leaf
    // 0x6 EAX does not enumerate whether Turbo is supported, but instead
    // dynamically reflects the actual Turbo state. Accordingly, to determine
    // whether Turbo is supported we must cross-reference both CPUID and MSR
    // state.
    //
    // IDA stands for "Intel Dynamic Acceleration", an earlier name/iteration of
    // Intel Turbo Boost.
    let vendor = VendorString::from_cpuid(cpuid).vendor();
    if MiscFeaturesMsr::is_supported(vendor) && cpuid.supports(THERMAL_AND_POWER_FEATURE_FLAGS_A) {
        let intel_features = cpuid.read(THERMAL_AND_POWER_FEATURE_FLAGS_A);
        let mut misc_enable = misc_enable_msr.read();
        let intel_supported_and_on = intel_features.turbo() || intel_features.turbo_max();
        let intel_supported_and_off = misc_enable.ida_disable();
        let intel_supported = intel_supported_and_on || intel_supported_and_off;
        if intel_supported {
            misc_enable.set_ida_disable(!enable);
            misc_enable_msr.write(misc_enable);
            return true;
        }
    }

    // The AMD way.
    // CPB stands for "Core Performance Boost", an earlier name/iteration of AMD
    // Turbo Core.
    if cpuid.supports(ADVANCED_POWER_FEATURE_FLAGS)
        && cpuid.read(ADVANCED_POWER_FEATURE_FLAGS).cpb()
    {
        amd_hw_cfg_msr.modify(|val| *val.set_cpb_dis(!enable));
        return true;
    }

    // Unsupported.
    false
}

#[cfg(test)]
mod tests {
    use regio::testing::ExpectationReg;
    use regio::testing::x86::FakeCpuid;

    use super::*;
    use crate::x86::cpuid::{
        AdvancedPowerFeatureFlags, MAX_EXTENDED_LEAF, MAX_LEAF, ThermalAndPowerFeatureFlagsA,
        VENDOR_STRING_B, VENDOR_STRING_C, VENDOR_STRING_D,
    };

    fn set_intel_vendor(cpuid: &mut FakeCpuid) {
        cpuid
            .set(VENDOR_STRING_B, u32::from_le_bytes(*b"Genu"))
            .set(VENDOR_STRING_D, u32::from_le_bytes(*b"ineI"))
            .set(VENDOR_STRING_C, u32::from_le_bytes(*b"ntel"));
    }

    fn set_amd_vendor(cpuid: &mut FakeCpuid) {
        cpuid
            .set(VENDOR_STRING_B, u32::from_le_bytes(*b"Auth"))
            .set(VENDOR_STRING_D, u32::from_le_bytes(*b"enti"))
            .set(VENDOR_STRING_C, u32::from_le_bytes(*b"cAMD"));
    }

    #[test]
    fn set_turbo_state_intel_supported_and_unsupported() {
        const INTEL_TURBO_MAYBE_SUPPORTED_AND_ON: u64 = 0b101_0101_0101;
        const INTEL_TURBO_SUPPORTED_AND_OFF: u64 =
            INTEL_TURBO_MAYBE_SUPPORTED_AND_ON | (1u64 << 38);

        // Supported via CPUID leaf 6 turbo = 1 (matching Intel Core i3-6100), exercising both
        // enabling (clearing IDA_DISABLE) and disabling (setting IDA_DISABLE when IDA_DISABLE = 0).
        {
            let mut cpuid = FakeCpuid::new();
            set_intel_vendor(&mut cpuid);
            let mut flags = ThermalAndPowerFeatureFlagsA::from(0);
            flags.set_turbo(true);
            cpuid.set(MAX_LEAF, 6).set(THERMAL_AND_POWER_FEATURE_FLAGS_A, flags);

            let mut misc = ExpectationReg::new();
            misc.expect_read(MiscFeaturesMsr::from(INTEL_TURBO_SUPPORTED_AND_OFF))
                .expect_write(MiscFeaturesMsr::from(INTEL_TURBO_MAYBE_SUPPORTED_AND_ON))
                .expect_read(MiscFeaturesMsr::from(INTEL_TURBO_MAYBE_SUPPORTED_AND_ON))
                .expect_write(MiscFeaturesMsr::from(INTEL_TURBO_SUPPORTED_AND_OFF));
            let amd_cfg = ExpectationReg::new();

            assert!(set_x86_cpu_turbo_state(&cpuid, &misc, &amd_cfg, true));
            assert!(set_x86_cpu_turbo_state(&cpuid, &misc, &amd_cfg, false));
        }

        // Supported via IDA_DISABLE = 1 in IA32_MISC_ENABLE even when CPUID leaf 6 turbo = 0.
        {
            let mut cpuid = FakeCpuid::new();
            set_intel_vendor(&mut cpuid);
            cpuid
                .set(MAX_LEAF, 6)
                .set(THERMAL_AND_POWER_FEATURE_FLAGS_A, ThermalAndPowerFeatureFlagsA::from(0));
            let mut misc = ExpectationReg::new();
            misc.expect_read(MiscFeaturesMsr::from(INTEL_TURBO_SUPPORTED_AND_OFF))
                .expect_write(MiscFeaturesMsr::from(INTEL_TURBO_MAYBE_SUPPORTED_AND_ON))
                .expect_read(MiscFeaturesMsr::from(INTEL_TURBO_SUPPORTED_AND_OFF))
                .expect_write(MiscFeaturesMsr::from(INTEL_TURBO_SUPPORTED_AND_OFF));
            let amd_cfg = ExpectationReg::new();
            assert!(set_x86_cpu_turbo_state(&cpuid, &misc, &amd_cfg, true));
            assert!(set_x86_cpu_turbo_state(&cpuid, &misc, &amd_cfg, false));
        }

        // Unsupported when both CPUID leaf 6 turbo/turbo_max and IDA_DISABLE are 0.
        {
            let mut cpuid = FakeCpuid::new();
            set_intel_vendor(&mut cpuid);
            cpuid
                .set(MAX_LEAF, 6)
                .set(THERMAL_AND_POWER_FEATURE_FLAGS_A, ThermalAndPowerFeatureFlagsA::from(0));
            let mut misc = ExpectationReg::new();
            misc.expect_read(MiscFeaturesMsr::from(INTEL_TURBO_MAYBE_SUPPORTED_AND_ON))
                .expect_read(MiscFeaturesMsr::from(INTEL_TURBO_MAYBE_SUPPORTED_AND_ON));
            let amd_cfg = ExpectationReg::new();
            assert!(!set_x86_cpu_turbo_state(&cpuid, &misc, &amd_cfg, true));
            assert!(!set_x86_cpu_turbo_state(&cpuid, &misc, &amd_cfg, false));
        }
    }

    #[test]
    fn set_turbo_state_amd_supported_and_unsupported() {
        const AMD_TURBO_ON: u64 = 0b110_1101_1011;
        const AMD_TURBO_OFF: u64 = AMD_TURBO_ON | (1u64 << 25);

        // Supported when CPB bit is set.
        {
            let mut cpuid = FakeCpuid::new();
            set_amd_vendor(&mut cpuid);
            let mut flags = AdvancedPowerFeatureFlags::from(0);
            flags.set_cpb(true);
            cpuid.set(MAX_EXTENDED_LEAF, 0x8000_0007).set(ADVANCED_POWER_FEATURE_FLAGS, flags);

            let misc = ExpectationReg::new();
            let mut amd_cfg = ExpectationReg::new();
            amd_cfg
                .expect_read(AmdHardwareConfigurationMsr::from(AMD_TURBO_OFF))
                .expect_write(AmdHardwareConfigurationMsr::from(AMD_TURBO_ON))
                .expect_read(AmdHardwareConfigurationMsr::from(AMD_TURBO_ON))
                .expect_write(AmdHardwareConfigurationMsr::from(AMD_TURBO_OFF));

            assert!(set_x86_cpu_turbo_state(&cpuid, &misc, &amd_cfg, true));
            assert!(set_x86_cpu_turbo_state(&cpuid, &misc, &amd_cfg, false));
        }

        // Unsupported when CPB bit is clear.
        {
            let mut cpuid = FakeCpuid::new();
            set_amd_vendor(&mut cpuid);
            cpuid
                .set(MAX_EXTENDED_LEAF, 0x8000_0007)
                .set(ADVANCED_POWER_FEATURE_FLAGS, AdvancedPowerFeatureFlags::from(0));
            let misc = ExpectationReg::new();
            let amd_cfg = ExpectationReg::new();
            assert!(!set_x86_cpu_turbo_state(&cpuid, &misc, &amd_cfg, true));
            assert!(!set_x86_cpu_turbo_state(&cpuid, &misc, &amd_cfg, false));
        }
    }
}
