-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.CondJumpSoundness
import Proof.MemLoadStoreSoundness

/-!
# Soundness of Atomic Memory Operations & Packet Loads

Proves forward invariant preservation (`ForwardVisitPost`) and backward 2-safety
/ termination (`SemTerminatesIn`) for:
1. Atomic read-modify-write instructions (`Insn.atomic`: 32/64-bit `add`, `or`,
   `and`, `xor` with/without `fetch`, `xchg`, and `cmpxchg`),
2. Direct and indirect legacy packet loads (`Insn.loadPacket`: `BPF_LD | ABS | IND`,
   including signed 32-bit offset overflow / out-of-bounds fast-exit when no
   resources are live, `r0` result write, and `r1..r5` scratch clobbering).
-/

set_option linter.unusedTactic false
set_option maxHeartbeats 800000

open Aeneas Aeneas.Std Result

namespace StarnixEbpf.Proof

open StarnixEbpf.Assumptions
open StarnixEbpf.Spec
open StarnixEbpf.LoopLemmas
open ebpf_core
open ebpf_core.scalar_value

/-! ## 1. Written-Scalar, Alignment, & 32-Bit RMW Helper Lemmas -/

private theorem i16_eq_of_bv_eq {a b : Std.I16} (h : a.bv = b.bv) : a = b := by
  cases a; cases b; exact congrArg IScalar.mk h

private theorem i32_eq_of_bv_eq {a b : Std.I32} (h : a.bv = b.bv) : a = b := by
  cases a; cases b; exact congrArg IScalar.mk h

theorem log_atomic_operation_ok_spec
    {self : verifier.ComputationContext} {op : verifier.AtomicLogOp}
    {vctx0 vctx1 : verifier.VerificationContext}
    {fetch : Bool} {dst src : Std.U8} {offset : Std.I16}
    (h : verifier.ComputationContext.log_atomic_operation self op vctx0 fetch dst offset src = ok vctx1) :
    vctx1.calling_context = vctx0.calling_context ∧
    vctx1.code = vctx0.code ∧
    vctx1.counter = vctx0.counter ∧
    vctx1.iteration = vctx0.iteration ∧
    vctx1.terminating_contexts = vctx0.terminating_contexts ∧
    vctx1.pending_successors = vctx0.pending_successors := by
  unfold verifier.ComputationContext.log_atomic_operation at h
  exact VerificationContext_emit_log_ok_spec h

private theorem aligned_of_tnum_mask_u32 {baseOff val unk : BitVec 64} {off16 : BitVec 16}
    (htnum : baseOff &&& ~~~unk = val)
    (hunk : unk &&& 3#64 = 0#64)
    (hval : (val + sextOffset16 off16) &&& 3#64 = 0#64) :
    isAligned (baseOff + sextOffset16 off16) .u32 = true := by
  unfold isAligned sextOffset16 DataWidth.bvBytes DataWidth.bytes at *
  rw [beq_iff_eq]
  revert htnum hunk hval
  bv_decide

private theorem aligned_of_tnum_mask_u64 {baseOff val unk : BitVec 64} {off16 : BitVec 16}
    (htnum : baseOff &&& ~~~unk = val)
    (hunk : unk &&& 7#64 = 0#64)
    (hval : (val + sextOffset16 off16) &&& 7#64 = 0#64) :
    isAligned (baseOff + sextOffset16 off16) .u64 = true := by
  unfold isAligned sextOffset16 DataWidth.bvBytes DataWidth.bytes at *
  rw [beq_iff_eq]
  revert htnum hunk hval
  bv_decide

private theorem check_atomic_alignment_sound
    {offSc : U64ScalarValueData} {is64 : Bool} {off : Std.I16} {baseOff : BitVec 64}
    (hconc : ConcretizesScalar64 offSc baseOff)
    (h : (do
      let i ← api.DataWidth.bytes (if is64 then api.DataWidth.U64 else api.DataWidth.U32)
      let i1 ← lift (UScalar.cast .U64 i)
      let alignment_mask ← i1 - 1#u64
      let i2 ← lift (offSc.unknown_mask &&& alignment_mask)
      if i2 != 0#u64 then
        ok (core.result.Result.Err error.VerifierErrorKind.MisalignedAtomicAccess)
      else
        let i3 ← lift (IScalar.cast .I64 off)
        let i4 ← lift (IScalar.hcast .U64 i3)
        let i5 ← lift (core.num.U64.wrapping_add offSc.value i4)
        let i6 ← lift (i5 &&& alignment_mask)
        if i6 != 0#u64 then
          ok (core.result.Result.Err error.VerifierErrorKind.MisalignedAtomicAccess)
        else ok (core.result.Result.Ok ())) = ok (.Ok ())) :
    isAligned (baseOff + sextOffset16 off.bv) (if is64 then .u64 else .u32) = true := by
  have hi4_bv : U64.bv (IScalar.hcast .U64 (IScalar.cast .I64 off)) = sextOffset16 off.bv := by
    change (IScalar.hcast .U64 (IScalar.cast .I64 off)).bv = off.bv.signExtend 64
    rw [IScalar.hcast_bv_eq, IScalar.cast_bv_eq]
    change (off.bv.signExtend 64).signExtend 64 = off.bv.signExtend 64
    exact BitVec.signExtend_eq (off.bv.signExtend 64)
  cases is64 with
  | false =>
    simp only [↓reduceIte, Bool.false_eq_true] at h ⊢
    obtain ⟨wb, hwb, hwb_val⟩ := DataWidth_bytes_spec .U32
    rw [bind_ok_eq hwb] at h
    have hcast_wb : UScalar.cast .U64 wb = 4#u64 :=
      (u64_eq_iff_bv _ _).mpr (by
        change (UScalar.cast .U64 wb).bv = 4#64
        rw [uscalar_cast_u64_of_usize_bv, hwb_val]; rfl)
    simp only [lift_eq_ok, bind_ok] at h
    rw [hcast_wb] at h
    have hsub4 : (4#u64 - 1#u64 : Result Std.U64) = ok 3#u64 := rfl
    rw [bind_ok_eq hsub4] at h
    cases hb1 : (offSc.unknown_mask &&& 3#u64 != 0#u64) <;>
      simp only [hb1, Bool.false_eq_true, ↓reduceIte] at h
    · simp only [u64_wrapping_add_eq] at h
      cases hb2 : (⟨offSc.value.bv + U64.bv (IScalar.hcast .U64 (IScalar.cast .I64 off))⟩ &&& 3#u64 != 0#u64) <;>
        simp only [hb2, Bool.false_eq_true, ↓reduceIte] at h
      · have hunk0 : offSc.unknown_mask.bv &&& 3#64 = 0#64 :=
          congrArg U64.bv (bne_eq_false_iff_eq.mp hb1)
        have hval0 : (offSc.value.bv + sextOffset16 off.bv) &&& 3#64 = 0#64 := by
          have heq := congrArg U64.bv (bne_eq_false_iff_eq.mp hb2)
          rw [u64_and_bv, u64_mk_bv, hi4_bv] at heq
          exact heq
        exact aligned_of_tnum_mask_u32 hconc.2.1 hunk0 hval0
      · cases ok_inj h
    · cases ok_inj h
  | true =>
    simp only [↓reduceIte] at h ⊢
    obtain ⟨wb, hwb, hwb_val⟩ := DataWidth_bytes_spec .U64
    rw [bind_ok_eq hwb] at h
    have hcast_wb : UScalar.cast .U64 wb = 8#u64 :=
      (u64_eq_iff_bv _ _).mpr (by
        change (UScalar.cast .U64 wb).bv = 8#64
        rw [uscalar_cast_u64_of_usize_bv, hwb_val]; rfl)
    simp only [lift_eq_ok, bind_ok] at h
    rw [hcast_wb] at h
    have hsub8 : (8#u64 - 1#u64 : Result Std.U64) = ok 7#u64 := rfl
    rw [bind_ok_eq hsub8] at h
    cases hb1 : (offSc.unknown_mask &&& 7#u64 != 0#u64) <;>
      simp only [hb1, Bool.false_eq_true, ↓reduceIte] at h
    · simp only [u64_wrapping_add_eq] at h
      cases hb2 : (⟨offSc.value.bv + U64.bv (IScalar.hcast .U64 (IScalar.cast .I64 off))⟩ &&& 7#u64 != 0#u64) <;>
        simp only [hb2, Bool.false_eq_true, ↓reduceIte] at h
      · have hunk0 : offSc.unknown_mask.bv &&& 7#64 = 0#64 :=
          congrArg U64.bv (bne_eq_false_iff_eq.mp hb1)
        have hval0 : (offSc.value.bv + sextOffset16 off.bv) &&& 7#64 = 0#64 := by
          have heq := congrArg U64.bv (bne_eq_false_iff_eq.mp hb2)
          rw [u64_and_bv, u64_mk_bv, hi4_bv] at heq
          exact heq
        exact aligned_of_tnum_mask_u64 hconc.2.1 hunk0 hval0
      · cases ok_inj h
    · cases ok_inj h

theorem check_atomic_access_ok_spec
    {self : verifier.ComputationContext} {addr : verifier.Type}
    {off : Std.I16} {is64 : Bool}
    (h : verifier.ComputationContext.check_atomic_access self addr
      { offset := off, width := if is64 then .U64 else .U32 } = ok (.Ok ())) :
    verifier.Type.inner addr self = ok (.Ok (pureUnwrapReleasable addr)) ∧
    addr ≠ .ScalarValue uninitScalar64 ∧
    ((∃ stack_offset,
        pureUnwrapReleasable addr = .PtrToStack stack_offset ∧
        ∀ baseOff, ConcretizesScalar64 stack_offset baseOff →
          isAligned (baseOff + sextOffset16 off.bv) (if is64 then .u64 else .u32) = true) ∨
     (∃ id offset buffer_size origin,
        pureUnwrapReleasable addr = .PtrToMemory id offset buffer_size origin ∧
        (MemoryOrigin.ofCore origin).isMap = true ∧
        ∀ baseOff, ConcretizesScalar64 offset baseOff →
          isAligned (baseOff + sextOffset16 off.bv) (if is64 then .u64 else .u32) = true)) := by
  unfold verifier.ComputationContext.check_atomic_access at h
  rcases bind_eq_ok h with ⟨r, hinner, h⟩
  cases r with
  | Err _ => cases ok_inj h
  | Ok a =>
    dsimp only at h
    have ha_eq : a = pureUnwrapReleasable addr := by
      unfold verifier.Type.inner at hinner
      rcases bind_eq_ok hinner with ⟨rc, _, hinner⟩
      cases rc with
      | Err _ => cases ok_inj hinner
      | Ok _ =>
        dsimp only at hinner
        rw [bind_ok_eq (Type_unwrap_releasable_spec addr)] at hinner
        exact (core.result.Result.Ok.inj (ok_inj hinner)).symm
    subst ha_eq
    have hne_uninit : addr ≠ .ScalarValue uninitScalar64 := by
      rintro rfl; cases ok_inj h
    refine ⟨hinner, hne_uninit, ?_⟩
    cases hu : pureUnwrapReleasable addr <;> rw [hu] at h
    all_goals try (cases ok_inj h)
    · -- PtrToStack
      rename_i stack_offset
      dsimp only [verifier.StackOffset.reg] at h
      rw [bind_ok] at h
      exact Or.inl ⟨stack_offset, rfl, fun baseOff hc => check_atomic_alignment_sound hc h⟩
    · -- PtrToMemory
      rename_i id offset buffer_size origin
      dsimp only at h
      rw [bind_ok_eq (PointerOrigin_is_map_value_spec origin)] at h
      cases hmap : (MemoryOrigin.ofCore origin).isMap <;>
        simp only [hmap, Bool.false_eq_true, ↓reduceIte] at h
      · cases ok_inj h
      · exact Or.inr ⟨id, offset, buffer_size, origin, rfl, hmap,
          fun baseOff hc => check_atomic_alignment_sound hc h⟩

theorem Stack_load_written_scalar_rel2
    {env : ExecEnv} {vCounter : Nat}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64}
    {self : verifier.Stack} (hs : StackValid env vCounter self)
    {stk₁ stk₂ : StackMem}
    {offset : verifier.StackOffset} {effOff : BitVec 64} {width : api.DataWidth}
    {loaded_data : U64ScalarValueData}
    (hunw : loaded_data.unwritten_mask = 0#u64)
    (hoff_rel : ConcretizesScalar64Rel2 offset effOff effOff)
    (hok : verifier.Stack.load self offset width = ok (.Ok (.ScalarValue loaded_data)))
    (hslot_rel : StackSlotRel2 env ρ resActive isNullId arrayLen
      (stackSlotType self (effOff.toNat / 8))
      (stk₁ (effOff.toNat / 8)) (stk₂ (effOff.toNat / 8))) :
    let w := DataWidth.ofCore width
    let slotIdx := effOff.toNat / 8
    let subIdx := effOff.toNat % 8
    effOff < 512#64 ∧
    ∃ oldSlot₁ initMask₁ oldSlot₂ initMask₂ : BitVec 64,
      stk₁ slotIdx = .bytes oldSlot₁ initMask₁ ∧
      stk₂ slotIdx = .bytes oldSlot₂ initMask₂ ∧
      extractSubValue initMask₁ subIdx w = w.valMask64 ∧
      extractSubValue initMask₂ subIdx w = w.valMask64 ∧
      extractSubValue oldSlot₂ subIdx w = extractSubValue oldSlot₁ subIdx w ∧
      ConcretizesScalar64Rel2 loaded_data
        (extractSubValue oldSlot₁ subIdx w) (extractSubValue oldSlot₁ subIdx w) := by
  dsimp only
  obtain ⟨heff_lt, _, _, vOut₁, vOut₂, hm1, hm2, hrel_out⟩ :=
    Stack_load_rel2 hs hoff_rel hok hslot_rel
  have hld_ne : loaded_data ≠ uninitScalar64 := fun hc => by
    subst hc; exact uninitScalar64_unwritten_ne_zero hunw
  have hu64_bytes : DataWidth.ofCore width = .u64 →
      ∃ oldSlot₁ initMask₁ oldSlot₂ initMask₂,
        stk₁ (effOff.toNat / 8) = .bytes oldSlot₁ initMask₁ ∧
        stk₂ (effOff.toNat / 8) = .bytes oldSlot₂ initMask₂ := by
    intro hw64
    have hwidth : width = .U64 := by cases width <;> cases hw64; rfl
    subst hwidth
    have hok' := hok
    unfold verifier.Stack.load at hok'
    obtain ⟨rchk, hrchk, hchk_iff⟩ := StackOffset_check_valid_offset_spec offset
    rw [bind_ok_eq hrchk] at hok'
    cases rchk with
    | Err _ => cases ok_inj hok'
    | Ok u0 =>
      obtain ⟨hoff_kn, hoff_lt⟩ := (hchk_iff u0).mp rfl
      have heff_eq : effOff = offset.value.bv := eq_value_of_known_64 hoff_kn hoff_rel.1
      have hoff_le : offset.value.bv ≤ 512#64 := BitVec.le_of_lt hoff_lt
      obtain ⟨sub, hsub, _, _⟩ := StackOffset_sub_index_spec hoff_le
      obtain ⟨bw, hbw, hbw_val⟩ := DataWidth_bytes_spec .U64
      have hbw_ne0 : bw.val ≠ 0 := by rw [hbw_val]; decide
      obtain ⟨rem, hrem, _⟩ := usize_rem_eq_ok sub bw hbw_ne0
      rw [bind_ok_eq hsub, bind_ok_eq hbw, bind_ok_eq hrem] at hok'
      cases hmis : (rem != 0#usize) <;> simp only [hmis, Bool.false_eq_true, ↓reduceIte] at hok'
      · obtain ⟨idx, hidx, hidx_val⟩ := StackOffset_array_index_spec hoff_le
        have hoff_512 : offset.value.bv.toNat < 512 := by rw [BitVec.lt_def] at hoff_lt; simpa using hoff_lt
        have hidx_lt64 : idx.val < 64 := by omega
        rw [bind_ok_eq hidx, bind_ok_eq (Stack_get_spec hs hidx_lt64),
            bind_ok_eq (Type_clone_spec _)] at hok'
        obtain ⟨beq, hbeq, heq_iff⟩ := DataWidth_eq_spec .U64 .U64
        have hbeq_true : beq = true := heq_iff.mpr rfl
        subst hbeq_true
        rw [bind_ok_eq hbeq] at hok'
        simp only [↓reduceIte] at hok'
        rw [bind_ok_eq (Type_is_initialized_spec _)] at hok'
        have hidx_eff : idx.val = effOff.toNat / 8 := by rw [heff_eq, hidx_val]
        rw [hidx_eff] at hok'
        split_ifs at hok'
        · have hst_eq : stackSlotType self (effOff.toNat / 8) = .ScalarValue loaded_data :=
            core.result.Result.Ok.inj (ok_inj hok')
          rw [hst_eq] at hslot_rel
          simp only [StackSlotRel2, hld_ne, ↓reduceIte] at hslot_rel
          rcases hslot_rel with ⟨v1, i1, v2, i2, hs1, hs2, _⟩
          exact ⟨v1, i1, v2, i2, hs1, hs2⟩
        · cases ok_inj hok'
      · cases ok_inj hok'
  have hslot_is_bytes :
      ∃ oldSlot₁ initMask₁ oldSlot₂ initMask₂,
        stk₁ (effOff.toNat / 8) = .bytes oldSlot₁ initMask₁ ∧
        stk₂ (effOff.toNat / 8) = .bytes oldSlot₂ initMask₂ := by
    cases h1 : stk₁ (effOff.toNat / 8) with
    | spilled _ =>
      rw [h1] at hm1
      rcases hu64_bytes hm1.1 with ⟨_, _, _, _, hb1, _⟩
      rw [h1] at hb1; cases hb1
    | bytes oldSlot₁ initMask₁ =>
      cases h2 : stk₂ (effOff.toNat / 8) with
      | spilled _ =>
        rw [h2] at hm2
        rcases hu64_bytes hm2.1 with ⟨_, _, _, _, _, hb2⟩
        rw [h2] at hb2; cases hb2
      | bytes oldSlot₂ initMask₂ =>
        exact ⟨oldSlot₁, initMask₁, oldSlot₂, initMask₂, rfl, rfl⟩
  rcases hslot_is_bytes with ⟨oldSlot₁, initMask₁, oldSlot₂, initMask₂, hstk1, hstk2⟩
  rw [hstk1] at hm1
  rw [hstk2] at hm2
  rcases hm1 with ⟨hinit1, rfl⟩
  rcases hm2 with ⟨hinit2, rfl⟩
  simp only [TypeRel2, hld_ne, ↓reduceIte] at hrel_out
  rcases hrel_out with ⟨x₁, x₂, hx1, hx2, hconc_ld⟩
  cases RuntimeVal.scalar.inj hx1
  cases RuntimeVal.scalar.inj hx2
  have hex_eq := (fully_initialized_eq_of_rel2_64 hunw hconc_ld).symm
  rw [hex_eq] at hconc_ld
  exact ⟨heff_lt, oldSlot₁, initMask₁, oldSlot₂, initMask₂, hstk1, hstk2,
    hinit1, hinit2, hex_eq, hconc_ld⟩

private theorem extractSubValue_u32_zext (v : BitVec 64) (subIdx : Nat) :
    ((extractSubValue v subIdx .u32).setWidth 32).zeroExtend 64 =
      extractSubValue v subIdx .u32 := by
  unfold extractSubValue DataWidth.valMask64
  bv_decide

private theorem readBufWord_u32_zext
    (buf : MemoryId → BitVec 64 → BitVec 8) (id : MemoryId) (off : BitVec 64) :
    ((readBufWord buf id off .u32).setWidth 32).zeroExtend 64 =
      readBufWord buf id off .u32 := by
  unfold readBufWord
  bv_decide

private theorem insertSubValue_u32_zext (orig val : BitVec 64) (subIdx : Nat) :
    insertSubValue orig ((val.setWidth 32).zeroExtend 64) subIdx .u32 =
      insertSubValue orig val subIdx .u32 := by
  unfold insertSubValue DataWidth.valMask64
  bv_decide

private theorem writeBufWord_u32_zext
    (buf : MemoryId → BitVec 64 → BitVec 8) (id : MemoryId) (off val : BitVec 64) :
    writeBufWord buf id off .u32 ((val.setWidth 32).zeroExtend 64) =
      writeBufWord buf id off .u32 val := by
  funext id' off'
  unfold writeBufWord writeBufByte
  by_cases h0 : (id' == id && off' == off + 3#64) = true
  · simp only [h0, ↓reduceIte]; bv_decide
  · simp only [h0, Bool.false_eq_true, ↓reduceIte]
    by_cases h1 : (id' == id && off' == off + 2#64) = true
    · simp only [h1, ↓reduceIte]; bv_decide
    · simp only [h1, Bool.false_eq_true, ↓reduceIte]
      by_cases h2 : (id' == id && off' == off + 1#64) = true
      · simp only [h2, ↓reduceIte]; bv_decide
      · simp only [h2, Bool.false_eq_true, ↓reduceIte]
        by_cases h3 : (id' == id && off' == off) = true
        · simp only [h3, ↓reduceIte]; bv_decide
        · simp only [h3, Bool.false_eq_true, ↓reduceIte]

private theorem DataDependencies_atomic_non_stack_spec
    {d0 dd : verifier.DataDependencies} {dst src : Std.U8} {is_cmpxchg : Bool}
    {res_b : core.result.Result Unit error.VerifierErrorKind}
    (h : (do
      let self1 ← if is_cmpxchg then verifier.DataDependencies.add_reg d0 0#u8 else ok d0
      let self2 ← verifier.DataDependencies.add_reg self1 src
      let self3 ← verifier.DataDependencies.add_reg self2 dst
      ok (core.result.Result.Ok (), self3) :
      Result ((core.result.Result Unit error.VerifierErrorKind) × verifier.DataDependencies)) =
        ok (res_b, dd)) :
    DataDepsHasReg dd dst.val = true ∧
    DataDepsHasReg dd src.val = true ∧
    (is_cmpxchg = true → DataDepsHasReg dd 0 = true) ∧
    DepsSubset d0 dd := by
  have h1_ex : ∃ self1,
      (if is_cmpxchg then verifier.DataDependencies.add_reg d0 0#u8 else ok d0) = ok self1 ∧
      DepsSubset d0 self1 ∧
      (is_cmpxchg = true → DataDepsHasReg self1 0 = true) := by
    cases is_cmpxchg with
    | false => exact ⟨d0, rfl, DepsSubset_refl d0, fun hc => by cases hc⟩
    | true =>
      simp only [↓reduceIte] at h ⊢
      rcases bind_eq_ok h with ⟨s1, ha0, _⟩
      obtain ⟨h0_lt16, _, hr0, _, hsub0⟩ := DataDependencies_add_reg_ok_spec ha0
      refine ⟨s1, ha0, hsub0, fun _ => ?_⟩
      have h0_val : (0#u8).val = 0 := rfl
      rw [← h0_val, hr0 (0#u8).val h0_lt16]; simp
  rcases h1_ex with ⟨self1, hs1, hsub1, hr0_1⟩
  rw [bind_ok_eq hs1] at h
  rcases bind_eq_ok h with ⟨self2, ha_src, h⟩
  rcases bind_eq_ok h with ⟨self3, ha_dst, h⟩
  rcases Prod.mk.inj (ok_inj h) with ⟨_, rfl⟩
  obtain ⟨hsrc_lt16, _, hr_src, _, hsub2⟩ := DataDependencies_add_reg_ok_spec ha_src
  obtain ⟨hdst_lt16, _, hr_dst, _, hsub3⟩ := DataDependencies_add_reg_ok_spec ha_dst
  refine ⟨?_, ?_, ?_, DepsSubset_trans hsub1 (DepsSubset_trans hsub2 hsub3)⟩
  · rw [hr_dst dst.val hdst_lt16]; simp
  · rw [hr_dst src.val hsrc_lt16, hr_src src.val hsrc_lt16]; simp
  · intro hcmp
    have h0 : DataDepsHasReg self1 0 = true := hr0_1 hcmp
    rw [hr_dst 0 (by omega), hr_src 0 (by omega), h0]; simp

/-! ## 2. Non-`cmpxchg` Atomic RMW (`atomic_operation` Forward & Backward) -/

private def nonCmpxchgAluOp (is64 : Bool) : AtomicOpKind → Option (Bool × verifier.ScalarAluOp)
  | .add f   => some (f, if is64 then .Add64 else .Add32)
  | .or  f   => some (f, if is64 then .Or64  else .Or32)
  | .and f   => some (f, if is64 then .And64 else .And32)
  | .xor f   => some (f, if is64 then .Xor64 else .Xor32)
  | .xchg    => some (true, .Xchg)
  | .cmpxchg => none

private theorem atomic_operation_ok_valid
    {env : ExecEnv} {ctx : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {log_op : verifier.AtomicLogOp} {width : api.DataWidth}
    {fetch : Bool} {dst src : Std.U8} {offset : Std.I16}
    {alu_op : verifier.ScalarAluOp}
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hop : verifier.ComputationContext.atomic_operation ctx log_op vctx0 width
      fetch dst offset src alu_op = ok (.Ok (), vctx1)) :
    ForwardVisitPost env vctx0 vctx1 ctx := by
  unfold verifier.ComputationContext.atomic_operation at hop
  rcases bind_eq_ok hop with ⟨vc1, hlog, hop⟩
  obtain ⟨hl1, hl2, hl3, hl4, hl5, hl6⟩ := log_atomic_operation_ok_spec hlog
  rcases bind_eq_ok hop with ⟨r, hreg_dst, hop⟩
  cases r with
  | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
  | Ok v =>
    dsimp only at hop
    obtain ⟨_, hv_valid⟩ := ComputationContext_reg_ok_valid hctx hreg_dst
    rcases bind_eq_ok hop with ⟨r1, hreg_src, hop⟩
    cases r1 with
    | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
    | Ok v1 =>
      dsimp only at hop
      obtain ⟨_, hv1_valid⟩ := ComputationContext_reg_ok_valid hctx hreg_src
      rw [bind_ok_eq (Field_new_spec offset width)] at hop
      rcases bind_eq_ok hop with ⟨r2, _, hop⟩
      cases r2 with
      | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
      | Ok _ =>
        dsimp only at hop
        rcases bind_eq_ok hop with ⟨⟨r3, vc2⟩, hlm, hop⟩
        cases r3 with
        | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
        | Ok v2 =>
          obtain ⟨hv2_valid, hm1, hm2, hm3, hm4, hm5, hm6⟩ :=
            ComputationContext_load_memory_ok_valid hctx hv_valid hlm
          rcases bind_eq_ok hop with ⟨o, hws2, hop⟩
          cases o with
          | none => cases (Prod.mk.inj (ok_inj hop)).1
          | some loaded_data =>
            dsimp only at hop
            obtain ⟨hv2_eq, _⟩ := Type_written_scalar_ok_some hws2
            have hld_valid : Scalar64Valid loaded_data := by rw [hv2_eq] at hv2_valid; exact hv2_valid
            rcases bind_eq_ok hop with ⟨o1, hws1, hop⟩
            cases o1 with
            | none => cases (Prod.mk.inj (ok_inj hop)).1
            | some value_data =>
              dsimp only at hop
              obtain ⟨hv1_eq, _⟩ := Type_written_scalar_ok_some hws1
              have hvd_valid : Scalar64Valid value_data := by rw [hv1_eq] at hv1_valid; exact hv1_valid
              rcases bind_eq_ok hop with ⟨usvd, heval, hop⟩
              have husvd_valid : TypeValid env vctx0.counter.bv.toNat (.ScalarValue usvd) :=
                ScalarAluOp_eval_valid hld_valid hvd_valid heval
              rcases bind_eq_ok hop with ⟨r4, hnext, hop⟩
              cases r4 with
              | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
              | Ok n =>
                dsimp only at hop
                obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext
                have hn_valid : CompCtxValid env vctx0.counter.bv.toNat n := by rw [hn_eq]; exact hctx
                rcases bind_eq_ok hop with ⟨⟨r5, n1, vc3⟩, hsm, hop⟩
                cases r5 with
                | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
                | Ok _ =>
                  obtain ⟨hn1_valid, hs1, hs2, hs3, hs4, hs5, hs6⟩ :=
                    ComputationContext_store_memory_ok_valid hn_valid husvd_valid hsm
                  cases fetch with
                  | true =>
                    simp only [↓reduceIte] at hop
                    rcases bind_eq_ok hop with ⟨⟨r6, n2⟩, hset, hop⟩
                    cases r6 with
                    | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
                    | Ok _ =>
                      obtain ⟨_, _, _, _, _, _, hn2_valid⟩ :=
                        ComputationContext_set_reg_ok_spec hn1_valid hv2_valid hset
                      rcases bind_eq_ok hop with ⟨vc4, hpush, hop⟩
                      rcases Prod.mk.inj (ok_inj hop) with ⟨_, rfl⟩
                      obtain ⟨hp1, hp2, hp3, hp4, hp5, _, hp7⟩ :=
                        VerificationContext_push_successor_ok_spec hpush
                      have hcnt : vc4.counter = vctx0.counter := hp3.trans (hs3.trans (hm3.trans hl3))
                      refine ⟨hp1.trans (hs1.trans (hm1.trans hl1)),
                        hp2.trans (hs2.trans (hm2.trans hl2)),
                        hp4.trans (hs4.trans (hm4.trans hl4)),
                        hp5.trans (hs5.trans (hm5.trans hl5)),
                        by rw [hcnt], by rwa [hcnt], ?_⟩
                      intro opt hopt
                      rw [hp7, hs6, hm6, hl6, hpending, List.nil_append, List.mem_singleton] at hopt
                      subst hopt
                      exact ⟨n2, rfl, by rwa [hcnt]⟩
                  | false =>
                    simp only [Bool.false_eq_true, ↓reduceIte] at hop
                    rcases bind_eq_ok hop with ⟨vc4, hpush, hop⟩
                    rcases Prod.mk.inj (ok_inj hop) with ⟨_, rfl⟩
                    obtain ⟨hp1, hp2, hp3, hp4, hp5, _, hp7⟩ :=
                      VerificationContext_push_successor_ok_spec hpush
                    have hcnt : vc4.counter = vctx0.counter := hp3.trans (hs3.trans (hm3.trans hl3))
                    refine ⟨hp1.trans (hs1.trans (hm1.trans hl1)),
                      hp2.trans (hs2.trans (hm2.trans hl2)),
                      hp4.trans (hs4.trans (hm4.trans hl4)),
                      hp5.trans (hs5.trans (hm5.trans hl5)),
                      by rw [hcnt], by rwa [hcnt], ?_⟩
                    intro opt hopt
                    rw [hp7, hs6, hm6, hl6, hpending, List.nil_append, List.mem_singleton] at hopt
                    subst hopt
                    exact ⟨n1, rfl, by rwa [hcnt]⟩

@[reducible] private def applyAtomicWb
    (regs : Nat → RuntimeVal) : Option (Nat × BitVec 64) → Nat → RuntimeVal
  | none => regs
  | some (r, v) => updateReg regs r (.scalar v)

private theorem StateRel2Deps_atomic_stack
    {env : ExecEnv} {ctx succ : verifier.ComputationContext}
    {dPre dPost : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    {slotIdx : Nat} {s' : verifier.Stack} {newSlot₁ newSlot₂ : StackSlot}
    {wb? : Option (Nat × BitVec 64)}
    {wbTy? : Option (Nat × verifier.Type)}
    (hslot_lt : slotIdx < 64)
    (hstk_eq : succ.stack = s')
    (hbm_eq : succ.array_bounds = ctx.array_bounds)
    (hres_eq : succ.resources = ctx.resources)
    (hslots_other : ∀ s < 64, s ≠ slotIdx → stackSlotType s' s = stackSlotType ctx.stack s)
    (hsub : DepsSubset dPost dPre)
    (hwb_match :
      (wb? = none ∧ wbTy? = none ∧ succ.registers = ctx.registers) ∨
      (∃ r wbVal wbTy,
        wb? = some (r, wbVal) ∧
        wbTy? = some (r, wbTy) ∧
        r < GENERAL_REGISTER_COUNT ∧
        succ.registers.val = ctx.registers.val.set r wbTy ∧
        TypeRel2 env ρ resActive isNullId st₁.world.arrayLen wbTy (.scalar wbVal) (.scalar wbVal)))
    (hrel : StateRel2Deps env ctx dPre ρ resActive isNullId st₁ st₂)
    (hnew_rel : DataDepsHasStack dPost slotIdx = true →
      StackSlotRel2 env ρ resActive isNullId st₁.world.arrayLen
        (stackSlotType s' slotIdx) newSlot₁ newSlot₂) :
    StateRel2Deps env succ dPost ρ resActive isNullId
      { pc := succ.pc.val, regs := applyAtomicWb st₁.regs wb?,
        stack := updateStack st₁.stack slotIdx newSlot₁, world := bumpWorldStep st₁.world }
      { pc := succ.pc.val, regs := applyAtomicWb st₂.regs wb?,
        stack := updateStack st₂.stack slotIdx newSlot₂, world := bumpWorldStep st₂.world } := by
  rcases hrel with ⟨_, _, hw, harr, hnull, hnodup, hlive, hbnds, hr10_1, hr10_2, hregs, hstk⟩
  have hr10_post :
      applyAtomicWb st₁.regs wb? GENERAL_REGISTER_COUNT = .stackPtr 512#64 ∧
      applyAtomicWb st₂.regs wb? GENERAL_REGISTER_COUNT = .stackPtr 512#64 := by
    rcases hwb_match with ⟨rfl, _, _⟩ | ⟨r, wbVal, _, rfl, _, hr_lt10, _, _⟩
    · exact ⟨hr10_1, hr10_2⟩
    · have hne : (GENERAL_REGISTER_COUNT == r) = false := beq_false_of_ne (by omega)
      simp only [applyAtomicWb, updateReg, hne, Bool.false_eq_true, ↓reduceIte]
      exact ⟨hr10_1, hr10_2⟩
  refine ⟨rfl, rfl, by simp [bumpWorldStep, hw], harr, hnull, hnodup,
    by simpa [bumpWorldStep, hres_eq] using hlive,
    by simpa [bumpWorldStep, hbm_eq] using hbnds,
    hr10_post.1, hr10_post.2, ?_, ?_⟩
  · intro r hr hlen hd
    rcases hwb_match with ⟨rfl, _, hregs_eq⟩ | ⟨r_wb, wbVal, wbTy, rfl, _, hr_wb_lt10, hregs_set, hwb_rel⟩
    · have hlen_pre : r < ctx.registers.val.length := by rwa [← hregs_eq]
      have hget : (succ.registers.val[r]'hlen) = (ctx.registers.val[r]'hlen_pre) := by simp [hregs_eq]
      rw [hget]
      exact hregs r hr hlen_pre (hsub.1 r hr hd)
    · have hlen_pre : r < ctx.registers.val.length := by
        rw [hregs_set, List.length_set] at hlen; exact hlen
      by_cases heq : r = r_wb
      · subst heq
        have hget : (succ.registers.val[r]'hlen) = wbTy := by simp [hregs_set]
        rw [hget]
        simp only [applyAtomicWb, updateReg, beq_self_eq_true, ↓reduceIte, bumpWorldStep]
        exact hwb_rel
      · have hbeq : (r == r_wb) = false := beq_false_of_ne heq
        have hget : (succ.registers.val[r]'hlen) = (ctx.registers.val[r]'hlen_pre) := by
          simp [hregs_set, List.getElem_set_ne (Ne.symm heq)]
        rw [hget]
        simp only [applyAtomicWb, updateReg, hbeq, Bool.false_eq_true, ↓reduceIte, bumpWorldStep]
        exact hregs r hr hlen_pre (hsub.1 r hr hd)
  · intro s hs hd
    rw [hstk_eq]
    by_cases heq : s = slotIdx
    · subst heq
      simp only [updateStack, beq_self_eq_true, ↓reduceIte, bumpWorldStep]
      exact hnew_rel hd
    · have hbeq : (s == slotIdx) = false := beq_false_of_ne heq
      simp only [updateStack, hbeq, Bool.false_eq_true, ↓reduceIte, bumpWorldStep]
      rw [hslots_other s hs heq]
      exact hstk s hs (hsub.2 s hs hd)

private theorem StateRel2Deps_atomic_mem
    {env : ExecEnv} {ctx succ : verifier.ComputationContext}
    {dPre dPost : verifier.DataDependencies} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {st₁ st₂ : MachineState}
    {w₁' w₂' : WorldState}
    {wb? : Option (Nat × BitVec 64)}
    {wbTy? : Option (Nat × verifier.Type)}
    (hstk_eq : succ.stack = ctx.stack)
    (hbm_eq : succ.array_bounds = ctx.array_bounds)
    (hres_eq : succ.resources = ctx.resources)
    (hw_eq : w₁' = w₂')
    (harr_eq : w₁'.arrayLen = st₁.world.arrayLen)
    (hnull_eq : w₁'.structFieldNull = st₁.world.structFieldNull)
    (hlive_eq : w₁'.liveResources = st₁.world.liveResources)
    (hsub : DepsSubset dPost dPre)
    (hwb_match :
      (wb? = none ∧ wbTy? = none ∧ succ.registers = ctx.registers) ∨
      (∃ r wbVal wbTy,
        wb? = some (r, wbVal) ∧
        wbTy? = some (r, wbTy) ∧
        r < GENERAL_REGISTER_COUNT ∧
        succ.registers.val = ctx.registers.val.set r wbTy ∧
        TypeRel2 env ρ resActive isNullId st₁.world.arrayLen wbTy (.scalar wbVal) (.scalar wbVal)))
    (hrel : StateRel2Deps env ctx dPre ρ resActive isNullId st₁ st₂) :
    StateRel2Deps env succ dPost ρ resActive isNullId
      { pc := succ.pc.val, regs := applyAtomicWb st₁.regs wb?, stack := st₁.stack, world := bumpWorldStep w₁' }
      { pc := succ.pc.val, regs := applyAtomicWb st₂.regs wb?, stack := st₂.stack, world := bumpWorldStep w₂' } := by
  rcases hrel with ⟨_, _, _, harr, hnull, hnodup, hlive, hbnds, hr10_1, hr10_2, hregs, hstk⟩
  have hr10_post :
      applyAtomicWb st₁.regs wb? GENERAL_REGISTER_COUNT = .stackPtr 512#64 ∧
      applyAtomicWb st₂.regs wb? GENERAL_REGISTER_COUNT = .stackPtr 512#64 := by
    rcases hwb_match with ⟨rfl, _, _⟩ | ⟨r, wbVal, _, rfl, _, hr_lt10, _, _⟩
    · exact ⟨hr10_1, hr10_2⟩
    · have hne : (GENERAL_REGISTER_COUNT == r) = false := beq_false_of_ne (by omega)
      simp only [applyAtomicWb, updateReg, hne, Bool.false_eq_true, ↓reduceIte]
      exact ⟨hr10_1, hr10_2⟩
  refine ⟨rfl, rfl, by simp [bumpWorldStep, hw_eq],
    by simpa [bumpWorldStep, harr_eq] using harr,
    by simpa [bumpWorldStep, hnull_eq] using hnull,
    by simpa [bumpWorldStep, hlive_eq] using hnodup,
    by simpa [bumpWorldStep, hres_eq, hlive_eq] using hlive,
    by simpa [bumpWorldStep, hbm_eq, harr_eq] using hbnds,
    hr10_post.1, hr10_post.2, ?_, ?_⟩
  · intro r hr hlen hd
    rcases hwb_match with ⟨rfl, _, hregs_eq⟩ | ⟨r_wb, wbVal, wbTy, rfl, _, _, hregs_set, hwb_rel⟩
    · have hlen_pre : r < ctx.registers.val.length := by rwa [← hregs_eq]
      have hget : (succ.registers.val[r]'hlen) = (ctx.registers.val[r]'hlen_pre) := by simp [hregs_eq]
      rw [hget]
      simpa [bumpWorldStep, harr_eq] using hregs r hr hlen_pre (hsub.1 r hr hd)
    · have hlen_pre : r < ctx.registers.val.length := by
        rw [hregs_set, List.length_set] at hlen; exact hlen
      by_cases heq : r = r_wb
      · subst heq
        have hget : (succ.registers.val[r]'hlen) = wbTy := by simp [hregs_set]
        rw [hget]
        simpa [applyAtomicWb, updateReg, bumpWorldStep, harr_eq] using hwb_rel
      · have hbeq : (r == r_wb) = false := beq_false_of_ne heq
        have hget : (succ.registers.val[r]'hlen) = (ctx.registers.val[r]'hlen_pre) := by
          simp [hregs_set, List.getElem_set_ne (Ne.symm heq)]
        rw [hget]
        simp only [applyAtomicWb, updateReg, hbeq, Bool.false_eq_true, ↓reduceIte, bumpWorldStep, harr_eq]
        exact hregs r hr hlen_pre (hsub.1 r hr hd)
  · intro s hs hd
    rw [hstk_eq]
    simpa [bumpWorldStep, harr_eq] using hstk s hs (hsub.2 s hs hd)

@[reducible] private def evalNonCmpxchgRmw
    (is64 : Bool) (op : AtomicOpKind) (oldVal sVal : BitVec 64) :
    BitVec 64 × Option (Nat × BitVec 64) :=
  if is64 then
    match op with
    | .add f   => (oldVal + sVal, if f then some (0, oldVal) else none)
    | .or  f   => (oldVal ||| sVal, if f then some (0, oldVal) else none)
    | .and f   => (oldVal &&& sVal, if f then some (0, oldVal) else none)
    | .xor f   => (oldVal ^^^ sVal, if f then some (0, oldVal) else none)
    | .xchg    => (sVal, some (0, oldVal))
    | .cmpxchg => (oldVal, none)
  else
    let o32 := oldVal.setWidth 32
    let s32 := sVal.setWidth 32
    let zext (v : BitVec 32) := v.zeroExtend 64
    match op with
    | .add f   => (zext (o32 + s32), if f then some (0, zext o32) else none)
    | .or  f   => (zext (o32 ||| s32), if f then some (0, zext o32) else none)
    | .and f   => (zext (o32 &&& s32), if f then some (0, zext o32) else none)
    | .xor f   => (zext (o32 ^^^ s32), if f then some (0, zext o32) else none)
    | .xchg    => (zext s32, some (0, zext o32))
    | .cmpxchg => (zext o32, none)

private theorem atomic_alu_eval_sound
    {is64 : Bool} {op : AtomicOpKind} {fetch : Bool} {alu_op : verifier.ScalarAluOp}
    (hkind : nonCmpxchgAluOp is64 op = some (fetch, alu_op))
    {loaded_data value_data usvd : U64ScalarValueData}
    {oldVal sVal : BitVec 64}
    (hconc_ld : ConcretizesScalar64Rel2 loaded_data oldVal oldVal)
    (hconc_vd : ConcretizesScalar64Rel2 value_data sVal sVal)
    (heval : verifier.ScalarAluOp.eval alu_op loaded_data value_data = ok usvd) :
    let w : DataWidth := if is64 then .u64 else .u32
    ∃ effStored : BitVec 64,
      ConcretizesScalar64Rel2 usvd effStored effStored ∧
      (∀ orig subIdx, insertSubValue orig (evalNonCmpxchgRmw is64 op oldVal sVal).1 subIdx w =
        insertSubValue orig effStored subIdx w) ∧
      (∀ buf id off, writeBufWord buf id off w (evalNonCmpxchgRmw is64 op oldVal sVal).1 =
        writeBufWord buf id off w effStored) := by
  dsimp only
  cases op with
  | cmpxchg => cases hkind
  | add f =>
    dsimp [nonCmpxchgAluOp] at hkind
    rcases Prod.mk.inj (Option.some.inj hkind) with ⟨rfl, rfl⟩
    cases is64 with
    | true =>
      simp only [↓reduceIte] at heval
      obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety .add64 hconc_ld hconc_vd
      rw [heval] at hres; cases ok_inj hres
      exact ⟨oldVal + sVal, hc, fun _ _ => rfl, fun _ _ _ => rfl⟩
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at heval
      obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety .add32 hconc_ld hconc_vd
      rw [heval] at hres; cases ok_inj hres
      exact ⟨((oldVal.setWidth 32 + sVal.setWidth 32).zeroExtend 64), hc, fun _ _ => rfl, fun _ _ _ => rfl⟩
  | or f =>
    dsimp [nonCmpxchgAluOp] at hkind
    rcases Prod.mk.inj (Option.some.inj hkind) with ⟨rfl, rfl⟩
    cases is64 with
    | true =>
      simp only [↓reduceIte] at heval
      obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety .or64 hconc_ld hconc_vd
      rw [heval] at hres; cases ok_inj hres
      exact ⟨oldVal ||| sVal, hc, fun _ _ => rfl, fun _ _ _ => rfl⟩
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at heval
      obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety .or32 hconc_ld hconc_vd
      rw [heval] at hres; cases ok_inj hres
      exact ⟨((oldVal.setWidth 32 ||| sVal.setWidth 32).zeroExtend 64), hc, fun _ _ => rfl, fun _ _ _ => rfl⟩
  | and f =>
    dsimp [nonCmpxchgAluOp] at hkind
    rcases Prod.mk.inj (Option.some.inj hkind) with ⟨rfl, rfl⟩
    cases is64 with
    | true =>
      simp only [↓reduceIte] at heval
      obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety .and64 hconc_ld hconc_vd
      rw [heval] at hres; cases ok_inj hres
      exact ⟨oldVal &&& sVal, hc, fun _ _ => rfl, fun _ _ _ => rfl⟩
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at heval
      obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety .and32 hconc_ld hconc_vd
      rw [heval] at hres; cases ok_inj hres
      exact ⟨((oldVal.setWidth 32 &&& sVal.setWidth 32).zeroExtend 64), hc, fun _ _ => rfl, fun _ _ _ => rfl⟩
  | xor f =>
    dsimp [nonCmpxchgAluOp] at hkind
    rcases Prod.mk.inj (Option.some.inj hkind) with ⟨rfl, rfl⟩
    cases is64 with
    | true =>
      simp only [↓reduceIte] at heval
      obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety .xor64 hconc_ld hconc_vd
      rw [heval] at hres; cases ok_inj hres
      exact ⟨oldVal ^^^ sVal, hc, fun _ _ => rfl, fun _ _ _ => rfl⟩
    | false =>
      simp only [Bool.false_eq_true, ↓reduceIte] at heval
      obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety .xor32 hconc_ld hconc_vd
      rw [heval] at hres; cases ok_inj hres
      exact ⟨((oldVal.setWidth 32 ^^^ sVal.setWidth 32).zeroExtend 64), hc, fun _ _ => rfl, fun _ _ _ => rfl⟩
  | xchg =>
    dsimp [nonCmpxchgAluOp] at hkind
    rcases Prod.mk.inj (Option.some.inj hkind) with ⟨rfl, rfl⟩
    dsimp [verifier.ScalarAluOp.eval] at heval
    cases ok_inj heval
    cases is64 with
    | true =>
      exact ⟨sVal, hconc_vd, fun _ _ => rfl, fun _ _ _ => rfl⟩
    | false =>
      exact ⟨sVal, hconc_vd,
        fun orig subIdx => insertSubValue_u32_zext orig sVal subIdx,
        fun buf id off => writeBufWord_u32_zext buf id off sVal⟩

private theorem atomic_operation_backward
    {prog : Program} {env : ExecEnv} {vCounter F : Nat}
    {ctx : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {dPost dd : verifier.DataDependencies}
    {is64 : Bool} {op : AtomicOpKind} {log_op : verifier.AtomicLogOp}
    {fetch : Bool} {dst8 src8 : Std.U8} {off : Std.I16}
    {alu_op : verifier.ScalarAluOp}
    {res_b : core.result.Result Unit error.VerifierErrorKind}
    (hlt_pc : ctx.pc.val < prog.size)
    (hinsn_at : prog[ctx.pc.val]'hlt_pc = .atomic is64 op dst8.val off.bv src8.val)
    (hkind : nonCmpxchgAluOp is64 op = some (fetch, alu_op))
    (hctx : CompCtxValid env vCounter ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hop : verifier.ComputationContext.atomic_operation ctx log_op vctx0
      (if is64 then .U64 else .U32) fetch dst8 off src8 alu_op = ok (.Ok (), vctx1))
    (hchildren : ∀ succ, some succ ∈ vctx1.pending_successors.val →
      SemTerminatesIn prog env vCounter succ dPost F)
    (hback : verifier.DataDependencies.atomic dPost ctx dst8 off src8 false = ok (res_b, dd)) :
    SemTerminatesIn prog env vCounter ctx dd (F + 1) := by
  have hneq_cmp : (op == .cmpxchg) = false := by
    cases op <;> simp [nonCmpxchgAluOp] at hkind ⊢
  unfold verifier.ComputationContext.atomic_operation at hop
  rcases bind_eq_ok hop with ⟨vc1, hlog, hop⟩
  obtain ⟨_, _, _, _, _, hl6⟩ := log_atomic_operation_ok_spec hlog
  rcases bind_eq_ok hop with ⟨r, hreg_dst, hop⟩
  cases r with
  | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
  | Ok v =>
    dsimp only at hop
    obtain ⟨hdst_lt, hv_valid⟩ := ComputationContext_reg_ok_valid hctx hreg_dst
    rcases bind_eq_ok hop with ⟨r1, hreg_src, hop⟩
    cases r1 with
    | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
    | Ok v1 =>
      dsimp only at hop
      obtain ⟨hsrc_lt, hv1_valid⟩ := ComputationContext_reg_ok_valid hctx hreg_src
      rw [bind_ok_eq (Field_new_spec off (if is64 then .U64 else .U32))] at hop
      rcases bind_eq_ok hop with ⟨r2, hcaa, hop⟩
      cases r2 with
      | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
      | Ok u_caa =>
        cases u_caa
        dsimp only at hop
        obtain ⟨hinner, hv_ne_uninit, hcaa_cases⟩ := check_atomic_access_ok_spec hcaa
        rcases bind_eq_ok hop with ⟨⟨r3, vc2⟩, hlm, hop⟩
        cases r3 with
        | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
        | Ok v2 =>
          obtain ⟨hv2_valid, _, _, _, _, _, hm6⟩ :=
            ComputationContext_load_memory_ok_valid hctx hv_valid hlm
          rcases bind_eq_ok hop with ⟨o, hws2, hop⟩
          cases o with
          | none => cases (Prod.mk.inj (ok_inj hop)).1
          | some loaded_data =>
            dsimp only at hop
            obtain ⟨hv2_eq, hld_unw⟩ := Type_written_scalar_ok_some hws2
            have hld_valid : Scalar64Valid loaded_data := by rw [hv2_eq] at hv2_valid; exact hv2_valid
            rcases bind_eq_ok hop with ⟨o1, hws1, hop⟩
            cases o1 with
            | none => cases (Prod.mk.inj (ok_inj hop)).1
            | some value_data =>
              dsimp only at hop
              obtain ⟨hv1_eq, hvd_unw⟩ := Type_written_scalar_ok_some hws1
              have hvd_valid : Scalar64Valid value_data := by rw [hv1_eq] at hv1_valid; exact hv1_valid
              rcases bind_eq_ok hop with ⟨usvd, heval, hop⟩
              have husvd_valid : TypeValid env vCounter (.ScalarValue usvd) :=
                ScalarAluOp_eval_valid hld_valid hvd_valid heval
              rcases bind_eq_ok hop with ⟨r4, hnext, hop⟩
              cases r4 with
              | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
              | Ok n =>
                dsimp only at hop
                obtain ⟨hpc_next, hn_eq⟩ := ComputationContext_next_ok_spec hnext
                have hn_valid : CompCtxValid env vCounter n := by rw [hn_eq]; exact hctx
                rcases bind_eq_ok hop with ⟨⟨r5, n1, vc3⟩, hsm, hop⟩
                cases r5 with
                | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
                | Ok u_sm =>
                  cases u_sm
                  obtain ⟨hn1_valid, _, _, _, _, _, hs6⟩ :=
                    ComputationContext_store_memory_ok_valid hn_valid husvd_valid hsm
                  -- Extract uniform successor `succ` for `fetch = true` vs `fetch = false`
                  have hsucc_ex : ∃ succ : verifier.ComputationContext,
                      SemTerminatesIn prog env vCounter succ dPost F ∧
                      succ.pc = n.pc ∧
                      succ.stack = n1.stack ∧
                      succ.array_bounds = n1.array_bounds ∧
                      succ.resources = n1.resources ∧
                      (fetch = true → src8.val < GENERAL_REGISTER_COUNT ∧
                        succ.registers.val = n1.registers.val.set src8.val v2) ∧
                      (fetch = false → succ.registers = n1.registers) := by
                    cases fetch with
                    | true =>
                      simp only [↓reduceIte] at hop
                      rcases bind_eq_ok hop with ⟨⟨r6, n2⟩, hset, hop⟩
                      cases r6 with
                      | Err _ => cases (Prod.mk.inj (ok_inj hop)).1
                      | Ok _ =>
                        obtain ⟨hsrc_lt10, hpc_n2, hregs_n2, hstk_n2, hbm_n2, hres_n2, _⟩ :=
                          ComputationContext_set_reg_ok_spec hn1_valid hv2_valid hset
                        rcases bind_eq_ok hop with ⟨vc4, hpush, hop⟩
                        rcases Prod.mk.inj (ok_inj hop) with ⟨_, rfl⟩
                        obtain ⟨_, _, _, _, _, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
                        have hmem : some n2 ∈ vc4.pending_successors.val := by
                          rw [hp7, hs6, hm6, hl6, hpending]; simp
                        have hpc_n1 : n1.pc = n.pc := by
                          unfold verifier.ComputationContext.store_memory at hsm
                          rcases bind_eq_ok hsm with ⟨rc, _, hsm⟩
                          cases rc with
                          | Err _ => cases (Prod.mk.inj (ok_inj hsm)).1
                          | Ok _ =>
                            dsimp only at hsm
                            rw [bind_ok_eq (Type_unwrap_releasable_spec v)] at hsm
                            rcases bind_eq_ok hsm with ⟨is_i, _, hsm⟩
                            cases hu0 : pureUnwrapReleasable v <;> rw [hu0] at hsm
                            all_goals try (cases (Prod.mk.inj (ok_inj hsm)).1)
                            · dsimp only at hsm
                              rcases bind_eq_ok hsm with ⟨_, _, hsm⟩
                              rcases bind_eq_ok hsm with ⟨⟨rs, _⟩, _, hsm⟩
                              cases rs with
                              | Err _ => cases (Prod.mk.inj (ok_inj hsm)).1
                              | Ok _ =>
                                rcases bind_eq_ok hsm with ⟨_, _, hsm⟩
                                rcases Prod.mk.inj (Prod.mk.inj (ok_inj hsm)).2 with ⟨rfl, _⟩; rfl
                            · rename_i _ _ _ orig1
                              dsimp only at hsm
                              rw [bind_ok_eq (PointerOrigin_is_writable_spec orig1)] at hsm
                              cases hmw1 : (MemoryOrigin.ofCore orig1).isWritable <;>
                                simp only [hmw1, Bool.false_eq_true, ↓reduceIte] at hsm
                              · cases (Prod.mk.inj (ok_inj hsm)).1
                              · rcases bind_eq_ok hsm with ⟨_, _, hsm⟩
                                rcases bind_eq_ok hsm with ⟨rc2, _, hsm⟩
                                cases rc2 with
                                | Err _ => cases (Prod.mk.inj (ok_inj hsm)).1
                                | Ok _ =>
                                  cases is_i <;> simp only [Bool.false_eq_true, ↓reduceIte] at hsm
                                  · cases (Prod.mk.inj (ok_inj hsm)).1
                                  · rcases bind_eq_ok hsm with ⟨_, _, hsm⟩
                                    rcases Prod.mk.inj (Prod.mk.inj (ok_inj hsm)).2 with ⟨rfl, _⟩; rfl
                            · rcases hcaa_cases with ⟨_, h1, _⟩ | ⟨_, _, _, _, h1, _⟩ <;>
                                (rw [hu0] at h1; cases h1)
                            · rcases hcaa_cases with ⟨_, h1, _⟩ | ⟨_, _, _, _, h1, _⟩ <;>
                                (rw [hu0] at h1; cases h1)
                        exact ⟨n2, hchildren n2 hmem, hpc_n2.trans hpc_n1, hstk_n2, hbm_n2, hres_n2,
                          (fun _ => ⟨hsrc_lt10, hregs_n2⟩), (fun hc => by cases hc)⟩
                    | false =>
                      simp only [Bool.false_eq_true, ↓reduceIte] at hop
                      rcases bind_eq_ok hop with ⟨vc4, hpush, hop⟩
                      rcases Prod.mk.inj (ok_inj hop) with ⟨_, rfl⟩
                      obtain ⟨_, _, _, _, _, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
                      have hmem : some n1 ∈ vc4.pending_successors.val := by
                        rw [hp7, hs6, hm6, hl6, hpending]; simp
                      have hpc_n1 : n1.pc = n.pc := by
                        unfold verifier.ComputationContext.store_memory at hsm
                        rcases bind_eq_ok hsm with ⟨rc, _, hsm⟩
                        cases rc with
                        | Err _ => cases (Prod.mk.inj (ok_inj hsm)).1
                        | Ok _ =>
                          dsimp only at hsm
                          rw [bind_ok_eq (Type_unwrap_releasable_spec v)] at hsm
                          rcases bind_eq_ok hsm with ⟨is_i, _, hsm⟩
                          cases hu0 : pureUnwrapReleasable v <;> rw [hu0] at hsm
                          all_goals try (cases (Prod.mk.inj (ok_inj hsm)).1)
                          · dsimp only at hsm
                            rcases bind_eq_ok hsm with ⟨_, _, hsm⟩
                            rcases bind_eq_ok hsm with ⟨⟨rs, _⟩, _, hsm⟩
                            cases rs with
                            | Err _ => cases (Prod.mk.inj (ok_inj hsm)).1
                            | Ok _ =>
                              rcases bind_eq_ok hsm with ⟨_, _, hsm⟩
                              rcases Prod.mk.inj (Prod.mk.inj (ok_inj hsm)).2 with ⟨rfl, _⟩; rfl
                          · rename_i _ _ _ orig2
                            dsimp only at hsm
                            rw [bind_ok_eq (PointerOrigin_is_writable_spec orig2)] at hsm
                            cases hmw2 : (MemoryOrigin.ofCore orig2).isWritable <;>
                              simp only [hmw2, Bool.false_eq_true, ↓reduceIte] at hsm
                            · cases (Prod.mk.inj (ok_inj hsm)).1
                            · rcases bind_eq_ok hsm with ⟨_, _, hsm⟩
                              rcases bind_eq_ok hsm with ⟨rc2, _, hsm⟩
                              cases rc2 with
                              | Err _ => cases (Prod.mk.inj (ok_inj hsm)).1
                              | Ok _ =>
                                cases is_i <;> simp only [Bool.false_eq_true, ↓reduceIte] at hsm
                                · cases (Prod.mk.inj (ok_inj hsm)).1
                                · rcases bind_eq_ok hsm with ⟨_, _, hsm⟩
                                  rcases Prod.mk.inj (Prod.mk.inj (ok_inj hsm)).2 with ⟨rfl, _⟩; rfl
                          · rcases hcaa_cases with ⟨_, h1, _⟩ | ⟨_, _, _, _, h1, _⟩ <;>
                              (rw [hu0] at h1; cases h1)
                          · rcases hcaa_cases with ⟨_, h1, _⟩ | ⟨_, _, _, _, h1, _⟩ <;>
                              (rw [hu0] at h1; cases h1)
                      exact ⟨n1, hchildren n1 hmem, hpc_n1, rfl, rfl, rfl,
                        (fun hc => by cases hc), (fun _ => rfl)⟩
                  rcases hsucc_ex with ⟨succ, hsem, hsucc_pc, hsucc_stk, hsucc_bm, hsucc_res, hsucc_t, hsucc_f⟩
                  have hsucc_pc_val : succ.pc.val = ctx.pc.val + 1 := by rw [hsucc_pc, hpc_next]
                  unfold verifier.DataDependencies.atomic at hback
                  rw [bind_ok_eq hreg_dst] at hback
                  dsimp only at hback
                  rw [bind_ok_eq hinner] at hback
                  dsimp only at hback
                  have hn_dst_ge : ¬ (dst8.val ≥ REGISTER_COUNT) := by omega
                  have hn_src_ge : ¬ (src8.val ≥ REGISTER_COUNT) := by omega
                  have hn_ro_fp : (fetch && decide (src8.val ≥ GENERAL_REGISTER_COUNT)) = false := by
                    cases fetch with
                    | false => rfl
                    | true =>
                      have hlt10 := (hsucc_t rfl).1
                      simp [Nat.not_le.mpr hlt10]
                  have hvd_ne_uninit : value_data ≠ uninitScalar64 := fun hc => by
                    subst hc; exact uninitScalar64_unwritten_ne_zero hvd_unw
                  have hld_ne_uninit : loaded_data ≠ uninitScalar64 := fun hc => by
                    subst hc; exact uninitScalar64_unwritten_ne_zero hld_unw
                  -- Unpack load_memory and store_memory across the 2 valid atomic regions:
                  unfold verifier.ComputationContext.load_memory at hlm
                  rw [bind_ok_eq hinner] at hlm
                  dsimp only at hlm
                  unfold verifier.ComputationContext.store_memory at hsm
                  rcases bind_eq_ok hsm with ⟨rc_sm, _, hsm⟩
                  cases rc_sm with
                  | Err _ => cases (Prod.mk.inj (ok_inj hsm)).1
                  | Ok _ =>
                    dsimp only at hsm
                    rw [bind_ok_eq (Type_unwrap_releasable_spec v)] at hsm
                    rcases bind_eq_ok hsm with ⟨is_init_sm, _, hsm⟩
                    rcases hcaa_cases with ⟨stack_offset, hu_stk, hal_stk⟩ |
                      ⟨id, offset, buffer_size, origin, hu_mem, hmap_orig, hal_mem⟩
                    · -- Case 1: PtrToStack
                      rw [hu_stk] at hback hlm hsm
                      dsimp only at hback hlm hsm
                      rcases bind_eq_ok hlm with ⟨offset_sum, hadd_off, hlm⟩
                      rw [bind_ok_eq hadd_off] at hback hsm
                      rcases bind_eq_ok hlm with ⟨r_sl, hsl, hlm⟩
                      cases r_sl with
                      | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
                      | Ok v_sl =>
                        dsimp only at hlm
                        rcases bind_eq_ok hlm with ⟨⟨r_ma_l, _⟩, _, hlm⟩
                        cases r_ma_l with
                        | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
                        | Ok _ =>
                          have hv_sl_eq : v_sl = v2 := core.result.Result.Ok.inj (Prod.mk.inj (ok_inj hlm)).1
                          subst v_sl
                          rw [hv2_eq] at hsl
                          rcases bind_eq_ok hsm with ⟨⟨r_st, s'⟩, hst_op, hsm⟩
                          cases r_st with
                          | Err _ => cases (Prod.mk.inj (ok_inj hsm)).1
                          | Ok _ =>
                            rcases bind_eq_ok hsm with ⟨_, _, hsm⟩
                            rcases Prod.mk.inj (Prod.mk.inj (ok_inj hsm)).2 with ⟨hn1_eq, _⟩
                            have hn_stk_eq : n.stack = ctx.stack := by rw [hn_eq]
                            rw [hn_stk_eq] at hst_op
                            obtain ⟨r0_st, s0_st, h0_st, _, _, hpost_st⟩ :=
                              Stack_store_spec hctx.2.1 offset_sum husvd_valid (if is64 then .U64 else .U32)
                            rw [hst_op] at h0_st
                            rcases Prod.mk.inj (ok_inj h0_st) with ⟨r0_eq, hs0_eq⟩
                            subst hs0_eq
                            obtain ⟨hoff_kn, hoff_lt, _, hslots_other, _, _⟩ := hpost_st () r0_eq.symm
                            obtain ⟨rchk0, hrchk0, hchk_iff⟩ := StackOffset_check_valid_offset_spec offset_sum
                            have hrchk_ok : rchk0 = .Ok () := (hchk_iff ()).mpr ⟨hoff_kn, hoff_lt⟩
                            subst hrchk_ok
                            rw [bind_ok_eq hrchk0] at hback
                            dsimp only at hback
                            obtain ⟨slot_u, hslot_u, hslot_val⟩ := StackOffset_array_index_spec (BitVec.le_of_lt hoff_lt)
                            have hoff_512 : offset_sum.value.bv.toNat < 512 := by
                              rw [BitVec.lt_def] at hoff_lt; simpa using hoff_lt
                            have hslot_lt64 : slot_u.val < 64 := by omega
                            rw [bind_ok_eq hslot_u] at hback
                            rcases bind_eq_ok hback with ⟨dd_stk, hadd_stk, hback⟩
                            obtain ⟨_, _, _, hstk_add_eq, hsub_stk⟩ := DataDependencies_add_stack_ok_spec hadd_stk
                            obtain ⟨hdst_live, hsrc_live, _, hsub_regs⟩ :=
                              DataDependencies_atomic_non_stack_spec hback
                            have hslot_live : DataDepsHasStack dd slot_u.val = true :=
                              hsub_regs.2 slot_u.val hslot_lt64 (by rw [hstk_add_eq slot_u.val hslot_lt64]; simp)
                            have hsub_all : DepsSubset dPost dd := DepsSubset_trans hsub_stk hsub_regs
                            intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
                            obtain ⟨_, _, hrel_dst⟩ :=
                              ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hdst_live) hreg_dst
                            obtain ⟨_, _, hca_dst1, hca_dst2⟩ :=
                              Type_inner_ok_valid_and_checkAlive_none hctx hrel hv_valid hv_ne_uninit hinner hrel_dst
                            obtain ⟨_, _, baseOff, hconc_base, hv_base1, hv_base2⟩ :=
                              unwrap_stack_ptr_rel2 hv_valid hu_stk hrel_dst
                            obtain ⟨_, _, hrel_src⟩ :=
                              ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hsrc_live) hreg_src
                            rw [hv1_eq] at hrel_src
                            simp only [TypeRel2, hvd_ne_uninit, ↓reduceIte] at hrel_src
                            rcases hrel_src with ⟨sVal₁, sVal₂, hv_src1, hv_src2, hconc_vd⟩
                            have hsVal_eq := fully_initialized_eq_of_rel2_64 hvd_unw hconc_vd
                            subst hsVal_eq
                            obtain ⟨off_s, hadd_s, hconc_sum⟩ :=
                              StackOffset_add_i16_sound_2safety stack_offset off hconc_base
                            rw [hadd_off] at hadd_s; cases ok_inj hadd_s
                            have heff_eq : baseOff + sextOffset16 off.bv = offset_sum.value.bv :=
                              eq_value_of_known_64 hoff_kn hconc_sum.1
                            have hslot_idx_eq : (baseOff + sextOffset16 off.bv).toNat / 8 = slot_u.val := by
                              rw [heff_eq, hslot_val]
                            have hslot_rel_pre := hrel.2.2.2.2.2.2.2.2.2.2.2
                              ((baseOff + sextOffset16 off.bv).toNat / 8)
                              (by rw [hslot_idx_eq]; exact hslot_lt64)
                              (by rw [hslot_idx_eq]; exact hslot_live)
                            obtain ⟨heff_lt512, oldSlot₁, initMask₁, oldSlot₂, initMask₂,
                                hstk1, hstk2, hinit1, hinit2, hold_eq, hconc_ld⟩ :=
                              Stack_load_written_scalar_rel2 hctx.2.1 hld_unw hconc_sum hsl hslot_rel_pre
                            let w : DataWidth := if is64 then .u64 else .u32
                            have hw_core : DataWidth.ofCore (if is64 then api.DataWidth.U64 else .U32) = w := by
                              cases is64 <;> rfl
                            rw [hw_core] at hinit1 hinit2 hold_eq hconc_ld
                            let effOff := baseOff + sextOffset16 off.bv
                            let slotIdx := effOff.toNat / 8
                            let subIdx := effOff.toNat % 8
                            let oldSlice := extractSubValue oldSlot₁ subIdx w
                            obtain ⟨effStored, hconc_eff, hins_eq, _⟩ :=
                              atomic_alu_eval_sound hkind hconc_ld hconc_vd heval
                            have hwb_old_eq : (if is64 then oldSlice else (oldSlice.setWidth 32).zeroExtend 64) = oldSlice := by
                              cases is64 with
                              | true => rfl
                              | false => exact extractSubValue_u32_zext oldSlot₁ subIdx
                            let wb? : Option (Nat × BitVec 64) :=
                              if fetch then some (src8.val, oldSlice) else none
                            let wbTy? : Option (Nat × verifier.Type) :=
                              if fetch then some (src8.val, v2) else none
                            have hwb_match :
                                (wb? = none ∧ wbTy? = none ∧ succ.registers = ctx.registers) ∨
                                (∃ r wbVal wbTy,
                                  wb? = some (r, wbVal) ∧
                                  wbTy? = some (r, wbTy) ∧
                                  r < GENERAL_REGISTER_COUNT ∧
                                  succ.registers.val = ctx.registers.val.set r wbTy ∧
                                  TypeRel2 env ρ resActive isNullId st₁.world.arrayLen wbTy (.scalar wbVal) (.scalar wbVal)) := by
                              have hn1_regs : n1.registers = ctx.registers := by rw [← hn1_eq, hn_eq]
                              cases fetch with
                              | false =>
                                exact Or.inl ⟨rfl, rfl, (hsucc_f rfl).trans hn1_regs⟩
                              | true =>
                                obtain ⟨hsrc_lt10, hset_r⟩ := hsucc_t rfl
                                rw [hn1_regs] at hset_r
                                refine Or.inr ⟨src8.val, oldSlice, v2, rfl, rfl, hsrc_lt10, hset_r, ?_⟩
                                rw [hv2_eq]
                                exact TypeRel2_scalar_rel2_of_concretizes hconc_ld
                            -- New stack slots via Stack_store_rel2:
                            have hrel_stored : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen
                                (.ScalarValue usvd) (.scalar effStored) (.scalar effStored) :=
                              TypeRel2_scalar_rel2_of_concretizes hconc_eff
                            have hrel_ss := (Stack_store_rel2 (layout₁ := l₁) (layout₂ := l₂)
                              hctx.2.1 husvd_valid hconc_sum hst_op hrel_stored (fun _ => hslot_rel_pre)).2
                            dsimp only at hrel_ss
                            rw [hw_core] at hrel_ss
                            let newSlot₁ := StackSlot.bytes
                              (insertSubValue oldSlot₁ effStored subIdx w)
                              (insertSubValue initMask₁ (~~~0#64) subIdx w)
                            let newSlot₂ := StackSlot.bytes
                              (insertSubValue oldSlot₂ effStored subIdx w)
                              (insertSubValue initMask₂ (~~~0#64) subIdx w)
                            have hss_eq : StackSlotRel2 env ρ resActive isNullId st₁.world.arrayLen
                                (stackSlotType s' slotIdx) newSlot₁ newSlot₂ := by
                              dsimp only [newSlot₁, newSlot₂, slotIdx, subIdx, effOff]
                              cases is64 with
                              | false =>
                                simp only [w, Bool.false_eq_true, ↓reduceIte] at hstk1 hstk2 hrel_ss ⊢
                                rw [hstk1, hstk2] at hrel_ss
                                have hb_ne : (DataWidth.u32 == DataWidth.u64) = false := rfl
                                simpa only [hb_ne, Bool.false_eq_true, ↓reduceIte] using hrel_ss
                              | true =>
                                have hsub0 : (baseOff + sextOffset16 off.bv).toNat % 8 = 0 := by
                                  have hal := hal_stk baseOff hconc_base.1
                                  unfold isAligned DataWidth.bvBytes DataWidth.bytes at hal
                                  rw [beq_iff_eq] at hal
                                  have hto := congrArg BitVec.toNat hal
                                  rw [BitVec.toNat_umod] at hto
                                  exact hto
                                have hins64_v : ∀ a b : BitVec 64, insertSubValue a b 0 .u64 = b := by
                                  intro a b; unfold insertSubValue DataWidth.valMask64; bv_decide
                                simp only [w, ↓reduceIte] at hrel_ss ⊢
                                rw [hsub0, hins64_v, hins64_v, hins64_v, hins64_v]
                                simpa only [beq_self_eq_true, ↓reduceIte] using hrel_ss
                            have hrel_post := StateRel2Deps_atomic_stack
                              (by dsimp only [slotIdx, effOff]; rw [hslot_idx_eq]; exact hslot_lt64)
                              (hsucc_stk.trans (by rw [← hn1_eq]))
                              (hsucc_bm.trans (by rw [← hn1_eq, hn_eq]))
                              (hsucc_res.trans (by rw [← hn1_eq, hn_eq]))
                              (fun s hs hne => hslots_other s hs (by rwa [← heff_eq]))
                              hsub_all hwb_match hrel (fun _ => hss_eq)
                            have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
                              simp only [bumpWorldStep]; omega
                            obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
                              hsem l₁ l₂ s₁ s₂ ρ resActive isNullId
                                { pc := succ.pc.val, regs := applyAtomicWb st₁.regs wb?,
                                  stack := updateStack st₁.stack slotIdx newSlot₁, world := bumpWorldStep st₁.world }
                                { pc := succ.pc.val, regs := applyAtomicWb st₂.regs wb?,
                                  stack := updateStack st₂.stack slotIdx newSlot₂, world := bumpWorldStep st₂.world }
                                hl₁ hl₂ hleak halloc' hρ hrel_post
                            have hn_ge512 : ¬ (baseOff + sextOffset16 off.bv ≥ 512#64) :=
                              BitVec.not_le.mpr heff_lt512
                            have hal_eff := hal_stk baseOff hconc_base.1
                            have hinit_bne1 : (extractSubValue initMask₁ subIdx w != w.valMask64) = false :=
                              bne_eq_false_iff_eq.mpr hinit1
                            have hinit_bne2 : (extractSubValue initMask₂ subIdx w != w.valMask64) = false :=
                              bne_eq_false_iff_eq.mpr hinit2
                            dsimp only [subIdx, effOff, w] at hinit_bne1 hinit_bne2 hold_eq
                            refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
                            · unfold runSteps step
                              rw [hv_base1] at hca_dst1
                              simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, hn_src_ge,
                                Bool.false_eq_true, ↓reduceIte, hca_dst1, hneq_cmp,
                                hv_base1, hn_ge512, hal_eff, Bool.not_true, hv_src1, hstk1, hinit_bne1]
                              cases op with
                              | cmpxchg => simp [nonCmpxchgAluOp] at hkind
                              | xchg =>
                                simp [nonCmpxchgAluOp] at hkind; rcases hkind with ⟨rfl, _⟩
                                simp only [hn_ro_fp, Bool.false_eq_true, ↓reduceIte]
                                rw [stepNextIte_eq_next (by
                                  dsimp only [newSlot₁]
                                  rw [hsucc_pc_val, ← hins_eq oldSlot₁ subIdx]
                                  dsimp only [slotIdx, subIdx, effOff, wb?, oldSlice, w, applyAtomicWb]
                                  cases is64 <;> simp [evalNonCmpxchgRmw, extractSubValue_u32_zext]) hr₁]
                                exact hr₁
                              | add f | and f | or f | xor f =>
                                simp [nonCmpxchgAluOp] at hkind; rcases hkind with ⟨rfl, _⟩
                                simp only [hn_ro_fp, Bool.false_eq_true, ↓reduceIte]
                                rw [stepNextIte_eq_next (by
                                  dsimp only [newSlot₁]
                                  rw [hsucc_pc_val, ← hins_eq oldSlot₁ subIdx]
                                  dsimp only [slotIdx, subIdx, effOff, wb?, oldSlice, w, applyAtomicWb]
                                  cases f <;> cases is64 <;> simp [evalNonCmpxchgRmw, extractSubValue_u32_zext]) hr₁]
                                exact hr₁
                            · unfold runSteps step
                              rw [hv_base2] at hca_dst2
                              simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, hn_src_ge,
                                Bool.false_eq_true, ↓reduceIte, hca_dst2, hneq_cmp,
                                hv_base2, hn_ge512, hal_eff, Bool.not_true, hv_src2, hstk2, hinit_bne2]
                              cases op with
                              | cmpxchg => simp [nonCmpxchgAluOp] at hkind
                              | xchg =>
                                simp [nonCmpxchgAluOp] at hkind; rcases hkind with ⟨rfl, _⟩
                                simp only [hn_ro_fp, Bool.false_eq_true, ↓reduceIte]
                                rw [stepNextIte_eq_next (by
                                  dsimp only [newSlot₂]
                                  rw [hsucc_pc_val, ← hins_eq oldSlot₂ subIdx]
                                  dsimp only [slotIdx, subIdx, effOff, wb?, oldSlice, w, applyAtomicWb]
                                  rw [hold_eq]
                                  cases is64 <;> simp [evalNonCmpxchgRmw, extractSubValue_u32_zext]) hr₂]
                                exact hr₂
                              | add f | and f | or f | xor f =>
                                simp [nonCmpxchgAluOp] at hkind; rcases hkind with ⟨rfl, _⟩
                                simp only [hn_ro_fp, Bool.false_eq_true, ↓reduceIte]
                                rw [stepNextIte_eq_next (by
                                  dsimp only [newSlot₂]
                                  rw [hsucc_pc_val, ← hins_eq oldSlot₂ subIdx]
                                  dsimp only [slotIdx, subIdx, effOff, wb?, oldSlice, w, applyAtomicWb]
                                  rw [hold_eq]
                                  cases f <;> cases is64 <;> simp [evalNonCmpxchgRmw, extractSubValue_u32_zext]) hr₂]
                                exact hr₂
                    · -- Case 2: PtrToMemory
                      rw [hu_mem] at hback hlm hsm
                      dsimp only at hback hlm hsm
                      obtain ⟨hdst_live, hsrc_live, _, hsub_all⟩ :=
                        DataDependencies_atomic_non_stack_spec hback
                      rcases bind_eq_ok hlm with ⟨bw, hbw, hlm⟩
                      rcases bind_eq_ok hlm with ⟨r_chk, hchk, hlm⟩
                      cases r_chk with
                      | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
                      | Ok _ =>
                        dsimp only at hlm
                        rw [bind_ok_eq (Type_unknown_written_scalar_eq _)] at hlm
                        rcases bind_eq_ok hlm with ⟨⟨r_ma_l, _⟩, _, hlm⟩
                        cases r_ma_l with
                        | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
                        | Ok _ =>
                          have hv2_unk : .ScalarValue (unknownWrittenScalarWidth (DataWidth.ofCore (if is64 then api.DataWidth.U64 else .U32))) = v2 :=
                            core.result.Result.Ok.inj (Prod.mk.inj (ok_inj hlm)).1
                          rw [bind_ok_eq (PointerOrigin_is_writable_spec origin)] at hsm
                          cases hw_orig : (MemoryOrigin.ofCore origin).isWritable <;>
                            simp only [hw_orig, Bool.false_eq_true, ↓reduceIte] at hsm
                          · cases (Prod.mk.inj (ok_inj hsm)).1
                          · rcases bind_eq_ok hsm with ⟨_, _, hsm⟩
                            rcases bind_eq_ok hsm with ⟨r_chk2, _, hsm⟩
                            cases r_chk2 with
                            | Err _ => cases (Prod.mk.inj (ok_inj hsm)).1
                            | Ok _ =>
                              cases is_init_sm <;> simp only [Bool.false_eq_true, ↓reduceIte] at hsm
                              · cases (Prod.mk.inj (ok_inj hsm)).1
                              · rcases bind_eq_ok hsm with ⟨_, _, hsm⟩
                                rcases Prod.mk.inj (Prod.mk.inj (ok_inj hsm)).2 with ⟨hn1_eq, _⟩
                                intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
                                obtain ⟨_, _, hrel_dst⟩ :=
                                  ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hdst_live) hreg_dst
                                obtain ⟨_, _, hca_dst1, hca_dst2⟩ :=
                                  Type_inner_ok_valid_and_checkAlive_none hctx hrel hv_valid hv_ne_uninit hinner hrel_dst
                                obtain ⟨_, _, _, baseOff, hconc_base, hv_base1, hv_base2⟩ :=
                                  unwrap_mem_ptr_rel2 hv_valid hu_mem hrel_dst
                                obtain ⟨_, _, hrel_src⟩ :=
                                  ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hsrc_live) hreg_src
                                rw [hv1_eq] at hrel_src
                                simp only [TypeRel2, hvd_ne_uninit, ↓reduceIte] at hrel_src
                                rcases hrel_src with ⟨sVal₁, sVal₂, hv_src1, hv_src2, hconc_vd⟩
                                have hsVal_eq := fully_initialized_eq_of_rel2_64 hvd_unw hconc_vd
                                subst hsVal_eq
                                let w : DataWidth := if is64 then .u64 else .u32
                                have hw_core : DataWidth.ofCore (if is64 then api.DataWidth.U64 else .U32) = w := by
                                  cases is64 <;> rfl
                                have hin_bnds : inBoundsRange (baseOff + sextOffset16 off.bv) w buffer_size.bv = true := by
                                  simpa only [hw_core] using
                                    ComputationContext_check_memory_access_ok_spec hbw hconc_base.1 hchk
                                have hal_eff := hal_mem baseOff hconc_base.1
                                let effOff := baseOff + sextOffset16 off.bv
                                let oldVal := readBufWord st₁.world.bufBytes (renameMemId ρ id) effOff w
                                have hld_eq_unk : loaded_data = unknownWrittenScalarWidth w := by
                                  rw [← hw_core]
                                  exact (verifier.Type.ScalarValue.inj (hv2_unk.trans hv2_eq)).symm
                                have hconc_ld : ConcretizesScalar64Rel2 loaded_data oldVal oldVal := by
                                  rw [hld_eq_unk]
                                  exact concretizesScalar64Rel2_readBufWord _ _ _ w
                                obtain ⟨effStored, _, _, hwr_eq⟩ :=
                                  atomic_alu_eval_sound hkind hconc_ld hconc_vd heval
                                let wb? : Option (Nat × BitVec 64) :=
                                  if fetch then some (src8.val, oldVal) else none
                                let wbTy? : Option (Nat × verifier.Type) :=
                                  if fetch then some (src8.val, v2) else none
                                have hwb_match :
                                    (wb? = none ∧ wbTy? = none ∧ succ.registers = ctx.registers) ∨
                                    (∃ r wbVal wbTy,
                                      wb? = some (r, wbVal) ∧
                                      wbTy? = some (r, wbTy) ∧
                                      r < GENERAL_REGISTER_COUNT ∧
                                      succ.registers.val = ctx.registers.val.set r wbTy ∧
                                      TypeRel2 env ρ resActive isNullId st₁.world.arrayLen wbTy (.scalar wbVal) (.scalar wbVal)) := by
                                  have hn1_regs : n1.registers = ctx.registers := by rw [← hn1_eq, hn_eq]
                                  cases fetch with
                                  | false =>
                                    exact Or.inl ⟨rfl, rfl, (hsucc_f rfl).trans hn1_regs⟩
                                  | true =>
                                    obtain ⟨hsrc_lt10, hset_r⟩ := hsucc_t rfl
                                    rw [hn1_regs] at hset_r
                                    refine Or.inr ⟨src8.val, oldVal, v2, rfl, rfl, hsrc_lt10, hset_r, ?_⟩
                                    rw [hv2_eq]
                                    exact TypeRel2_scalar_rel2_of_concretizes hconc_ld
                                let w₁' : WorldState :=
                                  { st₁.world with bufBytes := writeBufWord st₁.world.bufBytes (renameMemId ρ id) effOff w effStored }
                                let w₂' : WorldState :=
                                  { st₂.world with bufBytes := writeBufWord st₂.world.bufBytes (renameMemId ρ id) effOff w effStored }
                                have hw'_eq : w₁' = w₂' := by dsimp [w₁', w₂']; rw [hrel.2.2.1]
                                have hrel_post := StateRel2Deps_atomic_mem
                                  (hsucc_stk.trans (by rw [← hn1_eq, hn_eq]))
                                  (hsucc_bm.trans (by rw [← hn1_eq, hn_eq]))
                                  (hsucc_res.trans (by rw [← hn1_eq, hn_eq]))
                                  hw'_eq rfl rfl rfl hsub_all hwb_match hrel
                                have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
                                  simp only [bumpWorldStep]; omega
                                obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
                                  hsem l₁ l₂ s₁ s₂ ρ resActive isNullId
                                    { pc := succ.pc.val, regs := applyAtomicWb st₁.regs wb?, stack := st₁.stack, world := bumpWorldStep w₁' }
                                    { pc := succ.pc.val, regs := applyAtomicWb st₂.regs wb?, stack := st₂.stack, world := bumpWorldStep w₂' }
                                    hl₁ hl₂ hleak halloc' hρ hrel_post
                                dsimp only [w] at hin_bnds
                                refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
                                · unfold runSteps step
                                  rw [hv_base1] at hca_dst1
                                  simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, hn_src_ge,
                                    Bool.false_eq_true, ↓reduceIte, hca_dst1, hneq_cmp,
                                    hv_base1, hw_orig, hmap_orig, hin_bnds, hal_eff, Bool.not_true, hv_src1]
                                  cases op with
                                  | cmpxchg => simp [nonCmpxchgAluOp] at hkind
                                  | xchg =>
                                    simp [nonCmpxchgAluOp] at hkind; rcases hkind with ⟨rfl, _⟩
                                    simp only [hn_ro_fp, Bool.false_eq_true, ↓reduceIte]
                                    rw [stepNextIte_eq_next (by
                                      dsimp only [w₁']
                                      rw [hsucc_pc_val, ← hwr_eq st₁.world.bufBytes (renameMemId ρ id) effOff]
                                      dsimp only [effOff, wb?, oldVal, w, applyAtomicWb]
                                      cases is64 <;> simp [evalNonCmpxchgRmw, readBufWord_u32_zext]) hr₁]
                                    exact hr₁
                                  | add f | and f | or f | xor f =>
                                    simp [nonCmpxchgAluOp] at hkind; rcases hkind with ⟨rfl, _⟩
                                    simp only [hn_ro_fp, Bool.false_eq_true, ↓reduceIte]
                                    rw [stepNextIte_eq_next (by
                                      dsimp only [w₁']
                                      rw [hsucc_pc_val, ← hwr_eq st₁.world.bufBytes (renameMemId ρ id) effOff]
                                      dsimp only [effOff, wb?, oldVal, w, applyAtomicWb]
                                      cases f <;> cases is64 <;> simp [evalNonCmpxchgRmw, readBufWord_u32_zext]) hr₁]
                                    exact hr₁
                                · unfold runSteps step
                                  rw [hv_base2] at hca_dst2
                                  have hbuf_eq : st₂.world.bufBytes = st₁.world.bufBytes := by rw [hrel.2.2.1]
                                  simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, hn_src_ge,
                                    Bool.false_eq_true, ↓reduceIte, hca_dst2, hneq_cmp,
                                    hv_base2, hw_orig, hmap_orig, hin_bnds, hal_eff, Bool.not_true, hv_src2]
                                  cases op with
                                  | cmpxchg => simp [nonCmpxchgAluOp] at hkind
                                  | xchg =>
                                    simp [nonCmpxchgAluOp] at hkind; rcases hkind with ⟨rfl, _⟩
                                    simp only [hn_ro_fp, Bool.false_eq_true, ↓reduceIte]
                                    rw [stepNextIte_eq_next (by
                                      dsimp only [w₂']
                                      rw [hsucc_pc_val, hbuf_eq,
                                          ← hwr_eq st₁.world.bufBytes (renameMemId ρ id) effOff]
                                      dsimp only [effOff, wb?, oldVal, w, applyAtomicWb]
                                      cases is64 <;> simp [evalNonCmpxchgRmw, readBufWord_u32_zext]) hr₂]
                                    exact hr₂
                                  | add f | and f | or f | xor f =>
                                    simp [nonCmpxchgAluOp] at hkind; rcases hkind with ⟨rfl, _⟩
                                    simp only [hn_ro_fp, Bool.false_eq_true, ↓reduceIte]
                                    rw [stepNextIte_eq_next (by
                                      dsimp only [w₂']
                                      rw [hsucc_pc_val, hbuf_eq,
                                          ← hwr_eq st₁.world.bufBytes (renameMemId ρ id) effOff]
                                      dsimp only [effOff, wb?, oldVal, w, applyAtomicWb]
                                      cases f <;> cases is64 <;> simp [evalNonCmpxchgRmw, readBufWord_u32_zext]) hr₂]
                                    exact hr₂

/-! ## 3. Atomic Compare-Exchange (`raw_atomic_cmpxchg`, `visit_atomic_forward`, `visit_atomic_backward`) -/

private theorem Type_constraint_scalars_ctx_eq
    {context context1 : verifier.ComputationContext}
    {jump_type : verifier.JumpType} {jump_width : verifier.JumpWidth}
    {d1 d2 : U64ScalarValueData} {new1 new2 : verifier.Type}
    (hjt : jump_type = .Eq ∨ jump_type = .Ne)
    (h : verifier.Type.constraint context jump_type jump_width
      (.ScalarValue d1) (.ScalarValue d2) = ok (.Ok (new1, new2), context1)) :
    context1 = context := by
  rw [verifier.Type.constraint.eq_def] at h
  rcases bind_eq_ok h with ⟨r, _, h⟩
  cases r with
  | Err _ => cases (Prod.mk.inj (ok_inj h)).1
  | Ok _ =>
    dsimp only at h
    rcases bind_eq_ok h with ⟨r1, _, h⟩
    cases r1 with
    | Err _ => cases (Prod.mk.inj (ok_inj h)).1
    | Ok _ =>
      dsimp only at h
      rw [bind_ok_eq (Type_unwrap_releasable_spec (.ScalarValue d1)),
          bind_ok_eq (Type_unwrap_releasable_spec (.ScalarValue d2))] at h
      dsimp only [pureUnwrapReleasable] at h
      rcases hjt with rfl | rfl
      · cases jump_width with
        | W32 =>
          dsimp only at h
          rcases bind_eq_ok h with ⟨b, _, h⟩
          cases b with
          | false => exact (Prod.mk.inj (ok_inj h)).2.symm
          | true =>
            simp only [↓reduceIte] at h
            rcases bind_eq_ok h with ⟨b1, _, h⟩
            cases b1 with
            | false => exact (Prod.mk.inj (ok_inj h)).2.symm
            | true =>
              simp only [↓reduceIte] at h
              rcases bind_eq_ok h with ⟨_, _, h⟩
              rcases bind_eq_ok h with ⟨_, _, h⟩
              rcases bind_eq_ok h with ⟨b2, _, h⟩
              cases b2 with
              | false => exact (Prod.mk.inj (ok_inj h)).2.symm
              | true =>
                simp only [↓reduceIte] at h
                rcases bind_eq_ok h with ⟨i, _, h⟩
                rcases bind_eq_ok h with ⟨i1, _, h⟩
                rcases bind_eq_ok h with ⟨bfit, _, h⟩
                rcases bind_eq_ok h with ⟨⟨d11, d21, ur1, ur2⟩, _, h⟩
                simp only [lift, bind_ok] at h
                rcases bind_eq_ok h with ⟨i3, _, h⟩
                rcases bind_eq_ok h with ⟨i6, _, h⟩
                rcases bind_eq_ok h with ⟨o, _, h⟩
                rcases bind_eq_ok h with ⟨o1, _, h⟩
                cases o with
                | none => exact (Prod.mk.inj (ok_inj h)).2.symm
                | some _ => cases o1 <;> exact (Prod.mk.inj (ok_inj h)).2.symm
        | W64 =>
          dsimp only at h
          rcases bind_eq_ok h with ⟨b, _, h⟩
          cases b with
          | false => exact (Prod.mk.inj (ok_inj h)).2.symm
          | true =>
            simp only [↓reduceIte] at h
            rcases bind_eq_ok h with ⟨b1, _, h⟩
            cases b1 with
            | false => exact (Prod.mk.inj (ok_inj h)).2.symm
            | true =>
              simp only [↓reduceIte] at h
              rcases bind_eq_ok h with ⟨b2, _, h⟩
              cases b2 with
              | false => exact (Prod.mk.inj (ok_inj h)).2.symm
              | true =>
                simp only [↓reduceIte] at h
                rcases bind_eq_ok h with ⟨i, _, h⟩
                rcases bind_eq_ok h with ⟨i1, _, h⟩
                rcases bind_eq_ok h with ⟨umin, _, h⟩
                rcases bind_eq_ok h with ⟨i2, _, h⟩
                rcases bind_eq_ok h with ⟨i3, _, h⟩
                rcases bind_eq_ok h with ⟨umax, _, h⟩
                simp only [lift, bind_ok] at h
                rcases bind_eq_ok h with ⟨ur, _, h⟩
                rw [bind_ok_eq (new64_eq_ok _ _ 0#u64 ur)] at h
                rw [bind_ok_eq (Type_clone_spec _)] at h
                exact (Prod.mk.inj (ok_inj h)).2.symm
      · cases jump_width <;> exact (Prod.mk.inj (ok_inj h)).2.symm

private theorem cmpxchg_branch_ok_valid
    {env : ExecEnv} {vCounter : Nat}
    {self n3 : verifier.ComputationContext}
    {vc_in vc_out : verifier.VerificationContext}
    {v v1 v3 storedTy : verifier.Type}
    {field : verifier.Field} {jt : verifier.JumpType} {jw : verifier.JumpWidth}
    {c_dst c_src : verifier.Type} {n n1 n2 : verifier.ComputationContext}
    {vc_sm : verifier.VerificationContext}
    (hctx : CompCtxValid env vCounter self)
    (hv1 : TypeValid env vCounter v1)
    (hv3 : TypeValid env vCounter v3)
    (hstored_valid : TypeValid env vCounter c_dst → TypeValid env vCounter storedTy)
    (hnext : verifier.ComputationContext.next self = ok (.Ok n))
    (hcstr : verifier.Type.constraint n jt jw v1 v3 = ok (.Ok (c_dst, c_src), n1))
    (hset : verifier.ComputationContext.set_reg n1 0#u8 c_dst = ok (.Ok (), n2))
    (hsm : verifier.ComputationContext.store_memory n2 vc_in self.pc v field storedTy =
      ok (.Ok (), n3, vc_sm))
    (hpush : verifier.VerificationContext.push_successor vc_sm n3 = ok vc_out) :
    CompCtxValid env vCounter n3 ∧
    vc_out.calling_context = vc_in.calling_context ∧
    vc_out.code = vc_in.code ∧
    vc_out.counter = vc_in.counter ∧
    vc_out.iteration = vc_in.iteration ∧
    vc_out.terminating_contexts = vc_in.terminating_contexts ∧
    vc_out.pending_successors.val = vc_in.pending_successors.val ++ [some n3] := by
  obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext
  have hn_valid : CompCtxValid env vCounter n := by rw [hn_eq]; exact hctx
  obtain ⟨_, hn1_valid, hcdst_valid, _⟩ := Type_constraint_ok_valid hn_valid hv1 hv3 hcstr
  obtain ⟨_, _, _, _, _, _, hn2_valid⟩ := ComputationContext_set_reg_ok_spec hn1_valid hcdst_valid hset
  obtain ⟨hn3_valid, hs1, hs2, hs3, hs4, hs5, hs6⟩ :=
    ComputationContext_store_memory_ok_valid hn2_valid (hstored_valid hcdst_valid) hsm
  obtain ⟨hp1, hp2, hp3, hp4, hp5, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
  exact ⟨hn3_valid, hp1.trans hs1, hp2.trans hs2, hp3.trans hs3,
    hp4.trans hs4, hp5.trans hs5, hp7.trans (by rw [hs6])⟩

private theorem raw_atomic_cmpxchg_ok_valid
    {env : ExecEnv} {ctx : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {log_op : verifier.AtomicLogOp} {dst src : Std.U8} {offset : Std.I16}
    {jump_width : verifier.JumpWidth} {cond_op : verifier.BranchCondOp}
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hcmp : verifier.ComputationContext.raw_atomic_cmpxchg ctx log_op vctx0
      dst offset src jump_width cond_op = ok (.Ok (), vctx1)) :
    ForwardVisitPost env vctx0 vctx1 ctx := by
  unfold verifier.ComputationContext.raw_atomic_cmpxchg at hcmp
  rcases bind_eq_ok hcmp with ⟨vc1, hlog, hcmp⟩
  obtain ⟨hl1, hl2, hl3, hl4, hl5, hl6⟩ := log_atomic_operation_ok_spec hlog
  rcases bind_eq_ok hcmp with ⟨width_c, _, hcmp⟩
  rcases bind_eq_ok hcmp with ⟨r, hreg_dst, hcmp⟩
  cases r with
  | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
  | Ok v =>
    dsimp only at hcmp
    obtain ⟨_, hv_valid⟩ := ComputationContext_reg_ok_valid hctx hreg_dst
    rw [bind_ok_eq (Field_new_spec offset _)] at hcmp
    rcases bind_eq_ok hcmp with ⟨r1, _, hcmp⟩
    cases r1 with
    | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
    | Ok _ =>
      dsimp only at hcmp
      rcases bind_eq_ok hcmp with ⟨⟨r2, vc2⟩, hlm, hcmp⟩
      cases r2 with
      | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
      | Ok v1 =>
        obtain ⟨hv1_valid, hm1, hm2, hm3, hm4, hm5, hm6⟩ :=
          ComputationContext_load_memory_ok_valid hctx hv_valid hlm
        rcases bind_eq_ok hcmp with ⟨r3, hreg_src, hcmp⟩
        cases r3 with
        | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
        | Ok v2 =>
          dsimp only at hcmp
          obtain ⟨_, hv2_valid⟩ := ComputationContext_reg_ok_valid hctx hreg_src
          rcases bind_eq_ok hcmp with ⟨r4, hreg_r0, hcmp⟩
          cases r4 with
          | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
          | Ok v3 =>
            dsimp only at hcmp
            obtain ⟨_, hv3_valid⟩ := ComputationContext_reg_ok_valid hctx hreg_r0
            rcases bind_eq_ok hcmp with ⟨b, _, hcmp⟩
            cases b with
            | false => cases (Prod.mk.inj (ok_inj hcmp)).1
            | true =>
              simp only [↓reduceIte] at hcmp
              rcases bind_eq_ok hcmp with ⟨b1, _, hcmp⟩
              cases b1 with
              | false => cases (Prod.mk.inj (ok_inj hcmp)).1
              | true =>
                simp only [↓reduceIte] at hcmp
                rcases bind_eq_ok hcmp with ⟨b2, _, hcmp⟩
                cases b2 with
                | false => cases (Prod.mk.inj (ok_inj hcmp)).1
                | true =>
                  simp only [↓reduceIte] at hcmp
                  rcases bind_eq_ok hcmp with ⟨r5, _, hcmp⟩
                  cases r5 with
                  | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                  | Ok b3 =>
                    dsimp only at hcmp
                    rcases bind_eq_ok hcmp with ⟨take_eq, hteq, hcmp⟩
                    rcases bind_eq_ok hcmp with ⟨take_ne, htne, hcmp⟩
                    cases b3 with
                    | none =>
                      cases ok_inj hteq; cases ok_inj htne
                      simp only [↓reduceIte, Type_clone_spec, bind_ok] at hcmp
                      rcases bind_eq_ok hcmp with ⟨r6, hnext1, hcmp⟩
                      cases r6 with
                      | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                      | Ok n =>
                        rcases bind_eq_ok hcmp with ⟨⟨r7, n1⟩, hcstr1, hcmp⟩
                        cases r7 with
                        | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                        | Ok pair =>
                          rcases pair with ⟨c_dst, c_src⟩
                          rcases bind_eq_ok hcmp with ⟨⟨r8, n2⟩, hset1, hcmp⟩
                          cases r8 with
                          | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                          | Ok _ =>
                            rcases bind_eq_ok hcmp with ⟨⟨r9, n3, vc3⟩, hsm1, hcmp⟩
                            cases r9 with
                            | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                            | Ok _ =>
                              rcases bind_eq_ok hcmp with ⟨vc4, hpush1, hcmp⟩
                              obtain ⟨hn3_valid, ha1, ha2, ha3, ha4, ha5, ha6⟩ :=
                                cmpxchg_branch_ok_valid hctx hv1_valid hv3_valid (fun _ => hv2_valid)
                                  hnext1 hcstr1 hset1 hsm1 hpush1
                              rcases bind_eq_ok hcmp with ⟨r10, hnext2, hcmp⟩
                              cases r10 with
                              | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                              | Ok n4 =>
                                rcases bind_eq_ok hcmp with ⟨⟨r11, n5⟩, hcstr2, hcmp⟩
                                cases r11 with
                                | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                | Ok pair1 =>
                                  rcases pair1 with ⟨c_dst1, c_src1⟩
                                  rcases bind_eq_ok hcmp with ⟨⟨r12, n6⟩, hset2, hcmp⟩
                                  cases r12 with
                                  | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                  | Ok _ =>
                                    rcases bind_eq_ok hcmp with ⟨⟨r13, n7, vc5⟩, hsm2, hcmp⟩
                                    cases r13 with
                                    | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                    | Ok _ =>
                                      rcases bind_eq_ok hcmp with ⟨vc6, hpush2, hcmp⟩
                                      rcases Prod.mk.inj (ok_inj hcmp) with ⟨_, rfl⟩
                                      obtain ⟨hn7_valid, hb1, hb2, hb3, hb4, hb5, hb6⟩ :=
                                        cmpxchg_branch_ok_valid hctx hv1_valid hv3_valid id
                                          hnext2 hcstr2 hset2 hsm2 hpush2
                                      have hcnt : vc6.counter = vctx0.counter :=
                                        hb3.trans (ha3.trans (hm3.trans hl3))
                                      refine ⟨hb1.trans (ha1.trans (hm1.trans hl1)),
                                        hb2.trans (ha2.trans (hm2.trans hl2)),
                                        hb4.trans (ha4.trans (hm4.trans hl4)),
                                        hb5.trans (ha5.trans (hm5.trans hl5)),
                                        by rw [hcnt], by rwa [hcnt], ?_⟩
                                      intro opt hopt
                                      rw [hb6, ha6, hm6, hl6, hpending, List.nil_append,
                                          List.mem_append, List.mem_singleton, List.mem_singleton] at hopt
                                      rcases hopt with rfl | rfl
                                      · exact ⟨n3, rfl, by rwa [hcnt]⟩
                                      · exact ⟨n7, rfl, by rwa [hcnt]⟩
                    | some b4 =>
                      cases b4 with
                      | true =>
                        cases ok_inj hteq; cases ok_inj htne
                        simp only [↓reduceIte, Type_clone_spec, bind_ok] at hcmp
                        rcases bind_eq_ok hcmp with ⟨r6, hnext1, hcmp⟩
                        cases r6 with
                        | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                        | Ok n =>
                          rcases bind_eq_ok hcmp with ⟨⟨r7, n1⟩, hcstr1, hcmp⟩
                          cases r7 with
                          | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                          | Ok pair =>
                            rcases pair with ⟨c_dst, c_src⟩
                            rcases bind_eq_ok hcmp with ⟨⟨r8, n2⟩, hset1, hcmp⟩
                            cases r8 with
                            | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                            | Ok _ =>
                              rcases bind_eq_ok hcmp with ⟨⟨r9, n3, vc3⟩, hsm1, hcmp⟩
                              cases r9 with
                              | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                              | Ok _ =>
                                rcases bind_eq_ok hcmp with ⟨vc4, hpush1, hcmp⟩
                                rcases Prod.mk.inj (ok_inj hcmp) with ⟨_, rfl⟩
                                obtain ⟨hn3_valid, ha1, ha2, ha3, ha4, ha5, ha6⟩ :=
                                  cmpxchg_branch_ok_valid hctx hv1_valid hv3_valid (fun _ => hv2_valid)
                                    hnext1 hcstr1 hset1 hsm1 hpush1
                                have hcnt : vc4.counter = vctx0.counter := ha3.trans (hm3.trans hl3)
                                refine ⟨ha1.trans (hm1.trans hl1),
                                  ha2.trans (hm2.trans hl2),
                                  ha4.trans (hm4.trans hl4),
                                  ha5.trans (hm5.trans hl5),
                                  by rw [hcnt], by rwa [hcnt], ?_⟩
                                intro opt hopt
                                rw [ha6, hm6, hl6, hpending, List.nil_append, List.mem_singleton] at hopt
                                subst hopt
                                exact ⟨n3, rfl, by rwa [hcnt]⟩
                      | false =>
                        cases ok_inj hteq; cases ok_inj htne
                        simp only [Bool.false_eq_true, ↓reduceIte] at hcmp
                        rcases bind_eq_ok hcmp with ⟨r6, hnext1, hcmp⟩
                        cases r6 with
                        | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                        | Ok n =>
                          rcases bind_eq_ok hcmp with ⟨⟨r7, n1⟩, hcstr1, hcmp⟩
                          cases r7 with
                          | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                          | Ok pair =>
                            rcases pair with ⟨c_dst, c_src⟩
                            rcases bind_eq_ok hcmp with ⟨t_cl, hcl, hcmp⟩
                            cases ok_inj (hcl.symm.trans (Type_clone_spec c_dst))
                            rcases bind_eq_ok hcmp with ⟨⟨r8, n2⟩, hset1, hcmp⟩
                            cases r8 with
                            | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                            | Ok _ =>
                              rcases bind_eq_ok hcmp with ⟨⟨r9, n3, vc3⟩, hsm1, hcmp⟩
                              cases r9 with
                              | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                              | Ok _ =>
                                rcases bind_eq_ok hcmp with ⟨vc4, hpush1, hcmp⟩
                                rcases Prod.mk.inj (ok_inj hcmp) with ⟨_, rfl⟩
                                obtain ⟨hn3_valid, ha1, ha2, ha3, ha4, ha5, ha6⟩ :=
                                  cmpxchg_branch_ok_valid hctx hv1_valid hv3_valid id
                                    hnext1 hcstr1 hset1 hsm1 hpush1
                                have hcnt : vc4.counter = vctx0.counter := ha3.trans (hm3.trans hl3)
                                refine ⟨ha1.trans (hm1.trans hl1),
                                  ha2.trans (hm2.trans hl2),
                                  ha4.trans (hm4.trans hl4),
                                  ha5.trans (hm5.trans hl5),
                                  by rw [hcnt], by rwa [hcnt], ?_⟩
                                intro opt hopt
                                rw [ha6, hm6, hl6, hpending, List.nil_append, List.mem_singleton] at hopt
                                subst hopt
                                exact ⟨n3, rfl, by rwa [hcnt]⟩

private theorem cmpxchg_branch_backward_exists_succ
    {env : ExecEnv} {vCounter : Nat}
    {ctx n3 : verifier.ComputationContext}
    {vc_in vc_out : verifier.VerificationContext}
    {v : verifier.Type} {d1 d3 : U64ScalarValueData} {storedTy : verifier.Type}
    {field : verifier.Field} {jt : verifier.JumpType} {jw : verifier.JumpWidth}
    {c_dst c_src : verifier.Type} {n n1 n2 : verifier.ComputationContext}
    {vc_sm : verifier.VerificationContext}
    (hctx : CompCtxValid env vCounter ctx)
    (hd1_valid : Scalar64Valid d1)
    (hd3_valid : Scalar64Valid d3)
    (hjt : jt = .Eq ∨ jt = .Ne)
    (hstored_valid : TypeValid env vCounter c_dst → TypeValid env vCounter storedTy)
    (hnext : verifier.ComputationContext.next ctx = ok (.Ok n))
    (hcstr : verifier.Type.constraint n jt jw (.ScalarValue d1) (.ScalarValue d3) =
      ok (.Ok (c_dst, c_src), n1))
    (hset : verifier.ComputationContext.set_reg n1 0#u8 c_dst = ok (.Ok (), n2))
    (hsm : verifier.ComputationContext.store_memory n2 vc_in ctx.pc v field storedTy =
      ok (.Ok (), n3, vc_sm))
    (hpush : verifier.VerificationContext.push_successor vc_sm n3 = ok vc_out) :
    n1 = n ∧
    n2.pc.val = ctx.pc.val + 1 ∧
    n2.registers.val = ctx.registers.val.set 0 c_dst ∧
    n2.stack = ctx.stack ∧
    n2.array_bounds = ctx.array_bounds ∧
    n2.resources = ctx.resources ∧
    CompCtxValid env vCounter n2 ∧
    TypeValid env vCounter c_dst ∧
    vc_out.pending_successors.val = vc_in.pending_successors.val ++ [some n3] := by
  have hn1_eq_n := Type_constraint_scalars_ctx_eq hjt hcstr
  obtain ⟨hpc_next, hn_eq⟩ := ComputationContext_next_ok_spec hnext
  have hn_valid : CompCtxValid env vCounter n := by rw [hn_eq]; exact hctx
  obtain ⟨_, _, hcdst_valid, _⟩ :=
    Type_constraint_ok_valid hn_valid
      (show TypeValid env vCounter (.ScalarValue d1) from hd1_valid)
      (show TypeValid env vCounter (.ScalarValue d3) from hd3_valid)
      hcstr
  subst hn1_eq_n
  obtain ⟨_, hpc_n2, hregs_n2, hstk_n2, hbm_n2, hres_n2, hn2_valid⟩ :=
    ComputationContext_set_reg_ok_spec hn_valid hcdst_valid hset
  obtain ⟨_, _, _, _, _, _, hs6⟩ :=
    ComputationContext_store_memory_ok_valid hn2_valid (hstored_valid hcdst_valid) hsm
  obtain ⟨_, _, _, _, _, _, hp7⟩ := VerificationContext_push_successor_ok_spec hpush
  have h0_val : (0#u8).val = 0 := rfl
  rw [h0_val] at hregs_n2
  exact ⟨rfl, by rw [hpc_n2, hpc_next], by rw [hregs_n2, hn_eq],
    by rw [hstk_n2, hn_eq], by rw [hbm_n2, hn_eq], by rw [hres_n2, hn_eq],
    hn2_valid, hcdst_valid, by rw [hp7, hs6]⟩

private theorem insertSubValue_zero_u64 (a b : BitVec 64) :
    insertSubValue a b 0 .u64 = b := by
  unfold insertSubValue DataWidth.valMask64; bv_decide

private theorem raw_atomic_cmpxchg_backward
    {prog : Program} {env : ExecEnv} {vCounter F : Nat}
    {ctx : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {dPost dd : verifier.DataDependencies}
    {is64 : Bool} {log_op : verifier.AtomicLogOp}
    {dst8 src8 : Std.U8} {off : Std.I16}
    {res_b : core.result.Result Unit error.VerifierErrorKind}
    (hlt_pc : ctx.pc.val < prog.size)
    (hinsn_at : prog[ctx.pc.val]'hlt_pc = .atomic is64 .cmpxchg dst8.val off.bv src8.val)
    (hctx : CompCtxValid env vCounter ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hcmp : verifier.ComputationContext.raw_atomic_cmpxchg ctx log_op vctx0
      dst8 off src8 (jumpWidthOfBool is64) (condOpOfJump is64 .eq) = ok (.Ok (), vctx1))
    (hchildren : ∀ succ, some succ ∈ vctx1.pending_successors.val →
      SemTerminatesIn prog env vCounter succ dPost F)
    (hback : verifier.DataDependencies.atomic dPost ctx dst8 off src8 true = ok (res_b, dd)) :
    SemTerminatesIn prog env vCounter ctx dd (F + 1) := by
  unfold verifier.ComputationContext.raw_atomic_cmpxchg at hcmp
  rcases bind_eq_ok hcmp with ⟨vc1, hlog, hcmp⟩
  obtain ⟨_, _, _, _, _, hl6⟩ := log_atomic_operation_ok_spec hlog
  rcases bind_eq_ok hcmp with ⟨width_c, hwidth_c, hcmp⟩
  have hwc_eq : width_c = (if is64 then api.DataWidth.U64 else .U32) := by
    cases is64 <;> exact (ok_inj hwidth_c).symm
  subst hwc_eq
  rcases bind_eq_ok hcmp with ⟨r, hreg_dst, hcmp⟩
  cases r with
  | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
  | Ok v =>
    dsimp only at hcmp
    obtain ⟨hdst_lt, hv_valid⟩ := ComputationContext_reg_ok_valid hctx hreg_dst
    rw [bind_ok_eq (Field_new_spec off (if is64 then .U64 else .U32))] at hcmp
    rcases bind_eq_ok hcmp with ⟨r1, hcaa, hcmp⟩
    cases r1 with
    | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
    | Ok u_caa =>
      cases u_caa
      dsimp only at hcmp
      obtain ⟨hinner, hv_ne_uninit, hcaa_cases⟩ := check_atomic_access_ok_spec hcaa
      rcases bind_eq_ok hcmp with ⟨⟨r2, vc2⟩, hlm, hcmp⟩
      cases r2 with
      | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
      | Ok v1 =>
        obtain ⟨hv1_valid, _, _, _, _, _, hm6⟩ :=
          ComputationContext_load_memory_ok_valid hctx hv_valid hlm
        rcases bind_eq_ok hcmp with ⟨r3, hreg_src, hcmp⟩
        cases r3 with
        | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
        | Ok v2 =>
          dsimp only at hcmp
          obtain ⟨hsrc_lt, hv2_valid⟩ := ComputationContext_reg_ok_valid hctx hreg_src
          rcases bind_eq_ok hcmp with ⟨r4, hreg_r0, hcmp⟩
          cases r4 with
          | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
          | Ok v3 =>
            dsimp only at hcmp
            obtain ⟨_, hv3_valid⟩ := ComputationContext_reg_ok_valid hctx hreg_r0
            rcases bind_eq_ok hcmp with ⟨b, hws1, hcmp⟩
            cases b with
            | false => cases (Prod.mk.inj (ok_inj hcmp)).1
            | true =>
              simp only [↓reduceIte] at hcmp
              obtain ⟨loaded_data, hv1_eq, hld_unw⟩ := Type_is_written_scalar_ok_true hws1
              have hld_valid : Scalar64Valid loaded_data := by rw [hv1_eq] at hv1_valid; exact hv1_valid
              rcases bind_eq_ok hcmp with ⟨b1, hws2, hcmp⟩
              cases b1 with
              | false => cases (Prod.mk.inj (ok_inj hcmp)).1
              | true =>
                simp only [↓reduceIte] at hcmp
                obtain ⟨value_data, hv2_eq, hvd_unw⟩ := Type_is_written_scalar_ok_true hws2
                have hvd_valid : Scalar64Valid value_data := by rw [hv2_eq] at hv2_valid; exact hv2_valid
                rcases bind_eq_ok hcmp with ⟨b2, hws3, hcmp⟩
                cases b2 with
                | false => cases (Prod.mk.inj (ok_inj hcmp)).1
                | true =>
                  simp only [↓reduceIte] at hcmp
                  obtain ⟨r0_data, hv3_eq, hr0_unw⟩ := Type_is_written_scalar_ok_true hws3
                  have hr0_valid : Scalar64Valid r0_data := by rw [hv3_eq] at hv3_valid; exact hv3_valid
                  rcases bind_eq_ok hcmp with ⟨r5, hcb, hcmp⟩
                  cases r5 with
                  | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                  | Ok b3 =>
                    dsimp only at hcmp
                    rcases bind_eq_ok hcmp with ⟨take_eq, hteq, hcmp⟩
                    rcases bind_eq_ok hcmp with ⟨take_ne, htne, hcmp⟩
                    -- Extract the Eq-branch and Ne-branch obligations whenever b3 ≠ some false / b3 ≠ some true:
                    have hbranches :
                        (b3 ≠ some false →
                          ∃ (n n2 n3 : verifier.ComputationContext) (c_dst c_src : verifier.Type)
                            (vc_in vc_sm : verifier.VerificationContext),
                            some n3 ∈ vctx1.pending_successors.val ∧
                            verifier.ComputationContext.next ctx = ok (.Ok n) ∧
                            verifier.Type.constraint n .Eq (jumpWidthOfBool is64)
                              (.ScalarValue loaded_data) (.ScalarValue r0_data) =
                              ok (.Ok (c_dst, c_src), n) ∧
                            n2.pc.val = ctx.pc.val + 1 ∧
                            n2.registers.val = ctx.registers.val.set 0 c_dst ∧
                            n2.stack = ctx.stack ∧
                            n2.array_bounds = ctx.array_bounds ∧
                            n2.resources = ctx.resources ∧
                            CompCtxValid env vCounter n2 ∧
                            TypeValid env vCounter c_dst ∧
                            verifier.ComputationContext.store_memory n2 vc_in ctx.pc v
                              { offset := off, width := if is64 then .U64 else .U32 } v2 =
                              ok (.Ok (), n3, vc_sm)) ∧
                        (b3 ≠ some true →
                          ∃ (n n2 n3 : verifier.ComputationContext) (c_dst c_src : verifier.Type)
                            (vc_in vc_sm : verifier.VerificationContext),
                            some n3 ∈ vctx1.pending_successors.val ∧
                            verifier.ComputationContext.next ctx = ok (.Ok n) ∧
                            verifier.Type.constraint n .Ne (jumpWidthOfBool is64)
                              (.ScalarValue loaded_data) (.ScalarValue r0_data) =
                              ok (.Ok (c_dst, c_src), n) ∧
                            n2.pc.val = ctx.pc.val + 1 ∧
                            n2.registers.val = ctx.registers.val.set 0 c_dst ∧
                            n2.stack = ctx.stack ∧
                            n2.array_bounds = ctx.array_bounds ∧
                            n2.resources = ctx.resources ∧
                            CompCtxValid env vCounter n2 ∧
                            TypeValid env vCounter c_dst ∧
                            verifier.ComputationContext.store_memory n2 vc_in ctx.pc v
                              { offset := off, width := if is64 then .U64 else .U32 } c_dst =
                              ok (.Ok (), n3, vc_sm)) := by
                      rw [hv1_eq, hv3_eq] at hcmp
                      cases b3 with
                      | none =>
                        cases ok_inj hteq; cases ok_inj htne
                        simp only [↓reduceIte, Type_clone_spec, bind_ok] at hcmp
                        rcases bind_eq_ok hcmp with ⟨r6, hnext1, hcmp⟩
                        cases r6 with
                        | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                        | Ok n =>
                          rcases bind_eq_ok hcmp with ⟨⟨r7, n1⟩, hcstr1, hcmp⟩
                          cases r7 with
                          | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                          | Ok pair =>
                            rcases pair with ⟨c_dst, c_src⟩
                            rcases bind_eq_ok hcmp with ⟨⟨r8, n2⟩, hset1, hcmp⟩
                            cases r8 with
                            | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                            | Ok _ =>
                              rcases bind_eq_ok hcmp with ⟨⟨r9, n3, vc3⟩, hsm1, hcmp⟩
                              cases r9 with
                              | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                              | Ok _ =>
                                rcases bind_eq_ok hcmp with ⟨vc4, hpush1, hcmp⟩
                                obtain ⟨hn1_eq, hpc_n2, hregs_n2, hstk_n2, hbm_n2, hres_n2,
                                    hn2_valid, hcdst_valid, hp_vc4⟩ :=
                                  cmpxchg_branch_backward_exists_succ hctx hld_valid hr0_valid
                                    (Or.inl rfl) (fun _ => hv2_valid) hnext1 hcstr1 hset1 hsm1 hpush1
                                rw [hn1_eq] at hcstr1
                                rcases bind_eq_ok hcmp with ⟨r10, hnext2, hcmp⟩
                                cases r10 with
                                | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                | Ok n4 =>
                                  rcases bind_eq_ok hcmp with ⟨⟨r11, n5⟩, hcstr2, hcmp⟩
                                  cases r11 with
                                  | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                  | Ok pair1 =>
                                    rcases pair1 with ⟨c_dst1, c_src1⟩
                                    rcases bind_eq_ok hcmp with ⟨⟨r12, n6⟩, hset2, hcmp⟩
                                    cases r12 with
                                    | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                    | Ok _ =>
                                      rcases bind_eq_ok hcmp with ⟨⟨r13, n7, vc5⟩, hsm2, hcmp⟩
                                      cases r13 with
                                      | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                      | Ok _ =>
                                        rcases bind_eq_ok hcmp with ⟨vc6, hpush2, hcmp⟩
                                        rcases Prod.mk.inj (ok_inj hcmp) with ⟨_, rfl⟩
                                        obtain ⟨hn5_eq, hpc_n6, hregs_n6, hstk_n6, hbm_n6, hres_n6,
                                            hn6_valid, hcdst1_valid, hp_vc6⟩ :=
                                          cmpxchg_branch_backward_exists_succ hctx hld_valid hr0_valid
                                            (Or.inr rfl) id hnext2 hcstr2 hset2 hsm2 hpush2
                                        rw [hn5_eq] at hcstr2
                                        refine ⟨fun _ => ⟨n, n2, n3, c_dst, c_src, vc2, vc3, ?_,
                                            hnext1, hcstr1, hpc_n2, hregs_n2, hstk_n2, hbm_n2, hres_n2,
                                            hn2_valid, hcdst_valid, hsm1⟩,
                                          fun _ => ⟨n4, n6, n7, c_dst1, c_src1, vc4, vc5, ?_,
                                            hnext2, hcstr2, hpc_n6, hregs_n6, hstk_n6, hbm_n6, hres_n6,
                                            hn6_valid, hcdst1_valid, hsm2⟩⟩
                                        · rw [hp_vc6, hp_vc4, hm6, hl6, hpending]; simp
                                        · rw [hp_vc6, hp_vc4, hm6, hl6, hpending]; simp
                      | some b4 =>
                        cases b4 with
                        | true =>
                          cases ok_inj hteq; cases ok_inj htne
                          simp only [↓reduceIte, Type_clone_spec, bind_ok] at hcmp
                          rcases bind_eq_ok hcmp with ⟨r6, hnext1, hcmp⟩
                          cases r6 with
                          | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                          | Ok n =>
                            rcases bind_eq_ok hcmp with ⟨⟨r7, n1⟩, hcstr1, hcmp⟩
                            cases r7 with
                            | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                            | Ok pair =>
                              rcases pair with ⟨c_dst, c_src⟩
                              rcases bind_eq_ok hcmp with ⟨⟨r8, n2⟩, hset1, hcmp⟩
                              cases r8 with
                              | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                              | Ok _ =>
                                rcases bind_eq_ok hcmp with ⟨⟨r9, n3, vc3⟩, hsm1, hcmp⟩
                                cases r9 with
                                | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                | Ok _ =>
                                  rcases bind_eq_ok hcmp with ⟨vc4, hpush1, hcmp⟩
                                  rcases Prod.mk.inj (ok_inj hcmp) with ⟨_, rfl⟩
                                  obtain ⟨hn1_eq, hpc_n2, hregs_n2, hstk_n2, hbm_n2, hres_n2,
                                      hn2_valid, hcdst_valid, hp_vc4⟩ :=
                                    cmpxchg_branch_backward_exists_succ hctx hld_valid hr0_valid
                                      (Or.inl rfl) (fun _ => hv2_valid) hnext1 hcstr1 hset1 hsm1 hpush1
                                  rw [hn1_eq] at hcstr1
                                  refine ⟨fun _ => ⟨n, n2, n3, c_dst, c_src, vc2, vc3, ?_,
                                      hnext1, hcstr1, hpc_n2, hregs_n2, hstk_n2, hbm_n2, hres_n2,
                                      hn2_valid, hcdst_valid, hsm1⟩,
                                    fun hc => absurd rfl hc⟩
                                  rw [hp_vc4, hm6, hl6, hpending]; simp
                        | false =>
                          cases ok_inj hteq; cases ok_inj htne
                          simp only [Bool.false_eq_true, ↓reduceIte] at hcmp
                          rcases bind_eq_ok hcmp with ⟨r6, hnext1, hcmp⟩
                          cases r6 with
                          | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                          | Ok n =>
                            rcases bind_eq_ok hcmp with ⟨⟨r7, n1⟩, hcstr1, hcmp⟩
                            cases r7 with
                            | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                            | Ok pair =>
                              rcases pair with ⟨c_dst, c_src⟩
                              rcases bind_eq_ok hcmp with ⟨t_cl, hcl, hcmp⟩
                              cases ok_inj (hcl.symm.trans (Type_clone_spec c_dst))
                              rcases bind_eq_ok hcmp with ⟨⟨r8, n2⟩, hset1, hcmp⟩
                              cases r8 with
                              | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                              | Ok _ =>
                                rcases bind_eq_ok hcmp with ⟨⟨r9, n3, vc3⟩, hsm1, hcmp⟩
                                cases r9 with
                                | Err _ => cases (Prod.mk.inj (ok_inj hcmp)).1
                                | Ok _ =>
                                  rcases bind_eq_ok hcmp with ⟨vc4, hpush1, hcmp⟩
                                  rcases Prod.mk.inj (ok_inj hcmp) with ⟨_, rfl⟩
                                  obtain ⟨hn1_eq, hpc_n2, hregs_n2, hstk_n2, hbm_n2, hres_n2,
                                      hn2_valid, hcdst_valid, hp_vc4⟩ :=
                                    cmpxchg_branch_backward_exists_succ hctx hld_valid hr0_valid
                                      (Or.inr rfl) id hnext1 hcstr1 hset1 hsm1 hpush1
                                  rw [hn1_eq] at hcstr1
                                  refine ⟨fun hc => absurd rfl hc,
                                    fun _ => ⟨n, n2, n3, c_dst, c_src, vc2, vc3, ?_,
                                      hnext1, hcstr1, hpc_n2, hregs_n2, hstk_n2, hbm_n2, hres_n2,
                                      hn2_valid, hcdst_valid, hsm1⟩⟩
                                  rw [hp_vc4, hm6, hl6, hpending]; simp
                    unfold verifier.DataDependencies.atomic at hback
                    rw [bind_ok_eq hreg_dst] at hback
                    dsimp only at hback
                    rw [bind_ok_eq hinner] at hback
                    dsimp only at hback
                    unfold verifier.ComputationContext.load_memory at hlm
                    rw [bind_ok_eq hinner] at hlm
                    dsimp only at hlm
                    have hn_dst_ge : ¬ (dst8.val ≥ REGISTER_COUNT) := by omega
                    have hn_src_ge : ¬ (src8.val ≥ REGISTER_COUNT) := by omega
                    have hvd_ne_uninit : value_data ≠ uninitScalar64 := fun hc => by
                      subst hc; exact uninitScalar64_unwritten_ne_zero hvd_unw
                    have hr0_ne_uninit : r0_data ≠ uninitScalar64 := fun hc => by
                      subst hc; exact uninitScalar64_unwritten_ne_zero hr0_unw
                    have hld_ne_uninit : loaded_data ≠ uninitScalar64 := fun hc => by
                      subst hc; exact uninitScalar64_unwritten_ne_zero hld_unw
                    rcases hcaa_cases with ⟨stack_offset, hu_stk, hal_stk⟩ |
                      ⟨id, offset, buffer_size, origin, hu_mem, hmap_orig, hal_mem⟩
                    · -- Case 1: PtrToStack
                      rw [hu_stk] at hback hlm
                      dsimp only at hback hlm
                      rcases bind_eq_ok hlm with ⟨offset_sum, hadd_off, hlm⟩
                      rw [bind_ok_eq hadd_off] at hback
                      rcases bind_eq_ok hlm with ⟨r_sl, hsl, hlm⟩
                      cases r_sl with
                      | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
                      | Ok v_sl =>
                        dsimp only at hlm
                        rcases bind_eq_ok hlm with ⟨⟨r_ma_l, _⟩, _, hlm⟩
                        cases r_ma_l with
                        | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
                        | Ok _ =>
                          have hv_sl_eq : v_sl = v1 := core.result.Result.Ok.inj (Prod.mk.inj (ok_inj hlm)).1
                          subst v_sl
                          rw [hv1_eq] at hsl
                          obtain ⟨rchk0, hrchk0, hchk_iff⟩ := StackOffset_check_valid_offset_spec offset_sum
                          have hrchk_ok : rchk0 = .Ok () := by
                            unfold verifier.Stack.load at hsl
                            rw [bind_ok_eq hrchk0] at hsl
                            cases rchk0 with
                            | Err _ => cases ok_inj hsl
                            | Ok u => cases u; rfl
                          subst hrchk_ok
                          obtain ⟨hoff_kn, hoff_lt⟩ := (hchk_iff ()).mp rfl
                          rw [bind_ok_eq hrchk0] at hback
                          dsimp only at hback
                          obtain ⟨slot_u, hslot_u, hslot_val⟩ := StackOffset_array_index_spec (BitVec.le_of_lt hoff_lt)
                          have hoff_512 : offset_sum.value.bv.toNat < 512 := by
                            rw [BitVec.lt_def] at hoff_lt; simpa using hoff_lt
                          have hslot_lt64 : slot_u.val < 64 := by omega
                          rw [bind_ok_eq hslot_u] at hback
                          rcases bind_eq_ok hback with ⟨dd_stk, hadd_stk, hback⟩
                          obtain ⟨_, _, _, hstk_add_eq, hsub_stk⟩ := DataDependencies_add_stack_ok_spec hadd_stk
                          obtain ⟨hdst_live, hsrc_live, hr0_live_fn, hsub_regs⟩ :=
                            DataDependencies_atomic_non_stack_spec hback
                          have hr0_live := hr0_live_fn rfl
                          have hslot_live : DataDepsHasStack dd slot_u.val = true :=
                            hsub_regs.2 slot_u.val hslot_lt64 (by rw [hstk_add_eq slot_u.val hslot_lt64]; simp)
                          have hsub_all : DepsSubset dPost dd := DepsSubset_trans hsub_stk hsub_regs
                          intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
                          obtain ⟨_, _, hrel_dst⟩ :=
                            ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hdst_live) hreg_dst
                          obtain ⟨_, _, hca_dst1, hca_dst2⟩ :=
                            Type_inner_ok_valid_and_checkAlive_none hctx hrel hv_valid hv_ne_uninit hinner hrel_dst
                          obtain ⟨_, _, baseOff, hconc_base, hv_base1, hv_base2⟩ :=
                            unwrap_stack_ptr_rel2 hv_valid hu_stk hrel_dst
                          obtain ⟨_, _, hrel_src⟩ :=
                            ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hsrc_live) hreg_src
                          rw [hv2_eq] at hrel_src
                          simp only [TypeRel2, hvd_ne_uninit, ↓reduceIte] at hrel_src
                          rcases hrel_src with ⟨sVal₁, sVal₂, hv_src1, hv_src2, hconc_vd⟩
                          have hsVal_eq := fully_initialized_eq_of_rel2_64 hvd_unw hconc_vd
                          subst hsVal_eq
                          have h0_val : (0#u8).val = 0 := rfl
                          obtain ⟨_, _, hrel_r0⟩ :=
                            ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr (by rwa [h0_val])) hreg_r0
                          rw [h0_val, hv3_eq] at hrel_r0
                          simp only [TypeRel2, hr0_ne_uninit, ↓reduceIte] at hrel_r0
                          rcases hrel_r0 with ⟨r0Val₁, r0Val₂, hv_r0_1, hv_r0_2, hconc_r0⟩
                          have hr0Val_eq := fully_initialized_eq_of_rel2_64 hr0_unw hconc_r0
                          subst hr0Val_eq
                          obtain ⟨off_s, hadd_s, hconc_sum⟩ :=
                            StackOffset_add_i16_sound_2safety stack_offset off hconc_base
                          rw [hadd_off] at hadd_s; cases ok_inj hadd_s
                          have heff_eq : baseOff + sextOffset16 off.bv = offset_sum.value.bv :=
                            eq_value_of_known_64 hoff_kn hconc_sum.1
                          have hslot_idx_eq : (baseOff + sextOffset16 off.bv).toNat / 8 = slot_u.val := by
                            rw [heff_eq, hslot_val]
                          have hslot_rel_pre := hrel.2.2.2.2.2.2.2.2.2.2.2
                            ((baseOff + sextOffset16 off.bv).toNat / 8)
                            (by rw [hslot_idx_eq]; exact hslot_lt64)
                            (by rw [hslot_idx_eq]; exact hslot_live)
                          obtain ⟨heff_lt512, oldSlot₁, initMask₁, oldSlot₂, initMask₂,
                              hstk1, hstk2, hinit1, hinit2, hold_eq, hconc_ld⟩ :=
                            Stack_load_written_scalar_rel2 hctx.2.1 hld_unw hconc_sum hsl hslot_rel_pre
                          let w : DataWidth := if is64 then .u64 else .u32
                          have hw_core : DataWidth.ofCore (if is64 then api.DataWidth.U64 else .U32) = w := by
                            cases is64 <;> rfl
                          rw [hw_core] at hinit1 hinit2 hold_eq hconc_ld
                          let effOff := baseOff + sextOffset16 off.bv
                          let slotIdx := effOff.toNat / 8
                          let subIdx := effOff.toNat % 8
                          let oldSlice := extractSubValue oldSlot₁ subIdx w
                          have hrel_v1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen v1
                              (.scalar oldSlice) (.scalar oldSlice) := by
                            rw [hv1_eq]; exact TypeRel2_scalar_rel2_of_concretizes hconc_ld
                          have hrel_v3 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen v3
                              (.scalar r0Val₁) (.scalar r0Val₁) := by
                            rw [hv3_eq]; exact TypeRel2_scalar_rel2_of_concretizes hconc_r0
                          have hcb_sound := (compute_branch_ok_sound (op := .eq) hctx hrel hl₁ hl₂
                            hv1_valid hv3_valid hrel_v1 hrel_v3 hcb).2.2.2.2.2
                          let cond := if is64 then evalJump64 .eq oldSlice r0Val₁ else evalJump32 .eq oldSlice r0Val₁
                          -- Select active branch (`Eq` if `cond = true`, `Ne` if `cond = false`):
                          have hactive :
                              ∃ (n3 : verifier.ComputationContext) (c_dst storedTy : verifier.Type)
                                (s' : verifier.Stack) (effStored : BitVec 64),
                                some n3 ∈ vctx1.pending_successors.val ∧
                                n3.pc.val = ctx.pc.val + 1 ∧
                                n3.registers.val = ctx.registers.val.set 0 c_dst ∧
                                n3.stack = s' ∧
                                n3.array_bounds = ctx.array_bounds ∧
                                n3.resources = ctx.resources ∧
                                TypeValid env vCounter storedTy ∧
                                TypeRel2 env ρ resActive isNullId st₁.world.arrayLen c_dst
                                  (.scalar oldSlice) (.scalar oldSlice) ∧
                                TypeRel2 env ρ resActive isNullId st₁.world.arrayLen storedTy
                                  (.scalar effStored) (.scalar effStored) ∧
                                verifier.Stack.store ctx.stack offset_sum storedTy
                                  (if is64 then .U64 else .U32) = ok (.Ok (), s') ∧
                                (∀ orig si, insertSubValue orig
                                  (if is64 then (if oldSlice == r0Val₁ then sVal₁ else oldSlice)
                                   else ((if oldSlice.setWidth 32 == r0Val₁.setWidth 32 then sVal₁.setWidth 32
                                          else oldSlice.setWidth 32).zeroExtend 64)) si w =
                                  insertSubValue orig effStored si w) := by
                            cases hcond : cond with
                            | true =>
                              have hb3_ne_false : b3 ≠ some false := by
                                intro hc; have := hcb_sound false hc
                                dsimp only [RuntimeVal.toBits] at this
                                change cond = false at this
                                rw [hcond] at this; cases this
                              obtain ⟨n, n2, n3, c_dst, c_src, vc_in, vc_sm,
                                  hmem3, hnext1, hcstr1, hpc_n2, hregs_n2, hstk_n2, hbm_n2, hres_n2,
                                  _, _, hsm1⟩ := hbranches.1 hb3_ne_false
                              obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext1
                              have hn_valid : CompCtxValid env vCounter n := by rw [hn_eq]; exact hctx
                              have hrel_n : StateRel2Deps env n dd ρ resActive isNullId
                                  { st₁ with pc := n.pc.val } { st₂ with pc := n.pc.val } := by
                                rcases hrel with ⟨_, _, hw, harr, hnull, hnd, hlv, hbnd, hr10_1, hr10_2, hrg, hsk⟩
                                rw [hn_eq]; exact ⟨rfl, rfl, hw, harr, hnull, hnd, hlv, hbnd, hr10_1, hr10_2, hrg, hsk⟩
                              have hjt_eq : JumpTypeHolds .Eq (jumpWidthOfBool is64) oldSlice r0Val₁ := by
                                cases is64 <;> simp [cond, evalJump64, evalJump32] at hcond <;>
                                  simp [JumpTypeHolds, jumpWidthOfBool, hcond]
                              obtain ⟨_, _, _, _, hrel_cdst, _⟩ :=
                                Type_constraint_ok_sound (l₁ := l₁) hl₁ hn_valid hrel_n
                                  (show TypeValid env vCounter (.ScalarValue loaded_data) from hld_valid)
                                  (show TypeValid env vCounter (.ScalarValue r0_data) from hr0_valid)
                                  (TypeRel2_scalar_rel2_of_concretizes hconc_ld)
                                  (TypeRel2_scalar_rel2_of_concretizes hconc_r0)
                                  hjt_eq hcstr1
                              unfold verifier.ComputationContext.store_memory at hsm1
                              rcases bind_eq_ok hsm1 with ⟨rc_s, _, hsm1⟩
                              cases rc_s with
                              | Err _ => cases (Prod.mk.inj (ok_inj hsm1)).1
                              | Ok _ =>
                                dsimp only at hsm1
                                rw [bind_ok_eq (Type_unwrap_releasable_spec v), hu_stk] at hsm1
                                rcases bind_eq_ok hsm1 with ⟨_, _, hsm1⟩
                                dsimp only at hsm1
                                rw [bind_ok_eq hadd_off, hstk_n2] at hsm1
                                rcases bind_eq_ok hsm1 with ⟨⟨r_st, s'⟩, hst_op, hsm1⟩
                                cases r_st with
                                | Err _ => cases (Prod.mk.inj (ok_inj hsm1)).1
                                | Ok _ =>
                                  rcases bind_eq_ok hsm1 with ⟨_, _, hsm1⟩
                                  rcases Prod.mk.inj (Prod.mk.inj (ok_inj hsm1)).2 with ⟨hn3_eq, _⟩
                                  refine ⟨n3, c_dst, v2, s', sVal₁, hmem3,
                                    by rw [← hn3_eq]; exact hpc_n2,
                                    by rw [← hn3_eq]; exact hregs_n2,
                                    by rw [← hn3_eq],
                                    by rw [← hn3_eq]; exact hbm_n2,
                                    by rw [← hn3_eq]; exact hres_n2,
                                    hv2_valid, hrel_cdst,
                                    by rw [hv2_eq]; exact TypeRel2_scalar_rel2_of_concretizes hconc_vd,
                                    hst_op, fun orig si => ?_⟩
                                  cases is64 with
                                  | true =>
                                    simp [cond, evalJump64] at hcond
                                    simp [hcond]
                                  | false =>
                                    simp [cond, evalJump32] at hcond
                                    simp [w, hcond, insertSubValue_u32_zext]
                            | false =>
                              have hb3_ne_true : b3 ≠ some true := by
                                intro hc; have := hcb_sound true hc
                                dsimp only [RuntimeVal.toBits] at this
                                change cond = true at this
                                rw [hcond] at this; cases this
                              obtain ⟨n, n2, n3, c_dst, c_src, vc_in, vc_sm,
                                  hmem3, hnext1, hcstr1, hpc_n2, hregs_n2, hstk_n2, hbm_n2, hres_n2,
                                  _, hcdst_valid, hsm1⟩ := hbranches.2 hb3_ne_true
                              obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext1
                              have hn_valid : CompCtxValid env vCounter n := by rw [hn_eq]; exact hctx
                              have hrel_n : StateRel2Deps env n dd ρ resActive isNullId
                                  { st₁ with pc := n.pc.val } { st₂ with pc := n.pc.val } := by
                                rcases hrel with ⟨_, _, hw, harr, hnull, hnd, hlv, hbnd, hr10_1, hr10_2, hrg, hsk⟩
                                rw [hn_eq]; exact ⟨rfl, rfl, hw, harr, hnull, hnd, hlv, hbnd, hr10_1, hr10_2, hrg, hsk⟩
                              have hjt_ne : JumpTypeHolds .Ne (jumpWidthOfBool is64) oldSlice r0Val₁ := by
                                cases is64 <;> simp [cond, evalJump64, evalJump32] at hcond <;>
                                  simp [JumpTypeHolds, jumpWidthOfBool, hcond]
                              obtain ⟨_, _, _, _, hrel_cdst, _⟩ :=
                                Type_constraint_ok_sound (l₁ := l₁) hl₁ hn_valid hrel_n
                                  (show TypeValid env vCounter (.ScalarValue loaded_data) from hld_valid)
                                  (show TypeValid env vCounter (.ScalarValue r0_data) from hr0_valid)
                                  (TypeRel2_scalar_rel2_of_concretizes hconc_ld)
                                  (TypeRel2_scalar_rel2_of_concretizes hconc_r0)
                                  hjt_ne hcstr1
                              unfold verifier.ComputationContext.store_memory at hsm1
                              rcases bind_eq_ok hsm1 with ⟨rc_s, _, hsm1⟩
                              cases rc_s with
                              | Err _ => cases (Prod.mk.inj (ok_inj hsm1)).1
                              | Ok _ =>
                                dsimp only at hsm1
                                rw [bind_ok_eq (Type_unwrap_releasable_spec v), hu_stk] at hsm1
                                rcases bind_eq_ok hsm1 with ⟨_, _, hsm1⟩
                                dsimp only at hsm1
                                rw [bind_ok_eq hadd_off, hstk_n2] at hsm1
                                rcases bind_eq_ok hsm1 with ⟨⟨r_st, s'⟩, hst_op, hsm1⟩
                                cases r_st with
                                | Err _ => cases (Prod.mk.inj (ok_inj hsm1)).1
                                | Ok _ =>
                                  rcases bind_eq_ok hsm1 with ⟨_, _, hsm1⟩
                                  rcases Prod.mk.inj (Prod.mk.inj (ok_inj hsm1)).2 with ⟨hn3_eq, _⟩
                                  refine ⟨n3, c_dst, c_dst, s', oldSlice, hmem3,
                                    by rw [← hn3_eq]; exact hpc_n2,
                                    by rw [← hn3_eq]; exact hregs_n2,
                                    by rw [← hn3_eq],
                                    by rw [← hn3_eq]; exact hbm_n2,
                                    by rw [← hn3_eq]; exact hres_n2,
                                    hcdst_valid, hrel_cdst, hrel_cdst,
                                    hst_op, fun orig si => ?_⟩
                                  cases is64 with
                                  | true =>
                                    have hneq : (oldSlice == r0Val₁) = false := hcond
                                    simp [w, hneq]
                                  | false =>
                                    have hneq : (oldSlice.setWidth 32 == r0Val₁.setWidth 32) = false := hcond
                                    simp [w, hneq, insertSubValue_u32_zext]
                          rcases hactive with ⟨n3, c_dst, storedTy, s', effStored,
                            hmem3, hpc_n3, hregs_n3, hstk_n3, hbm_n3, hres_n3,
                            hstored_valid, hrel_cdst, hrel_stored, hst_op, hins_eq⟩
                          obtain ⟨r0_st, s0_st, h0_st, _, _, hpost_st⟩ :=
                            Stack_store_spec hctx.2.1 offset_sum hstored_valid (if is64 then .U64 else .U32)
                          rw [hst_op] at h0_st
                          rcases Prod.mk.inj (ok_inj h0_st) with ⟨r0_eq, hs0_eq⟩
                          subst hs0_eq
                          obtain ⟨_, _, _, hslots_other, _, _⟩ := hpost_st () r0_eq.symm
                          have hrel_ss := (Stack_store_rel2 (layout₁ := l₁) (layout₂ := l₂)
                            hctx.2.1 hstored_valid hconc_sum hst_op hrel_stored (fun _ => hslot_rel_pre)).2
                          dsimp only at hrel_ss
                          rw [hw_core] at hrel_ss
                          let newSlot₁ := StackSlot.bytes
                            (insertSubValue oldSlot₁ effStored subIdx w)
                            (insertSubValue initMask₁ (~~~0#64) subIdx w)
                          let newSlot₂ := StackSlot.bytes
                            (insertSubValue oldSlot₂ effStored subIdx w)
                            (insertSubValue initMask₂ (~~~0#64) subIdx w)
                          have hss_eq : StackSlotRel2 env ρ resActive isNullId st₁.world.arrayLen
                              (stackSlotType s' slotIdx) newSlot₁ newSlot₂ := by
                            dsimp only [newSlot₁, newSlot₂, slotIdx, subIdx, effOff]
                            cases is64 with
                            | false =>
                              simp only [w, Bool.false_eq_true, ↓reduceIte] at hstk1 hstk2 hrel_ss ⊢
                              rw [hstk1, hstk2] at hrel_ss
                              have hb_ne : (DataWidth.u32 == DataWidth.u64) = false := rfl
                              simpa only [hb_ne, Bool.false_eq_true, ↓reduceIte] using hrel_ss
                            | true =>
                              have hsub0 : (baseOff + sextOffset16 off.bv).toNat % 8 = 0 := by
                                have hal := hal_stk baseOff hconc_base.1
                                unfold isAligned DataWidth.bvBytes DataWidth.bytes at hal
                                rw [beq_iff_eq] at hal
                                have hto := congrArg BitVec.toNat hal
                                rw [BitVec.toNat_umod] at hto
                                exact hto
                              simp only [w, ↓reduceIte] at hrel_ss ⊢
                              rw [hsub0, insertSubValue_zero_u64, insertSubValue_zero_u64,
                                  insertSubValue_zero_u64, insertSubValue_zero_u64]
                              simpa only [beq_self_eq_true, ↓reduceIte] using hrel_ss
                          have hwb_match :
                              (some (0, oldSlice) = none ∧ some (0, c_dst) = none ∧ n3.registers = ctx.registers) ∨
                              (∃ r wbVal wbTy,
                                some (0, oldSlice) = some (r, wbVal) ∧
                                some (0, c_dst) = some (r, wbTy) ∧
                                r < GENERAL_REGISTER_COUNT ∧
                                n3.registers.val = ctx.registers.val.set r wbTy ∧
                                TypeRel2 env ρ resActive isNullId st₁.world.arrayLen wbTy (.scalar wbVal) (.scalar wbVal)) :=
                            Or.inr ⟨0, oldSlice, c_dst, rfl, rfl, by decide, hregs_n3, hrel_cdst⟩
                          have hrel_post := StateRel2Deps_atomic_stack
                            (by dsimp only [slotIdx, effOff]; rw [hslot_idx_eq]; exact hslot_lt64)
                            hstk_n3 hbm_n3 hres_n3
                            (fun s hs hne => hslots_other s hs (by rwa [← heff_eq]))
                            hsub_all hwb_match hrel (fun _ => hss_eq)
                          have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
                            simp only [bumpWorldStep]; omega
                          obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
                            hchildren n3 hmem3 l₁ l₂ s₁ s₂ ρ resActive isNullId
                              { pc := n3.pc.val, regs := applyAtomicWb st₁.regs (some (0, oldSlice)),
                                stack := updateStack st₁.stack slotIdx newSlot₁, world := bumpWorldStep st₁.world }
                              { pc := n3.pc.val, regs := applyAtomicWb st₂.regs (some (0, oldSlice)),
                                stack := updateStack st₂.stack slotIdx newSlot₂, world := bumpWorldStep st₂.world }
                              hl₁ hl₂ hleak halloc' hρ hrel_post
                          have hn_ge512 : ¬ (baseOff + sextOffset16 off.bv ≥ 512#64) :=
                            BitVec.not_le.mpr heff_lt512
                          have hal_eff := hal_stk baseOff hconc_base.1
                          have hinit_bne1 : (extractSubValue initMask₁ subIdx w != w.valMask64) = false :=
                            bne_eq_false_iff_eq.mpr hinit1
                          have hinit_bne2 : (extractSubValue initMask₂ subIdx w != w.valMask64) = false :=
                            bne_eq_false_iff_eq.mpr hinit2
                          dsimp only [subIdx, effOff, w] at hinit_bne1 hinit_bne2 hold_eq
                          refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
                          · unfold runSteps step
                            rw [hv_base1] at hca_dst1
                            simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, hn_src_ge,
                              Bool.false_and, Bool.false_eq_true, beq_self_eq_true, ↓reduceIte,
                              hca_dst1, hv_base1, hn_ge512, hal_eff, Bool.not_true,
                              hv_src1, hv_r0_1, hstk1, hinit_bne1]
                            rw [stepNextIte_eq_next (by
                              dsimp only [newSlot₁]
                              rw [hpc_n3, ← hins_eq oldSlot₁ subIdx]
                              dsimp only [slotIdx, subIdx, effOff, oldSlice, w, applyAtomicWb]
                              cases is64 <;> simp [extractSubValue_u32_zext]) hr₁]
                            exact hr₁
                          · unfold runSteps step
                            rw [hv_base2] at hca_dst2
                            simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, hn_src_ge,
                              Bool.false_and, Bool.false_eq_true, beq_self_eq_true, ↓reduceIte,
                              hca_dst2, hv_base2, hn_ge512, hal_eff, Bool.not_true,
                              hv_src2, hv_r0_2, hstk2, hinit_bne2]
                            rw [stepNextIte_eq_next (by
                              dsimp only [newSlot₂]
                              rw [hpc_n3, ← hins_eq oldSlot₂ subIdx]
                              dsimp only [slotIdx, subIdx, effOff, oldSlice, w, applyAtomicWb]
                              rw [hold_eq]
                              cases is64 <;> simp [extractSubValue_u32_zext]) hr₂]
                            exact hr₂
                    · -- Case 2: PtrToMemory
                      rw [hu_mem] at hback hlm
                      dsimp only at hback hlm
                      obtain ⟨hdst_live, hsrc_live, hr0_live_fn, hsub_all⟩ :=
                        DataDependencies_atomic_non_stack_spec hback
                      have hr0_live := hr0_live_fn rfl
                      rcases bind_eq_ok hlm with ⟨bw, hbw, hlm⟩
                      rcases bind_eq_ok hlm with ⟨r_chk, hchk, hlm⟩
                      cases r_chk with
                      | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
                      | Ok _ =>
                        dsimp only at hlm
                        rw [bind_ok_eq (Type_unknown_written_scalar_eq _)] at hlm
                        rcases bind_eq_ok hlm with ⟨⟨r_ma_l, _⟩, _, hlm⟩
                        cases r_ma_l with
                        | Err _ => cases (Prod.mk.inj (ok_inj hlm)).1
                        | Ok _ =>
                          have hv1_unk : .ScalarValue (unknownWrittenScalarWidth (DataWidth.ofCore (if is64 then api.DataWidth.U64 else .U32))) = v1 :=
                            core.result.Result.Ok.inj (Prod.mk.inj (ok_inj hlm)).1
                          intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
                          obtain ⟨_, _, hrel_dst⟩ :=
                            ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hdst_live) hreg_dst
                          obtain ⟨_, _, hca_dst1, hca_dst2⟩ :=
                            Type_inner_ok_valid_and_checkAlive_none hctx hrel hv_valid hv_ne_uninit hinner hrel_dst
                          obtain ⟨_, _, _, baseOff, hconc_base, hv_base1, hv_base2⟩ :=
                            unwrap_mem_ptr_rel2 hv_valid hu_mem hrel_dst
                          obtain ⟨_, _, hrel_src⟩ :=
                            ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr hsrc_live) hreg_src
                          rw [hv2_eq] at hrel_src
                          simp only [TypeRel2, hvd_ne_uninit, ↓reduceIte] at hrel_src
                          rcases hrel_src with ⟨sVal₁, sVal₂, hv_src1, hv_src2, hconc_vd⟩
                          have hsVal_eq := fully_initialized_eq_of_rel2_64 hvd_unw hconc_vd
                          subst hsVal_eq
                          have h0_val : (0#u8).val = 0 := rfl
                          obtain ⟨_, _, hrel_r0⟩ :=
                            ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr (by rwa [h0_val])) hreg_r0
                          rw [h0_val, hv3_eq] at hrel_r0
                          simp only [TypeRel2, hr0_ne_uninit, ↓reduceIte] at hrel_r0
                          rcases hrel_r0 with ⟨r0Val₁, r0Val₂, hv_r0_1, hv_r0_2, hconc_r0⟩
                          have hr0Val_eq := fully_initialized_eq_of_rel2_64 hr0_unw hconc_r0
                          subst hr0Val_eq
                          let w : DataWidth := if is64 then .u64 else .u32
                          have hw_core : DataWidth.ofCore (if is64 then api.DataWidth.U64 else .U32) = w := by
                            cases is64 <;> rfl
                          have hin_bnds : inBoundsRange (baseOff + sextOffset16 off.bv) w buffer_size.bv = true := by
                            simpa only [hw_core] using
                              ComputationContext_check_memory_access_ok_spec hbw hconc_base.1 hchk
                          have hal_eff := hal_mem baseOff hconc_base.1
                          let effOff := baseOff + sextOffset16 off.bv
                          let oldVal := readBufWord st₁.world.bufBytes (renameMemId ρ id) effOff w
                          have hld_eq_unk : loaded_data = unknownWrittenScalarWidth w := by
                            rw [← hw_core]
                            exact (verifier.Type.ScalarValue.inj (hv1_unk.trans hv1_eq)).symm
                          have hconc_ld : ConcretizesScalar64Rel2 loaded_data oldVal oldVal := by
                            rw [hld_eq_unk]
                            exact concretizesScalar64Rel2_readBufWord _ _ _ w
                          have hrel_v1 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen v1
                              (.scalar oldVal) (.scalar oldVal) := by
                            rw [hv1_eq]; exact TypeRel2_scalar_rel2_of_concretizes hconc_ld
                          have hrel_v3 : TypeRel2 env ρ resActive isNullId st₁.world.arrayLen v3
                              (.scalar r0Val₁) (.scalar r0Val₁) := by
                            rw [hv3_eq]; exact TypeRel2_scalar_rel2_of_concretizes hconc_r0
                          have hcb_sound := (compute_branch_ok_sound (op := .eq) hctx hrel hl₁ hl₂
                            hv1_valid hv3_valid hrel_v1 hrel_v3 hcb).2.2.2.2.2
                          let cond := if is64 then evalJump64 .eq oldVal r0Val₁ else evalJump32 .eq oldVal r0Val₁
                          have hactive :
                              ∃ (n3 : verifier.ComputationContext) (c_dst : verifier.Type)
                                (effStored : BitVec 64),
                                some n3 ∈ vctx1.pending_successors.val ∧
                                n3.pc.val = ctx.pc.val + 1 ∧
                                n3.registers.val = ctx.registers.val.set 0 c_dst ∧
                                n3.stack = ctx.stack ∧
                                n3.array_bounds = ctx.array_bounds ∧
                                n3.resources = ctx.resources ∧
                                (MemoryOrigin.ofCore origin).isWritable = true ∧
                                TypeRel2 env ρ resActive isNullId st₁.world.arrayLen c_dst
                                  (.scalar oldVal) (.scalar oldVal) ∧
                                (∀ buf mid o, writeBufWord buf mid o w
                                  (if is64 then (if oldVal == r0Val₁ then sVal₁ else oldVal)
                                   else ((if oldVal.setWidth 32 == r0Val₁.setWidth 32 then sVal₁.setWidth 32
                                          else oldVal.setWidth 32).zeroExtend 64)) =
                                  writeBufWord buf mid o w effStored) := by
                            cases hcond : cond with
                            | true =>
                              have hb3_ne_false : b3 ≠ some false := by
                                intro hc; have := hcb_sound false hc
                                dsimp only [RuntimeVal.toBits] at this
                                change cond = false at this
                                rw [hcond] at this; cases this
                              obtain ⟨n, n2, n3, c_dst, c_src, vc_in, vc_sm,
                                  hmem3, hnext1, hcstr1, hpc_n2, hregs_n2, hstk_n2, hbm_n2, hres_n2,
                                  _, _, hsm1⟩ := hbranches.1 hb3_ne_false
                              obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext1
                              have hn_valid : CompCtxValid env vCounter n := by rw [hn_eq]; exact hctx
                              have hrel_n : StateRel2Deps env n dd ρ resActive isNullId
                                  { st₁ with pc := n.pc.val } { st₂ with pc := n.pc.val } := by
                                rcases hrel with ⟨_, _, hw, harr, hnull, hnd, hlv, hbnd, hr10_1, hr10_2, hrg, hsk⟩
                                rw [hn_eq]; exact ⟨rfl, rfl, hw, harr, hnull, hnd, hlv, hbnd, hr10_1, hr10_2, hrg, hsk⟩
                              have hjt_eq : JumpTypeHolds .Eq (jumpWidthOfBool is64) oldVal r0Val₁ := by
                                cases is64 <;> simp [cond, evalJump64, evalJump32] at hcond <;>
                                  simp [JumpTypeHolds, jumpWidthOfBool, hcond]
                              obtain ⟨_, _, _, _, hrel_cdst, _⟩ :=
                                Type_constraint_ok_sound (l₁ := l₁) hl₁ hn_valid hrel_n
                                  (show TypeValid env vCounter (.ScalarValue loaded_data) from hld_valid)
                                  (show TypeValid env vCounter (.ScalarValue r0_data) from hr0_valid)
                                  (TypeRel2_scalar_rel2_of_concretizes hconc_ld)
                                  (TypeRel2_scalar_rel2_of_concretizes hconc_r0)
                                  hjt_eq hcstr1
                              unfold verifier.ComputationContext.store_memory at hsm1
                              rcases bind_eq_ok hsm1 with ⟨rc_s, _, hsm1⟩
                              cases rc_s with
                              | Err _ => cases (Prod.mk.inj (ok_inj hsm1)).1
                              | Ok _ =>
                                dsimp only at hsm1
                                rw [bind_ok_eq (Type_unwrap_releasable_spec v), hu_mem] at hsm1
                                rcases bind_eq_ok hsm1 with ⟨is_i, _, hsm1⟩
                                dsimp only at hsm1
                                rw [bind_ok_eq (PointerOrigin_is_writable_spec origin)] at hsm1
                                cases hw_orig : (MemoryOrigin.ofCore origin).isWritable <;>
                                  simp only [hw_orig, Bool.false_eq_true, ↓reduceIte] at hsm1
                                · cases (Prod.mk.inj (ok_inj hsm1)).1
                                · rcases bind_eq_ok hsm1 with ⟨_, _, hsm1⟩
                                  rcases bind_eq_ok hsm1 with ⟨r_ck, _, hsm1⟩
                                  cases r_ck with
                                  | Err _ => cases (Prod.mk.inj (ok_inj hsm1)).1
                                  | Ok _ =>
                                    cases is_i <;> simp only [Bool.false_eq_true, ↓reduceIte] at hsm1
                                    · cases (Prod.mk.inj (ok_inj hsm1)).1
                                    · rcases bind_eq_ok hsm1 with ⟨_, _, hsm1⟩
                                      rcases Prod.mk.inj (Prod.mk.inj (ok_inj hsm1)).2 with ⟨hn3_eq, _⟩
                                      refine ⟨n3, c_dst, sVal₁, hmem3,
                                        by rw [← hn3_eq]; exact hpc_n2,
                                        by rw [← hn3_eq]; exact hregs_n2,
                                        by rw [← hn3_eq]; exact hstk_n2,
                                        by rw [← hn3_eq]; exact hbm_n2,
                                        by rw [← hn3_eq]; exact hres_n2,
                                        rfl, hrel_cdst, fun buf mid o => ?_⟩
                                      cases is64 with
                                      | true =>
                                        simp [cond, evalJump64] at hcond; simp [hcond]
                                      | false =>
                                        simp [cond, evalJump32] at hcond
                                        simp [w, hcond, writeBufWord_u32_zext]
                            | false =>
                              have hb3_ne_true : b3 ≠ some true := by
                                intro hc; have := hcb_sound true hc
                                dsimp only [RuntimeVal.toBits] at this
                                change cond = true at this
                                rw [hcond] at this; cases this
                              obtain ⟨n, n2, n3, c_dst, c_src, vc_in, vc_sm,
                                  hmem3, hnext1, hcstr1, hpc_n2, hregs_n2, hstk_n2, hbm_n2, hres_n2,
                                  _, _, hsm1⟩ := hbranches.2 hb3_ne_true
                              obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext1
                              have hn_valid : CompCtxValid env vCounter n := by rw [hn_eq]; exact hctx
                              have hrel_n : StateRel2Deps env n dd ρ resActive isNullId
                                  { st₁ with pc := n.pc.val } { st₂ with pc := n.pc.val } := by
                                rcases hrel with ⟨_, _, hw, harr, hnull, hnd, hlv, hbnd, hr10_1, hr10_2, hrg, hsk⟩
                                rw [hn_eq]; exact ⟨rfl, rfl, hw, harr, hnull, hnd, hlv, hbnd, hr10_1, hr10_2, hrg, hsk⟩
                              have hjt_ne : JumpTypeHolds .Ne (jumpWidthOfBool is64) oldVal r0Val₁ := by
                                cases is64 <;> simp [cond, evalJump64, evalJump32] at hcond <;>
                                  simp [JumpTypeHolds, jumpWidthOfBool, hcond]
                              obtain ⟨_, _, _, _, hrel_cdst, _⟩ :=
                                Type_constraint_ok_sound (l₁ := l₁) hl₁ hn_valid hrel_n
                                  (show TypeValid env vCounter (.ScalarValue loaded_data) from hld_valid)
                                  (show TypeValid env vCounter (.ScalarValue r0_data) from hr0_valid)
                                  (TypeRel2_scalar_rel2_of_concretizes hconc_ld)
                                  (TypeRel2_scalar_rel2_of_concretizes hconc_r0)
                                  hjt_ne hcstr1
                              unfold verifier.ComputationContext.store_memory at hsm1
                              rcases bind_eq_ok hsm1 with ⟨rc_s, _, hsm1⟩
                              cases rc_s with
                              | Err _ => cases (Prod.mk.inj (ok_inj hsm1)).1
                              | Ok _ =>
                                dsimp only at hsm1
                                rw [bind_ok_eq (Type_unwrap_releasable_spec v), hu_mem] at hsm1
                                rcases bind_eq_ok hsm1 with ⟨is_i, _, hsm1⟩
                                dsimp only at hsm1
                                rw [bind_ok_eq (PointerOrigin_is_writable_spec origin)] at hsm1
                                cases hw_orig : (MemoryOrigin.ofCore origin).isWritable <;>
                                  simp only [hw_orig, Bool.false_eq_true, ↓reduceIte] at hsm1
                                · cases (Prod.mk.inj (ok_inj hsm1)).1
                                · rcases bind_eq_ok hsm1 with ⟨_, _, hsm1⟩
                                  rcases bind_eq_ok hsm1 with ⟨r_ck, _, hsm1⟩
                                  cases r_ck with
                                  | Err _ => cases (Prod.mk.inj (ok_inj hsm1)).1
                                  | Ok _ =>
                                    cases is_i <;> simp only [Bool.false_eq_true, ↓reduceIte] at hsm1
                                    · cases (Prod.mk.inj (ok_inj hsm1)).1
                                    · rcases bind_eq_ok hsm1 with ⟨_, _, hsm1⟩
                                      rcases Prod.mk.inj (Prod.mk.inj (ok_inj hsm1)).2 with ⟨hn3_eq, _⟩
                                      refine ⟨n3, c_dst, oldVal, hmem3,
                                        by rw [← hn3_eq]; exact hpc_n2,
                                        by rw [← hn3_eq]; exact hregs_n2,
                                        by rw [← hn3_eq]; exact hstk_n2,
                                        by rw [← hn3_eq]; exact hbm_n2,
                                        by rw [← hn3_eq]; exact hres_n2,
                                        rfl, hrel_cdst, fun buf mid o => ?_⟩
                                      cases is64 with
                                      | true =>
                                        have hneq : (oldVal == r0Val₁) = false := hcond
                                        simp [w, hneq]
                                      | false =>
                                        have hneq : (oldVal.setWidth 32 == r0Val₁.setWidth 32) = false := hcond
                                        simp [w, hneq, writeBufWord_u32_zext]
                          rcases hactive with ⟨n3, c_dst, effStored,
                            hmem3, hpc_n3, hregs_n3, hstk_n3, hbm_n3, hres_n3,
                            hw_orig, hrel_cdst, hwr_eq⟩
                          let w₁' : WorldState :=
                            { st₁.world with bufBytes := writeBufWord st₁.world.bufBytes (renameMemId ρ id) effOff w effStored }
                          let w₂' : WorldState :=
                            { st₂.world with bufBytes := writeBufWord st₂.world.bufBytes (renameMemId ρ id) effOff w effStored }
                          have hw'_eq : w₁' = w₂' := by dsimp [w₁', w₂']; rw [hrel.2.2.1]
                          have hwb_match :
                              (some (0, oldVal) = none ∧ some (0, c_dst) = none ∧ n3.registers = ctx.registers) ∨
                              (∃ r wbVal wbTy,
                                some (0, oldVal) = some (r, wbVal) ∧
                                some (0, c_dst) = some (r, wbTy) ∧
                                r < GENERAL_REGISTER_COUNT ∧
                                n3.registers.val = ctx.registers.val.set r wbTy ∧
                                TypeRel2 env ρ resActive isNullId st₁.world.arrayLen wbTy (.scalar wbVal) (.scalar wbVal)) :=
                            Or.inr ⟨0, oldVal, c_dst, rfl, rfl, by decide, hregs_n3, hrel_cdst⟩
                          have hrel_post := StateRel2Deps_atomic_mem
                            hstk_n3 hbm_n3 hres_n3 hw'_eq rfl rfl rfl hsub_all hwb_match hrel
                          have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
                            simp only [bumpWorldStep]; omega
                          obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
                            hchildren n3 hmem3 l₁ l₂ s₁ s₂ ρ resActive isNullId
                              { pc := n3.pc.val, regs := applyAtomicWb st₁.regs (some (0, oldVal)), stack := st₁.stack, world := bumpWorldStep w₁' }
                              { pc := n3.pc.val, regs := applyAtomicWb st₂.regs (some (0, oldVal)), stack := st₂.stack, world := bumpWorldStep w₂' }
                              hl₁ hl₂ hleak halloc' hρ hrel_post
                          dsimp only [w] at hin_bnds
                          refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
                          · unfold runSteps step
                            rw [hv_base1] at hca_dst1
                            simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, hn_src_ge,
                              Bool.false_and, Bool.false_eq_true, beq_self_eq_true, ↓reduceIte,
                              hca_dst1, hv_base1, hw_orig, hmap_orig,
                              hin_bnds, hal_eff, Bool.not_true, hv_src1, hv_r0_1]
                            rw [stepNextIte_eq_next (by
                              dsimp only [w₁']
                              rw [hpc_n3, ← hwr_eq st₁.world.bufBytes (renameMemId ρ id) effOff]
                              dsimp only [effOff, oldVal, w, applyAtomicWb]
                              cases is64 <;> simp [readBufWord_u32_zext]) hr₁]
                            exact hr₁
                          · unfold runSteps step
                            rw [hv_base2] at hca_dst2
                            have hbuf_eq : st₂.world.bufBytes = st₁.world.bufBytes := by rw [hrel.2.2.1]
                            simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, hn_dst_ge, hn_src_ge,
                              Bool.false_and, Bool.false_eq_true, beq_self_eq_true, ↓reduceIte,
                              hca_dst2, hv_base2, hw_orig, hmap_orig,
                              hin_bnds, hal_eff, Bool.not_true, hv_src2, hv_r0_2]
                            rw [stepNextIte_eq_next (by
                              dsimp only [w₂']
                              rw [hpc_n3, hbuf_eq, ← hwr_eq st₁.world.bufBytes (renameMemId ρ id) effOff]
                              dsimp only [effOff, oldVal, w, applyAtomicWb]
                              cases is64 <;> simp [readBufWord_u32_zext]) hr₂]
                            exact hr₂

theorem visit_atomic_forward
    {env : ExecEnv} {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {is64 : Bool} {op : AtomicOpKind} {base src : Nat} {off16 : BitVec 16}
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.atomic is64 op base off16 src)) :
    ctx_out = ctx ∧ ForwardVisitPost env vctx0 vctx1 ctx_out := by
  rcases hfwd with ⟨dst8, src8, off, _, _, _, hcall⟩
  have hcall' := hcall.symm
  cases is64 <;> cases op <;> (
    dsimp [atomicMethod, CompCtxVisitorInst,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_add64,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_add,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_or64,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_or,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_and64,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_and,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xor64,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xor,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xchg64,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xchg,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_cmpxchg64,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_cmpxchg] at hcall'
    rcases bind_eq_ok hcall' with ⟨⟨r0, vc1⟩, hop, hcall'⟩
    rcases Prod.mk.inj (ok_inj hcall') with ⟨rfl, hp⟩
    rcases Prod.mk.inj hp with ⟨rfl, rfl⟩
    first
    | exact ⟨rfl, atomic_operation_ok_valid hctx hpending hop⟩
    | exact ⟨rfl, raw_atomic_cmpxchg_ok_valid hctx hpending hop⟩)

theorem visit_atomic_backward
    {prog : Program} {env : ExecEnv} {vCounter F : Nat}
    {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 vctx_back vctx_back1 : verifier.VerificationContext}
    {dPost : verifier.DataDependencies} {tracker : verifier.DependencyTracker}
    {is64 : Bool} {op : AtomicOpKind} {base src : Nat} {off16 : BitVec 16}
    (hpc : ∃ hlt : ctx.pc.val < prog.size,
      prog[ctx.pc.val]'hlt = .atomic is64 op base off16 src)
    (hctx : CompCtxValid env vCounter ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.atomic is64 op base off16 src))
    (hchildren : ∀ succ, some succ ∈ vctx1.pending_successors.val →
      SemTerminatesIn prog env vCounter succ dPost F)
    (hback : VisitorDispatchNonLddw DepTrackerVisitorInst
      { context := ctx_out, deps := dPost } vctx_back
      (.ok (.Ok (), tracker, vctx_back1)) (.atomic is64 op base off16 src)) :
    ctx_out = ctx ∧
    tracker.context = ctx ∧
    vctx_back1 = vctx_back ∧
    SemTerminatesIn prog env vCounter ctx tracker.deps (F + 1) := by
  rcases hfwd with ⟨dst8, src8, off, hbase, hoff, hsrc, hcall⟩
  rcases hback with ⟨dst8_b, src8_b, off_b, hbase_b, hoff_b, hsrc_b, hback_call⟩
  have hdst_u8 : dst8_b = dst8 := UScalar.eq_of_val_eq (hbase_b.symm.trans hbase)
  have hsrc_u8 : src8_b = src8 := UScalar.eq_of_val_eq (hsrc_b.symm.trans hsrc)
  have hoff_i16 : off_b = off := i16_eq_of_bv_eq (hoff_b.symm.trans hoff)
  subst dst8_b; subst src8_b; subst off_b
  subst base; subst off16; subst src
  obtain ⟨hlt_pc, hinsn_at⟩ := hpc
  have hcall' := hcall.symm
  have hback' := hback_call.symm
  cases is64 <;> cases op <;> (
    dsimp [atomicMethod, CompCtxVisitorInst,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_add64,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_add,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_or64,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_or,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_and64,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_and,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xor64,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xor,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xchg64,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xchg,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_cmpxchg64,
      verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_cmpxchg] at hcall'
    rcases bind_eq_ok hcall' with ⟨⟨r0, vc1⟩, hop, hcall'⟩
    rcases Prod.mk.inj (ok_inj hcall') with ⟨rfl, hp⟩
    rcases Prod.mk.inj hp with ⟨rfl, rfl⟩
    dsimp [atomicMethod, DepTrackerVisitorInst,
      verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_add64,
      verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_add,
      verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_or64,
      verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_or,
      verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_and64,
      verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_and,
      verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xor64,
      verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xor,
      verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xchg64,
      verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_xchg,
      verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_cmpxchg64,
      verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.atomic_cmpxchg] at hback'
    rcases bind_eq_ok hback' with ⟨⟨res_b, dd⟩, hdd, hback'⟩
    rcases Prod.mk.inj (ok_inj hback') with ⟨_, hp_b⟩
    rcases Prod.mk.inj hp_b with ⟨rfl, rfl⟩
    first
    | exact ⟨rfl, rfl, rfl,
        atomic_operation_backward hlt_pc hinsn_at rfl hctx hpending hop hchildren hdd⟩
    | exact ⟨rfl, rfl, rfl,
        raw_atomic_cmpxchg_backward hlt_pc hinsn_at hctx hpending hop hchildren hdd⟩)

/-! ## 4. Direct & Indirect Packet Loads (`visit_loadPacket_forward`, `visit_loadPacket_backward`) -/

private theorem u64Scalar_eq_ok_true {a b : U64ScalarValueData}
    (h : scalar_value.U64ScalarValueData.Insts.CoreCmpPartialEqU64ScalarValueData.eq a b = ok true) :
    a.value = b.value ∧ a.unknown_mask = b.unknown_mask := by
  unfold scalar_value.U64ScalarValueData.Insts.CoreCmpPartialEqU64ScalarValueData.eq at h
  by_cases hv : a.value = b.value
  · simp only [hv, ↓reduceIte] at h
    by_cases hu : a.unknown_mask = b.unknown_mask
    · exact ⟨hv, hu⟩
    · simp only [hu, ↓reduceIte] at h; cases ok_inj h
  · simp only [hv, ↓reduceIte] at h; cases ok_inj h

private theorem packet_type_eq_matchesVal
    {env : ExecEnv} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {arrLen : MemoryId → BitVec 64}
    {pktTy v : verifier.Type} {val₁ val₂ : RuntimeVal}
    (hpkt : PacketTypeConforms (some pktTy) env.packetCtx)
    (heq : verifier.Type.Insts.CoreCmpPartialEqType.eq v pktTy = ok true)
    (hrel : TypeRel2 env ρ resActive isNullId arrLen v val₁ val₂) :
    ∃ pktSpec,
      env.packetCtx = some pktSpec ∧
      pktSpec.matchesVal val₁ = true ∧
      pktSpec.matchesVal val₂ = true := by
  cases hctx_pkt : env.packetCtx with
  | none => rw [hctx_pkt] at hpkt; cases pktTy <;> exact False.elim hpkt
  | some pktSpec =>
    rw [hctx_pkt] at hpkt
    cases pktTy <;> cases pktSpec <;> try exact False.elim hpkt
    · -- PtrToMemory / memCtx
      rename_i id' off' sz' orig' id_s orig_s sz_s
      rcases hpkt with ⟨rfl, hid_nv, hoff_v, hoff_u, _, _, _, rfl, rfl⟩
      unfold verifier.Type.Insts.CoreCmpPartialEqType.eq at heq
      cases v <;> try (cases ok_inj heq)
      rename_i id1 o1 b1 org1
      dsimp only at heq
      rw [bind_ok_eq (MemoryId_eq_spec id1 id')] at heq
      by_cases hid_eq : id1 = id'
      · subst hid_eq
        simp only at heq
        rcases bind_eq_ok heq with ⟨b_sc, hsc_eq, heq⟩
        cases b_sc with
        | false => cases ok_inj heq
        | true =>
          simp only [↓reduceIte] at heq
          obtain ⟨hval_eq, hunk_eq⟩ := u64Scalar_eq_ok_true hsc_eq
          by_cases hb_eq : b1 = sz'
          · subst hb_eq
            simp only [↓reduceIte] at heq
            obtain ⟨r_po, hpo, hpo_iff⟩ := PointerOrigin_eq_spec org1 orig'
            rw [hpo] at heq
            have horg_eq : org1 = orig' := hpo_iff.mp (ok_inj heq)
            subst horg_eq
            rcases hrel with ⟨_, o, hconc_o, rfl, rfl⟩
            have ho1_kn : o1.unknown_mask = 0#u64 :=
              (u64_eq_iff_bv _ _).mpr (by rw [hunk_eq, hoff_u]; rfl)
            have ho_zero : o = 0#64 := by
              rw [eq_value_of_known_64 ho1_kn hconc_o.1, hval_eq, hoff_v]
            subst ho_zero
            rw [renameMemId_noVerif_id ρ hid_nv]
            exact ⟨_, rfl, by simp [ContextObjSpec.matchesVal], by simp [ContextObjSpec.matchesVal]⟩
          · simp only [hb_eq, ↓reduceIte] at heq; cases ok_inj heq
      · simp only [hid_eq, decide_false, Bool.false_eq_true, ↓reduceIte] at heq; cases ok_inj heq
    · -- PtrToStruct / structCtx
      rename_i id' off' sd' id_s
      rcases hpkt with ⟨rfl, hid_nv, hoff_v, hoff_u, _, _, _⟩
      unfold verifier.Type.Insts.CoreCmpPartialEqType.eq at heq
      cases v <;> try (cases ok_inj heq)
      rename_i id1 o1 d1
      dsimp only at heq
      rw [bind_ok_eq (MemoryId_eq_spec id1 id')] at heq
      by_cases hid_eq : id1 = id'
      · subst hid_eq
        simp only at heq
        rcases bind_eq_ok heq with ⟨b_sc, hsc_eq, heq⟩
        cases b_sc with
        | false => cases ok_inj heq
        | true =>
          obtain ⟨hval_eq, hunk_eq⟩ := u64Scalar_eq_ok_true hsc_eq
          rcases hrel with ⟨_, _, o, hconc_o, rfl, rfl⟩
          have ho1_kn : o1.unknown_mask = 0#u64 :=
            (u64_eq_iff_bv _ _).mpr (by rw [hunk_eq, hoff_u]; rfl)
          have ho_zero : o = 0#64 := by
            rw [eq_value_of_known_64 ho1_kn hconc_o.1, hval_eq, hoff_v]
          subst ho_zero
          rw [renameMemId_noVerif_id ρ hid_nv]
          exact ⟨_, rfl, by simp [ContextObjSpec.matchesVal], by simp [ContextObjSpec.matchesVal]⟩
      · simp only [hid_eq, decide_false, Bool.false_eq_true, ↓reduceIte] at heq; cases ok_inj heq

theorem visit_loadPacket_forward
    {env : ExecEnv} {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 : verifier.VerificationContext}
    {w : DataWidth} {imm32 : BitVec 32} {indReg? : Option Nat}
    (hctx : CompCtxValid env vctx0.counter.bv.toNat ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.loadPacket w imm32 indReg?)) :
    ctx_out = ctx ∧ ForwardVisitPost env vctx0 vctx1 ctx_out := by
  rcases hfwd with ⟨imm, _, hcases⟩
  have runCommon :
      ∀ (reg_off : Option Std.U8),
        CompCtxVisitorInst.load_from_packet ctx vctx0 0#u8 6#u8 imm reg_off (coreWidthOfSpec w) =
          ok (.Ok (), ctx_out, vctx1) →
        ctx_out = ctx ∧ ForwardVisitPost env vctx0 vctx1 ctx_out := by
    intro reg_off hcall
    dsimp [CompCtxVisitorInst] at hcall
    unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.load_from_packet at hcall
    rcases bind_eq_ok hcall with ⟨vc1, hemit, hcall⟩
    obtain ⟨he1, he2, he3, he4, he5, he6⟩ := VerificationContext_emit_log_ok_spec hemit
    rcases bind_eq_ok hcall with ⟨r, _, hcall⟩
    cases r with
    | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
    | Ok v =>
      dsimp only at hcall
      rcases bind_eq_ok hcall with ⟨pair_pkt, hpkt_br, _⟩
      rcases pair_pkt with ⟨o, src_is_pkt⟩
      rw [bind_pair_ok_eq hpkt_br] at hcall
      cases src_is_pkt with
      | false => cases (Prod.mk.inj (ok_inj hcall)).1
      | true =>
        simp only [↓reduceIte] at hcall
        have ho_eq : o = vc1.calling_context.packet_type := by
          cases hp0 : vc1.calling_context.packet_type <;> rw [hp0] at hpkt_br
          · cases (Prod.mk.inj (ok_inj hpkt_br)).2
          · rcases bind_eq_ok hpkt_br with ⟨_, _, hpkt_br⟩
            exact (Prod.mk.inj (ok_inj hpkt_br)).1.symm
        subst ho_eq
        cases reg_off with
        | none =>
          dsimp only at hcall
          rw [bind_ok_eq (IdSet_len_spec ctx.resources)] at hcall
          cases hnz : (alloc.vec.Vec.len ctx.resources.entries != 0#usize) <;>
            simp only [hnz, Bool.false_eq_true, ↓reduceIte] at hcall
          · rcases bind_eq_ok hcall with ⟨r1, hnext, hcall⟩
            cases r1 with
            | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
            | Ok n =>
              dsimp only at hcall
              obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext
              have hn_valid : CompCtxValid env vctx0.counter.bv.toNat n := by rw [hn_eq]; exact hctx
              rw [bind_ok_eq Type_UNKNOWN_SCALAR_eq] at hcall
              rcases bind_eq_ok hcall with ⟨⟨r2, n1⟩, hset, hcall⟩
              cases r2 with
              | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
              | Ok _ =>
                obtain ⟨_, _, _, _, _, _, hn1_valid⟩ :=
                  ComputationContext_set_reg_ok_spec hn_valid
                    (show TypeValid env vctx0.counter.bv.toNat (.ScalarValue unknownWrittenScalar64)
                      from unknownWrittenScalar64_valid)
                    hset
                obtain ⟨a9, rfl, hsucc_valid, _, hc1, hc2, hc3, hc4, hc5, hc7⟩ :=
                  clobber_scratch_tail_spec hn1_valid hcall
                have hcnt : vctx1.counter = vctx0.counter := hc3.trans he3
                refine ⟨rfl, hc1.trans he1, hc2.trans he2, hc4.trans he4, hc5.trans he5,
                  by rw [hcnt], by rwa [hcnt], ?_⟩
                intro opt hopt
                rw [hc7, he6, hpending, List.nil_append, List.mem_singleton] at hopt
                subst hopt
                exact ⟨_, rfl, by rwa [hcnt]⟩
          · cases (Prod.mk.inj (ok_inj hcall)).1
        | some reg =>
          dsimp only at hcall
          rcases bind_eq_ok hcall with ⟨r1, _, hcall⟩
          cases r1 with
          | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
          | Ok v1 =>
            dsimp only at hcall
            rcases bind_eq_ok hcall with ⟨b, _, hcall⟩
            cases b with
            | false => cases (Prod.mk.inj (ok_inj hcall)).1
            | true =>
              simp only [↓reduceIte, IdSet_len_spec, bind_ok] at hcall
              cases hnz : (alloc.vec.Vec.len ctx.resources.entries != 0#usize) <;>
                simp only [hnz, Bool.false_eq_true, ↓reduceIte] at hcall
              · rcases bind_eq_ok hcall with ⟨r2, hnext, hcall⟩
                cases r2 with
                | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
                | Ok n =>
                  dsimp only at hcall
                  obtain ⟨_, hn_eq⟩ := ComputationContext_next_ok_spec hnext
                  have hn_valid : CompCtxValid env vctx0.counter.bv.toNat n := by rw [hn_eq]; exact hctx
                  rw [bind_ok_eq Type_UNKNOWN_SCALAR_eq] at hcall
                  rcases bind_eq_ok hcall with ⟨⟨r3, n1⟩, hset, hcall⟩
                  cases r3 with
                  | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
                  | Ok _ =>
                    obtain ⟨_, _, _, _, _, _, hn1_valid⟩ :=
                      ComputationContext_set_reg_ok_spec hn_valid
                        (show TypeValid env vctx0.counter.bv.toNat (.ScalarValue unknownWrittenScalar64)
                          from unknownWrittenScalar64_valid)
                        hset
                    obtain ⟨a9, rfl, hsucc_valid, _, hc1, hc2, hc3, hc4, hc5, hc7⟩ :=
                      clobber_scratch_tail_spec hn1_valid hcall
                    have hcnt : vctx1.counter = vctx0.counter := hc3.trans he3
                    refine ⟨rfl, hc1.trans he1, hc2.trans he2, hc4.trans he4, hc5.trans he5,
                      by rw [hcnt], by rwa [hcnt], ?_⟩
                    intro opt hopt
                    rw [hc7, he6, hpending, List.nil_append, List.mem_singleton] at hopt
                    subst hopt
                    exact ⟨_, rfl, by rwa [hcnt]⟩
              · cases (Prod.mk.inj (ok_inj hcall)).1
  rcases hcases with ⟨_, hcall⟩ | ⟨src8, _, hcall⟩
  · exact runCommon none hcall.symm
  · exact runCommon (some src8) hcall.symm

private theorem depTracker_loadPacket_spec
    {ctx : verifier.ComputationContext} {dPost : verifier.DataDependencies}
    {vctx vctx' : verifier.VerificationContext}
    {imm : Std.I32} {reg_off : Option Std.U8} {w : api.DataWidth}
    {res : core.result.Result Unit error.VerifierErrorKind}
    {tracker : verifier.DependencyTracker}
    (h : DepTrackerVisitorInst.load_from_packet { context := ctx, deps := dPost } vctx
      0#u8 6#u8 imm reg_off w = ok (res, tracker, vctx')) :
    tracker.context = ctx ∧
    vctx' = vctx ∧
    DataDepsHasReg tracker.deps 6 = true ∧
    (∀ reg, reg_off = some reg → DataDepsHasReg tracker.deps reg.val = true) ∧
    (∀ r < 10, r ≠ 0 → ¬ (1 ≤ r ∧ r ≤ 5) →
      DataDepsHasReg dPost r = true → DataDepsHasReg tracker.deps r = true) ∧
    (∀ s, DataDepsHasStack tracker.deps s = DataDepsHasStack dPost s) := by
  dsimp [DepTrackerVisitorInst] at h
  unfold verifier.DependencyTracker.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.load_from_packet at h
  simp only [lift_eq_ok, bind_ok] at h
  rcases bind_eq_ok h with ⟨⟨_, dd⟩, hrem0, h⟩
  obtain ⟨_, _, _, hrem_r, hrem_s⟩ := DataDependencies_remove_reg_ok_spec hrem0
  rcases bind_eq_ok h with ⟨dd1, hadd6, h⟩
  obtain ⟨h6_lt16, _, hadd6_r, hadd6_s, _⟩ := DataDependencies_add_reg_ok_spec hadd6
  have h0_val : (0#u8).val = 0 := rfl
  have h6_val : (6#u8).val = 6 := rfl
  have hpreserve_6_to_9 : ∀ r < 10, r ≠ 0 → ¬ (1 ≤ r ∧ r ≤ 5) →
      DataDepsHasReg dPost r = true → DataDepsHasReg dd1 r = true := by
    intro r hr hne0 hnot15 hd
    rw [hadd6_r r (by omega), hrem_r r (by omega), h0_val]
    have hmask_true : DataDepsHasReg { dPost with registers := dPost.registers &&& ~~~62#u16 } r = true := by
      rcases (by omega : r = 6 ∨ r = 7 ∨ r = 8 ∨ r = 9) with rfl | rfl | rfl | rfl <;> {
        change (dPost.registers.bv &&& (~~~(62#16))).getLsbD _ = true
        rw [BitVec.getLsbD_and]
        exact Bool.and_eq_true_iff.mpr ⟨hd, by decide⟩
      }
    rw [hmask_true]
    simp [hne0]
  have h6_dd1 : DataDepsHasReg dd1 6 = true := by
    rw [← h6_val, hadd6_r (6#u8).val h6_lt16]; simp
  cases reg_off with
  | none =>
    rcases Prod.mk.inj (Prod.mk.inj (ok_inj h)).2 with ⟨rfl, rfl⟩
    exact ⟨rfl, rfl, h6_dd1, (fun _ hc => by cases hc), hpreserve_6_to_9,
      fun s => (hadd6_s s).trans (hrem_s s)⟩
  | some reg =>
    dsimp only at h
    rcases bind_eq_ok h with ⟨dd2, hadd_reg, h⟩
    rcases Prod.mk.inj (Prod.mk.inj (ok_inj h)).2 with ⟨rfl, rfl⟩
    obtain ⟨hr_lt16, _, hadd_r, hadd_s, _⟩ := DataDependencies_add_reg_ok_spec hadd_reg
    refine ⟨rfl, rfl, ?_, ?_, ?_, fun s => (hadd_s s).trans ((hadd6_s s).trans (hrem_s s))⟩
    · rw [hadd_r 6 (by omega), h6_dd1, Bool.true_or]
    · intro r0 hr0; cases Option.some.inj hr0
      rw [hadd_r reg.val hr_lt16]; simp
    · intro r hr hne0 hnot15 hd
      rw [hadd_r r (by omega), hpreserve_6_to_9 r hr hne0 hnot15 hd, Bool.true_or]

theorem visit_loadPacket_backward
    {prog : Program} {env : ExecEnv} {vCounter F : Nat}
    {ctx ctx_out : verifier.ComputationContext}
    {vctx0 vctx1 vctx_back vctx_back1 : verifier.VerificationContext}
    {dPost : verifier.DataDependencies} {tracker : verifier.DependencyTracker}
    {w : DataWidth} {imm32 : BitVec 32} {indReg? : Option Nat}
    (hcctx : CallingContextEnvMatches vctx0.calling_context env)
    (hpc : ∃ hlt : ctx.pc.val < prog.size,
      prog[ctx.pc.val]'hlt = .loadPacket w imm32 indReg?)
    (hctx : CompCtxValid env vCounter ctx)
    (hpending : vctx0.pending_successors.val = [])
    (hfwd : VisitorDispatchNonLddw CompCtxVisitorInst ctx vctx0
      (.ok (.Ok (), ctx_out, vctx1)) (.loadPacket w imm32 indReg?))
    (hchildren : ∀ succ, some succ ∈ vctx1.pending_successors.val →
      SemTerminatesIn prog env vCounter succ dPost F)
    (hback : VisitorDispatchNonLddw DepTrackerVisitorInst
      { context := ctx_out, deps := dPost } vctx_back
      (.ok (.Ok (), tracker, vctx_back1)) (.loadPacket w imm32 indReg?)) :
    ctx_out = ctx ∧
    tracker.context = ctx ∧
    vctx_back1 = vctx_back ∧
    SemTerminatesIn prog env vCounter ctx tracker.deps (F + 1) := by
  rcases hfwd with ⟨imm, himm, hfwd_cases⟩
  rcases hback with ⟨imm_b, himm_b, hback_cases⟩
  have himm_eq : imm_b = imm := i32_eq_of_bv_eq (himm_b.symm.trans himm)
  subst imm_b; subst imm32
  obtain ⟨hlt_pc, hinsn_at⟩ := hpc
  -- Unify `reg_off : Option Std.U8` across `hfwd_cases` and `hback_cases`:
  have hreg_off_ex :
      ∃ reg_off : Option Std.U8,
        indReg? = reg_off.map UScalar.val ∧
        CompCtxVisitorInst.load_from_packet ctx vctx0 0#u8 6#u8 imm reg_off (coreWidthOfSpec w) =
          ok (.Ok (), ctx_out, vctx1) ∧
        DepTrackerVisitorInst.load_from_packet { context := ctx_out, deps := dPost } vctx_back
          0#u8 6#u8 imm reg_off (coreWidthOfSpec w) = ok (.Ok (), tracker, vctx_back1) := by
    rcases hfwd_cases with ⟨hind, hcall⟩ | ⟨src8, hind, hcall⟩
    · rcases hback_cases with ⟨_, hback_call⟩ | ⟨src8_b, hind_b, _⟩
      · exact ⟨none, hind, hcall.symm, hback_call.symm⟩
      · rw [hind] at hind_b; cases hind_b
    · rcases hback_cases with ⟨hind_b, _⟩ | ⟨src8_b, hind_b, hback_call⟩
      · rw [hind] at hind_b; cases hind_b
      · have hsrc_eq : src8_b = src8 := UScalar.eq_of_val_eq (Option.some.inj (hind_b.symm.trans hind))
        subst src8_b
        exact ⟨some src8, hind, hcall.symm, hback_call.symm⟩
  rcases hreg_off_ex with ⟨reg_off, hind_eq, hcall, hback_call⟩
  subst indReg?
  -- Unpack forward `load_from_packet`:
  dsimp [CompCtxVisitorInst] at hcall
  unfold verifier.ComputationContext.Insts.Ebpf_coreVisitorBpfVisitorVerificationContext.load_from_packet at hcall
  rcases bind_eq_ok hcall with ⟨vc1, hemit, hcall⟩
  obtain ⟨he1, _, _, _, _, he6⟩ := VerificationContext_emit_log_ok_spec hemit
  rcases bind_eq_ok hcall with ⟨r_r6, hreg6, hcall⟩
  cases r_r6 with
  | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
  | Ok v_r6 =>
    dsimp only at hcall
    rcases bind_eq_ok hcall with ⟨pair_pkt, hpkt_br, _⟩
    rcases pair_pkt with ⟨o, src_is_pkt⟩
    rw [bind_pair_ok_eq hpkt_br] at hcall
    cases src_is_pkt with
    | false => cases (Prod.mk.inj (ok_inj hcall)).1
    | true =>
      simp only [↓reduceIte] at hcall
      have hpkt_ty_ex :
          ∃ pktTy, vctx0.calling_context.packet_type = some pktTy ∧
            o = some pktTy ∧
            verifier.Type.Insts.CoreCmpPartialEqType.eq v_r6 pktTy = ok true := by
        rw [he1] at hpkt_br
        cases hp0 : vctx0.calling_context.packet_type <;> rw [hp0] at hpkt_br
        · cases (Prod.mk.inj (ok_inj hpkt_br)).2
        · rename_i pktTy
          dsimp only at hpkt_br
          rcases bind_eq_ok hpkt_br with ⟨b_eq, h_eq, hpkt_br⟩
          rcases Prod.mk.inj (ok_inj hpkt_br) with ⟨ho, hb⟩
          subst hb
          exact ⟨pktTy, rfl, ho.symm, h_eq⟩
      rcases hpkt_ty_ex with ⟨pktTy, hpkt_cctx, ho_eq, htype_eq⟩
      -- Extract uniform post-check facts across `reg_off = none` and `reg_off = some reg`:
      have hpost_ex :
          ctx_out = ctx ∧
          ctx.resources.entries.val = [] ∧
          (∀ reg, reg_off = some reg →
            ∃ v_ind d_ind,
              reg.val < REGISTER_COUNT ∧
              verifier.ComputationContext.reg ctx reg = ok (.Ok v_ind) ∧
              v_ind = .ScalarValue d_ind ∧
              d_ind.unwritten_mask = 0#u64) ∧
          ∃ (n n1 : verifier.ComputationContext) (a9 : Std.Array verifier.Type 10#usize),
            n.pc.val = ctx.pc.val + 1 ∧
            n = { ctx with pc := n.pc, path_depth := n.path_depth, jmp_seq := n.jmp_seq } ∧
            n1.pc = n.pc ∧
            n1.registers.val = ctx.registers.val.set 0 (.ScalarValue unknownWrittenScalar64) ∧
            n1.stack = n.stack ∧
            n1.array_bounds = n.array_bounds ∧
            n1.resources = n.resources ∧
            (∀ (r : Nat) (hr : r < 10), r < a9.val.length →
              if 1 ≤ r ∧ r ≤ 5 then
                (a9.val[r]'(by rw [array10_len]; exact hr)) = .ScalarValue uninitScalar64
              else
                (a9.val[r]'(by rw [array10_len]; exact hr)) =
                  (n1.registers.val[r]'(by rw [array10_len]; exact hr))) ∧
            some { n1 with registers := a9 } ∈ vctx1.pending_successors.val := by
        have ho_vc1 : o = vc1.calling_context.packet_type := by rw [he1, hpkt_cctx, ho_eq]
        subst ho_vc1
        cases reg_off with
        | none =>
          dsimp only at hcall
          rw [bind_ok_eq (IdSet_len_spec ctx.resources)] at hcall
          cases hnz : (alloc.vec.Vec.len ctx.resources.entries != 0#usize) <;>
            simp only [hnz, Bool.false_eq_true, ↓reduceIte] at hcall
          · have hres_nil : ctx.resources.entries.val = [] := by
              have hlen0 : alloc.vec.Vec.len ctx.resources.entries = 0#usize := bne_eq_false_iff_eq.mp hnz
              have hv0 := congrArg UScalar.val hlen0
              rw [usize_const_val_0] at hv0
              exact List.eq_nil_of_length_eq_zero hv0
            rcases bind_eq_ok hcall with ⟨r1, hnext, hcall⟩
            cases r1 with
            | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
            | Ok n =>
              dsimp only at hcall
              obtain ⟨hpc_next, hn_eq⟩ := ComputationContext_next_ok_spec hnext
              have hn_valid : CompCtxValid env vCounter n := by rw [hn_eq]; exact hctx
              rw [bind_ok_eq Type_UNKNOWN_SCALAR_eq] at hcall
              rcases bind_eq_ok hcall with ⟨⟨r2, n1⟩, hset, hcall⟩
              cases r2 with
              | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
              | Ok _ =>
                obtain ⟨_, hpc_n1, hregs_n1, hstk_n1, hbm_n1, hres_n1, hn1_valid⟩ :=
                  ComputationContext_set_reg_ok_spec hn_valid
                    (show TypeValid env vCounter (.ScalarValue unknownWrittenScalar64)
                      from unknownWrittenScalar64_valid)
                    hset
                obtain ⟨a9, rfl, _, ha9_get, _, _, _, _, _, hc7⟩ :=
                  clobber_scratch_tail_spec hn1_valid hcall
                have h0_val : (0#u8).val = 0 := rfl
                rw [h0_val, hn_eq] at hregs_n1
                refine ⟨rfl, hres_nil, (fun _ hc => by cases hc),
                  n, n1, a9, hpc_next, hn_eq, hpc_n1, hregs_n1, hstk_n1, hbm_n1, hres_n1, ha9_get, ?_⟩
                rw [hc7, he6, hpending]; simp
          · cases (Prod.mk.inj (ok_inj hcall)).1
        | some reg =>
          dsimp only at hcall
          rcases bind_eq_ok hcall with ⟨r1, hreg_ind, hcall⟩
          cases r1 with
          | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
          | Ok v_ind =>
            dsimp only at hcall
            obtain ⟨hreg_lt, _⟩ := ComputationContext_reg_ok_valid hctx hreg_ind
            rcases bind_eq_ok hcall with ⟨b, hws_ind, hcall⟩
            cases b with
            | false => cases (Prod.mk.inj (ok_inj hcall)).1
            | true =>
              obtain ⟨d_ind, hv_ind_eq, hd_ind_unw⟩ := Type_is_written_scalar_ok_true hws_ind
              simp only [↓reduceIte, IdSet_len_spec, bind_ok] at hcall
              cases hnz : (alloc.vec.Vec.len ctx.resources.entries != 0#usize) <;>
                simp only [hnz, Bool.false_eq_true, ↓reduceIte] at hcall
              · have hres_nil : ctx.resources.entries.val = [] := by
                  have hlen0 : alloc.vec.Vec.len ctx.resources.entries = 0#usize := bne_eq_false_iff_eq.mp hnz
                  have hv0 := congrArg UScalar.val hlen0
                  rw [usize_const_val_0] at hv0
                  exact List.eq_nil_of_length_eq_zero hv0
                rcases bind_eq_ok hcall with ⟨r2, hnext, hcall⟩
                cases r2 with
                | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
                | Ok n =>
                  dsimp only at hcall
                  obtain ⟨hpc_next, hn_eq⟩ := ComputationContext_next_ok_spec hnext
                  have hn_valid : CompCtxValid env vCounter n := by rw [hn_eq]; exact hctx
                  rw [bind_ok_eq Type_UNKNOWN_SCALAR_eq] at hcall
                  rcases bind_eq_ok hcall with ⟨⟨r3, n1⟩, hset, hcall⟩
                  cases r3 with
                  | Err _ => cases (Prod.mk.inj (ok_inj hcall)).1
                  | Ok _ =>
                    obtain ⟨_, hpc_n1, hregs_n1, hstk_n1, hbm_n1, hres_n1, hn1_valid⟩ :=
                      ComputationContext_set_reg_ok_spec hn_valid
                        (show TypeValid env vCounter (.ScalarValue unknownWrittenScalar64)
                          from unknownWrittenScalar64_valid)
                        hset
                    obtain ⟨a9, rfl, _, ha9_get, _, _, _, _, _, hc7⟩ :=
                      clobber_scratch_tail_spec hn1_valid hcall
                    have h0_val : (0#u8).val = 0 := rfl
                    rw [h0_val, hn_eq] at hregs_n1
                    refine ⟨rfl, hres_nil, fun r0 hr0 => ?_,
                      n, n1, a9, hpc_next, hn_eq, hpc_n1, hregs_n1, hstk_n1, hbm_n1, hres_n1, ha9_get, ?_⟩
                    · cases Option.some.inj hr0
                      exact ⟨v_ind, d_ind, hreg_lt, hreg_ind, hv_ind_eq, hd_ind_unw⟩
                    · rw [hc7, he6, hpending]; simp
              · cases (Prod.mk.inj (ok_inj hcall)).1
      rcases hpost_ex with ⟨hctx_out_eq, hres_nil, hind_info,
        n, n1, a9, hpc_next, hn_eq, hpc_n1, hregs_n1, hstk_n1, hbm_n1, hres_n1, ha9_get, hmem_succ⟩
      subst ctx_out
      have hres_n : n.resources = ctx.resources := by rw [hn_eq]
      have hbm_n : n.array_bounds = ctx.array_bounds := by rw [hn_eq]
      have hstk_n : n.stack = ctx.stack := by rw [hn_eq]
      obtain ⟨htrk_c, hv_b, hr6_live, hind_live, hregs_6_9, hstk_deps⟩ :=
        depTracker_loadPacket_spec hback_call
      refine ⟨rfl, htrk_c, hv_b, ?_⟩
      intro l₁ l₂ s₁ s₂ ρ resActive isNullId st₁ st₂ hl₁ hl₂ hleak halloc hρ hrel
      -- Packet context matches `st₁.regs 6` and `st₂.regs 6`:
      have h6_val : (6#u8).val = 6 := rfl
      obtain ⟨_, _, hrel_r6⟩ :=
        ComputationContext_reg_ok_valid_and_rel2 hctx hrel (Or.inr (by rwa [h6_val])) hreg6
      rw [h6_val] at hrel_r6
      have hpkt_conf : PacketTypeConforms (some pktTy) env.packetCtx := by
        have h := hcctx.2.1; rwa [hpkt_cctx] at h
      obtain ⟨pktSpec, henv_pkt, hmatch_r6_1, hmatch_r6_2⟩ :=
        packet_type_eq_matchesVal hpkt_conf htype_eq hrel_r6
      -- Live resources are empty in both `st₁` and `st₂`:
      have hlive_empty1 : st₁.world.liveResources = [] := by
        apply List.eq_nil_iff_forall_not_mem.mpr
        intro m hm
        rcases (hrel.2.2.2.2.2.2.1 m).mp hm with ⟨r_m, hr_mem, _⟩
        rw [hres_nil] at hr_mem; cases hr_mem
      have hlive_empty2 : st₂.world.liveResources = [] := by
        rw [← hrel.2.2.1]; exact hlive_empty1
      -- Evaluate optional index register in both `st₁` and `st₂`:
      have hind_cases :
          ∃ indVal32 : BitVec 32,
            (reg_off = none ∧ indVal32 = 0#32) ∨
            (∃ (reg : Std.U8) (iv : BitVec 64),
              reg_off = some reg ∧
              ¬ (reg.val ≥ REGISTER_COUNT) ∧
              st₁.regs reg.val = .scalar iv ∧
              st₂.regs reg.val = .scalar iv ∧
              indVal32 = iv.setWidth 32) := by
        cases reg_off with
        | none => exact ⟨0#32, Or.inl ⟨rfl, rfl⟩⟩
        | some reg =>
          obtain ⟨v_ind, d_ind, hreg_lt, hreg_ind, hv_ind_eq, hd_ind_unw⟩ := hind_info reg rfl
          obtain ⟨_, _, hrel_ind⟩ :=
            ComputationContext_reg_ok_valid_and_rel2 hctx hrel
              (Or.inr (hind_live reg rfl)) hreg_ind
          rw [hv_ind_eq] at hrel_ind
          have hd_ne : d_ind ≠ uninitScalar64 := fun hc => by
            subst hc; exact uninitScalar64_unwritten_ne_zero hd_ind_unw
          simp only [TypeRel2, hd_ne, ↓reduceIte] at hrel_ind
          rcases hrel_ind with ⟨iv₁, iv₂, hiv1, hiv2, hconc_ind⟩
          have hiv_eq := fully_initialized_eq_of_rel2_64 hd_ind_unw hconc_ind
          subst hiv_eq
          exact ⟨iv₁.setWidth 32, Or.inr ⟨reg, iv₁, rfl, by omega, hiv1, hiv2, rfl⟩⟩
      rcases hind_cases with ⟨indVal32, hind_disj⟩
      have hw_eq : st₂.world = st₁.world := hrel.2.2.1.symm
      -- Branch on overflow and packetOracle:
      by_cases hovf : BitVec.saddOverflow indVal32 imm.bv = true
      · refine ⟨0#64, bumpWorldStep st₁.world, ?_, ?_, by simp [bumpWorldStep, hlive_empty1]⟩
        · unfold runSteps step
          rcases hind_disj with ⟨rfl, rfl⟩ | ⟨reg, iv, rfl, hn_ge, hiv1, _, rfl⟩
          · simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, henv_pkt,
              hmatch_r6_1, Bool.not_true, Bool.false_eq_true, ↓reduceIte,
              Option.map_none, hovf, hlive_empty1, List.isEmpty_nil]
          · simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, henv_pkt,
              hmatch_r6_1, Bool.not_true, Bool.false_eq_true, ↓reduceIte,
              Option.map_some, hn_ge, hiv1, hovf, hlive_empty1, List.isEmpty_nil]
        · unfold runSteps step
          rcases hind_disj with ⟨rfl, rfl⟩ | ⟨reg, iv, rfl, hn_ge, _, hiv2, rfl⟩
          · simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, henv_pkt,
              hmatch_r6_2, Bool.not_true, Bool.false_eq_true, ↓reduceIte,
              Option.map_none, hovf, hw_eq, hlive_empty1, List.isEmpty_nil]
          · simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, henv_pkt,
              hmatch_r6_2, Bool.not_true, Bool.false_eq_true, ↓reduceIte,
              Option.map_some, hn_ge, hiv2, hovf, hw_eq, hlive_empty1, List.isEmpty_nil]
      · have hovf_f : BitVec.saddOverflow indVal32 imm.bv = false := Bool.eq_false_iff.mpr hovf
        cases horacle : env.packetOracle st₁.world.stepCount (indVal32 + imm.bv) w with
        | none =>
          refine ⟨0#64, bumpWorldStep st₁.world, ?_, ?_, by simp [bumpWorldStep, hlive_empty1]⟩
          · unfold runSteps step
            rcases hind_disj with ⟨rfl, rfl⟩ | ⟨reg, iv, rfl, hn_ge, hiv1, _, rfl⟩
            · simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, henv_pkt,
                hmatch_r6_1, Bool.not_true, Bool.false_eq_true, ↓reduceIte,
                Option.map_none, hovf_f, horacle, hlive_empty1, List.isEmpty_nil]
            · simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, henv_pkt,
                hmatch_r6_1, Bool.not_true, Bool.false_eq_true, ↓reduceIte,
                Option.map_some, hn_ge, hiv1, hovf_f, horacle, hlive_empty1, List.isEmpty_nil]
          · unfold runSteps step
            rcases hind_disj with ⟨rfl, rfl⟩ | ⟨reg, iv, rfl, hn_ge, _, hiv2, rfl⟩
            · simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, henv_pkt,
                hmatch_r6_2, Bool.not_true, Bool.false_eq_true, ↓reduceIte,
                Option.map_none, hovf_f, hw_eq, horacle, hlive_empty1, List.isEmpty_nil]
            · simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, henv_pkt,
                hmatch_r6_2, Bool.not_true, Bool.false_eq_true, ↓reduceIte,
                Option.map_some, hn_ge, hiv2, hovf_f, hw_eq, horacle, hlive_empty1, List.isEmpty_nil]
        | some loadedVal =>
          let clobber (l : HostLayout) (sec : SecretInit) (st : MachineState) (r : Nat) : RuntimeVal :=
            if r == 0 then .scalar (loadedVal &&& w.valMask64)
            else if 1 ≤ r && r ≤ 5 then
              if sec.preserveCallScratch then
                .scalar ((st.regs r).toBits l st.world.arrayLen)
              else
                .scalar (sec.regBits st.world.stepCount r)
            else st.regs r
          let succ : verifier.ComputationContext := { n1 with registers := a9 }
          have hsucc_pc : succ.pc.val = ctx.pc.val + 1 := by
            dsimp [succ]; rw [hpc_n1, hpc_next]
          let st₁' : MachineState :=
            { pc := succ.pc.val, regs := clobber l₁ s₁ st₁, stack := st₁.stack,
              world := bumpWorldStep st₁.world }
          let st₂' : MachineState :=
            { pc := succ.pc.val, regs := clobber l₂ s₂ st₂, stack := st₂.stack,
              world := bumpWorldStep st₂.world }
          have hrel_post : StateRel2Deps env succ dPost ρ resActive isNullId st₁' st₂' := by
            rcases hrel with ⟨_, _, hw, harr, hnull, hnodup, hlive, hbnds, hr10_1, hr10_2, hregs, hstk⟩
            refine ⟨rfl, rfl, by simp [st₁', st₂', bumpWorldStep, hw],
              by simpa [st₁', bumpWorldStep] using harr,
              by simpa [st₁', bumpWorldStep] using hnull,
              by simpa [st₁', bumpWorldStep] using hnodup,
              by simpa [st₁', succ, hres_n1, hres_n, bumpWorldStep] using hlive,
              by simpa [st₁', succ, hbm_n1, hbm_n, bumpWorldStep] using hbnds,
              by simpa [st₁', clobber, GENERAL_REGISTER_COUNT] using hr10_1,
              by simpa [st₂', clobber, GENERAL_REGISTER_COUNT] using hr10_2,
              ?_, ?_⟩
            · intro r hr hlen hd
              have ha9 := ha9_get r hr hlen
              by_cases h15 : 1 ≤ r ∧ r ≤ 5
              · simp only [h15, and_self, ↓reduceIte] at ha9
                change TypeRel2 env ρ resActive isNullId _ (a9.val[r]'hlen) _ _
                rw [ha9]
                simp [TypeRel2]
              · simp only [h15, ↓reduceIte] at ha9
                change TypeRel2 env ρ resActive isNullId _ (a9.val[r]'hlen) _ _
                rw [ha9]
                by_cases hr0 : r = 0
                · subst hr0
                  have hn1_0 : (n1.registers.val[0]'(by rw [array10_len]; omega)) =
                      .ScalarValue unknownWrittenScalar64 := by
                    simp [hregs_n1]
                  rw [hn1_0]
                  dsimp only [st₁', st₂', clobber, bumpWorldStep]
                  simp only [beq_self_eq_true, ↓reduceIte]
                  exact TypeRel2_scalar_rel2_of_concretizes (concretizesScalar64Rel2_unknownWritten _)
                · have hlen_ctx : r < ctx.registers.val.length := by rw [array10_len]; exact hr
                  have hn1_r : (n1.registers.val[r]'(by rw [array10_len]; exact hr)) =
                      (ctx.registers.val[r]'hlen_ctx) := by
                    simp only [hregs_n1, List.getElem_set_ne (Ne.symm hr0)]
                  rw [hn1_r]
                  have hb0 : (r == 0) = false := beq_false_of_ne hr0
                  have hb15 : (decide (1 ≤ r) && decide (r ≤ 5)) = false := by
                    simp only [Bool.and_eq_false_iff, decide_eq_false_iff_not]; omega
                  dsimp only [st₁', st₂', clobber, bumpWorldStep]
                  simp only [hb0, hb15, Bool.false_eq_true, ↓reduceIte]
                  exact hregs r hr hlen_ctx (hregs_6_9 r hr hr0 h15 hd)
            · intro s hs hd
              dsimp only [st₁', st₂', succ, bumpWorldStep]
              rw [hstk_n1, hstk_n]
              exact hstk s hs (by rw [hstk_deps s]; exact hd)
          have halloc' : (bumpWorldStep st₁.world).nextAllocId + 3 * F < 2 ^ 63 := by
            simp only [bumpWorldStep]; omega
          obtain ⟨ret, finalWorld, hr₁, hr₂, hlive_fin⟩ :=
            hchildren succ hmem_succ l₁ l₂ s₁ s₂ ρ resActive isNullId st₁' st₂' hl₁ hl₂ hleak halloc' hρ hrel_post
          refine ⟨ret, finalWorld, ?_, ?_, hlive_fin⟩
          · unfold runSteps step
            rcases hind_disj with ⟨rfl, rfl⟩ | ⟨reg, iv, rfl, hn_ge, hiv1, _, rfl⟩
            · simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, henv_pkt,
                hmatch_r6_1, Bool.not_true, Bool.false_eq_true, ↓reduceIte,
                Option.map_none, hovf_f, horacle]
              rw [stepNextIte_eq_next (by dsimp [st₁', clobber]; rw [hsucc_pc]) hr₁]
              exact hr₁
            · simp only [hrel.1, hlt_pc, ↓reduceDIte, hinsn_at, henv_pkt,
                hmatch_r6_1, Bool.not_true, Bool.false_eq_true, ↓reduceIte,
                Option.map_some, hn_ge, hiv1, hovf_f, horacle]
              rw [stepNextIte_eq_next (by dsimp [st₁', clobber]; rw [hsucc_pc]) hr₁]
              exact hr₁
          · unfold runSteps step
            rcases hind_disj with ⟨rfl, rfl⟩ | ⟨reg, iv, rfl, hn_ge, _, hiv2, rfl⟩
            · simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, henv_pkt,
                hmatch_r6_2, Bool.not_true, Bool.false_eq_true, ↓reduceIte,
                Option.map_none, hovf_f, hw_eq, horacle]
              rw [stepNextIte_eq_next (by dsimp [st₂', clobber]; rw [hsucc_pc, hw_eq]) hr₂]
              exact hr₂
            · simp only [hrel.2.1, hlt_pc, ↓reduceDIte, hinsn_at, henv_pkt,
                hmatch_r6_2, Bool.not_true, Bool.false_eq_true, ↓reduceIte,
                Option.map_some, hn_ge, hiv2, hovf_f, hw_eq, horacle]
              rw [stepNextIte_eq_next (by dsimp [st₂', clobber]; rw [hsucc_pc, hw_eq]) hr₂]
              exact hr₂

end StarnixEbpf.Proof

