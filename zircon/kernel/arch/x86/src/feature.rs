// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::arch::x86_64::{__cpuid, __cpuid_count, CpuidResult};
use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use boot_options::BootOptions;
use debug::ltracef;
use kprint::{kprint, kprintln};
use libarch::x86::cpuid::{
    EXTENDED_FEATURES_B, EXTENDED_FEATURES_D, FEATURE_FLAGS_C, FEATURE_FLAGS_D, HypervisorName,
    PERFORMANCE_MONITORING_A, PERFORMANCE_MONITORING_D, ProcessorName, VERSION_INFO, VendorString,
};
use libarch::x86::{
    AMD_LOAD_STORE_CONFIGURATION, CpuCacheInfo, IA32_ARCH_CAPABILITIES, IA32_MISC_ENABLE,
    IA32_TSX_CTRL, MSRC001_0015, SPECULATION_CONTROL, SpectreV2Mitigation, SpeculationControlMsr,
    VIRTUAL_SPECULATION_CONTROL, can_mitigate_x86_ssb_bug, disable_tsx, enable_ibrs, enable_stibp,
    get_preferred_spectre_v2_mitigation, has_ibpb, has_ibrs, has_stibp, has_x86_l1tf_bug,
    has_x86_mds_taa_bugs, has_x86_meltdown_bug, has_x86_ssb_bug, has_x86_swapgs_bug,
    mitigate_x86_ssb_bug, set_x86_cpu_turbo_state,
};
use regio::x86::{Cpuid, CpuidRawResult, DirectCpuid};
pub use x86_feature_bindings::{
    x86_get_timer_freq_func_t as X86GetTimerFreqFunc, x86_idle_state_t, x86_idle_states_t,
    x86_microarch_config_t as X86MicroarchConfig, x86_microarch_list as X86MicroarchList,
    x86_reboot_reason_func_t as X86RebootReasonFunc,
    x86_reboot_system_func_t as X86RebootSystemFunc, x86_vendor_list as X86VendorList,
};
use x86_idle_states_bindings::kX86IdleStateMaskC1Only;
use zx_status::Status;
use zx_types::zx_status_t;

use super::cpuid::{CpuId, Manufacturer, ManufacturerInfo, ProcessorId, RealCpuId};
use super::handoff_ffi::InitializeBootCpuid;
use super::idle_states::x86_cstate_c1;
use super::platform_access::RealMsrAccess;
use super::registers::{X86_MSR_IA32_FLUSH_CMD, X86_MSR_IA32_PRED_CMD};
pub use super::timer_freq::{x86_lookup_core_crystal_freq, x86_lookup_tsc_freq};
use super::x86::{inp, outp, read_msr_safe, write_msr};
use crate::kernel::auto_preempt_disabler::AutoPreemptDisabler;

const LOCAL_TRACE: u32 = 0;

/// Maximum supported standard CPUID leaf (`MAX_SUPPORTED_CPUID`).
pub const MAX_SUPPORTED_CPUID: u32 = x86_feature_bindings::MAX_SUPPORTED_CPUID;
/// Maximum supported hypervisor CPUID leaf (`MAX_SUPPORTED_CPUID_HYP`).
pub const MAX_SUPPORTED_CPUID_HYP: u32 = x86_feature_bindings::MAX_SUPPORTED_CPUID_HYP;
/// Maximum supported extended CPUID leaf (`MAX_SUPPORTED_CPUID_EXT`).
pub const MAX_SUPPORTED_CPUID_EXT: u32 = x86_feature_bindings::MAX_SUPPORTED_CPUID_EXT;

/// Maximum number of supported C-states (`X86_MAX_CSTATES`).
pub const X86_MAX_CSTATES: usize = x86_idle_states_bindings::X86_MAX_CSTATES as usize;

const CPUID_LEN: usize = (MAX_SUPPORTED_CPUID + 1) as usize;
const CPUID_HYP_LEN: usize =
    (MAX_SUPPORTED_CPUID_HYP - X86CpuidLeafNum::HypVendor as u32 + 1) as usize;
const CPUID_EXT_LEN: usize =
    (MAX_SUPPORTED_CPUID_EXT - X86CpuidLeafNum::ExtBase as u32 + 1) as usize;

const EMPTY_CPUID_LEAF: CpuidLeaf = CpuidLeaf { a: 0, b: 0, c: 0, d: 0 };

/// Wrapper around `UnsafeCell<T>` for globals written exclusively on the boot CPU during
/// single-threaded early boot (`early_init_percpu` / `cpu_feature_init`) prior to SMP bringup,
/// and read-only thereafter (except leaf `0x6` of `CPUID`, whose 32-bit registers are accessed via
/// `AtomicU32::from_ptr` during `cpu_feature_late_init_percpu`).
///
/// Statics exported to C++ and assembly via `#[unsafe(export_name)]` follow the same protocol: C++
/// (`arch/x86/feature.h`) and assembly only read them after the single-threaded boot writes, never
/// store to them, and access `_cpuid[6]` exclusively through `ktl::atomic_ref<uint32_t>`.
#[repr(transparent)]
struct BootStatic<T>(UnsafeCell<T>);

impl<T> BootStatic<T> {
    const fn new(val: T) -> Self {
        Self(UnsafeCell::new(val))
    }

    #[inline(always)]
    const fn get(&self) -> *mut T {
        self.0.get()
    }
}

// SAFETY: `BootStatic<T>` is a module-private wrapper whose only `&self` method (`get`) returns
// `*mut T` without dereferencing it, so safe code cannot perform unsynchronized reads or writes
// through `&BootStatic<T>`. All `unsafe` dereferences in this module uphold single-threaded
// early-boot initialization before SMP bringup (or `AtomicU32` synchronization for `CPUID` leaf
// `0x6`). `T: Send` permits values written on CPU 0 to be read by value on other CPUs, and
// `T: Sync` permits shared references formed after initialization to be used across threads.
unsafe impl<T: Send + Sync> Sync for BootStatic<T> {}

// SAFETY: `_cpuid` is a globally unique linker symbol matching the `extern "C"` array declaration
// in `arch/x86/feature.h` with identical `#[repr(C)]` element layout.
#[unsafe(export_name = "_cpuid")]
static CPUID: BootStatic<[CpuidLeaf; CPUID_LEN]> = BootStatic::new([EMPTY_CPUID_LEAF; CPUID_LEN]);

// SAFETY: `_cpuid_hyp` is a globally unique linker symbol matching the `extern "C"` array
// declaration in `arch/x86/feature.h` with identical `#[repr(C)]` element layout.
#[unsafe(export_name = "_cpuid_hyp")]
static CPUID_HYP: BootStatic<[CpuidLeaf; CPUID_HYP_LEN]> =
    BootStatic::new([EMPTY_CPUID_LEAF; CPUID_HYP_LEN]);

// SAFETY: `_cpuid_ext` is a globally unique linker symbol matching the `extern "C"` array
// declaration in `arch/x86/feature.h` with identical `#[repr(C)]` element layout.
#[unsafe(export_name = "_cpuid_ext")]
static CPUID_EXT: BootStatic<[CpuidLeaf; CPUID_EXT_LEN]> =
    BootStatic::new([EMPTY_CPUID_LEAF; CPUID_EXT_LEN]);

// SAFETY: `max_cpuid` is a globally unique linker symbol matching `extern uint32_t max_cpuid` in
// `arch/x86/feature.h`.
#[unsafe(export_name = "max_cpuid")]
static MAX_CPUID: BootStatic<u32> = BootStatic::new(0);

// SAFETY: `max_hyp_cpuid` is a globally unique linker symbol matching
// `extern uint32_t max_hyp_cpuid` in `arch/x86/feature.h`.
#[unsafe(export_name = "max_hyp_cpuid")]
static MAX_HYP_CPUID: BootStatic<u32> = BootStatic::new(0);

// SAFETY: `max_ext_cpuid` is a globally unique linker symbol matching
// `extern uint32_t max_ext_cpuid` in `arch/x86/feature.h`.
#[unsafe(export_name = "max_ext_cpuid")]
static MAX_EXT_CPUID: BootStatic<u32> = BootStatic::new(0);

// SAFETY: `x86_vendor` is a globally unique linker symbol matching
// `extern enum x86_vendor_list x86_vendor` in `arch/x86/feature.h`; C++ only reads it, so it always
// holds a valid `X86VendorList` discriminant written by Rust.
#[unsafe(export_name = "x86_vendor")]
static X86_VENDOR: BootStatic<X86VendorList> = BootStatic::new(X86VendorList::X86_VENDOR_UNKNOWN);

/// Microarchitecture configuration selected for the boot CPU (`x86_microarch_config`).
///
/// Safety invariant: When `Some(cfg)`, `cfg` points to one of the static `X86MicroarchConfig`
/// tables in this module matching the boot CPU: `cfg.get_apic_freq` has no preconditions;
/// `cfg.get_tsc_freq` has no preconditions beyond running in the kernel (CPL 0) with `%gs.base`
/// initialized and is safe to call from any thread context (`zen_tsc_freq` validates its own
/// execution context at runtime); `cfg.reboot_system` and `cfg.reboot_reason` require CPL 0 while
/// resetting the system;
/// and `cfg.idle_states.states` contains a terminating base `C1` state with every populated entry
/// up to and including base `C1` having a `name` pointer that is either null or a valid
/// NUL-terminated `'static` C string.
// SAFETY: `x86_microarch_config` is a globally unique linker symbol matching
// `extern const x86_microarch_config_t* x86_microarch_config` in `arch/x86/feature.h` (verified by
// `static_assert!` below to match pointer size and alignment via null-pointer optimization).
#[unsafe(export_name = "x86_microarch_config")]
static X86_MICROARCH_CONFIG: BootStatic<Option<&'static X86MicroarchConfig>> =
    BootStatic::new(None);

zr::static_assert!(
    core::mem::size_of::<Option<&'static X86MicroarchConfig>>()
        == core::mem::size_of::<*const X86MicroarchConfig>()
);
zr::static_assert!(
    core::mem::align_of::<Option<&'static X86MicroarchConfig>>()
        == core::mem::align_of::<*const X86MicroarchConfig>()
);

static MODEL_INFO: BootStatic<X86ModelInfo> = BootStatic::new(X86ModelInfo {
    processor_type: 0,
    family: 0,
    model: 0,
    stepping: 0,
    display_family: 0,
    display_model: 0,
    patch_level: 0,
});

// Pre-computed complex features to test for or features that are tested extremely
// regularly in the system.

/// Whether the FSGSBASE instructions are supported.
static G_X86_FEATURE_FSGSBASE: BootStatic<bool> = BootStatic::new(false);

// SAFETY: `g_x86_feature_invpcid` is a globally unique linker symbol matching `extern bool` in
// `arch/x86/feature.h`.
#[unsafe(export_name = "g_x86_feature_invpcid")]
static G_X86_FEATURE_INVPCID: BootStatic<bool> = BootStatic::new(false);

/// Combination of both PCID & INVPCID features present, and enabled by kernel cmdline option.
// SAFETY: `g_x86_feature_pcid_enabled` is a globally unique linker symbol matching `extern bool`
// in `arch/x86/feature.h`.
#[unsafe(export_name = "g_x86_feature_pcid_enabled")]
static G_X86_FEATURE_PCID_ENABLED: BootStatic<bool> = BootStatic::new(false);

// SAFETY: `g_x86_feature_has_smap` is a globally unique linker symbol matching `extern bool` in
// `arch/x86/feature.h`.
#[unsafe(export_name = "g_x86_feature_has_smap")]
static G_X86_FEATURE_HAS_SMAP: BootStatic<bool> = BootStatic::new(false);

static G_HAS_MELTDOWN: BootStatic<bool> = BootStatic::new(false);
static G_HAS_L1TF: BootStatic<bool> = BootStatic::new(false);

// SAFETY: `g_l1d_flush_on_vmentry` is a globally unique linker symbol matching `extern bool` in
// `arch/x86/feature.h`.
#[unsafe(export_name = "g_l1d_flush_on_vmentry")]
static G_L1D_FLUSH_ON_VMENTRY: BootStatic<bool> = BootStatic::new(false);

static G_HAS_MDS_TAA: BootStatic<bool> = BootStatic::new(false);
static G_HAS_SWAPGS_BUG: BootStatic<bool> = BootStatic::new(false);
static G_HAS_SSB: BootStatic<bool> = BootStatic::new(false);
static G_HAS_SSBD: BootStatic<bool> = BootStatic::new(false);
static G_SSB_MITIGATED: BootStatic<bool> = BootStatic::new(false);
static G_HAS_MD_CLEAR: BootStatic<bool> = BootStatic::new(false);

// SAFETY: `g_md_clear_on_user_return` is a globally unique linker symbol matching `extern bool` in
// `arch/x86/feature.h`.
#[unsafe(export_name = "g_md_clear_on_user_return")]
static G_MD_CLEAR_ON_USER_RETURN: BootStatic<bool> = BootStatic::new(false);

static G_HAS_SPEC_CTRL: BootStatic<bool> = BootStatic::new(false);

// SAFETY: `g_has_ibpb` is a globally unique linker symbol matching `extern bool` in
// `arch/x86/feature.h`.
#[unsafe(export_name = "g_has_ibpb")]
static G_HAS_IBPB: BootStatic<bool> = BootStatic::new(false);

static G_SHOULD_IBPB_ON_CTXT_SWITCH: BootStatic<bool> = BootStatic::new(false);
static G_RAS_FILL_ON_CTXT_SWITCH: BootStatic<bool> = BootStatic::new(false);

// SAFETY: `g_cpu_vulnerable_to_rsb_underflow` is a globally unique linker symbol matching
// `extern bool` in `arch/x86/feature.h`.
#[unsafe(export_name = "g_cpu_vulnerable_to_rsb_underflow")]
static G_CPU_VULNERABLE_TO_RSB_UNDERFLOW: BootStatic<bool> = BootStatic::new(false);

/// Mutated on each CPU during `cpu_feature_late_init_percpu()` (after secondary CPUs have been
/// brought online), while idle threads read it via `x86_cpu_vulnerable_to_rsb_cross_thread()`.
/// Accessed with `Ordering::Relaxed` because it is a monotonic `false -> true` latch guarding only
/// CPU-local RSB overwrite (`x86_ras_fill`) without publishing other shared memory state.
// SAFETY: `g_cpu_vulnerable_to_rsb_cross_thread` is a globally unique linker symbol matching
// `extern ktl::atomic<bool> g_cpu_vulnerable_to_rsb_cross_thread` in `arch/x86/feature.h`.
#[unsafe(export_name = "g_cpu_vulnerable_to_rsb_cross_thread")]
static G_CPU_VULNERABLE_TO_RSB_CROSS_THREAD: AtomicBool = AtomicBool::new(false);

zr::static_assert!(core::mem::size_of::<AtomicBool>() == core::mem::size_of::<bool>());
zr::static_assert!(core::mem::align_of::<AtomicBool>() == core::mem::align_of::<bool>());

static G_HAS_ENHANCED_IBRS: BootStatic<bool> = BootStatic::new(false);
static G_HAS_RETBLEED: BootStatic<bool> = BootStatic::new(false);

/// Mutated across CPUs during `cpu_feature_late_init_percpu()`. Accessed with `Ordering::Relaxed`
/// because it is a standalone monotonic `false -> true` diagnostic flag with no dependent memory
/// accesses.
static G_STIBP_ENABLED: AtomicBool = AtomicBool::new(false);

static X86_HYPERVISOR: BootStatic<X86HypervisorList> = BootStatic::new(X86HypervisorList::Unknown);

// SAFETY: `g_hypervisor_has_pv_clock` is a globally unique linker symbol matching `extern bool` in
// `arch/x86/feature.h`.
#[unsafe(export_name = "g_hypervisor_has_pv_clock")]
static HYPERVISOR_HAS_PV_CLOCK: BootStatic<bool> = BootStatic::new(false);

static HYPERVISOR_HAS_PV_EOI: BootStatic<bool> = BootStatic::new(false);
static HYPERVISOR_HAS_PV_IPI: BootStatic<bool> = BootStatic::new(false);

/// Atomically swapped from `false` to `true` on CPU 0 in `early_init_percpu()`, ensuring only the
/// boot CPU populates `CPUID*`, `X86_VENDOR`, `MODEL_INFO`, and `X86_HYPERVISOR` while secondary
/// CPUs return immediately.
static G_CPUID_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Set to `true` on CPU 0 once `CPUID`, `CPUID_EXT`, and `CPUID_HYP` are populated in
/// `early_init_percpu()`.
static G_CPUID_READY: BootStatic<bool> = BootStatic::new(false);

macro_rules! x86_cpuid_bit {
    ($name:ident, $leaf:ident, $word:expr, $bit:expr) => {
        /// CPUID feature bit descriptor.
        pub const $name: X86CpuidBit =
            X86CpuidBit { leaf_num: X86CpuidLeafNum::$leaf as u32, word: $word, bit: $bit };
    };
}

/// Registers returned by a CPUID leaf (`cpuid_leaf`).
pub type CpuidLeaf = x86_feature_bindings::cpuid_leaf;

// `AtomicU32::from_ptr` on the fields of `CPUID` leaf `0x6` requires 4-byte alignment.
zr::static_assert!(core::mem::align_of::<CpuidLeaf>() == core::mem::align_of::<AtomicU32>());

/// Converts `CPUID` instruction output into a cached [`CpuidLeaf`].
#[inline(always)]
const fn leaf_from_cpuid(res: CpuidResult) -> CpuidLeaf {
    CpuidLeaf { a: res.eax, b: res.ebx, c: res.ecx, d: res.edx }
}

/// Converts a cached [`CpuidLeaf`] into raw `CPUID` output.
#[inline(always)]
const fn raw_from_leaf(leaf: CpuidLeaf) -> CpuidRawResult {
    CpuidRawResult { eax: leaf.a, ebx: leaf.b, ecx: leaf.c, edx: leaf.d }
}

/// CPUID leaf numbers (`x86_cpuid_leaf_num`).
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X86CpuidLeafNum {
    /// Basic CPUID information and vendor string (`0x0`).
    Base = x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_BASE,
    /// Processor Info and Feature Bits (`0x1`).
    ModelFeatures = x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_MODEL_FEATURES,
    /// Cache and TLB Descriptor information (`0x2`).
    CacheV1 = x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_CACHE_V1,
    /// Deterministic Cache Parameters (`0x4`).
    CacheV2 = x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_CACHE_V2,
    /// MONITOR/MWAIT leaf (`0x5`).
    Mon = x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_MON,
    /// Thermal and Power Management leaf (`0x6`).
    ThermalAndPower = x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_THERMAL_AND_POWER,
    /// Structured Extended Feature Flags (`0x7`).
    ExtendedFeatureFlags =
        x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_EXTENDED_FEATURE_FLAGS,
    /// Architectural Performance Monitoring leaf (`0xa`).
    PerformanceMonitoring =
        x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_PERFORMANCE_MONITORING,
    /// Extended Topology Enumeration leaf (`0xb`).
    Topology = x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_TOPOLOGY,
    /// Processor Extended State Enumeration leaf (`0xd`).
    XSave = x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_XSAVE,
    /// Intel Processor Trace Enumeration leaf (`0x14`).
    Pt = x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_PT,
    /// Time Stamp Counter and Nominal Core Crystal Clock Information leaf (`0x15`).
    Tsc = x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_TSC,

    /// Hypervisor CPUID leaf range base and vendor leaf (`0x40000000`).
    HypVendor = x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_HYP_VENDOR,
    /// KVM hypervisor feature bits (`0x40000001`).
    KvmFeatures = x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_KVM_FEATURES,

    /// Extended Processor Info base leaf (`0x80000000`).
    ExtBase = x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_EXT_BASE,
    /// Extended Processor Info and Feature Bits (`0x80000001`).
    ExtModelFeatures = 0x8000_0001,
    /// Processor Brand String leaf (`0x80000002`).
    Brand = x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_BRAND,
    /// Advanced Power Management Information (`0x80000007`).
    ExtPowerManagement = 0x8000_0007,
    /// Virtual and Physical Address Sizes (`0x80000008`).
    ExtAddressSizes = 0x8000_0008,
    /// AMD Processor Topology leaf (`0x8000001e`).
    AmdTopology = x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_AMD_TOPOLOGY,
}

impl X86CpuidLeafNum {
    /// Base hypervisor CPUID leaf (`X86_CPUID_HYP_BASE`).
    pub const HYP_BASE: Self = Self::HypVendor;
}

zr::static_assert!(
    X86CpuidLeafNum::HYP_BASE as u32 == x86_feature_bindings::x86_cpuid_leaf_num_X86_CPUID_HYP_BASE
);

/// Descriptor for a single feature bit in a CPUID leaf (`x86_cpuid_bit`).
pub type X86CpuidBit = x86_feature_bindings::x86_cpuid_bit;

/// Decoded CPU family, model, stepping, and microcode patch level.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct X86ModelInfo {
    /// Processor type field from leaf `0x1`.
    pub processor_type: u8,
    /// Base family ID.
    pub family: u8,
    /// Base model ID.
    pub model: u8,
    /// Stepping ID.
    pub stepping: u8,

    /// Effective display family (including extended family when `family == 0xf`).
    pub display_family: u32,
    /// Effective display model (including extended model when `family == 0x6` or `0xf`).
    pub display_model: u32,

    /// Loaded microcode patch level (`IA32_BIOS_SIGN_ID`).
    pub patch_level: u32,
}

/// Supported hypervisors.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum X86HypervisorList {
    /// Unrecognized hypervisor.
    #[default]
    Unknown,
    /// Bare metal (no hypervisor detected).
    None,
    /// Linux KVM hypervisor.
    Kvm,
}

// add feature bits to test here
// format: x86_cpuid_bit!(name, cpuid leaf, register (eax-edx:0-3), bit)
x86_cpuid_bit! {X86_FEATURE_SSE3, ModelFeatures, 2, 0}
x86_cpuid_bit! {X86_FEATURE_MON, ModelFeatures, 2, 3}
x86_cpuid_bit! {X86_FEATURE_VMX, ModelFeatures, 2, 5}
x86_cpuid_bit! {X86_FEATURE_TM2, ModelFeatures, 2, 8}
x86_cpuid_bit! {X86_FEATURE_SSSE3, ModelFeatures, 2, 9}
x86_cpuid_bit! {X86_FEATURE_PDCM, ModelFeatures, 2, 15}
x86_cpuid_bit! {X86_FEATURE_PCID, ModelFeatures, 2, 17}
x86_cpuid_bit! {X86_FEATURE_SSE4_1, ModelFeatures, 2, 19}
x86_cpuid_bit! {X86_FEATURE_SSE4_2, ModelFeatures, 2, 20}
x86_cpuid_bit! {X86_FEATURE_X2APIC, ModelFeatures, 2, 21}
x86_cpuid_bit! {X86_FEATURE_TSC_DEADLINE, ModelFeatures, 2, 24}
x86_cpuid_bit! {X86_FEATURE_AESNI, ModelFeatures, 2, 25}
x86_cpuid_bit! {X86_FEATURE_XSAVE, ModelFeatures, 2, 26}
x86_cpuid_bit! {X86_FEATURE_AVX, ModelFeatures, 2, 28}
x86_cpuid_bit! {X86_FEATURE_RDRAND, ModelFeatures, 2, 30}
x86_cpuid_bit! {X86_FEATURE_HYPERVISOR, ModelFeatures, 2, 31}
x86_cpuid_bit! {X86_FEATURE_FPU, ModelFeatures, 3, 0}
x86_cpuid_bit! {X86_FEATURE_SEP, ModelFeatures, 3, 11}
x86_cpuid_bit! {X86_FEATURE_CLFLUSH, ModelFeatures, 3, 19}
x86_cpuid_bit! {X86_FEATURE_ACPI, ModelFeatures, 3, 22}
x86_cpuid_bit! {X86_FEATURE_MMX, ModelFeatures, 3, 23}
x86_cpuid_bit! {X86_FEATURE_FXSR, ModelFeatures, 3, 24}
x86_cpuid_bit! {X86_FEATURE_SSE, ModelFeatures, 3, 25}
x86_cpuid_bit! {X86_FEATURE_SSE2, ModelFeatures, 3, 26}
x86_cpuid_bit! {X86_FEATURE_TM, ModelFeatures, 3, 29}
x86_cpuid_bit! {X86_FEATURE_DTS, ThermalAndPower, 0, 0}
x86_cpuid_bit! {X86_FEATURE_TURBO, ThermalAndPower, 0, 1}
x86_cpuid_bit! {X86_FEATURE_PLN, ThermalAndPower, 0, 4}
x86_cpuid_bit! {X86_FEATURE_PTM, ThermalAndPower, 0, 6}
x86_cpuid_bit! {X86_FEATURE_HWP, ThermalAndPower, 0, 7}
x86_cpuid_bit! {X86_FEATURE_HWP_NOT, ThermalAndPower, 0, 8}
x86_cpuid_bit! {X86_FEATURE_HWP_ACT, ThermalAndPower, 0, 9}
x86_cpuid_bit! {X86_FEATURE_HWP_PREF, ThermalAndPower, 0, 10}
x86_cpuid_bit! {X86_FEATURE_TURBO_MAX, ThermalAndPower, 0, 14}
x86_cpuid_bit! {X86_FEATURE_HW_FEEDBACK, ThermalAndPower, 2, 0}
x86_cpuid_bit! {X86_FEATURE_PERF_BIAS, ThermalAndPower, 2, 3}
x86_cpuid_bit! {X86_FEATURE_FSGSBASE, ExtendedFeatureFlags, 1, 0}
x86_cpuid_bit! {X86_FEATURE_TSC_ADJUST, ExtendedFeatureFlags, 1, 1}
x86_cpuid_bit! {X86_FEATURE_AVX2, ExtendedFeatureFlags, 1, 5}
x86_cpuid_bit! {X86_FEATURE_SMEP, ExtendedFeatureFlags, 1, 7}
x86_cpuid_bit! {X86_FEATURE_ERMS, ExtendedFeatureFlags, 1, 9}
x86_cpuid_bit! {X86_FEATURE_INVPCID, ExtendedFeatureFlags, 1, 10}
x86_cpuid_bit! {X86_FEATURE_AVX512F, ExtendedFeatureFlags, 1, 16}
x86_cpuid_bit! {X86_FEATURE_AVX512DQ, ExtendedFeatureFlags, 1, 17}
x86_cpuid_bit! {X86_FEATURE_RDSEED, ExtendedFeatureFlags, 1, 18}
x86_cpuid_bit! {X86_FEATURE_SMAP, ExtendedFeatureFlags, 1, 20}
x86_cpuid_bit! {X86_FEATURE_AVX512IFMA, ExtendedFeatureFlags, 1, 21}
x86_cpuid_bit! {X86_FEATURE_CLFLUSHOPT, ExtendedFeatureFlags, 1, 23}
x86_cpuid_bit! {X86_FEATURE_CLWB, ExtendedFeatureFlags, 1, 24}
x86_cpuid_bit! {X86_FEATURE_PT, ExtendedFeatureFlags, 1, 25}
x86_cpuid_bit! {X86_FEATURE_AVX512PF, ExtendedFeatureFlags, 1, 26}
x86_cpuid_bit! {X86_FEATURE_AVX512ER, ExtendedFeatureFlags, 1, 27}
x86_cpuid_bit! {X86_FEATURE_AVX512CD, ExtendedFeatureFlags, 1, 28}
x86_cpuid_bit! {X86_FEATURE_AVX512BW, ExtendedFeatureFlags, 1, 30}
x86_cpuid_bit! {X86_FEATURE_AVX512VL, ExtendedFeatureFlags, 1, 31}
x86_cpuid_bit! {X86_FEATURE_AVX512VBMI, ExtendedFeatureFlags, 2, 1}
x86_cpuid_bit! {X86_FEATURE_UMIP, ExtendedFeatureFlags, 2, 2}
x86_cpuid_bit! {X86_FEATURE_PKU, ExtendedFeatureFlags, 2, 3}
x86_cpuid_bit! {X86_FEATURE_AVX512VBMI2, ExtendedFeatureFlags, 2, 6}
x86_cpuid_bit! {X86_FEATURE_AVX512VNNI, ExtendedFeatureFlags, 2, 11}
x86_cpuid_bit! {X86_FEATURE_AVX512BITALG, ExtendedFeatureFlags, 2, 12}
x86_cpuid_bit! {X86_FEATURE_AVX512VPDQ, ExtendedFeatureFlags, 2, 14}
x86_cpuid_bit! {X86_FEATURE_LA57, ExtendedFeatureFlags, 2, 16}
x86_cpuid_bit! {X86_FEATURE_AVX512QVNNIW, ExtendedFeatureFlags, 3, 2}
x86_cpuid_bit! {X86_FEATURE_AVX512QFMA, ExtendedFeatureFlags, 3, 3}
x86_cpuid_bit! {X86_FEATURE_MD_CLEAR, ExtendedFeatureFlags, 3, 10}
x86_cpuid_bit! {X86_FEATURE_IBRS_IBPB, ExtendedFeatureFlags, 3, 26}
x86_cpuid_bit! {X86_FEATURE_STIBP, ExtendedFeatureFlags, 3, 27}
x86_cpuid_bit! {X86_FEATURE_L1D_FLUSH, ExtendedFeatureFlags, 3, 28}
x86_cpuid_bit! {X86_FEATURE_ARCH_CAPABILITIES, ExtendedFeatureFlags, 3, 29}
x86_cpuid_bit! {X86_FEATURE_SSBD, ExtendedFeatureFlags, 3, 31}
x86_cpuid_bit! {X86_FEATURE_TOPOLOGY_SHIFT, Topology, 0, 0}
x86_cpuid_bit! {X86_FEATURE_TOPOLOGY_SINGLE_PROCESSOR, Topology, 0, 1}
x86_cpuid_bit! {X86_FEATURE_TOPOLOGY_LOGICAL_PROCESSOR, Topology, 0, 8}
x86_cpuid_bit! {X86_FEATURE_TOPOLOGY_CORE, Topology, 0, 9}
x86_cpuid_bit! {X86_FEATURE_KVM_PV_CLOCK, KvmFeatures, 0, 3}
x86_cpuid_bit! {X86_FEATURE_KVM_PV_EOI, KvmFeatures, 0, 6}
x86_cpuid_bit! {X86_FEATURE_KVM_PV_IPI, KvmFeatures, 0, 11}
x86_cpuid_bit! {X86_FEATURE_KVM_PV_CLOCK_STABLE, KvmFeatures, 0, 24}
x86_cpuid_bit! {X86_FEATURE_AMD_TOPO, ExtModelFeatures, 2, 22}
x86_cpuid_bit! {X86_FEATURE_SYSCALL, ExtModelFeatures, 3, 11}
x86_cpuid_bit! {X86_FEATURE_NX, ExtModelFeatures, 3, 20}
x86_cpuid_bit! {X86_FEATURE_HUGE_PAGE, ExtModelFeatures, 3, 26}
x86_cpuid_bit! {X86_FEATURE_RDTSCP, ExtModelFeatures, 3, 27}
x86_cpuid_bit! {X86_FEATURE_INVAR_TSC, ExtPowerManagement, 3, 8}
x86_cpuid_bit! {X86_FEATURE_INVLPGB, ExtAddressSizes, 1, 3}

/// Reads standard `CPUID` leaf `leaf_val` (`<= MAX_SUPPORTED_CPUID`), using relaxed 32-bit atomic
/// loads for leaf `0x6` (`ThermalAndPower`) because CPU 0 refreshes leaf `0x6` in
/// `cpu_feature_late_init_percpu` while secondary CPUs may read it.
///
/// # Safety
///
/// The caller must ensure that `leaf_val <= MAX_SUPPORTED_CPUID` and that for `leaf_val != 6`
/// (`ThermalAndPower`), no concurrent write to `CPUID[leaf_val as usize]` races with this read.
#[inline(always)]
unsafe fn read_standard_cpuid_leaf(leaf_val: u32) -> CpuidLeaf {
    debug_assert!(leaf_val <= MAX_SUPPORTED_CPUID);
    // SAFETY: Caller guarantees `leaf_val <= MAX_SUPPORTED_CPUID` and non-concurrent mutation for
    // `leaf_val != 6` (standard leaves `!= 6` are written only on CPU 0 in `early_init_percpu`,
    // plus leaf `0x7` in single-threaded pre-VM `cpu_feature_init`, before SMP bringup), and no
    // Rust `&` or `&mut` references to `CPUID` elements ever exist. Leaf `0x6` uses `Relaxed`
    // `AtomicU32` loads because refreshing `IA32_MISC_ENABLE`'s reflected turbo bit does not
    // synchronize other shared memory state; the four registers are loaded independently, so a
    // read concurrent with `write_cpuid_leaf_6` may observe a mix of old and new registers.
    unsafe {
        let ptr = CPUID.get().cast::<CpuidLeaf>().add(leaf_val as usize);
        if leaf_val == X86CpuidLeafNum::ThermalAndPower as u32 {
            CpuidLeaf {
                a: AtomicU32::from_ptr(&raw mut (*ptr).a).load(Ordering::Relaxed),
                b: AtomicU32::from_ptr(&raw mut (*ptr).b).load(Ordering::Relaxed),
                c: AtomicU32::from_ptr(&raw mut (*ptr).c).load(Ordering::Relaxed),
                d: AtomicU32::from_ptr(&raw mut (*ptr).d).load(Ordering::Relaxed),
            }
        } else {
            ptr.read()
        }
    }
}

/// Updates cached `CPUID` leaf `0x6` (`ThermalAndPower`) via 32-bit atomic stores.
#[inline(always)]
fn write_cpuid_leaf_6(leaf: CpuidLeaf) {
    const LEAF_6: usize = X86CpuidLeafNum::ThermalAndPower as usize;
    const _: () = assert!(LEAF_6 <= MAX_SUPPORTED_CPUID as usize);
    // SAFETY: `LEAF_6` is in-bounds of `CPUID`, 4-byte aligned, and never aliased by Rust
    // references; all Rust accesses (`read_standard_cpuid_leaf`, `write_cpuid_leaf_6`) and C++
    // accesses (`x86_feature_test` in `arch/x86/feature.h` via `ktl::atomic_ref<uint32_t>`) to leaf
    // `0x6` use 32-bit atomic operations exclusively. `Ordering::Relaxed` suffices because
    // refreshing leaf `0x6` publishes no other memory state, and every consumer inspects a single
    // register, so readers tolerate observing a mix of old and new registers across the four
    // independent stores.
    unsafe {
        let ptr = CPUID.get().cast::<CpuidLeaf>().add(LEAF_6);
        AtomicU32::from_ptr(&raw mut (*ptr).a).store(leaf.a, Ordering::Relaxed);
        AtomicU32::from_ptr(&raw mut (*ptr).b).store(leaf.b, Ordering::Relaxed);
        AtomicU32::from_ptr(&raw mut (*ptr).c).store(leaf.c, Ordering::Relaxed);
        AtomicU32::from_ptr(&raw mut (*ptr).d).store(leaf.d, Ordering::Relaxed);
    }
}

/// Returns the cached [`CpuidLeaf`] for `leaf_val` (`x86_get_cpuid_leaf`).
#[inline(always)]
fn x86_get_cpuid_leaf_raw(leaf_val: u32) -> Option<CpuidLeaf> {
    // SAFETY: `MAX_*_CPUID`, `G_CPUID_READY`, `CPUID_HYP`, and `CPUID_EXT` are populated on CPU 0
    // in single-threaded `early_init_percpu` before secondary CPUs start (`MAX_*_CPUID == 0` and
    // `G_CPUID_READY == false` reject all leaves beforehand), and `CPUID_HYP` / `CPUID_EXT` are
    // immutable thereafter. The range checks bound `leaf_val <= MAX_SUPPORTED_CPUID` for
    // `read_standard_cpuid_leaf` (with leaves `!= 6` mutated only in single-threaded CPU 0 init
    // before SMP bringup) and keep `offset` in-bounds of `CPUID_HYP` and `CPUID_EXT`.
    unsafe {
        if leaf_val < X86CpuidLeafNum::HYP_BASE as u32 {
            let max = MAX_CPUID.get().read();
            if (leaf_val == 0 && !G_CPUID_READY.get().read())
                || leaf_val > max
                || leaf_val > MAX_SUPPORTED_CPUID
            {
                core::hint::cold_path();
                return None;
            }
            Some(read_standard_cpuid_leaf(leaf_val))
        } else if leaf_val < X86CpuidLeafNum::ExtBase as u32 {
            let max_hyp = MAX_HYP_CPUID.get().read();
            if leaf_val > max_hyp || leaf_val > MAX_SUPPORTED_CPUID_HYP {
                core::hint::cold_path();
                return None;
            }
            let offset = (leaf_val - X86CpuidLeafNum::HYP_BASE as u32) as usize;
            Some(CPUID_HYP.get().cast_const().cast::<CpuidLeaf>().add(offset).read())
        } else {
            let max_ext = MAX_EXT_CPUID.get().read();
            if leaf_val > max_ext || leaf_val > MAX_SUPPORTED_CPUID_EXT {
                core::hint::cold_path();
                return None;
            }
            let offset = (leaf_val - X86CpuidLeafNum::ExtBase as u32) as usize;
            Some(CPUID_EXT.get().cast_const().cast::<CpuidLeaf>().add(offset).read())
        }
    }
}

/// Returns the cached [`CpuidLeaf`] for `leaf` (`x86_get_cpuid_leaf`).
#[inline(always)]
pub fn x86_get_cpuid_leaf(leaf: X86CpuidLeafNum) -> Option<CpuidLeaf> {
    x86_get_cpuid_leaf_raw(leaf as u32)
}

/// Tests whether the CPUID feature `bit` is set in the cached CPUID tables (`x86_feature_test`).
#[inline(always)]
pub fn x86_feature_test(bit: X86CpuidBit) -> bool {
    debug_assert!(bit.word <= 3 && bit.bit <= 31);
    if bit.word > 3 || bit.bit > 31 {
        core::hint::cold_path();
        return false;
    }
    let Some(leaf) = x86_get_cpuid_leaf_raw(bit.leaf_num) else {
        return false;
    };
    let val = match bit.word {
        0 => leaf.a,
        1 => leaf.b,
        2 => leaf.c,
        3 => leaf.d,
        _ => return false,
    };
    ((1u32 << bit.bit) & val) != 0
}

/// Returns the boot CPU's vendor (`x86_vendor`).
#[inline]
pub fn x86_get_vendor() -> X86VendorList {
    // SAFETY: Written once on CPU 0 in `early_init_percpu` before secondary CPUs start, and
    // read-only thereafter.
    unsafe { X86_VENDOR.get().read() }
}

/// Returns the boot CPU's model info.
#[inline]
pub fn x86_get_model() -> X86ModelInfo {
    // SAFETY: Written only on CPU 0 in `early_init_percpu` and `cpu_feature_init` before secondary
    // CPUs start, and read-only thereafter.
    unsafe { MODEL_INFO.get().read() }
}

/// Returns whether the hypervisor supports a paravirtualized clock (`x86_hypervisor_has_pv_clock`).
#[inline]
pub fn x86_hypervisor_has_pv_clock() -> bool {
    // SAFETY: Written once on CPU 0 in `early_init_percpu` before secondary CPUs start.
    unsafe { HYPERVISOR_HAS_PV_CLOCK.get().read() }
}

/// Returns whether the hypervisor supports paravirtualized EOI.
#[inline]
pub fn x86_hypervisor_has_pv_eoi() -> bool {
    // SAFETY: Written once on CPU 0 in `early_init_percpu` before secondary CPUs start.
    unsafe { HYPERVISOR_HAS_PV_EOI.get().read() }
}

/// Returns whether the hypervisor supports paravirtualized IPIs.
#[inline]
pub fn x86_hypervisor_has_pv_ipi() -> bool {
    // SAFETY: Written once on CPU 0 in `early_init_percpu` before secondary CPUs start.
    unsafe { HYPERVISOR_HAS_PV_IPI.get().read() }
}

/// Returns whether the kernel is running under a hypervisor.
#[inline]
pub fn x86_has_hypervisor() -> bool {
    x86_get_hypervisor() != X86HypervisorList::None
}

/// Returns the detected hypervisor.
#[inline]
pub fn x86_get_hypervisor() -> X86HypervisorList {
    // SAFETY: Written once on CPU 0 in `early_init_percpu` before secondary CPUs start.
    unsafe { X86_HYPERVISOR.get().read() }
}

/// Returns the active microarchitecture configuration (`x86_get_microarch_config`).
///
/// # Safety-usable invariant
///
/// When this function returns `Some(cfg)`, `cfg` describes the boot CPU's microarchitecture:
/// - `cfg.get_apic_freq` has no preconditions.
/// - `cfg.get_tsc_freq` has no preconditions beyond running in the kernel (CPL 0) with `%gs.base`
///   initialized and is safe to call from any thread context.
/// - `cfg.reboot_system` and `cfg.reboot_reason` are safe to call at CPL 0 when resetting the
///   system.
/// - `cfg.idle_states.states` contains a terminating base `C1` state, and every populated entry up
///   to and including base `C1` has a `name` pointer that is either null or points to a valid
///   NUL-terminated `'static` C string.
#[inline]
pub fn x86_get_microarch_config() -> Option<&'static X86MicroarchConfig> {
    // SAFETY: Written once on CPU 0 in `early_init_percpu` before secondary CPUs start with a
    // reference from `get_microarch_config(&RealCpuId)` satisfying `X86_MICROARCH_CONFIG`'s safety
    // invariant.
    unsafe { X86_MICROARCH_CONFIG.get().read() }
}

/// Returns whether the CPU supports Indirect Branch Prediction Barrier (`x86_cpu_has_ibpb`).
#[inline]
pub fn x86_cpu_has_ibpb() -> bool {
    // SAFETY: Written once on CPU 0 in `cpu_feature_init` before secondary CPUs start.
    unsafe { G_HAS_IBPB.get().read() }
}

/// Returns whether the Return Address Stack should be filled on context switch
/// (`x86_cpu_should_ras_fill_on_ctxt_switch`).
#[inline]
pub fn x86_cpu_should_ras_fill_on_ctxt_switch() -> bool {
    // SAFETY: Written once on CPU 0 in `cpu_feature_init` before secondary CPUs start.
    unsafe { G_RAS_FILL_ON_CTXT_SWITCH.get().read() }
}

/// Returns whether the CPU is vulnerable to cross-thread RSB predictions
/// (`x86_cpu_vulnerable_to_rsb_cross_thread`).
#[inline]
pub fn x86_cpu_vulnerable_to_rsb_cross_thread() -> bool {
    // `Ordering::Relaxed` suffices: monotonic `false -> true` latch set in
    // `cpu_feature_late_init_percpu` before each secondary CPU enters its idle loop, guarding only
    // CPU-local RSB overwrite without synchronizing other memory state.
    G_CPU_VULNERABLE_TO_RSB_CROSS_THREAD.load(Ordering::Relaxed)
}

/// Returns whether the CPU is vulnerable to RSB underflow (`x86_cpu_vulnerable_to_rsb_underflow`).
#[inline]
pub fn x86_cpu_vulnerable_to_rsb_underflow() -> bool {
    // SAFETY: Written once on CPU 0 in `cpu_feature_init` before secondary CPUs start.
    unsafe { G_CPU_VULNERABLE_TO_RSB_UNDERFLOW.get().read() }
}

/// Returns whether an Indirect Branch Prediction Barrier (`IBPB`) should be issued on context
/// switches (`x86_cpu_should_ibpb_on_ctxt_switch`).
///
/// # Safety-usable invariant
///
/// When this function returns `true`, [`x86_cpu_has_ibpb()`] is guaranteed to be `true`
/// (`G_SHOULD_IBPB_ON_CTXT_SWITCH` is only set in `cpu_feature_init` when `G_HAS_IBPB` is `true`).
#[inline]
pub fn x86_cpu_should_ibpb_on_ctxt_switch() -> bool {
    // SAFETY: Written once on CPU 0 in `cpu_feature_init` before secondary CPUs start.
    unsafe { G_SHOULD_IBPB_ON_CTXT_SWITCH.get().read() }
}

/// Returns whether Speculative Store Bypass is mitigated.
#[inline]
fn x86_cpu_should_mitigate_ssb() -> bool {
    // SAFETY: Written once on CPU 0 in `cpu_feature_init` before secondary CPUs start.
    unsafe { G_SSB_MITIGATED.get().read() }
}

/// Returns whether the L1D cache should be flushed on VM entry
/// (`x86_cpu_should_l1d_flush_on_vmentry`).
#[inline]
pub fn x86_cpu_should_l1d_flush_on_vmentry() -> bool {
    // SAFETY: Written once on CPU 0 in `cpu_feature_init` before secondary CPUs start.
    unsafe { G_L1D_FLUSH_ON_VMENTRY.get().read() }
}

/// Returns whether `VERW` (`MD_CLEAR`) should be executed on return to user space
/// (`x86_cpu_should_md_clear_on_user_return`).
#[inline]
pub fn x86_cpu_should_md_clear_on_user_return() -> bool {
    // SAFETY: Written once on CPU 0 in `cpu_feature_init` before secondary CPUs start.
    unsafe { G_MD_CLEAR_ON_USER_RETURN.get().read() }
}

/// Returns whether the CPU supports Supervisor Mode Access Prevention (SMAP).
#[inline]
pub fn x86_feature_has_smap() -> bool {
    // SAFETY: Written once on CPU 0 in `early_init_percpu` before secondary CPUs start.
    unsafe { G_X86_FEATURE_HAS_SMAP.get().read() }
}

/// Returns whether the CPU supports FSGSBASE instructions (`rdfsbase`, `rdgsbase`, `wrfsbase`,
/// `wrgsbase`).
#[inline]
pub fn x86_feature_fsgsbase() -> bool {
    // SAFETY: Written once on CPU 0 in `early_init_percpu` before secondary CPUs start.
    unsafe { G_X86_FEATURE_FSGSBASE.get().read() }
}

/// Retrieve the specified subleaf. This function is not cached.
/// Returns `None` if leaf `num` is invalid.
#[inline]
pub fn get_cpuid_subleaf(num: X86CpuidLeafNum, subleaf: u32) -> Option<CpuidLeaf> {
    get_cpuid_subleaf_raw(num as u32, subleaf)
}

/// Retrieve the specified subleaf using a raw leaf number.
#[inline(always)]
pub(super) fn get_cpuid_subleaf_raw(num_val: u32, subleaf: u32) -> Option<CpuidLeaf> {
    // SAFETY: `MAX_CPUID`, `MAX_HYP_CPUID`, and `MAX_EXT_CPUID` are written once during
    // single-threaded boot initialization on CPU 0 and are read-only thereafter.
    unsafe {
        if num_val < X86CpuidLeafNum::HYP_BASE as u32 {
            if num_val > MAX_CPUID.get().read() {
                core::hint::cold_path();
                return None;
            }
        } else if num_val < X86CpuidLeafNum::ExtBase as u32 {
            if num_val > MAX_HYP_CPUID.get().read() {
                core::hint::cold_path();
                return None;
            }
        } else if num_val > MAX_EXT_CPUID.get().read() {
            core::hint::cold_path();
            return None;
        }
    }
    let res = __cpuid_count(num_val, subleaf);
    Some(leaf_from_cpuid(res))
}

/// Issues an Indirect Branch Prediction Barrier (`IA32_PRED_CMD.IBPB`).
///
/// # Safety
///
/// The caller must ensure that CPL is 0 and the CPU supports `IBPB` ([`x86_cpu_has_ibpb`]).
#[inline(always)]
pub unsafe fn x86_cpu_ibpb() {
    debug_assert!(x86_cpu_has_ibpb());
    // SAFETY: Caller guarantees CPL 0 and that the CPU supports IBPB (`X86_MSR_IA32_PRED_CMD`), and
    // bit 0 (`1`) is the architectural IBPB command value.
    unsafe { write_msr(X86_MSR_IA32_PRED_CMD, 1) };
}

fn detect_hypervisor() -> X86HypervisorList {
    if !x86_feature_test(X86_FEATURE_HYPERVISOR) {
        return X86HypervisorList::None;
    }
    let res = BootCpuidIo.read_raw(X86CpuidLeafNum::HypVendor as u32, 0);
    let ebx = res.ebx.to_le_bytes();
    let ecx = res.ecx.to_le_bytes();
    let edx = res.edx.to_le_bytes();
    let vendor_string = [
        ebx[0], ebx[1], ebx[2], ebx[3], //
        ecx[0], ecx[1], ecx[2], ecx[3], //
        edx[0], edx[1], edx[2], edx[3], //
    ];
    if res.eax >= X86CpuidLeafNum::KvmFeatures as u32 && &vendor_string == b"KVMKVMKVM\0\0\0" {
        X86HypervisorList::Kvm
    } else {
        X86HypervisorList::Unknown
    }
}

/// Invoked on each CPU prior to lk_main being called (`x86_feature_early_init_percpu`). Only the
/// first caller populates the boot CPUID leaf cache, vendor, model info, microarchitecture config,
/// and hypervisor state.
///
/// The hypervisor vendor leaf (`0x4000_0000`) is cached in `CPUID_HYP[0]`, and the extended
/// maximum-leaf entry (`0x8000_0000`) in `CPUID_EXT[0]`.
///
/// # Safety
///
/// The caller must ensure that this function is called at CPL 0 with `%gs.base` initialized, and
/// that the first invocation occurs on CPU 0 during single-threaded early boot with interrupts
/// disabled before secondary CPUs are started.
pub(super) unsafe fn early_init_percpu() {
    if G_CPUID_INITIALIZED.swap(true, Ordering::SeqCst) {
        return;
    }

    debug_assert_eq!(crate::arch_rs::curr_cpu_num(), 0);

    // Pointers to the first element of each cache table. Raw element pointers are used so that no
    // whole-array reference ever aliases leaf `0x6`'s atomically accessed registers.
    let cpuid_ptr = CPUID.get().cast::<CpuidLeaf>();
    let cpuid_ext_ptr = CPUID_EXT.get().cast::<CpuidLeaf>();
    let cpuid_hyp_ptr = CPUID_HYP.get().cast::<CpuidLeaf>();

    // Test for cpuid count.
    let leaf0 = __cpuid(0);
    // SAFETY: `G_CPUID_INITIALIZED.swap(true, Ordering::SeqCst)` ensures this runs at most once,
    // on CPU 0 during single-threaded early boot before secondary CPUs start, so no other access
    // to `CPUID` races; index `0` is in-bounds.
    unsafe { cpuid_ptr.write(leaf_from_cpuid(leaf0)) };

    let max_cpuid = leaf0.eax.min(MAX_SUPPORTED_CPUID);
    ltracef!("max cpuid {:#x}\n", max_cpuid);

    // Figure out the vendor.
    let b = leaf0.ebx.to_le_bytes();
    let d = leaf0.edx.to_le_bytes();
    let c = leaf0.ecx.to_le_bytes();
    let vendor_string = [
        b[0], b[1], b[2], b[3], //
        d[0], d[1], d[2], d[3], //
        c[0], c[1], c[2], c[3], //
    ];
    let vendor = if &vendor_string == b"GenuineIntel" {
        X86VendorList::X86_VENDOR_INTEL
    } else if &vendor_string == b"AuthenticAMD" {
        X86VendorList::X86_VENDOR_AMD
    } else {
        X86VendorList::X86_VENDOR_UNKNOWN
    };
    // SAFETY: Single-threaded early boot on CPU 0 (see above).
    unsafe { X86_VENDOR.get().write(vendor) };

    // Read in the base cpuids.
    for i in 1..=max_cpuid {
        let entry = leaf_from_cpuid(__cpuid_count(i, 0));
        if i == X86CpuidLeafNum::ThermalAndPower as u32 {
            write_cpuid_leaf_6(entry);
        } else {
            // SAFETY: Single-threaded early boot on CPU 0 (see above); `i <= MAX_SUPPORTED_CPUID`
            // keeps the offset in-bounds of `CPUID`.
            unsafe { cpuid_ptr.add(i as usize).write(entry) };
        }
    }
    // SAFETY: Single-threaded early boot on CPU 0 (see above).
    unsafe { MAX_CPUID.get().write(max_cpuid) };

    // Test for extended cpuid count.
    let ext_base = X86CpuidLeafNum::ExtBase as u32;
    let ext0 = __cpuid(ext_base);
    // SAFETY: Single-threaded early boot on CPU 0 (see above); index `0` is in-bounds.
    unsafe { cpuid_ext_ptr.write(leaf_from_cpuid(ext0)) };

    let max_ext_cpuid = ext0.eax.min(MAX_SUPPORTED_CPUID_EXT);
    ltracef!("max extended cpuid {:#x}\n", ext0.eax);

    // Read in the extended cpuids.
    let mut i = ext_base + 1;
    while i - 1 < max_ext_cpuid {
        let index = (i - ext_base) as usize;
        // SAFETY: Single-threaded early boot on CPU 0 (see above); `i <= MAX_SUPPORTED_CPUID_EXT`
        // keeps `index` in-bounds of `CPUID_EXT`.
        unsafe { cpuid_ext_ptr.add(index).write(leaf_from_cpuid(__cpuid_count(i, 0))) };
        i += 1;
    }
    // SAFETY: Single-threaded early boot on CPU 0 (see above).
    unsafe { MAX_EXT_CPUID.get().write(max_ext_cpuid) };

    // Read in the hypervisor cpuids. The maximum leaf is reported at
    // `X86CpuidLeafNum::HYP_BASE`.
    let hyp_base = X86CpuidLeafNum::HYP_BASE as u32;
    let hyp0 = __cpuid(hyp_base);
    // SAFETY: Single-threaded early boot on CPU 0 (see above); index `0` is in-bounds.
    unsafe { cpuid_hyp_ptr.write(leaf_from_cpuid(hyp0)) };
    let max_hyp_cpuid = hyp0.eax.min(MAX_SUPPORTED_CPUID_HYP);
    for leaf_num in hyp_base + 1..=max_hyp_cpuid {
        let index = (leaf_num - hyp_base) as usize;
        // SAFETY: Single-threaded early boot on CPU 0 (see above);
        // `leaf_num <= MAX_SUPPORTED_CPUID_HYP` keeps `index` in-bounds of `CPUID_HYP`.
        unsafe { cpuid_hyp_ptr.add(index).write(leaf_from_cpuid(__cpuid(leaf_num))) };
    }
    // SAFETY: Single-threaded early boot on CPU 0 (see above).
    unsafe {
        MAX_HYP_CPUID.get().write(max_hyp_cpuid);
        G_CPUID_READY.get().write(true);
    }

    // Populate the model info.
    if let Some(leaf) = x86_get_cpuid_leaf(X86CpuidLeafNum::ModelFeatures) {
        let processor_type = ((leaf.a >> 12) & 0x3) as u8;
        let family = ((leaf.a >> 8) & 0xf) as u8;
        let model = ((leaf.a >> 4) & 0xf) as u8;
        let stepping = (leaf.a & 0xf) as u8;
        let mut display_family = u32::from(family);
        let mut display_model = u32::from(model);

        if family == 0xf {
            display_family += (leaf.a >> 20) & 0xff;
        }
        if family == 0xf || family == 0x6 {
            display_model += ((leaf.a >> 16) & 0xf) << 4;
        }

        // SAFETY: Single-threaded early boot on CPU 0 before secondary CPUs start.
        unsafe {
            MODEL_INFO.get().write(X86ModelInfo {
                processor_type,
                family,
                model,
                stepping,
                display_family,
                display_model,
                patch_level: 0,
            });
        }
    }

    let cpuid = RealCpuId;
    let microarch_cfg = get_microarch_config(&cpuid);
    let hypervisor = detect_hypervisor();
    // SAFETY: Single-threaded early boot on CPU 0 before secondary CPUs start; `microarch_cfg`
    // comes from `get_microarch_config(&RealCpuId)` on CPU 0, upholding `X86_MICROARCH_CONFIG`'s
    // safety invariant.
    unsafe {
        X86_MICROARCH_CONFIG.get().write(Some(microarch_cfg));
        X86_HYPERVISOR.get().write(hypervisor);
        HYPERVISOR_HAS_PV_CLOCK.get().write(
            hypervisor == X86HypervisorList::Kvm && x86_feature_test(X86_FEATURE_KVM_PV_CLOCK),
        );
        HYPERVISOR_HAS_PV_EOI.get().write(
            hypervisor == X86HypervisorList::Kvm && x86_feature_test(X86_FEATURE_KVM_PV_EOI),
        );
        HYPERVISOR_HAS_PV_IPI.get().write(
            hypervisor == X86HypervisorList::Kvm && x86_feature_test(X86_FEATURE_KVM_PV_IPI),
        );
        G_X86_FEATURE_HAS_SMAP.get().write(x86_feature_test(X86_FEATURE_SMAP));
        G_X86_FEATURE_FSGSBASE.get().write(x86_feature_test(X86_FEATURE_FSGSBASE));
        G_X86_FEATURE_INVPCID.get().write(x86_feature_test(X86_FEATURE_INVPCID));
    }
}

/// Cached CPUID I/O provider backed by the boot CPUID leaf tables (`CPUID`, `CPUID_EXT`,
/// `CPUID_HYP`), modeled on `arch::BootCpuidIo` in `boot-cpuid.h`.
///
/// Like `arch::BootCpuidIo`, the maximum-leaf entries (`0x0`, `0x4000_0000`, `0x8000_0000`) are
/// always served from the cache, even when the hypervisor range is unpopulated (e.g. QEMU TCG
/// reports a vendor string with `EAX = 0`). Unlike `arch::BootCpuidIo`, which serves every leaf
/// and subleaf from the boot CPU's snapshot, only subleaf `0` of leaves within
/// `MAX_SUPPORTED_CPUID*` is served from the cache; other supported leaves and subleaves are read
/// directly from the current CPU.
#[derive(Clone, Copy, Debug, Default)]
struct BootCpuidIo;

impl Cpuid for BootCpuidIo {
    // Only used by boot and debug code; keep the many inlined `read` call sites small.
    #[inline(never)]
    fn read_raw(&self, leaf: u32, subleaf: u32) -> CpuidRawResult {
        // SAFETY: `G_CPUID_READY`, `MAX_*_CPUID`, `CPUID_HYP`, and `CPUID_EXT` are populated on
        // CPU 0 in single-threaded `early_init_percpu` before secondary CPUs start and are
        // immutable thereafter. `0` and `leaf` satisfy `<= MAX_SUPPORTED_CPUID` for
        // `read_standard_cpuid_leaf`, whose non-leaf-`0x6` entries are mutated only on CPU 0
        // before SMP bringup (leaf `0x6` is accessed atomically). The `hyp_base` / `ext_base` and
        // `MAX_SUPPORTED_CPUID_*` guards keep `.add(...)` in-bounds of `CPUID_HYP` and
        // `CPUID_EXT`, and index `0` of both is always in-bounds.
        unsafe {
            if G_CPUID_READY.get().read() {
                let hyp_base = X86CpuidLeafNum::HYP_BASE as u32;
                let ext_base = X86CpuidLeafNum::ExtBase as u32;

                if leaf < hyp_base {
                    let hw_max = read_standard_cpuid_leaf(0).a;
                    if leaf > hw_max {
                        return CpuidRawResult::zeroed();
                    }
                    if subleaf == 0 && leaf <= MAX_CPUID.get().read() && leaf <= MAX_SUPPORTED_CPUID
                    {
                        return raw_from_leaf(read_standard_cpuid_leaf(leaf));
                    }
                } else if leaf < ext_base {
                    let hyp0 = CPUID_HYP.get().cast_const().cast::<CpuidLeaf>().read();
                    if leaf == hyp_base && subleaf == 0 {
                        return raw_from_leaf(hyp0);
                    }
                    let hw_max_hyp = hyp0.a;
                    if hw_max_hyp < hyp_base || leaf > hw_max_hyp {
                        return CpuidRawResult::zeroed();
                    }
                    if subleaf == 0
                        && leaf <= MAX_HYP_CPUID.get().read()
                        && leaf <= MAX_SUPPORTED_CPUID_HYP
                    {
                        let base = CPUID_HYP.get().cast_const().cast::<CpuidLeaf>();
                        return raw_from_leaf(base.add((leaf - hyp_base) as usize).read());
                    }
                } else {
                    let ext0 = CPUID_EXT.get().cast_const().cast::<CpuidLeaf>().read();
                    if leaf == ext_base && subleaf == 0 {
                        return raw_from_leaf(ext0);
                    }
                    let hw_max_ext = ext0.a;
                    if hw_max_ext < ext_base || leaf > hw_max_ext {
                        return CpuidRawResult::zeroed();
                    }
                    if subleaf == 0
                        && leaf <= MAX_EXT_CPUID.get().read()
                        && leaf <= MAX_SUPPORTED_CPUID_EXT
                    {
                        let base = CPUID_EXT.get().cast_const().cast::<CpuidLeaf>();
                        return raw_from_leaf(base.add((leaf - ext_base) as usize).read());
                    }
                }
            }
        }
        // Fall back to direct hardware CPUID for supported subleaves > 0 or supported leaves above
        // `MAX_SUPPORTED_CPUID*` (or before `G_CPUID_READY` is set).
        DirectCpuid {}.read_raw(leaf, subleaf)
    }
}

/// Invoked on the boot CPU during boot, after platform is available. Runs after command line and
/// UART are enabled, but before code patching or the MMU are enabled.
///
/// A successful `disable_tsx` refreshes the cached `CPUID` leaf `0x7`, so `x86_feature_test`
/// observes the TSX-masked feature bits.
///
/// # Safety
///
/// The caller must ensure that this function is called once at CPL 0 on CPU 0 during
/// single-threaded pre-VM boot with interrupts disabled and `%gs.base` initialized, after
/// [`early_init_percpu`] and before any secondary CPUs start.
pub(super) unsafe fn cpu_feature_init() {
    debug_assert_eq!(crate::arch_rs::curr_cpu_num(), 0);

    let cpuid = BootCpuidIo;

    let vendor = x86_get_vendor();
    // Get microcode patch level.
    // SAFETY: Running at CPL 0 after `early_init_percpu` initialized `X86_FEATURE_HYPERVISOR`, and
    // each `match` arm calls the vendor-specific function only on a processor of that vendor.
    let patch_level = unsafe {
        match vendor {
            X86VendorList::X86_VENDOR_INTEL => super::intel::x86_intel_get_patch_level(),
            X86VendorList::X86_VENDOR_AMD => super::amd::x86_amd_get_patch_level(),
            X86VendorList::X86_VENDOR_UNKNOWN => 0,
        }
    };
    // SAFETY: Single-threaded pre-VM boot on CPU 0 before secondary CPUs start, and no reference
    // to `MODEL_INFO` exists.
    unsafe {
        if vendor != X86VendorList::X86_VENDOR_UNKNOWN {
            (&raw mut (*MODEL_INFO.get()).patch_level).write(patch_level);
        }
    }

    let boot_opts = BootOptions::get();
    let has_swapgs_bug = has_x86_swapgs_bug(&cpuid);

    // If mitigations are enabled, try to disable TSX. Disabling TSX prevents exploiting
    // TAA/CacheOut attacks and potential future exploits. It also avoids MD_CLEAR on CPUs
    // without MDS.
    //
    // WARNING: If we disable TSX, we must do so before we determine whether we are affected by
    // TAA/Cacheout; otherwise the TAA/Cacheout determination code will run before the TSX
    // CPUID bit is masked.
    if !boot_opts.x86_disable_spec_mitigations
        && disable_tsx(cpuid, IA32_ARCH_CAPABILITIES, IA32_TSX_CTRL)
    {
        // If successful, repopulate the boot CPU's CPUID cache in order to reflect
        // the disabling.
        let res7 = __cpuid_count(X86CpuidLeafNum::ExtendedFeatureFlags as u32, 0);
        // SAFETY: Single-threaded pre-VM early boot on CPU 0 with interrupts disabled before
        // secondary CPUs start or concurrent readers access the boot CPUID cache; `7 <=
        // MAX_SUPPORTED_CPUID` and no Rust references to `CPUID` exist.
        unsafe {
            CPUID
                .get()
                .cast::<CpuidLeaf>()
                .add(X86CpuidLeafNum::ExtendedFeatureFlags as usize)
                .write(leaf_from_cpuid(res7));
        }
        // SAFETY: `InitializeBootCpuid` requires CPL 0 on CPU 0 during single-threaded early boot
        // with interrupts disabled before secondary CPUs start, which the caller guarantees.
        unsafe { InitializeBootCpuid() };
    }

    let has_md_clear = cpuid.read(EXTENDED_FEATURES_D).md_clear();
    let has_mds_taa = has_x86_mds_taa_bugs(&cpuid, &IA32_ARCH_CAPABILITIES);
    let md_clear_on_user_return = !boot_opts.x86_disable_spec_mitigations
        && has_mds_taa
        && has_md_clear
        && boot_opts.x86_md_clear_on_user_return;
    let has_spec_ctrl = SpeculationControlMsr::is_supported(cpuid);
    let has_ssb = has_x86_ssb_bug(&cpuid, &IA32_ARCH_CAPABILITIES);
    let has_ssbd = can_mitigate_x86_ssb_bug(&cpuid);
    let ssb_mitigated = !boot_opts.x86_disable_spec_mitigations
        && has_ssb
        && has_ssbd
        && boot_opts.x86_spec_store_bypass_disable;
    let has_ibpb_val = has_ibpb(&cpuid);
    let has_enhanced_ibrs =
        has_ibrs(&cpuid, &IA32_ARCH_CAPABILITIES, /*always_on_mode=*/ true);
    let has_meltdown = has_x86_meltdown_bug(&cpuid, &IA32_ARCH_CAPABILITIES);
    let has_l1tf = has_x86_l1tf_bug(&cpuid, &IA32_ARCH_CAPABILITIES);
    let l1d_flush_on_vmentry = !boot_opts.x86_disable_spec_mitigations
        && has_l1tf
        && cpuid.read(EXTENDED_FEATURES_D).l1d_flush();
    let ras_fill_on_ctxt_switch = !boot_opts.x86_disable_spec_mitigations;
    // TODO(https://fxbug.dev/42139346): Replace with newer lib/arch and hwreg counterparts.
    let rsb_fallback = !boot_opts.x86_disable_spec_mitigations
        && vendor == X86VendorList::X86_VENDOR_INTEL
        && super::intel::x86_intel_cpu_has_rsb_fallback(&RealCpuId, &RealMsrAccess);
    // TODO(https://fxbug.dev/42108888, https://fxbug.dev/42072538): Consider whether a process can
    // opt-out of an IBPB on switch, either on switch-in (ex: its compiled with a retpoline) or
    // switch-out (ex: it promises not to attack the next process).
    // TODO(https://fxbug.dev/42108888, https://fxbug.dev/42072538): Should we have an individual
    // knob for IBPB?
    let should_ibpb_on_ctxt_switch = !boot_opts.x86_disable_spec_mitigations && has_ibpb_val;

    let has_retbleed = match vendor {
        // TODO(https://fxbug.dev/42076199): Enumerate Intel CPUs affected by RETBLEED.
        X86VendorList::X86_VENDOR_INTEL => false,
        X86VendorList::X86_VENDOR_AMD => super::amd::x86_amd_has_retbleed(),
        X86VendorList::X86_VENDOR_UNKNOWN => false,
    };

    // SAFETY: Single-threaded pre-VM boot on CPU 0 before secondary CPUs start.
    unsafe {
        let pcid_enabled = x86_feature_test(X86_FEATURE_PCID)
            && G_X86_FEATURE_INVPCID.get().read()
            && boot_opts.x86_enable_pcid;

        G_HAS_SWAPGS_BUG.get().write(has_swapgs_bug);
        G_HAS_MD_CLEAR.get().write(has_md_clear);
        G_HAS_MDS_TAA.get().write(has_mds_taa);
        G_MD_CLEAR_ON_USER_RETURN.get().write(md_clear_on_user_return);
        G_HAS_SPEC_CTRL.get().write(has_spec_ctrl);
        G_HAS_SSB.get().write(has_ssb);
        G_HAS_SSBD.get().write(has_ssbd);
        G_SSB_MITIGATED.get().write(ssb_mitigated);
        G_HAS_IBPB.get().write(has_ibpb_val);
        G_HAS_ENHANCED_IBRS.get().write(has_enhanced_ibrs);
        G_HAS_MELTDOWN.get().write(has_meltdown);
        G_HAS_L1TF.get().write(has_l1tf);
        G_L1D_FLUSH_ON_VMENTRY.get().write(l1d_flush_on_vmentry);
        G_RAS_FILL_ON_CTXT_SWITCH.get().write(ras_fill_on_ctxt_switch);
        G_CPU_VULNERABLE_TO_RSB_UNDERFLOW.get().write(rsb_fallback);
        G_SHOULD_IBPB_ON_CTXT_SWITCH.get().write(should_ibpb_on_ctxt_switch);
        G_HAS_RETBLEED.get().write(has_retbleed);
        G_X86_FEATURE_PCID_ENABLED.get().write(pcid_enabled);
    }
}

/// Invoked on each CPU during boot, after platform init has taken place.
///
/// After a turbo state change, the cached `CPUID` leaf `0x6` is refreshed atomically, so
/// `x86_feature_test` observes the updated turbo feature bits.
///
/// # Safety
///
/// The caller must ensure that this function is called at CPL 0 on the target online CPU (once on
/// CPU 0 during boot, and once per bringup on secondary CPUs) with `%gs.base` initialized and
/// thread migration prevented, after [`cpu_feature_init`], topology initialization, and
/// [`super::pv::PvEoi::init_all`] have completed.
pub(super) unsafe fn cpu_feature_late_init_percpu() {
    let on_boot_cpu = crate::arch_rs::curr_cpu_num() == 0;
    let topology = crate::topology::get_system_topology();
    let boot_opts = BootOptions::get();
    let ht_disabled = (topology.processor_count() == topology.logical_processor_count())
        || !boot_opts.smp_ht_enabled;

    let cpuid = BootCpuidIo;

    // Same reasoning as was done in `cpu_feature_init()` for the boot CPU.
    if !boot_opts.x86_disable_spec_mitigations && !on_boot_cpu {
        disable_tsx(cpuid, IA32_ARCH_CAPABILITIES, IA32_TSX_CTRL);
    }

    // Spectre v2 hardware-related mitigations; retpolines may further be used,
    // which is taken care of by the code-patching engine.
    let mut stibp_enabled = false;
    if !boot_opts.x86_disable_spec_mitigations {
        let mut spectre_v2_mitigation =
            get_preferred_spectre_v2_mitigation(&cpuid, &IA32_ARCH_CAPABILITIES);
        if spectre_v2_mitigation == SpectreV2Mitigation::IbpbRetpolineStibp && ht_disabled {
            spectre_v2_mitigation = SpectreV2Mitigation::IbpbRetpoline;
        }
        match spectre_v2_mitigation {
            SpectreV2Mitigation::Ibrs => {
                // Enhanced IBRS
                enable_ibrs(&cpuid, &SPECULATION_CONTROL);
            }
            SpectreV2Mitigation::IbpbRetpoline => {}
            SpectreV2Mitigation::IbpbRetpolineStibp => {
                // Enable STIBP for added cross-hyperthread security.
                stibp_enabled = true;
                enable_stibp(&cpuid, &SPECULATION_CONTROL);
            }
        }
    }

    let vendor = x86_get_vendor();
    let model = x86_get_model();

    // AMD-SB-1045: On certain processors (Zen, Zen2), when a thread halts, its Return Stack Buffer
    // entries become available to the other hyperthread; the contents of the entries aren't
    // cleared, however. On these processors, clear the return stack buffer before entering MWAIT or
    // halt to prevent the hyperbuddy from consuming our (stale) RSB entries.
    if !boot_opts.x86_disable_spec_mitigations
        && vendor == X86VendorList::X86_VENDOR_AMD
        && model.display_family == 0x17
        && !ht_disabled
    {
        // `Ordering::Relaxed` suffices: monotonic `false -> true` latch set before userspace starts
        // (and before a secondary CPU enters its idle loop), with no dependent shared memory state.
        G_CPU_VULNERABLE_TO_RSB_CROSS_THREAD.store(true, Ordering::Relaxed);
    }

    // RETbleed mitigations
    // Some RETbleed mitigations may overlap with Spectre V2 mitigations.
    // SAFETY: `G_HAS_RETBLEED` was written once on CPU 0 in `cpu_feature_init` before SMP bringup.
    let has_retbleed = unsafe { G_HAS_RETBLEED.get().read() };
    if !boot_opts.x86_disable_spec_mitigations && has_retbleed {
        if vendor == X86VendorList::X86_VENDOR_AMD {
            if has_stibp(&cpuid, false) && !stibp_enabled && !ht_disabled {
                stibp_enabled = true;
                enable_stibp(&cpuid, &SPECULATION_CONTROL);
            }
            // SAFETY: `vendor == X86_VENDOR_AMD` establishes an AMD processor, `early_init_percpu`
            // initialized `X86_FEATURE_HYPERVISOR`, and the caller guarantees CPL 0; `model` is
            // the boot CPU's model info, and the kernel assumes all CPUs share the boot CPU's
            // family and model.
            unsafe { super::amd::x86_amd_zen2_retbleed_mitigation(&model) };
        }
        if vendor == X86VendorList::X86_VENDOR_INTEL {
            // TODO(https://fxbug.dev/42076199): Mitigate RETbleed on Intel processors.
        }
    }

    if stibp_enabled {
        // `Ordering::Relaxed` suffices: standalone diagnostic flag with no dependent memory state.
        G_STIBP_ENABLED.store(true, Ordering::Relaxed);
    }

    // Mitigate Spectre v4 (Speculative Store Bypass) if requested.
    if x86_cpu_should_mitigate_ssb()
        && !mitigate_x86_ssb_bug(
            &cpuid,
            &SPECULATION_CONTROL,
            &VIRTUAL_SPECULATION_CONTROL,
            &AMD_LOAD_STORE_CONFIGURATION,
        )
    {
        kprintln!("failed to mitigate SSB (Speculative Store Bypass) vulnerability");
    }

    // Enable/disable Turbo on the processor. Leaf 0x6 reflects this CPU's live Turbo state and
    // the boot CPU may refresh the cached copy concurrently, so query this CPU directly.
    if set_x86_cpu_turbo_state(
        &DirectCpuid {},
        &IA32_MISC_ENABLE,
        &MSRC001_0015,
        boot_opts.x86_turbo,
    ) {
        // Since IA32_MISC_ENABLE may be updated and leaf 0x6 references the
        // former's state, repopulate leaf 0x6 in the boot CPUID cache.
        if on_boot_cpu {
            let res6 = __cpuid_count(X86CpuidLeafNum::ThermalAndPower as u32, 0);
            write_cpuid_leaf_6(leaf_from_cpuid(res6));
            kprintln!(
                "Turbo performance boost: {:s}",
                if boot_opts.x86_turbo { "enabled" } else { "disabled" }
            );
        }
    } else if on_boot_cpu {
        kprintln!("Turbo performance boost: unsupported");
    }

    // Set up hardware-controlled performance states.
    if boot_opts.x86_hwp {
        let mut real_msr = RealMsrAccess;
        super::hwp::intel_hwp_init(&RealCpuId, &mut real_msr, boot_opts.x86_hwp_policy);
    }

    // If we are running under a hypervisor and paravirtual EOI (PV_EOI) is available, enable it.
    if x86_hypervisor_has_pv_eoi() {
        let mut msr_access = RealMsrAccess;
        super::pv::PvEoi::get().enable(&mut msr_access);
    }
}

/// Prints the cache hierarchy reported by `CPUID`.
///
/// Kept out of line so that the `CpuCacheInfo` values built here do not inflate the stack frame of
/// `feature_debug`.
#[inline(never)]
fn print_cache_info(io: &impl Cpuid) {
    kprintln!("==== X86 CACHE INFO ====");
    kprintln!(
        "{:<5s} | {:<11s} | {:<10s} | {:<5s} | {:<6s} |",
        "Level",
        "Type",
        "Size (KiB)",
        "Sets",
        "Assoc."
    );
    if let Some(caches) = &CpuCacheInfo::from_cpuid(io) {
        for cache in caches.as_slice() {
            kprintln!(
                "L{:<4} | {:<11s} | {:<10} | {:<5} | {:<6} |",
                cache.level,
                cache.cache_type.as_str(),
                cache.size_kb,
                cache.number_of_sets,
                cache.ways_of_associativity
            );
        }
    }
    kprintln!("");
}

/// Prints CPU cache, vendor, microarchitecture, feature flags, and security properties to the
/// kernel console.
pub(super) fn feature_debug() {
    kprintln!("");

    let io = BootCpuidIo;
    print_cache_info(&io);

    let vendor_enum = VendorString::from_cpuid(io).vendor();
    let version = io.read(VERSION_INFO);
    let uarch = version.microarchitecture(vendor_enum);
    let proc_name = ProcessorName::from_cpuid(&io);
    kprintln!("Vendor: {:s}", vendor_enum.as_str());
    kprintln!("Microarchitecture: {:s}", uarch.as_str());
    kprintln!("Processor: {:s}", proc_name.name());
    {
        let hypervisor = HypervisorName::from_cpuid(&io);
        let name = hypervisor.name();
        let name = if name.is_empty() { "None" } else { name };
        kprintln!("Hypervisor: {:s}", name);
    }

    kprintln!(
        "Family/Model/Stepping: {:#x}/{:#x}/{:#x}",
        version.family(),
        version.model(),
        version.stepping()
    );
    kprintln!("Patch level: {:x}", x86_get_model().patch_level);

    let mut col: usize = 0;
    let mut print_feature = |name: &str, value: u32| {
        if !name.is_empty() && value != 0 {
            let prefix = if col != 0 { ", " } else { "" };
            kprint!("{:s}{:s}", prefix, name);
            col += prefix.len() + name.len();
            if col >= 80 {
                kprintln!("");
                col = 0;
            }
        }
    };

    kprintln!("\nFeatures:");
    for (meta, val) in io.read(FEATURE_FLAGS_C).iter() {
        print_feature(meta.name, val);
    }
    for (meta, val) in io.read(FEATURE_FLAGS_D).iter() {
        print_feature(meta.name, val);
    }
    for (meta, val) in io.read(EXTENDED_FEATURES_B).iter() {
        print_feature(meta.name, val);
    }
    // TODO(https://fxbug.dev/42147424): Print when we can afford to.
    // io.read(AMD_FEATURE_FLAGS_C).iter()...
    kprintln!("");

    // Print synthetic 'features'/properties.
    let mut prop_col: usize = 0;
    let mut print_property = |property: &str, print: bool| {
        if print {
            let prefix = if prop_col != 0 { ", " } else { "" };
            kprint!("{:s}{:s}", prefix, property);
            prop_col += prefix.len() + property.len();
            if prop_col >= 80 {
                kprintln!("");
                prop_col = 0;
            }
        }
    };

    kprintln!("\nProperties:");
    // SAFETY: All `BootStatic` feature, mitigation, and hypervisor flags below are written only on
    // CPU 0 during single-threaded `early_init_percpu` and `cpu_feature_init` before secondary CPUs
    // start, and are read-only thereafter.
    let (
        has_meltdown,
        has_l1tf,
        has_mds_taa,
        has_md_clear,
        md_clear_on_user_return,
        has_swapgs_bug,
        pcid_enabled,
        has_spec_ctrl,
        has_ssb,
        has_ssbd,
        ssb_mitigated,
        has_ibpb,
        l1d_flush_on_vmentry,
        should_ibpb_on_ctxt_switch,
        ras_fill_on_ctxt_switch,
        has_enhanced_ibrs,
        has_retbleed,
        has_pv_clock,
        has_pv_eoi,
        has_pv_ipi,
    ) = unsafe {
        (
            G_HAS_MELTDOWN.get().read(),
            G_HAS_L1TF.get().read(),
            G_HAS_MDS_TAA.get().read(),
            G_HAS_MD_CLEAR.get().read(),
            G_MD_CLEAR_ON_USER_RETURN.get().read(),
            G_HAS_SWAPGS_BUG.get().read(),
            G_X86_FEATURE_PCID_ENABLED.get().read(),
            G_HAS_SPEC_CTRL.get().read(),
            G_HAS_SSB.get().read(),
            G_HAS_SSBD.get().read(),
            G_SSB_MITIGATED.get().read(),
            G_HAS_IBPB.get().read(),
            G_L1D_FLUSH_ON_VMENTRY.get().read(),
            G_SHOULD_IBPB_ON_CTXT_SWITCH.get().read(),
            G_RAS_FILL_ON_CTXT_SWITCH.get().read(),
            G_HAS_ENHANCED_IBRS.get().read(),
            G_HAS_RETBLEED.get().read(),
            HYPERVISOR_HAS_PV_CLOCK.get().read(),
            HYPERVISOR_HAS_PV_EOI.get().read(),
            HYPERVISOR_HAS_PV_IPI.get().read(),
        )
    };
    print_property("meltdown", has_meltdown);
    print_property("l1tf", has_l1tf);
    print_property("mds/taa", has_mds_taa);
    print_property("md_clear", has_md_clear);
    print_property("md_clear_user_return", md_clear_on_user_return);
    print_property("swapgs_bug", has_swapgs_bug);
    print_property("pcid_good", pcid_enabled);
    print_property("spec_ctrl", has_spec_ctrl);
    print_property("ssb", has_ssb);
    print_property("ssbd", has_ssbd);
    print_property("ssb_mitigated", ssb_mitigated);
    print_property("ibpb", has_ibpb);
    print_property("l1d_flush_on_vmentry", l1d_flush_on_vmentry);
    print_property("ibpb_ctxt_switch", should_ibpb_on_ctxt_switch);
    print_property("ras_fill", ras_fill_on_ctxt_switch);
    print_property("enhanced_ibrs", has_enhanced_ibrs);
    print_property("retpoline", x86_feature_bindings::kX86KernelRetpoline);
    print_property("jcc_fix", x86_feature_bindings::kX86KernelJccWorkaround);
    print_property("harden_sls", x86_feature_bindings::kX86HardenSls);
    print_property("retbleed", has_retbleed);
    // `Ordering::Relaxed` suffices for standalone diagnostic flags with no dependent memory.
    print_property("stibp_enabled", G_STIBP_ENABLED.load(Ordering::Relaxed));
    print_property("zero_call_used_regs", x86_feature_bindings::kX86ZeroCallUsedRegs);
    print_property(
        "cpu_vulnerable_to_rsb_cross_thread",
        G_CPU_VULNERABLE_TO_RSB_CROSS_THREAD.load(Ordering::Relaxed),
    );
    if io.supports(PERFORMANCE_MONITORING_A) {
        let eax = io.read(PERFORMANCE_MONITORING_A);
        let edx = io.read(PERFORMANCE_MONITORING_D);
        if eax.version() > 0 {
            kprintln!(
                "\narch_pmu version {} general purpose counters {} fixed counters {}",
                eax.version(),
                eax.num_general_counters(),
                edx.num_fixed_counters()
            );
        }
    }
    print_property("hyp_pv_clock", has_pv_clock);
    print_property("hyp_pv_eoi", has_pv_eoi);
    print_property("hyp_pv_ipi", has_pv_ipi);
    kprintln!("\n");
}

/// The highest priority mechanism to determine the APIC frequency.
extern "C" fn default_apic_freq() -> u64 {
    // The APIC frequency is the core crystal clock frequency if it is
    // enumerated in the CPUID leaf 0x15, or the processor's bus clock
    // frequency.
    if let Some(tsc_leaf) = x86_get_cpuid_leaf(X86CpuidLeafNum::Tsc)
        && tsc_leaf.c != 0
    {
        return u64::from(tsc_leaf.c);
    }
    0
}

/// Computes the fallback APIC frequency for cores with a constant bus frequency.
///
/// From Intel SDMv3 section 19.7.3 (Determining the Processor Base Frequency).
/// For cores that have a hard coded bus frequency or crystal clock,
/// fall back to this value if cpuid 15h doesn't fully specify it and we're not
/// running in a hypervisor.
fn apic_freq_constant_fallback(hardcoded_apic_freq: u64) -> u64 {
    let v = default_apic_freq();
    if v != 0 {
        return v;
    }
    if x86_feature_test(X86_FEATURE_HYPERVISOR) {
        return 0;
    }
    hardcoded_apic_freq
}

/// From Intel SDMv3 section 19.7.3 (Determining the Processor Base Frequency).
extern "C" fn skl_apic_freq() -> u64 {
    apic_freq_constant_fallback(24 * 1000 * 1000)
}

/// From Intel SDMv3 section 19.7.3 (Determining the Processor Base Frequency).
extern "C" fn skl_x_apic_freq() -> u64 {
    apic_freq_constant_fallback(25 * 1000 * 1000)
}

/// From Intel SDMv3 section 19.7.3 (Determining the Processor Base Frequency).
extern "C" fn bdw_apic_freq() -> u64 {
    apic_freq_constant_fallback(100 * 1000 * 1000)
}

extern "C" fn bulldozer_apic_freq() -> u64 {
    // 15h BKDG documents that it is 100MHz.
    apic_freq_constant_fallback(100 * 1000 * 1000)
}

extern "C" fn unknown_freq() -> u64 {
    0
}

extern "C" fn intel_tsc_freq() -> u64 {
    let config = x86_get_microarch_config().expect("x86_microarch_config not initialized");
    let get_apic_freq = config.get_apic_freq.expect("get_apic_freq not initialized");
    // SAFETY: Per `x86_get_microarch_config`'s safety-usable invariant, `get_apic_freq` has no
    // preconditions and is always safe to call.
    let core_crystal_clock_freq = unsafe { get_apic_freq() };

    // If this leaf is present, then 19.7.3 (Determining the Processor Base
    // Frequency) documents this as the nominal TSC frequency.
    if let Some(tsc_leaf) = x86_get_cpuid_leaf(X86CpuidLeafNum::Tsc)
        && tsc_leaf.a != 0
    {
        return (core_crystal_clock_freq * u64::from(tsc_leaf.b)) / u64::from(tsc_leaf.a);
    }
    0
}

fn amd_compute_p_state_clock(p_state_msr: u64) -> u64 {
    // Is it valid?
    if (p_state_msr & (1u64 << 63)) == 0 {
        return 0;
    }

    // Different AMD microarchitectures use slightly different formulas to compute
    // the effective clock rate of a P state.
    let cfg = x86_get_microarch_config().expect("x86_microarch_config not initialized");
    match cfg.x86_microarch {
        X86MicroarchList::X86_MICROARCH_AMD_BULLDOZER
        | X86MicroarchList::X86_MICROARCH_AMD_JAGUAR => {
            let did = (p_state_msr >> 6) & 0x7;
            let fid = p_state_msr & 0x3f;
            (100 * (fid + 0x10) / (1u64 << did)) * 1000 * 1000
        }
        X86MicroarchList::X86_MICROARCH_AMD_ZEN => {
            let fid = p_state_msr & 0xff;
            (fid * 25) * 1000 * 1000
        }
        _ => 0,
    }
}

/// Computes the nominal Zen TSC frequency from `P0_STATE_MSR` (`0xc001_0064`).
///
/// Installed as `get_tsc_freq`, which safe callers (`x86_lookup_tsc_freq`) may invoke from any
/// thread context, so this function validates the remaining `read_msr_safe` preconditions at
/// runtime rather than requiring them of its callers.
///
/// # Safety
///
/// The caller must ensure that this function is called at CPL 0 with `%gs.base` initialized on an
/// AMD Zen processor.
unsafe extern "C" fn zen_tsc_freq() -> u64 {
    const P0_STATE_MSR: u32 = 0xc001_0064; // base P-state MSR
    // `#GP` recovery in `read_msr_safe` goes through `x86_exception_handler` ->
    // `int_handler_start`, which requires a current thread (implying `x86_init_percpu` loaded the
    // kernel IDT on this CPU) and `!blocking_disallowed()`. Re-enabling preemption below may
    // preempt, which must not happen while the caller has interrupts disabled, so callers with
    // interrupts disabled must already hold preemption disabled. Every current caller satisfies
    // all of these, so failing this check indicates misuse.
    let can_recover_from_gpf = !crate::kernel::thread::current_get().is_null()
        && !crate::arch_rs::blocking_disallowed()
        && (!crate::arch_rs::ints_disabled() || crate::kernel::thread::preempt_disable_count() > 0);
    debug_assert!(can_recover_from_gpf);
    if !can_recover_from_gpf {
        core::hint::cold_path();
        return 0;
    }
    // `read_msr_safe` also requires interrupts or preemption to be disabled in thread context so
    // the thread cannot migrate before `#GP` recovery completes. Disabling preemption also keeps
    // the `#GP` handler from preempting; since the preempt disable count was already non-zero if
    // interrupts are disabled, dropping the guard can only preempt with interrupts enabled.
    let _preempt_disabler = AutoPreemptDisabler::new();
    // According to the Family 17h PPR, the first P-state MSR is indeed
    // P0 state and appears to be experimentally so.
    // SAFETY: The caller guarantees CPL 0 with `%gs.base` initialized on an AMD Zen processor,
    // where reading `0xc001_0064` has no side effects; a non-null current thread implies
    // `x86_gpf_handler` is installed, `!blocking_disallowed()` was checked above, and
    // `_preempt_disabler` disables preemption across the fault-recoverable `rdmsr`.
    let Ok(p0_state) = (unsafe { read_msr_safe(P0_STATE_MSR) }) else {
        return 0;
    };
    amd_compute_p_state_clock(p0_state)
}

extern "C" fn unknown_reboot_system() {}

extern "C" fn unknown_reboot_reason(reason: u64) {
    // Write the boot reason to cmos, which should be present on all PCs.
    crate::platform_pc::cmos_bootbyte::bootbyte_set_reason(reason);
}

/// Triggers a system reset via chipset Reset Control Register port `0xcf9`.
///
/// # Safety
///
/// The caller must ensure that this function is called at CPL 0 while resetting the system, on a
/// platform whose `X86MicroarchConfig` installed this function.
unsafe extern "C" fn cf9_reboot_system() {
    // 100-Series Chipset Reset Control Register: CPU + SYS Reset
    // AMD Bulldozer+ defines this the same way
    // SAFETY: Port `0xcf9` is the chipset Reset Control Register on every platform whose config
    // installs this function, and the caller guarantees CPL 0 while resetting the system. Writing
    // `0x06` either resets the platform or is ignored (e.g. under some hypervisors); neither
    // outcome affects memory safety.
    unsafe { outp(0xcf9, 0x06) };
}

/// Records `reason` in CMOS and triggers a system reset via port `0xcf9`.
///
/// # Safety
///
/// The caller must ensure that this function is called at CPL 0 while resetting the system, on a
/// platform whose `X86MicroarchConfig` installed this function.
unsafe extern "C" fn cf9_reboot_reason(reason: u64) {
    // Write the boot reason to cmos, which should be present on all PCs.
    crate::platform_pc::cmos_bootbyte::bootbyte_set_reason(reason);

    // 100-Series Chipset Reset Control Register: CPU + SYS Reset
    // AMD Bulldozer+ defines this the same way
    // SAFETY: Port `0xcf9` is the chipset Reset Control Register on every platform whose config
    // installs this function, and the caller guarantees CPL 0 while resetting the system. Writing
    // `0x02` only clears the reset sequence, reading the register has no side effects, and writing
    // `0x06` either resets the platform or is ignored (e.g. under some hypervisors); none of these
    // affect memory safety.
    unsafe {
        // Clear PCI reset sequence.
        outp(0xcf9, 0x02);
        // Discarded reads acting as a small delay on the bus.
        let _ = inp(0xcf9);
        let _ = inp(0xcf9);
        outp(0xcf9, 0x06);
    }
}

const EMPTY_IDLE_STATE: x86_idle_state_t =
    x86_idle_state_t { name: ptr::null(), mwait_hint: 0, exit_latency: 0, flushes_tlb: false };

const fn c1_only_idle_states() -> x86_idle_states_t {
    let mut states = [EMPTY_IDLE_STATE; X86_MAX_CSTATES];
    states[0] = x86_cstate_c1(0);
    x86_idle_states_t { states, default_state_mask: kX86IdleStateMaskC1Only }
}

// Intel microarches

static ICELAKE_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_INTEL_ICELAKE,
    get_apic_freq: Some(default_apic_freq),
    get_tsc_freq: Some(intel_tsc_freq),
    reboot_system: Some(cf9_reboot_system),
    reboot_reason: Some(cf9_reboot_reason),
    disable_c1e: true,
    idle_prefer_hlt: false,
    idle_states: c1_only_idle_states(),
};

static TIGER_LAKE_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_INTEL_TIGERLAKE,
    get_apic_freq: Some(default_apic_freq),
    get_tsc_freq: Some(intel_tsc_freq),
    reboot_system: Some(cf9_reboot_system),
    reboot_reason: Some(cf9_reboot_reason),
    disable_c1e: true,
    idle_prefer_hlt: false,
    // TODO(https://fxbug.dev/42053624): fill this in.
    idle_states: c1_only_idle_states(),
};

static ALDER_LAKE_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_INTEL_ALDERLAKE,
    get_apic_freq: Some(default_apic_freq),
    get_tsc_freq: Some(intel_tsc_freq),
    reboot_system: Some(cf9_reboot_system),
    reboot_reason: Some(cf9_reboot_reason),
    disable_c1e: true,
    idle_prefer_hlt: false,
    // TODO(https://fxbug.dev/42053624): fill this in.
    idle_states: c1_only_idle_states(),
};

static CANNON_LAKE_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_INTEL_CANNONLAKE,
    get_apic_freq: Some(default_apic_freq),
    get_tsc_freq: Some(intel_tsc_freq),
    reboot_system: Some(cf9_reboot_system),
    reboot_reason: Some(cf9_reboot_reason),
    disable_c1e: true,
    idle_prefer_hlt: false,
    idle_states: {
        let mut states = [EMPTY_IDLE_STATE; X86_MAX_CSTATES];
        // TODO(https://fxbug.dev/42110877): Read exit_latency from IRTL registers
        states[0] = x86_idle_state_t {
            name: c"C6".as_ptr(),
            mwait_hint: 0x20,
            exit_latency: 120,
            flushes_tlb: true,
        };
        states[1] = x86_cstate_c1(0);
        x86_idle_states_t { states, default_state_mask: kX86IdleStateMaskC1Only }
    },
};

static SKYLAKE_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_INTEL_SKYLAKE,
    get_apic_freq: Some(skl_apic_freq),
    get_tsc_freq: Some(intel_tsc_freq),
    reboot_system: Some(cf9_reboot_system),
    reboot_reason: Some(cf9_reboot_reason),
    disable_c1e: true,
    idle_prefer_hlt: false,
    idle_states: {
        let mut states = [EMPTY_IDLE_STATE; X86_MAX_CSTATES];
        states[0] = x86_idle_state_t {
            name: c"C10".as_ptr(),
            mwait_hint: 0x60,
            exit_latency: 890,
            flushes_tlb: true,
        };
        states[1] = x86_idle_state_t {
            name: c"C9".as_ptr(),
            mwait_hint: 0x50,
            exit_latency: 480,
            flushes_tlb: true,
        };
        states[2] = x86_idle_state_t {
            name: c"C8".as_ptr(),
            mwait_hint: 0x40,
            exit_latency: 200,
            flushes_tlb: true,
        };
        states[3] = x86_idle_state_t {
            name: c"C7s".as_ptr(),
            mwait_hint: 0x33,
            exit_latency: 124,
            flushes_tlb: true,
        };
        states[4] = x86_idle_state_t {
            name: c"C6".as_ptr(),
            mwait_hint: 0x20,
            exit_latency: 85,
            flushes_tlb: true,
        };
        states[5] = x86_idle_state_t {
            name: c"C3".as_ptr(),
            mwait_hint: 0x10,
            exit_latency: 70,
            flushes_tlb: true,
        };
        states[6] = x86_cstate_c1(0);
        x86_idle_states_t { states, default_state_mask: kX86IdleStateMaskC1Only }
    },
};

static SKYLAKE_X_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_INTEL_SKYLAKE,
    get_apic_freq: Some(skl_x_apic_freq),
    get_tsc_freq: Some(intel_tsc_freq),
    reboot_system: Some(cf9_reboot_system),
    reboot_reason: Some(cf9_reboot_reason),
    disable_c1e: true,
    idle_prefer_hlt: false,
    idle_states: c1_only_idle_states(),
};

static BROADWELL_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_INTEL_BROADWELL,
    get_apic_freq: Some(bdw_apic_freq),
    get_tsc_freq: Some(intel_tsc_freq),
    reboot_system: Some(cf9_reboot_system),
    reboot_reason: Some(cf9_reboot_reason),
    disable_c1e: true,
    idle_prefer_hlt: false,
    idle_states: c1_only_idle_states(),
};

static HASWELL_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_INTEL_HASWELL,
    get_apic_freq: Some(bdw_apic_freq),
    get_tsc_freq: Some(intel_tsc_freq),
    reboot_system: Some(cf9_reboot_system),
    reboot_reason: Some(cf9_reboot_reason),
    disable_c1e: true,
    idle_prefer_hlt: false,
    idle_states: c1_only_idle_states(),
};

static IVYBRIDGE_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_INTEL_IVY_BRIDGE,
    get_apic_freq: Some(bdw_apic_freq),
    get_tsc_freq: Some(intel_tsc_freq),
    reboot_system: Some(unknown_reboot_system),
    reboot_reason: Some(unknown_reboot_reason),
    disable_c1e: true,
    idle_prefer_hlt: false,
    idle_states: c1_only_idle_states(),
};

static SANDYBRIDGE_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_INTEL_SANDY_BRIDGE,
    get_apic_freq: Some(bdw_apic_freq),
    get_tsc_freq: Some(intel_tsc_freq),
    reboot_system: Some(unknown_reboot_system),
    reboot_reason: Some(unknown_reboot_reason),
    disable_c1e: true,
    idle_prefer_hlt: false,
    idle_states: c1_only_idle_states(),
};

static WESTMERE_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_INTEL_WESTMERE,
    get_apic_freq: Some(default_apic_freq),
    get_tsc_freq: Some(intel_tsc_freq),
    reboot_system: Some(unknown_reboot_system),
    reboot_reason: Some(unknown_reboot_reason),
    disable_c1e: true,
    idle_prefer_hlt: false,
    idle_states: c1_only_idle_states(),
};

static NEHALEM_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_INTEL_NEHALEM,
    get_apic_freq: Some(default_apic_freq),
    get_tsc_freq: Some(intel_tsc_freq),
    reboot_system: Some(unknown_reboot_system),
    reboot_reason: Some(unknown_reboot_reason),
    disable_c1e: true,
    idle_prefer_hlt: false,
    idle_states: c1_only_idle_states(),
};

static SILVERMONT_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_INTEL_SILVERMONT,
    get_apic_freq: Some(default_apic_freq),
    get_tsc_freq: Some(intel_tsc_freq),
    reboot_system: Some(unknown_reboot_system),
    reboot_reason: Some(unknown_reboot_reason),
    disable_c1e: false,
    idle_prefer_hlt: false,
    idle_states: c1_only_idle_states(),
};

static GOLDMONT_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_INTEL_GOLDMONT,
    get_apic_freq: Some(default_apic_freq),
    get_tsc_freq: Some(intel_tsc_freq),
    reboot_system: Some(cf9_reboot_system),
    reboot_reason: Some(cf9_reboot_reason),
    disable_c1e: false,
    // [APL30] Apollo Lake SOCs (Goldmont) have an errata which causes stores to not always wake
    // MWAIT-ing cores. Prefer HLT to avoid the issue.
    idle_prefer_hlt: true,
    idle_states: c1_only_idle_states(),
};

static GOLDMONT_PLUS_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_INTEL_GOLDMONT_PLUS,
    get_apic_freq: Some(default_apic_freq),
    get_tsc_freq: Some(intel_tsc_freq),
    reboot_system: Some(unknown_reboot_system),
    reboot_reason: Some(unknown_reboot_reason),
    disable_c1e: false,
    idle_prefer_hlt: false,
    idle_states: {
        let mut states = [EMPTY_IDLE_STATE; X86_MAX_CSTATES];
        // TODO(https://fxbug.dev/42110877): Read C6 and deeper latency from IRTL registers
        states[0] = x86_idle_state_t {
            name: c"C10".as_ptr(),
            mwait_hint: 0x60,
            exit_latency: 10000,
            flushes_tlb: true,
        };
        states[1] = x86_idle_state_t {
            name: c"C9".as_ptr(),
            mwait_hint: 0x50,
            exit_latency: 2000,
            flushes_tlb: true,
        };
        states[2] = x86_idle_state_t {
            name: c"C8".as_ptr(),
            mwait_hint: 0x40,
            exit_latency: 1000,
            flushes_tlb: true,
        };
        states[3] = x86_idle_state_t {
            name: c"C7s".as_ptr(),
            mwait_hint: 0x31,
            exit_latency: 155,
            flushes_tlb: true,
        };
        states[4] = x86_idle_state_t {
            name: c"C6".as_ptr(),
            mwait_hint: 0x20,
            exit_latency: 133,
            flushes_tlb: true,
        };
        states[5] = x86_idle_state_t {
            name: c"C1E".as_ptr(),
            mwait_hint: 0x01,
            exit_latency: 10,
            flushes_tlb: false,
        };
        states[6] = x86_cstate_c1(0);
        x86_idle_states_t { states, default_state_mask: kX86IdleStateMaskC1Only }
    },
};

static INTEL_DEFAULT_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_UNKNOWN,
    get_apic_freq: Some(default_apic_freq),
    get_tsc_freq: Some(intel_tsc_freq),
    reboot_system: Some(unknown_reboot_system),
    reboot_reason: Some(unknown_reboot_reason),
    disable_c1e: false,
    idle_prefer_hlt: false,
    idle_states: c1_only_idle_states(),
};

// AMD microarches

static ZEN_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_AMD_ZEN,
    get_apic_freq: Some(unknown_freq),
    get_tsc_freq: Some(zen_tsc_freq),
    reboot_system: Some(cf9_reboot_system),
    reboot_reason: Some(cf9_reboot_reason),
    disable_c1e: false,
    // Zen SOCs save substantial power using HLT instead of MWAIT.
    // TODO(https://fxbug.dev/42139534): Use a predictor/selection to use mwait for short sleeps.
    idle_prefer_hlt: true,
    idle_states: c1_only_idle_states(),
};

static JAGUAR_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_AMD_JAGUAR,
    get_apic_freq: Some(unknown_freq),
    get_tsc_freq: Some(unknown_freq),
    reboot_system: Some(cf9_reboot_system),
    reboot_reason: Some(cf9_reboot_reason),
    disable_c1e: false,
    idle_prefer_hlt: false,
    idle_states: c1_only_idle_states(),
};

static BULLDOZER_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_AMD_BULLDOZER,
    get_apic_freq: Some(bulldozer_apic_freq),
    get_tsc_freq: Some(unknown_freq),
    reboot_system: Some(cf9_reboot_system),
    reboot_reason: Some(cf9_reboot_reason),
    disable_c1e: false,
    // Excavator SOCs in particular save substantial power using HLT instead of MWAIT
    idle_prefer_hlt: true,
    idle_states: c1_only_idle_states(),
};

static AMD_DEFAULT_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_UNKNOWN,
    get_apic_freq: Some(unknown_freq),
    get_tsc_freq: Some(unknown_freq),
    reboot_system: Some(cf9_reboot_system),
    reboot_reason: Some(cf9_reboot_reason),
    disable_c1e: false,
    idle_prefer_hlt: false,
    idle_states: c1_only_idle_states(),
};

// Unknown vendor config

static UNKNOWN_VENDOR_CONFIG: X86MicroarchConfig = X86MicroarchConfig {
    x86_microarch: X86MicroarchList::X86_MICROARCH_UNKNOWN,
    get_apic_freq: Some(unknown_freq),
    get_tsc_freq: Some(unknown_freq),
    reboot_system: Some(unknown_reboot_system),
    reboot_reason: Some(unknown_reboot_reason),
    disable_c1e: false,
    idle_prefer_hlt: false,
    idle_states: c1_only_idle_states(),
};

fn get_microarch_config_by_id(
    vendor: &ManufacturerInfo,
    processor_id: &ProcessorId,
) -> &'static X86MicroarchConfig {
    if vendor.manufacturer() == Manufacturer::Intel && processor_id.family() == 0x6 {
        // Table largely from https://en.wikichip.org/wiki/intel/cpuid
        match processor_id.model() {
            // Big cores
            0x1a /* Nehalem */
            | 0x1e /* Nehalem */
            | 0x1f /* Nehalem */
            | 0x2e /* Nehalem */ => &NEHALEM_CONFIG,
            0x25 /* Westmere */
            | 0x2c /* Westmere */
            | 0x2f /* Westmere */ => &WESTMERE_CONFIG,
            0x2a /* Sandy Bridge */
            | 0x2d /* Sandy Bridge EP */ => &SANDYBRIDGE_CONFIG,
            0x3a /* Ivy Bridge */
            | 0x3e /* Ivy Bridge EP */ => &IVYBRIDGE_CONFIG,
            0x3c /* Haswell DT */
            | 0x3f /* Haswell MB */
            | 0x45 /* Haswell ULT */
            | 0x46 /* Haswell ULX */ => &HASWELL_CONFIG,
            0x3d /* Broadwell */
            | 0x47 /* Broadwell H */
            | 0x56 /* Broadwell EP */
            | 0x4f /* Broadwell EX */ => &BROADWELL_CONFIG,
            0x4e /* Skylake Y/U */
            | 0x5e /* Skylake H/S */
            | 0x8e /* Kaby Lake Y/U, Coffee Lake, Whiskey Lake, Amber Lake, Comet Lake U */
            | 0x9e /* Kaby Lake H/S, Coffee Lake, Whiskey Lake */
            | 0xa5 /* Comet Lake S/H */ => &SKYLAKE_CONFIG,
            0x55 /* Skylake X/SP, Cascade Lake */ => &SKYLAKE_X_CONFIG,
            0x66 /* Cannon Lake U */ => &CANNON_LAKE_CONFIG,
            0x6a /* Ice Lake-SP */
            | 0x6c /* Ice Lake-DE */
            | 0x7d /* Ice Lake Y */
            | 0x7e /* Ice Lake U */ => &ICELAKE_CONFIG,
            0x8c /* Tiger Lake UP */
            | 0x8d /* Tiger Lake H */
            | 0xa7 /* Rocket Lake S */ => &TIGER_LAKE_CONFIG,
            0x97 /* Alder Lake S */
            | 0x9a /* Alder Lake H/P/U */
            | 0xb7 /* Raptor Lake S */
            | 0x8f /* Sapphire Rapids-SP */
            | 0xcf /* Emerald Rapids-SP */ => &ALDER_LAKE_CONFIG,

            // Small cores
            0x37 /* Silvermont */
            | 0x4a /* Silvermont "Cherry View" */
            | 0x4d /* Silvermont "Avoton", "Rangeley" */
            | 0x5a /* Silvermont "Anniedale" */
            | 0x5d /* Silvermont "SoFIA" */
            | 0x4c /* Airmont "Braswell", "Cherry Trail" */ => &SILVERMONT_CONFIG,
            0x5c /* Goldmont (Apollo Lake) */
            | 0x5f /* Goldmont (Denverton) */ => &GOLDMONT_CONFIG,
            0x7a /* Goldmont Plus (Gemini Lake) */
            | 0x8a /* Tremont (Lakefield) */
            | 0x96 /* Tremont (Elkhart Lake) */
            | 0x9c /* Tremont (Jasper Lake) */ => &GOLDMONT_PLUS_CONFIG,

            _ => &INTEL_DEFAULT_CONFIG,
        }
    } else if vendor.manufacturer() == Manufacturer::Amd {
        match processor_id.family() {
            0x15 => &BULLDOZER_CONFIG,
            0x16 => &JAGUAR_CONFIG,
            0x17 /* Zen 1, 2 */
            | 0x19 /* Zen 3, 4 */ => &ZEN_CONFIG,
            _ => &AMD_DEFAULT_CONFIG,
        }
    } else {
        &UNKNOWN_VENDOR_CONFIG
    }
}

/// Returns the microarchitecture configuration entry for `cpuid`.
///
/// # Safety-usable invariant
///
/// For the returned `cfg`:
/// - `cfg.get_apic_freq` has no preconditions.
/// - `cfg.get_tsc_freq` has no preconditions beyond running in the kernel (CPL 0) with `%gs.base`
///   initialized on a CPU matching `cpuid` and is safe to call from any thread context.
/// - `cfg.reboot_system` and `cfg.reboot_reason` are safe to call at CPL 0 when resetting a
///   system whose CPU matches `cpuid`.
/// - `cfg.idle_states.states` contains a terminating base `C1` state, and every populated entry up
///   to and including base `C1` has a `name` pointer that is either null or points to a valid
///   NUL-terminated `'static` C string.
#[inline]
pub(super) fn get_microarch_config(cpuid: &impl CpuId) -> &'static X86MicroarchConfig {
    let vendor = cpuid.read_manufacturer_info();
    let processor_id = cpuid.read_processor_id();
    get_microarch_config_by_id(&vendor, &processor_id)
}

/// Flushes the L1D cache on `ZX_ERR_INVALID_ARGS` or `ZX_ERR_BAD_HANDLE` syscall returns when
/// speculative execution mitigations are enabled (`x86_cpu_maybe_l1d_flush`).
///
/// # Safety
///
/// The caller must ensure that this function is called at CPL 0.
#[inline(always)]
pub(super) unsafe fn cpu_maybe_l1d_flush(syscall_return: zx_status_t) {
    // Spectre V1: If we are returning from a syscall with one of these errors, flush the entire
    // L1D cache. This prevents hostile code from reading any data the kernel brought in to cache,
    // even speculatively.
    //
    // We only flush on these errors as they are not expected in the steady state and cover most
    // expected Spectre V1 attack constructions. Most attacks will either pass in invalid indexes
    // or invalid handles, to leak table contents; ZX_ERR_INVALID_ARGS and ZX_ERR_BAD_HANDLE cover
    // those cases.
    //
    // Allowing a process to cause an L1D cache flush is low risk; the process could cycle enough
    // data through the L1 to evict + replace all data very quickly. Allowing a process to cause
    // a WBINVD, however, would be higher-risk - it flushes every cache in the system, which could
    // be very disruptive to other work; therefore we don't fall back from IA32_FLUSH_CMD to WBINVD.
    // The syscall status is tested first so that other error returns avoid loading `BootOptions`;
    // the conditions are independent.
    if (syscall_return == Status::INVALID_ARGS.into_raw()
        || syscall_return == Status::BAD_HANDLE.into_raw())
        && x86_feature_test(X86_FEATURE_L1D_FLUSH)
        && !BootOptions::get().x86_disable_spec_mitigations
    {
        core::hint::cold_path();
        // SAFETY: `X86_FEATURE_L1D_FLUSH` is present, the caller guarantees CPL 0, and `1`
        // (bit 0, `L1D_FLUSH`) is the architectural `IA32_FLUSH_CMD` command value.
        unsafe { write_msr(X86_MSR_IA32_FLUSH_CMD, 1) };
    }
}

/// x86 CPU feature and microarchitecture selection tests.
#[cfg(ktest)]
#[unittest::suite(name = "x86_feature")]
mod tests {
    use unittest::{expect_eq, expect_true};

    use super::super::cpuid::tests::{
        CPU_ID_AMD_A4_9120C, CPU_ID_CELERON_J3455, CPU_ID_CORE_I5_6260U, CPU_ID_EPYC_7B13,
        CPU_ID_THREAD_RIPPER_2970WX, CPU_ID_XEON_2690V4,
    };

    /// Verify microarchitecture config selection across Intel and AMD CPUID datasets.
    #[test]
    fn test_x64_cpu_uarch_config_selection() {
        expect_eq!(
            get_microarch_config(&CPU_ID_CORE_I5_6260U).x86_microarch,
            X86MicroarchList::X86_MICROARCH_INTEL_SKYLAKE
        );

        // Intel Xeon E5-2690 V4 is Broadwell.
        expect_eq!(
            get_microarch_config(&CPU_ID_XEON_2690V4).x86_microarch,
            X86MicroarchList::X86_MICROARCH_INTEL_BROADWELL
        );

        // Intel Celeron J3455 is Goldmont.
        expect_eq!(
            get_microarch_config(&CPU_ID_CELERON_J3455).x86_microarch,
            X86MicroarchList::X86_MICROARCH_INTEL_GOLDMONT
        );

        // AMD A4-9120C is Bulldozer.
        expect_eq!(
            get_microarch_config(&CPU_ID_AMD_A4_9120C).x86_microarch,
            X86MicroarchList::X86_MICROARCH_AMD_BULLDOZER
        );

        // AMD Ryzen Threadripper 2970WX is Zen.
        expect_eq!(
            get_microarch_config(&CPU_ID_THREAD_RIPPER_2970WX).x86_microarch,
            X86MicroarchList::X86_MICROARCH_AMD_ZEN
        );

        // AMD EPYC 7B13 (Family 0x19) is Zen 3.
        expect_eq!(
            get_microarch_config(&CPU_ID_EPYC_7B13).x86_microarch,
            X86MicroarchList::X86_MICROARCH_AMD_ZEN
        );
    }

    /// Verify `BootCpuidIo` zeroes leaves beyond maximum basic, hypervisor, and extended leaves.
    #[test]
    fn test_boot_cpuid_io_out_of_range_zeroed() {
        let io = BootCpuidIo;
        expect_true!(io.read_raw(0x3fff_ffff, 0) == CpuidRawResult::zeroed());
        expect_true!(io.read_raw(0x4fff_ffff, 0) == CpuidRawResult::zeroed());
        expect_true!(io.read_raw(0xffff_ffff, 0) == CpuidRawResult::zeroed());
    }

    /// Verify `BootCpuidIo` always serves the maximum-leaf entry of each range.
    #[test]
    fn test_boot_cpuid_io_base_leaves() {
        let io = BootCpuidIo;
        for leaf in [
            X86CpuidLeafNum::Base as u32,
            X86CpuidLeafNum::HYP_BASE as u32,
            X86CpuidLeafNum::ExtBase as u32,
        ] {
            expect_true!(io.read_raw(leaf, 0) == DirectCpuid {}.read_raw(leaf, 0));
        }
    }

    /// Verify extended max leaf is preserved and hypervisor subleaf lookup is bounded.
    #[test]
    fn test_cpuid_ext_base_and_hyp_subleaf_bounds() {
        let ext_base = x86_get_cpuid_leaf(X86CpuidLeafNum::ExtBase);
        expect_true!(ext_base.is_some_and(|leaf| leaf.a >= X86CpuidLeafNum::ExtBase as u32));
        expect_true!(get_cpuid_subleaf_raw(0x7fff_ffff, 0).is_none());
    }
}
