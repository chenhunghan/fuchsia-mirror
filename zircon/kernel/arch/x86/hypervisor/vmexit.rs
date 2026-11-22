// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Handling of VM exits.

use core::ffi::c_void;
use core::mem::{offset_of, size_of};
use core::pin::Pin;
use core::sync::atomic::Ordering;
use debug::{dprintf, ltracef};
use zx_status::Status;
use zx_types::{
    ZX_ERR_INTERNAL_INTR_KILLED, ZX_GUEST_TRAP_BELL, ZX_GUEST_TRAP_IO, ZX_GUEST_TRAP_MEM,
    ZX_TIME_INFINITE, zx_gpaddr_t, zx_packet_guest_bell_t, zx_packet_guest_io_t,
    zx_packet_guest_mem_t, zx_packet_guest_vcpu_interrupt_t, zx_packet_guest_vcpu_startup_t,
    zx_packet_guest_vcpu_t, zx_packet_guest_vcpu_type_t, zx_packet_guest_vcpu_union_t,
    zx_packet_type_t, zx_paddr_t, zx_port_packet_t, zx_vaddr_t,
};

use super::guest::MAX_GUEST_VCPUS;
use super::invalidate::{InvVpid, invvpid};
use super::pv::{
    PvClockState, pv_clock_populate_offset, pv_clock_reset_clock, pv_clock_stop_clock,
    pv_clock_update_boot_time,
};
use super::vcpu::LocalApicState;
use super::vmcs::{
    AutoVmcs, ENTRY_CTLS_64BIT_MODE, GUEST_XX_ACCESS_RIGHTS_D, GUEST_XX_ACCESS_RIGHTS_DPL_USER,
    GUEST_XX_ACCESS_RIGHTS_L, INTERRUPTIBILITY_MOV_SS_BLOCKING, INTERRUPTIBILITY_STI_BLOCKING,
    PROCBASED_CTLS2_INVPCID, VmcsField16, VmcsField32, VmcsField64, VmcsFieldXX,
    X86_MSR_IA32_VMX_ENTRY_CTLS, X86_MSR_IA32_VMX_TRUE_ENTRY_CTLS, cr0_is_invalid,
};
use super::vmx_state::GuestState;
use crate::arch_rs::set_blocking_disallowed;
use crate::arch_rs::x86::exceptions::x86_call_external_interrupt_handler;
use crate::arch_rs::x86::feature::{
    CpuidLeaf, MAX_SUPPORTED_CPUID, MAX_SUPPORTED_CPUID_EXT, X86_FEATURE_ACPI,
    X86_FEATURE_ARCH_CAPABILITIES, X86_FEATURE_AVX512BITALG, X86_FEATURE_AVX512BW,
    X86_FEATURE_AVX512CD, X86_FEATURE_AVX512DQ, X86_FEATURE_AVX512ER, X86_FEATURE_AVX512F,
    X86_FEATURE_AVX512IFMA, X86_FEATURE_AVX512PF, X86_FEATURE_AVX512QFMA, X86_FEATURE_AVX512QVNNIW,
    X86_FEATURE_AVX512VBMI, X86_FEATURE_AVX512VBMI2, X86_FEATURE_AVX512VL, X86_FEATURE_AVX512VNNI,
    X86_FEATURE_AVX512VPDQ, X86_FEATURE_DTS, X86_FEATURE_HW_FEEDBACK, X86_FEATURE_HWP,
    X86_FEATURE_HWP_ACT, X86_FEATURE_HWP_NOT, X86_FEATURE_HWP_PREF, X86_FEATURE_HYPERVISOR,
    X86_FEATURE_IBRS_IBPB, X86_FEATURE_INVPCID, X86_FEATURE_L1D_FLUSH, X86_FEATURE_LA57,
    X86_FEATURE_MON, X86_FEATURE_PDCM, X86_FEATURE_PERF_BIAS, X86_FEATURE_PLN, X86_FEATURE_PT,
    X86_FEATURE_PTM, X86_FEATURE_SEP, X86_FEATURE_SSBD, X86_FEATURE_STIBP, X86_FEATURE_TM,
    X86_FEATURE_TM2, X86_FEATURE_TOPOLOGY_LOGICAL_PROCESSOR, X86_FEATURE_TOPOLOGY_SINGLE_PROCESSOR,
    X86_FEATURE_TSC_ADJUST, X86_FEATURE_TSC_DEADLINE, X86_FEATURE_TURBO, X86_FEATURE_TURBO_MAX,
    X86_FEATURE_VMX, X86_FEATURE_X2APIC, X86CpuidBit, X86CpuidLeafNum, get_cpuid_subleaf,
};
use crate::arch_rs::x86::interrupts::{X86_INT_GP_FAULT, X86_INT_NMI, X86_INT_PLATFORM_BASE};
use crate::arch_rs::x86::lapic::{
    APIC_PHYS_BASE, IA32_APIC_BASE_BSP, IA32_APIC_BASE_X2APIC_ENABLE, IA32_APIC_BASE_XAPIC_ENABLE,
    LVT_MASKED, LVT_TIMER_MODE_MASK, LVT_TIMER_MODE_ONESHOT, LVT_TIMER_MODE_PERIODIC,
    LVT_TIMER_MODE_RESERVED, LVT_TIMER_MODE_TSC_DEADLINE, LVT_TIMER_VECTOR_MASK,
};
use crate::arch_rs::x86::pv::{
    KVM_BOOT_TIME, KVM_BOOT_TIME_OLD, KVM_FEATURE_CLOCK_SOURCE, KVM_FEATURE_CLOCK_SOURCE_OLD,
    KVM_SYSTEM_TIME_MSR, KVM_SYSTEM_TIME_MSR_OLD,
};
use crate::arch_rs::x86::registers::{
    X86_CR0_CD, X86_CR0_ET, X86_CR0_NE, X86_CR0_NW, X86_CR0_PG, X86_CR4_OSXSAVE, X86_EFER_LMA,
    X86_EFER_LME, X86_MSR_DRAM_ENERGY_STATUS, X86_MSR_DRAM_POWER_LIMIT, X86_MSR_IA32_APIC_BASE,
    X86_MSR_IA32_BIOS_SIGN_ID, X86_MSR_IA32_CSTAR, X86_MSR_IA32_MCG_CAP, X86_MSR_IA32_MCG_STATUS,
    X86_MSR_IA32_MISC_ENABLE, X86_MSR_IA32_MTRR_DEF_TYPE, X86_MSR_IA32_MTRR_FIX4K_C0000,
    X86_MSR_IA32_MTRR_FIX4K_F8000, X86_MSR_IA32_MTRR_FIX16K_80000, X86_MSR_IA32_MTRR_FIX16K_A0000,
    X86_MSR_IA32_MTRR_FIX64K_00000, X86_MSR_IA32_MTRR_PHYSBASE0, X86_MSR_IA32_MTRR_PHYSMASK9,
    X86_MSR_IA32_MTRRCAP, X86_MSR_IA32_PLATFORM_ID, X86_MSR_IA32_PRED_CMD, X86_MSR_IA32_SPEC_CTRL,
    X86_MSR_IA32_TEMPERATURE_TARGET, X86_MSR_IA32_TSC_DEADLINE, X86_MSR_PKG_ENERGY_STATUS,
    X86_MSR_PLATFORM_ENERGY_COUNTER, X86_MSR_PLATFORM_POWER_LIMIT, X86_MSR_PP0_ENERGY_STATUS,
    X86_MSR_PP0_POWER_LIMIT, X86_MSR_PP1_ENERGY_STATUS, X86_MSR_PP1_POWER_LIMIT, X86_MSR_PPERF,
    X86_MSR_RAPL_POWER_UNIT, X86_MSR_SMI_COUNT, X86_XSAVE_STATE_BIT_AVX, X86_XSAVE_STATE_BIT_SSE,
    X86_XSAVE_STATE_BIT_X87,
};
use crate::arch_rs::x86::x86::read_msr;
use crate::hypervisor::aspace::GuestPhysicalAspace;
use crate::hypervisor::ktrace::{VcpuExit, ktrace_vcpu_exit};
use crate::hypervisor::state_invalidator::Invalidate;
use crate::hypervisor::trap_map::TrapMap;
use crate::kernel::bits::{bit, bit_shift, bits, bits_shift};
use crate::kernel::percpu::PerCpu;
use crate::kernel::timer::Timer;
use crate::platform_pc::timer::{
    convert_raw_tsc_duration_to_nanoseconds, convert_raw_tsc_timestamp_to_clock_monotonic,
};
use crate::platform_rs::timer::{InstantMono, current_mono_time};

const LOCAL_TRACE: u32 = 0;

/// `ZX_ERR_INTERNAL_INTR_KILLED`, which `Status` has no constant for.
const INTERNAL_INTR_KILLED: Status = Status::err_from_raw(ZX_ERR_INTERNAL_INTR_KILLED);

/// Increments the guest statistic `$name` of the current CPU.
macro_rules! guest_stats_inc {
    ($name:ident) => {
        // Relaxed: the statistics are independent counters that do not synchronize any other
        // memory.
        PerCpu::get_current().gstats.arch.$name.fetch_add(1, Ordering::Relaxed)
    };
}

/// VM exit reasons.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExitReason(u32);

impl ExitReason {
    const EXCEPTION_OR_NMI: Self = Self(0);
    const EXTERNAL_INTERRUPT: Self = Self(1);
    const TRIPLE_FAULT: Self = Self(2);
    const INIT_SIGNAL: Self = Self(3);
    const STARTUP_IPI: Self = Self(4);
    const IO_SMI: Self = Self(5);
    const OTHER_SMI: Self = Self(6);
    const INTERRUPT_WINDOW: Self = Self(7);
    const NMI_WINDOW: Self = Self(8);
    const TASK_SWITCH: Self = Self(9);
    const CPUID: Self = Self(10);
    const GETSEC: Self = Self(11);
    const HLT: Self = Self(12);
    const INVD: Self = Self(13);
    const INVLPG: Self = Self(14);
    const RDPMC: Self = Self(15);
    const RDTSC: Self = Self(16);
    const RSM: Self = Self(17);
    const VMCALL: Self = Self(18);
    const VMCLEAR: Self = Self(19);
    const VMLAUNCH: Self = Self(20);
    const VMPTRLD: Self = Self(21);
    const VMPTRST: Self = Self(22);
    const VMREAD: Self = Self(23);
    const VMRESUME: Self = Self(24);
    const VMWRITE: Self = Self(25);
    const VMXOFF: Self = Self(26);
    const VMXON: Self = Self(27);
    const CONTROL_REGISTER_ACCESS: Self = Self(28);
    const MOV_DR: Self = Self(29);
    const IO_INSTRUCTION: Self = Self(30);
    const RDMSR: Self = Self(31);
    const WRMSR: Self = Self(32);
    const ENTRY_FAILURE_GUEST_STATE: Self = Self(33);
    const ENTRY_FAILURE_MSR_LOADING: Self = Self(34);
    const MWAIT: Self = Self(36);
    const MONITOR_TRAP_FLAG: Self = Self(37);
    const MONITOR: Self = Self(39);
    const PAUSE: Self = Self(40);
    const ENTRY_FAILURE_MACHINE_CHECK: Self = Self(41);
    const TPR_BELOW_THRESHOLD: Self = Self(43);
    const APIC_ACCESS: Self = Self(44);
    const VIRTUALIZED_EOI: Self = Self(45);
    const ACCESS_GDTR_OR_IDTR: Self = Self(46);
    const ACCESS_LDTR_OR_TR: Self = Self(47);
    const EPT_VIOLATION: Self = Self(48);
    const EPT_MISCONFIGURATION: Self = Self(49);
    const INVEPT: Self = Self(50);
    const RDTSCP: Self = Self(51);
    const VMX_PREEMPT_TIMER_EXPIRED: Self = Self(52);
    const INVVPID: Self = Self(53);
    const WBINVD_OR_WBNOINVD: Self = Self(54);
    const XSETBV: Self = Self(55);
    const APIC_WRITE: Self = Self(56);
    const RDRAND: Self = Self(57);
    const INVPCID: Self = Self(58);
    const VMFUNC: Self = Self(59);
    const ENCLS: Self = Self(60);
    const RDSEED: Self = Self(61);
    const PAGE_MODIFICATION_LOG_FULL: Self = Self(62);
    const XSAVES: Self = Self(63);
    const XRSTORS: Self = Self(64);
    const SPP_EVENT: Self = Self(66);
    const UMWAIT: Self = Self(67);
    const TPAUSE: Self = Self(68);
    const LOADIWKEY: Self = Self(69);
}

/// Returns the name of `exit_reason`.
fn exit_reason_name(exit_reason: ExitReason) -> &'static str {
    macro_rules! exit_reason_name {
        ($($name:ident),* $(,)?) => {
            match exit_reason {
                $(ExitReason::$name => stringify!($name),)*
                _ => "UNKNOWN",
            }
        };
    }
    exit_reason_name!(
        EXCEPTION_OR_NMI,
        EXTERNAL_INTERRUPT,
        TRIPLE_FAULT,
        INIT_SIGNAL,
        STARTUP_IPI,
        IO_SMI,
        OTHER_SMI,
        INTERRUPT_WINDOW,
        NMI_WINDOW,
        TASK_SWITCH,
        CPUID,
        GETSEC,
        HLT,
        INVD,
        INVLPG,
        RDPMC,
        RDTSC,
        RSM,
        VMCALL,
        VMCLEAR,
        VMLAUNCH,
        VMPTRLD,
        VMPTRST,
        VMREAD,
        VMRESUME,
        VMWRITE,
        VMXOFF,
        VMXON,
        CONTROL_REGISTER_ACCESS,
        MOV_DR,
        IO_INSTRUCTION,
        RDMSR,
        WRMSR,
        ENTRY_FAILURE_GUEST_STATE,
        ENTRY_FAILURE_MSR_LOADING,
        MWAIT,
        MONITOR_TRAP_FLAG,
        MONITOR,
        PAUSE,
        ENTRY_FAILURE_MACHINE_CHECK,
        TPR_BELOW_THRESHOLD,
        APIC_ACCESS,
        VIRTUALIZED_EOI,
        ACCESS_GDTR_OR_IDTR,
        ACCESS_LDTR_OR_TR,
        EPT_VIOLATION,
        EPT_MISCONFIGURATION,
        INVEPT,
        RDTSCP,
        VMX_PREEMPT_TIMER_EXPIRED,
        INVVPID,
        WBINVD_OR_WBNOINVD,
        XSETBV,
        APIC_WRITE,
        RDRAND,
        INVPCID,
        VMFUNC,
        ENCLS,
        RDSEED,
        PAGE_MODIFICATION_LOG_FULL,
        XSAVES,
        XRSTORS,
        SPP_EVENT,
        UMWAIT,
        TPAUSE,
        LOADIWKEY,
    )
}

/// VM exit interruption type.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InterruptionType(u8);

impl InterruptionType {
    const EXTERNAL_INTERRUPT: Self = Self(0);
    const NON_MASKABLE_INTERRUPT: Self = Self(2);
    const HARDWARE_EXCEPTION: Self = Self(3);
    const SOFTWARE_EXCEPTION: Self = Self(6);
}

/// X2APIC MSR addresses from Volume 3, Section 10.12.1.2.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct X2ApicMsr(u32);

impl X2ApicMsr {
    const ID: Self = Self(0x802);
    const VERSION: Self = Self(0x803);
    const EOI: Self = Self(0x80b);
    const TPR: Self = Self(0x808);
    const LDR: Self = Self(0x80d);
    const SVR: Self = Self(0x80f);
    const ISR_31_0: Self = Self(0x810);
    const ISR_63_32: Self = Self(0x811);
    const ISR_95_64: Self = Self(0x812);
    const ISR_127_96: Self = Self(0x813);
    const ISR_159_128: Self = Self(0x814);
    const ISR_191_160: Self = Self(0x815);
    const ISR_223_192: Self = Self(0x816);
    const ISR_255_224: Self = Self(0x817);
    const TMR_31_0: Self = Self(0x818);
    const TMR_63_32: Self = Self(0x819);
    const TMR_95_64: Self = Self(0x81a);
    const TMR_127_96: Self = Self(0x81b);
    const TMR_159_128: Self = Self(0x81c);
    const TMR_191_160: Self = Self(0x81d);
    const TMR_223_192: Self = Self(0x81e);
    const TMR_255_224: Self = Self(0x81f);
    const IRR_31_0: Self = Self(0x820);
    const IRR_63_32: Self = Self(0x821);
    const IRR_95_64: Self = Self(0x822);
    const IRR_127_96: Self = Self(0x823);
    const IRR_159_128: Self = Self(0x824);
    const IRR_191_160: Self = Self(0x825);
    const IRR_223_192: Self = Self(0x826);
    const IRR_255_224: Self = Self(0x827);
    const ESR: Self = Self(0x828);
    const LVT_CMCI: Self = Self(0x82f);
    const ICR: Self = Self(0x830);
    const LVT_TIMER: Self = Self(0x832);
    const LVT_THERMAL_SENSOR: Self = Self(0x833);
    const LVT_MONITOR: Self = Self(0x834);
    const LVT_LINT0: Self = Self(0x835);
    const LVT_LINT1: Self = Self(0x836);
    const LVT_ERROR: Self = Self(0x837);
    const INITIAL_COUNT: Self = Self(0x838);
    const DCR: Self = Self(0x83e);
    const SELF_IPI: Self = Self(0x83f);
}

/// The delivery mode of an interrupt command.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InterruptDeliveryMode(u8);

impl InterruptDeliveryMode {
    const FIXED: Self = Self(0);
    const SMI: Self = Self(2);
    const NMI: Self = Self(4);
    const INIT: Self = Self(5);
    const STARTUP: Self = Self(6);
}

/// The destination mode of an interrupt command.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InterruptDestinationMode(bool);

impl InterruptDestinationMode {
    const PHYSICAL: Self = Self(false);
    const LOGICAL: Self = Self(true);
}

/// The destination shorthand of an interrupt command.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InterruptDestinationShorthand(u8);

impl InterruptDestinationShorthand {
    const NO_SHORTHAND: Self = Self(0);
    const SELF: Self = Self(1);
    const ALL_INCLUDING_SELF: Self = Self(2);
    const ALL_EXCLUDING_SELF: Self = Self(3);
}

/// The type of a control register access.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CrAccessType(u8);

impl CrAccessType {
    const MOV_TO_CR: Self = Self(0);
    const MOV_FROM_CR: Self = Self(1);
    const CLTS: Self = Self(2);
    const LMSW: Self = Self(3);
}

/// The status a VMCALL returns to the guest.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VmCallStatus(i64);

impl VmCallStatus {
    const OK: Self = Self(0);
    const NOT_PERMITTED: Self = Self(-1);
    const FAULT: Self = Self(-14);
    const NOT_SUPPORTED: Self = Self(-95);
    const UNKNOWN_HYPERCALL: Self = Self(-1000);
}

/// The type of a VMCALL.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VmCallType(u64);

impl VmCallType {
    const CLOCK_PAIRING: Self = Self(9);
}

/// The interrupt command destination that broadcasts to all VCPUs.
const IPI_BROADCAST_DESTINATION: u32 = 0xffff_ffff;

/// Stores VM exit info from VMCS fields.
struct ExitInfo {
    entry_failure: bool,
    exit_reason: ExitReason,
    exit_instruction_length: u32,
    guest_rip: u64,
}

impl ExitInfo {
    /// Reads the VM exit info of `vmcs`.
    fn new(vmcs: &AutoVmcs) -> Self {
        // From Volume 3, Section 26.7.
        let full_exit_reason = u64::from(vmcs.read(VmcsField32::EXIT_REASON));
        let entry_failure = bit(full_exit_reason, 31) != 0;
        let exit_reason = ExitReason(bits(full_exit_reason, 15, 0) as u32);

        let exit_instruction_length = vmcs.read(VmcsField32::EXIT_INSTRUCTION_LENGTH);
        let guest_rip = vmcs.read(VmcsFieldXX::GUEST_RIP);

        let exit_info = Self { entry_failure, exit_reason, exit_instruction_length, guest_rip };
        if exit_reason == ExitReason::EXTERNAL_INTERRUPT
            || exit_reason == ExitReason::IO_INSTRUCTION
        {
            return exit_info;
        }

        ltracef!("entry failure: {}\n", entry_failure);
        ltracef!("exit reason: {:#x} ({})\n", exit_reason.0, exit_reason_name(exit_reason));
        ltracef!("exit instruction length: {:#x}\n", exit_instruction_length);
        ltracef!("guest activity state: {:#x}\n", vmcs.read(VmcsField32::GUEST_ACTIVITY_STATE));
        ltracef!(
            "guest interruptibility state: {:#x}\n",
            vmcs.read(VmcsField32::GUEST_INTERRUPTIBILITY_STATE)
        );
        ltracef!("guest linear address: {:#x}\n", vmcs.read(VmcsFieldXX::GUEST_LINEAR_ADDRESS));
        ltracef!("guest rip: {:#x}\n", guest_rip);
        exit_info
    }
}

/// Stores VM exit interruption information. See Volume 3, Section 24.9.2.
struct ExitInterruptionInfo {
    vector: u8,
    interruption_type: InterruptionType,
    error_code_valid: bool,
    valid: bool,
}

impl ExitInterruptionInfo {
    /// Reads the VM exit interruption information of `vmcs`.
    fn new(vmcs: &AutoVmcs) -> Self {
        let int_info = u64::from(vmcs.read(VmcsField32::EXIT_INTERRUPTION_INFORMATION));
        Self {
            vector: bits(int_info, 7, 0) as u8,
            interruption_type: InterruptionType(bits_shift(int_info, 10, 8) as u8),
            error_code_valid: bit(int_info, 11) != 0,
            valid: bit(int_info, 31) != 0,
        }
    }
}

/// Stores EPT violation info from the VMCS exit qualification field.
struct EptViolationInfo {
    read: bool,
    write: bool,
    instruction: bool,
}

impl EptViolationInfo {
    /// Decodes the exit qualification of an EPT violation.
    fn new(qualification: u64) -> Self {
        // From Volume 3C, Table 27-7.
        Self {
            read: bit(qualification, 0) != 0,
            write: bit(qualification, 1) != 0,
            instruction: bit(qualification, 2) != 0,
        }
    }
}

/// Stores control register access info from the VMCS exit qualification field.
struct CrAccessInfo {
    cr_number: u8,
    access_type: CrAccessType,
    reg: u8,
}

impl CrAccessInfo {
    /// Decodes the exit qualification of a control register access.
    fn new(qualification: u64) -> Self {
        // From Volume 3, Table 27-3.
        Self {
            cr_number: bits(qualification, 3, 0) as u8,
            access_type: CrAccessType(bits_shift(qualification, 5, 4) as u8),
            reg: bits_shift(qualification, 11, 8) as u8,
        }
    }
}

/// Stores IO instruction info from the VMCS exit qualification field.
struct IoInfo {
    access_size: u8,
    input: bool,
    string: bool,
    repeat: bool,
    port: u16,
}

impl IoInfo {
    /// Decodes the exit qualification of an IO instruction.
    fn new(qualification: u64) -> Self {
        Self {
            access_size: (bits(qualification, 2, 0) + 1) as u8,
            input: bit_shift(qualification, 3) != 0,
            string: bit_shift(qualification, 4) != 0,
            repeat: bit_shift(qualification, 5) != 0,
            port: bits_shift(qualification, 31, 16) as u16,
        }
    }
}

/// Stores VMCALL type and arguments.
struct VmCallInfo {
    r#type: VmCallType,
    arg: [u64; 4],
}

impl VmCallInfo {
    /// Reads the VMCALL type and arguments from the guest's registers.
    fn new(guest_state: &GuestState) -> Self {
        // ABI is documented in Linux kernel documentation, see
        // Documents/virtual/kvm/hypercalls.txt
        Self {
            r#type: VmCallType(guest_state.rax),
            arg: [guest_state.rbx, guest_state.rcx, guest_state.rdx, guest_state.rsi],
        }
    }
}

/// Interrupt command register.
struct InterruptCommandRegister {
    destination: u32,
    destination_mode: InterruptDestinationMode,
    delivery_mode: InterruptDeliveryMode,
    destination_shorthand: InterruptDestinationShorthand,
    vector: u8,
}

impl InterruptCommandRegister {
    /// Decodes the interrupt command register `hi`:`lo`.
    fn new(hi: u32, lo: u32) -> Self {
        let lo = u64::from(lo);
        Self {
            destination: hi,
            destination_mode: InterruptDestinationMode(bit_shift(lo, 11) != 0),
            delivery_mode: InterruptDeliveryMode(bits_shift(lo, 10, 8) as u8),
            destination_shorthand: InterruptDestinationShorthand(bits_shift(lo, 19, 18) as u8),
            vector: bits(lo, 7, 0) as u8,
        }
    }
}

const LOCAL_APIC_PHYS_BASE: u64 =
    APIC_PHYS_BASE as u64 | IA32_APIC_BASE_XAPIC_ENABLE | IA32_APIC_BASE_X2APIC_ENABLE;

const X2APIC_MSR_BASE: u32 = 0x800;
const X2APIC_MSR_MAX: u32 = 0x83f;

const MISC_ENABLE_FAST_STRINGS: u64 = 1 << 0;

const FIRST_EXTENDED_STATE_COMPONENT: u32 = 2;
const LAST_EXTENDED_STATE_COMPONENT: u32 = 9;
/// From Volume 1, Section 13.4.
const XSAVE_LEGACY_REGION_SIZE: u32 = 512;
const XSAVE_HEADER_SIZE: u32 = 64;

/// NOTE: x86 instructions are guaranteed to be 15 bytes or fewer.
const MAX_INSTRUCTION_SIZE: u8 = 15;

const HYP_VENDOR_ID_REGS: [u32; 3] =
    [u32::from_ne_bytes(*b"KVMK"), u32::from_ne_bytes(*b"VMKV"), u32::from_ne_bytes(*b"M\0\0\0")];

const KVM_FEATURE_NO_IO_DELAY: u64 = 1 << 1;

// CPUID leaves.
const X86_CPUID_BASE: u32 = X86CpuidLeafNum::Base as u32;
const X86_CPUID_MODEL_FEATURES: u32 = X86CpuidLeafNum::ModelFeatures as u32;
const X86_CPUID_MON: u32 = X86CpuidLeafNum::Mon as u32;
const X86_CPUID_THERMAL_AND_POWER: u32 = X86CpuidLeafNum::ThermalAndPower as u32;
const X86_CPUID_EXTENDED_FEATURE_FLAGS: u32 = X86CpuidLeafNum::ExtendedFeatureFlags as u32;
const X86_CPUID_PERFORMANCE_MONITORING: u32 = X86CpuidLeafNum::PerformanceMonitoring as u32;
const X86_CPUID_TOPOLOGY: u32 = X86CpuidLeafNum::Topology as u32;
const X86_CPUID_XSAVE: u32 = X86CpuidLeafNum::XSave as u32;
const X86_CPUID_HYP_VENDOR: u32 = X86CpuidLeafNum::HypVendor as u32;
const X86_CPUID_KVM_FEATURES: u32 = X86CpuidLeafNum::KvmFeatures as u32;
const X86_CPUID_EXT_BASE: u32 = X86CpuidLeafNum::ExtBase as u32;

/// Returns the CPUID feature bit `feature` as a `u32` mask (`1 << feature.bit`).
fn feature_mask(feature: X86CpuidBit) -> u32 {
    1 << feature.bit
}

/// Stores `value` in the low 32 bits of `reg`, preserving the upper 32 bits.
fn set_low32(reg: &mut u64, value: u32) {
    *reg = (*reg & !u64::from(u32::MAX)) | u64::from(value);
}

/// Executes CPUID `leaf` with subleaf `subleaf`, storing the results in the low 32 bits of the
/// guest's RAX, RBX, RCX and RDX.
fn cpuid_c(leaf: u32, subleaf: u32, guest_state: &mut GuestState) {
    let result = core::arch::x86_64::__cpuid_count(leaf, subleaf);
    set_low32(&mut guest_state.rax, result.eax);
    set_low32(&mut guest_state.rbx, result.ebx);
    set_low32(&mut guest_state.rcx, result.ecx);
    set_low32(&mut guest_state.rdx, result.edx);
}

/// Executes CPUID `leaf`, storing the results in the low 32 bits of the guest's RAX, RBX, RCX and
/// RDX.
fn cpuid(leaf: u32, guest_state: &mut GuestState) {
    let result = core::arch::x86_64::__cpuid(leaf);
    set_low32(&mut guest_state.rax, result.eax);
    set_low32(&mut guest_state.rbx, result.ebx);
    set_low32(&mut guest_state.rcx, result.ecx);
    set_low32(&mut guest_state.rdx, result.edx);
}

/// Guest port packet payload whose leading `INIT_BYTES` bytes contain no uninitialized padding.
///
/// # Safety
///
/// For every value of `Self` passed to [`set_packet`], all bytes in `0..Self::INIT_BYTES` must be
/// initialized (with no unnamed padding in that prefix), and `Self::INIT_BYTES <= size_of::<Self>()`.
unsafe trait PacketPayload: Copy {
    const INIT_BYTES: usize = size_of::<Self>();
}

// SAFETY: `zx_packet_guest_bell_t` has a single `zx_gpaddr_t` field with no padding.
unsafe impl PacketPayload for zx_packet_guest_bell_t {}
zr::static_assert!(size_of::<zx_packet_guest_bell_t>() == size_of::<zx_gpaddr_t>());

// SAFETY: `zx_packet_guest_io_t` fields (`u16`, `u8`, `bool`, `[u8; 4]`) pack with no padding.
unsafe impl PacketPayload for zx_packet_guest_io_t {}
zr::static_assert!(
    size_of::<zx_packet_guest_io_t>()
        == size_of::<u16>() + size_of::<u8>() + size_of::<bool>() + size_of::<[u8; 4]>()
);

// SAFETY: `zx_packet_guest_vcpu_t` is initialized from `Default` (which zeroes its explicit
// `PadByte` fields) and populated with either `zx_packet_guest_vcpu_interrupt_t` (initialized from
// `Default`, zeroing its `PadByte` tail) or `zx_packet_guest_vcpu_startup_t` (three `u64`/`usize`
// fields with no padding).
unsafe impl PacketPayload for zx_packet_guest_vcpu_t {}
zr::static_assert!(size_of::<zx_packet_guest_vcpu_t>() == 32);
zr::static_assert!(size_of::<zx_packet_guest_vcpu_interrupt_t>() == 24);
zr::static_assert!(size_of::<zx_packet_guest_vcpu_startup_t>() == 24);

// SAFETY: `zx_packet_guest_mem_t` has 26 contiguous initialized field bytes (`addr`, `cr3`, `rip`,
// `instruction_size`, `default_operand_size`) followed by 6 bytes of trailing struct padding that
// `INIT_BYTES` excludes so `packet.union[26..32]` retains its zeroed bytes.
unsafe impl PacketPayload for zx_packet_guest_mem_t {
    const INIT_BYTES: usize = offset_of!(Self, default_operand_size) + size_of::<u8>();
}
zr::static_assert!(
    zx_packet_guest_mem_t::INIT_BYTES
        == size_of::<zx_gpaddr_t>()
            + size_of::<zx_gpaddr_t>()
            + size_of::<zx_vaddr_t>()
            + size_of::<u8>()
            + size_of::<u8>()
);

/// Writes the initialized payload bytes of `value` into the union of `packet`.
fn set_packet<T: PacketPayload>(packet: &mut zx_port_packet_t, value: T) {
    const {
        assert!(T::INIT_BYTES <= size_of::<T>());
        assert!(T::INIT_BYTES <= size_of::<[u8; 32]>());
    }
    // SAFETY: `T: PacketPayload` and the compile-time assertions guarantee that the first
    // `T::INIT_BYTES` bytes of `value` are initialized and fit within `packet.union`, and `value`
    // (a local stack copy) does not overlap `*packet`.
    unsafe {
        core::ptr::copy_nonoverlapping(
            (&raw const value).cast::<u8>(),
            packet.union.as_mut_ptr(),
            T::INIT_BYTES,
        );
    }
}

/// Returns the guest's `u16` VPID minus 1 as a signed 64-bit integer.
fn vpid_minus_one(vmcs: &AutoVmcs) -> i64 {
    i64::from(vmcs.read(VmcsField16::VPID)) - 1
}

#[cold]
#[inline(never)]
fn dump_guest_state(guest_state: &GuestState, exit_info: &ExitInfo) {
    dprintf!(
        INFO,
        " RAX: {:#18x}  RCX: {:#18x}  RDX: {:#18x}  RBX: {:#18x}\n",
        guest_state.rax,
        guest_state.rcx,
        guest_state.rdx,
        guest_state.rbx
    );
    dprintf!(
        INFO,
        " RSP:  xxxxxxxx xxxxxxxx  RBP: {:#18x}  RSI: {:#18x}  RDI: {:#18x}\n",
        guest_state.rbp,
        guest_state.rsi,
        guest_state.rdi
    );
    dprintf!(
        INFO,
        "  R8: {:#18x}   R9: {:#18x}  R10: {:#18x}  R11: {:#18x}\n",
        guest_state.r8,
        guest_state.r9,
        guest_state.r10,
        guest_state.r11
    );
    dprintf!(
        INFO,
        " R12: {:#18x}  R13: {:#18x}  R14: {:#18x}  R15: {:#18x}\n",
        guest_state.r12,
        guest_state.r13,
        guest_state.r14,
        guest_state.r15
    );
    dprintf!(
        INFO,
        " RIP: {:#18x}  CR2: {:#18x} XCR0: {:#18x}\n",
        exit_info.guest_rip,
        guest_state.cr2,
        guest_state.xcr0
    );

    dprintf!(INFO, "entry failure: {}\n", u8::from(exit_info.entry_failure));
    dprintf!(INFO, "exit instruction length: {}\n", exit_info.exit_instruction_length);
}

#[cold]
#[inline(never)]
fn log_vmexit_error(
    guest_state: &GuestState,
    exit_reason: ExitReason,
    guest_rip: u64,
    exit_instruction_length: u32,
    entry_failure: bool,
    status: Status,
) {
    dprintf!(
        CRITICAL,
        "hypervisor: VM exit handler for {} ({}) returned {}\n",
        exit_reason_name(exit_reason),
        exit_reason.0,
        status.into_raw()
    );
    dump_guest_state(
        guest_state,
        &ExitInfo { entry_failure, exit_reason, exit_instruction_length, guest_rip },
    );
}

fn next_rip(guest_rip: u64, instruction_length: u32, vmcs: &mut AutoVmcs) {
    // SAFETY: The guest RIP is guest state, and does not affect the host.
    unsafe {
        vmcs.write(VmcsFieldXX::GUEST_RIP, guest_rip.wrapping_add(u64::from(instruction_length)))
    };

    // Clear any flags blocking interrupt injection for a single instruction.
    let guest_interruptibility = vmcs.read(VmcsField32::GUEST_INTERRUPTIBILITY_STATE);
    let new_interruptibility = guest_interruptibility
        & !(INTERRUPTIBILITY_STI_BLOCKING | INTERRUPTIBILITY_MOV_SS_BLOCKING);
    if new_interruptibility != guest_interruptibility {
        // SAFETY: The guest interruptibility state is guest state, and does not affect the host.
        unsafe { vmcs.write(VmcsField32::GUEST_INTERRUPTIBILITY_STATE, new_interruptibility) };
    }
}

/// Dispatches an external interrupt that caused a VM exit.
///
/// # Safety
///
/// - `vmcs` must not have been invalidated (so hardware interrupts and preemption remain disabled
///   on the current CPU).
/// - CPL must be 0, `RFLAGS.DF == 0`, `DR7.GD` must be clear, and `%gs.base` must point to the
///   calling CPU's `x86_percpu` with `PerCpu`, `current_thread`, and the local APIC initialized.
/// - `TimerLock`, `IpiTaskLock`, and `INTERRUPT_MANAGER`'s spinlock must not be held.
unsafe fn handle_external_interrupt(vmcs: &mut AutoVmcs) {
    let int_info = ExitInterruptionInfo::new(vmcs);
    debug_assert!(int_info.valid);
    debug_assert!(int_info.interruption_type == InterruptionType::EXTERNAL_INTERRUPT);
    // Soundness requires `vector >= X86_INT_PLATFORM_BASE.0` (in particular, never `X86_INT_NMI`,
    // which assumes `%rsp` is on the NMI IST stack in `exceptions.S`).
    assert!(int_info.vector >= X86_INT_PLATFORM_BASE.0);
    // Clear blocking disallowed before invoking the interrupt handler (which
    // asserts `!blocking_disallowed()`), and dispatch the handler (which
    // issues the Local APIC EOI) before invalidating the VMCS (which re-enables
    // preemption).
    set_blocking_disallowed(false);
    // SAFETY: Discharged by `# Safety`, `set_blocking_disallowed(false)` above, and the assertion
    // that `int_info.vector` (`u8`, so `<= 255`) is `>= X86_INT_PLATFORM_BASE.0` (and therefore
    // not `X86_INT_NMI`).
    unsafe { x86_call_external_interrupt_handler(u64::from(int_info.vector)) };
    vmcs.invalidate();
}

fn handle_interrupt_window(vmcs: &mut AutoVmcs) {
    vmcs.interrupt_window_exiting(false);
}

/// From Volume 2, Section 3.2, Table 3-8 "Processor Extended State Enumeration
/// Main Leaf (EAX = 0DH, ECX = 0)".
///
/// Bits 31-00: Maximum size (bytes, from the beginning of the XSAVE/XRSTOR save
/// area) required by enabled features in XCR0. May be different than ECX if some
/// features at the end of the XSAVE save area are not enabled.
fn compute_xsave_size(guest_xcr0: u64) -> Result<u32, Status> {
    let mut xsave_size = XSAVE_LEGACY_REGION_SIZE + XSAVE_HEADER_SIZE;
    for i in FIRST_EXTENDED_STATE_COMPONENT..=LAST_EXTENDED_STATE_COMPONENT {
        if guest_xcr0 & (1 << i) == 0 {
            continue;
        }
        let Some(leaf) = get_cpuid_subleaf(X86CpuidLeafNum::XSave, i) else {
            return Err(Status::INTERNAL);
        };
        if leaf == (CpuidLeaf { a: 0, b: 0, c: 0, d: 0 }) {
            continue;
        }
        let component_offset = leaf.b;
        let component_size = leaf.a;
        xsave_size = component_offset.wrapping_add(component_size);
    }
    Ok(xsave_size)
}

fn handle_cpuid(
    exit_info: &ExitInfo,
    vmcs: &mut AutoVmcs,
    guest_state: &mut GuestState,
) -> Result<(), Status> {
    let leaf = guest_state.eax();
    let subleaf = guest_state.ecx();

    next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
    match leaf {
        X86_CPUID_BASE | X86_CPUID_EXT_BASE => {
            cpuid(leaf, guest_state);
            Ok(())
        }
        _ if (X86_CPUID_BASE + 1..=MAX_SUPPORTED_CPUID).contains(&leaf)
            || (X86_CPUID_EXT_BASE + 1..=MAX_SUPPORTED_CPUID_EXT).contains(&leaf) =>
        {
            cpuid_c(leaf, subleaf, guest_state);
            match leaf {
                X86_CPUID_MODEL_FEATURES => {
                    // Override the initial local APIC ID. From Vol 2, Table 3-8.
                    guest_state.rbx &= !(0xff_i32 << 24) as u64;
                    guest_state.rbx |= (vpid_minus_one(vmcs) << 24) as u64;
                    // Enable the hypervisor bit.
                    guest_state.rcx |= u64::from(feature_mask(X86_FEATURE_HYPERVISOR));
                    // Enable the x2APIC bit.
                    guest_state.rcx |= u64::from(feature_mask(X86_FEATURE_X2APIC));
                    // Always enable TSC deadline (this doesn't depend on the host feature).
                    guest_state.rcx |= u64::from(feature_mask(X86_FEATURE_TSC_DEADLINE));
                    // Disable the VMX bit.
                    guest_state.rcx &= u64::from(!feature_mask(X86_FEATURE_VMX));
                    // Disable the PDCM bit.
                    guest_state.rcx &= u64::from(!feature_mask(X86_FEATURE_PDCM));
                    // Disable MONITOR/MWAIT.
                    guest_state.rcx &= u64::from(!feature_mask(X86_FEATURE_MON));
                    // Disable THERM_INTERRUPT and THERM_STATUS MSRs
                    guest_state.rcx &= u64::from(!feature_mask(X86_FEATURE_TM2));
                    // Enable the SEP (SYSENTER support).
                    guest_state.rdx |= u64::from(feature_mask(X86_FEATURE_SEP));
                    // Disable the Thermal Monitor bit.
                    guest_state.rdx &= u64::from(!feature_mask(X86_FEATURE_TM));
                    // Disable the THERM_CONTROL_MSR bit.
                    guest_state.rdx &= u64::from(!feature_mask(X86_FEATURE_ACPI));
                }
                // Modern CPUs (2018+) support a newer extended topology leaf (leaf 0x1f). However,
                // reporting it breaks Linux guests. We can get away with reporting only the older
                // topology, since software should fallback to it. From Intel Volume 1, Section
                // 21.3: "CPUID.1FH is a preferred superset to leaf 0BH. Intel recommends first
                // checking for the existence of leaf 1FH before using leaf 0BH."
                X86_CPUID_TOPOLOGY => {
                    if subleaf == 0 {
                        guest_state.rax = 0;
                        guest_state.rbx = u64::from(X86_FEATURE_TOPOLOGY_SINGLE_PROCESSOR.bit);
                        // Bits [07-00] are the index (0), bits [15-08] are the subleaf identifier.
                        guest_state.rcx =
                            u64::from(feature_mask(X86_FEATURE_TOPOLOGY_LOGICAL_PROCESSOR));
                        guest_state.rdx = vpid_minus_one(vmcs) as u64;
                    } else {
                        // No need to report further subleafs.
                        guest_state.rax = 0;
                        guest_state.rbx = 0;
                        guest_state.rcx = 0;
                        guest_state.rdx = 0;
                    }
                }
                // XSAVE registers are enumerated in Intel Vol. 2A, Table 3-8.
                X86_CPUID_XSAVE => {
                    if subleaf == 0 {
                        let xsave_size = compute_xsave_size(guest_state.xcr0)?;
                        guest_state.rbx = u64::from(xsave_size);
                    } else if subleaf == 1 {
                        // TODO(https://fxbug.dev/506251881): Properly support XSAVEC, which is
                        // currently not feasible due to missing support for extended state
                        // components such as AVX-512. In the meantime, we'll clear bit 01, which
                        // indicates that we don't support XSAVEC.
                        guest_state.rax &= u64::from(!(1u32 << 1));
                        // Clear bit 03, which indicates that we don't support XSAVES/XRSTORS and
                        // IA32_XSS.
                        guest_state.rax &= u64::from(!(1u32 << 3));
                        // From the Intel manual (Vol. 2A, Table 3-8): "If EAX[1] and EAX[3] are
                        // both enumerated as 0, EBX enumerates zero."
                        guest_state.rbx = 0;
                        // ECX and EDX are for enumerating the IA32_XSS MSR size, which is a
                        // feature we've explicitly disabled above.
                        guest_state.rcx = 0;
                        guest_state.rdx = 0;
                    }
                }
                X86_CPUID_THERMAL_AND_POWER => {
                    // Disable the performance energy bias bit.
                    guest_state.rcx &= u64::from(!feature_mask(X86_FEATURE_PERF_BIAS));
                    // Disable the hardware coordination feedback bit.
                    guest_state.rcx &= u64::from(!feature_mask(X86_FEATURE_HW_FEEDBACK));
                    guest_state.rax &= u64::from(
                        !(
                            // Disable Digital Thermal Sensor
                            feature_mask(X86_FEATURE_DTS) |
                            // Disable Package Thermal Status MSR.
                            feature_mask(X86_FEATURE_PTM) |
                            // Disable THERM_STATUS MSR bits 10/11 & THERM_INTERRUPT MSR bit 24
                            feature_mask(X86_FEATURE_PLN) |
                            // Disable HWP MSRs.
                            feature_mask(X86_FEATURE_HWP) | feature_mask(X86_FEATURE_HWP_NOT) |
                            feature_mask(X86_FEATURE_HWP_ACT) | feature_mask(X86_FEATURE_HWP_PREF) |
                            // Don't advertise Turbo.
                            feature_mask(X86_FEATURE_TURBO) | feature_mask(X86_FEATURE_TURBO_MAX)
                        ),
                    );
                }
                X86_CPUID_PERFORMANCE_MONITORING => {
                    // Disable all performance monitoring.
                    // 31-07 = Reserved 0, 06-00 = 1 if event is not available.
                    const PERFORMANCE_MONITORING_NO_EVENTS: u32 = 0b1111111;
                    guest_state.rax = 0;
                    guest_state.rbx = u64::from(PERFORMANCE_MONITORING_NO_EVENTS);
                    guest_state.rcx = 0;
                    guest_state.rdx = 0;
                }
                X86_CPUID_MON => {
                    // MONITOR/MWAIT are not implemented.
                    guest_state.rax = 0;
                    guest_state.rbx = 0;
                    guest_state.rcx = 0;
                    guest_state.rdx = 0;
                }
                X86_CPUID_EXTENDED_FEATURE_FLAGS => {
                    // It's possible when running under KVM in nVMX mode, that host
                    // CPUID indicates that invpcid is supported but VMX doesn't allow
                    // to enable INVPCID bit in secondary processor based controls.
                    // Therefore explicitly clear INVPCID bit in CPUID if the VMX flag
                    // wasn't set.
                    if (vmcs.read(VmcsField32::PROCBASED_CTLS2) & PROCBASED_CTLS2_INVPCID) == 0 {
                        guest_state.rbx &= u64::from(!feature_mask(X86_FEATURE_INVPCID));
                    }
                    // Disable:
                    //  * Processor Trace bit
                    //  * TSC Adjust bit
                    guest_state.rbx &= u64::from(
                        !(feature_mask(X86_FEATURE_PT) | feature_mask(X86_FEATURE_TSC_ADJUST)),
                    );
                    // Disable the LA57 bit.
                    guest_state.rcx &= u64::from(!feature_mask(X86_FEATURE_LA57));
                    // Disable:
                    //  * Indirect Branch Prediction Barrier bit
                    //  * Single Thread Indirect Branch Predictors bit
                    //  * Speculative Store Bypass Disable bit
                    // These imply support for the IA32_SPEC_CTRL and IA32_PRED_CMD
                    // MSRs, which are not implemented.
                    guest_state.rdx &= u64::from(
                        !(feature_mask(X86_FEATURE_IBRS_IBPB)
                            | feature_mask(X86_FEATURE_STIBP)
                            | feature_mask(X86_FEATURE_SSBD)),
                    );
                    // Disable support for the IA32_ARCH_CAPABILITIES MSR.
                    guest_state.rdx &= u64::from(!feature_mask(X86_FEATURE_ARCH_CAPABILITIES));
                    // Disable support for the IA32_FLUSH_CMD MSR.
                    guest_state.rdx &= u64::from(!feature_mask(X86_FEATURE_L1D_FLUSH));

                    // TODO(https://fxbug.dev/42060002): Enable AVX-512 if supported.
                    //
                    // Disabling this to work around invalid opcode errors trying to execute these
                    // instructions.
                    guest_state.rbx &= u64::from(
                        !(feature_mask(X86_FEATURE_AVX512F)
                            | feature_mask(X86_FEATURE_AVX512DQ)
                            | feature_mask(X86_FEATURE_AVX512IFMA)
                            | feature_mask(X86_FEATURE_AVX512PF)
                            | feature_mask(X86_FEATURE_AVX512ER)
                            | feature_mask(X86_FEATURE_AVX512CD)
                            | feature_mask(X86_FEATURE_AVX512BW)
                            | feature_mask(X86_FEATURE_AVX512VL)),
                    );
                    guest_state.rcx &= u64::from(
                        !(feature_mask(X86_FEATURE_AVX512VBMI)
                            | feature_mask(X86_FEATURE_AVX512VBMI2)
                            | feature_mask(X86_FEATURE_AVX512VNNI)
                            | feature_mask(X86_FEATURE_AVX512BITALG)
                            | feature_mask(X86_FEATURE_AVX512VPDQ)),
                    );
                    guest_state.rdx &= u64::from(
                        !(feature_mask(X86_FEATURE_AVX512QVNNIW)
                            | feature_mask(X86_FEATURE_AVX512QFMA)),
                    );
                }
                _ => {}
            }
            Ok(())
        }
        X86_CPUID_HYP_VENDOR => {
            // This leaf is commonly used to identify a hypervisor via ebx:ecx:edx.
            // Since Zircon hypervisor disguises itself as KVM, it needs to return
            // in EAX max CPUID function supported by hypervisor. Zero in EAX
            // should be interpreted as 0x40000001. Details are available in the
            // Linux kernel documentation (Documentation/virtual/kvm/cpuid.txt).
            guest_state.rax = u64::from(X86_CPUID_KVM_FEATURES);
            guest_state.rbx = u64::from(HYP_VENDOR_ID_REGS[0]);
            guest_state.rcx = u64::from(HYP_VENDOR_ID_REGS[1]);
            guest_state.rdx = u64::from(HYP_VENDOR_ID_REGS[2]);
            Ok(())
        }
        X86_CPUID_KVM_FEATURES => {
            // We support KVM clock.
            guest_state.rax = u64::from(KVM_FEATURE_CLOCK_SOURCE_OLD | KVM_FEATURE_CLOCK_SOURCE)
                | KVM_FEATURE_NO_IO_DELAY;
            guest_state.rbx = 0;
            guest_state.rcx = 0;
            guest_state.rdx = 0;
            Ok(())
        }
        // From Volume 2A, CPUID instruction reference. If the EAX value is outside
        // the range recognized by CPUID then the information for the highest
        // supported base information leaf is returned. Any value in ECX is
        // honored.
        _ => {
            cpuid_c(MAX_SUPPORTED_CPUID, subleaf, guest_state);
            Ok(())
        }
    }
}

fn handle_hlt(
    exit_info: &ExitInfo,
    vmcs: &mut AutoVmcs,
    local_apic_state: &LocalApicState,
) -> Result<(), Status> {
    next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
    vmcs.invalidate();
    local_apic_state.interrupt_tracker.wait(InstantMono(ZX_TIME_INFINITE), None)
}

fn handle_cr0_write(
    vmcs: &mut AutoVmcs,
    val: u64,
    local_apic_state: &LocalApicState,
) -> Result<(), Status> {
    // X86_CR0_NE is masked so that guests may write to it, but depending on
    // IA32_VMX_CR0_FIXED0 it might be unsupported in VMX operation to set it to
    // zero. Allow the guest to control its value in CR0_READ_SHADOW but not in
    // GUEST_CR0 so that GUEST_CR0 stays valid.
    let mut cr0 = val | X86_CR0_NE;
    if cr0_is_invalid(vmcs, cr0) {
        return Err(Status::INVALID_ARGS);
    }

    // From Volume 3, Table 11-5: CD=0 and NW=1 is an invalid setting and should
    // generate a GP fault.
    if (val & X86_CR0_CD) == 0 && (val & X86_CR0_NW) != 0 {
        local_apic_state.interrupt_tracker.interrupt_const::<{ X86_INT_GP_FAULT.0 as u32 }>();
        return Ok(());
    }

    // If CR0.PG is being changed, then invalidate the VPID.
    let cr0_changed = val ^ vmcs.read(VmcsFieldXX::GUEST_CR0);
    if cr0_changed & X86_CR0_PG != 0 {
        let vpid = vmcs.read(VmcsField16::VPID);
        invvpid(InvVpid::SingleContext, vpid, 0);
    }

    // From Volume 3, Section 26.3.2.1: CR0 is loaded from the CR0 field with the
    // exception of the following bits, which are never modified on VM entry: ET
    // (bit 4); reserved bits ...; NW (bit 29) and CD (bit 30). The values of
    // these bits in the CR0 field are ignored.
    //
    // Even though these bits will be ignored on VM entry, to ensure that
    // GUEST_CR0 matches the actual value of CR0 while the guest is running set
    // those bits to match the host values. This is done only to make debugging
    // simpler.
    cr0 &= !(X86_CR0_NW | X86_CR0_CD);
    cr0 |= X86_CR0_ET;
    // SAFETY: The guest CR0 is guest state, and does not affect the host.
    unsafe { vmcs.write(VmcsFieldXX::GUEST_CR0, cr0) };

    // From Volume 3, Section 25.3: For each position corresponding to a bit clear
    // in the CR0 guest/host mask, the destination operand is loaded with the
    // value of the corresponding bit in CR0. For each position corresponding to a
    // bit set in the CR0 guest/host mask, the destination operand is loaded with
    // the value of the corresponding bit in the CR0 read shadow.
    //
    // Allow the guest to control the shadow.
    // SAFETY: The CR0 read shadow is only observed by the guest.
    unsafe { vmcs.write(VmcsFieldXX::CR0_READ_SHADOW, val) };

    // From Volume 3, Section 26.3.1.1: If CR0.PG and EFER.LME are set then
    // EFER.LMA and the IA-32e mode guest entry control must also be set.
    let efer = vmcs.read(VmcsField64::GUEST_IA32_EFER);
    if !(efer & X86_EFER_LME != 0 && cr0 & X86_CR0_PG != 0) {
        return Ok(());
    }
    // SAFETY: The guest EFER is guest state, and does not affect the host.
    unsafe { vmcs.write(VmcsField64::GUEST_IA32_EFER, efer | X86_EFER_LMA) };
    // SAFETY: `IA32_VMX_ENTRY_CTLS` exists on every CPU that supports VMX, and
    // `IA32_VMX_TRUE_ENTRY_CTLS` exists when `VmxInfo::vmx_controls` is set, which `vmxon_task`
    // verifies before entering VMX operation.
    let (true_msr, old_msr) = unsafe {
        (read_msr(X86_MSR_IA32_VMX_TRUE_ENTRY_CTLS), read_msr(X86_MSR_IA32_VMX_ENTRY_CTLS))
    };
    // SAFETY: Setting the IA-32e mode guest entry control only affects the mode the guest is
    // entered in, not the host.
    unsafe {
        vmcs.set_control(VmcsField32::ENTRY_CTLS, true_msr, old_msr, ENTRY_CTLS_64BIT_MODE, 0)
    }
}

fn register_value(
    vmcs: &AutoVmcs,
    guest_state: &GuestState,
    register_id: u8,
) -> Result<u64, Status> {
    match register_id {
        // From Intel Volume 3, Table 27-3.
        0 => Ok(guest_state.rax),
        1 => Ok(guest_state.rcx),
        2 => Ok(guest_state.rdx),
        3 => Ok(guest_state.rbx),
        4 => Ok(vmcs.read(VmcsFieldXX::GUEST_RSP)),
        5 => Ok(guest_state.rbp),
        6 => Ok(guest_state.rsi),
        7 => Ok(guest_state.rdi),
        8 => Ok(guest_state.r8),
        9 => Ok(guest_state.r9),
        10 => Ok(guest_state.r10),
        11 => Ok(guest_state.r11),
        12 => Ok(guest_state.r12),
        13 => Ok(guest_state.r13),
        14 => Ok(guest_state.r14),
        15 => Ok(guest_state.r15),
        _ => Err(Status::INVALID_ARGS),
    }
}

fn handle_control_register_access(
    exit_info: &ExitInfo,
    vmcs: &mut AutoVmcs,
    guest_state: &GuestState,
    local_apic_state: &LocalApicState,
) -> Result<(), Status> {
    let cr_access_info = CrAccessInfo::new(vmcs.read(VmcsFieldXX::EXIT_QUALIFICATION));
    match cr_access_info.access_type {
        CrAccessType::MOV_TO_CR => {
            // Handle CR0 only.
            if cr_access_info.cr_number != 0 {
                return Err(Status::NOT_SUPPORTED);
            }
            let val = register_value(vmcs, guest_state, cr_access_info.reg)?;
            handle_cr0_write(vmcs, val, local_apic_state)?;
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            Ok(())
        }
        _ => Err(Status::NOT_SUPPORTED),
    }
}

#[cold]
#[inline(never)]
fn log_unsupported_io_instruction() {
    dprintf!(INFO, "hypervisor: Unsupported guest IO instruction\n");
}

#[cold]
#[inline(never)]
fn log_unhandled_io_port(input: bool, port: u16) {
    dprintf!(
        INFO,
        "hypervisor: Unhandled guest IO port {} {:#x}\n",
        if input { "read" } else { "write" },
        port
    );
}

fn handle_io_instruction(
    exit_info: &ExitInfo,
    vmcs: &mut AutoVmcs,
    guest_state: &mut GuestState,
    traps: &TrapMap,
    packet: &mut zx_port_packet_t,
) -> Result<(), Status> {
    let io_info = IoInfo::new(vmcs.read(VmcsFieldXX::EXIT_QUALIFICATION));
    if io_info.string || io_info.repeat {
        log_unsupported_io_instruction();
        return Err(Status::NOT_SUPPORTED);
    }

    let trap = match traps.find_trap(ZX_GUEST_TRAP_IO, zx_gpaddr_t::from(io_info.port)) {
        Ok(trap) => trap,
        Err(status) => {
            log_unhandled_io_port(io_info.input, io_info.port);
            return Err(status);
        }
    };
    next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);

    *packet = zx_port_packet_t::default();
    packet.key = trap.key();
    packet.packet_type = zx_packet_type_t::ZX_PKT_TYPE_GUEST_IO;
    let mut io_packet = zx_packet_guest_io_t {
        port: io_info.port,
        access_size: io_info.access_size,
        input: io_info.input,
        data: [0; 4],
    };
    if io_info.input {
        // From Volume 1, Section 3.4.1.1: 32-bit operands generate a 32-bit
        // result, zero-extended to a 64-bit result in the destination general-
        // purpose register.
        if io_info.access_size == 4 {
            guest_state.rax = 0;
        }
    } else {
        let mask = (1u64 << (u32::from(io_info.access_size.min(4)) * 8)) - 1;
        io_packet.data = ((guest_state.rax & mask) as u32).to_ne_bytes();
    }
    set_packet(packet, io_packet);
    if !io_info.input && trap.has_port() {
        vmcs.invalidate();
        return trap.queue(packet, None);
    }
    // If there was no port for the range, then return to user-space.
    Err(Status::NEXT)
}

#[cold]
#[inline(never)]
fn log_unhandled_x2apic_rdmsr(msr: u64) {
    dprintf!(INFO, "hypervisor: Unhandled guest x2APIC RDMSR {:#x}\n", msr);
}

#[cold]
#[inline(never)]
fn log_unhandled_rdmsr(msr: u64) {
    dprintf!(INFO, "hypervisor: Unhandled guest RDMSR {:#x}\n", msr);
}

fn handle_apic_rdmsr(
    exit_info: &ExitInfo,
    vmcs: &mut AutoVmcs,
    guest_state: &mut GuestState,
    local_apic_state: &LocalApicState,
) {
    let reg = X2ApicMsr(guest_state.ecx());
    match reg {
        X2ApicMsr::ID => {
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            guest_state.rax = vpid_minus_one(vmcs) as u64;
        }
        X2ApicMsr::VERSION => {
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            // We choose 15H as it causes us to be seen as a modern APIC by Linux,
            // and is the highest non-reserved value. See Volume 3 Section 10.4.8.
            const VERSION: u32 = 0x15;
            const MAX_LVT_ENTRY: u32 = 0x6; // LVT entries minus 1.
            const EOI_SUPPRESSION: u32 = 0; // Disable support for EOI-broadcast suppression.
            guest_state.rax = u64::from(VERSION | (MAX_LVT_ENTRY << 16) | (EOI_SUPPRESSION << 24));
        }
        X2ApicMsr::SVR => {
            // Spurious interrupt vector resets to 0xff. See Volume 3 Section 10.12.5.1.
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            guest_state.rax = 0xff;
        }
        _ if matches!(
            reg,
            X2ApicMsr::TPR | X2ApicMsr::LDR | X2ApicMsr::ESR | X2ApicMsr::LVT_MONITOR
        ) || (X2ApicMsr::ISR_31_0..=X2ApicMsr::ISR_255_224).contains(&reg)
            || (X2ApicMsr::TMR_31_0..=X2ApicMsr::TMR_255_224).contains(&reg)
            || (X2ApicMsr::IRR_31_0..=X2ApicMsr::IRR_255_224).contains(&reg) =>
        {
            // These registers reset to 0. See Volume 3 Section 10.12.5.1.
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            guest_state.rax = 0;
        }
        X2ApicMsr::LVT_LINT0
        | X2ApicMsr::LVT_LINT1
        | X2ApicMsr::LVT_THERMAL_SENSOR
        | X2ApicMsr::LVT_CMCI => {
            // LVT registers reset with the mask bit set. See Volume 3 Section 10.12.5.1.
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            guest_state.rax = u64::from(LVT_MASKED);
        }
        X2ApicMsr::LVT_TIMER => {
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            guest_state.rax = u64::from(local_apic_state.lvt_timer.load(Ordering::Relaxed));
        }
        _ => {
            // Issue a general protection fault for write only and unimplemented
            // registers.
            log_unhandled_x2apic_rdmsr(guest_state.rcx);
            local_apic_state.interrupt_tracker.interrupt_const::<{ X86_INT_GP_FAULT.0 as u32 }>();
        }
    }
}

fn handle_rdmsr(
    exit_info: &ExitInfo,
    vmcs: &mut AutoVmcs,
    guest_state: &mut GuestState,
    local_apic_state: &LocalApicState,
) {
    // On execution of rdmsr, ecx specifies the MSR and the result is stored in edx:eax.
    match guest_state.ecx() {
        X86_MSR_IA32_APIC_BASE => {
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            let mut result = LOCAL_APIC_PHYS_BASE;
            if vmcs.read(VmcsField16::VPID) == 1 {
                result |= IA32_APIC_BASE_BSP;
            }
            guest_state.set_edx_eax(result);
        }
        // From Volume 4, Section 2.1, Table 2-2: For now, only enable fast strings.
        X86_MSR_IA32_MISC_ENABLE => {
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            // SAFETY: IA32_MISC_ENABLE is an architectural MSR.
            let misc_enable = unsafe { read_msr(X86_MSR_IA32_MISC_ENABLE) };
            guest_state.set_edx_eax(misc_enable & MISC_ENABLE_FAST_STRINGS);
        }
        X86_MSR_DRAM_ENERGY_STATUS
        | X86_MSR_DRAM_POWER_LIMIT
        // From Volume 3, Section 28.2.6.2: The MTRRs have no effect on the memory
        // type used for an access to a guest-physical address.
        | X86_MSR_IA32_MTRRCAP
        | X86_MSR_IA32_MTRR_DEF_TYPE
        | X86_MSR_IA32_MTRR_FIX64K_00000
        | X86_MSR_IA32_MTRR_FIX16K_80000..=X86_MSR_IA32_MTRR_FIX16K_A0000
        | X86_MSR_IA32_MTRR_FIX4K_C0000..=X86_MSR_IA32_MTRR_FIX4K_F8000
        | X86_MSR_IA32_MTRR_PHYSBASE0..=X86_MSR_IA32_MTRR_PHYSMASK9
        // From Volume 3, Section 9.11.4: For now, 0.
        | X86_MSR_IA32_PLATFORM_ID
        // From Volume 3, Section 9.11.7: 0 indicates no microcode update is loaded.
        | X86_MSR_IA32_BIOS_SIGN_ID
        // From Volume 3, Section 15.3.1: 0 indicates that our machine has no
        // checking capabilities.
        | X86_MSR_IA32_MCG_CAP
        | X86_MSR_IA32_MCG_STATUS
        | X86_MSR_IA32_TEMPERATURE_TARGET
        | X86_MSR_PKG_ENERGY_STATUS
        | X86_MSR_PLATFORM_ENERGY_COUNTER
        | X86_MSR_PLATFORM_POWER_LIMIT
        | X86_MSR_PP0_ENERGY_STATUS
        | X86_MSR_PP0_POWER_LIMIT
        | X86_MSR_PP1_ENERGY_STATUS
        | X86_MSR_PP1_POWER_LIMIT
        | X86_MSR_RAPL_POWER_UNIT
        // From Volume 3, Section 14.2: We've configured CPUID to report no MPERF/APERF
        // support, but Linux attempts to read stats anyhow. Just ignore it.
        | X86_MSR_PPERF
        // From Volume 4, Table 2-15: Number of SMI interrupts since boot.
        // We report 0 interrupts.
        | X86_MSR_SMI_COUNT => {
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            guest_state.set_edx_eax(0);
        }
        X2APIC_MSR_BASE..=X2APIC_MSR_MAX => {
            handle_apic_rdmsr(exit_info, vmcs, guest_state, local_apic_state);
        }
        _ => {
            log_unhandled_rdmsr(guest_state.rcx);
            local_apic_state.interrupt_tracker.interrupt_const::<{ X86_INT_GP_FAULT.0 as u32 }>();
        }
    }
}

fn lvt_deadline(local_apic_state: &LocalApicState, lvt_timer: u32) -> InstantMono {
    if (lvt_timer & LVT_TIMER_MODE_MASK) != LVT_TIMER_MODE_ONESHOT
        && (lvt_timer & LVT_TIMER_MODE_MASK) != LVT_TIMER_MODE_PERIODIC
    {
        return InstantMono(0);
    }
    let lvt_divide_config = u64::from(local_apic_state.lvt_divide_config.load(Ordering::Relaxed));
    let shift =
        (bits_shift(lvt_divide_config, 1, 0) | (bit_shift(lvt_divide_config, 3) << 2)) as u32;
    let divisor_shift = (shift + 1) & 7;
    let duration_tsc_ticks =
        (local_apic_state.lvt_initial_count.load(Ordering::Relaxed) << divisor_shift) as i64;
    let duration = convert_raw_tsc_duration_to_nanoseconds(duration_tsc_ticks);
    current_mono_time() + duration
}

/// Timer callback invoked when `LocalApicState::timer` expires.
///
/// # Safety
///
/// - `arg` must be non-null, properly aligned, and point to an initialized [`LocalApicState`] that
///   remains pinned in memory until it is dropped, with its `timer` canceled before drop and
///   currently firing on the calling CPU (not queued in a timer list or executing on another CPU).
/// - That `LocalApicState` must never be accessed via `&mut` or non-atomic writes while its
///   `timer` is armed or firing, and nothing outside `deadline_callback` may call `set_oneshot` or
///   `set_deadline` on `timer` while it is armed or firing without first calling `cancel()`
///   (`cancel()` may run concurrently with `deadline_callback`).
unsafe extern "C" fn deadline_callback(_timer: *mut Timer, _now: i64, arg: *mut c_void) {
    // SAFETY: By `# Safety`, `arg` is non-null, aligned, and points to an initialized
    // `LocalApicState` that outlives this callback and is never mutably aliased or modified via
    // non-atomic writes while the timer is armed or firing.
    let local_apic_state = unsafe { &*arg.cast::<LocalApicState>() };
    let lvt_timer = local_apic_state.lvt_timer.load(Ordering::Relaxed);
    if lvt_timer & LVT_MASKED != 0 {
        return;
    }
    if (lvt_timer & LVT_TIMER_MODE_MASK) == LVT_TIMER_MODE_PERIODIC {
        // SAFETY: Discharged by `# Safety`: `local_apic_state` is initialized, remains pinned
        // until dropped after `timer.cancel()`, is never accessed via `&mut` or non-atomic writes
        // while armed or firing, its `timer` is currently firing on this CPU (not queued or firing
        // elsewhere), and nothing outside `deadline_callback` calls `set_oneshot` or `set_deadline`
        // while armed or firing without first calling `cancel()`.
        unsafe { set_timer(local_apic_state, lvt_deadline(local_apic_state, lvt_timer)) };
    }
    let vector = (lvt_timer & LVT_TIMER_VECTOR_MASK) as u8;
    let result = local_apic_state.interrupt_tracker.interrupt(u32::from(vector));
    assert!(result.is_ok());
}

/// If `deadline.0 > 0`, arms an unqueued `local_apic_state.timer` to fire [`deadline_callback`] at
/// `deadline`.
///
/// # Safety
///
/// - `local_apic_state` must be initialized and remain pinned in memory until it is dropped, and
///   `local_apic_state.timer.cancel()` must be called before `local_apic_state` is dropped.
/// - `local_apic_state` must never be accessed via `&mut` or non-atomic writes while its `timer`
///   is armed or firing.
/// - `local_apic_state.timer` must not currently be queued in a timer list or executing its
///   callback on another CPU; nothing outside [`deadline_callback`] may call `set_oneshot` or
///   `set_deadline` on it concurrently or while it remains armed or firing without first calling
///   `cancel()`; and unless `set_timer` is called from `deadline_callback` itself, no other thread
///   may call `cancel()` on it concurrently.
unsafe fn set_timer(local_apic_state: &LocalApicState, deadline: InstantMono) {
    if deadline.0 > 0 {
        // SAFETY: By `# Safety`, `local_apic_state` (and structurally `#[pin] timer`) remains
        // pinned until dropped and `timer` is canceled before drop, satisfying `Pin::new_unchecked`
        // and `Timer::set_oneshot`. `arg` derives from `local_apic_state` and `# Safety` upholds
        // `deadline_callback`'s preconditions while armed or firing as well as `set_oneshot`'s
        // concurrency requirements.
        unsafe {
            Pin::new_unchecked(&local_apic_state.timer).set_oneshot(
                deadline.0,
                deadline_callback,
                core::ptr::from_ref(local_apic_state).cast_mut().cast(),
            );
        }
    }
}

/// Cancels `local_apic_state.timer` and, if `deadline.0 > 0`, arms it to fire
/// [`deadline_callback`] at `deadline`.
///
/// # Safety
///
/// - `local_apic_state` must be initialized and remain pinned in memory until it is dropped, and
///   `local_apic_state.timer.cancel()` must be called before `local_apic_state` is dropped.
/// - `local_apic_state` must never be accessed via `&mut` or non-atomic writes while its `timer`
///   is armed or firing.
/// - Must not be called from [`deadline_callback`], and nothing outside `deadline_callback` may
///   call `update_timer`, `set_oneshot`, `set_deadline`, or `cancel` on `local_apic_state.timer`
///   concurrently, nor call `set_oneshot` or `set_deadline` while it remains armed or firing
///   without first calling `cancel()` (`cancel()` below waits for any running `deadline_callback`
///   to complete before `set_timer` re-arms the timer).
unsafe fn update_timer(local_apic_state: &LocalApicState, deadline: InstantMono) {
    local_apic_state.timer.cancel();
    // SAFETY: Discharged by `# Safety`; `cancel()` above removed the timer from any list and
    // waited for any concurrently running `deadline_callback` to finish.
    unsafe { set_timer(local_apic_state, deadline) };
}

fn ipi_target_mask(icr: &InterruptCommandRegister, self_: u16) -> u64 {
    debug_assert!(usize::from(self_) < MAX_GUEST_VCPUS);

    match icr.destination_shorthand {
        InterruptDestinationShorthand::NO_SHORTHAND => {
            // Intel Volume 3, Section 10.12.9: A destination ID value of FFFF_FFFFH
            // is used for broadcast of interrupts in both logical destination and
            // physical destination modes.
            if icr.destination == IPI_BROADCAST_DESTINATION {
                return u64::MAX;
            }

            // If an invalid destination was provided, just return the empty mask.
            if icr.destination as usize >= MAX_GUEST_VCPUS {
                core::hint::cold_path();
                return 0;
            }

            // Otherwise, generate a mask for the target VCPU.
            return u64::from(1u32.wrapping_shl(icr.destination));
        }
        InterruptDestinationShorthand::SELF => return u64::from(1u32.wrapping_shl(self_.into())),
        InterruptDestinationShorthand::ALL_INCLUDING_SELF => return u64::MAX,
        InterruptDestinationShorthand::ALL_EXCLUDING_SELF => {
            return u64::from(!1u32.wrapping_shl(self_.into()));
        }
        _ => {}
    }

    0
}

#[cold]
#[inline(never)]
fn log_unsupported_logical_ipi() {
    dprintf!(
        INFO,
        "hypervisor: Logical IPI destination mode requested by guest is not supported\n"
    );
}

#[cold]
#[inline(never)]
fn log_unsupported_ipi_delivery_mode(delivery_mode: u8) {
    dprintf!(INFO, "hypervisor: Unsupported guest IPI delivery mode {:#x}\n", delivery_mode);
}

fn handle_ipi(
    exit_info: &ExitInfo,
    vmcs: &mut AutoVmcs,
    guest_state: &GuestState,
    packet: &mut zx_port_packet_t,
) -> Result<(), Status> {
    let icr = InterruptCommandRegister::new(guest_state.edx(), guest_state.eax());
    if icr.destination_mode == InterruptDestinationMode::LOGICAL {
        log_unsupported_logical_ipi();
        return Err(Status::NOT_SUPPORTED);
    }
    let mut vcpu_packet = zx_packet_guest_vcpu_t::default();
    match icr.delivery_mode {
        InterruptDeliveryMode::FIXED => {
            let self_ = vpid_minus_one(vmcs) as u16;
            let mut interrupt = zx_packet_guest_vcpu_interrupt_t::default();
            interrupt.mask = ipi_target_mask(&icr, self_);
            interrupt.vector = icr.vector;
            vcpu_packet.r#type = zx_packet_guest_vcpu_type_t::ZX_PKT_GUEST_VCPU_INTERRUPT;
            vcpu_packet.union = zx_packet_guest_vcpu_union_t { interrupt };
        }
        InterruptDeliveryMode::NMI => {
            let self_ = vpid_minus_one(vmcs) as u16;
            let mut interrupt = zx_packet_guest_vcpu_interrupt_t::default();
            // Intel Volume 3a, Table 10-4 specifies that NMI to self is an invalid configuration
            // and behavior is undefined for invalid configurations.
            //
            // For simplicity we'll just clear the self-bit in the mask.
            interrupt.mask =
                ipi_target_mask(&icr, self_) & !(1i32.wrapping_shl(self_.into()) as u64);

            // Intel Volume 3a, Section 10.6.1 Interrupt Command Register.
            //
            // For NMI the target information is ignored since the NMI vector is already defined.
            interrupt.vector = X86_INT_NMI.0;
            vcpu_packet.r#type = zx_packet_guest_vcpu_type_t::ZX_PKT_GUEST_VCPU_INTERRUPT;
            vcpu_packet.union = zx_packet_guest_vcpu_union_t { interrupt };
        }
        InterruptDeliveryMode::INIT => {
            // Ignore INIT IPIs, we only need STARTUP to bring up a VCPU.
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            return Ok(());
        }
        InterruptDeliveryMode::STARTUP => {
            vcpu_packet.r#type = zx_packet_guest_vcpu_type_t::ZX_PKT_GUEST_VCPU_STARTUP;
            vcpu_packet.union = zx_packet_guest_vcpu_union_t {
                startup: zx_packet_guest_vcpu_startup_t {
                    target: u64::from(icr.destination),
                    entry: zx_gpaddr_t::from(icr.vector) << 12,
                    context: 0,
                },
            };
        }
        _ => {
            log_unsupported_ipi_delivery_mode(icr.delivery_mode.0);
            return Err(Status::NOT_SUPPORTED);
        }
    }
    *packet = zx_port_packet_t::default();
    packet.packet_type = zx_packet_type_t::ZX_PKT_TYPE_GUEST_VCPU;
    set_packet(packet, vcpu_packet);
    next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
    Err(Status::NEXT)
}

#[cold]
#[inline(never)]
fn log_unhandled_x2apic_wrmsr(msr: u32) {
    dprintf!(INFO, "hypervisor: Unhandled guest x2APIC WRMSR {:#x}\n", msr);
}

#[cold]
#[inline(never)]
fn log_unhandled_wrmsr(msr: u64) {
    dprintf!(INFO, "hypervisor: Unhandled guest WRMSR {:#x}\n", msr);
}

/// # Safety
///
/// The caller must uphold the safety preconditions of [`update_timer`] for `local_apic_state`.
unsafe fn handle_apic_wrmsr(
    exit_info: &ExitInfo,
    vmcs: &mut AutoVmcs,
    guest_state: &GuestState,
    local_apic_state: &LocalApicState,
    packet: &mut zx_port_packet_t,
) -> Result<(), Status> {
    // Check for writes to reserved bits.
    //
    // From Volume 3, Section 10.12.1.2: "The upper 32-bits of all x2APIC MSRs
    // (except for the ICR) are reserved."
    let reg = X2ApicMsr(guest_state.ecx());
    if guest_state.edx() != 0 && reg != X2ApicMsr::ICR {
        core::hint::cold_path();
        local_apic_state.interrupt_tracker.interrupt_const::<{ X86_INT_GP_FAULT.0 as u32 }>();
        return Ok(());
    }

    match reg {
        X2ApicMsr::EOI | X2ApicMsr::ESR => {
            // From Volume 3, Section 10.12.1.2: "WRMSR of a non-zero value causes #GP(0)."
            if guest_state.eax() != 0 {
                local_apic_state
                    .interrupt_tracker
                    .interrupt_const::<{ X86_INT_GP_FAULT.0 as u32 }>();
                return Ok(());
            }
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            Ok(())
        }
        X2ApicMsr::TPR
        | X2ApicMsr::SVR
        | X2ApicMsr::LVT_MONITOR
        | X2ApicMsr::LVT_ERROR
        | X2ApicMsr::LVT_LINT0
        | X2ApicMsr::LVT_LINT1
        | X2ApicMsr::LVT_THERMAL_SENSOR
        | X2ApicMsr::LVT_CMCI => {
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            Ok(())
        }
        X2ApicMsr::LVT_TIMER => {
            let lvt_timer = guest_state.eax();
            if (lvt_timer & LVT_TIMER_MODE_MASK) == LVT_TIMER_MODE_RESERVED {
                return Err(Status::INVALID_ARGS);
            }
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            local_apic_state.lvt_timer.store(lvt_timer, Ordering::Relaxed);
            // SAFETY: Discharged by `# Safety`.
            unsafe { update_timer(local_apic_state, lvt_deadline(local_apic_state, lvt_timer)) };
            Ok(())
        }
        X2ApicMsr::INITIAL_COUNT => {
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            local_apic_state.lvt_initial_count.store(guest_state.eax(), Ordering::Relaxed);
            let lvt_timer = local_apic_state.lvt_timer.load(Ordering::Relaxed);
            // SAFETY: Discharged by `# Safety`.
            unsafe { update_timer(local_apic_state, lvt_deadline(local_apic_state, lvt_timer)) };
            Ok(())
        }
        X2ApicMsr::DCR => {
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            local_apic_state.lvt_divide_config.store(guest_state.eax(), Ordering::Relaxed);
            let lvt_timer = local_apic_state.lvt_timer.load(Ordering::Relaxed);
            // SAFETY: Discharged by `# Safety`.
            unsafe { update_timer(local_apic_state, lvt_deadline(local_apic_state, lvt_timer)) };
            Ok(())
        }
        X2ApicMsr::SELF_IPI => {
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            let vector = guest_state.eax() & u32::from(u8::MAX);
            local_apic_state.interrupt_tracker.interrupt(vector)
        }
        X2ApicMsr::ICR => handle_ipi(exit_info, vmcs, guest_state, packet),
        _ => {
            // Issue a general protection fault for read only and unimplemented
            // registers.
            log_unhandled_x2apic_wrmsr(guest_state.ecx());
            local_apic_state.interrupt_tracker.interrupt_const::<{ X86_INT_GP_FAULT.0 as u32 }>();
            Ok(())
        }
    }
}

fn handle_kvm_wrmsr(
    exit_info: &ExitInfo,
    vmcs: &mut AutoVmcs,
    guest_state: &GuestState,
    local_apic_state: &LocalApicState,
    pv_clock: &mut PvClockState,
    gpa: &GuestPhysicalAspace,
) -> Result<(), Status> {
    let guest_paddr = guest_state.edx_eax() as zx_paddr_t;

    next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
    match guest_state.ecx() {
        KVM_SYSTEM_TIME_MSR_OLD | KVM_SYSTEM_TIME_MSR => {
            vmcs.invalidate();
            if (guest_paddr & 1) != 0 {
                return pv_clock_reset_clock(pv_clock, gpa, guest_paddr & !1);
            } else {
                pv_clock_stop_clock(pv_clock);
            }
            Ok(())
        }
        KVM_BOOT_TIME_OLD | KVM_BOOT_TIME => {
            vmcs.invalidate();
            pv_clock_update_boot_time(gpa, guest_paddr)
        }
        _ => {
            local_apic_state.interrupt_tracker.interrupt_const::<{ X86_INT_GP_FAULT.0 as u32 }>();
            Ok(())
        }
    }
}

/// # Safety
///
/// The caller must uphold the safety preconditions of [`update_timer`] for `local_apic_state`.
#[allow(clippy::too_many_arguments)]
unsafe fn handle_wrmsr(
    exit_info: &ExitInfo,
    vmcs: &mut AutoVmcs,
    guest_state: &GuestState,
    local_apic_state: &LocalApicState,
    pv_clock: &mut PvClockState,
    gpa: &GuestPhysicalAspace,
    packet: &mut zx_port_packet_t,
) -> Result<(), Status> {
    // On execution of wrmsr, rcx specifies the MSR and edx:eax contains the value to be written.
    match guest_state.ecx() {
        X86_MSR_IA32_APIC_BASE => {
            if (guest_state.edx_eax() & !IA32_APIC_BASE_BSP) != LOCAL_APIC_PHYS_BASE {
                return Err(Status::INVALID_ARGS);
            }
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            Ok(())
        }
        // See note in handle_rdmsr.
        X86_MSR_IA32_MTRRCAP
        | X86_MSR_IA32_MTRR_DEF_TYPE
        | X86_MSR_IA32_MTRR_FIX64K_00000
        | X86_MSR_IA32_MTRR_FIX16K_80000..=X86_MSR_IA32_MTRR_FIX16K_A0000
        | X86_MSR_IA32_MTRR_FIX4K_C0000..=X86_MSR_IA32_MTRR_FIX4K_F8000
        | X86_MSR_IA32_MTRR_PHYSBASE0..=X86_MSR_IA32_MTRR_PHYSMASK9
        | X86_MSR_IA32_BIOS_SIGN_ID
        | X86_MSR_DRAM_POWER_LIMIT
        | X86_MSR_PP0_POWER_LIMIT
        | X86_MSR_PP1_POWER_LIMIT
        | X86_MSR_PLATFORM_POWER_LIMIT
        // We disable the associated CPUID bits, but Linux still writes to these
        // MSRs. Just ignore it.
        | X86_MSR_IA32_SPEC_CTRL
        | X86_MSR_IA32_PRED_CMD
        // From AMD64 Volume 2, Section 6.1.1: CSTAR is unused, but Linux likes to
        // set a null handler, even when not in compatibility mode. Just ignore it.
        | X86_MSR_IA32_CSTAR => {
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            Ok(())
        }
        X86_MSR_IA32_TSC_DEADLINE => {
            if (local_apic_state.lvt_timer.load(Ordering::Relaxed) & LVT_TIMER_MODE_MASK)
                != LVT_TIMER_MODE_TSC_DEADLINE
            {
                return Err(Status::INVALID_ARGS);
            }
            next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
            let tsc_deadline = guest_state.edx_eax() as i64;
            let mono_deadline =
                InstantMono(convert_raw_tsc_timestamp_to_clock_monotonic(tsc_deadline));
            // SAFETY: Discharged by `# Safety`.
            unsafe { update_timer(local_apic_state, mono_deadline) };
            Ok(())
        }
        X2APIC_MSR_BASE..=X2APIC_MSR_MAX => {
            // SAFETY: Discharged by `# Safety`.
            unsafe { handle_apic_wrmsr(exit_info, vmcs, guest_state, local_apic_state, packet) }
        }
        KVM_SYSTEM_TIME_MSR_OLD | KVM_SYSTEM_TIME_MSR | KVM_BOOT_TIME_OLD | KVM_BOOT_TIME => {
            handle_kvm_wrmsr(exit_info, vmcs, guest_state, local_apic_state, pv_clock, gpa)
        }
        _ => {
            log_unhandled_wrmsr(guest_state.rcx);
            local_apic_state.interrupt_tracker.interrupt_const::<{ X86_INT_GP_FAULT.0 as u32 }>();
            Ok(())
        }
    }
}

fn default_operand_size(efer: u64, cs_access_rights: u32) -> u8 {
    // See Volume 3, Section 5.2.1.
    if (efer & X86_EFER_LMA) != 0 && (cs_access_rights & GUEST_XX_ACCESS_RIGHTS_L) != 0 {
        // IA32-e 64 bit mode.
        4
    } else if cs_access_rights & GUEST_XX_ACCESS_RIGHTS_D != 0 {
        // CS.D set (and not 64 bit mode).
        4
    } else {
        // CS.D clear (and not 64 bit mode).
        2
    }
}

fn handle_trap(
    exit_info: &ExitInfo,
    vmcs: &mut AutoVmcs,
    read: bool,
    guest_paddr: zx_vaddr_t,
    traps: &TrapMap,
    packet: &mut zx_port_packet_t,
) -> Result<(), Status> {
    let trap = traps.find_trap(ZX_GUEST_TRAP_BELL, guest_paddr)?;
    next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);

    match trap.kind() {
        ZX_GUEST_TRAP_BELL => {
            if read {
                return Err(Status::NOT_SUPPORTED);
            }
            *packet = zx_port_packet_t::default();
            packet.key = trap.key();
            packet.packet_type = zx_packet_type_t::ZX_PKT_TYPE_GUEST_BELL;
            set_packet(packet, zx_packet_guest_bell_t { addr: guest_paddr });
            if !trap.has_port() {
                return Err(Status::BAD_STATE);
            }
            vmcs.invalidate();
            trap.queue(packet, None)
        }
        ZX_GUEST_TRAP_MEM => {
            if exit_info.exit_instruction_length > u32::from(MAX_INSTRUCTION_SIZE) {
                return Err(Status::INTERNAL);
            }
            *packet = zx_port_packet_t::default();
            packet.key = trap.key();
            packet.packet_type = zx_packet_type_t::ZX_PKT_TYPE_GUEST_MEM;
            set_packet(
                packet,
                zx_packet_guest_mem_t {
                    addr: guest_paddr,
                    cr3: vmcs.read(VmcsFieldXX::GUEST_CR3) as zx_gpaddr_t,
                    rip: exit_info.guest_rip as zx_vaddr_t,
                    instruction_size: exit_info.exit_instruction_length as u8,
                    default_operand_size: default_operand_size(
                        vmcs.read(VmcsField64::GUEST_IA32_EFER),
                        vmcs.read(VmcsField32::GUEST_CS_ACCESS_RIGHTS),
                    ),
                },
            );
            Err(Status::NEXT)
        }
        _ => Err(Status::BAD_STATE),
    }
}

#[cold]
#[inline(never)]
fn log_unhandled_ept_violation(guest_paddr: zx_gpaddr_t) {
    dprintf!(CRITICAL, "hypervisor: Unhandled EPT violation {:#x}\n", guest_paddr);
}

fn handle_ept_violation(
    exit_info: &ExitInfo,
    vmcs: &mut AutoVmcs,
    gpa: &GuestPhysicalAspace,
    traps: &TrapMap,
    packet: &mut zx_port_packet_t,
) -> Result<(), Status> {
    let ept_violation_info = EptViolationInfo::new(vmcs.read(VmcsFieldXX::EXIT_QUALIFICATION));
    let guest_paddr = vmcs.read(VmcsField64::GUEST_PHYSICAL_ADDRESS) as zx_gpaddr_t;
    let mut result =
        handle_trap(exit_info, vmcs, ept_violation_info.read, guest_paddr, traps, packet);
    if result != Err(Status::NOT_FOUND) {
        return result;
    }

    // We may have to block when handling the page fault.
    vmcs.invalidate();

    // If there was no trap associated with this address and it is outside of
    // guest physical address space, return failure.
    if guest_paddr >= gpa.size() {
        return Err(Status::OUT_OF_RANGE);
    }

    result = gpa.page_fault(guest_paddr);
    if result.is_err() {
        log_unhandled_ept_violation(guest_paddr);
    }
    result
}

fn handle_xsetbv(
    exit_info: &ExitInfo,
    vmcs: &mut AutoVmcs,
    guest_state: &mut GuestState,
) -> Result<(), Status> {
    let guest_cr4 = vmcs.read(VmcsFieldXX::GUEST_CR4);
    if guest_cr4 & X86_CR4_OSXSAVE == 0 {
        return Err(Status::INVALID_ARGS);
    }

    // We only support XCR0.
    if guest_state.rcx != 0 {
        return Err(Status::INVALID_ARGS);
    }

    let Some(leaf) = get_cpuid_subleaf(X86CpuidLeafNum::XSave, 0) else {
        return Err(Status::INTERNAL);
    };

    // Check that XCR0 is valid.
    let xcr0_bitmap = (u64::from(leaf.d) << 32) | u64::from(leaf.a);
    let xcr0 = guest_state.edx_eax();
    if !xcr0_bitmap & xcr0 != 0
        // x87 state must be enabled.
        || (xcr0 & X86_XSAVE_STATE_BIT_X87) != X86_XSAVE_STATE_BIT_X87
        // If AVX state is enabled, SSE state must be enabled.
        || (xcr0 & (X86_XSAVE_STATE_BIT_AVX | X86_XSAVE_STATE_BIT_SSE)) == X86_XSAVE_STATE_BIT_AVX
    {
        return Err(Status::INVALID_ARGS);
    }

    guest_state.xcr0 = xcr0;
    next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
    Ok(())
}

fn handle_pause(exit_info: &ExitInfo, vmcs: &mut AutoVmcs) {
    next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
}

fn is_cpl0(vmcs: &AutoVmcs) -> bool {
    let access_rights = vmcs.read(VmcsField32::GUEST_SS_ACCESS_RIGHTS);
    // We only accept a VMCALL if CPL is 0.
    (access_rights & GUEST_XX_ACCESS_RIGHTS_DPL_USER) == 0
}

#[cold]
#[inline(never)]
fn log_unsupported_clock_pairing_type(clock_type: u64) {
    dprintf!(
        INFO,
        "hypervisor: CLOCK_PAIRING hypercall doesn't support clock type {}\n",
        clock_type
    );
}

#[cold]
#[inline(never)]
fn log_clock_offset_error(status: Status) {
    dprintf!(INFO, "hypervisor: Failed to populate lock offset with error {}\n", status.into_raw());
}

#[cold]
#[inline(never)]
fn log_unknown_hypercall(type_: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) {
    dprintf!(
        INFO,
        "hypervisor: Unknown hypercall {} (arg0={:#x}, arg1={:#x}, arg2={:#x}, arg3={:#x})\n",
        type_,
        arg0,
        arg1,
        arg2,
        arg3
    );
}

fn handle_vmcall(
    exit_info: &ExitInfo,
    vmcs: &mut AutoVmcs,
    guest_state: &mut GuestState,
    gpa: &GuestPhysicalAspace,
) {
    next_rip(exit_info.guest_rip, exit_info.exit_instruction_length, vmcs);
    if !is_cpl0(vmcs) {
        guest_state.rax = VmCallStatus::NOT_PERMITTED.0 as u64;
        return;
    }
    vmcs.invalidate();

    // We never fail in case of hypercalls, we just return/propagate errors to the caller.
    let info = VmCallInfo::new(guest_state);
    match info.r#type {
        VmCallType::CLOCK_PAIRING => {
            if info.arg[1] != 0 {
                log_unsupported_clock_pairing_type(info.arg[1]);
                guest_state.rax = VmCallStatus::NOT_SUPPORTED.0 as u64;
                return;
            }
            if let Err(status) = pv_clock_populate_offset(gpa, info.arg[0] as zx_vaddr_t) {
                log_clock_offset_error(status);
                guest_state.rax = VmCallStatus::FAULT.0 as u64;
                return;
            }
            guest_state.rax = VmCallStatus::OK.0 as u64;
        }
        _ => {
            log_unknown_hypercall(
                info.r#type.0,
                info.arg[0],
                info.arg[1],
                info.arg[2],
                info.arg[3],
            );
            guest_state.rax = VmCallStatus::UNKNOWN_HYPERCALL.0 as u64;
        }
    }
}

/// Handles a VM exit of the guest of `vmcs`.
///
/// # Safety
///
/// - `vmcs` must not have been invalidated (so the VMCS is active and hardware interrupts and
///   preemption are disabled on the current CPU).
/// - CPL must be 0, `RFLAGS.DF == 0`, `DR7.GD` must be clear, and `%gs.base` must point to the
///   calling CPU's `x86_percpu` with `PerCpu`, `current_thread` (in `THREAD_RUNNING` state with
///   rescheduling permitted once the VMCS is invalidated), and the local APIC initialized.
/// - `TimerLock`, `IpiTaskLock`, and `INTERRUPT_MANAGER`'s spinlock must not be held (and no
///   spinlocks, thread lock, dispatcher locks, lockdep-tracked locks, or active
///   `ChainLockTransaction` may be held across rescheduling).
/// - Must not be called from [`deadline_callback`]. `local_apic_state` must be initialized, remain
///   pinned in memory until it is dropped (with `local_apic_state.timer.cancel()` called before
///   drop), never be accessed via `&mut` or non-atomic writes while its `timer` is armed or
///   firing, and not have `update_timer`, `set_oneshot`, `set_deadline`, or `cancel` called on
///   `local_apic_state.timer` concurrently outside `deadline_callback` (nor `set_oneshot` or
///   `set_deadline` while armed or firing without first calling `cancel()`).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub unsafe fn vmexit_handler(
    vmcs: &mut AutoVmcs,
    guest_state: &mut GuestState,
    local_apic_state: &LocalApicState,
    pv_clock: &mut PvClockState,
    gpa: &GuestPhysicalAspace,
    traps: &TrapMap,
    packet: &mut zx_port_packet_t,
) -> Result<(), Status> {
    let mut result = Ok(());
    let exit_info = ExitInfo::new(vmcs);
    match exit_info.exit_reason {
        ExitReason::EXTERNAL_INTERRUPT => {
            ktrace_vcpu_exit(VcpuExit::ExternalInterrupt, exit_info.guest_rip);
            guest_stats_inc!(interrupts);
            // SAFETY: Discharged by `# Safety`; `vmcs` has not yet been invalidated.
            unsafe { handle_external_interrupt(vmcs) };
        }
        ExitReason::INTERRUPT_WINDOW => {
            ktrace_vcpu_exit(VcpuExit::InterruptWindow, exit_info.guest_rip);
            guest_stats_inc!(interrupt_windows);
            handle_interrupt_window(vmcs);
        }
        ExitReason::CPUID => {
            ktrace_vcpu_exit(VcpuExit::Cpuid, exit_info.guest_rip);
            guest_stats_inc!(cpuid_instructions);
            result = handle_cpuid(&exit_info, vmcs, guest_state);
        }
        ExitReason::HLT => {
            ktrace_vcpu_exit(VcpuExit::Hlt, exit_info.guest_rip);
            guest_stats_inc!(hlt_instructions);
            result = handle_hlt(&exit_info, vmcs, local_apic_state);
        }
        ExitReason::CONTROL_REGISTER_ACCESS => {
            ktrace_vcpu_exit(VcpuExit::ControlRegisterAccess, exit_info.guest_rip);
            guest_stats_inc!(control_register_accesses);
            result =
                handle_control_register_access(&exit_info, vmcs, guest_state, local_apic_state);
        }
        ExitReason::IO_INSTRUCTION => {
            ktrace_vcpu_exit(VcpuExit::IoInstruction, exit_info.guest_rip);
            guest_stats_inc!(io_instructions);
            result = handle_io_instruction(&exit_info, vmcs, guest_state, traps, packet);
        }
        ExitReason::RDMSR => {
            ktrace_vcpu_exit(VcpuExit::Rdmsr, exit_info.guest_rip);
            guest_stats_inc!(rdmsr_instructions);
            handle_rdmsr(&exit_info, vmcs, guest_state, local_apic_state);
        }
        ExitReason::WRMSR => {
            ktrace_vcpu_exit(VcpuExit::Wrmsr, exit_info.guest_rip);
            guest_stats_inc!(wrmsr_instructions);
            // SAFETY: Discharged by `# Safety`.
            result = unsafe {
                handle_wrmsr(&exit_info, vmcs, guest_state, local_apic_state, pv_clock, gpa, packet)
            };
        }
        ExitReason::ENTRY_FAILURE_GUEST_STATE
        | ExitReason::ENTRY_FAILURE_MSR_LOADING
        | ExitReason::ENTRY_FAILURE_MACHINE_CHECK => {
            core::hint::cold_path();
            ktrace_vcpu_exit(VcpuExit::VmEntryFailure, exit_info.guest_rip);
            result = Err(Status::BAD_STATE);
        }
        ExitReason::EPT_VIOLATION => {
            ktrace_vcpu_exit(VcpuExit::EptViolation, exit_info.guest_rip);
            guest_stats_inc!(ept_violations);
            result = handle_ept_violation(&exit_info, vmcs, gpa, traps, packet);
        }
        ExitReason::XSETBV => {
            ktrace_vcpu_exit(VcpuExit::Xsetbv, exit_info.guest_rip);
            guest_stats_inc!(xsetbv_instructions);
            result = handle_xsetbv(&exit_info, vmcs, guest_state);
        }
        ExitReason::PAUSE => {
            ktrace_vcpu_exit(VcpuExit::Pause, exit_info.guest_rip);
            guest_stats_inc!(pause_instructions);
            handle_pause(&exit_info, vmcs);
        }
        ExitReason::VMCALL => {
            ktrace_vcpu_exit(VcpuExit::Vmcall, exit_info.guest_rip);
            guest_stats_inc!(vmcall_instructions);
            handle_vmcall(&exit_info, vmcs, guest_state, gpa);
        }
        // Currently all exceptions, except NMIs, are delivered directly to guests.
        // NMIs cause VM exits and are handled by the host via the IDT as any other
        // interrupt/exception.
        _ => {
            core::hint::cold_path();
            ktrace_vcpu_exit(VcpuExit::NotSupported, exit_info.guest_rip);
            result = Err(Status::NOT_SUPPORTED);
        }
    }
    match result {
        Ok(()) => {}
        Err(status)
            if status == Status::NEXT
                // `ZX_ERR_INTERNAL_INTR_RETRY` is an alias of `ZX_ERR_INTERRUPTED_RETRY`.
                || status == Status::INTERRUPTED_RETRY
                || status == INTERNAL_INTR_KILLED => {}
        Err(status) => {
            core::hint::cold_path();
            log_vmexit_error(
                guest_state,
                exit_info.exit_reason,
                exit_info.guest_rip,
                exit_info.exit_instruction_length,
                exit_info.entry_failure,
                status,
            );
        }
    }
    result
}

/// Unit tests for the VM exit decoders.
#[cfg(ktest)]
#[unittest::suite(name = "x86_hypervisor_vmexit")]
mod tests {
    use super::{
        CrAccessInfo, CrAccessType, EptViolationInfo, ExitReason, HYP_VENDOR_ID_REGS,
        IPI_BROADCAST_DESTINATION, InterruptCommandRegister, InterruptDeliveryMode,
        InterruptDestinationMode, InterruptDestinationShorthand, IoInfo, VmCallInfo, VmCallType,
        default_operand_size, exit_reason_name, ipi_target_mask, lvt_deadline, set_packet,
        update_timer,
    };
    use crate::arch_rs::x86::hypervisor::guest::MAX_GUEST_VCPUS;
    use crate::arch_rs::x86::hypervisor::vcpu::LocalApicState;
    use crate::arch_rs::x86::hypervisor::vmcs::{
        GUEST_XX_ACCESS_RIGHTS_D, GUEST_XX_ACCESS_RIGHTS_L,
    };
    use crate::arch_rs::x86::hypervisor::vmx_state::GuestState;
    use crate::arch_rs::x86::interrupts::X86_INT_PLATFORM_BASE;
    use crate::arch_rs::x86::lapic::LVT_TIMER_MODE_PERIODIC;
    use crate::arch_rs::x86::registers::X86_EFER_LMA;
    use crate::platform_rs::timer::{DurationMono, current_mono_time};
    use core::sync::atomic::Ordering;
    use pin_init::stack_pin_init;
    use unittest::{expect_eq, expect_false, expect_ok, expect_true};
    use zr::defer;
    use zx_types::{
        zx_packet_guest_bell_t, zx_packet_guest_io_t, zx_packet_guest_mem_t,
        zx_packet_guest_vcpu_interrupt_t, zx_packet_guest_vcpu_t, zx_packet_guest_vcpu_type_t,
        zx_packet_guest_vcpu_union_t, zx_port_packet_t,
    };

    /// Verifies decoding EPT violation exit qualifications.
    #[test]
    fn ept_violation_info() {
        let info = EptViolationInfo::new(0b101);
        expect_true!(info.read);
        expect_false!(info.write);
        expect_true!(info.instruction);
        let info = EptViolationInfo::new(0b010);
        expect_false!(info.read);
        expect_true!(info.write);
        expect_false!(info.instruction);
    }

    /// Verifies decoding control register access exit qualifications.
    #[test]
    fn cr_access_info() {
        // MOV to CR4 from RSI (6).
        let info = CrAccessInfo::new(0x604);
        expect_eq!(4, info.cr_number);
        expect_true!(info.access_type == CrAccessType::MOV_TO_CR);
        expect_eq!(6, info.reg);
        let info = CrAccessInfo::new(0x30);
        expect_true!(info.access_type == CrAccessType::LMSW);
    }

    /// Verifies decoding IO instruction exit qualifications.
    #[test]
    fn io_info() {
        // 4-byte IN from port 0x3f8.
        let info = IoInfo::new((0x3f8 << 16) | (1 << 3) | 3);
        expect_eq!(4, info.access_size);
        expect_true!(info.input);
        expect_false!(info.string);
        expect_false!(info.repeat);
        expect_eq!(0x3f8, info.port);
        // REP OUTSB to port 0x80.
        let info = IoInfo::new((0x80 << 16) | (1 << 5) | (1 << 4));
        expect_eq!(1, info.access_size);
        expect_false!(info.input);
        expect_true!(info.string);
        expect_true!(info.repeat);
        expect_eq!(0x80, info.port);
    }

    /// Verifies decoding the interrupt command register.
    #[test]
    fn interrupt_command_register() {
        // STARTUP to VCPU 2 with vector 0x9a, all excluding self.
        let icr = InterruptCommandRegister::new(2, (3 << 18) | (1 << 11) | (6 << 8) | 0x9a);
        expect_eq!(2, icr.destination);
        expect_true!(icr.destination_mode == InterruptDestinationMode::LOGICAL);
        expect_true!(icr.delivery_mode == InterruptDeliveryMode::STARTUP);
        expect_true!(
            icr.destination_shorthand == InterruptDestinationShorthand::ALL_EXCLUDING_SELF
        );
        expect_eq!(0x9a, icr.vector);
    }

    /// Verifies reading the VMCALL type and arguments.
    #[test]
    fn vmcall_info() {
        let guest_state =
            GuestState { rax: 9, rcx: 2, rdx: 3, rbx: 1, rsi: 4, ..Default::default() };
        let info = VmCallInfo::new(&guest_state);
        expect_true!(info.r#type == VmCallType::CLOCK_PAIRING);
        expect_true!(info.arg == [1, 2, 3, 4]);
    }

    /// Verifies the VCPU masks generated for IPIs.
    #[test]
    fn ipi_target_mask_values() {
        let icr = |destination, shorthand| InterruptCommandRegister {
            destination,
            destination_mode: InterruptDestinationMode::PHYSICAL,
            delivery_mode: InterruptDeliveryMode::FIXED,
            destination_shorthand: shorthand,
            vector: 0,
        };
        let no_shorthand = InterruptDestinationShorthand::NO_SHORTHAND;
        expect_eq!(u64::MAX, ipi_target_mask(&icr(IPI_BROADCAST_DESTINATION, no_shorthand), 0));
        expect_eq!(0, ipi_target_mask(&icr(MAX_GUEST_VCPUS as u32, no_shorthand), 0));
        expect_eq!(1 << 5, ipi_target_mask(&icr(5, no_shorthand), 0));
        expect_eq!(1 << 3, ipi_target_mask(&icr(0, InterruptDestinationShorthand::SELF), 3));
        expect_eq!(
            u64::MAX,
            ipi_target_mask(&icr(0, InterruptDestinationShorthand::ALL_INCLUDING_SELF), 3)
        );
        expect_eq!(
            u64::from(!(1u32 << 3)),
            ipi_target_mask(&icr(0, InterruptDestinationShorthand::ALL_EXCLUDING_SELF), 3)
        );
    }

    /// Verifies the default operand size of the guest.
    #[test]
    fn default_operand_size_values() {
        expect_eq!(4, default_operand_size(X86_EFER_LMA, GUEST_XX_ACCESS_RIGHTS_L));
        expect_eq!(4, default_operand_size(0, GUEST_XX_ACCESS_RIGHTS_D));
        expect_eq!(2, default_operand_size(0, GUEST_XX_ACCESS_RIGHTS_L));
        expect_eq!(2, default_operand_size(X86_EFER_LMA, 0));
    }

    /// Verifies the names of exit reasons.
    #[test]
    fn exit_reason_names() {
        expect_true!(exit_reason_name(ExitReason::CPUID) == "CPUID");
        expect_true!(exit_reason_name(ExitReason::LOADIWKEY) == "LOADIWKEY");
        expect_true!(exit_reason_name(ExitReason(35)) == "UNKNOWN");
    }

    /// Verifies the precomputed hypervisor vendor ID CPUID registers.
    #[test]
    fn hyp_vendor_id_regs() {
        let bytes = [
            HYP_VENDOR_ID_REGS[0].to_ne_bytes(),
            HYP_VENDOR_ID_REGS[1].to_ne_bytes(),
            HYP_VENDOR_ID_REGS[2].to_ne_bytes(),
        ];
        expect_true!(bytes.as_flattened() == b"KVMKVMKVM\0\0\0");
    }

    /// Verifies writing typed guest packet payloads without clobbering trailing bytes.
    #[test]
    fn set_packet_payloads() {
        let mut packet = zx_port_packet_t::default();
        set_packet(&mut packet, zx_packet_guest_bell_t { addr: 0x1234_5678_9abc_def0 });
        expect_true!(packet.union[..8] == 0x1234_5678_9abc_def0_u64.to_ne_bytes());
        expect_true!(packet.union[8..] == [0; 24]);

        packet = zx_port_packet_t::default();
        set_packet(
            &mut packet,
            zx_packet_guest_io_t {
                port: 0x3f8,
                access_size: 2,
                input: false,
                data: [0xaa, 0xbb, 0, 0],
            },
        );
        expect_true!(packet.union[..2] == 0x3f8_u16.to_ne_bytes());
        expect_eq!(2, packet.union[2]);
        expect_eq!(0, packet.union[3]);
        expect_true!(packet.union[4..8] == [0xaa, 0xbb, 0, 0]);
        expect_true!(packet.union[8..] == [0; 24]);

        // Verify `zx_packet_guest_mem_t` only copies its 26 initialized bytes (`0..26`), leaving
        // trailing bytes (`26..32`) untouched (zeroed after `zx_port_packet_t::default()`).
        packet.union = [0xff; 32];
        set_packet(
            &mut packet,
            zx_packet_guest_mem_t {
                addr: 0x1000,
                cr3: 0x2000,
                rip: 0x3000,
                instruction_size: 6,
                default_operand_size: 4,
            },
        );
        expect_true!(packet.union[0..8] == 0x1000_usize.to_ne_bytes());
        expect_true!(packet.union[8..16] == 0x2000_usize.to_ne_bytes());
        expect_true!(packet.union[16..24] == 0x3000_usize.to_ne_bytes());
        expect_eq!(6, packet.union[24]);
        expect_eq!(4, packet.union[25]);
        expect_true!(packet.union[26..32] == [0xff; 6]);

        packet.union = [0xff; 32];
        let mut vcpu = zx_packet_guest_vcpu_t::default();
        let mut interrupt = zx_packet_guest_vcpu_interrupt_t::default();
        interrupt.mask = 0x55;
        interrupt.vector = 0x20;
        vcpu.r#type = zx_packet_guest_vcpu_type_t::ZX_PKT_GUEST_VCPU_INTERRUPT;
        vcpu.union = zx_packet_guest_vcpu_union_t { interrupt };
        set_packet(&mut packet, vcpu);
        expect_true!(packet.union[4..8] == [0; 4]);
        expect_true!(packet.union[8..16] == 0x55_u64.to_ne_bytes());
        expect_eq!(0x20, packet.union[16]);
        expect_true!(packet.union[17..32] == [0; 15]);
    }

    /// Verifies that a periodic local APIC timer re-arms itself across ticks.
    #[test]
    fn periodic_apic_timer() {
        // At divide-by-2 (`lvt_divide_config = 0`), 500,000 initial counts is 1,000,000 TSC ticks
        // (~0.2-1 ms on 1-5 GHz CPUs), fast enough for a unit test without starving the CPU in
        // back-to-back timer interrupts before `_cancel` runs.
        const INITIAL_COUNT: u32 = 500_000;
        const WAIT_TIMEOUT: DurationMono = DurationMono::from_seconds(10);

        stack_pin_init!(let local_apic_state = LocalApicState::init());
        let vector = u32::from(X86_INT_PLATFORM_BASE.0);
        let lvt_timer = LVT_TIMER_MODE_PERIODIC | vector;
        local_apic_state.lvt_timer.store(lvt_timer, Ordering::Relaxed);
        local_apic_state.lvt_initial_count.store(INITIAL_COUNT, Ordering::Relaxed);
        let _cancel = defer(|| {
            local_apic_state.timer.cancel();
        });

        // SAFETY: Not called from `deadline_callback`, `local_apic_state` is initialized and
        // remains stack-pinned until dropped after `_cancel` cancels its timer, it is only
        // accessed via shared references while armed, and nothing outside `deadline_callback` arms
        // or cancels its timer until `_cancel` drops.
        unsafe { update_timer(&local_apic_state, lvt_deadline(&local_apic_state, lvt_timer)) };
        for _ in 0..2 {
            expect_ok!(
                local_apic_state.interrupt_tracker.wait(current_mono_time() + WAIT_TIMEOUT, None)
            );
            expect_true!(local_apic_state.interrupt_tracker.pop() == Some(vector));
        }
    }
}
