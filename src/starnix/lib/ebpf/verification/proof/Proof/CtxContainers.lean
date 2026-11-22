-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.StateInvariant

/-!
# Verifier Containers, Descriptors, `Type` Helpers, & `DataDependencies`

Proves total specifications, structural-invariant preservation, and relational
2-safety properties for the core verifier container and helper modules in
`ebpf_core::verifier`:
1. Platform `Usize` cast/checked-arithmetic lemmas, `DataWidth`, `visitor.Source`,
   `PointerOrigin`, `MemoryParameterSize`, `MapSchema`, and `MapTypeFilter`.
2. Sorted `IdSet` and `BoundsMap` (`IdSetValid` / `BoundsMapValid` preservation
   and exact membership/bound specifications).
3. `FieldType`, `FieldDescriptor`, `Field`, and `StructDescriptor.find_field`
   (linking directly to `Spec.findStructField`, `isAligned`, and `inBoundsRange`).
4. `verifier.Type` operations (`clone`, `eq`, `mask`, `written_scalar`,
   `is_initialized`, `is_non_zero`, `check_not_released`, `inner`, `with_offset`,
   `set_null`, `unregister_resource`) and `DataDependencies` bitset transformers.
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

/-! ## 1. Platform `Usize` Helpers, `DataWidth`, `PointerOrigin`, & `MapTypeFilter` -/

theorem usize_max_lt_2pow64 : Usize.max < 2 ^ 64 := by
  rcases Usize.bounds_eq with h | h <;> rw [h] <;> simp [U32.max_eq, U64.max_eq]

theorem usize_val_le_max (x : Std.Usize) : x.val ≤ Usize.max := by
  have h := x.hmax
  simp [Usize.max, Usize.numBits, UScalarTy.Usize_numBits_eq] at h ⊢
  omega

theorem usize_val_lt_2pow64 (x : Std.Usize) : x.val < 2 ^ 64 :=
  Nat.lt_of_le_of_lt (usize_val_le_max x) usize_max_lt_2pow64

theorem usize_const_val_0 : (0#usize).val = 0 := rfl

theorem usize_const_val_4 : (4#usize).val = 4 := by
  cases System.Platform.numBits_eq <;> simp_all [UScalar.val]

theorem usize_const_val_8 : (8#usize).val = 8 := by
  cases System.Platform.numBits_eq <;> simp_all [UScalar.val]

theorem usize_const_val_16 : (16#usize).val = 16 := by
  cases System.Platform.numBits_eq <;> simp_all [UScalar.val]

theorem usize_const_val_32 : (32#usize).val = 32 := by
  cases System.Platform.numBits_eq <;> simp_all [UScalar.val]

theorem usize_const_val_64 : (64#usize).val = 64 := by
  cases System.Platform.numBits_eq <;> simp_all [UScalar.val]

theorem usize_const_val_512 : (512#usize).val = 512 := by
  cases System.Platform.numBits_eq <;> simp_all [UScalar.val]

theorem uscalar_cast_u64_of_usize_val (x : Std.Usize) :
    (UScalar.cast .U64 x).val = x.val := by
  cases System.Platform.numBits_eq with
  | inl h32 =>
    dsimp only [UScalar.cast, UScalar.val, UScalarTy.U64_numBits_eq, UScalarTy.Usize_numBits_eq]
    simp only [BitVec.toNat_setWidth]
    exact Nat.mod_eq_of_lt (usize_val_lt_2pow64 x)
  | inr h64 =>
    dsimp only [UScalar.cast, UScalar.val, UScalarTy.U64_numBits_eq, UScalarTy.Usize_numBits_eq]
    simp only [BitVec.toNat_setWidth]
    exact Nat.mod_eq_of_lt (usize_val_lt_2pow64 x)

theorem uscalar_cast_u64_of_usize_bv (x : Std.Usize) :
    (UScalar.cast .U64 x).bv = BitVec.ofNat 64 x.val := by
  apply BitVec.eq_of_toNat_eq
  change (UScalar.cast .U64 x).val = (BitVec.ofNat 64 x.val).toNat
  rw [uscalar_cast_u64_of_usize_val, BitVec.toNat_ofNat,
    Nat.mod_eq_of_lt (usize_val_lt_2pow64 x)]

theorem core_usize_max_val : core.num.Usize.MAX.val = Usize.max := by
  rw [← UScalar.bv_toNat]
  rfl

theorem uscalar_cast_usize_of_u64_val {x : Std.U64} (hle : x.val ≤ Usize.max) :
    (UScalar.cast .Usize x).val = x.val := by
  cases System.Platform.numBits_eq with
  | inl h32 =>
    have hm : Usize.max = 2 ^ 32 - 1 := by
      simp [Usize.max, Usize.numBits, h32]
    dsimp only [UScalar.cast, UScalar.val]
    rw [UScalarTy.Usize_numBits_eq, h32, BitVec.toNat_setWidth, UScalar.bv_toNat]
    exact Nat.mod_eq_of_lt (by omega)
  | inr h64 =>
    dsimp only [UScalar.cast, UScalar.val]
    rw [UScalarTy.Usize_numBits_eq, h64, BitVec.toNat_setWidth]
    exact Nat.mod_eq_of_lt x.bv.isLt

theorem usize_add_eq_ok (a b : Std.Usize) (hle : a.val + b.val ≤ Usize.max) :
    ∃ c : Std.Usize, (a + b : Result Std.Usize) = ok c ∧ c.val = a.val + b.val := by
  have hle' : a.val + b.val ≤ UScalar.max .Usize := by rwa [UScalar.max_USize_eq]
  have hok := uscalar_add_eq_ok a b hle'
  exact ⟨⟨a.bv + b.bv⟩, hok, (uscalar_add_ok_val hok).1⟩

theorem usize_sub_eq_ok (a b : Std.Usize) (hle : b.val ≤ a.val) :
    ∃ c : Std.Usize, (a - b : Result Std.Usize) = ok c ∧ c.val = a.val - b.val := by
  have hok := uscalar_sub_eq_ok a b hle
  have hs := UScalar.sub_bv_spec hle
  rw [hok] at hs
  simp at hs
  exact ⟨⟨a.bv - b.bv⟩, hok, hs.1⟩

theorem usize_checked_add_spec (a b : Std.Usize) :
    (a.val + b.val ≤ Usize.max ∧
      ∃ c : Std.Usize, Usize.checked_add a b = some c ∧ c.val = a.val + b.val) ∨
    (Usize.max < a.val + b.val ∧ Usize.checked_add a b = none) := by
  have hs := Usize.checked_add_bv_spec a b
  cases hc : Usize.checked_add a b with
  | some c =>
    rw [hc] at hs
    exact Or.inl ⟨hs.1, c, rfl, hs.2.1⟩
  | none =>
    rw [hc] at hs
    exact Or.inr ⟨hs, rfl⟩

theorem slice_index_usize_eq_ok {α : Type u} (s : Slice α) (i : Std.Usize)
    (hlt : i.val < s.val.length) :
    Slice.index_usize s i = ok (s.val[i.val]'hlt) := by
  unfold Slice.index_usize
  rw [Slice.getElem?_Usize_eq, List.getElem?_eq_getElem hlt]

@[simp] theorem vec_deref_val {α : Type u} (v : alloc.vec.Vec α) :
    (alloc.vec.Vec.deref v).val = v.val := by
  unfold alloc.vec.Vec.deref
  simp

theorem vec_index_usize_eq_ok {α : Type u} (v : alloc.vec.Vec α) (i : Std.Usize)
    (hlt : i.val < v.val.length) :
    alloc.vec.Vec.index (core.slice.index.SliceIndexUsizeSlice α) v i =
      ok (v.val[i.val]'hlt) := by
  unfold alloc.vec.Vec.index core.slice.index.SliceIndexUsizeSlice
  exact slice_index_usize_eq_ok v.slice i hlt

theorem vec_push_eq_ok {α : Type u} (v : alloc.vec.Vec α) (x : α)
    (hlt : v.val.length < Usize.max) :
    ∃ v', alloc.vec.Vec.push v x = ok v' ∧ v'.val = v.val ++ [x] := by
  unfold alloc.vec.Vec.push
  dsimp only
  split
  · exact ⟨_, rfl, by simp [alloc.vec.Vec.from, alloc.vec.Vec.val]⟩
  · rename_i hnot
    exfalso
    apply hnot
    simp only [Bool.or_eq_true, decide_eq_true_eq]
    exact Or.inr (by omega)

theorem PartialEq_ne_of_eq {T : Type u} (inst : core.cmp.PartialEq T T)
    (a b : T) {r : Bool} (heq : inst.eq a b = ok r) (hiff : r = true ↔ a = b) :
    ∃ nr : Bool, core.cmp.PartialEq.ne.trait_default inst a b = ok nr ∧
      (nr = true ↔ a ≠ b) := by
  unfold core.cmp.PartialEq.ne.trait_default core.cmp.PartialEq.ne.default
  rw [heq]
  cases r
  · refine ⟨true, bind_tc_ok false _, ?_⟩
    simp only [true_iff]
    intro h
    have := hiff.mpr h
    cases this
  · refine ⟨false, bind_tc_ok true _, ?_⟩
    simp only [Bool.false_eq_true, false_iff, Classical.not_not]
    exact hiff.mp rfl

theorem DataWidth_clone_spec (w : api.DataWidth) :
    api.DataWidth.Insts.CoreCloneClone.clone w = ok w := rfl

theorem DataWidth_eq_spec (a b : api.DataWidth) :
    ∃ r : Bool, api.DataWidth.Insts.CoreCmpPartialEqDataWidth.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  unfold api.DataWidth.Insts.CoreCmpPartialEqDataWidth.eq api.DataWidth.read_discriminant
  cases a <;> cases b <;> refine ⟨_, rfl, ?_⟩ <;> simp

theorem DataWidth_ne_spec (a b : api.DataWidth) :
    ∃ nr : Bool,
      core.cmp.PartialEq.ne.trait_default
        api.DataWidth.Insts.CoreCmpPartialEqDataWidth a b = ok nr ∧
      (nr = true ↔ a ≠ b) := by
  obtain ⟨r, hr, hiff⟩ := DataWidth_eq_spec a b
  exact PartialEq_ne_of_eq _ a b hr hiff

theorem DataWidth_bits_spec (w : api.DataWidth) :
    ∃ n : Std.Usize, api.DataWidth.bits w = ok n ∧ n.val = (DataWidth.ofCore w).bits := by
  cases w
  · exact ⟨8#usize, rfl, usize_const_val_8⟩
  · exact ⟨16#usize, rfl, usize_const_val_16⟩
  · exact ⟨32#usize, rfl, usize_const_val_32⟩
  · exact ⟨64#usize, rfl, usize_const_val_64⟩

theorem DataWidth_bytes_spec (w : api.DataWidth) :
    ∃ n : Std.Usize, api.DataWidth.bytes w = ok n ∧ n.val = (DataWidth.ofCore w).bytes := by
  cases w
  · exact ⟨1#usize, rfl, usize_one_val⟩
  · exact ⟨2#usize, rfl, usize_two_val⟩
  · exact ⟨4#usize, rfl, usize_const_val_4⟩
  · exact ⟨8#usize, rfl, usize_const_val_8⟩

theorem Type_mask_spec (w : api.DataWidth) :
    verifier.Type.mask w = ok ⟨(DataWidth.ofCore w).valMask64⟩ := by
  unfold verifier.Type.mask
  cases w
  · have heq : api.DataWidth.Insts.CoreCmpPartialEqDataWidth.eq .U8 .U64 = ok false := by
      unfold api.DataWidth.Insts.CoreCmpPartialEqDataWidth.eq api.DataWidth.read_discriminant
      simp
    rw [bind_ok_eq heq]
    simp only [Bool.false_eq_true, ↓reduceIte, api.DataWidth.bits, lift_eq_ok, bind_ok,
      DataWidth.ofCore, DataWidth.valMask64]
    have hcast : UScalar.cast .U32 8#usize = 8#u32 := by
      apply U32.bv_eq_imp_eq
      change U32.bv (UScalar.cast .U32 8#usize) = (8#u32).bv
      rw [usize_cast_u32_bv, usize_const_val_8]
      rfl
    rw [hcast, u64_wrapping_shl_eq]
    exact uscalar_sub_eq_ok _ _ (by decide)
  · have heq : api.DataWidth.Insts.CoreCmpPartialEqDataWidth.eq .U16 .U64 = ok false := by
      unfold api.DataWidth.Insts.CoreCmpPartialEqDataWidth.eq api.DataWidth.read_discriminant
      simp
    rw [bind_ok_eq heq]
    simp only [Bool.false_eq_true, ↓reduceIte, api.DataWidth.bits, lift_eq_ok, bind_ok,
      DataWidth.ofCore, DataWidth.valMask64]
    have hcast : UScalar.cast .U32 16#usize = 16#u32 := by
      apply U32.bv_eq_imp_eq
      change U32.bv (UScalar.cast .U32 16#usize) = (16#u32).bv
      rw [usize_cast_u32_bv, usize_const_val_16]
      rfl
    rw [hcast, u64_wrapping_shl_eq]
    exact uscalar_sub_eq_ok _ _ (by decide)
  · have heq : api.DataWidth.Insts.CoreCmpPartialEqDataWidth.eq .U32 .U64 = ok false := by
      unfold api.DataWidth.Insts.CoreCmpPartialEqDataWidth.eq api.DataWidth.read_discriminant
      simp
    rw [bind_ok_eq heq]
    simp only [Bool.false_eq_true, ↓reduceIte, api.DataWidth.bits, lift_eq_ok, bind_ok,
      DataWidth.ofCore, DataWidth.valMask64]
    have hcast : UScalar.cast .U32 32#usize = 32#u32 := by
      apply U32.bv_eq_imp_eq
      change U32.bv (UScalar.cast .U32 32#usize) = (32#u32).bv
      rw [usize_cast_u32_bv, usize_const_val_32]
      rfl
    rw [hcast, u64_wrapping_shl_eq]
    exact uscalar_sub_eq_ok _ _ (by decide)
  · have heq : api.DataWidth.Insts.CoreCmpPartialEqDataWidth.eq .U64 .U64 = ok true := by
      unfold api.DataWidth.Insts.CoreCmpPartialEqDataWidth.eq api.DataWidth.read_discriminant
      simp
    rw [bind_ok_eq heq]
    simp only [↓reduceIte, DataWidth.ofCore, DataWidth.valMask64]
    rfl

theorem Source_clone_spec (s : visitor.Source) :
    visitor.Source.Insts.CoreCloneClone.clone s = ok s := rfl

theorem Source_eq_spec (a b : visitor.Source) :
    ∃ r : Bool, visitor.Source.Insts.CoreCmpPartialEqSource.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  unfold visitor.Source.Insts.CoreCmpPartialEqSource.eq visitor.Source.read_discriminant
  cases a with
  | Reg ra =>
    cases b with
    | Reg rb =>
      dsimp (config := { zetaHave := true }) only [lift, core.cmp.impls.PartialEqU8.eq]
      simp only [↓reduceIte]
      exact ⟨decide (ra = rb), rfl, by simp⟩
    | Value _ =>
      dsimp (config := { zetaHave := true }) only
      have hne : (0#isize : Std.Isize) ≠ 1#isize := by simp
      simp only [hne, ↓reduceIte]
      exact ⟨false, rfl, by simp⟩
  | Value va =>
    cases b with
    | Reg _ =>
      dsimp (config := { zetaHave := true }) only
      have hne : (1#isize : Std.Isize) ≠ 0#isize := by simp
      simp only [hne, ↓reduceIte]
      exact ⟨false, rfl, by simp⟩
    | Value vb =>
      dsimp (config := { zetaHave := true }) only [lift, core.cmp.impls.PartialEqU64.eq]
      simp only [↓reduceIte]
      exact ⟨decide (va = vb), rfl, by simp⟩

theorem verifier_min_u64_spec (a b : Std.U64) :
    verifier.min_u64 a b = ok ⟨if a.bv ≤ b.bv then a.bv else b.bv⟩ := by
  unfold verifier.min_u64
  by_cases h : a ≤ b
  · have hbv : a.bv ≤ b.bv := (u64_le_iff_bv a b).mp h
    simp [h, hbv]
  · have hbv : ¬(a.bv ≤ b.bv) := fun hc => h ((u64_le_iff_bv a b).mpr hc)
    simp [h, hbv]

theorem verifier_max_u64_spec (a b : Std.U64) :
    verifier.max_u64 a b = ok ⟨if b.bv ≤ a.bv then a.bv else b.bv⟩ := by
  unfold verifier.max_u64
  by_cases h : a ≥ b
  · have hbv : b.bv ≤ a.bv := (u64_ge_iff_bv a b).mp h
    simp [h, hbv]
  · have hbv : ¬(b.bv ≤ a.bv) := fun hc => h ((u64_ge_iff_bv a b).mpr hc)
    simp [h, hbv]

theorem PointerOrigin_clone_spec (o : verifier.PointerOrigin) :
    verifier.PointerOrigin.Insts.CoreCloneClone.clone o = ok o := rfl

theorem PointerOrigin_eq_spec (a b : verifier.PointerOrigin) :
    ∃ r : Bool, verifier.PointerOrigin.Insts.CoreCmpPartialEqPointerOrigin.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  unfold verifier.PointerOrigin.Insts.CoreCmpPartialEqPointerOrigin.eq verifier.PointerOrigin.tag
  cases a <;> cases b <;> simp only [bind_ok] <;> refine ⟨_, rfl, ?_⟩ <;> simp

theorem PointerOrigin_is_writable_spec (o : verifier.PointerOrigin) :
    verifier.PointerOrigin.is_writable o = ok (MemoryOrigin.ofCore o).isWritable := by
  cases o <;> rfl

theorem PointerOrigin_is_map_value_spec (o : verifier.PointerOrigin) :
    verifier.PointerOrigin.is_map_value o = ok (MemoryOrigin.ofCore o).isMap := by
  cases o <;> rfl

theorem MemoryParameterSize_clone_spec (s : verifier.MemoryParameterSize) :
    verifier.MemoryParameterSize.Insts.CoreCloneClone.clone s = ok s := rfl

theorem MemoryParameterSize_eq_spec (a b : verifier.MemoryParameterSize) :
    ∃ r : Bool,
      verifier.MemoryParameterSize.Insts.CoreCmpPartialEqMemoryParameterSize.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  unfold verifier.MemoryParameterSize.Insts.CoreCmpPartialEqMemoryParameterSize.eq
  cases a with
  | Value va =>
    cases b with
    | Value vb => exact ⟨decide (va = vb), rfl, by simp⟩
    | Reference _ => exact ⟨false, rfl, by simp⟩
  | Reference ra =>
    cases b with
    | Value _ => exact ⟨false, rfl, by simp⟩
    | Reference rb => exact ⟨decide (ra = rb), rfl, by simp⟩

theorem MapFlags_eq_spec (a b : maps.MapFlags) :
    maps.MapFlags.Insts.CoreCmpPartialEqMapFlags.eq a b = ok (decide (a = b)) := by
  unfold maps.MapFlags.Insts.CoreCmpPartialEqMapFlags.eq
  by_cases h : a = b <;> simp [h]

theorem MapSchema_clone_spec (s : maps.MapSchema) :
    maps.MapSchema.Insts.CoreCloneClone.clone s = ok s := rfl

theorem MapSchema_eq_spec (a b : maps.MapSchema) :
    ∃ r : Bool, maps.MapSchema.Insts.CoreCmpPartialEqMapSchema.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  cases a with | mk mt1 ks1 vs1 me1 fl1 =>
  cases b with | mk mt2 ks2 vs2 me2 fl2 =>
  unfold maps.MapSchema.Insts.CoreCmpPartialEqMapSchema.eq
  dsimp (config := { zetaHave := true }) only
  by_cases hks : ks1 = ks2
  · subst hks
    simp only [↓reduceIte]
    by_cases hvs : vs1 = vs2
    · subst hvs
      simp only [↓reduceIte]
      by_cases hme : me1 = me2
      · subst hme
        simp only [↓reduceIte]
        by_cases hmt : mt1 = mt2
        · subst hmt
          simp only [↓reduceIte, MapFlags_eq_spec]
          exact ⟨decide (fl1 = fl2), rfl, by simp⟩
        · simp only [hmt, ↓reduceIte]
          exact ⟨false, rfl, by simp [hmt]⟩
      · simp only [hme, ↓reduceIte]
        exact ⟨false, rfl, by simp [hme]⟩
    · simp only [hvs, ↓reduceIte]
      exact ⟨false, rfl, by simp [hvs]⟩
  · simp only [hks, ↓reduceIte]
    exact ⟨false, rfl, by simp [hks]⟩

theorem drop_take_cons_of_lt {α : Type u} (l : List α) {k limit : Nat}
    (hlt : k < limit) (hle : limit ≤ l.length) :
    (l.drop k).take (limit - k) =
      (l[k]'(Nat.lt_of_lt_of_le hlt hle)) :: (l.drop (k + 1)).take (limit - (k + 1)) := by
  have hk_lt : k < l.length := Nat.lt_of_lt_of_le hlt hle
  rw [List.drop_eq_getElem_cons hk_lt]
  have hsub : limit - k = (limit - (k + 1)) + 1 := by omega
  rw [hsub, List.take_succ_cons]

theorem clone_vec_u32_loop_spec (v : Slice Std.U32) (acc : alloc.vec.Vec Std.U32) (i : Std.Usize)
    (hpre : i.val ≤ v.val.length ∧ acc.val.length + (v.val.length - i.val) ≤ Usize.max) :
    ∃ out, verifier.clone_vec_u32_loop v acc i = ok out ∧
      out.val = acc.val ++ v.val.drop i.val := by
  suffices ∀ n (acc : alloc.vec.Vec Std.U32) (i : Std.Usize),
      v.val.length - i.val = n →
      i.val ≤ v.val.length →
      acc.val.length + (v.val.length - i.val) ≤ Usize.max →
      ∃ out, verifier.clone_vec_u32_loop v acc i = ok out ∧
        out.val = acc.val ++ v.val.drop i.val by
    exact this (v.val.length - i.val) acc i rfl hpre.1 hpre.2
  intro n
  induction n with
  | zero =>
    intro acc i hrem hle _
    have hi_eq : i.val = v.val.length := by omega
    unfold verifier.clone_vec_u32_loop
    rw [loop_unfold_step]
    unfold verifier.clone_vec_u32_loop.body
    have hnlt : ¬ (i < Slice.len v) := by
      intro hc
      have : i.val < v.val.length := hc
      omega
    simp [hnlt, hi_eq]
  | succ k ih =>
    intro acc i hrem hle hcap
    have hlt_val : i.val < v.val.length := by omega
    have hlt : i < Slice.len v := hlt_val
    unfold verifier.clone_vec_u32_loop
    rw [loop_unfold_step]
    unfold verifier.clone_vec_u32_loop.body
    simp only [hlt, ↓reduceIte]
    rw [bind_ok_eq (slice_index_usize_eq_ok v i hlt_val)]
    have hlen_lt : acc.val.length < Usize.max := by omega
    obtain ⟨acc1, hpush, hacc1⟩ := vec_push_eq_ok acc (v.val[i.val]'hlt_val) hlen_lt
    rw [bind_ok_eq hpush]
    have hv_le : v.val.length ≤ Usize.max := Slice.length_ineq v
    have hi_add_le : i.val + (1#usize).val ≤ Usize.max := by
      rw [usize_one_val]; omega
    obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize hi_add_le
    rw [usize_one_val] at hi1_val
    rw [bind_ok_eq hi1]
    simp only [bind_ok]
    have hacc1_len : acc1.val.length = acc.val.length + 1 := by simp [hacc1]
    obtain ⟨out, hout, hout_val⟩ := ih acc1 i1 (by omega) (by omega) (by omega)
    refine ⟨out, hout, ?_⟩
    rw [hout_val, hacc1, List.append_assoc, List.singleton_append, hi1_val,
      ← List.drop_eq_getElem_cons hlt_val]

theorem clone_vec_u32_spec (v : Slice Std.U32) :
    ∃ out, verifier.clone_vec_u32 v = ok out ∧ out.val = v.val := by
  unfold verifier.clone_vec_u32
  have hv_le : v.val.length ≤ Usize.max := Slice.length_ineq v
  have hpre : (0#usize).val ≤ v.val.length ∧
      (alloc.vec.Vec.new Std.U32).val.length + (v.val.length - (0#usize).val) ≤ Usize.max := by
    refine ⟨Nat.zero_le _, ?_⟩
    simp
  obtain ⟨out, hout, hval⟩ := clone_vec_u32_loop_spec v (alloc.vec.Vec.new Std.U32) 0#usize hpre
  exact ⟨out, hout, by simpa using hval⟩

theorem slice_contains_u32_spec (s : Slice Std.U32) (x : Std.U32) :
    verifier.slice_contains_u32 s x = ok ((s.val.map (·.bv)).contains x.bv) := by
  suffices ∀ n (i : Std.Usize),
      s.val.length - i.val = n →
      i.val ≤ s.val.length →
      verifier.slice_contains_u32_loop s x i =
        ok (((s.val.drop i.val).map (·.bv)).contains x.bv) by
    simpa [verifier.slice_contains_u32] using
      this s.val.length 0#usize (by simp) (Nat.zero_le _)
  intro n
  induction n with
  | zero =>
    intro i hrem hle
    have hi : i.val = s.val.length := by omega
    unfold verifier.slice_contains_u32_loop
    rw [loop_unfold_step]
    unfold verifier.slice_contains_u32_loop.body
    have hnlt : ¬ (i < Slice.len s) := by intro hc; have : i.val < s.val.length := hc; omega
    simp [hnlt, hi]
  | succ k ih =>
    intro i hrem hle
    have hlt_val : i.val < s.val.length := by omega
    have hlt : i < Slice.len s := hlt_val
    unfold verifier.slice_contains_u32_loop
    rw [loop_unfold_step]
    unfold verifier.slice_contains_u32_loop.body
    simp only [hlt, ↓reduceIte]
    rw [bind_ok_eq (slice_index_usize_eq_ok s i hlt_val),
      List.drop_eq_getElem_cons hlt_val, List.map_cons, List.contains_cons]
    by_cases heq : (s.val[i.val]'hlt_val) = x
    · simp [heq]
    · have hbvne : (x.bv == (s.val[i.val]'hlt_val).bv) = false := by
        apply beq_false_of_ne
        intro hc; exact heq ((u32_eq_iff_bv _ _).2 hc.symm)
      simp only [heq, ↓reduceIte]
      have hs_le : s.val.length ≤ Usize.max := Slice.length_ineq s
      have hi_add_le : i.val + (1#usize).val ≤ Usize.max := by
        rw [usize_one_val]; omega
      obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize hi_add_le
      rw [usize_one_val] at hi1_val
      rw [bind_ok_eq hi1]
      simp only [bind_ok]
      change verifier.slice_contains_u32_loop s x i1 = _
      rw [ih i1 (by omega) (by omega), hi1_val, hbvne, Bool.false_or]

theorem slice_eq_u32_spec (a b : Slice Std.U32) :
    verifier.slice_eq_u32 a b = ok (decide (a.val = b.val)) := by
  unfold verifier.slice_eq_u32
  by_cases hlen : Slice.len a != Slice.len b
  · have hne_len : a.val.length ≠ b.val.length := by
      intro hc
      have heq_u : Slice.len a = Slice.len b := by
        apply Usize.bv_eq_imp_eq
        apply BitVec.eq_of_toNat_eq
        exact hc
      exact (bne_iff_ne.mp hlen) heq_u
    have hne : a.val ≠ b.val := fun hc => hne_len (congrArg List.length hc)
    simp [hlen, hne]
  · simp only [hlen, Bool.false_eq_true, ↓reduceIte]
    have heq_len : a.val.length = b.val.length := by
      have heq_u : Slice.len a = Slice.len b := by
        by_contra hne; exact hlen (bne_iff_ne.mpr hne)
      exact congrArg UScalar.val heq_u
    suffices ∀ n (i : Std.Usize),
        a.val.length - i.val = n →
        i.val ≤ a.val.length →
        verifier.slice_eq_u32_loop a b i =
          ok (decide (a.val.drop i.val = b.val.drop i.val)) by
      simpa using this a.val.length 0#usize (by simp) (Nat.zero_le _)
    intro n
    induction n with
    | zero =>
      intro i hrem hle
      have hi : i.val = a.val.length := by omega
      unfold verifier.slice_eq_u32_loop
      rw [loop_unfold_step]
      unfold verifier.slice_eq_u32_loop.body
      have hnlt : ¬ (i < Slice.len a) := by intro hc; have : i.val < a.val.length := hc; omega
      have ha_drop : a.val.drop i.val = [] := by rw [hi, List.drop_length]
      have hb_drop : b.val.drop i.val = [] := by rw [hi, heq_len, List.drop_length]
      simp only [hnlt, ↓reduceIte, bind_ok, ha_drop, hb_drop, decide_true]
    | succ k ih =>
      intro i hrem hle
      have hlta : i.val < a.val.length := by omega
      have hltb : i.val < b.val.length := by omega
      have hlt : i < Slice.len a := hlta
      unfold verifier.slice_eq_u32_loop
      rw [loop_unfold_step]
      unfold verifier.slice_eq_u32_loop.body
      simp only [hlt, ↓reduceIte]
      rw [bind_ok_eq (slice_index_usize_eq_ok a i hlta),
        bind_ok_eq (slice_index_usize_eq_ok b i hltb),
        List.drop_eq_getElem_cons hlta, List.drop_eq_getElem_cons hltb]
      by_cases heq : ((a.val[i.val]'hlta) != (b.val[i.val]'hltb)) = true
      · have hne_elem : (a.val[i.val]'hlta) ≠ (b.val[i.val]'hltb) := bne_iff_ne.mp heq
        simp only [heq, ↓reduceIte, bind_ok]
        congr 1
        symm
        rw [decide_eq_false_iff_not]
        intro hc
        exact hne_elem (List.cons.inj hc).1
      · have heq_elem : (a.val[i.val]'hlta) = (b.val[i.val]'hltb) := by
          by_contra hne; exact heq (bne_iff_ne.mpr hne)
        simp only [heq, Bool.false_eq_true, ↓reduceIte]
        have ha_le : a.val.length ≤ Usize.max := Slice.length_ineq a
        have hi_add_le : i.val + (1#usize).val ≤ Usize.max := by
          rw [usize_one_val]; omega
        obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize hi_add_le
        rw [usize_one_val] at hi1_val
        rw [bind_ok_eq hi1]
        simp only [bind_ok, heq_elem, List.cons.injEq, true_and, ← hi1_val]
        change verifier.slice_eq_u32_loop a b i1 = _
        exact ih i1 (by omega) (by omega)

theorem MapTypeFilter_clone_spec (f : verifier.MapTypeFilter) :
    verifier.MapTypeFilter.Insts.CoreCloneClone.clone f = ok f := by
  cases f with
  | AllowList v =>
    unfold verifier.MapTypeFilter.Insts.CoreCloneClone.clone
    dsimp (config := { zetaHave := true }) only
    obtain ⟨out, hout, hval⟩ := clone_vec_u32_spec (alloc.vec.Vec.deref v)
    rw [bind_ok_eq hout]
    have hv : out = v := alloc.vec.Vec.ext out v (by simpa [vec_deref_val] using hval)
    rw [hv]
  | DenyList v =>
    unfold verifier.MapTypeFilter.Insts.CoreCloneClone.clone
    dsimp (config := { zetaHave := true }) only
    obtain ⟨out, hout, hval⟩ := clone_vec_u32_spec (alloc.vec.Vec.deref v)
    rw [bind_ok_eq hout]
    have hv : out = v := alloc.vec.Vec.ext out v (by simpa [vec_deref_val] using hval)
    rw [hv]

theorem MapTypeFilter_eq_spec (a b : verifier.MapTypeFilter) :
    ∃ r : Bool,
      verifier.MapTypeFilter.Insts.CoreCmpPartialEqMapTypeFilter.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  cases a with
  | AllowList va =>
    cases b with
    | AllowList vb =>
      unfold verifier.MapTypeFilter.Insts.CoreCmpPartialEqMapTypeFilter.eq
      dsimp (config := { zetaHave := true }) only
      rw [slice_eq_u32_spec]
      refine ⟨decide (va.deref.val = vb.deref.val), rfl, ?_⟩
      simp only [decide_eq_true_eq, verifier.MapTypeFilter.AllowList.injEq,
        alloc.vec.Vec.eq_iff, vec_deref_val]
    | DenyList _ =>
      exact ⟨false, rfl, by simp⟩
  | DenyList va =>
    cases b with
    | AllowList _ =>
      exact ⟨false, rfl, by simp⟩
    | DenyList vb =>
      unfold verifier.MapTypeFilter.Insts.CoreCmpPartialEqMapTypeFilter.eq
      dsimp (config := { zetaHave := true }) only
      rw [slice_eq_u32_spec]
      refine ⟨decide (va.deref.val = vb.deref.val), rfl, ?_⟩
      simp only [decide_eq_true_eq, verifier.MapTypeFilter.DenyList.injEq,
        alloc.vec.Vec.eq_iff, vec_deref_val]

theorem MapTypeFilter_is_allowed_allow_spec (v : alloc.vec.Vec Std.U32) (mt : Std.U32) :
    verifier.MapTypeFilter.is_allowed (.AllowList v) mt =
      ok ((HelperMapFilter.allowList (v.val.map (·.bv))).allows mt.bv) := by
  unfold verifier.MapTypeFilter.is_allowed HelperMapFilter.allows
  dsimp (config := { zetaHave := true }) only
  rw [slice_contains_u32_spec, vec_deref_val]

theorem MapTypeFilter_is_allowed_deny_spec (v : alloc.vec.Vec Std.U32) (mt : Std.U32) :
    verifier.MapTypeFilter.is_allowed (.DenyList v) mt =
      ok ((HelperMapFilter.denyList (v.val.map (·.bv))).allows mt.bv) := by
  unfold verifier.MapTypeFilter.is_allowed HelperMapFilter.allows
  dsimp (config := { zetaHave := true }) only
  rw [bind_ok_eq (slice_contains_u32_spec _ _), vec_deref_val]
  cases ((v.val.map (·.bv)).contains mt.bv) <;> rfl

/-! ## 2. Sorted `IdSet` and `BoundsMap` Specifications -/

private theorem pureMemoryIdCmp_gt_iff (a b : MemoryId) :
    pureMemoryIdCmp a b = .gt ↔ MemoryIdLt b a := by
  unfold MemoryIdLt
  rw [pureMemoryIdCmp_swap b a]
  cases pureMemoryIdCmp b a <;> decide

private theorem usize_eq_of_val_eq {x y : Std.Usize} (h : x.val = y.val) : x = y := by
  cases x with | mk bx =>
  cases y with | mk by_ =>
  congr
  apply BitVec.eq_of_toNat_eq
  simpa [UScalar.val] using h

private theorem pairwise_drop_of_pairwise {α : Type u} {R : α → α → Prop}
    {l : List α} (hpw : List.Pairwise R l) (k : Nat) :
    List.Pairwise R (l.drop k) :=
  hpw.sublist (List.drop_sublist k l)

private theorem pairwise_take_of_pairwise {α : Type u} {R : α → α → Prop}
    {l : List α} (hpw : List.Pairwise R l) (k : Nat) :
    List.Pairwise R (l.take k) :=
  hpw.sublist (List.take_sublist k l)

private theorem pairwise_take_insert_drop
    {l : List MemoryId} (hpw : List.Pairwise MemoryIdLt l) {idx : Nat} (hle : idx ≤ l.length)
    {x : MemoryId}
    (hbefore : ∀ j (hj : j < idx), MemoryIdLt (l[j]'(Nat.lt_of_lt_of_le hj hle)) x)
    (hafter : idx = l.length ∨ ∃ hlt : idx < l.length, MemoryIdLt x (l[idx]'hlt)) :
    List.Pairwise MemoryIdLt (l.take idx ++ [x] ++ l.drop idx) := by
  have hxs : ∀ y ∈ l.drop idx, MemoryIdLt x y := by
    rcases hafter with rfl | ⟨hlt, hrx⟩
    · simp
    · intro y hy
      rw [List.drop_eq_getElem_cons hlt] at hy
      have hpw_drop : List.Pairwise MemoryIdLt ((l[idx]'hlt) :: l.drop (idx + 1)) := by
        rw [← List.drop_eq_getElem_cons hlt]
        exact pairwise_drop_of_pairwise hpw idx
      rw [List.pairwise_cons] at hpw_drop
      rcases List.mem_cons.mp hy with rfl | htail
      · exact hrx
      · exact MemoryIdLt_trans hrx (hpw_drop.1 y htail)
  have hbx : ∀ u ∈ l.take idx, MemoryIdLt u x := by
    intro u hu
    rw [List.mem_take_iff_getElem] at hu
    rcases hu with ⟨j, hj, rfl⟩
    exact hbefore j (by omega)
  rw [List.append_assoc, List.pairwise_append]
  refine ⟨pairwise_take_of_pairwise hpw idx, ?_, ?_⟩
  · simp only [List.singleton_append, List.pairwise_cons]
    exact ⟨hxs, pairwise_drop_of_pairwise hpw idx⟩
  · intro u hu v hv
    simp only [List.singleton_append, List.mem_cons] at hv
    rcases hv with rfl | hvd
    · exact hbx u hu
    · exact MemoryIdLt_trans (hbx u hu) (hxs v hvd)

private theorem sublist_take_drop_succ {α : Type u} (l : List α) {idx : Nat}
    (hlt : idx < l.length) :
    List.Sublist (l.take idx ++ l.drop (idx + 1)) l := by
  conv => rhs; rw [← List.take_append_drop idx l, List.drop_eq_getElem_cons hlt]
  exact List.Sublist.append (List.Sublist.refl _) (List.Sublist.cons _ (List.Sublist.refl _))

private theorem list_map_fst_set {α β : Type u} :
    ∀ (l : List (α × β)) (idx : Nat) (k : α) (v : β) (hlt : idx < l.length),
      (l[idx]'hlt).1 = k →
      (l.set idx (k, v)).map Prod.fst = l.map Prod.fst
  | [], _, _, _, hlt, _ => by simp at hlt
  | (a, _) :: xs, 0, k, v, _, hk => by
    subst hk
    rfl
  | _ :: xs, idx + 1, k, v, hlt, hk => by
    simp only [List.set_cons_succ, List.map_cons, List.cons.injEq, true_and]
    exact list_map_fst_set xs idx k v (Nat.lt_of_succ_lt_succ hlt) hk

private theorem mem_set_iff_of_nodup_fst {α β : Type u} :
    ∀ (l : List (α × β)) (idx : Nat) (k : α) (v : β) (hlt : idx < l.length),
      (l.map Prod.fst).Nodup →
      (l[idx]'hlt).1 = k →
      ∀ m b, (m, b) ∈ l.set idx (k, v) ↔ (m = k ∧ b = v) ∨ (m ≠ k ∧ (m, b) ∈ l)
  | [], _, _, _, hlt, _, _, _, _ => by simp at hlt
  | (a, b0) :: xs, 0, k, v, _, hnd, hk, m, b => by
    subst hk
    simp only [List.map_cons, List.nodup_cons] at hnd
    simp only [List.set_cons_zero, List.mem_cons, Prod.mk.injEq]
    constructor
    · rintro (⟨rfl, rfl⟩ | hmem)
      · exact Or.inl ⟨rfl, rfl⟩
      · refine Or.inr ⟨?_, Or.inr hmem⟩
        intro hmk
        subst hmk
        exact hnd.1 (List.mem_map_of_mem (f := Prod.fst) hmem)
    · rintro (⟨rfl, rfl⟩ | ⟨hne, ⟨rfl, _⟩ | hmem⟩)
      · exact Or.inl ⟨rfl, rfl⟩
      · exact False.elim (hne rfl)
      · exact Or.inr hmem
  | (a, b0) :: xs, idx + 1, k, v, hlt, hnd, hk, m, b => by
    simp only [List.map_cons, List.nodup_cons] at hnd
    have hlt' : idx < xs.length := Nat.lt_of_succ_lt_succ hlt
    have hak : a ≠ k := by
      intro heq
      subst heq
      apply hnd.1
      rw [← hk]
      exact List.mem_map_of_mem (f := Prod.fst) (List.getElem_mem hlt')
    simp only [List.set_cons_succ, List.mem_cons, Prod.mk.injEq]
    rw [mem_set_iff_of_nodup_fst xs idx k v hlt' hnd.2 hk m b]
    constructor
    · rintro (⟨rfl, rfl⟩ | ⟨rfl, rfl⟩ | ⟨hne, hmem⟩)
      · exact Or.inr ⟨hak, Or.inl ⟨rfl, rfl⟩⟩
      · exact Or.inl ⟨rfl, rfl⟩
      · exact Or.inr ⟨hne, Or.inr hmem⟩
    · rintro (⟨rfl, rfl⟩ | ⟨hne, ⟨rfl, rfl⟩ | hmem⟩)
      · exact Or.inr (Or.inl ⟨rfl, rfl⟩)
      · exact Or.inl ⟨rfl, rfl⟩
      · exact Or.inr (Or.inr ⟨hne, hmem⟩)

/-! ### `IdSet` Operations -/

theorem IdSet_new_spec (vCounter : Nat) :
    ∃ s, verifier.IdSet.new = ok s ∧ s.entries.val = [] ∧ IdSetValid vCounter s := by
  refine ⟨{ entries := alloc.vec.Vec.new MemoryId }, rfl, rfl, ?_⟩
  exact ⟨List.Pairwise.nil, fun _ hm => by simp at hm⟩

theorem IdSet_default_spec (vCounter : Nat) :
    ∃ s, verifier.IdSet.Insts.CoreDefaultDefault.default = ok s ∧
      s.entries.val = [] ∧ IdSetValid vCounter s :=
  IdSet_new_spec vCounter

theorem IdSet_clear_spec (vCounter : Nat) (self : verifier.IdSet) :
    ∃ s, verifier.IdSet.clear self = ok s ∧ s.entries.val = [] ∧ IdSetValid vCounter s := by
  refine ⟨{ entries := alloc.vec.Vec.new MemoryId }, rfl, rfl, ?_⟩
  exact ⟨List.Pairwise.nil, fun _ hm => by simp at hm⟩

theorem IdSet_len_spec (self : verifier.IdSet) :
    verifier.IdSet.len self = ok (alloc.vec.Vec.len self.entries) := rfl

theorem IdSet_is_empty_spec (self : verifier.IdSet) :
    verifier.IdSet.is_empty self = ok (decide (self.entries.val = [])) := by
  unfold verifier.IdSet.is_empty
  dsimp (config := { zetaHave := true }) only
  congr 1
  rw [decide_eq_decide]
  constructor
  · intro h
    have hlen : self.entries.val.length = 0 := by
      have := congrArg UScalar.val h
      simpa [usize_const_val_0] using this
    exact List.eq_nil_of_length_eq_zero hlen
  · intro h
    exact usize_eq_of_val_eq (by simp [h])

private theorem memId_copy_to_limit_ok {v : alloc.vec.Vec MemoryId} {limit : Std.Usize}
    (hlim : limit.val ≤ v.val.length) :
    ∀ n (acc : alloc.vec.Vec MemoryId) (pos : Std.Usize)
      {out : alloc.vec.Vec MemoryId} {pos' : Std.Usize},
      limit.val - pos.val = n →
      pos.val ≤ limit.val →
      loop (fun (acc1, pos1) => do
        if pos1 < limit then
          let mi ← alloc.vec.Vec.index
            (core.slice.index.SliceIndexUsizeSlice MemoryId) v pos1
          let mi1 ← verifier.MemoryId.Insts.CoreCloneClone.clone mi
          let acc2 ← alloc.vec.Vec.push acc1 mi1
          let pos2 ← pos1 + 1#usize
          ok (ControlFlow.cont (acc2, pos2))
        else ok (ControlFlow.done (acc1, pos1))) (acc, pos) = ok (out, pos') →
      out.val = acc.val ++ (v.val.drop pos.val).take (limit.val - pos.val) ∧
      pos' = limit := by
  intro n
  induction n with
  | zero =>
    intro acc pos out pos' hrem hle hloop
    have hpeq : pos.val = limit.val := by omega
    have hnlt : ¬ (pos < limit) := by intro hc; have : pos.val < limit.val := hc; omega
    rw [loop_unfold_step] at hloop
    simp only [hnlt, ↓reduceIte, bind_ok] at hloop
    rcases Prod.mk.inj (ok_inj hloop) with ⟨rfl, rfl⟩
    exact ⟨by simp [hpeq], usize_eq_of_val_eq hpeq⟩
  | succ k ih =>
    intro acc pos out pos' hrem hle hloop
    have hlt_lim : pos.val < limit.val := by omega
    have hlt_v : pos.val < v.val.length := Nat.lt_of_lt_of_le hlt_lim hlim
    have hlt : pos < limit := hlt_lim
    rw [loop_unfold_step] at hloop
    rcases bind_eq_ok hloop with ⟨r, hbody, hloop⟩
    simp only [hlt, ↓reduceIte] at hbody
    rw [bind_ok_eq (vec_index_usize_eq_ok v pos hlt_v),
      bind_ok_eq (MemoryId_clone_spec _)] at hbody
    rcases bind_eq_ok hbody with ⟨acc1, hpush, hbody⟩
    have hacc1 : acc1.val = acc.val ++ [v.val[pos.val]'hlt_v] := vec_push_ok_val hpush
    have hv_max : v.val.length ≤ Usize.max := v.property
    obtain ⟨pos1, hpos1, hpos1_val⟩ := usize_add_eq_ok pos 1#usize (by rw [usize_one_val]; omega)
    rw [usize_one_val] at hpos1_val
    rw [bind_ok_eq hpos1] at hbody
    cases ok_inj hbody
    dsimp only at hloop
    obtain ⟨hout, hpos'⟩ := ih acc1 pos1 (by omega) (by omega) hloop
    refine ⟨?_, hpos'⟩
    rw [hout, hacc1, List.append_assoc, List.singleton_append, hpos1_val,
      ← drop_take_cons_of_lt v.val hlt_lim hlim]

private theorem memId_copy_to_end_ok (v : alloc.vec.Vec MemoryId) :
    ∀ n (acc : alloc.vec.Vec MemoryId) (pos : Std.Usize)
      {out : alloc.vec.Vec MemoryId},
      v.val.length - pos.val = n →
      pos.val ≤ v.val.length →
      loop (fun (acc1, pos1) => do
        let i := alloc.vec.Vec.len v
        if pos1 < i then
          let mi ← alloc.vec.Vec.index
            (core.slice.index.SliceIndexUsizeSlice MemoryId) v pos1
          let mi1 ← verifier.MemoryId.Insts.CoreCloneClone.clone mi
          let acc2 ← alloc.vec.Vec.push acc1 mi1
          let pos2 ← pos1 + 1#usize
          ok (ControlFlow.cont (acc2, pos2))
        else ok (ControlFlow.done acc1)) (acc, pos) = ok out →
      out.val = acc.val ++ v.val.drop pos.val := by
  intro n
  induction n with
  | zero =>
    intro acc pos out hrem hle hloop
    have hpeq : pos.val = v.val.length := by omega
    have hnlt : ¬ (pos < alloc.vec.Vec.len v) := by
      intro hc; have : pos.val < v.val.length := hc; omega
    rw [loop_unfold_step] at hloop
    simp only [hnlt, ↓reduceIte, bind_ok] at hloop
    cases ok_inj hloop
    simp [hpeq]
  | succ k ih =>
    intro acc pos out hrem hle hloop
    have hlt_v : pos.val < v.val.length := by omega
    have hlt : pos < alloc.vec.Vec.len v := hlt_v
    rw [loop_unfold_step] at hloop
    rcases bind_eq_ok hloop with ⟨r, hbody, hloop⟩
    simp only [hlt, ↓reduceIte] at hbody
    rw [bind_ok_eq (vec_index_usize_eq_ok v pos hlt_v),
      bind_ok_eq (MemoryId_clone_spec _)] at hbody
    rcases bind_eq_ok hbody with ⟨acc1, hpush, hbody⟩
    have hacc1 : acc1.val = acc.val ++ [v.val[pos.val]'hlt_v] := vec_push_ok_val hpush
    have hv_max : v.val.length ≤ Usize.max := v.property
    obtain ⟨pos1, hpos1, hpos1_val⟩ := usize_add_eq_ok pos 1#usize (by rw [usize_one_val]; omega)
    rw [usize_one_val] at hpos1_val
    rw [bind_ok_eq hpos1] at hbody
    cases ok_inj hbody
    dsimp only at hloop
    have hout := ih acc1 pos1 (by omega) (by omega) hloop
    rw [hout, hacc1, List.append_assoc, List.singleton_append, hpos1_val,
      ← List.drop_eq_getElem_cons hlt_v]

private theorem memId_copy_range_total {v : alloc.vec.Vec MemoryId} {limit : Std.Usize}
    (hlim : limit.val ≤ v.val.length) :
    ∀ n (acc : alloc.vec.Vec MemoryId) (pos : Std.Usize),
      limit.val - pos.val = n →
      pos.val ≤ limit.val →
      acc.val.length + (limit.val - pos.val) ≤ Usize.max →
      ∃ out,
        loop (fun (acc1, pos1) => do
          if pos1 < limit then
            let mi ← alloc.vec.Vec.index
              (core.slice.index.SliceIndexUsizeSlice MemoryId) v pos1
            let mi1 ← verifier.MemoryId.Insts.CoreCloneClone.clone mi
            let acc2 ← alloc.vec.Vec.push acc1 mi1
            let pos2 ← pos1 + 1#usize
            ok (ControlFlow.cont (acc2, pos2))
          else ok (ControlFlow.done acc1)) (acc, pos) = ok out ∧
        out.val = acc.val ++ (v.val.drop pos.val).take (limit.val - pos.val) := by
  intro n
  induction n with
  | zero =>
    intro acc pos hrem hle _
    have hpeq : pos.val = limit.val := by omega
    have hnlt : ¬ (pos < limit) := by intro hc; have : pos.val < limit.val := hc; omega
    refine ⟨acc, ?_, by simp [hpeq]⟩
    rw [loop_unfold_step]
    simp [hnlt]
  | succ k ih =>
    intro acc pos hrem hle hcap
    have hlt_lim : pos.val < limit.val := by omega
    have hlt_v : pos.val < v.val.length := Nat.lt_of_lt_of_le hlt_lim hlim
    have hlt : pos < limit := hlt_lim
    obtain ⟨acc1, hpush, hacc1⟩ := vec_push_eq_ok acc (v.val[pos.val]'hlt_v) (by omega)
    have hv_max : v.val.length ≤ Usize.max := v.property
    obtain ⟨pos1, hpos1, hpos1_val⟩ := usize_add_eq_ok pos 1#usize (by rw [usize_one_val]; omega)
    rw [usize_one_val] at hpos1_val
    have hacc1_len : acc1.val.length = acc.val.length + 1 := by simp [hacc1]
    obtain ⟨out, hout, hout_val⟩ := ih acc1 pos1 (by omega) (by omega) (by omega)
    refine ⟨out, ?_, ?_⟩
    · rw [loop_unfold_step]
      simp only [hlt, ↓reduceIte]
      rw [bind_ok_eq (vec_index_usize_eq_ok v pos hlt_v),
        bind_ok_eq (MemoryId_clone_spec _),
        bind_ok_eq hpush, bind_ok_eq hpos1]
      simp only [bind_ok]
      exact hout
    · rw [hout_val, hacc1, List.append_assoc, List.singleton_append, hpos1_val,
        ← drop_take_cons_of_lt v.val hlt_lim hlim]

theorem IdSet_clone_spec (s : verifier.IdSet) :
    verifier.IdSet.Insts.CoreCloneClone.clone s = ok s := by
  unfold verifier.IdSet.Insts.CoreCloneClone.clone verifier.IdSet.Insts.CoreCloneClone.clone_loop
    verifier.IdSet.Insts.CoreCloneClone.clone_loop.body
  have hs_max : s.entries.val.length ≤ Usize.max := s.entries.property
  obtain ⟨out, hout, hout_val⟩ :=
    memId_copy_range_total (v := s.entries) (limit := alloc.vec.Vec.len s.entries)
      (by simp) s.entries.val.length (alloc.vec.Vec.new MemoryId) 0#usize
      (by simp) (Nat.zero_le _) (by simp [hs_max])
  rw [bind_ok_eq hout]
  have heq : out = s.entries :=
    alloc.vec.Vec.ext out s.entries (by simpa [List.take_length] using hout_val)
  cases s with | mk entries =>
  simp [heq]

theorem IdSet_eq_spec (a b : verifier.IdSet) :
    ∃ r : Bool, verifier.IdSet.Insts.CoreCmpPartialEqIdSet.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  unfold verifier.IdSet.Insts.CoreCmpPartialEqIdSet.eq
  by_cases hlen : alloc.vec.Vec.len a.entries != alloc.vec.Vec.len b.entries
  · have hne_len : a.entries.val.length ≠ b.entries.val.length := by
      intro hc
      exact (bne_iff_ne.mp hlen) (usize_eq_of_val_eq hc)
    simp only [hlen, ↓reduceIte]
    refine ⟨false, rfl, ?_⟩
    simp only [Bool.false_eq_true, false_iff]
    intro hab
    subst hab
    exact hne_len rfl
  · simp only [hlen, Bool.false_eq_true, ↓reduceIte]
    have heq_len : a.entries.val.length = b.entries.val.length := by
      have hu : alloc.vec.Vec.len a.entries = alloc.vec.Vec.len b.entries := by
        by_contra hne; exact hlen (bne_iff_ne.mpr hne)
      exact congrArg UScalar.val hu
    suffices ∀ n (i : Std.Usize),
        a.entries.val.length - i.val = n →
        i.val ≤ a.entries.val.length →
        verifier.IdSet.Insts.CoreCmpPartialEqIdSet.eq_loop a.entries b.entries i =
          ok (decide (a.entries.val.drop i.val = b.entries.val.drop i.val)) by
      have h0 := this a.entries.val.length 0#usize (by simp) (Nat.zero_le _)
      simp only [usize_const_val_0, List.drop_zero] at h0
      refine ⟨decide (a.entries.val = b.entries.val), h0, ?_⟩
      cases a; cases b
      simp [alloc.vec.Vec.eq_iff]
    intro n
    induction n with
    | zero =>
      intro i hrem hle
      have hi : i.val = a.entries.val.length := by omega
      unfold verifier.IdSet.Insts.CoreCmpPartialEqIdSet.eq_loop
      rw [loop_unfold_step]
      unfold verifier.IdSet.Insts.CoreCmpPartialEqIdSet.eq_loop.body
      have hnlt : ¬ (i < alloc.vec.Vec.len a.entries) := by
        intro hc; have : i.val < a.entries.val.length := hc; omega
      have ha_d : a.entries.val.drop i.val = [] := by rw [hi, List.drop_length]
      have hb_d : b.entries.val.drop i.val = [] := by rw [hi, heq_len, List.drop_length]
      simp only [hnlt, ↓reduceIte, bind_ok, ha_d, hb_d, decide_true]
    | succ k ih =>
      intro i hrem hle
      have hlta : i.val < a.entries.val.length := by omega
      have hltb : i.val < b.entries.val.length := by omega
      have hlt : i < alloc.vec.Vec.len a.entries := hlta
      unfold verifier.IdSet.Insts.CoreCmpPartialEqIdSet.eq_loop
      rw [loop_unfold_step]
      unfold verifier.IdSet.Insts.CoreCmpPartialEqIdSet.eq_loop.body
      simp only [hlt, ↓reduceIte]
      rw [bind_ok_eq (vec_index_usize_eq_ok a.entries i hlta),
        bind_ok_eq (vec_index_usize_eq_ok b.entries i hltb),
        bind_ok_eq (MemoryId_ne_spec _ _),
        List.drop_eq_getElem_cons hlta, List.drop_eq_getElem_cons hltb]
      by_cases heq_e : (a.entries.val[i.val]'hlta) = (b.entries.val[i.val]'hltb)
      · simp only [heq_e, ne_eq, not_true_eq_false, decide_false, Bool.false_eq_true, ↓reduceIte]
        have ha_max : a.entries.val.length ≤ Usize.max := a.entries.property
        obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by rw [usize_one_val]; omega)
        rw [usize_one_val] at hi1_val
        rw [bind_ok_eq hi1]
        simp only [bind_ok, List.cons.injEq, true_and, ← hi1_val]
        change verifier.IdSet.Insts.CoreCmpPartialEqIdSet.eq_loop a.entries b.entries i1 = _
        exact ih i1 (by omega) (by omega)
      · simp only [ne_eq, heq_e, not_false_eq_true, decide_true, ↓reduceIte, bind_ok]
        congr 1; symm; rw [decide_eq_false_iff_not]
        intro hc; exact heq_e (List.cons.inj hc).1

theorem IdSet_contains_spec {vCounter : Nat} {s : verifier.IdSet}
    (hs : IdSetValid vCounter s) (mid : MemoryId) :
    verifier.IdSet.contains s mid = ok (decide (mid ∈ s.entries.val)) := by
  unfold verifier.IdSet.contains
  suffices ∀ n (i : Std.Usize),
      s.entries.val.length - i.val = n →
      i.val ≤ s.entries.val.length →
      verifier.IdSet.contains_loop s mid i = ok (decide (mid ∈ s.entries.val.drop i.val)) by
    simpa using this s.entries.val.length 0#usize (by simp) (Nat.zero_le _)
  intro n
  induction n with
  | zero =>
    intro i hrem hle
    have hi : i.val = s.entries.val.length := by omega
    unfold verifier.IdSet.contains_loop
    rw [loop_unfold_step]
    unfold verifier.IdSet.contains_loop.body
    have hnlt : ¬ (i < alloc.vec.Vec.len s.entries) := by
      intro hc; have : i.val < s.entries.val.length := hc; omega
    have hd : s.entries.val.drop i.val = [] := by rw [hi, List.drop_length]
    simp [hnlt, hd]
  | succ k ih =>
    intro i hrem hle
    have hlt_s : i.val < s.entries.val.length := by omega
    have hlt : i < alloc.vec.Vec.len s.entries := hlt_s
    unfold verifier.IdSet.contains_loop
    rw [loop_unfold_step]
    unfold verifier.IdSet.contains_loop.body
    simp only [hlt, ↓reduceIte]
    rw [bind_ok_eq (vec_index_usize_eq_ok s.entries i hlt_s),
      bind_ok_eq (MemoryId_cmp_id_spec (s.entries.val[i.val]'hlt_s) mid),
      List.drop_eq_getElem_cons hlt_s]
    cases hcmp : pureMemoryIdCmp (s.entries.val[i.val]'hlt_s) mid with
    | lt =>
      have hne : mid ≠ (s.entries.val[i.val]'hlt_s) := by
        intro heq; rw [heq, pureMemoryIdCmp_refl] at hcmp; cases hcmp
      have hs_max : s.entries.val.length ≤ Usize.max := s.entries.property
      obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by rw [usize_one_val]; omega)
      rw [usize_one_val] at hi1_val
      rw [bind_ok_eq hi1]
      simp only [bind_ok, List.mem_cons, eq_false hne, false_or, ← hi1_val]
      change verifier.IdSet.contains_loop s mid i1 = _
      exact ih i1 (by omega) (by omega)
    | eq =>
      have heq : (s.entries.val[i.val]'hlt_s) = mid :=
        (pureMemoryIdCmp_eq_iff _ mid).mp hcmp
      simp only [bind_ok, heq, List.mem_cons, true_or, decide_true]
    | gt =>
      have hgt : MemoryIdLt mid (s.entries.val[i.val]'hlt_s) :=
        (pureMemoryIdCmp_gt_iff _ mid).mp hcmp
      have hpw_drop : List.Pairwise MemoryIdLt
          ((s.entries.val[i.val]'hlt_s) :: s.entries.val.drop (i.val + 1)) := by
        rw [← List.drop_eq_getElem_cons hlt_s]
        exact pairwise_drop_of_pairwise hs.1 i.val
      rw [List.pairwise_cons] at hpw_drop
      have hnot_mem : mid ∉ (s.entries.val[i.val]'hlt_s) :: s.entries.val.drop (i.val + 1) := by
        intro hmem
        rcases List.mem_cons.mp hmem with hhead | htail
        · rw [hhead] at hgt
          exact MemoryIdLt_irrefl _ hgt
        · exact MemoryIdLt_irrefl _ (MemoryIdLt_trans hgt (hpw_drop.1 mid htail))
      simp only [bind_ok, eq_false hnot_mem, decide_false]

theorem IdSet_insert_ok_spec {vCounter : Nat} {s s' : verifier.IdSet}
    {mid : MemoryId} {b : Bool}
    (hs : IdSetValid vCounter s) (hid : memIdBounded vCounter mid = true)
    (hins : verifier.IdSet.insert s mid = ok (b, s')) :
    IdSetValid vCounter s' ∧
    (∀ x, x ∈ s'.entries.val ↔ x = mid ∨ x ∈ s.entries.val) ∧
    (b = true ↔ mid ∉ s.entries.val) := by
  unfold verifier.IdSet.insert at hins
  rcases bind_eq_ok hins with ⟨⟨b0, v⟩, hloop, hok⟩
  rcases Prod.mk.inj (ok_inj hok) with ⟨rfl, rfl⟩
  suffices ∀ n (i : Std.Usize),
      s.entries.val.length - i.val = n →
      (hle : i.val ≤ s.entries.val.length) →
      (∀ j (hj : j < i.val),
        MemoryIdLt (s.entries.val[j]'(Nat.lt_of_lt_of_le hj hle)) mid) →
      verifier.IdSet.insert_loop0 s mid i = ok (b0, v) →
      IdSetValid vCounter { entries := v } ∧
      (∀ x, x ∈ v.val ↔ x = mid ∨ x ∈ s.entries.val) ∧
      (b0 = true ↔ mid ∉ s.entries.val) by
    exact this s.entries.val.length 0#usize (by simp) (Nat.zero_le _)
      (fun _ hj => by simp at hj) hloop
  intro n
  induction n with
  | zero =>
    intro i hrem hle hbefore hl
    have hi : i.val = s.entries.val.length := by omega
    have hnlt : ¬ (i < alloc.vec.Vec.len s.entries) := by
      intro hc; have : i.val < s.entries.val.length := hc; omega
    unfold verifier.IdSet.insert_loop0 at hl
    rw [loop_unfold_step] at hl
    unfold verifier.IdSet.insert_loop0.body at hl
    rcases bind_eq_ok hl with ⟨r, hbody, hl⟩
    simp only [hnlt, ↓reduceIte] at hbody
    rcases bind_eq_ok hbody with ⟨⟨ne0, pos⟩, hc0, hbody⟩
    rcases bind_eq_ok hbody with ⟨ne1, hpush, hbody⟩
    rcases bind_eq_ok hbody with ⟨ne2, hc1, hbody⟩
    cases ok_inj hbody
    dsimp only at hl
    rcases Prod.mk.inj (ok_inj hl) with ⟨rfl, rfl⟩
    unfold verifier.IdSet.insert_loop0_loop2 verifier.IdSet.insert_loop0_loop2.body at hc0
    obtain ⟨hne0, hpos_eq⟩ := memId_copy_to_limit_ok hle i.val
      (alloc.vec.Vec.new MemoryId) 0#usize (by simp) (Nat.zero_le _) hc0
    subst pos
    have hne1 : ne1.val = s.entries.val.take i.val ++ [mid] := by
      rw [vec_push_ok_val hpush, hne0]; simp
    unfold verifier.IdSet.insert_loop0_loop3 verifier.IdSet.insert_loop0_loop3.body at hc1
    have hne2 := memId_copy_to_end_ok s.entries (s.entries.val.length - i.val)
      ne1 i rfl hle hc1
    rw [hne1] at hne2
    have hvalid : IdSetValid vCounter { entries := ne2 } := by
      refine ⟨?_, ?_⟩
      · rw [hne2]
        exact pairwise_take_insert_drop hs.1 hle hbefore (Or.inl hi)
      · intro m hm
        rw [hne2] at hm
        simp only [List.append_assoc, List.mem_append, List.mem_singleton] at hm
        rcases hm with hm | rfl | hm
        · exact hs.2 m (List.mem_of_mem_take hm)
        · exact hid
        · exact hs.2 m (List.mem_of_mem_drop hm)
    have hmem : ∀ x, x ∈ ne2.val ↔ x = mid ∨ x ∈ s.entries.val := by
      intro x
      rw [hne2]
      conv in x ∈ s.entries.val => rw [← List.take_append_drop i.val s.entries.val]
      simp only [List.append_assoc, List.mem_append, List.mem_singleton]
      constructor
      · rintro (htk | rfl | hdr)
        · exact Or.inr (Or.inl htk)
        · exact Or.inl rfl
        · exact Or.inr (Or.inr hdr)
      · rintro (rfl | htk | hdr)
        · exact Or.inr (Or.inl rfl)
        · exact Or.inl htk
        · exact Or.inr (Or.inr hdr)
    have hnotin : mid ∉ s.entries.val := by
      intro hmem_id
      rcases List.mem_iff_getElem.mp hmem_id with ⟨j, hj, rfl⟩
      have hlt_id := hbefore j (by omega)
      exact MemoryIdLt_irrefl _ hlt_id
    exact ⟨hvalid, hmem, by simp [hnotin]⟩
  | succ k ih =>
    intro i hrem hle hbefore hl
    have hlt_s : i.val < s.entries.val.length := by omega
    have hlt : i < alloc.vec.Vec.len s.entries := hlt_s
    unfold verifier.IdSet.insert_loop0 at hl
    rw [loop_unfold_step] at hl
    unfold verifier.IdSet.insert_loop0.body at hl
    rcases bind_eq_ok hl with ⟨r, hbody, hl⟩
    simp only [hlt, ↓reduceIte] at hbody
    let mi := s.entries.val[i.val]'hlt_s
    rw [bind_ok_eq (vec_index_usize_eq_ok s.entries i hlt_s),
      bind_ok_eq (MemoryId_cmp_id_spec mi mid)] at hbody
    cases hcmp : pureMemoryIdCmp mi mid with
    | lt =>
      rw [hcmp] at hbody
      have hs_max : s.entries.val.length ≤ Usize.max := s.entries.property
      obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by rw [usize_one_val]; omega)
      rw [usize_one_val] at hi1_val
      rw [bind_ok_eq hi1] at hbody
      cases ok_inj hbody
      dsimp only at hl
      have hbefore1 : ∀ j (hj : j < i1.val),
          MemoryIdLt (s.entries.val[j]'(by omega)) mid := by
        intro j hj
        by_cases hji : j < i.val
        · exact hbefore j hji
        · have hjeq : j = i.val := by omega
          subst hjeq
          exact hcmp
      exact ih i1 (by omega) (by omega) hbefore1 hl
    | eq =>
      rw [hcmp] at hbody
      cases ok_inj hbody
      dsimp only at hl
      rcases Prod.mk.inj (ok_inj hl) with ⟨rfl, rfl⟩
      have heq : mi = mid := (pureMemoryIdCmp_eq_iff mi mid).mp hcmp
      have hmem_mi : mid ∈ s.entries.val := by
        rw [← heq]; exact List.getElem_mem hlt_s
      refine ⟨hs, fun x => ?_, by simp [hmem_mi]⟩
      constructor
      · exact Or.inr
      · rintro (rfl | hx)
        · exact hmem_mi
        · exact hx
    | gt =>
      rw [hcmp] at hbody
      have hgt : MemoryIdLt mid mi := (pureMemoryIdCmp_gt_iff mi mid).mp hcmp
      rcases bind_eq_ok hbody with ⟨⟨ne0, pos⟩, hc0, hbody⟩
      rcases bind_eq_ok hbody with ⟨ne1, hpush, hbody⟩
      rcases bind_eq_ok hbody with ⟨ne2, hc1, hbody⟩
      cases ok_inj hbody
      dsimp only at hl
      rcases Prod.mk.inj (ok_inj hl) with ⟨rfl, rfl⟩
      unfold verifier.IdSet.insert_loop0_loop0 verifier.IdSet.insert_loop0_loop0.body at hc0
      obtain ⟨hne0, hpos_eq⟩ := memId_copy_to_limit_ok hle i.val
        (alloc.vec.Vec.new MemoryId) 0#usize (by simp) (Nat.zero_le _) hc0
      subst pos
      have hne1 : ne1.val = s.entries.val.take i.val ++ [mid] := by
        rw [vec_push_ok_val hpush, hne0]; simp
      unfold verifier.IdSet.insert_loop0_loop1 verifier.IdSet.insert_loop0_loop1.body at hc1
      have hne2 := memId_copy_to_end_ok s.entries (s.entries.val.length - i.val)
        ne1 i rfl hle hc1
      rw [hne1] at hne2
      have hvalid : IdSetValid vCounter { entries := ne2 } := by
        refine ⟨?_, ?_⟩
        · rw [hne2]
          exact pairwise_take_insert_drop hs.1 hle hbefore
            (Or.inr ⟨hlt_s, hgt⟩)
        · intro m hm
          rw [hne2] at hm
          simp only [List.append_assoc, List.mem_append, List.mem_singleton] at hm
          rcases hm with hm | rfl | hm
          · exact hs.2 m (List.mem_of_mem_take hm)
          · exact hid
          · exact hs.2 m (List.mem_of_mem_drop hm)
      have hmem : ∀ x, x ∈ ne2.val ↔ x = mid ∨ x ∈ s.entries.val := by
        intro x
        rw [hne2]
        conv in x ∈ s.entries.val => rw [← List.take_append_drop i.val s.entries.val]
        simp only [List.append_assoc, List.mem_append, List.mem_singleton]
        constructor
        · rintro (htk | rfl | hdr)
          · exact Or.inr (Or.inl htk)
          · exact Or.inl rfl
          · exact Or.inr (Or.inr hdr)
        · rintro (rfl | htk | hdr)
          · exact Or.inr (Or.inl rfl)
          · exact Or.inl htk
          · exact Or.inr (Or.inr hdr)
      have hnotin : mid ∉ s.entries.val := by
        intro hmem_id
        rw [← List.take_append_drop i.val s.entries.val, List.mem_append] at hmem_id
        rcases hmem_id with htake | hdrop
        · rw [List.mem_take_iff_getElem] at htake
          rcases htake with ⟨j, hj, heq⟩
          exact MemoryIdLt_irrefl _ (by simpa [heq] using hbefore j (by omega))
        · rw [List.drop_eq_getElem_cons hlt_s] at hdrop
          have hpw_drop : List.Pairwise MemoryIdLt (mi :: s.entries.val.drop (i.val + 1)) := by
            dsimp only [mi]
            rw [← List.drop_eq_getElem_cons hlt_s]
            exact pairwise_drop_of_pairwise hs.1 i.val
          rw [List.pairwise_cons] at hpw_drop
          rcases List.mem_cons.mp hdrop with hhead | htail
          · rw [hhead] at hgt
            exact MemoryIdLt_irrefl _ hgt
          · exact MemoryIdLt_irrefl _ (MemoryIdLt_trans hgt (hpw_drop.1 mid htail))
      exact ⟨hvalid, hmem, by simp [hnotin]⟩

theorem IdSet_remove_spec {vCounter : Nat} {s : verifier.IdSet}
    (hs : IdSetValid vCounter s) (mid : MemoryId) :
    ∃ b s',
      verifier.IdSet.remove s mid = ok (b, s') ∧
      IdSetValid vCounter s' ∧
      (∀ x, x ∈ s'.entries.val ↔ x ∈ s.entries.val ∧ x ≠ mid) ∧
      (b = true ↔ mid ∈ s.entries.val) := by
  unfold verifier.IdSet.remove
  suffices ∀ n (i : Std.Usize),
      s.entries.val.length - i.val = n →
      (hle : i.val ≤ s.entries.val.length) →
      (∀ j (hj : j < i.val),
        MemoryIdLt (s.entries.val[j]'(Nat.lt_of_lt_of_le hj hle)) mid) →
      ∃ b v,
        verifier.IdSet.remove_loop0 s mid i = ok (b, v) ∧
        IdSetValid vCounter { entries := v } ∧
        (∀ x, x ∈ v.val ↔ x ∈ s.entries.val ∧ x ≠ mid) ∧
        (b = true ↔ mid ∈ s.entries.val) by
    obtain ⟨b, v, hl, hval, hmem, hb⟩ :=
      this s.entries.val.length 0#usize (by simp) (Nat.zero_le _) (fun _ hj => by simp at hj)
    exact ⟨b, { entries := v }, by rw [bind_pair_ok_eq hl], hval, hmem, hb⟩
  intro n
  induction n with
  | zero =>
    intro i hrem hle hbefore
    have hi : i.val = s.entries.val.length := by omega
    have hnlt : ¬ (i < alloc.vec.Vec.len s.entries) := by
      intro hc; have : i.val < s.entries.val.length := hc; omega
    have hnotin : mid ∉ s.entries.val := by
      intro hm
      rcases List.mem_iff_getElem.mp hm with ⟨j, hj, rfl⟩
      exact MemoryIdLt_irrefl _ (hbefore j (by omega))
    refine ⟨false, s.entries, ?_, hs, ?_, by simp [hnotin]⟩
    · unfold verifier.IdSet.remove_loop0
      rw [loop_unfold_step]
      unfold verifier.IdSet.remove_loop0.body
      simp [hnlt]
    · intro x
      exact ⟨fun hx => ⟨hx, fun h => by subst h; exact hnotin hx⟩, fun hx => hx.1⟩
  | succ k ih =>
    intro i hrem hle hbefore
    have hlt_s : i.val < s.entries.val.length := by omega
    have hlt : i < alloc.vec.Vec.len s.entries := hlt_s
    let mi := s.entries.val[i.val]'hlt_s
    cases hcmp : pureMemoryIdCmp mi mid with
    | lt =>
      have hs_max : s.entries.val.length ≤ Usize.max := s.entries.property
      obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by rw [usize_one_val]; omega)
      rw [usize_one_val] at hi1_val
      have hbefore1 : ∀ j (hj : j < i1.val), MemoryIdLt (s.entries.val[j]'(by omega)) mid := by
        intro j hj
        by_cases hji : j < i.val
        · exact hbefore j hji
        · have hjeq : j = i.val := by omega
          subst hjeq
          exact hcmp
      obtain ⟨b, v, hl, hval, hmem, hb⟩ := ih i1 (by omega) (by omega) hbefore1
      refine ⟨b, v, ?_, hval, hmem, hb⟩
      unfold verifier.IdSet.remove_loop0
      rw [loop_unfold_step]
      unfold verifier.IdSet.remove_loop0.body
      simp only [hlt, ↓reduceIte]
      rw [bind_ok_eq (vec_index_usize_eq_ok s.entries i hlt_s),
        bind_ok_eq (MemoryId_cmp_id_spec mi mid), hcmp,
        bind_ok_eq hi1]
      simp only [bind_ok]
      exact hl
    | eq =>
      have heq : mi = mid := (pureMemoryIdCmp_eq_iff mi mid).mp hcmp
      have hs_max : s.entries.val.length ≤ Usize.max := s.entries.property
      have hnew_nil : (alloc.vec.Vec.new MemoryId).val = [] := rfl
      have hlen_s : (alloc.vec.Vec.len s.entries).val = s.entries.val.length := by simp
      obtain ⟨ne0, hc0, hne0⟩ :=
        memId_copy_range_total (v := s.entries) (limit := i) hle i.val
          (alloc.vec.Vec.new MemoryId) 0#usize (by simp) (Nat.zero_le _)
          (by rw [hnew_nil]; simp; omega)
      rw [hnew_nil, usize_const_val_0, Nat.sub_zero, List.drop_zero, List.nil_append] at hne0
      obtain ⟨pos, hpos, hpos_val⟩ := usize_add_eq_ok i 1#usize (by rw [usize_one_val]; omega)
      rw [usize_one_val] at hpos_val
      have hne0_len : ne0.val.length = i.val := by
        rw [hne0, List.length_take, Nat.min_eq_left hle]
      obtain ⟨ne1, hc1, hne1⟩ :=
        memId_copy_range_total (v := s.entries) (limit := alloc.vec.Vec.len s.entries)
          (by simp) (s.entries.val.length - pos.val) ne0 pos
          (by rw [hlen_s]) (by rw [hlen_s]; omega)
          (by rw [hlen_s, hne0_len]; omega)
      rw [hlen_s] at hne1
      have htake_end : (s.entries.val.drop pos.val).take (s.entries.val.length - pos.val) =
          s.entries.val.drop (i.val + 1) := by
        rw [hpos_val, ← List.length_drop, List.take_length]
      rw [hne0, htake_end] at hne1
      have hsub := sublist_take_drop_succ s.entries.val hlt_s
      have hvalid : IdSetValid vCounter { entries := ne1 } :=
        ⟨by rw [hne1]; exact hs.1.sublist hsub,
         fun m hm => hs.2 m (hsub.subset (by rwa [← hne1]))⟩
      have hnd : s.entries.val.Nodup := MemoryIdLt_nodup hs.1
      have heq' : (s.entries.val[i.val]'hlt_s) = mid := heq
      have hdecomp : s.entries.val =
          s.entries.val.take i.val ++ (mid :: s.entries.val.drop (i.val + 1)) := by
        conv => lhs; rw [← List.take_append_drop i.val s.entries.val,
          List.drop_eq_getElem_cons hlt_s, heq']
      have hid_not_rem : mid ∉ s.entries.val.take i.val ++ s.entries.val.drop (i.val + 1) := by
        rw [hdecomp] at hnd
        rcases List.nodup_append.mp hnd with ⟨_, hnd2, hdisj⟩
        rw [List.nodup_cons] at hnd2
        rw [List.mem_append]
        rintro (htk | hdr)
        · exact hdisj mid htk mid List.mem_cons_self rfl
        · exact hnd2.1 hdr
      have hmem : ∀ x, x ∈ ne1.val ↔ x ∈ s.entries.val ∧ x ≠ mid := by
        intro x
        rw [hne1]
        constructor
        · intro hx
          refine ⟨hsub.subset hx, ?_⟩
          intro hxi; subst hxi; exact hid_not_rem hx
        · rintro ⟨hx, hne⟩
          rw [hdecomp, List.mem_append, List.mem_cons] at hx
          rw [List.mem_append]
          rcases hx with htk | rfl | hdr
          · exact Or.inl htk
          · exact False.elim (hne rfl)
          · exact Or.inr hdr
      have hid_in : mid ∈ s.entries.val := by rw [← heq']; exact List.getElem_mem hlt_s
      refine ⟨true, ne1, ?_, hvalid, hmem, by simp [hid_in]⟩
      unfold verifier.IdSet.remove_loop0
      rw [loop_unfold_step]
      unfold verifier.IdSet.remove_loop0.body
      simp only [hlt, ↓reduceIte]
      rw [bind_ok_eq (vec_index_usize_eq_ok s.entries i hlt_s),
        bind_ok_eq (MemoryId_cmp_id_spec mi mid), hcmp]
      unfold verifier.IdSet.remove_loop0_loop0 verifier.IdSet.remove_loop0_loop0.body
        verifier.IdSet.remove_loop0_loop1 verifier.IdSet.remove_loop0_loop1.body
      dsimp (config := { zetaHave := true }) only
      rw [bind_ok_eq hc0, bind_ok_eq hpos, bind_ok_eq hc1]
      simp only [bind_ok]
    | gt =>
      have hgt : MemoryIdLt mid mi := (pureMemoryIdCmp_gt_iff mi mid).mp hcmp
      have hnotin : mid ∉ s.entries.val := by
        intro hm
        rw [← List.take_append_drop i.val s.entries.val, List.mem_append] at hm
        rcases hm with htk | hdr
        · rw [List.mem_take_iff_getElem] at htk
          rcases htk with ⟨j, hj, hjeq⟩
          exact MemoryIdLt_irrefl _ (by simpa [hjeq] using hbefore j (by omega))
        · rw [List.drop_eq_getElem_cons hlt_s] at hdr
          have hpw_drop : List.Pairwise MemoryIdLt (mi :: s.entries.val.drop (i.val + 1)) := by
            dsimp only [mi]
            rw [← List.drop_eq_getElem_cons hlt_s]
            exact pairwise_drop_of_pairwise hs.1 i.val
          rw [List.pairwise_cons] at hpw_drop
          rcases List.mem_cons.mp hdr with hhead | htail
          · rw [hhead] at hgt
            exact MemoryIdLt_irrefl _ hgt
          · exact MemoryIdLt_irrefl _ (MemoryIdLt_trans hgt (hpw_drop.1 mid htail))
      refine ⟨false, s.entries, ?_, hs, ?_, by simp [hnotin]⟩
      · unfold verifier.IdSet.remove_loop0
        rw [loop_unfold_step]
        unfold verifier.IdSet.remove_loop0.body
        simp only [hlt, ↓reduceIte]
        rw [bind_ok_eq (vec_index_usize_eq_ok s.entries i hlt_s),
          bind_ok_eq (MemoryId_cmp_id_spec mi mid), hcmp]
        simp only [bind_ok]
      · intro x
        exact ⟨fun hx => ⟨hx, fun h => by subst h; exact hnotin hx⟩, fun hx => hx.1⟩

/-! ### `BoundsMap` Operations -/

theorem BoundsMap_new_spec (vCounter : Nat) :
    ∃ bm, verifier.BoundsMap.new = ok bm ∧
      bm.entries.val = [] ∧ BoundsMapValid vCounter bm := by
  refine ⟨{ entries := alloc.vec.Vec.new (MemoryId × Std.U64) }, rfl, rfl, ?_⟩
  exact ⟨List.Pairwise.nil, fun _ hm => by simp at hm⟩

theorem BoundsMap_default_spec (vCounter : Nat) :
    ∃ bm, verifier.BoundsMap.Insts.CoreDefaultDefault.default = ok bm ∧
      bm.entries.val = [] ∧ BoundsMapValid vCounter bm :=
  BoundsMap_new_spec vCounter

theorem BoundsMap_clear_spec (vCounter : Nat) (self : verifier.BoundsMap) :
    ∃ bm, verifier.BoundsMap.clear self = ok bm ∧
      bm.entries.val = [] ∧ BoundsMapValid vCounter bm := by
  refine ⟨{ entries := alloc.vec.Vec.new (MemoryId × Std.U64) }, rfl, rfl, ?_⟩
  exact ⟨List.Pairwise.nil, fun _ hm => by simp at hm⟩

theorem BoundsMap_len_spec (self : verifier.BoundsMap) :
    verifier.BoundsMap.len self = ok (alloc.vec.Vec.len self.entries) := rfl

private theorem boundsEntry_copy_to_limit_ok
    {v : alloc.vec.Vec (MemoryId × Std.U64)} {limit : Std.Usize}
    (hlim : limit.val ≤ v.val.length) :
    ∀ n (acc : alloc.vec.Vec (MemoryId × Std.U64)) (pos : Std.Usize)
      {out : alloc.vec.Vec (MemoryId × Std.U64)} {pos' : Std.Usize},
      limit.val - pos.val = n →
      pos.val ≤ limit.val →
      loop (fun (acc1, pos1) => do
        if pos1 < limit then
          let (mi, val) ← alloc.vec.Vec.index
            (core.slice.index.SliceIndexUsizeSlice (MemoryId × Std.U64)) v pos1
          let mi1 ← verifier.MemoryId.Insts.CoreCloneClone.clone mi
          let acc2 ← alloc.vec.Vec.push acc1 (mi1, val)
          let pos2 ← pos1 + 1#usize
          ok (ControlFlow.cont (acc2, pos2))
        else ok (ControlFlow.done (acc1, pos1))) (acc, pos) = ok (out, pos') →
      out.val = acc.val ++ (v.val.drop pos.val).take (limit.val - pos.val) ∧
      pos' = limit := by
  intro n
  induction n with
  | zero =>
    intro acc pos out pos' hrem hle hloop
    have hpeq : pos.val = limit.val := by omega
    have hnlt : ¬ (pos < limit) := by intro hc; have : pos.val < limit.val := hc; omega
    rw [loop_unfold_step] at hloop
    simp only [hnlt, ↓reduceIte, bind_ok] at hloop
    rcases Prod.mk.inj (ok_inj hloop) with ⟨rfl, rfl⟩
    exact ⟨by simp [hpeq], usize_eq_of_val_eq hpeq⟩
  | succ k ih =>
    intro acc pos out pos' hrem hle hloop
    have hlt_lim : pos.val < limit.val := by omega
    have hlt_v : pos.val < v.val.length := Nat.lt_of_lt_of_le hlt_lim hlim
    have hlt : pos < limit := hlt_lim
    rw [loop_unfold_step] at hloop
    rcases bind_eq_ok hloop with ⟨r, hbody, hloop⟩
    simp only [hlt, ↓reduceIte] at hbody
    rcases hp : (v.val[pos.val]'hlt_v) with ⟨mi, val⟩
    have hidx : alloc.vec.Vec.index
        (core.slice.index.SliceIndexUsizeSlice (MemoryId × Std.U64)) v pos =
        ok (mi, val) := by
      rw [← hp]; exact vec_index_usize_eq_ok v pos hlt_v
    rw [bind_pair_ok_eq hidx, bind_ok_eq (MemoryId_clone_spec mi)] at hbody
    rcases bind_eq_ok hbody with ⟨acc1, hpush, hbody⟩
    have hacc1 : acc1.val = acc.val ++ [v.val[pos.val]'hlt_v] := by
      rw [hp]; exact vec_push_ok_val hpush
    have hv_max : v.val.length ≤ Usize.max := v.property
    obtain ⟨pos1, hpos1, hpos1_val⟩ := usize_add_eq_ok pos 1#usize (by rw [usize_one_val]; omega)
    rw [usize_one_val] at hpos1_val
    rw [bind_ok_eq hpos1] at hbody
    cases ok_inj hbody
    dsimp only at hloop
    obtain ⟨hout, hpos'⟩ := ih acc1 pos1 (by omega) (by omega) hloop
    refine ⟨?_, hpos'⟩
    rw [hout, hacc1, List.append_assoc, List.singleton_append, hpos1_val,
      ← drop_take_cons_of_lt v.val hlt_lim hlim]

private theorem boundsEntry_copy_to_end_ok (v : alloc.vec.Vec (MemoryId × Std.U64)) :
    ∀ n (acc : alloc.vec.Vec (MemoryId × Std.U64)) (pos : Std.Usize)
      {out : alloc.vec.Vec (MemoryId × Std.U64)},
      v.val.length - pos.val = n →
      pos.val ≤ v.val.length →
      loop (fun (acc1, pos1) => do
        let i := alloc.vec.Vec.len v
        if pos1 < i then
          let (mi, val) ← alloc.vec.Vec.index
            (core.slice.index.SliceIndexUsizeSlice (MemoryId × Std.U64)) v pos1
          let mi1 ← verifier.MemoryId.Insts.CoreCloneClone.clone mi
          let acc2 ← alloc.vec.Vec.push acc1 (mi1, val)
          let pos2 ← pos1 + 1#usize
          ok (ControlFlow.cont (acc2, pos2))
        else ok (ControlFlow.done acc1)) (acc, pos) = ok out →
      out.val = acc.val ++ v.val.drop pos.val := by
  intro n
  induction n with
  | zero =>
    intro acc pos out hrem hle hloop
    have hpeq : pos.val = v.val.length := by omega
    have hnlt : ¬ (pos < alloc.vec.Vec.len v) := by
      intro hc; have : pos.val < v.val.length := hc; omega
    rw [loop_unfold_step] at hloop
    simp only [hnlt, ↓reduceIte, bind_ok] at hloop
    cases ok_inj hloop
    simp [hpeq]
  | succ k ih =>
    intro acc pos out hrem hle hloop
    have hlt_v : pos.val < v.val.length := by omega
    have hlt : pos < alloc.vec.Vec.len v := hlt_v
    rw [loop_unfold_step] at hloop
    rcases bind_eq_ok hloop with ⟨r, hbody, hloop⟩
    simp only [hlt, ↓reduceIte] at hbody
    rcases hp : (v.val[pos.val]'hlt_v) with ⟨mi, val⟩
    have hidx : alloc.vec.Vec.index
        (core.slice.index.SliceIndexUsizeSlice (MemoryId × Std.U64)) v pos =
        ok (mi, val) := by
      rw [← hp]; exact vec_index_usize_eq_ok v pos hlt_v
    rw [bind_pair_ok_eq hidx, bind_ok_eq (MemoryId_clone_spec mi)] at hbody
    rcases bind_eq_ok hbody with ⟨acc1, hpush, hbody⟩
    have hacc1 : acc1.val = acc.val ++ [v.val[pos.val]'hlt_v] := by
      rw [hp]; exact vec_push_ok_val hpush
    have hv_max : v.val.length ≤ Usize.max := v.property
    obtain ⟨pos1, hpos1, hpos1_val⟩ := usize_add_eq_ok pos 1#usize (by rw [usize_one_val]; omega)
    rw [usize_one_val] at hpos1_val
    rw [bind_ok_eq hpos1] at hbody
    cases ok_inj hbody
    dsimp only at hloop
    have hout := ih acc1 pos1 (by omega) (by omega) hloop
    rw [hout, hacc1, List.append_assoc, List.singleton_append, hpos1_val,
      ← List.drop_eq_getElem_cons hlt_v]

theorem BoundsMap_clone_spec (bm : verifier.BoundsMap) :
    verifier.BoundsMap.Insts.CoreCloneClone.clone bm = ok bm := by
  unfold verifier.BoundsMap.Insts.CoreCloneClone.clone
  suffices ∀ n (acc : alloc.vec.Vec (MemoryId × Std.U64)) (i : Std.Usize),
      bm.entries.val.length - i.val = n →
      i.val ≤ bm.entries.val.length →
      acc.val.length + (bm.entries.val.length - i.val) ≤ Usize.max →
      ∃ out, verifier.BoundsMap.Insts.CoreCloneClone.clone_loop bm acc i = ok out ∧
        out.val = acc.val ++ bm.entries.val.drop i.val by
    have hbm_max : bm.entries.val.length ≤ Usize.max := bm.entries.property
    obtain ⟨out, hout, hval⟩ := this bm.entries.val.length
      (alloc.vec.Vec.new (MemoryId × Std.U64)) 0#usize (by simp) (Nat.zero_le _)
      (by simp [hbm_max])
    rw [bind_ok_eq hout]
    have heq : out = bm.entries := alloc.vec.Vec.ext out bm.entries (by simpa using hval)
    cases bm with | mk entries => simp [heq]
  intro n
  induction n with
  | zero =>
    intro acc i hrem hle _
    have hi : i.val = bm.entries.val.length := by omega
    have hnlt : ¬ (i < alloc.vec.Vec.len bm.entries) := by
      intro hc; have : i.val < bm.entries.val.length := hc; omega
    refine ⟨acc, ?_, by simp [hi]⟩
    unfold verifier.BoundsMap.Insts.CoreCloneClone.clone_loop
    rw [loop_unfold_step]
    unfold verifier.BoundsMap.Insts.CoreCloneClone.clone_loop.body
    simp [hnlt]
  | succ k ih =>
    intro acc i hrem hle hcap
    have hlt_v : i.val < bm.entries.val.length := by omega
    have hlt : i < alloc.vec.Vec.len bm.entries := hlt_v
    rcases hp : (bm.entries.val[i.val]'hlt_v) with ⟨mi, val⟩
    have hidx : alloc.vec.Vec.index
        (core.slice.index.SliceIndexUsizeSlice (MemoryId × Std.U64)) bm.entries i =
        ok (mi, val) := by
      rw [← hp]; exact vec_index_usize_eq_ok bm.entries i hlt_v
    obtain ⟨acc1, hpush, hacc1⟩ := vec_push_eq_ok acc (mi, val) (by omega)
    have hbm_max : bm.entries.val.length ≤ Usize.max := bm.entries.property
    obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by rw [usize_one_val]; omega)
    rw [usize_one_val] at hi1_val
    have hacc1_len : acc1.val.length = acc.val.length + 1 := by simp [hacc1]
    obtain ⟨out, hout, hout_val⟩ := ih acc1 i1 (by omega) (by omega) (by omega)
    refine ⟨out, ?_, ?_⟩
    · unfold verifier.BoundsMap.Insts.CoreCloneClone.clone_loop
      rw [loop_unfold_step]
      unfold verifier.BoundsMap.Insts.CoreCloneClone.clone_loop.body
      simp only [hlt, ↓reduceIte]
      rw [bind_pair_ok_eq hidx, bind_ok_eq (MemoryId_clone_spec mi),
        bind_ok_eq hpush, bind_ok_eq hi1]
      simp only [bind_ok]
      exact hout
    · rw [hout_val, hacc1, ← hp, List.append_assoc, List.singleton_append, hi1_val,
        ← List.drop_eq_getElem_cons hlt_v]

theorem BoundsMap_eq_spec (a b : verifier.BoundsMap) :
    ∃ r : Bool, verifier.BoundsMap.Insts.CoreCmpPartialEqBoundsMap.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  unfold verifier.BoundsMap.Insts.CoreCmpPartialEqBoundsMap.eq
  by_cases hlen : alloc.vec.Vec.len a.entries != alloc.vec.Vec.len b.entries
  · have hne_len : a.entries.val.length ≠ b.entries.val.length := by
      intro hc
      exact (bne_iff_ne.mp hlen) (usize_eq_of_val_eq hc)
    simp only [hlen, ↓reduceIte]
    refine ⟨false, rfl, ?_⟩
    simp only [Bool.false_eq_true, false_iff]
    intro hab; subst hab; exact hne_len rfl
  · simp only [hlen, Bool.false_eq_true, ↓reduceIte]
    have heq_len : a.entries.val.length = b.entries.val.length := by
      have hu : alloc.vec.Vec.len a.entries = alloc.vec.Vec.len b.entries := by
        by_contra hne; exact hlen (bne_iff_ne.mpr hne)
      exact congrArg UScalar.val hu
    suffices ∀ n (i : Std.Usize),
        a.entries.val.length - i.val = n →
        i.val ≤ a.entries.val.length →
        verifier.BoundsMap.Insts.CoreCmpPartialEqBoundsMap.eq_loop a.entries b.entries i =
          ok (decide (a.entries.val.drop i.val = b.entries.val.drop i.val)) by
      have h0 := this a.entries.val.length 0#usize (by simp) (Nat.zero_le _)
      simp only [usize_const_val_0, List.drop_zero] at h0
      refine ⟨decide (a.entries.val = b.entries.val), h0, ?_⟩
      cases a; cases b
      simp [alloc.vec.Vec.eq_iff]
    intro n
    induction n with
    | zero =>
      intro i hrem hle
      have hi : i.val = a.entries.val.length := by omega
      unfold verifier.BoundsMap.Insts.CoreCmpPartialEqBoundsMap.eq_loop
      rw [loop_unfold_step]
      unfold verifier.BoundsMap.Insts.CoreCmpPartialEqBoundsMap.eq_loop.body
      have hnlt : ¬ (i < alloc.vec.Vec.len a.entries) := by
        intro hc; have : i.val < a.entries.val.length := hc; omega
      have ha_d : a.entries.val.drop i.val = [] := by rw [hi, List.drop_length]
      have hb_d : b.entries.val.drop i.val = [] := by rw [hi, heq_len, List.drop_length]
      simp only [hnlt, ↓reduceIte, bind_ok, ha_d, hb_d, decide_true]
    | succ k ih =>
      intro i hrem hle
      have hlta : i.val < a.entries.val.length := by omega
      have hltb : i.val < b.entries.val.length := by omega
      have hlt : i < alloc.vec.Vec.len a.entries := hlta
      unfold verifier.BoundsMap.Insts.CoreCmpPartialEqBoundsMap.eq_loop
      rw [loop_unfold_step]
      unfold verifier.BoundsMap.Insts.CoreCmpPartialEqBoundsMap.eq_loop.body
      simp only [hlt, ↓reduceIte]
      rcases hpa : (a.entries.val[i.val]'hlta) with ⟨mia, va⟩
      rcases hpb : (b.entries.val[i.val]'hltb) with ⟨mib, vb⟩
      have hidxa : alloc.vec.Vec.index
          (core.slice.index.SliceIndexUsizeSlice (MemoryId × Std.U64)) a.entries i =
          ok (mia, va) := by rw [← hpa]; exact vec_index_usize_eq_ok a.entries i hlta
      have hidxb : alloc.vec.Vec.index
          (core.slice.index.SliceIndexUsizeSlice (MemoryId × Std.U64)) b.entries i =
          ok (mib, vb) := by rw [← hpb]; exact vec_index_usize_eq_ok b.entries i hltb
      rw [bind_pair_ok_eq hidxa, bind_pair_ok_eq hidxb,
        bind_ok_eq (MemoryId_ne_spec mia mib),
        List.drop_eq_getElem_cons hlta, List.drop_eq_getElem_cons hltb, hpa, hpb]
      by_cases heq_m : mia = mib
      · subst heq_m
        simp only [ne_eq, not_true_eq_false, decide_false, Bool.false_eq_true, ↓reduceIte]
        by_cases hne_v : (va != vb) = true
        · have hv_ne : va ≠ vb := bne_iff_ne.mp hne_v
          simp only [hne_v, ↓reduceIte, bind_ok]
          congr 1; symm; rw [decide_eq_false_iff_not]
          intro hc; exact hv_ne (Prod.mk.inj (List.cons.inj hc).1).2
        · have heq_v : va = vb := by by_contra hc; exact hne_v (bne_iff_ne.mpr hc)
          subst heq_v
          simp only [bne_self_eq_false, Bool.false_eq_true, ↓reduceIte]
          have ha_max : a.entries.val.length ≤ Usize.max := a.entries.property
          obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by rw [usize_one_val]; omega)
          rw [usize_one_val] at hi1_val
          rw [bind_ok_eq hi1]
          simp only [bind_ok, List.cons.injEq, true_and, ← hi1_val]
          change verifier.BoundsMap.Insts.CoreCmpPartialEqBoundsMap.eq_loop
            a.entries b.entries i1 = _
          exact ih i1 (by omega) (by omega)
      · simp only [ne_eq, heq_m, not_false_eq_true, decide_true, ↓reduceIte, bind_ok]
        congr 1; symm; rw [decide_eq_false_iff_not]
        intro hc; exact heq_m (Prod.mk.inj (List.cons.inj hc).1).1

theorem BoundsMap_get_spec {vCounter : Nat} {bm : verifier.BoundsMap}
    (hbm : BoundsMapValid vCounter bm) (mid : MemoryId) :
    ∃ o : Option Std.U64,
      verifier.BoundsMap.get bm mid = ok o ∧
      (∀ v, o = some v → (mid, v) ∈ bm.entries.val) ∧
      (o = none → ∀ v, (mid, v) ∉ bm.entries.val) := by
  unfold verifier.BoundsMap.get
  suffices ∀ n (i : Std.Usize),
      bm.entries.val.length - i.val = n →
      (hle : i.val ≤ bm.entries.val.length) →
      (∀ j (hj : j < i.val),
        MemoryIdLt (bm.entries.val[j]'(Nat.lt_of_lt_of_le hj hle)).1 mid) →
      ∃ o : Option Std.U64,
        verifier.BoundsMap.get_loop bm mid i = ok o ∧
        (∀ v, o = some v → (mid, v) ∈ bm.entries.val) ∧
        (o = none → ∀ v, (mid, v) ∉ bm.entries.val) by
    exact this bm.entries.val.length 0#usize (by simp) (Nat.zero_le _)
      (fun _ hj => by simp at hj)
  intro n
  induction n with
  | zero =>
    intro i hrem hle hbefore
    have hi : i.val = bm.entries.val.length := by omega
    have hnlt : ¬ (i < alloc.vec.Vec.len bm.entries) := by
      intro hc; have : i.val < bm.entries.val.length := hc; omega
    refine ⟨none, ?_, by simp, ?_⟩
    · unfold verifier.BoundsMap.get_loop
      rw [loop_unfold_step]
      unfold verifier.BoundsMap.get_loop.body
      simp [hnlt]
    · intro _ v hmem
      rcases List.mem_iff_getElem.mp hmem with ⟨j, hj, hjeq⟩
      have hlt_id := hbefore j (by omega)
      rw [hjeq] at hlt_id
      exact MemoryIdLt_irrefl mid hlt_id
  | succ k ih =>
    intro i hrem hle hbefore
    have hlt_b : i.val < bm.entries.val.length := by omega
    have hlt : i < alloc.vec.Vec.len bm.entries := hlt_b
    rcases hp : (bm.entries.val[i.val]'hlt_b) with ⟨mi, val⟩
    have hidx : alloc.vec.Vec.index
        (core.slice.index.SliceIndexUsizeSlice (MemoryId × Std.U64)) bm.entries i =
        ok (mi, val) := by rw [← hp]; exact vec_index_usize_eq_ok bm.entries i hlt_b
    cases hcmp : pureMemoryIdCmp mi mid with
    | lt =>
      have hbm_max : bm.entries.val.length ≤ Usize.max := bm.entries.property
      obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by rw [usize_one_val]; omega)
      rw [usize_one_val] at hi1_val
      have hbefore1 : ∀ j (hj : j < i1.val),
          MemoryIdLt (bm.entries.val[j]'(by omega)).1 mid := by
        intro j hj
        by_cases hji : j < i.val
        · exact hbefore j hji
        · have hjeq : j = i.val := by omega
          subst hjeq
          simpa [MemoryIdLt, hp] using hcmp
      obtain ⟨o, hl, hsome, hnone⟩ := ih i1 (by omega) (by omega) hbefore1
      refine ⟨o, ?_, hsome, hnone⟩
      unfold verifier.BoundsMap.get_loop
      rw [loop_unfold_step]
      unfold verifier.BoundsMap.get_loop.body
      simp only [hlt, ↓reduceIte]
      rw [bind_pair_ok_eq hidx, bind_ok_eq (MemoryId_cmp_id_spec mi mid), hcmp,
        bind_ok_eq hi1]
      simp only [bind_ok]
      exact hl
    | eq =>
      have heq : mi = mid := (pureMemoryIdCmp_eq_iff mi mid).mp hcmp
      subst heq
      refine ⟨some val, ?_, ?_, by simp⟩
      · unfold verifier.BoundsMap.get_loop
        rw [loop_unfold_step]
        unfold verifier.BoundsMap.get_loop.body
        simp only [hlt, ↓reduceIte]
        rw [bind_pair_ok_eq hidx, bind_ok_eq (MemoryId_cmp_id_spec mi mi), hcmp]
        simp only [bind_ok]
      · intro v hv
        cases Option.some.inj hv
        rw [← hp]
        exact List.getElem_mem hlt_b
    | gt =>
      have hgt : MemoryIdLt mid mi := (pureMemoryIdCmp_gt_iff mi mid).mp hcmp
      refine ⟨none, ?_, by simp, ?_⟩
      · unfold verifier.BoundsMap.get_loop
        rw [loop_unfold_step]
        unfold verifier.BoundsMap.get_loop.body
        simp only [hlt, ↓reduceIte]
        rw [bind_pair_ok_eq hidx, bind_ok_eq (MemoryId_cmp_id_spec mi mid), hcmp]
        simp only [bind_ok]
      · intro _ v hmem
        rw [← List.take_append_drop i.val bm.entries.val, List.mem_append] at hmem
        rcases hmem with htk | hdr
        · rw [List.mem_take_iff_getElem] at htk
          rcases htk with ⟨j, hj, hjeq⟩
          have hlt_id := hbefore j (by omega)
          rw [hjeq] at hlt_id
          exact MemoryIdLt_irrefl mid hlt_id
        · have hmap_mem : mid ∈ (bm.entries.val.drop i.val).map Prod.fst :=
            List.mem_map_of_mem (f := Prod.fst) hdr
          have hpw_drop : List.Pairwise MemoryIdLt
              ((bm.entries.val.map Prod.fst).drop i.val) :=
            pairwise_drop_of_pairwise hbm.1 i.val
          rw [← List.map_drop, List.drop_eq_getElem_cons hlt_b, hp, List.map_cons,
            List.pairwise_cons] at hpw_drop
          rw [List.drop_eq_getElem_cons hlt_b, hp, List.map_cons, List.mem_cons] at hmap_mem
          rcases hmap_mem with rfl | htail
          · exact MemoryIdLt_irrefl _ hgt
          · exact MemoryIdLt_irrefl _ (MemoryIdLt_trans hgt (hpw_drop.1 mid htail))

theorem BoundsMap_get_or_zero_spec {vCounter : Nat} {bm : verifier.BoundsMap}
    (hbm : BoundsMapValid vCounter bm) (mid : MemoryId) :
    ∃ b : Std.U64,
      verifier.BoundsMap.get_or_zero bm mid = ok b ∧
      ((b = 0#u64 ∧ ∀ v, (mid, v) ∉ bm.entries.val) ∨ (mid, b) ∈ bm.entries.val) := by
  unfold verifier.BoundsMap.get_or_zero
  obtain ⟨o, hget, hsome, hnone⟩ := BoundsMap_get_spec hbm mid
  rw [bind_ok_eq hget]
  cases o with
  | none => exact ⟨0#u64, rfl, Or.inl ⟨rfl, hnone rfl⟩⟩
  | some v => exact ⟨v, rfl, Or.inr (hsome v rfl)⟩

theorem BoundsMap_insert_ok_spec {vCounter : Nat} {bm bm' : verifier.BoundsMap}
    {mid : MemoryId} {val : Std.U64} {prev : Option Std.U64}
    (hbm : BoundsMapValid vCounter bm) (hid : memIdBounded vCounter mid = true)
    (hins : verifier.BoundsMap.insert bm mid val = ok (prev, bm')) :
    BoundsMapValid vCounter bm' ∧
    (∀ m b, (m, b) ∈ bm'.entries.val ↔
      (m = mid ∧ b = val) ∨ (m ≠ mid ∧ (m, b) ∈ bm.entries.val)) := by
  unfold verifier.BoundsMap.insert at hins
  rcases bind_eq_ok hins with ⟨⟨o, v⟩, hloop, hok⟩
  rcases Prod.mk.inj (ok_inj hok) with ⟨rfl, rfl⟩
  suffices ∀ n (i : Std.Usize),
      bm.entries.val.length - i.val = n →
      (hle : i.val ≤ bm.entries.val.length) →
      (∀ j (hj : j < i.val),
        MemoryIdLt (bm.entries.val[j]'(Nat.lt_of_lt_of_le hj hle)).1 mid) →
      verifier.BoundsMap.insert_loop0 bm mid val i = ok (o, v) →
      BoundsMapValid vCounter { entries := v } ∧
      (∀ m b, (m, b) ∈ v.val ↔
        (m = mid ∧ b = val) ∨ (m ≠ mid ∧ (m, b) ∈ bm.entries.val)) by
    exact this bm.entries.val.length 0#usize (by simp) (Nat.zero_le _)
      (fun _ hj => by simp at hj) hloop
  intro n
  induction n with
  | zero =>
    intro i hrem hle hbefore hl
    have hi : i.val = bm.entries.val.length := by omega
    have hnlt : ¬ (i < alloc.vec.Vec.len bm.entries) := by
      intro hc; have : i.val < bm.entries.val.length := hc; omega
    unfold verifier.BoundsMap.insert_loop0 at hl
    rw [loop_unfold_step] at hl
    unfold verifier.BoundsMap.insert_loop0.body at hl
    rcases bind_eq_ok hl with ⟨r, hbody, hl⟩
    simp only [hnlt, ↓reduceIte] at hbody
    rcases bind_eq_ok hbody with ⟨⟨ne0, pos⟩, hc0, hbody⟩
    rcases bind_eq_ok hbody with ⟨ne1, hpush, hbody⟩
    rcases bind_eq_ok hbody with ⟨ne2, hc1, hbody⟩
    cases ok_inj hbody
    dsimp only at hl
    rcases Prod.mk.inj (ok_inj hl) with ⟨rfl, rfl⟩
    unfold verifier.BoundsMap.insert_loop0_loop2 verifier.BoundsMap.insert_loop0_loop2.body at hc0
    obtain ⟨hne0, hpos_eq⟩ := boundsEntry_copy_to_limit_ok hle i.val
      (alloc.vec.Vec.new (MemoryId × Std.U64)) 0#usize (by simp) (Nat.zero_le _) hc0
    subst pos
    have hne1 : ne1.val = bm.entries.val.take i.val ++ [(mid, val)] := by
      rw [vec_push_ok_val hpush, hne0]; simp
    unfold verifier.BoundsMap.insert_loop0_loop3 verifier.BoundsMap.insert_loop0_loop3.body at hc1
    have hne2 := boundsEntry_copy_to_end_ok bm.entries (bm.entries.val.length - i.val)
      ne1 i rfl hle hc1
    rw [hne1] at hne2
    have hnotin : ∀ b0, (mid, b0) ∉ bm.entries.val := by
      intro b0 hm
      rcases List.mem_iff_getElem.mp hm with ⟨j, hj, hjeq⟩
      have hlt_id := hbefore j (by omega)
      rw [hjeq] at hlt_id
      exact MemoryIdLt_irrefl mid hlt_id
    refine ⟨⟨?_, ?_⟩, ?_⟩
    · rw [hne2]
      simp only [List.map_append, List.map_take, List.map_drop, List.map_singleton]
      exact pairwise_take_insert_drop hbm.1
        (by simpa using hle)
        (fun j hj => by simpa using hbefore j hj)
        (Or.inl (by simpa using hi))
    · intro p hp
      rw [hne2] at hp
      simp only [List.append_assoc, List.mem_append, List.mem_singleton] at hp
      rcases hp with hp | rfl | hp
      · exact hbm.2 p (List.mem_of_mem_take hp)
      · exact hid
      · exact hbm.2 p (List.mem_of_mem_drop hp)
    · intro m b
      rw [hne2]
      conv in (m, b) ∈ bm.entries.val => rw [← List.take_append_drop i.val bm.entries.val]
      simp only [List.append_assoc, List.mem_append, List.mem_singleton, Prod.mk.injEq]
      constructor
      · rintro (htk | ⟨rfl, rfl⟩ | hdr)
        · refine Or.inr ⟨?_, Or.inl htk⟩
          intro hmi; subst hmi; exact hnotin b (List.mem_of_mem_take htk)
        · exact Or.inl ⟨rfl, rfl⟩
        · refine Or.inr ⟨?_, Or.inr hdr⟩
          intro hmi; subst hmi; exact hnotin b (List.mem_of_mem_drop hdr)
      · rintro (⟨rfl, rfl⟩ | ⟨_, htk | hdr⟩)
        · exact Or.inr (Or.inl ⟨rfl, rfl⟩)
        · exact Or.inl htk
        · exact Or.inr (Or.inr hdr)
  | succ k ih =>
    intro i hrem hle hbefore hl
    have hlt_b : i.val < bm.entries.val.length := by omega
    have hlt : i < alloc.vec.Vec.len bm.entries := hlt_b
    unfold verifier.BoundsMap.insert_loop0 at hl
    rw [loop_unfold_step] at hl
    unfold verifier.BoundsMap.insert_loop0.body at hl
    rcases bind_eq_ok hl with ⟨r, hbody, hl⟩
    simp only [hlt, ↓reduceIte] at hbody
    rcases hp : (bm.entries.val[i.val]'hlt_b) with ⟨mi, i2⟩
    have hidx : alloc.vec.Vec.index
        (core.slice.index.SliceIndexUsizeSlice (MemoryId × Std.U64)) bm.entries i =
        ok (mi, i2) := by rw [← hp]; exact vec_index_usize_eq_ok bm.entries i hlt_b
    rw [bind_pair_ok_eq hidx, bind_ok_eq (MemoryId_cmp_id_spec mi mid)] at hbody
    cases hcmp : pureMemoryIdCmp mi mid with
    | lt =>
      rw [hcmp] at hbody
      have hbm_max : bm.entries.val.length ≤ Usize.max := bm.entries.property
      obtain ⟨i3, hi3, hi3_val⟩ := usize_add_eq_ok i 1#usize (by rw [usize_one_val]; omega)
      rw [usize_one_val] at hi3_val
      rw [bind_ok_eq hi3] at hbody
      cases ok_inj hbody
      dsimp only at hl
      have hbefore1 : ∀ j (hj : j < i3.val),
          MemoryIdLt (bm.entries.val[j]'(by omega)).1 mid := by
        intro j hj
        by_cases hji : j < i.val
        · exact hbefore j hji
        · have hjeq : j = i.val := by omega
          subst hjeq
          simpa [MemoryIdLt, hp] using hcmp
      exact ih i3 (by omega) (by omega) hbefore1 hl
    | eq =>
      rw [hcmp] at hbody
      have heq : mi = mid := (pureMemoryIdCmp_eq_iff mi mid).mp hcmp
      subst heq
      rcases bind_eq_ok hbody with ⟨⟨⟨mi1, u0⟩, back⟩, hmut, hbody⟩
      cases ok_inj hbody
      dsimp only at hl
      rcases Prod.mk.inj (ok_inj hl) with ⟨rfl, rfl⟩
      obtain ⟨_, hget_mut, hset_mut⟩ := vec_index_mut_usize_ok hmut
      rw [List.getElem?_eq_getElem hlt_b, hp, Option.some.injEq, Prod.mk.injEq] at hget_mut
      rcases hget_mut with ⟨rfl, rfl⟩
      have hv_val : (back (mi, val)).val = bm.entries.val.set i.val (mi, val) :=
        (hset_mut (mi, val)).1
      have hfst_i : (bm.entries.val[i.val]'hlt_b).1 = mi := by rw [hp]
      refine ⟨⟨?_, ?_⟩, ?_⟩
      · rw [hv_val, list_map_fst_set bm.entries.val i.val mi val hlt_b hfst_i]
        exact hbm.1
      · intro p hp_in
        rw [hv_val] at hp_in
        rcases List.mem_or_eq_of_mem_set hp_in with hmem | rfl
        · exact hbm.2 p hmem
        · exact hid
      · intro m b
        rw [hv_val]
        exact mem_set_iff_of_nodup_fst bm.entries.val i.val mi val hlt_b
          (MemoryIdLt_nodup hbm.1) hfst_i m b
    | gt =>
      rw [hcmp] at hbody
      have hgt : MemoryIdLt mid mi := (pureMemoryIdCmp_gt_iff mi mid).mp hcmp
      rcases bind_eq_ok hbody with ⟨⟨ne0, pos⟩, hc0, hbody⟩
      rcases bind_eq_ok hbody with ⟨ne1, hpush, hbody⟩
      rcases bind_eq_ok hbody with ⟨ne2, hc1, hbody⟩
      cases ok_inj hbody
      dsimp only at hl
      rcases Prod.mk.inj (ok_inj hl) with ⟨rfl, rfl⟩
      unfold verifier.BoundsMap.insert_loop0_loop0 verifier.BoundsMap.insert_loop0_loop0.body at hc0
      obtain ⟨hne0, hpos_eq⟩ := boundsEntry_copy_to_limit_ok hle i.val
        (alloc.vec.Vec.new (MemoryId × Std.U64)) 0#usize (by simp) (Nat.zero_le _) hc0
      subst pos
      have hne1 : ne1.val = bm.entries.val.take i.val ++ [(mid, val)] := by
        rw [vec_push_ok_val hpush, hne0]; simp
      unfold verifier.BoundsMap.insert_loop0_loop1 verifier.BoundsMap.insert_loop0_loop1.body at hc1
      have hne2 := boundsEntry_copy_to_end_ok bm.entries (bm.entries.val.length - i.val)
        ne1 i rfl hle hc1
      rw [hne1] at hne2
      have hnotin : ∀ b0, (mid, b0) ∉ bm.entries.val := by
        intro b0 hm
        rw [← List.take_append_drop i.val bm.entries.val, List.mem_append] at hm
        rcases hm with htk | hdr
        · rw [List.mem_take_iff_getElem] at htk
          rcases htk with ⟨j, hj, hjeq⟩
          have hlt_id := hbefore j (by omega)
          rw [hjeq] at hlt_id
          exact MemoryIdLt_irrefl mid hlt_id
        · have hmap_mem : mid ∈ (bm.entries.val.drop i.val).map Prod.fst :=
            List.mem_map_of_mem (f := Prod.fst) hdr
          have hpw_drop : List.Pairwise MemoryIdLt
              ((bm.entries.val.map Prod.fst).drop i.val) :=
            pairwise_drop_of_pairwise hbm.1 i.val
          rw [← List.map_drop, List.drop_eq_getElem_cons hlt_b, hp, List.map_cons,
            List.pairwise_cons] at hpw_drop
          rw [List.drop_eq_getElem_cons hlt_b, hp, List.map_cons, List.mem_cons] at hmap_mem
          rcases hmap_mem with rfl | htail
          · exact MemoryIdLt_irrefl _ hgt
          · exact MemoryIdLt_irrefl _ (MemoryIdLt_trans hgt (hpw_drop.1 mid htail))
      refine ⟨⟨?_, ?_⟩, ?_⟩
      · rw [hne2]
        simp only [List.map_append, List.map_take, List.map_drop, List.map_singleton]
        exact pairwise_take_insert_drop hbm.1
          (by simpa using hle)
          (fun j hj => by simpa using hbefore j hj)
          (Or.inr ⟨by simpa using hlt_b, by simpa [hp] using hgt⟩)
      · intro p hp_in
        rw [hne2] at hp_in
        simp only [List.append_assoc, List.mem_append, List.mem_singleton] at hp_in
        rcases hp_in with hp_in | rfl | hp_in
        · exact hbm.2 p (List.mem_of_mem_take hp_in)
        · exact hid
        · exact hbm.2 p (List.mem_of_mem_drop hp_in)
      · intro m b
        rw [hne2]
        conv in (m, b) ∈ bm.entries.val => rw [← List.take_append_drop i.val bm.entries.val]
        simp only [List.append_assoc, List.mem_append, List.mem_singleton, Prod.mk.injEq]
        constructor
        · rintro (htk | ⟨rfl, rfl⟩ | hdr)
          · refine Or.inr ⟨?_, Or.inl htk⟩
            intro hmi; subst hmi; exact hnotin b (List.mem_of_mem_take htk)
          · exact Or.inl ⟨rfl, rfl⟩
          · refine Or.inr ⟨?_, Or.inr hdr⟩
            intro hmi; subst hmi; exact hnotin b (List.mem_of_mem_drop hdr)
        · rintro (⟨rfl, rfl⟩ | ⟨_, htk | hdr⟩)
          · exact Or.inr (Or.inl ⟨rfl, rfl⟩)
          · exact Or.inl htk
          · exact Or.inr (Or.inr hdr)

theorem BoundsMap_update_max_ok_spec {vCounter : Nat} {bm bm' : verifier.BoundsMap}
    {mid : MemoryId} {nb : Std.U64}
    (hbm : BoundsMapValid vCounter bm) (hid : memIdBounded vCounter mid = true)
    (hupd : verifier.BoundsMap.update_max bm mid nb = ok bm') :
    BoundsMapValid vCounter bm' ∧
    (∀ m b, (m, b) ∈ bm'.entries.val →
      (m = mid ∧ b = nb) ∨ (m, b) ∈ bm.entries.val) ∧
    (∃ b, (mid, b) ∈ bm'.entries.val ∧ nb.bv ≤ b.bv) := by
  unfold verifier.BoundsMap.update_max at hupd
  rcases bind_eq_ok hupd with ⟨v, hloop, hok⟩
  cases ok_inj hok
  suffices ∀ n (i : Std.Usize),
      bm.entries.val.length - i.val = n →
      (hle : i.val ≤ bm.entries.val.length) →
      (∀ j (hj : j < i.val),
        MemoryIdLt (bm.entries.val[j]'(Nat.lt_of_lt_of_le hj hle)).1 mid) →
      verifier.BoundsMap.update_max_loop0 bm mid nb i = ok v →
      BoundsMapValid vCounter { entries := v } ∧
      (∀ m b, (m, b) ∈ v.val → (m = mid ∧ b = nb) ∨ (m, b) ∈ bm.entries.val) ∧
      (∃ b, (mid, b) ∈ v.val ∧ nb.bv ≤ b.bv) by
    exact this bm.entries.val.length 0#usize (by simp) (Nat.zero_le _)
      (fun _ hj => by simp at hj) hloop
  intro n
  induction n with
  | zero =>
    intro i hrem hle hbefore hl
    have hi : i.val = bm.entries.val.length := by omega
    have hnlt : ¬ (i < alloc.vec.Vec.len bm.entries) := by
      intro hc; have : i.val < bm.entries.val.length := hc; omega
    unfold verifier.BoundsMap.update_max_loop0 at hl
    rw [loop_unfold_step] at hl
    unfold verifier.BoundsMap.update_max_loop0.body at hl
    rcases bind_eq_ok hl with ⟨r, hbody, hl⟩
    simp only [hnlt, ↓reduceIte] at hbody
    rcases bind_eq_ok hbody with ⟨⟨ne0, pos⟩, hc0, hbody⟩
    rcases bind_eq_ok hbody with ⟨ne1, hpush, hbody⟩
    rcases bind_eq_ok hbody with ⟨ne2, hc1, hbody⟩
    cases ok_inj hbody
    dsimp only at hl
    cases ok_inj hl
    unfold verifier.BoundsMap.update_max_loop0_loop2
      verifier.BoundsMap.update_max_loop0_loop2.body at hc0
    obtain ⟨hne0, hpos_eq⟩ := boundsEntry_copy_to_limit_ok hle i.val
      (alloc.vec.Vec.new (MemoryId × Std.U64)) 0#usize (by simp) (Nat.zero_le _) hc0
    subst pos
    have hne1 : ne1.val = bm.entries.val.take i.val ++ [(mid, nb)] := by
      rw [vec_push_ok_val hpush, hne0]; simp
    unfold verifier.BoundsMap.update_max_loop0_loop3
      verifier.BoundsMap.update_max_loop0_loop3.body at hc1
    have hne2 := boundsEntry_copy_to_end_ok bm.entries (bm.entries.val.length - i.val)
      ne1 i rfl hle hc1
    rw [hne1] at hne2
    refine ⟨⟨?_, ?_⟩, ?_, ?_⟩
    · rw [hne2]
      simp only [List.map_append, List.map_take, List.map_drop, List.map_singleton]
      exact pairwise_take_insert_drop hbm.1
        (by simpa using hle)
        (fun j hj => by simpa using hbefore j hj)
        (Or.inl (by simpa using hi))
    · intro p hp_in
      rw [hne2] at hp_in
      simp only [List.append_assoc, List.mem_append, List.mem_singleton] at hp_in
      rcases hp_in with hp_in | rfl | hp_in
      · exact hbm.2 p (List.mem_of_mem_take hp_in)
      · exact hid
      · exact hbm.2 p (List.mem_of_mem_drop hp_in)
    · intro m b hmem
      rw [hne2] at hmem
      simp only [List.append_assoc, List.mem_append, List.mem_singleton, Prod.mk.injEq] at hmem
      rcases hmem with htk | ⟨rfl, rfl⟩ | hdr
      · exact Or.inr (List.mem_of_mem_take htk)
      · exact Or.inl ⟨rfl, rfl⟩
      · exact Or.inr (List.mem_of_mem_drop hdr)
    · refine ⟨nb, ?_, BitVec.le_refl _⟩
      rw [hne2]
      simp
  | succ k ih =>
    intro i hrem hle hbefore hl
    have hlt_b : i.val < bm.entries.val.length := by omega
    have hlt : i < alloc.vec.Vec.len bm.entries := hlt_b
    unfold verifier.BoundsMap.update_max_loop0 at hl
    rw [loop_unfold_step] at hl
    unfold verifier.BoundsMap.update_max_loop0.body at hl
    rcases bind_eq_ok hl with ⟨r, hbody, hl⟩
    simp only [hlt, ↓reduceIte] at hbody
    rcases hp : (bm.entries.val[i.val]'hlt_b) with ⟨mi, i2⟩
    have hidx : alloc.vec.Vec.index
        (core.slice.index.SliceIndexUsizeSlice (MemoryId × Std.U64)) bm.entries i =
        ok (mi, i2) := by rw [← hp]; exact vec_index_usize_eq_ok bm.entries i hlt_b
    rw [bind_pair_ok_eq hidx, bind_ok_eq (MemoryId_cmp_id_spec mi mid)] at hbody
    cases hcmp : pureMemoryIdCmp mi mid with
    | lt =>
      rw [hcmp] at hbody
      have hbm_max : bm.entries.val.length ≤ Usize.max := bm.entries.property
      obtain ⟨i3, hi3, hi3_val⟩ := usize_add_eq_ok i 1#usize (by rw [usize_one_val]; omega)
      rw [usize_one_val] at hi3_val
      rw [bind_ok_eq hi3] at hbody
      cases ok_inj hbody
      dsimp only at hl
      have hbefore1 : ∀ j (hj : j < i3.val),
          MemoryIdLt (bm.entries.val[j]'(by omega)).1 mid := by
        intro j hj
        by_cases hji : j < i.val
        · exact hbefore j hji
        · have hjeq : j = i.val := by omega
          subst hjeq
          simpa [MemoryIdLt, hp] using hcmp
      exact ih i3 (by omega) (by omega) hbefore1 hl
    | eq =>
      rw [hcmp, bind_ok_eq (verifier_max_u64_spec i2 nb)] at hbody
      have heq : mi = mid := (pureMemoryIdCmp_eq_iff mi mid).mp hcmp
      subst heq
      rcases bind_eq_ok hbody with ⟨⟨⟨mi1, u0⟩, back⟩, hmut, hbody⟩
      cases ok_inj hbody
      dsimp only at hl
      cases ok_inj hl
      obtain ⟨_, hget_mut, hset_mut⟩ := vec_index_mut_usize_ok hmut
      rw [List.getElem?_eq_getElem hlt_b, hp, Option.some.injEq, Prod.mk.injEq] at hget_mut
      rcases hget_mut with ⟨rfl, rfl⟩
      let mx : Std.U64 := ⟨if nb.bv ≤ i2.bv then i2.bv else nb.bv⟩
      have hv_val : (back (mi, mx)).val = bm.entries.val.set i.val (mi, mx) :=
        (hset_mut (mi, mx)).1
      have hfst_i : (bm.entries.val[i.val]'hlt_b).1 = mi := by rw [hp]
      have hmx_cases : mx = i2 ∨ mx = nb := by
        dsimp only [mx]
        split_ifs with hle_bv
        · left; rw [u64_eq_iff_bv]
        · right; rw [u64_eq_iff_bv]
      refine ⟨⟨?_, ?_⟩, ?_, ?_⟩
      · rw [hv_val, list_map_fst_set bm.entries.val i.val mi mx hlt_b hfst_i]
        exact hbm.1
      · intro p hp_in
        rw [hv_val] at hp_in
        rcases List.mem_or_eq_of_mem_set hp_in with hmem | rfl
        · exact hbm.2 p hmem
        · exact hid
      · intro m b hmem
        rw [hv_val] at hmem
        rcases List.mem_or_eq_of_mem_set hmem with hmem0 | heqp
        · exact Or.inr hmem0
        · rcases Prod.mk.inj heqp with ⟨rfl, rfl⟩
          rcases hmx_cases with hmx_eq | hmx_eq <;> rw [hmx_eq]
          · refine Or.inr ?_
            rw [← hp]; exact List.getElem_mem hlt_b
          · exact Or.inl ⟨rfl, rfl⟩
      · refine ⟨mx, ?_, ?_⟩
        · rw [hv_val]
          exact (mem_set_iff_of_nodup_fst bm.entries.val i.val mi mx hlt_b
            (MemoryIdLt_nodup hbm.1) hfst_i mi mx).mpr (Or.inl ⟨rfl, rfl⟩)
        · dsimp only [mx]
          split_ifs with hle_bv
          · exact hle_bv
          · exact BitVec.le_refl _
    | gt =>
      rw [hcmp] at hbody
      have hgt : MemoryIdLt mid mi := (pureMemoryIdCmp_gt_iff mi mid).mp hcmp
      rcases bind_eq_ok hbody with ⟨⟨ne0, pos⟩, hc0, hbody⟩
      rcases bind_eq_ok hbody with ⟨ne1, hpush, hbody⟩
      rcases bind_eq_ok hbody with ⟨ne2, hc1, hbody⟩
      cases ok_inj hbody
      dsimp only at hl
      cases ok_inj hl
      unfold verifier.BoundsMap.update_max_loop0_loop0
        verifier.BoundsMap.update_max_loop0_loop0.body at hc0
      obtain ⟨hne0, hpos_eq⟩ := boundsEntry_copy_to_limit_ok hle i.val
        (alloc.vec.Vec.new (MemoryId × Std.U64)) 0#usize (by simp) (Nat.zero_le _) hc0
      subst pos
      have hne1 : ne1.val = bm.entries.val.take i.val ++ [(mid, nb)] := by
        rw [vec_push_ok_val hpush, hne0]; simp
      unfold verifier.BoundsMap.update_max_loop0_loop1
        verifier.BoundsMap.update_max_loop0_loop1.body at hc1
      have hne2 := boundsEntry_copy_to_end_ok bm.entries (bm.entries.val.length - i.val)
        ne1 i rfl hle hc1
      rw [hne1] at hne2
      refine ⟨⟨?_, ?_⟩, ?_, ?_⟩
      · rw [hne2]
        simp only [List.map_append, List.map_take, List.map_drop, List.map_singleton]
        exact pairwise_take_insert_drop hbm.1
          (by simpa using hle)
          (fun j hj => by simpa using hbefore j hj)
          (Or.inr ⟨by simpa using hlt_b, by simpa [hp] using hgt⟩)
      · intro p hp_in
        rw [hne2] at hp_in
        simp only [List.append_assoc, List.mem_append, List.mem_singleton] at hp_in
        rcases hp_in with hp_in | rfl | hp_in
        · exact hbm.2 p (List.mem_of_mem_take hp_in)
        · exact hid
        · exact hbm.2 p (List.mem_of_mem_drop hp_in)
      · intro m b hmem
        rw [hne2] at hmem
        simp only [List.append_assoc, List.mem_append, List.mem_singleton, Prod.mk.injEq] at hmem
        rcases hmem with htk | ⟨rfl, rfl⟩ | hdr
        · exact Or.inr (List.mem_of_mem_take htk)
        · exact Or.inl ⟨rfl, rfl⟩
        · exact Or.inr (List.mem_of_mem_drop hdr)
      · refine ⟨nb, ?_, BitVec.le_refl _⟩
        rw [hne2]
        simp

/-! ## 3. `FieldType`, `FieldDescriptor`, `Field`, & `StructDescriptor` -/

theorem FieldType_clone_spec (ft : verifier.FieldType) :
    verifier.FieldType.Insts.CoreCloneClone.clone ft = ok ft := by
  cases ft <;> simp [verifier.FieldType.Insts.CoreCloneClone.clone, MemoryId_clone_spec]

theorem FieldType_eq_spec (a b : verifier.FieldType) :
    ∃ r : Bool, verifier.FieldType.Insts.CoreCmpPartialEqFieldType.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  unfold verifier.FieldType.Insts.CoreCmpPartialEqFieldType.eq
  cases a with
  | Scalar s1 =>
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i s2
    exact ⟨decide (s1 = s2), rfl, by simp⟩
  | MutableScalar s1 =>
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i s2
    exact ⟨decide (s1 = s2), rfl, by simp⟩
  | PtrToMemory b1 id1 sz1 =>
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i b2 id2 sz2
    dsimp only
    by_cases hb : b1 = b2
    · subst hb
      simp only [↓reduceIte, MemoryId_eq_spec, bind_ok]
      by_cases hid : id1 = id2
      · subst hid
        simp only [decide_true, ↓reduceIte]
        exact ⟨decide (sz1 = sz2), rfl, by simp⟩
      · simp only [hid, decide_false, Bool.false_eq_true, ↓reduceIte]
        exact ⟨false, rfl, by simp [hid]⟩
    · simp only [hb, ↓reduceIte]
      exact ⟨false, rfl, by simp [hb]⟩
  | NullablePtrToMemory b1 id1 sz1 =>
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i b2 id2 sz2
    dsimp only
    by_cases hb : b1 = b2
    · subst hb
      simp only [↓reduceIte, MemoryId_eq_spec, bind_ok]
      by_cases hid : id1 = id2
      · subst hid
        simp only [decide_true, ↓reduceIte]
        exact ⟨decide (sz1 = sz2), rfl, by simp⟩
      · simp only [hid, decide_false, Bool.false_eq_true, ↓reduceIte]
        exact ⟨false, rfl, by simp [hid]⟩
    · simp only [hb, ↓reduceIte]
      exact ⟨false, rfl, by simp [hb]⟩
  | PtrToArray b1 id1 =>
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i b2 id2
    dsimp only
    by_cases hb : b1 = b2
    · subst hb
      simp only [↓reduceIte, MemoryId_eq_spec]
      exact ⟨decide (id1 = id2), rfl, by simp⟩
    · simp only [hb, ↓reduceIte]
      exact ⟨false, rfl, by simp [hb]⟩
  | PtrToEndArray b1 id1 =>
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i b2 id2
    dsimp only
    by_cases hb : b1 = b2
    · subst hb
      simp only [↓reduceIte, MemoryId_eq_spec]
      exact ⟨decide (id1 = id2), rfl, by simp⟩
    · simp only [hb, ↓reduceIte]
      exact ⟨false, rfl, by simp [hb]⟩

theorem FieldDescriptor_clone_spec (fd : verifier.FieldDescriptor) :
    verifier.FieldDescriptor.Insts.CoreCloneClone.clone fd = ok fd := by
  cases fd
  simp [verifier.FieldDescriptor.Insts.CoreCloneClone.clone,
    core.clone.impls.CloneUsize.clone, lift, FieldType_clone_spec]

theorem FieldDescriptor_eq_spec (a b : verifier.FieldDescriptor) :
    ∃ r : Bool,
      verifier.FieldDescriptor.Insts.CoreCmpPartialEqFieldDescriptor.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  cases a with | mk off1 ft1 =>
  cases b with | mk off2 ft2 =>
  unfold verifier.FieldDescriptor.Insts.CoreCmpPartialEqFieldDescriptor.eq
  dsimp only
  by_cases hoff : off1 = off2
  · subst hoff
    simp only [↓reduceIte]
    obtain ⟨r, hr, hiff⟩ := FieldType_eq_spec ft1 ft2
    exact ⟨r, hr, by simp [hiff]⟩
  · simp only [hoff, ↓reduceIte]
    exact ⟨false, rfl, by simp [hoff]⟩

theorem FieldDescriptor_ne_spec (a b : verifier.FieldDescriptor) :
    ∃ nr : Bool,
      core.cmp.PartialEq.ne.trait_default
        verifier.FieldDescriptor.Insts.CoreCmpPartialEqFieldDescriptor a b = ok nr ∧
      (nr = true ↔ a ≠ b) := by
  obtain ⟨r, hr, hiff⟩ := FieldDescriptor_eq_spec a b
  exact PartialEq_ne_of_eq _ a b hr hiff

theorem FieldDescriptor_size_spec (fd : verifier.FieldDescriptor) :
    ∃ sz : Std.Usize,
      verifier.FieldDescriptor.size fd = ok sz ∧
      BitVec.ofNat 64 sz.val = (StructFieldDesc.ofCore fd).kind.byteSize ∧
      sz.val = (StructFieldDesc.ofCore fd).kind.byteSize.toNat := by
  unfold verifier.FieldDescriptor.size StructFieldDesc.ofCore
  cases fd.field_type with
  | Scalar size =>
    refine ⟨size, rfl, rfl, ?_⟩
    simp only [StructFieldKind.byteSize, BitVec.toNat_ofNat]
    exact (Nat.mod_eq_of_lt (usize_val_lt_2pow64 size)).symm
  | MutableScalar size =>
    refine ⟨size, rfl, rfl, ?_⟩
    simp only [StructFieldKind.byteSize, BitVec.toNat_ofNat]
    exact (Nat.mod_eq_of_lt (usize_val_lt_2pow64 size)).symm
  | PtrToMemory is32 _ _ | NullablePtrToMemory is32 _ _
  | PtrToArray is32 _ | PtrToEndArray is32 _ =>
    cases is32
    · refine ⟨8#usize, rfl, ?_, ?_⟩
      · simp [StructFieldKind.byteSize]
      · simp [StructFieldKind.byteSize]
    · refine ⟨4#usize, rfl, ?_, ?_⟩
      · simp [StructFieldKind.byteSize]
      · simp [StructFieldKind.byteSize]

theorem Field_new_spec (offset : Std.I16) (width : api.DataWidth) :
    verifier.Field.new offset width = ok { offset, width } := rfl

theorem Field_clone_spec (f : verifier.Field) :
    verifier.Field.Insts.CoreCloneClone.clone f = ok f := rfl

theorem Field_eq_spec (a b : verifier.Field) :
    ∃ r : Bool, verifier.Field.Insts.CoreCmpPartialEqField.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  cases a with | mk o1 w1 =>
  cases b with | mk o2 w2 =>
  unfold verifier.Field.Insts.CoreCmpPartialEqField.eq
  dsimp only
  by_cases ho : o1 = o2
  · subst ho
    simp only [↓reduceIte]
    obtain ⟨r, hr, hiff⟩ := DataWidth_eq_spec w1 w2
    exact ⟨r, hr, by simp [hiff]⟩
  · simp only [ho, ↓reduceIte]
    exact ⟨false, rfl, by simp [ho]⟩

theorem StructDescriptor_default_spec :
    ∃ sd, verifier.StructDescriptor.Insts.CoreDefaultDefault.default = ok sd ∧
      sd.fields.val = [] :=
  ⟨{ fields := alloc.vec.Vec.new verifier.FieldDescriptor }, rfl, rfl⟩

theorem StructDescriptor_clone_spec (sd : verifier.StructDescriptor) :
    verifier.StructDescriptor.Insts.CoreCloneClone.clone sd = ok sd := by
  unfold verifier.StructDescriptor.Insts.CoreCloneClone.clone
    verifier.StructDescriptor.Insts.CoreCloneClone.clone_loop
    verifier.StructDescriptor.Insts.CoreCloneClone.clone_loop.body
  suffices ∀ n (acc : alloc.vec.Vec verifier.FieldDescriptor) (i : Std.Usize),
      sd.fields.val.length - i.val = n →
      i.val ≤ sd.fields.val.length →
      acc.val.length + (sd.fields.val.length - i.val) ≤ Usize.max →
      ∃ out,
        loop (fun (fields1, i1) => do
          let i2 := alloc.vec.Vec.len sd.fields
          if i1 < i2 then
            let fd ← alloc.vec.Vec.index
              (core.slice.index.SliceIndexUsizeSlice verifier.FieldDescriptor) sd.fields i1
            let fd1 ← verifier.FieldDescriptor.Insts.CoreCloneClone.clone fd
            let fields2 ← alloc.vec.Vec.push fields1 fd1
            let i3 ← i1 + 1#usize
            ok (ControlFlow.cont (fields2, i3))
          else ok (ControlFlow.done fields1)) (acc, i) = ok out ∧
        out.val = acc.val ++ sd.fields.val.drop i.val by
    have hmax : sd.fields.val.length ≤ Usize.max := sd.fields.property
    obtain ⟨out, hout, hval⟩ :=
      this sd.fields.val.length (alloc.vec.Vec.new verifier.FieldDescriptor) 0#usize
        (by simp) (Nat.zero_le _) (by simp [hmax])
    rw [bind_ok_eq hout]
    have heq : out = sd.fields := alloc.vec.Vec.ext out sd.fields (by simpa using hval)
    cases sd
    simp [heq]
  intro n
  induction n with
  | zero =>
    intro acc i hrem hle _
    have hi : i.val = sd.fields.val.length := by omega
    have hnlt : ¬ (i < alloc.vec.Vec.len sd.fields) := by
      intro hc; have : i.val < sd.fields.val.length := hc; omega
    refine ⟨acc, ?_, by simp [hi]⟩
    rw [loop_unfold_step]
    simp [hnlt]
  | succ k ih =>
    intro acc i hrem hle hcap
    have hlt_v : i.val < sd.fields.val.length := by omega
    have hlt : i < alloc.vec.Vec.len sd.fields := hlt_v
    obtain ⟨acc1, hpush, hacc1⟩ := vec_push_eq_ok acc (sd.fields.val[i.val]'hlt_v) (by omega)
    have hmax : sd.fields.val.length ≤ Usize.max := sd.fields.property
    obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by rw [usize_one_val]; omega)
    rw [usize_one_val] at hi1_val
    have hacc1_len : acc1.val.length = acc.val.length + 1 := by simp [hacc1]
    obtain ⟨out, hout, hout_val⟩ := ih acc1 i1 (by omega) (by omega) (by omega)
    refine ⟨out, ?_, ?_⟩
    · rw [loop_unfold_step]
      simp only [hlt, ↓reduceIte]
      rw [bind_ok_eq (vec_index_usize_eq_ok sd.fields i hlt_v),
        bind_ok_eq (FieldDescriptor_clone_spec _),
        bind_ok_eq hpush, bind_ok_eq hi1]
      simp only [bind_ok]
      exact hout
    · rw [hout_val, hacc1, List.append_assoc, List.singleton_append, hi1_val,
        ← List.drop_eq_getElem_cons hlt_v]

theorem StructDescriptor_eq_spec (a b : verifier.StructDescriptor) :
    ∃ r : Bool,
      verifier.StructDescriptor.Insts.CoreCmpPartialEqStructDescriptor.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  unfold verifier.StructDescriptor.Insts.CoreCmpPartialEqStructDescriptor.eq
  by_cases hlen : alloc.vec.Vec.len a.fields != alloc.vec.Vec.len b.fields
  · have hne_len : a.fields.val.length ≠ b.fields.val.length := by
      intro hc
      exact (bne_iff_ne.mp hlen) (usize_eq_of_val_eq hc)
    simp only [hlen, ↓reduceIte]
    refine ⟨false, rfl, ?_⟩
    simp only [Bool.false_eq_true, false_iff]
    intro hab
    subst hab
    exact hne_len rfl
  · simp only [hlen, Bool.false_eq_true, ↓reduceIte]
    have heq_len : a.fields.val.length = b.fields.val.length := by
      have hu : alloc.vec.Vec.len a.fields = alloc.vec.Vec.len b.fields := by
        by_contra hne; exact hlen (bne_iff_ne.mpr hne)
      exact congrArg UScalar.val hu
    suffices ∀ n (i : Std.Usize),
        a.fields.val.length - i.val = n →
        i.val ≤ a.fields.val.length →
        ∃ r : Bool,
          verifier.StructDescriptor.Insts.CoreCmpPartialEqStructDescriptor.eq_loop
            a.fields b.fields i = ok r ∧
          (r = true ↔ a.fields.val.drop i.val = b.fields.val.drop i.val) by
      obtain ⟨r, hr, hiff⟩ := this a.fields.val.length 0#usize (by simp) (Nat.zero_le _)
      simp only [usize_const_val_0, List.drop_zero] at hiff
      refine ⟨r, hr, ?_⟩
      rw [hiff]
      cases a; cases b
      simp [alloc.vec.Vec.eq_iff]
    intro n
    induction n with
    | zero =>
      intro i hrem hle
      have hi : i.val = a.fields.val.length := by omega
      unfold verifier.StructDescriptor.Insts.CoreCmpPartialEqStructDescriptor.eq_loop
      rw [loop_unfold_step]
      unfold verifier.StructDescriptor.Insts.CoreCmpPartialEqStructDescriptor.eq_loop.body
      have hnlt : ¬ (i < alloc.vec.Vec.len a.fields) := by
        intro hc; have : i.val < a.fields.val.length := hc; omega
      have ha_d : a.fields.val.drop i.val = [] := by rw [hi, List.drop_length]
      have hb_d : b.fields.val.drop i.val = [] := by rw [hi, heq_len, List.drop_length]
      simp only [hnlt, ↓reduceIte, bind_ok]
      exact ⟨true, rfl, by simp [ha_d, hb_d]⟩
    | succ k ih =>
      intro i hrem hle
      have hlta : i.val < a.fields.val.length := by omega
      have hltb : i.val < b.fields.val.length := by omega
      have hlt : i < alloc.vec.Vec.len a.fields := hlta
      unfold verifier.StructDescriptor.Insts.CoreCmpPartialEqStructDescriptor.eq_loop
      rw [loop_unfold_step]
      unfold verifier.StructDescriptor.Insts.CoreCmpPartialEqStructDescriptor.eq_loop.body
      simp only [hlt, ↓reduceIte]
      rw [bind_ok_eq (vec_index_usize_eq_ok a.fields i hlta),
        bind_ok_eq (vec_index_usize_eq_ok b.fields i hltb)]
      obtain ⟨nr, hnr, hnr_iff⟩ :=
        FieldDescriptor_ne_spec (a.fields.val[i.val]'hlta) (b.fields.val[i.val]'hltb)
      rw [bind_ok_eq hnr]
      cases nr
      · have heq_e : (a.fields.val[i.val]'hlta) = (b.fields.val[i.val]'hltb) := by
          by_contra hne
          have := hnr_iff.mpr hne
          cases this
        simp only [Bool.false_eq_true, ↓reduceIte]
        have ha_max : a.fields.val.length ≤ Usize.max := a.fields.property
        obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by rw [usize_one_val]; omega)
        rw [usize_one_val] at hi1_val
        rw [bind_ok_eq hi1]
        simp only [bind_ok]
        obtain ⟨r, hr, hiff⟩ := ih i1 (by omega) (by omega)
        refine ⟨r, hr, ?_⟩
        rw [hiff, List.drop_eq_getElem_cons hlta, List.drop_eq_getElem_cons hltb, hi1_val]
        simp only [heq_e, List.cons.injEq, true_and]
      · have hne_e : (a.fields.val[i.val]'hlta) ≠ (b.fields.val[i.val]'hltb) := hnr_iff.mp rfl
        simp only [↓reduceIte, bind_ok]
        refine ⟨false, rfl, ?_⟩
        rw [List.drop_eq_getElem_cons hlta, List.drop_eq_getElem_cons hltb]
        simp only [Bool.false_eq_true, false_iff, List.cons.injEq, not_and]
        intro heq
        exact absurd heq hne_e

theorem StructDescriptor_find_field_loop_spec
    (sd : verifier.StructDescriptor) (offset : Std.Usize) :
    ∃ opt : Option verifier.FieldDescriptor,
      verifier.StructDescriptor.find_field_loop sd offset 0#usize = ok opt ∧
      opt.map StructFieldDesc.ofCore =
        findStructField (sd.fields.val.map StructFieldDesc.ofCore)
          (BitVec.ofNat 64 offset.val) ∧
      ∀ fd, opt = some fd →
        fd ∈ sd.fields.val ∧
        fd.offset.val ≤ offset.val ∧
        offset.val < fd.offset.val + (StructFieldDesc.ofCore fd).kind.byteSize.toNat := by
  have heff_toNat : (BitVec.ofNat 64 offset.val).toNat = offset.val := by
    rw [BitVec.toNat_ofNat]
    exact Nat.mod_eq_of_lt (usize_val_lt_2pow64 offset)
  have heff_le_max : (BitVec.ofNat 64 offset.val).toNat ≤ Usize.max := by
    rw [heff_toNat]; exact usize_val_le_max offset
  let pred : StructFieldDesc → Bool := fun f =>
    decide (f.offset.toNat + f.kind.byteSize.toNat ≤ Usize.max) &&
    (f.offset ≤ BitVec.ofNat 64 offset.val) &&
    (BitVec.ofNat 64 offset.val < f.offset + f.kind.byteSize)
  suffices ∀ n (i : Std.Usize),
      sd.fields.val.length - i.val = n →
      i.val ≤ sd.fields.val.length →
      ∃ opt : Option verifier.FieldDescriptor,
        verifier.StructDescriptor.find_field_loop sd offset i = ok opt ∧
        opt.map StructFieldDesc.ofCore =
          ((sd.fields.val.drop i.val).map StructFieldDesc.ofCore).find? pred ∧
        ∀ fd, opt = some fd →
          fd ∈ sd.fields.val.drop i.val ∧
          fd.offset.val ≤ offset.val ∧
          offset.val < fd.offset.val + (StructFieldDesc.ofCore fd).kind.byteSize.toNat by
    obtain ⟨opt, hopt, hmap, hmem⟩ := this sd.fields.val.length 0#usize (by simp) (Nat.zero_le _)
    simp only [usize_const_val_0, List.drop_zero] at hmap hmem
    refine ⟨opt, hopt, ?_, hmem⟩
    unfold findStructField
    simp only [heff_le_max, decide_true, ↓reduceIte]
    exact hmap
  intro n
  induction n with
  | zero =>
    intro i hrem hle
    have hi : i.val = sd.fields.val.length := by omega
    unfold verifier.StructDescriptor.find_field_loop
    rw [loop_unfold_step]
    unfold verifier.StructDescriptor.find_field_loop.body
    have hnlt : ¬ (i < alloc.vec.Vec.len sd.fields) := by
      intro hc; have : i.val < sd.fields.val.length := hc; omega
    have hdrop : sd.fields.val.drop i.val = [] := by rw [hi, List.drop_length]
    simp only [hnlt, ↓reduceIte, bind_ok]
    exact ⟨none, rfl, by simp [hdrop], fun _ h => by cases h⟩
  | succ k ih =>
    intro i hrem hle
    have hlt_v : i.val < sd.fields.val.length := by omega
    have hlt : i < alloc.vec.Vec.len sd.fields := hlt_v
    have hsf_off_toNat :
        (StructFieldDesc.ofCore (sd.fields.val[i.val]'hlt_v)).offset.toNat =
          (sd.fields.val[i.val]'hlt_v).offset.val := by
      simp only [StructFieldDesc.ofCore, BitVec.toNat_ofNat]
      exact Nat.mod_eq_of_lt (usize_val_lt_2pow64 (sd.fields.val[i.val]'hlt_v).offset)
    unfold verifier.StructDescriptor.find_field_loop
    rw [loop_unfold_step]
    unfold verifier.StructDescriptor.find_field_loop.body
    simp only [hlt, ↓reduceIte]
    rw [bind_ok_eq (vec_index_usize_eq_ok sd.fields i hlt_v)]
    obtain ⟨sz, hsz, _, hsz_nat⟩ := FieldDescriptor_size_spec (sd.fields.val[i.val]'hlt_v)
    rw [bind_ok_eq hsz, lift_eq_ok, bind_ok]
    have hmax : sd.fields.val.length ≤ Usize.max := sd.fields.property
    obtain ⟨i1, hi1, hi1_val⟩ := usize_add_eq_ok i 1#usize (by rw [usize_one_val]; omega)
    rw [usize_one_val] at hi1_val
    rcases usize_checked_add_spec (sd.fields.val[i.val]'hlt_v).offset sz with
      ⟨hsum_le, f_end, hfend, hfend_val⟩ | ⟨hsum_gt, hfnone⟩
    · rw [hfend]
      dsimp only
      have hpred_cap :
          decide ((StructFieldDesc.ofCore (sd.fields.val[i.val]'hlt_v)).offset.toNat +
            (StructFieldDesc.ofCore (sd.fields.val[i.val]'hlt_v)).kind.byteSize.toNat ≤ Usize.max) =
            true := by
        rw [decide_eq_true_eq, hsf_off_toNat, ← hsz_nat]
        exact hsum_le
      have hfend_lt_2p64 : (sd.fields.val[i.val]'hlt_v).offset.val + sz.val < 2 ^ 64 :=
        Nat.lt_of_le_of_lt hsum_le usize_max_lt_2pow64
      have hadd_toNat :
          ((StructFieldDesc.ofCore (sd.fields.val[i.val]'hlt_v)).offset +
            (StructFieldDesc.ofCore (sd.fields.val[i.val]'hlt_v)).kind.byteSize).toNat =
            f_end.val := by
        rw [BitVec.toNat_add, hsf_off_toNat, ← hsz_nat, hfend_val,
          Nat.mod_eq_of_lt hfend_lt_2p64]
      by_cases hfo_le : (sd.fields.val[i.val]'hlt_v).offset ≤ offset
      · have hfo_val : (sd.fields.val[i.val]'hlt_v).offset.val ≤ offset.val := hfo_le
        have hsf_le :
            (StructFieldDesc.ofCore (sd.fields.val[i.val]'hlt_v)).offset ≤
              BitVec.ofNat 64 offset.val := by
          rw [BitVec.le_def, hsf_off_toNat, heff_toNat]
          exact hfo_val
        simp only [hfo_le, ↓reduceIte]
        by_cases hoff_lt : offset < f_end
        · have hoff_val : offset.val < f_end.val := hoff_lt
          have hsf_lt :
              BitVec.ofNat 64 offset.val <
                (StructFieldDesc.ofCore (sd.fields.val[i.val]'hlt_v)).offset +
                  (StructFieldDesc.ofCore (sd.fields.val[i.val]'hlt_v)).kind.byteSize := by
            rw [BitVec.lt_def, heff_toNat, hadd_toNat]
            exact hoff_val
          have hpred_true : pred (StructFieldDesc.ofCore (sd.fields.val[i.val]'hlt_v)) = true := by
            dsimp only [pred]
            simp only [hpred_cap, decide_eq_true hsf_le, decide_eq_true hsf_lt, Bool.and_self]
          simp only [hoff_lt, ↓reduceIte, bind_ok]
          refine ⟨some (sd.fields.val[i.val]'hlt_v), rfl, ?_, ?_⟩
          · rw [List.drop_eq_getElem_cons hlt_v]
            simp only [List.map_cons, List.find?_cons, hpred_true, Option.map]
          · intro fd hfd
            cases Option.some.inj hfd
            rw [List.drop_eq_getElem_cons hlt_v]
            refine ⟨List.mem_cons_self, hfo_val, ?_⟩
            omega
        · have hn_val : ¬ (offset.val < f_end.val) := hoff_lt
          have hsf_nlt :
              ¬ (BitVec.ofNat 64 offset.val <
                (StructFieldDesc.ofCore (sd.fields.val[i.val]'hlt_v)).offset +
                  (StructFieldDesc.ofCore (sd.fields.val[i.val]'hlt_v)).kind.byteSize) := by
            rw [BitVec.lt_def, heff_toNat, hadd_toNat]
            exact hn_val
          have hpred_false : pred (StructFieldDesc.ofCore (sd.fields.val[i.val]'hlt_v)) = false := by
            dsimp only [pred]
            simp only [decide_eq_false hsf_nlt, Bool.and_false]
          simp only [hoff_lt, ↓reduceIte]
          rw [bind_ok_eq hi1]
          simp only [bind_ok]
          obtain ⟨opt, hopt, hmap, hmem⟩ := ih i1 (by omega) (by omega)
          refine ⟨opt, hopt, ?_, ?_⟩
          · rw [List.drop_eq_getElem_cons hlt_v]
            simp only [List.map_cons, List.find?_cons, hpred_false, ← hi1_val]
            exact hmap
          · intro fd hfd
            obtain ⟨hm, h1, h2⟩ := hmem fd hfd
            rw [List.drop_eq_getElem_cons hlt_v, ← hi1_val]
            exact ⟨List.mem_cons_of_mem _ hm, h1, h2⟩
      · have hn_val : ¬ ((sd.fields.val[i.val]'hlt_v).offset.val ≤ offset.val) := hfo_le
        have hsf_nle :
            ¬ ((StructFieldDesc.ofCore (sd.fields.val[i.val]'hlt_v)).offset ≤
              BitVec.ofNat 64 offset.val) := by
          rw [BitVec.le_def, hsf_off_toNat, heff_toNat]
          exact hn_val
        have hpred_false : pred (StructFieldDesc.ofCore (sd.fields.val[i.val]'hlt_v)) = false := by
          dsimp only [pred]
          simp only [decide_eq_false hsf_nle, Bool.and_false, Bool.false_and]
        simp only [hfo_le, ↓reduceIte]
        rw [bind_ok_eq hi1]
        simp only [bind_ok]
        obtain ⟨opt, hopt, hmap, hmem⟩ := ih i1 (by omega) (by omega)
        refine ⟨opt, hopt, ?_, ?_⟩
        · rw [List.drop_eq_getElem_cons hlt_v]
          simp only [List.map_cons, List.find?_cons, hpred_false, ← hi1_val]
          exact hmap
        · intro fd hfd
          obtain ⟨hm, h1, h2⟩ := hmem fd hfd
          rw [List.drop_eq_getElem_cons hlt_v, ← hi1_val]
          exact ⟨List.mem_cons_of_mem _ hm, h1, h2⟩
    · rw [hfnone]
      have hpred_cap :
          decide ((StructFieldDesc.ofCore (sd.fields.val[i.val]'hlt_v)).offset.toNat +
            (StructFieldDesc.ofCore (sd.fields.val[i.val]'hlt_v)).kind.byteSize.toNat ≤ Usize.max) =
            false := by
        rw [decide_eq_false_iff_not, hsf_off_toNat, ← hsz_nat]
        omega
      have hpred_false : pred (StructFieldDesc.ofCore (sd.fields.val[i.val]'hlt_v)) = false := by
        dsimp only [pred]
        simp [hpred_cap]
      dsimp only
      rw [bind_ok_eq hi1]
      simp only [bind_ok]
      obtain ⟨opt, hopt, hmap, hmem⟩ := ih i1 (by omega) (by omega)
      refine ⟨opt, hopt, ?_, ?_⟩
      · rw [List.drop_eq_getElem_cons hlt_v]
        simp only [List.map_cons, List.find?_cons, hpred_false, ← hi1_val]
        exact hmap
      · intro fd hfd
        obtain ⟨hm, h1, h2⟩ := hmem fd hfd
        rw [List.drop_eq_getElem_cons hlt_v, ← hi1_val]
        exact ⟨List.mem_cons_of_mem _ hm, h1, h2⟩

theorem StructDescriptor_find_field_ok_some_spec
    {sd : verifier.StructDescriptor} {base_offset : U64ScalarValueData}
    {field : verifier.Field} {fd : verifier.FieldDescriptor}
    {baseOff₁ baseOff₂ : BitVec 64}
    (hconc : ConcretizesScalar64Rel2 base_offset baseOff₁ baseOff₂)
    (hfind : verifier.StructDescriptor.find_field sd base_offset field = ok (some fd)) :
    let effOff := baseOff₁ + sextOffset16 field.offset.bv
    baseOff₂ + sextOffset16 field.offset.bv = effOff ∧
    effOff.toNat ≤ Usize.max ∧
    isAligned effOff (DataWidth.ofCore field.width) = true ∧
    findStructField (sd.fields.val.map StructFieldDesc.ofCore) effOff =
      some (StructFieldDesc.ofCore fd) ∧
    fd ∈ sd.fields.val ∧
    match fd.field_type with
    | .Scalar size | .MutableScalar size =>
        inBoundsRange (effOff - (StructFieldDesc.ofCore fd).offset)
          (DataWidth.ofCore field.width) (BitVec.ofNat 64 size.val) = true
    | .PtrToMemory is_32_bit _ _ | .NullablePtrToMemory is_32_bit _ _
    | .PtrToArray is_32_bit _ | .PtrToEndArray is_32_bit _ =>
        effOff = (StructFieldDesc.ofCore fd).offset ∧
        DataWidth.ofCore field.width = (if is_32_bit then .u32 else .u64) := by
  dsimp only
  unfold verifier.StructDescriptor.find_field at hfind
  obtain ⟨off_sc, hadd, hconc_off⟩ := add_i16_sound_2safety base_offset field.offset hconc
  rw [bind_ok_eq hadd] at hfind
  unfold scalar_value.U64ScalarValueData.is_known at hfind
  simp only [bind_ok] at hfind
  by_cases hkn : off_sc.unknown_mask = 0#u64
  · simp only [hkn, lift_eq_ok, bind_ok] at hfind
    have heff1 : baseOff₁ + sextOffset16 field.offset.bv = off_sc.value.bv :=
      eq_value_of_known_64 hkn hconc_off.1
    have heff2 : baseOff₂ + sextOffset16 field.offset.bv = off_sc.value.bv :=
      eq_value_of_known_64 hkn hconc_off.2.1
    by_cases hgt : off_sc.value > UScalar.cast .U64 core.num.Usize.MAX
    · simp [hgt] at hfind
    · simp only [hgt, ↓reduceIte] at hfind
      have hval_le_max : off_sc.value.val ≤ Usize.max := by
        have hnotgt : ¬ (UScalar.cast .U64 core.num.Usize.MAX).val < off_sc.value.val := hgt
        rw [uscalar_cast_u64_of_usize_val, core_usize_max_val] at hnotgt
        omega
      let offset : Std.Usize := UScalar.cast .Usize off_sc.value
      have hoff_val : offset.val = off_sc.value.val :=
        uscalar_cast_usize_of_u64_val hval_le_max
      have hoff_bv : BitVec.ofNat 64 offset.val = off_sc.value.bv := by
        apply BitVec.eq_of_toNat_eq
        rw [BitVec.toNat_ofNat, hoff_val, ← UScalar.bv_toNat,
          Nat.mod_eq_of_lt off_sc.value.bv.isLt]
      obtain ⟨wb, hwb, hwb_val⟩ := DataWidth_bytes_spec field.width
      rw [bind_ok_eq hwb] at hfind
      have hwb_pos : 0 < wb.val := by
        rw [hwb_val]; cases field.width <;> decide
      have hwb_ne0 : wb ≠ 0#usize := by
        intro hc; subst hc; simp at hwb_pos
      simp only [hwb_ne0, ↓reduceIte] at hfind
      rcases bind_eq_ok hfind with ⟨rem_u, hrem_u, hfind⟩
      have hrem_val : rem_u.val = offset.val % wb.val := by
        have hs := UScalar.rem_bv_spec offset (y := wb) (by omega)
        rw [hrem_u] at hs
        simp at hs
        exact hs.1
      by_cases hrem_ne : (rem_u != 0#usize) = true
      · simp [hrem_ne] at hfind
      · simp only [hrem_ne, Bool.false_eq_true, ↓reduceIte] at hfind
        have hrem_eq0 : rem_u = 0#usize := by
          by_contra hne; exact hrem_ne (bne_iff_ne.mpr hne)
        have hmod0 : offset.val % wb.val = 0 := by
          rw [← hrem_val, hrem_eq0]; rfl
        have halign : isAligned off_sc.value.bv (DataWidth.ofCore field.width) = true := by
          unfold isAligned
          rw [beq_iff_eq]
          apply BitVec.eq_of_toNat_eq
          have hbv_bytes : (DataWidth.ofCore field.width).bvBytes.toNat = wb.val := by
            rw [hwb_val]; cases field.width <;> rfl
          rw [BitVec.toNat_umod, UScalar.bv_toNat, ← hoff_val, hbv_bytes, hmod0]
          rfl
        obtain ⟨opt, hloop, hloop_map, hloop_mem⟩ :=
          StructDescriptor_find_field_loop_spec sd offset
        rw [bind_ok_eq hloop] at hfind
        cases opt with
        | none => simp at hfind
        | some fd0 =>
          dsimp only at hfind
          obtain ⟨hfd0_mem, hfd0_le, _⟩ := hloop_mem fd0 rfl
          rw [hoff_bv] at hloop_map
          have checkScalar :
              ∀ (size : Std.Usize),
                (match Usize.checked_add offset wb with
                  | none => ok none
                  | some access_end =>
                    match Usize.checked_add fd0.offset size with
                    | none => ok none
                    | some field_end =>
                      if access_end <= field_end then ok (some fd0) else ok none) = ok (some fd) →
                fd0 = fd ∧
                inBoundsRange (off_sc.value.bv - BitVec.ofNat 64 fd0.offset.val)
                  (DataWidth.ofCore field.width) (BitVec.ofNat 64 size.val) = true := by
            intro size hsc
            rcases usize_checked_add_spec offset wb with ⟨ha_le, aend, haend, haend_val⟩ | ⟨_, hanone⟩
            · rw [haend] at hsc
              dsimp only at hsc
              rcases usize_checked_add_spec fd0.offset size with ⟨hf_le, fend, hfend, hfend_val⟩ | ⟨_, hfnone⟩
              · rw [hfend] at hsc
                dsimp only at hsc
                by_cases hle_end : aend ≤ fend
                · simp only [hle_end, ↓reduceIte] at hsc
                  have hfd_eq : fd0 = fd := Option.some.inj (ok_inj hsc)
                  refine ⟨hfd_eq, ?_⟩
                  have hle_end_val : aend.val ≤ fend.val := hle_end
                  have hfd_off_toNat : (BitVec.ofNat 64 fd0.offset.val).toNat = fd0.offset.val := by
                    rw [BitVec.toNat_ofNat]
                    exact Nat.mod_eq_of_lt (usize_val_lt_2pow64 fd0.offset)
                  have hsz_toNat : (BitVec.ofNat 64 size.val).toNat = size.val := by
                    rw [BitVec.toNat_ofNat]
                    exact Nat.mod_eq_of_lt (usize_val_lt_2pow64 size)
                  have hsub_le : BitVec.ofNat 64 fd0.offset.val ≤ off_sc.value.bv := by
                    rw [BitVec.le_def, hfd_off_toNat, UScalar.bv_toNat, ← hoff_val]
                    exact hfd0_le
                  have hrel_toNat :
                      (off_sc.value.bv - BitVec.ofNat 64 fd0.offset.val).toNat =
                        offset.val - fd0.offset.val := by
                    rw [BitVec.toNat_sub_of_le hsub_le, UScalar.bv_toNat, ← hoff_val, hfd_off_toNat]
                  have hbv_bytes : (DataWidth.ofCore field.width).bvBytes.toNat = wb.val := by
                    rw [hwb_val]; cases field.width <;> rfl
                  have hmax_u64 := usize_max_lt_2pow64
                  have hrel_ne_max : (off_sc.value.bv - BitVec.ofNat 64 fd0.offset.val) ≠ ~~~0#64 := by
                    intro hc
                    have hto := congrArg BitVec.toNat hc
                    have hmax64 : (~~~0#64 : BitVec 64).toNat = 2 ^ 64 - 1 := by decide
                    rw [hrel_toNat, hmax64] at hto
                    omega
                  have hno_ovf :
                      ¬ BitVec.uaddOverflow (off_sc.value.bv - BitVec.ofNat 64 fd0.offset.val)
                        (DataWidth.ofCore field.width).bvBytes := by
                    unfold BitVec.uaddOverflow
                    rw [decide_eq_true_eq, hrel_toNat, hbv_bytes]
                    omega
                  have hsum_le_sz :
                      (off_sc.value.bv - BitVec.ofNat 64 fd0.offset.val) +
                        (DataWidth.ofCore field.width).bvBytes ≤ BitVec.ofNat 64 size.val := by
                    rw [BitVec.le_def, BitVec.toNat_add, hrel_toNat, hbv_bytes, hsz_toNat,
                      Nat.mod_eq_of_lt (by omega)]
                    omega
                  unfold inBoundsRange
                  simp only [Bool.and_eq_true, bne_iff_ne, Bool.not_eq_true', decide_eq_true_eq]
                  exact ⟨⟨hrel_ne_max, Bool.eq_false_iff.mpr hno_ovf⟩, hsum_le_sz⟩
                · simp [hle_end] at hsc
              · rw [hfnone] at hsc; simp at hsc
            · rw [hanone] at hsc; simp at hsc
          have checkPtr :
              ∀ (is_32_bit : Bool),
                (do
                  let expected_width ← if is_32_bit then ok api.DataWidth.U32 else ok api.DataWidth.U64
                  let is_valid_load ←
                    if offset = fd0.offset then
                      api.DataWidth.Insts.CoreCmpPartialEqDataWidth.eq field.width expected_width
                    else ok false
                  if is_valid_load then ok (some fd0) else ok none) = ok (some fd) →
                fd0 = fd ∧
                off_sc.value.bv = BitVec.ofNat 64 fd0.offset.val ∧
                DataWidth.ofCore field.width = (if is_32_bit then .u32 else .u64) := by
            intro is32 hptr
            have hexp :
                (if is32 then (ok api.DataWidth.U32 : Result api.DataWidth) else ok api.DataWidth.U64) =
                  ok (if is32 then api.DataWidth.U32 else api.DataWidth.U64) := by
              cases is32 <;> rfl
            rw [bind_ok_eq hexp] at hptr
            by_cases hoff_eq : offset = fd0.offset
            · simp only [hoff_eq, ↓reduceIte] at hptr
              obtain ⟨r, hr, hiff⟩ :=
                DataWidth_eq_spec field.width (if is32 then api.DataWidth.U32 else api.DataWidth.U64)
              rw [bind_ok_eq hr] at hptr
              cases r
              · simp at hptr
              · simp only [↓reduceIte] at hptr
                have hfd_eq : fd0 = fd := Option.some.inj (ok_inj hptr)
                have hw_eq := hiff.mp rfl
                refine ⟨hfd_eq, ?_, ?_⟩
                · rw [← hoff_bv, hoff_eq]
                · rw [hw_eq]; cases is32 <;> rfl
            · simp [hoff_eq] at hptr
          rw [heff1, heff2]
          cases hft : fd0.field_type with
          | Scalar size =>
            rw [hft] at hfind
            dsimp only at hfind
            obtain ⟨rfl, hinb⟩ := checkScalar size hfind
            refine ⟨rfl, ?_, halign, hloop_map.symm, hfd0_mem, ?_⟩
            · rw [UScalar.bv_toNat]; exact hval_le_max
            · simpa [hft, StructFieldDesc.ofCore] using hinb
          | MutableScalar size =>
            rw [hft] at hfind
            dsimp only at hfind
            obtain ⟨rfl, hinb⟩ := checkScalar size hfind
            refine ⟨rfl, ?_, halign, hloop_map.symm, hfd0_mem, ?_⟩
            · rw [UScalar.bv_toNat]; exact hval_le_max
            · simpa [hft, StructFieldDesc.ofCore] using hinb
          | PtrToMemory is32 _ _ =>
            rw [hft] at hfind
            obtain ⟨rfl, heq_off, heq_w⟩ := checkPtr is32 hfind
            refine ⟨rfl, ?_, halign, hloop_map.symm, hfd0_mem, ?_⟩
            · rw [UScalar.bv_toNat]; exact hval_le_max
            · simpa [hft, StructFieldDesc.ofCore] using And.intro heq_off heq_w
          | NullablePtrToMemory is32 _ _ =>
            rw [hft] at hfind
            obtain ⟨rfl, heq_off, heq_w⟩ := checkPtr is32 hfind
            refine ⟨rfl, ?_, halign, hloop_map.symm, hfd0_mem, ?_⟩
            · rw [UScalar.bv_toNat]; exact hval_le_max
            · simpa [hft, StructFieldDesc.ofCore] using And.intro heq_off heq_w
          | PtrToArray is32 _ =>
            rw [hft] at hfind
            obtain ⟨rfl, heq_off, heq_w⟩ := checkPtr is32 hfind
            refine ⟨rfl, ?_, halign, hloop_map.symm, hfd0_mem, ?_⟩
            · rw [UScalar.bv_toNat]; exact hval_le_max
            · simpa [hft, StructFieldDesc.ofCore] using And.intro heq_off heq_w
          | PtrToEndArray is32 _ =>
            rw [hft] at hfind
            obtain ⟨rfl, heq_off, heq_w⟩ := checkPtr is32 hfind
            refine ⟨rfl, ?_, halign, hloop_map.symm, hfd0_mem, ?_⟩
            · rw [UScalar.bv_toNat]; exact hval_le_max
            · simpa [hft, StructFieldDesc.ofCore] using And.intro heq_off heq_w
  · simp [hkn] at hfind

/-! ## 5. `StackOffset.eq`, `verifier.Type` Helpers, & `DataDependencies` -/

theorem StackOffset_clone_spec (s : verifier.StackOffset) :
    verifier.StackOffset.Insts.CoreCloneClone.clone s = ok s := rfl

theorem StackOffset_eq_spec (a b : verifier.StackOffset) :
    ∃ r : Bool,
      verifier.StackOffset.Insts.CoreCmpPartialEqStackOffset.eq a b = ok r ∧
      (r = true ↔ a = b) :=
  u64_scalar_eq_sound a b

theorem JumpType_clone_spec (j : verifier.JumpType) :
    verifier.JumpType.Insts.CoreCloneClone.clone j = ok j := rfl

theorem JumpType_eq_spec (a b : verifier.JumpType) :
    ∃ r : Bool,
      verifier.JumpType.Insts.CoreCmpPartialEqJumpType.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  unfold verifier.JumpType.Insts.CoreCmpPartialEqJumpType.eq verifier.JumpType.tag
  cases a <;> cases b <;> simp

theorem JumpType_is_strict_spec (j : verifier.JumpType) :
    verifier.JumpType.is_strict j =
      ok (match j with
        | .Gt | .Lt | .Ne | .StrictComparaison => true
        | _ => false) := by
  cases j <;> rfl

theorem AluType_clone_spec (a : verifier.AluType) :
    verifier.AluType.Insts.CoreCloneClone.clone a = ok a := rfl

theorem Type_clone_spec (t : verifier.Type) :
    verifier.Type.Insts.CoreCloneClone.clone t = ok t := by
  induction t with
  | ScalarValue _ | ConstPtrToMap _ _ | PtrToStack _
  | MapKeyParameter _ | MapValueParameter _ | MemoryParameter _ _ _
  | ContextParameter _ | ScalarValueParameter | AnyParameter =>
    rw [verifier.Type.Insts.CoreCloneClone.clone.eq_def]
  | PtrToMemory mid offset buffer_size origin =>
    rw [verifier.Type.Insts.CoreCloneClone.clone.eq_def]
    simp [MemoryId_clone_spec]
  | PtrToStruct mid offset descriptor =>
    rw [verifier.Type.Insts.CoreCloneClone.clone.eq_def]
    simp [MemoryId_clone_spec, StructDescriptor_clone_spec]
  | PtrToArray mid offset =>
    rw [verifier.Type.Insts.CoreCloneClone.clone.eq_def]
    simp [MemoryId_clone_spec]
  | PtrToEndArray mid =>
    rw [verifier.Type.Insts.CoreCloneClone.clone.eq_def]
    simp [MemoryId_clone_spec]
  | NullOr mid inner ih =>
    rw [verifier.Type.Insts.CoreCloneClone.clone.eq_def]
    simp [MemoryId_clone_spec, ih]
  | Releasable mid inner ih =>
    rw [verifier.Type.Insts.CoreCloneClone.clone.eq_def]
    simp [MemoryId_clone_spec, ih]
  | ConstPtrToMapParameter filter must_be_writable =>
    rw [verifier.Type.Insts.CoreCloneClone.clone.eq_def]
    simp [MapTypeFilter_clone_spec]
  | NullOrParameter inner ih =>
    rw [verifier.Type.Insts.CoreCloneClone.clone.eq_def]
    simp [ih]
  | StructParameter mid =>
    rw [verifier.Type.Insts.CoreCloneClone.clone.eq_def]
    simp [MemoryId_clone_spec]
  | ReleasableParameter mid inner ih =>
    rw [verifier.Type.Insts.CoreCloneClone.clone.eq_def]
    simp [MemoryId_clone_spec, ih]
  | ReleaseParameter mid =>
    rw [verifier.Type.Insts.CoreCloneClone.clone.eq_def]
    simp [MemoryId_clone_spec]

theorem Type_eq_spec (a b : verifier.Type) :
    ∃ r : Bool,
      verifier.Type.Insts.CoreCmpPartialEqType.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  induction a generalizing b with
  | ScalarValue d1 =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i d2
    obtain ⟨r, hr, hiff⟩ := u64_scalar_eq_sound d1 d2
    exact ⟨r, hr, by simp [hiff]⟩
  | ConstPtrToMap id1 s1 =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i id2 s2
    by_cases hid : id1 = id2
    · subst hid
      simp only [↓reduceIte]
      obtain ⟨r, hr, hiff⟩ := MapSchema_eq_spec s1 s2
      exact ⟨r, hr, by simp [hiff]⟩
    · exact ⟨false, by simp [hid], by simp [hid]⟩
  | PtrToStack o1 =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i o2
    obtain ⟨r, hr, hiff⟩ := StackOffset_eq_spec o1 o2
    exact ⟨r, hr, by simp [hiff]⟩
  | PtrToMemory id1 o1 b1 org1 =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i id2 o2 b2 org2
    dsimp only
    rw [bind_ok_eq (MemoryId_eq_spec id1 id2)]
    by_cases hid : id1 = id2
    · subst hid
      simp only [decide_true, ↓reduceIte]
      obtain ⟨ro, hro, hiffo⟩ := u64_scalar_eq_sound o1 o2
      rw [bind_ok_eq hro]
      cases ro
      · have hne : o1 ≠ o2 := fun h => Bool.noConfusion (hiffo.mpr h)
        exact ⟨false, rfl, by simp [hne]⟩
      · have ho := hiffo.mp rfl; subst ho
        simp only [↓reduceIte]
        by_cases hb : b1 = b2
        · subst hb
          simp only [↓reduceIte]
          obtain ⟨rorg, hrorg, hifforg⟩ := PointerOrigin_eq_spec org1 org2
          exact ⟨rorg, hrorg, by simp [hifforg]⟩
        · exact ⟨false, by simp [hb], by simp [hb]⟩
    · exact ⟨false, by simp [hid], by simp [hid]⟩
  | PtrToStruct id1 o1 d1 =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i id2 o2 d2
    dsimp only
    rw [bind_ok_eq (MemoryId_eq_spec id1 id2)]
    by_cases hid : id1 = id2
    · subst hid
      simp only [decide_true, ↓reduceIte]
      obtain ⟨ro, hro, hiffo⟩ := u64_scalar_eq_sound o1 o2
      rw [bind_ok_eq hro]
      cases ro
      · have hne : o1 ≠ o2 := fun h => Bool.noConfusion (hiffo.mpr h)
        exact ⟨false, rfl, by simp [hne]⟩
      · have ho := hiffo.mp rfl; subst ho
        simp only [↓reduceIte]
        obtain ⟨rd, hrd, hiffd⟩ := StructDescriptor_eq_spec d1 d2
        exact ⟨rd, hrd, by simp [hiffd]⟩
    · exact ⟨false, by simp [hid], by simp [hid]⟩
  | PtrToArray id1 o1 =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i id2 o2
    dsimp only
    rw [bind_ok_eq (MemoryId_eq_spec id1 id2)]
    by_cases hid : id1 = id2
    · subst hid
      simp only [decide_true, ↓reduceIte]
      obtain ⟨ro, hro, hiffo⟩ := u64_scalar_eq_sound o1 o2
      exact ⟨ro, hro, by simp [hiffo]⟩
    · exact ⟨false, by simp [hid], by simp [hid]⟩
  | PtrToEndArray id1 =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i id2
    exact ⟨decide (id1 = id2), MemoryId_eq_spec id1 id2, by simp⟩
  | NullOr id1 i1 ih =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i id2 i2
    dsimp only
    rw [bind_ok_eq (MemoryId_eq_spec id1 id2)]
    by_cases hid : id1 = id2
    · subst hid
      simp only [decide_true, ↓reduceIte]
      obtain ⟨ri, hri, hiffi⟩ := ih i2
      exact ⟨ri, hri, by simp [hiffi]⟩
    · exact ⟨false, by simp [hid], by simp [hid]⟩
  | Releasable id1 i1 ih =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i id2 i2
    dsimp only
    rw [bind_ok_eq (MemoryId_eq_spec id1 id2)]
    by_cases hid : id1 = id2
    · subst hid
      simp only [decide_true, ↓reduceIte]
      obtain ⟨ri, hri, hiffi⟩ := ih i2
      exact ⟨ri, hri, by simp [hiffi]⟩
    · exact ⟨false, by simp [hid], by simp [hid]⟩
  | ScalarValueParameter =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    exact ⟨true, rfl, by simp⟩
  | ConstPtrToMapParameter f1 w1 =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i f2 w2
    dsimp only
    obtain ⟨rf, hrf, hifff⟩ := MapTypeFilter_eq_spec f1 f2
    rw [bind_ok_eq hrf]
    cases rf
    · have hne : f1 ≠ f2 := fun h => Bool.noConfusion (hifff.mpr h)
      exact ⟨false, rfl, by simp [hne]⟩
    · have hf := hifff.mp rfl; subst hf
      simp only [↓reduceIte]
      exact ⟨decide (w1 = w2), rfl, by simp⟩
  | MapKeyParameter i1 =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i i2
    exact ⟨decide (i1 = i2), rfl, by simp⟩
  | MapValueParameter i1 =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i i2
    exact ⟨decide (i1 = i2), rfl, by simp⟩
  | MemoryParameter s1 in1 out1 =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i s2 in2 out2
    dsimp only
    obtain ⟨rs, hrs, hiffs⟩ := MemoryParameterSize_eq_spec s1 s2
    rw [bind_ok_eq hrs]
    cases rs
    · have hne : s1 ≠ s2 := fun h => Bool.noConfusion (hiffs.mpr h)
      exact ⟨false, rfl, by simp [hne]⟩
    · have hs := hiffs.mp rfl; subst hs
      simp only [↓reduceIte]
      by_cases hin : in1 = in2
      · subst hin; simp only [↓reduceIte]
        exact ⟨decide (out1 = out2), rfl, by simp⟩
      · exact ⟨false, by simp [hin], by simp [hin]⟩
  | NullOrParameter i1 ih =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i i2
    obtain ⟨ri, hri, hiffi⟩ := ih i2
    exact ⟨ri, hri, by simp [hiffi]⟩
  | StructParameter id1 =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i id2
    exact ⟨decide (id1 = id2), MemoryId_eq_spec id1 id2, by simp⟩
  | ContextParameter i1 =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i i2
    exact ⟨decide (i1 = i2), rfl, by simp⟩
  | ReleasableParameter id1 i1 ih =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i id2 i2
    dsimp only
    rw [bind_ok_eq (MemoryId_eq_spec id1 id2)]
    by_cases hid : id1 = id2
    · subst hid
      simp only [decide_true, ↓reduceIte]
      obtain ⟨ri, hri, hiffi⟩ := ih i2
      exact ⟨ri, hri, by simp [hiffi]⟩
    · exact ⟨false, by simp [hid], by simp [hid]⟩
  | ReleaseParameter id1 =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    rename_i id2
    exact ⟨decide (id1 = id2), MemoryId_eq_spec id1 id2, by simp⟩
  | AnyParameter =>
    rw [verifier.Type.Insts.CoreCmpPartialEqType.eq.eq_def]
    cases b <;> try exact ⟨false, rfl, by simp⟩
    exact ⟨true, rfl, by simp⟩

theorem Type_ne_spec (a b : verifier.Type) :
    ∃ r : Bool,
      core.cmp.PartialEq.ne.trait_default
        verifier.Type.Insts.CoreCmpPartialEqType a b = ok r ∧
      (r = true ↔ a ≠ b) := by
  obtain ⟨r, hr, hiff⟩ := Type_eq_spec a b
  exact PartialEq_ne_of_eq verifier.Type.Insts.CoreCmpPartialEqType a b hr hiff

theorem Type_from_scalar_spec (d : U64ScalarValueData) :
    verifier.Type.Insts.CoreConvertFromU64ScalarValueData.from d = ok (.ScalarValue d) := rfl

theorem Type_from_u64_spec (v : Std.U64) :
    verifier.Type.Insts.CoreConvertFromU64.from v = ok (.ScalarValue (knownScalar64 v)) := by
  unfold verifier.Type.Insts.CoreConvertFromU64.from
  rw [bind_ok_eq (from_known_64_eq v)]

theorem Type_default_spec :
    verifier.Type.Insts.CoreDefaultDefault.default = ok (.ScalarValue uninitScalar64) :=
  Type_UNINITIALIZED_eq

theorem uninitScalar64_unwritten_ne_zero : uninitScalar64.unwritten_mask ≠ 0#u64 := by
  intro hc
  have hbv := congrArg UScalar.bv hc
  simp only [uninitScalar64, u64_max_bv, u64_zero_bv] at hbv
  revert hbv; decide

theorem knownScalar64_ne_uninit (v : Std.U64) : knownScalar64 v ≠ uninitScalar64 := by
  intro hc
  have h := congrArg U64ScalarValueData.unwritten_mask hc
  exact uninitScalar64_unwritten_ne_zero h.symm

theorem unknownWrittenScalar64_ne_uninit : unknownWrittenScalar64 ≠ uninitScalar64 := by
  intro hc
  have h := congrArg U64ScalarValueData.unwritten_mask hc
  exact uninitScalar64_unwritten_ne_zero h.symm

theorem Type_written_scalar_spec (t : verifier.Type) :
    verifier.Type.written_scalar t =
      ok (match t with
        | .ScalarValue d => if d.unwritten_mask = 0#u64 then some d else none
        | _ => none) := by
  cases t <;> unfold verifier.Type.written_scalar U64ScalarValueData.is_fully_initialized <;> try rfl
  rename_i d
  by_cases h : d.unwritten_mask = 0#u64 <;> simp [h]

theorem Type_written_scalar_some_rel2
    {env : ExecEnv} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64}
    {t : verifier.Type} {d : U64ScalarValueData} {v₁ v₂ : RuntimeVal}
    (hw : verifier.Type.written_scalar t = ok (some d))
    (hrel : TypeRel2 env ρ resActive isNullId arrayLen t v₁ v₂) :
    t = .ScalarValue d ∧
    d.unwritten_mask = 0#u64 ∧
    ∃ x : BitVec 64,
      v₁ = .scalar x ∧ v₂ = .scalar x ∧ ConcretizesScalar64Rel2 d x x := by
  rw [Type_written_scalar_spec] at hw
  have hopt := ok_inj hw
  cases t <;> simp at hopt
  rename_i d0
  rcases hopt with ⟨hunw, rfl⟩
  have hne : d0 ≠ uninitScalar64 := by
    intro hc; subst hc; exact uninitScalar64_unwritten_ne_zero hunw
  simp only [TypeRel2, hne, ↓reduceIte] at hrel
  rcases hrel with ⟨x₁, x₂, rfl, rfl, hconc⟩
  have heq : x₁ = x₂ := fully_initialized_eq_of_rel2_64 hunw hconc
  subst heq
  exact ⟨rfl, hunw, x₁, rfl, rfl, hconc⟩

theorem Type_is_written_scalar_spec (t : verifier.Type) :
    verifier.Type.is_written_scalar t =
      ok (match t with
        | .ScalarValue d => decide (d.unwritten_mask = 0#u64)
        | _ => false) := by
  unfold verifier.Type.is_written_scalar
  rw [bind_ok_eq (Type_written_scalar_spec t)]
  cases t <;> simp [core.option.Option.is_some]
  rename_i d
  by_cases h : d.unwritten_mask = 0#u64 <;> simp [h]

theorem Type_is_initialized_spec (t : verifier.Type) :
    verifier.Type.is_initialized t =
      ok (match t with
        | .ScalarValue d => decide (d.unwritten_mask = 0#u64)
        | _ => true) := by
  cases t <;> unfold verifier.Type.is_initialized U64ScalarValueData.is_fully_initialized <;> rfl

theorem Type_is_initialized_toBits_eq
    {env : ExecEnv} {vCounter : Nat} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64}
    {layout₁ layout₂ : HostLayout}
    {t : verifier.Type} {v₁ v₂ : RuntimeVal}
    (ht : TypeValid env vCounter t)
    (hinit : verifier.Type.is_initialized t = ok true)
    (hrel : TypeRel2 env ρ resActive isNullId arrayLen t v₁ v₂)
    (hl : layout₁ = layout₂) :
    v₁.toBits layout₁ arrayLen = v₂.toBits layout₂ arrayLen := by
  subst hl
  rw [Type_is_initialized_spec] at hinit
  induction t with
  | ScalarValue d =>
    have hunw : d.unwritten_mask = 0#u64 := of_decide_eq_true (ok_inj hinit)
    have hne : d ≠ uninitScalar64 := by
      intro hc; subst hc; exact uninitScalar64_unwritten_ne_zero hunw
    simp only [TypeRel2, hne, ↓reduceIte] at hrel
    rcases hrel with ⟨x₁, x₂, rfl, rfl, hconc⟩
    rw [fully_initialized_eq_of_rel2_64 hunw hconc]
  | ConstPtrToMap _ _ =>
    rcases hrel with ⟨_, rfl, rfl⟩; rfl
  | PtrToStack _ =>
    rcases hrel with ⟨_, _, _, rfl, rfl⟩; rfl
  | PtrToMemory _ _ _ _ =>
    rcases hrel with ⟨_, _, _, rfl, rfl⟩; rfl
  | PtrToStruct _ _ _ =>
    rcases hrel with ⟨_, _, _, _, rfl, rfl⟩; rfl
  | PtrToArray _ _ =>
    rcases hrel with ⟨_, _, _, _, rfl, rfl⟩; rfl
  | PtrToEndArray _ =>
    rcases hrel with ⟨_, rfl, rfl⟩; rfl
  | Releasable relId inner =>
    cases inner with
    | PtrToMemory _ _ _ _ => rcases hrel with ⟨_, _, _, _, rfl, rfl⟩; rfl
    | PtrToStruct _ _ _ => rcases hrel with ⟨_, _, _, _, _, rfl, rfl⟩; rfl
    | _ => exact False.elim hrel
  | NullOr nullId inner ih =>
    rcases ht with ⟨_, hshape, hinnerV⟩
    unfold TypeRel2 at hrel
    by_cases hn : isNullId (renameMemId ρ nullId)
    · simp only [hn, ↓reduceIte] at hrel
      rcases hrel with ⟨_, rfl, rfl⟩; rfl
    · simp only [hn, Bool.false_eq_true, ↓reduceIte] at hrel
      cases inner <;> try exact False.elim hshape
      all_goals exact ih hinnerV rfl hrel
  | _ => exact False.elim hrel

theorem Type_is_non_zero_spec (t : verifier.Type) :
    ∃ b : Bool, verifier.Type.is_non_zero t = ok b := by
  induction t with
  | ScalarValue d =>
    rw [verifier.Type.is_non_zero.eq_def]
    unfold U64ScalarValueData.min
    simp only [bind_ok]
    exact ⟨decide (d.urange.min > 0#u64), rfl⟩
  | PtrToStack offset =>
    rw [verifier.Type.is_non_zero.eq_def]
    unfold verifier.StackOffset.reg U64ScalarValueData.is_fully_initialized U64ScalarValueData.max
    simp only [bind_ok, lift_eq_ok]
    by_cases hb : offset.unwritten_mask = 0#u64 <;> simp [hb]
  | PtrToMemory _ offset buffer_size _ =>
    rw [verifier.Type.is_non_zero.eq_def]
    unfold U64ScalarValueData.is_fully_initialized U64ScalarValueData.max
    simp only [bind_ok]
    by_cases hb : offset.unwritten_mask = 0#u64 <;> simp [hb]
  | PtrToStruct _ offset _ =>
    rw [verifier.Type.is_non_zero.eq_def]
    exact ⟨_, is_zero_64_eq_ok offset⟩
  | Releasable _ inner ih =>
    rw [verifier.Type.is_non_zero.eq_def]
    exact ih
  | _ =>
    rw [verifier.Type.is_non_zero.eq_def]
    exact ⟨false, rfl⟩

theorem Type_is_non_zero_sound
    {env : ExecEnv} {vCounter : Nat} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64}
    {layout : HostLayout}
    (hlayout : ValidHostLayout layout)
    {t : verifier.Type} {v₁ v₂ : RuntimeVal}
    (ht : TypeValid env vCounter t)
    (hnz : verifier.Type.is_non_zero t = ok true)
    (hrel : TypeRel2 env ρ resActive isNullId arrayLen t v₁ v₂) :
    v₁.toBits layout arrayLen ≠ 0#64 ∧ v₂.toBits layout arrayLen ≠ 0#64 := by
  cases t with
  | ScalarValue d =>
    rw [verifier.Type.is_non_zero.eq_def] at hnz
    unfold U64ScalarValueData.min at hnz
    simp only [bind_ok] at hnz
    have hgt : d.urange.min > 0#u64 := of_decide_eq_true (ok_inj hnz)
    have hgt_bv : 0#64 < d.urange.min.bv := (u64_gt_iff_bv d.urange.min 0#u64).mp hgt
    have hne : d ≠ uninitScalar64 := by
      intro hc; subst hc
      simp [uninitScalar64] at hgt_bv
    simp only [TypeRel2, hne, ↓reduceIte] at hrel
    rcases hrel with ⟨x₁, x₂, rfl, rfl, hconc⟩
    have hx1_ge : d.urange.min.bv ≤ x₁ := hconc.1.2.2.1
    have hx2_ge : d.urange.min.bv ≤ x₂ := hconc.2.1.2.2.1
    rw [BitVec.lt_def] at hgt_bv
    rw [BitVec.le_def] at hx1_ge hx2_ge
    simp only [RuntimeVal.toBits]
    constructor
    · intro h0; subst h0; simp at hgt_bv hx1_ge; omega
    · intro h0; subst h0; simp at hgt_bv hx2_ge; omega
  | PtrToStack _ | PtrToMemory _ _ _ _ | PtrToStruct _ _ _ | Releasable _ _ =>
    obtain ⟨heq, _, h1⟩ := TypeRel2_ptr_nonscalar_and_nonzero hlayout ht hrel trivial
    exact ⟨h1, heq ▸ h1⟩
  | ConstPtrToMap _ _ | PtrToArray _ _ | PtrToEndArray _ | NullOr _ _
  | ScalarValueParameter | ConstPtrToMapParameter _ _ | MapKeyParameter _
  | MapValueParameter _ | MemoryParameter _ _ _ | NullOrParameter _
  | StructParameter _ | ContextParameter _ | ReleasableParameter _ _
  | ReleaseParameter _ | AnyParameter =>
    rw [verifier.Type.is_non_zero.eq_def] at hnz
    simp at hnz

def typeReleasableId? : verifier.Type → Option MemoryId
  | .Releasable mid _ => some mid
  | .NullOr _ inner => typeReleasableId? inner
  | _ => none

theorem Type_check_not_released_spec
    {vCounter : Nat} {resources : verifier.IdSet}
    (hres : IdSetValid vCounter resources) (t : verifier.Type) :
    ∃ r : core.result.Result Unit error.VerifierErrorKind,
      verifier.Type.check_not_released t resources = ok r ∧
      (∀ u, r = .Ok u ↔
        match typeReleasableId? t with
        | some mid => mid ∈ resources.entries.val
        | none => True) := by
  induction t with
  | Releasable mid _ =>
    rw [verifier.Type.check_not_released.eq_def]
    dsimp only
    rw [bind_ok_eq (IdSet_contains_spec hres mid)]
    by_cases hm : mid ∈ resources.entries.val
    · simp only [hm, decide_true, ↓reduceIte]
      exact ⟨.Ok (), rfl, fun u => by cases u; simp [typeReleasableId?, hm]⟩
    · simp only [hm, decide_false, Bool.false_eq_true, ↓reduceIte]
      exact ⟨.Err .UseAfterRelease, rfl, fun u => by simp [typeReleasableId?, hm]⟩
  | NullOr nullId inner ih =>
    rw [verifier.Type.check_not_released.eq_def]
    simpa [typeReleasableId?] using ih
  | _ =>
    rw [verifier.Type.check_not_released.eq_def]
    exact ⟨.Ok (), rfl, fun u => by cases u; simp [typeReleasableId?]⟩

theorem Type_check_not_released_checkAlive_none
    {env : ExecEnv} {vCounter : Nat} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64}
    {resources : verifier.IdSet} {liveResources : List MemoryId}
    {t : verifier.Type} {v₁ v₂ : RuntimeVal}
    (hresV : IdSetValid vCounter resources)
    (hlive : ∀ m : MemoryId,
      m ∈ liveResources ↔
        ∃ r ∈ resources.entries.val,
          resActive (renameMemId ρ r) = true ∧ renameMemId ρ r = m)
    (ht : TypeValid env vCounter t)
    (hne : t ≠ .ScalarValue uninitScalar64)
    (hchk : verifier.Type.check_not_released t resources = ok (.Ok ()))
    (hrel : TypeRel2 env ρ resActive isNullId arrayLen t v₁ v₂) :
    checkAlive liveResources v₁ = none ∧ checkAlive liveResources v₂ = none := by
  induction t with
  | ScalarValue d =>
    have hd : d ≠ uninitScalar64 := fun hc => hne (hc ▸ rfl)
    simp only [TypeRel2, hd, ↓reduceIte] at hrel
    rcases hrel with ⟨_, _, rfl, rfl, _⟩
    exact ⟨rfl, rfl⟩
  | ConstPtrToMap _ _ =>
    rcases hrel with ⟨_, rfl, rfl⟩
    exact ⟨rfl, rfl⟩
  | PtrToStack _ =>
    rcases hrel with ⟨_, _, _, rfl, rfl⟩
    exact ⟨rfl, rfl⟩
  | PtrToMemory _ _ _ _ =>
    rcases hrel with ⟨_, _, _, rfl, rfl⟩
    exact ⟨rfl, rfl⟩
  | PtrToStruct _ _ _ =>
    rcases hrel with ⟨_, _, _, _, rfl, rfl⟩
    exact ⟨rfl, rfl⟩
  | PtrToArray _ _ =>
    rcases hrel with ⟨_, _, _, _, rfl, rfl⟩
    exact ⟨rfl, rfl⟩
  | PtrToEndArray _ =>
    rcases hrel with ⟨_, rfl, rfl⟩
    exact ⟨rfl, rfl⟩
  | Releasable relId inner =>
    obtain ⟨r, hr, hiff⟩ := Type_check_not_released_spec hresV (.Releasable relId inner)
    rw [hchk] at hr
    have hmem : relId ∈ resources.entries.val := by
      simpa [typeReleasableId?] using (hiff ()).mp (ok_inj hr).symm
    cases inner with
    | PtrToMemory _ _ _ _ =>
      rcases hrel with ⟨hact, _, _, _, rfl, rfl⟩
      have hin : renameMemId ρ relId ∈ liveResources :=
        (hlive _).mpr ⟨relId, hmem, hact, rfl⟩
      simp [checkAlive, RuntimeVal.resourceId?, hin]
    | PtrToStruct _ _ _ =>
      rcases hrel with ⟨hact, _, _, _, _, rfl, rfl⟩
      have hin : renameMemId ρ relId ∈ liveResources :=
        (hlive _).mpr ⟨relId, hmem, hact, rfl⟩
      simp [checkAlive, RuntimeVal.resourceId?, hin]
    | _ => exact False.elim hrel
  | NullOr nullId inner ih =>
    rcases ht with ⟨_, hshape, hinnerV⟩
    rw [verifier.Type.check_not_released.eq_def] at hchk
    unfold TypeRel2 at hrel
    by_cases hn : isNullId (renameMemId ρ nullId)
    · simp only [hn, ↓reduceIte] at hrel
      rcases hrel with ⟨_, rfl, rfl⟩
      exact ⟨rfl, rfl⟩
    · simp only [hn, Bool.false_eq_true, ↓reduceIte] at hrel
      have hne_in : inner ≠ .ScalarValue uninitScalar64 := by
        intro hc; subst hc; exact hshape
      exact ih hinnerV hne_in hchk hrel
  | _ => exact False.elim hrel

def pureUnwrapReleasable : verifier.Type → verifier.Type
  | .Releasable _ inner => inner
  | t => t

theorem Type_unwrap_releasable_spec (t : verifier.Type) :
    verifier.Type.unwrap_releasable t = ok (pureUnwrapReleasable t) := by
  cases t <;> rfl

theorem Type_unwrap_releasable_valid {env : ExecEnv} {vCounter : Nat} {t : verifier.Type}
    (ht : TypeValid env vCounter t) :
    TypeValid env vCounter (pureUnwrapReleasable t) := by
  cases t <;> try exact ht
  rename_i relId inner
  cases inner <;> try exact False.elim ht
  · exact ht.2
  · exact ht.2

theorem Type_unwrap_releasable_rel2
    {env : ExecEnv} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64}
    {t : verifier.Type} {v₁ v₂ : RuntimeVal}
    (hrel : TypeRel2 env ρ resActive isNullId arrayLen t v₁ v₂) :
    let w₁ := match t with | .Releasable _ _ => v₁.stripResource | _ => v₁
    let w₂ := match t with | .Releasable _ _ => v₂.stripResource | _ => v₂
    TypeRel2 env ρ resActive isNullId arrayLen (pureUnwrapReleasable t) w₁ w₂ := by
  dsimp only
  cases t <;> try exact hrel
  rename_i relId inner
  cases inner with
  | PtrToMemory mid off sz orig =>
    rcases hrel with ⟨_, hp, o, hconc, rfl, rfl⟩
    exact ⟨hp, o, hconc, rfl, rfl⟩
  | PtrToStruct mid off sd =>
    rcases hrel with ⟨_, hp, hsd, o, hconc, rfl, rfl⟩
    exact ⟨hp, hsd, o, hconc, rfl, rfl⟩
  | _ => exact False.elim hrel

theorem Type_inner_spec
    {vCounter : Nat} {ctx : verifier.ComputationContext}
    (hresV : IdSetValid vCounter ctx.resources) (t : verifier.Type) :
    ∃ r : core.result.Result verifier.Type error.VerifierErrorKind,
      verifier.Type.inner t ctx = ok r ∧
      (∀ u, r = .Ok u ↔
        u = pureUnwrapReleasable t ∧
        (match typeReleasableId? t with
         | some mid => mid ∈ ctx.resources.entries.val
         | none => True)) := by
  unfold verifier.Type.inner
  obtain ⟨rc, hrc, hiff⟩ := Type_check_not_released_spec hresV t
  rw [bind_ok_eq hrc]
  cases rc with
  | Ok u0 =>
    cases u0
    dsimp only
    rw [bind_ok_eq (Type_unwrap_releasable_spec t)]
    have hm := (hiff ()).mp rfl
    refine ⟨.Ok (pureUnwrapReleasable t), rfl, fun u => ?_⟩
    simp [hm, eq_comm]
  | Err e =>
    dsimp only
    have hnm : ¬ (match typeReleasableId? t with
        | some mid => mid ∈ ctx.resources.entries.val
        | none => True) := by
      intro hm
      have hok := (hiff ()).mpr hm
      cases hok
    refine ⟨.Err e, rfl, fun u => ?_⟩
    simp [hnm]

def pureTypeWithOffset : verifier.Type → U64ScalarValueData → verifier.Type
  | .PtrToStack _, off => .PtrToStack off
  | .PtrToMemory mid _ sz orig, off => .PtrToMemory mid off sz orig
  | .PtrToStruct mid _ sd, off => .PtrToStruct mid off sd
  | .PtrToArray mid _, off => .PtrToArray mid off
  | .Releasable mid inner, off => .Releasable mid (pureTypeWithOffset inner off)
  | t, _ => t

theorem Type_with_offset_spec (t : verifier.Type) (offset : U64ScalarValueData) :
    verifier.Type.with_offset t offset = ok (pureTypeWithOffset t offset) := by
  induction t with
  | PtrToStack _ =>
    rw [verifier.Type.with_offset.eq_def]; rfl
  | PtrToMemory mid _ sz orig =>
    rw [verifier.Type.with_offset.eq_def]
    simp [MemoryId_clone_spec, pureTypeWithOffset]
  | PtrToStruct mid _ sd =>
    rw [verifier.Type.with_offset.eq_def]
    simp [MemoryId_clone_spec, StructDescriptor_clone_spec, pureTypeWithOffset]
  | PtrToArray mid _ =>
    rw [verifier.Type.with_offset.eq_def]
    simp [MemoryId_clone_spec, pureTypeWithOffset]
  | Releasable mid inner ih =>
    rw [verifier.Type.with_offset.eq_def]
    simp [MemoryId_clone_spec, ih, pureTypeWithOffset]
  | _ =>
    rw [verifier.Type.with_offset.eq_def]
    simpa [pureTypeWithOffset] using Type_clone_spec _

def pureTypeSetNull (t : verifier.Type) (null_id : MemoryId) (is_null : Bool) : verifier.Type :=
  match t with
  | .NullOr mid inner =>
      if mid = null_id then
        if is_null then .ScalarValue (knownScalar64 0#u64) else inner
      else
        .NullOr mid inner
  | _ => t

theorem Type_set_null_spec (t : verifier.Type) (null_id : MemoryId) (is_null : Bool) :
    verifier.Type.set_null t null_id is_null = ok (pureTypeSetNull t null_id is_null) := by
  cases t with
  | NullOr mid inner =>
    unfold verifier.Type.set_null pureTypeSetNull
    dsimp only
    rw [bind_ok_eq (MemoryId_eq_spec mid null_id)]
    by_cases heq : mid = null_id
    · subst heq
      simp only [decide_true, ↓reduceIte]
      cases is_null
      · simp [Type_clone_spec]
      · simp [from_known_64_eq]
    · simp [heq]
  | _ => simp [verifier.Type.set_null, pureTypeSetNull]

theorem Type_set_null_valid {env : ExecEnv} {vCounter : Nat}
    {t : verifier.Type} {null_id : MemoryId} {is_null : Bool}
    (ht : TypeValid env vCounter t) :
    TypeValid env vCounter (pureTypeSetNull t null_id is_null) := by
  cases t <;> try exact ht
  rename_i mid inner
  unfold pureTypeSetNull
  by_cases heq : mid = null_id
  · simp only [heq, ↓reduceIte]
    cases is_null
    · exact ht.2.2
    · exact knownScalar64_valid 0#u64
  · simp only [heq, ↓reduceIte]
    exact ht

theorem Type_set_null_rel2
    {env : ExecEnv} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64}
    {t : verifier.Type} {null_id : MemoryId} {is_null : Bool}
    {v₁ v₂ : RuntimeVal}
    (hnullEq : isNullId (renameMemId ρ null_id) = is_null)
    (hrel : TypeRel2 env ρ resActive isNullId arrayLen t v₁ v₂) :
    TypeRel2 env ρ resActive isNullId arrayLen (pureTypeSetNull t null_id is_null) v₁ v₂ := by
  cases t <;> try exact hrel
  rename_i mid inner
  unfold pureTypeSetNull
  by_cases heq : mid = null_id
  · subst heq
    simp only [↓reduceIte]
    unfold TypeRel2 at hrel
    rw [hnullEq] at hrel
    cases is_null
    · simp only [Bool.false_eq_true, ↓reduceIte] at hrel
      exact hrel
    · simp only [↓reduceIte] at hrel
      rcases hrel with ⟨_, rfl, rfl⟩
      simp only [TypeRel2, knownScalar64_ne_uninit, ↓reduceIte]
      exact ⟨0#64, 0#64, rfl, rfl, concretizesScalar64Rel2_known 0#u64⟩
  · simp only [heq, ↓reduceIte]
    exact hrel

theorem Type_set_null_stack_rel2
    {env : ExecEnv} {ρ : Std.U64 → Std.U64}
    {resActive isNullId : MemoryId → Bool}
    {arrayLen : MemoryId → BitVec 64}
    {t : verifier.Type} {null_id : MemoryId} {is_null : Bool}
    {s₁ s₂ : StackSlot}
    (hnullEq : isNullId (renameMemId ρ null_id) = is_null)
    (hrel : StackSlotRel2 env ρ resActive isNullId arrayLen t s₁ s₂) :
    StackSlotRel2 env ρ resActive isNullId arrayLen (pureTypeSetNull t null_id is_null) s₁ s₂ := by
  cases t <;> try exact hrel
  rename_i mid inner
  unfold pureTypeSetNull
  by_cases heq : mid = null_id
  · subst heq
    simp only [↓reduceIte]
    unfold StackSlotRel2 at hrel
    rw [hnullEq] at hrel
    cases is_null
    · simp only [Bool.false_eq_true, ↓reduceIte] at hrel
      exact hrel
    · simp only [↓reduceIte] at hrel
      rcases hrel with ⟨_, rfl, rfl⟩
      simp only [StackSlotRel2, knownScalar64_ne_uninit, ↓reduceIte]
      have hz1 : (0#64 &&& ~~~0#64) = 0#64 := by decide
      have hz2 : (0#64 ||| ~~~(~~~0#64)) = 0#64 := by decide
      refine ⟨0#64, ~~~0#64, 0#64, ~~~0#64, rfl, rfl,
        concretizesScalar64Rel2_known 0#u64, ?_, ?_⟩ <;>
        (rw [hz1, hz2]; exact concretizesScalar64Rel2_known 0#u64)
  · simp only [heq, ↓reduceIte]
    exact hrel

theorem Type_unregister_resource_spec
    {vCounter : Nat} {ctx : verifier.ComputationContext}
    (hresV : IdSetValid vCounter ctx.resources) (t : verifier.Type) :
    ∃ ctx' : verifier.ComputationContext,
      verifier.Type.unregister_resource t ctx = ok ctx' ∧
      ctx'.pc = ctx.pc ∧
      ctx'.registers = ctx.registers ∧
      ctx'.stack = ctx.stack ∧
      ctx'.array_bounds = ctx.array_bounds ∧
      IdSetValid vCounter ctx'.resources ∧
      match t with
      | .Releasable mid _ => ∀ x, x ∈ ctx'.resources.entries.val ↔ x ∈ ctx.resources.entries.val ∧ x ≠ mid
      | _ => ctx'.resources = ctx.resources := by
  unfold verifier.Type.unregister_resource
  cases t <;> try exact ⟨ctx, rfl, rfl, rfl, rfl, rfl, hresV, rfl⟩
  rename_i mid inner
  dsimp only
  obtain ⟨_, is', hrem, hvalid', hmem, _⟩ := IdSet_remove_spec hresV mid
  rw [bind_pair_ok_eq hrem]
  exact ⟨{ ctx with resources := is' }, rfl, rfl, rfl, rfl, rfl, hvalid', hmem⟩

/-! ### `DataDependencies` Specifications -/

private theorem u16_eq_iff_bv (a b : Std.U16) : (a = b) ↔ (a.bv = b.bv) := by
  constructor
  · intro h; rw [h]
  · intro h; cases a; cases b; simp_all

private theorem bv_shift_one_getLsbD {w n i : Nat} (hn : n < w) (hi : i < w) :
    ((1#w) <<< n).getLsbD i = decide (i = n) := by
  have hw : 0 < w := by omega
  rw [BitVec.getLsbD_shiftLeft, BitVec.getLsbD_one]
  by_cases hlt : i < n
  · have hne : i ≠ n := by omega
    simp [hi, hlt, hne]
  · by_cases heq : i = n
    · subst heq
      simp [hi, hw]
    · have hsub_ne : i - n ≠ 0 := by omega
      simp [hi, hlt, hw, hsub_ne, heq]

private theorem bv_and_shift_one_bne_zero {w n : Nat} (x : BitVec w) (hn : n < w) :
    ((x &&& ((1#w) <<< n)) != 0#w) = x.getLsbD n := by
  by_cases hx : x.getLsbD n = true
  · rw [hx, bne_iff_ne]
    intro hc
    have h := congrArg (fun b : BitVec w => b.getLsbD n) hc
    rw [BitVec.getLsbD_and, bv_shift_one_getLsbD hn hn, BitVec.getLsbD_zero] at h
    simp [hx] at h
  · have hxf : x.getLsbD n = false := Bool.eq_false_iff.mpr hx
    rw [hxf]
    have heq0 : (x &&& ((1#w) <<< n)) = 0#w := by
      apply BitVec.eq_of_getLsbD_eq
      intro i hi
      rw [BitVec.getLsbD_and, bv_shift_one_getLsbD hn hi, BitVec.getLsbD_zero]
      by_cases hin : i = n
      · subst hin; simp [hxf]
      · simp [hin]
    simp [heq0]

private theorem u16_shl_one_eq_ok (reg : Std.U8) (h : reg.val < 16) :
    (1#u16 <<< reg) = ok ⟨(1#16) <<< reg.val⟩ := by
  change UScalar.shiftLeft_UScalar 1#u16 reg = _
  simp only [UScalar.shiftLeft_UScalar, UScalar.shiftLeft, UScalarTy.numBits, h, ↓reduceIte]
  rfl

private theorem u16_shl_one_ok_inv {reg : Std.U8} {m : Std.U16}
    (h : (1#u16 <<< reg) = ok m) :
    reg.val < 16 ∧ m = ⟨(1#16) <<< reg.val⟩ := by
  change UScalar.shiftLeft_UScalar 1#u16 reg = ok m at h
  unfold UScalar.shiftLeft_UScalar UScalar.shiftLeft at h
  simp only [UScalarTy.numBits] at h
  by_cases hlt : reg.val < 16
  · simp only [hlt, ↓reduceIte] at h
    exact ⟨hlt, (ok_inj h).symm⟩
  · simp [hlt] at h

private theorem u64_shl_one_eq_ok (s : Std.Usize) (h : s.val < 64) :
    (1#u64 <<< s) = ok ⟨(1#64) <<< s.val⟩ := by
  change UScalar.shiftLeft_UScalar 1#u64 s = _
  simp only [UScalar.shiftLeft_UScalar, UScalar.shiftLeft, UScalarTy.numBits, h, ↓reduceIte]
  rfl

private theorem u64_shl_one_ok_inv {s : Std.Usize} {m : Std.U64}
    (h : (1#u64 <<< s) = ok m) :
    s.val < 64 ∧ m = ⟨(1#64) <<< s.val⟩ := by
  change UScalar.shiftLeft_UScalar 1#u64 s = ok m at h
  unfold UScalar.shiftLeft_UScalar UScalar.shiftLeft at h
  simp only [UScalarTy.numBits] at h
  by_cases hlt : s.val < 64
  · simp only [hlt, ↓reduceIte] at h
    exact ⟨hlt, (ok_inj h).symm⟩
  · simp [hlt] at h

private theorem u64_shl_usize_eq_ok (x : Std.U64) (s : Std.Usize) (h : s.val < 64) :
    (x <<< s) = ok ⟨x.bv <<< s.val⟩ := by
  change UScalar.shiftLeft_UScalar x s = _
  simp only [UScalar.shiftLeft_UScalar, UScalar.shiftLeft, UScalarTy.numBits, h, ↓reduceIte]
  rfl

private theorem u64_shl_usize_ok_inv {x : Std.U64} {s : Std.Usize} {m : Std.U64}
    (h : (x <<< s) = ok m) :
    s.val < 64 ∧ m = ⟨x.bv <<< s.val⟩ := by
  change UScalar.shiftLeft_UScalar x s = ok m at h
  unfold UScalar.shiftLeft_UScalar UScalar.shiftLeft at h
  simp only [UScalarTy.numBits] at h
  by_cases hlt : s.val < 64
  · simp only [hlt, ↓reduceIte] at h
    exact ⟨hlt, (ok_inj h).symm⟩
  · simp [hlt] at h

theorem DataDependencies_default_spec :
    ∃ d : verifier.DataDependencies,
      verifier.DataDependencies.Insts.CoreDefaultDefault.default = ok d ∧
      d.registers = 0#u16 ∧ d.stack = 0#u64 ∧
      (∀ r, DataDepsHasReg d r = false) ∧
      (∀ s, DataDepsHasStack d s = false) := by
  refine ⟨{ registers := 0#u16, stack := 0#u64 }, rfl, rfl, rfl, ?_, ?_⟩
  · intro r; unfold DataDepsHasReg; exact BitVec.getLsbD_zero
  · intro s; unfold DataDepsHasStack; exact BitVec.getLsbD_zero

theorem DataDependencies_clone_spec (d : verifier.DataDependencies) :
    verifier.DataDependencies.Insts.CoreCloneClone.clone d = ok d := rfl

theorem DataDependencies_eq_spec (a b : verifier.DataDependencies) :
    ∃ r : Bool,
      verifier.DataDependencies.Insts.CoreCmpPartialEqDataDependencies.eq a b = ok r ∧
      (r = true ↔ a = b) := by
  cases a with | mk ar astk =>
  cases b with | mk br bstk =>
  unfold verifier.DataDependencies.Insts.CoreCmpPartialEqDataDependencies.eq
  by_cases hr : ar = br <;> simp [hr]

theorem DataDependencies_has_reg_ok_spec {self : verifier.DataDependencies}
    {reg : Std.U8} {b : Bool}
    (h : verifier.DataDependencies.has_reg self reg = ok b) :
    reg.val < 16 ∧ b = DataDepsHasReg self reg.val := by
  unfold verifier.DataDependencies.has_reg at h
  rcases bind_eq_ok h with ⟨mask, hshl, h⟩
  obtain ⟨hlt, rfl⟩ := u16_shl_one_ok_inv hshl
  simp only [lift_eq_ok, bind_ok] at h
  refine ⟨hlt, ?_⟩
  have hbne : ((self.registers &&& ⟨(1#16) <<< reg.val⟩) != 0#u16) =
      ((self.registers.bv &&& ((1#16) <<< reg.val)) != 0#16) := by
    by_cases heq : (self.registers.bv &&& ((1#16) <<< reg.val)) = 0#16
    · have hu : self.registers &&& ⟨(1#16) <<< reg.val⟩ = 0#u16 :=
        (u16_eq_iff_bv _ _).mpr heq
      simp [heq, hu]
    · have hu : self.registers &&& ⟨(1#16) <<< reg.val⟩ ≠ 0#u16 :=
        fun hc => heq ((u16_eq_iff_bv _ _).mp hc)
      rw [bne_iff_ne.mpr hu, bne_iff_ne.mpr heq]
  rw [← ok_inj h, hbne, bv_and_shift_one_bne_zero self.registers.bv hlt]
  rfl

theorem DataDependencies_has_reg_spec (self : verifier.DataDependencies)
    (reg : Std.U8) (h : reg.val < 16) :
    verifier.DataDependencies.has_reg self reg = ok (DataDepsHasReg self reg.val) := by
  have hex : ∃ b, verifier.DataDependencies.has_reg self reg = ok b := by
    unfold verifier.DataDependencies.has_reg
    rw [bind_ok_eq (u16_shl_one_eq_ok reg h)]
    simp only [lift_eq_ok, bind_ok]
    exact ⟨_, rfl⟩
  obtain ⟨b, hb⟩ := hex
  rw [(DataDependencies_has_reg_ok_spec hb).2] at hb
  exact hb

theorem DataDependencies_add_reg_ok_spec {self d' : verifier.DataDependencies}
    {reg : Std.U8}
    (h : verifier.DataDependencies.add_reg self reg = ok d') :
    reg.val < 16 ∧
    d'.stack = self.stack ∧
    (∀ r < 16, DataDepsHasReg d' r = (DataDepsHasReg self r || decide (r = reg.val))) ∧
    (∀ s, DataDepsHasStack d' s = DataDepsHasStack self s) ∧
    DepsSubset self d' := by
  unfold verifier.DataDependencies.add_reg at h
  rcases bind_eq_ok h with ⟨mask, hshl, h⟩
  obtain ⟨hlt, rfl⟩ := u16_shl_one_ok_inv hshl
  simp only [lift_eq_ok, bind_ok] at h
  have hd_eq := (ok_inj h).symm
  subst hd_eq
  have hreg : ∀ r < 16, DataDepsHasReg { self with registers := self.registers ||| ⟨(1#16) <<< reg.val⟩ } r =
      (DataDepsHasReg self r || decide (r = reg.val)) := by
    intro r hr
    unfold DataDepsHasReg
    change (self.registers.bv ||| ((1#16) <<< reg.val)).getLsbD r = _
    rw [BitVec.getLsbD_or, bv_shift_one_getLsbD hlt hr]
  refine ⟨hlt, rfl, hreg, fun _ => rfl, ?_⟩
  refine ⟨fun r hr hd => ?_, fun _ _ hs => hs⟩
  rw [hreg r (by omega), hd, Bool.true_or]

theorem DataDependencies_add_reg_spec (self : verifier.DataDependencies)
    (reg : Std.U8) (h : reg.val < 16) :
    ∃ d', verifier.DataDependencies.add_reg self reg = ok d' ∧
      d'.stack = self.stack ∧
      (∀ r < 16, DataDepsHasReg d' r = (DataDepsHasReg self r || decide (r = reg.val))) ∧
      (∀ s, DataDepsHasStack d' s = DataDepsHasStack self s) ∧
      DepsSubset self d' := by
  have hd : ∃ d', verifier.DataDependencies.add_reg self reg = ok d' := by
    unfold verifier.DataDependencies.add_reg
    rw [bind_ok_eq (u16_shl_one_eq_ok reg h)]
    simp only [lift_eq_ok, bind_ok]
    exact ⟨_, rfl⟩
  obtain ⟨d', hd'⟩ := hd
  obtain ⟨_, hstk, hr, hs, hsub⟩ := DataDependencies_add_reg_ok_spec hd'
  exact ⟨d', hd', hstk, hr, hs, hsub⟩

theorem DataDependencies_remove_reg_ok_spec {self d' : verifier.DataDependencies}
    {reg : Std.U8} {was_live : Bool}
    (h : verifier.DataDependencies.remove_reg self reg = ok (was_live, d')) :
    reg.val < 16 ∧
    was_live = DataDepsHasReg self reg.val ∧
    d'.stack = self.stack ∧
    (∀ r < 16, DataDepsHasReg d' r = (DataDepsHasReg self r && decide (r ≠ reg.val))) ∧
    (∀ s, DataDepsHasStack d' s = DataDepsHasStack self s) := by
  unfold verifier.DataDependencies.remove_reg at h
  rcases bind_eq_ok h with ⟨mask, hshl, h⟩
  obtain ⟨hlt, rfl⟩ := u16_shl_one_ok_inv hshl
  simp only [lift_eq_ok, bind_ok] at h
  have hinj := ok_inj h
  rcases Prod.mk.inj hinj with ⟨hw, hd'⟩
  have hd_eq := hd'.symm
  subst hd_eq
  have hbne : ((self.registers &&& ⟨(1#16) <<< reg.val⟩) != 0#u16) =
      ((self.registers.bv &&& ((1#16) <<< reg.val)) != 0#16) := by
    by_cases heq : (self.registers.bv &&& ((1#16) <<< reg.val)) = 0#16
    · have hu : self.registers &&& ⟨(1#16) <<< reg.val⟩ = 0#u16 :=
        (u16_eq_iff_bv _ _).mpr heq
      simp [heq, hu]
    · have hu : self.registers &&& ⟨(1#16) <<< reg.val⟩ ≠ 0#u16 :=
        fun hc => heq ((u16_eq_iff_bv _ _).mp hc)
      rw [bne_iff_ne.mpr hu, bne_iff_ne.mpr heq]
  refine ⟨hlt, ?_, rfl, ?_, fun _ => rfl⟩
  · rw [← hw, hbne, bv_and_shift_one_bne_zero self.registers.bv hlt]
    rfl
  · intro r hr
    unfold DataDepsHasReg
    change (self.registers.bv &&& ~~~((1#16) <<< reg.val)).getLsbD r = _
    rw [BitVec.getLsbD_and, BitVec.getLsbD_not, bv_shift_one_getLsbD hlt hr]
    by_cases heq : r = reg.val <;> simp [hr, heq]

theorem DataDependencies_remove_reg_spec (self : verifier.DataDependencies)
    (reg : Std.U8) (h : reg.val < 16) :
    ∃ was_live d',
      verifier.DataDependencies.remove_reg self reg = ok (was_live, d') ∧
      was_live = DataDepsHasReg self reg.val ∧
      d'.stack = self.stack ∧
      (∀ r < 16, DataDepsHasReg d' r = (DataDepsHasReg self r && decide (r ≠ reg.val))) ∧
      (∀ s, DataDepsHasStack d' s = DataDepsHasStack self s) := by
  have hex : ∃ p, verifier.DataDependencies.remove_reg self reg = ok p := by
    unfold verifier.DataDependencies.remove_reg
    rw [bind_ok_eq (u16_shl_one_eq_ok reg h)]
    simp only [lift_eq_ok, bind_ok]
    exact ⟨_, rfl⟩
  obtain ⟨⟨w, d'⟩, hp⟩ := hex
  obtain ⟨_, hw, hstk, hr, hs⟩ := DataDependencies_remove_reg_ok_spec hp
  exact ⟨w, d', hp, hw, hstk, hr, hs⟩

theorem DataDependencies_add_stack_ok_spec {self d' : verifier.DataDependencies}
    {slot : Std.Usize}
    (h : verifier.DataDependencies.add_stack self slot = ok d') :
    slot.val < 64 ∧
    d'.registers = self.registers ∧
    (∀ r, DataDepsHasReg d' r = DataDepsHasReg self r) ∧
    (∀ s < 64, DataDepsHasStack d' s = (DataDepsHasStack self s || decide (s = slot.val))) ∧
    DepsSubset self d' := by
  unfold verifier.DataDependencies.add_stack at h
  rcases bind_eq_ok h with ⟨mask, hshl, h⟩
  obtain ⟨hlt, rfl⟩ := u64_shl_one_ok_inv hshl
  simp only [lift_eq_ok, bind_ok] at h
  have hd_eq := (ok_inj h).symm
  subst hd_eq
  have hstk : ∀ s < 64, DataDepsHasStack { self with stack := self.stack ||| ⟨(1#64) <<< slot.val⟩ } s =
      (DataDepsHasStack self s || decide (s = slot.val)) := by
    intro s hs
    unfold DataDepsHasStack
    change (self.stack.bv ||| ((1#64) <<< slot.val)).getLsbD s = _
    rw [BitVec.getLsbD_or, bv_shift_one_getLsbD hlt hs]
  refine ⟨hlt, rfl, fun _ => rfl, hstk, ?_⟩
  refine ⟨fun _ _ hr => hr, fun s hs hd => ?_⟩
  rw [hstk s hs, hd, Bool.true_or]

theorem DataDependencies_add_stack_spec (self : verifier.DataDependencies)
    (slot : Std.Usize) (h : slot.val < 64) :
    ∃ d', verifier.DataDependencies.add_stack self slot = ok d' ∧
      d'.registers = self.registers ∧
      (∀ r, DataDepsHasReg d' r = DataDepsHasReg self r) ∧
      (∀ s < 64, DataDepsHasStack d' s = (DataDepsHasStack self s || decide (s = slot.val))) ∧
      DepsSubset self d' := by
  have hex : ∃ d', verifier.DataDependencies.add_stack self slot = ok d' := by
    unfold verifier.DataDependencies.add_stack
    rw [bind_ok_eq (u64_shl_one_eq_ok slot h)]
    simp only [lift_eq_ok, bind_ok]
    exact ⟨_, rfl⟩
  obtain ⟨d', hd'⟩ := hex
  obtain ⟨_, hreg, hr, hs, hsub⟩ := DataDependencies_add_stack_ok_spec hd'
  exact ⟨d', hd', hreg, hr, hs, hsub⟩

theorem DataDependencies_remove_stack_ok_spec {self d' : verifier.DataDependencies}
    {slot : Std.Usize} {was_live : Bool}
    (h : verifier.DataDependencies.remove_stack self slot = ok (was_live, d')) :
    slot.val < 64 ∧
    was_live = DataDepsHasStack self slot.val ∧
    d'.registers = self.registers ∧
    (∀ r, DataDepsHasReg d' r = DataDepsHasReg self r) ∧
    (∀ s < 64, DataDepsHasStack d' s = (DataDepsHasStack self s && decide (s ≠ slot.val))) := by
  unfold verifier.DataDependencies.remove_stack at h
  rcases bind_eq_ok h with ⟨mask, hshl, h⟩
  obtain ⟨hlt, rfl⟩ := u64_shl_one_ok_inv hshl
  simp only [lift_eq_ok, bind_ok] at h
  have hinj := ok_inj h
  rcases Prod.mk.inj hinj with ⟨hw, hd'⟩
  have hd_eq := hd'.symm
  subst hd_eq
  have hbne : ((self.stack &&& ⟨(1#64) <<< slot.val⟩) != 0#u64) =
      ((self.stack.bv &&& ((1#64) <<< slot.val)) != 0#64) := by
    by_cases heq : (self.stack.bv &&& ((1#64) <<< slot.val)) = 0#64
    · have hu : self.stack &&& ⟨(1#64) <<< slot.val⟩ = 0#u64 :=
        (u64_eq_iff_bv _ _).mpr heq
      simp [heq, hu]
    · have hu : self.stack &&& ⟨(1#64) <<< slot.val⟩ ≠ 0#u64 :=
        fun hc => heq ((u64_eq_iff_bv _ _).mp hc)
      rw [bne_iff_ne.mpr hu, bne_iff_ne.mpr heq]
  refine ⟨hlt, ?_, rfl, fun _ => rfl, ?_⟩
  · rw [← hw, hbne, bv_and_shift_one_bne_zero self.stack.bv hlt]
    rfl
  · intro s hs
    unfold DataDepsHasStack
    change (self.stack.bv &&& ~~~((1#64) <<< slot.val)).getLsbD s = _
    rw [BitVec.getLsbD_and, BitVec.getLsbD_not, bv_shift_one_getLsbD hlt hs]
    by_cases heq : s = slot.val <;> simp [hs, heq]

theorem DataDependencies_remove_stack_spec (self : verifier.DataDependencies)
    (slot : Std.Usize) (h : slot.val < 64) :
    ∃ was_live d',
      verifier.DataDependencies.remove_stack self slot = ok (was_live, d') ∧
      was_live = DataDepsHasStack self slot.val ∧
      d'.registers = self.registers ∧
      (∀ r, DataDepsHasReg d' r = DataDepsHasReg self r) ∧
      (∀ s < 64, DataDepsHasStack d' s = (DataDepsHasStack self s && decide (s ≠ slot.val))) := by
  have hex : ∃ p, verifier.DataDependencies.remove_stack self slot = ok p := by
    unfold verifier.DataDependencies.remove_stack
    rw [bind_ok_eq (u64_shl_one_eq_ok slot h)]
    simp only [lift_eq_ok, bind_ok]
    exact ⟨_, rfl⟩
  obtain ⟨⟨w, d'⟩, hp⟩ := hex
  obtain ⟨_, hw, hreg, hr, hs⟩ := DataDependencies_remove_stack_ok_spec hp
  exact ⟨w, d', hp, hw, hreg, hr, hs⟩

private theorem bv64_range_mask_getLsbD {c k : Nat} (hc : c < 64) :
    (((1#64) <<< c) - 1#64).getLsbD k = decide (k < c) := by
  by_cases hk : k < 64
  · have hpow_lt : 2 ^ c < 2 ^ 64 := Nat.pow_lt_pow_right (by omega) hc
    have hpow_ge : 1 ≤ 2 ^ c := Nat.one_le_two_pow
    have hshl_nat : ((1#64) <<< c).toNat = 2 ^ c := by
      rw [BitVec.toNat_shiftLeft]
      change (1 <<< c) % 2 ^ 64 = 2 ^ c
      rw [Nat.one_shiftLeft, Nat.mod_eq_of_lt hpow_lt]
    have h1_le : (1#64 : BitVec 64) ≤ (1#64) <<< c := by
      rw [BitVec.le_def, hshl_nat]
      exact hpow_ge
    have hsub_nat : (((1#64) <<< c) - 1#64).toNat = 2 ^ c - 1 := by
      rw [BitVec.toNat_sub_of_le h1_le, hshl_nat]
      rfl
    simp only [BitVec.getLsbD, hsub_nat, Nat.testBit_two_pow_sub_one]
  · have h1 : (((1#64) <<< c) - 1#64).getLsbD k = false :=
      BitVec.getLsbD_of_ge _ _ (by omega)
    have h2 : ¬ (k < c) := by omega
    simp [h1, h2]

theorem DataDependencies_add_stack_range_ok_spec
    {self d' : verifier.DataDependencies} {start «end» : Std.Usize}
    (h : verifier.DataDependencies.add_stack_range self start «end» = ok d') :
    d'.registers = self.registers ∧
    (∀ r, DataDepsHasReg d' r = DataDepsHasReg self r) ∧
    DepsSubset self d' ∧
    (∀ s < 64, start.val ≤ s → s < «end».val → DataDepsHasStack d' s = true) := by
  unfold verifier.DataDependencies.add_stack_range at h
  by_cases hgt : «end» > start
  · simp only [hgt, ↓reduceIte] at h
    have hgt_val : start.val < «end».val := hgt
    obtain ⟨cnt, hcnt, hcnt_val⟩ := usize_sub_eq_ok «end» start (by omega)
    rw [bind_ok_eq hcnt] at h
    by_cases hge64 : cnt >= 64#usize
    · simp only [hge64, ↓reduceIte, bind_ok, lift_eq_ok] at h
      have hd_eq : d' = { self with stack := self.stack ||| ~~~0#u64 } := (ok_inj h).symm
      subst d'
      have hstk_all : ∀ s < 64, DataDepsHasStack { self with stack := self.stack ||| ~~~0#u64 } s = true := by
        intro s hs
        unfold DataDepsHasStack
        change (self.stack.bv ||| ~~~0#64).getLsbD s = true
        rw [BitVec.getLsbD_or, BitVec.getLsbD_not, BitVec.getLsbD_zero]
        simp [hs]
      refine ⟨rfl, fun _ => rfl, ⟨fun _ _ hr => hr, fun s hs _ => hstk_all s hs⟩,
        fun s hs _ _ => hstk_all s hs⟩
    · simp only [hge64, ↓reduceIte] at h
      have hcnt_lt64 : cnt.val < 64 := by
        have hn : ¬ (64#usize).val ≤ cnt.val := hge64
        have h64 := usize_const_val_64
        omega
      rw [bind_ok_eq (u64_shl_one_eq_ok cnt hcnt_lt64)] at h
      have hpow_lt : 2 ^ cnt.val < 2 ^ 64 := Nat.pow_lt_pow_right (by omega) hcnt_lt64
      have hshl_val : (⟨(1#64) <<< cnt.val⟩ : Std.U64).val = 2 ^ cnt.val := by
        rw [← UScalar.bv_toNat, BitVec.toNat_shiftLeft]
        change (1 <<< cnt.val) % 2 ^ 64 = 2 ^ cnt.val
        rw [Nat.one_shiftLeft, Nat.mod_eq_of_lt hpow_lt]
      have h1_le : (1#u64).val ≤ (⟨(1#64) <<< cnt.val⟩ : Std.U64).val := by
        rw [hshl_val]; exact Nat.one_le_two_pow
      have hsub := uscalar_sub_eq_ok (⟨(1#64) <<< cnt.val⟩ : Std.U64) 1#u64 h1_le
      simp only [u64_one_uscalar_bv] at hsub
      rw [bind_ok_eq hsub] at h
      rcases bind_eq_ok h with ⟨mask, hshl_start, h⟩
      obtain ⟨hstart_lt64, rfl⟩ := u64_shl_usize_ok_inv hshl_start
      simp only [u64_mk_bv] at h
      simp only [lift_eq_ok, bind_ok] at h
      have hd_eq : d' = { self with stack := self.stack ||| ⟨(((1#64) <<< cnt.val) - 1#64) <<< start.val⟩ } :=
        (ok_inj h).symm
      subst d'
      have hstk : ∀ s < 64,
          DataDepsHasStack { self with stack := self.stack ||| ⟨(((1#64) <<< cnt.val) - 1#64) <<< start.val⟩ } s =
            (DataDepsHasStack self s || decide (start.val ≤ s ∧ s < «end».val)) := by
        intro s hs
        unfold DataDepsHasStack
        change (self.stack.bv ||| ((((1#64) <<< cnt.val) - 1#64) <<< start.val)).getLsbD s = _
        rw [BitVec.getLsbD_or, BitVec.getLsbD_shiftLeft, bv64_range_mask_getLsbD hcnt_lt64]
        by_cases h1 : start.val ≤ s
        · have hnlt : ¬ (s < start.val) := by omega
          by_cases h2 : s < «end».val
          · have hsub : s - start.val < cnt.val := by omega
            simp [hs, hnlt, h1, h2, hsub]
          · have hnsub : ¬ (s - start.val < cnt.val) := by omega
            simp [hs, hnlt, h1, h2, hnsub]
        · have hlt_s : s < start.val := by omega
          simp [hs, h1, hlt_s]
      refine ⟨rfl, fun _ => rfl, ⟨fun _ _ hr => hr, fun s hs hd => ?_⟩, fun s hs hlo hhi => ?_⟩
      · rw [hstk s hs, hd, Bool.true_or]
      · rw [hstk s hs]; simp [hlo, hhi]
  · simp only [hgt, ↓reduceIte] at h
    have hd_eq : d' = self := (ok_inj h).symm
    subst d'
    have hn : ¬ (start.val < «end».val) := hgt
    exact ⟨rfl, fun _ => rfl, DepsSubset_refl self, fun s _ hlo hhi => absurd (Nat.lt_of_le_of_lt hlo hhi) hn⟩

theorem DataDependencies_add_stack_range_spec
    (self : verifier.DataDependencies) (start «end» : Std.Usize)
    (hend : «end».val ≤ 64) :
    ∃ d', verifier.DataDependencies.add_stack_range self start «end» = ok d' ∧
      d'.registers = self.registers ∧
      (∀ r, DataDepsHasReg d' r = DataDepsHasReg self r) ∧
      DepsSubset self d' ∧
      (∀ s < 64, start.val ≤ s → s < «end».val → DataDepsHasStack d' s = true) := by
  have hex : ∃ d', verifier.DataDependencies.add_stack_range self start «end» = ok d' := by
    unfold verifier.DataDependencies.add_stack_range
    by_cases hgt : «end» > start
    · simp only [hgt, ↓reduceIte]
      have hgt_val : start.val < «end».val := hgt
      obtain ⟨cnt, hcnt, hcnt_val⟩ := usize_sub_eq_ok «end» start (by omega)
      rw [bind_ok_eq hcnt]
      by_cases hge64 : cnt >= 64#usize
      · simp only [hge64, ↓reduceIte, bind_ok, lift_eq_ok]
        exact ⟨_, rfl⟩
      · simp only [hge64, ↓reduceIte]
        have h64 := usize_const_val_64
        have hcnt_lt64 : cnt.val < 64 := by
          have hn : ¬ (64#usize).val ≤ cnt.val := hge64
          omega
        have hstart_lt64 : start.val < 64 := by omega
        rw [bind_ok_eq (u64_shl_one_eq_ok cnt hcnt_lt64)]
        have hpow_lt : 2 ^ cnt.val < 2 ^ 64 := Nat.pow_lt_pow_right (by omega) hcnt_lt64
        have hshl_val : (⟨(1#64) <<< cnt.val⟩ : Std.U64).val = 2 ^ cnt.val := by
          rw [← UScalar.bv_toNat, BitVec.toNat_shiftLeft]
          change (1 <<< cnt.val) % 2 ^ 64 = 2 ^ cnt.val
          rw [Nat.one_shiftLeft, Nat.mod_eq_of_lt hpow_lt]
        have h1_le : (1#u64).val ≤ (⟨(1#64) <<< cnt.val⟩ : Std.U64).val := by
          rw [hshl_val]; exact Nat.one_le_two_pow
        have hsub := uscalar_sub_eq_ok (⟨(1#64) <<< cnt.val⟩ : Std.U64) 1#u64 h1_le
        simp only [u64_one_uscalar_bv] at hsub
        rw [bind_ok_eq hsub,
            bind_ok_eq (u64_shl_usize_eq_ok ⟨((1#64) <<< cnt.val) - 1#64⟩ start hstart_lt64)]
        simp only [lift_eq_ok, bind_ok]
        exact ⟨_, rfl⟩
    · simp only [hgt, ↓reduceIte]
      exact ⟨self, rfl⟩
  obtain ⟨d', hd'⟩ := hex
  obtain ⟨hreg, hr, hsub, hs⟩ := DataDependencies_add_stack_range_ok_spec hd'
  exact ⟨d', hd', hreg, hr, hsub, hs⟩

theorem DataDependencies_merge_spec (a b : verifier.DataDependencies) :
    ∃ m : verifier.DataDependencies,
      verifier.DataDependencies.merge a b = ok m ∧
      (∀ r, DataDepsHasReg m r = (DataDepsHasReg a r || DataDepsHasReg b r)) ∧
      (∀ s, DataDepsHasStack m s = (DataDepsHasStack a s || DataDepsHasStack b s)) ∧
      DepsSubset a m ∧ DepsSubset b m := by
  unfold verifier.DataDependencies.merge
  simp only [lift_eq_ok, bind_ok]
  let m : verifier.DataDependencies := { registers := a.registers ||| b.registers, stack := a.stack ||| b.stack }
  have hreg : ∀ r, DataDepsHasReg m r = (DataDepsHasReg a r || DataDepsHasReg b r) := by
    intro r; unfold DataDepsHasReg; exact BitVec.getLsbD_or
  have hstk : ∀ s, DataDepsHasStack m s = (DataDepsHasStack a s || DataDepsHasStack b s) := by
    intro s; unfold DataDepsHasStack; exact BitVec.getLsbD_or
  refine ⟨m, rfl, hreg, hstk, ⟨fun r _ h => ?_, fun s _ h => ?_⟩, ⟨fun r _ h => ?_, fun s _ h => ?_⟩⟩
  · rw [hreg r, h, Bool.true_or]
  · rw [hstk s, h, Bool.true_or]
  · rw [hreg r, h, Bool.or_true]
  · rw [hstk s, h, Bool.or_true]

theorem DataDependencies_alu_ok_spec
    {self d' : verifier.DataDependencies} {dst : Std.U8} {src : visitor.Source}
    {res : core.result.Result Unit error.VerifierErrorKind}
    (h : verifier.DataDependencies.alu self dst src = ok (res, d')) :
    res = .Ok () ∧
    dst.val < 16 ∧
    d'.stack = self.stack ∧
    DepsSubset self d' ∧
    (DataDepsHasReg self dst.val = true →
      match src with
      | .Reg s => s.val < 16 ∧ DataDepsHasReg d' s.val = true
      | .Value _ => True) := by
  unfold verifier.DataDependencies.alu at h
  rcases bind_eq_ok h with ⟨b, hb, h⟩
  obtain ⟨hdst_lt, rfl⟩ := DataDependencies_has_reg_ok_spec hb
  by_cases hlive : DataDepsHasReg self dst.val = true
  · simp only [hlive, ↓reduceIte] at h
    cases src with
    | Reg s =>
      rcases bind_eq_ok h with ⟨d1, hadd, h⟩
      cases ok_inj h
      obtain ⟨hs_lt, hstk, hr, _, hsub⟩ := DataDependencies_add_reg_ok_spec hadd
      refine ⟨rfl, hdst_lt, hstk, hsub, fun _ => ⟨hs_lt, ?_⟩⟩
      rw [hr s.val hs_lt]; simp
    | Value _ =>
      cases ok_inj h
      exact ⟨rfl, hdst_lt, rfl, DepsSubset_refl self, fun _ => trivial⟩
  · simp only [hlive, Bool.false_eq_true, ↓reduceIte] at h
    cases ok_inj h
    exact ⟨rfl, hdst_lt, rfl, DepsSubset_refl self, fun hc => absurd hc hlive⟩

theorem DataDependencies_jmp_ok_spec
    {self d' : verifier.DataDependencies} {dst : Std.U8} {src : visitor.Source}
    {res : core.result.Result Unit error.VerifierErrorKind}
    (h : verifier.DataDependencies.jmp self dst src = ok (res, d')) :
    res = .Ok () ∧
    dst.val < 16 ∧
    d'.stack = self.stack ∧
    DepsSubset self d' ∧
    DataDepsHasReg d' dst.val = true ∧
    (match src with
     | .Reg s => s.val < 16 ∧ DataDepsHasReg d' s.val = true
     | .Value _ => True) := by
  unfold verifier.DataDependencies.jmp at h
  rcases bind_eq_ok h with ⟨d1, hadd1, h⟩
  obtain ⟨hdst_lt, hstk1, hr1, _, hsub1⟩ := DataDependencies_add_reg_ok_spec hadd1
  have hdst_in1 : DataDepsHasReg d1 dst.val = true := by
    rw [hr1 dst.val hdst_lt]; simp
  cases src with
  | Reg s =>
    rcases bind_eq_ok h with ⟨d2, hadd2, h⟩
    cases ok_inj h
    obtain ⟨hs_lt, hstk2, hr2, _, hsub2⟩ := DataDependencies_add_reg_ok_spec hadd2
    refine ⟨rfl, hdst_lt, hstk2.trans hstk1, DepsSubset_trans hsub1 hsub2, ?_, hs_lt, ?_⟩
    · rw [hr2 dst.val hdst_lt, hdst_in1, Bool.true_or]
    · rw [hr2 s.val hs_lt]; simp
  | Value _ =>
    cases ok_inj h
    exact ⟨rfl, hdst_lt, hstk1, hsub1, hdst_in1, trivial⟩

end StarnixEbpf.Proof
