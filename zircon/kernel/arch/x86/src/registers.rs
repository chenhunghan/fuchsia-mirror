// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::cell::UnsafeCell;
use core::fmt::Write;
use core::mem::{align_of, size_of};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use debug::ltracef;
use x86_registers_bindings::{x86_debug_state, x86_xsave_area, x86_xsave_legacy_area};

use super::feature::{self, X86CpuidLeafNum};
use super::x86::{read_msr, x86_get_cr0, x86_get_cr2, x86_get_cr4, x86_set_cr0, x86_set_cr4};
use super::{Iframe, vm};
use crate::kernel::thread::Thread;

// This header is intended to be included in both C and ASM
pub const X86_CR0_PE: u64 = 0x00000001; /* protected mode enable */
pub const X86_CR0_MP: u64 = 0x00000002; /* monitor coprocessor */
pub const X86_CR0_EM: u64 = 0x00000004; /* emulation */
pub const X86_CR0_TS: u64 = 0x00000008; /* task switched */
pub const X86_CR0_ET: u64 = 0x00000010; /* extension type */
pub const X86_CR0_NE: u64 = 0x00000020; /* enable x87 exception */
pub const X86_CR0_WP: u64 = 0x00010000; /* supervisor write protect */
pub const X86_CR0_NW: u64 = 0x20000000; /* not write-through */
pub const X86_CR0_CD: u64 = 0x40000000; /* cache disable */
pub const X86_CR0_PG: u64 = 0x80000000; /* enable paging */
pub const X86_CR4_PAE: u64 = 0x00000020; /* PAE paging */
pub const X86_CR3_BASE_MASK: u64 = ((1u64 << 39) - 1) << 12;
pub const X86_CR4_PGE: u64 = 0x00000080; /* page global enable */
pub const X86_CR4_OSFXSR: u64 = 0x00000200; /* os supports fxsave */
pub const X86_CR4_OSXMMEXPT: u64 = 0x00000400; /* os supports xmm exception */
pub const X86_CR4_UMIP: u64 = 0x00000800; /* User-mode instruction prevention */
pub const X86_CR4_LA57: u64 = 0x00001000; /* 57-bit linear addresses */
pub const X86_CR4_VMXE: u64 = 0x00002000; /* enable vmx */
pub const X86_CR4_FSGSBASE: u64 = 0x00010000; /* enable {rd,wr}{fs,gs}base */
pub const X86_CR4_PCIDE: u64 = 0x00020000; /* Process-context ID enable  */
pub const X86_CR4_OSXSAVE: u64 = 0x00040000; /* os supports xsave */
pub const X86_CR4_SMEP: u64 = 0x00100000; /* SMEP protection enabling */
pub const X86_CR4_SMAP: u64 = 0x00200000; /* SMAP protection enabling */
pub const X86_CR4_PKE: u64 = 0x00400000; /* Enable protection keys */
pub const X86_EFER_SCE: u64 = 0x00000001; /* enable SYSCALL */
pub const X86_EFER_LME: u64 = 0x00000100; /* long mode enable */
pub const X86_EFER_LMA: u64 = 0x00000400; /* long mode active */
pub const X86_EFER_NXE: u64 = 0x00000800; /* to enable execute disable bit */
pub const X86_MSR_IA32_PLATFORM_ID: u32 = 0x00000017; /* platform id */
pub const X86_MSR_IA32_APIC_BASE: u32 = 0x0000001b; /* APIC base physical address */
pub const X86_MSR_IA32_TSC_ADJUST: u32 = 0x0000003b; /* TSC adjust */
pub const X86_MSR_IA32_SPEC_CTRL: u32 = 0x00000048; /* Speculative Execution Controls */
pub const X86_SPEC_CTRL_IBRS: u64 = 1 << 0;
// Partitions indirect branch predictors across hyperthreads
pub const X86_SPEC_CTRL_STIBP: u64 = 1 << 1; /* Single Thread Indirect Branch Predictors */
pub const X86_SPEC_CTRL_SSBD: u64 = 1 << 2;
pub const X86_MSR_SMI_COUNT: u32 = 0x00000034; /* Number of SMI interrupts since boot */
pub const X86_MSR_IA32_PRED_CMD: u32 = 0x00000049; /* Indirect Branch Prediction Command */
pub const X86_MSR_IA32_BIOS_UPDT_TRIG: u32 = 0x00000079; /* Microcode Patch Loader */
pub const X86_MSR_IA32_BIOS_SIGN_ID: u32 = 0x0000008b; /* BIOS update signature */
pub const X86_MSR_IA32_MTRRCAP: u32 = 0x000000fe; /* MTRR capability */
pub const X86_MSR_IA32_ARCH_CAPABILITIES: u32 = 0x0000010a;
pub const X86_ARCH_CAPABILITIES_RDCL_NO: u64 = 1 << 0;
pub const X86_ARCH_CAPABILITIES_IBRS_ALL: u64 = 1 << 1;
pub const X86_ARCH_CAPABILITIES_RSBA: u64 = 1 << 2;
pub const X86_ARCH_CAPABILITIES_SSB_NO: u64 = 1 << 4;
pub const X86_ARCH_CAPABILITIES_MDS_NO: u64 = 1 << 5;
pub const X86_ARCH_CAPABILITIES_TSX_CTRL: u64 = 1 << 7;
pub const X86_ARCH_CAPABILITIES_TAA_NO: u64 = 1 << 8;
pub const X86_MSR_IA32_FLUSH_CMD: u32 = 0x0000010b; /* L1D$ Flush control */
pub const X86_MSR_IA32_TSX_CTRL: u32 = 0x00000122; /* Control to enable/disable TSX instructions */
pub const X86_TSX_CTRL_RTM_DISABLE: u64 = 1 << 0; /* Force all RTM instructions to abort */
pub const X86_TSX_CTRL_CPUID_DISABLE: u64 = 1 << 1; /* Mask RTM and HLE in CPUID */
pub const X86_MSR_IA32_SYSENTER_CS: u32 = 0x00000174; /* SYSENTER CS */
pub const X86_MSR_IA32_SYSENTER_ESP: u32 = 0x00000175; /* SYSENTER ESP */
pub const X86_MSR_IA32_SYSENTER_EIP: u32 = 0x00000176; /* SYSENTER EIP */
pub const X86_MSR_IA32_MCG_CAP: u32 = 0x00000179; /* global machine check capability */
pub const X86_MSR_IA32_MCG_STATUS: u32 = 0x0000017a; /* global machine check status */
pub const X86_MSR_IA32_MISC_ENABLE: u32 = 0x000001a0; /* enable/disable misc processor features */
pub const X86_MSR_IA32_MISC_ENABLE_TURBO_DISABLE: u64 = 1 << 38;
pub const X86_MSR_IA32_TEMPERATURE_TARGET: u32 = 0x000001a2; /* Temperature target */
pub const X86_MSR_IA32_ENERGY_PERF_BIAS: u32 = 0x000001b0; /* Energy / Performance Bias */
pub const X86_MSR_IA32_MTRR_PHYSBASE0: u32 = 0x00000200; /* MTRR PhysBase0 */
pub const X86_MSR_IA32_MTRR_PHYSMASK0: u32 = 0x00000201; /* MTRR PhysMask0 */
pub const X86_MSR_IA32_MTRR_PHYSMASK9: u32 = 0x00000213; /* MTRR PhysMask9 */
pub const X86_MSR_IA32_MTRR_DEF_TYPE: u32 = 0x000002ff; /* MTRR default type */
pub const X86_MSR_IA32_MTRR_FIX64K_00000: u32 = 0x00000250; /* MTRR FIX64K_00000 */
pub const X86_MSR_IA32_MTRR_FIX16K_80000: u32 = 0x00000258; /* MTRR FIX16K_80000 */
pub const X86_MSR_IA32_MTRR_FIX16K_A0000: u32 = 0x00000259; /* MTRR FIX16K_A0000 */
pub const X86_MSR_IA32_MTRR_FIX4K_C0000: u32 = 0x00000268; /* MTRR FIX4K_C0000 */
pub const X86_MSR_IA32_MTRR_FIX4K_F8000: u32 = 0x0000026f; /* MTRR FIX4K_F8000 */
pub const X86_MSR_IA32_PAT: u32 = 0x00000277; /* PAT */
pub const X86_MSR_IA32_TSC_DEADLINE: u32 = 0x000006e0; /* TSC deadline */

pub const X86_MSR_IA32_X2APIC_APICID: u32 = 0x00000802; /* x2APIC ID Register (R/O) */
pub const X86_MSR_IA32_X2APIC_VERSION: u32 = 0x00000803; /* x2APIC Version Register (R/O) */
pub const X86_MSR_IA32_X2APIC_TPR: u32 = 0x00000808; /* x2APIC Task Priority Register (R/W) */
pub const X86_MSR_IA32_X2APIC_PPR: u32 = 0x0000080A; /* x2APIC Processor Priority Register (R/O) */
pub const X86_MSR_IA32_X2APIC_EOI: u32 = 0x0000080B; /* x2APIC EOI Register (W/O) */
pub const X86_MSR_IA32_X2APIC_LDR: u32 = 0x0000080D; /* x2APIC Logical Destination Register (R/O) */
pub const X86_MSR_IA32_X2APIC_SIVR: u32 = 0x0000080F; /* x2APIC Spurious Interrupt Vector Register (R/W) */
pub const X86_MSR_IA32_X2APIC_ISR0: u32 = 0x00000810; /* x2APIC In-Service Register Bits 31:0 (R/O) */
pub const X86_MSR_IA32_X2APIC_ISR1: u32 = 0x00000811; /* x2APIC In-Service Register Bits 63:32 (R/O) */
pub const X86_MSR_IA32_X2APIC_ISR2: u32 = 0x00000812; /* x2APIC In-Service Register Bits 95:64 (R/O) */
pub const X86_MSR_IA32_X2APIC_ISR3: u32 = 0x00000813; /* x2APIC In-Service Register Bits 127:96 (R/O) */
pub const X86_MSR_IA32_X2APIC_ISR4: u32 = 0x00000814; /* x2APIC In-Service Register Bits 159:128 (R/O) */
pub const X86_MSR_IA32_X2APIC_ISR5: u32 = 0x00000815; /* x2APIC In-Service Register Bits 191:160 (R/O) */
pub const X86_MSR_IA32_X2APIC_ISR6: u32 = 0x00000816; /* x2APIC In-Service Register Bits 223:192 (R/O) */
pub const X86_MSR_IA32_X2APIC_ISR7: u32 = 0x00000817; /* x2APIC In-Service Register Bits 255:224 (R/O) */
pub const X86_MSR_IA32_X2APIC_TMR0: u32 = 0x00000818; /* x2APIC Trigger Mode Register Bits 31:0 (R/O) */
pub const X86_MSR_IA32_X2APIC_TMR1: u32 = 0x00000819; /* x2APIC Trigger Mode Register Bits 63:32 (R/O) */
pub const X86_MSR_IA32_X2APIC_TMR2: u32 = 0x0000081A; /* x2APIC Trigger Mode Register Bits 95:64 (R/O) */
pub const X86_MSR_IA32_X2APIC_TMR3: u32 = 0x0000081B; /* x2APIC Trigger Mode Register Bits 127:96 (R/O) */
pub const X86_MSR_IA32_X2APIC_TMR4: u32 = 0x0000081C; /* x2APIC Trigger Mode Register Bits 159:128 (R/O) */
pub const X86_MSR_IA32_X2APIC_TMR5: u32 = 0x0000081D; /* x2APIC Trigger Mode Register Bits 191:160 (R/O) */
pub const X86_MSR_IA32_X2APIC_TMR6: u32 = 0x0000081E; /* x2APIC Trigger Mode Register Bits 223:192 (R/O) */
pub const X86_MSR_IA32_X2APIC_TMR7: u32 = 0x0000081F; /* x2APIC Trigger Mode Register Bits 255:224 (R/O) */
pub const X86_MSR_IA32_X2APIC_IRR0: u32 = 0x00000820; /* x2APIC Interrupt Request Register Bits 31:0 (R/O) */
pub const X86_MSR_IA32_X2APIC_IRR1: u32 = 0x00000821; /* x2APIC Interrupt Request Register Bits 63:32 (R/O) */
pub const X86_MSR_IA32_X2APIC_IRR2: u32 = 0x00000822; /* x2APIC Interrupt Request Register Bits 95:64 (R/O) */
pub const X86_MSR_IA32_X2APIC_IRR3: u32 = 0x00000823; /* x2APIC Interrupt Request Register Bits 127:96 (R/O) */
pub const X86_MSR_IA32_X2APIC_IRR4: u32 = 0x00000824; /* x2APIC Interrupt Request Register Bits 159:128 (R/O) */
pub const X86_MSR_IA32_X2APIC_IRR5: u32 = 0x00000825; /* x2APIC Interrupt Request Register Bits 191:160 (R/O) */
pub const X86_MSR_IA32_X2APIC_IRR6: u32 = 0x00000826; /* x2APIC Interrupt Request Register Bits 223:192 (R/O) */
pub const X86_MSR_IA32_X2APIC_IRR7: u32 = 0x00000827; /* x2APIC Interrupt Request Register Bits 255:224 (R/O) */
pub const X86_MSR_IA32_X2APIC_ESR: u32 = 0x00000828; /* x2APIC Error Status Register (R/W) */
pub const X86_MSR_IA32_X2APIC_LVT_CMCI: u32 = 0x0000082F; /* x2APIC LVT Corrected Machine Check Interrupt Register (R/W) */
pub const X86_MSR_IA32_X2APIC_ICR: u32 = 0x00000830; /* x2APIC Interrupt Command Register (R/W) */
pub const X86_MSR_IA32_X2APIC_LVT_TIMER: u32 = 0x00000832; /* x2APIC LVT Timer Interrupt Register (R/W) */
pub const X86_MSR_IA32_X2APIC_LVT_THERMAL: u32 = 0x00000833; /* x2APIC LVT Thermal Sensor Interrupt Register (R/W) */
pub const X86_MSR_IA32_X2APIC_LVT_PMI: u32 = 0x00000834; /* x2APIC LVT Performance Monitor Interrupt Register (R/W) */
pub const X86_MSR_IA32_X2APIC_LVT_LINT0: u32 = 0x00000835; /* x2APIC LVT LINT0 Register (R/W) */
pub const X86_MSR_IA32_X2APIC_LVT_LINT1: u32 = 0x00000836; /* x2APIC LVT LINT1 Register (R/W) */
pub const X86_MSR_IA32_X2APIC_LVT_ERROR: u32 = 0x00000837; /* x2APIC LVT Error Register (R/W) */
pub const X86_MSR_IA32_X2APIC_INIT_COUNT: u32 = 0x00000838; /* x2APIC Initial Count Register (R/W) */
pub const X86_MSR_IA32_X2APIC_CUR_COUNT: u32 = 0x00000839; /* x2APIC Current Count Register (R/O) */
pub const X86_MSR_IA32_X2APIC_DIV_CONF: u32 = 0x0000083E; /* x2APIC Divide Configuration Register (R/W) */
pub const X86_MSR_IA32_X2APIC_SELF_IPI: u32 = 0x0000083F; /* x2APIC Self IPI Register (W/O) */

pub const X86_MSR_IA32_EFER: u32 = 0xc0000080; /* EFER */
pub const X86_MSR_IA32_STAR: u32 = 0xc0000081; /* system call address */
pub const X86_MSR_IA32_LSTAR: u32 = 0xc0000082; /* long mode call address */
pub const X86_MSR_IA32_CSTAR: u32 = 0xc0000083; /* ia32-e compat call address */
pub const X86_MSR_IA32_FMASK: u32 = 0xc0000084; /* system call flag mask */
pub const X86_MSR_IA32_FS_BASE: u32 = 0xc0000100; /* fs base address */
pub const X86_MSR_IA32_GS_BASE: u32 = 0xc0000101; /* gs base address */
pub const X86_MSR_IA32_KERNEL_GS_BASE: u32 = 0xc0000102; /* kernel gs base */
pub const X86_MSR_IA32_TSC_AUX: u32 = 0xc0000103; /* TSC aux */
pub const X86_MSR_IA32_PM_ENABLE: u32 = 0x00000770; /* enable/disable HWP */
pub const X86_MSR_IA32_HWP_CAPABILITIES: u32 = 0x00000771; /* HWP performance range enumeration */
pub const X86_MSR_IA32_HWP_REQUEST: u32 = 0x00000774; /* power manage control hints */
pub const X86_MSR_AMD_VIRT_SPEC_CTRL: u32 = 0xc001011f; /* AMD speculative execution controls */
/* See IA32_SPEC_CTRL */
pub const X86_CR4_PSE: u64 = 0xffffffef; /* Disabling PSE bit in the CR4 */

// Non-architectural MSRs
pub const X86_MSR_POWER_CTL: u32 = 0x000001fc; /* Power Control Register */
pub const X86_MSR_RAPL_POWER_UNIT: u32 = 0x00000606; /* RAPL unit multipliers */
pub const X86_MSR_PKG_POWER_LIMIT: u32 = 0x00000610; /* Package power limits */
pub const X86_MSR_PKG_ENERGY_STATUS: u32 = 0x00000611; /* Package energy status */
pub const X86_MSR_PKG_POWER_INFO: u32 = 0x00000614; /* Package power range info */
pub const X86_MSR_DRAM_POWER_LIMIT: u32 = 0x00000618; /* DRAM RAPL power limit control */
pub const X86_MSR_DRAM_ENERGY_STATUS: u32 = 0x00000619; /* DRAM energy status */
pub const X86_MSR_PP0_POWER_LIMIT: u32 = 0x00000638; /* PP0 RAPL power limit control */
pub const X86_MSR_PP0_ENERGY_STATUS: u32 = 0x00000639; /* PP0 energy status */
pub const X86_MSR_PP1_POWER_LIMIT: u32 = 0x00000640; /* PP1 RAPL power limit control */
pub const X86_MSR_PP1_ENERGY_STATUS: u32 = 0x00000641; /* PP1 energy status */
pub const X86_MSR_PLATFORM_ENERGY_COUNTER: u32 = 0x0000064d; /* Platform energy counter */
pub const X86_MSR_PPERF: u32 = 0x0000064e; /* Productive performance count */
pub const X86_MSR_PERF_LIMIT_REASONS: u32 = 0x0000064f; /* Clipping cause register */
pub const X86_MSR_GFX_PERF_LIMIT_REASONS: u32 = 0x000006b0; /* Clipping cause register for graphics */
pub const X86_MSR_PLATFORM_POWER_LIMIT: u32 = 0x0000065c; /* Platform power limit control */
pub const X86_MSR_AMD_F10_DE_CFG: u32 = 0xc0011029; /* AMD Family 10h+ decode config */
pub const X86_MSR_AMD_F10_DE_CFG_LFENCE_SERIALIZE: u64 = 1 << 1;

pub const X86_MSR_AMD_LS_CFG: u32 = 0xc0011020; /* Load/store unit configuration */
pub const X86_AMD_LS_CFG_F15H_SSBD: u64 = 1 << 54;
pub const X86_AMD_LS_CFG_F16H_SSBD: u64 = 1 << 33;
pub const X86_AMD_LS_CFG_F17H_SSBD: u64 = 1 << 10;
pub const X86_MSR_K7_HWCR: u32 = 0xc0010015; /* AMD Hardware Configuration */
pub const X86_MSR_K7_HWCR_CPB_DISABLE: u64 = 1 << 25; /* Set to disable turbo ('boost') */

// KVM MSRs
pub const X86_MSR_KVM_PV_EOI_EN: u32 = 0x4b564d04; /* Enable paravirtual fast APIC EOI */
pub const X86_MSR_KVM_PV_EOI_EN_ENABLE: u64 = 1 << 0;

/* EFLAGS/RFLAGS */
pub const X86_FLAGS_CF: u64 = 1 << 0;
pub const X86_FLAGS_PF: u64 = 1 << 2;
pub const X86_FLAGS_AF: u64 = 1 << 4;
pub const X86_FLAGS_ZF: u64 = 1 << 6;
pub const X86_FLAGS_SF: u64 = 1 << 7;
pub const X86_FLAGS_TF: u64 = 1 << 8;
pub const X86_FLAGS_IF: u64 = 1 << 9;
pub const X86_FLAGS_DF: u64 = 1 << 10;
pub const X86_FLAGS_OF: u64 = 1 << 11;
pub const X86_FLAGS_STATUS_MASK: u64 = 0xfff;
pub const X86_FLAGS_IOPL_MASK: u64 = 3 << 12;
pub const X86_FLAGS_IOPL_SHIFT: u32 = 12;
pub const X86_FLAGS_NT: u64 = 1 << 14;
pub const X86_FLAGS_RF: u64 = 1 << 16;
pub const X86_FLAGS_VM: u64 = 1 << 17;
pub const X86_FLAGS_AC: u64 = 1 << 18;
pub const X86_FLAGS_VIF: u64 = 1 << 19;
pub const X86_FLAGS_VIP: u64 = 1 << 20;
pub const X86_FLAGS_ID: u64 = 1 << 21;
pub const X86_FLAGS_RESERVED_ONES: u64 = 0x2;
pub const X86_FLAGS_RESERVED: u64 = 0xffc0802a;
pub const X86_FLAGS_USER: u64 = X86_FLAGS_CF
    | X86_FLAGS_PF
    | X86_FLAGS_AF
    | X86_FLAGS_ZF
    | X86_FLAGS_SF
    | X86_FLAGS_TF
    | X86_FLAGS_DF
    | X86_FLAGS_OF
    | X86_FLAGS_NT
    | X86_FLAGS_AC
    | X86_FLAGS_ID;

/* DR6 */
pub const X86_DR6_B0: u64 = 1 << 0;
pub const X86_DR6_B1: u64 = 1 << 1;
pub const X86_DR6_B2: u64 = 1 << 2;
pub const X86_DR6_B3: u64 = 1 << 3;
pub const X86_DR6_BD: u64 = 1 << 13;
pub const X86_DR6_BS: u64 = 1 << 14;
pub const X86_DR6_BT: u64 = 1 << 15;

// NOTE: DR6 is used as a read-only status registers, and it is not writeable through userspace.
//       Any bits attempted to be written will be ignored.
pub const X86_DR6_USER_MASK: u64 =
    X86_DR6_B0 | X86_DR6_B1 | X86_DR6_B2 | X86_DR6_B3 | X86_DR6_BD | X86_DR6_BS | X86_DR6_BT;
/* Only bits in X86_DR6_USER_MASK are writeable.
 * Bits 12 and 32:63 must be written with 0, the rest as 1s */
pub const X86_DR6_MASK: u64 = 0xffff0ff0;

/* DR7 */
pub const X86_DR7_L0: u64 = 1 << 0;
pub const X86_DR7_G0: u64 = 1 << 1;
pub const X86_DR7_L1: u64 = 1 << 2;
pub const X86_DR7_G1: u64 = 1 << 3;
pub const X86_DR7_L2: u64 = 1 << 4;
pub const X86_DR7_G2: u64 = 1 << 5;
pub const X86_DR7_L3: u64 = 1 << 6;
pub const X86_DR7_G3: u64 = 1 << 7;
pub const X86_DR7_LE: u64 = 1 << 8;
pub const X86_DR7_GE: u64 = 1 << 9;
pub const X86_DR7_GD: u64 = 1 << 13;
pub const X86_DR7_RW0: u64 = 3 << 16;
pub const X86_DR7_LEN0: u64 = 3 << 18;
pub const X86_DR7_RW1: u64 = 3 << 20;
pub const X86_DR7_LEN1: u64 = 3 << 22;
pub const X86_DR7_RW2: u64 = 3 << 24;
pub const X86_DR7_LEN2: u64 = 3 << 26;
pub const X86_DR7_RW3: u64 = 3 << 28;
pub const X86_DR7_LEN3: u64 = 3 << 30;

// NOTE1: Even though the GD bit is writable, we disable it for the write_state syscall because it
//        complicates a lot the reasoning about how to access the registers. This is because
//        enabling this bit would make any other access to debug registers to issue an exception.
//        New syscalls should be define to lock/unlock debug registers.
// NOTE2: LE/GE bits are normally ignored, but the manual recommends always setting it to 1 in
//        order to be backwards compatible. Hence they are not writable from userspace.
pub const X86_DR7_USER_MASK: u64 = X86_DR7_L0
    | X86_DR7_G0
    | X86_DR7_L1
    | X86_DR7_G1
    | X86_DR7_L2
    | X86_DR7_G2
    | X86_DR7_L3
    | X86_DR7_G3
    | X86_DR7_RW0
    | X86_DR7_LEN0
    | X86_DR7_RW1
    | X86_DR7_LEN1
    | X86_DR7_RW2
    | X86_DR7_LEN2
    | X86_DR7_RW3
    | X86_DR7_LEN3;

/* Bits 11:12, 14:15 and 32:63 must be cleared to 0. Bit 10 must be set to 1. */
pub const X86_DR7_MASK: u64 = (1 << 10) | X86_DR7_LE | X86_DR7_GE;

pub const HW_DEBUG_REGISTERS_COUNT: usize = 4;

/* Indices of xsave feature states; state components are
 * enumerated in Intel Vol 1 section 13.1 */
pub const X86_XSAVE_STATE_INDEX_X87: u32 = 0;
pub const X86_XSAVE_STATE_INDEX_SSE: u32 = 1;
pub const X86_XSAVE_STATE_INDEX_AVX: u32 = 2;
pub const X86_XSAVE_STATE_INDEX_MPX_BNDREG: u32 = 3;
pub const X86_XSAVE_STATE_INDEX_MPX_BNDCSR: u32 = 4;
pub const X86_XSAVE_STATE_INDEX_AVX512_OPMASK: u32 = 5;
pub const X86_XSAVE_STATE_INDEX_AVX512_LOWERZMM_HIGH: u32 = 6;
pub const X86_XSAVE_STATE_INDEX_AVX512_HIGHERZMM: u32 = 7;
pub const X86_XSAVE_STATE_INDEX_PT: u32 = 8;
pub const X86_XSAVE_STATE_INDEX_PKRU: u32 = 9;

/* Bit masks for xsave feature states. */
pub const X86_XSAVE_STATE_BIT_X87: u64 = 1 << X86_XSAVE_STATE_INDEX_X87;
pub const X86_XSAVE_STATE_BIT_SSE: u64 = 1 << X86_XSAVE_STATE_INDEX_SSE;
pub const X86_XSAVE_STATE_BIT_AVX: u64 = 1 << X86_XSAVE_STATE_INDEX_AVX;
pub const X86_XSAVE_STATE_BIT_MPX_BNDREG: u64 = 1 << X86_XSAVE_STATE_INDEX_MPX_BNDREG;
pub const X86_XSAVE_STATE_BIT_MPX_BNDCSR: u64 = 1 << X86_XSAVE_STATE_INDEX_MPX_BNDCSR;
pub const X86_XSAVE_STATE_BIT_AVX512_OPMASK: u64 = 1 << X86_XSAVE_STATE_INDEX_AVX512_OPMASK;
pub const X86_XSAVE_STATE_BIT_AVX512_LOWERZMM_HIGH: u64 =
    1 << X86_XSAVE_STATE_INDEX_AVX512_LOWERZMM_HIGH;
pub const X86_XSAVE_STATE_BIT_AVX512_HIGHERZMM: u64 = 1 << X86_XSAVE_STATE_INDEX_AVX512_HIGHERZMM;
pub const X86_XSAVE_STATE_BIT_PT: u64 = 1 << X86_XSAVE_STATE_INDEX_PT;
pub const X86_XSAVE_STATE_BIT_PKRU: u64 = 1 << X86_XSAVE_STATE_INDEX_PKRU;

// Maximum buffer size needed for xsave and variants. To allocate, see ...BUFFER_SIZE below.
pub const X86_MAX_EXTENDED_REGISTER_SIZE: usize = 1024;

// This file handles detection of supported extended register saving
// mechanisms.  Of the ones detected, the following is our preference for
// mechanisms, from best to worst:
//
// 1) XSAVES (performs modified+init optimizations, uses compressed register
//            form, and can save supervisor-only registers)
// 2) XSAVEOPT (performs modified+init optimizations)
// 3) XSAVE (no optimizations/compression, but can save all supported extended
//           registers)
// 4) FXSAVE (can only save FPU/SSE registers)
// 5) none (will not save any extended registers, will not allow enabling
//          features that use extended registers.)

const LOCAL_TRACE: u32 = 0;

const IA32_XSS_MSR: u32 = 0xda0;

/// Offset in xsave area that components >= 2 start at.
const XSAVE_EXTENDED_AREA_OFFSET: usize = 576;

/// The first xsave component in the extended (non-legacy) area.
const XSAVE_FIRST_EXT_COMPONENT: usize = 2;

/// Number of possible components in the state vector.
const XSAVE_MAX_COMPONENTS: usize = 63;

/// Bit in `XCOMP_BV` field of xsave indicating compacted format.
const XSAVE_XCOMP_BV_COMPACT: u64 = 1u64 << 63;

/// Legacy x87/SSE save area.
pub type X86XsaveLegacyArea = x86_xsave_legacy_area;

zr::static_assert!(size_of::<X86XsaveLegacyArea>() == 416);
zr::static_assert!(align_of::<X86XsaveLegacyArea>() == 8);

/// Format described in Intel 3A section 13.4 (`x86_xsave_area`).
type XsaveArea = x86_xsave_area;

zr::static_assert!(size_of::<XsaveArea>() == XSAVE_EXTENDED_AREA_OFFSET);
zr::static_assert!(align_of::<XsaveArea>() == 8);

/// Kernel tracking of the current state of the x86 debug registers for a particular thread
/// (`x86_debug_state_t`).
pub type X86DebugState = x86_debug_state;

zr::static_assert!(size_of::<X86DebugState>() == 48);
zr::static_assert!(align_of::<X86DebugState>() == 8);

/// Indexed by component. Components 0 and 1 are the "legacy" floating point and
/// SSE ones. These do not have a size or align64 set in this structure since
/// they are inside the legacy xsave area. Use `XSAVE_FIRST_EXT_COMPONENT` for
/// the first valid entry.
#[derive(Clone, Copy, Debug)]
struct StateComponent {
    /// Total size of this component in bytes.
    size: u32,
    /// If true, this component must be aligned to a 64-byte boundary.
    align64: bool,
}

impl StateComponent {
    const ZERO: Self = Self { size: 0, align64: false };
}

/// For `FXRSTOR`, we need 512 bytes to save the state. For `XSAVE`-based
/// mechanisms, we only need `512 + 64` bytes for the initial state, since
/// our initial state only needs to specify some SSE state (masking exceptions),
/// and `XSAVE` doesn't require space for any disabled register groups after
/// the last enabled one.
#[repr(C, align(64))]
struct AlignedInitState([u8; 512 + 64]);

struct ExtendedRegisterInfo {
    state_components: [StateComponent; XSAVE_MAX_COMPONENTS],
    /// Supported bits in `XCR0` (each corresponds to a state component).
    xcr0_component_bitmap: u64,
    /// Supported bits in `IA32_XSS` (each corresponds to a state component).
    xss_component_bitmap: u64,
    /// Maximum total size for xsave, if all features are enabled.
    xsave_max_area_size: usize,
    /// Does this processor support the `XSAVES` instruction.
    xsaves_supported: bool,
    /// Does this processor support the `XSAVEOPT` instruction.
    xsaveopt_supported: bool,
    /// Does this processor support the `XGETBV` instruction with `ecx=1`.
    xgetbv_1_supported: bool,
    /// Does this processor support the `XSAVE` instruction.
    xsave_supported: bool,
    /// Does this processor support `FXSAVE`.
    fxsave_supported: bool,
    extended_register_init_state: AlignedInitState,
}

impl ExtendedRegisterInfo {
    const fn new() -> Self {
        Self {
            state_components: [StateComponent::ZERO; XSAVE_MAX_COMPONENTS],
            xcr0_component_bitmap: 0,
            xss_component_bitmap: 0,
            xsave_max_area_size: 0,
            xsaves_supported: false,
            xsaveopt_supported: false,
            xgetbv_1_supported: false,
            xsave_supported: false,
            fxsave_supported: false,
            extended_register_init_state: AlignedInitState([0; 512 + 64]),
        }
    }
}

/// Wrapper providing `Sync` for [`ExtendedRegisterInfo`].
///
/// Note: [`ExtendedRegisterInfo`] (~1.6 KB) is initialized in-place on CPU 0 across
/// [`read_xsave_state_info`] and [`extended_register_cpu_init`] rather than via
/// `LazyInit<ExtendedRegisterInfo>` so the 1.6 KB struct is never constructed by value on the
/// kernel stack.
struct SyncExtendedRegisterInfo(UnsafeCell<ExtendedRegisterInfo>);

// SAFETY: `EXTENDED_REGISTER_INFO` is mutated only once on the bootstrap processor (`CPU 0`)
// during early single-threaded boot in `extended_register_init()` before `INFO_INITIALIZED` is
// published and before secondary CPUs are brought up, and is read-only thereafter.
unsafe impl Sync for SyncExtendedRegisterInfo {}

static EXTENDED_REGISTER_INFO: SyncExtendedRegisterInfo =
    SyncExtendedRegisterInfo(UnsafeCell::new(ExtendedRegisterInfo::new()));

/// Have we already read the CPU support info and initialized `EXTENDED_REGISTER_INFO`.
///
/// Set with [`Ordering::Release`] on CPU 0 at the end of [`extended_register_init`] after
/// [`EXTENDED_REGISTER_INFO`] is populated, and read with [`Ordering::Acquire`] by secondary CPUs
/// and query helpers.
static INFO_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Maximum register state size.
///
/// Stored as an [`AtomicUsize`] and accessed with [`Ordering::Relaxed`] because
/// [`extended_register_size`] reads this value, while updates are
/// done via `fetch_max` and do not synchronize
/// other memory accesses.
static REGISTER_STATE_SIZE: AtomicUsize = AtomicUsize::new(0);

/// Returns a reference to [`EXTENDED_REGISTER_INFO`] after CPU 0 initialization.
///
/// # Safety
///
/// The caller must ensure that [`extended_register_init`] has completed on CPU 0.
#[inline(always)]
unsafe fn info() -> &'static ExtendedRegisterInfo {
    debug_assert!(INFO_INITIALIZED.load(Ordering::Acquire));
    // SAFETY: `EXTENDED_REGISTER_INFO` is only mutated on CPU 0 in `extended_register_init()`
    // before `INFO_INITIALIZED` is set to `true` with `Release` ordering and before secondary CPUs
    // are started, and is immutable thereafter.
    unsafe { &*EXTENDED_REGISTER_INFO.0.get() }
}

/// # Safety
///
/// The caller must ensure that CPL is 0, `CR0.TS` is 0, and interrupts are disabled on the current
/// CPU.
unsafe fn extended_register_cpu_init(info: &ExtendedRegisterInfo) {
    if info.xsave_supported {
        // SAFETY: Caller guarantees CPL 0 and interrupts disabled.
        unsafe {
            let cr4 = x86_get_cr4();
            // Enable XSAVE feature set.
            x86_set_cr4(cr4 | X86_CR4_OSXSAVE);
            // Put xcr0 into a known state (X87 must be enabled in this register).
            xsetbv(0, X86_XSAVE_STATE_BIT_X87);
        }
    }

    // Enable the FPU.
    // SAFETY: Caller guarantees CPL 0, `CR0.TS` is 0, and interrupts disabled, and `CR4.OSXSAVE`
    // was just enabled above when `info.xsave_supported` is true.
    let enabled =
        unsafe { extended_register_enable_feature_with_info(info, ExtendedRegisterFeature::X87) };
    debug_assert!(enabled);
}

/// Sets the portions of the xsave legacy area such that the x87 state is considered in its
/// "initial configuration" as defined by Intel Vol 1 section 13.6.
///
/// "The x87 state component comprises bytes 23:0 and bytes 159:32." This doesn't count the MXCSR
/// register.
fn set_x87_initial_state(legacy_area: &mut X86XsaveLegacyArea) {
    legacy_area.fcw = 0x037f;
    legacy_area.fsw = 0;
    // The initial value of the FTW register is 0xffff. The FTW field in the xsave area is an
    // abbreviated version (see Intel manual sec 13.5.1). In the FTW register 1 bits indicate
    // the empty tag (two per register), while the abbreviated version uses 1 bit per register and
    // 0 indicates empty. So set to 0 to indicate all registers are empty.
    legacy_area.ftw = 0;
    legacy_area.fop = 0;
    legacy_area.fip = 0;
    legacy_area.fdp = 0;

    zr::static_assert!(
        // SAFETY: `X86XsaveLegacyArea` is a plain integer/array struct for which all-zero bytes
        // are valid, and `size_of_val` only queries its size without reading memory.
        unsafe { core::mem::size_of_val(&core::mem::zeroed::<X86XsaveLegacyArea>().st) } == 128
    );
    // Register values are all 0.
    legacy_area.st = Default::default();
}

/// SSE state is only the XMM registers which is all 0 and does not count MXCSR as defined by Intel
/// Vol 1 section 13.6.
fn set_sse_initial_state(legacy_area: &mut X86XsaveLegacyArea) {
    zr::static_assert!(
        // SAFETY: `X86XsaveLegacyArea` is a plain integer/array struct for which all-zero bytes
        // are valid, and `size_of_val` only queries its size without reading memory.
        unsafe { core::mem::size_of_val(&core::mem::zeroed::<X86XsaveLegacyArea>().xmm) } == 256
    );
    legacy_area.xmm = Default::default();
}

/// Sets legacy-area `component` (0 for x87, otherwise SSE) to its initial configuration.
///
/// This is only needed when a thread has not used the component yet, so it is kept out of line to
/// keep the inlined fast path of [`get_xsave_legacy_area`] small.
#[cold]
#[inline(never)]
fn set_legacy_component_initial_state(legacy_area: &mut X86XsaveLegacyArea, component: u32) {
    if component == 0 {
        set_x87_initial_state(legacy_area);
    } else {
        set_sse_initial_state(legacy_area);
    }
}

/// Figure out what forms of register saving this machine supports and
/// select the best one.
///
/// # Safety
///
/// The caller must ensure that CPL is 0, `CR0.TS` is 0, interrupts are disabled on the current
/// CPU, and the first invocation occurs on CPU 0 before secondary CPUs are started.
pub unsafe fn extended_register_init() {
    // Have we already read the cpu support info.
    let mut initialized_cpu_already = false;

    // Acquire ordering synchronizes with any prior Release store once CPU 0 has initialized
    // `EXTENDED_REGISTER_INFO`. `SeqCst` is not required because we only need publish-subscribe
    // synchronization.
    if !INFO_INITIALIZED.load(Ordering::Acquire) {
        debug_assert!(crate::arch_rs::curr_cpu_num() == 0);

        // SAFETY: Executed once on CPU 0 during single-threaded early boot before SMP
        // initialization and before `INFO_INITIALIZED` is set to `true`.
        let info_mut = unsafe { &mut *EXTENDED_REGISTER_INFO.0.get() };
        read_xsave_state_info(info_mut);

        // We currently assume that if xsave isn't support fxsave is.
        info_mut.fxsave_supported = feature::x86_feature_test(feature::X86_FEATURE_FXSR);

        // Set up initial states.
        if info_mut.fxsave_supported || info_mut.xsave_supported {
            // SAFETY: Caller guarantees CPL 0, `CR0.TS` is 0, and interrupts are disabled.
            unsafe { extended_register_cpu_init(info_mut) };
            initialized_cpu_already = true;

            // Intel Vol 3 section 13.5.4 describes the XSAVE initialization. The only change we
            // want to make to the init state is having SIMD exceptions masked. The "legacy" area
            // of the xsave structure is valid for fxsave as well.
            let xsave_supported = info_mut.xsave_supported;
            let xsaves_supported = info_mut.xsaves_supported;
            // SAFETY: `AlignedInitState` is 64-byte aligned (`>= align_of::<XsaveArea>() == 8`)
            // with size `576 == size_of::<XsaveArea>()`, and all-zero bytes are valid for
            // `XsaveArea`.
            let area = unsafe {
                &mut *info_mut.extended_register_init_state.0.as_mut_ptr().cast::<XsaveArea>()
            };
            set_x87_initial_state(&mut area.legacy);
            set_sse_initial_state(&mut area.legacy);
            area.legacy.mxcsr = 0x3f << 7;

            if xsave_supported {
                area.xstate_bv |= X86_XSAVE_STATE_BIT_SSE;

                // If xsaves is being used, then make the saved state be in
                // compact form. xrstors will GPF if it is not.
                if xsaves_supported {
                    area.xcomp_bv |= XSAVE_XCOMP_BV_COMPACT;
                    area.xcomp_bv |= area.xstate_bv;
                }
            }
        }

        if info_mut.xsave_supported {
            // SAFETY: Caller guarantees CPL 0 and interrupts disabled, and
            // `extended_register_cpu_init` enabled `CR4.OSXSAVE` above.
            unsafe { recompute_state_size(info_mut) };
        } else if info_mut.fxsave_supported {
            REGISTER_STATE_SIZE.store(512, Ordering::Relaxed);
        }

        // Release ordering publishes all writes to `EXTENDED_REGISTER_INFO` made on CPU 0 to
        // other CPUs. `SeqCst` is not required because we only need publish-subscribe
        // synchronization.
        INFO_INITIALIZED.store(true, Ordering::Release);
    }

    // SAFETY: `INFO_INITIALIZED` was either observed `true` via `Acquire` load or just stored on
    // CPU 0 above.
    let info = unsafe { info() };
    // Ensure that xsaves_supported == true implies xsave_supported == true.
    debug_assert!(!info.xsaves_supported || info.xsave_supported);
    // Ensure that xsaveopt_supported == true implies xsave_supported == true.
    debug_assert!(!info.xsaveopt_supported || info.xsave_supported);

    if !initialized_cpu_already {
        // SAFETY: Caller guarantees CPL 0, `CR0.TS` is 0, and interrupts disabled.
        unsafe { extended_register_cpu_init(info) };
    }
}

/// Enable the requested feature on this CPU, return true on success.
/// It is currently assumed that if a feature is enabled on one CPU, the caller
/// will ensure it is enabled on all CPUs.
///
/// Extended register state features managed via `CR0`, `CR4`, and `XCR0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExtendedRegisterFeature {
    X87,
    Sse,
    Avx,
}

/// # Safety
///
/// The caller must ensure that [`extended_register_init`] has completed on this CPU, CPL is 0,
/// `CR0.TS` is 0 (`CR0.EM` is 0 before enabling `Sse`, and `X87`/`Sse` are enabled before
/// enabling `Avx`), and interrupts are disabled on the current CPU.
pub(super) unsafe fn extended_register_enable_feature(feature: ExtendedRegisterFeature) -> bool {
    // SAFETY: Caller guarantees `extended_register_init` has completed, CPL 0, and interrupts
    // disabled.
    unsafe { extended_register_enable_feature_with_info(info(), feature) }
}

/// # Safety
///
/// The caller must ensure that CPL is 0, `CR0.TS` is 0, `CR4.OSXSAVE` is 1 when
/// `info.xsave_supported` is true (`CR0.EM` is 0 before enabling `Sse`, and `X87`/`Sse` are
/// enabled before enabling `Avx`), and interrupts are disabled on the current CPU.
unsafe fn extended_register_enable_feature_with_info(
    info: &ExtendedRegisterInfo,
    feature: ExtendedRegisterFeature,
) -> bool {
    // We currently assume this is only called during initialization.
    // We rely on interrupts being disabled so xgetbv/xsetbv will not be
    // racy.
    debug_assert!(crate::arch_rs::ints_disabled());

    match feature {
        ExtendedRegisterFeature::X87 => {
            if !feature::x86_feature_test(feature::X86_FEATURE_FPU)
                || (!info.fxsave_supported && !info.xsave_supported)
            {
                core::hint::cold_path();
                return false;
            }

            // SAFETY: Caller guarantees CPL 0, `CR0.TS` is 0, `CR4.OSXSAVE` is enabled when
            // `info.xsave_supported` is true, and interrupts are disabled.
            unsafe {
                // No x87 emul, monitor co-processor.
                let mut cr0 = x86_get_cr0();
                cr0 &= !X86_CR0_EM;
                cr0 |= X86_CR0_NE;
                cr0 |= X86_CR0_MP;
                x86_set_cr0(cr0);

                // Init x87, starts with exceptions masked.
                core::arch::asm!("finit", options(nostack, preserves_flags));

                if info.xsave_supported {
                    xsetbv(0, xgetbv(0) | X86_XSAVE_STATE_BIT_X87);
                }
            }
        }
        ExtendedRegisterFeature::Sse => {
            if !feature::x86_feature_test(feature::X86_FEATURE_SSE)
                || !feature::x86_feature_test(feature::X86_FEATURE_FXSR)
            {
                core::hint::cold_path();
                return false;
            }

            // SAFETY: Caller guarantees CPL 0, `CR0.EM == 0`, `CR0.TS == 0`, `CR4.OSXSAVE == 1`
            // when `info.xsave_supported` is true, and interrupts are disabled.
            unsafe {
                // Init SSE.
                let mut cr4 = x86_get_cr4();
                cr4 |= X86_CR4_OSXMMEXPT;
                cr4 |= X86_CR4_OSFXSR;
                x86_set_cr4(cr4);

                // Mask all exceptions while preserving existing MXCSR bits read by `stmxcsr`.
                let mut mxcsr: u32 = 0;
                core::arch::asm!(
                    "stmxcsr [{}]",
                    in(reg) &mut mxcsr,
                    options(nostack, preserves_flags),
                );
                mxcsr |= 0x3f << 7;
                core::arch::asm!(
                    "ldmxcsr [{}]",
                    in(reg) &mxcsr,
                    options(nostack, preserves_flags),
                );

                if info.xsave_supported {
                    xsetbv(0, xgetbv(0) | X86_XSAVE_STATE_BIT_SSE);
                }
            }
        }
        ExtendedRegisterFeature::Avx => {
            if !info.xsave_supported || (info.xcr0_component_bitmap & X86_XSAVE_STATE_BIT_AVX) == 0
            {
                core::hint::cold_path();
                return false;
            }

            // SAFETY: Caller guarantees CPL 0, `CR4.OSXSAVE == 1`, `XCR0.X87 == 1`,
            // `XCR0.SSE == 1`, and interrupts are disabled.
            unsafe {
                // Enable SIMD exceptions.
                let mut cr4 = x86_get_cr4();
                cr4 |= X86_CR4_OSXMMEXPT;
                x86_set_cr4(cr4);

                xsetbv(0, xgetbv(0) | X86_XSAVE_STATE_BIT_AVX);
            }
        }
    }

    // SAFETY: Caller guarantees CPL 0, `CR4.OSXSAVE == 1` when `info.xsave_supported` is true, and
    // interrupts are disabled.
    unsafe { recompute_state_size(info) };
    true
}

/// Return the size required for all requested features.
#[inline(always)]
#[cfg(ktest)]
fn extended_register_size() -> usize {
    REGISTER_STATE_SIZE.load(Ordering::Relaxed)
}

/// Return the size required for all supported features, whether requested or not.
#[inline(always)]
#[cfg(ktest)]
fn extended_register_max_size() -> usize {
    if !INFO_INITIALIZED.load(Ordering::Acquire) {
        core::hint::cold_path();
        return 0;
    }
    // SAFETY: `INFO_INITIALIZED` was observed `true` with `Acquire` ordering, which synchronizes
    // with the `Release` store at the end of CPU 0 initialization in `extended_register_init()`.
    unsafe { (*EXTENDED_REGISTER_INFO.0.get()).xsave_max_area_size }
}

/// Return all potentially supported (although possibly not currently enabled) state bits for xcr0.
#[inline(always)]
#[cfg(ktest)]
fn extended_xcr0_component_bitmap() -> u64 {
    if !INFO_INITIALIZED.load(Ordering::Acquire) {
        core::hint::cold_path();
        return 0;
    }
    // SAFETY: `INFO_INITIALIZED` was observed `true` with `Acquire` ordering, which synchronizes
    // with the `Release` store at the end of CPU 0 initialization in `extended_register_init()`.
    unsafe { (*EXTENDED_REGISTER_INFO.0.get()).xcr0_component_bitmap }
}

/// Returns whether or not xsave is supported by the CPU.
#[inline(always)]
pub fn xsave_supported() -> bool {
    // Acquire ordering (a plain `mov` load on x86-64) synchronizes with the Release store in
    // `extended_register_init()` so safe callers before `extended_register_init()` return `false`
    // without reading `EXTENDED_REGISTER_INFO` while CPU 0 may be initializing it.
    if !INFO_INITIALIZED.load(Ordering::Acquire) {
        core::hint::cold_path();
        return false;
    }
    // SAFETY: `INFO_INITIALIZED` was observed `true` with `Acquire` ordering, which synchronizes
    // with the `Release` store at the end of CPU 0 initialization in `extended_register_init()`.
    unsafe { (*EXTENDED_REGISTER_INFO.0.get()).xsave_supported }
}

/// Initialize a state vector. The passed in buffer must be [`X86_MAX_EXTENDED_REGISTER_SIZE`] big
/// and it must be 64-byte aligned. This function will initialize it for use in save and restore.
///
/// # Safety
///
/// The caller must ensure that [`extended_register_init`] has completed on CPU 0 and that
/// `register_state` points to a 64-byte aligned buffer of at least
/// [`X86_MAX_EXTENDED_REGISTER_SIZE`] (`1024`) uniquely writable bytes.
#[inline(always)]
pub unsafe fn extended_register_init_state(register_state: *mut u8) {
    // Copy the initialization state; this overcopies on systems that fall back
    // to fxsave, but the buffer is required to be large enough.
    // SAFETY: Caller guarantees `extended_register_init` has completed on CPU 0.
    let init = &unsafe { info() }.extended_register_init_state.0;
    // SAFETY: Caller guarantees `register_state` is valid and uniquely writable for
    // `X86_MAX_EXTENDED_REGISTER_SIZE` (`1024 >= init.len() == 576`) bytes, which cannot overlap
    // with the immutable static `EXTENDED_REGISTER_INFO`.
    unsafe {
        core::ptr::copy_nonoverlapping(init.as_ptr(), register_state, init.len());
    }
}

/// Initialize a state vector to a specific set of state bits. The passed in buffer must be
/// [`X86_MAX_EXTENDED_REGISTER_SIZE`] big and it must be 64-byte aligned. This function will
/// initialize it for use in save and restore.
///
/// # Safety
///
/// The caller must ensure that [`extended_register_init`] has completed on CPU 0 and that
/// `register_state` points to a 64-byte aligned buffer of at least
/// [`X86_MAX_EXTENDED_REGISTER_SIZE`] (`1024`) uniquely writable bytes. If `xstate_bv` sets any
/// component bits `>= 2` (such as [`X86_XSAVE_STATE_BIT_AVX`]), the caller must ensure the
/// corresponding extended area bytes in `register_state` are already initialized/zeroed.
#[inline]
pub unsafe fn extended_register_init_state_from_bv(register_state: *mut u8, xstate_bv: u64) {
    // TODO(https://fxbug.dev/540178114): `extended_register_init_state` only writes the first 576
    // bytes (`size_of::<XsaveArea>()`), leaving bytes `576..X86_MAX_EXTENDED_REGISTER_SIZE`
    // untouched when `xstate_bv` sets extended components `>= 2` (matching `registers.cc`).
    // SAFETY: Caller upholds `extended_register_init_state`'s safety preconditions.
    unsafe { extended_register_init_state(register_state) };

    // SAFETY: Caller guarantees `extended_register_init` has completed on CPU 0.
    let info = unsafe { info() };
    if !info.xsave_supported {
        return;
    }
    // SAFETY: `register_state` is 64-byte aligned and initialized with `sizeof(XsaveArea)` bytes.
    let area = unsafe { &mut *register_state.cast::<XsaveArea>() };
    area.xstate_bv = xstate_bv;

    // Ensure that xcomp_bv matches xstate_bv and that the saved state is in compact form. If any
    // bit in xcomp_bv is 0 and the corresponding bit in xstate_bv is 1, or if
    // XSAVE_XCOMP_BV_COMPACT is not set, xrstors will GPF.
    if info.xsaves_supported {
        area.xcomp_bv = area.xstate_bv | XSAVE_XCOMP_BV_COMPACT;
    }
}

/// Save current state to state vector.
///
/// # Safety
///
/// The caller must ensure that [`extended_register_init`] has completed on CPU 0, that
/// `register_state` is either null or points to a 64-byte aligned writable buffer of at least
/// [`X86_MAX_EXTENDED_REGISTER_SIZE`] bytes, and that CPL == 0, `CR0.EM == 0`, and `CR0.TS == 0`
/// (and `CR4.OSXSAVE == 1` when XSAVE is enabled).
#[inline(always)]
pub unsafe fn extended_register_save_state(register_state: *mut u8) {
    // The idle threads have no extended register state.
    if register_state.is_null() {
        core::hint::cold_path();
        return;
    }

    // SAFETY: Caller guarantees `extended_register_init` has completed on CPU 0, `register_state`
    // is non-null, 64-byte aligned, and large enough for the active save mechanism, and CPL == 0
    // with `CR0.EM == 0`, `CR0.TS == 0`, and `CR4.OSXSAVE == 1` when XSAVE is enabled.
    unsafe { extended_register_save_state_with_info(info(), register_state) };
}

/// # Safety
///
/// The caller must ensure that `register_state` is non-null and points to a 64-byte aligned
/// writable buffer of at least [`X86_MAX_EXTENDED_REGISTER_SIZE`] bytes, and that CPL == 0,
/// `CR0.EM == 0`, and `CR0.TS == 0` (and `CR4.OSXSAVE == 1` when `info.xsave_supported` is true).
#[inline(always)]
unsafe fn extended_register_save_state_with_info(
    info: &ExtendedRegisterInfo,
    register_state: *mut u8,
) {
    // SAFETY: Caller guarantees `register_state` is non-null, 64-byte aligned, and large enough
    // for the active save mechanism, and CPL == 0 with `CR0.EM == 0`, `CR0.TS == 0`, and
    // `CR4.OSXSAVE == 1` when `info.xsave_supported` is true.
    unsafe {
        if info.xsaves_supported {
            xsaves(register_state, !0u64);
        } else if info.xsaveopt_supported {
            xsaveopt(register_state, !0u64);
        } else if info.xsave_supported {
            xsave(register_state, !0u64);
        } else if info.fxsave_supported {
            fxsave(register_state);
        }
    }
}

/// Restore a state created by [`extended_register_init_state`] or
/// [`extended_register_save_state`].
///
/// # Safety
///
/// The caller must ensure that [`extended_register_init`] has completed on CPU 0, that
/// `register_state` is either null or points to a 64-byte aligned readable buffer containing a
/// valid extended register state image for the active mechanism, and that CPL == 0,
/// `CR0.EM == 0`, and `CR0.TS == 0` (and `CR4.OSXSAVE == 1` when XSAVE is enabled).
#[inline(always)]
pub unsafe fn extended_register_restore_state(register_state: *const u8) {
    // The idle threads have no extended register state.
    if register_state.is_null() {
        core::hint::cold_path();
        return;
    }

    // SAFETY: Caller guarantees `extended_register_init` has completed on CPU 0, `register_state`
    // is non-null, 64-byte aligned, and contains a valid state image for the active restore
    // mechanism, and CPL == 0 with `CR0.EM == 0`, `CR0.TS == 0`, and `CR4.OSXSAVE == 1` when XSAVE
    // is enabled.
    unsafe { extended_register_restore_state_with_info(info(), register_state) };
}

/// # Safety
///
/// The caller must ensure that `register_state` is non-null and points to a 64-byte aligned
/// readable buffer containing a valid extended register state image for the active mechanism, and
/// that CPL == 0, `CR0.EM == 0`, and `CR0.TS == 0` (and `CR4.OSXSAVE == 1` when
/// `info.xsave_supported` is true).
#[inline(always)]
unsafe fn extended_register_restore_state_with_info(
    info: &ExtendedRegisterInfo,
    register_state: *const u8,
) {
    // SAFETY: Caller guarantees `register_state` is non-null, 64-byte aligned, and contains a
    // valid state image for the active restore mechanism, and CPL == 0 with `CR0.EM == 0`,
    // `CR0.TS == 0`, and `CR4.OSXSAVE == 1` when `info.xsave_supported` is true.
    unsafe {
        if info.xsaves_supported {
            xrstors(register_state, !0u64);
        } else if info.xsave_supported {
            xrstor(register_state, !0u64);
        } else if info.fxsave_supported {
            fxrstor(register_state);
        }
    }
}

zr::static_assert!(align_of::<super::thread::ArchThread>() >= align_of::<AlignedInitState>());
zr::static_assert!(
    core::mem::offset_of!(super::thread::ArchThread, extended_register_buffer)
        % align_of::<AlignedInitState>()
        == 0
);
zr::static_assert!(
    // SAFETY: `ArchThread` (`arch_thread`) is a `#[repr(C)]` struct whose fields are primitive
    // integers, raw pointers, fixed-size arrays thereof, `bool` (`0` is `false`), and
    // `GeneralRegsSource` (`#[repr(u32)]` with `GeneralRegsSource::None = 0`), for all of which
    // all-zero bytes are valid, and `size_of_val` only queries the field size at compile time
    // without reading memory.
    unsafe {
        core::mem::size_of_val(
            &core::mem::zeroed::<super::thread::ArchThread>().extended_register_buffer,
        )
    } == X86_MAX_EXTENDED_REGISTER_SIZE
);

/// Performs an extended register context switch from `old_thread` to `new_thread`.
///
/// # Safety
///
/// The caller must ensure that [`extended_register_init`] has completed on CPU 0, that
/// `old_thread` and `new_thread` point to valid `Thread`s with 64-byte aligned
/// `extended_register_buffer`s (with exclusive access to `old_thread`'s writable buffer and shared
/// access to `new_thread`'s buffer containing a valid extended register state image created by
/// [`extended_register_init_state`], [`extended_register_init_state_from_bv`], or
/// [`extended_register_save_state`] for the duration of the call), and that CPL == 0,
/// `CR0.EM == 0`, and `CR0.TS == 0` (and `CR4.OSXSAVE == 1` when XSAVE is enabled).
#[inline(always)]
pub(super) unsafe fn extended_register_context_switch(
    old_thread: *mut Thread,
    new_thread: *const Thread,
) {
    // SAFETY: `extended_register_init` has completed on CPU 0, `old_thread` and `new_thread` are
    // valid per caller contract (making `&raw mut`/`&raw const` of `extended_register_buffer`
    // non-null, with `new_thread`'s buffer containing a valid extended register state image for the
    // active mechanism), CPL == 0 with `CR0.EM == 0`, `CR0.TS == 0`, and `CR4.OSXSAVE == 1` when
    // XSAVE is enabled, and the compile-time assertions above guarantee `extended_register_buffer`
    // is 64-byte aligned and `X86_MAX_EXTENDED_REGISTER_SIZE` bytes.
    unsafe {
        let info = info();
        let old_arch = super::thread::thread_arch(old_thread);
        extended_register_save_state_with_info(
            info,
            (&raw mut (*old_arch).extended_register_buffer).cast::<u8>(),
        );
        let new_arch = super::thread::thread_arch_const(new_thread);
        extended_register_restore_state_with_info(
            info,
            (&raw const (*new_arch).extended_register_buffer).cast::<u8>(),
        );
    }
}

fn read_xsave_state_info(info_mut: &mut ExtendedRegisterInfo) {
    let xsave_feat = feature::x86_feature_test(feature::X86_FEATURE_XSAVE);
    if !xsave_feat {
        ltracef!("xsave not supported\n");
        return;
    }

    // This procedure is described in Intel Vol 1 section 13.2.

    // Read feature support from subleaves 0 and 1.
    let Some(leaf0) = feature::get_cpuid_subleaf(X86CpuidLeafNum::XSave, 0) else {
        ltracef!("could not find xsave leaf\n");
        return;
    };
    let xcr0_bitmap = ((leaf0.d as u64) << 32) | (leaf0.a as u64);
    let mut max_area = XSAVE_EXTENDED_AREA_OFFSET;

    let leaf1 = feature::get_cpuid_subleaf(X86CpuidLeafNum::XSave, 1).unwrap_or_default();
    let xss_bitmap = ((leaf1.d as u64) << 32) | (leaf1.c as u64);

    ltracef!("xcr0 bitmap: {:016x}\n", xcr0_bitmap);
    ltracef!("xss bitmap: {:016x}\n", xss_bitmap);

    // Check; all CPUs that support xsave support components 0 and 1.
    debug_assert!((xcr0_bitmap & 0x3) == 0x3);
    if (xcr0_bitmap & 0x3) != 0x3 {
        ltracef!("unexpected xcr0 bitmap {:016x}\n", xcr0_bitmap);
        return;
    }

    // Read info about the state components.
    for (comp, i) in info_mut.state_components[XSAVE_FIRST_EXT_COMPONENT..XSAVE_MAX_COMPONENTS]
        .iter_mut()
        .zip(XSAVE_FIRST_EXT_COMPONENT..XSAVE_MAX_COMPONENTS)
    {
        let bit = 1u64 << i;
        if (xcr0_bitmap & bit) == 0 && (xss_bitmap & bit) == 0 {
            continue;
        }
        let leaf = feature::get_cpuid_subleaf(X86CpuidLeafNum::XSave, i as u32).unwrap_or_default();
        let align64 = (leaf.c & 0x2) != 0;

        *comp = StateComponent { size: leaf.a, align64 };
        ltracef!(
            "component {} size: {} (xcr0 {})\n",
            i,
            leaf.a,
            i32::from((xcr0_bitmap & bit) != 0),
        );

        if align64 {
            max_area = (max_area + 63) & !63;
        }
        max_area += leaf.a as usize;
    }

    // We're okay from now on out; if we bailed early above, all support fields remain false/0.
    info_mut.xcr0_component_bitmap = xcr0_bitmap;
    info_mut.xgetbv_1_supported = (leaf1.a & (1 << 2)) != 0;
    info_mut.xsaves_supported = (leaf1.a & (1 << 3)) != 0;
    info_mut.xsaveopt_supported = (leaf1.a & (1 << 0)) != 0;
    info_mut.xss_component_bitmap = xss_bitmap;
    info_mut.xsave_max_area_size = max_area;
    info_mut.xsave_supported = true;
    ltracef!("total xsave size: {}\n", max_area);
}

/// # Safety
///
/// The caller must ensure that CPL is 0, `CR4.OSXSAVE == 1` when `info.xsave_supported` is true,
/// and interrupts are disabled on the current CPU.
unsafe fn recompute_state_size(info: &ExtendedRegisterInfo) {
    debug_assert!(crate::arch_rs::ints_disabled());
    if !info.xsave_supported {
        return;
    }

    let mut new_size: usize;
    // If we're in a compacted form, compute the total size. The algorithm
    // for this is defined in Intel Vol 1 section 13.4.3.
    if info.xsaves_supported {
        new_size = XSAVE_EXTENDED_AREA_OFFSET;
        // SAFETY: Caller guarantees CPL 0 and `CR4.OSXSAVE == 1`.
        let enabled_features = unsafe { xgetbv(0) | read_msr(IA32_XSS_MSR) };
        for (comp, i) in info.state_components[XSAVE_FIRST_EXT_COMPONENT..XSAVE_MAX_COMPONENTS]
            .iter()
            .zip(XSAVE_FIRST_EXT_COMPONENT..XSAVE_MAX_COMPONENTS)
        {
            if (enabled_features & (1u64 << i)) == 0 {
                continue;
            }
            if comp.align64 {
                new_size = (new_size + 63) & !63;
            }
            new_size += comp.size as usize;
        }
    } else {
        // Otherwise, use CPUID.(EAX=0DH,ECX=0):EBX, which stores the computed
        // maximum size required for saving everything specified in XCR0.
        let leaf = feature::get_cpuid_subleaf(X86CpuidLeafNum::XSave, 0).unwrap_or_default();
        new_size = leaf.b as usize;
    }

    debug_assert!(new_size <= X86_MAX_EXTENDED_REGISTER_SIZE);
    // Only allow size to increase; all CPUs should converge to the same value,
    // but for safety let's keep it monotonically increasing.
    REGISTER_STATE_SIZE.fetch_max(new_size, Ordering::Relaxed);
}

/// # Safety
///
/// The caller must ensure that `register_state` is 16-byte aligned and valid for 512 writable
/// bytes, and that CPL is 0 with `CR0.EM == 0` and `CR0.TS == 0`.
#[inline(always)]
unsafe fn fxsave(register_state: *mut u8) {
    debug_assert!((register_state.addr() & 15) == 0);
    // SAFETY: Caller guarantees `register_state` is 16-byte aligned and valid for 512 writable
    // bytes at CPL 0 with `CR0.EM == 0` and `CR0.TS == 0`.
    unsafe {
        core::arch::asm!(
            "fxsave [{}]",
            in(reg) register_state,
            options(nostack, preserves_flags),
        );
    }
}

/// # Safety
///
/// The caller must ensure that `register_state` is 16-byte aligned and contains a valid 512-byte
/// FXSAVE image, and that CPL is 0 with `CR0.EM == 0` and `CR0.TS == 0`.
#[inline(always)]
unsafe fn fxrstor(register_state: *const u8) {
    debug_assert!((register_state.addr() & 15) == 0);
    // SAFETY: Caller guarantees `register_state` is 16-byte aligned and contains a valid 512-byte
    // FXSAVE image at CPL 0 with `CR0.EM == 0` and `CR0.TS == 0`.
    unsafe {
        core::arch::asm!(
            "fxrstor [{}]",
            in(reg) register_state,
            options(nostack, preserves_flags),
        );
    }
}

/// # Safety
///
/// The caller must ensure that CPL is 0, `CR0.TS == 0`, `CR4.OSXSAVE == 1`, and `register_state`
/// is 64-byte aligned and contains a valid XSAVE image.
#[inline(always)]
unsafe fn xrstor(register_state: *const u8, feature_mask: u64) {
    debug_assert!((register_state.addr() & 63) == 0);
    let low = feature_mask as u32;
    let high = (feature_mask >> 32) as u32;
    // SAFETY: Caller guarantees CPL 0, `CR0.TS == 0`, `CR4.OSXSAVE == 1`, and `register_state` is
    // 64-byte aligned and contains a valid XSAVE image.
    unsafe {
        core::arch::asm!(
            "xrstor [{}]",
            in(reg) register_state,
            in("edx") high,
            in("eax") low,
            options(nostack, preserves_flags),
        );
    }
}

/// # Safety
///
/// The caller must ensure that CPL is 0, `CR0.TS == 0`, `CR4.OSXSAVE == 1`, and `register_state`
/// is 64-byte aligned and contains a valid compacted XSAVES image.
#[inline(always)]
unsafe fn xrstors(register_state: *const u8, feature_mask: u64) {
    debug_assert!((register_state.addr() & 63) == 0);
    let low = feature_mask as u32;
    let high = (feature_mask >> 32) as u32;
    // SAFETY: Caller guarantees CPL 0, `CR0.TS == 0`, `CR4.OSXSAVE == 1`, and `register_state` is
    // 64-byte aligned and contains a valid compacted XSAVES image.
    unsafe {
        core::arch::asm!(
            "xrstors [{}]",
            in(reg) register_state,
            in("edx") high,
            in("eax") low,
            options(nostack, preserves_flags),
        );
    }
}

/// # Safety
///
/// The caller must ensure that CPL is 0, `CR0.TS == 0`, `CR4.OSXSAVE == 1`, and `register_state`
/// is 64-byte aligned and valid for the enabled XSAVE area size.
#[inline(always)]
unsafe fn xsave(register_state: *mut u8, feature_mask: u64) {
    debug_assert!((register_state.addr() & 63) == 0);
    let low = feature_mask as u32;
    let high = (feature_mask >> 32) as u32;
    // SAFETY: Caller guarantees CPL 0, `CR0.TS == 0`, `CR4.OSXSAVE == 1`, and `register_state` is
    // 64-byte aligned and valid for the enabled XSAVE area size.
    unsafe {
        core::arch::asm!(
            "xsave [{}]",
            in(reg) register_state,
            in("edx") high,
            in("eax") low,
            options(nostack, preserves_flags),
        );
    }
}

/// # Safety
///
/// The caller must ensure that CPL is 0, `CR0.TS == 0`, `CR4.OSXSAVE == 1`, and `register_state`
/// is 64-byte aligned and valid for the enabled XSAVE area size.
#[inline(always)]
unsafe fn xsaveopt(register_state: *mut u8, feature_mask: u64) {
    debug_assert!((register_state.addr() & 63) == 0);
    let low = feature_mask as u32;
    let high = (feature_mask >> 32) as u32;
    // SAFETY: Caller guarantees CPL 0, `CR0.TS == 0`, `CR4.OSXSAVE == 1`, and `register_state` is
    // 64-byte aligned and valid for the enabled XSAVE area size.
    unsafe {
        core::arch::asm!(
            "xsaveopt [{}]",
            in(reg) register_state,
            in("edx") high,
            in("eax") low,
            options(nostack, preserves_flags),
        );
    }
}

/// # Safety
///
/// The caller must ensure that CPL is 0, `CR0.TS == 0`, `CR4.OSXSAVE == 1`, and `register_state`
/// is 64-byte aligned and valid for the enabled XSAVES area size.
#[inline(always)]
unsafe fn xsaves(register_state: *mut u8, feature_mask: u64) {
    debug_assert!((register_state.addr() & 63) == 0);
    let low = feature_mask as u32;
    let high = (feature_mask >> 32) as u32;
    // SAFETY: Caller guarantees CPL 0, `CR0.TS == 0`, `CR4.OSXSAVE == 1`, and `register_state` is
    // 64-byte aligned and valid for the enabled XSAVES area size.
    unsafe {
        core::arch::asm!(
            "xsaves [{}]",
            in(reg) register_state,
            in("edx") high,
            in("eax") low,
            options(nostack, preserves_flags),
        );
    }
}

/// # Safety
///
/// The caller must ensure that `CR4.OSXSAVE` is enabled and `reg` is a valid XCR index.
#[inline(always)]
pub unsafe fn xgetbv(reg: u32) -> u64 {
    let hi: u32;
    let lo: u32;
    // SAFETY: Caller guarantees `CR4.OSXSAVE` is enabled and `reg` is valid.
    unsafe {
        core::arch::asm!(
            "xgetbv",
            out("edx") hi,
            out("eax") lo,
            in("ecx") reg,
            options(nostack, preserves_flags),
        );
    }
    ((hi as u64) << 32) + (lo as u64)
}

/// # Safety
///
/// The caller must ensure that CPL is 0, `CR4.OSXSAVE` is enabled, and `val` is a valid
/// configuration for XCR `reg`.
#[inline(always)]
pub unsafe fn xsetbv(reg: u32, val: u64) {
    let hi = (val >> 32) as u32;
    let lo = val as u32;
    // SAFETY: Caller guarantees CPL 0, `CR4.OSXSAVE` is enabled, and `val` is valid for `reg`.
    unsafe {
        core::arch::asm!(
            "xsetbv",
            in("ecx") reg,
            in("edx") hi,
            in("eax") lo,
            options(nostack, preserves_flags),
        );
    }
}

/// Returns a pointer to the [`X86XsaveLegacyArea`] within the given xsave area, which holds the
/// x87 (`component == 0`) and SSE (`component == 1`) state components. These two components are
/// always present, so unlike [`get_extended_register_state_component`] this cannot fail.
///
/// If the requested component was not written because its registers were in the initial
/// configuration, its initial values are filled in. If you plan on writing to the area, set
/// `mark_present = true` so the component is marked used and restored when the thread resumes.
/// `mark_present` only affects the requested component, so when writing both x87 and SSE state,
/// make two separate calls.
///
/// The returned pointer is non-null, 64-byte aligned, derived from `register_state`, and points
/// to an initialized [`X86XsaveLegacyArea`] occupying the first
/// `size_of::<X86XsaveLegacyArea>()` bytes of `register_state`.
///
/// # Safety
///
/// The caller must ensure that `component <= 1` and that `register_state` is 64-byte aligned,
/// valid and uniquely writable for [`X86_MAX_EXTENDED_REGISTER_SIZE`] (`1024`) bytes, and holds a
/// valid extended register state image: one initialized by [`extended_register_init_state`] or
/// [`extended_register_init_state_from_bv`], and optionally since updated by
/// [`extended_register_save_state`].
pub unsafe fn get_xsave_legacy_area(
    register_state: *mut u8,
    component: u32,
    mark_present: bool,
) -> *mut X86XsaveLegacyArea {
    debug_assert!(component <= 1);
    let area = register_state.cast::<XsaveArea>();
    let state_component_bit = 1u64 << component;

    // SAFETY: Caller guarantees `register_state` (`area`) is 64-byte aligned, uniquely writable
    // for `X86_MAX_EXTENDED_REGISTER_SIZE` (`1024 >= size_of::<XsaveArea>()`) bytes, and holds a
    // valid extended register state image, so its legacy area and header are initialized.
    unsafe {
        if ((*area).xstate_bv & state_component_bit) == 0 {
            // Component not written because registers were in the initial configuration. Set it
            // so the caller sees the correct initial values.
            set_legacy_component_initial_state(&mut (*area).legacy, component);
            if mark_present {
                (*area).xstate_bv |= state_component_bit;
            }
        }
        &raw mut (*area).legacy
    }
}

/// Returns the address within the given xsave area of the requested state component. The state
/// component indexes formats are described in section 13.4 of the Intel Software Developer's
/// manual. Use the `X86_XSAVE_STATE_INDEX_*` constants for the component indices.
///
/// The given register state must have previously been filled with the variant of XSAVE that the
/// system is using. Since the save area can be compressed, the offset of each component can vary
/// depending on the contents.
///
/// The components 0 and 1 are special and refer to the legacy area. In both cases a slice covering
/// the [`X86XsaveLegacyArea`] will be returned. Note that `mark_present = true` will only affect
/// the requested component, so if you're writing to both x87 and SSE states, make two separate
/// calls even though the returned slice will be the same.
///
/// Some parts of the xsave area can be marked as unused to optimize. If you plan on
/// writing to the area, set `mark_present = true` which will ensure that the corresponding area is
/// marked used. Without this, the registers might not be restored when the thread is resumed. This
/// is not currently supported for components >= 2. This means that to set AVX registers, for
/// example, AVX needed to have been previously used by the thread in question. This capability can
/// be added in the future if required.
///
/// Returns `Some(*mut [u8])` containing a slice of the requested component if present,
/// or `None` if the component is not present or invalid. A returned slice is non-null, derived
/// from `register_state`, lies within its first [`X86_MAX_EXTENDED_REGISTER_SIZE`] bytes, and is
/// initialized. For components 0 and 1 it covers the 64-byte aligned [`X86XsaveLegacyArea`] at the
/// start of `register_state` (see [`get_xsave_legacy_area`]).
///
/// # Safety
///
/// The caller must ensure that [`extended_register_init`] has completed on CPU 0, and that
/// `register_state` is 64-byte aligned, valid and uniquely writable for
/// [`X86_MAX_EXTENDED_REGISTER_SIZE`] (`1024`) bytes, and holds a valid extended register state
/// image: one initialized by [`extended_register_init_state`] or
/// [`extended_register_init_state_from_bv`], and optionally since updated by
/// [`extended_register_save_state`].
pub unsafe fn get_extended_register_state_component(
    register_state: *mut u8,
    component: u32,
    mark_present: bool,
) -> Option<*mut [u8]> {
    if (component as usize) >= XSAVE_MAX_COMPONENTS {
        core::hint::cold_path();
        return None;
    }

    // Components 0 and 1 are special and are always present in the legacy area.
    if component <= 1 {
        // SAFETY: Caller upholds the same preconditions as `get_xsave_legacy_area`, and
        // `component <= 1`.
        let legacy = unsafe { get_xsave_legacy_area(register_state, component, mark_present) };
        let size = size_of::<X86XsaveLegacyArea>();
        return Some(core::ptr::slice_from_raw_parts_mut(legacy.cast::<u8>(), size));
    }

    let area = register_state.cast::<XsaveArea>();
    let state_component_bit = 1u64 << component;

    // SAFETY: `area` points to the initialized `XsaveArea` header of `register_state`.
    let xcomp_bv = unsafe { (*area).xcomp_bv };
    if (xcomp_bv & XSAVE_XCOMP_BV_COMPACT) == 0 {
        // Standard format. The offset and size are provided by a static CPUID call.
        let leaf =
            feature::get_cpuid_subleaf(X86CpuidLeafNum::XSave, component).unwrap_or_default();
        let size = leaf.a as usize;
        if size == 0 {
            core::hint::cold_path();
            return None;
        }
        let offset = leaf.b as usize;
        // Soundness: ensure `offset + size` fits within the caller's 1024-byte buffer before
        // pointer arithmetic (`register_state.add(offset)`) and `write_bytes` / slice creation.
        if !offset.checked_add(size).is_some_and(|end| end <= X86_MAX_EXTENDED_REGISTER_SIZE) {
            debug_assert!(false, "component size exceeds max register size");
            core::hint::cold_path();
            return None;
        }
        // SAFETY: Caller guarantees `area` points to the valid `XsaveArea` header and
        // `register_state.add(offset)` is correctly bounded.
        let component_begin = unsafe {
            let component_begin = register_state.add(offset);
            if ((*area).xstate_bv & state_component_bit) == 0 {
                // Component not written because it's in the initial state. Write the initial
                // values to the structure so the caller sees the correct data. The initial state
                // of all non-x87 xsave components (x87 is handled above) is all 0's.
                core::ptr::write_bytes(component_begin, 0, size);
                if mark_present {
                    (*area).xstate_bv |= state_component_bit;
                }
            }
            component_begin
        };
        return Some(core::ptr::slice_from_raw_parts_mut(component_begin, size));
    }

    // Compacted format used. The corresponding bit in xcomp_bv indicates whether the component is
    // present.
    if (xcomp_bv & state_component_bit) == 0 {
        // Currently this doesn't support reading or writing compacted components that aren't
        // currently marked present. In the future, we may want to add this which will require
        // rewriting all the following components.
        core::hint::cold_path();
        return None;
    }

    // SAFETY: Caller guarantees `extended_register_init()` has completed on CPU 0.
    let info = unsafe { info() };
    // Walk all present components and add up their sizes (optionally aligned up) to get the
    // offset.
    let mut offset = XSAVE_EXTENDED_AREA_OFFSET;
    #[expect(clippy::needless_range_loop)]
    for i in XSAVE_FIRST_EXT_COMPONENT..component as usize {
        if (xcomp_bv & (1u64 << i)) == 0 {
            continue;
        }
        let comp = &info.state_components[i];
        if comp.align64 {
            offset = (offset + 63) & !63;
        }
        offset += comp.size as usize;
    }
    let comp = &info.state_components[component as usize];
    if comp.align64 {
        offset = (offset + 63) & !63;
    }
    let size = comp.size as usize;
    // Soundness: ensure `offset + size` fits within the caller's 1024-byte buffer before
    // pointer arithmetic (`register_state.add(offset)`) and `write_bytes` / slice creation.
    if !offset.checked_add(size).is_some_and(|end| end <= X86_MAX_EXTENDED_REGISTER_SIZE) {
        debug_assert!(false, "component size exceeds max register size");
        core::hint::cold_path();
        return None;
    }

    // SAFETY: Caller guarantees `area` points to the valid `XsaveArea` header and
    // `register_state.add(offset)` is correctly bounded.
    let component_begin = unsafe {
        let component_begin = register_state.add(offset);
        if ((*area).xstate_bv & state_component_bit) == 0 {
            // Component not written because it's in the initial state. Write the initial
            // values to the structure so the caller sees the correct data. The initial state
            // of all non-x87 xsave components (x87 is handled above) is all 0's.
            core::ptr::write_bytes(component_begin, 0, size);
            if mark_present {
                (*area).xstate_bv |= state_component_bit;
            }
        }
        component_begin
    };
    Some(core::ptr::slice_from_raw_parts_mut(component_begin, size))
}

// Debug Registers --------------------------------------------------------------------------------

/// Checks whether the given state is valid to install on a running thread.
///
/// Validates whether each non-zero address in `debug_state.dr` is a valid user-space address to
/// save into a debug register, and masks out reserved values on `DR6` and `DR7`. This is for the
/// caller's convenience, considering that we don't have a good mechanism to communicate back to the
/// user what went wrong with the call.
pub fn validate_debug_state(debug_state: &mut X86DebugState) -> bool {
    // Validate the addresses being written.
    for addr in debug_state.dr {
        if addr == 0 {
            continue;
        }
        if !vm::is_user_accessible(addr as usize) || !vm::is_vaddr_canonical(addr as usize) {
            return false;
        }
    }

    // Ensure that only user writable bits in DR6 are set.
    let dr6 = debug_state.dr6;
    debug_state.dr6 = X86_DR6_MASK | (dr6 & X86_DR6_USER_MASK);

    // DR7.
    let dr7 = debug_state.dr7;
    debug_state.dr7 = X86_DR7_MASK | (dr7 & X86_DR7_USER_MASK);

    true
}

/// Read the raw value of `dr6` without any fixups. This is exposed for informational purposes only.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 and `DR7.GD == 0` so debug
/// register access does not trap.
#[inline(always)]
pub(super) unsafe fn read_raw_debug_status() -> u64 {
    let value: u64;
    // SAFETY: Caller guarantees CPL 0 and `DR7.GD == 0`. Reading `%dr6` has no stack side effects.
    unsafe {
        core::arch::asm!("mov {}, dr6", out(reg) value, options(nostack));
    }
    value
}

/// Only update the status section of `debug_state` (`DR6`). All other state will not be modified.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 and `DR7.GD == 0` so debug
/// register access does not trap.
#[inline(always)]
pub(super) unsafe fn read_debug_status() -> u64 {
    // NOTE: There is a difference in bit 16 between Intel64 and AMD64.
    //       In AMD, bit 16 is reserved and always set to 0.
    //       In Intel, it can mean that a debug or breakpoint exception occurred during an RTM
    //       block. For now, we mask this bit to make both platforms uniform.
    // SAFETY: Caller guarantees CPL 0 and `DR7.GD == 0`.
    unsafe { read_raw_debug_status() | X86_DR6_MASK }
}

/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 and `DR7.GD == 0` so debug
/// register access does not trap.
#[inline(always)]
pub(super) unsafe fn write_debug_status(dr6: u64) {
    // Upper 32 bits must be 0.
    let full_value = dr6 & 0x0000_0000_ffff_ffff;
    // SAFETY: Caller guarantees CPL 0 and `DR7.GD == 0`. Writing `%dr6` with upper 32 bits cleared
    // has no stack side effects.
    unsafe {
        core::arch::asm!(
            "mov dr6, {}",
            in(reg) full_value,
            options(nostack),
        );
    }
}

/// Disables the HW debug functionalities for the current thread.
/// There is no "enable" call. To do this, use the [`write_hw_debug_regs`] call.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 and `DR7.GD == 0` so debug
/// register access does not trap.
#[inline(always)]
pub unsafe fn disable_debug_state() {
    // Disabling dr7 is enough to disable the debug functionality.
    let zero_val: u64 = 0;
    // SAFETY: Caller guarantees CPL 0 and `DR7.GD == 0`. Zeroing `%dr7` disables hardware debug
    // breakpoints/watchpoints.
    unsafe {
        core::arch::asm!("mov dr7, {}", in(reg) zero_val, options(nostack));
    }
}

/// Read from the CPU registers into `debug_state`.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 and `DR7.GD == 0` so debug
/// register access does not trap.
#[inline(always)]
pub unsafe fn read_hw_debug_regs() -> X86DebugState {
    let mut debug_state = X86DebugState::default();
    // SAFETY: Caller guarantees CPL 0 and `DR7.GD == 0`.
    unsafe {
        core::arch::asm!(
            "mov {}, dr0",
            out(reg) debug_state.dr[0],
            options(nostack),
        );
        core::arch::asm!(
            "mov {}, dr1",
            out(reg) debug_state.dr[1],
            options(nostack),
        );
        core::arch::asm!(
            "mov {}, dr2",
            out(reg) debug_state.dr[2],
            options(nostack),
        );
        core::arch::asm!(
            "mov {}, dr3",
            out(reg) debug_state.dr[3],
            options(nostack),
        );

        debug_state.dr6 = read_debug_status();

        core::arch::asm!(
            "mov {}, dr7",
            out(reg) debug_state.dr7,
            options(nostack),
        );
    }
    debug_state
}

/// Write from the `debug_state` into the CPU registers.
///
/// IMPORTANT: This function is used in the context switch, so no validation is done, just writing.
///            In any other context (eg. setting debug values from a syscall), you *MUST* call
///            [`validate_debug_state`] first.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0, `DR7.GD == 0`, and that
/// `debug_state` has previously been validated and masked with [`validate_debug_state`] (or saved
/// directly from hardware via [`read_hw_debug_regs`]).
#[inline(always)]
pub unsafe fn write_hw_debug_regs(debug_state: &X86DebugState) {
    // SAFETY: Caller guarantees CPL 0, `DR7.GD == 0`, and that `debug_state` was validated by
    // `validate_debug_state` or read from hardware.
    unsafe {
        core::arch::asm!(
            "mov dr0, {}",
            in(reg) debug_state.dr[0],
            options(nostack),
        );
        core::arch::asm!(
            "mov dr1, {}",
            in(reg) debug_state.dr[1],
            options(nostack),
        );
        core::arch::asm!(
            "mov dr2, {}",
            in(reg) debug_state.dr[2],
            options(nostack),
        );
        core::arch::asm!(
            "mov dr3, {}",
            in(reg) debug_state.dr[3],
            options(nostack),
        );

        // IMPORTANT: DR6 and DR7 should be already masked at this point by calling
        // `validate_debug_state`.
        write_debug_status(debug_state.dr6);

        core::arch::asm!(
            "mov dr7, {}",
            in(reg) debug_state.dr7,
            options(nostack),
        );
    }
}

#[cfg(ktest)]
fn x86_print_dr6(dr6: u64) {
    kprint::kprintln!(
        "0x{:08x}: B0={}, B1={}, B2={}, B3={}, BD={}, BS={}, BT={}",
        dr6,
        u32::from((dr6 & X86_DR6_B0) != 0),
        u32::from((dr6 & X86_DR6_B1) != 0),
        u32::from((dr6 & X86_DR6_B2) != 0),
        u32::from((dr6 & X86_DR6_B3) != 0),
        u32::from((dr6 & X86_DR6_BD) != 0),
        u32::from((dr6 & X86_DR6_BS) != 0),
        u32::from((dr6 & X86_DR6_BT) != 0),
    );
}

#[cfg(ktest)]
fn x86_print_dr7(dr7: u64) {
    kprint::kprintln!(
        "0x{:08x}: L0={}, G0={}, L1={}, G1={}, L2={}, G2={}, L3={}, G3={}, LE={}, GE={}, GD={}",
        dr7,
        u32::from((dr7 & X86_DR7_L0) != 0),
        u32::from((dr7 & X86_DR7_G0) != 0),
        u32::from((dr7 & X86_DR7_L1) != 0),
        u32::from((dr7 & X86_DR7_G1) != 0),
        u32::from((dr7 & X86_DR7_L2) != 0),
        u32::from((dr7 & X86_DR7_G2) != 0),
        u32::from((dr7 & X86_DR7_L3) != 0),
        u32::from((dr7 & X86_DR7_G3) != 0),
        u32::from((dr7 & X86_DR7_LE) != 0),
        u32::from((dr7 & X86_DR7_GE) != 0),
        u32::from((dr7 & X86_DR7_GD) != 0),
    );
    kprint::kprintln!(
        "{:12}R/W0={}, LEN0={}, R/W1={}, LEN1={}, R/W2={}, LEN2={}, R/W3={}, LEN3={}",
        " ",
        ((dr7 & X86_DR7_RW0) >> 16) as u32,
        ((dr7 & X86_DR7_LEN0) >> 18) as u32,
        ((dr7 & X86_DR7_RW1) >> 20) as u32,
        ((dr7 & X86_DR7_LEN1) >> 22) as u32,
        ((dr7 & X86_DR7_RW2) >> 24) as u32,
        ((dr7 & X86_DR7_LEN2) >> 26) as u32,
        ((dr7 & X86_DR7_RW3) >> 28) as u32,
        ((dr7 & X86_DR7_LEN3) >> 30) as u32,
    );
}

/// # Safety
///
/// The caller must ensure that CPL == 0 (required by `x86_get_cr2()`) and that `frame` points to a
/// valid, readable `Iframe`.
#[sanitize(address = "off")]
pub unsafe fn print_frame(frame: *const Iframe, file: &mut impl Write) {
    // SAFETY: Caller guarantees CPL 0 and that `frame` points to a valid, readable `Iframe`.
    // Copy `*frame` inside this `#[sanitize(address = "off")]` function before invoking `writeln!`
    // so formatting traits never dereference references into a potentially poisoned stack frame.
    let (f, cr2) = unsafe { (*frame, x86_get_cr2()) };
    let _ = writeln!(file, "iframe {:p}:", frame);
    let _ = writeln!(
        file,
        " CS:  {:#18x} RIP: {:#18x} EFL: {:#18x} CR2: {:#18x}",
        f.cs, f.ip, f.flags, cr2,
    );
    let _ = writeln!(
        file,
        " RAX: {:#18x} RBX: {:#18x} RCX: {:#18x} RDX: {:#18x}",
        f.rax, f.rbx, f.rcx, f.rdx,
    );
    let _ = writeln!(
        file,
        " RSI: {:#18x} RDI: {:#18x} RBP: {:#18x} RSP: {:#18x}",
        f.rsi, f.rdi, f.rbp, f.user_sp,
    );
    let _ = writeln!(
        file,
        "  R8: {:#18x}  R9: {:#18x} R10: {:#18x} R11: {:#18x}",
        f.r8, f.r9, f.r10, f.r11,
    );
    let _ = writeln!(
        file,
        " R12: {:#18x} R13: {:#18x} R14: {:#18x} R15: {:#18x}",
        f.r12, f.r13, f.r14, f.r15,
    );
    let _ = writeln!(file, " vector: {:#15x} errc: {:#17x}", f.vector, f.err_code);
}

/// Unit tests for x86 extended and debug register helpers.
#[cfg(ktest)]
#[unittest::suite(name = "x86_registers_tests")]
mod tests {
    use crate::arch_rs::InterruptDisableGuard;
    use kalloc::Box;
    use unittest::{expect_eq, expect_false, expect_true, unwrap_ok, unwrap_some};

    #[repr(C, align(64))]
    struct AlignedRegBuffer([u8; X86_MAX_EXTENDED_REGISTER_SIZE]);

    /// Tests extended register discovery and size invariants.
    #[test]
    fn test_extended_register_info() {
        let size = extended_register_size();
        expect_true!(size >= 512);
        expect_true!(size <= X86_MAX_EXTENDED_REGISTER_SIZE);
        if xsave_supported() {
            expect_true!(extended_register_max_size() >= size);
            expect_eq!(extended_xcr0_component_bitmap() & 0x3, 0x3);
        }
    }

    /// Tests extended register state initialization and component lookup.
    #[test]
    fn test_extended_register_state_components() {
        // SAFETY: AlignedRegBuffer only contains an array of u8, so any bit pattern is valid.
        let mut buf = unsafe {
            let mut uninit = unwrap_ok!(Box::<AlignedRegBuffer>::try_new_uninit());
            core::ptr::write_bytes(uninit.as_mut_ptr(), 0xff, 1);
            uninit.assume_init()
        };
        // SAFETY: `extended_register_init()` has completed on CPU 0, and `buf.0` is 64-byte aligned
        // and `X86_MAX_EXTENDED_REGISTER_SIZE` bytes long.
        unsafe { extended_register_init_state(buf.0.as_mut_ptr()) };

        // SAFETY: `extended_register_init()` has completed on CPU 0, and `buf.0` is a valid 64-byte
        // aligned extended register buffer.
        let x87_opt = unsafe {
            get_extended_register_state_component(
                buf.0.as_mut_ptr(),
                X86_XSAVE_STATE_INDEX_X87,
                false,
            )
        };
        expect_true!(x87_opt.is_some());
        let x87_slice = unwrap_some!(x87_opt);
        expect_eq!(x87_slice.len(), size_of::<X86XsaveLegacyArea>());
        // SAFETY: `x87_slice` contains the initialized `X86XsaveLegacyArea` inside `buf`.
        let legacy = unsafe { &*x87_slice.cast::<X86XsaveLegacyArea>() };
        expect_eq!(legacy.fcw, 0x037f);
        expect_eq!(legacy.mxcsr, 0x3f << 7);

        // SAFETY: `extended_register_init()` has completed on CPU 0, and `buf.0` is a valid 64-byte
        // aligned extended register buffer.
        unsafe { extended_register_init_state_from_bv(buf.0.as_mut_ptr(), 0) };
        // SAFETY: `extended_register_init()` has completed on CPU 0, and `buf.0` is a valid 64-byte
        // aligned extended register buffer.
        let sse_opt = unsafe {
            get_extended_register_state_component(
                buf.0.as_mut_ptr(),
                X86_XSAVE_STATE_INDEX_SSE,
                true,
            )
        };
        expect_true!(sse_opt.is_some());
        if xsave_supported() {
            // SAFETY: `buf.0` is a valid `XsaveArea`.
            let area = unsafe { &*buf.0.as_ptr().cast::<XsaveArea>() };
            expect_eq!(area.xstate_bv & X86_XSAVE_STATE_BIT_SSE, X86_XSAVE_STATE_BIT_SSE);
        }

        // SAFETY: `extended_register_init()` has completed on CPU 0 before unit tests run.
        let info = unsafe { info() };
        if info.xsave_supported && (info.xcr0_component_bitmap & X86_XSAVE_STATE_BIT_AVX) != 0 {
            // SAFETY: `extended_register_init()` has completed on CPU 0, and `buf.0` is a valid,
            // initialized 64-byte aligned extended register buffer.
            unsafe {
                extended_register_init_state_from_bv(buf.0.as_mut_ptr(), X86_XSAVE_STATE_BIT_AVX)
            };
            // SAFETY: `extended_register_init()` has completed on CPU 0, and `buf.0` is a valid
            // 64-byte aligned extended register buffer.
            let avx_opt = unsafe {
                get_extended_register_state_component(
                    buf.0.as_mut_ptr(),
                    X86_XSAVE_STATE_INDEX_AVX,
                    true,
                )
            };
            expect_true!(avx_opt.is_some());
        }

        // Out-of-bounds component index returns None.
        // SAFETY: `extended_register_init()` has completed on CPU 0, and `buf.0` is a valid 64-byte
        // aligned extended register buffer.
        let invalid_opt = unsafe {
            get_extended_register_state_component(
                buf.0.as_mut_ptr(),
                XSAVE_MAX_COMPONENTS as u32,
                false,
            )
        };
        expect_true!(invalid_opt.is_none());
    }

    /// Tests hardware debug register state validation and reserved bit masking.
    #[test]
    fn test_validate_debug_state() {
        let mut state =
            X86DebugState { dr: [0, 0x1000, 0x0000_7fff_ffff_0000, 0], dr6: !0u64, dr7: !0u64 };
        expect_true!(validate_debug_state(&mut state));
        expect_eq!(state.dr6, X86_DR6_MASK | X86_DR6_USER_MASK);
        expect_eq!(state.dr7, X86_DR7_MASK | X86_DR7_USER_MASK);
        expect_eq!(state.dr7 & X86_DR7_GD, 0);

        // Kernel address in DR0 is rejected.
        let mut bad_kernel = X86DebugState { dr: [0xffff_8000_0000_0000, 0, 0, 0], dr6: 0, dr7: 0 };
        expect_false!(validate_debug_state(&mut bad_kernel));

        // Non-canonical address in DR1 is rejected.
        let mut bad_non_canon =
            X86DebugState { dr: [0, 0x0000_8000_0000_0000, 0, 0], dr6: 0, dr7: 0 };
        expect_false!(validate_debug_state(&mut bad_non_canon));
    }

    /// Tests `print_frame`, `x86_print_dr6`, and `x86_print_dr7`.
    #[test]
    fn test_print_frame_and_dr() {
        struct CountWriter(usize);
        impl Write for CountWriter {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                self.0 += s.len();
                Ok(())
            }
        }
        let frame = Iframe::default();
        let mut writer = CountWriter(0);
        // SAFETY: Kernel tests execute at CPL 0 and `&frame` points to a valid `Iframe`.
        unsafe { print_frame(&frame, &mut writer) };
        expect_true!(writer.0 > 100);

        x86_print_dr6(X86_DR6_MASK | X86_DR6_B0);
        x86_print_dr7(X86_DR7_MASK | X86_DR7_L0);
    }

    /// Tests that save and restore do not crash and can round-trip the current CPU state.
    #[test]
    fn test_save_restore_round_trip() {
        // SAFETY: AlignedRegBuffer only contains an array of u8, which can safely be zeroed.
        let mut buf =
            unsafe { unwrap_ok!(Box::<AlignedRegBuffer>::try_new_zeroed()).assume_init() };
        let _guard = InterruptDisableGuard::new();
        // SAFETY: `extended_register_init()` has completed, we are at CPL 0 with `CR0.TS == 0`
        // and `CR4.OSXSAVE == 1` when XSAVE is enabled, and `buf.0` is a valid 64-byte aligned
        // extended register buffer.
        unsafe { extended_register_save_state(buf.0.as_mut_ptr()) };
        // SAFETY: Same as above, and `buf.0` was populated by `extended_register_save_state`.
        unsafe { extended_register_restore_state(buf.0.as_ptr()) };
    }

    /// Tests that debug registers can be read and written back without crashing.
    #[test]
    fn test_dr_read_write_round_trip() {
        let _guard = InterruptDisableGuard::new();
        // SAFETY: Executing in kernel space at CPL 0 with `DR7.GD == 0`.
        let mut state = unsafe { read_hw_debug_regs() };
        let orig_state = state;
        expect_true!(validate_debug_state(&mut state));
        // SAFETY: Executing in kernel space at CPL 0 with `DR7.GD == 0`, and `state` is validated.
        unsafe { write_hw_debug_regs(&state) };
        // SAFETY: Executing in kernel space at CPL 0 with `DR7.GD == 0`.
        let readback = unsafe { read_hw_debug_regs() };
        expect_eq!(readback.dr[0], state.dr[0]);
        expect_eq!(readback.dr[1], state.dr[1]);
        expect_eq!(readback.dr[2], state.dr[2]);
        expect_eq!(readback.dr[3], state.dr[3]);
        expect_eq!(readback.dr6, state.dr6);
        expect_eq!(readback.dr7, state.dr7);
        // Restore original hermetic state directly without mutating it via validate_debug_state.
        // SAFETY: Executing in kernel space at CPL 0 with `DR7.GD == 0`, and `orig_state` was saved
        // directly from hardware via `read_hw_debug_regs`.
        unsafe { write_hw_debug_regs(&orig_state) };
    }
}
