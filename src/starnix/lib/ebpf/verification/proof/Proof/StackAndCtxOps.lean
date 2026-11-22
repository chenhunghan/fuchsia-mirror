-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.CtxContainers

/-!
# Verifier `StackOffset` & `Stack` Operations

Proves total specifications, structural-invariant preservation (`StackValid`),
and relational 2-safety soundness (`StackSlotRel2`) for:
1. `popcount_u64`, `StackOffset`, and `Stack` container operations (`bit`,
   `position`, `default`, `clone`, `eq`, `get`, `set`, `set_null`, `retain`).
2. `Stack` sub-word and range memory operations (`extract_sub_value`,
   `insert_sub_value`, `read_element`, `read_data_ptr`, `load`, `store`,
   `write_data_ptr`).

`StateRel2Deps` stack-retention (`StateRel2Deps_stack_retain`), `ComputationContext`
& `VerificationContext` operations, `clear_dead_registers`, and initial
entry-state conformance (`verify_program_core_loop3`) live in
`Proof/ComputationCtxOps.lean`.
-/

set_option linter.unusedTactic false
set_option maxHeartbeats 800000

open Aeneas Aeneas.Std Result

namespace StarnixEbpf.Proof

open StarnixEbpf.Assumptions
open StarnixEbpf.Spec
open StarnixEbpf.LoopLemmas
open StarnixEbpf.ApiConverterLinkerExec
open ebpf_core
open ebpf_core.scalar_value

/-! ## 1. `Usize` Division/Remainder, `popcount_u64`, & `StackOffset` -/

theorem usize_eq_of_val_eq {a b : Std.Usize} (h : a.val = b.val) : a = b := by
  cases a; cases b
  rw [← UScalar.bv_toNat, ← UScalar.bv_toNat] at h
  cases BitVec.eq_of_toNat_eq h
  rfl

theorem usize_div_eq_ok (a b : Std.Usize) (hb : b.val ≠ 0) :
    ∃ c : Std.Usize, (a / b : Result Std.Usize) = ok c ∧ c.val = a.val / b.val := by
  have hbv_ne : b.bv ≠ 0 := by
    intro hc
    apply hb
    rw [← UScalar.bv_toNat]
    simpa using congrArg BitVec.toNat hc
  have hb0 : (b.bv != 0) = true := bne_iff_ne.mpr hbv_ne
  refine ⟨⟨a.bv / b.bv⟩, ?_, ?_⟩
  · change UScalar.div a b = _
    simp only [UScalar.div, hb0, ↓reduceIte]
    rfl
  · rw [← UScalar.bv_toNat]
    change (a.bv / b.bv).toNat = a.val / b.val
    rw [BitVec.toNat_udiv, UScalar.bv_toNat, UScalar.bv_toNat]

theorem usize_rem_eq_ok (a b : Std.Usize) (hb : b.val ≠ 0) :
    ∃ c : Std.Usize, (a % b : Result Std.Usize) = ok c ∧ c.val = a.val % b.val := by
  have hb0 : (b.val != 0) = true := bne_iff_ne.mpr hb
  refine ⟨⟨a.bv % b.bv⟩, ?_, ?_⟩
  · change UScalar.rem a b = _
    unfold UScalar.rem
    rw [hb0]
    simp only [↓reduceIte, BitVec.umod_eq]
  · rw [← UScalar.bv_toNat]
    change (a.bv % b.bv).toNat = a.val % b.val
    rw [BitVec.toNat_umod, UScalar.bv_toNat, UScalar.bv_toNat]

theorem usize_mul_eq_ok (a b : Std.Usize) (hle : a.val * b.val ≤ Usize.max) :
    ∃ c : Std.Usize, (a * b : Result Std.Usize) = ok c ∧ c.val = a.val * b.val := by
  have hle' : a.val * b.val ≤ UScalar.max .Usize := by rwa [UScalar.max_USize_eq]
  have hok := uscalar_mul_eq_ok a b hle'
  have hs := UScalar.mul_bv_spec hle'
  rw [hok] at hs
  simp at hs
  exact ⟨⟨a.bv * b.bv⟩, hok, hs⟩

theorem popcountRange_congr {b₁ b₂ : BitVec 64} {n : Nat}
    (h : ∀ j < n, b₁.getLsbD j = b₂.getLsbD j) :
    popcountRange b₁ n = popcountRange b₂ n := by
  induction n with
  | zero => rfl
  | succ k ih =>
    simp only [popcountRange]
    rw [ih (fun j hj => h j (by omega)), h k (by omega)]

theorem popcountRange_zero (n : Nat) : popcountRange 0#64 n = 0 := by
  induction n with
  | zero => rfl
  | succ k ih => simp [popcountRange, ih]

private theorem bv64_one_shl_getLsbD {k j : Nat} (hk : k < 64) (hj : j < 64) :
    ((1#64) <<< k).getLsbD j = decide (j = k) := by
  rw [BitVec.getLsbD_shiftLeft, BitVec.getLsbD_one]
  by_cases hlt : j < k
  · have hne : j ≠ k := by omega
    simp [hj, hlt, hne]
  · by_cases heq : j = k
    · subst heq; simp [hj]
    · have hsub : j - k ≠ 0 := by omega
      simp [hj, hlt, hsub, heq]

private theorem bv64_prefix_mask_getLsbD {k j : Nat} (hk : k < 64) :
    (((1#64) <<< k) - 1#64).getLsbD j = decide (j < k) := by
  by_cases hj : j < 64
  · have hpow_lt : 2 ^ k < 2 ^ 64 := Nat.pow_lt_pow_right (by omega) hk
    have hshl_nat : ((1#64) <<< k).toNat = 2 ^ k := by
      rw [BitVec.toNat_shiftLeft]
      change (1 <<< k) % 2 ^ 64 = 2 ^ k
      rw [Nat.one_shiftLeft, Nat.mod_eq_of_lt hpow_lt]
    have h1_le : (1#64 : BitVec 64) ≤ (1#64) <<< k := by
      rw [BitVec.le_def, hshl_nat]; exact Nat.one_le_two_pow
    have hsub_nat : (((1#64) <<< k) - 1#64).toNat = 2 ^ k - 1 := by
      rw [BitVec.toNat_sub_of_le h1_le, hshl_nat]; rfl
    simp only [BitVec.getLsbD, hsub_nat, Nat.testBit_two_pow_sub_one]
  · have h1 : (((1#64) <<< k) - 1#64).getLsbD j = false :=
      BitVec.getLsbD_of_ge _ _ (by omega)
    have h2 : ¬ (j < k) := by omega
    simp [h1, h2]

theorem popcountRange_clear_bit_lt {b : BitVec 64} {k n : Nat}
    (hk : k < 64) (hn : n ≤ 64) (hkn : k < n) (hb : b.getLsbD k = true) :
    popcountRange b n = popcountRange (b &&& ~~~((1#64) <<< k)) n + 1 := by
  induction n with
  | zero => omega
  | succ m ih =>
    have hm64 : m < 64 := by omega
    simp only [popcountRange, BitVec.getLsbD_and, BitVec.getLsbD_not,
      bv64_one_shl_getLsbD hk hm64]
    by_cases hmk : m = k
    · subst hmk
      have hcongr : popcountRange b m = popcountRange (b &&& ~~~((1#64) <<< m)) m := by
        apply popcountRange_congr
        intro j hj
        have hj64 : j < 64 := by omega
        rw [BitVec.getLsbD_and, BitVec.getLsbD_not, bv64_one_shl_getLsbD hk hj64]
        have hne : j ≠ m := by omega
        simp [hne, hj64]
      simp [hb, hcongr]
    · have hklt : k < m := by omega
      rw [ih (by omega) hklt]
      simp [hmk, hm64]
      omega

theorem popcountRange_clear_bit_ge {b : BitVec 64} {k n : Nat}
    (hk : k < 64) (hn : n ≤ k) :
    popcountRange (b &&& ~~~((1#64) <<< k)) n = popcountRange b n := by
  apply popcountRange_congr
  intro j hj
  have hj64 : j < 64 := by omega
  rw [BitVec.getLsbD_and, BitVec.getLsbD_not, bv64_one_shl_getLsbD hk hj64]
  have hne : j ≠ k := by omega
  simp [hne, hj64]

theorem popcountRange_set_bit_lt {b : BitVec 64} {k n : Nat}
    (hk : k < 64) (hn : n ≤ 64) (hkn : k < n) (hb : b.getLsbD k = false) :
    popcountRange (b ||| ((1#64) <<< k)) n = popcountRange b n + 1 := by
  induction n with
  | zero => omega
  | succ m ih =>
    simp only [popcountRange, BitVec.getLsbD_or,
      bv64_one_shl_getLsbD hk (by omega : m < 64)]
    by_cases hmk : m = k
    · subst hmk
      have hcongr : popcountRange (b ||| ((1#64) <<< m)) m = popcountRange b m := by
        apply popcountRange_congr
        intro j hj
        rw [BitVec.getLsbD_or, bv64_one_shl_getLsbD hk (by omega)]
        have hne : j ≠ m := by omega
        simp [hne]
      simp [hb, hcongr]
    · have hklt : k < m := by omega
      rw [ih (by omega) hklt]
      simp [hmk]
      omega

theorem popcountRange_set_bit_ge {b : BitVec 64} {k n : Nat}
    (hk : k < 64) (hn : n ≤ k) :
    popcountRange (b ||| ((1#64) <<< k)) n = popcountRange b n := by
  apply popcountRange_congr
  intro j hj
  rw [BitVec.getLsbD_or, bv64_one_shl_getLsbD hk (by omega)]
  have hne : j ≠ k := by omega
  simp [hne]

private theorem exists_lsb_true_aux (b : BitVec 64) :
    ∀ n ≤ 64,
      (∃ k < n, b.getLsbD k = true ∧ ∀ j < k, b.getLsbD j = false) ∨
      (∀ j < n, b.getLsbD j = false) := by
  intro n hn
  induction n with
  | zero => exact Or.inr (fun _ hj => by omega)
  | succ m ih =>
    rcases ih (by omega) with ⟨k, hkm, hk, hmin⟩ | hall
    · exact Or.inl ⟨k, by omega, hk, hmin⟩
    · by_cases hbm : b.getLsbD m = true
      · exact Or.inl ⟨m, by omega, hbm, hall⟩
      · refine Or.inr (fun j hj => ?_)
        by_cases hjm : j < m
        · exact hall j hjm
        · have heq : j = m := by omega
          subst heq
          exact Bool.eq_false_iff.mpr hbm

private theorem exists_lsb_true {b : BitVec 64} (hne : b ≠ 0#64) :
    ∃ k < 64, b.getLsbD k = true ∧ ∀ j < k, b.getLsbD j = false := by
  rcases exists_lsb_true_aux b 64 (Nat.le_refl _) with h | hall
  · exact h
  · exfalso
    apply hne
    apply BitVec.eq_of_getLsbD_eq
    intro i hi
    rw [hall i hi, BitVec.getLsbD_zero]

private theorem bv64_kernighan_step_identity (b m : BitVec 64)
    (hbm : b &&& m = m) (hlow : b &&& (m - 1#64) = 0#64) :
    b &&& (b - 1#64) = b &&& ~~~m := by
  bv_decide

private theorem bv64_kernighan_lt {b : BitVec 64} (hne : b ≠ 0#64) :
    (b &&& (b - 1#64)) < b := by
  bv_decide

private theorem popcount64_kernighan_step {b : BitVec 64} (hne : b ≠ 0#64) :
    popcount64 b = popcount64 (b &&& (b - 1#64)) + 1 := by
  obtain ⟨k, hk, hbk, hmin⟩ := exists_lsb_true hne
  let m : BitVec 64 := (1#64) <<< k
  have hbm : b &&& m = m := by
    apply BitVec.eq_of_getLsbD_eq
    intro i hi
    rw [BitVec.getLsbD_and, bv64_one_shl_getLsbD hk hi]
    by_cases hik : i = k
    · subst hik; simp [hbk]
    · simp [hik]
  have hlow : b &&& (m - 1#64) = 0#64 := by
    apply BitVec.eq_of_getLsbD_eq
    intro i hi
    rw [BitVec.getLsbD_and, bv64_prefix_mask_getLsbD hk, BitVec.getLsbD_zero]
    by_cases hik : i < k
    · simp [hik, hmin i hik]
    · simp [hik]
  rw [bv64_kernighan_step_identity b m hbm hlow]
  exact popcountRange_clear_bit_lt hk (Nat.le_refl 64) hk hbk

private theorem popcount_u64_loop_spec :
    ∀ (fuel : Nat) (x : Std.U64) (cnt : Std.Usize),
      popcount64 x.bv ≤ fuel →
      cnt.val + popcount64 x.bv ≤ 64 →
      ∃ res : Std.Usize,
        verifier.popcount_u64_loop x cnt = ok res ∧
        res.val = cnt.val + popcount64 x.bv := by
  intro fuel
  induction fuel with
  | zero =>
    intro x cnt hle _
    have hx0 : x = 0#u64 := by
      by_cases hne : x = 0#u64
      · exact hne
      · have hbv : x.bv ≠ 0#64 := fun hc => hne ((u64_eq_iff_bv _ _).mpr hc)
        have hstep := popcount64_kernighan_step hbv
        omega
    subst hx0
    unfold verifier.popcount_u64_loop
    rw [loop_unfold_step]
    unfold verifier.popcount_u64_loop.body
    simp [popcount64, popcountRange_zero]
  | succ f ih =>
    intro x cnt hle hsum
    unfold verifier.popcount_u64_loop
    rw [loop_unfold_step]
    unfold verifier.popcount_u64_loop.body
    by_cases hx0 : x = 0#u64
    · subst hx0
      simp [popcount64, popcountRange_zero]
    · have hbne : (x != 0#u64) = true := bne_iff_ne.mpr hx0
      have hbv_ne : x.bv ≠ 0#64 := fun hc => hx0 ((u64_eq_iff_bv _ _).mpr hc)
      have hx_ge1 : (1#u64).val ≤ x.val := by
        rw [← UScalar.bv_toNat, ← UScalar.bv_toNat]
        have : x.bv.toNat ≠ 0 := fun hc => hbv_ne (BitVec.eq_of_toNat_eq hc)
        change 1 ≤ x.bv.toNat
        omega
      have hsub := uscalar_sub_eq_ok x 1#u64 hx_ge1
      simp only [u64_one_uscalar_bv] at hsub
      have hpop_step := popcount64_kernighan_step hbv_ne
      have hmax64 := usize_max_ge_64
      rw [UScalar.max_USize_eq] at hmax64
      obtain ⟨cnt1, hcnt1, hcnt1_val⟩ := usize_add_eq_ok cnt 1#usize (by
        rw [usize_one_val]; omega)
      rw [usize_one_val] at hcnt1_val
      simp only [hbne, ↓reduceIte]
      rw [bind_ok_eq hsub]
      simp only [lift_eq_ok, bind_ok]
      rw [bind_ok_eq hcnt1]
      simp only [bind_ok]
      obtain ⟨res, hres, hres_val⟩ := ih (x &&& ⟨x.bv - 1#64⟩) cnt1
        (by change popcount64 (x.bv &&& (x.bv - 1#64)) ≤ f; omega)
        (by change cnt1.val + popcount64 (x.bv &&& (x.bv - 1#64)) ≤ 64; omega)
      unfold verifier.popcount_u64_loop verifier.popcount_u64_loop.body at hres
      simp only [lift_eq_ok, bind_ok] at hres
      exact ⟨res, hres, by
        simp only [u64_and_bv, u64_mk_bv] at hres_val
        omega⟩

theorem popcount_u64_spec (x : Std.U64) :
    ∃ res : Std.Usize,
      verifier.popcount_u64 x = ok res ∧
      res.val = popcount64 x.bv := by
  unfold verifier.popcount_u64
  have hle64 : popcount64 x.bv ≤ 64 := popcountRange_le x.bv 64
  obtain ⟨res, hres, hval⟩ := popcount_u64_loop_spec 64 x 0#usize hle64 (by simp [hle64])
  exact ⟨res, hres, by simpa using hval⟩

/-! ### `StackOffset` Operations -/

theorem base_64_ok_valid (a b : U64ScalarValueData) :
    ∃ res, U64ScalarValueData.base a b = ok res ∧ Scalar64Valid res := by
  unfold U64ScalarValueData.base U64ScalarValueData.is_fully_initialized
  rw [bind_ok_eq rfl]
  by_cases ha : a.unwritten_mask = 0#u64
  · simp only [ha, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
    by_cases hb : b.unwritten_mask = 0#u64
    · simp only [hb, decide_true, ↓reduceIte, UNKNOWN_WRITTEN_64_eq]
      exact ⟨unknownWrittenScalar64, rfl, unknownWrittenScalar64_valid⟩
    · simp only [hb, decide_false, Bool.false_eq_true, ↓reduceIte, UNINITIALIZED_64_eq]
      exact ⟨uninitScalar64, rfl, uninitScalar64_valid⟩
  · simp only [ha, decide_false, Bool.false_eq_true, ↓reduceIte, UNINITIALIZED_64_eq]
    exact ⟨uninitScalar64, rfl, uninitScalar64_valid⟩

theorem add_scalar_64_ok_valid (a b : U64ScalarValueData) :
    ∃ res, U64ScalarValueData.add_scalar a b = ok res ∧ Scalar64Valid res := by
  unfold U64ScalarValueData.add_scalar U64ScalarValueData.is_known lift
  rw [bind_ok_eq rfl]
  by_cases hak : a.unknown_mask = 0#u64
  · simp only [hak, decide_true, ↓reduceIte]; rw [bind_ok_eq rfl]
    by_cases hbk : b.unknown_mask = 0#u64
    · simp only [hbk, decide_true, ↓reduceIte]
      rw [bind_ok_eq rfl, u64_wrapping_add_eq, from_known_64_eq]
      exact ⟨_, rfl, knownScalar64_valid _⟩
    · simp only [hbk, decide_false, Bool.false_eq_true, ↓reduceIte]
      obtain ⟨ur, hur, _⟩ := range64_add_ok_and_sound a.urange b.urange
      obtain ⟨usvd, husvd, _⟩ := base_64_ok_valid a b
      rw [bind_ok_eq husvd, bind_ok_eq hur]
      unfold U64ScalarValueData.update_range
      rw [new64_eq_ok]
      exact ⟨_, rfl, pureNew64_valid _ _ _ _ _⟩
  · simp only [hak, decide_false, Bool.false_eq_true, ↓reduceIte]
    obtain ⟨ur, hur, _⟩ := range64_add_ok_and_sound a.urange b.urange
    obtain ⟨usvd, husvd, _⟩ := base_64_ok_valid a b
    rw [bind_ok_eq husvd, bind_ok_eq hur]
    unfold U64ScalarValueData.update_range
    rw [new64_eq_ok]
    exact ⟨_, rfl, pureNew64_valid _ _ _ _ _⟩

theorem add_u64_ok_valid (a : U64ScalarValueData) (rhs : Std.U64) :
    ∃ res, U64ScalarValueData.add_u64 a rhs = ok res ∧ Scalar64Valid res := by
  unfold U64ScalarValueData.add_u64
  rw [bind_ok_eq (from_known_64_eq rhs)]
  exact add_scalar_64_ok_valid a (knownScalar64 rhs)

theorem add_i16_ok_valid (a : U64ScalarValueData) (rhs : Std.I16) :
    ∃ res, U64ScalarValueData.add_i16 a rhs = ok res ∧ Scalar64Valid res := by
  unfold U64ScalarValueData.add_i16 lift
  rw [bind_ok_eq rfl, bind_ok_eq (from_known_64_eq (IScalar.hcast .U64 rhs))]
  exact add_scalar_64_ok_valid a _

private theorem cast_512_usize_eq_u64 : UScalar.cast .U64 512#usize = 512#u64 := by
  apply (u64_eq_iff_bv _ _).mpr
  change (UScalar.cast .U64 512#usize).bv = (512#u64).bv
  rw [uscalar_cast_u64_of_usize_bv, usize_const_val_512]
  rfl

theorem StackOffset_default_spec :
    verifier.StackOffset.Insts.CoreDefaultDefault.default = ok (knownScalar64 512#u64) := by
  unfold verifier.StackOffset.Insts.CoreDefaultDefault.default
  rw [api_BPF_STACK_SIZE_eq]
  simp only [lift_eq_ok, bind_ok, cast_512_usize_eq_u64, from_known_64_eq]

theorem StackOffset_new_spec (s : U64ScalarValueData) :
    verifier.StackOffset.new s = ok s := rfl

theorem StackOffset_reg_spec (s : verifier.StackOffset) :
    verifier.StackOffset.reg s = ok s := rfl

theorem StackOffset_from_u64_spec (v : Std.U64) :
    verifier.StackOffset.from_u64 v = ok (knownScalar64 v) := by
  unfold verifier.StackOffset.from_u64
  rw [from_known_64_eq]
  simp only [bind_ok]

theorem StackOffset_add_u64_ok_valid (s : verifier.StackOffset) (rhs : Std.U64) :
    ∃ res, verifier.StackOffset.add_u64 s rhs = ok res ∧ Scalar64Valid res := by
  unfold verifier.StackOffset.add_u64
  obtain ⟨res, hres, hval⟩ := add_u64_ok_valid s rhs
  rw [bind_ok_eq hres]
  exact ⟨res, rfl, hval⟩

theorem StackOffset_add_u64_sound_2safety (s : verifier.StackOffset) (rhs : Std.U64)
    {x₁ x₂ : BitVec 64} (hs : ConcretizesScalar64Rel2 s x₁ x₂) :
    ∃ res, verifier.StackOffset.add_u64 s rhs = ok res ∧
      ConcretizesScalar64Rel2 res (x₁ + rhs.bv) (x₂ + rhs.bv) := by
  unfold verifier.StackOffset.add_u64
  obtain ⟨res, hres, hrel⟩ := add_u64_sound_2safety s rhs hs
  rw [bind_ok_eq hres]
  exact ⟨res, rfl, hrel⟩

theorem StackOffset_add_i16_ok_valid (s : verifier.StackOffset) (rhs : Std.I16) :
    ∃ res, verifier.StackOffset.add_i16 s rhs = ok res ∧ Scalar64Valid res := by
  unfold verifier.StackOffset.add_i16
  obtain ⟨res, hres, hval⟩ := add_i16_ok_valid s rhs
  rw [bind_ok_eq hres]
  exact ⟨res, rfl, hval⟩

theorem StackOffset_add_i16_sound_2safety (s : verifier.StackOffset) (rhs : Std.I16)
    {x₁ x₂ : BitVec 64} (hs : ConcretizesScalar64Rel2 s x₁ x₂) :
    ∃ res, verifier.StackOffset.add_i16 s rhs = ok res ∧
      ConcretizesScalar64Rel2 res (x₁ + sextOffset16 rhs.bv) (x₂ + sextOffset16 rhs.bv) := by
  unfold verifier.StackOffset.add_i16 sextOffset16
  obtain ⟨res, hres, hrel⟩ := add_i16_sound_2safety s rhs hs
  rw [bind_ok_eq hres]
  exact ⟨res, rfl, hrel⟩

theorem StackOffset_check_valid_offset_spec (s : verifier.StackOffset) :
    ∃ r : core.result.Result Unit error.VerifierErrorKind,
      verifier.StackOffset.check_valid_offset s = ok r ∧
      (∀ u, r = .Ok u ↔ (s.unknown_mask = 0#u64 ∧ s.value.bv < 512#64)) := by
  unfold verifier.StackOffset.check_valid_offset U64ScalarValueData.is_known
  rw [api_BPF_STACK_SIZE_eq]
  simp only [bind_ok]
  by_cases hkn : s.unknown_mask = 0#u64
  · simp only [hkn, decide_true, ↓reduceIte, lift_eq_ok, bind_ok, cast_512_usize_eq_u64]
    by_cases hge : s.value >= 512#u64
    · have hnlt : ¬ (s.value.bv < 512#64) := by
        have h1 : 512#64 ≤ s.value.bv := (u64_ge_iff_bv s.value 512#u64).mp hge
        rw [BitVec.le_def] at h1
        rw [BitVec.lt_def]
        omega
      simp only [hge, ↓reduceIte]
      exact ⟨.Err .StackAccessOutOfBounds, rfl, fun _ => by simp [hnlt]⟩
    · have hlt : s.value.bv < 512#64 := by
        have h1 : ¬ (512#64 ≤ s.value.bv) := fun hc => hge ((u64_ge_iff_bv s.value 512#u64).mpr hc)
        rw [BitVec.le_def] at h1
        rw [BitVec.lt_def]
        omega
      simp only [hge, ↓reduceIte]
      exact ⟨.Ok (), rfl, fun u => by cases u; simp [hlt]⟩
  · simp only [hkn, decide_false, Bool.false_eq_true, ↓reduceIte]
    exact ⟨.Err .VariableStackOffset, rfl, fun _ => by simp⟩

theorem StackOffset_is_within_stack_spec (s : verifier.StackOffset) :
    verifier.StackOffset.is_within_stack s =
      ok (decide (s.unknown_mask = 0#u64 ∧ s.value.bv ≤ 512#64)) := by
  unfold verifier.StackOffset.is_within_stack U64ScalarValueData.is_known
  rw [api_BPF_STACK_SIZE_eq]
  simp only [bind_ok]
  by_cases hkn : s.unknown_mask = 0#u64
  · simp only [hkn, decide_true, ↓reduceIte, lift_eq_ok, bind_ok, cast_512_usize_eq_u64, true_and]
    exact congrArg ok (decide_eq_decide.mpr (u64_le_iff_bv s.value 512#u64))
  · simp [hkn]

theorem StackOffset_array_index_spec {s : verifier.StackOffset}
    (hle : s.value.bv ≤ 512#64) :
    ∃ idx : Std.Usize,
      verifier.StackOffset.array_index s = ok idx ∧
      idx.val = s.value.bv.toNat / 8 := by
  unfold verifier.StackOffset.array_index
  rw [verifier_STACK_ELEMENT_SIZE_eq]
  simp only [lift_eq_ok, bind_ok]
  have hval_le : s.value.val ≤ Usize.max := by
    have hval_eq : s.value.val = s.value.bv.toNat := rfl
    rw [hval_eq]
    have h512 : s.value.bv.toNat ≤ 512 := by
      rw [BitVec.le_def] at hle
      simpa using hle
    have h512_max : 512 ≤ Usize.max := by
      rcases Usize.bounds_eq with h | h <;> rw [h] <;> simp [U32.max_eq, U64.max_eq]
    omega
  obtain ⟨idx, hdiv, hdiv_val⟩ := usize_div_eq_ok (UScalar.cast .Usize s.value) 8#usize
    (by rw [usize_const_val_8]; omega)
  refine ⟨idx, hdiv, ?_⟩
  rw [hdiv_val, uscalar_cast_usize_of_u64_val hval_le, usize_const_val_8, UScalar.bv_toNat]

theorem StackOffset_sub_index_spec {s : verifier.StackOffset}
    (hle : s.value.bv ≤ 512#64) :
    ∃ sub : Std.Usize,
      verifier.StackOffset.sub_index s = ok sub ∧
      sub.val = s.value.bv.toNat % 8 ∧
      sub.val < 8 := by
  unfold verifier.StackOffset.sub_index
  rw [verifier_STACK_ELEMENT_SIZE_eq]
  simp only [lift_eq_ok, bind_ok]
  have hval_le : s.value.val ≤ Usize.max := by
    have hval_eq : s.value.val = s.value.bv.toNat := rfl
    rw [hval_eq]
    have h512 : s.value.bv.toNat ≤ 512 := by
      rw [BitVec.le_def] at hle
      simpa using hle
    have h512_max : 512 ≤ Usize.max := by
      rcases Usize.bounds_eq with h | h <;> rw [h] <;> simp [U32.max_eq, U64.max_eq]
    omega
  obtain ⟨sub, hrem, hrem_val⟩ := usize_rem_eq_ok (UScalar.cast .Usize s.value) 8#usize
    (by rw [usize_const_val_8]; omega)
  have heq : sub.val = s.value.bv.toNat % 8 := by
    rw [hrem_val, uscalar_cast_usize_of_u64_val hval_le, usize_const_val_8, UScalar.bv_toNat]
  exact ⟨sub, hrem, heq, by omega⟩

/-! ## 2. `Stack` Core Container Operations -/

theorem Stack_bit_spec {index : Std.Usize} (hlt : index.val < 64) :
    verifier.Stack.bit index = ok ⟨(1#64) <<< index.val⟩ := by
  unfold verifier.Stack.bit
  have hlt_u : index < 64#usize := by
    change index.val < (64#usize).val
    rwa [usize_const_val_64]
  have hm : massert (index < 64#usize) = ok () := by simp [massert, hlt_u]
  rw [bind_ok_eq hm]
  simp only [lift_eq_ok, bind_ok, u64_wrapping_shl_eq]
  congr 1
  apply (u64_eq_iff_bv _ _).mpr
  change (1#64) <<< (UScalar.cast .U32 index).bv.setWidth 6 = (1#64) <<< index.val
  rw [BitVec.shiftLeft_eq']
  have hcast : (UScalar.cast .U32 index).val = index.val :=
    usize_cast_u32_val_of_lt index (by omega)
  have hmod : ((UScalar.cast .U32 index).bv.setWidth 6).toNat = index.val := by
    rw [BitVec.toNat_setWidth, UScalar.bv_toNat, hcast]
    exact Nat.mod_eq_of_lt hlt
  rw [hmod]

theorem Stack_position_spec (self : verifier.Stack) {index : Std.Usize} (hlt : index.val < 64) :
    ∃ pos : Std.Usize,
      verifier.Stack.position self index = ok pos ∧
      pos.val = stackPos self.mask.bv index.val := by
  unfold verifier.Stack.position
  rw [bind_ok_eq (Stack_bit_spec hlt)]
  have hpow_lt : 2 ^ index.val < 2 ^ 64 := Nat.pow_lt_pow_right (by omega) hlt
  have hshl_val : (⟨(1#64) <<< index.val⟩ : Std.U64).val = 2 ^ index.val := by
    rw [← UScalar.bv_toNat, BitVec.toNat_shiftLeft]
    change (1 <<< index.val) % 2 ^ 64 = 2 ^ index.val
    rw [Nat.one_shiftLeft, Nat.mod_eq_of_lt hpow_lt]
  have h1_le : (1#u64).val ≤ (⟨(1#64) <<< index.val⟩ : Std.U64).val := by
    rw [hshl_val]; exact Nat.one_le_two_pow
  have hsub := uscalar_sub_eq_ok (⟨(1#64) <<< index.val⟩ : Std.U64) 1#u64 h1_le
  simp only [u64_one_uscalar_bv] at hsub
  rw [bind_ok_eq hsub]
  simp only [lift_eq_ok, bind_ok]
  obtain ⟨pos, hpop, hpop_val⟩ := popcount_u64_spec (self.mask &&& ⟨((1#64) <<< index.val) - 1#64⟩)
  refine ⟨pos, hpop, ?_⟩
  rw [hpop_val]
  unfold popcount64 stackPos
  have hsuff : ∀ n ≤ 64,
      popcountRange (self.mask.bv &&& (((1#64) <<< index.val) - 1#64)) n =
        if n ≤ index.val then popcountRange self.mask.bv n else popcountRange self.mask.bv index.val := by
    intro n hn
    induction n with
    | zero => simp [popcountRange]
    | succ m ih =>
      simp only [popcountRange, BitVec.getLsbD_and, bv64_prefix_mask_getLsbD hlt]
      have ihm := ih (by omega)
      by_cases hmi : m < index.val
      · have hle1 : m + 1 ≤ index.val := by omega
        have hle0 : m ≤ index.val := by omega
        simp [hmi, hle1, hle0] at ihm ⊢
        rw [ihm]
      · have hnle1 : ¬ (m + 1 ≤ index.val) := by omega
        simp only [hmi, decide_false, Bool.and_false, ↓reduceIte, hnle1]
        by_cases hmeq : m = index.val
        · subst hmeq; simpa using ihm
        · have hnle0 : ¬ (m ≤ index.val) := by omega
          simpa [hnle0] using ihm
  have h64 := hsuff 64 (Nat.le_refl 64)
  have hnle : ¬ (64 ≤ index.val) := by omega
  simpa [hnle] using h64

theorem Stack_default_spec {env : ExecEnv} {vCounter : Nat} :
    ∃ s : verifier.Stack,
      verifier.Stack.Insts.CoreDefaultDefault.default = ok s ∧
      StackValid env vCounter s ∧
      (∀ slot, stackSlotType s slot = .ScalarValue uninitScalar64) := by
  refine ⟨{ mask := 0#u64, data := alloc.vec.Vec.new verifier.Type }, rfl, ?_, ?_⟩
  · refine ⟨?_, fun _ h => by simp at h⟩
    simp [popcount64, popcountRange_zero]
  · intro slot
    unfold stackSlotType
    simp [BitVec.getLsbD_zero]

private theorem clone_types_loop_spec (s : Slice verifier.Type) :
    ∀ (n : Nat) (out : alloc.vec.Vec verifier.Type) (i : Std.Usize),
      s.val.length - i.val = n →
      i.val ≤ s.val.length →
      out.val = s.val.take i.val →
      ∃ out', verifier.clone_types_loop s out i = ok out' ∧ out'.val = s.val := by
  intro n
  induction n with
  | zero =>
    intro out i hrem hle hout
    have hi : i.val = s.val.length := by omega
    unfold verifier.clone_types_loop
    rw [loop_unfold_step]
    unfold verifier.clone_types_loop.body
    have hnlt : ¬ (i < Slice.len s) := by
      change ¬ (i.val < s.val.length); omega
    simp only [hnlt, ↓reduceIte, bind_ok]
    exact ⟨out, rfl, by rw [hout, hi, List.take_length]⟩
  | succ k ih =>
    intro out i hrem hle hout
    have hlt_val : i.val < s.val.length := by omega
    have hlt : i < Slice.len s := hlt_val
    unfold verifier.clone_types_loop
    rw [loop_unfold_step]
    unfold verifier.clone_types_loop.body
    simp only [hlt, ↓reduceIte]
    rw [bind_ok_eq (slice_index_usize_eq_ok s i hlt_val),
        bind_ok_eq (Type_clone_spec _)]
    have hs_max : s.val.length ≤ Usize.max := s.property
    have hlen_lt_max : out.val.length < Usize.max := by
      rw [hout, List.length_take_of_le hle]
      omega
    obtain ⟨out1, hpush, hout1⟩ := vec_push_eq_ok out (s.val[i.val]'hlt_val) hlen_lt_max
    obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by
      have := usize_one_val; omega)
    rw [usize_one_val] at hi1_val
    rw [bind_ok_eq hpush, bind_ok_eq hi1]
    simp only [bind_ok]
    obtain ⟨out', hout', hval'⟩ := ih out1 i1 (by omega) (by omega) (by
      rw [hout1, hout, hi1_val, List.take_add_one, List.getElem?_eq_getElem hlt_val]; simp)
    unfold verifier.clone_types_loop verifier.clone_types_loop.body at hout'
    exact ⟨out', hout', hval'⟩

theorem clone_types_spec (s : Slice verifier.Type) :
    ∃ v : alloc.vec.Vec verifier.Type,
      verifier.clone_types s = ok v ∧ v.val = s.val := by
  unfold verifier.clone_types
  exact clone_types_loop_spec s s.val.length (alloc.vec.Vec.new verifier.Type) 0#usize
    (by simp) (Nat.zero_le _) (by simp)

theorem Stack_clone_spec (s : verifier.Stack) :
    verifier.Stack.Insts.CoreCloneClone.clone s = ok s := by
  unfold verifier.Stack.Insts.CoreCloneClone.clone
  obtain ⟨v, hv, hval⟩ := clone_types_spec (alloc.vec.Vec.deref s.data)
  rw [bind_ok_eq hv]
  have hd : v = s.data := alloc.vec.Vec.ext v s.data (by simpa using hval)
  subst hd
  rfl

private theorem Stack_eq_loop_spec (v v1 : alloc.vec.Vec verifier.Type)
    (hlen : v.val.length = v1.val.length) :
    ∀ (n : Nat) (i : Std.Usize),
      v.val.length - i.val = n →
      i.val ≤ v.val.length →
      ∃ b : Bool,
        verifier.Stack.Insts.CoreCmpPartialEqStack.eq_loop v v1 i = ok b ∧
        (b = true ↔ ∀ j (hj : j < v.val.length), i.val ≤ j →
          v.val[j]'hj = v1.val[j]'(hlen ▸ hj)) := by
  intro n
  induction n with
  | zero =>
    intro i hrem hle
    have hi : i.val = v.val.length := by omega
    unfold verifier.Stack.Insts.CoreCmpPartialEqStack.eq_loop
    rw [loop_unfold_step]
    unfold verifier.Stack.Insts.CoreCmpPartialEqStack.eq_loop.body
    have hnlt : ¬ (i < alloc.vec.Vec.len v) := by change ¬ (i.val < v.val.length); omega
    simp only [hnlt, ↓reduceIte, bind_ok]
    exact ⟨true, rfl, ⟨fun _ _ hj hge => by omega, fun _ => rfl⟩⟩
  | succ k ih =>
    intro i hrem hle
    have hlt : i.val < v.val.length := by omega
    have hlt1 : i.val < v1.val.length := by omega
    have hlt_u : i < alloc.vec.Vec.len v := hlt
    unfold verifier.Stack.Insts.CoreCmpPartialEqStack.eq_loop
    rw [loop_unfold_step]
    unfold verifier.Stack.Insts.CoreCmpPartialEqStack.eq_loop.body
    simp only [hlt_u, ↓reduceIte]
    rw [bind_ok_eq (vec_index_usize_eq_ok v i hlt),
        bind_ok_eq (vec_index_usize_eq_ok v1 i hlt1)]
    obtain ⟨bne, hbne, hne_iff⟩ := Type_ne_spec (v.val[i.val]'hlt) (v1.val[i.val]'hlt1)
    rw [bind_ok_eq hbne]
    cases bne
    · simp only [Bool.false_eq_true, ↓reduceIte]
      have heq_i : v.val[i.val]'hlt = v1.val[i.val]'hlt1 := by
        by_contra hc; exact absurd (hne_iff.mpr hc) (by simp)
      have hv_max : v.val.length ≤ Usize.max := v.property
      obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by
        have := usize_one_val; omega)
      rw [usize_one_val] at hi1_val
      rw [bind_ok_eq hi1]
      simp only [bind_ok]
      obtain ⟨b, hb, hb_iff⟩ := ih i1 (by omega) (by omega)
      unfold verifier.Stack.Insts.CoreCmpPartialEqStack.eq_loop
        verifier.Stack.Insts.CoreCmpPartialEqStack.eq_loop.body at hb
      refine ⟨b, hb, ?_⟩
      rw [hb_iff]
      constructor
      · intro hrest j hj hge
        by_cases hji : j = i.val
        · subst hji; exact heq_i
        · exact hrest j hj (by omega)
      · intro hall j hj hge
        exact hall j hj (by omega)
    · simp only [↓reduceIte, bind_ok]
      have hne_i : v.val[i.val]'hlt ≠ v1.val[i.val]'hlt1 := hne_iff.mp rfl
      refine ⟨false, rfl, ?_⟩
      simp only [Bool.false_eq_true, false_iff]
      intro hall
      exact hne_i (hall i.val hlt (Nat.le_refl _))

theorem Stack_eq_spec (a b : verifier.Stack) :
    ∃ r : Bool,
      verifier.Stack.Insts.CoreCmpPartialEqStack.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  cases a with | mk am ad =>
  cases b with | mk bm bd =>
  unfold verifier.Stack.Insts.CoreCmpPartialEqStack.eq
  by_cases hm : am = bm
  · subst hm
    simp only [bne_self_eq_false, Bool.false_eq_true, ↓reduceIte]
    by_cases hlen : ad.len = bd.len
    · have hlen_val : ad.val.length = bd.val.length := congrArg UScalar.val hlen
      simp only [hlen, bne_self_eq_false, Bool.false_eq_true, ↓reduceIte]
      obtain ⟨r, hr, hiff⟩ := Stack_eq_loop_spec ad bd hlen_val ad.val.length 0#usize
        (by simp) (Nat.zero_le _)
      refine ⟨r, hr, ?_⟩
      rw [hiff]
      constructor
      · intro hall
        have hd : ad = bd := alloc.vec.Vec.ext ad bd
          (List.ext_getElem hlen_val (fun j hj _ => hall j hj (Nat.zero_le _)))
        subst hd; rfl
      · rintro ⟨rfl⟩ _ _ _
        rfl
    · have hbne : (ad.len != bd.len) = true := bne_iff_ne.mpr hlen
      refine ⟨false, by simp [hbne], ?_⟩
      simp only [Bool.false_eq_true, false_iff]
      rintro ⟨rfl⟩
      exact hlen rfl
  · have hbne : (am != bm) = true := bne_iff_ne.mpr hm
    refine ⟨false, by simp [hbne], ?_⟩
    simp [hm]

private theorem u64_and_bit_eq_zero_iff (m : Std.U64) {idx : Nat} (hlt : idx < 64) :
    (m &&& ⟨(1#64) <<< idx⟩ = 0#u64) ↔ m.bv.getLsbD idx = false := by
  rw [u64_eq_iff_bv]
  change (m.bv &&& ((1#64) <<< idx) = 0#64) ↔ m.bv.getLsbD idx = false
  constructor
  · intro h
    have hbit := congrArg (fun b : BitVec 64 => b.getLsbD idx) h
    rw [BitVec.getLsbD_and, bv64_one_shl_getLsbD hlt hlt, BitVec.getLsbD_zero] at hbit
    simpa using hbit
  · intro hf
    apply BitVec.eq_of_getLsbD_eq
    intro i hi
    rw [BitVec.getLsbD_and, bv64_one_shl_getLsbD hlt hi, BitVec.getLsbD_zero]
    by_cases heq : i = idx
    · subst heq; simp [hf]
    · simp [heq]

theorem Stack_get_spec {env : ExecEnv} {vCounter : Nat} {self : verifier.Stack}
    (hs : StackValid env vCounter self) {index : Std.Usize} (hlt : index.val < 64) :
    verifier.Stack.get self index = ok (stackSlotType self index.val) := by
  unfold verifier.Stack.get stackSlotType
  rw [bind_ok_eq (Stack_bit_spec hlt)]
  simp only [lift_eq_ok, bind_ok]
  by_cases hbit : self.mask.bv.getLsbD index.val = true
  · have hnz : ¬ (self.mask &&& ⟨(1#64) <<< index.val⟩ = 0#u64) := by
      rw [u64_and_bit_eq_zero_iff self.mask hlt]; simp [hbit]
    simp only [hnz, ↓reduceIte, hbit]
    obtain ⟨pos, hpos, hpos_val⟩ := Stack_position_spec self hlt
    rw [bind_ok_eq hpos]
    have hpos_lt : pos.val < self.data.val.length := by
      rw [hpos_val, hs.1]
      exact stackPos_lt_popcount64 self.mask.bv hlt hbit
    rw [vec_index_usize_eq_ok self.data pos hpos_lt]
    congr 1
    rw [← List.getD_eq_getElem self.data.val _ hpos_lt, hpos_val]
  · have hbf : self.mask.bv.getLsbD index.val = false := Bool.eq_false_iff.mpr hbit
    have hz : self.mask &&& ⟨(1#64) <<< index.val⟩ = 0#u64 :=
      (u64_and_bit_eq_zero_iff self.mask hlt).mpr hbf
    simp only [hz, ↓reduceIte, hbf, Bool.false_eq_true, Type_UNINITIALIZED_eq]

theorem vec_index_mut_usize_eq_ok {α : Type u} (v : alloc.vec.Vec α) (idx : Std.Usize)
    (hlt : idx.val < v.val.length) :
    ∃ back : α → alloc.vec.Vec α,
      alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice α) v idx =
        ok (v.val[idx.val]'hlt, back) ∧
      ∀ u, (back u).val = v.val.set idx.val u := by
  unfold alloc.vec.Vec.index_mut core.slice.index.SliceIndexUsizeSlice
    core.slice.index.Usize.index_mut Slice.index_mut_usize
  simp only [slice_index_usize_eq_ok v.slice idx hlt, bind_tc_ok]
  exact ⟨fun u => { slice := v.slice.set idx u }, rfl, fun u => Slice.set_val_eq v.slice idx u⟩

private theorem Stack_set_loop0_spec (v : alloc.vec.Vec verifier.Type) (pos : Std.Usize)
    (hpos : pos.val < v.val.length) :
    ∀ (n : Nat) (acc : alloc.vec.Vec verifier.Type) (i : Std.Usize),
      v.val.length - i.val = n →
      i.val ≤ v.val.length →
      acc.val =
        (if i.val ≤ pos.val then v.val.take i.val
         else v.val.take pos.val ++ (v.val.drop (pos.val + 1)).take (i.val - (pos.val + 1))) →
      ∃ out,
        verifier.Stack.set_loop0 v pos acc i = ok out ∧
        out.val = v.val.take pos.val ++ v.val.drop (pos.val + 1) := by
  intro n
  induction n with
  | zero =>
    intro acc i hrem hle hacc
    have hi : i.val = v.val.length := by omega
    have hnlt : ¬ (i < alloc.vec.Vec.len v) := by change ¬ (i.val < v.val.length); omega
    unfold verifier.Stack.set_loop0
    rw [loop_unfold_step]
    unfold verifier.Stack.set_loop0.body
    simp only [hnlt, ↓reduceIte, bind_ok]
    refine ⟨acc, rfl, ?_⟩
    have hnle : ¬ (i.val ≤ pos.val) := by omega
    rw [hacc]
    simp only [hnle, ↓reduceIte]
    rw [hi]
    congr 1
    exact List.take_of_length_le (by simp)
  | succ k ih =>
    intro acc i hrem hle hacc
    have hlt_v : i.val < v.val.length := by omega
    have hlt : i < alloc.vec.Vec.len v := hlt_v
    have hv_max : v.val.length ≤ Usize.max := v.property
    obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by
      rw [usize_one_val]; omega)
    rw [usize_one_val] at hi1_val
    unfold verifier.Stack.set_loop0
    rw [loop_unfold_step]
    unfold verifier.Stack.set_loop0.body
    simp only [hlt, ↓reduceIte]
    by_cases hip : i = pos
    · subst hip
      simp only [bne_self_eq_false, Bool.false_eq_true, ↓reduceIte, bind_ok]
      rw [bind_ok_eq hi1]
      simp only [bind_ok]
      obtain ⟨out, hout, hout_val⟩ := ih acc i1 (by omega) (by omega) (by
        rw [hacc]
        split_ifs with h1 h2
        · omega
        · have hsub0 : i1.val - (i.val + 1) = 0 := by omega
          simp [hsub0]
        · omega
        · omega)
      unfold verifier.Stack.set_loop0 verifier.Stack.set_loop0.body at hout
      exact ⟨out, hout, hout_val⟩
    · have hbne : (i != pos) = true := bne_iff_ne.mpr hip
      have hip_val : i.val ≠ pos.val := fun hc => hip (usize_eq_of_val_eq hc)
      simp only [hbne, ↓reduceIte]
      have hacc_lt : acc.val.length < Usize.max := by
        rw [hacc]
        split_ifs with hle_ip
        · rw [List.length_take_of_le (by omega)]; omega
        · rw [List.length_append, List.length_take_of_le (by omega),
              List.length_take_of_le (by simp; omega)]
          omega
      obtain ⟨acc1, hpush, hacc1⟩ := vec_push_eq_ok acc (v.val[i.val]'hlt_v) hacc_lt
      have hinner :
          (do let t ← alloc.vec.Vec.index (core.slice.index.SliceIndexUsizeSlice verifier.Type) v i
              let t1 ← verifier.Type.Insts.CoreCloneClone.clone t
              alloc.vec.Vec.push acc t1) = ok acc1 := by
        rw [bind_ok_eq (vec_index_usize_eq_ok v i hlt_v),
            bind_ok_eq (Type_clone_spec _), hpush]
      rw [bind_ok_eq hinner, bind_ok_eq hi1]
      simp only [bind_ok]
      obtain ⟨out, hout, hout_val⟩ := ih acc1 i1 (by omega) (by omega) (by
        rw [hacc1, hacc]
        by_cases hlt_ip : i.val < pos.val
        · split_ifs with h1 h2
          · rw [hi1_val, List.take_add_one, List.getElem?_eq_getElem hlt_v]; simp
          · omega
          · omega
          · omega
        · have hgt_ip : pos.val < i.val := by omega
          split_ifs with h1 h2
          · omega
          · omega
          · omega
          · have hsub1 : i1.val - (pos.val + 1) = (i.val - (pos.val + 1)) + 1 := by omega
            have hd_len : (v.val.drop (pos.val + 1)).length = v.val.length - (pos.val + 1) :=
              List.length_drop
            have hd_lt : i.val - (pos.val + 1) < (v.val.drop (pos.val + 1)).length := by omega
            rw [hsub1, List.take_add_one, List.getElem?_eq_getElem hd_lt, Option.toList_some,
                List.append_assoc]
            have hidx_eq : pos.val + 1 + (i.val - (pos.val + 1)) = i.val := by omega
            simp only [List.getElem_drop, hidx_eq])
      unfold verifier.Stack.set_loop0 verifier.Stack.set_loop0.body at hout
      exact ⟨out, hout, hout_val⟩

private theorem Stack_set_loop1_spec (v : alloc.vec.Vec verifier.Type) (pos : Std.Usize)
    (hpos : pos.val ≤ v.val.length) :
    ∀ (n : Nat) (acc : alloc.vec.Vec verifier.Type) (i : Std.Usize),
      pos.val - i.val = n →
      i.val ≤ pos.val →
      acc.val = v.val.take i.val →
      ∃ out,
        verifier.Stack.set_loop1 v pos acc i = ok (out, pos) ∧
        out.val = v.val.take pos.val := by
  intro n
  induction n with
  | zero =>
    intro acc i hrem hle hacc
    have hi_eq : i = pos := usize_eq_of_val_eq (by omega)
    subst hi_eq
    have hnlt : ¬ (i < i) := by change ¬ (i.val < i.val); omega
    unfold verifier.Stack.set_loop1
    rw [loop_unfold_step]
    unfold verifier.Stack.set_loop1.body
    simp only [hnlt, ↓reduceIte, bind_ok]
    exact ⟨acc, rfl, hacc⟩
  | succ k ih =>
    intro acc i hrem hle hacc
    have hlt_ip : i.val < pos.val := by omega
    have hlt_v : i.val < v.val.length := by omega
    have hlt : i < pos := hlt_ip
    have hv_max : v.val.length ≤ Usize.max := v.property
    have hacc_lt : acc.val.length < Usize.max := by
      rw [hacc, List.length_take_of_le (by omega)]; omega
    obtain ⟨acc1, hpush, hacc1⟩ := vec_push_eq_ok acc (v.val[i.val]'hlt_v) hacc_lt
    obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by
      have := usize_one_val; omega)
    rw [usize_one_val] at hi1_val
    unfold verifier.Stack.set_loop1
    rw [loop_unfold_step]
    unfold verifier.Stack.set_loop1.body
    simp only [hlt, ↓reduceIte]
    rw [bind_ok_eq (vec_index_usize_eq_ok v i hlt_v),
        bind_ok_eq (Type_clone_spec _),
        bind_ok_eq hpush, bind_ok_eq hi1]
    simp only [bind_ok]
    obtain ⟨out, hout, hout_val⟩ := ih acc1 i1 (by omega) (by omega) (by
      rw [hacc1, hacc, hi1_val, List.take_add_one, List.getElem?_eq_getElem hlt_v]; simp)
    unfold verifier.Stack.set_loop1 verifier.Stack.set_loop1.body at hout
    exact ⟨out, hout, hout_val⟩

private theorem Stack_set_loop2_spec (v : alloc.vec.Vec verifier.Type) :
    ∀ (n : Nat) (acc : alloc.vec.Vec verifier.Type) (i : Std.Usize),
      v.val.length - i.val = n →
      i.val ≤ v.val.length →
      acc.val.length + (v.val.length - i.val) ≤ Usize.max →
      ∃ out,
        verifier.Stack.set_loop2 v acc i = ok out ∧
        out.val = acc.val ++ v.val.drop i.val := by
  intro n
  induction n with
  | zero =>
    intro acc i hrem hle _
    have hi : i.val = v.val.length := by omega
    have hnlt : ¬ (i < alloc.vec.Vec.len v) := by change ¬ (i.val < v.val.length); omega
    unfold verifier.Stack.set_loop2
    rw [loop_unfold_step]
    unfold verifier.Stack.set_loop2.body
    simp only [hnlt, ↓reduceIte, bind_ok]
    exact ⟨acc, rfl, by rw [hi, List.drop_length, List.append_nil]⟩
  | succ k ih =>
    intro acc i hrem hle hcap
    have hlt_v : i.val < v.val.length := by omega
    have hlt : i < alloc.vec.Vec.len v := hlt_v
    have hv_max : v.val.length ≤ Usize.max := v.property
    obtain ⟨acc1, hpush, hacc1⟩ := vec_push_eq_ok acc (v.val[i.val]'hlt_v) (by omega)
    obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by
      have := usize_one_val; omega)
    rw [usize_one_val] at hi1_val
    have hacc1_len : acc1.val.length = acc.val.length + 1 := by simp [hacc1]
    unfold verifier.Stack.set_loop2
    rw [loop_unfold_step]
    unfold verifier.Stack.set_loop2.body
    simp only [hlt, ↓reduceIte]
    rw [bind_ok_eq (vec_index_usize_eq_ok v i hlt_v),
        bind_ok_eq (Type_clone_spec _),
        bind_ok_eq hpush, bind_ok_eq hi1]
    simp only [bind_ok]
    obtain ⟨out, hout, hout_val⟩ := ih acc1 i1 (by omega) (by omega) (by omega)
    unfold verifier.Stack.set_loop2 verifier.Stack.set_loop2.body at hout
    refine ⟨out, hout, ?_⟩
    rw [hout_val, hacc1, List.append_assoc, List.singleton_append, hi1_val,
        ← List.drop_eq_getElem_cons hlt_v]

theorem Stack_set_spec {env : ExecEnv} {vCounter : Nat} {self : verifier.Stack}
    (hs : StackValid env vCounter self) {index : Std.Usize} (hlt : index.val < 64)
    {t : verifier.Type} (ht : TypeValid env vCounter t) :
    ∃ s' : verifier.Stack,
      verifier.Stack.set self index t = ok s' ∧
      StackValid env vCounter s' ∧
      (∀ slot < 64,
        stackSlotType s' slot = if slot = index.val then t else stackSlotType self slot) := by
  unfold verifier.Stack.set
  rw [bind_ok_eq (Stack_bit_spec hlt)]
  obtain ⟨pos, hpos, hpos_val⟩ := Stack_position_spec self hlt
  rw [bind_ok_eq hpos]
  simp only [lift_eq_ok, bind_ok, Type_UNINITIALIZED_eq]
  obtain ⟨beq, hbeq, heq_iff⟩ := Type_eq_spec t (.ScalarValue uninitScalar64)
  rw [bind_ok_eq hbeq]
  cases beq
  · -- `t ≠ .ScalarValue uninitScalar64`
    have ht_ne : t ≠ .ScalarValue uninitScalar64 := by
      intro hc; exact absurd (heq_iff.mpr hc) (by simp)
    simp only [Bool.false_eq_true, ↓reduceIte]
    by_cases hbit : self.mask.bv.getLsbD index.val = true
    · -- Slot already present in `self.data`: update in place
      have hnz : (self.mask &&& ⟨(1#64) <<< index.val⟩ != 0#u64) = true := by
        rw [bne_iff_ne, ne_eq, u64_and_bit_eq_zero_iff self.mask hlt]; simp [hbit]
      simp only [hnz, ↓reduceIte]
      have hpos_lt : pos.val < self.data.val.length := by
        rw [hpos_val, hs.1]; exact stackPos_lt_popcount64 self.mask.bv hlt hbit
      rw [bind_ok_eq (vec_index_usize_eq_ok self.data pos hpos_lt)]
      obtain ⟨bne, hbne, hne_iff⟩ := Type_ne_spec (self.data.val[pos.val]'hpos_lt) t
      rw [bind_ok_eq hbne]
      cases bne
      · -- Equal: returns `self`
        have heq_cur : self.data.val[pos.val]'hpos_lt = t := by
          by_contra hc; exact absurd (hne_iff.mpr hc) (by simp)
        simp only [Bool.false_eq_true, ↓reduceIte]
        refine ⟨self, rfl, hs, fun slot hslot => ?_⟩
        by_cases hsi : slot = index.val
        · subst hsi
          simp only [↓reduceIte, stackSlotType, hbit, ← hpos_val,
            List.getD_eq_getElem _ _ hpos_lt, heq_cur]
        · simp [hsi]
      · -- Not equal: `index_mut` update
        simp only [↓reduceIte]
        obtain ⟨back, hmut, hback⟩ := vec_index_mut_usize_eq_ok self.data pos hpos_lt
        rw [bind_pair_ok_eq hmut]
        refine ⟨{ self with data := back t }, rfl, ?_, ?_⟩
        · refine ⟨?_, fun x hx => ?_⟩
          · rw [hback t, List.length_set, hs.1]
          · rw [hback t] at hx
            rcases List.mem_or_eq_of_mem_set hx with hmem | rfl
            · exact hs.2 x hmem
            · exact ht
        · intro slot hslot
          by_cases hsi : slot = index.val
          · subst hsi
            simp only [↓reduceIte, stackSlotType, hbit, hback t]
            have hlen_set : (self.data.val.set pos.val t).length = self.data.val.length :=
              List.length_set
            rw [← hpos_val, List.getD_eq_getElem _ _ (hlen_set ▸ hpos_lt),
                List.getElem_set_self]
          · simp only [hsi, ↓reduceIte, stackSlotType, hback t]
            by_cases hbs : self.mask.bv.getLsbD slot = true
            · simp only [hbs, ↓reduceIte]
              have hs_lt : stackPos self.mask.bv slot < self.data.val.length := by
                rw [hs.1]; exact stackPos_lt_popcount64 self.mask.bv hslot hbs
              have hlen_set : (self.data.val.set pos.val t).length = self.data.val.length :=
                List.length_set
              have hpos_ne : pos.val ≠ stackPos self.mask.bv slot := by
                rw [hpos_val]
                unfold stackPos
                by_cases hslt : slot < index.val
                · have := stackPos_lt_popcountRange self.mask.bv hslt hbs; omega
                · have higt : index.val < slot := by omega
                  have := stackPos_lt_popcountRange self.mask.bv higt hbit; omega
              rw [List.getD_eq_getElem _ _ (hlen_set ▸ hs_lt),
                  List.getD_eq_getElem _ _ hs_lt,
                  List.getElem_set_ne hpos_ne]
            · simp [hbs]
    · -- Slot absent in `self.data`: insert via `set_loop1` + `push` + `set_loop2`
      have hbf : self.mask.bv.getLsbD index.val = false := Bool.eq_false_iff.mpr hbit
      have hz : (self.mask &&& ⟨(1#64) <<< index.val⟩ != 0#u64) = false := by
        rw [bne_eq_false_iff_eq, u64_and_bit_eq_zero_iff self.mask hlt]; exact hbf
      simp only [hz, Bool.false_eq_true, ↓reduceIte]
      have hpos_le : pos.val ≤ self.data.val.length := by
        rw [hpos_val, hs.1]; exact popcountRange_mono self.mask.bv (by omega)
      obtain ⟨nd, hloop1, hnd_val⟩ := Stack_set_loop1_spec self.data pos hpos_le
        pos.val (alloc.vec.Vec.new verifier.Type) 0#usize (by simp) (Nat.zero_le _) (by simp)
      rw [bind_pair_ok_eq hloop1]
      have hpop_lt64 : popcountRange self.mask.bv 64 < 64 := by
        have h1 := popcountRange_set_bit_lt hlt (Nat.le_refl 64) hlt hbf
        have h2 := popcountRange_le (self.mask.bv ||| ((1#64) <<< index.val)) 64
        omega
      have hmax64 := usize_max_ge_64
      rw [UScalar.max_USize_eq] at hmax64
      have hs1_len : self.data.val.length = popcountRange self.mask.bv 64 := hs.1
      have hnd_lt_max : nd.val.length < Usize.max := by
        rw [hnd_val, List.length_take_of_le hpos_le]; omega
      obtain ⟨nd1, hpush, hnd1_val⟩ := vec_push_eq_ok nd t hnd_lt_max
      rw [bind_ok_eq hpush]
      have hcap2 : nd1.val.length + (self.data.val.length - pos.val) ≤ Usize.max := by
        rw [hnd1_val, hnd_val, List.length_append, List.length_take_of_le hpos_le]
        simp only [List.length_singleton]
        omega
      obtain ⟨nd2, hloop2, hnd2_val⟩ := Stack_set_loop2_spec self.data
        (self.data.val.length - pos.val) nd1 pos rfl hpos_le hcap2
      rw [bind_ok_eq hloop2]
      let s' : verifier.Stack := { mask := self.mask ||| ⟨(1#64) <<< index.val⟩, data := nd2 }
      have hmask_bv : s'.mask.bv = self.mask.bv ||| ((1#64) <<< index.val) := rfl
      have hnd2_eq : nd2.val = self.data.val.take pos.val ++ [t] ++ self.data.val.drop pos.val := by
        rw [hnd2_val, hnd1_val, hnd_val]
      have hlen_nd2 : nd2.val.length = self.data.val.length + 1 := by
        rw [hnd2_eq, List.length_append, List.length_append, List.length_take_of_le hpos_le,
            List.length_drop, List.length_singleton]
        omega
      refine ⟨s', rfl, ?_, ?_⟩
      · refine ⟨?_, fun x hx => ?_⟩
        · rw [hlen_nd2, hs.1, hmask_bv]
          exact (popcountRange_set_bit_lt hlt (Nat.le_refl 64) hlt hbf).symm
        · rw [hnd2_eq] at hx
          simp only [List.mem_append, List.mem_singleton] at hx
          rcases hx with (hx | rfl) | hx
          · exact hs.2 x (List.mem_of_mem_take hx)
          · exact ht
          · exact hs.2 x (List.mem_of_mem_drop hx)
      · intro slot hslot
        unfold stackSlotType
        rw [hmask_bv, BitVec.getLsbD_or, bv64_one_shl_getLsbD hlt hslot]
        by_cases hsi : slot = index.val
        · subst hsi
          simp only [decide_true, Bool.or_true, ↓reduceIte]
          have hsp : stackPos (self.mask.bv ||| ((1#64) <<< index.val)) index.val = pos.val := by
            unfold stackPos
            rw [popcountRange_set_bit_ge hlt (Nat.le_refl _), hpos_val]
          have htake_len : (self.data.val.take pos.val).length = pos.val :=
            List.length_take_of_le hpos_le
          change nd2.val.getD (stackPos (self.mask.bv ||| ((1#64) <<< index.val)) index.val)
            (.ScalarValue uninitScalar64) = t
          rw [hsp, hnd2_eq, List.append_assoc,
              List.getD_eq_getElem _ _ (by simp [htake_len]),
              List.getElem_append_right (by rw [htake_len])]
          simp [htake_len]
        · simp only [hsi, decide_false, Bool.or_false, ↓reduceIte]
          by_cases hbs : self.mask.bv.getLsbD slot = true
          · simp only [hbs, ↓reduceIte]
            have hs_lt : stackPos self.mask.bv slot < self.data.val.length := by
              rw [hs.1]; exact stackPos_lt_popcount64 self.mask.bv hslot hbs
            have htake_len : (self.data.val.take pos.val).length = pos.val :=
              List.length_take_of_le hpos_le
            by_cases hslt : slot < index.val
            · have hsp : stackPos (self.mask.bv ||| ((1#64) <<< index.val)) slot =
                  stackPos self.mask.bv slot :=
                popcountRange_set_bit_ge hlt (by omega)
              have hslt_pos : stackPos self.mask.bv slot < pos.val := by
                rw [hpos_val]; exact stackPos_lt_popcountRange self.mask.bv hslt hbs
              change nd2.val.getD (stackPos (self.mask.bv ||| ((1#64) <<< index.val)) slot)
                (.ScalarValue uninitScalar64) =
                self.data.val.getD (stackPos self.mask.bv slot) (.ScalarValue uninitScalar64)
              rw [hsp, hnd2_eq, List.append_assoc,
                  List.getD_eq_getElem _ _ (by simp [htake_len]; omega),
                  List.getD_eq_getElem _ _ hs_lt,
                  List.getElem_append_left (by rw [htake_len]; exact hslt_pos),
                  List.getElem_take]
            · have higt : index.val < slot := by omega
              have hsp : stackPos (self.mask.bv ||| ((1#64) <<< index.val)) slot =
                  stackPos self.mask.bv slot + 1 :=
                popcountRange_set_bit_lt hlt (by omega) higt hbf
              have hge_pos : pos.val ≤ stackPos self.mask.bv slot := by
                rw [hpos_val]; exact popcountRange_mono self.mask.bv (by omega)
              have hslt_nd2 : stackPos self.mask.bv slot + 1 < nd2.val.length := by
                rw [hlen_nd2]; omega
              rw [hsp, List.getD_eq_getElem _ _ hslt_nd2, List.getD_eq_getElem _ _ hs_lt]
              have hright : (self.data.val.take pos.val ++ [t] ++ self.data.val.drop pos.val)[stackPos self.mask.bv slot + 1]'(by
                  rw [← hnd2_eq]; exact hslt_nd2) = self.data.val[stackPos self.mask.bv slot]'hs_lt := by
                have hlen1 : (self.data.val.take pos.val ++ [t]).length = pos.val + 1 := by
                  simp [htake_len]
                rw [List.getElem_append_right (by rw [hlen1]; omega)]
                simp only [hlen1, List.getElem_drop]
                congr 1
                omega
              simpa [hnd2_eq] using hright
          · simp [hbs]
  · -- `t = .ScalarValue uninitScalar64`
    have ht_eq : t = .ScalarValue uninitScalar64 := heq_iff.mp rfl
    subst ht_eq
    simp only [↓reduceIte]
    by_cases hbit : self.mask.bv.getLsbD index.val = true
    · -- Remove slot from `self.data` via `set_loop0`
      have hnz : (self.mask &&& ⟨(1#64) <<< index.val⟩ != 0#u64) = true := by
        rw [bne_iff_ne, ne_eq, u64_and_bit_eq_zero_iff self.mask hlt]; simp [hbit]
      simp only [hnz, ↓reduceIte]
      have hpos_lt : pos.val < self.data.val.length := by
        rw [hpos_val, hs.1]; exact stackPos_lt_popcount64 self.mask.bv hlt hbit
      obtain ⟨nd, hloop0, hnd_val⟩ := Stack_set_loop0_spec self.data pos hpos_lt
        self.data.val.length (alloc.vec.Vec.new verifier.Type) 0#usize
        (by simp) (Nat.zero_le _) (by simp)
      rw [bind_ok_eq hloop0]
      let s' : verifier.Stack := { mask := self.mask &&& ~~~⟨(1#64) <<< index.val⟩, data := nd }
      have hmask_bv : s'.mask.bv = self.mask.bv &&& ~~~((1#64) <<< index.val) := rfl
      have hlen_nd : nd.val.length + 1 = self.data.val.length := by
        rw [hnd_val, List.length_append, List.length_take_of_le (by omega), List.length_drop]
        omega
      refine ⟨s', rfl, ?_, ?_⟩
      · refine ⟨?_, fun x hx => ?_⟩
        · have hpop := popcountRange_clear_bit_lt hlt (Nat.le_refl 64) hlt hbit
          change nd.val.length = popcount64 (self.mask.bv &&& ~~~((1#64) <<< index.val))
          have hs1 := hs.1
          unfold popcount64 at hs1 ⊢
          omega
        · rw [hnd_val, List.mem_append] at hx
          rcases hx with hx | hx
          · exact hs.2 x (List.mem_of_mem_take hx)
          · exact hs.2 x (List.mem_of_mem_drop hx)
      · intro slot hslot
        unfold stackSlotType
        rw [hmask_bv, BitVec.getLsbD_and, BitVec.getLsbD_not,
            bv64_one_shl_getLsbD hlt hslot]
        by_cases hsi : slot = index.val
        · subst hsi; simp [hslot]
        · simp only [hsi, decide_false, Bool.not_false, hslot, decide_true,
            Bool.and_true, ↓reduceIte]
          by_cases hbs : self.mask.bv.getLsbD slot = true
          · simp only [hbs, ↓reduceIte]
            have hs_lt : stackPos self.mask.bv slot < self.data.val.length := by
              rw [hs.1]; exact stackPos_lt_popcount64 self.mask.bv hslot hbs
            have htake_len : (self.data.val.take pos.val).length = pos.val :=
              List.length_take_of_le (by omega)
            by_cases hslt : slot < index.val
            · have hsp : stackPos (self.mask.bv &&& ~~~((1#64) <<< index.val)) slot =
                  stackPos self.mask.bv slot :=
                popcountRange_clear_bit_ge hlt (by omega)
              have hslt_pos : stackPos self.mask.bv slot < pos.val := by
                rw [hpos_val]; exact stackPos_lt_popcountRange self.mask.bv hslt hbs
              have hslt_nd : stackPos self.mask.bv slot < nd.val.length := by omega
              rw [hsp, List.getD_eq_getElem _ _ hslt_nd, List.getD_eq_getElem _ _ hs_lt]
              have hleft : (self.data.val.take pos.val ++ self.data.val.drop (pos.val + 1))[stackPos self.mask.bv slot]'(by
                  rw [← hnd_val]; exact hslt_nd) = self.data.val[stackPos self.mask.bv slot]'hs_lt := by
                rw [List.getElem_append_left (by rw [htake_len]; exact hslt_pos),
                    List.getElem_take]
              simpa [hnd_val] using hleft
            · have higt : index.val < slot := by omega
              have hsp := popcountRange_clear_bit_lt hlt (by omega : slot ≤ 64) higt hbit
              have hge_pos : pos.val ≤ stackPos (self.mask.bv &&& ~~~((1#64) <<< index.val)) slot := by
                rw [hpos_val]
                unfold stackPos
                rw [← popcountRange_clear_bit_ge (b := self.mask.bv) hlt (Nat.le_refl _)]
                exact popcountRange_mono _ (by omega)
              have hslt_nd : stackPos (self.mask.bv &&& ~~~((1#64) <<< index.val)) slot < nd.val.length := by
                unfold stackPos at hs_lt ⊢; omega
              rw [List.getD_eq_getElem _ _ hslt_nd, List.getD_eq_getElem _ _ hs_lt]
              have hright : (self.data.val.take pos.val ++ self.data.val.drop (pos.val + 1))[stackPos (self.mask.bv &&& ~~~((1#64) <<< index.val)) slot]'(by
                  rw [← hnd_val]; exact hslt_nd) = self.data.val[stackPos self.mask.bv slot]'hs_lt := by
                rw [List.getElem_append_right (by rw [htake_len]; exact hge_pos),
                    List.getElem_drop]
                congr 1
                unfold stackPos at hge_pos ⊢
                omega
              simpa [hnd_val] using hright
          · simp [hbs]
    · -- Already absent: returns `self`
      have hbf : self.mask.bv.getLsbD index.val = false := Bool.eq_false_iff.mpr hbit
      have hz : (self.mask &&& ⟨(1#64) <<< index.val⟩ != 0#u64) = false := by
        rw [bne_eq_false_iff_eq, u64_and_bit_eq_zero_iff self.mask hlt]; exact hbf
      simp only [hz, Bool.false_eq_true, ↓reduceIte]
      refine ⟨self, rfl, hs, fun slot hslot => ?_⟩
      by_cases hsi : slot = index.val
      · subst hsi; simp [stackSlotType, hbf]
      · simp [hsi]

private theorem Stack_set_null_loop_spec (null_id : MemoryId) (is_null : Bool) :
    ∀ (n : Nat) (cur : verifier.Stack) (i : Std.Usize),
      cur.data.val.length - i.val = n →
      i.val ≤ cur.data.val.length →
      ∃ outData,
        verifier.Stack.set_null_loop cur null_id is_null i = ok (cur.mask, outData) ∧
        outData.val =
          cur.data.val.take i.val ++
            (cur.data.val.drop i.val).map (fun t => pureTypeSetNull t null_id is_null) := by
  intro n
  induction n with
  | zero =>
    intro cur i hrem hle
    have hi : i.val = cur.data.val.length := by omega
    have hnlt : ¬ (i < alloc.vec.Vec.len cur.data) := by
      change ¬ (i.val < cur.data.val.length); omega
    unfold verifier.Stack.set_null_loop
    rw [loop_unfold_step]
    unfold verifier.Stack.set_null_loop.body
    simp only [hnlt, ↓reduceIte, bind_ok]
    exact ⟨cur.data, rfl, by rw [hi, List.take_length, List.drop_length]; simp⟩
  | succ k ih =>
    intro cur i hrem hle
    have hlt_d : i.val < cur.data.val.length := by omega
    have hlt : i < alloc.vec.Vec.len cur.data := hlt_d
    have hd_max : cur.data.val.length ≤ Usize.max := cur.data.property
    obtain ⟨back, hmut, hback⟩ := vec_index_mut_usize_eq_ok cur.data i hlt_d
    obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by
      have := usize_one_val; omega)
    rw [usize_one_val] at hi1_val
    let t1 := pureTypeSetNull (cur.data.val[i.val]'hlt_d) null_id is_null
    let cur1 : verifier.Stack := { cur with data := back t1 }
    have hcur1_len : cur1.data.val.length = cur.data.val.length := by
      simp [cur1, hback t1]
    unfold verifier.Stack.set_null_loop
    rw [loop_unfold_step]
    unfold verifier.Stack.set_null_loop.body
    simp only [hlt, ↓reduceIte]
    rw [bind_pair_ok_eq hmut,
        bind_ok_eq (Type_set_null_spec (cur.data.val[i.val]'hlt_d) null_id is_null),
        bind_ok_eq hi1]
    simp only [bind_ok]
    obtain ⟨outData, hout, hout_val⟩ := ih cur1 i1 (by omega) (by omega)
    unfold verifier.Stack.set_null_loop verifier.Stack.set_null_loop.body at hout
    exact ⟨outData, hout, by
      rw [hout_val]
      have htake1 : cur1.data.val.take i1.val = cur.data.val.take i.val ++ [t1] := by
        simp only [cur1, hback t1, hi1_val]
        rw [List.take_add_one, List.take_set_of_le (Nat.le_refl _),
            List.getElem?_eq_getElem (by rw [List.length_set]; exact hlt_d),
            List.getElem_set_self, Option.toList_some]
      have hdrop1 : cur1.data.val.drop i1.val = cur.data.val.drop (i.val + 1) := by
        simp only [cur1, hback t1, hi1_val]
        exact List.drop_set_of_lt (Nat.lt_succ_self i.val)
      rw [htake1, hdrop1, List.append_assoc, List.singleton_append,
          List.drop_eq_getElem_cons hlt_d, List.map_cons]⟩

theorem Stack_set_null_spec {env : ExecEnv} {vCounter : Nat}
    {self : verifier.Stack} (hs : StackValid env vCounter self)
    (null_id : MemoryId) (is_null : Bool) :
    ∃ s' : verifier.Stack,
      verifier.Stack.set_null self null_id is_null = ok s' ∧
      StackValid env vCounter s' ∧
      (∀ slot < 64, stackSlotType s' slot = pureTypeSetNull (stackSlotType self slot) null_id is_null) := by
  unfold verifier.Stack.set_null
  obtain ⟨outData, hloop, hout_val⟩ := Stack_set_null_loop_spec null_id is_null
    self.data.val.length self 0#usize (by simp) (Nat.zero_le _)
  simp only [usize_const_val_0, List.take_zero, List.drop_zero, List.nil_append] at hout_val
  rw [bind_pair_ok_eq hloop]
  refine ⟨{ mask := self.mask, data := outData }, rfl, ?_, ?_⟩
  · refine ⟨?_, fun x hx => ?_⟩
    · rw [hout_val, List.length_map, hs.1]
    · rw [hout_val, List.mem_map] at hx
      rcases hx with ⟨y, hy, rfl⟩
      exact Type_set_null_valid (hs.2 y hy)
  · intro slot hslot
    unfold stackSlotType
    by_cases hbit : self.mask.bv.getLsbD slot = true
    · simp only [hbit, ↓reduceIte, hout_val]
      have hpos : stackPos self.mask.bv slot < self.data.val.length := by
        rw [hs.1]; exact stackPos_lt_popcount64 self.mask.bv hslot hbit
      rw [List.getD_eq_getElem _ _ (by rw [List.length_map]; exact hpos),
          List.getD_eq_getElem _ _ hpos, List.getElem_map]
    · simp [hbit, pureTypeSetNull]

theorem Stack_set_null_rel2
    {env : ExecEnv} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool} {arrayLen : MemoryId → BitVec 64}
    {self s' : verifier.Stack} {null_id : MemoryId} {is_null : Bool}
    {stk₁ stk₂ : Nat → StackSlot}
    (hslot_eq : ∀ slot < 64,
      stackSlotType s' slot = pureTypeSetNull (stackSlotType self slot) null_id is_null)
    (hnullEq : isNullId (renameMemId ρ null_id) = is_null)
    (hrel : ∀ slot < 64,
      StackSlotRel2 env ρ resActive isNullId arrayLen (stackSlotType self slot) (stk₁ slot) (stk₂ slot)) :
    ∀ slot < 64,
      StackSlotRel2 env ρ resActive isNullId arrayLen (stackSlotType s' slot) (stk₁ slot) (stk₂ slot) := by
  intro slot hslot
  rw [hslot_eq slot hslot]
  exact Type_set_null_stack_rel2 hnullEq (hrel slot hslot)

private theorem Stack_retain_loop_spec {env : ExecEnv} {vCounter : Nat}
    {self : verifier.Stack} (hs : StackValid env vCounter self) (mask : Std.U64) :
    ∀ (n : Nat) (acc : alloc.vec.Vec verifier.Type) (dpos bidx : Std.Usize),
      64 - bidx.val = n →
      bidx.val ≤ 64 →
      dpos.val = popcountRange self.mask.bv bidx.val →
      acc.val.length = popcountRange (self.mask.bv &&& mask.bv) bidx.val →
      (∀ t ∈ acc.val, TypeValid env vCounter t) →
      (∀ slot < bidx.val, (self.mask.bv &&& mask.bv).getLsbD slot = true →
        acc.val.getD (popcountRange (self.mask.bv &&& mask.bv) slot) (.ScalarValue uninitScalar64) =
          self.data.val.getD (popcountRange self.mask.bv slot) (.ScalarValue uninitScalar64)) →
      ∃ out,
        verifier.Stack.retain_loop self.mask self.data mask acc dpos bidx = ok out ∧
        out.val.length = popcount64 (self.mask.bv &&& mask.bv) ∧
        (∀ t ∈ out.val, TypeValid env vCounter t) ∧
        (∀ slot < 64, (self.mask.bv &&& mask.bv).getLsbD slot = true →
          out.val.getD (stackPos (self.mask.bv &&& mask.bv) slot) (.ScalarValue uninitScalar64) =
            self.data.val.getD (stackPos self.mask.bv slot) (.ScalarValue uninitScalar64)) := by
  intro n
  induction n with
  | zero =>
    intro acc dpos bidx hrem hle _ hlen htv hslots
    have hb64 : bidx.val = 64 := by omega
    have hnlt : ¬ (bidx < 64#usize) := by
      change ¬ (bidx.val < (64#usize).val); rw [usize_const_val_64]; omega
    unfold verifier.Stack.retain_loop
    rw [loop_unfold_step]
    unfold verifier.Stack.retain_loop.body
    simp only [hnlt, ↓reduceIte, bind_ok]
    refine ⟨acc, rfl, ?_, htv, ?_⟩
    · unfold popcount64; rw [hb64] at hlen; exact hlen
    · intro slot hslt hand
      exact hslots slot (by omega) hand
  | succ k ih =>
    intro acc dpos bidx hrem hle hdpos hlen htv hslots
    have hlt64 : bidx.val < 64 := by omega
    have hlt_u : bidx < 64#usize := by
      change bidx.val < (64#usize).val; rwa [usize_const_val_64]
    have hmax64 := usize_max_ge_64
    rw [UScalar.max_USize_eq] at hmax64
    obtain ⟨bidx1, hbidx1, hbidx1_val⟩ := usize_add_eq_ok bidx 1#usize (by
      rw [usize_one_val]; omega)
    rw [usize_one_val] at hbidx1_val
    unfold verifier.Stack.retain_loop
    rw [loop_unfold_step]
    unfold verifier.Stack.retain_loop.body
    simp only [hlt_u, ↓reduceIte]
    rw [bind_ok_eq (Stack_bit_spec hlt64)]
    simp only [lift_eq_ok, bind_ok]
    by_cases hbs : self.mask.bv.getLsbD bidx.val = true
    · have hnz1 : (self.mask &&& ⟨(1#64) <<< bidx.val⟩ != 0#u64) = true := by
        rw [bne_iff_ne, ne_eq, u64_and_bit_eq_zero_iff self.mask hlt64]; simp [hbs]
      simp only [hnz1, ↓reduceIte]
      have hdpos_lt : dpos.val < self.data.val.length := by
        rw [hdpos, hs.1]; exact stackPos_lt_popcount64 self.mask.bv hlt64 hbs
      obtain ⟨dpos1, hdpos1, hdpos1_val⟩ := usize_add_eq_ok dpos 1#usize (by
        rw [usize_one_val]
        have := popcountRange_le self.mask.bv bidx.val
        omega)
      rw [usize_one_val] at hdpos1_val
      by_cases hbm : mask.bv.getLsbD bidx.val = true
      · have hnz2 : (mask &&& ⟨(1#64) <<< bidx.val⟩ != 0#u64) = true := by
          rw [bne_iff_ne, ne_eq, u64_and_bit_eq_zero_iff mask hlt64]; simp [hbm]
        have hacc_lt : acc.val.length < Usize.max := by
          have := popcountRange_le (self.mask.bv &&& mask.bv) bidx.val; omega
        obtain ⟨acc1, hpush, hacc1⟩ := vec_push_eq_ok acc (self.data.val[dpos.val]'hdpos_lt) hacc_lt
        have hbranch :
            (do
              let new_data2 ←
                if (mask &&& ⟨(1#64) <<< bidx.val⟩ != 0#u64) = true then do
                  let t ← alloc.vec.Vec.index (core.slice.index.SliceIndexUsizeSlice verifier.Type) self.data dpos
                  let t1 ← verifier.Type.Insts.CoreCloneClone.clone t
                  alloc.vec.Vec.push acc t1
                else ok acc
              let data_pos2 ← dpos + 1#usize
              ok (new_data2, data_pos2)) = ok (acc1, dpos1) := by
          simp only [hnz2, ↓reduceIte]
          rw [bind_ok_eq (vec_index_usize_eq_ok self.data dpos hdpos_lt),
              bind_ok_eq (Type_clone_spec _), bind_ok_eq hpush, bind_ok_eq hdpos1]
        rw [bind_pair_ok_eq hbranch, bind_ok_eq hbidx1]
        simp only [bind_ok]
        obtain ⟨out, hout, hout_len, hout_tv, hout_slots⟩ := ih acc1 dpos1 bidx1
          (by omega) (by omega)
          (by rw [hbidx1_val, popcountRange, hbs, hdpos1_val, hdpos]; simp)
          (by rw [hacc1, List.length_append, List.length_singleton, hlen, hbidx1_val,
                  popcountRange, BitVec.getLsbD_and, hbs, hbm]; simp)
          (fun t ht => by
            rw [hacc1, List.mem_append, List.mem_singleton] at ht
            rcases ht with ht | rfl
            · exact htv t ht
            · exact hs.2 _ (List.getElem_mem hdpos_lt))
          (fun slot hslt hand => by
            rw [hacc1]
            by_cases hsb : slot < bidx.val
            · have hpos_lt : popcountRange (self.mask.bv &&& mask.bv) slot < acc.val.length := by
                rw [hlen]; exact stackPos_lt_popcountRange _ hsb hand
              rw [List.getD_eq_getElem _ _ (by rw [List.length_append]; omega),
                  List.getElem_append_left hpos_lt,
                  ← List.getD_eq_getElem acc.val _ hpos_lt]
              exact hslots slot hsb hand
            · have hseq : slot = bidx.val := by omega
              subst hseq
              rw [← hlen, List.getD_eq_getElem _ _ (by simp),
                  List.getElem_append_right (Nat.le_refl _), ← hdpos,
                  List.getD_eq_getElem _ _ hdpos_lt]
              simp only [Nat.sub_self, List.getElem_singleton])
        unfold verifier.Stack.retain_loop verifier.Stack.retain_loop.body at hout
        simp only [lift_eq_ok, bind_ok] at hout
        exact ⟨out, hout, hout_len, hout_tv, hout_slots⟩
      · have hbmf : mask.bv.getLsbD bidx.val = false := Bool.eq_false_iff.mpr hbm
        have hz2 : (mask &&& ⟨(1#64) <<< bidx.val⟩ != 0#u64) = false := by
          rw [bne_eq_false_iff_eq, u64_and_bit_eq_zero_iff mask hlt64]; exact hbmf
        have hbranch :
            (do
              let new_data2 ←
                if (mask &&& ⟨(1#64) <<< bidx.val⟩ != 0#u64) = true then do
                  let t ← alloc.vec.Vec.index (core.slice.index.SliceIndexUsizeSlice verifier.Type) self.data dpos
                  let t1 ← verifier.Type.Insts.CoreCloneClone.clone t
                  alloc.vec.Vec.push acc t1
                else ok acc
              let data_pos2 ← dpos + 1#usize
              ok (new_data2, data_pos2)) = ok (acc, dpos1) := by
          simp only [hz2, Bool.false_eq_true, ↓reduceIte, bind_ok]
          rw [bind_ok_eq hdpos1]
        rw [bind_pair_ok_eq hbranch, bind_ok_eq hbidx1]
        simp only [bind_ok]
        obtain ⟨out, hout, hout_len, hout_tv, hout_slots⟩ := ih acc dpos1 bidx1
          (by omega) (by omega)
          (by rw [hbidx1_val, popcountRange, hbs, hdpos1_val, hdpos]; simp)
          (by rw [hlen, hbidx1_val, popcountRange, BitVec.getLsbD_and, hbs, hbmf]; simp)
          htv
          (fun slot hslt hand => by
            by_cases hsb : slot < bidx.val
            · exact hslots slot hsb hand
            · have hseq : slot = bidx.val := by omega
              subst hseq
              rw [BitVec.getLsbD_and, hbmf, Bool.and_false] at hand
              cases hand)
        unfold verifier.Stack.retain_loop verifier.Stack.retain_loop.body at hout
        simp only [lift_eq_ok, bind_ok] at hout
        exact ⟨out, hout, hout_len, hout_tv, hout_slots⟩
    · have hbsf : self.mask.bv.getLsbD bidx.val = false := Bool.eq_false_iff.mpr hbs
      have hz1 : (self.mask &&& ⟨(1#64) <<< bidx.val⟩ != 0#u64) = false := by
        rw [bne_eq_false_iff_eq, u64_and_bit_eq_zero_iff self.mask hlt64]; exact hbsf
      simp only [hz1, Bool.false_eq_true, ↓reduceIte]
      rw [bind_pair_ok_eq rfl, bind_ok_eq hbidx1]
      simp only [bind_ok]
      obtain ⟨out, hout, hout_len, hout_tv, hout_slots⟩ := ih acc dpos bidx1
        (by omega) (by omega)
        (by rw [hbidx1_val, popcountRange, hbsf, hdpos]; simp)
        (by rw [hlen, hbidx1_val, popcountRange, BitVec.getLsbD_and, hbsf]; simp)
        htv
        (fun slot hslt hand => by
          by_cases hsb : slot < bidx.val
          · exact hslots slot hsb hand
          · have hseq : slot = bidx.val := by omega
            subst hseq
            rw [BitVec.getLsbD_and, hbsf, Bool.false_and] at hand
            cases hand)
      unfold verifier.Stack.retain_loop verifier.Stack.retain_loop.body at hout
      simp only [lift_eq_ok, bind_ok] at hout
      exact ⟨out, hout, hout_len, hout_tv, hout_slots⟩

theorem Stack_retain_spec {env : ExecEnv} {vCounter : Nat}
    {self : verifier.Stack} (hs : StackValid env vCounter self) (mask : Std.U64) :
    ∃ s' : verifier.Stack,
      verifier.Stack.retain self mask = ok s' ∧
      StackValid env vCounter s' ∧
      (∀ slot < 64,
        stackSlotType s' slot =
          if mask.bv.getLsbD slot then stackSlotType self slot else .ScalarValue uninitScalar64) := by
  unfold verifier.Stack.retain
  simp only [lift_eq_ok, bind_ok]
  by_cases hsub : self.mask &&& ~~~mask = 0#u64
  · simp only [hsub, ↓reduceIte]
    refine ⟨self, rfl, hs, fun slot hslot => ?_⟩
    by_cases hm : mask.bv.getLsbD slot = true
    · simp [hm]
    · have hmf : mask.bv.getLsbD slot = false := Bool.eq_false_iff.mpr hm
      have hbit0 := congrArg (fun b : BitVec 64 => b.getLsbD slot) ((u64_eq_iff_bv _ _).mp hsub)
      change (self.mask.bv &&& ~~~mask.bv).getLsbD slot = (0#64).getLsbD slot at hbit0
      rw [BitVec.getLsbD_and, BitVec.getLsbD_not, BitVec.getLsbD_zero] at hbit0
      simp only [hmf, Bool.not_false, hslot, decide_true, Bool.and_true] at hbit0
      simp [hmf, stackSlotType, hbit0]
  · simp only [hsub, ↓reduceIte]
    by_cases hm0 : mask = 0#u64
    · subst hm0
      simp only [↓reduceIte]
      refine ⟨{ mask := 0#u64, data := alloc.vec.Vec.new verifier.Type }, rfl, ?_, ?_⟩
      · exact ⟨by simp [popcount64, popcountRange_zero], fun _ h => by simp at h⟩
      · intro slot _
        simp [stackSlotType, BitVec.getLsbD_zero]
    · simp only [hm0, ↓reduceIte]
      obtain ⟨nd, hloop, hlen, htv, hslots⟩ := Stack_retain_loop_spec hs mask
        64 (alloc.vec.Vec.new verifier.Type) 0#usize 0#usize
        rfl (Nat.zero_le _) (by simp [popcountRange]) (by simp [popcountRange])
        (fun _ h => by simp at h) (fun _ h => by rw [usize_const_val_0] at h; omega)
      rw [bind_ok_eq hloop]
      refine ⟨{ mask := self.mask &&& mask, data := nd }, rfl, ⟨hlen, htv⟩, fun slot hslot => ?_⟩
      unfold stackSlotType
      change (if (self.mask.bv &&& mask.bv).getLsbD slot then
          nd.val.getD (stackPos (self.mask.bv &&& mask.bv) slot) (.ScalarValue uninitScalar64)
        else .ScalarValue uninitScalar64) = _
      by_cases hm : mask.bv.getLsbD slot = true
      · by_cases hbs : self.mask.bv.getLsbD slot = true
        · have hand : (self.mask.bv &&& mask.bv).getLsbD slot = true := by
            rw [BitVec.getLsbD_and, hbs, hm]; rfl
          simp only [hand, hm, hbs, ↓reduceIte]
          exact hslots slot hslot hand
        · have hbsf : self.mask.bv.getLsbD slot = false := Bool.eq_false_iff.mpr hbs
          have hand : (self.mask.bv &&& mask.bv).getLsbD slot = false := by
            rw [BitVec.getLsbD_and, hbsf, Bool.false_and]
          simp [hand, hm, hbsf]
      · have hmf : mask.bv.getLsbD slot = false := Bool.eq_false_iff.mpr hm
        have hand : (self.mask.bv &&& mask.bv).getLsbD slot = false := by
          rw [BitVec.getLsbD_and, hmf, Bool.and_false]
        simp [hand, hmf]

/-! ## 3. `Stack` Sub-Word & Range Memory Operations (`load`, `store`, `read_data_ptr`, `write_data_ptr`) -/

theorem StackValid_slot_valid {env : ExecEnv} {vCounter : Nat}
    {self : verifier.Stack} (hs : StackValid env vCounter self) (slot : Nat) :
    TypeValid env vCounter (stackSlotType self slot) := by
  unfold stackSlotType
  by_cases hslot : slot < 64
  · by_cases hb : self.mask.bv.getLsbD slot = true
    · simp only [hb, ↓reduceIte]
      have hpos : stackPos self.mask.bv slot < self.data.val.length := by
        rw [hs.1]; exact stackPos_lt_popcount64 self.mask.bv hslot hb
      rw [List.getD_eq_getElem _ _ hpos]
      exact hs.2 _ (List.getElem_mem hpos)
    · simp only [hb, Bool.false_eq_true, ↓reduceIte]
      exact uninitScalar64_valid
  · have hb : self.mask.bv.getLsbD slot = false := BitVec.getLsbD_of_ge self.mask.bv slot (by omega)
    simp only [hb, Bool.false_eq_true, ↓reduceIte]
    exact uninitScalar64_valid

/-- Pure bitvector mirror of `verifier.Stack.extract_sub_value`. -/
def pureExtractSubBytes (v : BitVec 64) (off bc : Nat) : BitVec 64 :=
  if bc = 0 ∨ 8 ≤ off then 0#64
  else
    let shifted := v >>> BitVec.ofNat 6 (off * 8)
    if 8 ≤ bc then shifted
    else shifted &&& ((1#64 <<< BitVec.ofNat 6 (bc * 8)) - 1#64)

theorem Stack_extract_sub_value_spec (value : Std.U64) (offset byte_count : Std.Usize) :
    verifier.Stack.extract_sub_value value offset byte_count =
      ok ⟨pureExtractSubBytes value.bv offset.val byte_count.val⟩ := by
  unfold verifier.Stack.extract_sub_value pureExtractSubBytes
  have hmax64 := usize_max_ge_64
  rw [UScalar.max_USize_eq] at hmax64
  by_cases hbc0 : byte_count = 0#usize
  · subst hbc0
    simp only [↓reduceIte, usize_const_val_0, true_or]
    exact congrArg ok ((u64_eq_iff_bv _ _).mpr rfl)
  · have hbc_ne0 : byte_count.val ≠ 0 := fun hc =>
      hbc0 (usize_eq_of_val_eq (by simpa [usize_const_val_0] using hc))
    simp only [hbc0, hbc_ne0, false_or, ↓reduceIte]
    by_cases hoff8 : offset >= 8#usize
    · have hoff_ge : 8 ≤ offset.val := by
        change (8#usize).val ≤ offset.val at hoff8
        rwa [usize_const_val_8] at hoff8
      simp only [hoff8, hoff_ge, ↓reduceIte]
      exact congrArg ok ((u64_eq_iff_bv _ _).mpr rfl)
    · have hoff_lt : offset.val < 8 := by
        have hnot : ¬ ((8#usize).val ≤ offset.val) := hoff8
        rw [usize_const_val_8] at hnot
        omega
      have hn8 : ¬ (8 ≤ offset.val) := by omega
      simp only [hoff8, hn8, ↓reduceIte]
      obtain ⟨i, hi, hi_val⟩ := usize_mul_eq_ok offset 8#usize (by
        rw [usize_const_val_8]; omega)
      rw [usize_const_val_8] at hi_val
      rw [bind_ok_eq hi]
      simp only [lift_eq_ok, bind_ok, u64_wrapping_shr_eq]
      have hcast_i : (UScalar.cast .U32 i).bv.setWidth 6 = BitVec.ofNat 6 (offset.val * 8) := by
        apply BitVec.eq_of_toNat_eq
        rw [BitVec.toNat_setWidth, UScalar.bv_toNat,
            usize_cast_u32_val_of_lt i (by omega), hi_val, BitVec.toNat_ofNat]
      by_cases hbc8 : byte_count >= 8#usize
      · have hbc_ge : 8 ≤ byte_count.val := by
          change (8#usize).val ≤ byte_count.val at hbc8
          rwa [usize_const_val_8] at hbc8
        simp only [hbc8, hbc_ge, ↓reduceIte]
        congr 1
        apply (u64_eq_iff_bv _ _).mpr
        change value.bv >>> (UScalar.cast .U32 i).bv.setWidth 6 = _
        rw [hcast_i]
      · have hbc_lt : byte_count.val < 8 := by
          have hnot : ¬ ((8#usize).val ≤ byte_count.val) := hbc8
          rw [usize_const_val_8] at hnot
          omega
        have hnbc8 : ¬ (8 ≤ byte_count.val) := by omega
        simp only [hbc8, hnbc8, ↓reduceIte]
        obtain ⟨i1, hi1, hi1_val⟩ := usize_mul_eq_ok byte_count 8#usize (by
          rw [usize_const_val_8]; omega)
        rw [usize_const_val_8] at hi1_val
        rw [bind_ok_eq hi1]
        simp only [u64_wrapping_shl_eq, u64_one_uscalar_bv]
        have hcast_i1 : (UScalar.cast .U32 i1).bv.setWidth 6 = BitVec.ofNat 6 (byte_count.val * 8) := by
          apply BitVec.eq_of_toNat_eq
          rw [BitVec.toNat_setWidth, UScalar.bv_toNat,
              usize_cast_u32_val_of_lt i1 (by omega), hi1_val, BitVec.toNat_ofNat]
        have hshl_ge1 : (1#u64).val ≤ (⟨1#64 <<< (UScalar.cast .U32 i1).bv.setWidth 6⟩ : Std.U64).val := by
          rw [← UScalar.bv_toNat, hcast_i1]
          have hcases : byte_count.val = 1 ∨ byte_count.val = 2 ∨ byte_count.val = 3 ∨
              byte_count.val = 4 ∨ byte_count.val = 5 ∨ byte_count.val = 6 ∨ byte_count.val = 7 := by omega
          rcases hcases with h | h | h | h | h | h | h <;> (rw [h]; decide)
        have hsub1 := uscalar_sub_eq_ok (⟨1#64 <<< (UScalar.cast .U32 i1).bv.setWidth 6⟩ : Std.U64) 1#u64 hshl_ge1
        simp only [u64_one_uscalar_bv] at hsub1
        rw [bind_ok_eq hsub1]
        congr 1
        apply (u64_eq_iff_bv _ _).mpr
        change (value.bv >>> (UScalar.cast .U32 i).bv.setWidth 6) &&&
            ((1#64 <<< (UScalar.cast .U32 i1).bv.setWidth 6) - 1#64) = _
        rw [hcast_i, hcast_i1]

theorem pureExtractSubBytes_eq_extractSubValue (v : BitVec 64) {off : Nat}
    (hoff : off < 8) (w : DataWidth) :
    pureExtractSubBytes v off w.bytes = extractSubValue v off w := by
  unfold pureExtractSubBytes extractSubValue
  have hcases : off = 0 ∨ off = 1 ∨ off = 2 ∨ off = 3 ∨
      off = 4 ∨ off = 5 ∨ off = 6 ∨ off = 7 := by omega
  cases w <;> rcases hcases with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;>
    (simp only [DataWidth.bytes, DataWidth.valMask64]; bv_decide)

theorem Stack_insert_sub_value_spec (original value : Std.U64) (width : api.DataWidth)
    {offset : Std.Usize}
    (hfit : offset.val + (DataWidth.ofCore width).bytes ≤ 8) :
    verifier.Stack.insert_sub_value original value width offset =
      ok ⟨insertSubValue original.bv value.bv offset.val (DataWidth.ofCore width)⟩ := by
  unfold verifier.Stack.insert_sub_value insertSubValue
  obtain ⟨bi, hbi, hbi_val⟩ := DataWidth_bits_spec width
  rw [bind_ok_eq hbi]
  simp only [lift_eq_ok, bind_ok]
  have hmax64 := usize_max_ge_64
  rw [UScalar.max_USize_eq] at hmax64
  cases width with
  | U64 =>
    have hoff0 : offset.val = 0 := by simp [DataWidth.ofCore, DataWidth.bytes] at hfit; omega
    have hbi64 : UScalar.cast .U32 bi = 64#u32 := by
      apply (u32_eq_iff_bv _ _).mpr; apply BitVec.eq_of_toNat_eq
      rw [UScalar.bv_toNat, usize_cast_u32_val_of_lt bi (by simp [DataWidth.ofCore, DataWidth.bits] at hbi_val; omega),
          hbi_val]
      rfl
    rw [hbi64, hoff0]
    simp only [ge_iff_le, le_refl, ↓reduceIte, DataWidth.ofCore, DataWidth.valMask64]
    congr 1; apply (u64_eq_iff_bv _ _).mpr
    change value.bv = (original.bv &&& ~~~(~~~0#64 <<< BitVec.ofNat 6 (0 * 8))) |||
        ((value.bv &&& ~~~0#64) <<< BitVec.ofNat 6 (0 * 8))
    bv_decide
  | U8 =>
    have hoff_lt : offset.val < 8 := by simp [DataWidth.ofCore, DataWidth.bytes] at hfit; omega
    obtain ⟨i1, hi1, hi1_val⟩ := usize_mul_eq_ok offset 8#usize (by rw [usize_const_val_8]; omega)
    rw [usize_const_val_8] at hi1_val
    have hcast_off : (UScalar.cast .U32 i1).bv.setWidth 6 = BitVec.ofNat 6 (offset.val * 8) := by
      apply BitVec.eq_of_toNat_eq
      rw [BitVec.toNat_setWidth, UScalar.bv_toNat, usize_cast_u32_val_of_lt i1 (by omega), hi1_val, BitVec.toNat_ofNat]
    have hbi8 : UScalar.cast .U32 bi = 8#u32 := by
      apply (u32_eq_iff_bv _ _).mpr; apply BitVec.eq_of_toNat_eq
      rw [UScalar.bv_toNat, usize_cast_u32_val_of_lt bi (by simp [DataWidth.ofCore, DataWidth.bits] at hbi_val; omega), hbi_val]; rfl
    rw [hbi8]
    have hn64 : ¬ (8#u32 >= 64#u32) := by decide
    simp only [hn64, ↓reduceIte]
    rw [bind_ok_eq hi1]
    simp only [u64_wrapping_shl_eq, u64_one_uscalar_bv]
    have hsub8 := uscalar_sub_eq_ok (⟨1#64 <<< (8#u32).bv.setWidth 6⟩ : Std.U64) 1#u64 (by decide)
    simp only [u64_one_uscalar_bv] at hsub8
    rw [bind_ok_eq hsub8]
    simp only [DataWidth.ofCore, DataWidth.valMask64]
    congr 1; apply (u64_eq_iff_bv _ _).mpr
    have hm8 : (1#64 <<< (8#u32).bv.setWidth 6) - 1#64 = 255#64 := by decide
    change (original.bv &&& ~~~(((1#64 <<< (8#u32).bv.setWidth 6) - 1#64) <<< (UScalar.cast .U32 i1).bv.setWidth 6)) |||
        ((value.bv &&& ((1#64 <<< (8#u32).bv.setWidth 6) - 1#64)) <<< (UScalar.cast .U32 i1).bv.setWidth 6) = _
    rw [hm8, hcast_off]
  | U16 =>
    have hoff_lt : offset.val < 8 := by simp [DataWidth.ofCore, DataWidth.bytes] at hfit; omega
    obtain ⟨i1, hi1, hi1_val⟩ := usize_mul_eq_ok offset 8#usize (by rw [usize_const_val_8]; omega)
    rw [usize_const_val_8] at hi1_val
    have hcast_off : (UScalar.cast .U32 i1).bv.setWidth 6 = BitVec.ofNat 6 (offset.val * 8) := by
      apply BitVec.eq_of_toNat_eq
      rw [BitVec.toNat_setWidth, UScalar.bv_toNat, usize_cast_u32_val_of_lt i1 (by omega), hi1_val, BitVec.toNat_ofNat]
    have hbi16 : UScalar.cast .U32 bi = 16#u32 := by
      apply (u32_eq_iff_bv _ _).mpr; apply BitVec.eq_of_toNat_eq
      rw [UScalar.bv_toNat, usize_cast_u32_val_of_lt bi (by simp [DataWidth.ofCore, DataWidth.bits] at hbi_val; omega), hbi_val]; rfl
    rw [hbi16]
    have hn64 : ¬ (16#u32 >= 64#u32) := by decide
    simp only [hn64, ↓reduceIte]
    rw [bind_ok_eq hi1]
    simp only [u64_wrapping_shl_eq, u64_one_uscalar_bv]
    have hsub16 := uscalar_sub_eq_ok (⟨1#64 <<< (16#u32).bv.setWidth 6⟩ : Std.U64) 1#u64 (by decide)
    simp only [u64_one_uscalar_bv] at hsub16
    rw [bind_ok_eq hsub16]
    simp only [DataWidth.ofCore, DataWidth.valMask64]
    congr 1; apply (u64_eq_iff_bv _ _).mpr
    have hm16 : (1#64 <<< (16#u32).bv.setWidth 6) - 1#64 = 65535#64 := by decide
    change (original.bv &&& ~~~(((1#64 <<< (16#u32).bv.setWidth 6) - 1#64) <<< (UScalar.cast .U32 i1).bv.setWidth 6)) |||
        ((value.bv &&& ((1#64 <<< (16#u32).bv.setWidth 6) - 1#64)) <<< (UScalar.cast .U32 i1).bv.setWidth 6) = _
    rw [hm16, hcast_off]
  | U32 =>
    have hoff_lt : offset.val < 8 := by simp [DataWidth.ofCore, DataWidth.bytes] at hfit; omega
    obtain ⟨i1, hi1, hi1_val⟩ := usize_mul_eq_ok offset 8#usize (by rw [usize_const_val_8]; omega)
    rw [usize_const_val_8] at hi1_val
    have hcast_off : (UScalar.cast .U32 i1).bv.setWidth 6 = BitVec.ofNat 6 (offset.val * 8) := by
      apply BitVec.eq_of_toNat_eq
      rw [BitVec.toNat_setWidth, UScalar.bv_toNat, usize_cast_u32_val_of_lt i1 (by omega), hi1_val, BitVec.toNat_ofNat]
    have hbi32 : UScalar.cast .U32 bi = 32#u32 := by
      apply (u32_eq_iff_bv _ _).mpr; apply BitVec.eq_of_toNat_eq
      rw [UScalar.bv_toNat, usize_cast_u32_val_of_lt bi (by simp [DataWidth.ofCore, DataWidth.bits] at hbi_val; omega), hbi_val]; rfl
    rw [hbi32]
    have hn64 : ¬ (32#u32 >= 64#u32) := by decide
    simp only [hn64, ↓reduceIte]
    rw [bind_ok_eq hi1]
    simp only [u64_wrapping_shl_eq, u64_one_uscalar_bv]
    have hsub32 := uscalar_sub_eq_ok (⟨1#64 <<< (32#u32).bv.setWidth 6⟩ : Std.U64) 1#u64 (by decide)
    simp only [u64_one_uscalar_bv] at hsub32
    rw [bind_ok_eq hsub32]
    simp only [DataWidth.ofCore, DataWidth.valMask64]
    congr 1; apply (u64_eq_iff_bv _ _).mpr
    have hm32 : (1#64 <<< (32#u32).bv.setWidth 6) - 1#64 = 4294967295#64 := by decide
    change (original.bv &&& ~~~(((1#64 <<< (32#u32).bv.setWidth 6) - 1#64) <<< (UScalar.cast .U32 i1).bv.setWidth 6)) |||
        ((value.bv &&& ((1#64 <<< (32#u32).bv.setWidth 6) - 1#64)) <<< (UScalar.cast .U32 i1).bv.setWidth 6) = _
    rw [hm32, hcast_off]

private theorem pureExtractSubBytes_eq_zero_imp_byte (m : BitVec 64) {s e k : Nat}
    (hse : s < e) (he8 : e ≤ 8) (hsk : s ≤ k) (hke : k < e)
    (hz : pureExtractSubBytes m s (e - s) = 0#64) :
    extractSubValue m k .u8 = 0#64 := by
  unfold pureExtractSubBytes at hz
  unfold extractSubValue DataWidth.valMask64
  have hs_cases : s = 0 ∨ s = 1 ∨ s = 2 ∨ s = 3 ∨ s = 4 ∨ s = 5 ∨ s = 6 ∨ s = 7 := by omega
  have he_cases : e = 1 ∨ e = 2 ∨ e = 3 ∨ e = 4 ∨ e = 5 ∨ e = 6 ∨ e = 7 ∨ e = 8 := by omega
  have hk_cases : k = 0 ∨ k = 1 ∨ k = 2 ∨ k = 3 ∨ k = 4 ∨ k = 5 ∨ k = 6 ∨ k = 7 := by omega
  rcases hs_cases with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;>
    rcases he_cases with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;>
    rcases hk_cases with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;>
    first
    | omega
    | (simp at hz ⊢; revert hz; bv_decide)

private theorem extractSubValue_init_of_unwritten_zero {unwr init : BitVec 64} {k : Nat}
    (hk : k < 8)
    (hmask : ~~~unwr &&& init = ~~~unwr)
    (hz : extractSubValue unwr k .u8 = 0#64) :
    extractSubValue init k .u8 = 0xFF#64 := by
  unfold extractSubValue DataWidth.valMask64 at hz ⊢
  have hk_cases : k = 0 ∨ k = 1 ∨ k = 2 ∨ k = 3 ∨ k = 4 ∨ k = 5 ∨ k = 6 ∨ k = 7 := by omega
  rcases hk_cases with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;>
    (revert hmask hz; bv_decide)

theorem Stack_read_element_spec {env : ExecEnv} {vCounter : Nat}
    {self : verifier.Stack} (hs : StackValid env vCounter self)
    {index start_offset end_offset : Std.Usize}
    (hlt : index.val < 64)
    (hse : start_offset.val < end_offset.val)
    (he8 : end_offset.val ≤ 8) :
    ∃ r : core.result.Result Unit error.VerifierErrorKind,
      verifier.Stack.read_element self index start_offset end_offset = ok r ∧
      (∀ u, r = .Ok u →
        ∃ d : U64ScalarValueData,
          stackSlotType self index.val = .ScalarValue d ∧
          d ≠ uninitScalar64 ∧
          (∀ k, start_offset.val ≤ k → k < end_offset.val →
            extractSubValue d.unwritten_mask.bv k .u8 = 0#64)) := by
  unfold verifier.Stack.read_element
  rw [bind_ok_eq (Stack_get_spec hs hlt)]
  cases hst : stackSlotType self index.val <;>
    try exact ⟨.Err .NonScalarStackRead, rfl, fun _ h => by cases h⟩
  rename_i d
  dsimp only
  obtain ⟨diff, hdiff, hdiff_val⟩ := usize_sub_eq_ok end_offset start_offset (by omega)
  rw [bind_ok_eq hdiff, bind_ok_eq (Stack_extract_sub_value_spec d.unwritten_mask start_offset diff)]
  by_cases hz : (⟨pureExtractSubBytes d.unwritten_mask.bv start_offset.val diff.val⟩ : Std.U64) = 0#u64
  · simp only [hz, ↓reduceIte]
    refine ⟨.Ok (), rfl, fun _ _ => ?_⟩
    have hzbv : pureExtractSubBytes d.unwritten_mask.bv start_offset.val (end_offset.val - start_offset.val) = 0#64 := by
      rw [← hdiff_val]
      exact (u64_eq_iff_bv _ _).mp hz
    have hbytes : ∀ k, start_offset.val ≤ k → k < end_offset.val →
        extractSubValue d.unwritten_mask.bv k .u8 = 0#64 :=
      fun k hsk hke => pureExtractSubBytes_eq_zero_imp_byte d.unwritten_mask.bv hse he8 hsk hke hzbv
    have hne : d ≠ uninitScalar64 := by
      intro hc; subst hc
      have hb0 := hbytes start_offset.val (Nat.le_refl _) hse
      unfold uninitScalar64 extractSubValue DataWidth.valMask64 at hb0
      simp only [u64_max_bv] at hb0
      have hcases : start_offset.val = 0 ∨ start_offset.val = 1 ∨ start_offset.val = 2 ∨
          start_offset.val = 3 ∨ start_offset.val = 4 ∨ start_offset.val = 5 ∨
          start_offset.val = 6 ∨ start_offset.val = 7 := by omega
      rcases hcases with h | h | h | h | h | h | h | h <;> (rw [h] at hb0; revert hb0; decide)
    exact ⟨d, rfl, hne, hbytes⟩
  · simp only [hz, ↓reduceIte]
    exact ⟨.Err .UninitializedStackRead, rfl, fun _ h => by cases h⟩

private theorem Stack_read_data_ptr_loop0_spec {env : ExecEnv} {vCounter : Nat}
    {self : verifier.Stack} (hs : StackValid env vCounter self)
    {end_idx : Std.Usize} (hend : end_idx.val ≤ 64) :
    ∀ (n : Nat) (i : Std.Usize),
      end_idx.val - i.val = n →
      ∃ r : core.result.Result Unit error.VerifierErrorKind,
        verifier.Stack.read_data_ptr_loop0 self i end_idx = ok r ∧
        (∀ u, r = .Ok u →
          ∀ slot, i.val ≤ slot → slot < end_idx.val →
            ∃ d : U64ScalarValueData,
              stackSlotType self slot = .ScalarValue d ∧
              d ≠ uninitScalar64 ∧
              (∀ k < 8, extractSubValue d.unwritten_mask.bv k .u8 = 0#64)) := by
  intro n
  induction n with
  | zero =>
    intro i hrem
    have hnlt : ¬ (i < end_idx) := by change ¬ (i.val < end_idx.val); omega
    unfold verifier.Stack.read_data_ptr_loop0
    rw [loop_unfold_step]
    unfold verifier.Stack.read_data_ptr_loop0.body
    simp only [hnlt, ↓reduceIte, bind_ok]
    exact ⟨.Ok (), rfl, fun _ _ _ hge hlt => by omega⟩
  | succ m ih =>
    intro i hrem
    have hi_lt : i < end_idx := by change i.val < end_idx.val; omega
    have hi64 : i.val < 64 := by omega
    unfold verifier.Stack.read_data_ptr_loop0
    rw [loop_unfold_step]
    unfold verifier.Stack.read_data_ptr_loop0.body
    simp only [hi_lt, ↓reduceIte, verifier_STACK_ELEMENT_SIZE_eq]
    obtain ⟨re, hre, hre_post⟩ := Stack_read_element_spec hs hi64
      (start_offset := 0#usize) (end_offset := 8#usize)
      (by rw [usize_const_val_0, usize_const_val_8]; omega)
      (by rw [usize_const_val_8])
    rw [bind_ok_eq hre]
    cases re with
    | Err e =>
      simp only [bind_ok]
      exact ⟨.Err e, rfl, fun _ h => by cases h⟩
    | Ok u0 =>
      have hmax64 := usize_max_ge_64
      rw [UScalar.max_USize_eq] at hmax64
      obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by rw [usize_one_val]; omega)
      rw [usize_one_val] at hi1_val
      rw [bind_ok_eq hi1]
      simp only [bind_ok]
      obtain ⟨rl, hrl, hrl_post⟩ := ih i1 (by omega)
      unfold verifier.Stack.read_data_ptr_loop0 verifier.Stack.read_data_ptr_loop0.body at hrl
      simp only [verifier_STACK_ELEMENT_SIZE_eq] at hrl
      refine ⟨rl, hrl, fun u hrok slot hge hlt => ?_⟩
      by_cases hsi : slot = i.val
      · subst hsi
        obtain ⟨d, hd, hne, hbytes⟩ := hre_post u0 rfl
        exact ⟨d, hd, hne, fun k hk => hbytes k (by rw [usize_const_val_0]; omega)
          (by rwa [usize_const_val_8])⟩
      · exact hrl_post u hrok slot (by omega) hlt

private theorem Stack_read_data_ptr_loop1_spec {env : ExecEnv} {vCounter : Nat}
    {self : verifier.Stack} (hs : StackValid env vCounter self)
    {end_idx : Std.Usize} (hend : end_idx.val ≤ 64) :
    ∀ (n : Nat) (i : Std.Usize),
      end_idx.val - i.val = n →
      ∃ r : core.result.Result Unit error.VerifierErrorKind,
        verifier.Stack.read_data_ptr_loop1 self i end_idx = ok r ∧
        (∀ u, r = .Ok u →
          ∀ slot, i.val ≤ slot → slot < end_idx.val →
            ∃ d : U64ScalarValueData,
              stackSlotType self slot = .ScalarValue d ∧
              d ≠ uninitScalar64 ∧
              (∀ k < 8, extractSubValue d.unwritten_mask.bv k .u8 = 0#64)) := by
  intro n
  induction n with
  | zero =>
    intro i hrem
    have hnlt : ¬ (i < end_idx) := by change ¬ (i.val < end_idx.val); omega
    unfold verifier.Stack.read_data_ptr_loop1
    rw [loop_unfold_step]
    unfold verifier.Stack.read_data_ptr_loop1.body
    simp only [hnlt, ↓reduceIte, bind_ok]
    exact ⟨.Ok (), rfl, fun _ _ _ hge hlt => by omega⟩
  | succ m ih =>
    intro i hrem
    have hi_lt : i < end_idx := by change i.val < end_idx.val; omega
    have hi64 : i.val < 64 := by omega
    unfold verifier.Stack.read_data_ptr_loop1
    rw [loop_unfold_step]
    unfold verifier.Stack.read_data_ptr_loop1.body
    simp only [hi_lt, ↓reduceIte, verifier_STACK_ELEMENT_SIZE_eq]
    obtain ⟨re, hre, hre_post⟩ := Stack_read_element_spec hs hi64
      (start_offset := 0#usize) (end_offset := 8#usize)
      (by rw [usize_const_val_0, usize_const_val_8]; omega)
      (by rw [usize_const_val_8])
    rw [bind_ok_eq hre]
    cases re with
    | Err e =>
      simp only [bind_ok]
      exact ⟨.Err e, rfl, fun _ h => by cases h⟩
    | Ok u0 =>
      have hmax64 := usize_max_ge_64
      rw [UScalar.max_USize_eq] at hmax64
      obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by rw [usize_one_val]; omega)
      rw [usize_one_val] at hi1_val
      rw [bind_ok_eq hi1]
      simp only [bind_ok]
      obtain ⟨rl, hrl, hrl_post⟩ := ih i1 (by omega)
      unfold verifier.Stack.read_data_ptr_loop1 verifier.Stack.read_data_ptr_loop1.body at hrl
      simp only [verifier_STACK_ELEMENT_SIZE_eq] at hrl
      refine ⟨rl, hrl, fun u hrok slot hge hlt => ?_⟩
      by_cases hsi : slot = i.val
      · subst hsi
        obtain ⟨d, hd, hne, hbytes⟩ := hre_post u0 rfl
        exact ⟨d, hd, hne, fun k hk => hbytes k (by rw [usize_const_val_0]; omega)
          (by rwa [usize_const_val_8])⟩
      · exact hrl_post u hrok slot (by omega) hlt

theorem Stack_read_data_ptr_spec {env : ExecEnv} {vCounter : Nat}
    {self : verifier.Stack} (hs : StackValid env vCounter self)
    (offset : verifier.StackOffset) (bytes : Std.U64) :
    ∃ r : core.result.Result Unit error.VerifierErrorKind,
      verifier.Stack.read_data_ptr self offset bytes = ok r ∧
      (∀ u, r = .Ok u →
        bytes = 0#u64 ∨
        (bytes.bv ≤ 512#64 ∧
         offset.unknown_mask = 0#u64 ∧
         offset.value.bv < 512#64 ∧
         ¬ BitVec.uaddOverflow offset.value.bv bytes.bv ∧
         (offset.value.bv + bytes.bv) ≤ 512#64 ∧
         (∀ k < bytes.bv.toNat,
           let pos := offset.value.bv.toNat + k
           ∃ d : U64ScalarValueData,
             stackSlotType self (pos / 8) = .ScalarValue d ∧
             d ≠ uninitScalar64 ∧
             extractSubValue d.unwritten_mask.bv (pos % 8) .u8 = 0#64))) := by
  unfold verifier.Stack.read_data_ptr
  by_cases hb0 : bytes = 0#u64
  · subst hb0
    exact ⟨.Ok (), by simp, fun _ _ => Or.inl rfl⟩
  · simp only [hb0, ↓reduceIte]
    rw [api_BPF_STACK_SIZE_eq]
    simp only [lift_eq_ok, bind_ok, cast_512_usize_eq_u64]
    by_cases hbgt : bytes > 512#u64
    · simp only [hbgt, ↓reduceIte]
      exact ⟨.Err .StackAccessOutOfBounds, rfl, fun _ h => by cases h⟩
    · have hble : bytes.bv ≤ 512#64 := by
        have hnot : ¬ (512#64 < bytes.bv) := fun hc => hbgt ((u64_gt_iff_bv bytes 512#u64).mpr hc)
        rw [BitVec.lt_def] at hnot; rw [BitVec.le_def]; omega
      have hb_pos : 0 < bytes.bv.toNat := by
        by_contra hc
        have hz : bytes.bv.toNat = 0 := by omega
        apply hb0
        apply (u64_eq_iff_bv _ _).mpr
        exact BitVec.eq_of_toNat_eq (by simpa using hz)
      simp only [hbgt, ↓reduceIte]
      obtain ⟨rchk, hrchk, hchk_iff⟩ := StackOffset_check_valid_offset_spec offset
      rw [bind_ok_eq hrchk]
      cases rchk with
      | Err e => exact ⟨.Err e, rfl, fun _ h => by cases h⟩
      | Ok u0 =>
        obtain ⟨hoff_kn, hoff_lt⟩ := (hchk_iff u0).mp rfl
        have hend_eq : verifier.StackOffset.add_u64 offset bytes =
            ok (knownScalar64 ⟨offset.value.bv + bytes.bv⟩) := by
          unfold verifier.StackOffset.add_u64 U64ScalarValueData.add_u64
            U64ScalarValueData.add_scalar U64ScalarValueData.is_known lift
          rw [bind_ok_eq (from_known_64_eq bytes)]
          have hkn_b : (knownScalar64 bytes).unknown_mask = 0#u64 := rfl
          simp only [hoff_kn, hkn_b, decide_true, ↓reduceIte, bind_ok,
            u64_wrapping_add_eq, from_known_64_eq]
          rfl
        rw [bind_ok_eq hend_eq, bind_ok_eq (StackOffset_is_within_stack_spec _)]
        have h1_512 : offset.value.bv.toNat < 512 := by rw [BitVec.lt_def] at hoff_lt; simpa using hoff_lt
        have h2_512 : bytes.bv.toNat ≤ 512 := by rw [BitVec.le_def] at hble; simpa using hble
        have hno_ovf : ¬ BitVec.uaddOverflow offset.value.bv bytes.bv := by
          simp only [BitVec.uaddOverflow, decide_eq_true_eq]
          omega
        have hsum_toNat : (offset.value.bv + bytes.bv).toNat = offset.value.bv.toNat + bytes.bv.toNat := by
          rw [BitVec.toNat_add]
          exact Nat.mod_eq_of_lt (by omega)
        have hkn_end : (knownScalar64 ⟨offset.value.bv + bytes.bv⟩).unknown_mask = 0#u64 := rfl
        have hval_end : (knownScalar64 ⟨offset.value.bv + bytes.bv⟩).value.bv = offset.value.bv + bytes.bv := rfl
        by_cases hwithin : offset.value.bv + bytes.bv ≤ 512#64
        · have hw_dec : decide ((knownScalar64 ⟨offset.value.bv + bytes.bv⟩).unknown_mask = 0#u64 ∧
              (knownScalar64 ⟨offset.value.bv + bytes.bv⟩).value.bv ≤ 512#64) = true :=
            decide_eq_true ⟨hkn_end, hwithin⟩
          simp only [hw_dec, ↓reduceIte]
          have hoff_le : offset.value.bv ≤ 512#64 := BitVec.le_of_lt hoff_lt
          obtain ⟨i1, hi1, hi1_val⟩ := StackOffset_array_index_spec hoff_le
          obtain ⟨i2, hi2, hi2_val⟩ := StackOffset_array_index_spec (s := knownScalar64 ⟨offset.value.bv + bytes.bv⟩) hwithin
          rw [hval_end, hsum_toNat] at hi2_val
          obtain ⟨i3, hi3, hi3_val, hi3_lt8⟩ := StackOffset_sub_index_spec hoff_le
          obtain ⟨i4, hi4, hi4_val, hi4_lt8⟩ := StackOffset_sub_index_spec (s := knownScalar64 ⟨offset.value.bv + bytes.bv⟩) hwithin
          rw [hval_end, hsum_toNat] at hi4_val
          have hsum_le512 : offset.value.bv.toNat + bytes.bv.toNat ≤ 512 := by
            rw [← hsum_toNat]; rw [BitVec.le_def] at hwithin; simpa using hwithin
          have hi1_lt64 : i1.val < 64 := by omega
          have hi2_le64 : i2.val ≤ 64 := by omega
          rw [bind_ok_eq hi1, bind_ok_eq hi2]
          by_cases heq12 : i1 = i2
          · subst heq12
            simp only [↓reduceIte]
            rw [bind_ok_eq hi3, bind_ok_eq hi4]
            obtain ⟨re, hre, hre_post⟩ := Stack_read_element_spec hs hi1_lt64
              (start_offset := i3) (end_offset := i4) (by omega) (by omega)
            refine ⟨re, hre, fun u hrok => Or.inr ⟨hble, hoff_kn, hoff_lt, hno_ovf, hwithin, ?_⟩⟩
            obtain ⟨d, hd, hne, hbytes⟩ := hre_post u hrok
            intro k hk
            have hdiv : (offset.value.bv.toNat + k) / 8 = i1.val := by omega
            rw [hdiv]
            exact ⟨d, hd, hne, hbytes _ (by omega) (by omega)⟩
          · have hi1_lt_i2 : i1.val < i2.val := by
              have hne_val : i1.val ≠ i2.val := fun hc => heq12 (usize_eq_of_val_eq hc)
              omega
            simp only [heq12, ↓reduceIte, verifier_STACK_ELEMENT_SIZE_eq]
            rw [bind_ok_eq hi3]
            have hmax64 := usize_max_ge_64
            rw [UScalar.max_USize_eq] at hmax64
            obtain ⟨i5, hi5, hi5_val⟩ := usize_add_eq_ok i1 1#usize (by rw [usize_one_val]; omega)
            rw [usize_one_val] at hi5_val
            obtain ⟨r1, hr1, hr1_post⟩ := Stack_read_element_spec hs hi1_lt64
              (start_offset := i3) (end_offset := 8#usize)
              (by rw [usize_const_val_8]; omega) (by rw [usize_const_val_8])
            rw [bind_ok_eq hr1]
            cases r1 with
            | Err e => exact ⟨.Err e, rfl, fun _ h => by cases h⟩
            | Ok u1 =>
              obtain ⟨d1, hd1, hne1, hbytes1⟩ := hr1_post u1 rfl
              rw [bind_ok_eq hi4]
              by_cases hi4_nz : (i4 != 0#usize) = true
              · have hi4_pos : 0 < i4.val := by
                  have hne0 : i4 ≠ 0#usize := bne_iff_ne.mp hi4_nz
                  have : i4.val ≠ 0 := fun hc => hne0 (usize_eq_of_val_eq (by simpa [usize_const_val_0] using hc))
                  omega
                have hi2_lt64 : i2.val < 64 := by omega
                simp only [hi4_nz, ↓reduceIte]
                obtain ⟨r2, hr2, hr2_post⟩ := Stack_read_element_spec hs hi2_lt64
                  (start_offset := 0#usize) (end_offset := i4)
                  (by rw [usize_const_val_0]; omega) (by omega)
                rw [bind_ok_eq hr2]
                cases r2 with
                | Err e => exact ⟨.Err e, rfl, fun _ h => by cases h⟩
                | Ok u2 =>
                  obtain ⟨d2, hd2, hne2, hbytes2⟩ := hr2_post u2 rfl
                  rw [bind_ok_eq hi5]
                  obtain ⟨rl, hrl, hrl_post⟩ := Stack_read_data_ptr_loop0_spec hs hi2_le64
                    (i2.val - i5.val) i5 rfl
                  refine ⟨rl, hrl, fun u hrok => Or.inr ⟨hble, hoff_kn, hoff_lt, hno_ovf, hwithin, ?_⟩⟩
                  have hmid := hrl_post u hrok
                  intro k hk
                  set pos := offset.value.bv.toNat + k
                  by_cases hp1 : pos / 8 = i1.val
                  · rw [hp1]
                    exact ⟨d1, hd1, hne1, hbytes1 _ (by omega) (by rw [usize_const_val_8]; omega)⟩
                  · by_cases hp2 : pos / 8 = i2.val
                    · rw [hp2]
                      exact ⟨d2, hd2, hne2, hbytes2 _ (by rw [usize_const_val_0]; omega) (by omega)⟩
                    · obtain ⟨dm, hdm, hnem, hbytesm⟩ := hmid (pos / 8) (by omega) (by omega)
                      exact ⟨dm, hdm, hnem, hbytesm _ (by omega)⟩
              · have hi4_z : i4.val = 0 := by
                  have hbeq : (i4 != 0#usize) = false := Bool.eq_false_iff.mpr hi4_nz
                  rw [bne_eq_false_iff_eq] at hbeq
                  rw [hbeq, usize_const_val_0]
                simp only [hi4_nz, Bool.false_eq_true, ↓reduceIte]
                rw [bind_ok_eq hi5]
                obtain ⟨rl, hrl, hrl_post⟩ := Stack_read_data_ptr_loop1_spec hs hi2_le64
                  (i2.val - i5.val) i5 rfl
                refine ⟨rl, hrl, fun u hrok => Or.inr ⟨hble, hoff_kn, hoff_lt, hno_ovf, hwithin, ?_⟩⟩
                have hmid := hrl_post u hrok
                intro k hk
                set pos := offset.value.bv.toNat + k
                by_cases hp1 : pos / 8 = i1.val
                · rw [hp1]
                  exact ⟨d1, hd1, hne1, hbytes1 _ (by omega) (by rw [usize_const_val_8]; omega)⟩
                · obtain ⟨dm, hdm, hnem, hbytesm⟩ := hmid (pos / 8) (by omega) (by omega)
                  exact ⟨dm, hdm, hnem, hbytesm _ (by omega)⟩
        · have hw_dec : decide ((knownScalar64 ⟨offset.value.bv + bytes.bv⟩).unknown_mask = 0#u64 ∧
              (knownScalar64 ⟨offset.value.bv + bytes.bv⟩).value.bv ≤ 512#64) = false :=
            decide_eq_false (fun h => hwithin h.2)
          simp only [hw_dec, Bool.false_eq_true, ↓reduceIte]
          exact ⟨.Err .StackAccessOutOfBounds, rfl, fun _ h => by cases h⟩

theorem Stack_read_data_ptr_rel2 {env : ExecEnv} {vCounter : Nat}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64}
    {self : verifier.Stack} (hs : StackValid env vCounter self)
    {stk₁ stk₂ : StackMem}
    {offset : verifier.StackOffset} {startOff bytes : BitVec 64}
    (hoff_rel : ConcretizesScalar64Rel2 offset startOff startOff)
    (hok : verifier.Stack.read_data_ptr self offset ⟨bytes⟩ = ok (.Ok ()))
    (hslots : bytes ≠ 0#64 → ∀ slot < 64,
      (∃ k < bytes.toNat, (startOff.toNat + k) / 8 = slot) →
      StackSlotRel2 env ρ resActive isNullId arrayLen (stackSlotType self slot) (stk₁ slot) (stk₂ slot)) :
    checkStackRangeReadable stk₁ startOff bytes = true ∧
    checkStackRangeReadable stk₂ startOff bytes = true := by
  obtain ⟨r, hr, hpost⟩ := Stack_read_data_ptr_spec hs offset ⟨bytes⟩
  rw [hok] at hr
  have hr_eq : r = .Ok () := (ok_inj hr).symm
  rcases hpost () hr_eq with hb0 | ⟨hble, hoff_kn, hoff_lt, hno_ovf, hwithin, hbytes⟩
  · have hb0_bv : bytes = 0#64 := (u64_eq_iff_bv ⟨bytes⟩ 0#u64).mp hb0
    subst hb0_bv
    simp [checkStackRangeReadable]
  · have hstart_eq : startOff = offset.value.bv := eq_value_of_known_64 hoff_kn hoff_rel.1
    subst hstart_eq
    by_cases hbz : bytes = 0#64
    · subst hbz; simp [checkStackRangeReadable]
    · have h1_512 : offset.value.bv.toNat < 512 := by rw [BitVec.lt_def] at hoff_lt; simpa using hoff_lt
      have h2_512 : bytes.toNat ≤ 512 := by
        have hb : bytes ≤ 512#64 := hble
        rw [BitVec.le_def] at hb; simpa using hb
      have hn_gt : ¬ (bytes > 512#64) := by
        intro hc
        change 512#64 < bytes at hc
        have h1 : 512 < bytes.toNat := by rw [BitVec.lt_def] at hc; simpa using hc
        omega
      have hn_ge : ¬ (offset.value.bv ≥ 512#64) := by
        intro hc
        change 512#64 ≤ offset.value.bv at hc
        have h1 : 512 ≤ offset.value.bv.toNat := by rw [BitVec.le_def] at hc; simpa using hc
        omega
      have hn_ovf : BitVec.uaddOverflow offset.value.bv bytes = false :=
        Bool.eq_false_iff.mpr hno_ovf
      have hsum_le512 : offset.value.bv.toNat + bytes.toNat ≤ 512 := by
        have hw : (offset.value.bv + bytes).toNat ≤ 512 := by
          have hw0 : offset.value.bv + bytes ≤ 512#64 := hwithin
          rw [BitVec.le_def] at hw0; simpa using hw0
        rw [BitVec.toNat_add, Nat.mod_eq_of_lt (by omega)] at hw
        exact hw
      have hn_sum_gt : ¬ ((offset.value.bv + bytes) > 512#64) := by
        intro hc
        change 512#64 < offset.value.bv + bytes at hc
        have h1 : 512 < (offset.value.bv + bytes).toNat := by
          rw [BitVec.lt_def] at hc; simpa using hc
        rw [BitVec.toNat_add, Nat.mod_eq_of_lt (by omega)] at h1
        omega
      unfold checkStackRangeReadable
      simp only [beq_iff_eq, hbz, hn_gt, hn_ge, hn_ovf, hn_sum_gt,
        decide_false, Bool.or_self, Bool.false_eq_true, ↓reduceIte]
      constructor
      · rw [List.all_eq_true]
        intro k hk
        rw [List.mem_range] at hk
        obtain ⟨d, hd, hne, hbk⟩ := hbytes k hk
        have hslot_lt64 : (offset.value.bv.toNat + k) / 8 < 64 := by omega
        have hrel_slot := hslots hbz ((offset.value.bv.toNat + k) / 8) hslot_lt64 ⟨k, hk, rfl⟩
        rw [hd] at hrel_slot
        simp only [StackSlotRel2, hne, ↓reduceIte] at hrel_slot
        rcases hrel_slot with ⟨v1, i1, v2, i2, hs1, _, _, hc1, _⟩
        have hm1 := StackSlotRel2_scalar_init_mask hc1
        have hi1 := extractSubValue_init_of_unwritten_zero (by omega) hm1 hbk
        rw [hs1]; dsimp only; rw [hi1]
        rfl
      · rw [List.all_eq_true]
        intro k hk
        rw [List.mem_range] at hk
        obtain ⟨d, hd, hne, hbk⟩ := hbytes k hk
        have hslot_lt64 : (offset.value.bv.toNat + k) / 8 < 64 := by omega
        have hrel_slot := hslots hbz ((offset.value.bv.toNat + k) / 8) hslot_lt64 ⟨k, hk, rfl⟩
        rw [hd] at hrel_slot
        simp only [StackSlotRel2, hne, ↓reduceIte] at hrel_slot
        rcases hrel_slot with ⟨v1, i1, v2, i2, _, hs2, _, _, hc2⟩
        have hm2 := StackSlotRel2_scalar_init_mask hc2
        have hi2 := extractSubValue_init_of_unwritten_zero (by omega) hm2 hbk
        rw [hs2]; dsimp only; rw [hi2]
        rfl

private theorem extractSubValue_tnum {x v um : BitVec 64} {subIdx : Nat} {w : DataWidth}
    (hsub : subIdx < 8)
    (htnum : x &&& ~~~um = v) :
    extractSubValue x subIdx w &&& ~~~(extractSubValue um subIdx w) =
      extractSubValue v subIdx w &&& ~~~(extractSubValue um subIdx w) := by
  unfold extractSubValue
  have hcases : subIdx = 0 ∨ subIdx = 1 ∨ subIdx = 2 ∨ subIdx = 3 ∨
      subIdx = 4 ∨ subIdx = 5 ∨ subIdx = 6 ∨ subIdx = 7 := by omega
  cases w <;> rcases hcases with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;>
    (simp only [DataWidth.valMask64]; revert htnum; bv_decide)

private theorem extractSubValue_rel_mask {x₁ x₂ unwr : BitVec 64} {subIdx : Nat} {w : DataWidth}
    (hsub : subIdx < 8)
    (hsec : x₁ &&& ~~~unwr = x₂ &&& ~~~unwr) :
    extractSubValue x₁ subIdx w &&& ~~~(extractSubValue unwr subIdx w) =
      extractSubValue x₂ subIdx w &&& ~~~(extractSubValue unwr subIdx w) := by
  unfold extractSubValue
  have hcases : subIdx = 0 ∨ subIdx = 1 ∨ subIdx = 2 ∨ subIdx = 3 ∨
      subIdx = 4 ∨ subIdx = 5 ∨ subIdx = 6 ∨ subIdx = 7 := by omega
  cases w <;> rcases hcases with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;>
    (simp only [DataWidth.valMask64]; revert hsec; bv_decide)

private theorem extractSubValue_init_of_extremes_eq {val init : BitVec 64}
    {subIdx : Nat} {w : DataWidth}
    (hfit : subIdx + w.bytes ≤ 8)
    (heq : extractSubValue (val &&& init) subIdx w = extractSubValue (val ||| ~~~init) subIdx w) :
    extractSubValue init subIdx w = w.valMask64 := by
  unfold extractSubValue DataWidth.valMask64 at heq ⊢
  cases w with
  | u8 =>
    have hcases : subIdx = 0 ∨ subIdx = 1 ∨ subIdx = 2 ∨ subIdx = 3 ∨
        subIdx = 4 ∨ subIdx = 5 ∨ subIdx = 6 ∨ subIdx = 7 := by
      simp [DataWidth.bytes] at hfit; omega
    rcases hcases with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;> (revert heq; bv_decide)
  | u16 =>
    have hcases : subIdx = 0 ∨ subIdx = 1 ∨ subIdx = 2 ∨ subIdx = 3 ∨
        subIdx = 4 ∨ subIdx = 5 ∨ subIdx = 6 := by
      simp [DataWidth.bytes] at hfit; omega
    rcases hcases with rfl | rfl | rfl | rfl | rfl | rfl | rfl <;> (revert heq; bv_decide)
  | u32 =>
    have hcases : subIdx = 0 ∨ subIdx = 1 ∨ subIdx = 2 ∨ subIdx = 3 ∨ subIdx = 4 := by
      simp [DataWidth.bytes] at hfit; omega
    rcases hcases with rfl | rfl | rfl | rfl | rfl <;> (revert heq; bv_decide)
  | u64 =>
    have h0 : subIdx = 0 := by simp [DataWidth.bytes] at hfit; omega
    subst h0; revert heq; bv_decide

private theorem insertSubValue_tnum {ox ov oum sx sv sum : BitVec 64}
    {subIdx : Nat} {w : DataWidth}
    (hsub : subIdx < 8)
    (ho : ox &&& ~~~oum = ov)
    (hs : sx &&& ~~~sum = sv) :
    insertSubValue ox sx subIdx w &&& ~~~(insertSubValue oum sum subIdx w) =
      insertSubValue ov sv subIdx w &&& ~~~(insertSubValue oum sum subIdx w) := by
  unfold insertSubValue
  have hcases : subIdx = 0 ∨ subIdx = 1 ∨ subIdx = 2 ∨ subIdx = 3 ∨
      subIdx = 4 ∨ subIdx = 5 ∨ subIdx = 6 ∨ subIdx = 7 := by omega
  cases w <;> rcases hcases with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;>
    (simp only [DataWidth.valMask64]; revert ho hs; bv_decide)

private theorem insertSubValue_rel_mask {ox₁ ox₂ ounwr sx₁ sx₂ sunwr : BitVec 64}
    {subIdx : Nat} {w : DataWidth}
    (hsub : subIdx < 8)
    (ho : ox₁ &&& ~~~ounwr = ox₂ &&& ~~~ounwr)
    (hs : sx₁ &&& ~~~sunwr = sx₂ &&& ~~~sunwr) :
    insertSubValue ox₁ sx₁ subIdx w &&& ~~~(insertSubValue ounwr sunwr subIdx w) =
      insertSubValue ox₂ sx₂ subIdx w &&& ~~~(insertSubValue ounwr sunwr subIdx w) := by
  unfold insertSubValue
  have hcases : subIdx = 0 ∨ subIdx = 1 ∨ subIdx = 2 ∨ subIdx = 3 ∨
      subIdx = 4 ∨ subIdx = 5 ∨ subIdx = 6 ∨ subIdx = 7 := by omega
  cases w <;> rcases hcases with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;>
    (simp only [DataWidth.valMask64]; revert ho hs; bv_decide)

private theorem insertSubValue_extreme_lo (rawV s rawInit : BitVec 64)
    {subIdx : Nat} {w : DataWidth} (hsub : subIdx < 8) :
    insertSubValue rawV s subIdx w &&& insertSubValue rawInit (~~~0#64) subIdx w =
      insertSubValue (rawV &&& rawInit) s subIdx w := by
  unfold insertSubValue
  have hcases : subIdx = 0 ∨ subIdx = 1 ∨ subIdx = 2 ∨ subIdx = 3 ∨
      subIdx = 4 ∨ subIdx = 5 ∨ subIdx = 6 ∨ subIdx = 7 := by omega
  cases w <;> rcases hcases with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;>
    (simp only [DataWidth.valMask64]; bv_decide)

private theorem insertSubValue_extreme_hi (rawV s rawInit : BitVec 64)
    {subIdx : Nat} {w : DataWidth} (hsub : subIdx < 8) :
    insertSubValue rawV s subIdx w ||| ~~~(insertSubValue rawInit (~~~0#64) subIdx w) =
      insertSubValue (rawV ||| ~~~rawInit) s subIdx w := by
  unfold insertSubValue
  have hcases : subIdx = 0 ∨ subIdx = 1 ∨ subIdx = 2 ∨ subIdx = 3 ∨
      subIdx = 4 ∨ subIdx = 5 ∨ subIdx = 6 ∨ subIdx = 7 := by omega
  cases w <;> rcases hcases with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;>
    (simp only [DataWidth.valMask64]; bv_decide)

private theorem subword_aligned_fit {subIdx : Nat} (hsub : subIdx < 8) (w : DataWidth)
    (hal : subIdx % w.bytes = 0) :
    subIdx + w.bytes ≤ 8 := by
  cases w <;> (simp only [DataWidth.bytes] at hal ⊢; omega)

private theorem TypeRel2_ptr_nonscalar_eq {env : ExecEnv} {vCounter : Nat}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64}
    {t : verifier.Type} {v₁ v₂ : RuntimeVal}
    (ht : TypeValid env vCounter t)
    (hrel : TypeRel2 env ρ resActive isNullId arrayLen t v₁ v₂)
    (hnonnull : match t with
      | .ScalarValue _ => False
      | .NullOr nullId _ => isNullId (renameMemId ρ nullId) = false
      | _ => True) :
    v₁ = v₂ ∧ v₁.isScalar = false := by
  induction t with
  | ScalarValue _ => exact False.elim hnonnull
  | ConstPtrToMap _ _ => rcases hrel with ⟨_, rfl, rfl⟩; exact ⟨rfl, rfl⟩
  | PtrToStack _ => rcases hrel with ⟨_, _, _, rfl, rfl⟩; exact ⟨rfl, rfl⟩
  | PtrToMemory _ _ _ _ => rcases hrel with ⟨_, _, _, rfl, rfl⟩; exact ⟨rfl, rfl⟩
  | PtrToStruct _ _ _ => rcases hrel with ⟨_, _, _, _, rfl, rfl⟩; exact ⟨rfl, rfl⟩
  | PtrToArray _ _ => rcases hrel with ⟨_, _, _, _, rfl, rfl⟩; exact ⟨rfl, rfl⟩
  | PtrToEndArray _ => rcases hrel with ⟨_, rfl, rfl⟩; exact ⟨rfl, rfl⟩
  | Releasable _ inner =>
    cases inner with
    | PtrToMemory _ _ _ _ => rcases hrel with ⟨_, _, _, _, rfl, rfl⟩; exact ⟨rfl, rfl⟩
    | PtrToStruct _ _ _ => rcases hrel with ⟨_, _, _, _, _, rfl, rfl⟩; exact ⟨rfl, rfl⟩
    | _ => exact False.elim hrel
  | NullOr nullId inner ih =>
    rcases ht with ⟨_, hshape, hinnerV⟩
    simp only [TypeRel2, hnonnull, Bool.false_eq_true, ↓reduceIte] at hrel
    cases inner <;> try exact False.elim hshape
    all_goals exact ih hinnerV hrel trivial
  | _ => exact False.elim hrel

theorem Stack_load_spec {env : ExecEnv} {vCounter : Nat}
    {self : verifier.Stack} (hs : StackValid env vCounter self)
    (offset : verifier.StackOffset) (width : api.DataWidth) :
    ∃ r : core.result.Result verifier.Type error.VerifierErrorKind,
      verifier.Stack.load self offset width = ok r ∧
      (∀ tOut, r = .Ok tOut → TypeValid env vCounter tOut) := by
  unfold verifier.Stack.load
  obtain ⟨rchk, hrchk, hchk_iff⟩ := StackOffset_check_valid_offset_spec offset
  rw [bind_ok_eq hrchk]
  cases rchk with
  | Err e => exact ⟨.Err e, rfl, fun _ h => by cases h⟩
  | Ok u0 =>
    obtain ⟨hoff_kn, hoff_lt⟩ := (hchk_iff u0).mp rfl
    have hoff_le : offset.value.bv ≤ 512#64 := BitVec.le_of_lt hoff_lt
    obtain ⟨sub, hsub, hsub_val, hsub_lt8⟩ := StackOffset_sub_index_spec hoff_le
    obtain ⟨bw, hbw, hbw_val⟩ := DataWidth_bytes_spec width
    have hbw_pos : 0 < bw.val := by
      rw [hbw_val]; cases width <;> simp [DataWidth.ofCore, DataWidth.bytes]
    obtain ⟨rem, hrem, hrem_val⟩ := usize_rem_eq_ok sub bw (by omega)
    rw [bind_ok_eq hsub, bind_ok_eq hbw, bind_ok_eq hrem]
    by_cases hmis : (rem != 0#usize) = true
    · simp only [hmis, ↓reduceIte]
      exact ⟨.Err .MisalignedStackAccess, rfl, fun _ h => by cases h⟩
    · simp only [hmis, Bool.false_eq_true, ↓reduceIte]
      have hoff_512 : offset.value.bv.toNat < 512 := by rw [BitVec.lt_def] at hoff_lt; simpa using hoff_lt
      obtain ⟨idx, hidx, hidx_val⟩ := StackOffset_array_index_spec hoff_le
      have hidx_lt64 : idx.val < 64 := by omega
      rw [bind_ok_eq hidx, bind_ok_eq (Stack_get_spec hs hidx_lt64),
          bind_ok_eq (Type_clone_spec _)]
      obtain ⟨beq, hbeq, heq_iff⟩ := DataWidth_eq_spec width .U64
      rw [bind_ok_eq hbeq]
      cases beq
      · simp only [Bool.false_eq_true, ↓reduceIte]
        cases hst : stackSlotType self idx.val <;>
          try exact ⟨.Err .PartialNonScalarStackLoad, rfl, fun _ h => by cases h⟩
        rename_i data
        dsimp only
        rw [bind_ok_eq (Stack_extract_sub_value_spec data.value sub bw),
            bind_ok_eq (Stack_extract_sub_value_spec data.unknown_mask sub bw),
            bind_ok_eq (Stack_extract_sub_value_spec data.unwritten_mask sub bw)]
        have hur0 : U64Range.from_scalar 0#u64 = ok { min := 0#u64, max := 0#u64 } := rfl
        rw [bind_ok_eq hur0,
            bind_ok_eq (compute_range_for_bytes_swap_64_eq_ok _ _ _ _ _),
            bind_ok_eq (new64_eq_ok _ _ _ _),
            bind_ok_eq (Type_is_initialized_spec _)]
        split_ifs
        · exact ⟨.Ok _, rfl, fun _ h => by
            cases h; exact pureNew64_valid _ _ _ _ _⟩
        · exact ⟨.Err .UninitializedStackRead, rfl, fun _ h => by cases h⟩
      · simp only [↓reduceIte]
        rw [bind_ok_eq (Type_is_initialized_spec _)]
        split_ifs
        · exact ⟨.Ok _, rfl, fun _ h => by
            cases h; exact StackValid_slot_valid hs idx.val⟩
        · exact ⟨.Err .UninitializedStackRead, rfl, fun _ h => by cases h⟩

theorem Stack_load_rel2 {env : ExecEnv} {vCounter : Nat}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64}
    {self : verifier.Stack} (hs : StackValid env vCounter self)
    {stk₁ stk₂ : StackMem}
    {offset : verifier.StackOffset} {effOff : BitVec 64} {width : api.DataWidth}
    {tOut : verifier.Type}
    (hoff_rel : ConcretizesScalar64Rel2 offset effOff effOff)
    (hok : verifier.Stack.load self offset width = ok (.Ok tOut))
    (hslot_rel : StackSlotRel2 env ρ resActive isNullId arrayLen
      (stackSlotType self (effOff.toNat / 8))
      (stk₁ (effOff.toNat / 8)) (stk₂ (effOff.toNat / 8))) :
    let w := DataWidth.ofCore width
    let slotIdx := effOff.toNat / 8
    let subIdx := effOff.toNat % 8
    effOff < 512#64 ∧
    subIdx % w.bytes = 0 ∧
    TypeValid env vCounter tOut ∧
    ∃ vOut₁ vOut₂ : RuntimeVal,
      (match stk₁ slotIdx with
       | .spilled cap => w = .u64 ∧ vOut₁ = cap
       | .bytes val initMask =>
           extractSubValue initMask subIdx w = w.valMask64 ∧
           vOut₁ = .scalar (extractSubValue val subIdx w)) ∧
      (match stk₂ slotIdx with
       | .spilled cap => w = .u64 ∧ vOut₂ = cap
       | .bytes val initMask =>
           extractSubValue initMask subIdx w = w.valMask64 ∧
           vOut₂ = .scalar (extractSubValue val subIdx w)) ∧
      TypeRel2 env ρ resActive isNullId arrayLen tOut vOut₁ vOut₂ := by
  have htOut_valid : TypeValid env vCounter tOut := by
    obtain ⟨r, hr, hpost⟩ := Stack_load_spec hs offset width
    rw [hok] at hr
    exact hpost tOut (ok_inj hr).symm
  unfold verifier.Stack.load at hok
  obtain ⟨rchk, hrchk, hchk_iff⟩ := StackOffset_check_valid_offset_spec offset
  rw [bind_ok_eq hrchk] at hok
  cases rchk with
  | Err _ => cases ok_inj hok
  | Ok u0 =>
    obtain ⟨hoff_kn, hoff_lt⟩ := (hchk_iff u0).mp rfl
    have heff_eq : effOff = offset.value.bv := eq_value_of_known_64 hoff_kn hoff_rel.1
    subst heff_eq
    have hoff_le : offset.value.bv ≤ 512#64 := BitVec.le_of_lt hoff_lt
    obtain ⟨sub, hsub, hsub_val, hsub_lt8⟩ := StackOffset_sub_index_spec hoff_le
    obtain ⟨bw, hbw, hbw_val⟩ := DataWidth_bytes_spec width
    have hbw_pos : 0 < bw.val := by
      rw [hbw_val]; cases width <;> simp [DataWidth.ofCore, DataWidth.bytes]
    obtain ⟨rem, hrem, hrem_val⟩ := usize_rem_eq_ok sub bw (by omega)
    rw [bind_ok_eq hsub, bind_ok_eq hbw, bind_ok_eq hrem] at hok
    by_cases hmis : (rem != 0#usize) = true
    · simp only [hmis, ↓reduceIte] at hok; cases ok_inj hok
    · simp only [hmis, Bool.false_eq_true, ↓reduceIte] at hok
      have hrem0 : rem.val = 0 := by
        have hbeq : (rem != 0#usize) = false := Bool.eq_false_iff.mpr hmis
        rw [bne_eq_false_iff_eq] at hbeq
        rw [hbeq, usize_const_val_0]
      have hal : (offset.value.bv.toNat % 8) % (DataWidth.ofCore width).bytes = 0 := by
        rw [← hsub_val, ← hbw_val, ← hrem_val, hrem0]
      have hoff_512 : offset.value.bv.toNat < 512 := by rw [BitVec.lt_def] at hoff_lt; simpa using hoff_lt
      obtain ⟨idx, hidx, hidx_val⟩ := StackOffset_array_index_spec hoff_le
      have hidx_lt64 : idx.val < 64 := by omega
      rw [bind_ok_eq hidx, bind_ok_eq (Stack_get_spec hs hidx_lt64),
          bind_ok_eq (Type_clone_spec _)] at hok
      obtain ⟨beq, hbeq, heq_iff⟩ := DataWidth_eq_spec width .U64
      rw [bind_ok_eq hbeq] at hok
      refine ⟨hoff_lt, hal, htOut_valid, ?_⟩
      cases beq
      · -- Narrow load (`width ≠ .U64`)
        have hw_ne : width ≠ .U64 := fun hc => absurd (heq_iff.mpr hc) (by simp)
        have hw_cases : DataWidth.ofCore width = .u8 ∨ DataWidth.ofCore width = .u16 ∨ DataWidth.ofCore width = .u32 := by
          cases width <;> simp [DataWidth.ofCore] at hw_ne ⊢
        simp only [Bool.false_eq_true, ↓reduceIte] at hok
        have hslot_eq : stackSlotType self idx.val = stackSlotType self (offset.value.bv.toNat / 8) := by
          rw [hidx_val]
        rw [hslot_eq] at hok
        cases hst : stackSlotType self (offset.value.bv.toNat / 8) <;>
          rw [hst] at hok <;> try (cases ok_inj hok)
        rename_i data
        dsimp only at hok
        rw [bind_ok_eq (Stack_extract_sub_value_spec data.value sub bw),
            bind_ok_eq (Stack_extract_sub_value_spec data.unknown_mask sub bw),
            bind_ok_eq (Stack_extract_sub_value_spec data.unwritten_mask sub bw)] at hok
        set w := DataWidth.ofCore width
        have hex_val : pureExtractSubBytes data.value.bv sub.val bw.val = extractSubValue data.value.bv sub.val w := by
          rw [hbw_val]; exact pureExtractSubBytes_eq_extractSubValue _ hsub_lt8 w
        have hex_um : pureExtractSubBytes data.unknown_mask.bv sub.val bw.val = extractSubValue data.unknown_mask.bv sub.val w := by
          rw [hbw_val]; exact pureExtractSubBytes_eq_extractSubValue _ hsub_lt8 w
        have hex_unw : pureExtractSubBytes data.unwritten_mask.bv sub.val bw.val = extractSubValue data.unwritten_mask.bv sub.val w := by
          rw [hbw_val]; exact pureExtractSubBytes_eq_extractSubValue _ hsub_lt8 w
        have hfit : sub.val + w.bytes ≤ 8 := by
          apply subword_aligned_fit hsub_lt8 w
          rwa [hsub_val]
        have hur0 : U64Range.from_scalar 0#u64 = ok { min := 0#u64, max := 0#u64 } := rfl
        rw [bind_ok_eq hur0] at hok
        obtain ⟨rOut_u, hswap_u⟩ : ∃ rOut, U64Range.compute_range_for_bytes_swap { min := 0#u64, max := 0#u64 } data.urange 0#usize sub bw = ok rOut :=
          ⟨_, compute_range_for_bytes_swap_64_eq_ok _ _ _ _ _⟩
        rw [bind_ok_eq hswap_u] at hok
        obtain ⟨usvd, husvd⟩ : ∃ u, U64ScalarValueData.new ⟨pureExtractSubBytes data.value.bv sub.val bw.val⟩
            ⟨pureExtractSubBytes data.unknown_mask.bv sub.val bw.val⟩
            ⟨pureExtractSubBytes data.unwritten_mask.bv sub.val bw.val⟩ rOut_u = ok u :=
          ⟨_, new64_eq_ok _ _ _ _⟩
        rw [bind_ok_eq husvd, bind_ok_eq (Type_is_initialized_spec _)] at hok
        by_cases hunw0 : usvd.unwritten_mask = 0#u64
        · simp only [hunw0, decide_true, ↓reduceIte] at hok
          cases ok_inj hok
          have hdata_ne : data ≠ uninitScalar64 := by
            intro hc; subst hc
            have hc_uninit := concretizesScalar64Rel2_uninit 0#64 (~~~0#64)
            obtain ⟨r0, hr0, hcr0⟩ := compute_range_for_bytes_swap_load_sound uninitScalar64.urange sub bw w 0#64
              hw_cases hbw_val hfit hc_uninit.1.2.2
            obtain ⟨r1, hr1, hcr1⟩ := compute_range_for_bytes_swap_load_sound uninitScalar64.urange sub bw w (~~~0#64)
              hw_cases hbw_val hfit hc_uninit.2.1.2.2
            rw [hswap_u] at hr0 hr1
            have hr0_eq := ok_inj hr0
            have hr1_eq := ok_inj hr1
            rw [← hr0_eq] at hcr0
            rw [← hr1_eq] at hcr1
            obtain ⟨u', hu', hrel_u⟩ := new64_sound_2safety
              ⟨pureExtractSubBytes uninitScalar64.value.bv sub.val bw.val⟩
              ⟨pureExtractSubBytes uninitScalar64.unknown_mask.bv sub.val bw.val⟩
              ⟨pureExtractSubBytes uninitScalar64.unwritten_mask.bv sub.val bw.val⟩
              rOut_u (extractSubValue 0#64 sub.val w) (extractSubValue (~~~0#64) sub.val w)
              hcr0 (by simp only [u64_mk_bv]; rw [hex_val, hex_um]; exact extractSubValue_tnum hsub_lt8 hc_uninit.1.2.1)
              hcr1 (by simp only [u64_mk_bv]; rw [hex_val, hex_um]; exact extractSubValue_tnum hsub_lt8 hc_uninit.2.1.2.1)
              (by simp only [u64_mk_bv]; rw [hex_unw]; exact extractSubValue_rel_mask hsub_lt8 hc_uninit.2.2)
            rw [husvd] at hu'
            have hu_eq := ok_inj hu'
            rw [← hu_eq] at hrel_u
            have heq_ext := fully_initialized_eq_of_rel2_64 hunw0 hrel_u
            rcases hw_cases with hw8 | hw16 | hw32 <;> {
              try rw [hw8] at heq_ext
              try rw [hw16] at heq_ext
              try rw [hw32] at heq_ext
              unfold extractSubValue DataWidth.valMask64 at heq_ext
              have hc8 : sub.val = 0 ∨ sub.val = 1 ∨ sub.val = 2 ∨ sub.val = 3 ∨
                  sub.val = 4 ∨ sub.val = 5 ∨ sub.val = 6 ∨ sub.val = 7 := by omega
              rcases hc8 with h | h | h | h | h | h | h | h <;> (rw [h] at heq_ext; revert heq_ext; decide)
            }
          rw [hst] at hslot_rel
          simp only [StackSlotRel2, hdata_ne, ↓reduceIte] at hslot_rel
          rcases hslot_rel with ⟨val₁, init₁, val₂, init₂, hs1, hs2, hconc12, hconc_e1, hconc_e2⟩
          have hload_pair : ∀ x y : BitVec 64, ConcretizesScalar64Rel2 data x y →
              ConcretizesScalar64Rel2 usvd (extractSubValue x sub.val w) (extractSubValue y sub.val w) := by
            intro x y hxy
            obtain ⟨rx, hrx, hcrx⟩ := compute_range_for_bytes_swap_load_sound data.urange sub bw w x hw_cases hbw_val hfit hxy.1.2.2
            obtain ⟨ry, hry, hcry⟩ := compute_range_for_bytes_swap_load_sound data.urange sub bw w y hw_cases hbw_val hfit hxy.2.1.2.2
            rw [hswap_u] at hrx hry
            have hrx_eq := ok_inj hrx
            have hry_eq := ok_inj hry
            rw [← hrx_eq] at hcrx
            rw [← hry_eq] at hcry
            obtain ⟨u', hu', hrel_u⟩ := new64_sound_2safety
              ⟨pureExtractSubBytes data.value.bv sub.val bw.val⟩
              ⟨pureExtractSubBytes data.unknown_mask.bv sub.val bw.val⟩
              ⟨pureExtractSubBytes data.unwritten_mask.bv sub.val bw.val⟩
              rOut_u (extractSubValue x sub.val w) (extractSubValue y sub.val w)
              hcrx (by simp only [u64_mk_bv]; rw [hex_val, hex_um]; exact extractSubValue_tnum hsub_lt8 hxy.1.2.1)
              hcry (by simp only [u64_mk_bv]; rw [hex_val, hex_um]; exact extractSubValue_tnum hsub_lt8 hxy.2.1.2.1)
              (by simp only [u64_mk_bv]; rw [hex_unw]; exact extractSubValue_rel_mask hsub_lt8 hxy.2.2)
            rw [husvd] at hu'
            have hu_eq := ok_inj hu'
            rwa [← hu_eq] at hrel_u
          have hrel_main := hload_pair val₁ val₂ hconc12
          have hrel_e1 := hload_pair (val₁ &&& init₁) (val₁ ||| ~~~init₁) hconc_e1
          have hrel_e2 := hload_pair (val₂ &&& init₂) (val₂ ||| ~~~init₂) hconc_e2
          have hinit1 := extractSubValue_init_of_extremes_eq hfit (fully_initialized_eq_of_rel2_64 hunw0 hrel_e1)
          have hinit2 := extractSubValue_init_of_extremes_eq hfit (fully_initialized_eq_of_rel2_64 hunw0 hrel_e2)
          have husvd_ne : usvd ≠ uninitScalar64 := by
            intro hc; subst hc; exact uninitScalar64_unwritten_ne_zero hunw0
          rw [hsub_val] at hrel_main hinit1 hinit2
          refine ⟨.scalar (extractSubValue val₁ (offset.value.bv.toNat % 8) w),
                  .scalar (extractSubValue val₂ (offset.value.bv.toNat % 8) w), ?_, ?_, ?_⟩
          · rw [hs1]; exact ⟨hinit1, rfl⟩
          · rw [hs2]; exact ⟨hinit2, rfl⟩
          · simp only [TypeRel2, husvd_ne, ↓reduceIte]
            exact ⟨_, _, rfl, rfl, hrel_main⟩
        · simp only [hunw0, decide_false, Bool.false_eq_true, ↓reduceIte] at hok
          cases ok_inj hok
      · -- 64-bit load (`width = .U64`)
        have hw_eq : width = .U64 := heq_iff.mp rfl
        subst hw_eq
        simp only [↓reduceIte] at hok
        have hsub0 : offset.value.bv.toNat % 8 = 0 := by
          simp [DataWidth.ofCore, DataWidth.bytes] at hal; exact hal
        have hex64_id : ∀ x : BitVec 64, extractSubValue x 0 .u64 = x := by
          intro x; unfold extractSubValue DataWidth.valMask64; bv_decide
        rw [hidx_val] at hok
        rw [bind_ok_eq (Type_is_initialized_spec _)] at hok
        cases hst : stackSlotType self (offset.value.bv.toNat / 8)
        all_goals rw [hst] at hok hslot_rel
        case ScalarValue d =>
          by_cases hd0 : d.unwritten_mask = 0#u64
          · simp only [hd0, decide_true, ↓reduceIte] at hok
            cases ok_inj hok
            have hd_ne : d ≠ uninitScalar64 := by
              intro hc; subst hc; exact uninitScalar64_unwritten_ne_zero hd0
            simp only [StackSlotRel2, hd_ne, ↓reduceIte] at hslot_rel
            rcases hslot_rel with ⟨v1, i1, v2, i2, hs1, hs2, hconc, hc1, hc2⟩
            have hm1 := StackSlotRel2_scalar_init_mask hc1
            have hm2 := StackSlotRel2_scalar_init_mask hc2
            have hi1_full : i1 = ~~~0#64 := by
              have hd0_bv : d.unwritten_mask.bv = 0#64 := by
                rw [(u64_eq_iff_bv _ _).mp hd0, u64_zero_bv]
              rw [hd0_bv] at hm1; revert hm1; bv_decide
            have hi2_full : i2 = ~~~0#64 := by
              have hd0_bv : d.unwritten_mask.bv = 0#64 := by
                rw [(u64_eq_iff_bv _ _).mp hd0, u64_zero_bv]
              rw [hd0_bv] at hm2; revert hm2; bv_decide
            refine ⟨.scalar v1, .scalar v2, ?_, ?_, ?_⟩
            · dsimp only [DataWidth.ofCore]; rw [hs1, hsub0, hi1_full]; dsimp only; rw [hex64_id, hex64_id]; exact ⟨rfl, rfl⟩
            · dsimp only [DataWidth.ofCore]; rw [hs2, hsub0, hi2_full]; dsimp only; rw [hex64_id, hex64_id]; exact ⟨rfl, rfl⟩
            · simp only [TypeRel2, hd_ne, ↓reduceIte]; exact ⟨v1, v2, rfl, rfl, hconc⟩
          · simp only [hd0, decide_false, Bool.false_eq_true, ↓reduceIte] at hok
            cases ok_inj hok
        case NullOr nullId inner =>
          simp only [↓reduceIte] at hok
          cases ok_inj hok
          rcases htOut_valid with ⟨_, hshape, _⟩
          unfold StackSlotRel2 at hslot_rel
          by_cases hn : isNullId (renameMemId ρ nullId)
          · simp only [hn, ↓reduceIte] at hslot_rel
            rcases hslot_rel with ⟨hinact, hs1, hs2⟩
            refine ⟨.scalar 0#64, .scalar 0#64, ?_, ?_, ?_⟩
            · dsimp only [DataWidth.ofCore]; rw [hs1, hsub0]; dsimp only; rw [hex64_id, hex64_id]; exact ⟨rfl, rfl⟩
            · dsimp only [DataWidth.ofCore]; rw [hs2, hsub0]; dsimp only; rw [hex64_id, hex64_id]; exact ⟨rfl, rfl⟩
            · simp [TypeRel2, hn, hinact]
          · simp only [hn, Bool.false_eq_true, ↓reduceIte] at hslot_rel
            have hex_cap : ∃ cap : RuntimeVal,
                cap.isScalar = false ∧
                stk₁ (offset.value.bv.toNat / 8) = .spilled cap ∧
                stk₂ (offset.value.bv.toNat / 8) = .spilled cap ∧
                TypeRel2 env ρ resActive isNullId arrayLen inner cap cap := by
              cases inner <;> try exact False.elim hshape
              all_goals exact hslot_rel
            rcases hex_cap with ⟨cap, _, hs1, hs2, htr⟩
            refine ⟨cap, cap, ?_, ?_, ?_⟩
            · rw [hs1]; exact ⟨rfl, rfl⟩
            · rw [hs2]; exact ⟨rfl, rfl⟩
            · simp [TypeRel2, hn, htr]
        all_goals {
          simp only [↓reduceIte] at hok
          cases ok_inj hok
          rcases hslot_rel with ⟨cap, _, hs1, hs2, htr⟩
          refine ⟨cap, cap, ?_, ?_, htr⟩
          · rw [hs1]; exact ⟨rfl, rfl⟩
          · rw [hs2]; exact ⟨rfl, rfl⟩
        }

private def stackStoreOldScalar : verifier.Type → Result U64ScalarValueData
  | .ScalarValue d => ok d
  | .ConstPtrToMap _ _ => U64ScalarValueData.UNINITIALIZED
  | .PtrToStack _ => U64ScalarValueData.UNINITIALIZED
  | .PtrToMemory _ _ _ _ => U64ScalarValueData.UNINITIALIZED
  | .PtrToStruct _ _ _ => U64ScalarValueData.UNINITIALIZED
  | .PtrToArray _ _ => U64ScalarValueData.UNINITIALIZED
  | .PtrToEndArray _ => U64ScalarValueData.UNINITIALIZED
  | .NullOr _ _ => U64ScalarValueData.UNINITIALIZED
  | .Releasable _ _ => U64ScalarValueData.UNINITIALIZED
  | .ScalarValueParameter => U64ScalarValueData.UNINITIALIZED
  | .ConstPtrToMapParameter _ _ => U64ScalarValueData.UNINITIALIZED
  | .MapKeyParameter _ => U64ScalarValueData.UNINITIALIZED
  | .MapValueParameter _ => U64ScalarValueData.UNINITIALIZED
  | .MemoryParameter _ _ _ => U64ScalarValueData.UNINITIALIZED
  | .NullOrParameter _ => U64ScalarValueData.UNINITIALIZED
  | .StructParameter _ => U64ScalarValueData.UNINITIALIZED
  | .ContextParameter _ => U64ScalarValueData.UNINITIALIZED
  | .ReleasableParameter _ _ => U64ScalarValueData.UNINITIALIZED
  | .ReleaseParameter _ => U64ScalarValueData.UNINITIALIZED
  | .AnyParameter => U64ScalarValueData.UNINITIALIZED

private theorem stackStoreOldScalar_eq (t : verifier.Type) :
    stackStoreOldScalar t = ok (match t with | .ScalarValue d => d | _ => uninitScalar64) := by
  cases t <;> simp [stackStoreOldScalar, UNINITIALIZED_64_eq]

theorem Stack_store_spec {env : ExecEnv} {vCounter : Nat}
    {self : verifier.Stack} (hs : StackValid env vCounter self)
    (offset : verifier.StackOffset) {value : verifier.Type}
    (hv : TypeValid env vCounter value) (width : api.DataWidth) :
    ∃ (r : core.result.Result Unit error.VerifierErrorKind) (s' : verifier.Stack),
      verifier.Stack.store self offset value width = ok (r, s') ∧
      StackValid env vCounter s' ∧
      (∀ e, r = .Err e → s' = self) ∧
      (∀ u, r = .Ok u →
        offset.unknown_mask = 0#u64 ∧
        offset.value.bv < 512#64 ∧
        (offset.value.bv.toNat % 8) % (DataWidth.ofCore width).bytes = 0 ∧
        (∀ slot < 64, slot ≠ offset.value.bv.toNat / 8 →
          stackSlotType s' slot = stackSlotType self slot) ∧
        (width = .U64 → stackSlotType s' (offset.value.bv.toNat / 8) = value) ∧
        (width ≠ .U64 → ∃ data : U64ScalarValueData,
          value = .ScalarValue data ∧
          extractSubValue data.unwritten_mask.bv 0 (DataWidth.ofCore width) = 0#64)) := by
  unfold verifier.Stack.store
  obtain ⟨rchk, hrchk, hchk_iff⟩ := StackOffset_check_valid_offset_spec offset
  rw [bind_ok_eq hrchk]
  cases rchk with
  | Err e =>
    exact ⟨.Err e, self, rfl, hs, fun _ _ => rfl, fun _ h => by cases h⟩
  | Ok u0 =>
    obtain ⟨hoff_kn, hoff_lt⟩ := (hchk_iff u0).mp rfl
    have hoff_le : offset.value.bv ≤ 512#64 := BitVec.le_of_lt hoff_lt
    obtain ⟨sub, hsub, hsub_val, hsub_lt8⟩ := StackOffset_sub_index_spec hoff_le
    obtain ⟨bw, hbw, hbw_val⟩ := DataWidth_bytes_spec width
    have hbw_pos : 0 < bw.val := by
      rw [hbw_val]; cases width <;> simp [DataWidth.ofCore, DataWidth.bytes]
    obtain ⟨rem, hrem, hrem_val⟩ := usize_rem_eq_ok sub bw (by omega)
    rw [bind_ok_eq hsub, bind_ok_eq hbw, bind_ok_eq hrem]
    by_cases hmis : (rem != 0#usize) = true
    · simp only [hmis, ↓reduceIte]
      exact ⟨.Err .MisalignedStackAccess, self, rfl, hs, fun _ _ => rfl, fun _ h => by cases h⟩
    · simp only [hmis, Bool.false_eq_true, ↓reduceIte]
      have hrem0 : rem.val = 0 := by
        have hbeq : (rem != 0#usize) = false := Bool.eq_false_iff.mpr hmis
        rw [bne_eq_false_iff_eq] at hbeq
        rw [hbeq, usize_const_val_0]
      have hal : (offset.value.bv.toNat % 8) % (DataWidth.ofCore width).bytes = 0 := by
        rw [← hsub_val, ← hbw_val, ← hrem_val, hrem0]
      have hoff_512 : offset.value.bv.toNat < 512 := by rw [BitVec.lt_def] at hoff_lt; simpa using hoff_lt
      obtain ⟨idx, hidx, hidx_val⟩ := StackOffset_array_index_spec hoff_le
      have hidx_lt64 : idx.val < 64 := by omega
      obtain ⟨beq, hbeq, heq_iff⟩ := DataWidth_eq_spec width .U64
      rw [bind_ok_eq hidx, bind_ok_eq hbeq]
      cases beq
      · -- Narrow store (`width ≠ .U64`)
        have hw_ne : width ≠ .U64 := fun hc => absurd (heq_iff.mpr hc) (by simp)
        simp only [Bool.false_eq_true, ↓reduceIte]
        cases value <;>
          try exact ⟨.Err .PartialNonScalarStackStore, self, rfl, hs, fun _ _ => rfl, fun _ h => by cases h⟩
        rename_i data
        dsimp only
        rw [bind_ok_eq (Stack_extract_sub_value_spec data.unwritten_mask 0#usize bw)]
        by_cases hinv : (⟨pureExtractSubBytes data.unwritten_mask.bv (0#usize).val bw.val⟩ : Std.U64) != 0#u64
        · simp only [hinv, ↓reduceIte]
          exact ⟨.Err .InvalidStoredValue, self, rfl, hs, fun _ _ => rfl, fun _ h => by cases h⟩
        · simp only [hinv, Bool.false_eq_true, ↓reduceIte]
          have hunw_z : extractSubValue data.unwritten_mask.bv 0 (DataWidth.ofCore width) = 0#64 := by
            have hbeq0 : (⟨pureExtractSubBytes data.unwritten_mask.bv (0#usize).val bw.val⟩ : Std.U64) = 0#u64 :=
              bne_eq_false_iff_eq.mp (Bool.eq_false_iff.mpr hinv)
            have hbv0 : pureExtractSubBytes data.unwritten_mask.bv (0#usize).val bw.val = 0#64 := by
              simpa [u64_mk_bv, u64_zero_bv] using (u64_eq_iff_bv _ _).mp hbeq0
            rw [usize_const_val_0, hbw_val] at hbv0
            rwa [pureExtractSubBytes_eq_extractSubValue _ (by omega) _] at hbv0
          set w := DataWidth.ofCore width
          set old_data : U64ScalarValueData :=
            match stackSlotType self idx.val with
            | .ScalarValue d => d
            | _ => uninitScalar64
          have hfit : sub.val + w.bytes ≤ 8 := by
            apply subword_aligned_fit hsub_lt8
            rwa [hsub_val]
          obtain ⟨rOut_s, hswap_s⟩ : ∃ rOut, U64Range.compute_range_for_bytes_swap old_data.urange data.urange sub 0#usize bw = ok rOut :=
            ⟨_, compute_range_for_bytes_swap_64_eq_ok _ _ _ _ _⟩
          obtain ⟨usvd, husvd⟩ : ∃ u, U64ScalarValueData.new
              ⟨insertSubValue old_data.value.bv data.value.bv sub.val w⟩
              ⟨insertSubValue old_data.unknown_mask.bv data.unknown_mask.bv sub.val w⟩
              ⟨insertSubValue old_data.unwritten_mask.bv data.unwritten_mask.bv sub.val w⟩
              rOut_s = ok u := ⟨_, new64_eq_ok _ _ _ _⟩
          have husvd_valid : TypeValid env vCounter (.ScalarValue usvd) := by
            have hnew := new64_eq_ok
              ⟨insertSubValue old_data.value.bv data.value.bv sub.val w⟩
              ⟨insertSubValue old_data.unknown_mask.bv data.unknown_mask.bv sub.val w⟩
              ⟨insertSubValue old_data.unwritten_mask.bv data.unwritten_mask.bv sub.val w⟩
              rOut_s
            rw [husvd] at hnew
            cases ok_inj hnew
            exact pureNew64_valid _ _ _ _ _
          obtain ⟨s', hset, hs'_valid, hs'_slots⟩ := Stack_set_spec hs hidx_lt64 husvd_valid
          refine ⟨.Ok (), s', ?_, hs'_valid, (fun _ h => by cases h), ?_⟩
          · rw [bind_ok_eq (Stack_get_spec hs hidx_lt64)]
            change (do
              let od ← stackStoreOldScalar (stackSlotType self idx.val)
              let new_val ← verifier.Stack.insert_sub_value od.value data.value width sub
              let unknown_mask ← verifier.Stack.insert_sub_value od.unknown_mask data.unknown_mask width sub
              let unwritten_mask ← verifier.Stack.insert_sub_value od.unwritten_mask data.unwritten_mask width sub
              let urange ← U64Range.compute_range_for_bytes_swap od.urange data.urange sub 0#usize bw
              let u ← U64ScalarValueData.new new_val unknown_mask unwritten_mask urange
              let self1 ← verifier.Stack.set self idx (.ScalarValue u)
              ok (core.result.Result.Ok (), self1)) = ok (.Ok (), s')
            rw [bind_ok_eq (stackStoreOldScalar_eq _),
                bind_ok_eq (Stack_insert_sub_value_spec old_data.value data.value width hfit),
                bind_ok_eq (Stack_insert_sub_value_spec old_data.unknown_mask data.unknown_mask width hfit),
                bind_ok_eq (Stack_insert_sub_value_spec old_data.unwritten_mask data.unwritten_mask width hfit),
                bind_ok_eq hswap_s, bind_ok_eq husvd, bind_ok_eq hset]
          · intro _ _
            refine ⟨hoff_kn, hoff_lt, hal, ?_, fun hc => absurd hc hw_ne, fun _ => ⟨data, rfl, hunw_z⟩⟩
            intro slot hslot hne
            rw [hs'_slots slot hslot]
            have hne_idx : slot ≠ idx.val := by rwa [hidx_val]
            simp [hne_idx]
      · -- 64-bit store (`width = .U64`)
        have hw_eq : width = .U64 := heq_iff.mp rfl
        subst hw_eq
        simp only [↓reduceIte]
        obtain ⟨s', hset, hs'_valid, hs'_slots⟩ := Stack_set_spec hs hidx_lt64 hv
        rw [bind_ok_eq hset]
        refine ⟨.Ok (), s', rfl, hs'_valid, (fun _ h => by cases h), ?_⟩
        intro _ _
        refine ⟨hoff_kn, hoff_lt, hal, ?_, ?_, fun hc => absurd rfl hc⟩
        · intro slot hslot hne
          rw [hs'_slots slot hslot]
          have hne_idx : slot ≠ idx.val := by rwa [hidx_val]
          simp [hne_idx]
        · intro _
          rw [← hidx_val, hs'_slots idx.val hidx_lt64]
          simp

theorem Stack_store_rel2 {env : ExecEnv} {vCounter : Nat}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64} {layout₁ layout₂ : HostLayout}
    {self s' : verifier.Stack} (hs : StackValid env vCounter self)
    {stk₁ stk₂ : StackMem}
    {offset : verifier.StackOffset} {effOff : BitVec 64}
    {value : verifier.Type} (hv : TypeValid env vCounter value)
    {vSrc₁ vSrc₂ : RuntimeVal} {width : api.DataWidth}
    (hoff_rel : ConcretizesScalar64Rel2 offset effOff effOff)
    (hok : verifier.Stack.store self offset value width = ok (.Ok (), s'))
    (hsrc_rel : TypeRel2 env ρ resActive isNullId arrayLen value vSrc₁ vSrc₂)
    (hslot_old : width ≠ .U64 →
      StackSlotRel2 env ρ resActive isNullId arrayLen
        (stackSlotType self (effOff.toNat / 8))
        (stk₁ (effOff.toNat / 8)) (stk₂ (effOff.toNat / 8))) :
    let w := DataWidth.ofCore width
    let slotIdx := effOff.toNat / 8
    let subIdx := effOff.toNat % 8
    (w ≠ .u64 → ∃ s₁ s₂ : BitVec 64, vSrc₁ = .scalar s₁ ∧ vSrc₂ = .scalar s₂) ∧
    (let newSlot₁ :=
       if w == .u64 then
         match vSrc₁ with
         | .scalar s => StackSlot.bytes s (~~~0#64)
         | cap => StackSlot.spilled cap
       else
         match vSrc₁ with
         | .scalar s =>
             match stk₁ slotIdx with
             | .bytes oldV oldInit =>
                 StackSlot.bytes (insertSubValue oldV s subIdx w) (insertSubValue oldInit (~~~0#64) subIdx w)
             | .spilled cap =>
                 StackSlot.bytes (insertSubValue (cap.toBits layout₁ arrayLen) s subIdx w) (insertSubValue 0#64 (~~~0#64) subIdx w)
         | cap => StackSlot.spilled cap
     let newSlot₂ :=
       if w == .u64 then
         match vSrc₂ with
         | .scalar s => StackSlot.bytes s (~~~0#64)
         | cap => StackSlot.spilled cap
       else
         match vSrc₂ with
         | .scalar s =>
             match stk₂ slotIdx with
             | .bytes oldV oldInit =>
                 StackSlot.bytes (insertSubValue oldV s subIdx w) (insertSubValue oldInit (~~~0#64) subIdx w)
             | .spilled cap =>
                 StackSlot.bytes (insertSubValue (cap.toBits layout₂ arrayLen) s subIdx w) (insertSubValue 0#64 (~~~0#64) subIdx w)
         | cap => StackSlot.spilled cap
     StackSlotRel2 env ρ resActive isNullId arrayLen (stackSlotType s' slotIdx) newSlot₁ newSlot₂) := by
  obtain ⟨r0, s0, hst0, _, _, hpost0⟩ := Stack_store_spec hs offset hv width
  rw [hok] at hst0
  rcases Prod.mk.inj (ok_inj hst0) with ⟨hr0, hs0⟩
  subst r0 s0
  obtain ⟨hoff_kn, hoff_lt, hal, _, h64_slot, hnar_val⟩ := hpost0 () rfl
  have heff_eq : effOff = offset.value.bv := eq_value_of_known_64 hoff_kn hoff_rel.1
  subst heff_eq
  have hoff_le : offset.value.bv ≤ 512#64 := BitVec.le_of_lt hoff_lt
  have hoff_512 : offset.value.bv.toNat < 512 := by rw [BitVec.lt_def] at hoff_lt; simpa using hoff_lt
  by_cases hw64 : width = .U64
  · -- 64-bit store
    subst hw64
    have hslot_new := h64_slot rfl
    refine ⟨fun hc => absurd rfl hc, ?_⟩
    simp only [DataWidth.ofCore, beq_self_eq_true, ↓reduceIte]
    rw [hslot_new]
    cases value with
    | ScalarValue d =>
      by_cases hd_uninit : d = uninitScalar64
      · subst hd_uninit; simp [StackSlotRel2]
      · simp only [TypeRel2, hd_uninit, ↓reduceIte] at hsrc_rel
        rcases hsrc_rel with ⟨s₁, s₂, rfl, rfl, hconc⟩
        simp only [StackSlotRel2, hd_uninit, ↓reduceIte]
        have he1 : ∀ x : BitVec 64, x &&& ~~~0#64 = x := by intro x; bv_decide
        have he2 : ∀ x : BitVec 64, x ||| ~~~(~~~0#64) = x := by intro x; bv_decide
        refine ⟨s₁, ~~~0#64, s₂, ~~~0#64, rfl, rfl, hconc, ?_, ?_⟩
        · rw [he1, he2]; exact ⟨hconc.1, hconc.1, rfl⟩
        · rw [he1, he2]; exact ⟨hconc.2.1, hconc.2.1, rfl⟩
    | NullOr nullId inner =>
      rcases hv with ⟨_, hshape, hinnerV⟩
      unfold TypeRel2 at hsrc_rel
      by_cases hn : isNullId (renameMemId ρ nullId)
      · simp only [hn, ↓reduceIte] at hsrc_rel
        rcases hsrc_rel with ⟨hinact, rfl, rfl⟩
        simp [StackSlotRel2, hn, hinact]
      · simp only [hn, Bool.false_eq_true, ↓reduceIte] at hsrc_rel
        have ⟨heq, hsc⟩ := TypeRel2_ptr_nonscalar_eq hinnerV hsrc_rel
          (by cases inner <;> try exact False.elim hshape
              all_goals trivial)
        subst heq
        simp only [StackSlotRel2, hn, Bool.false_eq_true, ↓reduceIte]
        cases inner <;> try exact False.elim hshape
        all_goals {
          cases vSrc₁ <;> try cases hsc
          all_goals exact ⟨_, rfl, rfl, rfl, hsrc_rel⟩
        }
    | ConstPtrToMap _ _ | PtrToStack _ | PtrToMemory _ _ _ _
    | PtrToStruct _ _ _ | PtrToArray _ _ | PtrToEndArray _ | Releasable _ _ =>
      have ⟨heq, hsc⟩ := TypeRel2_ptr_nonscalar_eq hv hsrc_rel trivial
      subst heq
      cases vSrc₁ <;> try cases hsc
      all_goals exact ⟨_, rfl, rfl, rfl, hsrc_rel⟩
    | _ => exact False.elim hv
  · -- Narrow store (`width ≠ .U64`)
    obtain ⟨data, rfl, hunw_z⟩ := hnar_val hw64
    set w := DataWidth.ofCore width
    have hw_cases : w = .u8 ∨ w = .u16 ∨ w = .u32 := by
      cases width <;> simp [w, DataWidth.ofCore] at hw64 ⊢
    have hdata_ne : data ≠ uninitScalar64 := by
      intro hc; subst hc
      unfold uninitScalar64 extractSubValue DataWidth.valMask64 at hunw_z
      simp only [u64_max_bv] at hunw_z
      rcases hw_cases with hw8 | hw16 | hw32 <;>
        (first | rw [hw8] at hunw_z | rw [hw16] at hunw_z | rw [hw32] at hunw_z) <;>
        (revert hunw_z; decide)
    simp only [TypeRel2, hdata_ne, ↓reduceIte] at hsrc_rel
    rcases hsrc_rel with ⟨s₁, s₂, rfl, rfl, hconc_src⟩
    refine ⟨fun _ => ⟨s₁, s₂, rfl, rfl⟩, ?_⟩
    have hw_beq_f : (w == .u64) = false := by
      rcases hw_cases with h | h | h <;> simp [h]
    dsimp only
    rw [hw_beq_f]
    simp only [Bool.false_eq_true, ↓reduceIte]
    unfold verifier.Stack.store at hok
    obtain ⟨rchk, hrchk, _⟩ := StackOffset_check_valid_offset_spec offset
    rw [bind_ok_eq hrchk] at hok
    cases rchk with
    | Err _ => cases ok_inj hok
    | Ok _ =>
      obtain ⟨sub, hsub, hsub_val, hsub_lt8⟩ := StackOffset_sub_index_spec hoff_le
      obtain ⟨bw, hbw, hbw_val⟩ := DataWidth_bytes_spec width
      have hbw_pos : 0 < bw.val := by
        rw [hbw_val]; cases width <;> simp [DataWidth.ofCore, DataWidth.bytes]
      obtain ⟨rem, hrem, _⟩ := usize_rem_eq_ok sub bw (by omega)
      obtain ⟨idx, hidx, hidx_val⟩ := StackOffset_array_index_spec hoff_le
      have hidx_lt64 : idx.val < 64 := by omega
      obtain ⟨beq, hbeq, heq_iff⟩ := DataWidth_eq_spec width .U64
      have hbeq_f : beq = false := Bool.eq_false_iff.mpr (fun hc => hw64 (heq_iff.mp hc))
      subst hbeq_f
      rw [bind_ok_eq hsub, bind_ok_eq hbw, bind_ok_eq hrem] at hok
      by_cases hmis : (rem != 0#usize) = true
      · simp only [hmis, ↓reduceIte] at hok; cases ok_inj hok
      · simp only [hmis, Bool.false_eq_true, ↓reduceIte] at hok
        rw [bind_ok_eq hidx, bind_ok_eq hbeq] at hok
        simp only [Bool.false_eq_true, ↓reduceIte] at hok
        rw [bind_ok_eq (Stack_extract_sub_value_spec data.unwritten_mask 0#usize bw)] at hok
        by_cases hinv : (⟨pureExtractSubBytes data.unwritten_mask.bv (0#usize).val bw.val⟩ : Std.U64) != 0#u64
        · simp only [hinv, ↓reduceIte] at hok; cases ok_inj hok
        · simp only [hinv, Bool.false_eq_true, ↓reduceIte] at hok
          rw [bind_ok_eq (Stack_get_spec hs hidx_lt64)] at hok
          set old_data : U64ScalarValueData :=
            match stackSlotType self idx.val with
            | .ScalarValue d => d
            | _ => uninitScalar64
          have hfit : sub.val + w.bytes ≤ 8 := by
            apply subword_aligned_fit hsub_lt8 w
            rwa [hsub_val]
          obtain ⟨rOut_s, hswap_s⟩ : ∃ rOut, U64Range.compute_range_for_bytes_swap old_data.urange data.urange sub 0#usize bw = ok rOut :=
            ⟨_, compute_range_for_bytes_swap_64_eq_ok _ _ _ _ _⟩
          obtain ⟨usvd, husvd⟩ : ∃ u, U64ScalarValueData.new
              ⟨insertSubValue old_data.value.bv data.value.bv sub.val w⟩
              ⟨insertSubValue old_data.unknown_mask.bv data.unknown_mask.bv sub.val w⟩
              ⟨insertSubValue old_data.unwritten_mask.bv data.unwritten_mask.bv sub.val w⟩
              rOut_s = ok u := ⟨_, new64_eq_ok _ _ _ _⟩
          have husvd_valid : TypeValid env vCounter (.ScalarValue usvd) := by
            have hnew := new64_eq_ok
              ⟨insertSubValue old_data.value.bv data.value.bv sub.val w⟩
              ⟨insertSubValue old_data.unknown_mask.bv data.unknown_mask.bv sub.val w⟩
              ⟨insertSubValue old_data.unwritten_mask.bv data.unwritten_mask.bv sub.val w⟩
              rOut_s
            rw [husvd] at hnew
            cases ok_inj hnew
            exact pureNew64_valid _ _ _ _ _
          obtain ⟨s1, hset1, _, hs1_slots⟩ := Stack_set_spec hs hidx_lt64 husvd_valid
          have hok_eval : (do
              let od ← stackStoreOldScalar (stackSlotType self idx.val)
              let new_val ← verifier.Stack.insert_sub_value od.value data.value width sub
              let unknown_mask ← verifier.Stack.insert_sub_value od.unknown_mask data.unknown_mask width sub
              let unwritten_mask ← verifier.Stack.insert_sub_value od.unwritten_mask data.unwritten_mask width sub
              let urange ← U64Range.compute_range_for_bytes_swap od.urange data.urange sub 0#usize bw
              let u ← U64ScalarValueData.new new_val unknown_mask unwritten_mask urange
              let self1 ← verifier.Stack.set self idx (.ScalarValue u)
              ok (core.result.Result.Ok (), self1)) = ok (.Ok (), s') := hok
          rw [bind_ok_eq (stackStoreOldScalar_eq _),
              bind_ok_eq (Stack_insert_sub_value_spec old_data.value data.value width hfit),
              bind_ok_eq (Stack_insert_sub_value_spec old_data.unknown_mask data.unknown_mask width hfit),
              bind_ok_eq (Stack_insert_sub_value_spec old_data.unwritten_mask data.unwritten_mask width hfit),
              bind_ok_eq hswap_s, bind_ok_eq husvd, bind_ok_eq hset1] at hok_eval
          have hs'_eq : s1 = s' := (Prod.mk.inj (ok_inj hok_eval)).2
          subst s1
          have hslot_s1 : stackSlotType s' (offset.value.bv.toNat / 8) = .ScalarValue usvd := by
            rw [← hidx_val, hs1_slots idx.val hidx_lt64]; simp
          rw [hslot_s1]
          have hstore_pair : ∀ ox₁ ox₂ sx₁ sx₂ : BitVec 64,
              ConcretizesScalar64Rel2 old_data ox₁ ox₂ →
              ConcretizesScalar64Rel2 data sx₁ sx₂ →
              ConcretizesScalar64Rel2 usvd
                (insertSubValue ox₁ sx₁ sub.val w) (insertSubValue ox₂ sx₂ sub.val w) := by
            intro ox₁ ox₂ sx₁ sx₂ hox hsx
            obtain ⟨r1, hr1, hcr1⟩ := compute_range_for_bytes_swap_store_sound old_data.urange data.urange sub bw w ox₁ sx₁
              hw_cases hbw_val hfit hox.1.2.2 hsx.1.2.2
            obtain ⟨r2, hr2, hcr2⟩ := compute_range_for_bytes_swap_store_sound old_data.urange data.urange sub bw w ox₂ sx₂
              hw_cases hbw_val hfit hox.2.1.2.2 hsx.2.1.2.2
            rw [hswap_s] at hr1 hr2
            have hr1_eq := ok_inj hr1
            have hr2_eq := ok_inj hr2
            rw [← hr1_eq] at hcr1
            rw [← hr2_eq] at hcr2
            obtain ⟨u', hu', hrel_u⟩ := new64_sound_2safety
              ⟨insertSubValue old_data.value.bv data.value.bv sub.val w⟩
              ⟨insertSubValue old_data.unknown_mask.bv data.unknown_mask.bv sub.val w⟩
              ⟨insertSubValue old_data.unwritten_mask.bv data.unwritten_mask.bv sub.val w⟩
              rOut_s (insertSubValue ox₁ sx₁ sub.val w) (insertSubValue ox₂ sx₂ sub.val w)
              hcr1 (by simp only [u64_mk_bv]; exact insertSubValue_tnum hsub_lt8 hox.1.2.1 hsx.1.2.1)
              hcr2 (by simp only [u64_mk_bv]; exact insertSubValue_tnum hsub_lt8 hox.2.1.2.1 hsx.2.1.2.1)
              (by simp only [u64_mk_bv]; exact insertSubValue_rel_mask hsub_lt8 hox.2.2 hsx.2.2)
            rw [husvd] at hu'
            have hu_eq := ok_inj hu'
            rwa [← hu_eq] at hrel_u
          by_cases husvd_uninit : usvd = uninitScalar64
          · simp [StackSlotRel2, husvd_uninit]
          · simp only [StackSlotRel2, husvd_uninit, ↓reduceIte]
            have hold_rel := hslot_old hw64
            let raw₁ : BitVec 64 × BitVec 64 :=
              match stk₁ (offset.value.bv.toNat / 8) with
              | .bytes oldV oldInit => (oldV, oldInit)
              | .spilled cap => (cap.toBits layout₁ arrayLen, 0#64)
            let raw₂ : BitVec 64 × BitVec 64 :=
              match stk₂ (offset.value.bv.toNat / 8) with
              | .bytes oldV oldInit => (oldV, oldInit)
              | .spilled cap => (cap.toBits layout₂ arrayLen, 0#64)
            have hmatch₁ :
                (match stk₁ (offset.value.bv.toNat / 8) with
                 | .bytes oldV oldInit =>
                     StackSlot.bytes (insertSubValue oldV s₁ (offset.value.bv.toNat % 8) w)
                       (insertSubValue oldInit (~~~0#64) (offset.value.bv.toNat % 8) w)
                 | .spilled cap =>
                     StackSlot.bytes (insertSubValue (cap.toBits layout₁ arrayLen) s₁ (offset.value.bv.toNat % 8) w)
                       (insertSubValue 0#64 (~~~0#64) (offset.value.bv.toNat % 8) w)) =
                  .bytes (insertSubValue raw₁.1 s₁ sub.val w) (insertSubValue raw₁.2 (~~~0#64) sub.val w) := by
              dsimp only [raw₁]; rw [hsub_val]; cases stk₁ (offset.value.bv.toNat / 8) <;> rfl
            have hmatch₂ :
                (match stk₂ (offset.value.bv.toNat / 8) with
                 | .bytes oldV oldInit =>
                     StackSlot.bytes (insertSubValue oldV s₂ (offset.value.bv.toNat % 8) w)
                       (insertSubValue oldInit (~~~0#64) (offset.value.bv.toNat % 8) w)
                 | .spilled cap =>
                     StackSlot.bytes (insertSubValue (cap.toBits layout₂ arrayLen) s₂ (offset.value.bv.toNat % 8) w)
                       (insertSubValue 0#64 (~~~0#64) (offset.value.bv.toNat % 8) w)) =
                  .bytes (insertSubValue raw₂.1 s₂ sub.val w) (insertSubValue raw₂.2 (~~~0#64) sub.val w) := by
              dsimp only [raw₂]; rw [hsub_val]; cases stk₂ (offset.value.bv.toNat / 8) <;> rfl
            -- Extract `(rawV₁, rawInit₁, rawV₂, rawInit₂)` matching concrete `stk₁` and `stk₂` and `old_data`
            have hex_old : ∃ rawV₁ rawInit₁ rawV₂ rawInit₂ : BitVec 64,
                (match stk₁ (offset.value.bv.toNat / 8) with
                 | .bytes oldV oldInit =>
                     StackSlot.bytes (insertSubValue oldV s₁ (offset.value.bv.toNat % 8) w)
                       (insertSubValue oldInit (~~~0#64) (offset.value.bv.toNat % 8) w)
                 | .spilled cap =>
                     StackSlot.bytes (insertSubValue (cap.toBits layout₁ arrayLen) s₁ (offset.value.bv.toNat % 8) w)
                       (insertSubValue 0#64 (~~~0#64) (offset.value.bv.toNat % 8) w)) =
                  .bytes (insertSubValue rawV₁ s₁ sub.val w) (insertSubValue rawInit₁ (~~~0#64) sub.val w) ∧
                (match stk₂ (offset.value.bv.toNat / 8) with
                 | .bytes oldV oldInit =>
                     StackSlot.bytes (insertSubValue oldV s₂ (offset.value.bv.toNat % 8) w)
                       (insertSubValue oldInit (~~~0#64) (offset.value.bv.toNat % 8) w)
                 | .spilled cap =>
                     StackSlot.bytes (insertSubValue (cap.toBits layout₂ arrayLen) s₂ (offset.value.bv.toNat % 8) w)
                       (insertSubValue 0#64 (~~~0#64) (offset.value.bv.toNat % 8) w)) =
                  .bytes (insertSubValue rawV₂ s₂ sub.val w) (insertSubValue rawInit₂ (~~~0#64) sub.val w) ∧
                ConcretizesScalar64Rel2 old_data rawV₁ rawV₂ ∧
                ConcretizesScalar64Rel2 old_data (rawV₁ &&& rawInit₁) (rawV₁ ||| ~~~rawInit₁) ∧
                ConcretizesScalar64Rel2 old_data (rawV₂ &&& rawInit₂) (rawV₂ ||| ~~~rawInit₂) := by
              have hslot_idx_eq : stackSlotType self (offset.value.bv.toNat / 8) = stackSlotType self idx.val := by
                rw [hidx_val]
              rw [hslot_idx_eq] at hold_rel
              cases hst : stackSlotType self idx.val
              case ScalarValue d =>
                have hod : old_data = d := by simp [old_data, hst]
                rw [hod]
                rw [hst] at hold_rel
                by_cases hd_u : d = uninitScalar64
                · subst hd_u
                  exact ⟨raw₁.1, raw₁.2, raw₂.1, raw₂.2, hmatch₁, hmatch₂,
                    concretizesScalar64Rel2_uninit _ _,
                    concretizesScalar64Rel2_uninit _ _,
                    concretizesScalar64Rel2_uninit _ _⟩
                · simp only [StackSlotRel2, hd_u, ↓reduceIte] at hold_rel
                  rcases hold_rel with ⟨v1, i1, v2, i2, hs1, hs2, hc12, hce1, hce2⟩
                  rw [hs1, hs2]
                  exact ⟨v1, i1, v2, i2, by simp [hsub_val], by simp [hsub_val], hc12, hce1, hce2⟩
              all_goals {
                have hod : old_data = uninitScalar64 := by simp [old_data, hst]
                rw [hod]
                exact ⟨raw₁.1, raw₁.2, raw₂.1, raw₂.2, hmatch₁, hmatch₂,
                  concretizesScalar64Rel2_uninit _ _,
                  concretizesScalar64Rel2_uninit _ _,
                  concretizesScalar64Rel2_uninit _ _⟩
              }
            rcases hex_old with ⟨rv1, ri1, rv2, ri2, hnew1, hnew2, ho12, hoe1, hoe2⟩
            refine ⟨insertSubValue rv1 s₁ sub.val w, insertSubValue ri1 (~~~0#64) sub.val w,
                    insertSubValue rv2 s₂ sub.val w, insertSubValue ri2 (~~~0#64) sub.val w,
                    hnew1, hnew2, hstore_pair rv1 rv2 s₁ s₂ ho12 hconc_src, ?_, ?_⟩
            · rw [insertSubValue_extreme_lo rv1 s₁ ri1 hsub_lt8,
                  insertSubValue_extreme_hi rv1 s₁ ri1 hsub_lt8]
              exact hstore_pair (rv1 &&& ri1) (rv1 ||| ~~~ri1) s₁ s₁ hoe1 ⟨hconc_src.1, hconc_src.1, rfl⟩
            · rw [insertSubValue_extreme_lo rv2 s₂ ri2 hsub_lt8,
                  insertSubValue_extreme_hi rv2 s₂ ri2 hsub_lt8]
              exact hstore_pair (rv2 &&& ri2) (rv2 ||| ~~~ri2) s₂ s₂ hoe2 ⟨hconc_src.2.1, hconc_src.2.1, rfl⟩

private theorem Stack_write_data_ptr_loop_total {env : ExecEnv} {vCounter : Nat}
    (bytes : Std.U64) :
    ∀ (n : Nat) (self : verifier.Stack) (offset : verifier.StackOffset) (i : Std.U64),
      bytes.val - i.val = n →
      StackValid env vCounter self →
      ∃ (r : core.result.Result Unit error.VerifierErrorKind) (s' : verifier.Stack),
        verifier.Stack.write_data_ptr_loop self offset bytes i = ok (r, s') ∧
        StackValid env vCounter s' := by
  intro n
  induction n with
  | zero =>
    intro self offset i hrem hs
    have hnlt : ¬ (i < bytes) := by change ¬ (i.val < bytes.val); omega
    unfold verifier.Stack.write_data_ptr_loop
    rw [loop_unfold_step]
    unfold verifier.Stack.write_data_ptr_loop.body
    simp only [hnlt, ↓reduceIte, bind_ok]
    exact ⟨.Ok (), self, rfl, hs⟩
  | succ m ih =>
    intro self offset i hrem hs
    have hlt : i < bytes := by change i.val < bytes.val; omega
    unfold verifier.Stack.write_data_ptr_loop
    rw [loop_unfold_step]
    unfold verifier.Stack.write_data_ptr_loop.body
    simp only [hlt, ↓reduceIte, Type_UNKNOWN_SCALAR_eq, bind_ok]
    obtain ⟨r, self1, hst, hs1, _, _⟩ :=
      Stack_store_spec (value := .ScalarValue unknownWrittenScalar64)
        hs offset unknownWrittenScalar64_valid .U8
    rw [bind_pair_ok_eq hst]
    cases r with
    | Err e =>
      simp only [bind_ok]
      exact ⟨.Err e, self1, rfl, hs1⟩
    | Ok _ =>
      obtain ⟨off1, hoff1, _⟩ := StackOffset_add_u64_ok_valid offset 1#u64
      have hi_le : i.val + (1#u64).val ≤ UScalar.max .U64 := by
        have hb_max := bytes.hmax
        have h1 : (1#u64).val = 1 := rfl
        simp [U64.max, U64.numBits] at hb_max ⊢
        omega
      have hi1 := uscalar_add_eq_ok i 1#u64 hi_le
      have hi1_val : (⟨i.bv + 1#64⟩ : Std.U64).val = i.val + 1 := by
        simpa [u64_one_uscalar_bv] using (uscalar_add_ok_val hi1).1
      simp only [u64_one_uscalar_bv] at hi1
      rw [bind_ok_eq hoff1, bind_ok_eq hi1]
      simp only [bind_ok]
      obtain ⟨r', s', hout, hs'⟩ := ih self1 off1 ⟨i.bv + 1#64⟩ (by omega) hs1
      unfold verifier.Stack.write_data_ptr_loop verifier.Stack.write_data_ptr_loop.body at hout
      simp only [Type_UNKNOWN_SCALAR_eq, bind_ok] at hout
      exact ⟨r', s', hout, hs'⟩

theorem Stack_write_data_ptr_total {env : ExecEnv} {vCounter : Nat}
    {self : verifier.Stack} (hs : StackValid env vCounter self)
    (offset : verifier.StackOffset) (bytes : Std.U64) :
    ∃ (r : core.result.Result Unit error.VerifierErrorKind) (s' : verifier.Stack),
      verifier.Stack.write_data_ptr self offset bytes = ok (r, s') ∧
      StackValid env vCounter s' := by
  unfold verifier.Stack.write_data_ptr
  exact Stack_write_data_ptr_loop_total bytes bytes.val self offset 0#u64 (by simp) hs

private theorem insertSubValue_ff_eq_all_ones (init0 : BitVec 64) {r : Nat} (hr : r < 8) :
    insertSubValue init0 0xFF#64 r .u8 = insertSubValue init0 (~~~0#64) r .u8 := by
  unfold insertSubValue DataWidth.valMask64
  have hc : r = 0 ∨ r = 1 ∨ r = 2 ∨ r = 3 ∨ r = 4 ∨ r = 5 ∨ r = 6 ∨ r = 7 := by omega
  rcases hc with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;> bv_decide

private theorem Stack_write_data_ptr_loop_rel2 {env : ExecEnv} {vCounter : Nat}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64} {layout₁ layout₂ : HostLayout}
    {deps : verifier.DataDependencies}
    (writeByte : BitVec 64 → BitVec 8)
    {startOff bytes : BitVec 64}
    (hsum512 : startOff.toNat + bytes.toNat ≤ 512) :
    ∀ (rem : Nat) (k : Nat) (self : verifier.Stack) (stk₁ stk₂ : StackMem),
      bytes.toNat - k = rem →
      k ≤ bytes.toNat →
      StackValid env vCounter self →
      (∀ slot < 64, DataDepsHasStack deps slot = true →
        StackSlotRel2 env ρ resActive isNullId arrayLen (stackSlotType self slot) (stk₁ slot) (stk₂ slot)) →
      ∃ s' : verifier.Stack,
        verifier.Stack.write_data_ptr_loop self
          (knownScalar64 ⟨BitVec.ofNat 64 (startOff.toNat + k)⟩)
          ⟨bytes⟩ ⟨BitVec.ofNat 64 k⟩ = ok (.Ok (), s') ∧
        StackValid env vCounter s' ∧
        (let stepFn (ly : HostLayout) (curStk : StackMem) (idx : Nat) : StackMem :=
           let pos := startOff.toNat + idx
           let slotIdx := pos / 8
           let subIdx := pos % 8
           let bVal := (writeByte (BitVec.ofNat 64 idx)).zeroExtend 64
           let newSlot := match curStk slotIdx with
             | .bytes oldV oldInit =>
                 StackSlot.bytes (insertSubValue oldV bVal subIdx .u8) (insertSubValue oldInit 0xFF#64 subIdx .u8)
             | .spilled cap =>
                 StackSlot.bytes (insertSubValue (cap.toBits ly arrayLen) bVal subIdx .u8) (insertSubValue 0#64 0xFF#64 subIdx .u8)
           updateStack curStk slotIdx newSlot
         let stk₁' := (List.range' k rem).foldl (stepFn layout₁) stk₁
         let stk₂' := (List.range' k rem).foldl (stepFn layout₂) stk₂
         ∀ slot < 64, DataDepsHasStack deps slot = true →
           StackSlotRel2 env ρ resActive isNullId arrayLen (stackSlotType s' slot) (stk₁' slot) (stk₂' slot)) := by
  intro rem
  induction rem with
  | zero =>
    intro k self stk₁ stk₂ hrem hkle hs hrel
    have hk_eq : k = bytes.toNat := by omega
    subst hk_eq
    have hnlt : ¬ ((⟨BitVec.ofNat 64 bytes.toNat⟩ : Std.U64) < ⟨bytes⟩) := by
      change ¬ ((BitVec.ofNat 64 bytes.toNat).toNat < bytes.toNat)
      rw [BitVec.toNat_ofNat, Nat.mod_eq_of_lt (by omega)]; omega
    unfold verifier.Stack.write_data_ptr_loop
    rw [loop_unfold_step]
    unfold verifier.Stack.write_data_ptr_loop.body
    simp only [hnlt, ↓reduceIte, bind_ok]
    exact ⟨self, rfl, hs, hrel⟩
  | succ m ih =>
    intro k self stk₁ stk₂ hrem hkle hs hrel
    have hk_lt : k < bytes.toNat := by omega
    have hk_2_64 : k < 2 ^ 64 := by omega
    have hpos_512 : startOff.toNat + k < 512 := by omega
    have hlt_u64 : (⟨BitVec.ofNat 64 k⟩ : Std.U64) < ⟨bytes⟩ := by
      change (BitVec.ofNat 64 k).toNat < bytes.toNat
      rw [BitVec.toNat_ofNat, Nat.mod_eq_of_lt hk_2_64]; exact hk_lt
    set offk := knownScalar64 ⟨BitVec.ofNat 64 (startOff.toNat + k)⟩
    have hoffk_kn : offk.unknown_mask = 0#u64 := rfl
    have hoffk_val_toNat : offk.value.bv.toNat = startOff.toNat + k := by
      change (BitVec.ofNat 64 (startOff.toNat + k)).toNat = startOff.toNat + k
      rw [BitVec.toNat_ofNat, Nat.mod_eq_of_lt (by omega)]
    have hoffk_lt512 : offk.value.bv < 512#64 := by
      rw [BitVec.lt_def, hoffk_val_toNat]; simpa using hpos_512
    obtain ⟨rst, self1, hst, hs1, _, hpost_st⟩ :=
      Stack_store_spec (value := .ScalarValue unknownWrittenScalar64)
        hs offk unknownWrittenScalar64_valid .U8
    have hrst_ok : rst = .Ok () := by
      have hst_copy := hst
      unfold verifier.Stack.store at hst_copy
      obtain ⟨rchk, hrchk, hchk_iff⟩ := StackOffset_check_valid_offset_spec offk
      rw [bind_ok_eq hrchk] at hst_copy
      have hrchk_ok : rchk = .Ok () := by
        cases rchk with
        | Ok u => cases u; rfl
        | Err e => exfalso; have := (hchk_iff ()).mpr ⟨hoffk_kn, hoffk_lt512⟩; cases this
      subst hrchk_ok
      have hoffk_le : offk.value.bv ≤ 512#64 := BitVec.le_of_lt hoffk_lt512
      obtain ⟨sub, hsub, _, hsub_lt8⟩ := StackOffset_sub_index_spec hoffk_le
      obtain ⟨bw, hbw, hbw_val⟩ := DataWidth_bytes_spec .U8
      have hbw1 : bw.val = 1 := by simpa [DataWidth.ofCore, DataWidth.bytes] using hbw_val
      obtain ⟨rem0, hrem0, hrem0_val⟩ := usize_rem_eq_ok sub bw (by omega)
      have hrem0_eq : (rem0 != 0#usize) = false := by
        rw [bne_eq_false_iff_eq]
        apply usize_eq_of_val_eq
        rw [hrem0_val, hbw1, Nat.mod_one, usize_const_val_0]
      obtain ⟨idx, hidx, _⟩ := StackOffset_array_index_spec hoffk_le
      have hidx_lt64 : idx.val < 64 := by omega
      obtain ⟨beq_w, hbeq_f, heq_w⟩ := DataWidth_eq_spec .U8 .U64
      have hbeq_wf : beq_w = false := Bool.eq_false_iff.mpr (fun hc => by cases heq_w.mp hc)
      subst beq_w
      rw [bind_ok_eq hsub, bind_ok_eq hbw, bind_ok_eq hrem0] at hst_copy
      simp only [hrem0_eq, Bool.false_eq_true, ↓reduceIte] at hst_copy
      rw [bind_ok_eq hidx, bind_ok_eq hbeq_f] at hst_copy
      simp only [Bool.false_eq_true, ↓reduceIte] at hst_copy
      rw [bind_ok_eq (Stack_extract_sub_value_spec unknownWrittenScalar64.unwritten_mask 0#usize bw)] at hst_copy
      have hunw0_f : ((⟨pureExtractSubBytes unknownWrittenScalar64.unwritten_mask.bv (0#usize).val bw.val⟩ : Std.U64) != 0#u64) = false := by
        rw [bne_eq_false_iff_eq]
        apply (u64_eq_iff_bv _ _).mpr
        have : unknownWrittenScalar64.unwritten_mask.bv = 0#64 := rfl
        rw [this, hbw1, usize_const_val_0]
        decide
      simp only [hunw0_f, Bool.false_eq_true, ↓reduceIte] at hst_copy
      rw [bind_ok_eq (Stack_get_spec hs hidx_lt64)] at hst_copy
      have hfit8 : sub.val + (DataWidth.ofCore .U8).bytes ≤ 8 := by
        simp [DataWidth.ofCore, DataWidth.bytes]; omega
      set od := match stackSlotType self idx.val with | .ScalarValue d => d | _ => uninitScalar64
      obtain ⟨rOut_s, hswap_s⟩ : ∃ rOut, U64Range.compute_range_for_bytes_swap od.urange unknownWrittenScalar64.urange sub 0#usize bw = ok rOut :=
        ⟨_, compute_range_for_bytes_swap_64_eq_ok _ _ _ _ _⟩
      obtain ⟨usvd, husvd⟩ : ∃ u, U64ScalarValueData.new
          ⟨insertSubValue od.value.bv unknownWrittenScalar64.value.bv sub.val .u8⟩
          ⟨insertSubValue od.unknown_mask.bv unknownWrittenScalar64.unknown_mask.bv sub.val .u8⟩
          ⟨insertSubValue od.unwritten_mask.bv unknownWrittenScalar64.unwritten_mask.bv sub.val .u8⟩
          rOut_s = ok u := ⟨_, new64_eq_ok _ _ _ _⟩
      have husvd_v : TypeValid env vCounter (.ScalarValue usvd) := by
        have hnew := new64_eq_ok
          ⟨insertSubValue od.value.bv unknownWrittenScalar64.value.bv sub.val .u8⟩
          ⟨insertSubValue od.unknown_mask.bv unknownWrittenScalar64.unknown_mask.bv sub.val .u8⟩
          ⟨insertSubValue od.unwritten_mask.bv unknownWrittenScalar64.unwritten_mask.bv sub.val .u8⟩
          rOut_s
        rw [husvd] at hnew; cases ok_inj hnew; exact pureNew64_valid _ _ _ _ _
      obtain ⟨s_tmp, hset_tmp, _, _⟩ := Stack_set_spec hs hidx_lt64 husvd_v
      have hst_eval : (do
          let old_d ← stackStoreOldScalar (stackSlotType self idx.val)
          let new_val ← verifier.Stack.insert_sub_value old_d.value unknownWrittenScalar64.value .U8 sub
          let unknown_mask ← verifier.Stack.insert_sub_value old_d.unknown_mask unknownWrittenScalar64.unknown_mask .U8 sub
          let unwritten_mask ← verifier.Stack.insert_sub_value old_d.unwritten_mask unknownWrittenScalar64.unwritten_mask .U8 sub
          let urange ← U64Range.compute_range_for_bytes_swap old_d.urange unknownWrittenScalar64.urange sub 0#usize bw
          let u ← U64ScalarValueData.new new_val unknown_mask unwritten_mask urange
          let s1 ← verifier.Stack.set self idx (.ScalarValue u)
          ok (core.result.Result.Ok (), s1)) = ok (rst, self1) := hst_copy
      rw [bind_ok_eq (stackStoreOldScalar_eq _),
          bind_ok_eq (Stack_insert_sub_value_spec od.value unknownWrittenScalar64.value .U8 hfit8),
          bind_ok_eq (Stack_insert_sub_value_spec od.unknown_mask unknownWrittenScalar64.unknown_mask .U8 hfit8),
          bind_ok_eq (Stack_insert_sub_value_spec od.unwritten_mask unknownWrittenScalar64.unwritten_mask .U8 hfit8),
          bind_ok_eq hswap_s] at hst_eval
      dsimp only [DataWidth.ofCore] at hst_eval
      rw [bind_ok_eq husvd, bind_ok_eq hset_tmp] at hst_eval
      exact (Prod.mk.inj (ok_inj hst_eval)).1.symm
    subst hrst_ok
    obtain ⟨_, _, _, hframe1, _, _⟩ := hpost_st () rfl
    have hoff1 : verifier.StackOffset.add_u64 offk 1#u64 =
        ok (knownScalar64 ⟨BitVec.ofNat 64 (startOff.toNat + (k + 1))⟩) := by
      unfold verifier.StackOffset.add_u64 U64ScalarValueData.add_u64
        U64ScalarValueData.add_scalar U64ScalarValueData.is_known lift
      rw [bind_ok_eq (from_known_64_eq 1#u64)]
      have hkn1 : (knownScalar64 1#u64).unknown_mask = 0#u64 := rfl
      simp only [hoffk_kn, hkn1, decide_true, ↓reduceIte, bind_ok, u64_wrapping_add_eq, from_known_64_eq]
      congr 2
      apply (u64_eq_iff_bv _ _).mpr
      change BitVec.ofNat 64 (startOff.toNat + k) + 1#64 = BitVec.ofNat 64 (startOff.toNat + (k + 1))
      apply BitVec.eq_of_toNat_eq
      simp only [BitVec.toNat_add, BitVec.toNat_ofNat]
      omega
    have hk1_add : ((⟨BitVec.ofNat 64 k⟩ : Std.U64) + 1#u64 : Result Std.U64) =
        ok ⟨BitVec.ofNat 64 (k + 1)⟩ := by
      have hle_k : (⟨BitVec.ofNat 64 k⟩ : Std.U64).val + (1#u64).val ≤ UScalar.max .U64 := by
        have hval_k : (⟨BitVec.ofNat 64 k⟩ : Std.U64).val = k := by
          simp [← UScalar.bv_toNat]
          exact hk_2_64
        have hval_1 : (1#u64).val = 1 := rfl
        have hmax_u64 : UScalar.max .U64 = 18446744073709551615 := by simp [U64.max, U64.numBits]
        omega
      rw [uscalar_add_eq_ok _ _ hle_k]
      congr 1
      apply (u64_eq_iff_bv _ _).mpr
      change BitVec.ofNat 64 k + 1#64 = BitVec.ofNat 64 (k + 1)
      apply BitVec.eq_of_toNat_eq
      simp only [BitVec.toNat_add, BitVec.toNat_ofNat]
      omega
    unfold verifier.Stack.write_data_ptr_loop
    rw [loop_unfold_step]
    unfold verifier.Stack.write_data_ptr_loop.body
    simp only [hlt_u64, ↓reduceIte, Type_UNKNOWN_SCALAR_eq, bind_ok]
    rw [bind_pair_ok_eq hst, bind_ok_eq hoff1, bind_ok_eq hk1_add]
    simp only [bind_ok]
    set pos := startOff.toNat + k
    set bVal := (writeByte (BitVec.ofNat 64 k)).zeroExtend 64
    set newSlot₁ := match stk₁ (pos / 8) with
      | .bytes oldV oldInit =>
          StackSlot.bytes (insertSubValue oldV bVal (pos % 8) .u8) (insertSubValue oldInit 0xFF#64 (pos % 8) .u8)
      | .spilled cap =>
          StackSlot.bytes (insertSubValue (cap.toBits layout₁ arrayLen) bVal (pos % 8) .u8) (insertSubValue 0#64 0xFF#64 (pos % 8) .u8)
    set newSlot₂ := match stk₂ (pos / 8) with
      | .bytes oldV oldInit =>
          StackSlot.bytes (insertSubValue oldV bVal (pos % 8) .u8) (insertSubValue oldInit 0xFF#64 (pos % 8) .u8)
      | .spilled cap =>
          StackSlot.bytes (insertSubValue (cap.toBits layout₂ arrayLen) bVal (pos % 8) .u8) (insertSubValue 0#64 0xFF#64 (pos % 8) .u8)
    have hins_ff : ∀ init0 : BitVec 64, insertSubValue init0 0xFF#64 (pos % 8) .u8 =
        insertSubValue init0 (~~~0#64) (pos % 8) .u8 :=
      fun init0 => insertSubValue_ff_eq_all_ones init0 (by omega)
    have hrel1 : ∀ slot < 64, DataDepsHasStack deps slot = true →
        StackSlotRel2 env ρ resActive isNullId arrayLen (stackSlotType self1 slot)
          (updateStack stk₁ (pos / 8) newSlot₁ slot)
          (updateStack stk₂ (pos / 8) newSlot₂ slot) := by
      intro slot hslot hdep
      by_cases hsp : slot = pos / 8
      · subst hsp
        have hupd1 : updateStack stk₁ (pos / 8) newSlot₁ (pos / 8) = newSlot₁ := by
          simp [updateStack]
        have hupd2 : updateStack stk₂ (pos / 8) newSlot₂ (pos / 8) = newSlot₂ := by
          simp [updateStack]
        rw [hupd1, hupd2]
        have hsrc_u : TypeRel2 env ρ resActive isNullId arrayLen
            (.ScalarValue unknownWrittenScalar64) (.scalar bVal) (.scalar bVal) := by
          simp only [TypeRel2, unknownWrittenScalar64_ne_uninit, ↓reduceIte]
          exact ⟨bVal, bVal, rfl, rfl, concretizesScalar64Rel2_unknownWritten bVal⟩
        have hoffk_rel : ConcretizesScalar64Rel2 offk offk.value.bv offk.value.bv :=
          concretizesScalar64Rel2_known _
        have hrel_step := (Stack_store_rel2 (layout₁ := layout₁) (layout₂ := layout₂)
          (value := .ScalarValue unknownWrittenScalar64) (vSrc₁ := .scalar bVal) (vSrc₂ := .scalar bVal)
          hs unknownWrittenScalar64_valid hoffk_rel hst hsrc_u
          (fun _ => by rw [hoffk_val_toNat]; exact hrel (pos / 8) hslot hdep)).2
        change StackSlotRel2 env ρ resActive isNullId arrayLen
          (stackSlotType self1 (offk.value.bv.toNat / 8))
          (match stk₁ (offk.value.bv.toNat / 8) with
           | .bytes oldV oldInit =>
               StackSlot.bytes (insertSubValue oldV bVal (offk.value.bv.toNat % 8) .u8)
                 (insertSubValue oldInit (~~~0#64) (offk.value.bv.toNat % 8) .u8)
           | .spilled cap =>
               StackSlot.bytes (insertSubValue (cap.toBits layout₁ arrayLen) bVal (offk.value.bv.toNat % 8) .u8)
                 (insertSubValue 0#64 (~~~0#64) (offk.value.bv.toNat % 8) .u8))
          (match stk₂ (offk.value.bv.toNat / 8) with
           | .bytes oldV oldInit =>
               StackSlot.bytes (insertSubValue oldV bVal (offk.value.bv.toNat % 8) .u8)
                 (insertSubValue oldInit (~~~0#64) (offk.value.bv.toNat % 8) .u8)
           | .spilled cap =>
               StackSlot.bytes (insertSubValue (cap.toBits layout₂ arrayLen) bVal (offk.value.bv.toNat % 8) .u8)
                 (insertSubValue 0#64 (~~~0#64) (offk.value.bv.toNat % 8) .u8)) at hrel_step
        rw [hoffk_val_toNat] at hrel_step
        have hns1_eq : newSlot₁ = (match stk₁ (pos / 8) with
            | .bytes oldV oldInit =>
                StackSlot.bytes (insertSubValue oldV bVal (pos % 8) .u8) (insertSubValue oldInit (~~~0#64) (pos % 8) .u8)
            | .spilled cap =>
                StackSlot.bytes (insertSubValue (cap.toBits layout₁ arrayLen) bVal (pos % 8) .u8) (insertSubValue 0#64 (~~~0#64) (pos % 8) .u8)) := by
          dsimp only [newSlot₁]; cases stk₁ (pos / 8) <;> simp [hins_ff]
        have hns2_eq : newSlot₂ = (match stk₂ (pos / 8) with
            | .bytes oldV oldInit =>
                StackSlot.bytes (insertSubValue oldV bVal (pos % 8) .u8) (insertSubValue oldInit (~~~0#64) (pos % 8) .u8)
            | .spilled cap =>
                StackSlot.bytes (insertSubValue (cap.toBits layout₂ arrayLen) bVal (pos % 8) .u8) (insertSubValue 0#64 (~~~0#64) (pos % 8) .u8)) := by
          dsimp only [newSlot₂]; cases stk₂ (pos / 8) <;> simp [hins_ff]
        rw [hns1_eq, hns2_eq]
        exact hrel_step
      · have hupd1 : updateStack stk₁ (pos / 8) newSlot₁ slot = stk₁ slot := by
          simp [updateStack, hsp]
        have hupd2 : updateStack stk₂ (pos / 8) newSlot₂ slot = stk₂ slot := by
          simp [updateStack, hsp]
        rw [hupd1, hupd2, hframe1 slot hslot (by rwa [hoffk_val_toNat])]
        exact hrel slot hslot hdep
    obtain ⟨s', hout, hs', hrel_out⟩ :=
      ih (k + 1) self1 (updateStack stk₁ (pos / 8) newSlot₁) (updateStack stk₂ (pos / 8) newSlot₂)
        (by omega) (by omega) hs1 hrel1
    unfold verifier.Stack.write_data_ptr_loop verifier.Stack.write_data_ptr_loop.body at hout
    simp only [Type_UNKNOWN_SCALAR_eq, bind_ok] at hout
    refine ⟨s', hout, hs', ?_⟩
    simpa [List.range'_succ] using hrel_out

theorem Stack_write_data_ptr_rel2 {env : ExecEnv} {vCounter : Nat}
    {ρ : Std.U64 → Std.U64} {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64} {layout₁ layout₂ : HostLayout}
    {deps : verifier.DataDependencies}
    (writeByte : BitVec 64 → BitVec 8)
    {self : verifier.Stack} (hs : StackValid env vCounter self)
    {stk₁ stk₂ : StackMem}
    {offset : verifier.StackOffset} {startOff bytes : BitVec 64}
    (hoff_rel : ConcretizesScalar64Rel2 offset startOff startOff)
    (hoff_kn : offset.unknown_mask = 0#u64)
    (hoff_lt : offset.value.bv < 512#64)
    (hble : bytes ≤ 512#64)
    (hno_ovf : ¬ BitVec.uaddOverflow offset.value.bv bytes)
    (hwithin : offset.value.bv + bytes ≤ 512#64)
    (hrel : ∀ slot < 64, DataDepsHasStack deps slot = true →
      StackSlotRel2 env ρ resActive isNullId arrayLen (stackSlotType self slot) (stk₁ slot) (stk₂ slot)) :
    ∃ (s' : verifier.Stack) (stk₁' stk₂' : StackMem),
      verifier.Stack.write_data_ptr self offset ⟨bytes⟩ = ok (.Ok (), s') ∧
      writeStackRangeHelper layout₁ arrayLen stk₁ startOff bytes writeByte = some stk₁' ∧
      writeStackRangeHelper layout₂ arrayLen stk₂ startOff bytes writeByte = some stk₂' ∧
      StackValid env vCounter s' ∧
      (∀ slot < 64, DataDepsHasStack deps slot = true →
        StackSlotRel2 env ρ resActive isNullId arrayLen (stackSlotType s' slot) (stk₁' slot) (stk₂' slot)) := by
  have hstart_eq : startOff = offset.value.bv := eq_value_of_known_64 hoff_kn hoff_rel.1
  subst hstart_eq
  have h1_512 : offset.value.bv.toNat < 512 := by rw [BitVec.lt_def] at hoff_lt; simpa using hoff_lt
  have h2_512 : bytes.toNat ≤ 512 := by rw [BitVec.le_def] at hble; simpa using hble
  have hsum512 : offset.value.bv.toNat + bytes.toNat ≤ 512 := by
    have hw : (offset.value.bv + bytes).toNat ≤ 512 := by
      rw [BitVec.le_def] at hwithin; simpa using hwithin
    rw [BitVec.toNat_add, Nat.mod_eq_of_lt (by omega)] at hw
    exact hw
  have hoff_eq_known : offset = knownScalar64 offset.value := by
    have hvalid_off : Scalar64Consistent offset := hoff_rel.1.1
    rcases hvalid_off with ⟨hv_um, hunw_um, hmin_le, hle_max, hmax_le⟩
    have hum0 : offset.unknown_mask.bv = 0#64 := by
      rw [(u64_eq_iff_bv _ _).mp hoff_kn, u64_zero_bv]
    have hunw0 : offset.unwritten_mask = 0#u64 := by
      apply (u64_eq_iff_bv _ _).mpr
      rw [u64_zero_bv]
      rw [hum0] at hunw_um; revert hunw_um; bv_decide
    have hmin_eq : offset.urange.min = offset.value := by
      apply (u64_eq_iff_bv _ _).mpr
      rw [hum0] at hv_um hmax_le
      revert hv_um hmin_le hle_max hmax_le; bv_decide
    have hmax_eq : offset.urange.max = offset.value := by
      apply (u64_eq_iff_bv _ _).mpr
      rw [hum0] at hv_um hmax_le
      revert hv_um hmin_le hle_max hmax_le; bv_decide
    cases offset with | mk v um unw ur g =>
    cases ur with | mk rmin rmax =>
    dsimp only at hoff_kn hunw0 hmin_eq hmax_eq
    subst hoff_kn hunw0 hmin_eq hmax_eq
    rfl
  have hbv_off0 : (⟨BitVec.ofNat 64 (offset.value.bv.toNat + 0)⟩ : Std.U64) = offset.value := by
    apply (u64_eq_iff_bv _ _).mpr
    simp
  have hbv_k0 : (⟨BitVec.ofNat 64 0⟩ : Std.U64) = 0#u64 := rfl
  obtain ⟨s', hloop, hs', hrel_out⟩ :=
    Stack_write_data_ptr_loop_rel2 (deps := deps) (layout₁ := layout₁) (layout₂ := layout₂)
      writeByte hsum512 bytes.toNat 0 self stk₁ stk₂ (by omega) (Nat.zero_le _) hs hrel
  rw [hbv_off0, ← hoff_eq_known, hbv_k0] at hloop
  have hn_gt : ¬ (bytes > 512#64) := by
    intro hc; change 512#64 < bytes at hc
    have h1 : 512 < bytes.toNat := by rw [BitVec.lt_def] at hc; simpa using hc
    omega
  have hn_ge : ¬ (offset.value.bv ≥ 512#64) := by
    intro hc; change 512#64 ≤ offset.value.bv at hc
    have h1 : 512 ≤ offset.value.bv.toNat := by rw [BitVec.le_def] at hc; simpa using hc
    omega
  have hn_ovf : BitVec.uaddOverflow offset.value.bv bytes = false := Bool.eq_false_iff.mpr hno_ovf
  have hn_sum_gt : ¬ ((offset.value.bv + bytes) > 512#64) := by
    intro hc; change 512#64 < offset.value.bv + bytes at hc
    have h1 : 512 < (offset.value.bv + bytes).toNat := by rw [BitVec.lt_def] at hc; simpa using hc
    rw [BitVec.toNat_add, Nat.mod_eq_of_lt (by omega)] at h1; omega
  have hrange_eq : List.range bytes.toNat = List.range' 0 bytes.toNat := List.range_eq_range'
  let stepFn := fun (ly : HostLayout) (curStk : StackMem) (idx : Nat) =>
    let pos := offset.value.bv.toNat + idx
    let slotIdx := pos / 8
    let subIdx := pos % 8
    let bVal := (writeByte (BitVec.ofNat 64 idx)).zeroExtend 64
    let newSlot :=
      match curStk slotIdx with
      | .bytes oldV oldInit =>
          StackSlot.bytes (insertSubValue oldV bVal subIdx .u8)
            (insertSubValue oldInit 0xFF#64 subIdx .u8)
      | .spilled cap =>
          StackSlot.bytes (insertSubValue (cap.toBits ly arrayLen) bVal subIdx .u8)
            (insertSubValue 0#64 0xFF#64 subIdx .u8)
    updateStack curStk slotIdx newSlot
  let stk₁' := (List.range' 0 bytes.toNat).foldl (stepFn layout₁) stk₁
  let stk₂' := (List.range' 0 bytes.toNat).foldl (stepFn layout₂) stk₂
  refine ⟨s', stk₁', stk₂', hloop, ?_, ?_, hs', hrel_out⟩
  · unfold writeStackRangeHelper
    simp only [hn_gt, hn_ge, hn_ovf, hn_sum_gt, Bool.false_or, hrange_eq]
    rfl
  · unfold writeStackRangeHelper
    simp only [hn_gt, hn_ge, hn_ovf, hn_sum_gt, Bool.false_or, hrange_eq]
    rfl

end StarnixEbpf.Proof
