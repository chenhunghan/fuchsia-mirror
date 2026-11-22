// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::ArchCapabilitiesMsr;
use super::cpuid::{EXTENDED_AMD_FEATURES_B, EXTENDED_FEATURES_D};
use bitrs::layout;
use regio::RwSafe;
use regio::traits::{ReadReg, RwSafeReg};
use regio::x86::{Cpuid, Msr};

/// Speculation control.
///
/// [intel/vol4]: Table 2-2.  IA-32 Architectural MSRs (Contd.).
/// [amd/ibc]: PRESENCE.
/// [amd/ssbd]: PRESENCE.
pub const SPECULATION_CONTROL: Msr<0x0000_0048, SpeculationControlMsr, RwSafe> = Msr::new();

layout!({
    /// Layout for [`SPECULATION_CONTROL`].
    pub struct SpeculationControlMsr(u64);
    {
        let __ @ 63..3;
        let ssbd @ 2;
        let stibp @ 1;
        let ibrs @ 0;
    }
});

impl SpeculationControlMsr {
    pub fn is_supported(cpuid: impl Cpuid) -> bool {
        // Intel documents that the MSR is supported only if one of the kinds of
        // speculation it can control is itself enumerated; AMD does similarly, but
        // that information must be cobbled together from [amd/ibc] and [amd/ssbd].

        // The Intel way:
        let intel_features = cpuid.read(EXTENDED_FEATURES_D);
        if intel_features.ibrs_ibpb() || intel_features.stibp() || intel_features.ssbd() {
            return true;
        }

        // The AMD way:
        if !cpuid.supports(EXTENDED_AMD_FEATURES_B) {
            return false;
        }
        let amd_features = cpuid.read(EXTENDED_AMD_FEATURES_B);
        amd_features.ibrs() || amd_features.stibp() || amd_features.ssbd()
    }
}

/// Virtual speculation control (e.g., for hypervisor usage).
///
/// [amd/ssbd]: PRESENCE.
pub const VIRTUAL_SPECULATION_CONTROL: Msr<0xc001_011f, VirtualSpeculationControlMsr, RwSafe> =
    Msr::new();

layout!({
    /// Layout for [`VIRTUAL_SPECULATION_CONTROL`].
    pub struct VirtualSpeculationControlMsr(u64);
    {
        let __ @ 63..3;
        let ssbd @ 2;
        let __ @ 1..0;
    }
});

impl VirtualSpeculationControlMsr {
    pub fn is_supported(cpuid: impl Cpuid) -> bool {
        // [amd/ssbd]: HYPERVISOR USAGE MODELS.
        cpuid.supports(EXTENDED_AMD_FEATURES_B) && cpuid.read(EXTENDED_AMD_FEATURES_B).virt_ssbd()
    }
}

/// Whether the Indirect Branch Prediction Barrier (IBPB) is supported.
///
/// https://software.intel.com/security-software-guidance/deep-dives/deep-dive-indirect-branch-predictor-barrier.
pub fn has_ibpb(cpuid: &impl Cpuid) -> bool {
    // The Intel way.
    if cpuid.supports(EXTENDED_FEATURES_D) && cpuid.read(EXTENDED_FEATURES_D).ibrs_ibpb() {
        return true;
    }

    // [amd/ibc]: PRESENCE.
    // The AMD way.
    cpuid.supports(EXTENDED_AMD_FEATURES_B) && cpuid.read(EXTENDED_AMD_FEATURES_B).ibpb()
}

/// Whether Indirect Branch Restricted Speculation (IBRS) is supported. The
/// "always on" mode refers to an optimization in which IBRS need only be
/// enabled once; IBRS in this mode are also referred to as "enhanced".
///
/// https://software.intel.com/security-software-guidance/deep-dives/deep-dive-indirect-branch-restricted-speculation.
pub fn has_ibrs(
    cpuid: &impl Cpuid,
    msr: &impl ReadReg<ArchCapabilitiesMsr>,
    always_on_mode: bool,
) -> bool {
    // The Intel way.
    let intel_always_on = ArchCapabilitiesMsr::is_supported(cpuid) && msr.read().ibrs_all();
    let intel_present = cpuid.read(EXTENDED_FEATURES_D).ibrs_ibpb();
    if intel_present && (!always_on_mode || intel_always_on) {
        return true;
    }

    // The AMD way.
    if cpuid.supports(EXTENDED_AMD_FEATURES_B) {
        let features = cpuid.read(EXTENDED_AMD_FEATURES_B);
        if features.ibrs() && (!always_on_mode || features.ibrs_always_on()) {
            return true;
        }
    }

    false
}

/// Enables IBRS, which requires the feature to be supported.
pub fn enable_ibrs(cpuid: &impl Cpuid, msr: &impl RwSafeReg<SpeculationControlMsr>) {
    debug_assert!(SpeculationControlMsr::is_supported(cpuid));
    msr.modify(|val| *val.set_ibrs(true));
}

/// Whether Single Thread Indirect Branch Predictors (STIBP) are supported. The
/// "always on" mode refers to an optimization in which STIBP need only be
/// enabled once.
///
/// https://software.intel.com/security-software-guidance/deep-dives/deep-dive-single-thread-indirect-branch-predictors.
pub fn has_stibp(cpuid: &impl Cpuid, always_on_mode: bool) -> bool {
    // The Intel way.
    let intel_present = cpuid.read(EXTENDED_FEATURES_D).stibp();
    if intel_present && !always_on_mode {
        // Intel does not offer an "always on" mode.
        return true;
    }

    // The AMD way.
    if cpuid.supports(EXTENDED_AMD_FEATURES_B) {
        let features = cpuid.read(EXTENDED_AMD_FEATURES_B);
        if features.stibp() && (!always_on_mode || features.stibp_always_on()) {
            return true;
        }
    }
    false
}

/// Enables STIBP, which requires the feature to be supported.
pub fn enable_stibp(cpuid: &impl Cpuid, msr: &impl RwSafeReg<SpeculationControlMsr>) {
    debug_assert!(SpeculationControlMsr::is_supported(cpuid));
    msr.modify(|val| *val.set_stibp(true));
}

#[cfg(test)]
mod tests {
    use regio::testing::ExpectationReg;
    use regio::testing::x86::FakeCpuid;

    use super::*;
    use crate::x86::cpuid::{
        ExtendedAmdFeaturesB, ExtendedFeatureFlagsD, MAX_EXTENDED_LEAF, MAX_LEAF,
    };

    #[test]
    fn ibpb_and_spec_ctrl_enablers() {
        let mut cpuid = FakeCpuid::new();
        assert!(!has_ibpb(&cpuid));

        // Leaf 7 supported, but IBRS_IBPB bit clear.
        cpuid.set(MAX_LEAF, 7).set(EXTENDED_FEATURES_D, ExtendedFeatureFlagsD::from(0));
        assert!(!has_ibpb(&cpuid));

        // Intel IBRS_IBPB bit in leaf 7 EDX.
        let mut ext_d = ExtendedFeatureFlagsD::from(0);
        ext_d.set_ibrs_ibpb(true).set_stibp(true);
        cpuid.set(EXTENDED_FEATURES_D, ext_d);
        assert!(has_ibpb(&cpuid));

        let mut msr = ExpectationReg::new();
        msr.expect_read(SpeculationControlMsr::from(0))
            .expect_write(SpeculationControlMsr::from(0b01))
            .expect_read(SpeculationControlMsr::from(0b01))
            .expect_write(SpeculationControlMsr::from(0b11));
        enable_ibrs(&cpuid, &msr);
        enable_stibp(&cpuid, &msr);

        // AMD leaf 0x8000_0008 supported, but IBPB bit clear.
        let mut amd_cpuid = FakeCpuid::new();
        amd_cpuid
            .set(MAX_EXTENDED_LEAF, 0x8000_0008)
            .set(EXTENDED_AMD_FEATURES_B, ExtendedAmdFeaturesB::from(0));
        assert!(!has_ibpb(&amd_cpuid));

        // AMD IBPB bit in leaf 0x8000_0008 EBX.
        let mut amd_b = ExtendedAmdFeaturesB::from(0);
        amd_b.set_ibpb(true);
        amd_cpuid.set(EXTENDED_AMD_FEATURES_B, amd_b);
        assert!(has_ibpb(&amd_cpuid));
    }
}
