-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.ScalarAlu

/-!
# Remaining `scalar_value` Soundness Theorems

Completes verification of `ebpf_core::scalar_value`:
1. Unconditional structural validity (`Scalar64Valid` / `Scalar32Valid`) of all
   constructed abstract scalars.
2. Byte-slice range analysis (`mask_low_bits`, `extract_slices`,
   `assemble_slices`, `compute_range_for_bytes_swap`) for narrow stack loads and
   stores.
3. Masked-value range membership (`contains_masked_value`), `try_new`,
   `is_zero`, `is_uninitialized`, and `can_be_equal` (64-bit and 32-bit).
4. `PartialEq` and `PartialOrd` (`BroaderOrEqual64`) reflexivity, transitivity,
   and 2-safety concretization monotonicity.
-/

open Aeneas Aeneas.Std Result

namespace StarnixEbpf.Proof

open StarnixEbpf.Assumptions
open StarnixEbpf.Spec
open StarnixEbpf.LoopLemmas
open ebpf_core.scalar_value

/-! ## 1. Unconditional Structural Validity (`Scalar64Valid` / `Scalar32Valid`) -/

/-- Purely syntactic validity of a 64-bit abstract scalar (excluding range non-emptiness,
which holds whenever the scalar has at least one concrete inhabitant). Every constructor
in `scalar_value` establishes `Scalar64Valid` unconditionally. -/
def Scalar64Valid (d : U64ScalarValueData) : Prop :=
  (d.value.bv &&& d.unknown_mask.bv = 0#64) ∧
  (d.unwritten_mask.bv &&& ~~~d.unknown_mask.bv = 0#64) ∧
  d.value.bv ≤ d.urange.min.bv ∧
  d.urange.max.bv ≤ (d.value.bv ||| d.unknown_mask.bv)

/-- Purely syntactic validity of a 32-bit abstract scalar. -/
def Scalar32Valid (d : U32ScalarValueData) : Prop :=
  (d.value.bv &&& d.unknown_mask.bv = 0#32) ∧
  (d.unwritten_mask.bv &&& ~~~d.unknown_mask.bv = 0#32) ∧
  d.value.bv ≤ d.urange.min.bv ∧
  d.urange.max.bv ≤ (d.value.bv ||| d.unknown_mask.bv)

theorem Scalar64Valid.of_consistent {d : U64ScalarValueData} (h : Scalar64Consistent d) :
    Scalar64Valid d :=
  ⟨h.1, h.2.1, h.2.2.2.1, h.2.2.2.2⟩

theorem Scalar32Valid.of_consistent {d : U32ScalarValueData} (h : Scalar32Consistent d) :
    Scalar32Valid d :=
  ⟨h.1, h.2.1, h.2.2.2.1, h.2.2.2.2⟩

theorem pureNew64_valid (val unk unwr umin umax : BitVec 64) :
    Scalar64Valid (pureNew64 val unk unwr umin umax) := by
  unfold Scalar64Valid pureNew64
  simp only [u64_mk_bv]
  refine ⟨?_, ?_, ?_, ?_⟩ <;> bv_decide

theorem pureNew32_valid (val unk unwr umin umax : BitVec 32) :
    Scalar32Valid (pureNew32 val unk unwr umin umax) := by
  unfold Scalar32Valid pureNew32
  simp only [u32_mk_bv]
  refine ⟨?_, ?_, ?_, ?_⟩ <;> bv_decide

theorem new64_valid {value unknown_mask unwritten_mask : Std.U64} {urange : U64Range}
    {res : U64ScalarValueData}
    (h : U64ScalarValueData.new value unknown_mask unwritten_mask urange = ok res) :
    Scalar64Valid res := by
  rw [new64_eq_ok] at h
  cases ok_inj h
  exact pureNew64_valid _ _ _ _ _

theorem new32_valid {value unknown_mask unwritten_mask : Std.U32} {urange : U32Range}
    {res : U32ScalarValueData}
    (h : U32ScalarValueData.new value unknown_mask unwritten_mask urange = ok res) :
    Scalar32Valid res := by
  rw [new32_eq_ok] at h
  cases ok_inj h
  exact pureNew32_valid _ _ _ _ _

theorem UNINITIALIZED_64_valid {res : U64ScalarValueData}
    (h : U64ScalarValueData.UNINITIALIZED = ok res) : Scalar64Valid res := by
  obtain ⟨d, hd, hc, _⟩ := UNINITIALIZED_64_sound
  rw [hd] at h; cases ok_inj h
  exact Scalar64Valid.of_consistent hc

theorem UNKNOWN_WRITTEN_64_valid {res : U64ScalarValueData}
    (h : U64ScalarValueData.UNKNOWN_WRITTEN = ok res) : Scalar64Valid res := by
  obtain ⟨d, hd, hc, _⟩ := UNKNOWN_WRITTEN_64_sound
  rw [hd] at h; cases ok_inj h
  exact Scalar64Valid.of_consistent hc

theorem from_known_64_valid {n : Std.U64} {res : U64ScalarValueData}
    (h : U64ScalarValueData.from_known n = ok res) : Scalar64Valid res := by
  obtain ⟨d, hd, hc, _⟩ := from_known_64_sound n
  rw [hd] at h; cases ok_inj h
  exact Scalar64Valid.of_consistent hc

/-! ## 2. Shift, `UScalar` Arithmetic Existentials, & `Usize` Cast Helpers -/

theorem usize_sub_exists (x y : Std.Usize) (h : y.val ≤ x.val) :
    ∃ z : Std.Usize, (x - y : Result Std.Usize) = ok z ∧ z.val = x.val - y.val := by
  have hs := UScalar.sub_bv_spec h
  cases hr : (x - y : Result Std.Usize) with
  | ret z =>
    rw [hr] at hs; simp at hs
    exact ⟨z, rfl, hs.1⟩
  | vis _ _ => rw [hr] at hs; simp at hs
  | div => rw [hr] at hs; simp at hs

theorem usize_mul_exists (x y : Std.Usize) (h : x.val * y.val ≤ UScalar.max .Usize) :
    ∃ z : Std.Usize, (x * y : Result Std.Usize) = ok z ∧ z.val = x.val * y.val := by
  have hs := UScalar.mul_bv_spec h
  cases hr : (x * y : Result Std.Usize) with
  | ret z =>
    rw [hr] at hs; simp at hs
    exact ⟨z, rfl, hs.1⟩
  | vis _ _ => rw [hr] at hs; simp at hs
  | div => rw [hr] at hs; simp at hs

theorem u32_sub_exists (x y : Std.U32) (h : y.val ≤ x.val) :
    ∃ z : Std.U32, (x - y : Result Std.U32) = ok z ∧
      z.val = x.val - y.val ∧ z.bv = x.bv - y.bv := by
  have hs := UScalar.sub_bv_spec h
  cases hr : (x - y : Result Std.U32) with
  | ret z =>
    rw [hr] at hs; simp at hs
    exact ⟨z, rfl, hs.1, hs.2.2⟩
  | vis _ _ => rw [hr] at hs; simp at hs
  | div => rw [hr] at hs; simp at hs

theorem u32_add_exists (x y : Std.U32) (h : x.val + y.val ≤ UScalar.max .U32) :
    ∃ z : Std.U32, (x + y : Result Std.U32) = ok z ∧
      z.val = x.val + y.val ∧ z.bv = x.bv + y.bv := by
  have hs := UScalar.add_bv_spec h
  cases hr : (x + y : Result Std.U32) with
  | ret z =>
    rw [hr] at hs; simp at hs
    exact ⟨z, rfl, hs.1, hs.2⟩
  | vis _ _ => rw [hr] at hs; simp at hs
  | div => rw [hr] at hs; simp at hs

theorem u64_shl_u32_eq_ok (x : Std.U64) (s : Std.U32) (hs : s.val < 64) :
    ((x <<< s) : Result Std.U64) = ok ⟨x.bv <<< s.bv.setWidth 6⟩ := by
  change UScalar.shiftLeft_UScalar x s = _
  simp only [UScalar.shiftLeft_UScalar, UScalar.shiftLeft, UScalarTy.numBits, hs, ↓reduceIte]
  congr 1
  rw [u64_eq_iff_bv]
  change x.bv <<< s.val = x.bv <<< s.bv.setWidth 6
  have hmod : s.val % 64 = s.val := Nat.mod_eq_of_lt hs
  rw [BitVec.shiftLeft_eq', BitVec.toNat_setWidth, UScalar.bv_toNat, hmod]

theorem u64_shr_u32_eq_ok (x : Std.U64) (s : Std.U32) (hs : s.val < 64) :
    ((x >>> s) : Result Std.U64) = ok ⟨x.bv >>> s.bv.setWidth 6⟩ := by
  change UScalar.shiftRight_UScalar x s = _
  simp only [UScalar.shiftRight_UScalar, UScalar.shiftRight, UScalarTy.numBits, hs, ↓reduceIte]
  congr 1
  rw [u64_eq_iff_bv]
  change x.bv >>> s.val = x.bv >>> s.bv.setWidth 6
  have hmod : s.val % 64 = s.val := Nat.mod_eq_of_lt hs
  rw [BitVec.ushiftRight_eq', BitVec.toNat_setWidth, UScalar.bv_toNat, hmod]

theorem usize_max_ge_64 : 64 ≤ UScalar.max .Usize := by
  rcases Usize.bounds_eq with h | h <;>
    simp [UScalar.max_USize_eq, h, U32.max, U32.numBits, U64.max, U64.numBits]

theorem usize_cast_u32_bv (x : Std.Usize) :
    U32.bv (UScalar.cast .U32 x) = BitVec.ofNat 32 x.val := by
  apply BitVec.eq_of_toNat_eq
  change (UScalar.cast .U32 x).bv.toNat = (BitVec.ofNat 32 x.val).toNat
  rw [UScalar.bv_toNat, UScalar.cast_val_eq]
  simp

theorem usize_cast_u32_val_of_lt (x : Std.Usize) (hx : x.val < 2 ^ 32) :
    (UScalar.cast .U32 x).val = x.val := by
  have hx' : x.val < 4294967296 := hx
  rw [← UScalar.bv_toNat]
  change (U32.bv (UScalar.cast .U32 x)).toNat = x.val
  rw [usize_cast_u32_bv x]
  simp [Nat.mod_eq_of_lt hx']

/-! ## 3. Byte-Slice Range Analysis (`compute_range_for_bytes_swap`) -/

/-- Pure bitvector mirror of `U64Range.mask_low_bits` using `Bool` guards so
concrete shifts reduce computationally by `dsimp`. -/
def pureMaskLowBits64 (v : BitVec 64) (bc : BitVec 32) : BitVec 64 :=
  if bc == 0#32 then 0#64
  else if (64#32).ule bc then v
  else v &&& ((1#64 <<< bc.setWidth 6) - 1#64)

private theorem one_shl_ge_one_64 (sh : BitVec 32) (h0 : sh ≠ 0#32) (h64 : ¬(64#32 ≤ sh)) :
    1#64 ≤ (1#64 <<< sh.setWidth 6) := by
  bv_decide

theorem mask_low_bits_64_eq_ok (value : Std.U64) (bit_count : Std.U32) :
    U64Range.mask_low_bits value bit_count =
      ok ⟨pureMaskLowBits64 value.bv bit_count.bv⟩ := by
  unfold U64Range.mask_low_bits pureMaskLowBits64
  by_cases h0 : bit_count = 0#u32
  · subst h0
    simp only [↓reduceIte, u32_zero_bv, beq_self_eq_true]
    exact congrArg ok ((u64_eq_iff_bv _ _).mpr rfl)
  · have h0bv : (bit_count.bv == 0#32) = false := by
      rw [beq_eq_false_iff_ne]
      intro hc; apply h0; exact (u32_eq_iff_bv _ _).mpr hc
    simp only [h0, h0bv, Bool.false_eq_true, ↓reduceIte]
    by_cases h64 : bit_count ≥ 64#u32
    · have h64bv : 64#32 ≤ bit_count.bv := (u32_ge_iff_bv _ _).mp h64
      have h64ule : (64#32).ule bit_count.bv = true := BitVec.ule_iff_le.mpr h64bv
      simp only [h64, h64ule, ↓reduceIte]
    · have h64bv : ¬(64#32 ≤ bit_count.bv) := by
        intro hc; apply h64; exact (u32_ge_iff_bv _ _).mpr hc
      have h64ule : (64#32).ule bit_count.bv = false := by
        cases h : (64#32).ule bit_count.bv
        · rfl
        · exact False.elim (h64bv (BitVec.ule_iff_le.mp h))
      simp only [h64, h64ule, Bool.false_eq_true, ↓reduceIte]
      have hslt : bit_count.val < 64 := by
        have hlt : bit_count.bv < 64#32 := BitVec.not_le.mp h64bv
        rw [BitVec.lt_def] at hlt
        simpa [UScalar.bv_toNat] using hlt
      rw [bind_ok_eq (u64_shl_u32_eq_ok 1#u64 bit_count hslt)]
      have h0ne : bit_count.bv ≠ 0#32 := by
        intro hc; apply h0; exact (u32_eq_iff_bv _ _).mpr hc
      have h1le_bv : 1#64 ≤ (1#64 <<< bit_count.bv.setWidth 6) :=
        one_shl_ge_one_64 bit_count.bv h0ne h64bv
      have h1le_val : (1#u64).val ≤ (⟨(1#u64).bv <<< bit_count.bv.setWidth 6⟩ : Std.U64).val := by
        have : (1#u64).bv ≤ (⟨(1#u64).bv <<< bit_count.bv.setWidth 6⟩ : Std.U64).bv := h1le_bv
        exact (u64_le_iff_bv _ _).mpr this
      rw [bind_ok_eq (uscalar_sub_eq_ok _ 1#u64 h1le_val)]
      rfl

/-- Clamped `(low_bits, mid_bits, high_bits)` bit-offset triple for `extract_slices` /
`assemble_slices`. Separating the `Nat → BitVec 32` arithmetic into this pure function
lets concrete `(offNat, bcNat)` pairs reduce to `BitVec 32` literals via `decide`. -/
def clampSliceParams (offNat bcNat : Nat) : BitVec 32 × BitVec 32 × BitVec 32 :=
  let co := if Nat.blt 8 offNat then 8 else offNat
  let mc := 8 - co
  let cc := if Nat.blt mc bcNat then mc else bcNat
  let low : BitVec 32 := BitVec.ofNat 32 (co * 8)
  let mid : BitVec 32 := BitVec.ofNat 32 (cc * 8)
  (low, mid, low + mid)

/-- Pure bitvector mirror of `U64Range.extract_slices`. -/
def pureExtractSlices64 (v : BitVec 64) (offNat bcNat : Nat) :
    BitVec 64 × BitVec 64 × BitVec 64 :=
  let p := clampSliceParams offNat bcNat
  let low := p.1
  let mid := p.2.1
  let high := p.2.2
  let s1 := pureMaskLowBits64 v low
  let smid := if (64#32).ule low then 0#64 else v >>> low.setWidth 6
  let s2 := pureMaskLowBits64 smid mid
  let s3 := if (64#32).ule high then 0#64 else v >>> high.setWidth 6
  (s1, s2, s3)

/-- Pure bitvector mirror of `U64Range.assemble_slices`. -/
def pureAssembleSlices64 (v1 v2 v3 : BitVec 64) (offNat bcNat : Nat) : BitVec 64 :=
  let p := clampSliceParams offNat bcNat
  let low := p.1
  let mid := p.2.1
  let high := p.2.2
  let p1 := pureMaskLowBits64 v1 low
  let p2 := if (64#32).ule low then 0#64 else (pureMaskLowBits64 v2 mid) <<< low.setWidth 6
  let p3 := if (64#32).ule high then 0#64 else v3 <<< high.setWidth 6
  (p1 ||| p2) ||| p3

private theorem usize_val_8 : (8#usize).val = 8 := by
  cases System.Platform.numBits_eq <;> simp_all [UScalar.val]

private theorem usize_val_0 : (0#usize).val = 0 := rfl

theorem extract_slices_64_eq_ok (value : Std.U64) (offset byte_count : Std.Usize)
    {s1 s2 s3 : BitVec 64}
    (hp : pureExtractSlices64 value.bv offset.val byte_count.val = (s1, s2, s3)) :
    U64Range.extract_slices value offset byte_count = ok (⟨s1⟩, ⟨s2⟩, ⟨s3⟩) := by
  unfold U64Range.extract_slices
  have hmax64 := usize_max_ge_64
  have h8val := usize_val_8
  have hco : ∃ co : Std.Usize,
      (if offset > 8#usize then ok 8#usize else ok offset) = ok co ∧
      co.val = (if Nat.blt 8 offset.val then 8 else offset.val) ∧
      co.val ≤ 8 := by
    by_cases ho : offset > 8#usize
    · have hobv : Nat.blt 8 offset.val = true := by
        rw [Nat.blt_eq]; change (8#usize).val < offset.val at ho; omega
      exact ⟨8#usize, by simp [ho], by simp [hobv, h8val], by omega⟩
    · have hobv : Nat.blt 8 offset.val = false := by
        cases h : Nat.blt 8 offset.val
        · rfl
        · exfalso; apply ho; change (8#usize).val < offset.val
          have := Nat.blt_eq.mp h; omega
      exact ⟨offset, by simp [ho], by simp [hobv], by
        have : ¬(8 < offset.val) := fun hc => by
          have := Nat.blt_eq.mpr hc; simp [hobv] at this
        omega⟩
  rcases hco with ⟨co, hco_eq, hco_val, hco_le⟩
  rw [bind_ok_eq hco_eq]
  obtain ⟨mc, hmc_eq, hmc_val⟩ := usize_sub_exists 8#usize co (by omega)
  rw [h8val] at hmc_val
  rw [bind_ok_eq hmc_eq]
  have hcc : ∃ cc : Std.Usize,
      (if byte_count > mc then ok mc else ok byte_count) = ok cc ∧
      cc.val = (if Nat.blt mc.val byte_count.val then mc.val else byte_count.val) ∧
      co.val + cc.val ≤ 8 := by
    by_cases hb : byte_count > mc
    · have hbbv : Nat.blt mc.val byte_count.val = true := Nat.blt_eq.mpr hb
      exact ⟨mc, by simp [hb], by simp [hbbv], by omega⟩
    · have hbbv : Nat.blt mc.val byte_count.val = false := by
        cases h : Nat.blt mc.val byte_count.val
        · rfl
        · exact False.elim (hb (Nat.blt_eq.mp h))
      have hnlt : ¬(mc.val < byte_count.val) := hb
      exact ⟨byte_count, by simp [hb], by simp [hbbv], by omega⟩
  rcases hcc with ⟨cc, hcc_eq, hcc_val, hcc_le⟩
  rw [bind_ok_eq hcc_eq]
  obtain ⟨i0, hi0_eq, hi0_val⟩ := usize_mul_exists co 8#usize (by rw [h8val]; omega)
  rw [h8val] at hi0_val
  rw [bind_ok_eq hi0_eq, bind_ok_eq (lift_eq_ok _)]
  set low_bits : Std.U32 := UScalar.cast .U32 i0
  have hlow_bv : low_bits.bv = BitVec.ofNat 32 (co.val * 8) := by
    rw [usize_cast_u32_bv i0, hi0_val]
  have hlow_val : low_bits.val = co.val * 8 := by
    rw [usize_cast_u32_val_of_lt i0 (by omega), hi0_val]
  obtain ⟨i1, hi1_eq, hi1_val⟩ := usize_mul_exists cc 8#usize (by rw [h8val]; omega)
  rw [h8val] at hi1_val
  rw [bind_ok_eq hi1_eq, bind_ok_eq (lift_eq_ok _)]
  set mid_bits : Std.U32 := UScalar.cast .U32 i1
  have hmid_bv : mid_bits.bv = BitVec.ofNat 32 (cc.val * 8) := by
    rw [usize_cast_u32_bv i1, hi1_val]
  have hmid_val : mid_bits.val = cc.val * 8 := by
    rw [usize_cast_u32_val_of_lt i1 (by omega), hi1_val]
  have hu32max : UScalar.max .U32 = 4294967295 := by simp [U32.max, U32.numBits]
  obtain ⟨high_bits, hhi_eq, _, hhigh_bv0⟩ := u32_add_exists low_bits mid_bits (by omega)
  have hhigh_bv : high_bits.bv = BitVec.ofNat 32 (co.val * 8) + BitVec.ofNat 32 (cc.val * 8) := by
    rw [hhigh_bv0, hlow_bv, hmid_bv]
  rw [bind_ok_eq hhi_eq, bind_ok_eq (mask_low_bits_64_eq_ok value low_bits)]
  have hsmid : (if low_bits ≥ 64#u32 then ok 0#u64 else value >>> low_bits) =
      ok ⟨if (64#32).ule low_bits.bv then 0#64 else value.bv >>> low_bits.bv.setWidth 6⟩ := by
    by_cases hl : low_bits ≥ 64#u32
    · have hlbv : 64#32 ≤ low_bits.bv := (u32_ge_iff_bv _ _).mp hl
      have hlule : (64#32).ule low_bits.bv = true := BitVec.ule_iff_le.mpr hlbv
      simp only [hl, hlule, ↓reduceIte]
      exact congrArg ok ((u64_eq_iff_bv _ _).mpr rfl)
    · have hlbv : ¬(64#32 ≤ low_bits.bv) := fun hc => hl ((u32_ge_iff_bv _ _).mpr hc)
      have hlule : (64#32).ule low_bits.bv = false := by
        cases h : (64#32).ule low_bits.bv
        · rfl
        · exact False.elim (hlbv (BitVec.ule_iff_le.mp h))
      have hllt : low_bits.val < 64 := by
        have hlt : low_bits.bv < 64#32 := BitVec.not_le.mp hlbv
        rw [BitVec.lt_def] at hlt; simpa [UScalar.bv_toNat] using hlt
      simp only [hl, hlule, Bool.false_eq_true, ↓reduceIte]
      exact u64_shr_u32_eq_ok value low_bits hllt
  rw [bind_ok_eq hsmid, bind_ok_eq (mask_low_bits_64_eq_ok _ mid_bits)]
  have hclamp : clampSliceParams offset.val byte_count.val = (low_bits.bv, mid_bits.bv, high_bits.bv) := by
    unfold clampSliceParams
    dsimp only []
    rw [hmc_val] at hcc_val
    rw [← hco_val, ← hcc_val, ← hlow_bv, ← hmid_bv, ← hhigh_bv0]
  unfold pureExtractSlices64 at hp
  rw [hclamp] at hp
  dsimp only [] at hp
  rcases Prod.mk.inj hp with ⟨rfl, hp'⟩
  rcases Prod.mk.inj hp' with ⟨rfl, rfl⟩
  by_cases hh : high_bits ≥ 64#u32
  · have hhbv : 64#32 ≤ high_bits.bv := (u32_ge_iff_bv _ _).mp hh
    have hhule : (64#32).ule high_bits.bv = true := BitVec.ule_iff_le.mpr hhbv
    simp only [hh, hhule, ↓reduceIte, u64_mk_bv]
    rfl
  · have hhbv : ¬(64#32 ≤ high_bits.bv) := fun hc => hh ((u32_ge_iff_bv _ _).mpr hc)
    have hhule : (64#32).ule high_bits.bv = false := by
      cases h : (64#32).ule high_bits.bv
      · rfl
      · exact False.elim (hhbv (BitVec.ule_iff_le.mp h))
    have hhlt : high_bits.val < 64 := by
      have hlt' : high_bits.bv < 64#32 := BitVec.not_le.mp hhbv
      rw [BitVec.lt_def] at hlt'; simpa [UScalar.bv_toNat] using hlt'
    simp only [hh, hhule, Bool.false_eq_true, ↓reduceIte, u64_mk_bv]
    rw [bind_ok_eq (u64_shr_u32_eq_ok value high_bits hhlt)]

theorem assemble_slices_64_eq_ok (v1 v2 v3 : Std.U64) (offset byte_count : Std.Usize) :
    U64Range.assemble_slices (v1, v2, v3) offset byte_count =
      ok ⟨pureAssembleSlices64 v1.bv v2.bv v3.bv offset.val byte_count.val⟩ := by
  simp (config := { zetaHave := true }) only [U64Range.assemble_slices, pureAssembleSlices64,
    Aeneas.Std.uncurry]
  have hmax64 := usize_max_ge_64
  have h8val := usize_val_8
  have hco : ∃ co : Std.Usize,
      (if offset > 8#usize then ok 8#usize else ok offset) = ok co ∧
      co.val = (if Nat.blt 8 offset.val then 8 else offset.val) ∧
      co.val ≤ 8 := by
    by_cases ho : offset > 8#usize
    · have hobv : Nat.blt 8 offset.val = true := by
        rw [Nat.blt_eq]; change (8#usize).val < offset.val at ho; omega
      exact ⟨8#usize, by simp [ho], by simp [hobv, h8val], by omega⟩
    · have hobv : Nat.blt 8 offset.val = false := by
        cases h : Nat.blt 8 offset.val
        · rfl
        · exfalso; apply ho; change (8#usize).val < offset.val
          have := Nat.blt_eq.mp h; omega
      exact ⟨offset, by simp [ho], by simp [hobv], by
        have : ¬(8 < offset.val) := fun hc => by
          have := Nat.blt_eq.mpr hc; simp [hobv] at this
        omega⟩
  rcases hco with ⟨co, hco_eq, hco_val, hco_le⟩
  rw [bind_ok_eq hco_eq]
  obtain ⟨mc, hmc_eq, hmc_val⟩ := usize_sub_exists 8#usize co (by omega)
  rw [h8val] at hmc_val
  rw [bind_ok_eq hmc_eq]
  have hcc : ∃ cc : Std.Usize,
      (if byte_count > mc then ok mc else ok byte_count) = ok cc ∧
      cc.val = (if Nat.blt mc.val byte_count.val then mc.val else byte_count.val) ∧
      co.val + cc.val ≤ 8 := by
    by_cases hb : byte_count > mc
    · have hbbv : Nat.blt mc.val byte_count.val = true := Nat.blt_eq.mpr hb
      exact ⟨mc, by simp [hb], by simp [hbbv], by omega⟩
    · have hbbv : Nat.blt mc.val byte_count.val = false := by
        cases h : Nat.blt mc.val byte_count.val
        · rfl
        · exact False.elim (hb (Nat.blt_eq.mp h))
      have hnlt : ¬(mc.val < byte_count.val) := hb
      exact ⟨byte_count, by simp [hb], by simp [hbbv], by omega⟩
  rcases hcc with ⟨cc, hcc_eq, hcc_val, hcc_le⟩
  rw [bind_ok_eq hcc_eq]
  obtain ⟨i0, hi0_eq, hi0_val⟩ := usize_mul_exists co 8#usize (by rw [h8val]; omega)
  rw [h8val] at hi0_val
  rw [bind_ok_eq hi0_eq, bind_ok_eq (lift_eq_ok _)]
  set low_bits : Std.U32 := UScalar.cast .U32 i0
  have hlow_bv : low_bits.bv = BitVec.ofNat 32 (co.val * 8) := by
    rw [usize_cast_u32_bv i0, hi0_val]
  have hlow_val : low_bits.val = co.val * 8 := by
    rw [usize_cast_u32_val_of_lt i0 (by omega), hi0_val]
  obtain ⟨i1, hi1_eq, hi1_val⟩ := usize_mul_exists cc 8#usize (by rw [h8val]; omega)
  rw [h8val] at hi1_val
  rw [bind_ok_eq hi1_eq, bind_ok_eq (lift_eq_ok _)]
  set mid_bits : Std.U32 := UScalar.cast .U32 i1
  have hmid_bv : mid_bits.bv = BitVec.ofNat 32 (cc.val * 8) := by
    rw [usize_cast_u32_bv i1, hi1_val]
  have hmid_val : mid_bits.val = cc.val * 8 := by
    rw [usize_cast_u32_val_of_lt i1 (by omega), hi1_val]
  have hu32max : UScalar.max .U32 = 4294967295 := by simp [U32.max, U32.numBits]
  obtain ⟨high_bits, hhi_eq, _, hhigh_bv0⟩ := u32_add_exists low_bits mid_bits (by omega)
  have hhigh_bv : high_bits.bv = BitVec.ofNat 32 (co.val * 8) + BitVec.ofNat 32 (cc.val * 8) := by
    rw [hhigh_bv0, hlow_bv, hmid_bv]
  rw [bind_ok_eq hhi_eq, bind_ok_eq (mask_low_bits_64_eq_ok v1 low_bits)]
  have hp2 : (if low_bits ≥ 64#u32 then ok 0#u64
              else do let i2 ← U64Range.mask_low_bits v2 mid_bits; i2 <<< low_bits) =
      ok ⟨if (64#32).ule low_bits.bv then 0#64
          else (pureMaskLowBits64 v2.bv mid_bits.bv) <<< low_bits.bv.setWidth 6⟩ := by
    by_cases hl : low_bits ≥ 64#u32
    · have hlbv : 64#32 ≤ low_bits.bv := (u32_ge_iff_bv _ _).mp hl
      have hlule : (64#32).ule low_bits.bv = true := BitVec.ule_iff_le.mpr hlbv
      simp only [hl, hlule, ↓reduceIte]
      exact congrArg ok ((u64_eq_iff_bv _ _).mpr rfl)
    · have hlbv : ¬(64#32 ≤ low_bits.bv) := fun hc => hl ((u32_ge_iff_bv _ _).mpr hc)
      have hlule : (64#32).ule low_bits.bv = false := by
        cases h : (64#32).ule low_bits.bv
        · rfl
        · exact False.elim (hlbv (BitVec.ule_iff_le.mp h))
      have hllt : low_bits.val < 64 := by
        have hlt : low_bits.bv < 64#32 := BitVec.not_le.mp hlbv
        rw [BitVec.lt_def] at hlt; simpa [UScalar.bv_toNat] using hlt
      simp only [hl, hlule, Bool.false_eq_true, ↓reduceIte]
      rw [bind_ok_eq (mask_low_bits_64_eq_ok v2 mid_bits)]
      exact u64_shl_u32_eq_ok _ low_bits hllt
  rw [bind_ok_eq hp2]
  have hp3 : (if high_bits ≥ 64#u32 then ok 0#u64 else v3 <<< high_bits) =
      ok ⟨if (64#32).ule high_bits.bv then 0#64 else v3.bv <<< high_bits.bv.setWidth 6⟩ := by
    by_cases hh : high_bits ≥ 64#u32
    · have hhbv : 64#32 ≤ high_bits.bv := (u32_ge_iff_bv _ _).mp hh
      have hhule : (64#32).ule high_bits.bv = true := BitVec.ule_iff_le.mpr hhbv
      simp only [hh, hhule, ↓reduceIte]
      exact congrArg ok ((u64_eq_iff_bv _ _).mpr rfl)
    · have hhbv : ¬(64#32 ≤ high_bits.bv) := fun hc => hh ((u32_ge_iff_bv _ _).mpr hc)
      have hhule : (64#32).ule high_bits.bv = false := by
        cases h : (64#32).ule high_bits.bv
        · rfl
        · exact False.elim (hhbv (BitVec.ule_iff_le.mp h))
      have hhlt : high_bits.val < 64 := by
        have hlt : high_bits.bv < 64#32 := BitVec.not_le.mp hhbv
        rw [BitVec.lt_def] at hlt; simpa [UScalar.bv_toNat] using hlt
      simp only [hh, hhule, Bool.false_eq_true, ↓reduceIte]
      exact u64_shl_u32_eq_ok v3 high_bits hhlt
  rw [bind_ok_eq hp3, bind_ok_eq (lift_eq_ok _)]
  have hclamp : clampSliceParams offset.val byte_count.val = (low_bits.bv, mid_bits.bv, high_bits.bv) := by
    unfold clampSliceParams
    dsimp only []
    rw [hmc_val] at hcc_val
    rw [← hco_val, ← hcc_val, ← hlow_bv, ← hmid_bv, ← hhigh_bv0]
  rw [hclamp]
  rfl

/-- Pure bitvector mirror of `U64Range.compute_range_for_bytes_swap`, avoiding
tuple-valued `if` expressions so `bv_decide` bit-blasts directly without `split_ifs`. -/
def pureBytesSwapRange64 (tmin tmax smin smax : BitVec 64)
    (toffNat soffNat bcNat : Nat) : U64Range :=
  let (t_umin1, t_umin2, t_umin3) := pureExtractSlices64 tmin toffNat bcNat
  let (t_umax1, t_umax2, t_umax3) := pureExtractSlices64 tmax toffNat bcNat
  let (_, s_umin2, s_umin3) := pureExtractSlices64 smin soffNat bcNat
  let (_, s_umax2, s_umax3) := pureExtractSlices64 smax soffNat bcNat
  let f_umin2 := if s_umin3.ult s_umax3 then 0#64 else s_umin2
  let f_umax2 := if s_umin3.ult s_umax3 then ~~~0#64 else s_umax2
  let f_umin1 :=
    if t_umin3.ult t_umax3 then 0#64
    else if t_umin2.ult t_umax2 then 0#64
    else t_umin1
  let f_umax1 :=
    if t_umin3.ult t_umax3 then ~~~0#64
    else if t_umin2.ult t_umax2 then ~~~0#64
    else t_umax1
  let fmin := pureAssembleSlices64 f_umin1 f_umin2 t_umin3 toffNat bcNat
  let fmax := pureAssembleSlices64 f_umax1 f_umax2 t_umax3 toffNat bcNat
  { min := ⟨fmin⟩, max := ⟨fmax⟩ }

theorem compute_range_for_bytes_swap_64_eq_ok
    (target source : U64Range) (target_offset source_offset byte_count : Std.Usize) :
    U64Range.compute_range_for_bytes_swap target source target_offset source_offset byte_count =
      ok (pureBytesSwapRange64 target.min.bv target.max.bv source.min.bv source.max.bv
            target_offset.val source_offset.val byte_count.val) := by
  unfold U64Range.compute_range_for_bytes_swap pureBytesSwapRange64 U64Range.new
  rcases h1 : pureExtractSlices64 target.min.bv target_offset.val byte_count.val with ⟨tu1, tu2, tu3⟩
  rcases h2 : pureExtractSlices64 target.max.bv target_offset.val byte_count.val with ⟨tx1, tx2, tx3⟩
  rcases h3 : pureExtractSlices64 source.min.bv source_offset.val byte_count.val with ⟨su1, su2, su3⟩
  rcases h4 : pureExtractSlices64 source.max.bv source_offset.val byte_count.val with ⟨sx1, sx2, sx3⟩
  rw [bind_triple_ok_eq (extract_slices_64_eq_ok target.min target_offset byte_count h1)]
  rw [bind_triple_ok_eq (extract_slices_64_eq_ok target.max target_offset byte_count h2)]
  rw [bind_triple_ok_eq (extract_slices_64_eq_ok source.min source_offset byte_count h3)]
  rw [bind_triple_ok_eq (extract_slices_64_eq_ok source.max source_offset byte_count h4)]
  dsimp only []
  have hs_pair :
      (if (⟨sx3⟩ : Std.U64) > ⟨su3⟩ then ok (0#u64, core.num.U64.MAX) else ok (⟨su2⟩, ⟨sx2⟩)) =
        ok (if su3.ult sx3 then (0#u64 : Std.U64) else ⟨su2⟩,
            if su3.ult sx3 then core.num.U64.MAX else ⟨sx2⟩) := by
    by_cases hs : (⟨sx3⟩ : Std.U64) > ⟨su3⟩
    · have hsbv : su3.ult sx3 = true := BitVec.ult_iff_lt.mpr ((u64_gt_iff_bv _ _).mp hs)
      simp only [hs, hsbv, ↓reduceIte]
    · have hsbv : su3.ult sx3 = false := by
        cases h : su3.ult sx3
        · rfl
        · exact False.elim (hs ((u64_gt_iff_bv _ _).mpr (BitVec.ult_iff_lt.mp h)))
      simp only [hs, hsbv, Bool.false_eq_true, ↓reduceIte]
  rw [bind_pair_ok_eq hs_pair]
  have ht_pair :
      (if (⟨tx3⟩ : Std.U64) > ⟨tu3⟩ then ok (0#u64, core.num.U64.MAX)
       else if (⟨tx2⟩ : Std.U64) > ⟨tu2⟩ then ok (0#u64, core.num.U64.MAX)
       else ok (⟨tu1⟩, ⟨tx1⟩)) =
        ok (if tu3.ult tx3 then 0#u64 else if tu2.ult tx2 then 0#u64 else ⟨tu1⟩,
            if tu3.ult tx3 then core.num.U64.MAX else if tu2.ult tx2 then core.num.U64.MAX else ⟨tx1⟩) := by
    by_cases ht3 : (⟨tx3⟩ : Std.U64) > ⟨tu3⟩
    · have ht3bv : tu3.ult tx3 = true := BitVec.ult_iff_lt.mpr ((u64_gt_iff_bv _ _).mp ht3)
      simp only [ht3, ht3bv, ↓reduceIte]
    · have ht3bv : tu3.ult tx3 = false := by
        cases h : tu3.ult tx3
        · rfl
        · exact False.elim (ht3 ((u64_gt_iff_bv _ _).mpr (BitVec.ult_iff_lt.mp h)))
      simp only [ht3, ht3bv, Bool.false_eq_true, ↓reduceIte]
      by_cases ht2 : (⟨tx2⟩ : Std.U64) > ⟨tu2⟩
      · have ht2bv : tu2.ult tx2 = true := BitVec.ult_iff_lt.mpr ((u64_gt_iff_bv _ _).mp ht2)
        simp only [ht2, ht2bv, ↓reduceIte]
      · have ht2bv : ¬(tu2 < tx2) := fun hc => ht2 ((u64_gt_iff_bv _ _).mpr hc)
        have ht2u : tu2.ult tx2 = false := by
          cases h : tu2.ult tx2
          · rfl
          · exact False.elim (ht2bv (BitVec.ult_iff_lt.mp h))
        simp only [ht2, ht2u, Bool.false_eq_true, ↓reduceIte]
  rw [bind_pair_ok_eq ht_pair]
  rw [bind_ok_eq (assemble_slices_64_eq_ok _ _ _ target_offset byte_count)]
  rw [bind_ok_eq (assemble_slices_64_eq_ok _ _ _ target_offset byte_count)]
  cases su3.ult sx3 <;> cases tu3.ult tx3 <;> cases tu2.ult tx2 <;>
    simp only [Bool.false_eq_true, ↓reduceIte, u64_zero_bv, u64_max_bv, u64_mk_bv]

private theorem clampSliceParams_eval :
    clampSliceParams 0 1 = (0#32, 8#32, 8#32) ∧
    clampSliceParams 1 1 = (8#32, 8#32, 16#32) ∧
    clampSliceParams 2 1 = (16#32, 8#32, 24#32) ∧
    clampSliceParams 3 1 = (24#32, 8#32, 32#32) ∧
    clampSliceParams 4 1 = (32#32, 8#32, 40#32) ∧
    clampSliceParams 5 1 = (40#32, 8#32, 48#32) ∧
    clampSliceParams 6 1 = (48#32, 8#32, 56#32) ∧
    clampSliceParams 7 1 = (56#32, 8#32, 64#32) ∧
    clampSliceParams 0 2 = (0#32, 16#32, 16#32) ∧
    clampSliceParams 1 2 = (8#32, 16#32, 24#32) ∧
    clampSliceParams 2 2 = (16#32, 16#32, 32#32) ∧
    clampSliceParams 3 2 = (24#32, 16#32, 40#32) ∧
    clampSliceParams 4 2 = (32#32, 16#32, 48#32) ∧
    clampSliceParams 5 2 = (40#32, 16#32, 56#32) ∧
    clampSliceParams 6 2 = (48#32, 16#32, 64#32) ∧
    clampSliceParams 0 4 = (0#32, 32#32, 32#32) ∧
    clampSliceParams 1 4 = (8#32, 32#32, 40#32) ∧
    clampSliceParams 2 4 = (16#32, 32#32, 48#32) ∧
    clampSliceParams 3 4 = (24#32, 32#32, 56#32) ∧
    clampSliceParams 4 4 = (32#32, 32#32, 64#32) := by
  decide

private theorem bitvec_ofNat_6_eval :
    BitVec.ofNat 6 (0 * 8) = 0#6 ∧
    BitVec.ofNat 6 (1 * 8) = 8#6 ∧
    BitVec.ofNat 6 (2 * 8) = 16#6 ∧
    BitVec.ofNat 6 (3 * 8) = 24#6 ∧
    BitVec.ofNat 6 (4 * 8) = 32#6 ∧
    BitVec.ofNat 6 (5 * 8) = 40#6 ∧
    BitVec.ofNat 6 (6 * 8) = 48#6 ∧
    BitVec.ofNat 6 (7 * 8) = 56#6 := by
  decide

private theorem pureBytesSwap_load_sound_bv (smin smax x : BitVec 64)
    (subIdx : Nat) (w : DataWidth)
    (hw : w = .u8 ∨ w = .u16 ∨ w = .u32)
    (hsub : subIdx + w.bytes ≤ 8)
    (hmin : smin ≤ x) (hmax : x ≤ smax) :
    ConcretizesRange64 (pureBytesSwapRange64 0#64 0#64 smin smax 0 subIdx w.bytes)
      (extractSubValue x subIdx w) := by
  unfold ConcretizesRange64 pureBytesSwapRange64 extractSubValue
  have hc := clampSliceParams_eval
  have hb := bitvec_ofNat_6_eval
  rcases hw with rfl | rfl | rfl
  · have hcases : subIdx = 0 ∨ subIdx = 1 ∨ subIdx = 2 ∨ subIdx = 3 ∨
        subIdx = 4 ∨ subIdx = 5 ∨ subIdx = 6 ∨ subIdx = 7 := by
      simp [DataWidth.bytes] at hsub; omega
    rcases hcases with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;> {
      simp only [DataWidth.bytes, DataWidth.valMask64,
        pureExtractSlices64, pureAssembleSlices64, pureMaskLowBits64, u64_mk_bv,
        hc, hb]
      bv_decide
    }
  · have hcases : subIdx = 0 ∨ subIdx = 1 ∨ subIdx = 2 ∨ subIdx = 3 ∨
        subIdx = 4 ∨ subIdx = 5 ∨ subIdx = 6 := by
      simp [DataWidth.bytes] at hsub; omega
    rcases hcases with rfl | rfl | rfl | rfl | rfl | rfl | rfl <;> {
      simp only [DataWidth.bytes, DataWidth.valMask64,
        pureExtractSlices64, pureAssembleSlices64, pureMaskLowBits64, u64_mk_bv,
        hc, hb]
      bv_decide
    }
  · have hcases : subIdx = 0 ∨ subIdx = 1 ∨ subIdx = 2 ∨ subIdx = 3 ∨ subIdx = 4 := by
      simp [DataWidth.bytes] at hsub; omega
    rcases hcases with rfl | rfl | rfl | rfl | rfl <;> {
      simp only [DataWidth.bytes, DataWidth.valMask64,
        pureExtractSlices64, pureAssembleSlices64, pureMaskLowBits64, u64_mk_bv,
        hc, hb]
      bv_decide
    }

theorem compute_range_for_bytes_swap_load_sound
    (src : U64Range) (subIdx bw : Std.Usize) (w : DataWidth) (x : BitVec 64)
    (hw : w = .u8 ∨ w = .u16 ∨ w = .u32)
    (hbw : bw.val = w.bytes)
    (hsub : subIdx.val + w.bytes ≤ 8)
    (hx : ConcretizesRange64 src x) :
    ∃ rOut,
      U64Range.compute_range_for_bytes_swap { min := 0#u64, max := 0#u64 } src 0#usize subIdx bw =
        ok rOut ∧
      ConcretizesRange64 rOut (extractSubValue x subIdx.val w) := by
  refine ⟨_, compute_range_for_bytes_swap_64_eq_ok _ _ _ _ _, ?_⟩
  rw [usize_val_0, hbw]
  exact pureBytesSwap_load_sound_bv src.min.bv src.max.bv x subIdx.val w hw hsub hx.1 hx.2

private theorem pureBytesSwap_store_sound_bv (tmin tmax smin smax orig val : BitVec 64)
    (subIdx : Nat) (w : DataWidth)
    (hw : w = .u8 ∨ w = .u16 ∨ w = .u32)
    (hsub : subIdx + w.bytes ≤ 8)
    (htmin : tmin ≤ orig) (htmax : orig ≤ tmax)
    (hsmin : smin ≤ val) (hsmax : val ≤ smax) :
    ConcretizesRange64 (pureBytesSwapRange64 tmin tmax smin smax subIdx 0 w.bytes)
      (insertSubValue orig val subIdx w) := by
  unfold ConcretizesRange64 pureBytesSwapRange64 insertSubValue
  have hc := clampSliceParams_eval
  have hb := bitvec_ofNat_6_eval
  rcases hw with rfl | rfl | rfl
  · have hcases : subIdx = 0 ∨ subIdx = 1 ∨ subIdx = 2 ∨ subIdx = 3 ∨
        subIdx = 4 ∨ subIdx = 5 ∨ subIdx = 6 ∨ subIdx = 7 := by
      simp [DataWidth.bytes] at hsub; omega
    rcases hcases with rfl | rfl | rfl | rfl | rfl | rfl | rfl | rfl <;> {
      simp only [DataWidth.bytes, DataWidth.valMask64,
        pureExtractSlices64, pureAssembleSlices64, pureMaskLowBits64, u64_mk_bv,
        hc, hb]
      bv_decide
    }
  · have hcases : subIdx = 0 ∨ subIdx = 1 ∨ subIdx = 2 ∨ subIdx = 3 ∨
        subIdx = 4 ∨ subIdx = 5 ∨ subIdx = 6 := by
      simp [DataWidth.bytes] at hsub; omega
    rcases hcases with rfl | rfl | rfl | rfl | rfl | rfl | rfl <;> {
      simp only [DataWidth.bytes, DataWidth.valMask64,
        pureExtractSlices64, pureAssembleSlices64, pureMaskLowBits64, u64_mk_bv,
        hc, hb]
      bv_decide
    }
  · have hcases : subIdx = 0 ∨ subIdx = 1 ∨ subIdx = 2 ∨ subIdx = 3 ∨ subIdx = 4 := by
      simp [DataWidth.bytes] at hsub; omega
    rcases hcases with rfl | rfl | rfl | rfl | rfl <;> {
      simp only [DataWidth.bytes, DataWidth.valMask64,
        pureExtractSlices64, pureAssembleSlices64, pureMaskLowBits64, u64_mk_bv,
        hc, hb]
      bv_decide
    }

theorem compute_range_for_bytes_swap_store_sound
    (tgt src : U64Range) (subIdx bw : Std.Usize) (w : DataWidth) (orig val : BitVec 64)
    (hw : w = .u8 ∨ w = .u16 ∨ w = .u32)
    (hbw : bw.val = w.bytes)
    (hsub : subIdx.val + w.bytes ≤ 8)
    (htgt : ConcretizesRange64 tgt orig)
    (hsrc : ConcretizesRange64 src val) :
    ∃ rOut,
      U64Range.compute_range_for_bytes_swap tgt src subIdx 0#usize bw = ok rOut ∧
      ConcretizesRange64 rOut (insertSubValue orig val subIdx.val w) := by
  refine ⟨_, compute_range_for_bytes_swap_64_eq_ok _ _ _ _ _, ?_⟩
  rw [usize_val_0, hbw]
  exact pureBytesSwap_store_sound_bv tgt.min.bv tgt.max.bv src.min.bv src.max.bv orig val
    subIdx.val w hw hsub htgt.1 htgt.2 hsrc.1 hsrc.2

/-! ## 4. `contains_masked_value`, `try_new`, `is_zero`, `is_uninitialized`, `can_be_equal` -/

/-- Pure bitvector mirror of `U64Range.contains_masked_value`. -/
def pureContainsMaskedValue64 (rmin rmax val um : BitVec 64) : Bool :=
  let val1 := val &&& ~~~um
  if rmax < rmin then false
  else
    let diff := rmin ^^^ rmax
    if diff == 0#64 then
      ((rmin ^^^ val1) &&& ~~~um) == 0#64
    else
      let b : BitVec 32 := 63#32 - diff.clz.setWidth 32
      let common_mask : BitVec 64 :=
        if b == 63#32 then 0#64 else (~~~0#64) <<< (b + 1#32).setWidth 6
      if ((rmin ^^^ val1) &&& common_mask &&& ~~~um) != 0#64 then false
      else
        let val2 := (val1 &&& ~~~common_mask) ||| (rmin &&& common_mask)
        let um1 := um &&& ~~~common_mask
        let bit_b : BitVec 64 := 1#64 <<< b.setWidth 6
        if (um1 &&& bit_b) != 0#64 then
          if rmin ≤ (val2 ||| (um1 &&& ~~~bit_b)) then true
          else (val2 ||| bit_b) ≤ rmax
        else
          if val2 ≤ rmax then rmin ≤ (val2 ||| um1)
          else false

/-- Pure bitvector mirror of `U32Range.contains_masked_value`. -/
def pureContainsMaskedValue32 (rmin rmax val um : BitVec 32) : Bool :=
  let val1 := val &&& ~~~um
  if rmax < rmin then false
  else
    let diff := rmin ^^^ rmax
    if diff == 0#32 then
      ((rmin ^^^ val1) &&& ~~~um) == 0#32
    else
      let b : BitVec 32 := 31#32 - diff.clz
      let common_mask : BitVec 32 :=
        if b == 31#32 then 0#32 else (~~~0#32) <<< (b + 1#32).setWidth 5
      if ((rmin ^^^ val1) &&& common_mask &&& ~~~um) != 0#32 then false
      else
        let val2 := (val1 &&& ~~~common_mask) ||| (rmin &&& common_mask)
        let um1 := um &&& ~~~common_mask
        let bit_b : BitVec 32 := 1#32 <<< b.setWidth 5
        if (um1 &&& bit_b) != 0#32 then
          if rmin ≤ (val2 ||| (um1 &&& ~~~bit_b)) then true
          else (val2 ||| bit_b) ≤ rmax
        else
          if val2 ≤ rmax then rmin ≤ (val2 ||| um1)
          else false

private theorem clz64_le_63_of_ne_zero (d : BitVec 64) (hd : d ≠ 0#64) :
    d.clz.setWidth 32 ≤ 63#32 := by
  bv_decide

private theorem sub_63_clz64_lt_63 (d : BitVec 64) (hd : d ≠ 0#64)
    (hne : 63#32 - d.clz.setWidth 32 ≠ 63#32) :
    (63#32 - d.clz.setWidth 32) < 63#32 := by
  bv_decide

private theorem clz32_le_31_of_ne_zero (d : BitVec 32) (hd : d ≠ 0#32) :
    d.clz ≤ 31#32 := by
  bv_decide

private theorem sub_31_clz32_lt_31 (d : BitVec 32) (hd : d ≠ 0#32)
    (hne : 31#32 - d.clz ≠ 31#32) :
    (31#32 - d.clz) < 31#32 := by
  bv_decide

private theorem ite_true {α : Type} {c : Prop} [Decidable c] (hc : c) (a b : α) :
    (if c then a else b) = a := by
  split_ifs; rfl

private theorem ite_false {α : Type} {c : Prop} [Decidable c] (hc : ¬c) (a b : α) :
    (if c then a else b) = b := by
  split_ifs; rfl

theorem contains_masked_value_64_eq_ok (self : U64Range) (value unknown_mask : Std.U64) :
    U64Range.contains_masked_value self value unknown_mask =
      ok (pureContainsMaskedValue64 self.min.bv self.max.bv value.bv unknown_mask.bv) := by
  unfold U64Range.contains_masked_value pureContainsMaskedValue64
  rw [bind_ok_eq (lift_eq_ok _), bind_ok_eq (lift_eq_ok _)]
  by_cases hgt : self.min > self.max
  · have hgt_bv : self.max.bv < self.min.bv := (u64_gt_iff_bv _ _).mp hgt
    simp [hgt, hgt_bv]
  · have hgt_bv : ¬(self.max.bv < self.min.bv) := fun hc => hgt ((u64_gt_iff_bv _ _).mpr hc)
    simp only [hgt, hgt_bv, ↓reduceIte]
    rw [bind_ok_eq (lift_eq_ok _)]
    by_cases hdiff : (self.min ^^^ self.max) = 0#u64
    · have hdiff_bv : ((self.min.bv ^^^ self.max.bv) == 0#64) = true := by
        rw [beq_iff_eq]; exact (u64_eq_iff_bv _ _).mp hdiff
      simp only [hdiff, hdiff_bv, ↓reduceIte, lift, bind_ok]
      congr 1
      cases hbeq : (((self.min.bv ^^^ (value.bv &&& ~~~unknown_mask.bv)) &&& ~~~unknown_mask.bv) == 0#64)
      · have hne : ¬((((self.min ^^^ (value &&& ~~~unknown_mask)) &&& ~~~unknown_mask) : Std.U64) = 0#u64) := by
          rw [u64_eq_iff_bv]; exact beq_eq_false_iff_ne.mp hbeq
        simp [hne]
      · have heq : (((self.min ^^^ (value &&& ~~~unknown_mask)) &&& ~~~unknown_mask) : Std.U64) = 0#u64 := by
          rw [u64_eq_iff_bv]; exact beq_iff_eq.mp hbeq
        simp [heq]
    · have hdiff_ne : (self.min.bv ^^^ self.max.bv) ≠ 0#64 := by
        intro hc; apply hdiff; exact (u64_eq_iff_bv _ _).mpr hc
      have hdiff_bv : ((self.min.bv ^^^ self.max.bv) == 0#64) = false :=
        beq_eq_false_iff_ne.mpr hdiff_ne
      simp only [hdiff, hdiff_bv, Bool.false_eq_true, ↓reduceIte]
      have h63 : (64#u32 - 1#u32 : Result Std.U32) = ok 63#u32 := by
        rw [uscalar_sub_eq_ok 64#u32 1#u32 (by decide)]
        exact congrArg ok ((u32_eq_iff_bv _ _).mpr (by decide))
      rw [bind_ok_eq h63, bind_ok_eq (lift_eq_ok _)]
      set lz := core.num.U64.leading_zeros (self.min ^^^ self.max)
      have hlz_bv : lz.bv = (self.min.bv ^^^ self.max.bv).clz.setWidth 32 :=
        leadingZeros64_eq_clz (self.min.bv ^^^ self.max.bv)
      have hlz_le : lz.val ≤ (63#u32).val := by
        have hbvle := clz64_le_63_of_ne_zero _ hdiff_ne
        rw [← hlz_bv] at hbvle
        exact (u32_le_iff_bv lz 63#u32).mpr hbvle
      obtain ⟨b, hb_eq, _, hb_bv0⟩ := u32_sub_exists 63#u32 lz hlz_le
      have hb_bv : b.bv = 63#32 - (self.min.bv ^^^ self.max.bv).clz.setWidth 32 := by
        rw [hb_bv0, hlz_bv]; rfl
      rw [bind_ok_eq hb_eq]
      have hcm : (if b = 63#u32 then ok 0#u64
                  else do
                    let i3 ← lift (~~~0#u64)
                    let i4 ← b + 1#u32
                    ok (core.num.U64.wrapping_shl i3 i4)) =
          ok ⟨if b.bv == 63#32 then 0#64 else (~~~0#64) <<< (b.bv + 1#32).setWidth 6⟩ := by
        by_cases hb63 : b = 63#u32
        · have hb63_bv : (b.bv == 63#32) = true := beq_iff_eq.mpr ((u32_eq_iff_bv _ _).mp hb63)
          rw [ite_true hb63, ite_true hb63_bv]
          exact congrArg ok ((u64_eq_iff_bv _ _).mpr rfl)
        · have hb63_ne : b.bv ≠ 63#32 := fun hc => hb63 ((u32_eq_iff_bv _ _).mpr hc)
          have hb63_bv : (b.bv == 63#32) = false := beq_eq_false_iff_ne.mpr hb63_ne
          rw [ite_false hb63]; simp only [hb63_bv, Bool.false_eq_true, ↓reduceIte]
          rw [bind_ok_eq (lift_eq_ok _)]
          have hb_lt63 : b.val < 63 := by
            have hlt : b.bv < 63#32 := by
              rw [hb_bv] at hb63_ne ⊢
              exact sub_63_clz64_lt_63 _ hdiff_ne hb63_ne
            rw [BitVec.lt_def] at hlt; simpa [UScalar.bv_toNat] using hlt
          have hu32max : UScalar.max .U32 = 4294967295 := by simp [U32.max, U32.numBits]
          have h1val : (1#u32).val = 1 := by decide
          obtain ⟨i4, hi4_eq, _, hi4_bv⟩ := u32_add_exists b 1#u32 (by omega)
          rw [bind_ok_eq hi4_eq, u64_wrapping_shl_eq]
          congr 1; apply (u64_eq_iff_bv _ _).mpr
          change (~~~0#64) <<< i4.bv.setWidth 6 = (~~~0#64) <<< (b.bv + 1#32).setWidth 6
          rw [hi4_bv]; rfl
      rw [bind_ok_eq hcm, ← hb_bv]
      simp only [lift, bind_ok, u64_wrapping_shl_eq, u64_one_uscalar_bv]
      set cm : BitVec 64 :=
        if b.bv == 63#32 then 0#64 else (~~~0#64) <<< (b.bv + 1#32).setWidth 6
      have hcond1 :
          (((self.min ^^^ (value &&& ~~~unknown_mask)) &&& (⟨cm⟩ : Std.U64) &&& ~~~unknown_mask) != 0#u64) =
          ((((self.min.bv ^^^ (value.bv &&& ~~~unknown_mask.bv)) &&& cm) &&& ~~~unknown_mask.bv) != 0#64) := by
        cases hbeq : (((self.min.bv ^^^ (value.bv &&& ~~~unknown_mask.bv)) &&& cm) &&& ~~~unknown_mask.bv == 0#64)
        · have hne : ¬((((self.min ^^^ (value &&& ~~~unknown_mask)) &&& (⟨cm⟩ : Std.U64) &&& ~~~unknown_mask) : Std.U64) = 0#u64) := by
            rw [u64_eq_iff_bv]; exact beq_eq_false_iff_ne.mp hbeq
          simp [hne, bne, hbeq]
        · have heq : (((self.min ^^^ (value &&& ~~~unknown_mask)) &&& (⟨cm⟩ : Std.U64) &&& ~~~unknown_mask) : Std.U64) = 0#u64 := by
            rw [u64_eq_iff_bv]; exact beq_iff_eq.mp hbeq
          simp [heq, bne, hbeq]
      rw [hcond1]
      by_cases hc1 : ((((self.min.bv ^^^ (value.bv &&& ~~~unknown_mask.bv)) &&& cm) &&& ~~~unknown_mask.bv) != 0#64) = true
      · rw [ite_true hc1, ite_true hc1]
      · rw [ite_false hc1, ite_false hc1]
        set v2u : Std.U64 := ((value &&& ~~~unknown_mask) &&& ~~~(⟨cm⟩ : Std.U64)) ||| (self.min &&& ⟨cm⟩)
        set um1u : Std.U64 := unknown_mask &&& ~~~(⟨cm⟩ : Std.U64)
        set bitbu : Std.U64 := ⟨1#64 <<< b.bv.setWidth 6⟩
        set v2 : BitVec 64 := ((value.bv &&& ~~~unknown_mask.bv) &&& ~~~cm) ||| (self.min.bv &&& cm)
        set um1 : BitVec 64 := unknown_mask.bv &&& ~~~cm
        set bitb : BitVec 64 := 1#64 <<< b.bv.setWidth 6
        have hv2bv : v2u.bv = v2 := rfl
        have hum1bv : um1u.bv = um1 := rfl
        have hbitbbv : bitbu.bv = bitb := rfl
        have hcond2 : ((um1u &&& bitbu) != 0#u64) = ((um1 &&& bitb) != 0#64) := by
          cases hbeq : ((um1 &&& bitb) == 0#64)
          · have hne : ¬((um1u &&& bitbu) = 0#u64) := by
              rw [u64_eq_iff_bv]; exact beq_eq_false_iff_ne.mp hbeq
            simp [hne, bne, hbeq]
          · have heq : (um1u &&& bitbu) = 0#u64 := by
              rw [u64_eq_iff_bv]; exact beq_iff_eq.mp hbeq
            simp [heq, bne, hbeq]
        rw [hcond2]
        by_cases hc2 : ((um1 &&& bitb) != 0#64) = true
        · rw [ite_true hc2, ite_true hc2]
          by_cases hc3 : (v2u ||| (um1u &&& ~~~bitbu)) ≥ self.min
          · have hc3_bv : self.min.bv ≤ (v2 ||| (um1 &&& ~~~bitb)) := (u64_ge_iff_bv _ _).mp hc3
            rw [ite_true hc3, ite_true hc3_bv]
          · have hc3_bv : ¬(self.min.bv ≤ (v2 ||| (um1 &&& ~~~bitb))) :=
              fun hc => hc3 ((u64_ge_iff_bv _ _).mpr hc)
            rw [ite_false hc3, ite_false hc3_bv]
            congr 1
        · rw [ite_false hc2, ite_false hc2]
          by_cases hc4 : v2u ≤ self.max
          · have hc4_bv : v2 ≤ self.max.bv := (u64_le_iff_bv _ _).mp hc4
            rw [ite_true hc4, ite_true hc4_bv]
            congr 1
          · have hc4_bv : ¬(v2 ≤ self.max.bv) := fun hc => hc4 ((u64_le_iff_bv _ _).mpr hc)
            rw [ite_false hc4, ite_false hc4_bv]

theorem contains_masked_value_32_eq_ok (self : U32Range) (value unknown_mask : Std.U32) :
    U32Range.contains_masked_value self value unknown_mask =
      ok (pureContainsMaskedValue32 self.min.bv self.max.bv value.bv unknown_mask.bv) := by
  unfold U32Range.contains_masked_value pureContainsMaskedValue32
  rw [bind_ok_eq (lift_eq_ok _), bind_ok_eq (lift_eq_ok _)]
  by_cases hgt : self.min > self.max
  · have hgt_bv : self.max.bv < self.min.bv := (u32_gt_iff_bv _ _).mp hgt
    simp [hgt, hgt_bv]
  · have hgt_bv : ¬(self.max.bv < self.min.bv) := fun hc => hgt ((u32_gt_iff_bv _ _).mpr hc)
    simp only [hgt, hgt_bv, ↓reduceIte]
    rw [bind_ok_eq (lift_eq_ok _)]
    by_cases hdiff : (self.min ^^^ self.max) = 0#u32
    · have hdiff_bv : ((self.min.bv ^^^ self.max.bv) == 0#32) = true := by
        rw [beq_iff_eq]; exact (u32_eq_iff_bv _ _).mp hdiff
      simp only [hdiff, hdiff_bv, ↓reduceIte, lift, bind_ok]
      congr 1
      cases hbeq : (((self.min.bv ^^^ (value.bv &&& ~~~unknown_mask.bv)) &&& ~~~unknown_mask.bv) == 0#32)
      · have hne : ¬((((self.min ^^^ (value &&& ~~~unknown_mask)) &&& ~~~unknown_mask) : Std.U32) = 0#u32) := by
          rw [u32_eq_iff_bv]; exact beq_eq_false_iff_ne.mp hbeq
        simp [hne]
      · have heq : (((self.min ^^^ (value &&& ~~~unknown_mask)) &&& ~~~unknown_mask) : Std.U32) = 0#u32 := by
          rw [u32_eq_iff_bv]; exact beq_iff_eq.mp hbeq
        simp [heq]
    · have hdiff_ne : (self.min.bv ^^^ self.max.bv) ≠ 0#32 := by
        intro hc; apply hdiff; exact (u32_eq_iff_bv _ _).mpr hc
      have hdiff_bv : ((self.min.bv ^^^ self.max.bv) == 0#32) = false :=
        beq_eq_false_iff_ne.mpr hdiff_ne
      simp only [hdiff, hdiff_bv, Bool.false_eq_true, ↓reduceIte]
      have h31 : (32#u32 - 1#u32 : Result Std.U32) = ok 31#u32 := by
        rw [uscalar_sub_eq_ok 32#u32 1#u32 (by decide)]
        exact congrArg ok ((u32_eq_iff_bv _ _).mpr (by decide))
      rw [bind_ok_eq h31, bind_ok_eq (lift_eq_ok _)]
      set lz := core.num.U32.leading_zeros (self.min ^^^ self.max)
      have hlz_bv : lz.bv = (self.min.bv ^^^ self.max.bv).clz :=
        leadingZeros32_eq_clz (self.min.bv ^^^ self.max.bv)
      have hlz_le : lz.val ≤ (31#u32).val := by
        have hbvle := clz32_le_31_of_ne_zero _ hdiff_ne
        rw [← hlz_bv] at hbvle
        exact (u32_le_iff_bv lz 31#u32).mpr hbvle
      obtain ⟨b, hb_eq, _, hb_bv0⟩ := u32_sub_exists 31#u32 lz hlz_le
      have hb_bv : b.bv = 31#32 - (self.min.bv ^^^ self.max.bv).clz := by
        rw [hb_bv0, hlz_bv]; rfl
      rw [bind_ok_eq hb_eq]
      have hcm : (if b = 31#u32 then ok 0#u32
                  else do
                    let i3 ← lift (~~~0#u32)
                    let i4 ← b + 1#u32
                    ok (core.num.U32.wrapping_shl i3 i4)) =
          ok ⟨if b.bv == 31#32 then 0#32 else (~~~0#32) <<< (b.bv + 1#32).setWidth 5⟩ := by
        by_cases hb31 : b = 31#u32
        · have hb31_bv : (b.bv == 31#32) = true := beq_iff_eq.mpr ((u32_eq_iff_bv _ _).mp hb31)
          rw [ite_true hb31, ite_true hb31_bv]
          exact congrArg ok ((u32_eq_iff_bv _ _).mpr rfl)
        · have hb31_ne : b.bv ≠ 31#32 := fun hc => hb31 ((u32_eq_iff_bv _ _).mpr hc)
          have hb31_bv : (b.bv == 31#32) = false := beq_eq_false_iff_ne.mpr hb31_ne
          rw [ite_false hb31]; simp only [hb31_bv, Bool.false_eq_true, ↓reduceIte]
          rw [bind_ok_eq (lift_eq_ok _)]
          have hb_lt31 : b.val < 31 := by
            have hlt : b.bv < 31#32 := by
              rw [hb_bv] at hb31_ne ⊢
              exact sub_31_clz32_lt_31 _ hdiff_ne hb31_ne
            rw [BitVec.lt_def] at hlt; simpa [UScalar.bv_toNat] using hlt
          have hu32max : UScalar.max .U32 = 4294967295 := by simp [U32.max, U32.numBits]
          have h1val : (1#u32).val = 1 := by decide
          obtain ⟨i4, hi4_eq, _, hi4_bv⟩ := u32_add_exists b 1#u32 (by omega)
          rw [bind_ok_eq hi4_eq, u32_wrapping_shl_eq]
          congr 1; apply (u32_eq_iff_bv _ _).mpr
          change (~~~0#32) <<< i4.bv.setWidth 5 = (~~~0#32) <<< (b.bv + 1#32).setWidth 5
          rw [hi4_bv]; rfl
      rw [bind_ok_eq hcm, ← hb_bv]
      simp only [lift, bind_ok, u32_wrapping_shl_eq, u32_one_uscalar_bv]
      set cm : BitVec 32 :=
        if b.bv == 31#32 then 0#32 else (~~~0#32) <<< (b.bv + 1#32).setWidth 5
      have hcond1 :
          (((self.min ^^^ (value &&& ~~~unknown_mask)) &&& (⟨cm⟩ : Std.U32) &&& ~~~unknown_mask) != 0#u32) =
          ((((self.min.bv ^^^ (value.bv &&& ~~~unknown_mask.bv)) &&& cm) &&& ~~~unknown_mask.bv) != 0#32) := by
        cases hbeq : (((self.min.bv ^^^ (value.bv &&& ~~~unknown_mask.bv)) &&& cm) &&& ~~~unknown_mask.bv == 0#32)
        · have hne : ¬((((self.min ^^^ (value &&& ~~~unknown_mask)) &&& (⟨cm⟩ : Std.U32) &&& ~~~unknown_mask) : Std.U32) = 0#u32) := by
            rw [u32_eq_iff_bv]; exact beq_eq_false_iff_ne.mp hbeq
          simp [hne, bne, hbeq]
        · have heq : (((self.min ^^^ (value &&& ~~~unknown_mask)) &&& (⟨cm⟩ : Std.U32) &&& ~~~unknown_mask) : Std.U32) = 0#u32 := by
            rw [u32_eq_iff_bv]; exact beq_iff_eq.mp hbeq
          simp [heq, bne, hbeq]
      rw [hcond1]
      by_cases hc1 : ((((self.min.bv ^^^ (value.bv &&& ~~~unknown_mask.bv)) &&& cm) &&& ~~~unknown_mask.bv) != 0#32) = true
      · rw [ite_true hc1, ite_true hc1]
      · rw [ite_false hc1, ite_false hc1]
        set v2u : Std.U32 := ((value &&& ~~~unknown_mask) &&& ~~~(⟨cm⟩ : Std.U32)) ||| (self.min &&& ⟨cm⟩)
        set um1u : Std.U32 := unknown_mask &&& ~~~(⟨cm⟩ : Std.U32)
        set bitbu : Std.U32 := ⟨1#32 <<< b.bv.setWidth 5⟩
        set v2 : BitVec 32 := ((value.bv &&& ~~~unknown_mask.bv) &&& ~~~cm) ||| (self.min.bv &&& cm)
        set um1 : BitVec 32 := unknown_mask.bv &&& ~~~cm
        set bitb : BitVec 32 := 1#32 <<< b.bv.setWidth 5
        have hv2bv : v2u.bv = v2 := rfl
        have hum1bv : um1u.bv = um1 := rfl
        have hbitbbv : bitbu.bv = bitb := rfl
        have hcond2 : ((um1u &&& bitbu) != 0#u32) = ((um1 &&& bitb) != 0#32) := by
          cases hbeq : ((um1 &&& bitb) == 0#32)
          · have hne : ¬((um1u &&& bitbu) = 0#u32) := by
              rw [u32_eq_iff_bv]; exact beq_eq_false_iff_ne.mp hbeq
            simp [hne, bne, hbeq]
          · have heq : (um1u &&& bitbu) = 0#u32 := by
              rw [u32_eq_iff_bv]; exact beq_iff_eq.mp hbeq
            simp [heq, bne, hbeq]
        rw [hcond2]
        by_cases hc2 : ((um1 &&& bitb) != 0#32) = true
        · rw [ite_true hc2, ite_true hc2]
          by_cases hc3 : (v2u ||| (um1u &&& ~~~bitbu)) ≥ self.min
          · have hc3_bv : self.min.bv ≤ (v2 ||| (um1 &&& ~~~bitb)) := (u32_ge_iff_bv _ _).mp hc3
            rw [ite_true hc3, ite_true hc3_bv]
          · have hc3_bv : ¬(self.min.bv ≤ (v2 ||| (um1 &&& ~~~bitb))) :=
              fun hc => hc3 ((u32_ge_iff_bv _ _).mpr hc)
            rw [ite_false hc3, ite_false hc3_bv]
            congr 1
        · rw [ite_false hc2, ite_false hc2]
          by_cases hc4 : v2u ≤ self.max
          · have hc4_bv : v2 ≤ self.max.bv := (u32_le_iff_bv _ _).mp hc4
            rw [ite_true hc4, ite_true hc4_bv]
            congr 1
          · have hc4_bv : ¬(v2 ≤ self.max.bv) := fun hc => hc4 ((u32_le_iff_bv _ _).mpr hc)
            rw [ite_false hc4, ite_false hc4_bv]

theorem pureContainsMaskedValue64_complete (rmin rmax val um x : BitVec 64)
    (hmin : rmin ≤ x) (hmax : x ≤ rmax) (ht : x &&& ~~~um = val &&& ~~~um) :
    pureContainsMaskedValue64 rmin rmax val um = true := by
  unfold pureContainsMaskedValue64
  simp only []
  split_ifs <;> bv_decide

theorem pureContainsMaskedValue32_complete (rmin rmax val um x : BitVec 32)
    (hmin : rmin ≤ x) (hmax : x ≤ rmax) (ht : x &&& ~~~um = val &&& ~~~um) :
    pureContainsMaskedValue32 rmin rmax val um = true := by
  unfold pureContainsMaskedValue32
  simp only []
  split_ifs <;> bv_decide

theorem try_new_64_eq_ok (value unknown_mask unwritten_mask : Std.U64) (urange : U64Range) :
    U64ScalarValueData.try_new value unknown_mask unwritten_mask urange =
      ok (if pureContainsMaskedValue64 urange.min.bv urange.max.bv value.bv unknown_mask.bv then
            some (pureNew64 value.bv unknown_mask.bv unwritten_mask.bv urange.min.bv urange.max.bv)
          else none) := by
  unfold U64ScalarValueData.try_new
  rw [bind_ok_eq (contains_masked_value_64_eq_ok urange value unknown_mask)]
  cases pureContainsMaskedValue64 urange.min.bv urange.max.bv value.bv unknown_mask.bv
  · rfl
  · simp only [↓reduceIte, bind_ok_eq (new64_eq_ok value unknown_mask unwritten_mask urange)]

theorem try_new_32_eq_ok (value unknown_mask unwritten_mask : Std.U32) (urange : U32Range) :
    U32ScalarValueData.try_new value unknown_mask unwritten_mask urange =
      ok (if pureContainsMaskedValue32 urange.min.bv urange.max.bv value.bv unknown_mask.bv then
            some (pureNew32 value.bv unknown_mask.bv unwritten_mask.bv urange.min.bv urange.max.bv)
          else none) := by
  unfold U32ScalarValueData.try_new
  rw [bind_ok_eq (contains_masked_value_32_eq_ok urange value unknown_mask)]
  cases pureContainsMaskedValue32 urange.min.bv urange.max.bv value.bv unknown_mask.bv
  · rfl
  · simp only [↓reduceIte, bind_ok_eq (new32_eq_ok value unknown_mask unwritten_mask urange)]

theorem try_new_64_valid {value unknown_mask unwritten_mask : Std.U64} {urange : U64Range}
    {res : U64ScalarValueData}
    (h : U64ScalarValueData.try_new value unknown_mask unwritten_mask urange = ok (some res)) :
    Scalar64Valid res := by
  rw [try_new_64_eq_ok] at h
  have hinj := ok_inj h
  split_ifs at hinj
  cases hinj
  exact pureNew64_valid _ _ _ _ _

theorem try_new_64_none_unsat {value unknown_mask unwritten_mask : Std.U64} {urange : U64Range}
    (h : U64ScalarValueData.try_new value unknown_mask unwritten_mask urange = ok none)
    {x : BitVec 64} (hr : ConcretizesRange64 urange x)
    (ht : x &&& ~~~unknown_mask.bv = value.bv &&& ~~~unknown_mask.bv) :
    False := by
  rw [try_new_64_eq_ok] at h
  have hinj := ok_inj h
  have hcomp := pureContainsMaskedValue64_complete urange.min.bv urange.max.bv
    value.bv unknown_mask.bv x hr.1 hr.2 ht
  simp [hcomp] at hinj

theorem try_new_64_some_sound_2safety {value unknown_mask unwritten_mask : Std.U64}
    {urange : U64Range} {res : U64ScalarValueData}
    (h : U64ScalarValueData.try_new value unknown_mask unwritten_mask urange = ok (some res))
    {x₁ x₂ : BitVec 64}
    (hr1 : ConcretizesRange64 urange x₁)
    (ht1 : x₁ &&& ~~~unknown_mask.bv = value.bv &&& ~~~unknown_mask.bv)
    (hr2 : ConcretizesRange64 urange x₂)
    (ht2 : x₂ &&& ~~~unknown_mask.bv = value.bv &&& ~~~unknown_mask.bv)
    (hsec : x₁ &&& ~~~unwritten_mask.bv = x₂ &&& ~~~unwritten_mask.bv) :
    ConcretizesScalar64Rel2 res x₁ x₂ := by
  rw [try_new_64_eq_ok] at h
  have hinj := ok_inj h
  split_ifs at hinj
  cases hinj
  obtain ⟨d, hd, hc⟩ := new64_sound_2safety value unknown_mask unwritten_mask urange x₁ x₂
    hr1 ht1 hr2 ht2 hsec
  rw [new64_eq_ok] at hd
  cases ok_inj hd
  exact hc

theorem try_new_32_none_unsat {value unknown_mask unwritten_mask : Std.U32} {urange : U32Range}
    (h : U32ScalarValueData.try_new value unknown_mask unwritten_mask urange = ok none)
    {x : BitVec 32} (hr : ConcretizesRange32 urange x)
    (ht : x &&& ~~~unknown_mask.bv = value.bv &&& ~~~unknown_mask.bv) :
    False := by
  rw [try_new_32_eq_ok] at h
  have hinj := ok_inj h
  have hcomp := pureContainsMaskedValue32_complete urange.min.bv urange.max.bv
    value.bv unknown_mask.bv x hr.1 hr.2 ht
  simp [hcomp] at hinj

theorem try_new_32_some_sound_2safety {value unknown_mask unwritten_mask : Std.U32}
    {urange : U32Range} {res : U32ScalarValueData}
    (h : U32ScalarValueData.try_new value unknown_mask unwritten_mask urange = ok (some res))
    {x₁ x₂ : BitVec 32}
    (hr1 : ConcretizesRange32 urange x₁)
    (ht1 : x₁ &&& ~~~unknown_mask.bv = value.bv &&& ~~~unknown_mask.bv)
    (hr2 : ConcretizesRange32 urange x₂)
    (ht2 : x₂ &&& ~~~unknown_mask.bv = value.bv &&& ~~~unknown_mask.bv)
    (hsec : x₁ &&& ~~~unwritten_mask.bv = x₂ &&& ~~~unwritten_mask.bv) :
    ConcretizesScalar32Rel2 res x₁ x₂ := by
  rw [try_new_32_eq_ok] at h
  have hinj := ok_inj h
  split_ifs at hinj
  cases hinj
  obtain ⟨d, hd, hc⟩ := new32_sound_2safety value unknown_mask unwritten_mask urange x₁ x₂
    hr1 ht1 hr2 ht2 hsec
  rw [new32_eq_ok] at hd
  cases ok_inj hd
  exact hc

theorem is_zero_64_eq_ok (self : U64ScalarValueData) :
    U64ScalarValueData.is_zero self =
      ok (decide (self.unknown_mask = 0#u64) && decide (self.value = 0#u64)) := by
  unfold U64ScalarValueData.is_zero U64ScalarValueData.is_known
  rw [bind_ok_eq rfl]
  by_cases hk : self.unknown_mask = 0#u64 <;> simp [hk]

theorem is_zero_64_sound {self : U64ScalarValueData} {x : BitVec 64}
    (hz : U64ScalarValueData.is_zero self = ok true) (hx : ConcretizesScalar64 self x) :
    x = 0#64 := by
  rw [is_zero_64_eq_ok] at hz
  have hb := ok_inj hz
  simp only [Bool.and_eq_true, decide_eq_true_eq] at hb
  have heq := eq_value_of_known_64 hb.1 hx
  rw [heq, hb.2]; rfl

theorem is_zero_32_eq_ok (self : U32ScalarValueData) :
    U32ScalarValueData.is_zero self =
      ok (decide (self.unknown_mask = 0#u32) && decide (self.value = 0#u32)) := by
  unfold U32ScalarValueData.is_zero U32ScalarValueData.is_known
  rw [bind_ok_eq rfl]
  by_cases hk : self.unknown_mask = 0#u32 <;> simp [hk]

theorem is_zero_32_sound {self : U32ScalarValueData} {x : BitVec 32}
    (hz : U32ScalarValueData.is_zero self = ok true) (hx : ConcretizesScalar32 self x) :
    x = 0#32 := by
  rw [is_zero_32_eq_ok] at hz
  have hb := ok_inj hz
  simp only [Bool.and_eq_true, decide_eq_true_eq] at hb
  have heq := eq_value_of_known_32 hb.1 hx
  rw [heq, hb.2]; rfl

theorem is_uninitialized_64_eq_ok (self : U64ScalarValueData) :
    U64ScalarValueData.is_uninitialized self =
      ok (decide (self.unwritten_mask = core.num.U64.MAX)) := rfl

theorem is_uninitialized_32_eq_ok (self : U32ScalarValueData) :
    U32ScalarValueData.is_uninitialized self =
      ok (decide (self.unwritten_mask = core.num.U32.MAX)) := rfl

private theorem can_be_equal_mask_helper_64 (av am bv bm x : BitVec 64)
    (hax : x &&& ~~~am = av) (hbx : x &&& ~~~bm = bv)
    (ha_disj : av &&& am = 0#64) (hb_disj : bv &&& bm = 0#64) :
    ((av ^^^ bv) &&& ~~~(am ||| bm) = 0#64) ∧
    (x &&& ~~~(am &&& bm) = (av ||| bv) &&& ~~~(am &&& bm)) := by
  refine ⟨?_, ?_⟩ <;> bv_decide

private theorem can_be_equal_mask_helper_32 (av am bv bm x : BitVec 32)
    (hax : x &&& ~~~am = av) (hbx : x &&& ~~~bm = bv)
    (ha_disj : av &&& am = 0#32) (hb_disj : bv &&& bm = 0#32) :
    ((av ^^^ bv) &&& ~~~(am ||| bm) = 0#32) ∧
    (x &&& ~~~(am &&& bm) = (av ||| bv) &&& ~~~(am &&& bm)) := by
  refine ⟨?_, ?_⟩ <;> bv_decide

theorem can_be_equal_64_ok (self other : U64ScalarValueData) :
    ∃ b, U64ScalarValueData.can_be_equal self other = ok b := by
  unfold U64ScalarValueData.can_be_equal U64ScalarValueData.min U64ScalarValueData.max U64Range.new
  simp only [lift, bind_ok]
  split_ifs
  · exact ⟨false, rfl⟩
  · rw [bind_ok_eq (max_u64_eq_ok self.urange.min other.urange.min)]
    rw [bind_ok_eq (min_u64_eq_ok self.urange.max other.urange.max)]
    exact ⟨_, contains_masked_value_64_eq_ok _ _ _⟩

theorem can_be_equal_64_false_sound {self other : U64ScalarValueData} {x y : BitVec 64}
    (hf : U64ScalarValueData.can_be_equal self other = ok false)
    (hx : ConcretizesScalar64 self x) (hy : ConcretizesScalar64 other y) :
    x ≠ y := by
  intro hxy
  subst hxy
  rcases hx with ⟨⟨ha_disj, _, _, _, _⟩, hax, hamin, hamax⟩
  rcases hy with ⟨⟨hb_disj, _, _, _, _⟩, hbx, hbmin, hbmax⟩
  obtain ⟨hdiff0, hcomb⟩ :=
    can_be_equal_mask_helper_64 self.value.bv self.unknown_mask.bv
      other.value.bv other.unknown_mask.bv x hax hbx ha_disj hb_disj
  unfold U64ScalarValueData.can_be_equal U64ScalarValueData.min U64ScalarValueData.max
    U64Range.new at hf
  simp only [lift, bind_ok] at hf
  have hne_false : (((self.value ^^^ other.value) &&& ~~~(self.unknown_mask ||| other.unknown_mask)) != 0#u64) = false := by
    have heq : ((self.value ^^^ other.value) &&& ~~~(self.unknown_mask ||| other.unknown_mask)) = 0#u64 := by
      rw [u64_eq_iff_bv]; exact hdiff0
    simp [heq, bne]
  rw [hne_false] at hf
  simp only [Bool.false_eq_true, ↓reduceIte] at hf
  rw [bind_ok_eq (max_u64_eq_ok self.urange.min other.urange.min)] at hf
  rw [bind_ok_eq (min_u64_eq_ok self.urange.max other.urange.max)] at hf
  rw [contains_masked_value_64_eq_ok] at hf
  have hinj := ok_inj hf
  have hrmin : (if other.urange.min.bv ≤ self.urange.min.bv then self.urange.min.bv
                else other.urange.min.bv) ≤ x := by
    split_ifs <;> assumption
  have hrmax : x ≤ (if self.urange.max.bv ≤ other.urange.max.bv then self.urange.max.bv
                    else other.urange.max.bv) := by
    split_ifs <;> assumption
  have hcomp := pureContainsMaskedValue64_complete
    (if other.urange.min.bv ≤ self.urange.min.bv then self.urange.min.bv else other.urange.min.bv)
    (if self.urange.max.bv ≤ other.urange.max.bv then self.urange.max.bv else other.urange.max.bv)
    (self.value.bv ||| other.value.bv)
    (self.unknown_mask.bv &&& other.unknown_mask.bv)
    x hrmin hrmax hcomb
  exact Bool.noConfusion (hinj.symm.trans hcomp)

theorem can_be_equal_32_ok (self other : U32ScalarValueData) :
    ∃ b, U32ScalarValueData.can_be_equal self other = ok b := by
  unfold U32ScalarValueData.can_be_equal U32ScalarValueData.min U32ScalarValueData.max U32Range.new
  simp only [lift, bind_ok]
  split_ifs
  · exact ⟨false, rfl⟩
  · rw [bind_ok_eq (max_u32_eq_ok self.urange.min other.urange.min)]
    rw [bind_ok_eq (min_u32_eq_ok self.urange.max other.urange.max)]
    exact ⟨_, contains_masked_value_32_eq_ok _ _ _⟩

theorem can_be_equal_32_false_sound {self other : U32ScalarValueData} {x y : BitVec 32}
    (hf : U32ScalarValueData.can_be_equal self other = ok false)
    (hx : ConcretizesScalar32 self x) (hy : ConcretizesScalar32 other y) :
    x ≠ y := by
  intro hxy
  subst hxy
  rcases hx with ⟨⟨ha_disj, _, _, _, _⟩, hax, hamin, hamax⟩
  rcases hy with ⟨⟨hb_disj, _, _, _, _⟩, hbx, hbmin, hbmax⟩
  obtain ⟨hdiff0, hcomb⟩ :=
    can_be_equal_mask_helper_32 self.value.bv self.unknown_mask.bv
      other.value.bv other.unknown_mask.bv x hax hbx ha_disj hb_disj
  unfold U32ScalarValueData.can_be_equal U32ScalarValueData.min U32ScalarValueData.max
    U32Range.new at hf
  simp only [lift, bind_ok] at hf
  have hne_false : (((self.value ^^^ other.value) &&& ~~~(self.unknown_mask ||| other.unknown_mask)) != 0#u32) = false := by
    have heq : ((self.value ^^^ other.value) &&& ~~~(self.unknown_mask ||| other.unknown_mask)) = 0#u32 := by
      rw [u32_eq_iff_bv]; exact hdiff0
    simp [heq, bne]
  rw [hne_false] at hf
  simp only [Bool.false_eq_true, ↓reduceIte] at hf
  rw [bind_ok_eq (max_u32_eq_ok self.urange.min other.urange.min)] at hf
  rw [bind_ok_eq (min_u32_eq_ok self.urange.max other.urange.max)] at hf
  rw [contains_masked_value_32_eq_ok] at hf
  have hinj := ok_inj hf
  have hrmin : (if other.urange.min.bv ≤ self.urange.min.bv then self.urange.min.bv
                else other.urange.min.bv) ≤ x := by
    split_ifs <;> assumption
  have hrmax : x ≤ (if self.urange.max.bv ≤ other.urange.max.bv then self.urange.max.bv
                    else other.urange.max.bv) := by
    split_ifs <;> assumption
  have hcomp := pureContainsMaskedValue32_complete
    (if other.urange.min.bv ≤ self.urange.min.bv then self.urange.min.bv else other.urange.min.bv)
    (if self.urange.max.bv ≤ other.urange.max.bv then self.urange.max.bv else other.urange.max.bv)
    (self.value.bv ||| other.value.bv)
    (self.unknown_mask.bv &&& other.unknown_mask.bv)
    x hrmin hrmax hcomb
  exact Bool.noConfusion (hinj.symm.trans hcomp)

/-! ## 5. `PartialEq`, `PartialOrd`, and `BroaderOrEqual64` -/

theorem u64_range_eq_sound (a b : U64Range) :
    ∃ r : Bool,
      U64Range.Insts.CoreCmpPartialEqU64Range.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  cases a with | mk amin amax =>
  cases b with | mk bmin bmax =>
  unfold U64Range.Insts.CoreCmpPartialEqU64Range.eq
  by_cases h1 : amin = bmin <;> by_cases h2 : amax = bmax <;> simp [h1, h2]

theorem u64_scalar_eq_sound (a b : U64ScalarValueData) :
    ∃ r : Bool,
      U64ScalarValueData.Insts.CoreCmpPartialEqU64ScalarValueData.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  cases a with | mk av aum aunw aur ag =>
  cases b with | mk bv bum bunw bur bg =>
  cases ag; cases bg
  unfold U64ScalarValueData.Insts.CoreCmpPartialEqU64ScalarValueData.eq
  have hunit : core.cmp.impls.PartialEqUnit.eq () () = ok true := rfl
  by_cases h1 : av = bv
  · subst h1; simp only [↓reduceIte]
    by_cases h2 : aum = bum
    · subst h2; simp only [↓reduceIte]
      by_cases h3 : aunw = bunw
      · subst h3; simp only [↓reduceIte, bind_ok_eq hunit]
        obtain ⟨r, hr, hiff⟩ := u64_range_eq_sound aur bur
        exact ⟨r, hr, by simp [hiff]⟩
      · exact ⟨false, by simp [h3], by simp [h3]⟩
    · exact ⟨false, by simp [h2], by simp [h2]⟩
  · exact ⟨false, by simp [h1], by simp [h1]⟩

/-- Information-order broadening predicate on `U64ScalarValueData`: `d₁` is at least
as broad (permits at least all concretizations of) `d₂`. -/
def BroaderOrEqual64 (d₁ d₂ : U64ScalarValueData) : Prop :=
  d₁.unwritten_mask.bv &&& d₂.unwritten_mask.bv = d₂.unwritten_mask.bv ∧
  d₁.unknown_mask.bv &&& d₂.unknown_mask.bv = d₂.unknown_mask.bv ∧
  d₁.value.bv &&& ~~~d₁.unknown_mask.bv = d₂.value.bv &&& ~~~d₁.unknown_mask.bv ∧
  d₁.urange.min.bv ≤ d₂.urange.min.bv ∧
  d₂.urange.max.bv ≤ d₁.urange.max.bv

theorem BroaderOrEqual64.refl (d : U64ScalarValueData) : BroaderOrEqual64 d d := by
  refine ⟨by simp, by simp, rfl, BitVec.le_refl _, BitVec.le_refl _⟩

theorem BroaderOrEqual64.trans {a b c : U64ScalarValueData}
    (hab : BroaderOrEqual64 a b) (hbc : BroaderOrEqual64 b c) :
    BroaderOrEqual64 a c := by
  rcases hab with ⟨huw1, hum1, hv1, hmin1, hmax1⟩
  rcases hbc with ⟨huw2, hum2, hv2, hmin2, hmax2⟩
  refine ⟨?_, ?_, ?_, ?_, ?_⟩
  · generalize a.unwritten_mask.bv = uwa, b.unwritten_mask.bv = uwb, c.unwritten_mask.bv = uwc at *
    revert huw1 huw2; bv_decide
  · generalize a.unknown_mask.bv = uma, b.unknown_mask.bv = umb, c.unknown_mask.bv = umc at *
    revert hum1 hum2; bv_decide
  · generalize a.value.bv = va, b.value.bv = vb, c.value.bv = vc,
      a.unknown_mask.bv = uma, b.unknown_mask.bv = umb, c.unknown_mask.bv = umc at *
    revert hum1 hv1 hv2; bv_decide
  · generalize a.urange.min.bv = ma, b.urange.min.bv = mb, c.urange.min.bv = mc at *
    revert hmin1 hmin2; bv_decide
  · generalize a.urange.max.bv = ma, b.urange.max.bv = mb, c.urange.max.bv = mc at *
    revert hmax1 hmax2; bv_decide

theorem BroaderOrEqual64.of_uninitialized {u : U64ScalarValueData}
    (hu : U64ScalarValueData.UNINITIALIZED = ok u) (d : U64ScalarValueData) :
    BroaderOrEqual64 u d := by
  unfold U64ScalarValueData.UNINITIALIZED U64Range.impl.max U64Range.new at hu
  rw [bind_ok_eq rfl, new64_eq_ok] at hu
  cases ok_inj hu
  unfold BroaderOrEqual64 pureNew64
  simp only [u64_zero_bv, u64_max_uscalar_bv, u64_mk_bv]
  generalize d.unwritten_mask.bv = duw, d.unknown_mask.bv = dum, d.value.bv = dv,
    d.urange.min.bv = dmin, d.urange.max.bv = dmax
  refine ⟨?_, ?_, ?_, ?_, ?_⟩ <;> bv_decide

theorem BroaderOrEqual64.monotone_rel2 {d₁ d₂ : U64ScalarValueData} {x₁ x₂ : BitVec 64}
    (hv1 : Scalar64Valid d₁)
    (hbr : BroaderOrEqual64 d₁ d₂)
    (hc2 : ConcretizesScalar64Rel2 d₂ x₁ x₂) :
    ConcretizesScalar64Rel2 d₁ x₁ x₂ := by
  rcases hv1 with ⟨h1_disj, h1_unwr, h1_vmin, h1_vmax⟩
  rcases hbr with ⟨hbr_unwr, hbr_unk, hbr_val, hbr_min, hbr_max⟩
  rcases hc2 with ⟨⟨⟨h2_disj, _, _, _, _⟩, ht1, hmin1, hmax1⟩, ⟨_, ht2, hmin2, hmax2⟩, hsec⟩
  have h1_nonempty : d₁.urange.min.bv ≤ d₁.urange.max.bv := by
    generalize d₁.urange.min.bv = m1, d₁.urange.max.bv = x1max,
      d₂.urange.min.bv = m2, d₂.urange.max.bv = x2max at *
    revert hbr_min hbr_max hmin1 hmax1; bv_decide
  have hcons1 : Scalar64Consistent d₁ := ⟨h1_disj, h1_unwr, h1_nonempty, h1_vmin, h1_vmax⟩
  refine ⟨⟨hcons1, ?_, ?_, ?_⟩, ⟨hcons1, ?_, ?_, ?_⟩, ?_⟩
  · generalize d₁.value.bv = v1, d₁.unknown_mask.bv = um1,
      d₂.value.bv = v2, d₂.unknown_mask.bv = um2 at *
    revert h1_disj hbr_unk hbr_val ht1; bv_decide
  · generalize d₁.urange.min.bv = m1, d₂.urange.min.bv = m2 at *
    revert hbr_min hmin1; bv_decide
  · generalize d₁.urange.max.bv = m1, d₂.urange.max.bv = m2 at *
    revert hbr_max hmax1; bv_decide
  · generalize d₁.value.bv = v1, d₁.unknown_mask.bv = um1,
      d₂.value.bv = v2, d₂.unknown_mask.bv = um2 at *
    revert h1_disj hbr_unk hbr_val ht2; bv_decide
  · generalize d₁.urange.min.bv = m1, d₂.urange.min.bv = m2 at *
    revert hbr_min hmin2; bv_decide
  · generalize d₁.urange.max.bv = m1, d₂.urange.max.bv = m2 at *
    revert hbr_max hmax2; bv_decide
  · generalize d₁.unwritten_mask.bv = uw1, d₂.unwritten_mask.bv = uw2 at *
    revert hbr_unwr hsec; bv_decide

theorem u64_range_partial_cmp_sound (a b : U64Range) :
    ∃ opt,
      U64Range.Insts.CoreCmpPartialOrdU64Range.partial_cmp a b = ok opt ∧
      (opt = some .eq → a = b) ∧
      (opt = some .gt → a.min.bv ≤ b.min.bv ∧ b.max.bv ≤ a.max.bv) ∧
      (opt = some .lt → b.min.bv ≤ a.min.bv ∧ a.max.bv ≤ b.max.bv) := by
  unfold U64Range.Insts.CoreCmpPartialOrdU64Range.partial_cmp
  obtain ⟨req, hreq_ok, hreq_iff⟩ := u64_range_eq_sound a b
  rw [bind_ok_eq hreq_ok]
  cases req with
  | true =>
    have hab : a = b := hreq_iff.mp rfl
    subst hab
    exact ⟨some .eq, rfl, fun _ => rfl, by simp, by simp⟩
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte]
    by_cases h1 : a.min ≤ b.min
    · by_cases h2 : a.max ≥ b.max
      · simp only [h1, h2, ↓reduceIte]
        exact ⟨some .gt, rfl, by simp,
          fun _ => ⟨(u64_le_iff_bv _ _).mp h1, (u64_ge_iff_bv _ _).mp h2⟩, by simp⟩
      · simp only [h1, h2, ↓reduceIte]
        by_cases h3 : a.min ≥ b.min
        · by_cases h4 : a.max ≤ b.max
          · simp only [h3, h4, ↓reduceIte]
            exact ⟨some .lt, rfl, by simp, by simp,
              fun _ => ⟨(u64_ge_iff_bv _ _).mp h3, (u64_le_iff_bv _ _).mp h4⟩⟩
          · simp only [h3, h4, ↓reduceIte]
            exact ⟨none, rfl, by simp, by simp, by simp⟩
        · simp only [h3, ↓reduceIte]
          exact ⟨none, rfl, by simp, by simp, by simp⟩
    · simp only [h1, ↓reduceIte]
      by_cases h3 : a.min ≥ b.min
      · by_cases h4 : a.max ≤ b.max
        · simp only [h3, h4, ↓reduceIte]
          exact ⟨some .lt, rfl, by simp, by simp,
            fun _ => ⟨(u64_ge_iff_bv _ _).mp h3, (u64_le_iff_bv _ _).mp h4⟩⟩
        · simp only [h3, h4, ↓reduceIte]
          exact ⟨none, rfl, by simp, by simp, by simp⟩
      · simp only [h3, ↓reduceIte]
        exact ⟨none, rfl, by simp, by simp, by simp⟩

theorem u64_scalar_partial_cmp_sound (a b : U64ScalarValueData) :
    ∃ opt,
      U64ScalarValueData.Insts.CoreCmpPartialOrdU64ScalarValueData.partial_cmp a b = ok opt ∧
      (opt = some .eq → a = b ∧ BroaderOrEqual64 a b ∧ BroaderOrEqual64 b a) ∧
      (opt = some .gt → BroaderOrEqual64 a b) ∧
      (opt = some .lt → BroaderOrEqual64 b a) := by
  unfold U64ScalarValueData.Insts.CoreCmpPartialOrdU64ScalarValueData.partial_cmp
  obtain ⟨seq, hseq_ok, hseq_iff⟩ := u64_scalar_eq_sound a b
  rw [bind_ok_eq hseq_ok]
  cases seq with
  | true =>
    have hab : a = b := hseq_iff.mp rfl
    subst hab
    exact ⟨some .eq, rfl, fun _ => ⟨rfl, BroaderOrEqual64.refl a, BroaderOrEqual64.refl a⟩,
      by simp, by simp⟩
  | false =>
    simp only [Bool.false_eq_true, ↓reduceIte, lift, bind_ok]
    obtain ⟨ropt, hropt, hreq, hrgt, hrlt⟩ := u64_range_partial_cmp_sound a.urange b.urange
    by_cases g1 : (a.unwritten_mask &&& b.unwritten_mask) = b.unwritten_mask
    · simp only [g1, ↓reduceIte]
      by_cases g2 : (a.unknown_mask &&& b.unknown_mask) = b.unknown_mask
      · simp only [g2, ↓reduceIte]
        by_cases g3 : (a.value &&& ~~~a.unknown_mask) = (b.value &&& ~~~a.unknown_mask)
        · simp only [g3, ↓reduceIte, bind_ok_eq hropt]
          cases ropt with
          | none =>
            simp only [bind_ok]
            by_cases l1 : (b.unwritten_mask &&& a.unwritten_mask) = a.unwritten_mask <;>
            by_cases l2 : (b.unknown_mask &&& a.unknown_mask) = a.unknown_mask <;>
            by_cases l3 : (a.value &&& ~~~b.unknown_mask) = (b.value &&& ~~~b.unknown_mask) <;>
            simp [l1, l2, l3]
          | some rord =>
            cases rord with
            | eq =>
              simp only [bind_ok, ↓reduceIte]
              have hr := hreq rfl
              refine ⟨some .gt, rfl, by simp, fun _ => ?_, by simp⟩
              exact ⟨(u64_eq_iff_bv _ _).mp g1, (u64_eq_iff_bv _ _).mp g2,
                (u64_eq_iff_bv _ _).mp g3, by rw [hr]; exact BitVec.le_refl _,
                by rw [hr]; exact BitVec.le_refl _⟩
            | gt =>
              simp only [bind_ok, ↓reduceIte]
              obtain ⟨hmin, hmax⟩ := hrgt rfl
              refine ⟨some .gt, rfl, by simp, fun _ => ?_, by simp⟩
              exact ⟨(u64_eq_iff_bv _ _).mp g1, (u64_eq_iff_bv _ _).mp g2,
                (u64_eq_iff_bv _ _).mp g3, hmin, hmax⟩
            | lt =>
              simp only [bind_ok, Bool.false_eq_true, ↓reduceIte]
              obtain ⟨hmin, hmax⟩ := hrlt rfl
              by_cases l1 : (b.unwritten_mask &&& a.unwritten_mask) = a.unwritten_mask <;>
              by_cases l2 : (b.unknown_mask &&& a.unknown_mask) = a.unknown_mask <;>
              by_cases l3 : (a.value &&& ~~~b.unknown_mask) = (b.value &&& ~~~b.unknown_mask)
              · simp only [l1, l2, l3, ↓reduceIte]
                refine ⟨some .lt, rfl, by simp, by simp, fun _ => ?_⟩
                exact ⟨(u64_eq_iff_bv _ _).mp l1, (u64_eq_iff_bv _ _).mp l2,
                  ((u64_eq_iff_bv _ _).mp l3).symm, hmin, hmax⟩
              all_goals { simp [l1, l2, l3] }
        · simp only [g3, ↓reduceIte]
          by_cases l1 : (b.unwritten_mask &&& a.unwritten_mask) = a.unwritten_mask <;>
          by_cases l2 : (b.unknown_mask &&& a.unknown_mask) = a.unknown_mask <;>
          by_cases l3 : (a.value &&& ~~~b.unknown_mask) = (b.value &&& ~~~b.unknown_mask)
          · simp only [l1, l2, l3, ↓reduceIte, bind_ok_eq hropt]
            cases ropt with
            | none => simp [bind_ok]
            | some rord =>
              cases rord with
              | gt => simp [bind_ok]
              | eq =>
                simp only [bind_ok, ↓reduceIte]
                have hr := hreq rfl
                refine ⟨some .lt, rfl, by simp, by simp, fun _ => ?_⟩
                exact ⟨(u64_eq_iff_bv _ _).mp l1, (u64_eq_iff_bv _ _).mp l2,
                  ((u64_eq_iff_bv _ _).mp l3).symm, by rw [hr]; exact BitVec.le_refl _,
                  by rw [hr]; exact BitVec.le_refl _⟩
              | lt =>
                simp only [bind_ok, ↓reduceIte]
                obtain ⟨hmin, hmax⟩ := hrlt rfl
                refine ⟨some .lt, rfl, by simp, by simp, fun _ => ?_⟩
                exact ⟨(u64_eq_iff_bv _ _).mp l1, (u64_eq_iff_bv _ _).mp l2,
                  ((u64_eq_iff_bv _ _).mp l3).symm, hmin, hmax⟩
          all_goals { simp [l1, l2, l3] }
      · simp only [g2, ↓reduceIte]
        by_cases l1 : (b.unwritten_mask &&& a.unwritten_mask) = a.unwritten_mask <;>
        by_cases l2 : (b.unknown_mask &&& a.unknown_mask) = a.unknown_mask <;>
        by_cases l3 : (a.value &&& ~~~b.unknown_mask) = (b.value &&& ~~~b.unknown_mask)
        · simp only [l1, l2, l3, ↓reduceIte, bind_ok_eq hropt]
          cases ropt with
          | none => simp [bind_ok]
          | some rord =>
            cases rord with
            | gt => simp [bind_ok]
            | eq =>
              simp only [bind_ok, ↓reduceIte]
              have hr := hreq rfl
              refine ⟨some .lt, rfl, by simp, by simp, fun _ => ?_⟩
              exact ⟨(u64_eq_iff_bv _ _).mp l1, (u64_eq_iff_bv _ _).mp l2,
                ((u64_eq_iff_bv _ _).mp l3).symm, by rw [hr]; exact BitVec.le_refl _,
                by rw [hr]; exact BitVec.le_refl _⟩
            | lt =>
              simp only [bind_ok, ↓reduceIte]
              obtain ⟨hmin, hmax⟩ := hrlt rfl
              refine ⟨some .lt, rfl, by simp, by simp, fun _ => ?_⟩
              exact ⟨(u64_eq_iff_bv _ _).mp l1, (u64_eq_iff_bv _ _).mp l2,
                ((u64_eq_iff_bv _ _).mp l3).symm, hmin, hmax⟩
        all_goals { simp [l1, l2, l3] }
    · simp only [g1, ↓reduceIte]
      by_cases l1 : (b.unwritten_mask &&& a.unwritten_mask) = a.unwritten_mask <;>
      by_cases l2 : (b.unknown_mask &&& a.unknown_mask) = a.unknown_mask <;>
      by_cases l3 : (a.value &&& ~~~b.unknown_mask) = (b.value &&& ~~~b.unknown_mask)
      · simp only [l1, l2, l3, ↓reduceIte, bind_ok_eq hropt]
        cases ropt with
        | none => simp [bind_ok]
        | some rord =>
          cases rord with
          | gt => simp [bind_ok]
          | eq =>
            simp only [bind_ok, ↓reduceIte]
            have hr := hreq rfl
            refine ⟨some .lt, rfl, by simp, by simp, fun _ => ?_⟩
            exact ⟨(u64_eq_iff_bv _ _).mp l1, (u64_eq_iff_bv _ _).mp l2,
              ((u64_eq_iff_bv _ _).mp l3).symm, by rw [hr]; exact BitVec.le_refl _,
              by rw [hr]; exact BitVec.le_refl _⟩
          | lt =>
            simp only [bind_ok, ↓reduceIte]
            obtain ⟨hmin, hmax⟩ := hrlt rfl
            refine ⟨some .lt, rfl, by simp, by simp, fun _ => ?_⟩
            exact ⟨(u64_eq_iff_bv _ _).mp l1, (u64_eq_iff_bv _ _).mp l2,
              ((u64_eq_iff_bv _ _).mp l3).symm, hmin, hmax⟩
      all_goals { simp [l1, l2, l3] }

theorem u64_scalar_partial_ord_le_sound {a b : U64ScalarValueData}
    (h : core.cmp.PartialOrd.le.trait_default
          U64ScalarValueData.Insts.CoreCmpPartialOrdU64ScalarValueData a b = ok true) :
    BroaderOrEqual64 b a := by
  unfold core.cmp.PartialOrd.le.trait_default
    U64ScalarValueData.Insts.CoreCmpPartialOrdU64ScalarValueData at h
  rcases bind_eq_ok h with ⟨cmp, hcmp, hret⟩
  dsimp only [] at hcmp
  obtain ⟨opt, hopt, heq, _, hlt⟩ := u64_scalar_partial_cmp_sound a b
  rw [hopt] at hcmp; cases ok_inj hcmp
  have hb := ok_inj hret
  cases cmp with
  | none => simp at hb
  | some ord =>
    cases ord with
    | lt => exact hlt rfl
    | eq => exact (heq rfl).2.2
    | gt => simp at hb

theorem u64_scalar_partial_ord_ge_sound {a b : U64ScalarValueData}
    (h : core.cmp.PartialOrd.ge.trait_default
          U64ScalarValueData.Insts.CoreCmpPartialOrdU64ScalarValueData a b = ok true) :
    BroaderOrEqual64 a b := by
  unfold core.cmp.PartialOrd.ge.trait_default
    U64ScalarValueData.Insts.CoreCmpPartialOrdU64ScalarValueData at h
  rcases bind_eq_ok h with ⟨cmp, hcmp, hret⟩
  dsimp only [] at hcmp
  obtain ⟨opt, hopt, heq, hgt, _⟩ := u64_scalar_partial_cmp_sound a b
  rw [hopt] at hcmp; cases ok_inj hcmp
  have hb := ok_inj hret
  cases cmp with
  | none => simp at hb
  | some ord =>
    cases ord with
    | gt => exact hgt rfl
    | eq => exact (heq rfl).2.1
    | lt => simp at hb

end StarnixEbpf.Proof
