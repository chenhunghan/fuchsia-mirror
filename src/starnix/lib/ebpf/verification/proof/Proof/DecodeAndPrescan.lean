-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.LoopLemmas
import Proof.ApiConverterLinkerExec
import Proof.ScalarProofs

set_option linter.unusedTactic false
set_option maxHeartbeats 4000000

namespace StarnixEbpf.Proof

open Aeneas Aeneas.Std Result
open StarnixEbpf.Assumptions
open StarnixEbpf.Spec
open StarnixEbpf.LoopLemmas
open StarnixEbpf.ApiConverterLinkerExec

/-!
# Bytecode Decoding, Prescan, Visitor Dispatch & Dead-Code Elimination Proofs

Proves the bit-level specifications of:
1. `ebpf_core.api.EbpfInstruction` accessors and `dead_insn` construction,
2. `ebpf_core.visitor` immediate validators and `BpfVisitor.visit.default`,
3. `ebpf_core.verifier.prescan_step` and `verify_program_core_loop0..loop6`,
establishing that every program accepted by `verify_program_core` decodes via
`Spec.decodeVerifiedCode` to an equal-length `Spec.Program` whose reachable
instructions match `BpfVisitor.visit.default` and never land on `Insn.lddwCont`.
-/

-- ============================================================================
-- Section 1: Platform `Usize` Bounds & `EbpfInstruction` Bit-Level Specs
-- ============================================================================

theorem usize_65536_le_max : 65536 ≤ Usize.max := by
  rcases Usize.bounds_eq with h | h <;> rw [h] <;> simp [U32.max_eq, U64.max_eq]

theorem usize_655361_le_max : 655361 ≤ Usize.max := by
  rcases Usize.bounds_eq with h | h <;> rw [h] <;> simp [U32.max_eq, U64.max_eq]

theorem usize_one_val : (1#usize).val = 1 := by
  cases System.Platform.numBits_eq <;> simp_all [UScalar.val]

theorem usize_two_val : (2#usize).val = 2 := by
  cases System.Platform.numBits_eq <;> simp_all [UScalar.val]

theorem uscalar_add_ok_val {ty : UScalarTy} {x y z : UScalar ty}
    (h : (x + y : Result (UScalar ty)) = .ok z) :
    z.val = x.val + y.val ∧ z.bv = x.bv + y.bv := by
  have heq := UScalar.add_equiv x y
  rw [h] at heq
  simpa using heq.2

theorem bpf_max_insts_val : ebpf_core.api.BPF_MAX_INSTS.val = 65536 := by
  simp only [global_simps]
  cases System.Platform.numBits_eq <;> simp_all [UScalar.val]

theorem usize_655360_val : (655360#usize).val = 655360 := by
  cases System.Platform.numBits_eq <;> simp_all [UScalar.val]

theorem bpf_max_steps_mul_eq_ok :
    (10#usize * ebpf_core.api.BPF_MAX_INSTS : Result Usize) = .ok 655360#usize := by
  have hval10 : (10#usize).val = 10 := by
    cases System.Platform.numBits_eq <;> simp_all [UScalar.val]
  have hmax : (10#usize).val * ebpf_core.api.BPF_MAX_INSTS.val ≤ UScalar.max .Usize := by
    rw [hval10, bpf_max_insts_val, UScalar.max_USize_eq]
    have := usize_655361_le_max
    omega
  have hs := UScalar.mul_bv_spec hmax
  cases hr : (10#usize * ebpf_core.api.BPF_MAX_INSTS : Result Usize) with
  | ret z =>
    rw [hr] at hs; simp at hs
    apply congrArg ok
    apply Usize.bv_eq_imp_eq
    apply BitVec.eq_of_toNat_eq
    change z.val = (655360#usize).val
    rw [hs.1, bpf_max_insts_val, usize_655360_val]
  | vis _ _ => rw [hr] at hs; simp at hs
  | div => rw [hr] at hs; simp at hs

theorem bpf_lddw_eq : ebpf_core.api.BPF_LDDW = 24#u8 := by
  simp only [global_simps]
  decide

theorem bpf_map_type_array_eq : ebpf_core.api.BPF_MAP_TYPE_ARRAY = 2#u32 := by
  simp only [global_simps]

theorem bpf_argument_count_val :
    (UScalar.cast .Usize ebpf_core.api.ARGUMENT_COUNT).val = 5 := by
  simp only [global_simps]
  cases System.Platform.numBits_eq with
  | inl h =>
    dsimp only [UScalar.cast, UScalar.val, UScalarTy.Usize_numBits_eq]
    rw [h]
    decide
  | inr h =>
    dsimp only [UScalar.cast, UScalar.val, UScalarTy.Usize_numBits_eq]
    rw [h]
    decide

private theorem u64_shr_i32_const (x : U64) (s : I32)
    (hge : s.val ≥ 0) (hlt : s.toNat < 64) :
    (x >>> s : Result U64) = .ok ⟨x.bv >>> s.toNat⟩ := by
  change UScalar.shiftRight_IScalar x s = _
  simp only [UScalar.shiftRight_IScalar, hge, ↓reduceIte, UScalar.shiftRight,
    UScalarTy.numBits, hlt]
  rfl

private theorem u64_shl_i32_const (x : U64) (s : I32)
    (hge : s.val ≥ 0) (hlt : s.toNat < 64) :
    (x <<< s : Result U64) = .ok ⟨x.bv <<< s.toNat⟩ := by
  change UScalar.shiftLeft_IScalar x s = _
  simp only [UScalar.shiftLeft_IScalar, hge, ↓reduceIte, UScalar.shiftLeft,
    UScalarTy.numBits, hlt]
  rfl

theorem ebpf_code_eq_ok (inst : ebpf_core.api.EbpfInstruction) :
    ebpf_core.api.EbpfInstruction.code inst = .ok ⟨inst.bv.setWidth 8⟩ := by
  unfold ebpf_core.api.EbpfInstruction.code
  rfl

theorem ebpf_dst_reg_eq_ok (inst : ebpf_core.api.EbpfInstruction) :
    ebpf_core.api.EbpfInstruction.dst_reg inst =
      .ok ⟨((inst.bv >>> 8) &&& 0xF#64).setWidth 8⟩ := by
  unfold ebpf_core.api.EbpfInstruction.dst_reg
  have hsh := u64_shr_i32_const (inst &&& 3840#u64) 8#i32 (by decide) (by decide)
  simp only [lift, bind_ok, hsh, Result.ok.injEq]
  apply U8.bv_eq_imp_eq
  change BitVec.setWidth 8 ((inst.bv &&& 3840#64) >>> 8) =
    BitVec.setWidth 8 ((inst.bv >>> 8) &&& 0xF#64)
  bv_decide

theorem ebpf_src_reg_eq_ok (inst : ebpf_core.api.EbpfInstruction) :
    ebpf_core.api.EbpfInstruction.src_reg inst =
      .ok ⟨((inst.bv >>> 12) &&& 0xF#64).setWidth 8⟩ := by
  unfold ebpf_core.api.EbpfInstruction.src_reg
  have hsh := u64_shr_i32_const (inst &&& 61440#u64) 12#i32 (by decide) (by decide)
  simp only [lift, bind_ok, hsh, Result.ok.injEq]
  apply U8.bv_eq_imp_eq
  change BitVec.setWidth 8 ((inst.bv &&& 61440#64) >>> 12) =
    BitVec.setWidth 8 ((inst.bv >>> 12) &&& 0xF#64)
  bv_decide

theorem ebpf_offset_eq_ok (inst : ebpf_core.api.EbpfInstruction) :
    ebpf_core.api.EbpfInstruction.offset inst =
      .ok (UScalar.hcast .I16 (⟨(inst.bv >>> 16).setWidth 16⟩ : U16)) := by
  unfold ebpf_core.api.EbpfInstruction.offset
  have hsh := u64_shr_i32_const inst 16#i32 (by decide) (by decide)
  simp only [lift, bind_ok, hsh]
  rfl

theorem ebpf_imm_eq_ok (inst : ebpf_core.api.EbpfInstruction) :
    ebpf_core.api.EbpfInstruction.imm inst =
      .ok (UScalar.hcast .I32 (⟨(inst.bv >>> 32).setWidth 32⟩ : U32)) := by
  unfold ebpf_core.api.EbpfInstruction.imm
  have hsh := u64_shr_i32_const inst 32#i32 (by decide) (by decide)
  simp only [lift, bind_ok, hsh]
  rfl

theorem nibble_setWidth8_val (w : BitVec 64) (s : Nat) :
    (⟨((w >>> s) &&& 0xF#64).setWidth 8⟩ : U8).val = ((w >>> s) &&& 0xF#64).toNat := by
  change BitVec.toNat (BitVec.setWidth 8 ((w >>> s) &&& 0xF#64)) =
    BitVec.toNat ((w >>> s) &&& 0xF#64)
  generalize (w >>> s) = u
  rw [BitVec.toNat_setWidth]
  have hlt : (u &&& 0xF#64).toNat < 256 := by
    have hle : (u &&& 0xF#64) ≤ 0xF#64 := by bv_decide
    exact Nat.lt_of_le_of_lt (BitVec.le_def.mp hle) (by decide)
  exact Nat.mod_eq_of_lt hlt

theorem hcast_i16_val (b16 : BitVec 16) :
    (UScalar.hcast .I16 (⟨b16⟩ : U16)).val = b16.toInt := rfl

theorem hcast_i16_bv (b16 : BitVec 16) :
    (UScalar.hcast .I16 (⟨b16⟩ : U16)).bv = b16 := rfl

theorem hcast_i32_bv (b32 : BitVec 32) :
    (UScalar.hcast .I32 (⟨b32⟩ : U32)).bv = b32 := rfl

theorem hcast_i32_to_u64 (b32 : BitVec 32) :
    IScalar.hcast .U64 (UScalar.hcast .I32 (⟨b32⟩ : U32)) = ⟨b32.signExtend 64⟩ := rfl

theorem hcast_i32_to_u32 (b32 : BitVec 32) :
    IScalar.hcast .U32 (UScalar.hcast .I32 (⟨b32⟩ : U32)) = ⟨b32⟩ := by
  apply U32.bv_eq_imp_eq
  change BitVec.signExtend 32 b32 = b32
  simp

theorem hcast_u32_i32_roundtrip (x : I32) :
    UScalar.hcast .I32 (IScalar.hcast .U32 x) = x := by
  apply I32.bv_eq_imp_eq
  change BitVec.zeroExtend 32 (BitVec.signExtend 32 x.bv) = x.bv
  simp

theorem hcast_i32_to_u8 (b32 : BitVec 32) :
    IScalar.hcast .U8 (UScalar.hcast .I32 (⟨b32⟩ : U32)) = ⟨b32.setWidth 8⟩ := by
  apply U8.bv_eq_imp_eq
  change BitVec.signExtend 8 b32 = b32.setWidth 8
  bv_decide

theorem lddw_cont_zero_low32 (inst : ebpf_core.api.EbpfInstruction)
    (hCode : (⟨inst.bv.setWidth 8⟩ : U8) = 0#u8)
    (hOff : UScalar.hcast .I16 (⟨(inst.bv >>> 16).setWidth 16⟩ : U16) = 0#i16)
    (hSrc : (⟨((inst.bv >>> 12) &&& 0xF#64).setWidth 8⟩ : U8) = 0#u8)
    (hDst : (⟨((inst.bv >>> 8) &&& 0xF#64).setWidth 8⟩ : U8) = 0#u8) :
    inst.bv.setWidth 32 = 0#32 := by
  have hc : inst.bv.setWidth 8 = 0#8 := congrArg UScalar.bv hCode
  have ho : (inst.bv >>> 16).setWidth 16 = 0#16 := congrArg IScalar.bv hOff
  have hs : ((inst.bv >>> 12) &&& 0xF#64).setWidth 8 = 0#8 := congrArg UScalar.bv hSrc
  have hd : ((inst.bv >>> 8) &&& 0xF#64).setWidth 8 = 0#8 := congrArg UScalar.bv hDst
  generalize inst.bv = w at hc ho hs hd ⊢
  bv_decide

def deadEbpfInsn : ebpf_core.api.EbpfInstruction := ⟨0x0000_0000_FFFF_0005#64⟩

theorem dead_insn_new_eq_ok :
    ebpf_core.api.EbpfInstruction.new
      (ebpf_core.api.BPF_JMP ||| ebpf_core.api.BPF_JA)
      0#u8 0#u8 (-1)#i16 0#i32 = .ok deadEbpfInsn := by
  unfold ebpf_core.api.EbpfInstruction.new deadEbpfInsn
  simp only [global_simps]
  have hm0 : (0#u8 < 16#u8) := by decide
  simp only [massert, hm0, ↓reduceIte, lift, bind_ok,
    u64_shl_i32_const _ 8#i32 (by decide) (by decide),
    u64_shl_i32_const _ 12#i32 (by decide) (by decide),
    u64_shl_i32_const _ 16#i32 (by decide) (by decide),
    u64_shl_i32_const _ 32#i32 (by decide) (by decide),
    Result.ok.injEq]
  apply U64.bv_eq_imp_eq
  decide

theorem dead_insn_code_eq : deadEbpfInsn.bv.setWidth 8 = 0x05#8 := by decide

theorem dead_insn_not_lddw : deadEbpfInsn.bv.setWidth 8 ≠ 0x18#8 := by decide

theorem dead_insn_decode : decodeSingleInsn deadEbpfInsn = some (.jump (-1)) := by decide

-- ============================================================================
-- Section 2: `visitor` Validator Specs (`div_imm`, `shift_imm`, `width_from_imm`)
-- ============================================================================

theorem div_imm_spec (b32 : BitVec 32) :
    ebpf_core.visitor.div_imm (UScalar.hcast .I32 (⟨b32⟩ : U32)) =
      .ok (if b32 == 0#32 then .Err .DivByZero else .Ok (.Value ⟨b32.signExtend 64⟩)) := by
  unfold ebpf_core.visitor.div_imm
  have heq : (UScalar.hcast .I32 (⟨b32⟩ : U32) = 0#i32) ↔ (b32 == 0#32) = true := by
    constructor
    · intro h
      have hb : b32 = 0#32 := congrArg IScalar.bv h
      subst hb; rfl
    · intro h
      have hb : b32 = 0#32 := eq_of_beq h
      subst hb; rfl
  by_cases h0 : (b32 == 0#32) = true
  · have hi : UScalar.hcast .I32 (⟨b32⟩ : U32) = 0#i32 := heq.mpr h0
    simp [hi, h0]
  · have hi : UScalar.hcast .I32 (⟨b32⟩ : U32) ≠ 0#i32 := fun hc => h0 (heq.mp hc)
    have hbf : (b32 == 0#32) = false := Bool.eq_false_iff.mpr h0
    simp [hi, hbf, lift, hcast_i32_to_u64]

private theorem i32_lt_zero_iff_msb (b32 : BitVec 32) :
    (UScalar.hcast .I32 (⟨b32⟩ : U32) < 0#i32) ↔ b32.msb = true := by
  rw [IScalar.lt_equiv]
  have hval : (UScalar.hcast .I32 (⟨b32⟩ : U32)).val = b32.toInt := rfl
  have h0 : (0#i32).val = (0#32).toInt := rfl
  rw [hval, h0]
  have hslt : (BitVec.slt b32 0#32 = true) ↔ b32.toInt < (0#32).toInt := by
    unfold BitVec.slt; exact decide_eq_true_iff
  rw [← hslt]
  bv_decide

private theorem i32_nonneg_ge_32_iff (b32 : BitVec 32) (_hmsb : b32.msb = false) :
    (UScalar.hcast .I32 (⟨b32⟩ : U32) ≥ 32#i32) ↔ ¬ (b32 < 32#32) := by
  rw [ge_iff_le, IScalar.le_equiv]
  have hval : (UScalar.hcast .I32 (⟨b32⟩ : U32)).val = b32.toInt := rfl
  have h32 : (32#i32).val = (32#32).toInt := rfl
  rw [hval, h32]
  have hsle : (BitVec.sle 32#32 b32 = true) ↔ (32#32).toInt ≤ b32.toInt := by
    unfold BitVec.sle; exact decide_eq_true_iff
  rw [← hsle]
  bv_decide

private theorem i32_nonneg_ge_64_iff (b32 : BitVec 32) (_hmsb : b32.msb = false) :
    (UScalar.hcast .I32 (⟨b32⟩ : U32) ≥ 64#i32) ↔ ¬ (b32 < 64#32) := by
  rw [ge_iff_le, IScalar.le_equiv]
  have hval : (UScalar.hcast .I32 (⟨b32⟩ : U32)).val = b32.toInt := rfl
  have h64 : (64#i32).val = (64#32).toInt := rfl
  rw [hval, h64]
  have hsle : (BitVec.sle 64#32 b32 = true) ↔ (64#32).toInt ≤ b32.toInt := by
    unfold BitVec.sle; exact decide_eq_true_iff
  rw [← hsle]
  bv_decide

theorem shift_imm_32_spec (b32 : BitVec 32) :
    ebpf_core.visitor.shift_imm (UScalar.hcast .I32 (⟨b32⟩ : U32)) 32#i32 =
      .ok (if (!b32.msb && decide (b32 < 32#32)) then .Ok (.Value ⟨b32.signExtend 64⟩)
           else .Err (.InvalidShift (UScalar.hcast .I32 (⟨b32⟩ : U32)))) := by
  unfold ebpf_core.visitor.shift_imm
  by_cases hm : b32.msb = true
  · have hlt : UScalar.hcast .I32 (⟨b32⟩ : U32) < 0#i32 := (i32_lt_zero_iff_msb b32).mpr hm
    simp [hlt, hm]
  · have hmf : b32.msb = false := Bool.eq_false_iff.mpr hm
    have hnlt : ¬ (UScalar.hcast .I32 (⟨b32⟩ : U32) < 0#i32) :=
      fun hc => hm ((i32_lt_zero_iff_msb b32).mp hc)
    by_cases hb : b32 < 32#32
    · have hnge : ¬ (UScalar.hcast .I32 (⟨b32⟩ : U32) ≥ 32#i32) :=
        fun hc => (i32_nonneg_ge_32_iff b32 hmf).mp hc hb
      simp [hnlt, hnge, hmf, hb, lift, hcast_i32_to_u64]
    · have hge : UScalar.hcast .I32 (⟨b32⟩ : U32) ≥ 32#i32 :=
        (i32_nonneg_ge_32_iff b32 hmf).mpr hb
      simp [hnlt, hge, hmf, hb]

theorem shift_imm_64_spec (b32 : BitVec 32) :
    ebpf_core.visitor.shift_imm (UScalar.hcast .I32 (⟨b32⟩ : U32)) 64#i32 =
      .ok (if (!b32.msb && decide (b32 < 64#32)) then .Ok (.Value ⟨b32.signExtend 64⟩)
           else .Err (.InvalidShift (UScalar.hcast .I32 (⟨b32⟩ : U32)))) := by
  unfold ebpf_core.visitor.shift_imm
  by_cases hm : b32.msb = true
  · have hlt : UScalar.hcast .I32 (⟨b32⟩ : U32) < 0#i32 := (i32_lt_zero_iff_msb b32).mpr hm
    simp [hlt, hm]
  · have hmf : b32.msb = false := Bool.eq_false_iff.mpr hm
    have hnlt : ¬ (UScalar.hcast .I32 (⟨b32⟩ : U32) < 0#i32) :=
      fun hc => hm ((i32_lt_zero_iff_msb b32).mp hc)
    by_cases hb : b32 < 64#32
    · have hnge : ¬ (UScalar.hcast .I32 (⟨b32⟩ : U32) ≥ 64#i32) :=
        fun hc => (i32_nonneg_ge_64_iff b32 hmf).mp hc hb
      simp [hnlt, hnge, hmf, hb, lift, hcast_i32_to_u64]
    · have hge : UScalar.hcast .I32 (⟨b32⟩ : U32) ≥ 64#i32 :=
        (i32_nonneg_ge_64_iff b32 hmf).mpr hb
      simp [hnlt, hge, hmf, hb]

def coreWidthOfEndian : EndianWidth → ebpf_core.api.DataWidth
  | .w16 => .U16
  | .w32 => .U32
  | .w64 => .U64

def coreWidthOfSpec : DataWidth → ebpf_core.api.DataWidth
  | .u8  => .U8
  | .u16 => .U16
  | .u32 => .U32
  | .u64 => .U64

@[simp] theorem dataWidth_ofCore_coreWidthOfSpec (w : DataWidth) :
    DataWidth.ofCore (coreWidthOfSpec w) = w := by
  cases w <;> rfl

theorem width_from_imm_spec (b32 : BitVec 32) :
    ebpf_core.visitor.width_from_imm (UScalar.hcast .I32 (⟨b32⟩ : U32)) =
      .ok (match decodeEndianWidth b32 with
           | some ew => .Ok (coreWidthOfEndian ew)
           | none => .Err (.InvalidByteSwapWidth (UScalar.hcast .I32 (⟨b32⟩ : U32)))) := by
  unfold ebpf_core.visitor.width_from_imm decodeEndianWidth
  change (match (⟨b32⟩ : I32) with
    | 16#iscalar => ok (core.result.Result.Ok ebpf_core.api.DataWidth.U16)
    | 32#iscalar => ok (core.result.Result.Ok ebpf_core.api.DataWidth.U32)
    | 64#iscalar => ok (core.result.Result.Ok ebpf_core.api.DataWidth.U64)
    | _ => ok (core.result.Result.Err (ebpf_core.error.VerifierErrorKind.InvalidByteSwapWidth ⟨b32⟩))) = _
  by_cases h16 : b32 = 16#32
  · subst h16; rfl
  · by_cases h32 : b32 = 32#32
    · subst h32; rfl
    · by_cases h64 : b32 = 64#32
      · subst h64; rfl
      · have b16f : (b32 == 16#32) = false := beq_eq_false_iff_ne.mpr h16
        have b32f : (b32 == 32#32) = false := beq_eq_false_iff_ne.mpr h32
        have b64f : (b32 == 64#32) = false := beq_eq_false_iff_ne.mpr h64
        rw [b16f, b32f, b64f]
        split
        · rename_i heq; exact False.elim (h16 (congrArg IScalar.bv heq))
        · rename_i heq; exact False.elim (h32 (congrArg IScalar.bv heq))
        · rename_i heq; exact False.elim (h64 (congrArg IScalar.bv heq))
        · rfl

-- ============================================================================
-- Section 3: Grouped Visitor Dispatch for `BpfVisitor.visit.default`
-- ============================================================================

def operandOfSource : ebpf_core.visitor.Source → Operand
  | .Reg r => .reg r.val
  | .Value v => .imm v.bv

def alu64Method {Self C : Type} (V : ebpf_core.visitor.BpfVisitor Self C) :
    AluOp → Self → C → U8 → ebpf_core.visitor.Source →
    Result ((core.result.Result Unit ebpf_core.error.VerifierErrorKind) × Self × C)
  | .add  => V.add64
  | .sub  => V.sub64
  | .mul  => V.mul64
  | .div  => V.div64
  | .rem  => V.mod64
  | .and  => V.and64
  | .or   => V.or64
  | .xor  => V.xor64
  | .lsh  => V.lsh64
  | .rsh  => V.rsh64
  | .arsh => V.arsh64
  | .mov  => V.mov64

def alu32Method {Self C : Type} (V : ebpf_core.visitor.BpfVisitor Self C) :
    AluOp → Self → C → U8 → ebpf_core.visitor.Source →
    Result ((core.result.Result Unit ebpf_core.error.VerifierErrorKind) × Self × C)
  | .add  => V.add
  | .sub  => V.sub
  | .mul  => V.mul
  | .div  => V.div
  | .rem  => V.mod
  | .and  => V.and
  | .or   => V.or
  | .xor  => V.xor
  | .lsh  => V.lsh
  | .rsh  => V.rsh
  | .arsh => V.arsh
  | .mov  => V.mov

def condJumpMethod {Self C : Type} (V : ebpf_core.visitor.BpfVisitor Self C)
    (is64 : Bool) : JumpOp → Self → C → U8 → ebpf_core.visitor.Source → I16 →
    Result ((core.result.Result Unit ebpf_core.error.VerifierErrorKind) × Self × C) :=
  if is64 then
    fun op => match op with
      | .eq  => V.jeq64  | .ne  => V.jne64
      | .gt  => V.jgt64  | .ge  => V.jge64
      | .lt  => V.jlt64  | .le  => V.jle64
      | .sgt => V.jsgt64 | .sge => V.jsge64
      | .slt => V.jslt64 | .sle => V.jsle64
      | .set => V.jset64
  else
    fun op => match op with
      | .eq  => V.jeq  | .ne  => V.jne
      | .gt  => V.jgt  | .ge  => V.jge
      | .lt  => V.jlt  | .le  => V.jle
      | .sgt => V.jsgt | .sge => V.jsge
      | .slt => V.jslt | .sle => V.jsle
      | .set => V.jset

def atomicMethod {Self C : Type} (V : ebpf_core.visitor.BpfVisitor Self C)
    (is64 : Bool) : AtomicOpKind → Self → C → U8 → I16 → U8 →
    Result ((core.result.Result Unit ebpf_core.error.VerifierErrorKind) × Self × C) :=
  if is64 then
    fun op self ctx dst off src => match op with
      | .add f   => V.atomic_add64 self ctx f dst off src
      | .or  f   => V.atomic_or64  self ctx f dst off src
      | .and f   => V.atomic_and64 self ctx f dst off src
      | .xor f   => V.atomic_xor64 self ctx f dst off src
      | .xchg    => V.atomic_xchg64 self ctx true dst off src
      | .cmpxchg => V.atomic_cmpxchg64 self ctx dst off src
  else
    fun op self ctx dst off src => match op with
      | .add f   => V.atomic_add self ctx f dst off src
      | .or  f   => V.atomic_or  self ctx f dst off src
      | .and f   => V.atomic_and self ctx f dst off src
      | .xor f   => V.atomic_xor self ctx f dst off src
      | .xchg    => V.atomic_xchg self ctx true dst off src
      | .cmpxchg => V.atomic_cmpxchg self ctx dst off src

/-- Exact correspondence between a non-`LDDW` decoded `Spec.Insn` and the
    `BpfVisitor` method call executed by `BpfVisitor.visit.default`. -/
def VisitorDispatchNonLddw {Self C : Type} (V : ebpf_core.visitor.BpfVisitor Self C)
    (self : Self) (ctx : C)
    (out : Result ((core.result.Result Unit ebpf_core.error.VerifierErrorKind) × Self × C)) :
    Insn → Prop
  | .alu64 op dst src =>
      ∃ (dst8 : U8) (vsrc : ebpf_core.visitor.Source),
        dst = dst8.val ∧ src = operandOfSource vsrc ∧
        ((op = .div ∨ op = .rem) → ∀ v, vsrc = .Value v → v.bv ≠ 0#64) ∧
        out = alu64Method V op self ctx dst8 vsrc
  | .alu32 op dst src =>
      ∃ (dst8 : U8) (vsrc : ebpf_core.visitor.Source),
        dst = dst8.val ∧ src = operandOfSource vsrc ∧
        ((op = .div ∨ op = .rem) → ∀ v, vsrc = .Value v → v.bv.setWidth 32 ≠ 0#32) ∧
        out = alu32Method V op self ctx dst8 vsrc
  | .neg is64 dst =>
      ∃ (dst8 : U8),
        dst = dst8.val ∧
        out = (if is64 then V.neg64 self ctx dst8 else V.neg self ctx dst8)
  | .endian doSwap ew dst =>
      ∃ (dst8 : U8),
        dst = dst8.val ∧
        out = (if doSwap then V.be self ctx dst8 (coreWidthOfEndian ew)
               else V.le self ctx dst8 (coreWidthOfEndian ew))
  | .jump offInt =>
      ∃ (off : I16), offInt = off.val ∧ out = V.jump self ctx off
  | .condJump is64 op dst src offInt =>
      ∃ (dst8 : U8) (vsrc : ebpf_core.visitor.Source) (off : I16),
        dst = dst8.val ∧ src = operandOfSource vsrc ∧ offInt = off.val ∧
        out = condJumpMethod V is64 op self ctx dst8 vsrc off
  | .load w dst base off16 =>
      ∃ (dst8 src8 : U8) (off : I16),
        dst = dst8.val ∧ base = src8.val ∧ off16 = off.bv ∧
        out = V.load self ctx dst8 off src8 (coreWidthOfSpec w)
  | .store w base off16 src =>
      ∃ (dst8 : U8) (off : I16) (vsrc : ebpf_core.visitor.Source),
        base = dst8.val ∧ off16 = off.bv ∧ src = operandOfSource vsrc ∧
        out = V.store self ctx dst8 off vsrc (coreWidthOfSpec w)
  | .atomic is64 aop base off16 src =>
      ∃ (dst8 src8 : U8) (off : I16),
        base = dst8.val ∧ off16 = off.bv ∧ src = src8.val ∧
        out = atomicMethod V is64 aop self ctx dst8 off src8
  | .loadPacket w imm32 indReg? =>
      ∃ (imm : I32),
        imm32 = imm.bv ∧
        ((indReg? = none ∧
            out = V.load_from_packet self ctx 0#u8 6#u8 imm none (coreWidthOfSpec w)) ∨
         (∃ (src8 : U8),
            indReg? = some src8.val ∧
            out = V.load_from_packet self ctx 0#u8 6#u8 imm (some src8) (coreWidthOfSpec w)))
  | .callExternal helperIdx =>
      ∃ (idx32 : U32), helperIdx = idx32.val ∧ out = V.call_external self ctx idx32
  | .exit =>
      out = V.exit self ctx
  | .lddwImm _ _ | .lddwMap _ _ | .lddwMapValue _ _ _ | .lddwCont =>
      False

theorem sext32_ne_zero_of_ne_zero {b32 : BitVec 32} (h : b32 ≠ 0#32) :
    b32.signExtend 64 ≠ 0#64 := by
  intro hc
  apply h
  bv_decide

theorem sext32_setWidth32_eq (b32 : BitVec 32) :
    (b32.signExtend 64).setWidth 32 = b32 := by
  bv_decide

theorem sext32_setWidth32_ne_zero_of_ne_zero {b32 : BitVec 32} (h : b32 ≠ 0#32) :
    (b32.signExtend 64).setWidth 32 ≠ 0#32 := by
  rw [sext32_setWidth32_eq]
  exact h

theorem VisitorDispatchNonLddw.not_lddwCont {Self C : Type}
    {V : ebpf_core.visitor.BpfVisitor Self C} {self : Self} {ctx : C}
    {m : Result ((core.result.Result Unit ebpf_core.error.VerifierErrorKind) × Self × C)}
    {insn : Insn}
    (h : VisitorDispatchNonLddw V self ctx m insn) : insn ≠ .lddwCont := by
  rintro rfl
  exact h

private theorem visit_nondiv_vacuous64 {op : AluOp} {vsrc : ebpf_core.visitor.Source}
    (h : op ≠ .div ∧ op ≠ .rem) :
    (op = .div ∨ op = .rem) → ∀ v : U64, vsrc = .Value v → v.bv ≠ 0#64 := by
  intro hor
  rcases hor with rfl | rfl
  · exact False.elim (h.1 rfl)
  · exact False.elim (h.2 rfl)

private theorem visit_nondiv_vacuous32 {op : AluOp} {vsrc : ebpf_core.visitor.Source}
    (h : op ≠ .div ∧ op ≠ .rem) :
    (op = .div ∨ op = .rem) → ∀ v : U64, vsrc = .Value v → v.bv.setWidth 32 ≠ 0#32 := by
  intro hor
  rcases hor with rfl | rfl
  · exact False.elim (h.1 rfl)
  · exact False.elim (h.2 rfl)

private theorem visit_reg_vacuous64 {P : Prop} (r : U8) :
    P → ∀ (v : U64), (ebpf_core.visitor.Source.Reg r = .Value v) → v.bv ≠ 0#64 := by
  intro _ _ hc; cases hc

private theorem visit_reg_vacuous32 {P : Prop} (r : U8) :
    P → ∀ (v : U64), (ebpf_core.visitor.Source.Reg r = .Value v) → v.bv.setWidth 32 ≠ 0#32 := by
  intro _ _ hc; cases hc

section VisitDefaultSpec
set_option maxHeartbeats 4000000
set_option linter.auxLemma false
set_option linter.unusedSimpArgs false

private def decodeSingleInsnAt (opcode : BitVec 8) (raw : ebpf_core.api.EbpfInstruction) : Option Insn :=
  let w : BitVec 64 := raw.bv
  let dst : Nat := ((w >>> 8) &&& 0xF#64).toNat
  let srcReg : Nat := ((w >>> 12) &&& 0xF#64).toNat
  let off16 : BitVec 16 := (w >>> 16).setWidth 16
  let offInt : Int := off16.toInt
  let imm32 : BitVec 32 := (w >>> 32).setWidth 32
  let immSext64 : BitVec 64 := imm32.signExtend 64
  let srcImm : Operand := .imm immSext64
  let srcX : Operand := .reg srcReg
  -- 32-bit ALU
  if opcode == 0x04#8 then some (.alu32 .add dst srcImm)
  else if opcode == 0x0c#8 then some (.alu32 .add dst srcX)
  else if opcode == 0x14#8 then some (.alu32 .sub dst srcImm)
  else if opcode == 0x1c#8 then some (.alu32 .sub dst srcX)
  else if opcode == 0x24#8 then some (.alu32 .mul dst srcImm)
  else if opcode == 0x2c#8 then some (.alu32 .mul dst srcX)
  else if opcode == 0x34#8 then
    if imm32 == 0#32 then none else some (.alu32 .div dst srcImm)
  else if opcode == 0x3c#8 then some (.alu32 .div dst srcX)
  else if opcode == 0x44#8 then some (.alu32 .or dst srcImm)
  else if opcode == 0x4c#8 then some (.alu32 .or dst srcX)
  else if opcode == 0x54#8 then some (.alu32 .and dst srcImm)
  else if opcode == 0x5c#8 then some (.alu32 .and dst srcX)
  else if opcode == 0x64#8 then
    if !imm32.msb && imm32 < 32#32 then some (.alu32 .lsh dst srcImm) else none
  else if opcode == 0x6c#8 then some (.alu32 .lsh dst srcX)
  else if opcode == 0x74#8 then
    if !imm32.msb && imm32 < 32#32 then some (.alu32 .rsh dst srcImm) else none
  else if opcode == 0x7c#8 then some (.alu32 .rsh dst srcX)
  else if opcode == 0x84#8 then some (.neg false dst)
  else if opcode == 0x94#8 then
    if imm32 == 0#32 then none else some (.alu32 .rem dst srcImm)
  else if opcode == 0x9c#8 then some (.alu32 .rem dst srcX)
  else if opcode == 0xa4#8 then some (.alu32 .xor dst srcImm)
  else if opcode == 0xac#8 then some (.alu32 .xor dst srcX)
  else if opcode == 0xb4#8 then some (.alu32 .mov dst srcImm)
  else if opcode == 0xbc#8 then some (.alu32 .mov dst srcX)
  else if opcode == 0xc4#8 then
    if !imm32.msb && imm32 < 32#32 then some (.alu32 .arsh dst srcImm) else none
  else if opcode == 0xcc#8 then some (.alu32 .arsh dst srcX)
  -- Endianness
  else if opcode == 0xd4#8 then
    (decodeEndianWidth imm32).map (fun ew => .endian false ew dst)
  else if opcode == 0xdc#8 then
    (decodeEndianWidth imm32).map (fun ew => .endian true ew dst)
  -- 64-bit ALU
  else if opcode == 0x07#8 then some (.alu64 .add dst srcImm)
  else if opcode == 0x0f#8 then some (.alu64 .add dst srcX)
  else if opcode == 0x17#8 then some (.alu64 .sub dst srcImm)
  else if opcode == 0x1f#8 then some (.alu64 .sub dst srcX)
  else if opcode == 0x27#8 then some (.alu64 .mul dst srcImm)
  else if opcode == 0x2f#8 then some (.alu64 .mul dst srcX)
  else if opcode == 0x37#8 then
    if imm32 == 0#32 then none else some (.alu64 .div dst srcImm)
  else if opcode == 0x3f#8 then some (.alu64 .div dst srcX)
  else if opcode == 0x47#8 then some (.alu64 .or dst srcImm)
  else if opcode == 0x4f#8 then some (.alu64 .or dst srcX)
  else if opcode == 0x57#8 then some (.alu64 .and dst srcImm)
  else if opcode == 0x5f#8 then some (.alu64 .and dst srcX)
  else if opcode == 0x67#8 then
    if !imm32.msb && imm32 < 64#32 then some (.alu64 .lsh dst srcImm) else none
  else if opcode == 0x6f#8 then some (.alu64 .lsh dst srcX)
  else if opcode == 0x77#8 then
    if !imm32.msb && imm32 < 64#32 then some (.alu64 .rsh dst srcImm) else none
  else if opcode == 0x7f#8 then some (.alu64 .rsh dst srcX)
  else if opcode == 0x87#8 then some (.neg true dst)
  else if opcode == 0x97#8 then
    if imm32 == 0#32 then none else some (.alu64 .rem dst srcImm)
  else if opcode == 0x9f#8 then some (.alu64 .rem dst srcX)
  else if opcode == 0xa7#8 then some (.alu64 .xor dst srcImm)
  else if opcode == 0xaf#8 then some (.alu64 .xor dst srcX)
  else if opcode == 0xb7#8 then some (.alu64 .mov dst srcImm)
  else if opcode == 0xbf#8 then some (.alu64 .mov dst srcX)
  else if opcode == 0xc7#8 then
    if !imm32.msb && imm32 < 64#32 then some (.alu64 .arsh dst srcImm) else none
  else if opcode == 0xcf#8 then some (.alu64 .arsh dst srcX)
  -- 32-bit conditional jumps
  else if opcode == 0x16#8 then some (.condJump false .eq dst srcImm offInt)
  else if opcode == 0x1e#8 then some (.condJump false .eq dst srcX offInt)
  else if opcode == 0x26#8 then some (.condJump false .gt dst srcImm offInt)
  else if opcode == 0x2e#8 then some (.condJump false .gt dst srcX offInt)
  else if opcode == 0x36#8 then some (.condJump false .ge dst srcImm offInt)
  else if opcode == 0x3e#8 then some (.condJump false .ge dst srcX offInt)
  else if opcode == 0x46#8 then some (.condJump false .set dst srcImm offInt)
  else if opcode == 0x4e#8 then some (.condJump false .set dst srcX offInt)
  else if opcode == 0x56#8 then some (.condJump false .ne dst srcImm offInt)
  else if opcode == 0x5e#8 then some (.condJump false .ne dst srcX offInt)
  else if opcode == 0x66#8 then some (.condJump false .sgt dst srcImm offInt)
  else if opcode == 0x6e#8 then some (.condJump false .sgt dst srcX offInt)
  else if opcode == 0x76#8 then some (.condJump false .sge dst srcImm offInt)
  else if opcode == 0x7e#8 then some (.condJump false .sge dst srcX offInt)
  else if opcode == 0xa6#8 then some (.condJump false .lt dst srcImm offInt)
  else if opcode == 0xae#8 then some (.condJump false .lt dst srcX offInt)
  else if opcode == 0xb6#8 then some (.condJump false .le dst srcImm offInt)
  else if opcode == 0xbe#8 then some (.condJump false .le dst srcX offInt)
  else if opcode == 0xc6#8 then some (.condJump false .slt dst srcImm offInt)
  else if opcode == 0xce#8 then some (.condJump false .slt dst srcX offInt)
  else if opcode == 0xd6#8 then some (.condJump false .sle dst srcImm offInt)
  else if opcode == 0xde#8 then some (.condJump false .sle dst srcX offInt)
  -- Unconditional jump, 64-bit conditional jumps, external call, exit
  else if opcode == 0x05#8 then some (.jump offInt)
  else if opcode == 0x15#8 then some (.condJump true .eq dst srcImm offInt)
  else if opcode == 0x1d#8 then some (.condJump true .eq dst srcX offInt)
  else if opcode == 0x25#8 then some (.condJump true .gt dst srcImm offInt)
  else if opcode == 0x2d#8 then some (.condJump true .gt dst srcX offInt)
  else if opcode == 0x35#8 then some (.condJump true .ge dst srcImm offInt)
  else if opcode == 0x3d#8 then some (.condJump true .ge dst srcX offInt)
  else if opcode == 0x45#8 then some (.condJump true .set dst srcImm offInt)
  else if opcode == 0x4d#8 then some (.condJump true .set dst srcX offInt)
  else if opcode == 0x55#8 then some (.condJump true .ne dst srcImm offInt)
  else if opcode == 0x5d#8 then some (.condJump true .ne dst srcX offInt)
  else if opcode == 0x65#8 then some (.condJump true .sgt dst srcImm offInt)
  else if opcode == 0x6d#8 then some (.condJump true .sgt dst srcX offInt)
  else if opcode == 0x75#8 then some (.condJump true .sge dst srcImm offInt)
  else if opcode == 0x7d#8 then some (.condJump true .sge dst srcX offInt)
  else if opcode == 0xa5#8 then some (.condJump true .lt dst srcImm offInt)
  else if opcode == 0xad#8 then some (.condJump true .lt dst srcX offInt)
  else if opcode == 0xb5#8 then some (.condJump true .le dst srcImm offInt)
  else if opcode == 0xbd#8 then some (.condJump true .le dst srcX offInt)
  else if opcode == 0xc5#8 then some (.condJump true .slt dst srcImm offInt)
  else if opcode == 0xcd#8 then some (.condJump true .slt dst srcX offInt)
  else if opcode == 0xd5#8 then some (.condJump true .sle dst srcImm offInt)
  else if opcode == 0xdd#8 then some (.condJump true .sle dst srcX offInt)
  else if opcode == 0x85#8 then
    if srcReg == 0 then some (.callExternal imm32.toNat) else none
  else if opcode == 0x95#8 then some .exit
  -- Legacy packet loads
  else if opcode == 0x20#8 then some (.loadPacket .u32 imm32 none)
  else if opcode == 0x28#8 then some (.loadPacket .u16 imm32 none)
  else if opcode == 0x30#8 then some (.loadPacket .u8 imm32 none)
  else if opcode == 0x38#8 then some (.loadPacket .u64 imm32 none)
  else if opcode == 0x40#8 then some (.loadPacket .u32 imm32 (some srcReg))
  else if opcode == 0x48#8 then some (.loadPacket .u16 imm32 (some srcReg))
  else if opcode == 0x50#8 then some (.loadPacket .u8 imm32 (some srcReg))
  else if opcode == 0x58#8 then some (.loadPacket .u64 imm32 (some srcReg))
  -- Memory loads
  else if opcode == 0x61#8 then some (.load .u32 dst srcReg off16)
  else if opcode == 0x69#8 then some (.load .u16 dst srcReg off16)
  else if opcode == 0x71#8 then some (.load .u8 dst srcReg off16)
  else if opcode == 0x79#8 then some (.load .u64 dst srcReg off16)
  -- Memory immediate stores
  else if opcode == 0x62#8 then some (.store .u32 dst off16 srcImm)
  else if opcode == 0x6a#8 then some (.store .u16 dst off16 srcImm)
  else if opcode == 0x72#8 then some (.store .u8 dst off16 srcImm)
  else if opcode == 0x7a#8 then some (.store .u64 dst off16 srcImm)
  -- Memory register stores
  else if opcode == 0x63#8 then some (.store .u32 dst off16 srcX)
  else if opcode == 0x6b#8 then some (.store .u16 dst off16 srcX)
  else if opcode == 0x73#8 then some (.store .u8 dst off16 srcX)
  else if opcode == 0x7b#8 then some (.store .u64 dst off16 srcX)
  -- Atomic operations
  else if opcode == 0xc3#8 then
    (decodeAtomicOp (imm32.setWidth 8)).map (fun aop => .atomic false aop dst off16 srcReg)
  else if opcode == 0xdb#8 then
    (decodeAtomicOp (imm32.setWidth 8)).map (fun aop => .atomic true aop dst off16 srcReg)
  else none

private theorem decodeSingleInsn_eq_at (raw : ebpf_core.api.EbpfInstruction) :
    decodeSingleInsn raw = decodeSingleInsnAt (raw.bv.setWidth 8) raw := rfl

private theorem decodeSingleInsnAt_0xc3_eq (raw : ebpf_core.api.EbpfInstruction) :
    decodeSingleInsnAt 0xc3#8 raw =
      (decodeAtomicOp (((raw.bv >>> 32).setWidth 32).setWidth 8)).map
        (fun aop => .atomic false aop (((raw.bv >>> 8) &&& 0xF#64).toNat)
          ((raw.bv >>> 16).setWidth 16) (((raw.bv >>> 12) &&& 0xF#64).toNat)) := by
  unfold decodeSingleInsnAt
  simp +decide only [↓reduceIte]

private theorem decodeSingleInsnAt_0xdb_eq (raw : ebpf_core.api.EbpfInstruction) :
    decodeSingleInsnAt 0xdb#8 raw =
      (decodeAtomicOp (((raw.bv >>> 32).setWidth 32).setWidth 8)).map
        (fun aop => .atomic true aop (((raw.bv >>> 8) &&& 0xF#64).toNat)
          ((raw.bv >>> 16).setWidth 16) (((raw.bv >>> 12) &&& 0xF#64).toNat)) := by
  unfold decodeSingleInsnAt
  simp +decide only [↓reduceIte]

private def VisitDefaultPost {Self C : Type}
    (V : ebpf_core.visitor.BpfVisitor Self C)
    (self self' : Self) (ctx ctx' : C) (inst : ebpf_core.api.EbpfInstruction) : Prop :=
  inst.bv.setWidth 8 ≠ 0#8 ∧
  ((inst.bv.setWidth 8 = 0x18#8 ∧
      V.load64 self ctx
        ⟨((inst.bv >>> 8) &&& 0xF#64).setWidth 8⟩
        ⟨((inst.bv >>> 12) &&& 0xF#64).setWidth 8⟩
        ⟨(inst.bv >>> 32).setWidth 32⟩ = .ok (.Ok (), self', ctx')) ∨
   (inst.bv.setWidth 8 ≠ 0x18#8 ∧
      ∃ insn,
        decodeSingleInsn inst = some insn ∧
        VisitorDispatchNonLddw V self ctx (.ok (.Ok (), self', ctx')) insn))

private theorem bv8_class_cases (b : BitVec 8) :
    b &&& 7#8 = 0#8 ∨ b &&& 7#8 = 1#8 ∨ b &&& 7#8 = 2#8 ∨ b &&& 7#8 = 3#8 ∨
    b &&& 7#8 = 4#8 ∨ b &&& 7#8 = 5#8 ∨ b &&& 7#8 = 6#8 ∨ b &&& 7#8 = 7#8 := by
  bv_decide

private theorem bv8_msb_cases (b : BitVec 8) :
    b &&& 0x80#8 = 0#8 ∨ b &&& 0x80#8 = 0x80#8 := by
  bv_decide

private theorem bv8_nibble_cases (b : BitVec 8) :
    (b >>> 3) &&& 0xF#8 = 0#8 ∨ (b >>> 3) &&& 0xF#8 = 1#8 ∨
    (b >>> 3) &&& 0xF#8 = 2#8 ∨ (b >>> 3) &&& 0xF#8 = 3#8 ∨
    (b >>> 3) &&& 0xF#8 = 4#8 ∨ (b >>> 3) &&& 0xF#8 = 5#8 ∨
    (b >>> 3) &&& 0xF#8 = 6#8 ∨ (b >>> 3) &&& 0xF#8 = 7#8 ∨
    (b >>> 3) &&& 0xF#8 = 8#8 ∨ (b >>> 3) &&& 0xF#8 = 9#8 ∨
    (b >>> 3) &&& 0xF#8 = 10#8 ∨ (b >>> 3) &&& 0xF#8 = 11#8 ∨
    (b >>> 3) &&& 0xF#8 = 12#8 ∨ (b >>> 3) &&& 0xF#8 = 13#8 ∨
    (b >>> 3) &&& 0xF#8 = 14#8 ∨ (b >>> 3) &&& 0xF#8 = 15#8 := by
  bv_decide

private theorem bv8_reconstruct (b c m k : BitVec 8)
    (hc : b &&& 7#8 = c) (hm : b &&& 0x80#8 = m) (hk : (b >>> 3) &&& 0xF#8 = k) :
    b = (k <<< 3) ||| m ||| c := by
  bv_decide

local macro "prep_visit_leaf" inst:ident hop8:ident hVisit:ident : tactic =>
  `(tactic| (
    delta ebpf_core.visitor.BpfVisitor.visit.default
      ebpf_core.visitor.BpfVisitor.visit.default.match_1 at $hVisit:ident;
    rw [bind_ok_eq (ebpf_dst_reg_eq_ok $inst),
        bind_ok_eq (ebpf_imm_eq_ok $inst),
        bind_ok_eq (lift_eq_ok _),
        bind_ok_eq (ebpf_src_reg_eq_ok $inst),
        bind_ok_eq (ebpf_code_eq_ok $inst),
        $hop8:ident] at $hVisit:ident;
    simp +decide only [lift, bind_ok, ↓reduceDIte, hcast_i32_to_u64, hcast_i32_to_u32,
      hcast_i32_to_u8, ebpf_offset_eq_ok] at $hVisit:ident))

local macro "close_invalid_leaf" inst:ident hc:ident hm:ident hk:ident hVisit:ident : tactic =>
  `(tactic| (
    have hop8 := bv8_reconstruct (BitVec.setWidth 8 (UScalar.bv $inst)) _ _ _ $hc $hm $hk;
    prep_visit_leaf $inst hop8 $hVisit;
    exact False.elim (by cases (Prod.mk.inj (ok_inj $hVisit)).1)))

private theorem mk_visit_non_lddw {Self C : Type}
    {V : ebpf_core.visitor.BpfVisitor Self C}
    {self self' : Self} {ctx ctx' : C} {inst : ebpf_core.api.EbpfInstruction}
    {c : BitVec 8} {insn : Insn}
    (hop8 : inst.bv.setWidth 8 = c)
    (hc0 : c ≠ 0#8) (hc18 : c ≠ 0x18#8)
    (hdec : decodeSingleInsnAt c inst = some insn)
    (hdisp : VisitorDispatchNonLddw V self ctx (.ok (.Ok (), self', ctx')) insn) :
    VisitDefaultPost V self self' ctx ctx' inst :=
  ⟨hop8 ▸ hc0, Or.inr ⟨hop8 ▸ hc18, insn, (decodeSingleInsn_eq_at inst).trans (hop8 ▸ hdec), hdisp⟩⟩

private theorem mk_visit_non_lddw' {Self C : Type}
    {V : ebpf_core.visitor.BpfVisitor Self C}
    {self self' : Self} {ctx ctx' : C} {inst : ebpf_core.api.EbpfInstruction}
    {c : BitVec 8} (insn : Insn)
    (hop8 : inst.bv.setWidth 8 = c)
    (hc0 : c ≠ 0#8) (hc18 : c ≠ 0x18#8)
    (hdec : decodeSingleInsnAt c inst = some insn)
    (hdisp : VisitorDispatchNonLddw V self ctx (.ok (.Ok (), self', ctx')) insn) :
    VisitDefaultPost V self self' ctx ctx' inst :=
  mk_visit_non_lddw hop8 hc0 hc18 hdec hdisp

private theorem visit_default_ok_class0_lo {Self C : Type}
    (V : ebpf_core.visitor.BpfVisitor Self C)
    (self self' : Self) (ctx ctx' : C) (inst : ebpf_core.api.EbpfInstruction)
    (hc : inst.bv.setWidth 8 &&& 7#8 = 0#8)
    (hm : inst.bv.setWidth 8 &&& 0x80#8 = 0#8)
    (hVisit : ebpf_core.visitor.BpfVisitor.visit.default V self ctx inst =
      .ok (.Ok (), self', ctx')) :
    VisitDefaultPost V self self' ctx ctx' inst := by
  have hSrcVal := nibble_setWidth8_val inst.bv 12
  rcases bv8_nibble_cases (inst.bv.setWidth 8) with
    hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · have hop8 : inst.bv.setWidth 8 = 0x18#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    exact ⟨by rw [hop8]; decide, Or.inl ⟨hop8, hVisit⟩⟩
  · have hop8 : inst.bv.setWidth 8 = 0x20#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, rfl, Or.inl ⟨rfl, hVisit.symm⟩⟩
  · have hop8 : inst.bv.setWidth 8 = 0x28#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, rfl, Or.inl ⟨rfl, hVisit.symm⟩⟩
  · have hop8 : inst.bv.setWidth 8 = 0x30#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, rfl, Or.inl ⟨rfl, hVisit.symm⟩⟩
  · have hop8 : inst.bv.setWidth 8 = 0x38#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, rfl, Or.inl ⟨rfl, hVisit.symm⟩⟩
  · have hop8 : inst.bv.setWidth 8 = 0x40#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, rfl, Or.inr ⟨_, congrArg some hSrcVal.symm, hVisit.symm⟩⟩
  · have hop8 : inst.bv.setWidth 8 = 0x48#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, rfl, Or.inr ⟨_, congrArg some hSrcVal.symm, hVisit.symm⟩⟩
  · have hop8 : inst.bv.setWidth 8 = 0x50#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, rfl, Or.inr ⟨_, congrArg some hSrcVal.symm, hVisit.symm⟩⟩
  · have hop8 : inst.bv.setWidth 8 = 0x58#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, rfl, Or.inr ⟨_, congrArg some hSrcVal.symm, hVisit.symm⟩⟩
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit

private theorem visit_default_ok_all_invalid_hi {Self C : Type}
    (V : ebpf_core.visitor.BpfVisitor Self C)
    (self self' : Self) (ctx ctx' : C) (inst : ebpf_core.api.EbpfInstruction)
    (c : BitVec 8) (hc_val : c = 0#8 ∨ c = 1#8 ∨ c = 2#8)
    (hc : inst.bv.setWidth 8 &&& 7#8 = c)
    (hm : inst.bv.setWidth 8 &&& 0x80#8 = 0x80#8)
    (hVisit : ebpf_core.visitor.BpfVisitor.visit.default V self ctx inst =
      .ok (.Ok (), self', ctx')) :
    VisitDefaultPost V self self' ctx ctx' inst := by
  rcases hc_val with rfl | rfl | rfl <;> {
    rcases bv8_nibble_cases (inst.bv.setWidth 8) with
      hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk <;>
      close_invalid_leaf inst hc hm hk hVisit
  }

private theorem visit_default_ok_class1_lo {Self C : Type}
    (V : ebpf_core.visitor.BpfVisitor Self C)
    (self self' : Self) (ctx ctx' : C) (inst : ebpf_core.api.EbpfInstruction)
    (hc : inst.bv.setWidth 8 &&& 7#8 = 1#8)
    (hm : inst.bv.setWidth 8 &&& 0x80#8 = 0#8)
    (hVisit : ebpf_core.visitor.BpfVisitor.visit.default V self ctx inst =
      .ok (.Ok (), self', ctx')) :
    VisitDefaultPost V self self' ctx ctx' inst := by
  have hDstVal := nibble_setWidth8_val inst.bv 8
  have hSrcVal := nibble_setWidth8_val inst.bv 12
  rcases bv8_nibble_cases (inst.bv.setWidth 8) with
    hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · have hop8 : inst.bv.setWidth 8 = 0x61#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, _, _, hDstVal.symm, hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x69#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, _, _, hDstVal.symm, hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x71#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, _, _, hDstVal.symm, hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x79#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, _, _, hDstVal.symm, hSrcVal.symm, rfl, hVisit.symm⟩

private theorem visit_default_ok_class2_lo {Self C : Type}
    (V : ebpf_core.visitor.BpfVisitor Self C)
    (self self' : Self) (ctx ctx' : C) (inst : ebpf_core.api.EbpfInstruction)
    (hc : inst.bv.setWidth 8 &&& 7#8 = 2#8)
    (hm : inst.bv.setWidth 8 &&& 0x80#8 = 0#8)
    (hVisit : ebpf_core.visitor.BpfVisitor.visit.default V self ctx inst =
      .ok (.Ok (), self', ctx')) :
    VisitDefaultPost V self self' ctx ctx' inst := by
  have hDstVal := nibble_setWidth8_val inst.bv 8
  rcases bv8_nibble_cases (inst.bv.setWidth 8) with
    hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · have hop8 : inst.bv.setWidth 8 = 0x62#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, _, .Value _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x6a#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, _, .Value _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x72#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, _, .Value _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x7a#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, _, .Value _, hDstVal.symm, rfl, rfl, hVisit.symm⟩

private theorem visit_default_ok_class3_lo {Self C : Type}
    (V : ebpf_core.visitor.BpfVisitor Self C)
    (self self' : Self) (ctx ctx' : C) (inst : ebpf_core.api.EbpfInstruction)
    (hc : inst.bv.setWidth 8 &&& 7#8 = 3#8)
    (hm : inst.bv.setWidth 8 &&& 0x80#8 = 0#8)
    (hVisit : ebpf_core.visitor.BpfVisitor.visit.default V self ctx inst =
      .ok (.Ok (), self', ctx')) :
    VisitDefaultPost V self self' ctx ctx' inst := by
  have hDstVal := nibble_setWidth8_val inst.bv 8
  have hSrcVal := nibble_setWidth8_val inst.bv 12
  rcases bv8_nibble_cases (inst.bv.setWidth 8) with
    hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · have hop8 : inst.bv.setWidth 8 = 0x63#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, _, .Reg _, hDstVal.symm, rfl, congrArg Operand.reg hSrcVal.symm, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x6b#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, _, .Reg _, hDstVal.symm, rfl, congrArg Operand.reg hSrcVal.symm, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x73#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, _, .Reg _, hDstVal.symm, rfl, congrArg Operand.reg hSrcVal.symm, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x7b#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, _, .Reg _, hDstVal.symm, rfl, congrArg Operand.reg hSrcVal.symm, hVisit.symm⟩

private theorem visit_default_ok_class3_hi {Self C : Type}
    (V : ebpf_core.visitor.BpfVisitor Self C)
    (self self' : Self) (ctx ctx' : C) (inst : ebpf_core.api.EbpfInstruction)
    (hc : inst.bv.setWidth 8 &&& 7#8 = 3#8)
    (hm : inst.bv.setWidth 8 &&& 0x80#8 = 0x80#8)
    (hVisit : ebpf_core.visitor.BpfVisitor.visit.default V self ctx inst =
      .ok (.Ok (), self', ctx')) :
    VisitDefaultPost V self self' ctx ctx' inst := by
  have hDstVal := nibble_setWidth8_val inst.bv 8
  have hSrcVal := nibble_setWidth8_val inst.bv 12
  rcases bv8_nibble_cases (inst.bv.setWidth 8) with
    hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · -- 0xc3: 32-bit atomics
    have hop8 : inst.bv.setWidth 8 = 0xc3#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    split at hVisit
    all_goals
      try (simp only [Result.ok.injEq, Prod.mk.injEq] at hVisit; exact False.elim (by cases hVisit.1))
    all_goals
      rename_i hAtEq
      have hat8 : ((inst.bv >>> 32).setWidth 32).setWidth 8 = _ := congrArg UScalar.bv hAtEq
      apply mk_visit_non_lddw hop8 (by decide) (by decide)
      · rw [decodeSingleInsnAt_0xc3_eq, hat8]; rfl
      · exact ⟨_, _, _, hDstVal.symm, rfl, hSrcVal.symm, hVisit.symm⟩
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · -- 0xdb: 64-bit atomics
    have hop8 : inst.bv.setWidth 8 = 0xdb#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    split at hVisit
    all_goals
      try (simp only [Result.ok.injEq, Prod.mk.injEq] at hVisit; exact False.elim (by cases hVisit.1))
    all_goals
      rename_i hAtEq
      have hat8 : ((inst.bv >>> 32).setWidth 32).setWidth 8 = _ := congrArg UScalar.bv hAtEq
      apply mk_visit_non_lddw hop8 (by decide) (by decide)
      · rw [decodeSingleInsnAt_0xdb_eq, hat8]; rfl
      · exact ⟨_, _, _, hDstVal.symm, rfl, hSrcVal.symm, hVisit.symm⟩
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit

private theorem visit_default_ok_class4_lo {Self C : Type}
    (V : ebpf_core.visitor.BpfVisitor Self C)
    (self self' : Self) (ctx ctx' : C) (inst : ebpf_core.api.EbpfInstruction)
    (hc : inst.bv.setWidth 8 &&& 7#8 = 4#8)
    (hm : inst.bv.setWidth 8 &&& 0x80#8 = 0#8)
    (hVisit : ebpf_core.visitor.BpfVisitor.visit.default V self ctx inst =
      .ok (.Ok (), self', ctx')) :
    VisitDefaultPost V self self' ctx ctx' inst := by
  have hDstVal := nibble_setWidth8_val inst.bv 8
  have hSrcVal := nibble_setWidth8_val inst.bv 12
  rcases bv8_nibble_cases (inst.bv.setWidth 8) with
    hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk
  · have hop8 : inst.bv.setWidth 8 = 0x04#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous32 ⟨by decide, by decide⟩, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x0c#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous32 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x14#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous32 ⟨by decide, by decide⟩, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x1c#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous32 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x24#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous32 ⟨by decide, by decide⟩, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x2c#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous32 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x34#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    rw [div_imm_spec] at hVisit; simp only [bind_ok] at hVisit
    by_cases hz : ((inst.bv >>> 32).setWidth 32 == 0#32) = true
    · simp only [hz, ↓reduceIte, Result.ok.injEq, Prod.mk.injEq] at hVisit; cases hVisit.1
    · have hzf := Bool.eq_false_iff.mpr hz
      simp only [hzf, Bool.false_eq_true, ↓reduceIte] at hVisit
      refine mk_visit_non_lddw'
        (.alu32 .div ((inst.bv >>> 8) &&& 0xF#64).toNat (.imm (((inst.bv >>> 32).setWidth 32).signExtend 64)))
        hop8 (by decide) (by decide) ?_ ?_
      · unfold decodeSingleInsnAt; simp +decide only [hzf, Bool.false_eq_true, ↓reduceIte]
      · refine ⟨_, .Value _, hDstVal.symm, rfl, ?_, hVisit.symm⟩
        intro _ v hv; injection hv with hv'; subst hv'
        exact sext32_setWidth32_ne_zero_of_ne_zero (beq_eq_false_iff_ne.mp hzf)
  · have hop8 : inst.bv.setWidth 8 = 0x3c#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous32 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x44#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous32 ⟨by decide, by decide⟩, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x4c#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous32 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x54#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous32 ⟨by decide, by decide⟩, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x5c#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous32 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x64#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    rw [shift_imm_32_spec] at hVisit; simp only [bind_ok] at hVisit
    by_cases hs : (!((inst.bv >>> 32).setWidth 32).msb && decide ((inst.bv >>> 32).setWidth 32 < 32#32)) = true
    · simp only [hs, ↓reduceIte] at hVisit
      apply mk_visit_non_lddw hop8 (by decide) (by decide)
      · unfold decodeSingleInsnAt; dsimp only []; rw [hs]; rfl
      · exact ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous32 ⟨by decide, by decide⟩, hVisit.symm⟩
    · have hsf := Bool.eq_false_iff.mpr hs
      simp only [hsf, Bool.false_eq_true, ↓reduceIte, Result.ok.injEq, Prod.mk.injEq] at hVisit; cases hVisit.1
  · have hop8 : inst.bv.setWidth 8 = 0x6c#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous32 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x74#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    rw [shift_imm_32_spec] at hVisit; simp only [bind_ok] at hVisit
    by_cases hs : (!((inst.bv >>> 32).setWidth 32).msb && decide ((inst.bv >>> 32).setWidth 32 < 32#32)) = true
    · simp only [hs, ↓reduceIte] at hVisit
      apply mk_visit_non_lddw hop8 (by decide) (by decide)
      · unfold decodeSingleInsnAt; dsimp only []; rw [hs]; rfl
      · exact ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous32 ⟨by decide, by decide⟩, hVisit.symm⟩
    · have hsf := Bool.eq_false_iff.mpr hs
      simp only [hsf, Bool.false_eq_true, ↓reduceIte, Result.ok.injEq, Prod.mk.injEq] at hVisit; cases hVisit.1
  · have hop8 : inst.bv.setWidth 8 = 0x7c#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous32 _, hVisit.symm⟩

private theorem visit_default_ok_class4_hi {Self C : Type}
    (V : ebpf_core.visitor.BpfVisitor Self C)
    (self self' : Self) (ctx ctx' : C) (inst : ebpf_core.api.EbpfInstruction)
    (hc : inst.bv.setWidth 8 &&& 7#8 = 4#8)
    (hm : inst.bv.setWidth 8 &&& 0x80#8 = 0x80#8)
    (hVisit : ebpf_core.visitor.BpfVisitor.visit.default V self ctx inst =
      .ok (.Ok (), self', ctx')) :
    VisitDefaultPost V self self' ctx ctx' inst := by
  have hDstVal := nibble_setWidth8_val inst.bv 8
  have hSrcVal := nibble_setWidth8_val inst.bv 12
  rcases bv8_nibble_cases (inst.bv.setWidth 8) with
    hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk
  · have hop8 : inst.bv.setWidth 8 = 0x84#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, hDstVal.symm, hVisit.symm⟩
  · close_invalid_leaf inst hc hm hk hVisit
  · have hop8 : inst.bv.setWidth 8 = 0x94#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    rw [div_imm_spec] at hVisit; simp only [bind_ok] at hVisit
    by_cases hz : ((inst.bv >>> 32).setWidth 32 == 0#32) = true
    · simp only [hz, ↓reduceIte, Result.ok.injEq, Prod.mk.injEq] at hVisit; cases hVisit.1
    · have hzf := Bool.eq_false_iff.mpr hz
      simp only [hzf, Bool.false_eq_true, ↓reduceIte] at hVisit
      refine mk_visit_non_lddw'
        (.alu32 .rem ((inst.bv >>> 8) &&& 0xF#64).toNat (.imm (((inst.bv >>> 32).setWidth 32).signExtend 64)))
        hop8 (by decide) (by decide) ?_ ?_
      · unfold decodeSingleInsnAt; simp +decide only [hzf, Bool.false_eq_true, ↓reduceIte]
      · refine ⟨_, .Value _, hDstVal.symm, rfl, ?_, hVisit.symm⟩
        intro _ v hv; injection hv with hv'; subst hv'
        exact sext32_setWidth32_ne_zero_of_ne_zero (beq_eq_false_iff_ne.mp hzf)
  · have hop8 : inst.bv.setWidth 8 = 0x9c#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous32 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xa4#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous32 ⟨by decide, by decide⟩, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xac#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous32 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xb4#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous32 ⟨by decide, by decide⟩, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xbc#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous32 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xc4#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    rw [shift_imm_32_spec] at hVisit; simp only [bind_ok] at hVisit
    by_cases hs : (!((inst.bv >>> 32).setWidth 32).msb && decide ((inst.bv >>> 32).setWidth 32 < 32#32)) = true
    · simp only [hs, ↓reduceIte] at hVisit
      apply mk_visit_non_lddw hop8 (by decide) (by decide)
      · unfold decodeSingleInsnAt; dsimp only []; rw [hs]; rfl
      · exact ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous32 ⟨by decide, by decide⟩, hVisit.symm⟩
    · have hsf := Bool.eq_false_iff.mpr hs
      simp only [hsf, Bool.false_eq_true, ↓reduceIte, Result.ok.injEq, Prod.mk.injEq] at hVisit; cases hVisit.1
  · have hop8 : inst.bv.setWidth 8 = 0xcc#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous32 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xd4#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    rw [width_from_imm_spec] at hVisit; simp only [bind_ok] at hVisit
    cases hew : decodeEndianWidth ((inst.bv >>> 32).setWidth 32) with
    | none => simp only [hew, Result.ok.injEq, Prod.mk.injEq] at hVisit; cases hVisit.1
    | some ew =>
      simp only [hew] at hVisit
      apply mk_visit_non_lddw hop8 (by decide) (by decide)
      · unfold decodeSingleInsnAt; dsimp only []; rw [hew]; rfl
      · exact ⟨_, hDstVal.symm, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xdc#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    rw [width_from_imm_spec] at hVisit; simp only [bind_ok] at hVisit
    cases hew : decodeEndianWidth ((inst.bv >>> 32).setWidth 32) with
    | none => simp only [hew, Result.ok.injEq, Prod.mk.injEq] at hVisit; cases hVisit.1
    | some ew =>
      simp only [hew] at hVisit
      apply mk_visit_non_lddw hop8 (by decide) (by decide)
      · unfold decodeSingleInsnAt; dsimp only []; rw [hew]; rfl
      · exact ⟨_, hDstVal.symm, hVisit.symm⟩
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit

private theorem visit_default_ok_class5_lo {Self C : Type}
    (V : ebpf_core.visitor.BpfVisitor Self C)
    (self self' : Self) (ctx ctx' : C) (inst : ebpf_core.api.EbpfInstruction)
    (hc : inst.bv.setWidth 8 &&& 7#8 = 5#8)
    (hm : inst.bv.setWidth 8 &&& 0x80#8 = 0#8)
    (hVisit : ebpf_core.visitor.BpfVisitor.visit.default V self ctx inst =
      .ok (.Ok (), self', ctx')) :
    VisitDefaultPost V self self' ctx ctx' inst := by
  have hDstVal := nibble_setWidth8_val inst.bv 8
  have hSrcVal := nibble_setWidth8_val inst.bv 12
  rcases bv8_nibble_cases (inst.bv.setWidth 8) with
    hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk
  · have hop8 : inst.bv.setWidth 8 = 0x05#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, rfl, hVisit.symm⟩
  · close_invalid_leaf inst hc hm hk hVisit
  · have hop8 : inst.bv.setWidth 8 = 0x15#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x1d#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x25#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x2d#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x35#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x3d#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x45#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x4d#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x55#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x5d#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x65#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x6d#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x75#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x7d#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩

private theorem visit_default_ok_class5_hi {Self C : Type}
    (V : ebpf_core.visitor.BpfVisitor Self C)
    (self self' : Self) (ctx ctx' : C) (inst : ebpf_core.api.EbpfInstruction)
    (hc : inst.bv.setWidth 8 &&& 7#8 = 5#8)
    (hm : inst.bv.setWidth 8 &&& 0x80#8 = 0x80#8)
    (hVisit : ebpf_core.visitor.BpfVisitor.visit.default V self ctx inst =
      .ok (.Ok (), self', ctx')) :
    VisitDefaultPost V self self' ctx ctx' inst := by
  have hDstVal := nibble_setWidth8_val inst.bv 8
  have hSrcVal := nibble_setWidth8_val inst.bv 12
  rcases bv8_nibble_cases (inst.bv.setWidth 8) with
    hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk
  · have hop8 : inst.bv.setWidth 8 = 0x85#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    split at hVisit
    · rename_i hSrc0
      have hs0 : ((inst.bv >>> 12) &&& 0xF#64).toNat = 0 := by rw [← hSrcVal, hSrc0]; rfl
      refine mk_visit_non_lddw' (.callExternal ((inst.bv >>> 32).setWidth 32).toNat)
        hop8 (by decide) (by decide) ?_ ?_
      · unfold decodeSingleInsnAt; simp +decide only [hs0, beq_self_eq_true, ↓reduceIte]
      · exact ⟨⟨(inst.bv >>> 32).setWidth 32⟩, rfl, hVisit.symm⟩
    · simp only [Result.ok.injEq, Prod.mk.injEq] at hVisit; cases hVisit.1
  · close_invalid_leaf inst hc hm hk hVisit
  · have hop8 : inst.bv.setWidth 8 = 0x95#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl hVisit.symm
  · close_invalid_leaf inst hc hm hk hVisit
  · have hop8 : inst.bv.setWidth 8 = 0xa5#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xad#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xb5#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xbd#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xc5#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xcd#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xd5#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xdd#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit

private theorem visit_default_ok_class6_lo {Self C : Type}
    (V : ebpf_core.visitor.BpfVisitor Self C)
    (self self' : Self) (ctx ctx' : C) (inst : ebpf_core.api.EbpfInstruction)
    (hc : inst.bv.setWidth 8 &&& 7#8 = 6#8)
    (hm : inst.bv.setWidth 8 &&& 0x80#8 = 0#8)
    (hVisit : ebpf_core.visitor.BpfVisitor.visit.default V self ctx inst =
      .ok (.Ok (), self', ctx')) :
    VisitDefaultPost V self self' ctx ctx' inst := by
  have hDstVal := nibble_setWidth8_val inst.bv 8
  have hSrcVal := nibble_setWidth8_val inst.bv 12
  rcases bv8_nibble_cases (inst.bv.setWidth 8) with
    hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · have hop8 : inst.bv.setWidth 8 = 0x16#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x1e#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x26#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x2e#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x36#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x3e#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x46#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x4e#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x56#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x5e#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x66#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x6e#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x76#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x7e#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩

private theorem visit_default_ok_class6_hi {Self C : Type}
    (V : ebpf_core.visitor.BpfVisitor Self C)
    (self self' : Self) (ctx ctx' : C) (inst : ebpf_core.api.EbpfInstruction)
    (hc : inst.bv.setWidth 8 &&& 7#8 = 6#8)
    (hm : inst.bv.setWidth 8 &&& 0x80#8 = 0x80#8)
    (hVisit : ebpf_core.visitor.BpfVisitor.visit.default V self ctx inst =
      .ok (.Ok (), self', ctx')) :
    VisitDefaultPost V self self' ctx ctx' inst := by
  have hDstVal := nibble_setWidth8_val inst.bv 8
  have hSrcVal := nibble_setWidth8_val inst.bv 12
  rcases bv8_nibble_cases (inst.bv.setWidth 8) with
    hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · have hop8 : inst.bv.setWidth 8 = 0xa6#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xae#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xb6#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xbe#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xc6#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xce#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xd6#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, _, hDstVal.symm, rfl, rfl, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xde#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, rfl, hVisit.symm⟩
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit

private theorem visit_default_ok_class7_lo {Self C : Type}
    (V : ebpf_core.visitor.BpfVisitor Self C)
    (self self' : Self) (ctx ctx' : C) (inst : ebpf_core.api.EbpfInstruction)
    (hc : inst.bv.setWidth 8 &&& 7#8 = 7#8)
    (hm : inst.bv.setWidth 8 &&& 0x80#8 = 0#8)
    (hVisit : ebpf_core.visitor.BpfVisitor.visit.default V self ctx inst =
      .ok (.Ok (), self', ctx')) :
    VisitDefaultPost V self self' ctx ctx' inst := by
  have hDstVal := nibble_setWidth8_val inst.bv 8
  have hSrcVal := nibble_setWidth8_val inst.bv 12
  rcases bv8_nibble_cases (inst.bv.setWidth 8) with
    hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk
  · have hop8 : inst.bv.setWidth 8 = 0x07#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous64 ⟨by decide, by decide⟩, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x0f#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous64 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x17#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous64 ⟨by decide, by decide⟩, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x1f#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous64 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x27#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous64 ⟨by decide, by decide⟩, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x2f#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous64 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x37#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    rw [div_imm_spec] at hVisit; simp only [bind_ok] at hVisit
    by_cases hz : ((inst.bv >>> 32).setWidth 32 == 0#32) = true
    · simp only [hz, ↓reduceIte, Result.ok.injEq, Prod.mk.injEq] at hVisit; cases hVisit.1
    · have hzf := Bool.eq_false_iff.mpr hz
      simp only [hzf, Bool.false_eq_true, ↓reduceIte] at hVisit
      refine mk_visit_non_lddw'
        (.alu64 .div ((inst.bv >>> 8) &&& 0xF#64).toNat (.imm (((inst.bv >>> 32).setWidth 32).signExtend 64)))
        hop8 (by decide) (by decide) ?_ ?_
      · unfold decodeSingleInsnAt; simp +decide only [hzf, Bool.false_eq_true, ↓reduceIte]
      · refine ⟨_, .Value _, hDstVal.symm, rfl, ?_, hVisit.symm⟩
        intro _ v hv; injection hv with hv'; subst hv'
        exact sext32_ne_zero_of_ne_zero (beq_eq_false_iff_ne.mp hzf)
  · have hop8 : inst.bv.setWidth 8 = 0x3f#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous64 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x47#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous64 ⟨by decide, by decide⟩, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x4f#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous64 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x57#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous64 ⟨by decide, by decide⟩, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x5f#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous64 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x67#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    rw [shift_imm_64_spec] at hVisit; simp only [bind_ok] at hVisit
    by_cases hs : (!((inst.bv >>> 32).setWidth 32).msb && decide ((inst.bv >>> 32).setWidth 32 < 64#32)) = true
    · simp only [hs, ↓reduceIte] at hVisit
      apply mk_visit_non_lddw hop8 (by decide) (by decide)
      · unfold decodeSingleInsnAt; dsimp only []; rw [hs]; rfl
      · exact ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous64 ⟨by decide, by decide⟩, hVisit.symm⟩
    · have hsf := Bool.eq_false_iff.mpr hs
      simp only [hsf, Bool.false_eq_true, ↓reduceIte, Result.ok.injEq, Prod.mk.injEq] at hVisit; cases hVisit.1
  · have hop8 : inst.bv.setWidth 8 = 0x6f#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous64 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0x77#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    rw [shift_imm_64_spec] at hVisit; simp only [bind_ok] at hVisit
    by_cases hs : (!((inst.bv >>> 32).setWidth 32).msb && decide ((inst.bv >>> 32).setWidth 32 < 64#32)) = true
    · simp only [hs, ↓reduceIte] at hVisit
      apply mk_visit_non_lddw hop8 (by decide) (by decide)
      · unfold decodeSingleInsnAt; dsimp only []; rw [hs]; rfl
      · exact ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous64 ⟨by decide, by decide⟩, hVisit.symm⟩
    · have hsf := Bool.eq_false_iff.mpr hs
      simp only [hsf, Bool.false_eq_true, ↓reduceIte, Result.ok.injEq, Prod.mk.injEq] at hVisit; cases hVisit.1
  · have hop8 : inst.bv.setWidth 8 = 0x7f#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous64 _, hVisit.symm⟩

private theorem visit_default_ok_class7_hi {Self C : Type}
    (V : ebpf_core.visitor.BpfVisitor Self C)
    (self self' : Self) (ctx ctx' : C) (inst : ebpf_core.api.EbpfInstruction)
    (hc : inst.bv.setWidth 8 &&& 7#8 = 7#8)
    (hm : inst.bv.setWidth 8 &&& 0x80#8 = 0x80#8)
    (hVisit : ebpf_core.visitor.BpfVisitor.visit.default V self ctx inst =
      .ok (.Ok (), self', ctx')) :
    VisitDefaultPost V self self' ctx ctx' inst := by
  have hDstVal := nibble_setWidth8_val inst.bv 8
  have hSrcVal := nibble_setWidth8_val inst.bv 12
  rcases bv8_nibble_cases (inst.bv.setWidth 8) with
    hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk | hk
  · have hop8 : inst.bv.setWidth 8 = 0x87#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, hDstVal.symm, hVisit.symm⟩
  · close_invalid_leaf inst hc hm hk hVisit
  · have hop8 : inst.bv.setWidth 8 = 0x97#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    rw [div_imm_spec] at hVisit; simp only [bind_ok] at hVisit
    by_cases hz : ((inst.bv >>> 32).setWidth 32 == 0#32) = true
    · simp only [hz, ↓reduceIte, Result.ok.injEq, Prod.mk.injEq] at hVisit; cases hVisit.1
    · have hzf := Bool.eq_false_iff.mpr hz
      simp only [hzf, Bool.false_eq_true, ↓reduceIte] at hVisit
      refine mk_visit_non_lddw'
        (.alu64 .rem ((inst.bv >>> 8) &&& 0xF#64).toNat (.imm (((inst.bv >>> 32).setWidth 32).signExtend 64)))
        hop8 (by decide) (by decide) ?_ ?_
      · unfold decodeSingleInsnAt; simp +decide only [hzf, Bool.false_eq_true, ↓reduceIte]
      · refine ⟨_, .Value _, hDstVal.symm, rfl, ?_, hVisit.symm⟩
        intro _ v hv; injection hv with hv'; subst hv'
        exact sext32_ne_zero_of_ne_zero (beq_eq_false_iff_ne.mp hzf)
  · have hop8 : inst.bv.setWidth 8 = 0x9f#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous64 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xa7#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous64 ⟨by decide, by decide⟩, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xaf#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous64 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xb7#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous64 ⟨by decide, by decide⟩, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xbf#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous64 _, hVisit.symm⟩
  · have hop8 : inst.bv.setWidth 8 = 0xc7#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    rw [shift_imm_64_spec] at hVisit; simp only [bind_ok] at hVisit
    by_cases hs : (!((inst.bv >>> 32).setWidth 32).msb && decide ((inst.bv >>> 32).setWidth 32 < 64#32)) = true
    · simp only [hs, ↓reduceIte] at hVisit
      apply mk_visit_non_lddw hop8 (by decide) (by decide)
      · unfold decodeSingleInsnAt; dsimp only []; rw [hs]; rfl
      · exact ⟨_, .Value _, hDstVal.symm, rfl, visit_nondiv_vacuous64 ⟨by decide, by decide⟩, hVisit.symm⟩
    · have hsf := Bool.eq_false_iff.mpr hs
      simp only [hsf, Bool.false_eq_true, ↓reduceIte, Result.ok.injEq, Prod.mk.injEq] at hVisit; cases hVisit.1
  · have hop8 : inst.bv.setWidth 8 = 0xcf#8 := bv8_reconstruct _ _ _ _ hc hm hk
    prep_visit_leaf inst hop8 hVisit
    refine mk_visit_non_lddw hop8 (by decide) (by decide) rfl ⟨_, .Reg _, hDstVal.symm, congrArg Operand.reg hSrcVal.symm, visit_reg_vacuous64 _, hVisit.symm⟩
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit
  · close_invalid_leaf inst hc hm hk hVisit

/-- Complete soundness of `BpfVisitor.visit.default`: whenever `visit.default`
    returns `.ok (.Ok (), self', ctx')`, the instruction's opcode is non-zero,
    and either it is a primary `BPF_LDDW` (`0x18`) dispatching to `V.load64`, or
    `decodeSingleInsn` succeeds with `some insn` matching `VisitorDispatchNonLddw`. -/
theorem visit_default_ok_spec {Self C : Type}
    (V : ebpf_core.visitor.BpfVisitor Self C)
    (self self' : Self) (ctx ctx' : C) (inst : ebpf_core.api.EbpfInstruction)
    (hVisit : ebpf_core.visitor.BpfVisitor.visit.default V self ctx inst =
      .ok (.Ok (), self', ctx')) :
    inst.bv.setWidth 8 ≠ 0#8 ∧
    ((inst.bv.setWidth 8 = 0x18#8 ∧
        V.load64 self ctx
          ⟨((inst.bv >>> 8) &&& 0xF#64).setWidth 8⟩
          ⟨((inst.bv >>> 12) &&& 0xF#64).setWidth 8⟩
          ⟨(inst.bv >>> 32).setWidth 32⟩ = .ok (.Ok (), self', ctx')) ∨
     (inst.bv.setWidth 8 ≠ 0x18#8 ∧
        ∃ insn,
          decodeSingleInsn inst = some insn ∧
          VisitorDispatchNonLddw V self ctx (.ok (.Ok (), self', ctx')) insn)) := by
  change VisitDefaultPost V self self' ctx ctx' inst
  rcases bv8_msb_cases (inst.bv.setWidth 8) with hm | hm <;>
    rcases bv8_class_cases (inst.bv.setWidth 8) with hc | hc | hc | hc | hc | hc | hc | hc
  · exact visit_default_ok_class0_lo V self self' ctx ctx' inst hc hm hVisit
  · exact visit_default_ok_class1_lo V self self' ctx ctx' inst hc hm hVisit
  · exact visit_default_ok_class2_lo V self self' ctx ctx' inst hc hm hVisit
  · exact visit_default_ok_class3_lo V self self' ctx ctx' inst hc hm hVisit
  · exact visit_default_ok_class4_lo V self self' ctx ctx' inst hc hm hVisit
  · exact visit_default_ok_class5_lo V self self' ctx ctx' inst hc hm hVisit
  · exact visit_default_ok_class6_lo V self self' ctx ctx' inst hc hm hVisit
  · exact visit_default_ok_class7_lo V self self' ctx ctx' inst hc hm hVisit
  · exact visit_default_ok_all_invalid_hi V self self' ctx ctx' inst _ (Or.inl rfl) hc hm hVisit
  · exact visit_default_ok_all_invalid_hi V self self' ctx ctx' inst _ (Or.inr (Or.inl rfl)) hc hm hVisit
  · exact visit_default_ok_all_invalid_hi V self self' ctx ctx' inst _ (Or.inr (Or.inr rfl)) hc hm hVisit
  · exact visit_default_ok_class3_hi V self self' ctx ctx' inst hc hm hVisit
  · exact visit_default_ok_class4_hi V self self' ctx ctx' inst hc hm hVisit
  · exact visit_default_ok_class5_hi V self self' ctx ctx' inst hc hm hVisit
  · exact visit_default_ok_class6_hi V self self' ctx ctx' inst hc hm hVisit
  · exact visit_default_ok_class7_hi V self self' ctx ctx' inst hc hm hVisit

end VisitDefaultSpec

-- ============================================================================
-- Section 4: `resolve_lddw`, `PrescanValidTail`, and `verify_program_core` Loops
-- ============================================================================

theorem slice_index_usize_ok {α : Type u} {s : Slice α} {idx : Usize} {elem : α}
    (h : Slice.index_usize s idx = .ok elem) :
    idx.val < s.val.length ∧ s.val[idx.val]? = some elem := by
  unfold Slice.index_usize at h
  cases hGet : s[idx]? <;> rw [hGet] at h
  · simp at h
  · cases ok_inj h
    have hEq : s.val[idx.val]? = some elem := hGet
    exact ⟨(List.getElem?_eq_some_iff.mp hEq).1, by simp⟩

theorem vec_index_usize_ok {α : Type u} {v : alloc.vec.Vec α} {idx : Usize} {elem : α}
    (h : alloc.vec.Vec.index (core.slice.index.SliceIndexUsizeSlice α) v idx = .ok elem) :
    idx.val < v.val.length ∧ v.val[idx.val]? = some elem := by
  unfold alloc.vec.Vec.index core.slice.index.SliceIndexUsizeSlice
    core.slice.index.Usize.index at h
  exact slice_index_usize_ok h

theorem vec_push_ok_val {α : Type u} {v v' : alloc.vec.Vec α} {elem : α}
    (h : alloc.vec.Vec.push v elem = .ok v') :
    v'.val = v.val ++ [elem] := by
  unfold alloc.vec.Vec.push at h
  dsimp only at h
  split at h
  · have hv : v' = alloc.vec.Vec.from (v.val.concat elem) _ := (ok_inj h).symm
    subst hv
    simp [alloc.vec.Vec.from, alloc.vec.Vec.val]
  · simp at h

theorem vec_index_mut_usize_ok {α : Type u} {v : alloc.vec.Vec α} {idx : Usize}
    {elem : α} {back : α → alloc.vec.Vec α}
    (h : alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice α) v idx =
      .ok (elem, back)) :
    idx.val < v.val.length ∧ v.val[idx.val]? = some elem ∧
    ∀ u, (back u).val = v.val.set idx.val u ∧ (back u).val.length = v.val.length := by
  unfold alloc.vec.Vec.index_mut core.slice.index.SliceIndexUsizeSlice
    core.slice.index.Usize.index_mut Slice.index_mut_usize at h
  rcases bind_eq_ok h with ⟨⟨o, sb⟩, h1, h2⟩
  rcases bind_eq_ok h1 with ⟨x, hIdx, h1ok⟩
  have ⟨hLt, hGet⟩ := slice_index_usize_ok hIdx
  have hPair1 := Prod.mk.inj (ok_inj h1ok)
  have hPair2 := Prod.mk.inj (ok_inj h2)
  have hElem : elem = x := hPair2.1.symm.trans hPair1.1.symm
  have hsb : sb = v.slice.set idx := hPair1.2.symm
  have hback : back = fun o' => { slice := sb o' } := hPair2.2.symm
  refine ⟨hLt, hElem ▸ hGet, fun u => ?_⟩
  subst hback hsb
  refine ⟨Slice.set_val_eq v.slice idx u, ?_⟩
  change (v.slice.set idx u).val.length = v.slice.val.length
  simp [Slice.set]

theorem loop_unfold_step {α : Type u} {β : Type v}
    (body : α → Result (ControlFlow α β)) (x : α) :
    loop body x = (do
      let r ← body x
      match r with
      | .cont x' => loop body x'
      | .done y => Result.ok y) := by
  rw [loop.eq_def]
  rfl

theorem loop_ok_pre_post {α : Type u} {β : Type v}
    (body : α → Result (ControlFlow α β))
    (Pre : α → Prop)
    (Rel : α → β → Prop)
    (h_cont : ∀ x x' y, Pre x → body x = .ok (.cont x') → Pre x' ∧ (Rel x' y → Rel x y))
    (h_done : ∀ x y, Pre x → body x = .ok (.done y) → Rel x y) :
    ∀ x y, Pre x → loop body x = .ok y → Rel x y := by
  intro x0 y0 hPre0 hLoop
  exact loop_ok_rel_induct body (fun x y => Pre x → Rel x y)
    (fun x =>
      ⟨fun x' y hBody hIH hPre =>
          let ⟨hPre', hStep⟩ := h_cont x x' y hPre hBody
          hStep (hIH hPre'),
       fun y hBody hPre => h_done x y hPre hBody⟩)
    x0 y0 hLoop hPre0

theorem hcast_i32_to_usize_val (b32 : BitVec 32) (hmsb : b32.msb = false) :
    (IScalar.hcast .Usize (UScalar.hcast .I32 (⟨b32⟩ : U32))).val = b32.toNat := by
  dsimp only [IScalar.hcast, UScalar.hcast, UScalar.val]
  rw [IScalarTy.I32_numBits_eq, UScalarTy.Usize_numBits_eq]
  have hz : BitVec.zeroExtend 32 b32 = b32 := by simp
  rw [hz]
  cases System.Platform.numBits_eq with
  | inl h32 =>
    rw [h32]
    have hse : BitVec.signExtend 32 b32 = b32 := by simp
    rw [hse]
  | inr h64 =>
    rw [h64]
    have hse : BitVec.signExtend 64 b32 = BitVec.zeroExtend 64 b32 := by bv_decide
    simp [hse]
    exact Nat.lt_trans b32.isLt (by decide)

def lddwInsnOfResolved (dst : Nat) : ebpf_core.verifier.ResolvedLddw → Insn
  | .Imm val => .lddwImm dst val.bv
  | .MapPtr mapIdx _ => .lddwMap dst mapIdx.val
  | .MapValuePtr mapIdx _ offset => .lddwMapValue dst mapIdx.val (offset.bv.zeroExtend 64)

theorem lddwInsnOfResolved_not_lddwCont (dst : Nat) (r : ebpf_core.verifier.ResolvedLddw) :
    lddwInsnOfResolved dst r ≠ .lddwCont := by
  cases r <;> intro hc <;> cases hc

theorem resolve_lddw_ok_spec
    (cctx : ebpf_core.verifier.CoreCallingContext) (src : U8) (b1 b2 : BitVec 32)
    (rlddw : ebpf_core.verifier.ResolvedLddw)
    (hRes : ebpf_core.verifier.CoreCallingContext.resolve_lddw cctx src
      (UScalar.hcast .I32 (⟨b1⟩ : U32)) (UScalar.hcast .I32 (⟨b2⟩ : U32)) =
      .ok (.Ok rlddw)) :
    (src = 0#u8 ∧ rlddw = .Imm ⟨b1.zeroExtend 64 ||| ((b2.zeroExtend 64) <<< 32)⟩) ∨
    (∃ schema,
      src = 5#u8 ∧ b1.msb = false ∧
      b1.toNat < cctx.maps.val.length ∧
      cctx.maps.val[b1.toNat]? = some schema ∧
      rlddw = .MapPtr ⟨b1⟩ schema) ∨
    (∃ schema,
      src = 6#u8 ∧ b1.msb = false ∧ b2.msb = false ∧
      b1.toNat < cctx.maps.val.length ∧
      cctx.maps.val[b1.toNat]? = some schema ∧
      schema.map_type = ebpf_core.api.BPF_MAP_TYPE_ARRAY ∧
      schema.max_entries ≠ 0#u32 ∧
      b2.toNat < schema.value_size.val ∧
      rlddw = .MapValuePtr ⟨b1⟩ schema ⟨b2⟩) := by
  unfold ebpf_core.verifier.CoreCallingContext.resolve_lddw at hRes
  split at hRes
  · -- src = 0
    simp only [lift, bind_ok, hcast_i32_to_u32,
      u64_shl_i32_const _ 32#i32 (by decide) (by decide),
      Result.ok.injEq, core.result.Result.Ok.injEq] at hRes
    exact Or.inl ⟨rfl, hRes.symm⟩
  · -- src = 5
    simp only [lift, bind_ok, hcast_i32_to_u32] at hRes
    by_cases hm1 : b1.msb = true
    · have hlt1 : UScalar.hcast .I32 (⟨b1⟩ : U32) < 0#i32 := (i32_lt_zero_iff_msb b1).mpr hm1
      simp only [hlt1, ↓reduceIte] at hRes
      cases ok_inj hRes
    · have hm1f : b1.msb = false := Bool.eq_false_iff.mpr hm1
      have hnlt1 : ¬ (UScalar.hcast .I32 (⟨b1⟩ : U32) < 0#i32) :=
        fun hc => hm1 ((i32_lt_zero_iff_msb b1).mp hc)
      simp only [hnlt1, ↓reduceIte] at hRes
      split at hRes
      · cases ok_inj hRes
      · rcases bind_eq_ok hRes with ⟨schema, hIdx, hOk⟩
        have hOkEq : ebpf_core.verifier.ResolvedLddw.MapPtr ⟨b1⟩ schema = rlddw :=
          core.result.Result.Ok.inj (ok_inj hOk)
        have hVal1 := hcast_i32_to_usize_val b1 hm1f
        have ⟨hLtIdx, hGetIdx⟩ := vec_index_usize_ok hIdx
        rw [hVal1] at hLtIdx hGetIdx
        exact Or.inr (Or.inl ⟨schema, rfl, hm1f, hLtIdx, hGetIdx, hOkEq.symm⟩)
  · -- src = 6
    simp only [lift, bind_ok, hcast_i32_to_u32] at hRes
    by_cases hm1 : b1.msb = true
    · have hlt1 : UScalar.hcast .I32 (⟨b1⟩ : U32) < 0#i32 := (i32_lt_zero_iff_msb b1).mpr hm1
      simp only [hlt1, ↓reduceIte] at hRes
      cases ok_inj hRes
    · have hm1f : b1.msb = false := Bool.eq_false_iff.mpr hm1
      have hnlt1 : ¬ (UScalar.hcast .I32 (⟨b1⟩ : U32) < 0#i32) :=
        fun hc => hm1 ((i32_lt_zero_iff_msb b1).mp hc)
      simp only [hnlt1, ↓reduceIte] at hRes
      split at hRes
      · cases ok_inj hRes
      · rcases bind_eq_ok hRes with ⟨schema, hIdx, hRest⟩
        have hVal1 := hcast_i32_to_usize_val b1 hm1f
        have ⟨hLtIdx, hGetIdx⟩ := vec_index_usize_ok hIdx
        rw [hVal1] at hLtIdx hGetIdx
        split at hRest
        · cases ok_inj hRest
        · rename_i hNotNeType
          have hType : schema.map_type = ebpf_core.api.BPF_MAP_TYPE_ARRAY := by
            by_contra hne
            exact hNotNeType (bne_iff_ne.mpr hne)
          split at hRest
          · cases ok_inj hRest
          · rename_i hMaxNe
            by_cases hm2 : b2.msb = true
            · have hlt2 : UScalar.hcast .I32 (⟨b2⟩ : U32) < 0#i32 :=
                (i32_lt_zero_iff_msb b2).mpr hm2
              simp only [hlt2, ↓reduceIte] at hRest
              cases ok_inj hRest
            · have hm2f : b2.msb = false := Bool.eq_false_iff.mpr hm2
              have hnlt2 : ¬ (UScalar.hcast .I32 (⟨b2⟩ : U32) < 0#i32) :=
                fun hc => hm2 ((i32_lt_zero_iff_msb b2).mp hc)
              simp only [hnlt2, ↓reduceIte] at hRest
              split at hRest
              · cases ok_inj hRest
              · rename_i hNgeVal
                have hOkEq : ebpf_core.verifier.ResolvedLddw.MapValuePtr ⟨b1⟩ schema ⟨b2⟩ = rlddw :=
                  core.result.Result.Ok.inj (ok_inj hRest)
                try rw [hcast_i32_to_u32] at hNgeVal
                have hOffLt : b2.toNat < schema.value_size.val := by
                  have hltU : (⟨b2⟩ : U32) < schema.value_size := not_le.mp hNgeVal
                  exact (UScalar.lt_equiv _ _).mp hltU
                exact Or.inr (Or.inr ⟨schema, rfl, hm1f, hm2f, hLtIdx, hGetIdx,
                  hType, hMaxNe, hOffLt, hOkEq.symm⟩)
  · cases ok_inj hRes

theorem decodeVerifiedCodeList_single_step
    (raw : ebpf_core.api.EbpfInstruction) (tail : List ebpf_core.api.EbpfInstruction)
    (insn : Insn) (decodedTail : List Insn)
    (hNotLddw : raw.bv.setWidth 8 ≠ 0x18#8)
    (hDec : decodeSingleInsn raw = some insn)
    (hTail : decodeVerifiedCodeList tail = some decodedTail) :
    decodeVerifiedCodeList (raw :: tail) = some (insn :: decodedTail) := by
  unfold decodeVerifiedCodeList
  have hbf : (raw.bv.setWidth 8 == 0x18#8) = false := beq_eq_false_iff_ne.mpr hNotLddw
  simp only [hbf, Bool.false_eq_true, ↓reduceIte, hDec, hTail]

theorem decodeVerifiedCodeList_lddw_step
    (cctx : ebpf_core.verifier.CoreCallingContext)
    (raw1 raw2 : ebpf_core.api.EbpfInstruction) (tail : List ebpf_core.api.EbpfInstruction)
    (rlddw : ebpf_core.verifier.ResolvedLddw) (decodedTail : List Insn)
    (hIsLddw : raw1.bv.setWidth 8 = 0x18#8)
    (hContZero : raw2.bv.setWidth 32 = 0#32)
    (hRes : ebpf_core.verifier.CoreCallingContext.resolve_lddw cctx
      ⟨((raw1.bv >>> 12) &&& 0xF#64).setWidth 8⟩
      (UScalar.hcast .I32 (⟨(raw1.bv >>> 32).setWidth 32⟩ : U32))
      (UScalar.hcast .I32 (⟨(raw2.bv >>> 32).setWidth 32⟩ : U32)) = .ok (.Ok rlddw))
    (hTail : decodeVerifiedCodeList tail = some decodedTail) :
    decodeVerifiedCodeList (raw1 :: raw2 :: tail) =
      some (lddwInsnOfResolved (((raw1.bv >>> 8) &&& 0xF#64).toNat) rlddw ::
            .lddwCont :: decodedTail) := by
  have hSpec := resolve_lddw_ok_spec cctx
    ⟨((raw1.bv >>> 12) &&& 0xF#64).setWidth 8⟩
    ((raw1.bv >>> 32).setWidth 32) ((raw2.bv >>> 32).setWidth 32) rlddw hRes
  have hSrcVal := nibble_setWidth8_val raw1.bv 12
  unfold decodeVerifiedCodeList
  have hbLddw : (raw1.bv.setWidth 8 == 0x18#8) = true := beq_iff_eq.mpr hIsLddw
  have hbCont : (raw2.bv.setWidth 32 != 0#32) = false := by simp [hContZero]
  rcases hSpec with ⟨hSrc0, rfl⟩ | ⟨schema, hSrc5, hm1, _, _, rfl⟩ |
      ⟨schema, hSrc6, hm1, hm2, _, _, _, _, _, rfl⟩
  · have hs0 : ((raw1.bv >>> 12) &&& 0xF#64).toNat = 0 := by rw [← hSrcVal, hSrc0]; rfl
    dsimp only [lddwInsnOfResolved]
    simp [hbLddw, hbCont, hs0, hTail]
  · have hs5 : ((raw1.bv >>> 12) &&& 0xF#64).toNat = 5 := by rw [← hSrcVal, hSrc5]; rfl
    dsimp only [lddwInsnOfResolved, UScalar.val]
    simp [hbLddw, hbCont, hs5, hm1, hTail]
  · have hs6 : ((raw1.bv >>> 12) &&& 0xF#64).toNat = 6 := by rw [← hSrcVal, hSrc6]; rfl
    dsimp only [lddwInsnOfResolved, UScalar.val]
    simp [hbLddw, hbCont, hs6, hm1, hm2, hTail]

/-- Inductive decomposition of the remaining instruction stream guaranteed by `prescan_step`. -/
inductive PrescanValidTail (cctx : ebpf_core.verifier.CoreCallingContext) :
    List ebpf_core.api.EbpfInstruction → Prop where
  | nil : PrescanValidTail cctx []
  | single (raw : ebpf_core.api.EbpfInstruction) (rest : List ebpf_core.api.EbpfInstruction)
      (hNotLddw : raw.bv.setWidth 8 ≠ 0x18#8)
      (hTail : PrescanValidTail cctx rest) :
      PrescanValidTail cctx (raw :: rest)
  | lddw (raw1 raw2 : ebpf_core.api.EbpfInstruction) (rest : List ebpf_core.api.EbpfInstruction)
      (rlddw : ebpf_core.verifier.ResolvedLddw)
      (hIsLddw : raw1.bv.setWidth 8 = 0x18#8)
      (hContZero : raw2.bv.setWidth 32 = 0#32)
      (hResolve : ebpf_core.verifier.CoreCallingContext.resolve_lddw cctx
        ⟨((raw1.bv >>> 12) &&& 0xF#64).setWidth 8⟩
        (UScalar.hcast .I32 (⟨(raw1.bv >>> 32).setWidth 32⟩ : U32))
        (UScalar.hcast .I32 (⟨(raw2.bv >>> 32).setWidth 32⟩ : U32)) = .ok (.Ok rlddw))
      (hTail : PrescanValidTail cctx rest) :
      PrescanValidTail cctx (raw1 :: raw2 :: rest)

/-- Pointwise lookup into `PrescanValidTail`: any slot `pc` whose opcode is `BPF_LDDW` (`0x18#8`)
    could NOT have been the second slot `raw2` of a preceding `LDDW` (since `raw2` has opcode `0#8`),
    so it MUST be a primary `LDDW` slot with a valid continuation `raw2` at `pc + 1`! -/
theorem PrescanValidTail.lookup_lddw
    {cctx : ebpf_core.verifier.CoreCallingContext}
    {cs : List ebpf_core.api.EbpfInstruction}
    (hPre : PrescanValidTail cctx cs) :
    ∀ (k : Nat) (raw1 : ebpf_core.api.EbpfInstruction),
      cs[k]? = some raw1 →
      raw1.bv.setWidth 8 = 0x18#8 →
      ∃ (raw2 : ebpf_core.api.EbpfInstruction) (rlddw : ebpf_core.verifier.ResolvedLddw),
        cs[k + 1]? = some raw2 ∧
        raw2.bv.setWidth 32 = 0#32 ∧
        ebpf_core.verifier.CoreCallingContext.resolve_lddw cctx
          ⟨((raw1.bv >>> 12) &&& 0xF#64).setWidth 8⟩
          (UScalar.hcast .I32 (⟨(raw1.bv >>> 32).setWidth 32⟩ : U32))
          (UScalar.hcast .I32 (⟨(raw2.bv >>> 32).setWidth 32⟩ : U32)) = .ok (.Ok rlddw) := by
  induction hPre with
  | nil =>
    intro k raw1 hk
    simp at hk
  | single raw rest hNotLddw _ ih =>
    intro k raw1 hk hIsLddw
    cases k with
    | zero =>
      simp only [List.getElem?_cons_zero, Option.some.injEq] at hk
      subst hk
      exact False.elim (hNotLddw hIsLddw)
    | succ k' =>
      simp only [List.getElem?_cons_succ] at hk
      exact ih k' raw1 hk hIsLddw
  | lddw r1 r2 rest rlddw _ hContZero hResolve _ ih =>
    intro k raw1 hk hIsLddw
    cases k with
    | zero =>
      simp only [List.getElem?_cons_zero, Option.some.injEq] at hk
      subst hk
      exact ⟨r2, rlddw, by simp, hContZero, hResolve⟩
    | succ k1 =>
      cases k1 with
      | zero =>
        simp only [List.getElem?_cons_succ, List.getElem?_cons_zero, Option.some.injEq] at hk
        have h0 : r2.bv.setWidth 8 = 0#8 := by
          generalize r2.bv = w at hContZero ⊢
          bv_decide
        rw [← hk, h0] at hIsLddw
        cases hIsLddw
      | succ k2 =>
        simp only [List.getElem?_cons_succ] at hk
        simpa only [List.getElem?_cons_succ] using ih k2 raw1 hk hIsLddw

private theorem list_drop_eq_nil_imp {α : Type u} :
    ∀ (xs : List α) (n : Nat), xs.drop n = [] → xs.length ≤ n
  | [], _, _ => Nat.zero_le _
  | _ :: _, 0, h => by cases h
  | _ :: xs, n + 1, h => Nat.succ_le_succ (list_drop_eq_nil_imp xs n h)

private theorem list_getElem?_imp_drop {α : Type u} :
    ∀ (xs : List α) (n : Nat) (x : α),
      xs[n]? = some x → xs.drop n = x :: xs.drop (n + 1)
  | [], _, _, h => by simp at h
  | _ :: _, 0, _, h => by
    simp only [List.getElem?_cons_zero, Option.some.injEq] at h
    subst h; rfl
  | _ :: xs, n + 1, x, h => by
    simp only [List.getElem?_cons_succ] at h
    simpa only [List.drop_succ_cons] using list_getElem?_imp_drop xs n x h

private theorem list_drop_cons_imp {α : Type u} :
    ∀ (xs : List α) (n : Nat) (x : α) (rest : List α),
      xs.drop n = x :: rest →
      n < xs.length ∧ xs[n]? = some x ∧ xs.drop (n + 1) = rest
  | [], _, _, _, h => by simp at h
  | _ :: xs, 0, _, _, h => by
    simp only [List.drop_zero, List.cons.injEq] at h
    rcases h with ⟨rfl, rfl⟩
    exact ⟨Nat.zero_lt_succ _, rfl, by simp⟩
  | _ :: xs, n + 1, x, rest, h => by
    simp only [List.drop_succ_cons] at h
    rcases list_drop_cons_imp xs n x rest h with ⟨hLt, hGet, hRest⟩
    exact ⟨Nat.succ_lt_succ hLt, by simpa using hGet, by simpa using hRest⟩

private theorem list_drop_set_gt {α : Type u} :
    ∀ (xs : List α) (i n : Nat) (v : α),
      i < n → (xs.set i v).drop n = xs.drop n
  | [], _, _, _, _ => rfl
  | _ :: _, 0, 0, _, h => by omega
  | _ :: _, 0, _ + 1, _, _ => rfl
  | _ :: _, _ + 1, 0, _, h => by omega
  | _ :: xs, i + 1, n + 1, v, h =>
    list_drop_set_gt xs i n v (Nat.lt_of_succ_lt_succ h)

private theorem list_getElem?_set_self {α : Type u} :
    ∀ (xs : List α) (i : Nat) (v : α),
      i < xs.length → (xs.set i v)[i]? = some v
  | [], _, _, h => by simp at h
  | _ :: _, 0, _, _ => rfl
  | _ :: xs, i + 1, v, h =>
    list_getElem?_set_self xs i v (Nat.lt_of_succ_lt_succ h)

private theorem list_getElem?_set_ne {α : Type u} :
    ∀ (xs : List α) (i j : Nat) (v : α),
      i ≠ j → (xs.set i v)[j]? = xs[j]?
  | [], _, _, _, _ => rfl
  | _ :: _, 0, 0, _, h => False.elim (h rfl)
  | _ :: _, 0, _ + 1, _, _ => rfl
  | _ :: _, _ + 1, 0, _, _ => rfl
  | _ :: xs, i + 1, j + 1, v, h =>
    list_getElem?_set_ne xs i j v (fun hc => h (congrArg Nat.succ hc))

theorem decodeVerifiedCodeList_length :
    ∀ (raw : List ebpf_core.api.EbpfInstruction) (dec : List Insn),
      decodeVerifiedCodeList raw = some dec → dec.length = raw.length
  | [], dec, h => by
    simp [decodeVerifiedCodeList] at h
    subst h; rfl
  | raw1 :: rest, dec, h => by
    unfold decodeVerifiedCodeList at h
    dsimp only at h
    split at h
    · cases rest with
      | nil => simp at h
      | cons raw2 tail =>
        dsimp only at h
        split at h
        · simp at h
        · split at h
          · rename_i _ decodedTail _ hTail
            simp only [Option.some.injEq] at h
            subst h
            have ih := decodeVerifiedCodeList_length tail decodedTail hTail
            simp [ih]
          · simp at h
    · split at h
      · rename_i _ decodedTail _ hTail
        simp only [Option.some.injEq] at h
        subst h
        have ih := decodeVerifiedCodeList_length rest decodedTail hTail
        simp [ih]
      · simp at h
termination_by raw => raw.length

private theorem apply_resultOkPost {α : Type u} {m : Result α} {y : α}
    (P : α → Prop) (hm : m = .ok y) (hPost : ResultOkPost m P) : P y :=
  hPost y hm

section PrescanStepSpec

private theorem prescan_mark_cmpxchg_spec
    (is_cmpxchg : Bool) (scan_pc code_len adv : Usize) (pp pp' : alloc.vec.Vec Bool)
    (h : (if is_cmpxchg = true then
            do
              let i4 ← scan_pc + 1#usize
              if i4 < code_len then
                do
                  let (_, index_mut_back) ←
                    alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice Bool) pp i4
                  ok (core.result.Result.Ok 1#usize, index_mut_back true)
              else ok (core.result.Result.Ok 1#usize, pp)
          else ok (core.result.Result.Ok 1#usize, pp)) =
            (.ok (.Ok adv, pp') : Result (core.result.Result Usize ebpf_core.error.VerifierErrorKind × alloc.vec.Vec Bool))) :
    pp'.val.length = pp.val.length ∧ adv = 1#usize := by
  cases is_cmpxchg
  · simp only [Bool.false_eq_true, ↓reduceIte] at h
    have hp := Prod.mk.inj (ok_inj h)
    exact ⟨hp.2 ▸ rfl, (core.result.Result.Ok.inj hp.1).symm⟩
  · simp only [↓reduceIte] at h
    rcases bind_eq_ok h with ⟨i4, _, h⟩
    by_cases hlt : i4 < code_len
    · simp only [hlt, ↓reduceIte] at h
      rcases bind_eq_ok h with ⟨⟨_, back⟩, hMut, h⟩
      have ⟨_, _, hBack⟩ := vec_index_mut_usize_ok hMut
      have hp := Prod.mk.inj (ok_inj h)
      exact ⟨hp.2 ▸ (hBack true).2, (core.result.Result.Ok.inj hp.1).symm⟩
    · simp only [hlt, ↓reduceIte] at h
      have hp := Prod.mk.inj (ok_inj h)
      exact ⟨hp.2 ▸ rfl, (core.result.Result.Ok.inj hp.1).symm⟩

private theorem prescan_jmp_branch_spec
    (op : U8) (inst : ebpf_core.api.EbpfInstruction) (is_cmpxchg : Bool)
    (scan_pc code_len adv : Usize) (pp pp' : alloc.vec.Vec Bool)
    (h : (if (op != ebpf_core.api.BPF_CALL) = true then
            if (op != ebpf_core.api.BPF_EXIT) = true then
              do
                let i4 ← ebpf_core.api.EbpfInstruction.offset inst
                let i5 ← lift (IScalar.cast .Isize i4)
                let i6 ← i5 + 1#isize
                let o ← ebpf_core.verifier.checked_add_signed_usize scan_pc i6
                let is_prune_point1 ←
                  match o with
                  | none => ok pp
                  | some target_pc =>
                    if target_pc < code_len then
                      do
                        let (_, index_mut_back) ←
                          alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice Bool) pp target_pc
                        ok (index_mut_back true)
                    else ok pp
                if (op != ebpf_core.api.BPF_JA) = true then
                  do
                    let i7 ← scan_pc + 1#usize
                    if i7 < code_len then
                      do
                        let (_, index_mut_back) ←
                          alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice Bool) is_prune_point1 i7
                        ok (core.result.Result.Ok 1#usize, index_mut_back true)
                    else ok (core.result.Result.Ok 1#usize, is_prune_point1)
                else ok (core.result.Result.Ok 1#usize, is_prune_point1)
            else
              if is_cmpxchg = true then
                do
                  let i4 ← scan_pc + 1#usize
                  if i4 < code_len then
                    do
                      let (_, index_mut_back) ←
                        alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice Bool) pp i4
                      ok (core.result.Result.Ok 1#usize, index_mut_back true)
                  else ok (core.result.Result.Ok 1#usize, pp)
              else ok (core.result.Result.Ok 1#usize, pp)
          else
            if is_cmpxchg = true then
              do
                let i4 ← scan_pc + 1#usize
                if i4 < code_len then
                  do
                    let (_, index_mut_back) ←
                      alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice Bool) pp i4
                    ok (core.result.Result.Ok 1#usize, index_mut_back true)
                else ok (core.result.Result.Ok 1#usize, pp)
            else ok (core.result.Result.Ok 1#usize, pp)) =
            (.ok (.Ok adv, pp') : Result (core.result.Result Usize ebpf_core.error.VerifierErrorKind × alloc.vec.Vec Bool))) :
    pp'.val.length = pp.val.length ∧ adv = 1#usize := by
  by_cases hc1 : (op != ebpf_core.api.BPF_CALL) = true
  · simp only [hc1, ↓reduceIte] at h
    by_cases hc2 : (op != ebpf_core.api.BPF_EXIT) = true
    · simp only [hc2, ↓reduceIte] at h
      rcases bind_eq_ok h with ⟨i4, _, h⟩
      rcases bind_eq_ok h with ⟨i5, _, h⟩
      rcases bind_eq_ok h with ⟨i6, _, h⟩
      rcases bind_eq_ok h with ⟨o, _, h⟩
      rcases bind_eq_ok h with ⟨pp1, hPp1, h⟩
      have hLen1 : pp1.val.length = pp.val.length := by
        cases o with
        | none => exact (ok_inj hPp1) ▸ rfl
        | some target_pc =>
          dsimp only at hPp1
          by_cases ht : target_pc < code_len
          · simp only [ht, ↓reduceIte] at hPp1
            rcases bind_eq_ok hPp1 with ⟨⟨_, back⟩, hMut, hPp1⟩
            have ⟨_, _, hBack⟩ := vec_index_mut_usize_ok hMut
            exact (ok_inj hPp1) ▸ (hBack true).2
          · simp only [ht, ↓reduceIte] at hPp1
            exact (ok_inj hPp1) ▸ rfl
      by_cases hc3 : (op != ebpf_core.api.BPF_JA) = true
      · simp only [hc3, ↓reduceIte] at h
        rcases bind_eq_ok h with ⟨i7, _, h⟩
        by_cases hi7 : i7 < code_len
        · simp only [hi7, ↓reduceIte] at h
          rcases bind_eq_ok h with ⟨⟨_, back⟩, hMut, h⟩
          have ⟨_, _, hBack⟩ := vec_index_mut_usize_ok hMut
          have hp := Prod.mk.inj (ok_inj h)
          exact ⟨(hp.2 ▸ (hBack true).2).trans hLen1, (core.result.Result.Ok.inj hp.1).symm⟩
        · simp only [hi7, ↓reduceIte] at h
          have hp := Prod.mk.inj (ok_inj h)
          exact ⟨hp.2.symm ▸ hLen1, (core.result.Result.Ok.inj hp.1).symm⟩
      · simp only [hc3] at h
        have hp := Prod.mk.inj (ok_inj h)
        exact ⟨hp.2.symm ▸ hLen1, (core.result.Result.Ok.inj hp.1).symm⟩
    · simp only [hc2] at h
      exact prescan_mark_cmpxchg_spec is_cmpxchg scan_pc code_len adv pp pp' h
  · simp only [hc1] at h
    exact prescan_mark_cmpxchg_spec is_cmpxchg scan_pc code_len adv pp pp' h

private theorem prescan_after_cmpxchg_spec
    (cls op : U8) (inst : ebpf_core.api.EbpfInstruction) (is_cmpxchg : Bool)
    (scan_pc code_len adv : Usize) (pp pp' : alloc.vec.Vec Bool)
    (h : (if cls = ebpf_core.api.BPF_JMP then
            if (op != ebpf_core.api.BPF_CALL) = true then
              if (op != ebpf_core.api.BPF_EXIT) = true then
                do
                  let i4 ← ebpf_core.api.EbpfInstruction.offset inst
                  let i5 ← lift (IScalar.cast .Isize i4)
                  let i6 ← i5 + 1#isize
                  let o ← ebpf_core.verifier.checked_add_signed_usize scan_pc i6
                  let is_prune_point1 ←
                    match o with
                    | none => ok pp
                    | some target_pc =>
                      if target_pc < code_len then
                        do
                          let (_, index_mut_back) ←
                            alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice Bool) pp target_pc
                          ok (index_mut_back true)
                      else ok pp
                  if (op != ebpf_core.api.BPF_JA) = true then
                    do
                      let i7 ← scan_pc + 1#usize
                      if i7 < code_len then
                        do
                          let (_, index_mut_back) ←
                            alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice Bool) is_prune_point1 i7
                          ok (core.result.Result.Ok 1#usize, index_mut_back true)
                      else ok (core.result.Result.Ok 1#usize, is_prune_point1)
                  else ok (core.result.Result.Ok 1#usize, is_prune_point1)
              else
                if is_cmpxchg = true then
                  do
                    let i4 ← scan_pc + 1#usize
                    if i4 < code_len then
                      do
                        let (_, index_mut_back) ←
                          alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice Bool) pp i4
                        ok (core.result.Result.Ok 1#usize, index_mut_back true)
                    else ok (core.result.Result.Ok 1#usize, pp)
                else ok (core.result.Result.Ok 1#usize, pp)
            else
              if is_cmpxchg = true then
                do
                  let i4 ← scan_pc + 1#usize
                  if i4 < code_len then
                    do
                      let (_, index_mut_back) ←
                        alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice Bool) pp i4
                      ok (core.result.Result.Ok 1#usize, index_mut_back true)
                  else ok (core.result.Result.Ok 1#usize, pp)
              else ok (core.result.Result.Ok 1#usize, pp)
          else
            if cls = ebpf_core.api.BPF_JMP32 then
              if (op != ebpf_core.api.BPF_CALL) = true then
                if (op != ebpf_core.api.BPF_EXIT) = true then
                  do
                    let i4 ← ebpf_core.api.EbpfInstruction.offset inst
                    let i5 ← lift (IScalar.cast .Isize i4)
                    let i6 ← i5 + 1#isize
                    let o ← ebpf_core.verifier.checked_add_signed_usize scan_pc i6
                    let is_prune_point1 ←
                      match o with
                      | none => ok pp
                      | some target_pc =>
                        if target_pc < code_len then
                          do
                            let (_, index_mut_back) ←
                              alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice Bool) pp target_pc
                            ok (index_mut_back true)
                        else ok pp
                    if (op != ebpf_core.api.BPF_JA) = true then
                      do
                        let i7 ← scan_pc + 1#usize
                        if i7 < code_len then
                          do
                            let (_, index_mut_back) ←
                              alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice Bool) is_prune_point1 i7
                            ok (core.result.Result.Ok 1#usize, index_mut_back true)
                        else ok (core.result.Result.Ok 1#usize, is_prune_point1)
                    else ok (core.result.Result.Ok 1#usize, is_prune_point1)
                else
                  if is_cmpxchg = true then
                    do
                      let i4 ← scan_pc + 1#usize
                      if i4 < code_len then
                        do
                          let (_, index_mut_back) ←
                            alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice Bool) pp i4
                          ok (core.result.Result.Ok 1#usize, index_mut_back true)
                      else ok (core.result.Result.Ok 1#usize, pp)
                  else ok (core.result.Result.Ok 1#usize, pp)
              else
                if is_cmpxchg = true then
                  do
                    let i4 ← scan_pc + 1#usize
                    if i4 < code_len then
                      do
                        let (_, index_mut_back) ←
                          alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice Bool) pp i4
                        ok (core.result.Result.Ok 1#usize, index_mut_back true)
                    else ok (core.result.Result.Ok 1#usize, pp)
                else ok (core.result.Result.Ok 1#usize, pp)
            else
              if is_cmpxchg = true then
                do
                  let i4 ← scan_pc + 1#usize
                  if i4 < code_len then
                    do
                      let (_, index_mut_back) ←
                        alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice Bool) pp i4
                      ok (core.result.Result.Ok 1#usize, index_mut_back true)
                  else ok (core.result.Result.Ok 1#usize, pp)
              else ok (core.result.Result.Ok 1#usize, pp)) =
            (.ok (.Ok adv, pp') : Result (core.result.Result Usize ebpf_core.error.VerifierErrorKind × alloc.vec.Vec Bool))) :
    pp'.val.length = pp.val.length ∧ adv = 1#usize := by
  by_cases hj1 : cls = ebpf_core.api.BPF_JMP
  · simp only [hj1, ↓reduceIte] at h
    exact prescan_jmp_branch_spec op inst is_cmpxchg scan_pc code_len adv pp pp' h
  · simp only [hj1, ↓reduceIte] at h
    by_cases hj2 : cls = ebpf_core.api.BPF_JMP32
    · simp only [hj2, ↓reduceIte] at h
      exact prescan_jmp_branch_spec op inst is_cmpxchg scan_pc code_len adv pp pp' h
    · simp only [hj2, ↓reduceIte] at h
      exact prescan_mark_cmpxchg_spec is_cmpxchg scan_pc code_len adv pp pp' h

theorem prescan_step_ok_spec
    (code : Slice ebpf_core.api.EbpfInstruction) (scan_pc : Usize)
    (cctx : ebpf_core.verifier.CoreCallingContext)
    (pp pp' : alloc.vec.Vec Bool) (adv : Usize)
    (hStep : ebpf_core.verifier.prescan_step code scan_pc cctx pp = .ok (.Ok adv, pp')) :
    pp'.val.length = pp.val.length ∧
    ((adv = 1#usize ∧ ∃ raw1,
        code.val[scan_pc.val]? = some raw1 ∧
        raw1.bv.setWidth 8 ≠ 0x18#8) ∨
     (∃ raw1 raw2 rlddw,
        adv = 2#usize ∧
        code.val[scan_pc.val]? = some raw1 ∧
        code.val[scan_pc.val + 1]? = some raw2 ∧
        raw1.bv.setWidth 8 = 0x18#8 ∧
        raw2.bv.setWidth 32 = 0#32 ∧
        ebpf_core.verifier.CoreCallingContext.resolve_lddw cctx
          ⟨((raw1.bv >>> 12) &&& 0xF#64).setWidth 8⟩
          (UScalar.hcast .I32 (⟨(raw1.bv >>> 32).setWidth 32⟩ : U32))
          (UScalar.hcast .I32 (⟨(raw2.bv >>> 32).setWidth 32⟩ : U32)) =
            .ok (.Ok rlddw))) := by
  unfold ebpf_core.verifier.prescan_step at hStep
  dsimp (config := { zetaHave := true }) only at hStep
  rcases bind_eq_ok hStep with ⟨inst, hIdx, hStep⟩
  have ⟨_, hGet0⟩ := slice_index_usize_ok hIdx
  rw [bind_ok_eq (ebpf_code_eq_ok inst)] at hStep
  by_cases hIsLddwU8 : (⟨inst.bv.setWidth 8⟩ : U8) = ebpf_core.api.BPF_LDDW
  · -- BPF_LDDW branch
    simp only [hIsLddwU8, ↓reduceIte] at hStep
    have hIsLddw : inst.bv.setWidth 8 = 0x18#8 := by
      rw [bpf_lddw_eq] at hIsLddwU8
      exact congrArg UScalar.bv hIsLddwU8
    rcases bind_eq_ok hStep with ⟨pc1, hPc1Add, hStep⟩
    have hPc1Val : pc1.val = scan_pc.val + 1 := by
      simpa [usize_one_val] using (uscalar_add_ok_val hPc1Add).1
    by_cases hGeLen : pc1 >= Slice.len code
    · simp only [hGeLen, ↓reduceIte] at hStep
      cases (Prod.mk.inj (ok_inj hStep)).1
    · simp only [hGeLen, ↓reduceIte] at hStep
      rcases bind_eq_ok hStep with ⟨next_inst, hNextIdx, hStep⟩
      have ⟨_, hGet1⟩ := slice_index_usize_ok hNextIdx
      rw [hPc1Val] at hGet1
      rw [bind_ok_eq (ebpf_code_eq_ok next_inst)] at hStep
      by_cases hCode0 : (⟨next_inst.bv.setWidth 8⟩ : U8) != 0#u8
      · simp only [hCode0, ↓reduceIte] at hStep
        cases (Prod.mk.inj (ok_inj hStep)).1
      · simp only [hCode0, Bool.false_eq_true, ↓reduceIte] at hStep
        rw [bind_ok_eq (ebpf_offset_eq_ok next_inst)] at hStep
        by_cases hOff0 : UScalar.hcast .I16 (⟨(next_inst.bv >>> 16).setWidth 16⟩ : U16) != 0#i16
        · simp only [hOff0, ↓reduceIte] at hStep
          cases (Prod.mk.inj (ok_inj hStep)).1
        · simp only [hOff0, Bool.false_eq_true, ↓reduceIte] at hStep
          rw [bind_ok_eq (ebpf_src_reg_eq_ok next_inst)] at hStep
          by_cases hSrc0 : (⟨((next_inst.bv >>> 12) &&& 0xF#64).setWidth 8⟩ : U8) != 0#u8
          · simp only [hSrc0, ↓reduceIte] at hStep
            cases (Prod.mk.inj (ok_inj hStep)).1
          · simp only [hSrc0, Bool.false_eq_true, ↓reduceIte] at hStep
            rw [bind_ok_eq (ebpf_dst_reg_eq_ok next_inst)] at hStep
            by_cases hDst0 : (⟨((next_inst.bv >>> 8) &&& 0xF#64).setWidth 8⟩ : U8) != 0#u8
            · simp only [hDst0, ↓reduceIte] at hStep
              cases (Prod.mk.inj (ok_inj hStep)).1
            · simp only [hDst0, Bool.false_eq_true, ↓reduceIte] at hStep
              rw [bind_ok_eq (ebpf_src_reg_eq_ok inst),
                  bind_ok_eq (ebpf_imm_eq_ok inst),
                  bind_ok_eq (ebpf_imm_eq_ok next_inst)] at hStep
              rcases bind_eq_ok hStep with ⟨r, hRes, hStep⟩
              cases r with
              | Err err =>
                cases (Prod.mk.inj (ok_inj hStep)).1
              | Ok rlddw =>
                have hPair := Prod.mk.inj (ok_inj hStep)
                have hAdv : adv = 2#usize := (core.result.Result.Ok.inj hPair.1).symm
                have hc0 : (⟨next_inst.bv.setWidth 8⟩ : U8) = 0#u8 := by
                  by_contra hne; exact hCode0 (bne_iff_ne.mpr hne)
                have ho0 : UScalar.hcast .I16 (⟨(next_inst.bv >>> 16).setWidth 16⟩ : U16) = 0#i16 := by
                  by_contra hne; exact hOff0 (bne_iff_ne.mpr hne)
                have hs0 : (⟨((next_inst.bv >>> 12) &&& 0xF#64).setWidth 8⟩ : U8) = 0#u8 := by
                  by_contra hne; exact hSrc0 (bne_iff_ne.mpr hne)
                have hd0 : (⟨((next_inst.bv >>> 8) &&& 0xF#64).setWidth 8⟩ : U8) = 0#u8 := by
                  by_contra hne; exact hDst0 (bne_iff_ne.mpr hne)
                have hZero32 := lddw_cont_zero_low32 next_inst hc0 ho0 hs0 hd0
                exact ⟨hPair.2 ▸ rfl, Or.inr ⟨inst, next_inst, rlddw, hAdv, hGet0, hGet1,
                  hIsLddw, hZero32, hRes⟩⟩
  · -- Non-BPF_LDDW branch
    simp only [hIsLddwU8, ↓reduceIte] at hStep
    have hNotLddw : inst.bv.setWidth 8 ≠ 0x18#8 := by
      rw [bpf_lddw_eq] at hIsLddwU8
      intro hc
      exact hIsLddwU8 (U8.bv_eq_imp_eq _ _ hc)
    have ⟨hPpLen, hAdv⟩ : pp'.val.length = pp.val.length ∧ adv = 1#usize := by
      rcases bind_eq_ok hStep with ⟨i1, _, hStep⟩
      by_cases hCall : (⟨inst.bv.setWidth 8⟩ : U8) = i1
      · simp only [hCall, ↓reduceIte] at hStep
        rcases bind_eq_ok hStep with ⟨i2, _, hStep⟩
        by_cases hSrc : (i2 != 0#u8) = true
        · simp only [hSrc, ↓reduceIte] at hStep
          cases (Prod.mk.inj (ok_inj hStep)).1
        · simp only [hSrc] at hStep
          rcases bind_eq_ok hStep with ⟨cls, _, hStep⟩
          rcases bind_eq_ok hStep with ⟨op, _, hStep⟩
          rcases bind_eq_ok hStep with ⟨i3, _, hStep⟩
          rcases bind_eq_ok hStep with ⟨i4, _, hStep⟩
          rcases bind_eq_ok hStep with ⟨is_cmpxchg, _, hStep⟩
          exact prescan_after_cmpxchg_spec cls op inst is_cmpxchg scan_pc (Slice.len code) adv pp pp' hStep
      · simp only [hCall, ↓reduceIte] at hStep
        rcases bind_eq_ok hStep with ⟨cls, _, hStep⟩
        rcases bind_eq_ok hStep with ⟨op, _, hStep⟩
        rcases bind_eq_ok hStep with ⟨i2, _, hStep⟩
        rcases bind_eq_ok hStep with ⟨i3, _, hStep⟩
        rcases bind_eq_ok hStep with ⟨is_cmpxchg, _, hStep⟩
        exact prescan_after_cmpxchg_spec cls op inst is_cmpxchg scan_pc (Slice.len code) adv pp pp' hStep
    exact ⟨hPpLen, Or.inl ⟨hAdv, inst, hGet0, hNotLddw⟩⟩

end PrescanStepSpec

theorem verify_program_core_loop0_spec
    (code_len : Usize) (pp : alloc.vec.Vec Bool) (i : Usize) (ppOut : alloc.vec.Vec Bool)
    (hPre : pp.val.length = i.val ∧ i.val ≤ code_len.val)
    (hLoop : ebpf_core.verifier.verify_program_core_loop0 code_len pp i = .ok ppOut) :
    ppOut.val.length = code_len.val := by
  unfold ebpf_core.verifier.verify_program_core_loop0 at hLoop
  refine loop_ok_induct
    (fun (pp1, i1) => ebpf_core.verifier.verify_program_core_loop0.body code_len pp1 i1)
    (fun (pp1, i1) => pp1.val.length = i1.val ∧ i1.val ≤ code_len.val)
    (fun out => out.val.length = code_len.val)
    ?_ (pp, i) ppOut hPre hLoop
  rintro ⟨pp1, i1⟩ ⟨hLen, hLe⟩
  unfold ebpf_core.verifier.verify_program_core_loop0.body
  dsimp (config := { zetaHave := true }) only at hLen hLe ⊢
  constructor
  · rintro ⟨pp2, i2⟩ hBody
    split at hBody
    · rename_i hlt
      rcases bind_eq_ok hBody with ⟨pp2', hPush, hBody⟩
      rcases bind_eq_ok hBody with ⟨i2', hAdd, hBody⟩
      have hPushLen := vec_push_ok_len hPush
      have hAddVal : i2'.val = i1.val + 1 := by
        simpa [usize_one_val] using (uscalar_add_ok_val hAdd).1
      have hltV : i1.val < code_len.val := (UScalar.lt_equiv _ _).mp hlt
      cases ok_inj hBody
      dsimp only
      omega
    · simp at hBody
  · intro out hBody
    split at hBody
    · rcases bind_eq_ok hBody with ⟨_, _, hBody⟩
      rcases bind_eq_ok hBody with ⟨_, _, hBody⟩
      simp at hBody
    · rename_i hnlt
      cases ok_inj hBody
      have hge : code_len.val ≤ i1.val :=
        not_lt.mp (fun hc => hnlt ((UScalar.lt_equiv _ _).mpr hc))
      omega

theorem verify_program_core_loop4_spec
    (code_len : Usize) (vis : alloc.vec.Vec Bool) (i : Usize) (visOut : alloc.vec.Vec Bool)
    (hPre : vis.val.length = i.val ∧ i.val ≤ code_len.val)
    (hLoop : ebpf_core.verifier.verify_program_core_loop4 code_len vis i = .ok visOut) :
    visOut.val.length = code_len.val ∧ (∀ b ∈ visOut.val, (∀ b0 ∈ vis.val, b0 = false) → b = false) := by
  unfold ebpf_core.verifier.verify_program_core_loop4 at hLoop
  refine loop_ok_induct
    (fun (v1, i1) => ebpf_core.verifier.verify_program_core_loop4.body code_len v1 i1)
    (fun (v1, i1) =>
      v1.val.length = i1.val ∧ i1.val ≤ code_len.val ∧
      ((∀ b0 ∈ vis.val, b0 = false) → ∀ b ∈ v1.val, b = false))
    (fun out =>
      out.val.length = code_len.val ∧
      (∀ b ∈ out.val, (∀ b0 ∈ vis.val, b0 = false) → b = false))
    ?_ (vis, i) visOut ⟨hPre.1, hPre.2, fun h0 b hb => h0 b hb⟩ hLoop
  rintro ⟨v1, i1⟩ ⟨hLen, hLe, hFalse⟩
  unfold ebpf_core.verifier.verify_program_core_loop4.body
  dsimp (config := { zetaHave := true }) only at hLen hLe hFalse ⊢
  constructor
  · rintro ⟨v2, i2⟩ hBody
    split at hBody
    · rename_i hlt
      rcases bind_eq_ok hBody with ⟨v2', hPush, hBody⟩
      rcases bind_eq_ok hBody with ⟨i2', hAdd, hBody⟩
      have hPushLen := vec_push_ok_len hPush
      have hPushVal := vec_push_ok_val hPush
      have hAddVal : i2'.val = i1.val + 1 := by
        simpa [usize_one_val] using (uscalar_add_ok_val hAdd).1
      have hltV : i1.val < code_len.val := (UScalar.lt_equiv _ _).mp hlt
      cases ok_inj hBody
      dsimp only
      refine ⟨by omega, by omega, fun h0 b hb => ?_⟩
      rw [hPushVal, List.mem_append, List.mem_singleton] at hb
      rcases hb with hb1 | rfl
      · exact hFalse h0 b hb1
      · rfl
    · simp at hBody
  · intro out hBody
    split at hBody
    · rcases bind_eq_ok hBody with ⟨_, _, hBody⟩
      rcases bind_eq_ok hBody with ⟨_, _, hBody⟩
      simp at hBody
    · rename_i hnlt
      cases ok_inj hBody
      have hge : code_len.val ≤ i1.val :=
        not_lt.mp (fun hc => hnlt ((UScalar.lt_equiv _ _).mpr hc))
      exact ⟨by omega, fun b hb h0 => hFalse h0 b hb⟩

theorem verify_program_core_loop2_spec
    (code_len : Usize) (pp : alloc.vec.Vec Bool)
    (tc : alloc.vec.Vec (Option ebpf_core.verifier.TerminatingContexts))
    (i : Usize)
    (tcOut : alloc.vec.Vec (Option ebpf_core.verifier.TerminatingContexts))
    (hPre : tc.val.length = i.val ∧ i.val ≤ code_len.val ∧
      (∀ opt ∈ tc.val, ∀ t, opt = some t → t.contexts.val = []))
    (hLoop : ebpf_core.verifier.verify_program_core_loop2 code_len pp tc i = .ok tcOut) :
    tcOut.val.length = code_len.val ∧
    (∀ opt ∈ tcOut.val, ∀ t, opt = some t → t.contexts.val = []) := by
  unfold ebpf_core.verifier.verify_program_core_loop2 at hLoop
  refine loop_ok_induct
    (fun (tc1, i1) => ebpf_core.verifier.verify_program_core_loop2.body code_len pp tc1 i1)
    (fun (tc1, i1) =>
      tc1.val.length = i1.val ∧ i1.val ≤ code_len.val ∧
      (∀ opt ∈ tc1.val, ∀ t, opt = some t → t.contexts.val = []))
    (fun out =>
      out.val.length = code_len.val ∧
      (∀ opt ∈ out.val, ∀ t, opt = some t → t.contexts.val = []))
    ?_ (tc, i) tcOut hPre hLoop
  rintro ⟨tc1, i1⟩ ⟨hLen, hLe, hEmpty⟩
  unfold ebpf_core.verifier.verify_program_core_loop2.body
  dsimp (config := { zetaHave := true }) only at hLen hLe hEmpty ⊢
  constructor
  · rintro ⟨tc2, i2⟩ hBody
    split at hBody
    · rename_i hlt
      rcases bind_eq_ok hBody with ⟨b, _, hBody⟩
      have hltV : i1.val < code_len.val := (UScalar.lt_equiv _ _).mp hlt
      cases b with
      | true =>
        simp only [↓reduceIte, ebpf_core.verifier.TerminatingContexts.Insts.CoreDefaultDefault.default,
          bind_ok] at hBody
        rcases bind_eq_ok hBody with ⟨tc2', hPush, hBody⟩
        rcases bind_eq_ok hBody with ⟨i2', hAdd, hBody⟩
        have hPushLen := vec_push_ok_len hPush
        have hPushVal := vec_push_ok_val hPush
        have hAddVal : i2'.val = i1.val + 1 := by
          simpa [usize_one_val] using (uscalar_add_ok_val hAdd).1
        cases ok_inj hBody
        dsimp only
        refine ⟨by omega, by omega, fun opt hMem t hSome => ?_⟩
        rw [hPushVal, List.mem_append, List.mem_singleton] at hMem
        rcases hMem with hMem1 | rfl
        · exact hEmpty opt hMem1 t hSome
        · cases Option.some.inj hSome; rfl
      | false =>
        simp only [Bool.false_eq_true, ↓reduceIte] at hBody
        rcases bind_eq_ok hBody with ⟨tc2', hPush, hBody⟩
        rcases bind_eq_ok hBody with ⟨i2', hAdd, hBody⟩
        have hPushLen := vec_push_ok_len hPush
        have hPushVal := vec_push_ok_val hPush
        have hAddVal : i2'.val = i1.val + 1 := by
          simpa [usize_one_val] using (uscalar_add_ok_val hAdd).1
        cases ok_inj hBody
        dsimp only
        refine ⟨by omega, by omega, fun opt hMem t hSome => ?_⟩
        rw [hPushVal, List.mem_append, List.mem_singleton] at hMem
        rcases hMem with hMem1 | rfl
        · exact hEmpty opt hMem1 t hSome
        · cases hSome
    · simp at hBody
  · intro out hBody
    split at hBody
    · rcases bind_eq_ok hBody with ⟨b, _, hBody⟩
      cases b <;> simp [ebpf_core.verifier.TerminatingContexts.Insts.CoreDefaultDefault.default] at hBody
      · rcases bind_eq_ok hBody with ⟨_, _, hBody⟩
        rcases bind_eq_ok hBody with ⟨_, _, hBody⟩
        simp at hBody
      · rcases bind_eq_ok hBody with ⟨_, _, hBody⟩
        rcases bind_eq_ok hBody with ⟨_, _, hBody⟩
        simp at hBody
    · rename_i hnlt
      cases ok_inj hBody
      have hge : code_len.val ≤ i1.val :=
        not_lt.mp (fun hc => hnlt ((UScalar.lt_equiv _ _).mpr hc))
      exact ⟨by omega, hEmpty⟩

theorem verify_program_core_loop1_spec
    (code : alloc.vec.Vec ebpf_core.api.EbpfInstruction)
    (cctx : ebpf_core.verifier.CoreCallingContext)
    (code_len : Usize) (pp : alloc.vec.Vec Bool)
    (scan_err : Option ebpf_core.error.VerifierError) (scan_pc : Usize)
    (ppOut : alloc.vec.Vec Bool)
    (hCodeLen : code_len = alloc.vec.Vec.len code)
    (hPre : scan_err = none → pp.val.length = code.val.length ∧ scan_pc.val ≤ code.val.length)
    (hLoop : ebpf_core.verifier.verify_program_core_loop1 code cctx code_len pp scan_err scan_pc =
      .ok (ppOut, none)) :
    ppOut.val.length = code.val.length ∧
    PrescanValidTail cctx (code.val.drop scan_pc.val) := by
  unfold ebpf_core.verifier.verify_program_core_loop1 at hLoop
  have hRel := loop_ok_pre_post
    (fun (pp1, err1, pc1) =>
      ebpf_core.verifier.verify_program_core_loop1.body code cctx code_len pp1 err1 pc1)
    (fun (pp1, err1, pc1) =>
      err1 = none → pp1.val.length = code.val.length ∧ pc1.val ≤ code.val.length)
    (fun (_pp1, err1, pc1) (outPp, outErr) =>
      (err1 ≠ none → outErr ≠ none) ∧
      (outErr = none → outPp.val.length = code.val.length ∧
        PrescanValidTail cctx (code.val.drop pc1.val)))
    (by
      rintro ⟨pp1, err1, pc1⟩ ⟨pp2, err2, pc2⟩ ⟨outPp, outErr⟩ hPre1 hBody
      unfold ebpf_core.verifier.verify_program_core_loop1.body at hBody
      dsimp (config := { zetaHave := true }) only at hPre1 hBody ⊢
      split at hBody
      · rename_i hPcLtU
        split at hBody
        · rename_i hIsNone
          cases err1 with
          | some _ => simp [core.option.Option.is_none] at hIsNone
          | none =>
            have ⟨hPpLen, _⟩ := hPre1 rfl
            rcases bind_eq_ok hBody with ⟨⟨r, ppNext⟩, hStep, hBody⟩
            cases r with
            | Err kind =>
              cases ok_inj hBody
              refine ⟨(fun h => by cases h), fun hIH => ⟨fun h => False.elim (h rfl), fun hOutNone => ?_⟩⟩
              exact False.elim ((hIH.1 (by simp)) hOutNone)
            | Ok adv =>
              rcases bind_eq_ok hBody with ⟨pcNext, hAdd, hBody⟩
              have hAddVal : pcNext.val = pc1.val + adv.val := (uscalar_add_ok_val hAdd).1
              have ⟨hPpNextLen, hCases⟩ := prescan_step_ok_spec code.deref pc1 cctx pp1 ppNext adv hStep
              have hDeref : (alloc.vec.Vec.deref code).val = code.val := by
                simp [alloc.vec.Vec.deref]
              rw [hDeref] at hCases
              cases ok_inj hBody
              rcases hCases with ⟨rfl, raw1, hGet0, hNotLddw⟩ |
                  ⟨raw1, raw2, rlddw, rfl, hGet0, hGet1, hIsLddw, hZero32, hRes⟩
              · rw [usize_one_val] at hAddVal
                have hLt0 : pc1.val < code.val.length := (List.getElem?_eq_some_iff.mp hGet0).1
                have hDrop := list_getElem?_imp_drop code.val pc1.val raw1 hGet0
                refine ⟨fun _ => ⟨hPpNextLen.trans hPpLen, by omega⟩,
                  fun hIH => ⟨fun h => False.elim (h rfl), fun hOutNone => ?_⟩⟩
                have ⟨hOutLen, hTail⟩ := hIH.2 hOutNone
                rw [hAddVal] at hTail
                exact ⟨hOutLen, hDrop ▸ PrescanValidTail.single raw1 _ hNotLddw hTail⟩
              · rw [usize_two_val] at hAddVal
                have hLt1 : pc1.val + 1 < code.val.length := (List.getElem?_eq_some_iff.mp hGet1).1
                have hDrop0 := list_getElem?_imp_drop code.val pc1.val raw1 hGet0
                have hDrop1 := list_getElem?_imp_drop code.val (pc1.val + 1) raw2 hGet1
                refine ⟨fun _ => ⟨hPpNextLen.trans hPpLen, by omega⟩,
                  fun hIH => ⟨fun h => False.elim (h rfl), fun hOutNone => ?_⟩⟩
                have ⟨hOutLen, hTail⟩ := hIH.2 hOutNone
                rw [hAddVal] at hTail
                have hDrop2 : code.val.drop pc1.val = raw1 :: raw2 :: code.val.drop (pc1.val + 2) := by
                  rw [hDrop0, hDrop1]
                exact ⟨hOutLen, hDrop2 ▸ PrescanValidTail.lddw raw1 raw2 _ rlddw
                  hIsLddw hZero32 hRes hTail⟩
        · simp at hBody
      · simp at hBody)
    (by
      rintro ⟨pp1, err1, pc1⟩ ⟨outPp, outErr⟩ hPre1 hBody
      unfold ebpf_core.verifier.verify_program_core_loop1.body at hBody
      dsimp (config := { zetaHave := true }) only at hPre1 hBody ⊢
      split at hBody
      · split at hBody
        · rcases bind_eq_ok hBody with ⟨⟨r, _⟩, _, hBody⟩
          cases r with
          | Err _ => simp at hBody
          | Ok _ =>
            rcases bind_eq_ok hBody with ⟨_, _, hBody⟩
            simp at hBody
        · rename_i hNotIsNone
          cases ok_inj hBody
          cases err1 with
          | none => simp [core.option.Option.is_none] at hNotIsNone
          | some _ => exact ⟨fun _ => by simp, fun h => by cases h⟩
      · rename_i hNotLt
        cases ok_inj hBody
        cases err1 with
        | some _ => exact ⟨fun _ => by simp, fun h => by cases h⟩
        | none =>
          have ⟨hPpLen, hPcLe⟩ := hPre1 rfl
          have hGe : code.val.length ≤ pc1.val := by
            rw [hCodeLen] at hNotLt
            exact not_lt.mp (fun hc => hNotLt ((UScalar.lt_equiv _ _).mpr
              (by simpa [alloc.vec.Vec.len_val] using hc)))
          have hEq : pc1.val = code.val.length := by omega
          refine ⟨fun h => False.elim (h rfl), fun _ => ⟨hPpLen, ?_⟩⟩
          rw [hEq, List.drop_length]
          exact PrescanValidTail.nil)
    (pp, scan_err, scan_pc) (ppOut, none) hPre hLoop
  exact hRel.2 rfl

/-- Dead-code rewriting (`verify_program_core_loop6`) preserves prefix slots `< pc.val`,
    preserves total `code` length, and produces a suffix `codeOut.val.drop pc.val` that
    decodes via `decodeVerifiedCodeList` while keeping every visited slot (`visited[k] = true`)
    identical to its original verified instruction and never producing `Insn.lddwCont` at
    any visited PC. -/
theorem verify_program_core_loop6_spec
    {cctx : ebpf_core.verifier.CoreCallingContext}
    {cs : List ebpf_core.api.EbpfInstruction}
    (hTail : PrescanValidTail cctx cs) :
    ∀ (visited : alloc.vec.Vec Bool)
      (code codeOut : alloc.vec.Vec ebpf_core.api.EbpfInstruction)
      (dead_insn : ebpf_core.api.EbpfInstruction)
      (pc : Usize),
      code.val.drop pc.val = cs →
      visited.val.length = code.val.length →
      dead_insn = deadEbpfInsn →
      (∀ k, pc.val ≤ k → k < code.val.length → visited.val[k]? = some true →
        ∃ raw, code.val[k]? = some raw ∧
          raw.bv.setWidth 8 ≠ 0#8 ∧
          (raw.bv.setWidth 8 ≠ 0x18#8 →
            ∃ insn, insn ≠ .lddwCont ∧ decodeSingleInsn raw = some insn)) →
      ebpf_core.verifier.verify_program_core_loop6 visited code dead_insn pc = .ok codeOut →
      codeOut.val.length = code.val.length ∧
      (∀ j, j < pc.val → codeOut.val[j]? = code.val[j]?) ∧
      ∃ progTail,
        decodeVerifiedCodeList (codeOut.val.drop pc.val) = some progTail ∧
        (∀ m, m < cs.length → visited.val[pc.val + m]? = some true →
          ∃ raw insn,
            code.val[pc.val + m]? = some raw ∧
            progTail[m]? = some insn ∧
            insn ≠ .lddwCont ∧
            ((raw.bv.setWidth 8 = 0x18#8 →
                ∃ raw2 rlddw,
                  code.val[pc.val + m + 1]? = some raw2 ∧
                  raw2.bv.setWidth 32 = 0#32 ∧
                  ebpf_core.verifier.CoreCallingContext.resolve_lddw cctx
                    ⟨((raw.bv >>> 12) &&& 0xF#64).setWidth 8⟩
                    (UScalar.hcast .I32 (⟨(raw.bv >>> 32).setWidth 32⟩ : U32))
                    (UScalar.hcast .I32 (⟨(raw2.bv >>> 32).setWidth 32⟩ : U32)) = .ok (.Ok rlddw) ∧
                  insn = lddwInsnOfResolved (((raw.bv >>> 8) &&& 0xF#64).toNat) rlddw) ∧
             (raw.bv.setWidth 8 ≠ 0x18#8 →
                decodeSingleInsn raw = some insn))) := by
  induction hTail with
  | nil =>
    intro visited code codeOut dead_insn pc hDrop _ _ _ hLoop
    have hGe : code.val.length ≤ pc.val := list_drop_eq_nil_imp code.val pc.val hDrop
    have hNotLt : ¬ (pc < alloc.vec.Vec.len code) := by
      intro hlt
      have hltV : pc.val < code.val.length := by
        simpa [alloc.vec.Vec.len_val] using (UScalar.lt_equiv _ _).mp hlt
      omega
    unfold ebpf_core.verifier.verify_program_core_loop6 at hLoop
    rw [loop_unfold_step] at hLoop
    dsimp only at hLoop
    unfold ebpf_core.verifier.verify_program_core_loop6.body at hLoop
    simp only [hNotLt, ↓reduceIte, bind_ok] at hLoop
    cases ok_inj hLoop
    refine ⟨rfl, fun _ _ => rfl, [], ?_, fun m hm => by simp at hm⟩
    rw [hDrop]
    rfl
  | single raw rest hNotLddw _ ih =>
    intro visited code codeOut dead_insn pc hDrop hVisLen hDead hVisValid hLoop
    subst hDead
    rcases list_drop_cons_imp code.val pc.val raw rest hDrop with ⟨hPcLt, hGetCode, hDrop1⟩
    have hPcLtU : pc < alloc.vec.Vec.len code :=
      (UScalar.lt_equiv _ _).mpr (by simpa [alloc.vec.Vec.len_val] using hPcLt)
    unfold ebpf_core.verifier.verify_program_core_loop6 at hLoop
    rw [loop_unfold_step] at hLoop
    dsimp only at hLoop
    unfold ebpf_core.verifier.verify_program_core_loop6.body at hLoop
    simp only [hPcLtU, ↓reduceIte] at hLoop
    rcases bind_eq_ok hLoop with ⟨r, hBody, hContLoop⟩
    rcases bind_eq_ok hBody with ⟨ei, hEiIdx, hBody⟩
    have ⟨_, hGetEi⟩ := vec_index_usize_ok hEiIdx
    have hEiEq : ei = raw := Option.some.inj (hGetEi.symm.trans hGetCode)
    rw [hEiEq] at hBody
    rw [bind_ok_eq (ebpf_code_eq_ok raw)] at hBody
    have hNotLddwU8 : (⟨raw.bv.setWidth 8⟩ : U8) ≠ ebpf_core.api.BPF_LDDW := by
      rw [bpf_lddw_eq]
      intro hc
      exact hNotLddw (congrArg UScalar.bv hc)
    simp only [hNotLddwU8, ↓reduceIte] at hBody
    rcases bind_eq_ok hBody with ⟨b, hVisIdx, hBody⟩
    have ⟨_, hGetVis⟩ := vec_index_usize_ok hVisIdx
    cases b with
    | true =>
      simp only [↓reduceIte, bind_ok] at hBody
      rcases bind_eq_ok hBody with ⟨pc1, hPc1Add, hBody⟩
      cases ok_inj hBody
      dsimp only at hContLoop
      have hPc1Val : pc1.val = pc.val + 1 := by
        simpa [usize_one_val] using (uscalar_add_ok_val hPc1Add).1
      rcases hVisValid pc.val (Nat.le_refl _) hPcLt hGetVis with ⟨raw', hGetRaw', _, hNonLddwDec⟩
      have hRawEq : raw' = raw := Option.some.inj (hGetRaw'.symm.trans hGetCode)
      rw [hRawEq] at hNonLddwDec
      rcases hNonLddwDec hNotLddw with ⟨insn0, hNotCont0, hDec0⟩
      have hDropPc1 : code.val.drop pc1.val = rest := by rw [hPc1Val]; exact hDrop1
      have hVisValid1 : ∀ k, pc1.val ≤ k → k < code.val.length → visited.val[k]? = some true →
          ∃ r0, code.val[k]? = some r0 ∧ r0.bv.setWidth 8 ≠ 0#8 ∧
            (r0.bv.setWidth 8 ≠ 0x18#8 → ∃ i0, i0 ≠ .lddwCont ∧ decodeSingleInsn r0 = some i0) :=
        fun k hk1 hkLen hkVis => hVisValid k (by omega) hkLen hkVis
      rcases ih visited code codeOut deadEbpfInsn pc1 hDropPc1 hVisLen rfl hVisValid1
          hContLoop with ⟨hOutLen, hPrefix1, tailInsns, hDecTail, hCorrTail⟩
      have hOutPc : codeOut.val[pc.val]? = some raw :=
        (hPrefix1 pc.val (by omega)).trans hGetCode
      have hOutDrop : codeOut.val.drop pc.val = raw :: codeOut.val.drop pc1.val := by
        rw [hPc1Val]
        exact list_getElem?_imp_drop codeOut.val pc.val raw hOutPc
      refine ⟨hOutLen, fun j hj => hPrefix1 j (by omega), insn0 :: tailInsns, ?_, ?_⟩
      · rw [hOutDrop]
        exact decodeVerifiedCodeList_single_step raw _ insn0 tailInsns hNotLddw hDec0 hDecTail
      · intro m hm hVisM
        cases m with
        | zero =>
          simp only [Nat.add_zero] at hVisM ⊢
          exact ⟨raw, insn0, hGetCode, rfl, hNotCont0,
            ⟨fun hc => False.elim (hNotLddw hc), fun _ => hDec0⟩⟩
        | succ m' =>
          have hm' : m' < rest.length := Nat.lt_of_succ_lt_succ hm
          have hEqIdx : pc.val + (m' + 1) = pc1.val + m' := by omega
          rw [hEqIdx] at hVisM ⊢
          rcases hCorrTail m' hm' hVisM with ⟨rM, iM, hCodeM, hProgM, hNotContM, hSpecM⟩
          exact ⟨rM, iM, hCodeM, by simpa using hProgM, hNotContM,
            by simpa [Nat.add_assoc] using hSpecM⟩
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at hBody
      rcases bind_eq_ok hBody with ⟨code1, hMut, hBody⟩
      rcases bind_eq_ok hBody with ⟨pc1, hPc1Add, hBody⟩
      cases ok_inj hBody
      dsimp only at hContLoop
      rcases bind_eq_ok hMut with ⟨⟨_, back⟩, hIdxMut, hMutOk⟩
      cases ok_inj hMutOk
      have ⟨hMutLt, _, hMutSet⟩ := vec_index_mut_usize_ok hIdxMut
      have ⟨hCode1Val, hCode1Len⟩ := hMutSet deadEbpfInsn
      have hPc1Val : pc1.val = pc.val + 1 := by
        simpa [usize_one_val] using (uscalar_add_ok_val hPc1Add).1
      have hDropPc1 : (back deadEbpfInsn).val.drop pc1.val = rest := by
        rw [hCode1Val, hPc1Val, list_drop_set_gt code.val pc.val (pc.val + 1) deadEbpfInsn (by omega)]
        exact hDrop1
      have hCode1Gt : ∀ k, pc1.val ≤ k → (back deadEbpfInsn).val[k]? = code.val[k]? := by
        intro k hk
        rw [hCode1Val]
        exact list_getElem?_set_ne code.val pc.val k deadEbpfInsn (by omega)
      have hVisValid1 : ∀ k, pc1.val ≤ k → k < (back deadEbpfInsn).val.length →
          visited.val[k]? = some true →
          ∃ r0, (back deadEbpfInsn).val[k]? = some r0 ∧ r0.bv.setWidth 8 ≠ 0#8 ∧
            (r0.bv.setWidth 8 ≠ 0x18#8 → ∃ i0, i0 ≠ .lddwCont ∧ decodeSingleInsn r0 = some i0) := by
        intro k hk1 hkLen hkVis
        rw [hCode1Gt k hk1]
        exact hVisValid k (by omega) (by omega) hkVis
      rcases ih visited (back deadEbpfInsn) codeOut deadEbpfInsn pc1 hDropPc1
          (hVisLen.trans hCode1Len.symm) rfl hVisValid1 hContLoop with
          ⟨hOutLen, hPrefix1, tailInsns, hDecTail, hCorrTail⟩
      have hOutPc : codeOut.val[pc.val]? = some deadEbpfInsn := by
        rw [hPrefix1 pc.val (by omega), hCode1Val]
        exact list_getElem?_set_self code.val pc.val deadEbpfInsn hMutLt
      have hOutDrop : codeOut.val.drop pc.val = deadEbpfInsn :: codeOut.val.drop pc1.val := by
        rw [hPc1Val]
        exact list_getElem?_imp_drop codeOut.val pc.val deadEbpfInsn hOutPc
      refine ⟨hOutLen.trans hCode1Len, ?_, .jump (-1) :: tailInsns, ?_, ?_⟩
      · intro j hj
        rw [hPrefix1 j (by omega), hCode1Val]
        exact list_getElem?_set_ne code.val pc.val j deadEbpfInsn (by omega)
      · rw [hOutDrop]
        exact decodeVerifiedCodeList_single_step deadEbpfInsn _ (.jump (-1)) tailInsns
          dead_insn_not_lddw dead_insn_decode hDecTail
      · intro m hm hVisM
        cases m with
        | zero =>
          simp only [Nat.add_zero] at hVisM
          rw [hGetVis] at hVisM
          cases hVisM
        | succ m' =>
          have hm' : m' < rest.length := Nat.lt_of_succ_lt_succ hm
          have hEqIdx : pc.val + (m' + 1) = pc1.val + m' := by omega
          rw [hEqIdx] at hVisM ⊢
          rcases hCorrTail m' hm' hVisM with ⟨rM, iM, hCodeM, hProgM, hNotContM, hSpecM⟩
          rw [hCode1Gt (pc1.val + m') (by omega)] at hCodeM
          have hSpecM' :
              (rM.bv.setWidth 8 = 0x18#8 →
                ∃ raw2 rlddw,
                  code.val[pc1.val + m' + 1]? = some raw2 ∧
                  raw2.bv.setWidth 32 = 0#32 ∧
                  ebpf_core.verifier.CoreCallingContext.resolve_lddw cctx
                    ⟨((rM.bv >>> 12) &&& 0xF#64).setWidth 8⟩
                    (UScalar.hcast .I32 (⟨(rM.bv >>> 32).setWidth 32⟩ : U32))
                    (UScalar.hcast .I32 (⟨(raw2.bv >>> 32).setWidth 32⟩ : U32)) = .ok (.Ok rlddw) ∧
                  iM = lddwInsnOfResolved (((rM.bv >>> 8) &&& 0xF#64).toNat) rlddw) ∧
              (rM.bv.setWidth 8 ≠ 0x18#8 → decodeSingleInsn rM = some iM) := by
            refine ⟨fun h18 => ?_, hSpecM.2⟩
            rcases hSpecM.1 h18 with ⟨raw2, rlddw, hGet2, hZ2, hR2, hI2⟩
            rw [hCode1Gt (pc1.val + m' + 1) (by omega)] at hGet2
            exact ⟨raw2, rlddw, hGet2, hZ2, hR2, hI2⟩
          exact ⟨rM, iM, hCodeM, by simpa using hProgM, hNotContM, hSpecM'⟩
  | lddw raw1 raw2 rest rlddw hIsLddw hContZero hResolve _ ih =>
    intro visited code codeOut dead_insn pc hDrop hVisLen hDead hVisValid hLoop
    subst hDead
    rcases list_drop_cons_imp code.val pc.val raw1 (raw2 :: rest) hDrop with
      ⟨hPcLt, hGetCode0, hDrop1⟩
    rcases list_drop_cons_imp code.val (pc.val + 1) raw2 rest hDrop1 with
      ⟨hPc1Lt, hGetCode1, hDrop2⟩
    have hRaw2Zero8 : raw2.bv.setWidth 8 = 0#8 := by
      generalize raw2.bv = w at hContZero ⊢
      bv_decide
    have hVis1NotTrue : visited.val[pc.val + 1]? ≠ some true := by
      intro hVis1
      rcases hVisValid (pc.val + 1) (by omega) hPc1Lt hVis1 with ⟨r2, hGetR2, hNe0, _⟩
      have hEqR2 : r2 = raw2 := Option.some.inj (hGetR2.symm.trans hGetCode1)
      subst hEqR2
      exact hNe0 hRaw2Zero8
    have hPcLtU : pc < alloc.vec.Vec.len code :=
      (UScalar.lt_equiv _ _).mpr (by simpa [alloc.vec.Vec.len_val] using hPcLt)
    unfold ebpf_core.verifier.verify_program_core_loop6 at hLoop
    rw [loop_unfold_step] at hLoop
    dsimp only at hLoop
    unfold ebpf_core.verifier.verify_program_core_loop6.body at hLoop
    simp only [hPcLtU, ↓reduceIte] at hLoop
    rcases bind_eq_ok hLoop with ⟨r, hBody, hContLoop⟩
    rcases bind_eq_ok hBody with ⟨ei, hEiIdx, hBody⟩
    have ⟨_, hGetEi⟩ := vec_index_usize_ok hEiIdx
    have hEiEq : ei = raw1 := Option.some.inj (hGetEi.symm.trans hGetCode0)
    rw [hEiEq] at hBody
    rw [bind_ok_eq (ebpf_code_eq_ok raw1)] at hBody
    have hIsLddwU8 : (⟨raw1.bv.setWidth 8⟩ : U8) = ebpf_core.api.BPF_LDDW := by
      rw [bpf_lddw_eq]
      exact U8.bv_eq_imp_eq _ _ hIsLddw
    simp only [hIsLddwU8, ↓reduceIte] at hBody
    rcases bind_eq_ok hBody with ⟨b, hVisIdx, hBody⟩
    have ⟨_, hGetVis0⟩ := vec_index_usize_ok hVisIdx
    cases b with
    | true =>
      simp only [↓reduceIte, bind_ok] at hBody
      rcases bind_eq_ok hBody with ⟨pc2, hPc2Add, hBody⟩
      cases ok_inj hBody
      dsimp only at hContLoop
      have hPc2Val : pc2.val = pc.val + 2 := by
        simpa [usize_two_val] using (uscalar_add_ok_val hPc2Add).1
      have hDropPc2 : code.val.drop pc2.val = rest := by rw [hPc2Val]; exact hDrop2
      have hVisValid2 : ∀ k, pc2.val ≤ k → k < code.val.length → visited.val[k]? = some true →
          ∃ r0, code.val[k]? = some r0 ∧ r0.bv.setWidth 8 ≠ 0#8 ∧
            (r0.bv.setWidth 8 ≠ 0x18#8 → ∃ i0, i0 ≠ .lddwCont ∧ decodeSingleInsn r0 = some i0) :=
        fun k hk2 hkLen hkVis => hVisValid k (by omega) hkLen hkVis
      rcases ih visited code codeOut deadEbpfInsn pc2 hDropPc2 hVisLen rfl hVisValid2
          hContLoop with ⟨hOutLen, hPrefix2, tailInsns, hDecTail, hCorrTail⟩
      have hOutPc0 : codeOut.val[pc.val]? = some raw1 :=
        (hPrefix2 pc.val (by omega)).trans hGetCode0
      have hOutPc1 : codeOut.val[pc.val + 1]? = some raw2 :=
        (hPrefix2 (pc.val + 1) (by omega)).trans hGetCode1
      have hOutDrop : codeOut.val.drop pc.val = raw1 :: raw2 :: codeOut.val.drop pc2.val := by
        have hd0 := list_getElem?_imp_drop codeOut.val pc.val raw1 hOutPc0
        have hd1 := list_getElem?_imp_drop codeOut.val (pc.val + 1) raw2 hOutPc1
        rw [hd0, hd1, hPc2Val]
      let insn0 := lddwInsnOfResolved (((raw1.bv >>> 8) &&& 0xF#64).toNat) rlddw
      refine ⟨hOutLen, fun j hj => hPrefix2 j (by omega), insn0 :: .lddwCont :: tailInsns, ?_, ?_⟩
      · rw [hOutDrop]
        exact decodeVerifiedCodeList_lddw_step cctx raw1 raw2 _ rlddw tailInsns
          hIsLddw hContZero hResolve hDecTail
      · intro m hm hVisM
        cases m with
        | zero =>
          simp only [Nat.add_zero] at hVisM ⊢
          exact ⟨raw1, insn0, hGetCode0, rfl, lddwInsnOfResolved_not_lddwCont _ rlddw,
            ⟨fun _ => ⟨raw2, rlddw, hGetCode1, hContZero, hResolve, rfl⟩,
             fun hc => False.elim (hc hIsLddw)⟩⟩
        | succ m1 =>
          cases m1 with
          | zero =>
            exact False.elim (hVis1NotTrue hVisM)
          | succ m2 =>
            have hm2 : m2 < rest.length := by
              simp only [List.length_cons] at hm; omega
            have hEqIdx : pc.val + (m2 + 1 + 1) = pc2.val + m2 := by omega
            rw [hEqIdx] at hVisM ⊢
            rcases hCorrTail m2 hm2 hVisM with ⟨rM, iM, hCodeM, hProgM, hNotContM, hSpecM⟩
            exact ⟨rM, iM, hCodeM, by simpa using hProgM, hNotContM, hSpecM⟩
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at hBody
      rcases bind_eq_ok hBody with ⟨codeMid, hMut, hBody⟩
      rcases bind_eq_ok hBody with ⟨pc2, hPc2Add, hBody⟩
      cases ok_inj hBody
      dsimp only at hContLoop
      rcases bind_eq_ok hMut with ⟨⟨_, back0⟩, hIdxMut0, hMutRest⟩
      rcases bind_eq_ok hMutRest with ⟨pc1, hPc1Add, hMutRest⟩
      rcases bind_eq_ok hMutRest with ⟨⟨_, back1⟩, hIdxMut1, hMutOk⟩
      cases ok_inj hMutOk
      have hPc1Val : pc1.val = pc.val + 1 := by
        simpa [usize_one_val] using (uscalar_add_ok_val hPc1Add).1
      have hPc2Val : pc2.val = pc.val + 2 := by
        simpa [usize_two_val] using (uscalar_add_ok_val hPc2Add).1
      have ⟨hMutLt0, _, hSet0⟩ := vec_index_mut_usize_ok hIdxMut0
      have ⟨hVal0, hLen0⟩ := hSet0 deadEbpfInsn
      have ⟨hMutLt1, _, hSet1⟩ := vec_index_mut_usize_ok hIdxMut1
      have ⟨hVal1, hLen1⟩ := hSet1 deadEbpfInsn
      let code2 := back1 deadEbpfInsn
      have hDropPc2 : code2.val.drop pc2.val = rest := by
        change (back1 deadEbpfInsn).val.drop pc2.val = rest
        rw [hVal1, hVal0, hPc2Val]
        rw [list_drop_set_gt _ pc1.val (pc.val + 2) deadEbpfInsn (by omega)]
        rw [list_drop_set_gt _ pc.val (pc.val + 2) deadEbpfInsn (by omega)]
        exact hDrop2
      have hCode2Gt : ∀ k, pc2.val ≤ k → code2.val[k]? = code.val[k]? := by
        intro k hk
        change (back1 deadEbpfInsn).val[k]? = code.val[k]?
        rw [hVal1, list_getElem?_set_ne _ pc1.val k deadEbpfInsn (by omega),
            hVal0, list_getElem?_set_ne _ pc.val k deadEbpfInsn (by omega)]
      have hCode2Len : code2.val.length = code.val.length := hLen1.trans hLen0
      have hVisValid2 : ∀ k, pc2.val ≤ k → k < code2.val.length →
          visited.val[k]? = some true →
          ∃ r0, code2.val[k]? = some r0 ∧ r0.bv.setWidth 8 ≠ 0#8 ∧
            (r0.bv.setWidth 8 ≠ 0x18#8 → ∃ i0, i0 ≠ .lddwCont ∧ decodeSingleInsn r0 = some i0) := by
        intro k hk2 hkLen hkVis
        rw [hCode2Gt k hk2]
        exact hVisValid k (by omega) (by omega) hkVis
      rcases ih visited code2 codeOut deadEbpfInsn pc2 hDropPc2
          (hVisLen.trans hCode2Len.symm) rfl hVisValid2 hContLoop with
          ⟨hOutLen, hPrefix2, tailInsns, hDecTail, hCorrTail⟩
      have hOutPc0 : codeOut.val[pc.val]? = some deadEbpfInsn := by
        rw [hPrefix2 pc.val (by omega)]
        change (back1 deadEbpfInsn).val[pc.val]? = some deadEbpfInsn
        rw [hVal1, list_getElem?_set_ne _ pc1.val pc.val deadEbpfInsn (by omega),
            hVal0]
        exact list_getElem?_set_self code.val pc.val deadEbpfInsn hMutLt0
      have hOutPc1 : codeOut.val[pc.val + 1]? = some deadEbpfInsn := by
        rw [hPrefix2 (pc.val + 1) (by omega)]
        change (back1 deadEbpfInsn).val[pc.val + 1]? = some deadEbpfInsn
        rw [hVal1, ← hPc1Val]
        exact list_getElem?_set_self (back0 deadEbpfInsn).val pc1.val deadEbpfInsn hMutLt1
      have hOutDrop : codeOut.val.drop pc.val =
          deadEbpfInsn :: deadEbpfInsn :: codeOut.val.drop pc2.val := by
        have hd0 := list_getElem?_imp_drop codeOut.val pc.val deadEbpfInsn hOutPc0
        have hd1 := list_getElem?_imp_drop codeOut.val (pc.val + 1) deadEbpfInsn hOutPc1
        rw [hd0, hd1, hPc2Val]
      refine ⟨hOutLen.trans hCode2Len, ?_, .jump (-1) :: .jump (-1) :: tailInsns, ?_, ?_⟩
      · intro j hj
        rw [hPrefix2 j (by omega)]
        change (back1 deadEbpfInsn).val[j]? = code.val[j]?
        rw [hVal1, list_getElem?_set_ne _ pc1.val j deadEbpfInsn (by omega),
            hVal0, list_getElem?_set_ne _ pc.val j deadEbpfInsn (by omega)]
      · rw [hOutDrop]
        exact decodeVerifiedCodeList_single_step deadEbpfInsn _ (.jump (-1)) _
          dead_insn_not_lddw dead_insn_decode
          (decodeVerifiedCodeList_single_step deadEbpfInsn _ (.jump (-1)) tailInsns
            dead_insn_not_lddw dead_insn_decode hDecTail)
      · intro m hm hVisM
        cases m with
        | zero =>
          simp only [Nat.add_zero] at hVisM
          rw [hGetVis0] at hVisM
          cases hVisM
        | succ m1 =>
          cases m1 with
          | zero =>
            exact False.elim (hVis1NotTrue hVisM)
          | succ m2 =>
            have hm2 : m2 < rest.length := by
              simp only [List.length_cons] at hm; omega
            have hEqIdx : pc.val + (m2 + 1 + 1) = pc2.val + m2 := by omega
            rw [hEqIdx] at hVisM ⊢
            rcases hCorrTail m2 hm2 hVisM with ⟨rM, iM, hCodeM, hProgM, hNotContM, hSpecM⟩
            rw [hCode2Gt (pc2.val + m2) (by omega)] at hCodeM
            have hSpecM' :
                (rM.bv.setWidth 8 = 0x18#8 →
                  ∃ raw2' rlddw',
                    code.val[pc2.val + m2 + 1]? = some raw2' ∧
                    raw2'.bv.setWidth 32 = 0#32 ∧
                    ebpf_core.verifier.CoreCallingContext.resolve_lddw cctx
                      ⟨((rM.bv >>> 12) &&& 0xF#64).setWidth 8⟩
                      (UScalar.hcast .I32 (⟨(rM.bv >>> 32).setWidth 32⟩ : U32))
                      (UScalar.hcast .I32 (⟨(raw2'.bv >>> 32).setWidth 32⟩ : U32)) = .ok (.Ok rlddw') ∧
                    iM = lddwInsnOfResolved (((rM.bv >>> 8) &&& 0xF#64).toNat) rlddw') ∧
                (rM.bv.setWidth 8 ≠ 0x18#8 → decodeSingleInsn rM = some iM) := by
              refine ⟨fun h18 => ?_, hSpecM.2⟩
              rcases hSpecM.1 h18 with ⟨raw2', rlddw', hGet2, hZ2, hR2, hI2⟩
              rw [hCode2Gt (pc2.val + m2 + 1) (by omega)] at hGet2
              exact ⟨raw2', rlddw', hGet2, hZ2, hR2, hI2⟩
            exact ⟨rM, iM, hCodeM, by simpa using hProgM, hNotContM, hSpecM'⟩

/-- Top-level specialization of `verify_program_core_loop6_spec` at `pc = 0#usize`,
    yielding `decodeVerifiedCode codeOut.val = some prog` with `prog.size = code.val.length`
    and exact instruction correspondence at every `visited[pc] = true` slot. -/
theorem verify_program_core_loop6_zero_spec
    {cctx : ebpf_core.verifier.CoreCallingContext}
    (visited : alloc.vec.Vec Bool)
    (code codeOut : alloc.vec.Vec ebpf_core.api.EbpfInstruction)
    (dead_insn : ebpf_core.api.EbpfInstruction)
    (hPrescan : PrescanValidTail cctx code.val)
    (hVisLen : visited.val.length = code.val.length)
    (hDead : dead_insn = deadEbpfInsn)
    (hVisValid : ∀ k, k < code.val.length → visited.val[k]? = some true →
      ∃ raw, code.val[k]? = some raw ∧
        raw.bv.setWidth 8 ≠ 0#8 ∧
        (raw.bv.setWidth 8 ≠ 0x18#8 →
          ∃ insn, insn ≠ .lddwCont ∧ decodeSingleInsn raw = some insn))
    (hLoop : ebpf_core.verifier.verify_program_core_loop6 visited code dead_insn 0#usize = .ok codeOut) :
    ∃ prog : Program,
      decodeVerifiedCode codeOut.val = some prog ∧
      prog.size = code.val.length ∧
      (∀ k, k < code.val.length → visited.val[k]? = some true →
        ∃ raw insn,
          code.val[k]? = some raw ∧
          prog[k]? = some insn ∧
          insn ≠ .lddwCont ∧
          ((raw.bv.setWidth 8 = 0x18#8 →
              ∃ raw2 rlddw,
                code.val[k + 1]? = some raw2 ∧
                raw2.bv.setWidth 32 = 0#32 ∧
                ebpf_core.verifier.CoreCallingContext.resolve_lddw cctx
                  ⟨((raw.bv >>> 12) &&& 0xF#64).setWidth 8⟩
                  (UScalar.hcast .I32 (⟨(raw.bv >>> 32).setWidth 32⟩ : U32))
                  (UScalar.hcast .I32 (⟨(raw2.bv >>> 32).setWidth 32⟩ : U32)) = .ok (.Ok rlddw) ∧
                insn = lddwInsnOfResolved (((raw.bv >>> 8) &&& 0xF#64).toNat) rlddw) ∧
           (raw.bv.setWidth 8 ≠ 0x18#8 →
              decodeSingleInsn raw = some insn))) := by
  have hZeroVal : (0#usize).val = 0 := by simp
  have hDrop0 : code.val.drop (0#usize).val = code.val := by simp [hZeroVal]
  rcases verify_program_core_loop6_spec hPrescan visited code codeOut dead_insn 0#usize
      hDrop0 hVisLen hDead (fun k _ hk hv => hVisValid k hk hv) hLoop with
      ⟨hOutLen, _, progList, hDecList, hCorr⟩
  rw [hDrop0] at *
  have hProgLen : progList.length = code.val.length :=
    (decodeVerifiedCodeList_length codeOut.val progList (by simpa [hZeroVal] using hDecList)).trans hOutLen
  refine ⟨progList.toArray, ?_, ?_, ?_⟩
  · unfold decodeVerifiedCode
    simpa [hZeroVal] using hDecList
  · simpa using hProgLen
  · intro k hk hVisK
    rcases hCorr k hk (by simpa [hZeroVal] using hVisK) with ⟨raw, insn, hCodeK, hListK, hNotCont, hSpecK⟩
    simp only [hZeroVal, Nat.zero_add] at hCodeK hSpecK
    exact ⟨raw, insn, hCodeK, by simpa using hListK, hNotCont, hSpecK⟩

end StarnixEbpf.Proof
