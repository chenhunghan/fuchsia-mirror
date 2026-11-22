// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::fmt;
use core::str::Utf8Error;

use bitrs::{bitfield_repr, layout};
use regio::x86::{Cpuid, CpuidValue, EAX, EBX, ECX, EDX};

use super::Vendor;

/// Leaf/Function 0x0, EAX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.1, CPUID Fn0000_0000_EAX Largest Standard Function Number.
pub const MAX_LEAF: CpuidValue<0x0, 0x0, EAX, u32> = CpuidValue::new();

/// Leaf/Function 0x0, EBX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.1, CPUID Fn0000_0000_E[D,C,B]X Processor Vendor.
pub const VENDOR_STRING_B: CpuidValue<0x0, 0x0, EBX, u32> = CpuidValue::new();

/// Leaf/Function 0x0, ECX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.1, CPUID Fn0000_0000_E[D,C,B]X Processor Vendor.
pub const VENDOR_STRING_C: CpuidValue<0x0, 0x0, ECX, u32> = CpuidValue::new();

/// Leaf/Function 0x0, EDX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.1, CPUID Fn0000_0000_E[D,C,B]X Processor Vendor.
pub const VENDOR_STRING_D: CpuidValue<0x0, 0x0, EDX, u32> = CpuidValue::new();

/// A vendor string derived from CPUID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VendorString([u8; 12]);

impl VendorString {
    /// Intel's vendor string.
    pub const INTEL: Self = Self(*b"GenuineIntel");

    /// AMD's vendor string.
    pub const AMD: Self = Self(*b"AuthenticAMD");

    /// Returns the CPUID-based vendor string.
    pub fn from_cpuid(cpuid: impl Cpuid) -> Self {
        let ebx = cpuid.read(VENDOR_STRING_B).to_le_bytes();
        let ecx = cpuid.read(VENDOR_STRING_C).to_le_bytes();
        let edx = cpuid.read(VENDOR_STRING_D).to_le_bytes();
        Self([
            ebx[0], ebx[1], ebx[2], ebx[3], //
            edx[0], edx[1], edx[2], edx[3], //
            ecx[0], ecx[1], ecx[2], ecx[3], //
        ])
    }

    /// Returns the vendor identified by this string.
    ///
    /// Returns [`Vendor::Unknown`] when the vendor string does not correspond
    /// to a known vendor.
    pub fn vendor(&self) -> Vendor {
        match *self {
            Self::INTEL => Vendor::Intel,
            Self::AMD => Vendor::Amd,
            _ => Vendor::Unknown,
        }
    }

    pub fn as_str(&self) -> Result<&str, Utf8Error> {
        str::from_utf8(&self.0)
    }
}

/// Leaf/Function 0x1, EBX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.2  Function 1h—Processor and Processor Feature Identifiers.
pub const PROCESSOR_INFO: CpuidValue<0x1, 0x0, EBX, ProcessorInfo> = CpuidValue::new();

layout!({
    /// The layout of EBX in [`PROCESSOR_INFO`].
    pub struct ProcessorInfo(u32);
    {
        let initial_apic_id @ 31..24;
        let max_logical_processors @ 23..16;
        let clflush_size @ 15..8;
        let brand_index @ 7..0;
    }
});

impl ProcessorInfo {
    pub fn cache_line_size_bytes(&self) -> usize {
        self.clflush_size() as usize * 8
    }
}

/// Leaf/Function 0x1, ECX
///
/// [intel/vol2]: Table 3-10.  Feature Information Returned in the ECX Register.
/// [amd/vol3]: E.3.2, CPUID Fn0000_0001_ECX Feature Identifiers.
pub const FEATURE_FLAGS_C: CpuidValue<0x1, 0x0, ECX, FeatureFlagsC> = CpuidValue::new();

layout!({
    /// The layout of ECX in [`FEATURE_FLAGS_C`].
    pub struct FeatureFlagsC(u32);
    {
        let hypervisor @ 31;
        let rdrand @ 30;
        let f16c @ 29;
        let avx @ 28;
        let osxsave @ 27;
        let xsave @ 26;
        let aes @ 25;
        let tsc_deadline @ 24;
        let popcnt @ 23;
        let movbe @ 22;
        let x2apic @ 21;
        let sse4_2 @ 20;
        let sse4_1 @ 19;
        let dca @ 18;
        let pcid @ 17;
        let __ @ 16;
        let pdcm @ 15;
        let xtpr @ 14;
        let cmpxchg16b @ 13;
        let fma @ 12;
        let sdbg @ 11;
        let cnxt_id @ 10;
        let ssse3 @ 9;
        let tm2 @ 8;
        let eist @ 7;
        let smx @ 6;
        let vmx @ 5;
        let ds_cpl @ 4;
        let monitor @ 3;
        let dtes64 @ 2;
        let pclmulqdq @ 1;
        let sse3 @ 0;
    }
});

/// Leaf/Function 0x1, EDX
///
/// [intel/vol2]: Table 3-11.  More on Feature Information Returned in the EDX Register.
/// [amd/vol3]: E.3.6  Function 7h—Structured Extended Feature Identifiers.
pub const FEATURE_FLAGS_D: CpuidValue<0x1, 0x0, EDX, FeatureFlagsD> = CpuidValue::new();

layout!({
    /// The layout of EDX in [`FEATURE_FLAGS_D`].
    pub struct FeatureFlagsD(u32);
    {
        let pbe @ 31;
        let __ @ 30;
        let tm @ 29;
        let htt @ 28;
        let ss @ 27;
        let sse2 @ 26;
        let sse @ 25;
        let fxsr @ 24;
        let mmx @ 23;
        let acpi @ 22;
        let ds @ 21;
        let __ @ 20;
        let clfsh @ 19;
        let psn @ 18;
        let pse36 @ 17;
        let pat @ 16;
        let cmov @ 15;
        let mca @ 14;
        let pge @ 13;
        let mtrr @ 12;
        let sep @ 11;
        let __ @ 10;
        let apic @ 9;
        let cx8 @ 8;
        let mce @ 7;
        let pae @ 6;
        let msr @ 5;
        let tsc @ 4;
        let pse @ 3;
        let de @ 2;
        let vme @ 1;
        let fpu @ 0;
    }
});

/// Leaf/Function 0x7, EBX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.6, CPUID Fn0000_0007_EBX_x0 Structured Extended Feature
/// Identifiers (ECX=0).
pub const EXTENDED_FEATURES_B: CpuidValue<0x7, 0x0, EBX, ExtendedFeatureFlagsB> = CpuidValue::new();

layout!({
    /// The layout of EBX in [`EXTENDED_FEATURES_B`].
    pub struct ExtendedFeatureFlagsB(u32);
    {
        let avx512vl @ 31;
        let avx512bw @ 30;
        let sha @ 29;
        let avx512cd @ 28;
        let avx512er @ 27;
        let avx512pf @ 26;
        let intel_pt @ 25;
        let clwb @ 24;
        let clflushopt @ 23;
        let __ @ 22;
        let avx512_ifma @ 21;
        let smap @ 20;
        let adx @ 19;
        let rdseed @ 18;
        let avx512dq @ 17;
        let avx512f @ 16;
        let rdt_a @ 15;
        let mpx @ 14;
        let fpu_cs_ds_deprecated @ 13;
        let rdt_m @ 12;
        let rtm @ 11;
        let invpcid @ 10;
        let erms @ 9;
        let bmi2 @ 8;
        let smep @ 7;
        let fdp_excptn_only_x87 @ 6;
        let avx2 @ 5;
        let hle @ 4;
        let bmi1 @ 3;
        let sgx @ 2;
        let tsc_adjust @ 1;
        let fsgsbase @ 0;
    }
});

/// Leaf/Function 0x7, ECX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.6, CPUID Fn0000_0007_ECX_x0 Structured Extended Feature
/// Identifiers (ECX=0).
pub const EXTENDED_FEATURES_C: CpuidValue<0x7, 0x0, ECX, ExtendedFeatureFlagsC> = CpuidValue::new();

layout!({
    /// The layout of ECX in [`EXTENDED_FEATURES_C`].
    pub struct ExtendedFeatureFlagsC(u32);
    {
        let pks @ 31;
        let sgx_lc @ 30;
        let __ @ 29;
        let movdir64b @ 28;
        let movdiri @ 27;
        let __ @ 26;
        let cldemote @ 25;
        let __ @ 24;
        let kl @ 23;
        let rdpid @ 22;
        // Bits [21:17] are 'The value of MAWAU used by the BNDLDX and BNDSTX
        // instructions in 64-bit mode.'
        let __ @ 21..17;
        let la57 @ 16;
        let __ @ 15;
        let avx512_vpopcntdq @ 14;
        let tme_en @ 13;
        let avx512_bitalg @ 12;
        let avx512_vnni @ 11;
        let vpclmulqdq @ 10;
        let vaes @ 9;
        let gfni @ 8;
        let cet_ss @ 7;
        let avx512_vbmi2 @ 6;
        let waitpkg @ 5;
        let ospke @ 4;
        let pku @ 3;
        let umip @ 2;
        let avx512_vbmi @ 1;
        let prefetchwt1 @ 0;
    }
});

/// Leaf/Function 0x7, EDX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.6, CPUID Fn0000_0007_EDX_x0 Structured Extended Feature
/// Identifiers (ECX=0).
pub const EXTENDED_FEATURES_D: CpuidValue<0x7, 0x0, EDX, ExtendedFeatureFlagsD> = CpuidValue::new();

layout!({
    /// The layout of EDX in [`EXTENDED_FEATURES_D`].
    pub struct ExtendedFeatureFlagsD(u32);
    {
        let ssbd @ 31;
        let ia32_core_capabilities @ 30;
        let ia32_arch_capabilities @ 29;
        let l1d_flush @ 28;
        let stibp @ 27;
        let ibrs_ibpb @ 26;
        let __ @ 25..21;
        let cet_ibt @ 20;
        let __ @ 19;
        let pconfig @ 18;
        let __ @ 17..16;
        let hybrid @ 15;
        let serialize @ 14;
        let __ @ 13..11;
        let md_clear @ 10;
        let __ @ 9;
        let avx512_vp2intersect @ 8;
        let __ @ 7..5;
        let fsrm @ 4;
        let avx512_4fmaps @ 3;
        let avx512_4vnniw @ 2;
        let __ @ 1..0;
    }
});

/// Cache type for Cache Topology leaves.
#[bitfield_repr(u8)]
#[derive(Clone, Copy)]
pub enum CacheType {
    Null = 0,
    Data = 1,
    Instruction = 2,
    Unified = 3,
}

impl CacheType {
    /// Returns the human-readable cache type string (`arch::ToString(X86CacheType)`).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Null => "Null",
            Self::Data => "Data",
            Self::Instruction => "Instruction",
            Self::Unified => "Unified",
        }
    }
}

impl fmt::Display for CacheType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// TODO(https://github.com/rust-lang/rust/issues/113521): These values should
// be generic over subleaf.

/// Leaf/Function 0x4, EAX (subleaf 0)
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
pub const INTEL_CACHE_TOPOLOGY_A: CpuidValue<0x4, 0, EAX, CacheTopologyA> = CpuidValue::new();

/// Leaf/Function 0x8000_001d, EAX (subleaf 0)
///
/// [amd/vol3]: E.4.15  Function 8000_001Dh—Cache Topology Information.
pub const AMD_CACHE_TOPOLOGY_A: CpuidValue<0x8000_001d, 0, EAX, CacheTopologyA> = CpuidValue::new();

layout!({
    /// The layout of EAX in Intel and AMD Cache Topology leaves.
    pub struct CacheTopologyA(u32);
    {
        let max_cores @ 31..26; // Reserved on AMD.
        let max_sharing_logical_processors @ 25..14;
        let __ @ 13..10;
        let fully_associative @ 9;
        let self_initializing @ 8;
        let cache_level @ 7..5;
        let cache_type @ 4..0: CacheType;
    }
});

layout!({
    /// The layout of EBX in Intel and AMD Cache Topology leaves.
    pub struct CacheTopologyB(u32);
    {
        let ways @ 31..22;
        let physical_line_partitions @ 21..12;
        let system_coherency_line_size @ 11..0;
    }
});

layout!({
    /// The layout of ECX in Intel and AMD Cache Topology leaves.
    pub struct CacheTopologyC(u32);
    {
        let sets @ 31..0;
    }
});

layout!({
    /// The layout of EDX in Intel and AMD Cache Topology leaves.
    pub struct CacheTopologyD(u32);
    {
        let __ @ 31..3;
        let complex_cache_indexing @ 2;
        let inclusive @ 1;
        let wbinvd @ 0;
    }
});

/// Leaf/Function 0x8000_0005, ECX
///
/// [amd/vol3]: E.4.4  Function 8000_0005h — L1 Cache and TLB Information.
pub const AMD_L1_DATA_CACHE_INFO: CpuidValue<0x8000_0005, 0, ECX, AmdL1CacheInformation> =
    CpuidValue::new();

/// Leaf/Function 0x8000_0005, EDX
///
/// [amd/vol3]: E.4.4  Function 8000_0005h — L1 Cache and TLB Information.
pub const AMD_L1_INSTRUCTION_CACHE_INFO: CpuidValue<0x8000_0005, 0, EDX, AmdL1CacheInformation> =
    CpuidValue::new();

layout!({
    /// The layout of [`AMD_L1_DATA_CACHE_INFO`] and
    /// [`AMD_L1_INSTRUCTION_CACHE_INFO`].
    pub struct AmdL1CacheInformation(u32);
    {
        let size_kb @ 31..24;
        let assoc @ 23..16;
        let lines_per_tag @ 15..8;
        let line_size @ 7..0;
    }
});

impl AmdL1CacheInformation {
    pub const FULLY_ASSOCIATIVE: u8 = 0xff;

    pub fn ways_of_associativity(&self) -> usize {
        if self.assoc() == Self::FULLY_ASSOCIATIVE { 0 } else { self.assoc() as usize }
    }

    pub fn fully_associative(&self) -> Option<bool> {
        match self.assoc() {
            0 => None,
            Self::FULLY_ASSOCIATIVE => Some(true),
            _ => Some(false),
        }
    }
}

/// Associativity encoding for L2 and L3 cache information leaves.
#[bitfield_repr(u8)]
#[derive(Clone, Copy)]
pub enum AmdL2L3Associativity {
    Disabled = 0x0,
    DirectMapped = 0x1,
    Ways2 = 0x2,
    Ways3 = 0x3,
    Ways4 = 0x4,
    Ways6 = 0x5,
    Ways8 = 0x6,
    // 0x7 is reserved.
    Ways16 = 0x8,
    SeeLeaf0x8000001d = 0x9,
    Ways32 = 0xa,
    Ways48 = 0xb,
    Ways64 = 0xc,
    Ways96 = 0xd,
    Ways128 = 0xe,
    FullyAssociative = 0xf,
}

impl AmdL2L3Associativity {
    /// Indeterminate if zero.
    pub fn ways_of_associativity(&self) -> usize {
        match self {
            Self::Disabled | Self::SeeLeaf0x8000001d | Self::FullyAssociative => 0,
            Self::DirectMapped => 1,
            Self::Ways2 => 2,
            Self::Ways3 => 3,
            Self::Ways4 => 4,
            Self::Ways6 => 6,
            Self::Ways8 => 8,
            Self::Ways16 => 16,
            Self::Ways32 => 32,
            Self::Ways48 => 48,
            Self::Ways64 => 64,
            Self::Ways96 => 96,
            Self::Ways128 => 128,
        }
    }

    /// Indeterminate if std::nullopt.
    pub fn fully_associative(&self) -> Option<bool> {
        match self {
            Self::Disabled => None,
            Self::FullyAssociative => Some(true),
            _ => Some(false),
        }
    }
}

/// Leaf/Function 0x8000_0006, ECX
///
/// [amd/vol3]: E.4.5  Function 8000_0006h—L2 Cache and TLB and L3 Cache Information.
pub const AMD_L2_CACHE_INFO: CpuidValue<0x8000_0006, 0, ECX, AmdL2CacheInformation> =
    CpuidValue::new();

layout!({
    /// The layout of ECX in [`AMD_L2_CACHE_INFO`].
    pub struct AmdL2CacheInformation(u32);
    {
        let size_kb @ 31..16;
        let assoc @ 15..12: AmdL2L3Associativity;
        let lines_per_tag @ 11..8;
        let line_size @ 7..0;
    }
});

impl AmdL2CacheInformation {
    pub fn ways_of_associativity(&self) -> usize {
        self.assoc().ways_of_associativity()
    }

    pub fn fully_associative(&self) -> Option<bool> {
        self.assoc().fully_associative()
    }
}

/// Leaf/Function 0x8000_0006, EDX
///
/// [amd/vol3]: E.4.5  Function 8000_0006h—L2 Cache and TLB and L3 Cache Information.
pub const AMD_L3_CACHE_INFO: CpuidValue<0x8000_0006, 0, EDX, AmdL3CacheInformation> =
    CpuidValue::new();

layout!({
    /// The layout of EDX in [`AMD_L3_CACHE_INFO`].
    pub struct AmdL3CacheInformation(u32);
    {
        let size @ 31..18;
        let __ @ 17..16;
        let assoc @ 15..12: AmdL2L3Associativity;
        let lines_per_tag @ 11..8;
        let line_size @ 7..0;
    }
});

impl AmdL3CacheInformation {
    pub fn ways_of_associativity(&self) -> usize {
        self.assoc().ways_of_associativity()
    }

    pub fn fully_associative(&self) -> Option<bool> {
        self.assoc().fully_associative()
    }
}

/// Topology level type for CPUID topology enumeration leaves.
#[bitfield_repr(u8)]
#[derive(Clone, Copy)]
pub enum TopologyLevelType {
    Invalid = 0,
    Smt = 1,
    Core = 2,
    Module = 3,
    Tile = 4,
    Die = 5,
}

layout!({
    /// The layout of EAX in topology enumeration leaves.
    pub struct TopologyEnumerationA(u32);
    {
        let __ @ 31..5;
        let next_level_apic_id_shift @ 4..0;
    }
});

layout!({
    /// The layout of EBX in topology enumeration leaves.
    pub struct TopologyEnumerationB(u32);
    {
        let __ @ 31..16;
        let num_logical_processors @ 15..0;
    }
});

layout!({
    /// The layout of ECX in topology enumeration leaves.
    pub struct TopologyEnumerationC(u32);
    {
        let __ @ 31..16;
        let level_type @ 15..8: TopologyLevelType;
        let level_number @ 7..0;
    }
});

layout!({
    /// The layout of EDX in topology enumeration leaves.
    pub struct TopologyEnumerationD(u32);
    {
        let x2apic_id @ 31..0;
    }
});

/// Leaf/Function 0xb (V1 Topology), EAX (subleaf 0)
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
pub const V1_TOPOLOGY_A: CpuidValue<0xb, 0, EAX, TopologyEnumerationA> = CpuidValue::new();

/// Leaf/Function 0xb (V1 Topology), EBX (subleaf 0)
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
pub const V1_TOPOLOGY_B: CpuidValue<0xb, 0, EBX, TopologyEnumerationB> = CpuidValue::new();

/// Leaf/Function 0xb (V1 Topology), ECX (subleaf 0)
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
pub const V1_TOPOLOGY_C: CpuidValue<0xb, 0, ECX, TopologyEnumerationC> = CpuidValue::new();

/// Leaf/Function 0xb (V1 Topology), EDX (subleaf 0)
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
pub const V1_TOPOLOGY_D: CpuidValue<0xb, 0, EDX, TopologyEnumerationD> = CpuidValue::new();

/// Leaf/Function 0x1f (V2 Topology), EAX (subleaf 0)
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
pub const V2_TOPOLOGY_A: CpuidValue<0x1f, 0, EAX, TopologyEnumerationA> = CpuidValue::new();

/// Leaf/Function 0x1f (V2 Topology), EBX (subleaf 0)
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
pub const V2_TOPOLOGY_B: CpuidValue<0x1f, 0, EBX, TopologyEnumerationB> = CpuidValue::new();

/// Leaf/Function 0x1f (V2 Topology), ECX (subleaf 0)
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
pub const V2_TOPOLOGY_C: CpuidValue<0x1f, 0, ECX, TopologyEnumerationC> = CpuidValue::new();

/// Leaf/Function 0x1f (V2 Topology), EDX (subleaf 0)
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
pub const V2_TOPOLOGY_D: CpuidValue<0x1f, 0, EDX, TopologyEnumerationD> = CpuidValue::new();

/// Leaf/Function 0x8000_0008, EAX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.4.7  Function 8000_0008h-Processor Capacity Parameters and
/// Extended Feature Identification.
pub const ADDRESS_SIZE_INFO: CpuidValue<0x8000_0008, 0, EAX, AddressSizeInfo> = CpuidValue::new();

layout!({
    /// The layout of EAX in [`ADDRESS_SIZE_INFO`].
    pub struct AddressSizeInfo(u32);
    {
        let __ @ 31..24;
        let guest_phys_addr_bits @ 23..16;
        let linear_addr_bits @ 15..8;
        let phys_addr_bits @ 7..0;
    }
});

/// Performance timestamp counter size.
#[bitfield_repr(u8)]
#[derive(Clone, Copy)]
pub enum PerfTimestampCounterSize {
    Bits40 = 0b00,
    Bits48 = 0b01,
    Bits56 = 0b10,
    Bits64 = 0b11,
}

/// Leaf/Function 0x8000_0008, ECX
///
/// [amd/vol3]: E.4.7  Function 8000_0008h—Processor Capacity Parameters and
/// Extended Feature Identification.
pub const EXTENDED_SIZE_INFO: CpuidValue<0x8000_0008, 0, ECX, ExtendedSizeInfo> = CpuidValue::new();

layout!({
    /// The layout of ECX in [`EXTENDED_SIZE_INFO`].
    pub struct ExtendedSizeInfo(u32);
    {
        let __ @ 31..18;
        let perf_tsc_size @ 17..16: PerfTimestampCounterSize;
        let apic_id_size @ 15..12;
        let __ @ 11..8;
        let nc @ 7..0;
    }
});

/// Leaf/Function 0x8000_0008, EBX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.4.7  Function 8000_0008h-Processor Capacity Parameters and Extended Feature
/// Identification.
pub const EXTENDED_AMD_FEATURES_B: CpuidValue<0x8000_0008, 0, EBX, ExtendedAmdFeaturesB> =
    CpuidValue::new();

layout!({
    /// Layout for [`EXTENDED_AMD_FEATURES_B`].
    ///
    /// [amd/ibc] details bits [18:14] and 12.
    /// [amd/ssbd] details bits [26:24].
    pub struct ExtendedAmdFeaturesB(u32);
    {
        let __ @ 31..27;
        let ssb_no @ 26;
        let virt_ssbd @ 25;
        let ssbd @ 24;
        let __ @ 23..19;
        let prefers_ibrs @ 18;
        let stibp_always_on @ 17;
        let ibrs_always_on @ 16;
        let stibp @ 15;
        let ibrs @ 14;
        let __ @ 13;
        let ibpb @ 12;
        let __ @ 11..10;
        let wbnoinvd @ 9;
        let mcommit @ 8;
        let __ @ 7..5;
        let rdpru @ 4;
        let __ @ 3;
        let rstr_fp_err_ptrs @ 2;
        let inst_ret_cnt_msr @ 1;
        let clzero @ 0;
    }
});

/// Leaf/Function 0x8000_001e, EAX
///
/// [amd/vol3]: E.4.16  Function 8000_001Eh—Processor Topology Information.
pub const EXTENDED_APIC_ID: CpuidValue<0x8000_001e, 0, EAX, ExtendedApicId> = CpuidValue::new();

layout!({
    /// The layout of EAX in [`EXTENDED_APIC_ID`].
    pub struct ExtendedApicId(u32);
    {
        let x2apic_id @ 31..0;
    }
});

/// Leaf/Function 0x8000_001e, EBX
///
/// [amd/vol3]: E.4.16  Function 8000_001Eh—Processor Topology Information.
pub const COMPUTE_UNIT_INFO: CpuidValue<0x8000_001e, 0, EBX, ComputeUnitInfo> = CpuidValue::new();

layout!({
    /// The layout of EBX in [`COMPUTE_UNIT_INFO`].
    pub struct ComputeUnitInfo(u32);
    {
        let __ @ 31..16;
        let threads_per_compute_unit @ 15..8;
        let compute_unit_id @ 7..0;
    }
});

/// Leaf/Function 0x8000_001e, ECX
///
/// [amd/vol3]: E.4.16  Function 8000_001Eh—Processor Topology Information.
pub const NODE_INFO: CpuidValue<0x8000_001e, 0, ECX, NodeInfo> = CpuidValue::new();

layout!({
    /// The layout of ECX in [`NODE_INFO`].
    pub struct NodeInfo(u32);
    {
        let __ @ 31..11;
        let nodes_per_package @ 10..8;
        let node_id @ 7..0;
    }
});

/// Leaf/Function 0x1, EAX.
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.2  Function 1h-Processor and Processor Feature Identifiers
/// [intel/vol2]: Figure 3-6.  Version Information Returned by CPUID in EAX.
/// [amd/vol3]: E.3.2, CPUID Fn0000_0001_EAX  Family, Model, Stepping Identifiers.
pub const VERSION_INFO: CpuidValue<0x1, 0x0, EAX, VersionInfo> = CpuidValue::new();

/// Encoding for intel processor type in [`VersionInfo`].
///
/// [intel/vol2]: Table 3-9.  Processor Type Field.
#[bitfield_repr(u8)]
#[derive(Clone, Copy)]
pub enum IntelProcessorType {
    OriginalOem = 0b00,
    IntelOverdrive = 0b01,
    Dual = 0b10,
    Reserved = 0b11,
}

layout!({
    /// The layout of EAX in [`VERSION_INFO`].
    pub struct VersionInfo(u32);
    {
        let __ @ 31..28;
        let extended_family @ 27..20;
        let extended_model @ 19..16;
        let __ @ 15..14;
        let intel_processor @ 13..12: IntelProcessorType;
        let base_family @ 11..8;
        let base_model @ 7..4;
        let stepping @ 3..0;
    }
});

impl VersionInfo {
    pub fn family(self) -> u8 {
        if self.base_family() == 0xf {
            self.base_family() + self.extended_family()
        } else {
            self.base_family()
        }
    }

    pub fn model(self) -> u8 {
        if self.base_family() == 0x6 || self.base_family() == 0xf {
            (self.extended_model() << 4) | self.base_model()
        } else {
            self.base_model()
        }
    }

    /// Attempts to derives the microarchitecture with the assumption that the
    /// system relates to a particular vendor.
    pub fn microarchitecture(self, vendor: Vendor) -> Microarchitecture {
        // TODO(https://fxbug.dev/42138852): check in a source of truth for this information and
        // refer to that here.
        match vendor {
            Vendor::Intel => {
                // Table largely from https://en.wikichip.org/wiki/intel/cpuid
                match self.family() {
                    0x6 => match self.model() {
                        // Big cores
                        0x0f | // Merom
                        0x16 | // Merom L
                        0x17 | // Penryn, Wolfdale, Yorkfield, Harpertown, QC
                        0x1d   // Dunnington
                            => Microarchitecture::IntelCore2,
                        0x1a | // Bloomfield, EP, WS
                        0x1e | // Lynnfield, Clarksfield
                        0x1f | // Auburndale, Havendale
                        0x2e   // EX
                            => Microarchitecture::IntelNehalem,
                        0x25 | // Arrandale, Clarkdale
                        0x2c | // Gulftown, EP
                        0x2f   // EX
                            => Microarchitecture::IntelWestmere,
                        0x2a | // M, H
                        0x2d   // E, EN, EP
                            => Microarchitecture::IntelSandyBridge,
                        0x3a | // M, H, Gladden
                        0x3e   // E, EN, EP, EX
                            => Microarchitecture::IntelIvyBridge,
                        0x3c | // S
                        0x3f | // E, EP, EX
                        0x45 | // ULT
                        0x46   // GT3E
                            => Microarchitecture::IntelHaswell,
                        0x3d | // U, Y, S
                        0x47 | // H, C, W
                        0x56 | // DE, Hewitt Lake
                        0x4f   // E, EP, EX
                            => Microarchitecture::IntelBroadwell,
                        0x4e | // Skylake Y, U
                        0x5e | // Skylake DT, H, S
                        0x8e | // Kaby Lake Y, U, Coffee Lake U;
                               // Whiskey Lake U; Amber Lake Y; Comet Lake U
                        0x9e | // Kaby Lake T, H, S, X, Coffee Lake S, H, E
                        0xa5   // Comet Lake S, H
                            => Microarchitecture::IntelSkylake,
                        0x55   // Skylake SP, X, DE, W, Cascade Lake SP, X, W; Cooper Lake
                            => Microarchitecture::IntelSkylakeServer,
                        0x66   // Cannon Lake U
                            => Microarchitecture::IntelCannonLake,
                        0x6a | // Ice Lake Server SP
                        0x6c | // Ice Lake Server DE
                        0x7d | // Ice Lake Y
                        0x7e   // Ice Lake U
                            => Microarchitecture::IntelIceLake,
                        0x8c | // Tiger Lake UP
                        0x8d   // Tiger Lake H
                            => Microarchitecture::IntelTigerLake,
                        0x97 | // Alder Lake S
                        0x9a   // Alder Lake H, P, U
                            => Microarchitecture::IntelAlderLake,
                        0xb7   // Raptor Lake S
                            => Microarchitecture::IntelRaptorLake,

                        // Small cores
                        0x1c | // Silverthorne, Diamondville, Pineview
                        0x26   // Lincroft
                            => Microarchitecture::IntelBonnell,
                        0x27 | // Penwell
                        0x35 | // Cloverview
                        0x36   // Cedarview
                            => Microarchitecture::IntelSaltwell,
                        0x37 | // Bay Trail
                        0x4a | // Tangier
                        0x4d | // Avoton, Rangeley
                        0x5a | // Anniedale
                        0x5d   // SoFIA
                            => Microarchitecture::IntelSilvermont,
                        0x4c   // Cherry Trail, Braswell
                            => Microarchitecture::IntelAirmont,
                        0x5c | // Apollo Lake, Broxton
                        0x5f   // Denverton
                            => Microarchitecture::IntelGoldmont,
                        0x7a   // Gemini Lake
                            => Microarchitecture::IntelGoldmontPlus,
                        0x8a | // Lakefield
                        0x96 | // Elkhart Lake
                        0x9c   // Jasper Lake
                            => Microarchitecture::IntelTremont,
                        _ => Microarchitecture::Unknown,
                    },
                    _ => Microarchitecture::Unknown,
                }
            }
            Vendor::Amd => {
                // Table largely from https://en.wikichip.org/wiki/amd/cpuid
                match self.family() {
                    0x15 // Bulldozer/Piledriver/Steamroller/Excavator
                        => Microarchitecture::AmdFamilyBulldozer,
                    0x16 // Jaguar
                        => Microarchitecture::AmdFamilyJaguar,
                    0x17 // Zen 1 - 2
                        => Microarchitecture::AmdFamilyZen,
                    0x19 // Zen 3 - 4
                        => Microarchitecture::AmdFamilyZen3,
                    _ => Microarchitecture::Unknown,
                }
            }
            Vendor::Unknown => Microarchitecture::Unknown,
        }
    }
}

/// The list is not exhaustive and is in chronological order within groupings.
/// Microarchictectures that use the same processor (and, say, differ only in
/// performance or SoC composition) are regarded as equivalent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Microarchitecture {
    Unknown,

    // Intel Core family (64-bit, display family 0x6).
    IntelCore2,
    IntelNehalem,
    IntelWestmere,
    IntelSandyBridge,
    IntelIvyBridge,
    IntelHaswell,
    IntelBroadwell,
    /// Includes Kaby/Coffee/Whiskey/Amber/Comet Lake.
    IntelSkylake,
    /// Includes Cascade/Cooper Lake.
    IntelSkylakeServer,
    /// A 10nm prototype only ever released on the Intel Core i3-8121U.
    IntelCannonLake,
    IntelIceLake,
    IntelTigerLake,
    IntelAlderLake,
    IntelRaptorLake,

    // Intel Atom family.
    IntelBonnell,
    IntelSaltwell,
    IntelSilvermont,
    IntelAirmont,
    IntelGoldmont,
    IntelGoldmontPlus,
    IntelTremont,

    // AMD families.
    /// Bulldozer/Piledriver/Steamroller/Excavator.
    AmdFamilyBulldozer,
    /// Jaguar.
    AmdFamilyJaguar,
    /// Zen 1, 1+, 2.
    AmdFamilyZen,
    /// Zen 3, 4.
    AmdFamilyZen3,
}

impl Microarchitecture {
    /// Returns the human-readable microarchitecture name (`arch::ToString(Microarchitecture)`).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::IntelCore2 => "Intel Core 2",
            Self::IntelNehalem => "Intel Nehalem",
            Self::IntelWestmere => "Intel Westmere",
            Self::IntelSandyBridge => "Intel Sandy Bridge",
            Self::IntelIvyBridge => "Intel Ivy Bridge",
            Self::IntelBroadwell => "Intel Broadwell",
            Self::IntelHaswell => "Intel Haswell",
            Self::IntelSkylake => "Intel Skylake",
            Self::IntelSkylakeServer => "Intel Skylake (server)",
            Self::IntelCannonLake => "Intel Cannon Lake",
            Self::IntelIceLake => "Intel Ice Lake",
            Self::IntelTigerLake => "Intel Tiger Lake",
            Self::IntelAlderLake => "Intel Alder Lake",
            Self::IntelRaptorLake => "Intel Raptor Lake",
            Self::IntelBonnell => "Intel Bonnell",
            Self::IntelSaltwell => "Intel Saltwell",
            Self::IntelSilvermont => "Intel Silvermont",
            Self::IntelAirmont => "Intel Airmont",
            Self::IntelGoldmont => "Intel Goldmont",
            Self::IntelGoldmontPlus => "Intel Goldmont Plus",
            Self::IntelTremont => "Intel Tremont",
            Self::AmdFamilyBulldozer => "AMD Bulldozer",
            Self::AmdFamilyJaguar => "AMD Jaguar",
            Self::AmdFamilyZen => "AMD Zen 1-2",
            Self::AmdFamilyZen3 => "AMD Zen 3-4",
        }
    }
}

impl fmt::Display for Microarchitecture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Leaf/Function 0x6, EAX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.3.5  Function 6h--Power Management Related Features.
pub const THERMAL_AND_POWER_FEATURE_FLAGS_A: CpuidValue<
    0x6,
    0x0,
    EAX,
    ThermalAndPowerFeatureFlagsA,
> = CpuidValue::new();

layout!({
    /// Layout for [`THERMAL_AND_POWER_FEATURE_FLAGS_A`].
    pub struct ThermalAndPowerFeatureFlagsA(u32);
    {
        // Bits [31:21] are reserved.
        let __ @ 31..21;
        let ignoring_idle_logical_processor_hwp @ 20;
        let hw_feedback @ 19;
        let fast_access_mode @ 18;
        let flexible_hwp @ 17;
        let hwp_peci_override @ 16;
        let hwp_capabilities @ 15;
        let turbo_max @ 14;
        let hdc @ 13;
        // Bit 12 is reserved.
        let __ @ 12;
        let hwp_package_level_request @ 11;
        let hwp_epp @ 10;
        let hwp_activity_window @ 9;
        let hwp_notification @ 8;
        let hwp @ 7;
        let ptm @ 6;
        let ecmd @ 5;
        let pln @ 4;
        // Bit 3 is reserved.
        let __ @ 3;
        let arat @ 2;
        let turbo @ 1;
        let digital_temperature_sensor @ 0;
    }
});

/// Leaf/Function 0xa, EAX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
pub const PERFORMANCE_MONITORING_A: CpuidValue<0xa, 0x0, EAX, PerformanceMonitoringA> =
    CpuidValue::new();

layout!({
    /// Layout for [`PERFORMANCE_MONITORING_A`].
    pub struct PerformanceMonitoringA(u32);
    {
        let ebx_vector_length @ 31..24;
        let general_counter_width @ 23..16;
        let num_general_counters @ 15..8;
        let version @ 7..0;
    }
});

/// Leaf/Function 0xa, EDX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
pub const PERFORMANCE_MONITORING_D: CpuidValue<0xa, 0x0, EDX, PerformanceMonitoringD> =
    CpuidValue::new();

layout!({
    /// Layout for [`PERFORMANCE_MONITORING_D`].
    pub struct PerformanceMonitoringD(u32);
    {
        let __ @ 31..16;
        let anythread_deprecation @ 15;
        let __ @ 14..13;
        let fixed_counter_width @ 12..5;
        let num_fixed_counters @ 4..0;
    }
});

/// Leaf/Function 0x4000_0000, EAX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
pub const MAX_HYPERVISOR_LEAF: CpuidValue<0x4000_0000, 0x0, EAX, u32> = CpuidValue::new();

/// Leaf/Function 0x8000_0000, EAX
///
/// [intel/vol2]: Table 3-8.  Information Returned by CPUID Instruction.
/// [amd/vol3]: E.4.1  Function 8000_0000h--Maximum Extended Function Number and Vendor String.
pub const MAX_EXTENDED_LEAF: CpuidValue<0x8000_0000, 0x0, EAX, u32> = CpuidValue::new();

/// Leaf/Function 0x8000_0007, EDX
///
/// [amd/vol3]: E.4.6  Function 8000_0007h--Processor Power Management and RAS Capabilities.
pub const ADVANCED_POWER_FEATURE_FLAGS: CpuidValue<
    0x8000_0007,
    0x0,
    EDX,
    AdvancedPowerFeatureFlags,
> = CpuidValue::new();

layout!({
    /// Layout for [`ADVANCED_POWER_FEATURE_FLAGS`].
    pub struct AdvancedPowerFeatureFlags(u32);
    {
        // Bits [31:13] are reserved.
        let __ @ 31..13;
        let proc_power_reporting @ 12;
        let proc_feedback_interface @ 11;
        let eff_freq @ 10;
        let cpb @ 9;
        let tsc_invariant @ 8;
        let hw_pstate @ 7;
        let has_100mhz_steps @ 6;
        // Bit 5 is reserved.
        let __ @ 5;
        let tm @ 4;
        let ttp @ 3;
        let vid @ 2;
        let fid @ 1;
        let ts @ 0;
    }
});

/// Holds the 12-byte hypervisor vendor string (`arch::HypervisorName`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HypervisorName([u8; 12]);

impl HypervisorName {
    /// Reads the hypervisor vendor string from CPUID leaf `0x4000_0000` if
    /// `FEATURE_FLAGS_C.hypervisor` is set.
    pub fn from_cpuid(io: &impl Cpuid) -> Self {
        // Check if we are actually within a hypervisor.
        if io.read(FEATURE_FLAGS_C).hypervisor() {
            let raw = io.read_raw(0x4000_0000, 0);
            let ebx = raw.ebx.to_le_bytes();
            let ecx = raw.ecx.to_le_bytes();
            let edx = raw.edx.to_le_bytes();
            Self([
                ebx[0], ebx[1], ebx[2], ebx[3], //
                ecx[0], ecx[1], ecx[2], ecx[3], //
                edx[0], edx[1], edx[2], edx[3], //
            ])
        } else {
            Self([0u8; 12])
        }
    }

    /// Returns a string representation of name of the hypervisor, valid for as long as the
    /// associated `HypervisorName` is in scope.
    pub fn name(&self) -> &str {
        nul_trimmed_str(&self.0)
    }
}

/// Holds the 48-byte processor brand string (`arch::ProcessorName`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessorName([u8; 48]);

impl ProcessorName {
    /// Reads the processor brand string from CPUID leaves `0x8000_0002..=0x8000_0004`.
    pub fn from_cpuid(io: &impl Cpuid) -> Self {
        // The name string needs leaves 0x8000_0002-0x8000_0004.
        let mut buf = [0u8; 48];
        if io.read(MAX_EXTENDED_LEAF) >= 0x8000_0004 {
            for (i, leaf) in (0x8000_0002..=0x8000_0004).enumerate() {
                let raw = io.read_raw(leaf, 0);
                let chunk = &mut buf[i * 16..(i + 1) * 16];
                chunk[0..4].copy_from_slice(&raw.eax.to_le_bytes());
                chunk[4..8].copy_from_slice(&raw.ebx.to_le_bytes());
                chunk[8..12].copy_from_slice(&raw.ecx.to_le_bytes());
                chunk[12..16].copy_from_slice(&raw.edx.to_le_bytes());
            }
        }
        Self(buf)
    }

    /// Returns a string representation of name of the processor, valid for as long as the
    /// associated `ProcessorName` is in scope.
    pub fn name(&self) -> &str {
        nul_trimmed_str(&self.0)
    }
}

/// Returns `bytes` up to the first NUL, truncated to its longest valid UTF-8 prefix.
fn nul_trimmed_str(bytes: &[u8]) -> &str {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let bytes = &bytes[..len];
    match str::from_utf8(bytes) {
        Ok(s) => s,
        // `valid_up_to()` marks the end of the longest valid UTF-8 prefix, which always parses.
        Err(e) => str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use regio::testing::x86::FakeCpuid;

    use super::*;

    // Basic exercise of the above utilities.
    #[cfg(target_arch = "x86")]
    #[test]
    fn feature_detection() {
        use regio::x86::DirectCpuid;

        let vendor = VendorString::from_cpuid(DirectCpuid {});
        println!("Vendor string: {}", vendor.as_str().unwrap_or("invalid vendor string"));
    }

    #[test]
    fn amd_vendor_string() {
        // EBX, ECX, EDX copied verbatim from the manual.
        let mut cpuid = FakeCpuid::new();
        cpuid
            .set(VENDOR_STRING_B, 0x6874_7541)
            .set(VENDOR_STRING_C, 0x444d_4163)
            .set(VENDOR_STRING_D, 0x6974_6e65);
        let vendor_str = VendorString::from_cpuid(&cpuid);
        assert_eq!(vendor_str, VendorString::AMD);
    }

    #[test]
    fn address_size_info() {
        let info = AddressSizeInfo::from(0x002b_3027);
        assert_eq!(info.phys_addr_bits(), 0x27);
        assert_eq!(info.linear_addr_bits(), 0x30);
        assert_eq!(info.guest_phys_addr_bits(), 0x2b);
    }

    #[test]
    fn hypervisor_and_processor_names() {
        const HYP_B: CpuidValue<0x4000_0000, 0x0, EBX, u32> = CpuidValue::new();
        const HYP_C: CpuidValue<0x4000_0000, 0x0, ECX, u32> = CpuidValue::new();
        const HYP_D: CpuidValue<0x4000_0000, 0x0, EDX, u32> = CpuidValue::new();
        const BRAND_2A: CpuidValue<0x8000_0002, 0x0, EAX, u32> = CpuidValue::new();
        const BRAND_2B: CpuidValue<0x8000_0002, 0x0, EBX, u32> = CpuidValue::new();

        let mut cpuid = FakeCpuid::new();
        assert_eq!(HypervisorName::from_cpuid(&cpuid).name(), "");
        assert_eq!(ProcessorName::from_cpuid(&cpuid).name(), "");

        let mut feat_c = FeatureFlagsC::from(0);
        feat_c.set_hypervisor(true);
        cpuid
            .set(FEATURE_FLAGS_C, feat_c)
            .set(HYP_B, u32::from_le_bytes(*b"KVMK"))
            .set(HYP_C, u32::from_le_bytes(*b"VMKV"))
            .set(HYP_D, u32::from_le_bytes(*b"M\0\0\0"))
            .set(MAX_EXTENDED_LEAF, 0x8000_0004)
            .set(BRAND_2A, u32::from_le_bytes(*b"Zirc"))
            .set(BRAND_2B, u32::from_le_bytes(*b"on\0\0"));

        assert_eq!(HypervisorName::from_cpuid(&cpuid).name(), "KVMKVMKVM");
        assert_eq!(ProcessorName::from_cpuid(&cpuid).name(), "Zircon");
        assert_eq!(CacheType::Unified.as_str(), "Unified");
        assert_eq!(Microarchitecture::IntelSkylake.as_str(), "Intel Skylake");
    }
}
