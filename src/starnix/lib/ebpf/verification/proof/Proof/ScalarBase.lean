-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Spec
import Proof.LoopLemmas
import Std.Tactic.BVDecide

/-!
# Foundational Lemmas and Range Soundness for `ebpf_core::scalar_value`

Provides monadic/scalar bridge lemmas, `leading_zeros` = `BitVec.clz` bridges,
unconditional `.ok` and 2-safety soundness for `U64ScalarValueData::new` and
`U32ScalarValueData::new`, core scalar constants/helpers, and interval soundness
for all `U64Range` and `U32Range` operations.
-/

open Aeneas Aeneas.Std Result

namespace StarnixEbpf.Proof

open StarnixEbpf.Spec
open ebpf_core.scalar_value

/-! ## 1. Monadic (`Result`), Scalar (`UScalar`), and `leadingZeros` Bridge Lemmas -/

/-- Relational 2-safety concretization for 32-bit abstract scalars. -/
def ConcretizesScalar32Rel2
    (d : ebpf_core.scalar_value.U32ScalarValueData) (x₁ x₂ : BitVec 32) : Prop :=
  ConcretizesScalar32 d x₁ ∧
  ConcretizesScalar32 d x₂ ∧
  (x₁ &&& ~~~d.unwritten_mask.bv = x₂ &&& ~~~d.unwritten_mask.bv)

theorem bind_ok_eq {α β : Type} {m : Result α} {v : α} {f : α → Result β}
    (hm : m = ok v) : (do let x ← m; f x) = f v := by
  rw [hm]; exact bind_tc_ok v f

theorem bind_pair_ok_eq {α β γ : Type} {m : Result (α × β)} {a : α} {b : β}
    {f : α → β → Result γ} (hm : m = ok (a, b)) :
    (do let (x, y) ← m; f x y) = f a b := by
  rw [hm]; exact bind_tc_ok (a, b) _

theorem bind_triple_ok_eq {α β γ δ : Type} {m : Result (α × β × γ)} {a : α} {b : β} {c : γ}
    {f : α → β → γ → Result δ} (hm : m = ok (a, b, c)) :
    (do let (x, y, z) ← m; f x y z) = f a b c := by
  rw [hm]; exact bind_tc_ok (a, b, c) _

theorem lift_eq_ok {α : Type} (x : α) : lift x = ok x := rfl

theorem u64_le_iff_bv (a b : Std.U64) : (a ≤ b) ↔ (a.bv ≤ b.bv) := by
  simp [UScalar.le_equiv, BitVec.le_def]

theorem u64_lt_iff_bv (a b : Std.U64) : (a < b) ↔ (a.bv < b.bv) := by
  simp [UScalar.lt_equiv, BitVec.lt_def]

theorem u64_ge_iff_bv (a b : Std.U64) : (a ≥ b) ↔ (b.bv ≤ a.bv) := u64_le_iff_bv b a
theorem u64_gt_iff_bv (a b : Std.U64) : (a > b) ↔ (b.bv < a.bv) := u64_lt_iff_bv b a

theorem u64_eq_iff_bv (a b : Std.U64) : (a = b) ↔ (a.bv = b.bv) := by
  constructor
  · intro h; rw [h]
  · intro h; cases a; cases b; simp_all

theorem u32_le_iff_bv (a b : Std.U32) : (a ≤ b) ↔ (a.bv ≤ b.bv) := by
  simp [UScalar.le_equiv, BitVec.le_def]

theorem u32_lt_iff_bv (a b : Std.U32) : (a < b) ↔ (a.bv < b.bv) := by
  simp [UScalar.lt_equiv, BitVec.lt_def]

theorem u32_ge_iff_bv (a b : Std.U32) : (a ≥ b) ↔ (b.bv ≤ a.bv) := u32_le_iff_bv b a
theorem u32_gt_iff_bv (a b : Std.U32) : (a > b) ↔ (b.bv < a.bv) := u32_lt_iff_bv b a

theorem u32_eq_iff_bv (a b : Std.U32) : (a = b) ↔ (a.bv = b.bv) := by
  constructor
  · intro h; rw [h]
  · intro h; cases a; cases b; simp_all

@[simp, bv_normalize] theorem u64_mk_bv (v : BitVec 64) : U64.bv ⟨v⟩ = v := rfl
@[simp, bv_normalize] theorem u64_mk_uscalar_bv (v : BitVec 64) : (⟨v⟩ : Std.U64).bv = v := rfl
@[simp, bv_normalize] theorem u64_xor_bv (a b : Std.U64) : U64.bv (a ^^^ b) = a.bv ^^^ b.bv := rfl
@[simp, bv_normalize] theorem u64_and_bv (a b : Std.U64) : U64.bv (a &&& b) = a.bv &&& b.bv := rfl
@[simp, bv_normalize] theorem u64_or_bv (a b : Std.U64) : U64.bv (a ||| b) = a.bv ||| b.bv := rfl
@[simp, bv_normalize] theorem u64_not_bv (a : Std.U64) : U64.bv (~~~a) = ~~~a.bv := rfl
@[simp, bv_normalize] theorem u64_zero_bv : U64.bv 0#u64 = 0#64 := rfl
@[simp, bv_normalize] theorem u64_zero_uscalar_bv : (0#u64).bv = 0#64 := rfl
@[simp, bv_normalize] theorem u64_one_bv : U64.bv 1#u64 = 1#64 := rfl
@[simp, bv_normalize] theorem u64_one_uscalar_bv : (1#u64).bv = 1#64 := rfl
@[simp, bv_normalize] theorem u64_max_bv : U64.bv core.num.U64.MAX = ~~~0#64 := by decide
@[simp, bv_normalize] theorem u64_max_uscalar_bv : core.num.U64.MAX.bv = ~~~0#64 := by decide
@[simp, bv_normalize] theorem u64_rmax_bv : BitVec.ofNat 64 U64.rMax = ~~~0#64 := by decide

@[simp, bv_normalize] theorem u32_mk_bv (v : BitVec 32) : U32.bv ⟨v⟩ = v := rfl
@[simp, bv_normalize] theorem u32_mk_uscalar_bv (v : BitVec 32) : (⟨v⟩ : Std.U32).bv = v := rfl
@[simp, bv_normalize] theorem u32_xor_bv (a b : Std.U32) : U32.bv (a ^^^ b) = a.bv ^^^ b.bv := rfl
@[simp, bv_normalize] theorem u32_and_bv (a b : Std.U32) : U32.bv (a &&& b) = a.bv &&& b.bv := rfl
@[simp, bv_normalize] theorem u32_or_bv (a b : Std.U32) : U32.bv (a ||| b) = a.bv ||| b.bv := rfl
@[simp, bv_normalize] theorem u32_not_bv (a : Std.U32) : U32.bv (~~~a) = ~~~a.bv := rfl
@[simp, bv_normalize] theorem u32_zero_bv : U32.bv 0#u32 = 0#32 := rfl
@[simp, bv_normalize] theorem u32_zero_uscalar_bv : (0#u32).bv = 0#32 := rfl
@[simp, bv_normalize] theorem u32_one_bv : U32.bv 1#u32 = 1#32 := rfl
@[simp, bv_normalize] theorem u32_one_uscalar_bv : (1#u32).bv = 1#32 := rfl
@[simp, bv_normalize] theorem u32_max_bv : U32.bv core.num.U32.MAX = ~~~0#32 := by decide
@[simp, bv_normalize] theorem u32_max_uscalar_bv : core.num.U32.MAX.bv = ~~~0#32 := by decide
@[simp, bv_normalize] theorem u32_rmax_bv : BitVec.ofNat 32 U32.rMax = ~~~0#32 := by decide

theorem uscalar_sub_eq_ok {ty : UScalarTy} (x y : UScalar ty) (h : y.val ≤ x.val) :
    (x - y : Result (UScalar ty)) = ok ⟨x.bv - y.bv⟩ := by
  have hs := UScalar.sub_bv_spec h
  cases hr : (x - y : Result (UScalar ty)) with
  | ret z =>
    rw [hr] at hs; simp at hs
    cases z
    exact congrArg ok (congrArg UScalar.mk hs.2.2)
  | vis _ _ =>
    rw [hr] at hs; simp at hs
  | div =>
    rw [hr] at hs; simp at hs

theorem uscalar_add_eq_ok {ty : UScalarTy} (x y : UScalar ty) (h : x.val + y.val ≤ UScalar.max ty) :
    (x + y : Result (UScalar ty)) = ok ⟨x.bv + y.bv⟩ := by
  have hs := UScalar.add_bv_spec h
  cases hr : (x + y : Result (UScalar ty)) with
  | ret z =>
    rw [hr] at hs; simp at hs
    cases z
    exact congrArg ok (congrArg UScalar.mk hs.2)
  | vis _ _ =>
    rw [hr] at hs; simp at hs
  | div =>
    rw [hr] at hs; simp at hs

theorem uscalar_mul_eq_ok {ty : UScalarTy} (x y : UScalar ty) (h : x.val * y.val ≤ UScalar.max ty) :
    (x * y : Result (UScalar ty)) = ok ⟨x.bv * y.bv⟩ := by
  have hs := UScalar.mul_bv_spec h
  cases hr : (x * y : Result (UScalar ty)) with
  | ret z =>
    rw [hr] at hs; simp at hs
    cases z
    exact congrArg ok (congrArg UScalar.mk hs.2)
  | vis _ _ =>
    rw [hr] at hs; simp at hs
  | div =>
    rw [hr] at hs; simp at hs

@[simp] theorem u64_guard_eta (d : U64ScalarValueData) : { d with _guard := () } = d := rfl
@[simp] theorem u32_guard_eta (d : U32ScalarValueData) : { d with _guard := () } = d := rfl

@[simp] theorem cast_u32_to_u64_bv (x : Std.U32) :
    (UScalar.cast .U64 x).bv = x.bv.zeroExtend 64 := by
  change BitVec.setWidth 64 x.bv = x.bv.zeroExtend 64; rfl

@[simp] theorem cast_u64_to_u32_bv (x : Std.U64) :
    (UScalar.cast .U32 x).bv = x.bv.setWidth 32 := by
  change BitVec.setWidth 32 x.bv = x.bv.setWidth 32; rfl

theorem max_u64_eq_ok (a b : Std.U64) :
    ebpf_core.scalar_value.max_u64 a b =
      ok ⟨if b.bv ≤ a.bv then a.bv else b.bv⟩ := by
  unfold ebpf_core.scalar_value.max_u64
  by_cases h : a ≥ b
  · have hbv : b.bv ≤ a.bv := (u64_ge_iff_bv a b).mp h
    simp [h, hbv]
  · have hbv : ¬(b.bv ≤ a.bv) := fun hc => h ((u64_ge_iff_bv a b).mpr hc)
    simp [h, hbv]

theorem min_u64_eq_ok (a b : Std.U64) :
    ebpf_core.scalar_value.min_u64 a b =
      ok ⟨if a.bv ≤ b.bv then a.bv else b.bv⟩ := by
  unfold ebpf_core.scalar_value.min_u64
  by_cases h : a ≤ b
  · have hbv : a.bv ≤ b.bv := (u64_le_iff_bv a b).mp h
    simp [h, hbv]
  · have hbv : ¬(a.bv ≤ b.bv) := fun hc => h ((u64_le_iff_bv a b).mpr hc)
    simp [h, hbv]

theorem max_u32_eq_ok (a b : Std.U32) :
    ebpf_core.scalar_value.max_u32 a b =
      ok ⟨if b.bv ≤ a.bv then a.bv else b.bv⟩ := by
  unfold ebpf_core.scalar_value.max_u32
  by_cases h : a ≥ b
  · have hbv : b.bv ≤ a.bv := (u32_ge_iff_bv a b).mp h
    simp [h, hbv]
  · have hbv : ¬(b.bv ≤ a.bv) := fun hc => h ((u32_ge_iff_bv a b).mpr hc)
    simp [h, hbv]

theorem min_u32_eq_ok (a b : Std.U32) :
    ebpf_core.scalar_value.min_u32 a b =
      ok ⟨if a.bv ≤ b.bv then a.bv else b.bv⟩ := by
  unfold ebpf_core.scalar_value.min_u32
  by_cases h : a ≤ b
  · have hbv : a.bv ≤ b.bv := (u32_le_iff_bv a b).mp h
    simp [h, hbv]
  · have hbv : ¬(a.bv ≤ b.bv) := fun hc => h ((u32_le_iff_bv a b).mpr hc)
    simp [h, hbv]

private theorem clz64_of_bounds (b : BitVec 64) (k : BitVec 7)
    (hk : k ≤ 63#7)
    (hlo : (1#64 <<< k.setWidth 6) ≤ b)
    (hhi : k = 63#7 ∨ b < (1#64 <<< (k + 1#7).setWidth 6)) :
    b.clz = 63#64 - k.setWidth 64 := by
  bv_decide

/-- Bridge connecting `Aeneas.Std.BitVec.leadingZeros` (`Nat.log 2`) to `BitVec.clz` on 64 bits. -/
theorem leadingZeros64_eq_clz (b : BitVec 64) :
    BitVec.ofNat 32 (Aeneas.Std.BitVec.leadingZeros b) = b.clz.setWidth 32 := by
  unfold Aeneas.Std.BitVec.leadingZeros
  by_cases hb : b = 0
  · subst hb; decide
  · simp only [hb, ↓reduceIte]
    have hb_pos : b.toNat ≠ 0 := by
      intro h; apply hb; exact BitVec.eq_of_toNat_eq (by simpa using h)
    set k := Nat.log 2 b.toNat
    have hk_lt : k < 64 := (Nat.log_lt_iff_lt_pow (by decide) hb_pos).2 b.isLt
    have hpow_le : 2 ^ k ≤ b.toNat := (Nat.le_log_iff_pow_le (by decide) hb_pos).1 le_rfl
    have hlt_pow : b.toNat < 2 ^ (k + 1) :=
      (Nat.log_lt_iff_lt_pow (by decide) hb_pos).1 (Nat.lt_succ_self k)
    let kbv : BitVec 7 := BitVec.ofNat 7 k
    have hkbv : kbv ≤ 63#7 := by
      rw [BitVec.le_def]; simp [kbv]; omega
    have hlo : (1#64 <<< kbv.setWidth 6) ≤ b := by
      rw [BitVec.le_def]
      have h1 : (1#64 <<< kbv.setWidth 6).toNat = 2 ^ k := by
        simp [kbv, BitVec.toNat_shiftLeft, Nat.shiftLeft_eq]
        have hm : k % 64 = k := Nat.mod_eq_of_lt hk_lt
        rw [hm]
        exact Nat.mod_eq_of_lt (Nat.pow_lt_pow_right (by decide : 1 < 2) hk_lt)
      omega
    have hhi : kbv = 63#7 ∨ b < (1#64 <<< (kbv + 1#7).setWidth 6) := by
      by_cases hk63 : k = 63
      · left; apply BitVec.eq_of_toNat_eq; simp [kbv, hk63]
      · right
        have hk1_lt : k + 1 < 64 := by omega
        rw [BitVec.lt_def]
        have h2 : (1#64 <<< (kbv + 1#7).setWidth 6).toNat = 2 ^ (k + 1) := by
          simp [kbv, BitVec.toNat_shiftLeft, BitVec.toNat_add, Nat.shiftLeft_eq]
          have hm1 : (k + 1) % 64 = k + 1 := Nat.mod_eq_of_lt hk1_lt
          rw [hm1]
          exact Nat.mod_eq_of_lt (Nat.pow_lt_pow_right (by decide : 1 < 2) hk1_lt)
        omega
    have hclz := clz64_of_bounds b kbv hkbv hlo hhi
    rw [hclz]
    apply BitVec.eq_of_toNat_eq
    simp [kbv, BitVec.toNat_sub]
    omega

private theorem clz32_of_bounds (b : BitVec 32) (k : BitVec 6)
    (hk : k ≤ 31#6)
    (hlo : (1#32 <<< k.setWidth 5) ≤ b)
    (hhi : k = 31#6 ∨ b < (1#32 <<< (k + 1#6).setWidth 5)) :
    b.clz = 31#32 - k.setWidth 32 := by
  bv_decide

/-- Bridge connecting `core.num.U32.leading_zeros` (`Nat.log 2`) to `BitVec.clz` on 32 bits. -/
theorem leadingZeros32_eq_clz (b : BitVec 32) :
    (core.num.U32.leading_zeros ⟨b⟩).bv = b.clz := by
  change BitVec.ofNat 32 (Aeneas.Std.BitVec.leadingZeros b) = b.clz
  unfold Aeneas.Std.BitVec.leadingZeros
  by_cases hb : b = 0
  · subst hb; decide
  · simp only [hb, ↓reduceIte]
    have hb_pos : b.toNat ≠ 0 := by
      intro h; apply hb; exact BitVec.eq_of_toNat_eq (by simpa using h)
    set k := Nat.log 2 b.toNat
    have hk_lt : k < 32 := (Nat.log_lt_iff_lt_pow (by decide) hb_pos).2 b.isLt
    have hpow_le : 2 ^ k ≤ b.toNat := (Nat.le_log_iff_pow_le (by decide) hb_pos).1 le_rfl
    have hlt_pow : b.toNat < 2 ^ (k + 1) :=
      (Nat.log_lt_iff_lt_pow (by decide) hb_pos).1 (Nat.lt_succ_self k)
    let kbv : BitVec 6 := BitVec.ofNat 6 k
    have hkbv : kbv ≤ 31#6 := by
      rw [BitVec.le_def]; simp [kbv]; omega
    have hlo : (1#32 <<< kbv.setWidth 5) ≤ b := by
      rw [BitVec.le_def]
      have h1 : (1#32 <<< kbv.setWidth 5).toNat = 2 ^ k := by
        simp [kbv, BitVec.toNat_shiftLeft, Nat.shiftLeft_eq]
        have hm : k % 32 = k := Nat.mod_eq_of_lt hk_lt
        rw [hm]
        exact Nat.mod_eq_of_lt (Nat.pow_lt_pow_right (by decide : 1 < 2) hk_lt)
      omega
    have hhi : kbv = 31#6 ∨ b < (1#32 <<< (kbv + 1#6).setWidth 5) := by
      by_cases hk31 : k = 31
      · left; apply BitVec.eq_of_toNat_eq; simp [kbv, hk31]
      · right
        have hk1_lt : k + 1 < 32 := by omega
        rw [BitVec.lt_def]
        have h2 : (1#32 <<< (kbv + 1#6).setWidth 5).toNat = 2 ^ (k + 1) := by
          simp [kbv, BitVec.toNat_shiftLeft, BitVec.toNat_add, Nat.shiftLeft_eq]
          have hm1 : (k + 1) % 32 = k + 1 := Nat.mod_eq_of_lt hk1_lt
          rw [hm1]
          exact Nat.mod_eq_of_lt (Nat.pow_lt_pow_right (by decide : 1 < 2) hk1_lt)
        omega
    have hclz := clz32_of_bounds b kbv hkbv hlo hhi
    rw [hclz]
    apply BitVec.eq_of_toNat_eq
    simp [kbv, BitVec.toNat_sub]
    omega

theorem sub_64_clz (x : Std.U64) :
    (64#u32 - core.num.U64.leading_zeros x : Result Std.U32) =
      ok ⟨64#32 - x.bv.clz.setWidth 32⟩ := by
  have hclz_le : x.bv.clz ≤ 64#64 := by bv_decide
  have hclz32_le : (x.bv.clz.setWidth 32).toNat ≤ 64 := by
    have : x.bv.clz.setWidth 32 ≤ 64#32 := by revert hclz_le; bv_decide
    exact this
  have hbv : (core.num.U64.leading_zeros x).bv = x.bv.clz.setWidth 32 := leadingZeros64_eq_clz x.bv
  have hval : (core.num.U64.leading_zeros x).val = (x.bv.clz.setWidth 32).toNat := by
    rw [← UScalar.bv_toNat]
    exact congrArg BitVec.toNat hbv
  change UScalar.sub 64#u32 (core.num.U64.leading_zeros x) = _
  have hbits : (64#u32).val = 64 := by decide
  have hnotlt : ¬((64#u32).val < (core.num.U64.leading_zeros x).val) := by omega
  simp only [UScalar.sub, hnotlt, ↓reduceIte]
  congr 1
  rw [u32_eq_iff_bv]
  apply BitVec.eq_of_toNat_eq
  rw [hbits, hval, BitVec.toNat_sub_of_le]
  · change (BitVec.ofNat 32 (64 - (x.bv.clz.setWidth 32).toNat)).toNat = 64 - (x.bv.clz.setWidth 32).toNat
    rw [BitVec.toNat_ofNat]
    omega
  · exact hclz32_le

theorem sub_32_clz (x : Std.U32) :
    (32#u32 - core.num.U32.leading_zeros x : Result Std.U32) =
      ok ⟨32#32 - x.bv.clz⟩ := by
  have hclz_le : x.bv.clz.toNat ≤ 32 := by
    have : x.bv.clz ≤ 32#32 := by bv_decide
    exact this
  have hbv : (core.num.U32.leading_zeros x).bv = x.bv.clz := leadingZeros32_eq_clz x.bv
  have hval : (core.num.U32.leading_zeros x).val = x.bv.clz.toNat := by
    rw [← UScalar.bv_toNat]
    exact congrArg BitVec.toNat hbv
  change UScalar.sub 32#u32 (core.num.U32.leading_zeros x) = _
  have hbits : (32#u32).val = 32 := by decide
  have hnotlt : ¬((32#u32).val < (core.num.U32.leading_zeros x).val) := by omega
  simp only [UScalar.sub, hnotlt, ↓reduceIte]
  congr 1
  rw [u32_eq_iff_bv]
  apply BitVec.eq_of_toNat_eq
  rw [hbits, hval, BitVec.toNat_sub_of_le]
  · change (BitVec.ofNat 32 (32 - x.bv.clz.toNat)).toNat = 32 - x.bv.clz.toNat
    rw [BitVec.toNat_ofNat]
    omega
  · exact hclz_le

theorem u64_wrapping_add_eq (x y : Std.U64) :
    core.num.U64.wrapping_add x y = ⟨x.bv + y.bv⟩ := rfl

theorem u64_wrapping_sub_eq (x y : Std.U64) :
    core.num.U64.wrapping_sub x y = ⟨x.bv - y.bv⟩ := rfl

theorem u64_wrapping_mul_eq (x y : Std.U64) :
    core.num.U64.wrapping_mul x y = ⟨x.bv * y.bv⟩ := rfl

theorem u64_wrapping_shl_eq (x : Std.U64) (s : Std.U32) :
    core.num.U64.wrapping_shl x s = ⟨x.bv <<< s.bv.setWidth 6⟩ := by
  rw [u64_eq_iff_bv]
  change x.bv <<< (s.val % 64) = x.bv <<< s.bv.setWidth 6
  rw [BitVec.shiftLeft_eq', BitVec.toNat_setWidth, UScalar.bv_toNat]

theorem u64_wrapping_shr_eq (x : Std.U64) (s : Std.U32) :
    core.num.U64.wrapping_shr x s = ⟨x.bv >>> s.bv.setWidth 6⟩ := by
  rw [u64_eq_iff_bv]
  change x.bv >>> (s.val % 64) = x.bv >>> s.bv.setWidth 6
  rw [BitVec.ushiftRight_eq', BitVec.toNat_setWidth, UScalar.bv_toNat]

theorem u32_wrapping_add_eq (x y : Std.U32) :
    core.num.U32.wrapping_add x y = ⟨x.bv + y.bv⟩ := rfl

theorem u32_wrapping_sub_eq (x y : Std.U32) :
    core.num.U32.wrapping_sub x y = ⟨x.bv - y.bv⟩ := rfl

theorem u32_wrapping_mul_eq (x y : Std.U32) :
    core.num.U32.wrapping_mul x y = ⟨x.bv * y.bv⟩ := rfl

theorem u32_wrapping_shl_eq (x : Std.U32) (s : Std.U32) :
    core.num.U32.wrapping_shl x s = ⟨x.bv <<< s.bv.setWidth 5⟩ := by
  rw [u32_eq_iff_bv]
  change x.bv <<< (s.val % 32) = x.bv <<< s.bv.setWidth 5
  rw [BitVec.shiftLeft_eq', BitVec.toNat_setWidth, UScalar.bv_toNat]

theorem u32_wrapping_shr_eq (x : Std.U32) (s : Std.U32) :
    core.num.U32.wrapping_shr x s = ⟨x.bv >>> s.bv.setWidth 5⟩ := by
  rw [u32_eq_iff_bv]
  change x.bv >>> (s.val % 32) = x.bv >>> s.bv.setWidth 5
  rw [BitVec.ushiftRight_eq', BitVec.toNat_setWidth, UScalar.bv_toNat]

theorem ashr_raw_u64_eq_ok (x y : Std.U64) :
    U64ScalarValueData.ashr.ashr_raw x y = ok ⟨BitVec.sshiftRight' x.bv (y.bv.setWidth 6)⟩ := by
  unfold U64ScalarValueData.ashr.ashr_raw
  simp only [lift, bind_ok]
  congr 1
  rw [u64_eq_iff_bv]
  have h1 : (UScalar.hcast .I64 x).bv = x.bv := rfl
  have h2 : ∀ (z : Std.I64), (IScalar.hcast .U64 z).bv = z.bv := by
    intro z; change z.bv.signExtend 64 = z.bv; exact BitVec.signExtend_eq z.bv
  change (IScalar.hcast .U64 (core.num.I64.wrapping_shr (UScalar.hcast .I64 x) (UScalar.cast .U32 y))).bv =
    BitVec.sshiftRight' x.bv (y.bv.setWidth 6)
  rw [h2]
  change (UScalar.hcast .I64 x).bv.sshiftRight ((UScalar.cast .U32 y).val % 64) =
    x.bv.sshiftRight (y.bv.setWidth 6).toNat
  rw [h1, ← UScalar.bv_toNat, cast_u64_to_u32_bv]
  congr 1
  change (BitVec.setWidth 32 y.bv).toNat % 64 = (BitVec.setWidth 6 y.bv).toNat
  rw [BitVec.toNat_setWidth, BitVec.toNat_setWidth]
  omega

theorem ashr_raw_u32_eq_ok (x y : Std.U32) :
    U32ScalarValueData.ashr.ashr_raw x y = ok ⟨BitVec.sshiftRight' x.bv (y.bv.setWidth 5)⟩ := by
  unfold U32ScalarValueData.ashr.ashr_raw
  simp only [lift, bind_ok]
  congr 1
  rw [u32_eq_iff_bv]
  have h1 : (UScalar.hcast .I32 x).bv = x.bv := rfl
  have h2 : ∀ (z : Std.I32), (IScalar.hcast .U32 z).bv = z.bv := by
    intro z; change z.bv.signExtend 32 = z.bv; exact BitVec.signExtend_eq z.bv
  change (IScalar.hcast .U32 (core.num.I32.wrapping_shr (UScalar.hcast .I32 x) y)).bv =
    BitVec.sshiftRight' x.bv (y.bv.setWidth 5)
  rw [h2]
  change (UScalar.hcast .I32 x).bv.sshiftRight (y.val % 32) =
    x.bv.sshiftRight (y.bv.setWidth 5).toNat
  rw [h1, BitVec.toNat_setWidth, UScalar.bv_toNat]

theorem u64_shr_32_i32_eq_ok (x : Std.U64) :
    ((x >>> 32#i32) : Result Std.U64) = ok ⟨x.bv >>> 32#6⟩ := by
  change UScalar.shiftRight_IScalar x 32#i32 = _
  simp only [UScalar.shiftRight_IScalar]
  have hge : (32#i32).val ≥ 0 := by decide
  have hto : (32#i32).toNat = 32 := by decide
  simp only [hge, ↓reduceIte, hto, UScalar.shiftRight, UScalarTy.numBits,
    show (32 : Nat) < 64 by decide]
  congr 1

/-! ## 2. `U64ScalarValueData::new`, `U32ScalarValueData::new`, and Core Constants -/

/-- Pure bitvector mirror of `U64ScalarValueData.new`. -/
def pureNew64 (val unk unwr umin umax : BitVec 64) : U64ScalarValueData :=
  let diff := umin ^^^ umax
  let shift32 : BitVec 32 := 64#32 - diff.clz.setWidth 32
  let common_mask : BitVec 64 :=
    if shift32 == 64#32 then 0#64 else (~~~0#64) <<< shift32.setWidth 6
  let unk1 := unk &&& ~~~common_mask
  let unwr1 := unwr &&& unk1
  let val1 := ((val &&& ~~~unk1) &&& ~~~common_mask) ||| (umin &&& common_mask)
  let rmin := if unk1 == 0#64 then val1 else if umin ≤ val1 then val1 else umin
  let rmax := if unk1 == 0#64 then val1 else if (val1 ||| unk1) ≤ umax then (val1 ||| unk1) else umax
  { value := ⟨val1⟩
    unknown_mask := ⟨unk1⟩
    unwritten_mask := ⟨unwr1⟩
    urange := { min := ⟨rmin⟩, max := ⟨rmax⟩ }
    _guard := () }

/-- Pure bitvector mirror of `U32ScalarValueData.new`. -/
def pureNew32 (val unk unwr umin umax : BitVec 32) : U32ScalarValueData :=
  let diff := umin ^^^ umax
  let shift32 : BitVec 32 := 32#32 - diff.clz
  let common_mask : BitVec 32 :=
    if shift32 == 32#32 then 0#32 else (~~~0#32) <<< shift32.setWidth 5
  let unk1 := unk &&& ~~~common_mask
  let unwr1 := unwr &&& unk1
  let val1 := ((val &&& ~~~unk1) &&& ~~~common_mask) ||| (umin &&& common_mask)
  let rmin := if unk1 == 0#32 then val1 else if umin ≤ val1 then val1 else umin
  let rmax := if unk1 == 0#32 then val1 else if (val1 ||| unk1) ≤ umax then (val1 ||| unk1) else umax
  { value := ⟨val1⟩
    unknown_mask := ⟨unk1⟩
    unwritten_mask := ⟨unwr1⟩
    urange := { min := ⟨rmin⟩, max := ⟨rmax⟩ }
    _guard := () }

/-- `U64ScalarValueData.new` unconditionally returns `.ok (pureNew64 ...)` with zero preconditions. -/
theorem new64_eq_ok (value unknown_mask unwritten_mask : Std.U64) (urange : U64Range) :
    U64ScalarValueData.new value unknown_mask unwritten_mask urange =
      ok (pureNew64 value.bv unknown_mask.bv unwritten_mask.bv urange.min.bv urange.max.bv) := by
  unfold U64ScalarValueData.new U64Range.new pureNew64
  simp only [lift_eq_ok, bind_ok]
  rw [sub_64_clz (urange.min ^^^ urange.max)]
  simp only [bind_ok, u64_xor_bv]
  set sh32 : BitVec 32 := 64#32 - (urange.min.bv ^^^ urange.max.bv).clz.setWidth 32
  by_cases hsh64 : (⟨sh32⟩ : Std.U32) = 64#u32
  · have hsh64' : sh32 = 64#32 := (u32_eq_iff_bv _ _).1 hsh64
    simp only [hsh64, ↓reduceIte, bind_ok]
    simp only [hsh64', beq_self_eq_true, ↓reduceIte,
      max_u64_eq_ok, min_u64_eq_ok, bind_ok, u64_eq_iff_bv, u64_and_bv, u64_or_bv, u64_not_bv,
      u64_zero_bv, beq_iff_eq]
    by_cases h0 : unknown_mask.bv &&& ~~~0#64 = 0#64
    · simp only [h0, ↓reduceIte]
      congr 1
      simp only [U64ScalarValueData.mk.injEq, U64Range.mk.injEq, u64_eq_iff_bv,
        u64_and_bv, u64_or_bv, u64_not_bv, u64_zero_bv, u64_mk_uscalar_bv, h0, and_self]
    · simp only [h0, ↓reduceIte]
      congr 1
  · have hsh64' : sh32 ≠ 64#32 := fun h => hsh64 ((u32_eq_iff_bv _ _).2 h)
    have hsh64_beq : (sh32 == 64#32) = false := beq_false_of_ne hsh64'
    simp only [hsh64, ↓reduceIte, bind_ok]
    simp only [hsh64_beq, Bool.false_eq_true, ↓reduceIte,
      u64_wrapping_shl_eq, max_u64_eq_ok, min_u64_eq_ok, bind_ok, u64_eq_iff_bv,
      u64_and_bv, u64_or_bv, u64_not_bv, u64_zero_bv, u64_mk_uscalar_bv,
      u32_mk_uscalar_bv, beq_iff_eq]
    by_cases h0 : unknown_mask.bv &&& ~~~(~~~0#64 <<< sh32.setWidth 6) = 0#64
    · simp only [h0, ↓reduceIte]
      congr 1
      simp only [U64ScalarValueData.mk.injEq, U64Range.mk.injEq, u64_eq_iff_bv,
        u64_and_bv, u64_or_bv, u64_not_bv, u64_mk_uscalar_bv, h0, and_self]
    · simp only [h0, ↓reduceIte]
      congr 1

/-- `U32ScalarValueData.new` unconditionally returns `.ok (pureNew32 ...)` with zero preconditions. -/
theorem new32_eq_ok (value unknown_mask unwritten_mask : Std.U32) (urange : U32Range) :
    U32ScalarValueData.new value unknown_mask unwritten_mask urange =
      ok (pureNew32 value.bv unknown_mask.bv unwritten_mask.bv urange.min.bv urange.max.bv) := by
  unfold U32ScalarValueData.new U32Range.new pureNew32
  simp only [lift_eq_ok, bind_ok]
  rw [sub_32_clz (urange.min ^^^ urange.max)]
  simp only [bind_ok, u32_xor_bv]
  set sh32 : BitVec 32 := 32#32 - (urange.min.bv ^^^ urange.max.bv).clz
  by_cases hsh32 : (⟨sh32⟩ : Std.U32) = 32#u32
  · have hsh32' : sh32 = 32#32 := (u32_eq_iff_bv _ _).1 hsh32
    simp only [hsh32, ↓reduceIte, bind_ok]
    simp only [hsh32', beq_self_eq_true, ↓reduceIte,
      max_u32_eq_ok, min_u32_eq_ok, bind_ok, u32_eq_iff_bv, u32_and_bv, u32_or_bv, u32_not_bv,
      u32_zero_bv, beq_iff_eq]
    by_cases h0 : unknown_mask.bv &&& ~~~0#32 = 0#32
    · simp only [h0, ↓reduceIte]
      congr 1
      simp only [U32ScalarValueData.mk.injEq, U32Range.mk.injEq, u32_eq_iff_bv,
        u32_and_bv, u32_or_bv, u32_not_bv, u32_zero_bv, u32_mk_uscalar_bv, h0, and_self]
    · simp only [h0, ↓reduceIte]
      congr 1
  · have hsh32' : sh32 ≠ 32#32 := fun h => hsh32 ((u32_eq_iff_bv _ _).2 h)
    have hsh32_beq : (sh32 == 32#32) = false := beq_false_of_ne hsh32'
    simp only [hsh32, ↓reduceIte, bind_ok]
    simp only [hsh32_beq, Bool.false_eq_true, ↓reduceIte,
      u32_wrapping_shl_eq, max_u32_eq_ok, min_u32_eq_ok, bind_ok, u32_eq_iff_bv,
      u32_and_bv, u32_or_bv, u32_not_bv, u32_zero_bv, u32_mk_uscalar_bv, beq_iff_eq]
    by_cases h0 : unknown_mask.bv &&& ~~~(~~~0#32 <<< sh32.setWidth 5) = 0#32
    · simp only [h0, ↓reduceIte]
      congr 1
      simp only [U32ScalarValueData.mk.injEq, U32Range.mk.injEq, u32_eq_iff_bv,
        u32_and_bv, u32_or_bv, u32_not_bv, u32_mk_uscalar_bv, h0, and_self]
    · simp only [h0, ↓reduceIte]
      congr 1

private theorem pureNew64_sound_2safety (val unk unwr umin umax x₁ x₂ : BitVec 64)
    (hmin1 : umin ≤ x₁) (hmax1 : x₁ ≤ umax)
    (htnum1 : x₁ &&& ~~~unk = val &&& ~~~unk)
    (hmin2 : umin ≤ x₂) (hmax2 : x₂ ≤ umax)
    (htnum2 : x₂ &&& ~~~unk = val &&& ~~~unk)
    (hsec : x₁ &&& ~~~unwr = x₂ &&& ~~~unwr) :
    ConcretizesScalar64Rel2 (pureNew64 val unk unwr umin umax) x₁ x₂ := by
  unfold ConcretizesScalar64Rel2 ConcretizesScalar64 Scalar64Consistent pureNew64
  simp only [u64_mk_uscalar_bv]
  bv_decide

private theorem pureNew32_sound_2safety (val unk unwr umin umax x₁ x₂ : BitVec 32)
    (hmin1 : umin ≤ x₁) (hmax1 : x₁ ≤ umax)
    (htnum1 : x₁ &&& ~~~unk = val &&& ~~~unk)
    (hmin2 : umin ≤ x₂) (hmax2 : x₂ ≤ umax)
    (htnum2 : x₂ &&& ~~~unk = val &&& ~~~unk)
    (hsec : x₁ &&& ~~~unwr = x₂ &&& ~~~unwr) :
    ConcretizesScalar32Rel2 (pureNew32 val unk unwr umin umax) x₁ x₂ := by
  unfold ConcretizesScalar32Rel2 ConcretizesScalar32 Scalar32Consistent pureNew32
  simp only [u32_mk_uscalar_bv]
  bv_decide

/-- Zero-mask-precondition 2-safety soundness of `U64ScalarValueData.new`. -/
theorem new64_sound_2safety (value unknown_mask unwritten_mask : Std.U64) (urange : U64Range)
    (x₁ x₂ : BitVec 64)
    (hrange1 : ConcretizesRange64 urange x₁)
    (htnum1 : x₁ &&& ~~~unknown_mask.bv = value.bv &&& ~~~unknown_mask.bv)
    (hrange2 : ConcretizesRange64 urange x₂)
    (htnum2 : x₂ &&& ~~~unknown_mask.bv = value.bv &&& ~~~unknown_mask.bv)
    (hsec : x₁ &&& ~~~unwritten_mask.bv = x₂ &&& ~~~unwritten_mask.bv) :
    ∃ res, U64ScalarValueData.new value unknown_mask unwritten_mask urange = ok res ∧
      ConcretizesScalar64Rel2 res x₁ x₂ := by
  exact ⟨_, new64_eq_ok value unknown_mask unwritten_mask urange,
    pureNew64_sound_2safety value.bv unknown_mask.bv unwritten_mask.bv
      urange.min.bv urange.max.bv x₁ x₂ hrange1.1 hrange1.2 htnum1 hrange2.1 hrange2.2 htnum2 hsec⟩

/-- Zero-mask-precondition 2-safety soundness of `U32ScalarValueData.new`. -/
theorem new32_sound_2safety (value unknown_mask unwritten_mask : Std.U32) (urange : U32Range)
    (x₁ x₂ : BitVec 32)
    (hrange1 : ConcretizesRange32 urange x₁)
    (htnum1 : x₁ &&& ~~~unknown_mask.bv = value.bv &&& ~~~unknown_mask.bv)
    (hrange2 : ConcretizesRange32 urange x₂)
    (htnum2 : x₂ &&& ~~~unknown_mask.bv = value.bv &&& ~~~unknown_mask.bv)
    (hsec : x₁ &&& ~~~unwritten_mask.bv = x₂ &&& ~~~unwritten_mask.bv) :
    ∃ res, U32ScalarValueData.new value unknown_mask unwritten_mask urange = ok res ∧
      ConcretizesScalar32Rel2 res x₁ x₂ := by
  exact ⟨_, new32_eq_ok value unknown_mask unwritten_mask urange,
    pureNew32_sound_2safety value.bv unknown_mask.bv unwritten_mask.bv
      urange.min.bv urange.max.bv x₁ x₂ hrange1.1 hrange1.2 htnum1 hrange2.1 hrange2.2 htnum2 hsec⟩

theorem concretizes64_iff_rel2_diag (d : U64ScalarValueData) (x : BitVec 64) :
    ConcretizesScalar64 d x ↔ ConcretizesScalar64Rel2 d x x := by
  unfold ConcretizesScalar64Rel2; simp

theorem concretizes32_iff_rel2_diag (d : U32ScalarValueData) (x : BitVec 32) :
    ConcretizesScalar32 d x ↔ ConcretizesScalar32Rel2 d x x := by
  unfold ConcretizesScalar32Rel2; simp

theorem fully_initialized_eq_of_rel2_64 {d : U64ScalarValueData} {x₁ x₂ : BitVec 64}
    (hinit : d.unwritten_mask = 0#u64)
    (hrel : ConcretizesScalar64Rel2 d x₁ x₂) :
    x₁ = x₂ := by
  have hunwr : d.unwritten_mask.bv = 0#64 := (u64_eq_iff_bv _ _).1 hinit
  have hsec := hrel.2.2
  rw [hunwr] at hsec
  revert hsec; bv_decide

theorem fully_initialized_eq_of_rel2_32 {d : U32ScalarValueData} {x₁ x₂ : BitVec 32}
    (hinit : d.unwritten_mask = 0#u32)
    (hrel : ConcretizesScalar32Rel2 d x₁ x₂) :
    x₁ = x₂ := by
  have hunwr : d.unwritten_mask.bv = 0#32 := (u32_eq_iff_bv _ _).1 hinit
  have hsec := hrel.2.2
  rw [hunwr] at hsec
  revert hsec; bv_decide

theorem eq_value_of_known_64 {d : U64ScalarValueData} {x : BitVec 64}
    (hk : d.unknown_mask = 0#u64) (hc : ConcretizesScalar64 d x) : x = d.value.bv := by
  have hbv : d.unknown_mask.bv = 0#64 := (u64_eq_iff_bv _ _).1 hk
  have ht := hc.2.1
  rw [hbv] at ht
  revert ht; bv_decide

theorem eq_value_of_known_32 {d : U32ScalarValueData} {x : BitVec 32}
    (hk : d.unknown_mask = 0#u32) (hc : ConcretizesScalar32 d x) : x = d.value.bv := by
  have hbv : d.unknown_mask.bv = 0#32 := (u32_eq_iff_bv _ _).1 hk
  have ht := hc.2.1
  rw [hbv] at ht
  revert ht; bv_decide

theorem UNINITIALIZED_64_eq_ok :
    U64ScalarValueData.UNINITIALIZED =
      ok { value := 0#u64, unknown_mask := core.num.U64.MAX,
           unwritten_mask := core.num.U64.MAX,
           urange := { min := 0#u64, max := core.num.U64.MAX }, _guard := () } := by
  unfold U64ScalarValueData.UNINITIALIZED U64Range.impl.max U64Range.new
  rw [bind_ok_eq rfl, new64_eq_ok]
  rfl

theorem UNINITIALIZED_64_sound :
    ∃ d, U64ScalarValueData.UNINITIALIZED = ok d ∧ Scalar64Consistent d ∧
      ∀ x₁ x₂ : BitVec 64, ConcretizesScalar64Rel2 d x₁ x₂ := by
  refine ⟨_, UNINITIALIZED_64_eq_ok, by unfold Scalar64Consistent; simp, fun x₁ x₂ => ?_⟩
  unfold ConcretizesScalar64Rel2 ConcretizesScalar64 Scalar64Consistent
  simp; bv_decide

theorem UNKNOWN_WRITTEN_64_eq_ok :
    U64ScalarValueData.UNKNOWN_WRITTEN =
      ok { value := 0#u64, unknown_mask := core.num.U64.MAX,
           unwritten_mask := 0#u64,
           urange := { min := 0#u64, max := core.num.U64.MAX }, _guard := () } := by
  unfold U64ScalarValueData.UNKNOWN_WRITTEN U64Range.impl.max U64Range.new
  rw [bind_ok_eq rfl, new64_eq_ok]
  rfl

theorem UNKNOWN_WRITTEN_64_sound :
    ∃ d, U64ScalarValueData.UNKNOWN_WRITTEN = ok d ∧ Scalar64Consistent d ∧
      ∀ x : BitVec 64, ConcretizesScalar64Rel2 d x x := by
  refine ⟨_, UNKNOWN_WRITTEN_64_eq_ok, by unfold Scalar64Consistent; simp, fun x => ?_⟩
  unfold ConcretizesScalar64Rel2 ConcretizesScalar64 Scalar64Consistent
  simp; bv_decide

theorem UNINITIALIZED_32_eq_ok :
    U32ScalarValueData.UNINITIALIZED =
      ok { value := 0#u32, unknown_mask := core.num.U32.MAX,
           unwritten_mask := core.num.U32.MAX,
           urange := { min := 0#u32, max := core.num.U32.MAX }, _guard := () } := by
  unfold U32ScalarValueData.UNINITIALIZED U32Range.impl.max U32Range.new
  rw [bind_ok_eq rfl, new32_eq_ok]
  rfl

theorem UNINITIALIZED_32_sound :
    ∃ d, U32ScalarValueData.UNINITIALIZED = ok d ∧ Scalar32Consistent d ∧
      ∀ x₁ x₂ : BitVec 32, ConcretizesScalar32Rel2 d x₁ x₂ := by
  refine ⟨_, UNINITIALIZED_32_eq_ok, by unfold Scalar32Consistent; simp, fun x₁ x₂ => ?_⟩
  unfold ConcretizesScalar32Rel2 ConcretizesScalar32 Scalar32Consistent
  simp; bv_decide

theorem UNKNOWN_WRITTEN_32_eq_ok :
    U32ScalarValueData.UNKNOWN_WRITTEN =
      ok { value := 0#u32, unknown_mask := core.num.U32.MAX,
           unwritten_mask := 0#u32,
           urange := { min := 0#u32, max := core.num.U32.MAX }, _guard := () } := by
  unfold U32ScalarValueData.UNKNOWN_WRITTEN U32Range.impl.max U32Range.new
  rw [bind_ok_eq rfl, new32_eq_ok]
  rfl

theorem UNKNOWN_WRITTEN_32_sound :
    ∃ d, U32ScalarValueData.UNKNOWN_WRITTEN = ok d ∧ Scalar32Consistent d ∧
      ∀ x : BitVec 32, ConcretizesScalar32Rel2 d x x := by
  refine ⟨_, UNKNOWN_WRITTEN_32_eq_ok, by unfold Scalar32Consistent; simp, fun x => ?_⟩
  unfold ConcretizesScalar32Rel2 ConcretizesScalar32 Scalar32Consistent
  simp; bv_decide

theorem from_known_64_eq_ok (v : Std.U64) :
    U64ScalarValueData.from_known v = ok (pureNew64 v.bv 0#64 0#64 v.bv v.bv) := by
  unfold U64ScalarValueData.from_known U64Range.from_scalar U64Range.new
  rw [bind_ok_eq rfl, new64_eq_ok]
  rfl

theorem from_known_64_sound (v : Std.U64) :
    ∃ d, U64ScalarValueData.from_known v = ok d ∧ Scalar64Consistent d ∧
      ConcretizesScalar64Rel2 d v.bv v.bv := by
  unfold U64ScalarValueData.from_known U64Range.from_scalar U64Range.new
  rw [bind_ok_eq rfl]
  obtain ⟨d, hd, hc⟩ :=
    new64_sound_2safety v 0#u64 0#u64 { min := v, max := v } v.bv v.bv
      ⟨BitVec.le_refl _, BitVec.le_refl _⟩ rfl
      ⟨BitVec.le_refl _, BitVec.le_refl _⟩ rfl rfl
  exact ⟨d, hd, hc.1.1, hc⟩

theorem from_known_32_eq_ok (v : Std.U32) :
    U32ScalarValueData.from_known v = ok (pureNew32 v.bv 0#32 0#32 v.bv v.bv) := by
  unfold U32ScalarValueData.from_known U32Range.from_scalar U32Range.new
  rw [bind_ok_eq rfl, new32_eq_ok]
  rfl

theorem from_known_32_sound (v : Std.U32) :
    ∃ d, U32ScalarValueData.from_known v = ok d ∧ Scalar32Consistent d ∧
      ConcretizesScalar32Rel2 d v.bv v.bv := by
  unfold U32ScalarValueData.from_known U32Range.from_scalar U32Range.new
  rw [bind_ok_eq rfl]
  obtain ⟨d, hd, hc⟩ :=
    new32_sound_2safety v 0#u32 0#u32 { min := v, max := v } v.bv v.bv
      ⟨BitVec.le_refl _, BitVec.le_refl _⟩ rfl
      ⟨BitVec.le_refl _, BitVec.le_refl _⟩ rfl rfl
  exact ⟨d, hd, hc.1.1, hc⟩

theorem base_64_ok (a b : U64ScalarValueData) :
    ∃ usvd, U64ScalarValueData.base a b = ok usvd := by
  unfold U64ScalarValueData.base U64ScalarValueData.is_fully_initialized
  rw [bind_ok_eq rfl]; split_ifs
  · rw [bind_ok_eq rfl]; split_ifs
    · exact ⟨_, UNKNOWN_WRITTEN_64_eq_ok⟩
    · exact ⟨_, UNINITIALIZED_64_eq_ok⟩
  · exact ⟨_, UNINITIALIZED_64_eq_ok⟩

theorem base_32_ok (a b : U32ScalarValueData) :
    ∃ usvd, U32ScalarValueData.base a b = ok usvd := by
  unfold U32ScalarValueData.base U32ScalarValueData.is_fully_initialized
  rw [bind_ok_eq rfl]; split_ifs
  · rw [bind_ok_eq rfl]; split_ifs
    · exact ⟨_, UNKNOWN_WRITTEN_32_eq_ok⟩
    · exact ⟨_, UNINITIALIZED_32_eq_ok⟩
  · exact ⟨_, UNINITIALIZED_32_eq_ok⟩

theorem base_update_range_64_sound_2safety (a b : U64ScalarValueData) (ur : U64Range)
    {x₁ x₂ y₁ y₂ : BitVec 64} (f : BitVec 64 → BitVec 64 → BitVec 64)
    (ha : ConcretizesScalar64Rel2 a x₁ x₂) (hb : ConcretizesScalar64Rel2 b y₁ y₂)
    (hur1 : ConcretizesRange64 ur (f x₁ y₁)) (hur2 : ConcretizesRange64 ur (f x₂ y₂)) :
    ∃ res, (do let usvd ← U64ScalarValueData.base a b; U64ScalarValueData.update_range usvd ur) = ok res ∧
      ConcretizesScalar64Rel2 res (f x₁ y₁) (f x₂ y₂) := by
  unfold U64ScalarValueData.base U64ScalarValueData.is_fully_initialized U64ScalarValueData.update_range
  rw [bind_ok_eq rfl]
  by_cases ha_init : a.unwritten_mask = 0#u64
  · simp only [ha_init, decide_true, ↓reduceIte]
    rw [bind_ok_eq rfl]
    by_cases hb_init : b.unwritten_mask = 0#u64
    · simp only [hb_init, decide_true, ↓reduceIte]
      rw [bind_ok_eq UNKNOWN_WRITTEN_64_eq_ok]
      have hx_eq := fully_initialized_eq_of_rel2_64 ha_init ha
      have hy_eq := fully_initialized_eq_of_rel2_64 hb_init hb
      subst hx_eq hy_eq
      exact new64_sound_2safety 0#u64 core.num.U64.MAX 0#u64 ur (f x₁ y₁) (f x₁ y₁)
        hur1 (by simp) hur2 (by simp) rfl
    · simp only [hb_init, decide_false, Bool.false_eq_true, ↓reduceIte]
      rw [bind_ok_eq UNINITIALIZED_64_eq_ok]
      exact new64_sound_2safety 0#u64 core.num.U64.MAX core.num.U64.MAX ur (f x₁ y₁) (f x₂ y₂)
        hur1 (by simp) hur2 (by simp) (by simp)
  · simp only [ha_init, decide_false, Bool.false_eq_true, ↓reduceIte]
    rw [bind_ok_eq UNINITIALIZED_64_eq_ok]
    exact new64_sound_2safety 0#u64 core.num.U64.MAX core.num.U64.MAX ur (f x₁ y₁) (f x₂ y₂)
      hur1 (by simp) hur2 (by simp) (by simp)

theorem base_update_range_32_sound_2safety (a b : U32ScalarValueData) (ur : U32Range)
    {x₁ x₂ y₁ y₂ : BitVec 32} (f : BitVec 32 → BitVec 32 → BitVec 32)
    (ha : ConcretizesScalar32Rel2 a x₁ x₂) (hb : ConcretizesScalar32Rel2 b y₁ y₂)
    (hur1 : ConcretizesRange32 ur (f x₁ y₁)) (hur2 : ConcretizesRange32 ur (f x₂ y₂)) :
    ∃ res, (do let usvd ← U32ScalarValueData.base a b; U32ScalarValueData.update_range usvd ur) = ok res ∧
      ConcretizesScalar32Rel2 res (f x₁ y₁) (f x₂ y₂) := by
  unfold U32ScalarValueData.base U32ScalarValueData.is_fully_initialized U32ScalarValueData.update_range
  rw [bind_ok_eq rfl]
  by_cases ha_init : a.unwritten_mask = 0#u32
  · simp only [ha_init, decide_true, ↓reduceIte]
    rw [bind_ok_eq rfl]
    by_cases hb_init : b.unwritten_mask = 0#u32
    · simp only [hb_init, decide_true, ↓reduceIte]
      rw [bind_ok_eq UNKNOWN_WRITTEN_32_eq_ok]
      have hx_eq := fully_initialized_eq_of_rel2_32 ha_init ha
      have hy_eq := fully_initialized_eq_of_rel2_32 hb_init hb
      subst hx_eq hy_eq
      exact new32_sound_2safety 0#u32 core.num.U32.MAX 0#u32 ur (f x₁ y₁) (f x₁ y₁)
        hur1 (by simp) hur2 (by simp) rfl
    · simp only [hb_init, decide_false, Bool.false_eq_true, ↓reduceIte]
      rw [bind_ok_eq UNINITIALIZED_32_eq_ok]
      exact new32_sound_2safety 0#u32 core.num.U32.MAX core.num.U32.MAX ur (f x₁ y₁) (f x₂ y₂)
        hur1 (by simp) hur2 (by simp) (by simp)
  · simp only [ha_init, decide_false, Bool.false_eq_true, ↓reduceIte]
    rw [bind_ok_eq UNINITIALIZED_32_eq_ok]
    exact new32_sound_2safety 0#u32 core.num.U32.MAX core.num.U32.MAX ur (f x₁ y₁) (f x₂ y₂)
      hur1 (by simp) hur2 (by simp) (by simp)

/-! ## 3. `U64Range` and `U32Range` Interval Soundness -/

theorem range64_max_sound (z : BitVec 64) :
    ConcretizesRange64 { min := 0#u64, max := core.num.U64.MAX } z := by
  unfold ConcretizesRange64; simp; bv_decide

theorem range32_max_sound (z : BitVec 32) :
    ConcretizesRange32 { min := 0#u32, max := core.num.U32.MAX } z := by
  unfold ConcretizesRange32; simp; bv_decide

theorem range64_add_ok_and_sound (r₁ r₂ : U64Range) :
    ∃ r, U64Range.add_range r₁ r₂ = ok r ∧
      ∀ x y : BitVec 64, ConcretizesRange64 r₁ x → ConcretizesRange64 r₂ y → ConcretizesRange64 r (x + y) := by
  rcases r₁ with ⟨⟨min1⟩, ⟨max1⟩⟩
  rcases r₂ with ⟨⟨min2⟩, ⟨max2⟩⟩
  change BitVec 64 at min1 max1 min2 max2
  unfold U64Range.add_range U64Range.impl.max U64Range.new core.num.U64.overflowing_add UScalar.overflowing_add lift
  rw [bind_pair_ok_eq rfl, bind_pair_ok_eq rfl]
  dsimp only [u64_mk_uscalar_bv]
  by_cases hov : (min1.uaddOverflow min2 != max1.uaddOverflow max2) = true
  · simp only [hov, ↓reduceIte]
    exact ⟨_, rfl, fun x y _ _ => range64_max_sound (x + y)⟩
  · simp only [hov, Bool.false_eq_true, ↓reduceIte]
    refine ⟨_, rfl, fun x y hx hy => ?_⟩
    unfold ConcretizesRange64 at *
    simp only [u64_mk_uscalar_bv] at *
    revert hov hx hy; bv_decide

theorem range32_add_ok_and_sound (r₁ r₂ : U32Range) :
    ∃ r, U32Range.add_range r₁ r₂ = ok r ∧
      ∀ x y : BitVec 32, ConcretizesRange32 r₁ x → ConcretizesRange32 r₂ y → ConcretizesRange32 r (x + y) := by
  rcases r₁ with ⟨⟨min1⟩, ⟨max1⟩⟩
  rcases r₂ with ⟨⟨min2⟩, ⟨max2⟩⟩
  change BitVec 32 at min1 max1 min2 max2
  unfold U32Range.add_range U32Range.impl.max U32Range.new core.num.U32.overflowing_add UScalar.overflowing_add lift
  rw [bind_pair_ok_eq rfl, bind_pair_ok_eq rfl]
  dsimp only [u32_mk_uscalar_bv]
  by_cases hov : (min1.uaddOverflow min2 != max1.uaddOverflow max2) = true
  · simp only [hov, ↓reduceIte]
    exact ⟨_, rfl, fun x y _ _ => range32_max_sound (x + y)⟩
  · simp only [hov, Bool.false_eq_true, ↓reduceIte]
    refine ⟨_, rfl, fun x y hx hy => ?_⟩
    unfold ConcretizesRange32 at *
    simp only [u32_mk_uscalar_bv] at *
    revert hov hx hy; bv_decide

theorem range64_sub_ok_and_sound (r₁ r₂ : U64Range) :
    ∃ r, U64Range.sub_range r₁ r₂ = ok r ∧
      ∀ x y : BitVec 64, ConcretizesRange64 r₁ x → ConcretizesRange64 r₂ y → ConcretizesRange64 r (x - y) := by
  rcases r₁ with ⟨⟨min1⟩, ⟨max1⟩⟩
  rcases r₂ with ⟨⟨min2⟩, ⟨max2⟩⟩
  change BitVec 64 at min1 max1 min2 max2
  unfold U64Range.sub_range U64Range.impl.max U64Range.new core.num.U64.overflowing_sub UScalar.overflowing_sub lift
  rw [bind_pair_ok_eq rfl, bind_pair_ok_eq rfl]
  dsimp only [u64_mk_uscalar_bv]
  by_cases hov : (BitVec.usubOverflow min1 max2 != BitVec.usubOverflow max1 min2) = true
  · simp only [hov, ↓reduceIte]
    exact ⟨_, rfl, fun x y _ _ => range64_max_sound (x - y)⟩
  · simp only [hov, Bool.false_eq_true, ↓reduceIte]
    refine ⟨_, rfl, fun x y hx hy => ?_⟩
    unfold ConcretizesRange64 at *
    change (min1 - max2 : BitVec 64) ≤ x - y ∧ x - y ≤ (max1 - min2 : BitVec 64)
    simp only [u64_mk_uscalar_bv] at *
    revert hov hx hy; bv_decide

theorem range32_sub_ok_and_sound (r₁ r₂ : U32Range) :
    ∃ r, U32Range.sub_range r₁ r₂ = ok r ∧
      ∀ x y : BitVec 32, ConcretizesRange32 r₁ x → ConcretizesRange32 r₂ y → ConcretizesRange32 r (x - y) := by
  rcases r₁ with ⟨⟨min1⟩, ⟨max1⟩⟩
  rcases r₂ with ⟨⟨min2⟩, ⟨max2⟩⟩
  change BitVec 32 at min1 max1 min2 max2
  unfold U32Range.sub_range U32Range.impl.max U32Range.new core.num.U32.overflowing_sub UScalar.overflowing_sub lift
  rw [bind_pair_ok_eq rfl, bind_pair_ok_eq rfl]
  dsimp only [u32_mk_uscalar_bv]
  by_cases hov : (BitVec.usubOverflow min1 max2 != BitVec.usubOverflow max1 min2) = true
  · simp only [hov, ↓reduceIte]
    exact ⟨_, rfl, fun x y _ _ => range32_max_sound (x - y)⟩
  · simp only [hov, Bool.false_eq_true, ↓reduceIte]
    refine ⟨_, rfl, fun x y hx hy => ?_⟩
    unfold ConcretizesRange32 at *
    change (min1 - max2 : BitVec 32) ≤ x - y ∧ x - y ≤ (max1 - min2 : BitVec 32)
    simp only [u32_mk_uscalar_bv] at *
    revert hov hx hy; bv_decide

theorem range64_mul_ok_and_sound (r₁ r₂ : U64Range) :
    ∃ r, U64Range.mul_range r₁ r₂ = ok r ∧
      ∀ x y : BitVec 64, ConcretizesRange64 r₁ x → ConcretizesRange64 r₂ y → ConcretizesRange64 r (x * y) := by
  unfold U64Range.mul_range U64Range.impl.max U64Range.new lift
  rw [bind_ok_eq rfl]
  have hs1 := U64.checked_mul_bv_spec r₁.min r₂.min
  cases h1 : U64.checked_mul r₁.min r₂.min with
  | none =>
    exact ⟨_, rfl, fun x y _ _ => range64_max_sound (x * y)⟩
  | some min =>
    rw [h1] at hs1; dsimp only
    rw [bind_ok_eq rfl]
    have hs2 := U64.checked_mul_bv_spec r₁.max r₂.max
    cases h2 : U64.checked_mul r₁.max r₂.max with
    | none =>
      exact ⟨_, rfl, fun x y _ _ => range64_max_sound (x * y)⟩
    | some max =>
      rw [h2] at hs2
      refine ⟨{ min := min, max := max }, rfl, fun x y hx hy => ?_⟩
      rcases hx with ⟨hx1, hx2⟩; rcases hy with ⟨hy1, hy2⟩
      rw [BitVec.le_def] at hx1 hx2 hy1 hy2
      have hu64max : U64.max = 2 ^ 64 - 1 := by simp [U64.max, U64.numBits]
      have hmin_lt : r₁.min.bv.toNat * r₂.min.bv.toNat < 2 ^ 64 := by
        have hle := hs1.1
        simp only [← UScalar.bv_toNat, hu64max] at hle
        exact Nat.lt_of_le_of_lt hle (by decide)
      have hmax_lt : r₁.max.bv.toNat * r₂.max.bv.toNat < 2 ^ 64 := by
        have hle := hs2.1
        simp only [← UScalar.bv_toNat, hu64max] at hle
        exact Nat.lt_of_le_of_lt hle (by decide)
      have hxy_ge : r₁.min.bv.toNat * r₂.min.bv.toNat ≤ x.toNat * y.toNat := Nat.mul_le_mul hx1 hy1
      have hxy_le : x.toNat * y.toNat ≤ r₁.max.bv.toNat * r₂.max.bv.toNat := Nat.mul_le_mul hx2 hy2
      have hxy_lt : x.toNat * y.toNat < 2 ^ 64 := Nat.lt_of_le_of_lt hxy_le hmax_lt
      unfold ConcretizesRange64
      dsimp only
      rw [hs1.2.2, hs2.2.2]
      simp only [BitVec.le_def, BitVec.toNat_mul, Nat.mod_eq_of_lt hmin_lt,
        Nat.mod_eq_of_lt hmax_lt, Nat.mod_eq_of_lt hxy_lt]
      exact ⟨hxy_ge, hxy_le⟩

theorem range32_mul_ok_and_sound (r₁ r₂ : U32Range) :
    ∃ r, U32Range.mul_range r₁ r₂ = ok r ∧
      ∀ x y : BitVec 32, ConcretizesRange32 r₁ x → ConcretizesRange32 r₂ y → ConcretizesRange32 r (x * y) := by
  unfold U32Range.mul_range U32Range.impl.max U32Range.new lift
  rw [bind_ok_eq rfl]
  have hs1 := U32.checked_mul_bv_spec r₁.min r₂.min
  cases h1 : U32.checked_mul r₁.min r₂.min with
  | none =>
    exact ⟨_, rfl, fun x y _ _ => range32_max_sound (x * y)⟩
  | some min =>
    rw [h1] at hs1; dsimp only
    rw [bind_ok_eq rfl]
    have hs2 := U32.checked_mul_bv_spec r₁.max r₂.max
    cases h2 : U32.checked_mul r₁.max r₂.max with
    | none =>
      exact ⟨_, rfl, fun x y _ _ => range32_max_sound (x * y)⟩
    | some max =>
      rw [h2] at hs2
      refine ⟨{ min := min, max := max }, rfl, fun x y hx hy => ?_⟩
      rcases hx with ⟨hx1, hx2⟩; rcases hy with ⟨hy1, hy2⟩
      rw [BitVec.le_def] at hx1 hx2 hy1 hy2
      have hu32max : U32.max = 2 ^ 32 - 1 := by simp [U32.max, U32.numBits]
      have hmin_lt : r₁.min.bv.toNat * r₂.min.bv.toNat < 2 ^ 32 := by
        have hle := hs1.1
        simp only [← UScalar.bv_toNat, hu32max] at hle
        exact Nat.lt_of_le_of_lt hle (by decide)
      have hmax_lt : r₁.max.bv.toNat * r₂.max.bv.toNat < 2 ^ 32 := by
        have hle := hs2.1
        simp only [← UScalar.bv_toNat, hu32max] at hle
        exact Nat.lt_of_le_of_lt hle (by decide)
      have hxy_ge : r₁.min.bv.toNat * r₂.min.bv.toNat ≤ x.toNat * y.toNat := Nat.mul_le_mul hx1 hy1
      have hxy_le : x.toNat * y.toNat ≤ r₁.max.bv.toNat * r₂.max.bv.toNat := Nat.mul_le_mul hx2 hy2
      have hxy_lt : x.toNat * y.toNat < 2 ^ 32 := Nat.lt_of_le_of_lt hxy_le hmax_lt
      unfold ConcretizesRange32
      dsimp only
      rw [hs1.2.2, hs2.2.2]
      simp only [BitVec.le_def, BitVec.toNat_mul, Nat.mod_eq_of_lt hmin_lt,
        Nat.mod_eq_of_lt hmax_lt, Nat.mod_eq_of_lt hxy_lt]
      exact ⟨hxy_ge, hxy_le⟩

theorem u64_div_eq_ok (a b : Std.U64) (hb : b ≠ 0#u64) :
    (a / b : Result Std.U64) = ok ⟨a.bv / b.bv⟩ := by
  have hb0 : (b.bv != 0) = true := bne_iff_ne.mpr (fun h => hb ((u64_eq_iff_bv _ _).2 h))
  change UScalar.div a b = _
  simp only [UScalar.div, hb0, ↓reduceIte]
  rfl

theorem u32_div_eq_ok (a b : Std.U32) (hb : b ≠ 0#u32) :
    (a / b : Result Std.U32) = ok ⟨a.bv / b.bv⟩ := by
  have hb0 : (b.bv != 0) = true := bne_iff_ne.mpr (fun h => hb ((u32_eq_iff_bv _ _).2 h))
  change UScalar.div a b = _
  simp only [UScalar.div, hb0, ↓reduceIte]
  rfl

theorem u64_rem_eq_ok (a b : Std.U64) (hb : b ≠ 0#u64) :
    (a % b : Result Std.U64) = ok ⟨a.bv % b.bv⟩ := by
  have hb0 : (b.val != 0) = true := by
    apply bne_iff_ne.mpr
    intro h
    apply hb
    rw [← UScalar.bv_toNat] at h
    exact (u64_eq_iff_bv _ _).2 (BitVec.eq_of_toNat_eq h)
  change UScalar.rem a b = _
  unfold UScalar.rem
  rw [hb0]
  simp only [↓reduceIte, BitVec.umod_eq]

theorem u32_rem_eq_ok (a b : Std.U32) (hb : b ≠ 0#u32) :
    (a % b : Result Std.U32) = ok ⟨a.bv % b.bv⟩ := by
  have hb0 : (b.val != 0) = true := by
    apply bne_iff_ne.mpr
    intro h
    apply hb
    rw [← UScalar.bv_toNat] at h
    exact (u32_eq_iff_bv _ _).2 (BitVec.eq_of_toNat_eq h)
  change UScalar.rem a b = _
  unfold UScalar.rem
  rw [hb0]
  simp only [↓reduceIte, BitVec.umod_eq]

theorem range64_div_ok_and_sound (r₁ r₂ : U64Range) :
    ∃ r, U64Range.div_range r₁ r₂ = ok r ∧
      ∀ x y : BitVec 64, ConcretizesRange64 r₁ x → ConcretizesRange64 r₂ y →
        ConcretizesRange64 r (if y == 0#64 then 0#64 else x / y) := by
  unfold U64Range.div_range U64Range.new
  by_cases hrmax0 : r₂.max = 0#u64
  · simp only [hrmax0, ↓reduceIte]
    refine ⟨_, rfl, fun x y _ hy => ?_⟩
    have hm0 : r₂.max.bv = 0#64 := (u64_eq_iff_bv _ _).1 hrmax0
    have hy0 : y == 0#64 := by
      have := hy.2; rw [hm0] at this; revert this; bv_decide
    simp [hy0, ConcretizesRange64]
  · simp only [hrmax0, ↓reduceIte]
    have hdmin_ne : ∀ b : BitVec 64,
        (⟨if b ≤ (1#u64).bv then (1#u64).bv else b⟩ : Std.U64) ≠ 0#u64 := by
      intro b h; have hbv := (u64_eq_iff_bv _ _).1 h
      change (if b ≤ 1#64 then 1#64 else b) = 0#64 at hbv
      revert hbv; bv_decide
    by_cases hrmin0 : r₂.min = 0#u64
    · simp only [hrmin0, ↓reduceIte]
      rw [bind_pair_ok_eq rfl, bind_ok_eq (max_u64_eq_ok 1#u64 0#u64),
          bind_ok_eq (u64_div_eq_ok r₁.max _ (hdmin_ne _))]
      refine ⟨_, rfl, fun x y hx _ => ?_⟩
      unfold ConcretizesRange64 at *
      simp only [u64_mk_uscalar_bv, u64_zero_uscalar_bv, u64_one_uscalar_bv]
      have hx2 := hx.2
      revert hx2; bv_decide
    · simp only [hrmin0, ↓reduceIte]
      rw [bind_ok_eq (u64_div_eq_ok r₁.min r₂.max hrmax0), bind_pair_ok_eq rfl,
          bind_ok_eq (max_u64_eq_ok 1#u64 r₂.min),
          bind_ok_eq (u64_div_eq_ok r₁.max _ (hdmin_ne _))]
      refine ⟨_, rfl, fun x y hx hy => ?_⟩
      have hm_ne : r₂.min.bv ≠ 0#64 := fun h => hrmin0 ((u64_eq_iff_bv _ _).2 h)
      have hm_pos : 0 < r₂.min.bv.toNat := by
        apply Nat.pos_of_ne_zero; intro h; exact hm_ne (BitVec.eq_of_toNat_eq h)
      have hx1 := (BitVec.le_def).mp hx.1
      have hx2 := (BitVec.le_def).mp hx.2
      have hy1 := (BitVec.le_def).mp hy.1
      have hy2 := (BitVec.le_def).mp hy.2
      have hy_pos : 0 < y.toNat := Nat.lt_of_lt_of_le hm_pos hy1
      have hy0 : (y == 0#64) = false := by
        apply Bool.eq_false_iff.mpr; intro h; have := eq_of_beq h; subst this; simp at hy_pos
      have hd_eq : (if r₂.min.bv ≤ 1#64 then 1#64 else r₂.min.bv) = r₂.min.bv := by
        revert hm_ne; bv_decide
      unfold ConcretizesRange64
      simp only [u64_mk_uscalar_bv, u64_one_uscalar_bv]
      rw [hd_eq]
      simp only [hy0, Bool.false_eq_true, ↓reduceIte, BitVec.le_def, BitVec.toNat_udiv]
      exact ⟨Nat.le_trans (Nat.div_le_div_left hy2 hy_pos) (Nat.div_le_div_right hx1),
             Nat.le_trans (Nat.div_le_div_right hx2) (Nat.div_le_div_left hy1 hm_pos)⟩

theorem range32_div_ok_and_sound (r₁ r₂ : U32Range) :
    ∃ r, U32Range.div_range r₁ r₂ = ok r ∧
      ∀ x y : BitVec 32, ConcretizesRange32 r₁ x → ConcretizesRange32 r₂ y →
        ConcretizesRange32 r (if y == 0#32 then 0#32 else x / y) := by
  unfold U32Range.div_range U32Range.new
  by_cases hrmax0 : r₂.max = 0#u32
  · simp only [hrmax0, ↓reduceIte]
    refine ⟨_, rfl, fun x y _ hy => ?_⟩
    have hm0 : r₂.max.bv = 0#32 := (u32_eq_iff_bv _ _).1 hrmax0
    have hy0 : y == 0#32 := by
      have := hy.2; rw [hm0] at this; revert this; bv_decide
    simp [hy0, ConcretizesRange32]
  · simp only [hrmax0, ↓reduceIte]
    have hdmin_ne : ∀ b : BitVec 32,
        (⟨if b ≤ (1#u32).bv then (1#u32).bv else b⟩ : Std.U32) ≠ 0#u32 := by
      intro b h; have hbv := (u32_eq_iff_bv _ _).1 h
      change (if b ≤ 1#32 then 1#32 else b) = 0#32 at hbv
      revert hbv; bv_decide
    by_cases hrmin0 : r₂.min = 0#u32
    · simp only [hrmin0, ↓reduceIte]
      rw [bind_pair_ok_eq rfl, bind_ok_eq (max_u32_eq_ok 1#u32 0#u32),
          bind_ok_eq (u32_div_eq_ok r₁.max _ (hdmin_ne _))]
      refine ⟨_, rfl, fun x y hx _ => ?_⟩
      unfold ConcretizesRange32 at *
      simp only [u32_mk_uscalar_bv, u32_zero_uscalar_bv, u32_one_uscalar_bv]
      have hx2 := hx.2
      revert hx2; bv_decide
    · simp only [hrmin0, ↓reduceIte]
      rw [bind_ok_eq (u32_div_eq_ok r₁.min r₂.max hrmax0), bind_pair_ok_eq rfl,
          bind_ok_eq (max_u32_eq_ok 1#u32 r₂.min),
          bind_ok_eq (u32_div_eq_ok r₁.max _ (hdmin_ne _))]
      refine ⟨_, rfl, fun x y hx hy => ?_⟩
      have hm_ne : r₂.min.bv ≠ 0#32 := fun h => hrmin0 ((u32_eq_iff_bv _ _).2 h)
      have hm_pos : 0 < r₂.min.bv.toNat := by
        apply Nat.pos_of_ne_zero; intro h; exact hm_ne (BitVec.eq_of_toNat_eq h)
      have hx1 := (BitVec.le_def).mp hx.1
      have hx2 := (BitVec.le_def).mp hx.2
      have hy1 := (BitVec.le_def).mp hy.1
      have hy2 := (BitVec.le_def).mp hy.2
      have hy_pos : 0 < y.toNat := Nat.lt_of_lt_of_le hm_pos hy1
      have hy0 : (y == 0#32) = false := by
        apply Bool.eq_false_iff.mpr; intro h; have := eq_of_beq h; subst this; simp at hy_pos
      have hd_eq : (if r₂.min.bv ≤ 1#32 then 1#32 else r₂.min.bv) = r₂.min.bv := by
        revert hm_ne; bv_decide
      unfold ConcretizesRange32
      simp only [u32_mk_uscalar_bv, u32_one_uscalar_bv]
      rw [hd_eq]
      simp only [hy0, Bool.false_eq_true, ↓reduceIte, BitVec.le_def, BitVec.toNat_udiv]
      exact ⟨Nat.le_trans (Nat.div_le_div_left hy2 hy_pos) (Nat.div_le_div_right hx1),
             Nat.le_trans (Nat.div_le_div_right hx2) (Nat.div_le_div_left hy1 hm_pos)⟩

private theorem u64_sub_one_eq_ok (a : Std.U64) (ha : a ≠ 0#u64) :
    (a - 1#u64 : Result Std.U64) = ok ⟨a.bv - 1#64⟩ := by
  apply uscalar_sub_eq_ok
  have hbv : a.bv ≠ 0#64 := fun h => ha ((u64_eq_iff_bv _ _).2 h)
  have hle : (1#u64).bv ≤ a.bv := by
    change 1#64 ≤ a.bv; revert hbv; bv_decide
  simp only [← UScalar.bv_toNat]
  exact (BitVec.le_def).mp hle

private theorem u32_sub_one_eq_ok (a : Std.U32) (ha : a ≠ 0#u32) :
    (a - 1#u32 : Result Std.U32) = ok ⟨a.bv - 1#32⟩ := by
  apply uscalar_sub_eq_ok
  have hbv : a.bv ≠ 0#32 := fun h => ha ((u32_eq_iff_bv _ _).2 h)
  have hle : (1#u32).bv ≤ a.bv := by
    change 1#32 ≤ a.bv; revert hbv; bv_decide
  simp only [← UScalar.bv_toNat]
  exact (BitVec.le_def).mp hle

theorem range64_rem_ok_and_sound (r₁ r₂ : U64Range) :
    ∃ r, U64Range.rem_range r₁ r₂ = ok r ∧
      ∀ x y : BitVec 64, ConcretizesRange64 r₁ x → ConcretizesRange64 r₂ y →
        ConcretizesRange64 r (if y == 0#64 then x else x % y) := by
  unfold U64Range.rem_range
  by_cases hrmax0 : r₂.max = 0#u64
  · simp only [hrmax0, ↓reduceIte]
    refine ⟨_, rfl, fun x y hx hy => ?_⟩
    have hm0 : r₂.max.bv = 0#64 := (u64_eq_iff_bv _ _).1 hrmax0
    have hy0 : y == 0#64 := by have := hy.2; rw [hm0] at this; revert this; bv_decide
    simpa [hy0] using hx
  · simp only [hrmax0, ↓reduceIte]
    by_cases hrmin0 : r₂.min = 0#u64
    · simp only [hrmin0, ↓reduceIte]
      refine ⟨_, rfl, fun x y hx _ => ?_⟩
      unfold ConcretizesRange64 at *; dsimp only [u64_zero_uscalar_bv]
      by_cases hy0 : y == 0#64
      · simp only [hy0, ↓reduceIte, BitVec.le_def, BitVec.toNat_ofNat]
        exact ⟨Nat.zero_le _, (BitVec.le_def).mp hx.2⟩
      · simp only [hy0, Bool.false_eq_true, ↓reduceIte, BitVec.le_def, BitVec.toNat_ofNat, BitVec.toNat_umod]
        exact ⟨Nat.zero_le _, Nat.le_trans (Nat.mod_le _ _) ((BitVec.le_def).mp hx.2)⟩
    · simp only [hrmin0, ↓reduceIte]
      rw [bind_ok_eq (u64_sub_one_eq_ok r₂.max hrmax0), bind_ok_eq (min_u64_eq_ok r₁.max _)]
      refine ⟨_, rfl, fun x y hx hy => ?_⟩
      have hm_ne : r₂.min.bv ≠ 0#64 := fun h => hrmin0 ((u64_eq_iff_bv _ _).2 h)
      have hm_pos : 0 < r₂.min.bv.toNat := Nat.pos_of_ne_zero (fun h => hm_ne (BitVec.eq_of_toNat_eq h))
      have hx2 := (BitVec.le_def).mp hx.2
      have hy1 := (BitVec.le_def).mp hy.1
      have hy2 := (BitVec.le_def).mp hy.2
      have hy_pos : 0 < y.toNat := Nat.lt_of_lt_of_le hm_pos hy1
      have hy0 : (y == 0#64) = false := by
        apply Bool.eq_false_iff.mpr; intro h; have := eq_of_beq h; subst this; simp at hy_pos
      have hrmax_pos : 1 ≤ r₂.max.bv.toNat := by omega
      have hbm1 : (r₂.max.bv - 1#64).toNat = r₂.max.bv.toNat - 1 := by
        rw [BitVec.toNat_sub_of_le]; · rfl
        · rw [BitVec.le_def]; exact hrmax_pos
      have hmod_x : x.toNat % y.toNat ≤ r₁.max.bv.toNat := Nat.le_trans (Nat.mod_le _ _) hx2
      have hmod_y : x.toNat % y.toNat < y.toNat := Nat.mod_lt _ hy_pos
      have hmod_bm1 : x.toNat % y.toNat ≤ (r₂.max.bv - 1#64).toNat := by omega
      unfold ConcretizesRange64; dsimp only [u64_mk_uscalar_bv, u64_zero_uscalar_bv]
      simp only [hy0, Bool.false_eq_true, ↓reduceIte]
      split_ifs <;> simp only [BitVec.le_def, BitVec.toNat_ofNat, BitVec.toNat_umod]
      · exact ⟨Nat.zero_le _, hmod_x⟩
      · exact ⟨Nat.zero_le _, hmod_bm1⟩

theorem range32_rem_ok_and_sound (r₁ r₂ : U32Range) :
    ∃ r, U32Range.rem_range r₁ r₂ = ok r ∧
      ∀ x y : BitVec 32, ConcretizesRange32 r₁ x → ConcretizesRange32 r₂ y →
        ConcretizesRange32 r (if y == 0#32 then x else x % y) := by
  unfold U32Range.rem_range
  by_cases hrmax0 : r₂.max = 0#u32
  · simp only [hrmax0, ↓reduceIte]
    refine ⟨_, rfl, fun x y hx hy => ?_⟩
    have hm0 : r₂.max.bv = 0#32 := (u32_eq_iff_bv _ _).1 hrmax0
    have hy0 : y == 0#32 := by have := hy.2; rw [hm0] at this; revert this; bv_decide
    simpa [hy0] using hx
  · simp only [hrmax0, ↓reduceIte]
    by_cases hrmin0 : r₂.min = 0#u32
    · simp only [hrmin0, ↓reduceIte]
      refine ⟨_, rfl, fun x y hx _ => ?_⟩
      unfold ConcretizesRange32 at *; dsimp only [u32_zero_uscalar_bv]
      by_cases hy0 : y == 0#32
      · simp only [hy0, ↓reduceIte, BitVec.le_def, BitVec.toNat_ofNat]
        exact ⟨Nat.zero_le _, (BitVec.le_def).mp hx.2⟩
      · simp only [hy0, Bool.false_eq_true, ↓reduceIte, BitVec.le_def, BitVec.toNat_ofNat, BitVec.toNat_umod]
        exact ⟨Nat.zero_le _, Nat.le_trans (Nat.mod_le _ _) ((BitVec.le_def).mp hx.2)⟩
    · simp only [hrmin0, ↓reduceIte]
      rw [bind_ok_eq (u32_sub_one_eq_ok r₂.max hrmax0), bind_ok_eq (min_u32_eq_ok r₁.max _)]
      refine ⟨_, rfl, fun x y hx hy => ?_⟩
      have hm_ne : r₂.min.bv ≠ 0#32 := fun h => hrmin0 ((u32_eq_iff_bv _ _).2 h)
      have hm_pos : 0 < r₂.min.bv.toNat := Nat.pos_of_ne_zero (fun h => hm_ne (BitVec.eq_of_toNat_eq h))
      have hx2 := (BitVec.le_def).mp hx.2
      have hy1 := (BitVec.le_def).mp hy.1
      have hy2 := (BitVec.le_def).mp hy.2
      have hy_pos : 0 < y.toNat := Nat.lt_of_lt_of_le hm_pos hy1
      have hy0 : (y == 0#32) = false := by
        apply Bool.eq_false_iff.mpr; intro h; have := eq_of_beq h; subst this; simp at hy_pos
      have hrmax_pos : 1 ≤ r₂.max.bv.toNat := by omega
      have hbm1 : (r₂.max.bv - 1#32).toNat = r₂.max.bv.toNat - 1 := by
        rw [BitVec.toNat_sub_of_le]; · rfl
        · rw [BitVec.le_def]; exact hrmax_pos
      have hmod_x : x.toNat % y.toNat ≤ r₁.max.bv.toNat := Nat.le_trans (Nat.mod_le _ _) hx2
      have hmod_y : x.toNat % y.toNat < y.toNat := Nat.mod_lt _ hy_pos
      have hmod_bm1 : x.toNat % y.toNat ≤ (r₂.max.bv - 1#32).toNat := by omega
      unfold ConcretizesRange32; dsimp only [u32_mk_uscalar_bv, u32_zero_uscalar_bv]
      simp only [hy0, Bool.false_eq_true, ↓reduceIte]
      split_ifs <;> simp only [BitVec.le_def, BitVec.toNat_ofNat, BitVec.toNat_umod]
      · exact ⟨Nat.zero_le _, hmod_x⟩
      · exact ⟨Nat.zero_le _, hmod_bm1⟩

theorem range64_neg_ok_and_sound (r₁ : U64Range) :
    ∃ r, U64Range.neg_range r₁ = ok r ∧
      ∀ x : BitVec 64, ConcretizesRange64 r₁ x → ConcretizesRange64 r (-x) := by
  rcases r₁ with ⟨⟨min1⟩, ⟨max1⟩⟩
  change BitVec 64 at min1 max1
  unfold U64Range.neg_range U64Range.impl.max U64Range.new lift
  by_cases hmax0 : (⟨max1⟩ : Std.U64) = 0#u64
  · simp only [hmax0, ↓reduceIte]
    have hm0 : max1 = 0#64 := (u64_eq_iff_bv _ _).1 hmax0
    clear hmax0
    refine ⟨_, rfl, fun x hx => ?_⟩
    unfold ConcretizesRange64 at *; simp only [u64_mk_uscalar_bv, u64_zero_uscalar_bv] at *
    revert hm0 hx; bv_decide
  · simp only [hmax0, ↓reduceIte]
    by_cases hmin0 : (⟨min1⟩ : Std.U64) = 0#u64
    · simp only [hmin0, ↓reduceIte]
      exact ⟨_, rfl, fun x _ => range64_max_sound (-x)⟩
    · simp only [hmin0, ↓reduceIte, bind_ok, u64_wrapping_sub_eq, u64_zero_uscalar_bv, u64_mk_uscalar_bv]
      have hmn0 : min1 ≠ 0#64 := fun h => hmin0 ((u64_eq_iff_bv _ _).2 h)
      clear hmax0 hmin0
      refine ⟨_, rfl, fun x hx => ?_⟩
      unfold ConcretizesRange64 at *; simp only [u64_mk_uscalar_bv] at *
      revert hmn0 hx; bv_decide

theorem range32_neg_ok_and_sound (r₁ : U32Range) :
    ∃ r, U32Range.neg_range r₁ = ok r ∧
      ∀ x : BitVec 32, ConcretizesRange32 r₁ x → ConcretizesRange32 r (-x) := by
  rcases r₁ with ⟨⟨min1⟩, ⟨max1⟩⟩
  change BitVec 32 at min1 max1
  unfold U32Range.neg_range U32Range.impl.max U32Range.new lift
  by_cases hmax0 : (⟨max1⟩ : Std.U32) = 0#u32
  · simp only [hmax0, ↓reduceIte]
    have hm0 : max1 = 0#32 := (u32_eq_iff_bv _ _).1 hmax0
    clear hmax0
    refine ⟨_, rfl, fun x hx => ?_⟩
    unfold ConcretizesRange32 at *; simp only [u32_mk_uscalar_bv, u32_zero_uscalar_bv] at *
    revert hm0 hx; bv_decide
  · simp only [hmax0, ↓reduceIte]
    by_cases hmin0 : (⟨min1⟩ : Std.U32) = 0#u32
    · simp only [hmin0, ↓reduceIte]
      exact ⟨_, rfl, fun x _ => range32_max_sound (-x)⟩
    · simp only [hmin0, ↓reduceIte, bind_ok, u32_wrapping_sub_eq, u32_zero_uscalar_bv, u32_mk_uscalar_bv]
      have hmn0 : min1 ≠ 0#32 := fun h => hmin0 ((u32_eq_iff_bv _ _).2 h)
      clear hmax0 hmin0
      refine ⟨_, rfl, fun x hx => ?_⟩
      unfold ConcretizesRange32 at *; simp only [u32_mk_uscalar_bv] at *
      revert hmn0 hx; bv_decide

theorem range64_shl_ok_and_sound (r₁ r₂ : U64Range) :
    ∃ r, U64Range.shl_range r₁ r₂ = ok r ∧
      ∀ x y : BitVec 64, ConcretizesRange64 r₁ x → ConcretizesRange64 r₂ y →
        ConcretizesRange64 r (x <<< y.setWidth 6) := by
  rcases r₁ with ⟨⟨min1⟩, ⟨max1⟩⟩
  rcases r₂ with ⟨⟨min2⟩, ⟨max2⟩⟩
  change BitVec 64 at min1 max1 min2 max2
  unfold U64Range.shl_range U64Range.impl.max U64Range.new
  simp only [lift, bind_ok]
  have hu32max : (UScalar.cast .U64 core.num.U32.MAX) = 0xFFFF_FFFF#u64 := by decide
  rw [hu32max]
  by_cases h1 : (⟨max2⟩ : Std.U64) > 0xFFFF_FFFF#u64
  · simp only [h1, ↓reduceIte]
    exact ⟨_, rfl, fun _ _ _ _ => range64_max_sound _⟩
  · simp only [h1, ↓reduceIte]
    by_cases h2 : (⟨min2⟩ : Std.U64) > 0xFFFF_FFFF#u64
    · simp only [h2, ↓reduceIte]
      exact ⟨_, rfl, fun _ _ _ _ => range64_max_sound _⟩
    · simp only [h2, ↓reduceIte]
      by_cases hmax0 : (⟨max1⟩ : Std.U64) = 0#u64
      · simp only [hmax0, ↓reduceIte]
        have hm0 : max1 = 0#64 := (u64_eq_iff_bv _ _).1 hmax0
        clear h1 h2 hmax0
        refine ⟨_, rfl, fun x _ hx _ => ?_⟩
        unfold ConcretizesRange64 at *; simp only [u64_mk_uscalar_bv, u64_zero_uscalar_bv] at *
        revert hm0 hx; bv_decide
      · simp only [hmax0, ↓reduceIte]
        have hm0 : max1 ≠ 0#64 := fun h => hmax0 ((u64_eq_iff_bv _ _).2 h)
        by_cases hclz : UScalar.cast .U32 (⟨max2⟩ : Std.U64) > core.num.U64.leading_zeros ⟨max1⟩
        · simp only [hclz, ↓reduceIte]
          exact ⟨_, rfl, fun _ _ _ _ => range64_max_sound _⟩
        · simp only [hclz, ↓reduceIte]
          rw [u64_wrapping_shl_eq, u64_wrapping_shl_eq]
          refine ⟨_, rfl, fun x y hx hy => ?_⟩
          have hb1 : ¬(0xFFFF_FFFF#64 < max2) := fun h => h1 ((u64_gt_iff_bv _ _).2 h)
          have hb2 : ¬(0xFFFF_FFFF#64 < min2) := fun h => h2 ((u64_gt_iff_bv _ _).2 h)
          have hle_clz : (max2.setWidth 32) ≤ (max1.clz.setWidth 32) := by
            have hnotlt : ¬((core.num.U64.leading_zeros ⟨max1⟩).bv < (UScalar.cast .U32 (⟨max2⟩ : Std.U64)).bv) :=
              fun h => hclz ((u32_lt_iff_bv _ _).2 h)
            have hclz_eq : (core.num.U64.leading_zeros ⟨max1⟩).bv = max1.clz.setWidth 32 :=
              leadingZeros64_eq_clz max1
            rw [hclz_eq] at hnotlt
            change ¬(max1.clz.setWidth 32 < max2.setWidth 32) at hnotlt
            clear h1 h2 hmax0 hclz
            revert hnotlt; bv_decide
          have hc1 : (UScalar.cast .U32 (⟨min2⟩ : Std.U64)).bv.setWidth 6 = min2.setWidth 6 := by
            change (BitVec.setWidth 32 min2).setWidth 6 = min2.setWidth 6; bv_decide
          have hc2 : (UScalar.cast .U32 (⟨max2⟩ : Std.U64)).bv.setWidth 6 = max2.setWidth 6 := by
            change (BitVec.setWidth 32 max2).setWidth 6 = max2.setWidth 6; bv_decide
          unfold ConcretizesRange64 at *; simp only [u64_mk_uscalar_bv] at *
          rw [hc1, hc2]
          clear h1 h2 hmax0 hclz
          revert hb1 hb2 hm0 hle_clz hx hy; bv_decide

theorem range32_shl_ok_and_sound (r₁ r₂ : U32Range) :
    ∃ r, U32Range.shl_range r₁ r₂ = ok r ∧
      ∀ x y : BitVec 32, ConcretizesRange32 r₁ x → ConcretizesRange32 r₂ y →
        ConcretizesRange32 r (x <<< y.setWidth 5) := by
  rcases r₁ with ⟨⟨min1⟩, ⟨max1⟩⟩
  rcases r₂ with ⟨⟨min2⟩, ⟨max2⟩⟩
  change BitVec 32 at min1 max1 min2 max2
  unfold U32Range.shl_range U32Range.impl.max U32Range.new
  simp only [lift, bind_ok]
  by_cases h1 : (⟨max2⟩ : Std.U32) > core.num.U32.MAX
  · simp only [h1, ↓reduceIte]
    exact ⟨_, rfl, fun _ _ _ _ => range32_max_sound _⟩
  · simp only [h1, ↓reduceIte]
    by_cases h2 : (⟨min2⟩ : Std.U32) > core.num.U32.MAX
    · simp only [h2, ↓reduceIte]
      exact ⟨_, rfl, fun _ _ _ _ => range32_max_sound _⟩
    · simp only [h2, ↓reduceIte]
      by_cases hmax0 : (⟨max1⟩ : Std.U32) = 0#u32
      · simp only [hmax0, ↓reduceIte]
        have hm0 : max1 = 0#32 := (u32_eq_iff_bv _ _).1 hmax0
        clear h1 h2 hmax0
        refine ⟨_, rfl, fun x _ hx _ => ?_⟩
        unfold ConcretizesRange32 at *; simp only [u32_mk_uscalar_bv, u32_zero_uscalar_bv] at *
        revert hm0 hx; bv_decide
      · simp only [hmax0, ↓reduceIte]
        have hm0 : max1 ≠ 0#32 := fun h => hmax0 ((u32_eq_iff_bv _ _).2 h)
        by_cases hclz : (⟨max2⟩ : Std.U32) > core.num.U32.leading_zeros ⟨max1⟩
        · simp only [hclz, ↓reduceIte]
          exact ⟨_, rfl, fun _ _ _ _ => range32_max_sound _⟩
        · simp only [hclz, ↓reduceIte]
          rw [u32_wrapping_shl_eq, u32_wrapping_shl_eq]
          refine ⟨_, rfl, fun x y hx hy => ?_⟩
          have hle_clz : max2 ≤ max1.clz := by
            have hnotlt : ¬((core.num.U32.leading_zeros ⟨max1⟩).bv < max2) :=
              fun h => hclz ((u32_lt_iff_bv _ _).2 h)
            rw [leadingZeros32_eq_clz max1] at hnotlt
            clear h1 h2 hmax0 hclz
            revert hnotlt; bv_decide
          unfold ConcretizesRange32 at *; simp only [u32_mk_uscalar_bv] at *
          clear h1 h2 hmax0 hclz
          revert hm0 hle_clz hx hy; bv_decide

theorem range64_shr_ok_and_sound (r₁ r₂ : U64Range) :
    ∃ r, U64Range.shr_range r₁ r₂ = ok r ∧
      ∀ x y : BitVec 64, ConcretizesRange64 r₁ x → ConcretizesRange64 r₂ y →
        ConcretizesRange64 r (x >>> y.setWidth 6) := by
  rcases r₁ with ⟨⟨min1⟩, ⟨max1⟩⟩
  rcases r₂ with ⟨⟨min2⟩, ⟨max2⟩⟩
  change BitVec 64 at min1 max1 min2 max2
  unfold U64Range.shr_range
  simp only [lift, bind_ok]
  by_cases hge : (⟨max2⟩ : Std.U64) ≥ 64#u64
  · simp only [hge, ↓reduceIte]
    clear hge
    refine ⟨_, rfl, fun x _ hx _ => ?_⟩
    unfold ConcretizesRange64 at *; simp only [u64_mk_uscalar_bv, u64_zero_uscalar_bv] at *
    revert hx; bv_decide
  · simp only [hge, ↓reduceIte]
    rw [u64_wrapping_shr_eq, u64_wrapping_shr_eq]
    refine ⟨_, rfl, fun x y hx hy => ?_⟩
    have hlt64 : ¬(64#64 ≤ max2) := fun h => hge ((u64_ge_iff_bv _ _).2 h)
    clear hge
    have hc1 : (UScalar.cast .U32 (⟨max2⟩ : Std.U64)).bv.setWidth 6 = max2.setWidth 6 := by
      change (BitVec.setWidth 32 max2).setWidth 6 = max2.setWidth 6; bv_decide
    have hc2 : (UScalar.cast .U32 (⟨min2⟩ : Std.U64)).bv.setWidth 6 = min2.setWidth 6 := by
      change (BitVec.setWidth 32 min2).setWidth 6 = min2.setWidth 6; bv_decide
    unfold ConcretizesRange64 at *; simp only [u64_mk_uscalar_bv] at *
    rw [hc1, hc2]
    revert hlt64 hx hy; bv_decide

theorem range32_shr_ok_and_sound (r₁ r₂ : U32Range) :
    ∃ r, U32Range.shr_range r₁ r₂ = ok r ∧
      ∀ x y : BitVec 32, ConcretizesRange32 r₁ x → ConcretizesRange32 r₂ y →
        ConcretizesRange32 r (x >>> y.setWidth 5) := by
  rcases r₁ with ⟨⟨min1⟩, ⟨max1⟩⟩
  rcases r₂ with ⟨⟨min2⟩, ⟨max2⟩⟩
  change BitVec 32 at min1 max1 min2 max2
  unfold U32Range.shr_range
  simp only [lift, bind_ok]
  by_cases hge : (⟨max2⟩ : Std.U32) ≥ 32#u32
  · simp only [hge, ↓reduceIte]
    clear hge
    refine ⟨_, rfl, fun x _ hx _ => ?_⟩
    unfold ConcretizesRange32 at *; simp only [u32_mk_uscalar_bv, u32_zero_uscalar_bv] at *
    revert hx; bv_decide
  · simp only [hge, ↓reduceIte]
    rw [u32_wrapping_shr_eq, u32_wrapping_shr_eq]
    refine ⟨_, rfl, fun x y hx hy => ?_⟩
    have hlt32 : ¬(32#32 ≤ max2) := fun h => hge ((u32_ge_iff_bv _ _).2 h)
    clear hge
    unfold ConcretizesRange32 at *; simp only [u32_mk_uscalar_bv] at *
    revert hlt32 hx hy; bv_decide

theorem range64_checked_add_ok_and_sound (r₁ r₂ : U64Range) :
    ∃ opt, U64Range.checked_add r₁ r₂ = ok opt ∧
      ∀ r, opt = some r → ∀ x y : BitVec 64,
        ConcretizesRange64 r₁ x → ConcretizesRange64 r₂ y → ¬BitVec.uaddOverflow x y ∧ ConcretizesRange64 r (x + y) := by
  unfold U64Range.checked_add lift
  rw [bind_ok_eq rfl]
  have hs1 := U64.checked_add_bv_spec r₁.min r₂.min
  cases h1 : U64.checked_add r₁.min r₂.min with
  | none => exact ⟨none, rfl, fun _ h => by cases h⟩
  | some min =>
    rw [h1] at hs1; dsimp only; rw [bind_ok_eq rfl]
    have hs2 := U64.checked_add_bv_spec r₁.max r₂.max
    cases h2 : U64.checked_add r₁.max r₂.max with
    | none => exact ⟨none, rfl, fun _ h => by cases h⟩
    | some max =>
      rw [h2] at hs2
      refine ⟨some { min := min, max := max }, rfl, fun r hr x y hx hy => ?_⟩
      cases hr
      have hle_max : r₁.max.bv ≤ max.bv := by
        rw [BitVec.le_def, UScalar.bv_toNat, UScalar.bv_toNat]; have := hs2.2.1; omega
      have hmin_bv : min.bv = r₁.min.bv + r₂.min.bv := hs1.2.2
      have hmax_bv : max.bv = r₁.max.bv + r₂.max.bv := hs2.2.2
      rw [hmax_bv] at hle_max
      unfold ConcretizesRange64 at *; dsimp only
      rw [hmin_bv, hmax_bv]
      revert hle_max hx hy; bv_decide

theorem range64_checked_sub_ok_and_sound (r₁ r₂ : U64Range) :
    ∃ opt, U64Range.checked_sub r₁ r₂ = ok opt ∧
      ∀ r, opt = some r → ∀ x y : BitVec 64,
        ConcretizesRange64 r₁ x → ConcretizesRange64 r₂ y → ¬BitVec.usubOverflow x y ∧ ConcretizesRange64 r (x - y) := by
  unfold U64Range.checked_sub lift
  rw [bind_ok_eq rfl]
  have hs1 := U64.checked_sub_bv_spec r₁.min r₂.max
  cases h1 : U64.checked_sub r₁.min r₂.max with
  | none => exact ⟨none, rfl, fun _ h => by cases h⟩
  | some min =>
    rw [h1] at hs1; dsimp only; rw [bind_ok_eq rfl]
    have hs2 := U64.checked_sub_bv_spec r₁.max r₂.min
    cases h2 : U64.checked_sub r₁.max r₂.min with
    | none => exact ⟨none, rfl, fun _ h => by cases h⟩
    | some max =>
      rw [h2] at hs2
      refine ⟨some { min := min, max := max }, rfl, fun r hr x y hx hy => ?_⟩
      cases hr
      have hle1 : r₂.max.bv ≤ r₁.min.bv := by
        rw [BitVec.le_def, UScalar.bv_toNat, UScalar.bv_toNat]; exact hs1.1
      have hle2 : r₂.min.bv ≤ r₁.max.bv := by
        rw [BitVec.le_def, UScalar.bv_toNat, UScalar.bv_toNat]; exact hs2.1
      have hmin_bv : min.bv = r₁.min.bv - r₂.max.bv := hs1.2.2
      have hmax_bv : max.bv = r₁.max.bv - r₂.min.bv := hs2.2.2
      unfold ConcretizesRange64 at *; dsimp only
      rw [hmin_bv, hmax_bv]
      revert hle1 hle2 hx hy; bv_decide

end StarnixEbpf.Proof
