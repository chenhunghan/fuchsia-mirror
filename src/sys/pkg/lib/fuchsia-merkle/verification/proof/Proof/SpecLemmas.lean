-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Assumptions
import Spec
import Extracted
import Std.Tactic.BVDecide

set_option maxRecDepth 8192

/-!
# Protocol Domain-Separation, Collision-Reduction, and Zero-Extension Lemmas

Foundational lemmas for the Fuchsia Merkle Tree protocol definitions in `Assumptions` and `Spec`:

1. **Bit-Level Header Injectivity & Domain Separation (`make_identity_injective`)**:
   Verified via `bv_decide` on 64-bit and 32-bit bitvectors: whenever byte `offset` is a multiple
   of `8192` (`2^13`) and tree `level < 8192`, the 12-byte little-endian block identity
   `u64(offset | level) || u32(length)` uniquely determines `(length, level, offset)`.
   In particular, level-0 leaf preimages (`level = 0`) and internal node preimages
   (`1 ≤ level < 8192`) are strictly disjoint across all lengths and offsets
   (`leaf_internal_domain_separation`).
2. **Leaf & Internal Node Preimage Injectivity (`specHashBlock_eq_imp_eq_or_collision`,
   `specHashInternal_eq_imp_eq_or_collision`)**:
   Equal digests imply either equal canonical inputs or an explicit SHA-256 collision witness
   (`HashCollision sha256`).
3. **Tree Root Collision Soundness (`specRootFromLeaves_eq_imp_eq_or_collision`)**:
   Two leaf lists of equal canonical length have the same Merkle root only if they are identical
   or `sha256` admits a collision.
4. **Zero-Hash Extension Characterization (`specRootFromLeaves_append_zeroHash`)**:
   For any `2 ≤ M` with `M % 256 ≠ 0`, appending `zeroHash` (`[0u8; 32]`) preserves the root
   with zero SHA-256 collisions.
-/

namespace FuchsiaMerkle

open Aeneas Aeneas.Std Result
open fuchsia_merkle_core hiding Hash
open FuchsiaMerkle.Assumptions
open FuchsiaMerkle.Spec

-- ============================================================================
-- 1. Basic Lemmas on `u32OfNat`, `u64OfNat`, `zeroHash`, `concatHashes`, and `chunksOf`
-- ============================================================================

@[simp]
theorem u32OfNat_val_of_lt (n : Nat) (h : n < 4294967296) : (u32OfNat n).val = n := by
  simp only [u32OfNat, UScalar.val]
  exact Nat.mod_eq_of_lt h

@[simp]
theorem u64OfNat_val_of_le (n : Nat) (h : n ≤ U64.max) : (u64OfNat n).val = n := by
  simp only [u64OfNat, UScalar.val]
  have hmax : U64.max = 18446744073709551615 := by simp [U64.max, U64.numBits]
  exact Nat.mod_eq_of_lt (by omega)

theorem zeroHash_val : zeroHash.val = List.replicate 32 0#u8 := by
  simp [zeroHash]

@[simp]
theorem specIdentity_length (length : Std.U32) (level : Std.U64) (offset : Std.U64) :
    (specIdentity length level offset).length = 12 := by
  simp [specIdentity]

@[simp]
theorem specZeroPad_length (n : Nat) : (specZeroPad n).length = n := by
  simp [specZeroPad]

theorem specZeroPad_add (a b : Nat) :
    specZeroPad (a + b) = specZeroPad a ++ specZeroPad b :=
  List.replicate_add a b 0#u8

@[simp]
theorem concatHashes_nil : concatHashes [] = [] := rfl

@[simp]
theorem concatHashes_cons (h : Hash) (hs : List Hash) :
    concatHashes (h :: hs) = h.val ++ concatHashes hs := rfl

@[simp]
theorem concatHashes_append (xs ys : List Hash) :
    concatHashes (xs ++ ys) = concatHashes xs ++ concatHashes ys := by
  induction xs with
  | nil => rfl
  | cons x xs ih => simp [ih, List.append_assoc]

@[simp]
theorem concatHashes_length (hs : List Hash) :
    (concatHashes hs).length = hs.length * 32 := by
  induction hs with
  | nil => simp
  | cons h hs ih =>
    have hh : h.val.length = 32 := h.property
    simp only [concatHashes_cons, List.length_append, List.length_cons, hh, ih]
    omega

theorem concatHashes_replicate_zeroHash (n : Nat) :
    concatHashes (List.replicate n zeroHash) = specZeroPad (n * 32) := by
  induction n with
  | zero => rfl
  | succ k ih =>
    rw [List.replicate_succ, concatHashes_cons, zeroHash_val, ih]
    have hmul : (k + 1) * 32 = 32 + k * 32 := by omega
    rw [hmul, specZeroPad_add]
    rfl

@[simp]
theorem chunksOf_nil {α : Type _} (k : Nat) (hk : 0 < k) :
    chunksOf k hk ([] : List α) = [] := by
  unfold chunksOf; rfl

theorem chunksOf_cons {α : Type _} (k : Nat) (hk : 0 < k) (x : α) (xs : List α) :
    chunksOf k hk (x :: xs) = (x :: xs).take k :: chunksOf k hk ((x :: xs).drop k) := by
  conv => lhs; unfold chunksOf

theorem chunksOf_of_nonempty {α : Type _} (k : Nat) (hk : 0 < k) (xs : List α) (hne : xs ≠ []) :
    chunksOf k hk xs = xs.take k :: chunksOf k hk (xs.drop k) := by
  match xs with
  | [] => contradiction
  | x :: rest => exact chunksOf_cons k hk x rest

theorem chunksOf_flatten {α : Type _} (k : Nat) (hk : 0 < k) :
    ∀ xs : List α, (chunksOf k hk xs).flatten = xs
  | [] => by simp
  | x :: xs => by
    rw [chunksOf_cons, List.flatten_cons, chunksOf_flatten k hk ((x :: xs).drop k),
        List.take_append_drop]
termination_by xs => xs.length
decreasing_by
  simp only [List.length_drop, List.length_cons]
  omega

theorem chunksOf_length {α : Type _} (k : Nat) (hk : 0 < k) :
    ∀ xs : List α, (chunksOf k hk xs).length = (xs.length + k - 1) / k
  | [] => by
    have : (k - 1) / k = 0 := Nat.div_eq_of_lt (by omega)
    simpa using this.symm
  | x :: xs => by
    rw [chunksOf_cons, List.length_cons, chunksOf_length k hk ((x :: xs).drop k)]
    simp only [List.length_drop, List.length_cons]
    by_cases hle : xs.length + 1 ≤ k
    · have hd : xs.length + 1 - k = 0 := by omega
      have h0 : (k - 1) / k = 0 := Nat.div_eq_of_lt (by omega)
      have h1 : (xs.length + k) / k = 1 := by
        rw [Nat.add_div_right _ hk, Nat.div_eq_of_lt (by omega)]
      have heq2 : xs.length + 1 + k - 1 = xs.length + k := by omega
      rw [hd, heq2]
      simp [h0, h1]
    · have heq : xs.length + 1 - k + k - 1 = xs.length := by omega
      have heq2 : xs.length + 1 + k - 1 = xs.length + k := by omega
      rw [heq, heq2, Nat.add_div_right _ hk]
termination_by xs => xs.length
decreasing_by
  simp only [List.length_drop, List.length_cons]
  omega

theorem chunksOf_append_exact {α : Type _} (k : Nat) (hk : 0 < k)
    (xs ys : List α) (hxs : xs.length = k) :
    chunksOf k hk (xs ++ ys) = xs :: chunksOf k hk ys := by
  have hne : xs ++ ys ≠ [] := by
    intro h
    have hlen := congrArg List.length h
    simp [hxs] at hlen
    omega
  rw [chunksOf_of_nonempty k hk (xs ++ ys) hne]
  have ht : (xs ++ ys).take k = xs := by
    rw [List.take_append_of_le_length (by omega), ← hxs, List.take_length]
  have hd : (xs ++ ys).drop k = ys := by
    rw [List.drop_append_of_le_length (by omega), ← hxs, List.drop_length, List.nil_append]
  rw [ht, hd]

theorem chunksOf_getElem {α : Type _} (k : Nat) (hk : 0 < k) :
    ∀ (xs : List α) (i : Nat) (hi : i < (chunksOf k hk xs).length),
      (chunksOf k hk xs)[i]'hi = (xs.drop (i * k)).take k
  | [], i, hi => by simp at hi
  | x :: xs, 0, hi => by simp [chunksOf_cons]
  | x :: xs, i + 1, hi => by
    have hi' : i < (chunksOf k hk ((x :: xs).drop k)).length := by
      rw [chunksOf_cons, List.length_cons] at hi; omega
    have hstep := chunksOf_getElem k hk ((x :: xs).drop k) i hi'
    have hcons : (chunksOf k hk (x :: xs))[i + 1]'hi =
                 (chunksOf k hk ((x :: xs).drop k))[i]'hi' := by
      simp [chunksOf_cons]
    rw [hcons, hstep, List.drop_drop]
    have : k + i * k = (i + 1) * k := by ring
    rw [this]
termination_by xs _ _ => xs.length
decreasing_by
  simp only [List.length_drop, List.length_cons]
  omega

theorem specLevelStep_length (sha256 : List Std.U8 → Hash) (level : Nat) (nodes : List Hash) :
    (specLevelStep sha256 level nodes).length = (nodes.length + 255) / 256 := by
  simp [specLevelStep, chunksOf_length, CHILDREN_PER_NODE]

@[simp]
theorem specReduceLevels_nil (sha256 : List Std.U8 → Hash) (level : Nat) :
    specReduceLevels sha256 level [] = specHashBlock sha256 [] 0#u64 := by
  unfold specReduceLevels; rfl

@[simp]
theorem specReduceLevels_singleton (sha256 : List Std.U8 → Hash) (level : Nat) (h : Hash) :
    specReduceLevels sha256 level [h] = h := by
  unfold specReduceLevels; rfl

theorem specReduceLevels_two_or_more (sha256 : List Std.U8 → Hash) (level : Nat)
    (a b : Hash) (rest : List Hash) :
    specReduceLevels sha256 level (a :: b :: rest) =
      specReduceLevels sha256 (level + 1) (specLevelStep sha256 level (a :: b :: rest)) := by
  conv => lhs; unfold specReduceLevels

-- ============================================================================
-- 2. Bit-Level Header Injectivity & Domain Separation (`bv_decide`)
-- ============================================================================

/-- Bit-level disjointness proved by `bv_decide`: when `offset` is 8,192-byte aligned
    (`low 13 bits = 0`) and `level < 8192` (`fits in low 13 bits`), `offset ||| level`
    uniquely encodes both `offset` and `level`. -/
theorem offset_or_level_injective_bv
    (off₁ lvl₁ off₂ lvl₂ : BitVec 64)
    (hoff₁ : off₁ &&& 8191#64 = 0#64)
    (hoff₂ : off₂ &&& 8191#64 = 0#64)
    (hlvl₁ : lvl₁ < 8192#64)
    (hlvl₂ : lvl₂ < 8192#64)
    (heq : (off₁ ||| lvl₁) = (off₂ ||| lvl₂)) :
    off₁ = off₂ ∧ lvl₁ = lvl₂ := by
  bv_decide

theorem offset_aligned_and_mask (off : Std.U64) (h : off.val % 8192 = 0) :
    off.bv &&& 8191#64 = 0#64 := by
  have hmod : off.bv % 8192#64 = 0#64 := by
    apply BitVec.eq_of_toNat_eq
    rw [BitVec.toNat_umod]
    exact h
  have hand : off.bv &&& (2#64 ^ 13 - 1#64) = off.bv % 2#64 ^ 13 :=
    BitVec.and_two_pow_sub_one_eq_mod off.bv 13
  have hc1 : (2#64 ^ 13 - 1#64 : BitVec 64) = 8191#64 := by decide
  have hc2 : (2#64 ^ 13 : BitVec 64) = 8192#64 := by decide
  rw [hc1, hc2] at hand
  rw [hand, hmod]

theorem map_uscalar_mk_u8_injective {xs ys : List Byte}
    (h : xs.map (@UScalar.mk UScalarTy.U8) = ys.map (@UScalar.mk UScalarTy.U8)) :
    xs = ys := by
  induction xs generalizing ys with
  | nil => cases ys <;> simp_all
  | cons x xs ih =>
    cases ys with
    | nil => simp at h
    | cons y ys =>
      simp only [List.map_cons, List.cons.injEq] at h
      obtain ⟨hxy, hrest⟩ := h
      cases hxy
      exact congrArg (x :: ·) (ih hrest)

theorem toLEBytes_injective {w : Nat} (hw : w % 8 = 0) {a b : BitVec w}
    (h : a.toLEBytes = b.toLEBytes) : a = b := by
  have ha := congrArg BitVec.toNat (BitVec.fromLEBytes_toLEBytes hw a)
  have hb := congrArg BitVec.toNat (BitVec.fromLEBytes_toLEBytes hw b)
  simp only [BitVec.toNat_cast] at ha hb
  apply BitVec.eq_of_toNat_eq
  rw [← ha, ← hb, h]

/-- `specIdentity` is strictly injective whenever `offset` is 8,192-byte aligned and `level < 8192`. -/
theorem specIdentity_injective
    {len₁ len₂ : Std.U32} {lvl₁ lvl₂ off₁ off₂ : Std.U64}
    (hoff₁ : off₁.val % 8192 = 0)
    (hoff₂ : off₂.val % 8192 = 0)
    (hlvl₁ : lvl₁.val < 8192)
    (hlvl₂ : lvl₂.val < 8192)
    (heq : specIdentity len₁ lvl₁ off₁ = specIdentity len₂ lvl₂ off₂) :
    len₁ = len₂ ∧ lvl₁ = lvl₂ ∧ off₁ = off₂ := by
  unfold specIdentity at heq
  have hl1 : ((off₁ ||| lvl₁).bv.toLEBytes.map (@UScalar.mk UScalarTy.U8)).length =
             ((off₂ ||| lvl₂).bv.toLEBytes.map (@UScalar.mk UScalarTy.U8)).length := by
    simp
  obtain ⟨hleft, hright⟩ := List.append_inj heq hl1
  have hbv64 : (off₁ ||| lvl₁).bv = (off₂ ||| lvl₂).bv :=
    toLEBytes_injective (by decide) (map_uscalar_mk_u8_injective hleft)
  have hbv32 : len₁.bv = len₂.bv :=
    toLEBytes_injective (by decide) (map_uscalar_mk_u8_injective hright)
  have hlen : len₁ = len₂ := by cases len₁; cases len₂; simp_all
  have hl1_bv : lvl₁.bv < 8192#64 := by
    rw [BitVec.lt_def]; exact hlvl₁
  have hl2_bv : lvl₂.bv < 8192#64 := by
    rw [BitVec.lt_def]; exact hlvl₂
  obtain ⟨hoff_bv, hlvl_bv⟩ := offset_or_level_injective_bv
    off₁.bv lvl₁.bv off₂.bv lvl₂.bv
    (offset_aligned_and_mask off₁ hoff₁)
    (offset_aligned_and_mask off₂ hoff₂)
    hl1_bv hl2_bv hbv64
  refine ⟨hlen, ?_, ?_⟩
  · cases lvl₁; cases lvl₂; simp_all
  · cases off₁; cases off₂; simp_all

/-- Aeneas-extracted `make_identity` always succeeds (`ok`) and matches `specIdentity` byte-for-byte. -/
theorem make_identity_spec (length : Std.U32) (level : Std.U64) (offset : Std.U64) :
    ∃ arr, make_identity length level offset = ok arr ∧
      arr.val = specIdentity length level offset := by
  unfold make_identity
  set a := core.num.U64.to_le_bytes (offset ||| level)
  set a1 := core.num.U32.to_le_bytes length
  have ha_val : a.val = (offset ||| level).bv.toLEBytes.map (@UScalar.mk UScalarTy.U8) := by
    simp [a, core.num.U64.to_le_bytes]
  have ha1_val : a1.val = length.bv.toLEBytes.map (@UScalar.mk UScalarTy.U8) := by
    simp [a1, core.num.U32.to_le_bytes]
  have ha_len : a.val.length = 8 := by simp [ha_val]
  have ha1_len : a1.val.length = 4 := by simp [ha1_val]
  rcases h64 : a.val with _ | ⟨o0, _ | ⟨o1, _ | ⟨o2, _ | ⟨o3, _ | ⟨o4, _ | ⟨o5, _ | ⟨o6, _ | ⟨o7, rest64⟩⟩⟩⟩⟩⟩⟩⟩
  all_goals try (simp [h64] at ha_len)
  subst ha_len
  rcases h32 : a1.val with _ | ⟨l0, _ | ⟨l1, _ | ⟨l2, _ | ⟨l3, rest32⟩⟩⟩⟩
  all_goals try (simp [h32] at ha1_len)
  subst ha1_len
  have idx_a0 : a.index_usize 0#usize = ok o0 := by simp [Aeneas.Std.Array.index_usize, h64]
  have idx_a1 : a.index_usize 1#usize = ok o1 := by simp [Aeneas.Std.Array.index_usize, h64]
  have idx_a2 : a.index_usize 2#usize = ok o2 := by simp [Aeneas.Std.Array.index_usize, h64]
  have idx_a3 : a.index_usize 3#usize = ok o3 := by simp [Aeneas.Std.Array.index_usize, h64]
  have idx_a4 : a.index_usize 4#usize = ok o4 := by simp [Aeneas.Std.Array.index_usize, h64]
  have idx_a5 : a.index_usize 5#usize = ok o5 := by simp [Aeneas.Std.Array.index_usize, h64]
  have idx_a6 : a.index_usize 6#usize = ok o6 := by simp [Aeneas.Std.Array.index_usize, h64]
  have idx_a7 : a.index_usize 7#usize = ok o7 := by simp [Aeneas.Std.Array.index_usize, h64]
  have idx_b0 : a1.index_usize 0#usize = ok l0 := by simp [Aeneas.Std.Array.index_usize, h32]
  have idx_b1 : a1.index_usize 1#usize = ok l1 := by simp [Aeneas.Std.Array.index_usize, h32]
  have idx_b2 : a1.index_usize 2#usize = ok l2 := by simp [Aeneas.Std.Array.index_usize, h32]
  have idx_b3 : a1.index_usize 3#usize = ok l3 := by simp [Aeneas.Std.Array.index_usize, h32]
  refine ⟨Aeneas.Std.Array.make 12#usize [o0, o1, o2, o3, o4, o5, o6, o7, l0, l1, l2, l3], ?_, ?_⟩
  · simp [lift, a, a1, idx_a0, idx_a1, idx_a2, idx_a3, idx_a4, idx_a5, idx_a6, idx_a7,
          idx_b0, idx_b1, idx_b2, idx_b3]
  · change [o0, o1, o2, o3, o4, o5, o6, o7, l0, l1, l2, l3] = specIdentity length level offset
    have hcat : [o0, o1, o2, o3, o4, o5, o6, o7, l0, l1, l2, l3] =
        [o0, o1, o2, o3, o4, o5, o6, o7] ++ [l0, l1, l2, l3] := rfl
    rw [hcat, ← h64, ← h32, ha_val, ha1_val]
    rfl

/-- Extracted `make_identity` inherits full bit-level injectivity on valid Merkle coordinates. -/
theorem make_identity_injective_lem
    {len₁ len₂ : Std.U32} {lvl₁ lvl₂ off₁ off₂ : Std.U64}
    {id₁ id₂ : Aeneas.Std.Array Std.U8 12#usize}
    (hoff₁ : off₁.val % 8192 = 0)
    (hoff₂ : off₂.val % 8192 = 0)
    (hlvl₁ : lvl₁.val < 8192)
    (hlvl₂ : lvl₂.val < 8192)
    (h₁ : make_identity len₁ lvl₁ off₁ = ok id₁)
    (h₂ : make_identity len₂ lvl₂ off₂ = ok id₂)
    (heq : id₁ = id₂) :
    len₁ = len₂ ∧ lvl₁ = lvl₂ ∧ off₁ = off₂ := by
  obtain ⟨a₁, ha₁, hv₁⟩ := make_identity_spec len₁ lvl₁ off₁
  obtain ⟨a₂, ha₂, hv₂⟩ := make_identity_spec len₂ lvl₂ off₂
  have eq₁ : a₁ = id₁ := Result.ok_injective (ha₁.symm.trans h₁)
  have eq₂ : a₂ = id₂ := Result.ok_injective (ha₂.symm.trans h₂)
  subst eq₁; subst eq₂; subst heq
  exact specIdentity_injective hoff₁ hoff₂ hlvl₁ hlvl₂ (hv₁.symm.trans hv₂)

/-- Domain separation corollary: a level-0 leaf block preimage never equals an internal node
    preimage (`1 ≤ level < 8192`), regardless of payload bytes, lengths, or aligned offsets. -/
theorem leaf_internal_domain_separation_lem
    (len₁ len₂ : Std.U32) (lvl : Std.U64) (off₁ off₂ : Std.U64)
    (rest₁ rest₂ : List Std.U8)
    (hoff₁ : off₁.val % 8192 = 0)
    (hoff₂ : off₂.val % 8192 = 0)
    (hlvl_pos : 0 < lvl.val)
    (hlvl_lt : lvl.val < 8192) :
    specIdentity len₁ 0#u64 off₁ ++ rest₁ ≠ specIdentity len₂ lvl off₂ ++ rest₂ := by
  intro heq
  have h12 : (specIdentity len₁ 0#u64 off₁).length = (specIdentity len₂ lvl off₂).length := by
    simp
  obtain ⟨hid, _⟩ := List.append_inj heq h12
  obtain ⟨_, hlvl_eq, _⟩ := specIdentity_injective hoff₁ hoff₂ (by simp) hlvl_lt hid
  have : (0#u64).val = lvl.val := congrArg UScalar.val hlvl_eq
  simp at this
  omega

-- ============================================================================
-- 3. Preimage Injectivity & Collision Reduction for Leaves, Nodes, and Roots
-- ============================================================================

/-- Two canonical leaf blocks (`len ≤ 8192`) hash to the same digest only if both the byte slice
    and the 64-bit byte offset are identical, or `HashCollision sha256`. -/
theorem specHashBlock_eq_imp_eq_or_collision
    {sha256 : List Std.U8 → Hash}
    {b₁ b₂ : List Std.U8} {off₁ off₂ : Std.U64}
    (hb₁ : b₁.length ≤ BLOCK_BYTES)
    (hb₂ : b₂.length ≤ BLOCK_BYTES)
    (hoff₁ : off₁.val % 8192 = 0)
    (hoff₂ : off₂.val % 8192 = 0)
    (heq : specHashBlock sha256 b₁ off₁ = specHashBlock sha256 b₂ off₂) :
    (b₁ = b₂ ∧ off₁ = off₂) ∨ HashCollision sha256 := by
  unfold specHashBlock at heq
  dsimp only at heq
  have hlen₁_val : (u32OfNat b₁.length).val = b₁.length :=
    u32OfNat_val_of_lt b₁.length (by unfold BLOCK_BYTES at hb₁; omega)
  have hlen₂_val : (u32OfNat b₂.length).val = b₂.length :=
    u32OfNat_val_of_lt b₂.length (by unfold BLOCK_BYTES at hb₂; omega)
  set p₁ := if b₁.length = 0 ∧ off₁ = 0#u64 then specIdentity (u32OfNat b₁.length) 0#u64 off₁
            else specIdentity (u32OfNat b₁.length) 0#u64 off₁ ++ b₁ ++ specZeroPad (BLOCK_BYTES - b₁.length)
  set p₂ := if b₂.length = 0 ∧ off₂ = 0#u64 then specIdentity (u32OfNat b₂.length) 0#u64 off₂
            else specIdentity (u32OfNat b₂.length) 0#u64 off₂ ++ b₂ ++ specZeroPad (BLOCK_BYTES - b₂.length)
  by_cases hp : p₁ = p₂
  · left
    by_cases hc₁ : b₁.length = 0 ∧ off₁ = 0#u64 <;> by_cases hc₂ : b₂.length = 0 ∧ off₂ = 0#u64
    · rcases hc₁ with ⟨hl1, ho1⟩; rcases hc₂ with ⟨hl2, ho2⟩
      exact ⟨by cases b₁ <;> cases b₂ <;> simp_all, ho1.trans ho2.symm⟩
    · exfalso
      have hp1 : p₁.length = 12 := by simp only [p₁, hc₁]; simp
      have hp2 : p₂.length = 12 + b₂.length + (8192 - b₂.length) := by
        simp only [p₂, hc₂, ↓reduceIte]; simp [BLOCK_BYTES]; omega
      have hlen_eq : p₁.length = p₂.length := congrArg List.length hp
      unfold BLOCK_BYTES at hb₂
      omega
    · exfalso
      have hp1 : p₁.length = 12 + b₁.length + (8192 - b₁.length) := by
        simp only [p₁, hc₁, ↓reduceIte]; simp [BLOCK_BYTES]; omega
      have hp2 : p₂.length = 12 := by simp only [p₂, hc₂]; simp
      have hlen_eq : p₁.length = p₂.length := congrArg List.length hp
      unfold BLOCK_BYTES at hb₁
      omega
    · simp only [p₁, p₂, hc₁, hc₂, ↓reduceIte, List.append_assoc] at hp
      obtain ⟨hid, hbody⟩ := List.append_inj hp (by simp)
      obtain ⟨hlen_eq, _, hoff_eq⟩ := specIdentity_injective hoff₁ hoff₂ (by simp) (by simp) hid
      have hblen : b₁.length = b₂.length := by
        rw [← hlen₁_val, ← hlen₂_val, hlen_eq]
      obtain ⟨hb_eq, _⟩ := List.append_inj hbody hblen
      exact ⟨hb_eq, hoff_eq⟩
  · right
    exact ⟨p₁, p₂, hp, heq⟩

/-- `concatHashes` is injective on hash lists of equal length (since every `Hash` is 32 bytes). -/
theorem concatHashes_injective_of_length_eq :
    ∀ {xs ys : List Hash}, xs.length = ys.length → concatHashes xs = concatHashes ys → xs = ys
  | [], [], _, _ => rfl
  | x :: xs, y :: ys, hlen, hcat => by
    simp only [List.length_cons, Nat.add_right_cancel_iff] at hlen
    simp only [concatHashes_cons] at hcat
    have hxlen : x.val.length = y.val.length := by simp
    obtain ⟨hxy_val, hrest⟩ := List.append_inj hcat hxlen
    have hxy : x = y := Aeneas.Std.Array.ext x y hxy_val
    exact congrArg₂ List.cons hxy (concatHashes_injective_of_length_eq hlen hrest)

/-- Two internal nodes with the same `hashesPerNode` and `bytesPerNode` produce equal digests only
    if either `HashCollision sha256` holds, or their headers match and their zero-hash-padded child
    vectors are identical. -/
theorem specHashInternal_eq_imp_padded_eq_or_collision
    {sha256 : List Std.U8 → Hash}
    {hashesPerNode : Nat} {bytesPerNode : Std.U32}
    {lvl₁ lvl₂ off₁ off₂ : Std.U64}
    {c₁ c₂ : List Hash}
    (hc₁ : c₁.length ≤ hashesPerNode)
    (hc₂ : c₂.length ≤ hashesPerNode)
    (hoff₁ : off₁.val % 8192 = 0)
    (hoff₂ : off₂.val % 8192 = 0)
    (hlvl₁ : lvl₁.val < 8192)
    (hlvl₂ : lvl₂.val < 8192)
    (heq : specHashInternal sha256 hashesPerNode bytesPerNode lvl₁ off₁ c₁ =
           specHashInternal sha256 hashesPerNode bytesPerNode lvl₂ off₂ c₂) :
    (lvl₁ = lvl₂ ∧ off₁ = off₂ ∧
      c₁ ++ List.replicate (hashesPerNode - c₁.length) zeroHash =
      c₂ ++ List.replicate (hashesPerNode - c₂.length) zeroHash) ∨
    HashCollision sha256 := by
  unfold specHashInternal specInternalPreimage at heq
  set p₁ := specIdentity bytesPerNode lvl₁ off₁ ++ concatHashes c₁ ++
            specZeroPad ((hashesPerNode - c₁.length) * 32)
  set p₂ := specIdentity bytesPerNode lvl₂ off₂ ++ concatHashes c₂ ++
            specZeroPad ((hashesPerNode - c₂.length) * 32)
  by_cases hp : p₁ = p₂
  · left
    simp only [p₁, p₂, List.append_assoc] at hp
    obtain ⟨hid, hbody⟩ := List.append_inj hp (by simp)
    obtain ⟨_, hlvl_eq, hoff_eq⟩ := specIdentity_injective hoff₁ hoff₂ hlvl₁ hlvl₂ hid
    rw [← concatHashes_replicate_zeroHash, ← concatHashes_replicate_zeroHash,
        ← concatHashes_append, ← concatHashes_append] at hbody
    have hpad_len :
        (c₁ ++ List.replicate (hashesPerNode - c₁.length) zeroHash).length =
        (c₂ ++ List.replicate (hashesPerNode - c₂.length) zeroHash).length := by
      simp; omega
    exact ⟨hlvl_eq, hoff_eq, concatHashes_injective_of_length_eq hpad_len hbody⟩
  · right
    exact ⟨p₁, p₂, hp, heq⟩

/-- When `c₁` and `c₂` also have the same length (`c₁.length = c₂.length ≤ hashesPerNode`),
    `specHashInternal` equality implies `c₁ = c₂` (and equal coordinates) or a SHA-256 collision. -/
theorem specHashInternal_eq_imp_eq_or_collision
    {sha256 : List Std.U8 → Hash}
    {hashesPerNode : Nat} {bytesPerNode : Std.U32}
    {lvl₁ lvl₂ off₁ off₂ : Std.U64}
    {c₁ c₂ : List Hash}
    (hc₁ : c₁.length ≤ hashesPerNode)
    (hlen : c₁.length = c₂.length)
    (hoff₁ : off₁.val % 8192 = 0)
    (hoff₂ : off₂.val % 8192 = 0)
    (hlvl₁ : lvl₁.val < 8192)
    (hlvl₂ : lvl₂.val < 8192)
    (heq : specHashInternal sha256 hashesPerNode bytesPerNode lvl₁ off₁ c₁ =
           specHashInternal sha256 hashesPerNode bytesPerNode lvl₂ off₂ c₂) :
    (lvl₁ = lvl₂ ∧ off₁ = off₂ ∧ c₁ = c₂) ∨ HashCollision sha256 := by
  rcases specHashInternal_eq_imp_padded_eq_or_collision
    hc₁ (hlen ▸ hc₁) hoff₁ hoff₂ hlvl₁ hlvl₂ heq with ⟨hl, ho, hpad⟩ | hcol
  · left
    obtain ⟨hceq, _⟩ := List.append_inj hpad hlen
    exact ⟨hl, ho, hceq⟩
  · exact Or.inr hcol

theorem chunksOf_eq_imp_eq {α : Type _} (k : Nat) (hk : 0 < k) {xs ys : List α}
    (h : chunksOf k hk xs = chunksOf k hk ys) : xs = ys := by
  rw [← chunksOf_flatten k hk xs, ← chunksOf_flatten k hk ys, h]

theorem chunksOf_elem_length_le {α : Type _} (k : Nat) (hk : 0 < k)
    (xs : List α) (i : Nat) (hi : i < (chunksOf k hk xs).length) :
    ((chunksOf k hk xs)[i]'hi).length ≤ k := by
  rw [chunksOf_getElem k hk xs i hi]
  exact List.length_take_le k _

theorem chunksOf_same_len_elem_length_eq {α : Type _} (k : Nat) (hk : 0 < k)
    {xs ys : List α} (hlen : xs.length = ys.length)
    (i : Nat)
    (hix : i < (chunksOf k hk xs).length)
    (hiy : i < (chunksOf k hk ys).length) :
    ((chunksOf k hk xs)[i]'hix).length = ((chunksOf k hk ys)[i]'hiy).length := by
  rw [chunksOf_getElem k hk xs i hix, chunksOf_getElem k hk ys i hiy]
  simp [hlen]

/-- Single-level injectivity: if two node lists of equal length (`(nodes₁.length / 256) * 8192 ≤ U64.max`)
    reduce to the same parent list under `specLevelStep`, then `nodes₁ = nodes₂` or `HashCollision sha256`. -/
theorem specLevelStep_eq_imp_eq_or_collision
    {sha256 : List Std.U8 → Hash} {level : Nat} {nodes₁ nodes₂ : List Hash}
    (hlvl : level < 8192)
    (hlen : nodes₁.length = nodes₂.length)
    (hbound : (nodes₁.length / 256) * 8192 ≤ U64.max)
    (heq : specLevelStep sha256 level nodes₁ = specLevelStep sha256 level nodes₂) :
    nodes₁ = nodes₂ ∨ HashCollision sha256 := by
  have hclen :
      (chunksOf CHILDREN_PER_NODE (by decide) nodes₁).length =
      (chunksOf CHILDREN_PER_NODE (by decide) nodes₂).length := by
    simp [chunksOf_length, hlen]
  by_cases hchunks :
      chunksOf CHILDREN_PER_NODE (by decide) nodes₁ =
      chunksOf CHILDREN_PER_NODE (by decide) nodes₂
  · exact Or.inl (chunksOf_eq_imp_eq CHILDREN_PER_NODE (by decide) hchunks)
  · right
    have hex : ∃ i, ∃ (h1 : i < (chunksOf CHILDREN_PER_NODE (by decide) nodes₁).length)
                      (h2 : i < (chunksOf CHILDREN_PER_NODE (by decide) nodes₂).length),
        (chunksOf CHILDREN_PER_NODE (by decide) nodes₁)[i]'h1 ≠
        (chunksOf CHILDREN_PER_NODE (by decide) nodes₂)[i]'h2 := by
      by_contra hall
      apply hchunks
      apply List.ext_getElem hclen
      intro i h1 h2
      by_contra hne
      exact hall ⟨i, h1, h2, hne⟩
    obtain ⟨i, hi1, hi2, hneq⟩ := hex
    have hget := congrArg (fun l => l[i]?) heq
    simp only [specLevelStep, List.getElem?_mapIdx,
      List.getElem?_eq_getElem hi1, List.getElem?_eq_getElem hi2,
      Option.map_some, Option.some.injEq] at hget
    have hi_bound : i * BLOCK_BYTES ≤ U64.max := by
      rw [chunksOf_length] at hi1
      have : i ≤ nodes₁.length / 256 := by
        simp [CHILDREN_PER_NODE] at hi1; omega
      have : i * 8192 ≤ (nodes₁.length / 256) * 8192 := Nat.mul_le_mul_right 8192 this
      simpa [BLOCK_BYTES] using le_trans this hbound
    have hoff : (u64OfNat (i * BLOCK_BYTES)).val % 8192 = 0 := by
      rw [u64OfNat_val_of_le _ hi_bound]
      simp [BLOCK_BYTES]
    have hlvl_u64 : (u64OfNat level).val < 8192 := by
      rw [u64OfNat_val_of_le level (by simp [U64.max, U64.numBits]; omega)]
      exact hlvl
    have hc1_le := chunksOf_elem_length_le CHILDREN_PER_NODE (by decide) nodes₁ i hi1
    have hcelem_len := chunksOf_same_len_elem_length_eq CHILDREN_PER_NODE (by decide) hlen i hi1 hi2
    rcases specHashInternal_eq_imp_eq_or_collision
      hc1_le hcelem_len hoff hoff hlvl_u64 hlvl_u64 hget with ⟨_, _, hceq⟩ | hcol
    · exact absurd hceq hneq
    · exact hcol

/-- Multi-level root soundness parameterized by tree depth bound `d` (`nodes₁.length ≤ 256 ^ d`):
    if two node lists of equal length produce the same digest under `specReduceLevels`, then
    `nodes₁ = nodes₂` or `HashCollision sha256`. -/
theorem specReduceLevels_eq_imp_eq_or_collision
    {sha256 : List Std.U8 → Hash} :
    ∀ (d level : Nat) (nodes₁ nodes₂ : List Hash),
      nodes₁.length ≤ 256 ^ d →
      level + d < 8192 →
      nodes₁.length = nodes₂.length →
      (nodes₁.length / 256) * 8192 ≤ U64.max →
      specReduceLevels sha256 level nodes₁ = specReduceLevels sha256 level nodes₂ →
      nodes₁ = nodes₂ ∨ HashCollision sha256
  | _, _, [], [], _, _, _, _, _ => Or.inl rfl
  | _, _, [_], [], _, _, hlen, _, _ => by simp at hlen
  | _, _, [], [_], _, _, hlen, _, _ => by simp at hlen
  | _, _, [a], [b], _, _, _, _, heq => by
    simp only [specReduceLevels_singleton] at heq
    exact Or.inl (by subst heq; rfl)
  | _, _, _ :: _ :: _, [], _, _, hlen, _, _ => by simp at hlen
  | _, _, _ :: _ :: _, [_], _, _, hlen, _, _ => by simp at hlen
  | _, _, [], _ :: _ :: _, _, _, hlen, _, _ => by simp at hlen
  | _, _, [_], _ :: _ :: _, _, _, hlen, _, _ => by simp at hlen
  | 0, _, _ :: _ :: _, _ :: _ :: _, hd, _, _, _, _ => by
    simp only [Nat.pow_zero, List.length_cons] at hd
    omega
  | d + 1, level, a₁ :: b₁ :: rest₁, a₂ :: b₂ :: rest₂, hd, hlvl, hlen, hbound, heq => by
    rw [specReduceLevels_two_or_more, specReduceLevels_two_or_more] at heq
    set ns₁ := a₁ :: b₁ :: rest₁
    set ns₂ := a₂ :: b₂ :: rest₂
    have hlen_step :
        (specLevelStep sha256 level ns₁).length = (specLevelStep sha256 level ns₂).length := by
      simp [specLevelStep_length, hlen]
    have hd_step : (specLevelStep sha256 level ns₁).length ≤ 256 ^ d := by
      rw [specLevelStep_length]
      have hpow : 256 ^ (d + 1) = 256 ^ d * 256 := Nat.pow_succ 256 d
      omega
    have hlvl_step : (level + 1) + d < 8192 := by omega
    have hbound_step : ((specLevelStep sha256 level ns₁).length / 256) * 8192 ≤ U64.max := by
      rw [specLevelStep_length]
      have : ((ns₁.length + 255) / 256) / 256 ≤ ns₁.length / 256 := by omega
      exact le_trans (Nat.mul_le_mul_right 8192 this) hbound
    rcases specReduceLevels_eq_imp_eq_or_collision d (level + 1)
        (specLevelStep sha256 level ns₁) (specLevelStep sha256 level ns₂)
        hd_step hlvl_step hlen_step hbound_step heq with hparents_eq | hcol
    · exact specLevelStep_eq_imp_eq_or_collision (by omega) hlen hbound hparents_eq
    · exact Or.inr hcol

/-- Top-level tree root collision soundness: two leaf digest vectors of equal canonical length
    (`leaves₁.length * 8192 ≤ U64.max`) yield equal roots only if `leaves₁ = leaves₂` or
    `HashCollision sha256`. -/
theorem specRootFromLeaves_eq_imp_eq_or_collision_lem
    {sha256 : List Std.U8 → Hash}
    {leaves₁ leaves₂ : List Hash}
    (hlen : leaves₁.length = leaves₂.length)
    (hbound : leaves₁.length * 8192 ≤ U64.max)
    (heq : specRootFromLeaves sha256 leaves₁ = specRootFromLeaves sha256 leaves₂) :
    leaves₁ = leaves₂ ∨ HashCollision sha256 := by
  have hd8 : leaves₁.length ≤ 256 ^ 8 := by
    have hmax : U64.max = 18446744073709551615 := by simp [U64.max, U64.numBits]
    have hp : 256 ^ 8 = 18446744073709551616 := by decide
    omega
  have hdiv : (leaves₁.length / 256) * 8192 ≤ U64.max := by
    have : (leaves₁.length / 256) * 8192 ≤ leaves₁.length * 8192 :=
      Nat.mul_le_mul_right 8192 (Nat.div_le_self _ _)
    exact le_trans this hbound
  exact specReduceLevels_eq_imp_eq_or_collision 8 1 leaves₁ leaves₂ hd8 (by omega) hlen hdiv heq

-- ============================================================================
-- 4. Zero-Hash Leaf Extension Characterization
-- ============================================================================

theorem specInternalPreimage_append_zeroHash
    {hashesPerNode : Nat} (bytesPerNode : Std.U32) (level levelOffset : Std.U64)
    (children : List Hash) (hcap : children.length < hashesPerNode) :
    specInternalPreimage hashesPerNode bytesPerNode level levelOffset (children ++ [zeroHash]) =
    specInternalPreimage hashesPerNode bytesPerNode level levelOffset children := by
  unfold specInternalPreimage
  rw [concatHashes_append, concatHashes_cons, concatHashes_nil, List.append_nil,
      zeroHash_val, List.length_append, List.length_singleton]
  simp only [List.append_assoc]
  congr 2
  have hsub : hashesPerNode - children.length = 1 + (hashesPerNode - (children.length + 1)) := by
    omega
  rw [hsub, Nat.add_mul, Nat.one_mul, specZeroPad_add]
  rfl

/-- Appending `zeroHash` to any non-empty node list whose length is not a multiple of `256`
    produces the exact same parent list under `specLevelStep` with zero SHA-256 collisions. -/
theorem specLevelStep_append_zeroHash
    (sha256 : List Std.U8 → Hash) (level : Nat) (nodes : List Hash)
    (hne : nodes ≠ []) (hrem : nodes.length % 256 ≠ 0) :
    specLevelStep sha256 level (nodes ++ [zeroHash]) = specLevelStep sha256 level nodes := by
  have hclen :
      (chunksOf CHILDREN_PER_NODE (by decide) (nodes ++ [zeroHash])).length =
      (chunksOf CHILDREN_PER_NODE (by decide) nodes).length := by
    simp only [chunksOf_length, List.length_append, List.length_singleton, CHILDREN_PER_NODE]
    omega
  unfold specLevelStep
  apply List.ext_getElem
  · simp [hclen]
  · intro i hi1 hi2
    simp only [List.length_mapIdx] at hi1 hi2
    simp only [List.getElem_mapIdx]
    rw [chunksOf_getElem CHILDREN_PER_NODE (by decide) (nodes ++ [zeroHash]) i hi1,
        chunksOf_getElem CHILDREN_PER_NODE (by decide) nodes i hi2]
    rw [chunksOf_length] at hi2
    simp only [CHILDREN_PER_NODE] at hi2 ⊢
    by_cases hlast : i + 1 < (nodes.length + 255) / 256
    · have hle : i * 256 + 256 ≤ nodes.length := by omega
      have hdrop : ((nodes ++ [zeroHash]).drop (i * 256)).take 256 =
                   (nodes.drop (i * 256)).take 256 := by
        rw [List.drop_append_of_le_length (by omega)]
        have hlen_drop : 256 ≤ (nodes.drop (i * 256)).length := by simp; omega
        rw [List.take_append_of_le_length hlen_drop]
      rw [hdrop]
    · have hpos : 0 < nodes.length := List.length_pos_iff.mpr hne
      have hi_eq : i = (nodes.length - 1) / 256 := by omega
      have hlt_len : (nodes.drop (i * 256)).length < 256 := by
        simp [hi_eq]; omega
      have hdrop1 : ((nodes ++ [zeroHash]).drop (i * 256)).take 256 =
                    nodes.drop (i * 256) ++ [zeroHash] := by
        have hle_drop : i * 256 ≤ nodes.length := by
          subst hi_eq; exact le_trans (Nat.div_mul_le_self _ _) (by omega)
        rw [List.drop_append_of_le_length hle_drop]
        apply List.take_of_length_le
        simp; omega
      have hdrop2 : (nodes.drop (i * 256)).take 256 = nodes.drop (i * 256) :=
        List.take_of_length_le (by omega)
      rw [hdrop1, hdrop2]
      unfold specHashInternal
      rw [specInternalPreimage_append_zeroHash _ _ _ _ hlt_len]

/-- **Property P5 (Zero-Hash Leaf Extension Characterization)**:
    Whenever `2 ≤ leaves.length` and `leaves.length % 256 ≠ 0`, `leaves ++ [zeroHash]` has the
    identical Fuchsia Merkle root as `leaves` for every hash function `sha256`. -/
theorem specRootFromLeaves_append_zeroHash_lem
    (sha256 : List Std.U8 → Hash) (leaves : List Hash)
    (hge2 : 2 ≤ leaves.length) (hrem : leaves.length % 256 ≠ 0) :
    specRootFromLeaves sha256 (leaves ++ [zeroHash]) = specRootFromLeaves sha256 leaves := by
  rcases leaves with _ | ⟨a, _ | ⟨b, rest⟩⟩
  · simp at hge2
  · simp at hge2
  · unfold specRootFromLeaves
    simp only [List.cons_append]
    rw [specReduceLevels_two_or_more, specReduceLevels_two_or_more]
    have hne : a :: b :: rest ≠ [] := by simp
    have heq := specLevelStep_append_zeroHash sha256 1 (a :: b :: rest) hne hrem
    simpa [List.cons_append] using congrArg (specReduceLevels sha256 2) heq

end FuchsiaMerkle
