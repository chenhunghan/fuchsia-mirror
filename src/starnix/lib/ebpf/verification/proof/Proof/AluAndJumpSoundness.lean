-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.ComputationCtxOps

/-!
# Scalar & Pointer ALU Soundness (`ScalarAluOp.eval` & `apply_computation`)

Proves `runSteps_pc_valid`, uninitialized scalar arithmetic helpers, structural
validity and relational 2-safety soundness for `ScalarAluOp.eval`, and scalar /
pointer ALU transfer soundness for `ComputationContext.apply_computation`.

`compute_source` and PC-step (`jump_with_offset`, `next`, `checked_jump_with_offset`)
specifications live in `Proof/ComputationCtxOps.lean`; instruction-level
`.alu64` / `.alu32` (`mov`, ALU), `.neg`, endianness (`le` / `be`), unconditional
`.jump`, and `.exit` soundness live in `Proof/InsnAluEndExit.lean`; and
conditional jumps live in `Proof/CondJumpSoundness.lean`.
-/

set_option linter.unusedTactic false
set_option linter.unnecessarySimpa false
set_option linter.unnecessarySeqFocus false
set_option maxHeartbeats 800000

open Aeneas Aeneas.Std Result

namespace StarnixEbpf.Proof

open StarnixEbpf.Assumptions
open StarnixEbpf.Spec
open StarnixEbpf.LoopLemmas
open ebpf_core
open ebpf_core.scalar_value

/-! ## 1. `runSteps` PC Validity & Abstract Scalar Uninitialized Arithmetic Helpers -/

theorem runSteps_pc_valid {prog : Program} {env : ExecEnv}
    {layout : HostLayout} {secret : SecretInit}
    {f : Nat} {st : MachineState} {res : BitVec 64 × WorldState}
    (h : runSteps prog env layout secret f st = some (.inl res)) :
    0 < f ∧ ∃ hpc : st.pc < prog.size, (prog[st.pc]'hpc == .lddwCont) = false := by
  cases f with
  | zero => cases h
  | succ k =>
    refine ⟨Nat.zero_lt_succ k, ?_⟩
    unfold runSteps at h
    by_cases hpc : st.pc < prog.size
    · refine ⟨hpc, ?_⟩
      by_cases hlddw : (prog[st.pc]'hpc == .lddwCont) = true
      · have heq : prog[st.pc]'hpc = .lddwCont := by
          cases hi : prog[st.pc]'hpc <;> rw [hi] at hlddw <;> first | rfl | cases hlddw
        unfold step at h
        simp only [hpc, ↓reduceDIte, heq] at h
        cases h
      · exact Bool.eq_false_iff.mpr hlddw
    · unfold step at h
      simp only [hpc, ↓reduceDIte] at h
      cases h

theorem TypeRel2_scalar_toBits_rel2
    {env : ExecEnv} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64}
    {layout₁ layout₂ : HostLayout}
    {d : U64ScalarValueData} {v₁ v₂ : RuntimeVal}
    (hrel : TypeRel2 env ρ resActive isNullId arrayLen (.ScalarValue d) v₁ v₂) :
    ConcretizesScalar64Rel2 d (v₁.toBits layout₁ arrayLen) (v₂.toBits layout₂ arrayLen) := by
  by_cases hd : d = uninitScalar64
  · subst hd
    exact concretizesScalar64Rel2_uninit _ _
  · simp only [TypeRel2, hd, ↓reduceIte] at hrel
    rcases hrel with ⟨x₁, x₂, rfl, rfl, hconc⟩
    exact hconc

theorem TypeRel2_scalar_range_nonempty
    {env : ExecEnv} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64}
    {d : U64ScalarValueData} {v₁ v₂ : RuntimeVal}
    (hrel : TypeRel2 env ρ resActive isNullId arrayLen (.ScalarValue d) v₁ v₂) :
    d.urange.min.bv ≤ d.urange.max.bv := by
  have hc := TypeRel2_scalar_toBits_rel2 (layout₁ := dummyHostLayout) (layout₂ := dummyHostLayout) hrel
  exact hc.1.1.2.2.1

theorem uninitScalar64_unknown_ne_zero : uninitScalar64.unknown_mask ≠ 0#u64 := by
  intro hc
  have hbv := congrArg UScalar.bv hc
  simp only [uninitScalar64, u64_max_bv, u64_zero_bv] at hbv
  revert hbv; decide

private theorem add_range_uninit_left {r : U64Range} (hle : r.min.bv ≤ r.max.bv) :
    U64Range.add_range uninitScalar64.urange r = ok uninitScalar64.urange := by
  rcases r with ⟨⟨min2⟩, ⟨max2⟩⟩
  change BitVec 64 at min2 max2
  dsimp only [u64_mk_bv] at hle
  unfold U64Range.add_range U64Range.impl.max U64Range.new
    core.num.U64.overflowing_add UScalar.overflowing_add lift uninitScalar64
  rw [bind_pair_ok_eq rfl, bind_pair_ok_eq rfl]
  dsimp only [u64_mk_uscalar_bv, UScalarTy.numBits]
  by_cases hov : ((0#u64).bv.uaddOverflow min2 != (core.num.U64.MAX).bv.uaddOverflow max2) = true
  · simp only [hov, ↓reduceIte]
  · simp only [hov, Bool.false_eq_true, ↓reduceIte]
    congr 1
    simp only [U64Range.mk.injEq, u64_eq_iff_bv, u64_mk_bv, u64_zero_bv, u64_max_bv] at hov ⊢
    dsimp only [UScalarTy.numBits] at hov
    change 0#64 + min2 = 0#64 ∧ ~~~0#64 + max2 = ~~~0#64
    revert hov hle; bv_decide

private theorem add_range_uninit_right {r : U64Range} (hle : r.min.bv ≤ r.max.bv) :
    U64Range.add_range r uninitScalar64.urange = ok uninitScalar64.urange := by
  rcases r with ⟨⟨min1⟩, ⟨max1⟩⟩
  change BitVec 64 at min1 max1
  dsimp only [u64_mk_bv] at hle
  unfold U64Range.add_range U64Range.impl.max U64Range.new
    core.num.U64.overflowing_add UScalar.overflowing_add lift uninitScalar64
  rw [bind_pair_ok_eq rfl, bind_pair_ok_eq rfl]
  dsimp only [u64_mk_uscalar_bv, UScalarTy.numBits]
  by_cases hov : (min1.uaddOverflow (0#u64).bv != max1.uaddOverflow (core.num.U64.MAX).bv) = true
  · simp only [hov, ↓reduceIte]
  · simp only [hov, Bool.false_eq_true, ↓reduceIte]
    congr 1
    simp only [U64Range.mk.injEq, u64_eq_iff_bv, u64_mk_bv, u64_zero_bv, u64_max_bv] at hov ⊢
    change min1 + 0#64 = 0#64 ∧ max1 + ~~~0#64 = ~~~0#64
    revert hov hle; bv_decide

private theorem sub_range_uninit_left {r : U64Range} (hle : r.min.bv ≤ r.max.bv) :
    U64Range.sub_range uninitScalar64.urange r = ok uninitScalar64.urange := by
  rcases r with ⟨⟨min2⟩, ⟨max2⟩⟩
  change BitVec 64 at min2 max2
  dsimp only [u64_mk_bv] at hle
  unfold U64Range.sub_range U64Range.impl.max U64Range.new
    core.num.U64.overflowing_sub UScalar.overflowing_sub lift uninitScalar64
  rw [bind_pair_ok_eq rfl, bind_pair_ok_eq rfl]
  dsimp only [u64_mk_uscalar_bv, UScalarTy.numBits]
  by_cases hov : ((0#u64).bv.usubOverflow max2 != (core.num.U64.MAX).bv.usubOverflow min2) = true
  · simp only [hov, ↓reduceIte]
  · simp only [hov, Bool.false_eq_true, ↓reduceIte]
    congr 1
    simp only [U64Range.mk.injEq, u64_eq_iff_bv, u64_mk_bv, u64_zero_bv, u64_max_bv] at hov ⊢
    dsimp only [UScalarTy.numBits] at hov
    change 0#64 - max2 = 0#64 ∧ ~~~0#64 - min2 = ~~~0#64
    revert hov hle; bv_decide

private theorem sub_range_uninit_right {r : U64Range} (hle : r.min.bv ≤ r.max.bv) :
    U64Range.sub_range r uninitScalar64.urange = ok uninitScalar64.urange := by
  rcases r with ⟨⟨min1⟩, ⟨max1⟩⟩
  change BitVec 64 at min1 max1
  dsimp only [u64_mk_bv] at hle
  unfold U64Range.sub_range U64Range.impl.max U64Range.new
    core.num.U64.overflowing_sub UScalar.overflowing_sub lift uninitScalar64
  rw [bind_pair_ok_eq rfl, bind_pair_ok_eq rfl]
  dsimp only [u64_mk_uscalar_bv, UScalarTy.numBits]
  by_cases hov : (min1.usubOverflow (core.num.U64.MAX).bv != max1.usubOverflow (0#u64).bv) = true
  · simp only [hov, ↓reduceIte]
  · simp only [hov, Bool.false_eq_true, ↓reduceIte]
    congr 1
    simp only [U64Range.mk.injEq, u64_eq_iff_bv, u64_mk_bv, u64_zero_bv, u64_max_bv] at hov ⊢
    change min1 - ~~~0#64 = 0#64 ∧ max1 - 0#64 = ~~~0#64
    revert hov hle; bv_decide

private theorem update_range_uninit_self :
    U64ScalarValueData.update_range uninitScalar64 uninitScalar64.urange = ok uninitScalar64 := by
  unfold U64ScalarValueData.update_range
  rw [new64_eq_ok]
  congr 1

private theorem base_uninit_either {a b : U64ScalarValueData}
    (hu : a = uninitScalar64 ∨ b = uninitScalar64) :
    U64ScalarValueData.base a b = ok uninitScalar64 := by
  unfold U64ScalarValueData.base U64ScalarValueData.is_fully_initialized
  rw [bind_ok_eq rfl]
  rcases hu with rfl | rfl
  · simp only [uninitScalar64_unwritten_ne_zero, decide_false, Bool.false_eq_true, ↓reduceIte,
      UNINITIALIZED_64_eq]
  · by_cases ha : a.unwritten_mask = 0#u64
    · simp only [ha, decide_true, ↓reduceIte]
      rw [bind_ok_eq rfl]
      simp only [uninitScalar64_unwritten_ne_zero, decide_false, Bool.false_eq_true, ↓reduceIte,
        UNINITIALIZED_64_eq]
    · simp only [ha, decide_false, Bool.false_eq_true, ↓reduceIte, UNINITIALIZED_64_eq]

theorem add_scalar_uninit_eq {a b : U64ScalarValueData}
    (ha_le : a.urange.min.bv ≤ a.urange.max.bv)
    (hb_le : b.urange.min.bv ≤ b.urange.max.bv)
    (hu : a = uninitScalar64 ∨ b = uninitScalar64) :
    U64ScalarValueData.add_scalar a b = ok uninitScalar64 := by
  unfold U64ScalarValueData.add_scalar U64ScalarValueData.is_known
  rw [bind_ok_eq rfl]
  have hbase := base_uninit_either hu
  have hur : U64Range.add_range a.urange b.urange = ok uninitScalar64.urange := by
    rcases hu with rfl | rfl
    · exact add_range_uninit_left hb_le
    · exact add_range_uninit_right ha_le
  by_cases hak : a.unknown_mask = 0#u64
  · rcases hu with rfl | rfl
    · exact absurd hak uninitScalar64_unknown_ne_zero
    · simp only [hak, decide_true, ↓reduceIte]
      rw [bind_ok_eq rfl]
      simp only [uninitScalar64_unknown_ne_zero, decide_false, Bool.false_eq_true, ↓reduceIte]
      rw [bind_ok_eq hbase, bind_ok_eq hur, update_range_uninit_self]
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
    rw [bind_ok_eq hbase, bind_ok_eq hur, update_range_uninit_self]

theorem sub_scalar_uninit_eq {a b : U64ScalarValueData}
    (ha_le : a.urange.min.bv ≤ a.urange.max.bv)
    (hb_le : b.urange.min.bv ≤ b.urange.max.bv)
    (hu : a = uninitScalar64 ∨ b = uninitScalar64) :
    U64ScalarValueData.sub_scalar a b = ok uninitScalar64 := by
  unfold U64ScalarValueData.sub_scalar U64ScalarValueData.is_known
  rw [bind_ok_eq rfl]
  have hbase := base_uninit_either hu
  have hur : U64Range.sub_range a.urange b.urange = ok uninitScalar64.urange := by
    rcases hu with rfl | rfl
    · exact sub_range_uninit_left hb_le
    · exact sub_range_uninit_right ha_le
  by_cases hak : a.unknown_mask = 0#u64
  · rcases hu with rfl | rfl
    · exact absurd hak uninitScalar64_unknown_ne_zero
    · simp only [hak, decide_true, ↓reduceIte]
      rw [bind_ok_eq rfl]
      simp only [uninitScalar64_unknown_ne_zero, decide_false, Bool.false_eq_true, ↓reduceIte]
      rw [bind_ok_eq hbase, bind_ok_eq hur, update_range_uninit_self]
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
    rw [bind_ok_eq hbase, bind_ok_eq hur, update_range_uninit_self]

/-! ## 2. `ScalarAluOp.eval` Structural Validity, Initialization Preservation, & 2-Safety -/

private theorem update_range_64_valid {d res : U64ScalarValueData} {ur : U64Range}
    (h : U64ScalarValueData.update_range d ur = ok res) : Scalar64Valid res :=
  new64_valid h

private theorem base_64_valid {a b res : U64ScalarValueData}
    (h : U64ScalarValueData.base a b = ok res) : Scalar64Valid res := by
  unfold U64ScalarValueData.base U64ScalarValueData.is_fully_initialized at h
  rw [bind_ok_eq rfl] at h
  split_ifs at h
  · rw [bind_ok_eq rfl] at h
    split_ifs at h
    · exact UNKNOWN_WRITTEN_64_valid h
    · exact UNINITIALIZED_64_valid h
  · exact UNINITIALIZED_64_valid h

private theorem into_u64_scalar_valid {v : U32ScalarValueData} {res : U64ScalarValueData}
    (h : U32ScalarValueData.into_u64_scalar v = ok res) : Scalar64Valid res := by
  unfold U32ScalarValueData.into_u64_scalar at h
  rcases bind_eq_ok h with ⟨_, _, h⟩
  rcases bind_eq_ok h with ⟨_, _, h⟩
  rcases bind_eq_ok h with ⟨_, _, h⟩
  rcases bind_eq_ok h with ⟨_, _, h⟩
  rcases bind_eq_ok h with ⟨_, _, h⟩
  rcases bind_eq_ok h with ⟨_, _, h⟩
  rcases bind_eq_ok h with ⟨_, _, h⟩
  rcases bind_eq_ok h with ⟨_, _, h⟩
  exact new64_valid h

theorem checked_add_64_some_valid {a b res : U64ScalarValueData}
    (h : U64ScalarValueData.checked_add a b = ok (some res)) : Scalar64Valid res := by
  unfold U64ScalarValueData.checked_add U64ScalarValueData.checked_add_scalar
    U64ScalarValueData.is_known at h
  rw [bind_ok_eq rfl] at h
  by_cases hak : a.unknown_mask = 0#u64
  · simp only [hak, decide_true, ↓reduceIte] at h
    rw [bind_ok_eq rfl] at h
    by_cases hbk : b.unknown_mask = 0#u64
    · simp only [hbk, decide_true, ↓reduceIte, lift, bind_ok] at h
      cases hck : U64.checked_add a.value b.value with
      | none => simp [hck] at h
      | some v =>
        simp only [hck] at h
        rcases bind_eq_ok h with ⟨d, hd, h⟩
        cases ok_inj h
        exact from_known_64_valid hd
    · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte] at h
      rcases bind_eq_ok h with ⟨opt, _, h⟩
      cases opt with
      | none => cases ok_inj h
      | some ur =>
        rcases bind_eq_ok h with ⟨_, _, h⟩
        rcases bind_eq_ok h with ⟨d, hd, h⟩
        cases ok_inj h
        exact update_range_64_valid hd
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte] at h
    rcases bind_eq_ok h with ⟨opt, _, h⟩
    cases opt with
    | none => cases ok_inj h
    | some ur =>
      rcases bind_eq_ok h with ⟨_, _, h⟩
      rcases bind_eq_ok h with ⟨d, hd, h⟩
      cases ok_inj h
      exact update_range_64_valid hd

theorem checked_sub_64_some_valid {a b res : U64ScalarValueData}
    (h : U64ScalarValueData.checked_sub a b = ok (some res)) : Scalar64Valid res := by
  unfold U64ScalarValueData.checked_sub U64ScalarValueData.checked_sub_scalar
    U64ScalarValueData.is_known at h
  rw [bind_ok_eq rfl] at h
  by_cases hak : a.unknown_mask = 0#u64
  · simp only [hak, decide_true, ↓reduceIte] at h
    rw [bind_ok_eq rfl] at h
    by_cases hbk : b.unknown_mask = 0#u64
    · simp only [hbk, decide_true, ↓reduceIte, lift, bind_ok] at h
      cases hck : U64.checked_sub a.value b.value with
      | none => simp [hck] at h
      | some v =>
        simp only [hck] at h
        rcases bind_eq_ok h with ⟨d, hd, h⟩
        cases ok_inj h
        exact from_known_64_valid hd
    · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte] at h
      rcases bind_eq_ok h with ⟨opt, _, h⟩
      cases opt with
      | none => cases ok_inj h
      | some ur =>
        rcases bind_eq_ok h with ⟨_, _, h⟩
        rcases bind_eq_ok h with ⟨d, hd, h⟩
        cases ok_inj h
        exact update_range_64_valid hd
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte] at h
    rcases bind_eq_ok h with ⟨opt, _, h⟩
    cases opt with
    | none => cases ok_inj h
    | some ur =>
      rcases bind_eq_ok h with ⟨_, _, h⟩
      rcases bind_eq_ok h with ⟨d, hd, h⟩
      cases ok_inj h
      exact update_range_64_valid hd

theorem ScalarAluOp_eval_valid {op : verifier.ScalarAluOp}
    {x y res : U64ScalarValueData}
    (hx : Scalar64Valid x) (hy : Scalar64Valid y)
    (h : verifier.ScalarAluOp.eval op x y = ok res) :
    Scalar64Valid res := by
  unfold verifier.ScalarAluOp.eval at h
  cases op with
  | Add32 | And32 | Arsh32 | Div32 | Lsh32 | Mod32 | Mul32 | Or32 | Rsh32 | Sub32 | Xor32 =>
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    exact into_u64_scalar_valid h
  | Neg32 =>
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    exact into_u64_scalar_valid h
  | Add64 =>
    unfold U64ScalarValueData.add_scalar at h
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1
    · rcases bind_eq_ok h with ⟨_, _, h⟩
      rcases bind_eq_ok h with ⟨_, _, h⟩
      exact update_range_64_valid h
    · rcases bind_eq_ok h with ⟨b2, _, h⟩
      cases b2
      · rcases bind_eq_ok h with ⟨_, _, h⟩
        rcases bind_eq_ok h with ⟨_, _, h⟩
        exact update_range_64_valid h
      · rcases bind_eq_ok h with ⟨_, _, h⟩
        exact from_known_64_valid h
  | Sub64 =>
    unfold U64ScalarValueData.sub_scalar at h
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1
    · rcases bind_eq_ok h with ⟨_, _, h⟩
      rcases bind_eq_ok h with ⟨_, _, h⟩
      exact update_range_64_valid h
    · rcases bind_eq_ok h with ⟨b2, _, h⟩
      cases b2
      · rcases bind_eq_ok h with ⟨_, _, h⟩
        rcases bind_eq_ok h with ⟨_, _, h⟩
        exact update_range_64_valid h
      · rcases bind_eq_ok h with ⟨_, _, h⟩
        exact from_known_64_valid h
  | Mul64 =>
    unfold U64ScalarValueData.mul_scalar at h
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1
    · rcases bind_eq_ok h with ⟨_, _, h⟩
      rcases bind_eq_ok h with ⟨_, _, h⟩
      exact update_range_64_valid h
    · rcases bind_eq_ok h with ⟨b2, _, h⟩
      cases b2
      · rcases bind_eq_ok h with ⟨_, _, h⟩
        rcases bind_eq_ok h with ⟨_, _, h⟩
        exact update_range_64_valid h
      · rcases bind_eq_ok h with ⟨_, _, h⟩
        exact from_known_64_valid h
  | Div64 =>
    unfold U64ScalarValueData.div_scalar at h
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1
    · rcases bind_eq_ok h with ⟨_, _, h⟩
      rcases bind_eq_ok h with ⟨_, _, h⟩
      exact update_range_64_valid h
    · rcases bind_eq_ok h with ⟨b2, _, h⟩
      cases b2
      · rcases bind_eq_ok h with ⟨_, _, h⟩
        rcases bind_eq_ok h with ⟨_, _, h⟩
        exact update_range_64_valid h
      · by_cases hzero : y.value = 0#u64
        · simp only [hzero, ↓reduceIte] at h
          exact from_known_64_valid h
        · simp only [hzero, ↓reduceIte] at h
          rcases bind_eq_ok h with ⟨_, _, h⟩
          exact from_known_64_valid h
  | Mod64 =>
    unfold U64ScalarValueData.rem_scalar at h
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1
    · rcases bind_eq_ok h with ⟨_, _, h⟩
      rcases bind_eq_ok h with ⟨_, _, h⟩
      exact update_range_64_valid h
    · rcases bind_eq_ok h with ⟨b2, _, h⟩
      cases b2
      · rcases bind_eq_ok h with ⟨_, _, h⟩
        rcases bind_eq_ok h with ⟨_, _, h⟩
        exact update_range_64_valid h
      · by_cases hzero : y.value = 0#u64
        · simp only [hzero, ↓reduceIte] at h
          cases ok_inj h; exact hx
        · simp only [hzero, ↓reduceIte] at h
          rcases bind_eq_ok h with ⟨_, _, h⟩
          exact from_known_64_valid h
  | Neg64 =>
    unfold U64ScalarValueData.neg_scalar at h
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1
    · rcases bind_eq_ok h with ⟨_, _, h⟩
      rcases bind_eq_ok h with ⟨_, _, h⟩
      exact update_range_64_valid h
    · rcases bind_eq_ok h with ⟨_, _, h⟩
      exact from_known_64_valid h
  | And64 =>
    unfold U64ScalarValueData.bitand_scalar at h
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    exact new64_valid h
  | Or64 =>
    unfold U64ScalarValueData.bitor_scalar at h
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    exact new64_valid h
  | Xor64 =>
    unfold U64ScalarValueData.bitxor_scalar at h
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨_, _, h⟩
    exact new64_valid h
  | Lsh64 =>
    unfold U64ScalarValueData.shl_scalar at h
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1
    · rcases bind_eq_ok h with ⟨_, _, h⟩
      exact update_range_64_valid h
    · rcases bind_eq_ok h with ⟨_, _, h⟩
      rcases bind_eq_ok h with ⟨_, _, h⟩
      rcases bind_eq_ok h with ⟨_, _, h⟩
      rcases bind_eq_ok h with ⟨_, _, h⟩
      exact new64_valid h
  | Rsh64 =>
    unfold U64ScalarValueData.shr_scalar at h
    rcases bind_eq_ok h with ⟨_, _, h⟩
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1
    · rcases bind_eq_ok h with ⟨_, _, h⟩
      exact update_range_64_valid h
    · rcases bind_eq_ok h with ⟨_, _, h⟩
      rcases bind_eq_ok h with ⟨_, _, h⟩
      rcases bind_eq_ok h with ⟨_, _, h⟩
      rcases bind_eq_ok h with ⟨_, _, h⟩
      exact new64_valid h
  | Arsh64 =>
    unfold U64ScalarValueData.ashr at h
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1
    · exact base_64_valid h
    · rcases bind_eq_ok h with ⟨b2, _, h⟩
      cases b2
      · rcases bind_eq_ok h with ⟨_, _, h⟩
        rcases bind_eq_ok h with ⟨_, _, h⟩
        rcases bind_eq_ok h with ⟨_, _, h⟩
        rcases bind_eq_ok h with ⟨_, _, h⟩
        rcases bind_eq_ok h with ⟨_, _, h⟩
        rcases bind_eq_ok h with ⟨_, _, h⟩
        exact new64_valid h
      · rcases bind_eq_ok h with ⟨_, _, h⟩
        exact from_known_64_valid h
  | Xchg =>
    cases ok_inj h; exact hy

private theorem new64_unwritten_zero {v unk unwr : Std.U64} {ur : U64Range} {res : U64ScalarValueData}
    (hz : unwr.bv = 0#64)
    (h : U64ScalarValueData.new v unk unwr ur = ok res) :
    res.unwritten_mask.bv = 0#64 := by
  rw [new64_eq_ok] at h
  cases ok_inj h
  unfold pureNew64
  simp only [u64_mk_bv, hz, BitVec.zero_and]

private theorem new32_unwritten_zero {v unk unwr : Std.U32} {ur : U32Range} {res : U32ScalarValueData}
    (hz : unwr.bv = 0#32)
    (h : U32ScalarValueData.new v unk unwr ur = ok res) :
    res.unwritten_mask.bv = 0#32 := by
  rw [new32_eq_ok] at h
  cases ok_inj h
  unfold pureNew32
  simp only [u32_mk_bv, hz, BitVec.zero_and]

private theorem from_known_64_unwritten_zero {v : Std.U64} {res : U64ScalarValueData}
    (h : U64ScalarValueData.from_known v = ok res) : res.unwritten_mask.bv = 0#64 := by
  rw [from_known_64_eq] at h
  cases ok_inj h
  rfl

private theorem base_64_unwritten_zero {a b res : U64ScalarValueData}
    (ha : a.unwritten_mask.bv = 0#64) (hb : b.unwritten_mask.bv = 0#64)
    (h : U64ScalarValueData.base a b = ok res) : res.unwritten_mask.bv = 0#64 := by
  have ha0 : a.unwritten_mask = 0#u64 := (u64_eq_iff_bv _ _).mpr ha
  have hb0 : b.unwritten_mask = 0#u64 := (u64_eq_iff_bv _ _).mpr hb
  unfold U64ScalarValueData.base U64ScalarValueData.is_fully_initialized at h
  simp only [ha0, hb0, decide_true, ↓reduceIte, bind_ok, UNKNOWN_WRITTEN_64_eq] at h
  cases ok_inj h
  rfl

private theorem update_range_64_unwritten_zero {d res : U64ScalarValueData} {ur : U64Range}
    (hd : d.unwritten_mask.bv = 0#64)
    (h : U64ScalarValueData.update_range d ur = ok res) : res.unwritten_mask.bv = 0#64 :=
  new64_unwritten_zero hd h

theorem checked_add_64_unwritten_zero {a b res : U64ScalarValueData}
    (ha : a.unwritten_mask.bv = 0#64) (hb : b.unwritten_mask.bv = 0#64)
    (h : U64ScalarValueData.checked_add a b = ok (some res)) :
    res.unwritten_mask.bv = 0#64 := by
  unfold U64ScalarValueData.checked_add U64ScalarValueData.checked_add_scalar
    U64ScalarValueData.is_known at h
  rw [bind_ok_eq rfl] at h
  by_cases hak : a.unknown_mask = 0#u64
  · simp only [hak, decide_true, ↓reduceIte] at h
    rw [bind_ok_eq rfl] at h
    by_cases hbk : b.unknown_mask = 0#u64
    · simp only [hbk, decide_true, ↓reduceIte, lift, bind_ok] at h
      cases hck : U64.checked_add a.value b.value with
      | none => simp [hck] at h
      | some v =>
        simp only [hck] at h
        rcases bind_eq_ok h with ⟨d, hd, h⟩
        cases ok_inj h
        exact from_known_64_unwritten_zero hd
    · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte] at h
      rcases bind_eq_ok h with ⟨opt, _, h⟩
      cases opt with
      | none => cases ok_inj h
      | some ur =>
        rcases bind_eq_ok h with ⟨bs, hbs, h⟩
        rcases bind_eq_ok h with ⟨d, hd, h⟩
        cases ok_inj h
        exact update_range_64_unwritten_zero (base_64_unwritten_zero ha hb hbs) hd
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte] at h
    rcases bind_eq_ok h with ⟨opt, _, h⟩
    cases opt with
    | none => cases ok_inj h
    | some ur =>
      rcases bind_eq_ok h with ⟨bs, hbs, h⟩
      rcases bind_eq_ok h with ⟨d, hd, h⟩
      cases ok_inj h
      exact update_range_64_unwritten_zero (base_64_unwritten_zero ha hb hbs) hd

theorem checked_sub_64_unwritten_zero {a b res : U64ScalarValueData}
    (ha : a.unwritten_mask.bv = 0#64) (hb : b.unwritten_mask.bv = 0#64)
    (h : U64ScalarValueData.checked_sub a b = ok (some res)) :
    res.unwritten_mask.bv = 0#64 := by
  unfold U64ScalarValueData.checked_sub U64ScalarValueData.checked_sub_scalar
    U64ScalarValueData.is_known at h
  rw [bind_ok_eq rfl] at h
  by_cases hak : a.unknown_mask = 0#u64
  · simp only [hak, decide_true, ↓reduceIte] at h
    rw [bind_ok_eq rfl] at h
    by_cases hbk : b.unknown_mask = 0#u64
    · simp only [hbk, decide_true, ↓reduceIte, lift, bind_ok] at h
      cases hck : U64.checked_sub a.value b.value with
      | none => simp [hck] at h
      | some v =>
        simp only [hck] at h
        rcases bind_eq_ok h with ⟨d, hd, h⟩
        cases ok_inj h
        exact from_known_64_unwritten_zero hd
    · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte] at h
      rcases bind_eq_ok h with ⟨opt, _, h⟩
      cases opt with
      | none => cases ok_inj h
      | some ur =>
        rcases bind_eq_ok h with ⟨bs, hbs, h⟩
        rcases bind_eq_ok h with ⟨d, hd, h⟩
        cases ok_inj h
        exact update_range_64_unwritten_zero (base_64_unwritten_zero ha hb hbs) hd
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte] at h
    rcases bind_eq_ok h with ⟨opt, _, h⟩
    cases opt with
    | none => cases ok_inj h
    | some ur =>
      rcases bind_eq_ok h with ⟨bs, hbs, h⟩
      rcases bind_eq_ok h with ⟨d, hd, h⟩
      cases ok_inj h
      exact update_range_64_unwritten_zero (base_64_unwritten_zero ha hb hbs) hd

theorem add_or_sub_64_unwritten_zero {op : verifier.ScalarAluOp}
    {a b res : U64ScalarValueData}
    (hop : op = .Add64 ∨ op = .Sub64)
    (ha : a.unwritten_mask.bv = 0#64) (hb : b.unwritten_mask.bv = 0#64)
    (h : verifier.ScalarAluOp.eval op a b = ok res) :
    res.unwritten_mask.bv = 0#64 := by
  rcases hop with rfl | rfl
  · unfold verifier.ScalarAluOp.eval U64ScalarValueData.add_scalar at h
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1
    · rcases bind_eq_ok h with ⟨bs, hbs, h⟩
      rcases bind_eq_ok h with ⟨_, _, h⟩
      exact update_range_64_unwritten_zero (base_64_unwritten_zero ha hb hbs) h
    · rcases bind_eq_ok h with ⟨b2, _, h⟩
      cases b2
      · rcases bind_eq_ok h with ⟨bs, hbs, h⟩
        rcases bind_eq_ok h with ⟨_, _, h⟩
        exact update_range_64_unwritten_zero (base_64_unwritten_zero ha hb hbs) h
      · rcases bind_eq_ok h with ⟨_, _, h⟩
        exact from_known_64_unwritten_zero h
  · unfold verifier.ScalarAluOp.eval U64ScalarValueData.sub_scalar at h
    rcases bind_eq_ok h with ⟨b1, _, h⟩
    cases b1
    · rcases bind_eq_ok h with ⟨bs, hbs, h⟩
      rcases bind_eq_ok h with ⟨_, _, h⟩
      exact update_range_64_unwritten_zero (base_64_unwritten_zero ha hb hbs) h
    · rcases bind_eq_ok h with ⟨b2, _, h⟩
      cases b2
      · rcases bind_eq_ok h with ⟨bs, hbs, h⟩
        rcases bind_eq_ok h with ⟨_, _, h⟩
        exact update_range_64_unwritten_zero (base_64_unwritten_zero ha hb hbs) h
      · rcases bind_eq_ok h with ⟨_, _, h⟩
        exact from_known_64_unwritten_zero h

/-- Pairing invariant between instruction-level `(is64, AluOp)` and verifier-level
    `(AluType, ScalarAluOp)`. -/
inductive AluOpMatches : Bool → AluOp → verifier.AluType → verifier.ScalarAluOp → Prop where
  | add64  : AluOpMatches true  .add  .Add   .Add64
  | sub64  : AluOpMatches true  .sub  .Sub   .Sub64
  | mul64  : AluOpMatches true  .mul  .Plain .Mul64
  | div64  : AluOpMatches true  .div  .Plain .Div64
  | rem64  : AluOpMatches true  .rem  .Plain .Mod64
  | and64  : AluOpMatches true  .and  .Plain .And64
  | or64   : AluOpMatches true  .or   .Plain .Or64
  | xor64  : AluOpMatches true  .xor  .Plain .Xor64
  | lsh64  : AluOpMatches true  .lsh  .Plain .Lsh64
  | rsh64  : AluOpMatches true  .rsh  .Plain .Rsh64
  | arsh64 : AluOpMatches true  .arsh .Plain .Arsh64
  | add32  : AluOpMatches false .add  .Plain .Add32
  | sub32  : AluOpMatches false .sub  .Plain .Sub32
  | mul32  : AluOpMatches false .mul  .Plain .Mul32
  | div32  : AluOpMatches false .div  .Plain .Div32
  | rem32  : AluOpMatches false .rem  .Plain .Mod32
  | and32  : AluOpMatches false .and  .Plain .And32
  | or32   : AluOpMatches false .or   .Plain .Or32
  | xor32  : AluOpMatches false .xor  .Plain .Xor32
  | lsh32  : AluOpMatches false .lsh  .Plain .Lsh32
  | rsh32  : AluOpMatches false .rsh  .Plain .Rsh32
  | arsh32 : AluOpMatches false .arsh .Plain .Arsh32

theorem ScalarAluOp_eval_sound_2safety
    {is64 : Bool} {op : AluOp} {alu_type : verifier.AluType} {alu_op : verifier.ScalarAluOp}
    (hm : AluOpMatches is64 op alu_type alu_op)
    {a b : U64ScalarValueData} {x₁ x₂ y₁ y₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂)
    (hb : ConcretizesScalar64Rel2 b y₁ y₂) :
    ∃ res, verifier.ScalarAluOp.eval alu_op a b = ok res ∧
      ConcretizesScalar64Rel2 res
        (if is64 then evalAlu64 op x₁ y₁ else evalAlu32 op x₁ y₁)
        (if is64 then evalAlu64 op x₂ y₂ else evalAlu32 op x₂ y₂) := by
  cases hm with
  | add64  => exact add_scalar_64_sound_2safety a b ha hb
  | sub64  => exact sub_scalar_64_sound_2safety a b ha hb
  | mul64  => exact mul_scalar_64_sound_2safety a b ha hb
  | div64  => exact div_scalar_64_sound_2safety a b ha hb
  | rem64  => exact rem_scalar_64_sound_2safety a b ha hb
  | and64  => exact bitand_scalar_64_sound_2safety a b ha hb
  | or64   => exact bitor_scalar_64_sound_2safety a b ha hb
  | xor64  => exact bitxor_scalar_64_sound_2safety a b ha hb
  | lsh64  => exact shl_scalar_64_sound_2safety a b ha hb
  | rsh64  => exact shr_scalar_64_sound_2safety a b ha hb
  | arsh64 => exact ashr_64_sound_2safety a b ha hb
  | add32  =>
    dsimp only [verifier.ScalarAluOp.eval, evalAlu32, ↓reduceIte]
    obtain ⟨u1, hu1, hc1⟩ := from_u64_scalar_sound_2safety a ha
    obtain ⟨u2, hu2, hc2⟩ := from_u64_scalar_sound_2safety b hb
    obtain ⟨u3, hu3, hc3⟩ := add_scalar_32_sound_2safety u1 u2 hc1 hc2
    obtain ⟨res, hres, hc⟩ := into_u64_scalar_sound_2safety u3 hc3
    rw [bind_ok_eq hu1, bind_ok_eq hu2, bind_ok_eq hu3]
    exact ⟨res, hres, hc⟩
  | sub32  =>
    dsimp only [verifier.ScalarAluOp.eval, evalAlu32, ↓reduceIte]
    obtain ⟨u1, hu1, hc1⟩ := from_u64_scalar_sound_2safety a ha
    obtain ⟨u2, hu2, hc2⟩ := from_u64_scalar_sound_2safety b hb
    obtain ⟨u3, hu3, hc3⟩ := sub_scalar_32_sound_2safety u1 u2 hc1 hc2
    obtain ⟨res, hres, hc⟩ := into_u64_scalar_sound_2safety u3 hc3
    rw [bind_ok_eq hu1, bind_ok_eq hu2, bind_ok_eq hu3]
    exact ⟨res, hres, hc⟩
  | mul32  =>
    dsimp only [verifier.ScalarAluOp.eval, evalAlu32, ↓reduceIte]
    obtain ⟨u1, hu1, hc1⟩ := from_u64_scalar_sound_2safety a ha
    obtain ⟨u2, hu2, hc2⟩ := from_u64_scalar_sound_2safety b hb
    obtain ⟨u3, hu3, hc3⟩ := mul_scalar_32_sound_2safety u1 u2 hc1 hc2
    obtain ⟨res, hres, hc⟩ := into_u64_scalar_sound_2safety u3 hc3
    rw [bind_ok_eq hu1, bind_ok_eq hu2, bind_ok_eq hu3]
    exact ⟨res, hres, hc⟩
  | div32  =>
    dsimp only [verifier.ScalarAluOp.eval, evalAlu32, ↓reduceIte]
    obtain ⟨u1, hu1, hc1⟩ := from_u64_scalar_sound_2safety a ha
    obtain ⟨u2, hu2, hc2⟩ := from_u64_scalar_sound_2safety b hb
    obtain ⟨u3, hu3, hc3⟩ := div_scalar_32_sound_2safety u1 u2 hc1 hc2
    obtain ⟨res, hres, hc⟩ := into_u64_scalar_sound_2safety u3 hc3
    rw [bind_ok_eq hu1, bind_ok_eq hu2, bind_ok_eq hu3]
    exact ⟨res, hres, hc⟩
  | rem32  =>
    dsimp only [verifier.ScalarAluOp.eval, evalAlu32, ↓reduceIte]
    obtain ⟨u1, hu1, hc1⟩ := from_u64_scalar_sound_2safety a ha
    obtain ⟨u2, hu2, hc2⟩ := from_u64_scalar_sound_2safety b hb
    obtain ⟨u3, hu3, hc3⟩ := rem_scalar_32_sound_2safety u1 u2 hc1 hc2
    obtain ⟨res, hres, hc⟩ := into_u64_scalar_sound_2safety u3 hc3
    rw [bind_ok_eq hu1, bind_ok_eq hu2, bind_ok_eq hu3]
    exact ⟨res, hres, hc⟩
  | and32  =>
    dsimp only [verifier.ScalarAluOp.eval, evalAlu32, ↓reduceIte]
    obtain ⟨u1, hu1, hc1⟩ := from_u64_scalar_sound_2safety a ha
    obtain ⟨u2, hu2, hc2⟩ := from_u64_scalar_sound_2safety b hb
    obtain ⟨u3, hu3, hc3⟩ := bitand_scalar_32_sound_2safety u1 u2 hc1 hc2
    obtain ⟨res, hres, hc⟩ := into_u64_scalar_sound_2safety u3 hc3
    rw [bind_ok_eq hu1, bind_ok_eq hu2, bind_ok_eq hu3]
    exact ⟨res, hres, hc⟩
  | or32   =>
    dsimp only [verifier.ScalarAluOp.eval, evalAlu32, ↓reduceIte]
    obtain ⟨u1, hu1, hc1⟩ := from_u64_scalar_sound_2safety a ha
    obtain ⟨u2, hu2, hc2⟩ := from_u64_scalar_sound_2safety b hb
    obtain ⟨u3, hu3, hc3⟩ := bitor_scalar_32_sound_2safety u1 u2 hc1 hc2
    obtain ⟨res, hres, hc⟩ := into_u64_scalar_sound_2safety u3 hc3
    rw [bind_ok_eq hu1, bind_ok_eq hu2, bind_ok_eq hu3]
    exact ⟨res, hres, hc⟩
  | xor32  =>
    dsimp only [verifier.ScalarAluOp.eval, evalAlu32, ↓reduceIte]
    obtain ⟨u1, hu1, hc1⟩ := from_u64_scalar_sound_2safety a ha
    obtain ⟨u2, hu2, hc2⟩ := from_u64_scalar_sound_2safety b hb
    obtain ⟨u3, hu3, hc3⟩ := bitxor_scalar_32_sound_2safety u1 u2 hc1 hc2
    obtain ⟨res, hres, hc⟩ := into_u64_scalar_sound_2safety u3 hc3
    rw [bind_ok_eq hu1, bind_ok_eq hu2, bind_ok_eq hu3]
    exact ⟨res, hres, hc⟩
  | lsh32  =>
    dsimp only [verifier.ScalarAluOp.eval, evalAlu32, ↓reduceIte]
    obtain ⟨u1, hu1, hc1⟩ := from_u64_scalar_sound_2safety a ha
    obtain ⟨u2, hu2, hc2⟩ := from_u64_scalar_sound_2safety b hb
    obtain ⟨u3, hu3, hc3⟩ := shl_scalar_32_sound_2safety u1 u2 hc1 hc2
    obtain ⟨res, hres, hc⟩ := into_u64_scalar_sound_2safety u3 hc3
    rw [bind_ok_eq hu1, bind_ok_eq hu2, bind_ok_eq hu3]
    exact ⟨res, hres, hc⟩
  | rsh32  =>
    dsimp only [verifier.ScalarAluOp.eval, evalAlu32, ↓reduceIte]
    obtain ⟨u1, hu1, hc1⟩ := from_u64_scalar_sound_2safety a ha
    obtain ⟨u2, hu2, hc2⟩ := from_u64_scalar_sound_2safety b hb
    obtain ⟨u3, hu3, hc3⟩ := shr_scalar_32_sound_2safety u1 u2 hc1 hc2
    obtain ⟨res, hres, hc⟩ := into_u64_scalar_sound_2safety u3 hc3
    rw [bind_ok_eq hu1, bind_ok_eq hu2, bind_ok_eq hu3]
    exact ⟨res, hres, hc⟩
  | arsh32 =>
    dsimp only [verifier.ScalarAluOp.eval, evalAlu32, ↓reduceIte]
    obtain ⟨u1, hu1, hc1⟩ := from_u64_scalar_sound_2safety a ha
    obtain ⟨u2, hu2, hc2⟩ := from_u64_scalar_sound_2safety b hb
    obtain ⟨u3, hu3, hc3⟩ := ashr_32_sound_2safety u1 u2 hc1 hc2
    obtain ⟨res, hres, hc⟩ := into_u64_scalar_sound_2safety u3 hc3
    rw [bind_ok_eq hu1, bind_ok_eq hu2, bind_ok_eq hu3]
    exact ⟨res, hres, hc⟩

theorem ScalarAluOp_eval_neg_sound_2safety
    (is64 : Bool) (a b : U64ScalarValueData) {x₁ x₂ : BitVec 64}
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) :
    ∃ res, verifier.ScalarAluOp.eval (if is64 then .Neg64 else .Neg32) a b = ok res ∧
      ConcretizesScalar64Rel2 res (evalNeg is64 x₁) (evalNeg is64 x₂) := by
  cases is64 with
  | true =>
    dsimp only [↓reduceIte, verifier.ScalarAluOp.eval]
    exact neg_scalar_64_sound_2safety a ha
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte]
    dsimp only [verifier.ScalarAluOp.eval, evalNeg]
    simp only [Bool.false_eq_true, ↓reduceIte]
    obtain ⟨u1, hu1, hc1⟩ := from_u64_scalar_sound_2safety a ha
    obtain ⟨u2, hu2, hc2⟩ := neg_scalar_32_sound_2safety u1 hc1
    obtain ⟨res, hres, hc⟩ := into_u64_scalar_sound_2safety u2 hc2
    rw [bind_ok_eq hu1, bind_ok_eq hu2]
    exact ⟨res, hres, hc⟩

/-! ## 3. `ComputationContext.apply_computation` Validity & Relational 2-Safety -/

private theorem evalAlu64Val_add_comm (layout : HostLayout) (arrayLen : MemoryId → BitVec 64)
    (v1 v2 : RuntimeVal) :
    evalAlu64Val layout arrayLen .add v1 v2 = evalAlu64Val layout arrayLen .add v2 v1 := by
  cases v1 <;> cases v2 <;> simp [evalAlu64Val, evalAlu64, BitVec.add_comm]

private theorem evalAlu64Val_plain (layout : HostLayout) (arrayLen : MemoryId → BitVec 64)
    {op : AluOp} (hne_mov : op ≠ .mov) (hne_add : op ≠ .add) (hne_sub : op ≠ .sub)
    (v1 v2 : RuntimeVal) :
    evalAlu64Val layout arrayLen op v1 v2 =
      .scalar (evalAlu64 op (v1.toBits layout arrayLen) (v2.toBits layout arrayLen)) := by
  cases op <;> first
    | exact absurd rfl hne_mov
    | exact absurd rfl hne_add
    | exact absurd rfl hne_sub
    | rfl

private theorem TypeRel2_scalar_of_concretizes
    {env : ExecEnv} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64}
    {d : U64ScalarValueData} {x₁ x₂ : BitVec 64}
    (hc : ConcretizesScalar64Rel2 d x₁ x₂) :
    TypeRel2 env ρ resActive isNullId arrayLen (.ScalarValue d) (.scalar x₁) (.scalar x₂) := by
  by_cases hd : d = uninitScalar64
  · simp [TypeRel2, hd]
  · simp only [TypeRel2, hd, ↓reduceIte]
    exact ⟨x₁, x₂, rfl, rfl, hc⟩

private theorem pureUnwrapReleasable_not_releasable {env : ExecEnv} {vCounter : Nat}
    {op : verifier.Type} (h : TypeValid env vCounter op) :
    ∀ r i, pureUnwrapReleasable op ≠ .Releasable r i := by
  intro r i heq
  cases op with
  | Releasable relId inner =>
    dsimp [pureUnwrapReleasable] at heq
    subst heq
    exact h
  | _ => cases heq

private theorem bpf_max_var_off_le_u32_max {i : Std.U64}
    (h : ¬ (i > 536870912#u64)) : i.bv ≤ 0xFFFF_FFFF#64 := by
  have hle : i.bv ≤ (536870912#u64).bv := by
    rw [u64_gt_iff_bv] at h
    exact BitVec.not_lt.mp h
  have hbound : (536870912#u64).bv ≤ 0xFFFF_FFFF#64 := by decide
  exact BitVec.le_trans hle hbound

private theorem max_packet_offset_le {i : Std.U64}
    (h : ¬ (i > 65535#u64)) : i.bv ≤ MAX_PACKET_OFFSET := by
  rw [u64_gt_iff_bv] at h
  exact BitVec.not_lt.mp h

private def isSwapPtrType : verifier.Type → Prop
  | .PtrToStack _ | .PtrToMemory _ _ _ _ | .PtrToStruct _ _ _ | .PtrToArray _ _ => True
  | _ => False

private theorem apply_computation_noswap_ok_valid_and_rel2
    {env : ExecEnv} {vCounter : Nat} {ctx : verifier.ComputationContext}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64} {layout₁ layout₂ : HostLayout}
    {is64 : Bool} {op : AluOp} {alu_type : verifier.AluType} {alu_op : verifier.ScalarAluOp}
    {op1 op2 resTy : verifier.Type} {v₁ v₂ w₁ w₂ : RuntimeVal}
    (hctx : CompCtxValid env vCounter ctx)
    (hop1_valid : TypeValid env vCounter op1)
    (hop2_valid : TypeValid env vCounter op2)
    (hm : AluOpMatches is64 op alu_type alu_op)
    (hrel1 : TypeRel2 env ρ resActive isNullId arrayLen op1 v₁ v₂)
    (hrel2 : TypeRel2 env ρ resActive isNullId arrayLen op2 w₁ w₂)
    (hnoswap : ¬ ((∃ d, pureUnwrapReleasable op1 = .ScalarValue d) ∧
                  alu_type = .Add ∧ isSwapPtrType (pureUnwrapReleasable op2)))
    (hok : verifier.ComputationContext.apply_computation ctx op1 op2 alu_type alu_op = ok (.Ok resTy)) :
    TypeValid env vCounter resTy ∧
    TypeRel2 env ρ resActive isNullId arrayLen resTy
      (if is64 then evalAlu64Val layout₁ arrayLen op v₁ w₁
       else .scalar (evalAlu32 op (v₁.toBits layout₁ arrayLen) (w₁.toBits layout₁ arrayLen)))
      (if is64 then evalAlu64Val layout₂ arrayLen op v₂ w₂
       else .scalar (evalAlu32 op (v₂.toBits layout₂ arrayLen) (w₂.toBits layout₂ arrayLen))) := by
  rw [verifier.ComputationContext.apply_computation.eq_def] at hok
  obtain ⟨r1, hr1, hr1_iff⟩ := Type_inner_spec hctx.2.2.2 op1
  rw [bind_ok_eq hr1] at hok
  cases r1 with
  | Err _ => cases ok_inj hok
  | Ok t =>
    dsimp only at hok
    have ht_eq : t = pureUnwrapReleasable op1 := ((hr1_iff t).mp rfl).1
    obtain ⟨r2, hr2, hr2_iff⟩ := Type_inner_spec hctx.2.2.2 op2
    rw [bind_ok_eq hr2] at hok
    cases r2 with
    | Err _ => cases ok_inj hok
    | Ok t1 =>
      dsimp only at hok
      have ht1_eq : t1 = pureUnwrapReleasable op2 := ((hr2_iff t1).mp rfl).1
      have ht_valid : TypeValid env vCounter t := ht_eq ▸ Type_unwrap_releasable_valid hop1_valid
      have ht1_valid : TypeValid env vCounter t1 := ht1_eq ▸ Type_unwrap_releasable_valid hop2_valid
      have ht_not_rel : ∀ r i, t ≠ .Releasable r i :=
        ht_eq ▸ pureUnwrapReleasable_not_releasable hop1_valid
      have ht1_not_rel : ∀ r i, t1 ≠ .Releasable r i :=
        ht1_eq ▸ pureUnwrapReleasable_not_releasable hop2_valid
      have closeUninit : resTy = .ScalarValue uninitScalar64 →
          TypeValid env vCounter resTy ∧
          TypeRel2 env ρ resActive isNullId arrayLen resTy
            (if is64 then evalAlu64Val layout₁ arrayLen op v₁ w₁
             else .scalar (evalAlu32 op (v₁.toBits layout₁ arrayLen) (w₁.toBits layout₁ arrayLen)))
            (if is64 then evalAlu64Val layout₂ arrayLen op v₂ w₂
             else .scalar (evalAlu32 op (v₂.toBits layout₂ arrayLen) (w₂.toBits layout₂ arrayLen))) := by
        rintro rfl
        exact ⟨uninitScalar64_valid, by simp [TypeRel2]⟩
      cases ht_case : t with
      | Releasable r i => exact absurd ht_case (ht_not_rel r i)
      | ScalarValueParameter | ConstPtrToMapParameter _ _ | MapKeyParameter _
      | MapValueParameter _ | MemoryParameter _ _ _ | NullOrParameter _
      | StructParameter _ | ContextParameter _ | ReleasableParameter _ _
      | ReleaseParameter _ | AnyParameter =>
        rw [ht_case] at ht_valid; exact False.elim ht_valid
      | ConstPtrToMap _ _ | NullOr _ _ =>
        rw [ht_case] at hok
        cases ht1_case : t1 <;> rw [ht1_case] at hok <;>
          (try cases alu_type) <;>
          simp only [Type_UNINITIALIZED_eq, bind_ok] at hok <;>
          exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
      | ScalarValue data1 =>
        rw [ht_case] at hok ht_valid
        have hop1_sv : op1 = .ScalarValue data1 := by
          cases op1 with
          | Releasable _ inner =>
            cases inner <;> try exact False.elim hop1_valid
            all_goals simp [pureUnwrapReleasable, ht_case] at ht_eq
          | _ => simpa [pureUnwrapReleasable, ht_case] using ht_eq.symm
        subst hop1_sv
        cases ht1_case : t1 with
        | Releasable r i => exact absurd ht1_case (ht1_not_rel r i)
        | ScalarValueParameter | ConstPtrToMapParameter _ _ | MapKeyParameter _
        | MapValueParameter _ | MemoryParameter _ _ _ | NullOrParameter _
        | StructParameter _ | ContextParameter _ | ReleasableParameter _ _
        | ReleaseParameter _ | AnyParameter =>
          rw [ht1_case] at ht1_valid; exact False.elim ht1_valid
        | ConstPtrToMap _ _ | PtrToEndArray _ | NullOr _ _ =>
          rw [ht1_case] at hok
          cases alu_type <;>
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok <;>
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
        | PtrToStack _ | PtrToMemory _ _ _ _ | PtrToStruct _ _ _ | PtrToArray _ _ =>
          rw [ht1_case] at hok
          cases halu : alu_type with
          | Plain | Sub =>
            rw [halu] at hok
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Add =>
            exfalso
            apply hnoswap
            refine ⟨⟨data1, rfl⟩, halu, ?_⟩
            rw [← ht1_eq, ht1_case]
            trivial
        | ScalarValue data2 =>
          rw [ht1_case] at hok ht1_valid
          have hop2_sv : op2 = .ScalarValue data2 := by
            cases op2 with
            | Releasable _ inner =>
              cases inner <;> try exact False.elim hop2_valid
              all_goals simp [pureUnwrapReleasable, ht1_case] at ht1_eq
            | _ => simpa [pureUnwrapReleasable, ht1_case] using ht1_eq.symm
          subst hop2_sv
          dsimp only at hok
          rcases bind_eq_ok hok with ⟨usvd, heval, hok⟩
          cases core.result.Result.Ok.inj (ok_inj hok)
          refine ⟨ScalarAluOp_eval_valid ht_valid ht1_valid heval, ?_⟩
          cases his64 : is64 with
          | false =>
            simp only [Bool.false_eq_true, ↓reduceIte]
            rw [his64] at hm
            obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety hm
              (TypeRel2_scalar_toBits_rel2 (layout₁ := layout₁) (layout₂ := layout₂) hrel1)
              (TypeRel2_scalar_toBits_rel2 (layout₁ := layout₁) (layout₂ := layout₂) hrel2)
            simp only [Bool.false_eq_true, ↓reduceIte] at hc
            rw [heval] at hres; cases ok_inj hres
            exact TypeRel2_scalar_of_concretizes hc
          | true =>
            simp only [↓reduceIte]
            rw [his64] at hm
            by_cases h_uninit_addsub :
                (op = .add ∨ op = .sub) ∧ (data1 = uninitScalar64 ∨ data2 = uninitScalar64)
            · rcases h_uninit_addsub with ⟨hop_as, hu⟩
              have hd1_le := TypeRel2_scalar_range_nonempty hrel1
              have hd2_le := TypeRel2_scalar_range_nonempty hrel2
              have husvd : usvd = uninitScalar64 := by
                cases hm with
                | add64 =>
                  change U64ScalarValueData.add_scalar data1 data2 = ok usvd at heval
                  rw [add_scalar_uninit_eq hd1_le hd2_le hu] at heval
                  exact (ok_inj heval).symm
                | sub64 =>
                  change U64ScalarValueData.sub_scalar data1 data2 = ok usvd at heval
                  rw [sub_scalar_uninit_eq hd1_le hd2_le hu] at heval
                  exact (ok_inj heval).symm
                | _ => rcases hop_as with h | h <;> cases h
              subst husvd
              simp [TypeRel2]
            · by_cases hop_as : op = .add ∨ op = .sub
              · have hboth : data1 ≠ uninitScalar64 ∧ data2 ≠ uninitScalar64 := by
                  constructor <;> intro hc
                  · exact h_uninit_addsub ⟨hop_as, Or.inl hc⟩
                  · exact h_uninit_addsub ⟨hop_as, Or.inr hc⟩
                have hr1' := hrel1
                have hr2' := hrel2
                simp only [TypeRel2, hboth.1, hboth.2, ↓reduceIte] at hr1' hr2'
                rcases hr1' with ⟨x₁, x₂, rfl, rfl, hc1⟩
                rcases hr2' with ⟨y₁, y₂, rfl, rfl, hc2⟩
                obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety hm hc1 hc2
                simp only [↓reduceIte] at hc
                rw [heval] at hres; cases ok_inj hres
                rcases hop_as with rfl | rfl <;>
                  exact TypeRel2_scalar_of_concretizes hc
              · have hne_add : op ≠ .add := fun h => hop_as (Or.inl h)
                have hne_sub : op ≠ .sub := fun h => hop_as (Or.inr h)
                have hne_mov : op ≠ .mov := by intro h; subst h; cases hm
                rw [evalAlu64Val_plain layout₁ arrayLen hne_mov hne_add hne_sub,
                    evalAlu64Val_plain layout₂ arrayLen hne_mov hne_add hne_sub]
                obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety hm
                  (TypeRel2_scalar_toBits_rel2 (layout₁ := layout₁) (layout₂ := layout₂) hrel1)
                  (TypeRel2_scalar_toBits_rel2 (layout₁ := layout₁) (layout₂ := layout₂) hrel2)
                simp only [↓reduceIte] at hc
                rw [heval] at hres; cases ok_inj hres
                exact TypeRel2_scalar_of_concretizes hc
      | PtrToStack x =>
        rw [ht_case] at hok ht_valid
        have hop1_st : op1 = .PtrToStack x := by
          cases op1 with
          | Releasable _ inner =>
            cases inner <;> try exact False.elim hop1_valid
            all_goals simp [pureUnwrapReleasable, ht_case] at ht_eq
          | _ => simpa [pureUnwrapReleasable, ht_case] using ht_eq.symm
        subst hop1_st
        cases ht1_case : t1 with
        | Releasable r i => exact absurd ht1_case (ht1_not_rel r i)
        | ScalarValueParameter | ConstPtrToMapParameter _ _ | MapKeyParameter _
        | MapValueParameter _ | MemoryParameter _ _ _ | NullOrParameter _
        | StructParameter _ | ContextParameter _ | ReleasableParameter _ _
        | ReleaseParameter _ | AnyParameter =>
          rw [ht1_case] at ht1_valid; exact False.elim ht1_valid
        | ConstPtrToMap _ _ | PtrToMemory _ _ _ _ | PtrToStruct _ _ _
        | PtrToArray _ _ | PtrToEndArray _ | NullOr _ _ =>
          rw [ht1_case] at hok
          (try cases alu_type) <;>
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok <;>
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
        | ScalarValue data =>
          rw [ht1_case] at hok ht1_valid
          have hop2_sv : op2 = .ScalarValue data := by
            cases op2 with
            | Releasable _ inner =>
              cases inner <;> try exact False.elim hop2_valid
              all_goals simp [pureUnwrapReleasable, ht1_case] at ht1_eq
            | _ => simpa [pureUnwrapReleasable, ht1_case] using ht1_eq.symm
          subst hop2_sv
          cases halu : alu_type with
          | Plain =>
            rw [halu] at hok
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, Bool.false_eq_true, ↓reduceIte, Type_UNINITIALIZED_eq] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Add | Sub =>
            rw [halu] at hok hm
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, ↓reduceIte] at hok
            unfold U64ScalarValueData.is_fully_initialized at hok
            simp only [bind_ok] at hok
            by_cases hd_init : data.unwritten_mask = 0#u64
            · simp only [hd_init, decide_true, ↓reduceIte, StackOffset_reg_spec, bind_ok] at hok
              rcases bind_eq_ok hok with ⟨offset, heval, hok⟩
              unfold U64ScalarValueData.max at hok
              simp only [BPF_MAX_VAR_OFF_eq, bind_ok] at hok
              by_cases hgt : offset.urange.max > 536870912#u64
              · simp [hgt] at hok
              · simp only [hgt, ↓reduceIte] at hok
                cases core.result.Result.Ok.inj (ok_inj hok)
                have hd_ne : data ≠ uninitScalar64 :=
                  fun hc => uninitScalar64_unwritten_ne_zero (hc ▸ hd_init)
                rcases hrel1 with ⟨hx_off, o, hconc_o, rfl, rfl⟩
                simp only [TypeRel2, hd_ne, ↓reduceIte] at hrel2
                rcases hrel2 with ⟨y₁, y₂, rfl, rfl, hconc_y⟩
                have hy_eq := fully_initialized_eq_of_rel2_64 hd_init hconc_y
                subst hy_eq
                have hop_as : alu_op = .Add64 ∨ alu_op = .Sub64 := by
                  cases hm <;> simp
                have hoff_valid : PtrOffsetValid 0xFFFF_FFFF#64 offset :=
                  ⟨ScalarAluOp_eval_valid hx_off.1 ht1_valid heval,
                   add_or_sub_64_unwritten_zero hop_as hx_off.2.1
                     ((u64_eq_iff_bv _ _).mp hd_init) heval,
                   bpf_max_var_off_le_u32_max hgt⟩
                obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety hm hconc_o hconc_y
                rw [heval] at hres; cases ok_inj hres
                cases hm <;>
                  exact ⟨hoff_valid, hoff_valid, _, hc, rfl, rfl⟩
            · simp [hd_init] at hok
        | PtrToStack x2 =>
          rw [ht1_case] at hok ht1_valid
          have hop2_st : op2 = .PtrToStack x2 := by
            cases op2 with
            | Releasable _ inner =>
              cases inner <;> try exact False.elim hop2_valid
              all_goals simp [pureUnwrapReleasable, ht1_case] at ht1_eq
            | _ => simpa [pureUnwrapReleasable, ht1_case] using ht1_eq.symm
          subst hop2_st
          cases halu : alu_type with
          | Plain | Add =>
            rw [halu] at hok
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Sub =>
            rw [halu] at hok hm
            dsimp only at hok
            simp only [StackOffset_reg_spec, bind_ok] at hok
            rcases bind_eq_ok hok with ⟨usvd2, heval, hok⟩
            cases core.result.Result.Ok.inj (ok_inj hok)
            rcases hrel1 with ⟨hx_off, o1, hc1, rfl, rfl⟩
            rcases hrel2 with ⟨hx2_off, o2, hc2, rfl, rfl⟩
            obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety hm hc1 hc2
            rw [heval] at hres; cases ok_inj hres
            cases hm
            exact ⟨ScalarAluOp_eval_valid hx_off.1 hx2_off.1 heval,
              TypeRel2_scalar_of_concretizes hc⟩
      | PtrToMemory id1 x sz orig =>
        rw [ht_case] at hok ht_valid
        cases ht1_case : t1 with
        | Releasable r i => exact absurd ht1_case (ht1_not_rel r i)
        | ScalarValueParameter | ConstPtrToMapParameter _ _ | MapKeyParameter _
        | MapValueParameter _ | MemoryParameter _ _ _ | NullOrParameter _
        | StructParameter _ | ContextParameter _ | ReleasableParameter _ _
        | ReleaseParameter _ | AnyParameter =>
          rw [ht1_case] at ht1_valid; exact False.elim ht1_valid
        | ConstPtrToMap _ _ | PtrToStack _ | PtrToStruct _ _ _
        | PtrToArray _ _ | PtrToEndArray _ | NullOr _ _ =>
          rw [ht1_case] at hok
          (try cases alu_type) <;>
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok <;>
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
        | ScalarValue data =>
          rw [ht1_case] at hok ht1_valid
          have hop2_sv : op2 = .ScalarValue data := by
            cases op2 with
            | Releasable _ inner =>
              cases inner <;> try exact False.elim hop2_valid
              all_goals simp [pureUnwrapReleasable, ht1_case] at ht1_eq
            | _ => simpa [pureUnwrapReleasable, ht1_case] using ht1_eq.symm
          subst hop2_sv
          cases halu : alu_type with
          | Plain =>
            rw [halu] at hok
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, Bool.false_eq_true, ↓reduceIte, Type_UNINITIALIZED_eq] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Add | Sub =>
            rw [halu] at hok hm
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, ↓reduceIte] at hok
            unfold U64ScalarValueData.is_fully_initialized at hok
            simp only [bind_ok] at hok
            by_cases hd_init : data.unwritten_mask = 0#u64
            · simp only [hd_init, decide_true, ↓reduceIte] at hok
              rcases bind_eq_ok hok with ⟨offset, heval, hok⟩
              unfold U64ScalarValueData.max at hok
              simp only [BPF_MAX_VAR_OFF_1_eq, bind_ok] at hok
              by_cases hgt : offset.urange.max > 536870912#u64
              · simp [hgt] at hok
              · simp only [hgt, ↓reduceIte, Type_with_offset_spec, bind_ok] at hok
                cases core.result.Result.Ok.inj (ok_inj hok)
                have hd_ne : data ≠ uninitScalar64 :=
                  fun hc => uninitScalar64_unwritten_ne_zero (hc ▸ hd_init)
                simp only [TypeRel2, hd_ne, ↓reduceIte] at hrel2
                rcases hrel2 with ⟨y₁, y₂, rfl, rfl, hconc_y⟩
                have hy_eq := fully_initialized_eq_of_rel2_64 hd_init hconc_y
                subst hy_eq
                have hop_as : alu_op = .Add64 ∨ alu_op = .Sub64 := by
                  cases hm <;> simp
                have hoff_valid : PtrOffsetValid 0xFFFF_FFFF#64 offset :=
                  ⟨ScalarAluOp_eval_valid ht_valid.2.1 ht1_valid heval,
                   add_or_sub_64_unwritten_zero hop_as ht_valid.2.2.1
                     ((u64_eq_iff_bv _ _).mp hd_init) heval,
                   bpf_max_var_off_le_u32_max hgt⟩
                cases op1 with
                | Releasable relId inner =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq; subst ht_eq
                  rcases hrel1 with ⟨hact, _, o, hconc_o, rfl, rfl⟩
                  obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety hm hconc_o hconc_y
                  rw [heval] at hres; cases ok_inj hres
                  cases hm <;>
                    exact ⟨⟨hop1_valid.1, ht_valid.1, hoff_valid⟩,
                      hact, hoff_valid, _, hc, rfl, rfl⟩
                | PtrToMemory mid1 mx msz morig =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq
                  cases ht_eq
                  rcases hrel1 with ⟨_, o, hconc_o, rfl, rfl⟩
                  obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety hm hconc_o hconc_y
                  rw [heval] at hres; cases ok_inj hres
                  cases hm <;>
                    exact ⟨⟨ht_valid.1, hoff_valid⟩, hoff_valid, _, hc, rfl, rfl⟩
                | _ =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq; cases ht_eq
            · simp [hd_init] at hok
        | PtrToMemory id2 x2 sz2 orig2 =>
          rw [ht1_case] at hok ht1_valid
          cases halu : alu_type with
          | Plain | Add =>
            rw [halu] at hok
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Sub =>
            rw [halu] at hok hm
            dsimp only at hok
            rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at hok
            by_cases hid : id1 = id2
            · subst hid
              simp only [decide_true, ↓reduceIte] at hok
              rcases bind_eq_ok hok with ⟨usvd, heval, hok⟩
              cases core.result.Result.Ok.inj (ok_inj hok)
              have hex1 : ∃ resOpt1 o1, ConcretizesScalar64Rel2 x o1 o1 ∧
                  v₁ = .memPtr (renameMemId ρ id1) (MemoryOrigin.ofCore orig) resOpt1 sz.bv o1 ∧
                  v₂ = .memPtr (renameMemId ρ id1) (MemoryOrigin.ofCore orig) resOpt1 sz.bv o1 := by
                cases op1 with
                | Releasable relId inner =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq; subst ht_eq
                  rcases hrel1 with ⟨_, _, o1, hc1, rfl, rfl⟩
                  exact ⟨some (renameMemId ρ relId), o1, hc1, rfl, rfl⟩
                | PtrToMemory _ _ _ _ =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq; cases ht_eq
                  rcases hrel1 with ⟨_, o1, hc1, rfl, rfl⟩
                  exact ⟨none, o1, hc1, rfl, rfl⟩
                | _ =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq; cases ht_eq
              have hex2 : ∃ resOpt2 o2, ConcretizesScalar64Rel2 x2 o2 o2 ∧
                  w₁ = .memPtr (renameMemId ρ id1) (MemoryOrigin.ofCore orig2) resOpt2 sz2.bv o2 ∧
                  w₂ = .memPtr (renameMemId ρ id1) (MemoryOrigin.ofCore orig2) resOpt2 sz2.bv o2 := by
                cases op2 with
                | Releasable relId inner =>
                  dsimp [pureUnwrapReleasable] at ht1_eq
                  rw [ht1_case] at ht1_eq; subst ht1_eq
                  rcases hrel2 with ⟨_, _, o2, hc2, rfl, rfl⟩
                  exact ⟨some (renameMemId ρ relId), o2, hc2, rfl, rfl⟩
                | PtrToMemory _ _ _ _ =>
                  dsimp [pureUnwrapReleasable] at ht1_eq
                  rw [ht1_case] at ht1_eq; cases ht1_eq
                  rcases hrel2 with ⟨_, o2, hc2, rfl, rfl⟩
                  exact ⟨none, o2, hc2, rfl, rfl⟩
                | _ =>
                  dsimp [pureUnwrapReleasable] at ht1_eq
                  rw [ht1_case] at ht1_eq; cases ht1_eq
              rcases hex1 with ⟨resOpt1, o1, hc1, rfl, rfl⟩
              rcases hex2 with ⟨resOpt2, o2, hc2, rfl, rfl⟩
              obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety hm hc1 hc2
              rw [heval] at hres; cases ok_inj hres
              cases hm
              simp only [↓reduceIte, evalAlu64Val, beq_self_eq_true]
              exact ⟨ScalarAluOp_eval_valid ht_valid.2.1 ht1_valid.2.1 heval,
                TypeRel2_scalar_of_concretizes hc⟩
            · simp only [hid, decide_false, Bool.false_eq_true, ↓reduceIte,
                Type_UNINITIALIZED_eq, bind_ok] at hok
              exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
      | PtrToStruct id1 x sd =>
        rw [ht_case] at hok ht_valid
        cases ht1_case : t1 with
        | Releasable r i => exact absurd ht1_case (ht1_not_rel r i)
        | ScalarValueParameter | ConstPtrToMapParameter _ _ | MapKeyParameter _
        | MapValueParameter _ | MemoryParameter _ _ _ | NullOrParameter _
        | StructParameter _ | ContextParameter _ | ReleasableParameter _ _
        | ReleaseParameter _ | AnyParameter =>
          rw [ht1_case] at ht1_valid; exact False.elim ht1_valid
        | ConstPtrToMap _ _ | PtrToStack _ | PtrToMemory _ _ _ _
        | PtrToArray _ _ | PtrToEndArray _ | NullOr _ _ =>
          rw [ht1_case] at hok
          (try cases alu_type) <;>
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok <;>
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
        | ScalarValue data =>
          rw [ht1_case] at hok ht1_valid
          have hop2_sv : op2 = .ScalarValue data := by
            cases op2 with
            | Releasable _ inner =>
              cases inner <;> try exact False.elim hop2_valid
              all_goals simp [pureUnwrapReleasable, ht1_case] at ht1_eq
            | _ => simpa [pureUnwrapReleasable, ht1_case] using ht1_eq.symm
          subst hop2_sv
          cases halu : alu_type with
          | Plain =>
            rw [halu] at hok
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, Bool.false_eq_true, ↓reduceIte, Type_UNINITIALIZED_eq] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Add | Sub =>
            rw [halu] at hok hm
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, ↓reduceIte] at hok
            unfold U64ScalarValueData.is_fully_initialized at hok
            simp only [bind_ok] at hok
            by_cases hd_init : data.unwritten_mask = 0#u64
            · simp only [hd_init, decide_true, ↓reduceIte] at hok
              rcases bind_eq_ok hok with ⟨offset, heval, hok⟩
              unfold U64ScalarValueData.max at hok
              simp only [BPF_MAX_VAR_OFF_1_eq, bind_ok] at hok
              by_cases hgt : offset.urange.max > 536870912#u64
              · simp [hgt] at hok
              · simp only [hgt, ↓reduceIte, Type_with_offset_spec, bind_ok] at hok
                cases core.result.Result.Ok.inj (ok_inj hok)
                have hd_ne : data ≠ uninitScalar64 :=
                  fun hc => uninitScalar64_unwritten_ne_zero (hc ▸ hd_init)
                simp only [TypeRel2, hd_ne, ↓reduceIte] at hrel2
                rcases hrel2 with ⟨y₁, y₂, rfl, rfl, hconc_y⟩
                have hy_eq := fully_initialized_eq_of_rel2_64 hd_init hconc_y
                subst hy_eq
                have hop_as : alu_op = .Add64 ∨ alu_op = .Sub64 := by
                  cases hm <;> simp
                have hoff_valid : PtrOffsetValid 0xFFFF_FFFF#64 offset :=
                  ⟨ScalarAluOp_eval_valid ht_valid.2.1.1 ht1_valid heval,
                   add_or_sub_64_unwritten_zero hop_as ht_valid.2.1.2.1
                     ((u64_eq_iff_bv _ _).mp hd_init) heval,
                   bpf_max_var_off_le_u32_max hgt⟩
                cases op1 with
                | Releasable relId inner =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq; subst ht_eq
                  rcases hrel1 with ⟨hact, _, hsd, o, hconc_o, rfl, rfl⟩
                  obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety hm hconc_o hconc_y
                  rw [heval] at hres; cases ok_inj hres
                  cases hm <;>
                    exact ⟨⟨hop1_valid.1, ht_valid.1, hoff_valid, ht_valid.2.2⟩,
                      hact, hoff_valid, hsd, _, hc, rfl, rfl⟩
                | PtrToStruct _ _ _ =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq; cases ht_eq
                  rcases hrel1 with ⟨_, hsd, o, hconc_o, rfl, rfl⟩
                  obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety hm hconc_o hconc_y
                  rw [heval] at hres; cases ok_inj hres
                  cases hm <;>
                    exact ⟨⟨ht_valid.1, hoff_valid, ht_valid.2.2⟩,
                      hoff_valid, hsd, _, hc, rfl, rfl⟩
                | _ =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq; cases ht_eq
            · simp [hd_init] at hok
        | PtrToStruct id2 x2 sd2 =>
          rw [ht1_case] at hok ht1_valid
          cases halu : alu_type with
          | Plain | Add =>
            rw [halu] at hok
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Sub =>
            rw [halu] at hok hm
            dsimp only at hok
            rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at hok
            by_cases hid : id1 = id2
            · subst hid
              simp only [decide_true, ↓reduceIte] at hok
              rcases bind_eq_ok hok with ⟨usvd, heval, hok⟩
              cases core.result.Result.Ok.inj (ok_inj hok)
              have hex1 : ∃ resOpt1 o1, ConcretizesScalar64Rel2 x o1 o1 ∧
                  v₁ = .structPtr (renameMemId ρ id1) resOpt1 o1 ∧
                  v₂ = .structPtr (renameMemId ρ id1) resOpt1 o1 := by
                cases op1 with
                | Releasable relId inner =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq; subst ht_eq
                  rcases hrel1 with ⟨_, _, _, o1, hc1, rfl, rfl⟩
                  exact ⟨some (renameMemId ρ relId), o1, hc1, rfl, rfl⟩
                | PtrToStruct _ _ _ =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq; cases ht_eq
                  rcases hrel1 with ⟨_, _, o1, hc1, rfl, rfl⟩
                  exact ⟨none, o1, hc1, rfl, rfl⟩
                | _ =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq; cases ht_eq
              have hex2 : ∃ resOpt2 o2, ConcretizesScalar64Rel2 x2 o2 o2 ∧
                  w₁ = .structPtr (renameMemId ρ id1) resOpt2 o2 ∧
                  w₂ = .structPtr (renameMemId ρ id1) resOpt2 o2 := by
                cases op2 with
                | Releasable relId inner =>
                  dsimp [pureUnwrapReleasable] at ht1_eq
                  rw [ht1_case] at ht1_eq; subst ht1_eq
                  rcases hrel2 with ⟨_, _, _, o2, hc2, rfl, rfl⟩
                  exact ⟨some (renameMemId ρ relId), o2, hc2, rfl, rfl⟩
                | PtrToStruct _ _ _ =>
                  dsimp [pureUnwrapReleasable] at ht1_eq
                  rw [ht1_case] at ht1_eq; cases ht1_eq
                  rcases hrel2 with ⟨_, _, o2, hc2, rfl, rfl⟩
                  exact ⟨none, o2, hc2, rfl, rfl⟩
                | _ =>
                  dsimp [pureUnwrapReleasable] at ht1_eq
                  rw [ht1_case] at ht1_eq; cases ht1_eq
              rcases hex1 with ⟨resOpt1, o1, hc1, rfl, rfl⟩
              rcases hex2 with ⟨resOpt2, o2, hc2, rfl, rfl⟩
              obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety hm hc1 hc2
              rw [heval] at hres; cases ok_inj hres
              cases hm
              simp only [↓reduceIte, evalAlu64Val, beq_self_eq_true]
              exact ⟨ScalarAluOp_eval_valid ht_valid.2.1.1 ht1_valid.2.1.1 heval,
                TypeRel2_scalar_of_concretizes hc⟩
            · simp only [hid, decide_false, Bool.false_eq_true, ↓reduceIte,
                Type_UNINITIALIZED_eq, bind_ok] at hok
              exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
      | PtrToArray id1 x =>
        rw [ht_case] at hok ht_valid
        have hop1_arr : op1 = .PtrToArray id1 x := by
          cases op1 with
          | Releasable _ inner =>
            cases inner <;> try exact False.elim hop1_valid
            all_goals simp [pureUnwrapReleasable, ht_case] at ht_eq
          | _ => simpa [pureUnwrapReleasable, ht_case] using ht_eq.symm
        subst hop1_arr
        cases ht1_case : t1 with
        | Releasable r i => exact absurd ht1_case (ht1_not_rel r i)
        | ScalarValueParameter | ConstPtrToMapParameter _ _ | MapKeyParameter _
        | MapValueParameter _ | MemoryParameter _ _ _ | NullOrParameter _
        | StructParameter _ | ContextParameter _ | ReleasableParameter _ _
        | ReleaseParameter _ | AnyParameter =>
          rw [ht1_case] at ht1_valid; exact False.elim ht1_valid
        | ConstPtrToMap _ _ | PtrToStack _ | PtrToMemory _ _ _ _
        | PtrToStruct _ _ _ | NullOr _ _ =>
          rw [ht1_case] at hok
          (try cases alu_type) <;>
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok <;>
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
        | ScalarValue data =>
          rw [ht1_case] at hok ht1_valid
          have hop2_sv : op2 = .ScalarValue data := by
            cases op2 with
            | Releasable _ inner =>
              cases inner <;> try exact False.elim hop2_valid
              all_goals simp [pureUnwrapReleasable, ht1_case] at ht1_eq
            | _ => simpa [pureUnwrapReleasable, ht1_case] using ht1_eq.symm
          subst hop2_sv
          cases halu : alu_type with
          | Plain =>
            rw [halu] at hok
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, Bool.false_eq_true, ↓reduceIte, Type_UNINITIALIZED_eq] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Sub =>
            rw [halu] at hok hm
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, ↓reduceIte] at hok
            unfold U64ScalarValueData.is_fully_initialized at hok
            simp only [bind_ok] at hok
            by_cases hd_init : data.unwritten_mask = 0#u64
            · simp only [hd_init, decide_true, ↓reduceIte] at hok
              rcases bind_eq_ok hok with ⟨opt, hck, hok⟩
              cases opt with
              | none => cases ok_inj hok
              | some offset =>
                dsimp only at hok
                unfold U64ScalarValueData.max at hok
                simp only [verifier_MAX_PACKET_OFFSET_eq, bind_ok] at hok
                by_cases hgt : offset.urange.max > 65535#u64
                · simp [hgt] at hok
                · simp only [hgt, ↓reduceIte, Type_with_offset_spec, bind_ok] at hok
                  cases core.result.Result.Ok.inj (ok_inj hok)
                  have hd_ne : data ≠ uninitScalar64 :=
                    fun hc => uninitScalar64_unwritten_ne_zero (hc ▸ hd_init)
                  rcases hrel1 with ⟨hx_off, hlen, o, hconc_o, rfl, rfl⟩
                  simp only [TypeRel2, hd_ne, ↓reduceIte] at hrel2
                  rcases hrel2 with ⟨y₁, y₂, rfl, rfl, hconc_y⟩
                  have hy_eq := fully_initialized_eq_of_rel2_64 hd_init hconc_y
                  subst hy_eq
                  obtain ⟨opt', hck', hsnd⟩ := checked_sub_64_sound_2safety x data hconc_o hconc_y
                  rw [hck] at hck'; cases ok_inj hck'
                  obtain ⟨hno, _, hc⟩ := hsnd offset rfl
                  have hoff_valid : PtrOffsetValid MAX_PACKET_OFFSET offset :=
                    ⟨checked_sub_64_some_valid hck,
                     checked_sub_64_unwritten_zero hx_off.2.1 ((u64_eq_iff_bv _ _).mp hd_init) hck,
                     max_packet_offset_le hgt⟩
                  cases hm
                  have hno_b : (!BitVec.usubOverflow o y₁) = true := by simp [hno]
                  simp only [↓reduceIte, evalAlu64Val, hno_b]
                  exact ⟨⟨ht_valid.1, hoff_valid⟩, hoff_valid, hlen, o - y₁, hc, rfl, rfl⟩
            · simp [hd_init] at hok
          | Add =>
            rw [halu] at hok hm
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, ↓reduceIte] at hok
            unfold U64ScalarValueData.is_fully_initialized at hok
            simp only [bind_ok] at hok
            by_cases hd_init : data.unwritten_mask = 0#u64
            · simp only [hd_init, decide_true, ↓reduceIte] at hok
              rcases bind_eq_ok hok with ⟨opt, hck, hok⟩
              cases opt with
              | none => cases ok_inj hok
              | some offset =>
                dsimp only at hok
                unfold U64ScalarValueData.max at hok
                simp only [verifier_MAX_PACKET_OFFSET_eq, bind_ok] at hok
                by_cases hgt : offset.urange.max > 65535#u64
                · simp [hgt] at hok
                · simp only [hgt, ↓reduceIte, Type_with_offset_spec, bind_ok] at hok
                  cases core.result.Result.Ok.inj (ok_inj hok)
                  have hd_ne : data ≠ uninitScalar64 :=
                    fun hc => uninitScalar64_unwritten_ne_zero (hc ▸ hd_init)
                  rcases hrel1 with ⟨hx_off, hlen, o, hconc_o, rfl, rfl⟩
                  simp only [TypeRel2, hd_ne, ↓reduceIte] at hrel2
                  rcases hrel2 with ⟨y₁, y₂, rfl, rfl, hconc_y⟩
                  have hy_eq := fully_initialized_eq_of_rel2_64 hd_init hconc_y
                  subst hy_eq
                  obtain ⟨opt', hck', hsnd⟩ := checked_add_64_sound_2safety x data hconc_o hconc_y
                  rw [hck] at hck'; cases ok_inj hck'
                  obtain ⟨hno, _, hc⟩ := hsnd offset rfl
                  have hoff_valid : PtrOffsetValid MAX_PACKET_OFFSET offset :=
                    ⟨checked_add_64_some_valid hck,
                     checked_add_64_unwritten_zero hx_off.2.1 ((u64_eq_iff_bv _ _).mp hd_init) hck,
                     max_packet_offset_le hgt⟩
                  cases hm
                  have hno_b : (!BitVec.uaddOverflow o y₁) = true := by simp [hno]
                  simp only [↓reduceIte, evalAlu64Val, hno_b]
                  exact ⟨⟨ht_valid.1, hoff_valid⟩, hoff_valid, hlen, o + y₁, hc, rfl, rfl⟩
            · simp [hd_init] at hok
        | PtrToArray id2 x2 =>
          rw [ht1_case] at hok ht1_valid
          have hop2_arr : op2 = .PtrToArray id2 x2 := by
            cases op2 with
            | Releasable _ inner =>
              cases inner <;> try exact False.elim hop2_valid
              all_goals simp [pureUnwrapReleasable, ht1_case] at ht1_eq
            | _ => simpa [pureUnwrapReleasable, ht1_case] using ht1_eq.symm
          subst hop2_arr
          cases halu : alu_type with
          | Plain | Add =>
            rw [halu] at hok
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Sub =>
            rw [halu] at hok hm
            dsimp only at hok
            rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at hok
            by_cases hid : id1 = id2
            · subst hid
              simp only [decide_true, ↓reduceIte] at hok
              rcases bind_eq_ok hok with ⟨usvd, heval, hok⟩
              cases core.result.Result.Ok.inj (ok_inj hok)
              rcases hrel1 with ⟨hx_off, _, o1, hc1, rfl, rfl⟩
              rcases hrel2 with ⟨hx2_off, _, o2, hc2, rfl, rfl⟩
              obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_sound_2safety hm hc1 hc2
              rw [heval] at hres; cases ok_inj hres
              cases hm
              simp only [↓reduceIte, evalAlu64Val, beq_self_eq_true]
              exact ⟨ScalarAluOp_eval_valid hx_off.1 hx2_off.1 heval,
                TypeRel2_scalar_of_concretizes hc⟩
            · simp only [hid, decide_false, Bool.false_eq_true, ↓reduceIte,
                Type_UNINITIALIZED_eq, bind_ok] at hok
              exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
        | PtrToEndArray id2 =>
          rw [ht1_case] at hok ht1_valid
          have hop2_end : op2 = .PtrToEndArray id2 := by
            cases op2 with
            | Releasable _ inner =>
              cases inner <;> try exact False.elim hop2_valid
              all_goals simp [pureUnwrapReleasable, ht1_case] at ht1_eq
            | _ => simpa [pureUnwrapReleasable, ht1_case] using ht1_eq.symm
          subst hop2_end
          cases halu : alu_type with
          | Plain | Add =>
            rw [halu] at hok
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Sub =>
            rw [halu] at hok hm
            dsimp only at hok
            rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at hok
            by_cases hid : id1 = id2
            · subst hid
              simp only [decide_true, ↓reduceIte, Type_UNKNOWN_SCALAR_eq, bind_ok] at hok
              cases core.result.Result.Ok.inj (ok_inj hok)
              rcases hrel1 with ⟨_, _, o1, _, rfl, rfl⟩
              rcases hrel2 with ⟨_, rfl, rfl⟩
              cases hm
              simp only [↓reduceIte, evalAlu64Val, beq_self_eq_true]
              exact ⟨unknownWrittenScalar64_valid,
                TypeRel2_scalar_of_concretizes (concretizesScalar64Rel2_unknownWritten _)⟩
            · simp only [hid, decide_false, Bool.false_eq_true, ↓reduceIte,
                Type_UNINITIALIZED_eq, bind_ok] at hok
              exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
      | PtrToEndArray id1 =>
        rw [ht_case] at hok ht_valid
        have hop1_end : op1 = .PtrToEndArray id1 := by
          cases op1 with
          | Releasable _ inner =>
            cases inner <;> try exact False.elim hop1_valid
            all_goals simp [pureUnwrapReleasable, ht_case] at ht_eq
          | _ => simpa [pureUnwrapReleasable, ht_case] using ht_eq.symm
        subst hop1_end
        cases ht1_case : t1 with
        | Releasable r i => exact absurd ht1_case (ht1_not_rel r i)
        | ScalarValueParameter | ConstPtrToMapParameter _ _ | MapKeyParameter _
        | MapValueParameter _ | MemoryParameter _ _ _ | NullOrParameter _
        | StructParameter _ | ContextParameter _ | ReleasableParameter _ _
        | ReleaseParameter _ | AnyParameter =>
          rw [ht1_case] at ht1_valid; exact False.elim ht1_valid
        | ScalarValue _ | ConstPtrToMap _ _ | PtrToStack _
        | PtrToMemory _ _ _ _ | PtrToStruct _ _ _ | PtrToEndArray _ | NullOr _ _ =>
          rw [ht1_case] at hok
          (try cases alu_type) <;>
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok <;>
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
        | PtrToArray id2 x2 =>
          rw [ht1_case] at hok ht1_valid
          have hop2_arr : op2 = .PtrToArray id2 x2 := by
            cases op2 with
            | Releasable _ inner =>
              cases inner <;> try exact False.elim hop2_valid
              all_goals simp [pureUnwrapReleasable, ht1_case] at ht1_eq
            | _ => simpa [pureUnwrapReleasable, ht1_case] using ht1_eq.symm
          subst hop2_arr
          cases halu : alu_type with
          | Plain | Add =>
            rw [halu] at hok
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Sub =>
            rw [halu] at hok hm
            dsimp only at hok
            rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at hok
            by_cases hid : id1 = id2
            · subst hid
              simp only [decide_true, ↓reduceIte, Type_UNKNOWN_SCALAR_eq, bind_ok] at hok
              cases core.result.Result.Ok.inj (ok_inj hok)
              rcases hrel1 with ⟨_, rfl, rfl⟩
              rcases hrel2 with ⟨_, _, o2, _, rfl, rfl⟩
              cases hm
              simp only [↓reduceIte, evalAlu64Val, beq_self_eq_true]
              exact ⟨unknownWrittenScalar64_valid,
                TypeRel2_scalar_of_concretizes (concretizesScalar64Rel2_unknownWritten _)⟩
            · simp only [hid, decide_false, Bool.false_eq_true, ↓reduceIte,
                Type_UNINITIALIZED_eq, bind_ok] at hok
              exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm

theorem apply_computation_ok_valid_and_rel2
    {env : ExecEnv} {vCounter : Nat} {ctx : verifier.ComputationContext}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64} {layout₁ layout₂ : HostLayout}
    {is64 : Bool} {op : AluOp} {alu_type : verifier.AluType} {alu_op : verifier.ScalarAluOp}
    {op1 op2 resTy : verifier.Type} {v₁ v₂ w₁ w₂ : RuntimeVal}
    (hctx : CompCtxValid env vCounter ctx)
    (hop1_valid : TypeValid env vCounter op1)
    (hop2_valid : TypeValid env vCounter op2)
    (hm : AluOpMatches is64 op alu_type alu_op)
    (hrel1 : TypeRel2 env ρ resActive isNullId arrayLen op1 v₁ v₂)
    (hrel2 : TypeRel2 env ρ resActive isNullId arrayLen op2 w₁ w₂)
    (hok : verifier.ComputationContext.apply_computation ctx op1 op2 alu_type alu_op = ok (.Ok resTy)) :
    TypeValid env vCounter resTy ∧
    TypeRel2 env ρ resActive isNullId arrayLen resTy
      (if is64 then evalAlu64Val layout₁ arrayLen op v₁ w₁
       else .scalar (evalAlu32 op (v₁.toBits layout₁ arrayLen) (w₁.toBits layout₁ arrayLen)))
      (if is64 then evalAlu64Val layout₂ arrayLen op v₂ w₂
       else .scalar (evalAlu32 op (v₂.toBits layout₂ arrayLen) (w₂.toBits layout₂ arrayLen))) := by
  by_cases hswap : (∃ d, pureUnwrapReleasable op1 = .ScalarValue d) ∧
      alu_type = .Add ∧ isSwapPtrType (pureUnwrapReleasable op2)
  · rcases hswap with ⟨⟨d, hd⟩, halu, hptr⟩
    subst halu
    cases hm
    have hop1_sv : op1 = .ScalarValue d := by
      cases op1 with
      | Releasable _ inner =>
        cases inner <;> try exact False.elim hop1_valid
        all_goals simp [pureUnwrapReleasable] at hd
      | _ =>
        dsimp [pureUnwrapReleasable] at hd
        exact hd
    subst hop1_sv
    have hok' : verifier.ComputationContext.apply_computation ctx op2 (.ScalarValue d) .Add .Add64 =
        ok (.Ok resTy) := by
      rw [verifier.ComputationContext.apply_computation.eq_def] at hok
      obtain ⟨r1, hr1, hr1_iff⟩ := Type_inner_spec hctx.2.2.2 (.ScalarValue d)
      rw [bind_ok_eq hr1] at hok
      cases r1 with
      | Err _ => cases ok_inj hok
      | Ok t =>
        have ht_eq : t = .ScalarValue d := ((hr1_iff t).mp rfl).1
        subst ht_eq
        dsimp only at hok
        obtain ⟨r2, hr2, hr2_iff⟩ := Type_inner_spec hctx.2.2.2 op2
        rw [bind_ok_eq hr2] at hok
        cases r2 with
        | Err _ => cases ok_inj hok
        | Ok t1 =>
          have ht1_eq : t1 = pureUnwrapReleasable op2 := ((hr2_iff t1).mp rfl).1
          subst ht1_eq
          dsimp only at hok
          cases h2 : pureUnwrapReleasable op2 <;>
            (rw [h2] at hptr hok; first | exact False.elim hptr | exact hok)
    have hnoswap2 : ¬ ((∃ d2, pureUnwrapReleasable op2 = .ScalarValue d2) ∧
        verifier.AluType.Add = .Add ∧ isSwapPtrType (pureUnwrapReleasable (.ScalarValue d))) := by
      rintro ⟨⟨d2, hd2⟩, _, _⟩
      rw [hd2] at hptr
      exact hptr
    simp only [↓reduceIte]
    rw [evalAlu64Val_add_comm layout₁ arrayLen v₁ w₁,
        evalAlu64Val_add_comm layout₂ arrayLen v₂ w₂]
    exact apply_computation_noswap_ok_valid_and_rel2 hctx hop2_valid hop1_valid
      .add64 hrel2 hrel1 hnoswap2 hok'
  · exact apply_computation_noswap_ok_valid_and_rel2 hctx hop1_valid hop2_valid
      hm hrel1 hrel2 hswap hok

theorem apply_computation_neg_ok_valid_and_rel2
    {env : ExecEnv} {vCounter : Nat} {ctx : verifier.ComputationContext}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64} {layout₁ layout₂ : HostLayout}
    {is64 : Bool} {op1 op2 resTy : verifier.Type} {v₁ v₂ : RuntimeVal}
    (hctx : CompCtxValid env vCounter ctx)
    (hop1_valid : TypeValid env vCounter op1)
    (hop2_valid : TypeValid env vCounter op2)
    (hrel1 : TypeRel2 env ρ resActive isNullId arrayLen op1 v₁ v₂)
    (hok : verifier.ComputationContext.apply_computation ctx op1 op2 .Plain
        (if is64 then .Neg64 else .Neg32) = ok (.Ok resTy)) :
    TypeValid env vCounter resTy ∧
    TypeRel2 env ρ resActive isNullId arrayLen resTy
      (.scalar (evalNeg is64 (v₁.toBits layout₁ arrayLen)))
      (.scalar (evalNeg is64 (v₂.toBits layout₂ arrayLen))) := by
  rw [verifier.ComputationContext.apply_computation.eq_def] at hok
  obtain ⟨r1, hr1, hr1_iff⟩ := Type_inner_spec hctx.2.2.2 op1
  rw [bind_ok_eq hr1] at hok
  cases r1 with
  | Err _ => cases ok_inj hok
  | Ok t =>
    dsimp only at hok
    have ht_eq : t = pureUnwrapReleasable op1 := ((hr1_iff t).mp rfl).1
    obtain ⟨r2, hr2, hr2_iff⟩ := Type_inner_spec hctx.2.2.2 op2
    rw [bind_ok_eq hr2] at hok
    cases r2 with
    | Err _ => cases ok_inj hok
    | Ok t1 =>
      dsimp only at hok
      have ht1_eq : t1 = pureUnwrapReleasable op2 := ((hr2_iff t1).mp rfl).1
      have ht_valid : TypeValid env vCounter t := ht_eq ▸ Type_unwrap_releasable_valid hop1_valid
      have ht1_valid : TypeValid env vCounter t1 := ht1_eq ▸ Type_unwrap_releasable_valid hop2_valid
      have ht_not_rel : ∀ r i, t ≠ .Releasable r i :=
        ht_eq ▸ pureUnwrapReleasable_not_releasable hop1_valid
      have ht1_not_rel : ∀ r i, t1 ≠ .Releasable r i :=
        ht1_eq ▸ pureUnwrapReleasable_not_releasable hop2_valid
      have closeUninit : resTy = .ScalarValue uninitScalar64 →
          TypeValid env vCounter resTy ∧
          TypeRel2 env ρ resActive isNullId arrayLen resTy
            (.scalar (evalNeg is64 (v₁.toBits layout₁ arrayLen)))
            (.scalar (evalNeg is64 (v₂.toBits layout₂ arrayLen))) := by
        rintro rfl
        exact ⟨uninitScalar64_valid, by simp [TypeRel2]⟩
      cases ht_case : t with
      | Releasable r i => exact absurd ht_case (ht_not_rel r i)
      | ScalarValueParameter | ConstPtrToMapParameter _ _ | MapKeyParameter _
      | MapValueParameter _ | MemoryParameter _ _ _ | NullOrParameter _
      | StructParameter _ | ContextParameter _ | ReleasableParameter _ _
      | ReleaseParameter _ | AnyParameter =>
        rw [ht_case] at ht_valid; exact False.elim ht_valid
      | ConstPtrToMap _ _ | PtrToStack _ | PtrToMemory _ _ _ _
      | PtrToStruct _ _ _ | PtrToArray _ _ | PtrToEndArray _ | NullOr _ _ =>
        rw [ht_case] at hok
        cases ht1_case : t1 <;> rw [ht1_case] at hok <;>
          dsimp only [verifier.AluType.is_ptr_compatible] at hok <;>
          simp only [Type_UNINITIALIZED_eq, bind_ok, Bool.false_eq_true, ↓reduceIte] at hok <;>
          exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
      | ScalarValue data1 =>
        rw [ht_case] at hok ht_valid
        have hop1_sv : op1 = .ScalarValue data1 := by
          cases op1 with
          | Releasable _ inner =>
            cases inner <;> try exact False.elim hop1_valid
            all_goals simp [pureUnwrapReleasable, ht_case] at ht_eq
          | _ => dsimp [pureUnwrapReleasable] at ht_eq; exact ht_eq.symm.trans ht_case
        subst hop1_sv
        cases ht1_case : t1 with
        | Releasable r i => exact absurd ht1_case (ht1_not_rel r i)
        | ScalarValueParameter | ConstPtrToMapParameter _ _ | MapKeyParameter _
        | MapValueParameter _ | MemoryParameter _ _ _ | NullOrParameter _
        | StructParameter _ | ContextParameter _ | ReleasableParameter _ _
        | ReleaseParameter _ | AnyParameter =>
          rw [ht1_case] at ht1_valid; exact False.elim ht1_valid
        | ConstPtrToMap _ _ | PtrToStack _ | PtrToMemory _ _ _ _
        | PtrToStruct _ _ _ | PtrToArray _ _ | PtrToEndArray _ | NullOr _ _ =>
          rw [ht1_case] at hok
          simp only [Type_UNINITIALIZED_eq, bind_ok] at hok
          exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
        | ScalarValue data2 =>
          rw [ht1_case] at hok ht1_valid
          dsimp only at hok
          rcases bind_eq_ok hok with ⟨usvd, heval, hok⟩
          cases core.result.Result.Ok.inj (ok_inj hok)
          obtain ⟨res, hres, hc⟩ := ScalarAluOp_eval_neg_sound_2safety is64 data1 data2
            (TypeRel2_scalar_toBits_rel2 (layout₁ := layout₁) (layout₂ := layout₂) hrel1)
          rw [heval] at hres; cases ok_inj hres
          exact ⟨ScalarAluOp_eval_valid ht_valid ht1_valid heval,
            TypeRel2_scalar_of_concretizes hc⟩

private theorem apply_computation_noswap_ok_valid
    {env : ExecEnv} {vCounter : Nat} {ctx : verifier.ComputationContext}
    {alu_type : verifier.AluType} {alu_op : verifier.ScalarAluOp}
    {op1 op2 resTy : verifier.Type}
    (hctx : CompCtxValid env vCounter ctx)
    (hop1_valid : TypeValid env vCounter op1)
    (hop2_valid : TypeValid env vCounter op2)
    (hcompat : (alu_type = .Add ∨ alu_type = .Sub) → (alu_op = .Add64 ∨ alu_op = .Sub64))
    (hnoswap : ¬ ((∃ d, pureUnwrapReleasable op1 = .ScalarValue d) ∧
                  alu_type = .Add ∧ isSwapPtrType (pureUnwrapReleasable op2)))
    (hok : verifier.ComputationContext.apply_computation ctx op1 op2 alu_type alu_op = ok (.Ok resTy)) :
    TypeValid env vCounter resTy := by
  rw [verifier.ComputationContext.apply_computation.eq_def] at hok
  obtain ⟨r1, hr1, hr1_iff⟩ := Type_inner_spec hctx.2.2.2 op1
  rw [bind_ok_eq hr1] at hok
  cases r1 with
  | Err _ => cases ok_inj hok
  | Ok t =>
    dsimp only at hok
    have ht_eq : t = pureUnwrapReleasable op1 := ((hr1_iff t).mp rfl).1
    obtain ⟨r2, hr2, hr2_iff⟩ := Type_inner_spec hctx.2.2.2 op2
    rw [bind_ok_eq hr2] at hok
    cases r2 with
    | Err _ => cases ok_inj hok
    | Ok t1 =>
      dsimp only at hok
      have ht1_eq : t1 = pureUnwrapReleasable op2 := ((hr2_iff t1).mp rfl).1
      have ht_valid : TypeValid env vCounter t := ht_eq ▸ Type_unwrap_releasable_valid hop1_valid
      have ht1_valid : TypeValid env vCounter t1 := ht1_eq ▸ Type_unwrap_releasable_valid hop2_valid
      have ht_not_rel : ∀ r i, t ≠ .Releasable r i :=
        ht_eq ▸ pureUnwrapReleasable_not_releasable hop1_valid
      have ht1_not_rel : ∀ r i, t1 ≠ .Releasable r i :=
        ht1_eq ▸ pureUnwrapReleasable_not_releasable hop2_valid
      have closeUninit : resTy = .ScalarValue uninitScalar64 → TypeValid env vCounter resTy := by
        rintro rfl; exact uninitScalar64_valid
      cases ht_case : t with
      | Releasable r i => exact absurd ht_case (ht_not_rel r i)
      | ScalarValueParameter | ConstPtrToMapParameter _ _ | MapKeyParameter _
      | MapValueParameter _ | MemoryParameter _ _ _ | NullOrParameter _
      | StructParameter _ | ContextParameter _ | ReleasableParameter _ _
      | ReleaseParameter _ | AnyParameter =>
        rw [ht_case] at ht_valid; exact False.elim ht_valid
      | ConstPtrToMap _ _ | NullOr _ _ =>
        rw [ht_case] at hok
        cases ht1_case : t1 <;> rw [ht1_case] at hok <;>
          (try cases alu_type) <;>
          simp only [Type_UNINITIALIZED_eq, bind_ok] at hok <;>
          exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
      | ScalarValue data1 =>
        rw [ht_case] at hok ht_valid
        cases ht1_case : t1 with
        | Releasable r i => exact absurd ht1_case (ht1_not_rel r i)
        | ScalarValueParameter | ConstPtrToMapParameter _ _ | MapKeyParameter _
        | MapValueParameter _ | MemoryParameter _ _ _ | NullOrParameter _
        | StructParameter _ | ContextParameter _ | ReleasableParameter _ _
        | ReleaseParameter _ | AnyParameter =>
          rw [ht1_case] at ht1_valid; exact False.elim ht1_valid
        | ConstPtrToMap _ _ | PtrToEndArray _ | NullOr _ _ =>
          rw [ht1_case] at hok
          cases alu_type <;>
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok <;>
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
        | PtrToStack _ | PtrToMemory _ _ _ _ | PtrToStruct _ _ _ | PtrToArray _ _ =>
          rw [ht1_case] at hok
          cases halu : alu_type with
          | Plain | Sub =>
            rw [halu] at hok
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Add =>
            exfalso
            apply hnoswap
            refine ⟨⟨data1, ht_eq.symm.trans ht_case⟩, halu, ?_⟩
            rw [← ht1_eq, ht1_case]
            trivial
        | ScalarValue data2 =>
          rw [ht1_case] at hok ht1_valid
          dsimp only at hok
          rcases bind_eq_ok hok with ⟨usvd, heval, hok⟩
          cases core.result.Result.Ok.inj (ok_inj hok)
          exact ScalarAluOp_eval_valid ht_valid ht1_valid heval
      | PtrToStack x =>
        rw [ht_case] at hok ht_valid
        cases ht1_case : t1 with
        | Releasable r i => exact absurd ht1_case (ht1_not_rel r i)
        | ScalarValueParameter | ConstPtrToMapParameter _ _ | MapKeyParameter _
        | MapValueParameter _ | MemoryParameter _ _ _ | NullOrParameter _
        | StructParameter _ | ContextParameter _ | ReleasableParameter _ _
        | ReleaseParameter _ | AnyParameter =>
          rw [ht1_case] at ht1_valid; exact False.elim ht1_valid
        | ConstPtrToMap _ _ | PtrToMemory _ _ _ _ | PtrToStruct _ _ _
        | PtrToArray _ _ | PtrToEndArray _ | NullOr _ _ =>
          rw [ht1_case] at hok
          (try cases alu_type) <;>
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok <;>
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
        | ScalarValue data =>
          rw [ht1_case] at hok ht1_valid
          cases halu : alu_type with
          | Plain =>
            rw [halu] at hok
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, Bool.false_eq_true, ↓reduceIte, Type_UNINITIALIZED_eq] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Add | Sub =>
            have hop_as : alu_op = .Add64 ∨ alu_op = .Sub64 :=
              hcompat (by cases halu <;> simp)
            rw [halu] at hok
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, ↓reduceIte] at hok
            unfold U64ScalarValueData.is_fully_initialized at hok
            simp only [bind_ok] at hok
            by_cases hd_init : data.unwritten_mask = 0#u64
            · simp only [hd_init, decide_true, ↓reduceIte, StackOffset_reg_spec, bind_ok] at hok
              rcases bind_eq_ok hok with ⟨offset, heval, hok⟩
              unfold U64ScalarValueData.max at hok
              simp only [BPF_MAX_VAR_OFF_eq, bind_ok] at hok
              by_cases hgt : offset.urange.max > 536870912#u64
              · simp [hgt] at hok
              · simp only [hgt, ↓reduceIte] at hok
                cases core.result.Result.Ok.inj (ok_inj hok)
                exact ⟨ScalarAluOp_eval_valid ht_valid.1 ht1_valid heval,
                  add_or_sub_64_unwritten_zero hop_as ht_valid.2.1
                    ((u64_eq_iff_bv _ _).mp hd_init) heval,
                  bpf_max_var_off_le_u32_max hgt⟩
            · simp [hd_init] at hok
        | PtrToStack x2 =>
          rw [ht1_case] at hok ht1_valid
          cases halu : alu_type with
          | Plain | Add =>
            rw [halu] at hok
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Sub =>
            rw [halu] at hok
            dsimp only at hok
            simp only [StackOffset_reg_spec, bind_ok] at hok
            rcases bind_eq_ok hok with ⟨usvd2, heval, hok⟩
            cases core.result.Result.Ok.inj (ok_inj hok)
            exact ScalarAluOp_eval_valid ht_valid.1 ht1_valid.1 heval
      | PtrToMemory id1 x sz orig =>
        rw [ht_case] at hok ht_valid
        cases ht1_case : t1 with
        | Releasable r i => exact absurd ht1_case (ht1_not_rel r i)
        | ScalarValueParameter | ConstPtrToMapParameter _ _ | MapKeyParameter _
        | MapValueParameter _ | MemoryParameter _ _ _ | NullOrParameter _
        | StructParameter _ | ContextParameter _ | ReleasableParameter _ _
        | ReleaseParameter _ | AnyParameter =>
          rw [ht1_case] at ht1_valid; exact False.elim ht1_valid
        | ConstPtrToMap _ _ | PtrToStack _ | PtrToStruct _ _ _
        | PtrToArray _ _ | PtrToEndArray _ | NullOr _ _ =>
          rw [ht1_case] at hok
          (try cases alu_type) <;>
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok <;>
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
        | ScalarValue data =>
          rw [ht1_case] at hok ht1_valid
          cases halu : alu_type with
          | Plain =>
            rw [halu] at hok
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, Bool.false_eq_true, ↓reduceIte, Type_UNINITIALIZED_eq] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Add | Sub =>
            have hop_as : alu_op = .Add64 ∨ alu_op = .Sub64 :=
              hcompat (by cases halu <;> simp)
            rw [halu] at hok
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, ↓reduceIte] at hok
            unfold U64ScalarValueData.is_fully_initialized at hok
            simp only [bind_ok] at hok
            by_cases hd_init : data.unwritten_mask = 0#u64
            · simp only [hd_init, decide_true, ↓reduceIte] at hok
              rcases bind_eq_ok hok with ⟨offset, heval, hok⟩
              unfold U64ScalarValueData.max at hok
              simp only [BPF_MAX_VAR_OFF_1_eq, bind_ok] at hok
              by_cases hgt : offset.urange.max > 536870912#u64
              · simp [hgt] at hok
              · simp only [hgt, ↓reduceIte, Type_with_offset_spec, bind_ok] at hok
                cases core.result.Result.Ok.inj (ok_inj hok)
                have hoff_valid : PtrOffsetValid 0xFFFF_FFFF#64 offset :=
                  ⟨ScalarAluOp_eval_valid ht_valid.2.1 ht1_valid heval,
                   add_or_sub_64_unwritten_zero hop_as ht_valid.2.2.1
                     ((u64_eq_iff_bv _ _).mp hd_init) heval,
                   bpf_max_var_off_le_u32_max hgt⟩
                cases op1 with
                | Releasable relId inner =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq; subst ht_eq
                  exact ⟨hop1_valid.1, ht_valid.1, hoff_valid⟩
                | PtrToMemory _ _ _ _ =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq; cases ht_eq
                  exact ⟨ht_valid.1, hoff_valid⟩
                | _ =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq; cases ht_eq
            · simp [hd_init] at hok
        | PtrToMemory id2 x2 sz2 orig2 =>
          rw [ht1_case] at hok ht1_valid
          cases halu : alu_type with
          | Plain | Add =>
            rw [halu] at hok
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Sub =>
            rw [halu] at hok
            dsimp only at hok
            rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at hok
            by_cases hid : id1 = id2
            · simp only [hid, decide_true, ↓reduceIte] at hok
              rcases bind_eq_ok hok with ⟨usvd, heval, hok⟩
              cases core.result.Result.Ok.inj (ok_inj hok)
              exact ScalarAluOp_eval_valid ht_valid.2.1 ht1_valid.2.1 heval
            · simp only [hid, decide_false, Bool.false_eq_true, ↓reduceIte,
                Type_UNINITIALIZED_eq, bind_ok] at hok
              exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
      | PtrToStruct id1 x sd =>
        rw [ht_case] at hok ht_valid
        cases ht1_case : t1 with
        | Releasable r i => exact absurd ht1_case (ht1_not_rel r i)
        | ScalarValueParameter | ConstPtrToMapParameter _ _ | MapKeyParameter _
        | MapValueParameter _ | MemoryParameter _ _ _ | NullOrParameter _
        | StructParameter _ | ContextParameter _ | ReleasableParameter _ _
        | ReleaseParameter _ | AnyParameter =>
          rw [ht1_case] at ht1_valid; exact False.elim ht1_valid
        | ConstPtrToMap _ _ | PtrToStack _ | PtrToMemory _ _ _ _
        | PtrToArray _ _ | PtrToEndArray _ | NullOr _ _ =>
          rw [ht1_case] at hok
          (try cases alu_type) <;>
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok <;>
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
        | ScalarValue data =>
          rw [ht1_case] at hok ht1_valid
          cases halu : alu_type with
          | Plain =>
            rw [halu] at hok
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, Bool.false_eq_true, ↓reduceIte, Type_UNINITIALIZED_eq] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Add | Sub =>
            have hop_as : alu_op = .Add64 ∨ alu_op = .Sub64 :=
              hcompat (by cases halu <;> simp)
            rw [halu] at hok
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, ↓reduceIte] at hok
            unfold U64ScalarValueData.is_fully_initialized at hok
            simp only [bind_ok] at hok
            by_cases hd_init : data.unwritten_mask = 0#u64
            · simp only [hd_init, decide_true, ↓reduceIte] at hok
              rcases bind_eq_ok hok with ⟨offset, heval, hok⟩
              unfold U64ScalarValueData.max at hok
              simp only [BPF_MAX_VAR_OFF_1_eq, bind_ok] at hok
              by_cases hgt : offset.urange.max > 536870912#u64
              · simp [hgt] at hok
              · simp only [hgt, ↓reduceIte, Type_with_offset_spec, bind_ok] at hok
                cases core.result.Result.Ok.inj (ok_inj hok)
                have hoff_valid : PtrOffsetValid 0xFFFF_FFFF#64 offset :=
                  ⟨ScalarAluOp_eval_valid ht_valid.2.1.1 ht1_valid heval,
                   add_or_sub_64_unwritten_zero hop_as ht_valid.2.1.2.1
                     ((u64_eq_iff_bv _ _).mp hd_init) heval,
                   bpf_max_var_off_le_u32_max hgt⟩
                cases op1 with
                | Releasable relId inner =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq; subst ht_eq
                  exact ⟨hop1_valid.1, ht_valid.1, hoff_valid, ht_valid.2.2⟩
                | PtrToStruct _ _ _ =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq; cases ht_eq
                  exact ⟨ht_valid.1, hoff_valid, ht_valid.2.2⟩
                | _ =>
                  dsimp [pureUnwrapReleasable] at ht_eq
                  rw [ht_case] at ht_eq; cases ht_eq
            · simp [hd_init] at hok
        | PtrToStruct id2 x2 sd2 =>
          rw [ht1_case] at hok ht1_valid
          cases halu : alu_type with
          | Plain | Add =>
            rw [halu] at hok
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Sub =>
            rw [halu] at hok
            dsimp only at hok
            rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at hok
            by_cases hid : id1 = id2
            · simp only [hid, decide_true, ↓reduceIte] at hok
              rcases bind_eq_ok hok with ⟨usvd, heval, hok⟩
              cases core.result.Result.Ok.inj (ok_inj hok)
              exact ScalarAluOp_eval_valid ht_valid.2.1.1 ht1_valid.2.1.1 heval
            · simp only [hid, decide_false, Bool.false_eq_true, ↓reduceIte,
                Type_UNINITIALIZED_eq, bind_ok] at hok
              exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
      | PtrToArray id1 x =>
        rw [ht_case] at hok ht_valid
        cases ht1_case : t1 with
        | Releasable r i => exact absurd ht1_case (ht1_not_rel r i)
        | ScalarValueParameter | ConstPtrToMapParameter _ _ | MapKeyParameter _
        | MapValueParameter _ | MemoryParameter _ _ _ | NullOrParameter _
        | StructParameter _ | ContextParameter _ | ReleasableParameter _ _
        | ReleaseParameter _ | AnyParameter =>
          rw [ht1_case] at ht1_valid; exact False.elim ht1_valid
        | ConstPtrToMap _ _ | PtrToStack _ | PtrToMemory _ _ _ _
        | PtrToStruct _ _ _ | NullOr _ _ =>
          rw [ht1_case] at hok
          (try cases alu_type) <;>
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok <;>
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
        | ScalarValue data =>
          rw [ht1_case] at hok ht1_valid
          cases halu : alu_type with
          | Plain =>
            rw [halu] at hok
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, Bool.false_eq_true, ↓reduceIte, Type_UNINITIALIZED_eq] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Sub =>
            rw [halu] at hok
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, ↓reduceIte] at hok
            unfold U64ScalarValueData.is_fully_initialized at hok
            simp only [bind_ok] at hok
            by_cases hd_init : data.unwritten_mask = 0#u64
            · simp only [hd_init, decide_true, ↓reduceIte] at hok
              rcases bind_eq_ok hok with ⟨opt, hck, hok⟩
              cases opt with
              | none => cases ok_inj hok
              | some offset =>
                dsimp only at hok
                unfold U64ScalarValueData.max at hok
                simp only [verifier_MAX_PACKET_OFFSET_eq, bind_ok] at hok
                by_cases hgt : offset.urange.max > 65535#u64
                · simp [hgt] at hok
                · simp only [hgt, ↓reduceIte, Type_with_offset_spec, bind_ok] at hok
                  cases core.result.Result.Ok.inj (ok_inj hok)
                  have hoff_valid : PtrOffsetValid MAX_PACKET_OFFSET offset :=
                    ⟨checked_sub_64_some_valid hck,
                     checked_sub_64_unwritten_zero ht_valid.2.2.1 ((u64_eq_iff_bv _ _).mp hd_init) hck,
                     max_packet_offset_le hgt⟩
                  cases op1 with
                  | Releasable relId inner =>
                    dsimp [pureUnwrapReleasable] at ht_eq
                    rw [ht_case] at ht_eq; subst ht_eq
                    exact False.elim hop1_valid
                  | PtrToArray _ _ =>
                    dsimp [pureUnwrapReleasable] at ht_eq
                    rw [ht_case] at ht_eq; cases ht_eq
                    exact ⟨ht_valid.1, hoff_valid⟩
                  | _ =>
                    dsimp [pureUnwrapReleasable] at ht_eq
                    rw [ht_case] at ht_eq; cases ht_eq
            · simp [hd_init] at hok
          | Add =>
            rw [halu] at hok
            dsimp only [verifier.AluType.is_ptr_compatible] at hok
            simp only [bind_ok, ↓reduceIte] at hok
            unfold U64ScalarValueData.is_fully_initialized at hok
            simp only [bind_ok] at hok
            by_cases hd_init : data.unwritten_mask = 0#u64
            · simp only [hd_init, decide_true, ↓reduceIte] at hok
              rcases bind_eq_ok hok with ⟨opt, hck, hok⟩
              cases opt with
              | none => cases ok_inj hok
              | some offset =>
                dsimp only at hok
                unfold U64ScalarValueData.max at hok
                simp only [verifier_MAX_PACKET_OFFSET_eq, bind_ok] at hok
                by_cases hgt : offset.urange.max > 65535#u64
                · simp [hgt] at hok
                · simp only [hgt, ↓reduceIte, Type_with_offset_spec, bind_ok] at hok
                  cases core.result.Result.Ok.inj (ok_inj hok)
                  have hoff_valid : PtrOffsetValid MAX_PACKET_OFFSET offset :=
                    ⟨checked_add_64_some_valid hck,
                     checked_add_64_unwritten_zero ht_valid.2.2.1 ((u64_eq_iff_bv _ _).mp hd_init) hck,
                     max_packet_offset_le hgt⟩
                  cases op1 with
                  | Releasable relId inner =>
                    dsimp [pureUnwrapReleasable] at ht_eq
                    rw [ht_case] at ht_eq; subst ht_eq
                    exact False.elim hop1_valid
                  | PtrToArray _ _ =>
                    dsimp [pureUnwrapReleasable] at ht_eq
                    rw [ht_case] at ht_eq; cases ht_eq
                    exact ⟨ht_valid.1, hoff_valid⟩
                  | _ =>
                    dsimp [pureUnwrapReleasable] at ht_eq
                    rw [ht_case] at ht_eq; cases ht_eq
            · simp [hd_init] at hok
        | PtrToArray id2 x2 =>
          rw [ht1_case] at hok ht1_valid
          cases halu : alu_type with
          | Plain | Add =>
            rw [halu] at hok
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Sub =>
            rw [halu] at hok
            dsimp only at hok
            rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at hok
            by_cases hid : id1 = id2
            · simp only [hid, decide_true, ↓reduceIte] at hok
              rcases bind_eq_ok hok with ⟨usvd, heval, hok⟩
              cases core.result.Result.Ok.inj (ok_inj hok)
              exact ScalarAluOp_eval_valid ht_valid.2.1 ht1_valid.2.1 heval
            · simp only [hid, decide_false, Bool.false_eq_true, ↓reduceIte,
                Type_UNINITIALIZED_eq, bind_ok] at hok
              exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
        | PtrToEndArray id2 =>
          rw [ht1_case] at hok ht1_valid
          cases halu : alu_type with
          | Plain | Add =>
            rw [halu] at hok
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Sub =>
            rw [halu] at hok
            dsimp only at hok
            rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at hok
            by_cases hid : id1 = id2
            · simp only [hid, decide_true, ↓reduceIte, Type_UNKNOWN_SCALAR_eq, bind_ok] at hok
              cases core.result.Result.Ok.inj (ok_inj hok)
              exact unknownWrittenScalar64_valid
            · simp only [hid, decide_false, Bool.false_eq_true, ↓reduceIte,
                Type_UNINITIALIZED_eq, bind_ok] at hok
              exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
      | PtrToEndArray id1 =>
        rw [ht_case] at hok ht_valid
        cases ht1_case : t1 with
        | Releasable r i => exact absurd ht1_case (ht1_not_rel r i)
        | ScalarValueParameter | ConstPtrToMapParameter _ _ | MapKeyParameter _
        | MapValueParameter _ | MemoryParameter _ _ _ | NullOrParameter _
        | StructParameter _ | ContextParameter _ | ReleasableParameter _ _
        | ReleaseParameter _ | AnyParameter =>
          rw [ht1_case] at ht1_valid; exact False.elim ht1_valid
        | ScalarValue _ | ConstPtrToMap _ _ | PtrToStack _
        | PtrToMemory _ _ _ _ | PtrToStruct _ _ _ | PtrToEndArray _ | NullOr _ _ =>
          rw [ht1_case] at hok
          (try cases alu_type) <;>
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok <;>
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
        | PtrToArray id2 x2 =>
          rw [ht1_case] at hok ht1_valid
          cases halu : alu_type with
          | Plain | Add =>
            rw [halu] at hok
            simp only [Type_UNINITIALIZED_eq, bind_ok] at hok
            exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm
          | Sub =>
            rw [halu] at hok
            dsimp only at hok
            rw [bind_ok_eq (MemoryId_eq_spec id1 id2)] at hok
            by_cases hid : id1 = id2
            · simp only [hid, decide_true, ↓reduceIte, Type_UNKNOWN_SCALAR_eq, bind_ok] at hok
              cases core.result.Result.Ok.inj (ok_inj hok)
              exact unknownWrittenScalar64_valid
            · simp only [hid, decide_false, Bool.false_eq_true, ↓reduceIte,
                Type_UNINITIALIZED_eq, bind_ok] at hok
              exact closeUninit (core.result.Result.Ok.inj (ok_inj hok)).symm

theorem apply_computation_ok_valid
    {env : ExecEnv} {vCounter : Nat} {ctx : verifier.ComputationContext}
    {alu_type : verifier.AluType} {alu_op : verifier.ScalarAluOp}
    {op1 op2 resTy : verifier.Type}
    (hctx : CompCtxValid env vCounter ctx)
    (hop1_valid : TypeValid env vCounter op1)
    (hop2_valid : TypeValid env vCounter op2)
    (hcompat : (alu_type = .Add ∨ alu_type = .Sub) → (alu_op = .Add64 ∨ alu_op = .Sub64))
    (hok : verifier.ComputationContext.apply_computation ctx op1 op2 alu_type alu_op = ok (.Ok resTy)) :
    TypeValid env vCounter resTy := by
  by_cases hswap : (∃ d, pureUnwrapReleasable op1 = .ScalarValue d) ∧
      alu_type = .Add ∧ isSwapPtrType (pureUnwrapReleasable op2)
  · rcases hswap with ⟨⟨d, hd⟩, halu, hptr⟩
    subst halu
    have hop1_sv : op1 = .ScalarValue d := by
      cases op1 with
      | Releasable _ inner =>
        cases inner <;> try exact False.elim hop1_valid
        all_goals simp [pureUnwrapReleasable] at hd
      | _ => dsimp [pureUnwrapReleasable] at hd; exact hd
    subst hop1_sv
    have hok' : verifier.ComputationContext.apply_computation ctx op2 (.ScalarValue d) .Add alu_op =
        ok (.Ok resTy) := by
      rw [verifier.ComputationContext.apply_computation.eq_def] at hok
      obtain ⟨r1, hr1, hr1_iff⟩ := Type_inner_spec hctx.2.2.2 (.ScalarValue d)
      rw [bind_ok_eq hr1] at hok
      cases r1 with
      | Err _ => cases ok_inj hok
      | Ok t =>
        have ht_eq : t = .ScalarValue d := ((hr1_iff t).mp rfl).1
        subst ht_eq
        dsimp only at hok
        obtain ⟨r2, hr2, hr2_iff⟩ := Type_inner_spec hctx.2.2.2 op2
        rw [bind_ok_eq hr2] at hok
        cases r2 with
        | Err _ => cases ok_inj hok
        | Ok t1 =>
          have ht1_eq : t1 = pureUnwrapReleasable op2 := ((hr2_iff t1).mp rfl).1
          subst ht1_eq
          dsimp only at hok
          cases h2 : pureUnwrapReleasable op2 <;>
            (rw [h2] at hptr hok; first | exact False.elim hptr | exact hok)
    have hnoswap2 : ¬ ((∃ d2, pureUnwrapReleasable op2 = .ScalarValue d2) ∧
        verifier.AluType.Add = .Add ∧ isSwapPtrType (pureUnwrapReleasable (.ScalarValue d))) := by
      rintro ⟨⟨d2, hd2⟩, _, _⟩
      rw [hd2] at hptr
      exact hptr
    exact apply_computation_noswap_ok_valid hctx hop2_valid hop1_valid hcompat hnoswap2 hok'
  · exact apply_computation_noswap_ok_valid hctx hop1_valid hop2_valid hcompat hswap hok

end StarnixEbpf.Proof
