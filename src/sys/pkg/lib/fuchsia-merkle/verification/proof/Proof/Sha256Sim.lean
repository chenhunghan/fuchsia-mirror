-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.SpecLemmas

set_option maxRecDepth 8192
set_option maxHeartbeats 800000

/-!
# Scalar/Slice/Vec Helpers & Polymorphic `ValidSha256State` Simulation Lemmas

Proves total arithmetic/slice/`Vec` helpers and functional specifications for
`update_with_zeros`, `hash_block`, `hash_block_aligned`, `make_hash_hasher`, and
`hash_hashes_slice` over any external `Sha256State` implementation satisfying
`ValidSha256State`.
-/

namespace FuchsiaMerkle

open Aeneas Aeneas.Std Result
open fuchsia_merkle_core hiding Hash
open FuchsiaMerkle.Assumptions
open FuchsiaMerkle.Spec

-- ============================================================================
-- 1. Loop, Platform Bounds, Scalar Casts, and Total Arithmetic Helpers
-- ============================================================================

theorem loop_done {α β : Type _} (body : α → Result (ControlFlow α β)) (x : α) (r : β)
    (h : body x = ok (.done r)) :
    loop body x = ok r := by
  conv => lhs; rw [loop]
  simp [h, bind_ok]

theorem loop_cont {α β : Type _} (body : α → Result (ControlFlow α β)) (x y : α)
    (h : body x = ok (.cont y)) :
    loop body x = loop body y := by
  conv => lhs; rw [loop]
  simp [h, bind_ok]

theorem bind_ok_eq {α β : Type _} {m₁ : Result α} {a : α} {f : α → Result β} {r : Result β}
    (hm : m₁ = ok a) (hf : f a = r) : (m₁ >>= f) = r := by
  rw [hm, bind_tc_ok]
  exact hf

theorem bind_ok_step {α β : Type _} {m₁ : Result α} {a : α} {f : α → Result β} {r : Result β}
    (hm : m₁ = ok a) (hf : f a = r) :
    (do let x ← m₁; f x) = r := by
  simp only [hm, bind_ok]
  exact hf

@[simp]
theorem StreamSha256_new (sha256 : List Std.U8 → Hash) :
    (StreamSha256 sha256).new = ok [] := rfl

@[simp]
theorem StreamSha256_update (sha256 : List Std.U8 → Hash) (acc : List Std.U8) (s : Slice Std.U8) :
    (StreamSha256 sha256).update acc s = ok (acc ++ s.val) := rfl

@[simp]
theorem StreamSha256_finalize (sha256 : List Std.U8 → Hash) (acc : List Std.U8) :
    (StreamSha256 sha256).finalize acc = ok (sha256 acc) := rfl

theorem sha256_max_bytes_eq : SHA256_MAX_BYTES = 2305843009213693951 := by
  decide

theorem validSha256State_stream (sha256 : List Std.U8 → Hash) :
    ValidSha256State sha256 (StreamSha256 sha256) Eq where
  new_ok := ⟨[], rfl, rfl⟩
  update_ok := fun _ _ bytes h _ => ⟨_, congrArg ok (congrArg (· ++ bytes.val) h), rfl⟩
  finalize_ok := fun _ _ h _ => congrArg ok (congrArg sha256 h)

theorem validLeafCollector_list (maxLeaves : Nat) :
    ValidLeafCollector ListLeafCollector maxLeaves Eq where
  add_leaf_hash_ok := fun _ _ h _ hrel =>
    ⟨_, congrArg ok (congrArg (· ++ [h]) hrel), rfl⟩

theorem validLeafCollector_noop (maxLeaves : Nat) :
    ValidLeafCollector NoopLeafCollector.Insts.Fuchsia_merkle_coreLeafCollector
      maxLeaves (fun _ _ => True) where
  add_leaf_hash_ok := fun c _ _ _ _ => ⟨c, rfl, trivial⟩

theorem uscalar_max_usize : UScalar.max .Usize = Usize.max := by
  rcases System.Platform.numBits_eq with h | h <;>
    simp [UScalar.max, UScalarTy.numBits, Usize.max, Usize.numBits, h]

theorem uscalar_max_u64 : UScalar.max .U64 = U64.max := by
  simp [UScalarTy.numBits, U64.max, U64.numBits]

theorem uscalar_max_u32 : UScalar.max .U32 = U32.max := by
  simp [UScalarTy.numBits, U32.max, U32.numBits]

theorem usize_bounds : U32.max ≤ Usize.max ∧ Usize.max ≤ U64.max := by
  rcases System.Platform.numBits_eq with h | h <;>
    simp [Usize.max, Usize.numBits, U32.max, U32.numBits, U64.max, U64.numBits, h]

theorem usize_max_val_ge : 4294967295 ≤ Usize.max := by
  have ⟨h, _⟩ := usize_bounds
  have hu32 : U32.max = 4294967295 := by simp [U32.max, U32.numBits]
  omega

theorem u64_max_val_eq : U64.max = 18446744073709551615 := by
  simp [U64.max, U64.numBits]

theorem u32_max_val_eq : U32.max = 4294967295 := by
  simp [U32.max, U32.numBits]

@[simp, scalar_tac_simps]
theorem BLOCK_SIZE_val : BLOCK_SIZE.val = 8192 := by
  simp [BLOCK_SIZE]

@[simp, scalar_tac_simps]
theorem HASH_SIZE_val : HASH_SIZE.val = 32 := by
  simp [HASH_SIZE]

@[simp, scalar_tac_simps]
theorem PAGE_SIZE_MIN_val : PAGE_SIZE_MIN.val = 4096 := by
  simp [PAGE_SIZE_MIN]

@[simp, scalar_tac_simps]
theorem HASHES_PER_BLOCK_val : HASHES_PER_BLOCK.val = 256 := by
  simp [HASHES_PER_BLOCK]

theorem cast_u64_BLOCK_SIZE : UScalar.cast .U64 BLOCK_SIZE = 8192#u64 := by
  apply UScalar.eq_of_val_eq
  simp [BLOCK_SIZE, UScalar.cast_val_eq]

theorem cast_u32_BLOCK_SIZE : UScalar.cast .U32 BLOCK_SIZE = 8192#u32 := by
  apply UScalar.eq_of_val_eq
  simp [BLOCK_SIZE, UScalar.cast_val_eq]

theorem cast_u64_HASH_SIZE : UScalar.cast .U64 HASH_SIZE = 32#u64 := by
  apply UScalar.eq_of_val_eq
  simp [HASH_SIZE, UScalar.cast_val_eq]

def usizeOfNat (n : Nat) : Std.Usize :=
  ⟨BitVec.ofNat System.Platform.numBits n⟩

@[simp]
theorem usizeOfNat_val_of_le (n : Nat) (h : n ≤ Usize.max) : (usizeOfNat n).val = n := by
  change (BitVec.ofNat System.Platform.numBits n).toNat = n
  rw [BitVec.toNat_ofNat]
  have p32 : 2 ^ 32 = 4294967296 := by decide
  have p64 : 2 ^ 64 = 18446744073709551616 := by decide
  rcases System.Platform.numBits_eq with hp | hp <;>
    simp only [UScalarTy.numBits, Usize.max, Usize.numBits, hp, p32, p64] at h ⊢ <;>
    exact Nat.mod_eq_of_lt (by omega)

theorem usize_val_le_u64_max (x : Std.Usize) : x.val ≤ U64.max := by
  have ⟨_, hle⟩ := usize_bounds
  have hx : x.val ≤ Usize.max := by scalar_tac
  exact le_trans hx hle

theorem cast_u64_usize_eq (x : Std.Usize) :
    UScalar.cast .U64 x = u64OfNat x.val := by
  have hx := usize_val_le_u64_max x
  apply UScalar.eq_of_val_eq
  rw [UScalar.cast_val_eq, u64OfNat_val_of_le x.val hx]
  have : 2 ^ UScalarTy.U64.numBits = 18446744073709551616 := by decide
  rw [this]
  have hmax := u64_max_val_eq
  exact Nat.mod_eq_of_lt (by omega)

theorem cast_u32_usize_eq (x : Std.Usize) (hx : x.val < 4294967296) :
    UScalar.cast .U32 x = u32OfNat x.val := by
  apply UScalar.eq_of_val_eq
  rw [UScalar.cast_val_eq, u32OfNat_val_of_lt x.val hx]
  have : 2 ^ UScalarTy.U32.numBits = 4294967296 := by decide
  rw [this]
  exact Nat.mod_eq_of_lt hx

theorem cast_usize_u64_val (x : Std.U64) (hx : x.val ≤ Usize.max) :
    (UScalar.cast .Usize x).val = x.val := by
  rw [UScalar.cast_val_eq]
  have p32 : 2 ^ 32 = 4294967296 := by decide
  have p64 : 2 ^ 64 = 18446744073709551616 := by decide
  rcases System.Platform.numBits_eq with hp | hp <;>
    simp only [UScalarTy.numBits, Usize.max, Usize.numBits, hp, p32, p64] at hx ⊢ <;>
    exact Nat.mod_eq_of_lt (by omega)

theorem slice_range_ok {α : Type _} (s : Slice α) (start end_ : Std.Usize)
    (h1 : start.val ≤ end_.val) (h2 : end_.val ≤ s.val.length) :
    ∃ sub : Slice α,
      core.slice.index.Slice.index (core.slice.index.SliceIndexRangeUsizeSlice α)
        s { start := start, «end» := end_ } = ok sub ∧
      sub.val = (s.val.drop start.val).take (end_.val - start.val) := by
  unfold core.slice.index.Slice.index core.slice.index.SliceIndexRangeUsizeSlice
    core.slice.index.SliceIndexRangeUsizeSlice.index
  have hs1 : start ≤ end_ := by scalar_tac
  have hs2 : end_.val ≤ s.length := h2
  simp only [hs1, hs2, and_self, ↓reduceIte]
  refine ⟨_, rfl, ?_⟩
  simp [List.slice]

theorem slice_range_from_ok {α : Type _} (s : Slice α) (start : Std.Usize)
    (h : start.val ≤ s.val.length) :
    ∃ sub : Slice α,
      core.slice.index.Slice.index (core.slice.index.SliceIndexRangeFromUsizeSlice α)
        s { start := start } = ok sub ∧
      sub.val = s.val.drop start.val := by
  unfold core.slice.index.Slice.index core.slice.index.SliceIndexRangeFromUsizeSlice
    core.slice.index.SliceIndexRangeFromUsizeSlice.index
  have hs : start.val ≤ s.length := h
  simp only [hs, ↓reduceIte]
  refine ⟨_, rfl, ?_⟩
  simp

theorem uscalar_add_ok {ty : UScalarTy} (x y : UScalar ty)
    (h : x.val + y.val ≤ UScalar.max ty) :
    ∃ z : UScalar ty, (x + y : Result (UScalar ty)) = ok z ∧ z.val = x.val + y.val := by
  have heq := UScalar.add_equiv x y
  split at heq
  · rename_i z hm
    exact ⟨z, Result.match.isOk.mp hm, heq.2.1⟩
  · exfalso; apply heq
    have : 0 < 2 ^ ty.numBits := by simp
    simp only [UScalar.inBounds, UScalar.max] at h ⊢; omega
  · contradiction

theorem uscalar_sub_ok {ty : UScalarTy} (x y : UScalar ty)
    (h : y.val ≤ x.val) :
    ∃ z : UScalar ty, (x - y : Result (UScalar ty)) = ok z ∧ z.val = x.val - y.val := by
  have heq := UScalar.sub_equiv x y
  split at heq
  · rename_i z hm
    exact ⟨z, Result.match.isOk.mp hm, by omega⟩
  · omega
  · contradiction

theorem uscalar_mul_ok {ty : UScalarTy} (x y : UScalar ty)
    (h : x.val * y.val ≤ UScalar.max ty) :
    ∃ z : UScalar ty, (x * y : Result (UScalar ty)) = ok z ∧ z.val = x.val * y.val := by
  have heq := UScalar.mul_equiv x y
  have hmul_def : (x * y : Result (UScalar ty)) = UScalar.mul x y := rfl
  rw [hmul_def]
  split at heq
  · rename_i z hm
    exact ⟨z, Result.match.isOk.mp hm, heq.2.1⟩
  · omega
  · contradiction

theorem uscalar_div_ok {ty : UScalarTy} (x y : UScalar ty)
    (h : y.val ≠ 0) :
    ∃ z : UScalar ty, (x / y : Result (UScalar ty)) = ok z ∧ z.val = x.val / y.val := by
  have hdiv_def : (x / y : Result (UScalar ty)) = UScalar.div x y := rfl
  rw [hdiv_def]
  unfold UScalar.div
  have hbv : (y.bv != 0) = true := by
    apply bne_iff_ne.mpr
    intro hz
    apply h
    rw [UScalar.val]
    exact congrArg BitVec.toNat hz
  rw [hbv]
  simp only [↓reduceIte]
  refine ⟨⟨BitVec.udiv x.bv y.bv⟩, rfl, ?_⟩
  change (BitVec.udiv x.bv y.bv).toNat = x.bv.toNat / y.bv.toNat
  exact BitVec.toNat_udiv

theorem uscalar_rem_ok {ty : UScalarTy} (x y : UScalar ty)
    (h : y.val ≠ 0) :
    ∃ z : UScalar ty, (x % y : Result (UScalar ty)) = ok z ∧ z.val = x.val % y.val := by
  have hrem_def : (x % y : Result (UScalar ty)) = UScalar.rem x y := rfl
  rw [hrem_def]
  unfold UScalar.rem
  have hnz : (y.val != 0) = true := bne_iff_ne.mpr h
  rw [hnz]
  simp only [↓reduceIte]
  refine ⟨⟨BitVec.umod x.bv y.bv⟩, rfl, ?_⟩
  change (BitVec.umod x.bv y.bv).toNat = x.bv.toNat % y.bv.toNat
  exact BitVec.toNat_umod

/-- `div_ceil_usize a b` never overflows or panics for any `a : Usize` and `b > 0`,
    and computes exact natural ceiling division `(a.val + b.val - 1) / b.val`. -/
theorem div_ceil_usize_spec (a b : Std.Usize) (hb : 0 < b.val) :
    ∃ c : Std.Usize, div_ceil_usize a b = ok c ∧ c.val = (a.val + b.val - 1) / b.val := by
  unfold div_ceil_usize
  obtain ⟨q, hq, hq_val⟩ := uscalar_div_ok a b (by omega)
  obtain ⟨r, hr, hr_val⟩ := uscalar_rem_ok a b (by omega)
  rw [hq, bind_ok, hr, bind_ok]
  have hdiv_mod := Nat.div_add_mod a.val b.val
  by_cases hrz : r = 0#usize
  · subst hrz
    simp only [bne_self_eq_false, Bool.false_eq_true, ↓reduceIte]
    have hrem0 : a.val % b.val = 0 := by simpa using hr_val.symm
    refine ⟨q, rfl, ?_⟩
    rw [hq_val]
    have h1 : (b.val - 1) / b.val = 0 := Nat.div_eq_of_lt (by omega)
    calc a.val / b.val
        = (b.val * (a.val / b.val) + (b.val - 1)) / b.val := by
            rw [Nat.mul_add_div hb, h1, Nat.add_zero]
      _ = (a.val + b.val - 1) / b.val := by congr 1; omega
  · have hr_ne : (r != 0#usize) = true := bne_iff_ne.mpr hrz
    rw [hr_ne]
    simp only [↓reduceIte]
    have hrem_pos : 0 < a.val % b.val := by
      have : r.val ≠ 0 := fun h0 => hrz (UScalar.eq_of_val_eq (by simpa using h0))
      omega
    have hb2 : 2 ≤ b.val := by
      have := Nat.mod_lt a.val hb
      omega
    have hq_lt : q.val + 1 ≤ UScalar.max .Usize := by
      rw [uscalar_max_usize]
      have ha_max : a.val ≤ Usize.max := by scalar_tac
      have hu_ge := usize_max_val_ge
      have hq_le : a.val / b.val ≤ a.val / 2 := Nat.div_le_div_left hb2 (by omega)
      omega
    obtain ⟨c, hc, hc_val⟩ := uscalar_add_ok q 1#usize hq_lt
    refine ⟨c, hc, ?_⟩
    rw [hc_val]
    simp only [UScalar.ofNatCore_val_eq]
    rw [hq_val]
    have hrem_lt : a.val % b.val + b.val - 1 < 2 * b.val := by
      have := Nat.mod_lt a.val hb; omega
    have hmid : (a.val % b.val + b.val - 1) / b.val = 1 := by
      have hsub : (a.val % b.val + b.val - 1 - b.val) / b.val = 0 :=
        Nat.div_eq_of_lt (by omega)
      have heq : a.val % b.val + b.val - 1 = (a.val % b.val + b.val - 1 - b.val) + b.val := by omega
      rw [heq, Nat.add_div_right _ hb, hsub]
    have htot : a.val + b.val - 1 = b.val * (a.val / b.val) + (a.val % b.val + b.val - 1) := by
      omega
    rw [htot, Nat.mul_add_div hb, hmid]

theorem min_usize_spec (x y : Std.Usize) :
    ∃ z : Std.Usize, core.cmp.min core.cmp.OrdUsize x y = ok z ∧ z.val = min x.val y.val := by
  refine ⟨core.cmp.impls.OrdUsize.min x y, ?_, ?_⟩
  · simp [core.cmp.min]
  · simp [core.cmp.impls.OrdUsize.min]
    split_ifs <;> scalar_tac

private theorem allM_u8_eq_step {f : Std.U8 × Std.U8 → Result Bool}
    (hf : ∀ p : Std.U8 × Std.U8, f p = ok (decide (p.1 = p.2))) :
    ∀ (l₁ l₂ : List Std.U8), l₁.length = l₂.length →
      List.allM f (List.zip l₁ l₂) = ok (decide (l₁ = l₂))
  | [], [], _ => rfl
  | x :: xs, y :: ys, hlen => by
    simp only [List.length_cons, Nat.add_right_cancel_iff] at hlen
    simp only [List.zip_cons_cons, List.allM, hf (x, y)]
    by_cases hxy : x = y
    · subst hxy
      simp only [decide_true, bind_tc_ok, List.cons.injEq, true_and]
      exact allM_u8_eq_step hf xs ys hlen
    · simp only [hxy, decide_false, bind_tc_ok, List.cons.injEq, false_and]
      rfl

theorem hash_eq_spec (h₁ h₂ : Hash) :
    Hash.Insts.CoreCmpPartialEqHash.eq h₁ h₂ = ok (decide (h₁ = h₂)) := by
  unfold Hash.Insts.CoreCmpPartialEqHash.eq core.array.equality.PartialEqArray.eq
  have hlen : h₁.length = h₂.length := by simp
  simp only [hlen, ↓reduceIte]
  rw [allM_u8_eq_step ?_ h₁.val h₂.val (by simp)]
  · congr 1
    simp [Aeneas.Std.Array.eq_iff]
  · intro ⟨a, b⟩
    by_cases hab : a = b
    · subst hab
      simp [core.cmp.impls.PartialEqU8.ne, liftFun2, bind_tc_ok]
    · simp only [core.cmp.impls.PartialEqU8.ne, liftFun2, bind_tc_ok, hab]
      rfl

theorem hash_ne_spec (h₁ h₂ : Hash) :
    core.cmp.PartialEq.ne.trait_default Hash.Insts.CoreCmpPartialEqHash h₁ h₂ =
      ok (decide (h₁ ≠ h₂)) := by
  unfold core.cmp.PartialEq.ne.trait_default core.cmp.PartialEq.ne.default
  simp only [hash_eq_spec, bind_tc_ok]
  by_cases h : h₁ = h₂ <;> simp [h]

theorem vec_push_ok {α : Type _} (v : alloc.vec.Vec α) (x : α)
    (h : v.val.length < Usize.max) :
    ∃ v1 : alloc.vec.Vec α, alloc.vec.Vec.push v x = ok v1 ∧ v1.val = v.val ++ [x] := by
  unfold alloc.vec.Vec.push
  have hbool : (decide (List.length v.val + 1 ≤ U32.max) ||
                decide (List.length v.val + 1 ≤ Usize.max)) = true := by
    simp; exact Or.inr h
  simp only [hbool, ↓reduceDIte]
  exact ⟨_, rfl, by simp⟩

theorem vec_index_ok {α : Type _} (v : alloc.vec.Vec α) (i : Std.Usize)
    (h : i.val < v.val.length) :
    alloc.vec.Vec.index (core.slice.index.SliceIndexUsizeSlice α) v i =
      ok (v.val[i.val]'h) := by
  simp [alloc.vec.Vec.index_usize, List.getElem?_eq_getElem h]

theorem slice_index_ok {α : Type _} (s : Slice α) (i : Std.Usize)
    (h : i.val < s.val.length) :
    Slice.index_usize s i = ok (s.val[i.val]'h) := by
  simp [Slice.index_usize, List.getElem?_eq_getElem h]

theorem vec_index_mut_ok {α : Type _} (v : alloc.vec.Vec α) (i : Std.Usize)
    (h : i.val < v.val.length) :
    alloc.vec.Vec.index_mut (core.slice.index.SliceIndexUsizeSlice α) v i =
      ok (v.val[i.val]'h, v.set i) := by
  simp [alloc.vec.Vec.index_mut_usize, alloc.vec.Vec.index_usize,
        List.getElem?_eq_getElem h]

theorem vec_extend_u8_ok (v : alloc.vec.Vec Std.U8) (s : Slice Std.U8)
    (h : v.val.length + s.val.length ≤ Usize.max) :
    ∃ v1 : alloc.vec.Vec Std.U8,
      alloc.vec.Vec.extend_from_slice core.clone.CloneU8 v s = ok v1 ∧
      v1.val = v.val ++ s.val := by
  unfold alloc.vec.Vec.extend_from_slice
  have hlen : v.length + s.length ≤ Usize.max := h
  simp only [hlen, ↓reduceDIte]
  have hcl : ∀ x ∈ s.val, core.clone.CloneU8.clone x = ok x := by
    intro x _; rfl
  have hspec := List.clone_spec hcl
  cases hres : List.clone core.clone.CloneU8.clone s.val with
  | ret s' =>
    rw [hres] at hspec
    have hs'_both : s'.val = s.val ∧ s'.val.length = s.val.length := by
      simpa [WP.spec_ok] using hspec
    simp only [bind_tc_ok]
    exact ⟨_, rfl, by simp [hs'_both.1]⟩
  | vis _ _ =>
    rw [hres] at hspec; simp [WP.spec_vis] at hspec
  | div =>
    rw [hres] at hspec; simp [WP.spec_div] at hspec

theorem vec_resize_zero_u8_ok (v : alloc.vec.Vec Std.U8) :
    ∃ v2 : alloc.vec.Vec Std.U8,
      alloc.vec.Vec.resize core.clone.CloneU8 v 0#usize 0#u8 = ok v2 ∧
      v2.val = [] := by
  unfold alloc.vec.Vec.resize
  split_ifs
  · exact ⟨_, rfl, by simp [List.resize]⟩
  · simp only [bind_tc_ok]
    exact ⟨_, rfl, by simp [List.resize]⟩

theorem slice_split_at_ok {α : Type _} (s : Slice α) (n : Std.Usize)
    (h : n.val ≤ s.val.length) :
    ∃ head tail : Slice α,
      core.slice.Slice.split_at s n = ok (head, tail) ∧
      head.val = s.val.take n.val ∧
      tail.val = s.val.drop n.val := by
  unfold core.slice.Slice.split_at
  have h0 : n.val ≤ s.length := h
  simp only [h0, ↓reduceDIte]
  refine ⟨_, _, rfl, ?_, ?_⟩ <;> simp [List.splitAt_eq]

theorem u64_checked_add_ok (x y : Std.U64) (h : x.val + y.val ≤ U64.max) :
    ∃ z : Std.U64, U64.checked_add x y = some z ∧ z.val = x.val + y.val := by
  have hspec := U64.checked_add_bv_spec x y
  cases hres : U64.checked_add x y with
  | none => rw [hres] at hspec; omega
  | some z => rw [hres] at hspec; exact ⟨z, rfl, hspec.2.1⟩

theorem u64_checked_mul_spec_opt (x y : Std.U64) :
    (U64.checked_mul x y = none ∧ U64.max < x.val * y.val) ∨
    (∃ z : Std.U64, U64.checked_mul x y = some z ∧
      x.val * y.val ≤ U64.max ∧ z.val = x.val * y.val) := by
  have hspec := U64.checked_mul_bv_spec x y
  cases hres : U64.checked_mul x y with
  | none => rw [hres] at hspec; exact Or.inl ⟨rfl, hspec⟩
  | some z => rw [hres] at hspec; exact Or.inr ⟨z, rfl, hspec.1, hspec.2.1⟩

theorem usize_checked_add_spec_opt (x y : Std.Usize) :
    (Usize.checked_add x y = none ∧ Usize.max < x.val + y.val) ∨
    (∃ z : Std.Usize, Usize.checked_add x y = some z ∧
      x.val + y.val ≤ Usize.max ∧ z.val = x.val + y.val) := by
  have hspec := Usize.checked_add_bv_spec x y
  cases hres : Usize.checked_add x y with
  | none => rw [hres] at hspec; exact Or.inl ⟨rfl, hspec⟩
  | some z => rw [hres] at hspec; exact Or.inr ⟨z, rfl, hspec.1, hspec.2.1⟩

theorem usize_checked_mul_spec_opt (x y : Std.Usize) :
    (Usize.checked_mul x y = none ∧ Usize.max < x.val * y.val) ∨
    (∃ z : Std.Usize, Usize.checked_mul x y = some z ∧
      x.val * y.val ≤ Usize.max ∧ z.val = x.val * y.val) := by
  have hspec := Usize.checked_mul_bv_spec x y
  cases hres : Usize.checked_mul x y with
  | none => rw [hres] at hspec; exact Or.inl ⟨rfl, hspec⟩
  | some z => rw [hres] at hspec; exact Or.inr ⟨z, rfl, hspec.1, hspec.2.1⟩

theorem usize_saturating_add_val (x y : Std.Usize) :
    (core.num.Usize.saturating_add x y).val = min Usize.max (x.val + y.val) := by
  unfold core.num.Usize.saturating_add UScalar.saturating_add
  rw [uscalar_max_usize]
  change (BitVec.ofNat System.Platform.numBits (min Usize.max (x.val + y.val))).toNat =
         min Usize.max (x.val + y.val)
  rw [BitVec.toNat_ofNat]
  have hle : min Usize.max (x.val + y.val) ≤ Usize.max := Nat.min_le_left _ _
  have p32 : 2 ^ 32 = 4294967296 := by decide
  have p64 : 2 ^ 64 = 18446744073709551616 := by decide
  rcases System.Platform.numBits_eq with hp | hp <;>
    simp only [UScalarTy.numBits, Usize.max, Usize.numBits, hp, p32, p64] at hle ⊢ <;>
    exact Nat.mod_eq_of_lt (by omega)

-- ============================================================================
-- 2. Polymorphic Functional Specifications over `ValidSha256State`
-- ============================================================================

private theorem zeros_array_val :
    update_with_zeros.ZEROS.val = specZeroPad 512 := by
  unfold update_with_zeros.ZEROS specZeroPad
  simp only [Array.repeat_val]
  rfl

theorem update_with_zeros_loop_gen_spec
    {H : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    (hsha : ValidSha256State sha256 hInst R_H) :
    ∀ (steps : Nat) (hasher : H) (acc : List Std.U8) (count : Std.Usize),
      count.val / 512 = steps →
      R_H hasher acc →
      acc.length + count.val ≤ SHA256_MAX_BYTES →
      ∃ (hasher1 : H) (rem : Std.Usize),
        update_with_zeros_loop hInst hasher count = ok (hasher1, rem) ∧
        R_H hasher1 (acc ++ specZeroPad ((count.val / 512) * 512)) ∧
        rem.val = count.val % 512
  | 0, hasher, acc, count, hsteps, hrel, _ => by
    have hbody :
        update_with_zeros_loop.body hInst hasher count =
          ok (.done (hasher, count)) := by
      unfold update_with_zeros_loop.body
      have hge : ¬ (count >= update_with_zeros.BUF_SIZE) := by
        have : update_with_zeros.BUF_SIZE.val = 512 := by simp [update_with_zeros.BUF_SIZE]
        scalar_tac
      simp [hge]
    refine ⟨hasher, count, ?_, ?_, by omega⟩
    · unfold update_with_zeros_loop
      exact loop_done _ (hasher, count) _ hbody
    · simpa [hsteps, specZeroPad] using hrel
  | steps + 1, hasher, acc, count, hsteps, hrel, hmax => by
    have hge_nat : 512 ≤ count.val := by omega
    have hbuf : update_with_zeros.BUF_SIZE.val = 512 := by simp [update_with_zeros.BUF_SIZE]
    obtain ⟨count1, hsub, hsub_val⟩ := uscalar_sub_ok count update_with_zeros.BUF_SIZE (by omega)
    rw [hbuf] at hsub_val
    set zslice := Array.to_slice update_with_zeros.ZEROS
    have hz_val : zslice.val = specZeroPad 512 := by
      simp [zslice, zeros_array_val]
    obtain ⟨hasher_mid, hupd, hrel_mid⟩ := hsha.update_ok hasher acc zslice hrel
      (by rw [hz_val, specZeroPad_length]; omega)
    rw [hz_val] at hrel_mid
    have hbody :
        update_with_zeros_loop.body hInst hasher count =
          ok (.cont (hasher_mid, count1)) := by
      unfold update_with_zeros_loop.body
      have hge : count >= update_with_zeros.BUF_SIZE := by scalar_tac
      simp only [hge, ↓reduceIte, hsub, lift, zslice, hupd, bind_ok]
    have hdiv1 : count1.val / 512 = steps := by omega
    obtain ⟨hasher1, rem, ih, hrel1, hrem⟩ :=
      update_with_zeros_loop_gen_spec hsha steps hasher_mid (acc ++ specZeroPad 512) count1
        hdiv1 hrel_mid (by simp [specZeroPad_length]; omega)
    refine ⟨hasher1, rem, ?_, ?_, ?_⟩
    · unfold update_with_zeros_loop at ih ⊢
      rw [loop_cont _ (hasher, count) _ hbody, ih]
    · rw [List.append_assoc, hdiv1] at hrel1
      have hmul : (count.val / 512) * 512 = 512 + steps * 512 := by omega
      simpa [hmul, specZeroPad_add] using hrel1
    · rw [hrem, hsub_val]
      have heq : count.val = (count.val - 512) + 512 := by omega
      conv => rhs; rw [heq]
      rw [Nat.add_mod_right]

/-- `update_with_zeros` succeeds without panicking for any `ValidSha256State` whenever
    `acc.length + count.val ≤ SHA256_MAX_BYTES`, appending `count.val` zero bytes. -/
theorem update_with_zeros_gen_spec
    {H : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hasher : H) (acc : List Std.U8) (count : Std.Usize)
    (hrel : R_H hasher acc)
    (hmax : acc.length + count.val ≤ SHA256_MAX_BYTES) :
    ∃ hasher',
      update_with_zeros hInst hasher count = ok hasher' ∧
      R_H hasher' (acc ++ specZeroPad count.val) := by
  unfold update_with_zeros
  obtain ⟨hasher1, rem, hloop, hrel1, hrem_val⟩ :=
    update_with_zeros_loop_gen_spec hsha (count.val / 512) hasher acc count rfl hrel hmax
  rw [hloop, bind_ok, uncurry_apply_pair]
  have hrem_lt : rem.val < 512 := by rw [hrem_val]; exact Nat.mod_lt _ (by decide)
  have hdiv_mod := Nat.div_add_mod count.val 512
  by_cases hgt : rem > 0#usize
  · simp only [hgt, ↓reduceIte]
    unfold core.array.Array.index core.ops.index.IndexSlice
    obtain ⟨sub, hsub, hsub_val⟩ := slice_range_ok
      update_with_zeros.ZEROS.to_slice 0#usize rem (Nat.zero_le _)
      (by simp [zeros_array_val]; omega)
    have hsub_eq : sub.val = specZeroPad rem.val := by
      rw [hsub_val, Array.val_to_slice, zeros_array_val]
      simp only [UScalar.ofNatCore_val_eq, List.drop_zero, Nat.sub_zero]
      unfold specZeroPad
      rw [List.take_replicate, Nat.min_eq_left (le_of_lt hrem_lt)]
    obtain ⟨hasher2, hupd2, hrel2⟩ := hsha.update_ok hasher1
      (acc ++ specZeroPad ((count.val / 512) * 512)) sub hrel1
      (by simp [hsub_eq, specZeroPad_length]; omega)
    refine ⟨hasher2, ?_, ?_⟩
    · simp only [hsub, hupd2, bind_ok]
    · rw [hsub_eq, List.append_assoc, ← specZeroPad_add, hrem_val] at hrel2
      have htot : (count.val / 512) * 512 + count.val % 512 = count.val := by omega
      simpa [htot] using hrel2
  · simp only [hgt, ↓reduceIte]
    have hrem0 : count.val % 512 = 0 := by
      have : rem.val = 0 := by scalar_tac
      omega
    have htot : (count.val / 512) * 512 = count.val := by omega
    exact ⟨hasher1, rfl, by simpa [htot] using hrel1⟩

/-- `hash_block` succeeds without panicking on valid Merkle blocks (`len ≤ 8192`, `offset % 8192 = 0`)
    for any `ValidSha256State` and computes `specHashBlock sha256 block.val offset`. -/
theorem hash_block_gen_spec
    {H : Type} (sha256 : List Std.U8 → Hash)
    (hInst : Sha256State H) (R_H : H → List Std.U8 → Prop)
    (block : Slice Std.U8) (offset : Std.U64)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hlen : block.val.length ≤ BLOCK_BYTES)
    (hoff : offset.val % BLOCK_BYTES = 0) :
    hash_block hInst block offset = ok (specHashBlock sha256 block.val offset) := by
  unfold hash_block
  have hlen8192 : block.val.length ≤ 8192 := by simpa [BLOCK_BYTES] using hlen
  have hoff8192 : offset.val % 8192 = 0 := by simpa [BLOCK_BYTES] using hoff
  have hsmax := sha256_max_bytes_eq
  have hm1 : Slice.len block <= BLOCK_SIZE := by scalar_tac
  have hmult : core.num.U64.is_multiple_of offset 8192#u64 = ok true := by
    simp [core.num.U64.is_multiple_of, UScalar.is_multiple_of, hoff8192]
  have hcast32 : UScalar.cast .U32 (Slice.len block) = u32OfNat block.val.length := by
    rw [cast_u32_usize_eq (Slice.len block) (by simp; omega)]
    simp
  obtain ⟨h0, hnew, hrel0⟩ := hsha.new_ok
  obtain ⟨idArr, hid, hid_val⟩ := make_identity_spec (u32OfNat block.val.length) 0#u64 offset
  obtain ⟨h1, hupd1, hrel1⟩ := hsha.update_ok h0 [] idArr.to_slice hrel0
    (by simp [hid_val]; omega)
  rw [Array.val_to_slice, hid_val, List.nil_append] at hrel1
  obtain ⟨h2, hupd2, hrel2⟩ := hsha.update_ok h1 (specIdentity (u32OfNat block.val.length) 0#u64 offset)
    block hrel1 (by simp; omega)
  simp only [massert, hm1, ↓reduceIte, lift, cast_u64_BLOCK_SIZE, hmult,
             hnew, hcast32, hid, hupd1, hupd2, bind_ok]
  by_cases hfull : Slice.len block != BLOCK_SIZE
  · simp only [hfull, ↓reduceIte, core.slice.Slice.is_empty, Slice.length, bind_ok]
    obtain ⟨padLen, hsub, hsub_val⟩ := uscalar_sub_ok BLOCK_SIZE (Slice.len block) (by
      simp; omega)
    simp only [BLOCK_SIZE_val, Slice.len_val, Slice.length] at hsub_val
    by_cases hemp : block.val.length = 0
    · simp only [hemp, decide_true, ↓reduceIte]
      by_cases hoff0 : offset = 0#u64
      · subst hoff0
        simp only [↓reduceIte, bind_ok]
        have hbnil : block.val = [] := List.eq_nil_of_length_eq_zero hemp
        rw [hbnil, List.append_nil] at hrel2
        have hfin := hsha.finalize_ok h2 _ hrel2 (by simp; omega)
        simpa [specHashBlock, hbnil] using hfin
      · simp only [hoff0, ↓reduceIte, hsub, bind_ok]
        obtain ⟨h3, hzupd, hrel3⟩ := update_with_zeros_gen_spec hsha h2 _ padLen hrel2
          (by simp [hsub_val]; omega)
        rw [hsub_val] at hrel3
        have hfin := hsha.finalize_ok h3 _ hrel3 (by simp; omega)
        simp only [hzupd, bind_ok, hfin, specHashBlock, hemp, hoff0, and_false, ↓reduceIte,
                   BLOCK_BYTES, List.append_assoc]
    · simp only [hemp, decide_false, Bool.false_eq_true, ↓reduceIte, hsub, bind_ok]
      obtain ⟨h3, hzupd, hrel3⟩ := update_with_zeros_gen_spec hsha h2 _ padLen hrel2
        (by simp [hsub_val]; omega)
      rw [hsub_val] at hrel3
      have hfin := hsha.finalize_ok h3 _ hrel3 (by simp; omega)
      simp only [hzupd, bind_ok, hfin, specHashBlock, hemp, false_and, ↓reduceIte,
                 BLOCK_BYTES, List.append_assoc]
  · simp only [hfull, Bool.false_eq_true, ↓reduceIte, bind_ok]
    have heq8192 : block.val.length = 8192 := by
      have hbeq : Slice.len block = BLOCK_SIZE := by
        by_contra hne
        exact hfull (bne_iff_ne.mpr hne)
      simpa [BLOCK_SIZE_val] using congrArg UScalar.val hbeq
    have hfin := hsha.finalize_ok h2 _ hrel2 (by simp [heq8192]; omega)
    simpa [specHashBlock, heq8192, BLOCK_BYTES, specZeroPad] using hfin

/-- Specialization of `hash_block_gen_spec` to `StreamSha256 sha256`. -/
theorem hash_block_stream_spec
    (sha256 : List Std.U8 → Hash) (block : Slice Std.U8) (offset : Std.U64)
    (hlen : block.val.length ≤ BLOCK_BYTES)
    (hoff : offset.val % 8192 = 0) :
    hash_block (StreamSha256 sha256) block offset =
      ok (specHashBlock sha256 block.val offset) :=
  hash_block_gen_spec sha256 (StreamSha256 sha256) Eq block offset
    (validSha256State_stream sha256) hlen (by simpa [BLOCK_BYTES] using hoff)

/-- `hash_block_aligned` succeeds without panicking for any `ValidSha256State` when
    `buf_len ∈ {8192, 4096}`, `0 < u_len ≤ buf_len ≤ data.len`, and `offset % 8192 = 0`. -/
theorem hash_block_aligned_gen_spec
    {H : Type} (sha256 : List Std.U8 → Hash)
    (hInst : Sha256State H) (R_H : H → List Std.U8 → Prop)
    (offset : Std.U64) (data : Slice Std.U8) (buf_len u_len : Std.Usize)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hbuf : buf_len.val = BLOCK_BYTES ∨ buf_len.val = MIN_PAGE_BYTES)
    (hpos : 0 < u_len.val)
    (hule : u_len.val ≤ buf_len.val)
    (hoff : offset.val % BLOCK_BYTES = 0)
    (hdlen : buf_len.val ≤ data.val.length) :
    hash_block_aligned hInst offset data buf_len u_len =
      ok (sha256 (specIdentity (u32OfNat u_len.val) 0#u64 offset ++
        data.val.take buf_len.val ++
        specZeroPad (BLOCK_BYTES - buf_len.val))) := by
  unfold hash_block_aligned
  have hbuf' : buf_len.val = 8192 ∨ buf_len.val = 4096 := by
    simpa [BLOCK_BYTES, MIN_PAGE_BYTES] using hbuf
  have hoff' : offset.val % 8192 = 0 := by simpa [BLOCK_BYTES] using hoff
  have hsmax := sha256_max_bytes_eq
  have hm_buf : ((buf_len = BLOCK_SIZE) || (buf_len = PAGE_SIZE_MIN)) = true := by
    rcases hbuf' with h8 | h4
    · have : buf_len = BLOCK_SIZE := UScalar.eq_of_val_eq (by simp [h8])
      simp [this]
    · have : buf_len = PAGE_SIZE_MIN := UScalar.eq_of_val_eq (by simp [h4])
      simp [this]
  have hm_unz : u_len > 0#usize := by scalar_tac
  have hm_ule : u_len <= buf_len := by scalar_tac
  have hmult : core.num.U64.is_multiple_of offset 8192#u64 = ok true := by
    simp [core.num.U64.is_multiple_of, UScalar.is_multiple_of, hoff']
  have hm_dlen : Slice.len data >= buf_len := by scalar_tac
  have hcast32 : UScalar.cast .U32 u_len = u32OfNat u_len.val :=
    cast_u32_usize_eq u_len (by rcases hbuf' with h8 | h4 <;> omega)
  obtain ⟨h0, hnew, hrel0⟩ := hsha.new_ok
  obtain ⟨idArr, hid, hid_val⟩ := make_identity_spec (u32OfNat u_len.val) 0#u64 offset
  obtain ⟨h1, hupd1, hrel1⟩ := hsha.update_ok h0 [] idArr.to_slice hrel0
    (by simp [hid_val]; omega)
  rw [Array.val_to_slice, hid_val, List.nil_append] at hrel1
  obtain ⟨sub, hsub_s, hsub_s_val⟩ := slice_range_ok data 0#usize buf_len
    (Nat.zero_le _) hdlen
  have hsub_take : sub.val = data.val.take buf_len.val := by
    simpa [List.drop_zero] using hsub_s_val
  have hsub_len : sub.val.length = buf_len.val := by
    rw [hsub_take, List.length_take_of_le hdlen]
  obtain ⟨h2, hupd2, hrel2⟩ := hsha.update_ok h1
    (specIdentity (u32OfNat u_len.val) 0#u64 offset) sub hrel1
    (by rw [hsub_len]; simp; rcases hbuf' with h8 | h4 <;> omega)
  rw [hsub_take] at hrel2
  simp only [massert, hm_buf, hm_unz, hm_ule, hm_dlen, ↓reduceIte,
             lift, cast_u64_BLOCK_SIZE, hmult, hnew, hcast32, hid,
             hupd1, hsub_s, hupd2, bind_ok]
  rcases hbuf' with h8 | h4
  · have hlt : ¬ (buf_len < BLOCK_SIZE) := by scalar_tac
    simp only [hlt, ↓reduceIte, bind_ok]
    have hfin := hsha.finalize_ok h2 _ hrel2 (by simp [h8]; omega)
    simpa [h8, BLOCK_BYTES, specZeroPad] using hfin
  · have hlt : buf_len < BLOCK_SIZE := by scalar_tac
    obtain ⟨padLen, hsub, hsub_val⟩ := uscalar_sub_ok BLOCK_SIZE buf_len (by simp [h4])
    simp only [BLOCK_SIZE_val, h4] at hsub_val
    obtain ⟨h3, hzupd, hrel3⟩ := update_with_zeros_gen_spec hsha h2 _ padLen hrel2
      (by simp [h4, hsub_val]; omega)
    rw [hsub_val, List.append_assoc] at hrel3
    have hfin := hsha.finalize_ok h3 _ hrel3 (by simp [h4]; omega)
    simp only [hlt, ↓reduceIte, hsub, hzupd, bind_ok]
    simpa [h4, BLOCK_BYTES, List.append_assoc] using hfin

/-- `make_hash_hasher` succeeds for all `level > 0` under any `ValidSha256State` and yields
    a hasher state related to `specIdentity bytes_per_hash level offset`. -/
theorem make_hash_hasher_gen_spec
    {H : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (bytes_per_hash : Std.U32) (level offset : Std.U64)
    (hlvl : 0 < level.val) :
    ∃ h,
      make_hash_hasher hInst bytes_per_hash level offset = ok h ∧
      R_H h (specIdentity bytes_per_hash level offset) := by
  unfold make_hash_hasher
  have hm : level > 0#u64 := by scalar_tac
  have hsmax := sha256_max_bytes_eq
  obtain ⟨h0, hnew, hrel0⟩ := hsha.new_ok
  obtain ⟨idArr, hid, hid_val⟩ := make_identity_spec bytes_per_hash level offset
  obtain ⟨h1, hupd, hrel1⟩ := hsha.update_ok h0 [] idArr.to_slice hrel0
    (by simp [hid_val]; omega)
  rw [Array.val_to_slice, hid_val, List.nil_append] at hrel1
  refine ⟨h1, ?_, hrel1⟩
  simp [massert, hm, hnew, hid, lift, hupd]

theorem hash_hashes_slice_loop_gen_spec
    {H : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hashes : Slice Hash) :
    ∀ (rem : Nat) (hasher : H) (acc : List Std.U8) (i : Std.Usize),
      i.val ≤ hashes.val.length →
      hashes.val.length - i.val = rem →
      R_H hasher acc →
      acc.length + (hashes.val.length - i.val) * 32 ≤ SHA256_MAX_BYTES →
      ∃ hasher',
        hash_hashes_slice_loop hInst hashes hasher i = ok hasher' ∧
        R_H hasher' (acc ++ concatHashes (hashes.val.drop i.val))
  | 0, hasher, acc, i, _, _, hrel, _ => by
    have heq : i.val = hashes.val.length := by omega
    have hbody :
        hash_hashes_slice_loop.body hInst hashes hasher i = ok (.done hasher) := by
      unfold hash_hashes_slice_loop.body
      have hlt : ¬ (i < Slice.len hashes) := by scalar_tac
      simp [hlt]
    refine ⟨hasher, ?_, ?_⟩
    · unfold hash_hashes_slice_loop
      exact loop_done _ (hasher, i) _ hbody
    · simpa [heq] using hrel
  | rem + 1, hasher, acc, i, _, hrem, hrel, hmax => by
    have hlt_nat : i.val < hashes.val.length := by omega
    have hlt : i < Slice.len hashes := by scalar_tac
    set elem := hashes.val[i.val]'hlt_nat
    have hidx : Slice.index_usize hashes i = ok elem := by
      simp [Slice.index_usize, List.getElem?_eq_getElem hlt_nat, elem]
    have hi1_bound : i.val + 1 ≤ UScalar.max .Usize := by
      rw [uscalar_max_usize]
      have := hashes.property; omega
    obtain ⟨i1, hi1, hi1_val⟩ := uscalar_add_ok i 1#usize hi1_bound
    simp only [UScalar.ofNatCore_val_eq] at hi1_val
    have helem_len : elem.to_slice.val.length = 32 := by
      simp [Array.val_to_slice]
    obtain ⟨hasher_mid, hupd, hrel_mid⟩ := hsha.update_ok hasher acc elem.to_slice hrel
      (by rw [helem_len]; omega)
    rw [Array.val_to_slice] at hrel_mid
    have hbody :
        hash_hashes_slice_loop.body hInst hashes hasher i =
          ok (.cont (hasher_mid, i1)) := by
      unfold hash_hashes_slice_loop.body
      simp only [hlt, ↓reduceIte, hidx, lift, hupd, hi1, bind_ok]
    obtain ⟨hasher', ih, hrel'⟩ :=
      hash_hashes_slice_loop_gen_spec hsha hashes rem hasher_mid (acc ++ elem.val) i1
        (by omega) (by omega) hrel_mid
        (by simp [elem.property]; omega)
    refine ⟨hasher', ?_, ?_⟩
    · unfold hash_hashes_slice_loop at ih ⊢
      rw [loop_cont _ (hasher, i) _ hbody, ih]
    · rw [List.drop_eq_getElem_cons hlt_nat, concatHashes_cons, ← hi1_val]
      simpa [List.append_assoc, elem] using hrel'

/-- `hash_hashes_slice` succeeds without panicking for any `ValidSha256State` when
    `hashes.length ≤ hashes_per_hash`, `(hashes_per_hash - hashes.length) * 32 ≤ Usize.max`,
    `index * bytes_per_hash ≤ U64.max`, and `12 + hashes_per_hash * 32 ≤ SHA256_MAX_BYTES`,
    computing `specHashInternal` at `level = 1`. -/
theorem hash_hashes_slice_gen_spec
    {H : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hashes_per_hash : Std.Usize) (bytes_per_hash : Std.U32)
    (index : Std.Usize) (hashes : Slice Hash)
    (hlen : hashes.length ≤ hashes_per_hash.val)
    (hpad : (hashes_per_hash.val - hashes.length) * 32 ≤ Usize.max)
    (hoff : index.val * bytes_per_hash.val ≤ U64.max)
    (hmax : 12 + hashes_per_hash.val * 32 ≤ SHA256_MAX_BYTES) :
    hash_hashes_slice hInst hashes_per_hash bytes_per_hash index hashes =
      ok (specHashInternal sha256 hashes_per_hash.val bytes_per_hash 1#u64
            (u64OfNat (index.val * bytes_per_hash.val)) hashes.val) := by
  unfold hash_hashes_slice
  have hm : Slice.len hashes <= hashes_per_hash := by scalar_tac
  have hc_idx : UScalar.cast .U64 index = u64OfNat index.val := cast_u64_usize_eq index
  have hc_bph : (UScalar.cast .U64 bytes_per_hash).val = bytes_per_hash.val := by
    rw [UScalar.cast_val_eq]
    have : 2 ^ UScalarTy.U64.numBits = 18446744073709551616 := by decide
    rw [this]
    have hb : bytes_per_hash.val ≤ U32.max := by scalar_tac
    have hu := u32_max_val_eq
    exact Nat.mod_eq_of_lt (by omega)
  have hidx_val : (u64OfNat index.val).val = index.val :=
    u64OfNat_val_of_le _ (usize_val_le_u64_max index)
  obtain ⟨loff, hmul, hloff_val⟩ := uscalar_mul_ok
    (u64OfNat index.val) (UScalar.cast .U64 bytes_per_hash) (by
      rw [uscalar_max_u64, hidx_val, hc_bph]; exact hoff)
  have hloff_eq : loff = u64OfNat (index.val * bytes_per_hash.val) := by
    apply UScalar.eq_of_val_eq
    rw [hloff_val, hidx_val, hc_bph, u64OfNat_val_of_le _ hoff]
  simp only [massert, hm, ↓reduceIte, lift, hc_idx, hmul, hloff_eq, bind_ok]
  set loff64 := u64OfNat (index.val * bytes_per_hash.val)
  obtain ⟨h0, hmhh, hrel0⟩ := make_hash_hasher_gen_spec hsha bytes_per_hash 1#u64 loff64 (by decide)
  obtain ⟨h1, hloop, hrel1⟩ := hash_hashes_slice_loop_gen_spec hsha hashes
    hashes.val.length h0 (specIdentity bytes_per_hash 1#u64 loff64) 0#usize
    (Nat.zero_le _) (by simp) hrel0
    (by simp [Slice.length] at hlen ⊢; omega)
  simp only [UScalar.ofNatCore_val_eq, List.drop_zero] at hrel1
  rw [hmhh, bind_ok, hloop, bind_ok]
  by_cases hne : Slice.len hashes != hashes_per_hash
  · simp only [hne, ↓reduceIte]
    obtain ⟨diff, hsub, hdiff_val⟩ := uscalar_sub_ok hashes_per_hash (Slice.len hashes) (by
      simp; omega)
    simp only [Slice.len_val] at hdiff_val
    obtain ⟨zcount, hmul2, hzcount_val⟩ := uscalar_mul_ok diff HASH_SIZE (by
      rw [uscalar_max_usize, hdiff_val, HASH_SIZE_val]; exact hpad)
    rw [hdiff_val, HASH_SIZE_val] at hzcount_val
    obtain ⟨h2, hzupd, hrel2⟩ := update_with_zeros_gen_spec hsha h1 _ zcount hrel1
      (by simp [hzcount_val, Slice.length] at hlen ⊢; omega)
    rw [hzcount_val] at hrel2
    have hfin := hsha.finalize_ok h2 _ hrel2
      (by simp [Slice.length] at hlen ⊢; omega)
    simp only [hsub, hmul2, hzupd, hfin, bind_ok, specHashInternal, specInternalPreimage]
  · simp only [hne, Bool.false_eq_true, ↓reduceIte]
    have heq : hashes.val.length = hashes_per_hash.val := by
      have hbeq : Slice.len hashes = hashes_per_hash := by
        by_contra hne'
        exact hne (bne_iff_ne.mpr hne')
      simpa [Slice.length] using congrArg UScalar.val hbeq
    have hfin := hsha.finalize_ok h1 _ hrel1 (by simp [heq]; omega)
    simpa [specHashInternal, specInternalPreimage, heq, specZeroPad] using hfin

end FuchsiaMerkle
