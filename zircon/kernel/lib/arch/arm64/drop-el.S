// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/arm64/feature-asm.h>
#include <lib/arch/arm64/system-asm.h>
#include <lib/arch/asm.h>

// void ArmDropToEl1();
//
// **NOTE:** This function can be called as a normal C function.  But it takes
// care to touch only a few registers.  It leaves the x0..x8 argument registers
// intact.  It uses only the following x9..x11 registers for scratch (and x30
// for return address).  Calls from assembly can rely on all other registers
// being preserved, as well as no references to sp or any memory, etc.

scratch .req x9
sysreg_scratch .req x10
hcrx_bits .req x11

// Do a tst on the value of a system register.
.macro mrs_tst sysreg, mask
  mrs sysreg_scratch, \sysreg
  tst sysreg_scratch, \mask
.endm

// Do an mrs with an immmediate or other operand only valid for mov, not mrs.
.macro msr_mov sysreg, value
  mov sysreg_scratch, \value
  msr \sysreg, sysreg_scratch
.endm

// Copy one system register to another.
.macro msr_mrs sysreg, value
  mrs sysreg_scratch, \value
  msr \sysreg, sysreg_scratch
.endm

.function ArmDropToEl1, global
  mrs scratch, CurrentEL

  // Check the current exception level.
  cmp scratch, CURRENT_EL_EL_FIELD(1)
  beq .Ltarget
  cmp scratch, CURRENT_EL_EL_FIELD(2)
  beq .Lconfigure_el2
  // Otherwise, we are in EL3.
  //
  // Throw a fatal exception for now; support will be added for this case when
  // it can be developed against hardware that drops us there.
  udf #0

.Lconfigure_el2:
  // Check for FEAT_HCX.
  mrs_tst id_aa64mmfr1_el1, #ID_AA64MMFR1_EL1_HCX
  beq .Lno_hcx

  // Set HCRX_EL2 bits so nothing traps to EL2 and EL2 doesn't constrain what
  // EL1 can do.  Unfortunately, the 0 state is not always that state.  Several
  // bits should be 1, but only if some FEAT_* is available.  The comments in
  // <lib/arch/arm64/system.h> for each HCRX_EL2 bit say whether each bit
  // should be set for some FEAT_*.  If more HCRX_EL2 bits are defined, then
  // new `set_hcrx_bits` invocations should be added here.

  mov hcrx_bits, xzr
  .macro set_hcrx_bits feature_reg, feature_mask, set_bit
    mov scratch, \set_bit
    mrs_tst \feature_reg, \feature_mask
    csel scratch, scratch, xzr, ne
    orr hcrx_bits, hcrx_bits, scratch
  .endm

  // FEAT_SYSREG128
  set_hcrx_bits id_aa64isar2_el1, #ID_AA64ISAR2_EL1_SYSREG_128, #HCRX_EL2_ENIDCP128

  // FEAT_D128
  set_hcrx_bits id_aa64mmfr3_el1, #ID_AA64MMFR3_EL1_D128, #HCRX_EL2_D128EN

  // FEAT_THE
  set_hcrx_bits id_aa64pfr1_el1, #ID_AA64PFR1_EL1_THE, #HCRX_EL2_PTTWI

  // FEAT_SCTLR2
  set_hcrx_bits id_aa64mmfr3_el1, #ID_AA64MMFR3_EL1_SCTLRX, #HCRX_EL2_SCTLR2EN

  // FEAT_TCR2
  set_hcrx_bits id_aa64mmfr3_el1, #ID_AA64MMFR3_EL1_TCRX, #HCRX_EL2_TCR2EN

  // FEAT_MOPS
  set_hcrx_bits id_aa64isar2_el1, #ID_AA64ISAR2_EL1_MOPS, #HCRX_EL2_MSCEN

  // FEAT_GCS: Guarded Control Stack.
  // HCRX_EL2.GCSEN (bit 22) has 0-traps polarity: when FEAT_GCS is implemented
  // and GCSEN is 0, EL1/EL0 accesses to GCS registers (GCSCR_EL1, GCSCRE0_EL1,
  // GCSPR_EL1, GCSPR_EL0) and GCS instructions trap to EL2 whenever
  // HCR_EL2.{E2H, TGE} != {1, 1} (both after dropping to EL1 on non-VHE
  // systems and when running an EL1&0 guest with TGE=0 under VHE).
  set_hcrx_bits id_aa64pfr1_el1, #ID_AA64PFR1_EL1_GCS, #HCRX_EL2_GCSEN

  // FEAT_LS64
  set_hcrx_bits id_aa64isar1_el1, #ID_AA64ISAR1_EL1_LS64, \
           #(HCRX_EL2_ENASR | HCRX_EL2_ENALS | HCRX_EL2_ENAS0)

  .arch armv8.7-a
  msr hcrx_el2, hcrx_bits
.Lno_hcx:

  // Sanitize EL2 registers to prevent traps to EL2.
  // When dropping to EL1 and disabling the EL2 monitor (setting vbar_el2 to 0),
  // any trap to EL2 will result in a crash. When staying in EL2 with VHE,
  // any leftover trap bits will cause unexpected traps when running EL1/EL0
  // guests with HCR_EL2.TGE = 0.

  // MDCR_EL2: Monitor Debug Configuration Register.
  // Clear all trap bits (like TDOSA which traps OSLAR_EL1) but preserve PMCR_EL0.N
  // in HPMN to allow EL1 access to all physical performance counters.
  mrs scratch, pmcr_el0
  ubfx scratch, scratch, #11, #5
  msr mdcr_el2, scratch

  // HSTR_EL2: Hypervisor System Trap Register.
  // Zeroing this disables trapping of CP15 system register accesses to EL2.
  msr hstr_el2, xzr

  // CNTHP_CTL_EL2: Counter-timer Hypervisor Physical Timer Control.
  // Zeroing this disables the EL2 physical timer and its interrupts.
  msr cnthp_ctl_el2, xzr

  // Check for FEAT_FGT (Fine-Grained Traps).
  // If supported, initialize the fine-grained trap registers to a defined
  // state that disables all traps and prevents info leaks.
  // Note: Fine-grained traps are globally enabled at EL3 by SCR_EL3.FGTEn
  // (there is no HCR_EL2 enable bit for FEAT_FGT) and apply whenever
  // HCR_EL2.{E2H, TGE} != {1, 1} (both after dropping to EL1 on non-VHE
  // systems and when running an EL1&0 guest with TGE=0 under VHE).
  mrs_tst id_aa64mmfr0_el1, #ID_AA64MMFR0_EL1_FGT
  beq .Lno_fgt

  .arch armv8.7-a
  // HFGRTR_EL2 / HFGWTR_EL2: Hypervisor Fine-Grained Read/Write Trap Registers.
  // Clear bits [49:0] (positive-polarity trap bits where 0 disables trapping
  // and 1 traps to EL2), and set the upper negative-polarity (n*) trap bits
  // (where 0 traps to EL2 and 1 disables trapping) to 1 (HFGRTR_EL2_NO_TRAPS /
  // HFGWTR_EL2_NO_TRAPS):
  //   - bit 50: nACCDATA_EL1 (FEAT_LS64_ACCDATA)
  //   - bit 51: RES0
  //   - bit 52: nSMPRI_EL1   (FEAT_SME)
  //   - bit 53: nTPIDR2_EL0  (FEAT_SME)
  //   - bit 54: nRCWMASK_EL1 (FEAT_THE)
  //   - bit 55: nPIRE0_EL1   (FEAT_S1PIE)
  //   - bit 56: nPIR_EL1     (FEAT_S1PIE)
  //   - bit 57: nPOR_EL0     (FEAT_S1POE)
  //   - bit 58: nPOR_EL1     (FEAT_S1POE)
  //   - bit 59: nS2POR_EL1   (FEAT_S2POE)
  //   - bit 60: nMAIR2_EL1   (FEAT_AIE)
  //   - bit 61: RES0
  //   - bit 62: nGCS_EL0     (FEAT_GCS)
  //   - bit 63: nGCS_EL1     (FEAT_GCS)
  msr_mov hfgrtr_el2, #HFGRTR_EL2_NO_TRAPS
  msr_mov hfgwtr_el2, #HFGWTR_EL2_NO_TRAPS

  // HFGITR_EL2: Hypervisor Fine-Grained Instruction Trap Register.
  // Clear positive-polarity instruction trap bits (where 0 disables trapping
  // and 1 traps to EL2, including bits [56:0] and bit 58 SVC_EL1), and set the
  // negative-polarity (n*) instruction trap bits (where 0 traps to EL2 and 1
  // disables trapping) to 1 (HFGITR_EL2_NO_TRAPS):
  //   - bit 57: nGCSEPP  (FEAT_GCS: GCSSS1 and GCSSS2 instructions)
  //   - bit 58: SVC_EL1  (positive-polarity: 0 disables trapping)
  //   - bit 59: nBRBIALL (FEAT_BRBE: BRB IALL instruction)
  //   - bit 60: nBRBINJ  (FEAT_BRBE: BRB INJ instruction)
  //   - bits [63:61]: RES0
  msr_mov hfgitr_el2, #HFGITR_EL2_NO_TRAPS

  // HDFGRTR_EL2 / HDFGWTR_EL2: Hypervisor Debug Fine-Grained Read/Write Trap
  // Registers. Zeroing these disables all debug fine-grained traps, as all
  // defined bits in HDFGRTR_EL2 and HDFGWTR_EL2 have positive polarity
  // (0 disables trapping, 1 traps to EL2).
  msr hdfgrtr_el2, xzr
  msr hdfgwtr_el2, xzr
.Lno_fgt:

  // Disable any virtual timer offset.
  msr cntvoff_el2, xzr

  // Clear stage 2 translation table base register.
  // Should already be disabled by writing 0 to HCR_EL2.VM.
  msr vttbr_el2, xzr

  // VMPIDR_EL2 and VPIDR_EL2 are what are returned from reads of MPIDR_EL1 and
  // MIDR_EL2 when in non-secure EL1, respectively. Reads of MPIDR_EL1 and
  // MIDR_EL1 when in EL2 return the real values, so we make sure now that
  // their VM*_EL2 counterparts reflect those values too.
  msr_mrs vmpidr_el2, mpidr_el1
  msr_mrs vpidr_el2, midr_el1

  // Check whether the GIC system registers are supported.
  mrs_tst id_aa64pfr0_el1, #ID_AA64PFR0_EL1_GIC
  beq .Lno_gic_sysregs

  // Enable the GIC system registers in EL2, and allow their use in EL1.
  mrs scratch, icc_sre_el2
  orr scratch, scratch, #ICC_SRE_EL2_ENABLE
  orr scratch, scratch, #ICC_SRE_EL2_SRE
  msr icc_sre_el2, scratch

  // Disable the GIC virtual CPU interface.
  msr ich_hcr_el2, xzr
.Lno_gic_sysregs:

  // If FEAT_VHE is supported, stay in EL2 with VHE host mode (E2H=1, TGE=1).
  mrs_tst id_aa64mmfr1_el1, #ID_AA64MMFR1_EL1_VH
  bne .Lvhe_stay_in_el2

  // Clear the vector table for EL2 before dropping to EL1.
  msr vbar_el2, xzr

  // Set EL1 to 64-bit, ensuring also that E2H is clear, as the assumed
  // layouts of CNTHCTL_EL2 and CPTR_EL2 depend on that. Also, ensure
  // that TSC is clear so that `smc` traps in EL3.
  msr_mov hcr_el2, #HCR_EL2_RW

  // Disable traps from EL1/EL0 regarding the physical counter.
  // Disable EL2 Event counter.
  // FEAT_ECV: Disable Enhanced Counter Virtualization feature and
  //   traps of virtual timer registers.
  msr_mov cnthctl_el2, #CNTHCTL_EL2_EL1PCEN | CNTHCTL_EL2_EL1PCTEN

  // Clear EL2 stage 1 translation control register.
  msr tcr_el2, xzr

  // Disable EL2 coprocessor traps.
  msr_mov cptr_el2, #CPTR_EL2_RES1

  // Set the return address and exception level.
  adr scratch, .Ltarget
  msr elr_el2, scratch
  msr_mov spsr_el2, #SPSR_EL2_DAIF | SPSR_EL2_M_EL1H

.Ldrop_to_el1:
  // Reuse the current stack pointer after the drop to EL1.
  msr_mov sp_el1, sp

  isb
  eret
  speculation_postfence

.Lvhe_stay_in_el2:
  // Enable VHE host mode (RW=1, E2H=1, TGE=1).
  movlit sysreg_scratch, (HCR_EL2_RW | HCR_EL2_E2H | HCR_EL2_TGE)
  msr hcr_el2, sysreg_scratch
  isb

  // With E2H=1, CNTHCTL_EL2 and CPTR_EL2 use the EL2 host layout.
  // Allow EL1 and EL0 access to physical and virtual counters/timers.
  msr_mov cnthctl_el2, #(CNTHCTL_EL2_E2H1_EL1PTEN | CNTHCTL_EL2_E2H1_EL1PCTEN | \
                         CNTHCTL_EL2_E2H1_EL0VCTEN | CNTHCTL_EL2_E2H1_EL0PCTEN)

  // Disable FP/SIMD trapping during early boot; the kernel proper will
  // configure CPACR_EL1 (redirected to CPTR_EL2) during arch init.
  msr_mov cptr_el2, #CPTR_EL2_E2H1_FPEN

  // Ensure we are using SP_EL2 (EL2h) with the current stack pointer and
  // all interrupts masked, matching the post-eret state of the EL1 path.
  mov sysreg_scratch, sp
  msr SPSel, #1
  mov sp, sysreg_scratch
  msr sp_el0, xzr
  msr DAIFSet, #DAIFSETCLR_MASK
  isb

.Ltarget:
  ret
  speculation_postfence
.end_function
