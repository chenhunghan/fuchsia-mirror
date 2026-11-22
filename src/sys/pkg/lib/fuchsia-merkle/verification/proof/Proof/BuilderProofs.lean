-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.Sha256Sim

set_option maxRecDepth 8192
set_option maxHeartbeats 800000

/-!
# Polymorphic Functional Correctness of `MerkleRootBuilder`, `BufferedMerkleRootBuilder`, and `rebuild_root`

Machine-checked functional specifications over arbitrary `ValidSha256State` and
`ValidLeafCollector` implementations for:
1. `rebuild_root_gen_spec`: bottom-up root reconstruction from leaf hashes via `NoopLeafCollector`.
2. `merkle_root_builder_complete_gen_spec`: single-pass Merkle root and leaf collection.
3. `buffered_streaming_gen_spec`: arbitrary-chunk streaming via `BufferedMerkleRootBuilder`.
4. `buffered_chunk_invariance_lem`: chunk-partition invariance for `(StreamSha256, ListLeafCollector)`.
-/

namespace FuchsiaMerkle

open Aeneas Aeneas.Std Result
open fuchsia_merkle_core hiding Hash
open FuchsiaMerkle.Assumptions
open FuchsiaMerkle.Spec

-- ============================================================================
-- 1. Pure Multi-Level Streaming Decomposition (`fullParents`, `streamedAt`, `iterLevelStep`)
-- ============================================================================

private theorem chunksOf_append_mul {α : Type _} (k : Nat) (hk : 0 < k) :
    ∀ (q : Nat) (xs ys : List α),
      xs.length = q * k →
      chunksOf k hk (xs ++ ys) = chunksOf k hk xs ++ chunksOf k hk ys
  | 0, xs, ys, hxs => by
    have hnil : xs = [] := List.eq_nil_of_length_eq_zero (by omega)
    subst hnil; simp
  | q + 1, xs, ys, hxs => by
    have hge : k ≤ xs.length := by
      have : (q + 1) * k = q * k + k := Nat.succ_mul q k
      omega
    have hsplit : xs = xs.take k ++ xs.drop k := (List.take_append_drop k xs).symm
    have htake_len : (xs.take k).length = k := List.length_take_of_le hge
    have hdrop_len : (xs.drop k).length = q * k := by
      rw [List.length_drop, hxs, Nat.succ_mul]; omega
    conv => lhs; rw [hsplit, List.append_assoc]
    rw [chunksOf_append_exact k hk (xs.take k) (xs.drop k ++ ys) htake_len]
    rw [chunksOf_append_mul k hk q (xs.drop k) ys hdrop_len]
    conv => rhs; arg 1; rw [hsplit]
    rw [chunksOf_append_exact k hk (xs.take k) (xs.drop k) htake_len]
    rfl

private theorem chunksOf_single {α : Type _} (k : Nat) (hk : 0 < k) (xs : List α)
    (hne : xs ≠ []) (hle : xs.length ≤ k) :
    chunksOf k hk xs = [xs] := by
  rw [chunksOf_of_nonempty k hk xs hne, List.take_of_length_le hle,
      List.drop_of_length_le hle, chunksOf_nil]

/-- Full 256-child parent digests emitted so far at `level ≥ 1`. -/
def fullParents (sha256 : List Std.U8 → Hash) (level : Nat) (nodes : List Hash) : List Hash :=
  specLevelStep sha256 level (nodes.take ((nodes.length / 256) * 256))

private theorem fullParents_length (sha256 : List Std.U8 → Hash) (level : Nat) (nodes : List Hash) :
    (fullParents sha256 level nodes).length = nodes.length / 256 := by
  unfold fullParents
  rw [specLevelStep_length]
  have hle : (nodes.length / 256) * 256 ≤ nodes.length := Nat.div_mul_le_self _ _
  rw [List.length_take_of_le hle]
  omega

private theorem specLevelStep_append_mul
    (sha256 : List Std.U8 → Hash) (level q : Nat) (xs ys : List Hash)
    (hxs : xs.length = q * 256) :
    specLevelStep sha256 level (xs ++ ys) =
      specLevelStep sha256 level xs ++
        (chunksOf CHILDREN_PER_NODE (by decide) ys).mapIdx fun idx grp =>
          specHashInternal sha256 CHILDREN_PER_NODE 8192#u32
            (u64OfNat level) (u64OfNat ((q + idx) * BLOCK_BYTES)) grp := by
  have hclen : (chunksOf CHILDREN_PER_NODE (by decide) xs).length = q := by
    rw [chunksOf_length, hxs]; simp [CHILDREN_PER_NODE]; omega
  conv =>
    lhs
    unfold specLevelStep
    rw [chunksOf_append_mul CHILDREN_PER_NODE (by decide) q xs ys hxs, List.mapIdx_append, hclen]
  refine congrArg (specLevelStep sha256 level xs ++ ·) ?_
  apply List.ext_getElem
  · simp
  · intro idx h1 h2
    simp [Nat.add_comm idx q]

private theorem fullParents_append_singleton_of_not_dvd
    (sha256 : List Std.U8 → Hash) (level : Nat) (xs : List Hash) (d : Hash)
    (hndvd : (xs.length + 1) % 256 ≠ 0) :
    fullParents sha256 level (xs ++ [d]) = fullParents sha256 level xs := by
  unfold fullParents
  have hlen : (xs ++ [d]).length = xs.length + 1 := by simp
  have hdiv : (xs.length + 1) / 256 = xs.length / 256 := by omega
  have hle : (xs.length / 256) * 256 ≤ xs.length := Nat.div_mul_le_self _ _
  rw [hlen, hdiv, List.take_append_of_le_length hle]

private theorem fullParents_append_singleton_of_dvd
    (sha256 : List Std.U8 → Hash) (level : Nat) (xs : List Hash) (d : Hash)
    (hdvd : (xs.length + 1) % 256 = 0) :
    fullParents sha256 level (xs ++ [d]) =
      fullParents sha256 level xs ++
        [specHashInternal sha256 CHILDREN_PER_NODE 8192#u32
          (u64OfNat level) (u64OfNat ((xs.length / 256) * BLOCK_BYTES))
          (xs.drop ((xs.length / 256) * 256) ++ [d])] := by
  unfold fullParents
  set q := xs.length / 256
  have hlen : (xs ++ [d]).length = (q + 1) * 256 := by simp; omega
  have hdiv : ((xs ++ [d]).length / 256) * 256 = (xs ++ [d]).length := by omega
  rw [hdiv, List.take_length]
  have hsplit : xs ++ [d] = xs.take (q * 256) ++ (xs.drop (q * 256) ++ [d]) := by
    rw [← List.append_assoc, List.take_append_drop]
  have htake_len : (xs.take (q * 256)).length = q * 256 :=
    List.length_take_of_le (Nat.div_mul_le_self _ _)
  have htail_len : (xs.drop (q * 256) ++ [d]).length = 256 := by
    simp [List.length_drop]; omega
  rw [hsplit, specLevelStep_append_mul sha256 level q _ _ htake_len]
  have hsingle :
      chunksOf CHILDREN_PER_NODE (by decide) (xs.drop (q * 256) ++ [d]) =
        [xs.drop (q * 256) ++ [d]] :=
    chunksOf_single CHILDREN_PER_NODE (by decide) _ (by simp)
      (by simp [CHILDREN_PER_NODE, htail_len])
  rw [hsingle]
  simp

private theorem specLevelStep_of_dvd
    (sha256 : List Std.U8 → Hash) (level : Nat) (items : List Hash)
    (hdvd : items.length % 256 = 0) :
    specLevelStep sha256 level items = fullParents sha256 level items := by
  unfold fullParents
  have h : (items.length / 256) * 256 = items.length := by omega
  rw [h, List.take_length]

private theorem specLevelStep_eq_fullParents_append
    (sha256 : List Std.U8 → Hash) (level : Nat) (items extra : List Hash)
    (hne : items.drop ((items.length / 256) * 256) ++ extra ≠ [])
    (hle : (items.drop ((items.length / 256) * 256) ++ extra).length ≤ 256) :
    specLevelStep sha256 level (items ++ extra) =
      fullParents sha256 level items ++
        [specHashInternal sha256 CHILDREN_PER_NODE 8192#u32
          (u64OfNat level) (u64OfNat ((items.length / 256) * BLOCK_BYTES))
          (items.drop ((items.length / 256) * 256) ++ extra)] := by
  set q := items.length / 256
  have hsplit : items ++ extra = items.take (q * 256) ++ (items.drop (q * 256) ++ extra) := by
    rw [← List.append_assoc, List.take_append_drop]
  have htake_len : (items.take (q * 256)).length = q * 256 :=
    List.length_take_of_le (Nat.div_mul_le_self _ _)
  rw [hsplit, specLevelStep_append_mul sha256 level q _ _ htake_len]
  have hsingle :
      chunksOf CHILDREN_PER_NODE (by decide) (items.drop (q * 256) ++ extra) =
        [items.drop (q * 256) ++ extra] :=
    chunksOf_single CHILDREN_PER_NODE (by decide) _ hne (by simpa [CHILDREN_PER_NODE] using hle)
  rw [hsingle]
  simp [fullParents, q]

/-- Full digests emitted to level `k` (`k = 0` is `leaves`; `k + 1` is `fullParents` of level `k`). -/
def streamedAt (sha256 : List Std.U8 → Hash) : Nat → List Hash → List Hash
  | 0, leaves => leaves
  | k + 1, leaves => fullParents sha256 (k + 1) (streamedAt sha256 k leaves)

private theorem streamedAt_length (sha256 : List Std.U8 → Hash) :
    ∀ (k : Nat) (leaves : List Hash),
      (streamedAt sha256 k leaves).length = leaves.length / (256 ^ k)
  | 0, leaves => by simp [streamedAt]
  | k + 1, leaves => by
    simp only [streamedAt, fullParents_length, streamedAt_length sha256 k leaves,
               Nat.pow_succ, Nat.div_div_eq_div_mul]

private theorem streamedAt_eq_of_eq_above
    (sha256 : List Std.U8 → Hash) {leaves₁ leaves₂ : List Hash} {j : Nat}
    (heq : streamedAt sha256 j leaves₁ = streamedAt sha256 j leaves₂) :
    ∀ k, j ≤ k → streamedAt sha256 k leaves₁ = streamedAt sha256 k leaves₂ := by
  intro k hjk
  obtain ⟨d, rfl⟩ := Nat.exists_eq_add_of_le hjk
  clear hjk
  induction d with
  | zero => simpa using heq
  | succ d ih => rw [Nat.add_succ, streamedAt, streamedAt, ih]

/-- Iterated `specLevelStep` across `i` tree levels. -/
def iterLevelStep (sha256 : List Std.U8 → Hash) : Nat → List Hash → List Hash
  | 0, leaves => leaves
  | i + 1, leaves => specLevelStep sha256 (i + 1) (iterLevelStep sha256 i leaves)

private theorem iterLevelStep_length_le_pow (sha256 : List Std.U8 → Hash) :
    ∀ (i D : Nat) (leaves : List Hash),
      leaves.length ≤ 256 ^ D →
      i ≤ D →
      (iterLevelStep sha256 i leaves).length ≤ 256 ^ (D - i)
  | 0, D, leaves, hle, _ => by simpa [iterLevelStep] using hle
  | i + 1, D, leaves, hle, hi => by
    have ih := iterLevelStep_length_le_pow sha256 i D leaves hle (by omega)
    simp only [iterLevelStep, specLevelStep_length]
    have hpow : 256 ^ (D - i) = 256 ^ (D - (i + 1)) * 256 := by
      have : D - i = (D - (i + 1)) + 1 := by omega
      rw [this, Nat.pow_succ]
    omega

private theorem iterLevelStep_length_ge_two (sha256 : List Std.U8 → Hash) :
    ∀ (i : Nat) (leaves : List Hash),
      256 ^ i < leaves.length →
      2 ≤ (iterLevelStep sha256 i leaves).length
  | 0, leaves, hgt => by simp only [iterLevelStep, Nat.pow_zero] at hgt ⊢; omega
  | i + 1, leaves, hgt => by
    have hpow_gt : ∀ (m : Nat),
        m ≤ i + 1 → 256 ^ (i + 1 - m) < (iterLevelStep sha256 m leaves).length := by
      intro m hm
      induction m with
      | zero => simpa [iterLevelStep] using hgt
      | succ m ihm =>
        have hprev := ihm (by omega)
        simp only [iterLevelStep, specLevelStep_length]
        have hexp : i + 1 - m = (i + 1 - (m + 1)) + 1 := by omega
        rw [hexp, Nat.pow_succ] at hprev
        omega
    have htop := hpow_gt (i + 1) (le_refl _)
    simp at htop
    omega

private theorem iterLevelStep_length_pos (sha256 : List Std.U8 → Hash) :
    ∀ (i : Nat) (leaves : List Hash),
      0 < leaves.length →
      0 < (iterLevelStep sha256 i leaves).length
  | 0, leaves, hpos => by simpa [iterLevelStep] using hpos
  | i + 1, leaves, hpos => by
    have ih := iterLevelStep_length_pos sha256 i leaves hpos
    simp only [iterLevelStep, specLevelStep_length]
    omega

private theorem specReduceLevels_eq_of_iterLevelStep
    (sha256 : List Std.U8 → Hash) (leaves : List Hash) :
    ∀ (i : Nat),
      (∀ k < i, 2 ≤ (iterLevelStep sha256 k leaves).length) →
      specRootFromLeaves sha256 leaves =
        specReduceLevels sha256 (i + 1) (iterLevelStep sha256 i leaves)
  | 0, _ => rfl
  | i + 1, hge2 => by
    rw [specReduceLevels_eq_of_iterLevelStep sha256 leaves i (fun k hk => hge2 k (by omega))]
    have hlen2 := hge2 i (Nat.lt_succ_self i)
    rcases hlist : iterLevelStep sha256 i leaves with _ | ⟨a, _ | ⟨b, rest⟩⟩
    · simp [hlist] at hlen2
    · simp [hlist] at hlen2
    · rw [specReduceLevels_two_or_more]
      simp [iterLevelStep, hlist]

private theorem iterLevelStep_eq_streamedAt_of_pow
    (sha256 : List Std.U8 → Hash) (D : Nat) (leaves : List Hash)
    (hlen : leaves.length = 256 ^ D) :
    ∀ i ≤ D, iterLevelStep sha256 i leaves = streamedAt sha256 i leaves
  | 0, _ => rfl
  | i + 1, hi => by
    simp only [iterLevelStep, streamedAt]
    rw [iterLevelStep_eq_streamedAt_of_pow sha256 D leaves hlen i (by omega)]
    apply specLevelStep_of_dvd
    rw [streamedAt_length, hlen]
    have hexp : D = (D - (i + 1) + 1) + i := by omega
    rw [hexp, Nat.pow_add, Nat.mul_div_cancel _ (by positivity), Nat.pow_succ]
    omega

-- ============================================================================
-- 2. Relational Level Hasher & Polymorphic `BuilderInv`
-- ============================================================================

/-- Pure byte accumulator fed into `MerkleRootBuilder.levels[k].hasher` after `leaves`. -/
def specLevelAcc (sha256 : List Std.U8 → Hash) (k : Nat) (leaves : List Hash) : List Std.U8 :=
  let items := streamedAt sha256 k leaves
  let fullBlocks := items.length / 256
  specIdentity 8192#u32 (u64OfNat (k + 1)) (u64OfNat (fullBlocks * 8192)) ++
    concatHashes (items.drop (fullBlocks * 256))

/-- Every active `LevelHasher` holds at most `12 + 255 * 32 = 8,172` bytes (`≪ SHA256_MAX_BYTES`). -/
private theorem specLevelAcc_length_le
    (sha256 : List Std.U8 → Hash) (k : Nat) (leaves : List Hash) :
    (specLevelAcc sha256 k leaves).length ≤ 8172 := by
  unfold specLevelAcc
  dsimp only
  rw [List.length_append, specIdentity_length, concatHashes_length, List.length_drop]
  have hmod : (streamedAt sha256 k leaves).length -
      ((streamedAt sha256 k leaves).length / 256) * 256 < 256 := by
    omega
  omega

/-- Relational specification of `MerkleRootBuilder.levels[k]` over `R_H`. -/
def LevelHasherRel {H : Type} (R_H : H → List Std.U8 → Prop)
    (sha256 : List Std.U8 → Hash) (k : Nat) (leaves : List Hash)
    (lh : LevelHasher H) : Prop :=
  R_H lh.hasher (specLevelAcc sha256 k leaves) ∧
  lh.offset = u64OfNat ((streamedAt sha256 k leaves).length * 32)

private theorem levelHasherRel_of_streamedAt_eq
    {H : Type} {R_H : H → List Std.U8 → Prop}
    {sha256 : List Std.U8 → Hash} {k : Nat} {leaves₁ leaves₂ : List Hash}
    {lh : LevelHasher H}
    (heq : streamedAt sha256 k leaves₁ = streamedAt sha256 k leaves₂)
    (hlh : LevelHasherRel R_H sha256 k leaves₁ lh) :
    LevelHasherRel R_H sha256 k leaves₂ lh := by
  unfold LevelHasherRel specLevelAcc at hlh ⊢
  simpa [heq] using hlh

/-- Polymorphic state invariant of `MerkleRootBuilder H C` after ingesting `leaves`. -/
structure BuilderInv {H C : Type}
    (sha256 : List Std.U8 → Hash)
    (R_H : H → List Std.U8 → Prop)
    (R_C : C → List Hash → Prop)
    (leaves : List Hash)
    (b : MerkleRootBuilder H C) : Prop where
  hbound : leaves.length * 8192 ≤ U64.max
  hcollector : R_C b.collector leaves
  hoffset : b.offset = u64OfNat (leaves.length * 8192)
  htree :
    (leaves = [] ∧ b.levels.val = [] ∧ b.root = none) ∨
    (∃ D r,
      leaves.length = 256 ^ D ∧
      streamedAt sha256 D leaves = [r] ∧
      b.levels.val.length = D ∧
      (∀ k (hk : k < b.levels.val.length),
        LevelHasherRel R_H sha256 k leaves (b.levels.val[k]'hk)) ∧
      b.root = some r) ∨
    (∃ D,
      256 ^ D < leaves.length ∧
      leaves.length < 256 ^ (D + 1) ∧
      b.levels.val.length = D + 1 ∧
      (∀ k (hk : k < b.levels.val.length),
        LevelHasherRel R_H sha256 k leaves (b.levels.val[k]'hk)) ∧
      b.root = none)

private theorem depth_le_six_of_u64_bound {D n : Nat}
    (hpow : 256 ^ D ≤ n) (hbound : n * 8192 ≤ U64.max) : D ≤ 6 := by
  by_contra hgt
  have hd7 : 7 ≤ D := by omega
  have hp7 : 256 ^ 7 ≤ 256 ^ D := Nat.pow_le_pow_right (by decide) hd7
  have hmax := u64_max_val_eq
  have hc : 256 ^ 7 * 8192 = 590295810358705651712 := by decide
  have : 256 ^ 7 * 8192 ≤ n * 8192 := Nat.mul_le_mul_right 8192 (le_trans hp7 hpow)
  omega

theorem merkle_new_inv
    {H C : Type} (sha256 : List Std.U8 → Hash)
    (hInst : Sha256State H) (R_H : H → List Std.U8 → Prop)
    (cInst : LeafCollector C) (R_C : C → List Hash → Prop)
    (c₀ : C) (hc₀ : R_C c₀ []) :
    ∃ b, MerkleRootBuilder.new hInst cInst c₀ = ok b ∧
      BuilderInv sha256 R_H R_C [] b := by
  refine ⟨_, rfl, ?_⟩
  refine ⟨by simp, hc₀, ?_, Or.inl ⟨rfl, rfl, rfl⟩⟩
  apply UScalar.eq_of_val_eq
  simp

-- ============================================================================
-- 3. Polymorphic `push_hash_loop` & `push_hash` Invariant Preservation
-- ============================================================================

theorem push_hash_loop_gen_spec
    {H : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (D : Nat) (leaves : List Hash) (d : Hash)
    (hbound' : (leaves.length + 1) * 8192 ≤ U64.max)
    (hgtD : 256 ^ D < leaves.length + 1)
    (hleD1 : leaves.length + 1 ≤ 256 ^ (D + 1)) :
    ∀ (rem : Nat) (j : Std.Usize) (v : alloc.vec.Vec (LevelHasher H)) (cur : Hash),
      j.val ≤ D + 1 →
      D + 1 - j.val = rem →
      v.val.length = D + 1 →
      (∀ k (hk : k < v.val.length), k < j.val →
        LevelHasherRel R_H sha256 k (leaves ++ [d]) (v.val[k]'hk)) →
      (∀ k (hk : k < v.val.length), j.val ≤ k →
        LevelHasherRel R_H sha256 k leaves (v.val[k]'hk)) →
      streamedAt sha256 j.val (leaves ++ [d]) = streamedAt sha256 j.val leaves ++ [cur] →
      ∃ v_out root_out,
        MerkleRootBuilder.push_hash_loop hInst v none cur j =
          ok (v_out, root_out) ∧
        v_out.val.length = D + 1 ∧
        (∀ k (hk : k < v_out.val.length),
          LevelHasherRel R_H sha256 k (leaves ++ [d]) (v_out.val[k]'hk)) ∧
        ((leaves.length + 1 < 256 ^ (D + 1) ∧ root_out = none) ∨
         (∃ r', leaves.length + 1 = 256 ^ (D + 1) ∧
           streamedAt sha256 (D + 1) (leaves ++ [d]) = [r'] ∧ root_out = some r'))
  | 0, j, v, cur, hj_le, hrem, hv_len, hv_below, _, hstream => by
    have hj_eq : j.val = D + 1 := by omega
    have hbody :
        MerkleRootBuilder.push_hash_loop.body hInst none v cur j =
          ok (.done (v, some cur)) := by
      unfold MerkleRootBuilder.push_hash_loop.body
      have hlt : ¬ (j < alloc.vec.Vec.len v) := by scalar_tac
      simp [hlt]
    refine ⟨v, some cur, ?_, hv_len, ?_, Or.inr ⟨cur, ?_, ?_, rfl⟩⟩
    · unfold MerkleRootBuilder.push_hash_loop
      exact loop_done _ (v, cur, j) _ hbody
    · intro k hk
      exact hv_below k hk (by omega)
    · have hlen_s := congrArg List.length hstream
      rw [streamedAt_length, List.length_append, List.length_singleton] at hlen_s
      have hpos : 1 ≤ (leaves.length + 1) / 256 ^ (D + 1) := by simp [hj_eq] at hlen_s; omega
      have hge : 256 ^ (D + 1) ≤ leaves.length + 1 := by
        by_contra hlt'
        have : (leaves.length + 1) / 256 ^ (D + 1) = 0 := Nat.div_eq_of_lt (by omega)
        omega
      omega
    · rw [← hj_eq, hstream]
      have h0 : (streamedAt sha256 j.val leaves).length = 0 := by
        rw [streamedAt_length, hj_eq]
        exact Nat.div_eq_of_lt (by omega)
      rw [List.eq_nil_of_length_eq_zero h0]; rfl
  | rem + 1, j, v, cur, _, hrem, hv_len, hv_below, hv_above, hstream => by
    have hj_lt : j.val < D + 1 := by omega
    have hD6 : D ≤ 6 := depth_le_six_of_u64_bound (by omega) hbound'
    have hsmax := sha256_max_bytes_eq
    have hlt_v : j.val < v.val.length := by omega
    have hlt : j < alloc.vec.Vec.len v := by scalar_tac
    set lh_j := v.val[j.val]'hlt_v
    obtain ⟨hrel_j, hoff_j⟩ := hv_above j.val hlt_v (le_refl _)
    set items := streamedAt sha256 j.val leaves
    have hoff_lh : lh_j.offset = u64OfNat (items.length * 32) := hoff_j
    have hm_len : items.length + 1 = (leaves.length + 1) / 256 ^ j.val := by
      have h1 := congrArg List.length hstream
      rw [streamedAt_length, List.length_append, List.length_singleton] at h1
      simpa [items] using h1.symm
    have hm_bound32 : (items.length + 1) * 32 ≤ U64.max := by
      have hle_n : items.length + 1 ≤ leaves.length + 1 := by
        rw [hm_len]; exact Nat.div_le_self _ _
      have : (items.length + 1) * 32 ≤ (leaves.length + 1) * 8192 := by omega
      exact le_trans this hbound'
    have hm_off_bound : items.length * 32 ≤ U64.max := by omega
    have hc_j : UScalar.cast .U64 j = u64OfNat j.val := cast_u64_usize_eq j
    have hj_u64_val : (u64OfNat j.val).val = j.val :=
      u64OfNat_val_of_le _ (by have := u64_max_val_eq; omega)
    obtain ⟨lvl_num, hlvl_add, hlvl_val⟩ := uscalar_add_ok (u64OfNat j.val) 1#u64 (by
      rw [uscalar_max_u64, hj_u64_val]; simp; have := u64_max_val_eq; omega)
    simp only [hj_u64_val, UScalar.ofNatCore_val_eq] at hlvl_val
    have hlvl_eq : lvl_num = u64OfNat (j.val + 1) := by
      apply UScalar.eq_of_val_eq
      rw [hlvl_val, u64OfNat_val_of_le _ (by have := u64_max_val_eq; omega)]
    have hidx_mut := vec_index_mut_ok v j hlt_v
    have hacc_len := specLevelAcc_length_le sha256 j.val leaves
    have hcur_len : (Array.to_slice cur).val.length = 32 := by simp [Array.val_to_slice]
    obtain ⟨t_upd, hupd, hrel_upd⟩ := hsha.update_ok lh_j.hasher
      (specLevelAcc sha256 j.val leaves) (Array.to_slice cur) hrel_j
      (by rw [hcur_len]; omega)
    rw [Array.val_to_slice] at hrel_upd
    obtain ⟨i3, hi3_add, hi3_val⟩ := uscalar_add_ok (u64OfNat (items.length * 32)) 32#u64 (by
      rw [uscalar_max_u64, u64OfNat_val_of_le _ hm_off_bound]; simp; omega)
    rw [u64OfNat_val_of_le _ hm_off_bound] at hi3_val
    simp only [UScalar.ofNatCore_val_eq] at hi3_val
    have hi3_eq : i3 = u64OfNat ((items.length + 1) * 32) := by
      apply UScalar.eq_of_val_eq
      rw [hi3_val, u64OfNat_val_of_le _ hm_bound32]; omega
    by_cases hdvd : (items.length + 1) % 256 = 0
    · have hmult_true : core.num.U64.is_multiple_of i3 8192#u64 = ok true := by
        have hi3_mod : i3.val % 8192 = 0 := by rw [hi3_val]; omega
        simp [core.num.U64.is_multiple_of, UScalar.is_multiple_of, hi3_mod]
      have hj1_bound : j.val + 1 ≤ UScalar.max .Usize := by
        rw [uscalar_max_usize]; have := usize_max_val_ge; omega
      obtain ⟨j1, hj1_add, hj1_val⟩ := uscalar_add_ok j 1#usize hj1_bound
      simp only [UScalar.ofNatCore_val_eq] at hj1_val
      obtain ⟨new_h, hmhh, hrel_new⟩ :=
        make_hash_hasher_gen_spec hsha 8192#u32 (u64OfNat (j.val + 1)) i3
          (by rw [← hlvl_eq, hlvl_val]; omega)
      set old_acc := specLevelAcc sha256 j.val leaves ++ cur.val
      have hcur_vlen : cur.val.length = 32 := by simp
      have hfin := hsha.finalize_ok t_upd old_acc hrel_upd (by
        simp only [old_acc, List.length_append, hcur_vlen]; omega)
      set new_lh : LevelHasher H := { hasher := new_h, offset := i3 }
      have hbody :
          MerkleRootBuilder.push_hash_loop.body hInst none v cur j =
            ok (.cont (v.set j new_lh, sha256 old_acc, j1)) := by
        unfold MerkleRootBuilder.push_hash_loop.body
        simp only [hlt, ↓reduceIte, lift, hc_j, hlvl_add, hlvl_eq, hidx_mut,
                   show v.val[j.val]'hlt_v = lh_j from rfl, hoff_lh, hupd,
                   cast_u64_HASH_SIZE, hi3_add, cast_u64_BLOCK_SIZE, hmult_true,
                   cast_u32_BLOCK_SIZE, hmhh, core.mem.replace, hfin, hj1_add,
                   new_lh, bind_ok, uncurry]
      have hnew_lh_rel : LevelHasherRel R_H sha256 j.val (leaves ++ [d]) new_lh := by
        unfold LevelHasherRel specLevelAcc
        rw [hstream]
        have hlen' : (items ++ [cur]).length = items.length + 1 := by simp
        have hfb : ((items.length + 1) / 256) * 8192 = (items.length + 1) * 32 := by omega
        have hdrop_empty : (items ++ [cur]).drop (((items.length + 1) / 256) * 256) = [] := by
          apply List.drop_of_length_le
          rw [hlen']; omega
        simpa [new_lh, items, hlen', hdrop_empty, hi3_eq, hfb] using hrel_new
      have hv1_len : (v.set j new_lh).val.length = D + 1 := by
        simp [alloc.vec.Vec.set_val_eq, hv_len]
      have hv1_below : ∀ k (hk : k < (v.set j new_lh).val.length), k < j1.val →
          LevelHasherRel R_H sha256 k (leaves ++ [d]) ((v.set j new_lh).val[k]'hk) := by
        intro k hk hkj1
        simp only [alloc.vec.Vec.set_val_eq] at hk ⊢
        by_cases hkj : j.val = k
        · subst hkj
          simpa using hnew_lh_rel
        · rw [List.getElem_set_ne hkj]
          exact hv_below k (by simpa [List.length_set] using hk) (by omega)
      have hv1_above : ∀ k (hk : k < (v.set j new_lh).val.length), j1.val ≤ k →
          LevelHasherRel R_H sha256 k leaves ((v.set j new_lh).val[k]'hk) := by
        intro k hk hkj1
        have hkj : j.val ≠ k := by omega
        simp only [alloc.vec.Vec.set_val_eq] at hk ⊢
        rw [List.getElem_set_ne hkj]
        exact hv_above k (by simpa [List.length_set] using hk) (by omega)
      have hold_eq :
          sha256 old_acc =
            specHashInternal sha256 CHILDREN_PER_NODE 8192#u32
              (u64OfNat (j.val + 1)) (u64OfNat ((items.length / 256) * BLOCK_BYTES))
              (items.drop ((items.length / 256) * 256) ++ [cur]) := by
        unfold old_acc specLevelAcc specHashInternal specInternalPreimage
        have hgrp_len : (items.drop ((items.length / 256) * 256) ++ [cur]).length = 256 := by
          simp [List.length_drop]; omega
        simp [items, CHILDREN_PER_NODE, BLOCK_BYTES, hgrp_len, specZeroPad, List.append_assoc]
      have hstream_next :
          streamedAt sha256 j1.val (leaves ++ [d]) =
            streamedAt sha256 j1.val leaves ++ [sha256 old_acc] := by
        rw [hj1_val]
        simp only [streamedAt]
        rw [hstream, fullParents_append_singleton_of_dvd sha256 (j.val + 1) items cur hdvd,
            hold_eq]
      obtain ⟨v_out, root_out, ih, hv_out_len, hv_out_rel, hroot_out⟩ :=
        push_hash_loop_gen_spec hsha D leaves d hbound' hgtD hleD1
          rem j1 (v.set j new_lh) (sha256 old_acc)
          (by omega) (by omega) hv1_len hv1_below hv1_above hstream_next
      refine ⟨v_out, root_out, ?_, hv_out_len, hv_out_rel, hroot_out⟩
      unfold MerkleRootBuilder.push_hash_loop at ih ⊢
      rw [loop_cont _ (v, cur, j) _ hbody]
      exact ih
    · have hmult_false : core.num.U64.is_multiple_of i3 8192#u64 = ok false := by
        have hi3_mod : i3.val % 8192 ≠ 0 := by rw [hi3_val]; omega
        simp [core.num.U64.is_multiple_of, UScalar.is_multiple_of, hi3_mod]
      set upd_lh : LevelHasher H := { hasher := t_upd, offset := i3 }
      have hbody :
          MerkleRootBuilder.push_hash_loop.body hInst none v cur j =
            ok (.done (v.set j upd_lh, none)) := by
        unfold MerkleRootBuilder.push_hash_loop.body
        simp only [hlt, ↓reduceIte, lift, hc_j, hlvl_add, hlvl_eq, hidx_mut,
                   show v.val[j.val]'hlt_v = lh_j from rfl, hoff_lh, hupd,
                   cast_u64_HASH_SIZE, hi3_add, cast_u64_BLOCK_SIZE,
                   hmult_false, Bool.false_eq_true, upd_lh, bind_ok, uncurry]
      have hupd_rel : LevelHasherRel R_H sha256 j.val (leaves ++ [d]) upd_lh := by
        unfold LevelHasherRel specLevelAcc
        rw [hstream]
        have hlen' : (items ++ [cur]).length = items.length + 1 := by simp
        have hdiv_eq : (items.length + 1) / 256 = items.length / 256 := by omega
        have hle_m : (items.length / 256) * 256 ≤ items.length := Nat.div_mul_le_self _ _
        have hdrop_app :
            (items ++ [cur]).drop ((items.length / 256) * 256) =
              items.drop ((items.length / 256) * 256) ++ [cur] :=
          List.drop_append_of_le_length hle_m
        refine ⟨?_, ?_⟩
        · simpa [upd_lh, specLevelAcc, items, hlen', hdiv_eq, hdrop_app, List.append_assoc]
            using hrel_upd
        · simp [upd_lh, items, hlen', hi3_eq]
      have hstream_same :
          streamedAt sha256 (j.val + 1) (leaves ++ [d]) = streamedAt sha256 (j.val + 1) leaves := by
        simp only [streamedAt]
        rw [hstream]
        exact fullParents_append_singleton_of_not_dvd sha256 (j.val + 1) items cur hdvd
      refine ⟨v.set j upd_lh, none, ?_, ?_, ?_, Or.inl ⟨?_, rfl⟩⟩
      · unfold MerkleRootBuilder.push_hash_loop
        exact loop_done _ (v, cur, j) _ hbody
      · simp [alloc.vec.Vec.set_val_eq, hv_len]
      · intro k hk
        simp only [alloc.vec.Vec.set_val_eq] at hk ⊢
        have hk_orig : k < v.val.length := by simpa [List.length_set] using hk
        by_cases hkj_lt : k < j.val
        · rw [List.getElem_set_ne (by omega)]
          exact hv_below k hk_orig hkj_lt
        · by_cases hkj_eq : j.val = k
          · subst hkj_eq
            simpa using hupd_rel
          · rw [List.getElem_set_ne hkj_eq]
            have hsk := streamedAt_eq_of_eq_above sha256 hstream_same.symm k (by omega)
            exact levelHasherRel_of_streamedAt_eq hsk (hv_above k hk_orig (by omega))
      · by_contra hge
        have heq_pow : leaves.length + 1 = 256 ^ (D + 1) := by omega
        rw [heq_pow] at hm_len
        have hexp : D + 1 = (D - j.val + 1) + j.val := by omega
        rw [hexp, Nat.pow_add, Nat.mul_div_cancel _ (by positivity), Nat.pow_succ] at hm_len
        omega

/-- `MerkleRootBuilder.push_hash` preserves `BuilderInv` for any `ValidSha256State` and
    `ValidLeafCollector` whenever `(leaves.length + 1) * 8192 ≤ U64.max` and
    `leaves.length < maxLeaves`. -/
theorem push_hash_gen_inv
    {H C : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {cInst : LeafCollector C} {maxLeaves : Nat} {R_C : C → List Hash → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hcol : ValidLeafCollector cInst maxLeaves R_C)
    (leaves : List Hash) (self : MerkleRootBuilder H C) (digest : Hash)
    (hinv : BuilderInv sha256 R_H R_C leaves self)
    (hbound' : (leaves.length + 1) * 8192 ≤ U64.max)
    (hcap : leaves.length < maxLeaves) :
    ∃ self',
      MerkleRootBuilder.push_hash hInst cInst self digest = ok self' ∧
      BuilderInv sha256 R_H R_C (leaves ++ [digest]) self' := by
  unfold MerkleRootBuilder.push_hash
  have hsmax := sha256_max_bytes_eq
  obtain ⟨next_off, hchk_add, hnext_off_val⟩ :=
    u64_checked_add_ok self.offset 8192#u64 (by
      rw [hinv.hoffset, u64OfNat_val_of_le _ hinv.hbound]; simp; omega)
  have hnext_off_eq : next_off = u64OfNat ((leaves.length + 1) * 8192) := by
    apply UScalar.eq_of_val_eq
    rw [hnext_off_val, hinv.hoffset, u64OfNat_val_of_le _ hinv.hbound,
        u64OfNat_val_of_le _ hbound']; simp; omega
  obtain ⟨c', hc_add, hc_rel⟩ := hcol.add_leaf_hash_ok self.collector leaves digest hcap hinv.hcollector
  have hopt : ofOption (some next_off) Error.panic = ok next_off := rfl
  simp only [lift, cast_u64_BLOCK_SIZE, hchk_add, core.option.Option.unwrap, hopt,
             hc_add, core.option.Option.take, bind_ok, uncurry]
  rcases hinv.htree with ⟨hempty, hlvls0, hroot0⟩ |
                         ⟨D, r, hpow, hstrD, hlvls_len, hlvls_rel, hrootD⟩ |
                         ⟨D, hgtD, hltD1, hlvls_len, hlvls_rel, hrootD⟩
  · -- Empty -> 1 leaf (`256^0`)
    subst hempty
    simp only [hroot0, bind_ok]
    have hbody :
        MerkleRootBuilder.push_hash_loop.body hInst none self.levels digest 0#usize =
          ok (.done (self.levels, some digest)) := by
      unfold MerkleRootBuilder.push_hash_loop.body
      have hlt : ¬ (0#usize < alloc.vec.Vec.len self.levels) := by
        simp [alloc.vec.Vec.len, hlvls0]
      simp [hlt]
    have hloop :
        MerkleRootBuilder.push_hash_loop hInst self.levels none digest 0#usize =
          ok (self.levels, some digest) := by
      unfold MerkleRootBuilder.push_hash_loop
      exact loop_done _ (self.levels, digest, 0#usize) _ hbody
    simp only [hloop, bind_ok, uncurry]
    refine ⟨_, rfl, ?_⟩
    refine ⟨by simpa using hbound', hc_rel, by simpa using hnext_off_eq,
            Or.inr (Or.inl ⟨0, digest, by simp, rfl, ?_, ?_, rfl⟩)⟩
    · simp [hlvls0]
    · intro k hk
      simp [hlvls0] at hk
  · -- Exact power `N = 256^D` -> `256^D + 1`
    simp only [hrootD]
    have hD6 : D ≤ 6 := depth_le_six_of_u64_bound (by omega) hinv.hbound
    have hc_len : UScalar.cast .U64 (alloc.vec.Vec.len self.levels) = u64OfNat D := by
      rw [cast_u64_usize_eq]; simp [hlvls_len]
    have hD_u64 : (u64OfNat D).val = D := u64OfNat_val_of_le _ (by have := u64_max_val_eq; omega)
    obtain ⟨next_lvl, hnl_add, hnl_val⟩ := uscalar_add_ok (u64OfNat D) 1#u64 (by
      rw [uscalar_max_u64, hD_u64]; simp; have := u64_max_val_eq; omega)
    simp only [hD_u64, UScalar.ofNatCore_val_eq] at hnl_val
    have hnl_eq : next_lvl = u64OfNat (D + 1) := by
      apply UScalar.eq_of_val_eq
      rw [hnl_val, u64OfNat_val_of_le _ (by have := u64_max_val_eq; omega)]
    obtain ⟨h0, hmhh, hrel0⟩ :=
      make_hash_hasher_gen_spec hsha 8192#u32 (u64OfNat (D + 1)) 0#u64
        (by rw [← hnl_eq, hnl_val]; omega)
    have hr_len : (Array.to_slice r).val.length = 32 := by simp [Array.val_to_slice]
    obtain ⟨h1, hupd1, hrel1⟩ :=
      hsha.update_ok h0 (specIdentity 8192#u32 (u64OfNat (D + 1)) 0#u64) (Array.to_slice r)
        hrel0 (by rw [hr_len]; simp; omega)
    rw [Array.val_to_slice] at hrel1
    set top_lh : LevelHasher H := { hasher := h1, offset := 32#u64 }
    have hpush_cap : self.levels.val.length < Usize.max := by
      rw [hlvls_len]; have := usize_max_val_ge; omega
    obtain ⟨v1, hv1_push, hv1_val⟩ := vec_push_ok self.levels top_lh hpush_cap
    have htop_rel : LevelHasherRel R_H sha256 D leaves top_lh := by
      unfold LevelHasherRel specLevelAcc
      rw [hstrD]
      have h32 : (32#u64 : Std.U64) = u64OfNat 32 := by
        apply UScalar.eq_of_val_eq
        rw [u64OfNat_val_of_le 32 (by have := u64_max_val_eq; omega)]; simp
      have h0_eq : (0#u64 : Std.U64) = u64OfNat 0 := by
        apply UScalar.eq_of_val_eq
        rw [u64OfNat_val_of_le 0 (by have := u64_max_val_eq; omega)]; simp
      refine ⟨?_, ?_⟩
      · simpa [top_lh, h0_eq] using hrel1
      · simp [top_lh, h32]
    have hv1_len : v1.val.length = D + 1 := by simp [hv1_val, hlvls_len]
    have hv1_above : ∀ k (hk : k < v1.val.length), 0 ≤ k →
        LevelHasherRel R_H sha256 k leaves (v1.val[k]'hk) := by
      intro k hk _
      simp only [hv1_val] at hk ⊢
      by_cases hkD : k < D
      · rw [List.getElem_append_left (by omega)]
        exact hlvls_rel k (by omega)
      · have hk_eq : k = D := by
          simp [hlvls_len] at hk; omega
        subst hk_eq
        rw [List.getElem_append_right (by omega)]
        simpa [hlvls_len] using htop_rel
    have hgt' : 256 ^ D < leaves.length + 1 := by omega
    have hlt' : leaves.length + 1 < 256 ^ (D + 1) := by
      rw [hpow, Nat.pow_succ]; have : 1 ≤ 256 ^ D := Nat.one_le_pow D 256 (by decide); omega
    obtain ⟨v_out, root_out, hloop, hv_out_len, hv_out_rel, hroot_cases⟩ :=
      push_hash_loop_gen_spec hsha D leaves digest hbound' hgt' (le_of_lt hlt')
        (D + 1) 0#usize v1 digest (by simp) (by simp) hv1_len
        (by intro k _ hk0; simp at hk0)
        (by simpa using hv1_above)
        (by simp [streamedAt])
    simp only [hc_len, hnl_add, hnl_eq, cast_u32_BLOCK_SIZE, hmhh,
               hupd1, cast_u64_HASH_SIZE, top_lh, hv1_push, hloop, bind_ok, uncurry]
    refine ⟨_, rfl, ⟨by simpa using hbound', hc_rel, by simpa using hnext_off_eq, ?_⟩⟩
    rcases hroot_cases with ⟨_, rfl⟩ | ⟨r', heq_pow, _, _⟩
    · exact Or.inr (Or.inr ⟨D, by simpa using hgt', by simpa using hlt',
                            hv_out_len, hv_out_rel, rfl⟩)
    · omega
  · -- Between powers `256^D < N < 256^(D + 1)`
    obtain ⟨v_out, root_out, hloop, hv_out_len, hv_out_rel, hroot_cases⟩ :=
      push_hash_loop_gen_spec hsha D leaves digest hbound' (by omega) (by omega)
        (D + 1) 0#usize self.levels digest (by simp) (by simp) hlvls_len
        (by intro k _ hk0; simp at hk0)
        (by intro k hk _; exact hlvls_rel k hk)
        (by simp [streamedAt])
    simp only [hrootD, hloop, bind_ok, uncurry]
    refine ⟨_, rfl, ⟨by simpa using hbound', hc_rel, by simpa using hnext_off_eq, ?_⟩⟩
    rcases hroot_cases with ⟨hlt', rfl⟩ | ⟨r', heq_pow, hstr', rfl⟩
    · exact Or.inr (Or.inr ⟨D, by simp; omega, by simpa using hlt',
                            hv_out_len, hv_out_rel, rfl⟩)
    · exact Or.inr (Or.inl ⟨D + 1, r', by simpa using heq_pow, hstr',
                            hv_out_len, hv_out_rel, rfl⟩)

-- ============================================================================
-- 4. Polymorphic `complete_loop`, `complete` Suffix, and `rebuild_root_gen_spec`
-- ============================================================================

private theorem complete_bubbling_step
    {H α : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (v1 : α) (lvl : Nat) (leaves : List Hash)
    (lh : LevelHasher H) (bubbling : Option Hash)
    (hlh : LevelHasherRel R_H sha256 lvl leaves lh)
    (hm_plus : ((streamedAt sha256 lvl leaves).length + 1) * 32 ≤ U64.max) :
    ∃ (t1 : H) (i3 : Std.U64),
      (match bubbling with
       | none => ok (v1, lh.hasher, lh.offset)
       | some digest => do
         let t2 ← hInst.update lh.hasher (Array.to_slice digest)
         let i5 ← lh.offset + UScalar.cast .U64 HASH_SIZE
         ok (v1, t2, i5)) =
      ok (v1, t1, i3) ∧
      R_H t1
        (specIdentity 8192#u32 (u64OfNat (lvl + 1))
          (u64OfNat (((streamedAt sha256 lvl leaves).length / 256) * 8192)) ++
          concatHashes ((streamedAt sha256 lvl leaves).drop
            (((streamedAt sha256 lvl leaves).length / 256) * 256) ++ bubbling.toList)) ∧
      i3.val = ((streamedAt sha256 lvl leaves).length / 256) * 8192 +
               ((streamedAt sha256 lvl leaves).drop
                 (((streamedAt sha256 lvl leaves).length / 256) * 256) ++ bubbling.toList).length * 32 := by
  obtain ⟨hrel_lh, hoff_lh⟩ := hlh
  have hm_off : (streamedAt sha256 lvl leaves).length * 32 ≤ U64.max := by omega
  have htail_len :
      ((streamedAt sha256 lvl leaves).drop
        (((streamedAt sha256 lvl leaves).length / 256) * 256)).length =
      (streamedAt sha256 lvl leaves).length % 256 := by
    simp [List.length_drop]; omega
  cases bubbling with
  | none =>
    refine ⟨lh.hasher, lh.offset, rfl, ?_, ?_⟩
    · simpa [specLevelAcc] using hrel_lh
    · simp only [hoff_lh, u64OfNat_val_of_le _ hm_off,
                 Option.toList, List.append_nil, htail_len]
      omega
  | some b =>
    have hsmax := sha256_max_bytes_eq
    have hacc_len := specLevelAcc_length_le sha256 lvl leaves
    have hb_len : (Array.to_slice b).val.length = 32 := by simp [Array.val_to_slice]
    obtain ⟨t2, hupd, hrel_t2⟩ := hsha.update_ok lh.hasher (specLevelAcc sha256 lvl leaves)
      (Array.to_slice b) hrel_lh (by rw [hb_len]; omega)
    rw [Array.val_to_slice] at hrel_t2
    obtain ⟨i5, hi5_add, hi5_val⟩ :=
      uscalar_add_ok (u64OfNat ((streamedAt sha256 lvl leaves).length * 32)) 32#u64 (by
        rw [uscalar_max_u64, u64OfNat_val_of_le _ hm_off]
        simp only [UScalar.ofNatCore_val_eq]
        omega)
    rw [u64OfNat_val_of_le _ hm_off] at hi5_val
    simp only [UScalar.ofNatCore_val_eq] at hi5_val
    refine ⟨t2, i5, ?_, ?_, ?_⟩
    · simp only [hupd, cast_u64_HASH_SIZE, hoff_lh, hi5_add, bind_ok]
    · simpa [specLevelAcc, List.append_assoc] using hrel_t2
    · rw [hi5_val]
      simp only [Option.toList, List.length_append, List.length_singleton, htail_len]
      omega

theorem complete_loop_gen_spec
    {H : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (D : Nat) (leaves : List Hash)
    (hbound : leaves.length * 8192 ≤ U64.max)
    (hgtD : 256 ^ D < leaves.length)
    (hltD1 : leaves.length < 256 ^ (D + 1)) :
    ∀ (rem : Nat) (i : Std.Usize)
      (v : alloc.vec.Vec (LevelHasher H)) (bubbling : Option Hash),
      i.val ≤ D + 1 →
      D + 1 - i.val = rem →
      v.val.length = D + 1 →
      (∀ k, i.val ≤ k → (hk : k < v.val.length) →
        LevelHasherRel R_H sha256 k leaves (v.val[k]'hk)) →
      iterLevelStep sha256 i.val leaves = streamedAt sha256 i.val leaves ++ bubbling.toList →
      ∃ root,
        MerkleRootBuilder.complete_loop hInst v bubbling i = ok (some root) ∧
        iterLevelStep sha256 (D + 1) leaves = [root]
  | 0, i, v, bubbling, hi_le, _, hv_len, _, hiter => by
    have hi_eq : i.val = D + 1 := by omega
    have hbody :
        MerkleRootBuilder.complete_loop.body hInst v bubbling i =
          ok (.done bubbling) := by
      unfold MerkleRootBuilder.complete_loop.body
      have hlt : ¬ (i < alloc.vec.Vec.len v) := by scalar_tac
      simp [hlt]
    have hstr_empty : streamedAt sha256 (D + 1) leaves = [] := by
      apply List.eq_nil_of_length_eq_zero
      rw [streamedAt_length]; exact Nat.div_eq_of_lt hltD1
    rw [hi_eq, hstr_empty, List.nil_append] at hiter
    have hlen_pos : 0 < (iterLevelStep sha256 (D + 1) leaves).length :=
      iterLevelStep_length_pos sha256 (D + 1) leaves (by
        have : 1 ≤ 256 ^ D := Nat.one_le_pow D 256 (by decide); omega)
    cases bubbling with
    | none =>
      rw [hiter] at hlen_pos; simp at hlen_pos
    | some root =>
      refine ⟨root, ?_, hiter⟩
      unfold MerkleRootBuilder.complete_loop
      exact loop_done _ (v, some root, i) _ hbody
  | rem + 1, i, v, bubbling, _, hrem, hv_len, hv_above, hiter => by
    have hi_lt : i.val < D + 1 := by omega
    have hD6 : D ≤ 6 := depth_le_six_of_u64_bound (le_of_lt hgtD) hbound
    have hsmax := sha256_max_bytes_eq
    have hlt_v : i.val < v.val.length := by omega
    have hlt : i < alloc.vec.Vec.len v := by scalar_tac
    set lh_i := v.val[i.val]'hlt_v
    have hlh_i : LevelHasherRel R_H sha256 i.val leaves lh_i :=
      hv_above i.val (le_refl _) hlt_v
    have hidx_mut := vec_index_mut_ok v i hlt_v
    obtain ⟨t_new, hnew, _⟩ := hsha.new_ok
    have hi1_bound : i.val + 1 ≤ UScalar.max .Usize := by
      rw [uscalar_max_usize]; have := usize_max_val_ge; omega
    obtain ⟨i1, hi1_add, hi1_val⟩ := uscalar_add_ok i 1#usize hi1_bound
    simp only [UScalar.ofNatCore_val_eq] at hi1_val
    set items := streamedAt sha256 i.val leaves
    have hoff_lh_i : lh_i.offset = u64OfNat (items.length * 32) := hlh_i.2
    have hm_le : items.length ≤ leaves.length := by
      rw [show items = streamedAt sha256 i.val leaves from rfl, streamedAt_length]
      exact Nat.div_le_self _ _
    have hm_off_bound : items.length * 32 ≤ U64.max := by
      have : items.length * 32 ≤ leaves.length * 8192 := by omega
      exact le_trans this hbound
    set dummy_lh : LevelHasher H := { hasher := t_new, offset := 0#u64 }
    have hv1_len : (v.set i dummy_lh).val.length = D + 1 := by
      simp [alloc.vec.Vec.set_val_eq, hv_len]
    have hv1_above : ∀ k, i1.val ≤ k → (hk : k < (v.set i dummy_lh).val.length) →
        LevelHasherRel R_H sha256 k leaves ((v.set i dummy_lh).val[k]'hk) := by
      intro k hk_ge hk_lt
      have hk_ne : i.val ≠ k := by omega
      have hk_lt_orig : k < v.val.length := by omega
      simp only [alloc.vec.Vec.set_val_eq] at hk_lt ⊢
      rw [List.getElem_set_ne hk_ne]
      exact hv_above k (by omega) hk_lt_orig
    by_cases hskip : items.length % 256 = 0 ∧ bubbling = none
    · rcases hskip with ⟨hdvd_items, rfl⟩
      have hmult_true :
          core.num.U64.is_multiple_of (u64OfNat (items.length * 32)) 8192#u64 = ok true := by
        have hmod0 : (u64OfNat (items.length * 32)).val % 8192 = 0 := by
          rw [u64OfNat_val_of_le _ hm_off_bound]; omega
        simp [core.num.U64.is_multiple_of, UScalar.is_multiple_of, hmod0]
      have hbody :
          MerkleRootBuilder.complete_loop.body hInst v none i =
            ok (.cont (v.set i dummy_lh, none, i1)) := by
        unfold MerkleRootBuilder.complete_loop.body
        simp only [hlt, ↓reduceIte, hidx_mut, show v.val[i.val]'hlt_v = lh_i from rfl,
                   hoff_lh_i, hnew, core.mem.replace, lift, cast_u64_BLOCK_SIZE,
                   hmult_true, core.option.Option.is_none, Option.isNone, hi1_add,
                   dummy_lh, bind_ok, uncurry]
      have hiter_next :
          iterLevelStep sha256 i1.val leaves =
            streamedAt sha256 i1.val leaves ++ (none : Option Hash).toList := by
        rw [hi1_val]
        simp only [iterLevelStep, streamedAt, Option.toList]
        rw [hiter]
        simp only [Option.toList, List.append_nil]
        simpa [items] using specLevelStep_of_dvd sha256 (i.val + 1) items hdvd_items
      obtain ⟨root, ih, hroot⟩ :=
        complete_loop_gen_spec hsha D leaves hbound hgtD hltD1
          rem i1 (v.set i dummy_lh) none (by omega) (by omega) hv1_len hv1_above hiter_next
      refine ⟨root, ?_, hroot⟩
      unfold MerkleRootBuilder.complete_loop at ih ⊢
      rw [loop_cont _ (v, none, i) _ hbody]
      exact ih
    · set tail := items.drop ((items.length / 256) * 256)
      set extra := tail ++ bubbling.toList
      have htail_len : tail.length = items.length % 256 := by
        simp [tail, List.length_drop]; omega
      have hextra_le : extra.length ≤ 256 := by
        simp only [extra, List.length_append, htail_len]
        have := Nat.mod_lt items.length (by decide : 0 < 256)
        cases bubbling <;> simp <;> omega
      have hextra_pos : 0 < extra.length := by
        simp only [extra, List.length_append, htail_len]
        cases bubbling with
        | none =>
          have : items.length % 256 ≠ 0 := fun h0 => hskip ⟨h0, rfl⟩
          omega
        | some b => simp
      have hextra_ne : extra ≠ [] := List.ne_nil_of_length_pos hextra_pos
      have hm_plus_bound : (items.length + 1) * 32 ≤ U64.max := by
        have hlt_pow : items.length < 256 ^ (D + 1 - i.val) := by
          rw [show items = streamedAt sha256 i.val leaves from rfl, streamedAt_length]
          have hexp : D + 1 = (D + 1 - i.val) + i.val := by omega
          have hlt' : leaves.length < 256 ^ (D + 1 - i.val) * 256 ^ i.val := by
            rw [← Nat.pow_add, ← hexp]; exact hltD1
          exact Nat.div_lt_of_lt_mul (by rw [Nat.mul_comm]; exact hlt')
        have h7 : D + 1 - i.val ≤ 7 := by omega
        have hp_le : 256 ^ (D + 1 - i.val) ≤ 256 ^ 7 := Nat.pow_le_pow_right (by decide) h7
        have hmax := u64_max_val_eq
        have hc7 : 256 ^ 7 = 72057594037927936 := by decide
        omega
      obtain ⟨t1, i3, hupd_eq, hrel_t1, hi3_val⟩ :=
        complete_bubbling_step hsha (v.set i dummy_lh) i.val leaves lh_i bubbling
          hlh_i hm_plus_bound
      have hcast_bs_nz : (UScalar.cast .U64 BLOCK_SIZE).val ≠ 0 := by
        rw [cast_u64_BLOCK_SIZE]; decide
      obtain ⟨i5_rem, hi5_mod, hi5_mod_val⟩ :=
        uscalar_rem_ok i3 (UScalar.cast .U64 BLOCK_SIZE) hcast_bs_nz
      rw [cast_u64_BLOCK_SIZE] at hi5_mod_val
      simp only [UScalar.ofNatCore_val_eq] at hi5_mod_val
      have hi5_le_usize : i5_rem.val ≤ Usize.max := by
        have := usize_max_val_ge; omega
      set rem_u := UScalar.cast .Usize i5_rem
      have hrem_u_val : rem_u.val = (extra.length * 32) % 8192 := by
        rw [cast_usize_u64_val i5_rem hi5_le_usize, hi5_mod_val, hi3_val]
        change (items.length / 256 * 8192 + extra.length * 32) % 8192 = (extra.length * 32) % 8192
        omega
      set acc1 :=
        specIdentity 8192#u32 (u64OfNat (i.val + 1)) (u64OfNat ((items.length / 256) * 8192)) ++
          concatHashes extra
      have hrel_acc1 : R_H t1 acc1 := by simpa [acc1, extra, tail, items] using hrel_t1
      have hacc1_len : acc1.length = 12 + extra.length * 32 := by simp [acc1]
      set expected_h :=
        specHashInternal sha256 CHILDREN_PER_NODE 8192#u32
          (u64OfNat (i.val + 1)) (u64OfNat ((items.length / 256) * BLOCK_BYTES)) extra
      have hpad_tail :
          (do
            let i5 ← i3 % UScalar.cast .U64 BLOCK_SIZE
            let t2 ←
              if UScalar.cast .Usize i5 != 0#usize then do
                let i6 ← BLOCK_SIZE - UScalar.cast .Usize i5
                update_with_zeros hInst t1 i6
              else ok t1
            let h ← hInst.finalize t2
            let i6 ← i + 1#usize
            ok (ControlFlow.cont (v.set i dummy_lh, some h, i6) :
                ControlFlow _ (Option Hash))) =
          ok (ControlFlow.cont (v.set i dummy_lh, some expected_h, i1)) := by
        simp only [hi5_mod, bind_ok, show UScalar.cast .Usize i5_rem = rem_u from rfl]
        by_cases hfull256 : extra.length = 256
        · -- Bug 01 exact boundary (`extra.length = 256`, `rem_u = 0`): no zero-padding!
          have hrem0 : rem_u = 0#usize := UScalar.eq_of_val_eq (by rw [hrem_u_val, hfull256]; simp)
          have hfin := hsha.finalize_ok t1 acc1 hrel_acc1 (by rw [hacc1_len, hfull256]; omega)
          simp only [hrem0, bne_self_eq_false, Bool.false_eq_true, ↓reduceIte,
                     hfin, hi1_add, bind_ok]
          congr 3
          simp [acc1, expected_h, specHashInternal, specInternalPreimage,
                CHILDREN_PER_NODE, BLOCK_BYTES, hfull256, specZeroPad]
        · have hlt256 : extra.length < 256 := by omega
          have hrem_val_exact : rem_u.val = extra.length * 32 := by
            rw [hrem_u_val]; exact Nat.mod_eq_of_lt (by omega)
          have hrem_nz : (rem_u != 0#usize) = true := by
            apply bne_iff_ne.mpr
            intro hz
            have hv0 : rem_u.val = 0 := by rw [hz]; rfl
            omega
          obtain ⟨pad_u, hsub_pad, hpad_u_val⟩ := uscalar_sub_ok BLOCK_SIZE rem_u (by
            simp [hrem_val_exact]; omega)
          rw [BLOCK_SIZE_val, hrem_val_exact] at hpad_u_val
          obtain ⟨t2, hzupd, hrel_t2⟩ := update_with_zeros_gen_spec hsha t1 acc1 pad_u
            hrel_acc1 (by rw [hacc1_len, hpad_u_val]; omega)
          have hfin := hsha.finalize_ok t2 (acc1 ++ specZeroPad pad_u.val) hrel_t2
            (by rw [List.length_append, hacc1_len, specZeroPad_length, hpad_u_val]; omega)
          simp only [hrem_nz, ↓reduceIte, hsub_pad, hzupd, hfin, hi1_add, bind_ok]
          congr 3
          have hpad_eq : 8192 - extra.length * 32 = (256 - extra.length) * 32 := by omega
          simp [acc1, expected_h, specHashInternal, specInternalPreimage,
                CHILDREN_PER_NODE, BLOCK_BYTES, hpad_u_val, hpad_eq, List.append_assoc]
      have hbody :
          MerkleRootBuilder.complete_loop.body hInst v bubbling i =
            ok (.cont (v.set i dummy_lh, some expected_h, i1)) := by
        unfold MerkleRootBuilder.complete_loop.body
        by_cases hdvd_off : items.length % 256 = 0
        · have hmult_true :
              core.num.U64.is_multiple_of lh_i.offset
                (UScalar.cast .U64 BLOCK_SIZE) = ok true := by
            rw [hoff_lh_i, cast_u64_BLOCK_SIZE]
            have hmod0 : (u64OfNat (items.length * 32)).val % 8192 = 0 := by
              rw [u64OfNat_val_of_le _ hm_off_bound]; omega
            simp [core.num.U64.is_multiple_of, UScalar.is_multiple_of, hmod0]
          have hb_none_false : core.option.Option.is_none bubbling = false := by
            cases bubbling with
            | none => exfalso; exact hskip ⟨hdvd_off, rfl⟩
            | some _ => rfl
          simp only [hlt, ↓reduceIte, hidx_mut, show v.val[i.val]'hlt_v = lh_i from rfl,
                     hnew, core.mem.replace, lift, hmult_true, hb_none_false,
                     Bool.false_eq_true, core.option.Option.take, dummy_lh, bind_ok, uncurry]
          exact bind_ok_eq hupd_eq hpad_tail
        · have hmult_false :
              core.num.U64.is_multiple_of lh_i.offset
                (UScalar.cast .U64 BLOCK_SIZE) = ok false := by
            rw [hoff_lh_i, cast_u64_BLOCK_SIZE]
            have hmod0 : (u64OfNat (items.length * 32)).val % 8192 ≠ 0 := by
              rw [u64OfNat_val_of_le _ hm_off_bound]; omega
            simp [core.num.U64.is_multiple_of, UScalar.is_multiple_of, hmod0]
          simp only [hlt, ↓reduceIte, hidx_mut, show v.val[i.val]'hlt_v = lh_i from rfl,
                     hnew, core.mem.replace, lift, hmult_false, Bool.false_eq_true,
                     core.option.Option.take, dummy_lh, bind_ok, uncurry]
          exact bind_ok_eq hupd_eq hpad_tail
      have hiter_next :
          iterLevelStep sha256 i1.val leaves =
            streamedAt sha256 i1.val leaves ++ (some expected_h).toList := by
        rw [hi1_val]
        simp only [iterLevelStep, streamedAt, Option.toList]
        rw [hiter]
        exact specLevelStep_eq_fullParents_append sha256 (i.val + 1) items bubbling.toList
          hextra_ne hextra_le
      obtain ⟨root, ih, hroot⟩ :=
        complete_loop_gen_spec hsha D leaves hbound hgtD hltD1
          rem i1 (v.set i dummy_lh) (some expected_h)
          (by omega) (by omega) hv1_len hv1_above hiter_next
      refine ⟨root, ?_, hroot⟩
      unfold MerkleRootBuilder.complete_loop at ih ⊢
      rw [loop_cont _ (v, bubbling, i) _ hbody]
      exact ih

/-- Post-block-ingestion suffix of `MerkleRootBuilder.complete`. -/
private theorem complete_finish_gen_spec
    {H C : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {cInst : LeafCollector C} {maxLeaves : Nat} {R_C : C → List Hash → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hcol : ValidLeafCollector cInst maxLeaves R_C)
    (leaves : List Hash) (self : MerkleRootBuilder H C)
    (hinv : BuilderInv sha256 R_H R_C leaves self)
    (hempty_cap : leaves = [] → 0 < maxLeaves) :
    ∃ c',
      (if self.offset = 0#u64 then do
        let s ← lift (Array.to_slice (Std.Array.empty Std.U8))
        let empty_root ← hash_block hInst s 0#u64
        let t ← cInst.add_leaf_hash self.collector empty_root
        ok (empty_root, t)
       else
        match self.root with
        | none => do
          let bubbling_digest ←
            MerkleRootBuilder.complete_loop hInst self.levels none 0#usize
          match bubbling_digest with
          | none => fail Error.panic
          | some root => ok (root, self.collector)
        | some root => ok (root, self.collector)) =
        ok (specRootFromLeaves sha256 leaves, c') ∧
      R_C c' (if leaves.isEmpty then [specHashBlock sha256 [] 0#u64] else leaves) := by
  rcases hinv.htree with ⟨rfl, _, _⟩ |
                         ⟨D, r, hpow, hstrD, _, _, hrootD⟩ |
                         ⟨D, hgtD, hltD1, hlvls_len, hlvls_rel, hrootD⟩
  · -- `leaves = []`
    have hoff0 : self.offset = 0#u64 := by
      rw [hinv.hoffset]; apply UScalar.eq_of_val_eq
      rw [u64OfNat_val_of_le _ (by decide)]; simp
    have harr_empty : (Std.Array.empty Std.U8).to_slice.val = [] := rfl
    have hhb := hash_block_gen_spec sha256 hInst R_H
      (Std.Array.empty Std.U8).to_slice 0#u64 hsha (by simp [harr_empty]) (by simp)
    rw [harr_empty] at hhb
    obtain ⟨c', hc_add, hc_rel⟩ := hcol.add_leaf_hash_ok self.collector []
      (specHashBlock sha256 [] 0#u64) (by simpa using hempty_cap rfl) hinv.hcollector
    refine ⟨c', ?_, by simpa using hc_rel⟩
    simp [hoff0, lift, hhb, hc_add, specRootFromLeaves]
  · -- `leaves.length = 256^D`
    have hpos : 0 < leaves.length := by
      rw [hpow]; exact Nat.pos_of_ne_zero (by positivity)
    have hoff_nz : ¬ (self.offset = 0#u64) := by
      intro hz
      have hv : self.offset.val = 0 := by rw [hz]; rfl
      rw [hinv.hoffset, u64OfNat_val_of_le _ hinv.hbound] at hv
      omega
    simp only [hoff_nz, ↓reduceIte, hrootD]
    have hiterD : iterLevelStep sha256 D leaves = [r] := by
      rw [iterLevelStep_eq_streamedAt_of_pow sha256 D leaves hpow D (le_refl _), hstrD]
    have hroot_eq : specRootFromLeaves sha256 leaves = r := by
      rw [specReduceLevels_eq_of_iterLevelStep sha256 leaves D (fun k hk =>
            iterLevelStep_length_ge_two sha256 k leaves (by
              rw [hpow]; exact Nat.pow_lt_pow_right (by decide) hk)),
          hiterD, specReduceLevels_singleton]
    have hne : leaves.isEmpty = false :=
      List.isEmpty_eq_false_iff.mpr (List.ne_nil_of_length_pos hpos)
    exact ⟨self.collector, by simp [hroot_eq], by simpa [hne] using hinv.hcollector⟩
  · -- `256^D < leaves.length < 256^(D + 1)`
    have hpos : 0 < leaves.length := by
      have : 1 ≤ 256 ^ D := Nat.one_le_pow D 256 (by decide); omega
    have hoff_nz : ¬ (self.offset = 0#u64) := by
      intro hz
      have hv : self.offset.val = 0 := by rw [hz]; rfl
      rw [hinv.hoffset, u64OfNat_val_of_le _ hinv.hbound] at hv
      omega
    simp only [hoff_nz, ↓reduceIte, hrootD]
    obtain ⟨root, hloop, hiter_top⟩ :=
      complete_loop_gen_spec hsha D leaves hinv.hbound hgtD hltD1
        (D + 1) 0#usize self.levels none (by simp) (by simp) hlvls_len
        (by intro k _ hk; exact hlvls_rel k hk)
        (by simp [iterLevelStep, streamedAt])
    rw [hloop, bind_ok]
    have hroot_eq : specRootFromLeaves sha256 leaves = root := by
      rw [specReduceLevels_eq_of_iterLevelStep sha256 leaves (D + 1) (fun k hk =>
            iterLevelStep_length_ge_two sha256 k leaves (by
              have : 256 ^ k ≤ 256 ^ D := Nat.pow_le_pow_right (by decide) (by omega)
              omega)),
          hiter_top, specReduceLevels_singleton]
    have hne : leaves.isEmpty = false :=
      List.isEmpty_eq_false_iff.mpr (List.ne_nil_of_length_pos hpos)
    exact ⟨self.collector, by simp [hroot_eq], by simpa [hne] using hinv.hcollector⟩

/-- Completing `MerkleRootBuilder` on an empty trailing slice `[]` after ingesting `leaves`. -/
private theorem complete_empty_tail_gen_spec
    {H C : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {cInst : LeafCollector C} {maxLeaves : Nat} {R_C : C → List Hash → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hcol : ValidLeafCollector cInst maxLeaves R_C)
    (leaves : List Hash) (self : MerkleRootBuilder H C) (emptySlice : Slice Std.U8)
    (hempty_s : emptySlice.val = [])
    (hinv : BuilderInv sha256 R_H R_C leaves self)
    (hempty_cap : leaves = [] → 0 < maxLeaves) :
    ∃ c',
      MerkleRootBuilder.complete hInst cInst self emptySlice =
        ok (specRootFromLeaves sha256 leaves, c') ∧
      R_C c' (if leaves.isEmpty then [specHashBlock sha256 [] 0#u64] else leaves) := by
  unfold MerkleRootBuilder.complete MerkleRootBuilder.add_blocks
  have hbody_ab :
      MerkleRootBuilder.add_blocks_loop.body hInst cInst emptySlice self 0#usize =
        ok (.done (self, 0#usize)) := by
    unfold MerkleRootBuilder.add_blocks_loop.body
    have hsub0 : Slice.len emptySlice - 0#usize = ok 0#usize := by
      obtain ⟨z, hz, hzv⟩ := uscalar_sub_ok (Slice.len emptySlice) 0#usize (by simp [hempty_s])
      have : z = 0#usize := UScalar.eq_of_val_eq (by simp [hempty_s] at hzv; exact hzv)
      subst this; exact hz
    simp [hsub0]
  have hloop_ab :
      MerkleRootBuilder.add_blocks_loop hInst cInst self emptySlice 0#usize =
        ok (self, 0#usize) := by
    unfold MerkleRootBuilder.add_blocks_loop
    exact loop_done _ (self, 0#usize) _ hbody_ab
  obtain ⟨rem_s, hrem_s, hrem_s_val⟩ :=
    slice_range_from_ok emptySlice 0#usize (by simp [hempty_s])
  have hrem_empty : rem_s.val = [] := by simpa [hempty_s] using hrem_s_val
  have his_empty : core.slice.Slice.is_empty rem_s = ok true := by
    simp [core.slice.Slice.is_empty, Slice.length, hrem_empty]
  rw [hloop_ab]
  simp only [bind_ok, uncurry]
  rw [hrem_s]
  simp only [bind_ok, uncurry, his_empty, ↓reduceIte]
  exact complete_finish_gen_spec hsha hcol leaves self hinv hempty_cap

private theorem rebuild_root_loop_gen_spec
    {H : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (leaf_hashes : Slice Hash)
    (hbound : leaf_hashes.val.length * 8192 ≤ U64.max) :
    ∀ (rem : Nat) (i : Std.Usize) (b : MerkleRootBuilder H NoopLeafCollector),
      i.val ≤ leaf_hashes.val.length →
      leaf_hashes.val.length - i.val = rem →
      BuilderInv sha256 R_H (fun _ _ => True) (leaf_hashes.val.take i.val) b →
      ∃ b_out,
        rebuild_root_loop hInst leaf_hashes b i = ok b_out ∧
        BuilderInv sha256 R_H (fun _ _ => True) leaf_hashes.val b_out
  | 0, i, b, _, _, hinv => by
    have heq : i.val = leaf_hashes.val.length := by omega
    have hbody :
        rebuild_root_loop.body hInst leaf_hashes b i = ok (.done b) := by
      unfold rebuild_root_loop.body
      have hlt : ¬ (i < Slice.len leaf_hashes) := by scalar_tac
      simp [hlt]
    refine ⟨b, ?_, by simpa [heq] using hinv⟩
    unfold rebuild_root_loop
    exact loop_done _ (b, i) _ hbody
  | rem + 1, i, b, _, hrem, hinv => by
    have hlt_nat : i.val < leaf_hashes.val.length := by omega
    have hlt : i < Slice.len leaf_hashes := by scalar_tac
    have hidx : Slice.index_usize leaf_hashes i = ok (leaf_hashes.val[i.val]'hlt_nat) := by
      simp [Slice.index_usize, List.getElem?_eq_getElem hlt_nat]
    have htake_len : (leaf_hashes.val.take i.val).length = i.val :=
      List.length_take_of_le (le_of_lt hlt_nat)
    obtain ⟨b1, hpush, hinv1⟩ :=
      push_hash_gen_inv hsha (validLeafCollector_noop (leaf_hashes.val.length + 1))
        (leaf_hashes.val.take i.val) b (leaf_hashes.val[i.val]'hlt_nat) hinv
        (by rw [htake_len]
            exact le_trans (Nat.mul_le_mul_right 8192 (by omega)) hbound)
        (by rw [htake_len]; omega)
    have hi1_bound : i.val + 1 ≤ UScalar.max .Usize := by
      rw [uscalar_max_usize]; have := leaf_hashes.property; omega
    obtain ⟨i1, hi1_add, hi1_val⟩ := uscalar_add_ok i 1#usize hi1_bound
    simp only [UScalar.ofNatCore_val_eq] at hi1_val
    have hbody :
        rebuild_root_loop.body hInst leaf_hashes b i = ok (.cont (b1, i1)) := by
      unfold rebuild_root_loop.body
      simp only [hlt, ↓reduceIte, hidx, hpush, hi1_add, bind_ok]
    have htake_succ :
        leaf_hashes.val.take i1.val =
          leaf_hashes.val.take i.val ++ [leaf_hashes.val[i.val]'hlt_nat] := by
      rw [hi1_val, List.take_add_one, List.getElem?_eq_getElem hlt_nat]
      simp
    obtain ⟨b_out, ih, hinv_out⟩ :=
      rebuild_root_loop_gen_spec hsha leaf_hashes hbound rem i1 b1
        (by omega) (by omega) (by simpa [htake_succ] using hinv1)
    refine ⟨b_out, ?_, hinv_out⟩
    unfold rebuild_root_loop at ih ⊢
    rw [loop_cont _ (b, i) _ hbody]
    exact ih

/-- `rebuild_root` succeeds without panicking for any `ValidSha256State` when
    `leaf_hashes.val.length * BLOCK_BYTES ≤ U64.max` and returns
    `specRootFromLeaves sha256 leaf_hashes.val`. -/
theorem rebuild_root_gen_spec
    {H : Type} (sha256 : List Std.U8 → Hash)
    (hInst : Sha256State H) (R_H : H → List Std.U8 → Prop)
    (leaf_hashes : Slice Hash)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hbound : leaf_hashes.val.length * BLOCK_BYTES ≤ U64.max) :
    rebuild_root hInst leaf_hashes =
      ok (specRootFromLeaves sha256 leaf_hashes.val) := by
  unfold rebuild_root
  have hbound8192 : leaf_hashes.val.length * 8192 ≤ U64.max := by
    simpa [BLOCK_BYTES] using hbound
  obtain ⟨b0, hb0, hinv0⟩ := merkle_new_inv sha256 hInst R_H
    NoopLeafCollector.Insts.Fuchsia_merkle_coreLeafCollector (fun _ _ => True) () trivial
  obtain ⟨b1, hloop, hinv1⟩ :=
    rebuild_root_loop_gen_spec hsha leaf_hashes hbound8192
      leaf_hashes.val.length 0#usize b0 (Nat.zero_le _) (by simp) (by simpa using hinv0)
  obtain ⟨_, hfin, _⟩ :=
    complete_empty_tail_gen_spec hsha (validLeafCollector_noop (leaf_hashes.val.length + 1))
      leaf_hashes.val b1 (Std.Array.empty Std.U8).to_slice rfl hinv1 (fun _ => by omega)
  simp only [hb0, hloop, lift, hfin, bind_ok, uncurry]

-- ============================================================================
-- 5. `fullBlockHashes`, `add_block`, `add_blocks`, and `merkle_root_builder_complete_gen_spec`
-- ============================================================================

/-- Leaf hashes of all full 8,192-byte blocks in `bytes`. -/
def fullBlockHashes (sha256 : List Std.U8 → Hash) (bytes : List Std.U8) : List Hash :=
  (chunksOf BLOCK_BYTES (by decide) (bytes.take ((bytes.length / 8192) * 8192))).mapIdx
    fun idx blk => specHashBlock sha256 blk (u64OfNat (idx * BLOCK_BYTES))

private theorem fullBlockHashes_nil (sha256 : List Std.U8 → Hash) :
    fullBlockHashes sha256 [] = [] := by
  simp [fullBlockHashes]

private theorem fullBlockHashes_length (sha256 : List Std.U8 → Hash) (bytes : List Std.U8) :
    (fullBlockHashes sha256 bytes).length = bytes.length / 8192 := by
  unfold fullBlockHashes
  have hle : (bytes.length / 8192) * 8192 ≤ bytes.length := Nat.div_mul_le_self _ _
  simp [chunksOf_length, List.length_take_of_le hle, BLOCK_BYTES]
  omega

private theorem fullBlockHashes_append_block
    (sha256 : List Std.U8 → Hash) (bytes blk : List Std.U8)
    (hbytes : bytes.length % 8192 = 0) (hblk : blk.length = 8192) :
    fullBlockHashes sha256 (bytes ++ blk) =
      fullBlockHashes sha256 bytes ++
        [specHashBlock sha256 blk (u64OfNat ((bytes.length / 8192) * BLOCK_BYTES))] := by
  unfold fullBlockHashes
  set q := bytes.length / 8192
  have hbytes_len : bytes.length = q * 8192 := by omega
  have htot_div : ((bytes ++ blk).length / 8192) * 8192 = (bytes ++ blk).length := by
    simp [hbytes_len, hblk]; omega
  rw [htot_div, List.take_length, ← hbytes_len, List.take_length]
  rw [chunksOf_append_mul BLOCK_BYTES (by decide) q bytes blk hbytes_len, List.mapIdx_append]
  have hclen : (chunksOf BLOCK_BYTES (by decide) bytes).length = q := by
    rw [chunksOf_length, hbytes_len]; simp [BLOCK_BYTES]; omega
  have hsingle : chunksOf BLOCK_BYTES (by decide) blk = [blk] :=
    chunksOf_single BLOCK_BYTES (by decide) blk
      (by intro h; simp [h] at hblk) (by simp [BLOCK_BYTES, hblk])
  rw [hclen, hsingle]
  simp only [List.mapIdx_cons, List.mapIdx_nil]
  congr 2
  simp [q, BLOCK_BYTES, ← hbytes_len]

private theorem fullBlockHashes_take_aligned (sha256 : List Std.U8 → Hash) (bytes : List Std.U8) :
    fullBlockHashes sha256 (bytes.take ((bytes.length / 8192) * 8192)) =
    fullBlockHashes sha256 bytes := by
  unfold fullBlockHashes
  have hle : (bytes.length / 8192) * 8192 ≤ bytes.length := Nat.div_mul_le_self _ _
  have hlen : (bytes.take ((bytes.length / 8192) * 8192)).length = (bytes.length / 8192) * 8192 :=
    List.length_take_of_le hle
  rw [hlen, Nat.mul_div_cancel _ (by decide), List.take_take, Nat.min_self]

private theorem aligned_add_tail_div_mul (p d pos : Nat)
    (hp : p % 8192 = 0) (hle : pos ≤ d) (hpm : pos % 8192 = 0) (hlt : d - pos < 8192) :
    ((p + d) / 8192) * 8192 = p + pos := by
  omega

private theorem specLeafHashes_of_full_and_rem
    (sha256 : List Std.U8 → Hash) (bytes : List Std.U8) (hne : bytes ≠ []) :
    specLeafHashes sha256 bytes =
      if bytes.length % 8192 = 0 then
        fullBlockHashes sha256 bytes
      else
        fullBlockHashes sha256 bytes ++
          [specHashBlock sha256 (bytes.drop ((bytes.length / 8192) * 8192))
            (u64OfNat ((bytes.length / 8192) * BLOCK_BYTES))] := by
  have his_emp : bytes.isEmpty = false := List.isEmpty_eq_false_iff.mpr hne
  unfold specLeafHashes fullBlockHashes
  rw [his_emp]
  simp only [Bool.false_eq_true, ↓reduceIte]
  set q := bytes.length / 8192
  by_cases hdvd : bytes.length % 8192 = 0
  · simp only [hdvd, ↓reduceIte]
    have hlen : q * 8192 = bytes.length := by omega
    rw [hlen, List.take_length]
  · simp only [hdvd, ↓reduceIte]
    have hsplit : bytes = bytes.take (q * 8192) ++ bytes.drop (q * 8192) :=
      (List.take_append_drop (q * 8192) bytes).symm
    have htake_len : (bytes.take (q * 8192)).length = q * 8192 :=
      List.length_take_of_le (Nat.div_mul_le_self _ _)
    have hdrop_ne : bytes.drop (q * 8192) ≠ [] :=
      List.ne_nil_of_length_pos (by simp [List.length_drop]; omega)
    have hdrop_le : (bytes.drop (q * 8192)).length ≤ BLOCK_BYTES := by
      simp [List.length_drop, BLOCK_BYTES]; omega
    conv => lhs; rw [hsplit]
    rw [chunksOf_append_mul BLOCK_BYTES (by decide) q _ _ htake_len, List.mapIdx_append]
    have hclen : (chunksOf BLOCK_BYTES (by decide) (bytes.take (q * 8192))).length = q := by
      rw [chunksOf_length, htake_len]; simp [BLOCK_BYTES]; omega
    have hsingle : chunksOf BLOCK_BYTES (by decide) (bytes.drop (q * 8192)) = [bytes.drop (q * 8192)] :=
      chunksOf_single BLOCK_BYTES (by decide) _ hdrop_ne hdrop_le
    rw [hclen, hsingle]
    simp [q]

private theorem ceil_blocks_le_specLeafHashes_length
    (sha256 : List Std.U8 → Hash) (blob : List Std.U8) :
    (blob.length + 8191) / 8192 ≤ (specLeafHashes sha256 blob).length ∧
    0 < (specLeafHashes sha256 blob).length := by
  unfold specLeafHashes
  cases hemp : blob.isEmpty
  · simp only [Bool.false_eq_true, ↓reduceIte, List.length_mapIdx, chunksOf_length, BLOCK_BYTES]
    have hne : blob ≠ [] := by
      intro hnil; simp [hnil] at hemp
    have hpos : 0 < blob.length := List.length_pos_iff.mpr hne
    omega
  · simp only [↓reduceIte, List.length_singleton]
    have hnil : blob = [] := List.isEmpty_iff.mp hemp
    subst hnil
    simp

theorem add_block_gen_inv
    {H C : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {cInst : LeafCollector C} {maxLeaves : Nat} {R_C : C → List Hash → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hcol : ValidLeafCollector cInst maxLeaves R_C)
    (leaves : List Hash) (self : MerkleRootBuilder H C) (block : Slice Std.U8)
    (hblk : block.val.length = 8192)
    (hinv : BuilderInv sha256 R_H R_C leaves self)
    (hbound' : (leaves.length + 1) * 8192 ≤ U64.max)
    (hcap : leaves.length < maxLeaves) :
    ∃ self',
      MerkleRootBuilder.add_block hInst cInst self block = ok self' ∧
      BuilderInv sha256 R_H R_C
        (leaves ++ [specHashBlock sha256 block.val (u64OfNat (leaves.length * 8192))]) self' := by
  unfold MerkleRootBuilder.add_block
  have hm : Slice.len block = BLOCK_SIZE := UScalar.eq_of_val_eq (by simp [hblk])
  have hoff_mod : self.offset.val % BLOCK_BYTES = 0 := by
    rw [hinv.hoffset, u64OfNat_val_of_le _ hinv.hbound]
    simp [BLOCK_BYTES]
  have hhb := hash_block_gen_spec sha256 hInst R_H block self.offset
    hsha (by simp [BLOCK_BYTES, hblk]) hoff_mod
  rw [hinv.hoffset] at hhb
  obtain ⟨self', hpush, hinv'⟩ :=
    push_hash_gen_inv hsha hcol leaves self
      (specHashBlock sha256 block.val (u64OfNat (leaves.length * 8192)))
      hinv hbound' hcap
  refine ⟨self', ?_, hinv'⟩
  simp only [massert, hm, ↓reduceIte, hinv.hoffset, hhb, hpush, bind_ok]

theorem add_blocks_loop_gen_spec
    {H C : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {cInst : LeafCollector C} {maxLeaves : Nat} {R_C : C → List Hash → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hcol : ValidLeafCollector cInst maxLeaves R_C)
    (prefixBytes : List Std.U8) (hprefix_aligned : prefixBytes.length % 8192 = 0)
    (data : Slice Std.U8)
    (hbound : ((prefixBytes.length + data.val.length) / 8192) * 8192 ≤ U64.max)
    (hcap : (prefixBytes.length + data.val.length) / 8192 ≤ maxLeaves) :
    ∀ (steps : Nat) (pos : Std.Usize) (self : MerkleRootBuilder H C),
      pos.val ≤ data.val.length →
      pos.val % 8192 = 0 →
      (data.val.length - pos.val) / 8192 = steps →
      BuilderInv sha256 R_H R_C
        (fullBlockHashes sha256 (prefixBytes ++ data.val.take pos.val)) self →
      ∃ self' endPos,
        MerkleRootBuilder.add_blocks_loop hInst cInst self data pos = ok (self', endPos) ∧
        endPos.val = (data.val.length / 8192) * 8192 ∧
        BuilderInv sha256 R_H R_C (fullBlockHashes sha256 (prefixBytes ++ data.val)) self'
  | 0, pos, self, hpos_le, hpos_mod, hsteps, hinv => by
    have hlt8192 : data.val.length - pos.val < 8192 := by omega
    obtain ⟨rem_len, hsub, hrem_val⟩ := uscalar_sub_ok (Slice.len data) pos (by simp; omega)
    simp only [Slice.len_val, Slice.length] at hrem_val
    have hbody :
        MerkleRootBuilder.add_blocks_loop.body hInst cInst data self pos =
          ok (.done (self, pos)) := by
      unfold MerkleRootBuilder.add_blocks_loop.body
      have hge : ¬ (rem_len >= BLOCK_SIZE) := by scalar_tac
      simp only [hsub, bind_ok, hge, ↓reduceIte]
    have hpos_eq : pos.val = (data.val.length / 8192) * 8192 := by omega
    have hfb_eq :
        fullBlockHashes sha256 (prefixBytes ++ data.val.take pos.val) =
        fullBlockHashes sha256 (prefixBytes ++ data.val) := by
      unfold fullBlockHashes
      have hlen1 : ((prefixBytes ++ data.val.take pos.val).length / 8192) * 8192 =
                   (prefixBytes ++ data.val.take pos.val).length := by
        rw [List.length_append, List.length_take_of_le hpos_le]
        exact aligned_add_tail_div_mul prefixBytes.length pos.val pos.val
          hprefix_aligned (le_refl _) hpos_mod (by omega)
      have hlen2 : ((prefixBytes ++ data.val).length / 8192) * 8192 =
                   prefixBytes.length + pos.val := by
        rw [List.length_append]
        exact aligned_add_tail_div_mul prefixBytes.length data.val.length pos.val
          hprefix_aligned hpos_le hpos_mod hlt8192
      have htake2 : (prefixBytes ++ data.val).take (prefixBytes.length + pos.val) =
                    prefixBytes ++ data.val.take pos.val := by
        rw [List.take_append]
        have ht1 : prefixBytes.take (prefixBytes.length + pos.val) = prefixBytes :=
          List.take_of_length_le (Nat.le_add_right prefixBytes.length pos.val)
        have hsub_p : prefixBytes.length + pos.val - prefixBytes.length = pos.val :=
          Nat.add_sub_cancel_left prefixBytes.length pos.val
        rw [ht1, hsub_p]
      rw [hlen1, List.take_length, hlen2, htake2]
    refine ⟨self, pos, ?_, hpos_eq, hfb_eq ▸ hinv⟩
    unfold MerkleRootBuilder.add_blocks_loop
    exact loop_done _ (self, pos) _ hbody
  | steps + 1, pos, self, hpos_le, hpos_mod, hsteps, hinv => by
    have hge_nat : 8192 ≤ data.val.length - pos.val := by omega
    obtain ⟨rem_len, hsub, hrem_val⟩ := uscalar_sub_ok (Slice.len data) pos (by simp; omega)
    simp only [Slice.len_val, Slice.length] at hrem_val
    have hge : rem_len >= BLOCK_SIZE := by scalar_tac
    obtain ⟨pos2, hadd_pos, hpos2_val⟩ := uscalar_add_ok pos BLOCK_SIZE (by
      rw [uscalar_max_usize, BLOCK_SIZE_val]; have := data.property; omega)
    rw [BLOCK_SIZE_val] at hpos2_val
    obtain ⟨blkSlice, hblk_s, hblk_val⟩ :=
      slice_range_ok data pos pos2 (by omega) (by omega)
    have hblk_len : blkSlice.val.length = 8192 := by
      rw [hblk_val, List.length_take, List.length_drop]; omega
    set curBytes := prefixBytes ++ data.val.take pos.val
    have hcur_len : curBytes.length = prefixBytes.length + pos.val := by
      simp [curBytes, List.length_take_of_le hpos_le]
    have hcur_mod : curBytes.length % 8192 = 0 := by omega
    have hfb_len : (fullBlockHashes sha256 curBytes).length = curBytes.length / 8192 :=
      fullBlockHashes_length sha256 curBytes
    have htot_blocks : curBytes.length / 8192 + 1 ≤ (prefixBytes.length + data.val.length) / 8192 := by
      omega
    obtain ⟨self1, hadd_blk, hinv1⟩ :=
      add_block_gen_inv hsha hcol (fullBlockHashes sha256 curBytes) self blkSlice hblk_len hinv
        (by rw [hfb_len]; exact le_trans (Nat.mul_le_mul_right 8192 htot_blocks) hbound)
        (by rw [hfb_len]; omega)
    have hbody :
        MerkleRootBuilder.add_blocks_loop.body hInst cInst data self pos =
          ok (.cont (self1, pos2)) := by
      unfold MerkleRootBuilder.add_blocks_loop.body
      dsimp only
      rw [hsub]
      simp only [bind_ok, hge, ↓reduceIte, hadd_pos]
      rw [hblk_s]
      simp only [bind_ok, hadd_blk]
    have hnext_bytes :
        prefixBytes ++ data.val.take pos2.val = curBytes ++ blkSlice.val := by
      simp only [curBytes, hblk_val, hpos2_val, Nat.add_sub_cancel_left]
      rw [List.append_assoc]
      congr 1
      have hsplit := List.take_append_drop pos.val (data.val.take (pos.val + 8192))
      rw [List.take_take, Nat.min_eq_left (by omega), List.drop_take] at hsplit
      simpa [Nat.add_sub_cancel_left] using hsplit.symm
    have hinv1' :
        BuilderInv sha256 R_H R_C
          (fullBlockHashes sha256 (prefixBytes ++ data.val.take pos2.val)) self1 := by
      rw [hnext_bytes, fullBlockHashes_append_block sha256 curBytes blkSlice.val hcur_mod hblk_len]
      simpa [hfb_len, BLOCK_BYTES] using hinv1
    obtain ⟨self', endPos, ih, hend, hinv_out⟩ :=
      add_blocks_loop_gen_spec hsha hcol prefixBytes hprefix_aligned data hbound hcap
        steps pos2 self1 (by omega) (by omega) (by omega) hinv1'
    refine ⟨self', endPos, ?_, hend, hinv_out⟩
    unfold MerkleRootBuilder.add_blocks_loop at ih ⊢
    rw [loop_cont _ (self, pos) _ hbody]
    exact ih

theorem add_blocks_gen_inv
    {H C : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {cInst : LeafCollector C} {maxLeaves : Nat} {R_C : C → List Hash → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hcol : ValidLeafCollector cInst maxLeaves R_C)
    (prefixBytes : List Std.U8) (hprefix_aligned : prefixBytes.length % 8192 = 0)
    (self : MerkleRootBuilder H C) (data : Slice Std.U8)
    (hinv : BuilderInv sha256 R_H R_C (fullBlockHashes sha256 prefixBytes) self)
    (hbound : ((prefixBytes.length + data.val.length) / 8192) * 8192 ≤ U64.max)
    (hcap : (prefixBytes.length + data.val.length) / 8192 ≤ maxLeaves) :
    ∃ remSlice self',
      MerkleRootBuilder.add_blocks hInst cInst self data = ok (remSlice, self') ∧
      remSlice.val = data.val.drop ((data.val.length / 8192) * 8192) ∧
      BuilderInv sha256 R_H R_C (fullBlockHashes sha256 (prefixBytes ++ data.val)) self' := by
  unfold MerkleRootBuilder.add_blocks
  obtain ⟨self', endPos, hloop, hend_val, hinv'⟩ :=
    add_blocks_loop_gen_spec hsha hcol prefixBytes hprefix_aligned data hbound hcap
      (data.val.length / 8192) 0#usize self (Nat.zero_le _) (by simp) (by simp)
      (by simpa using hinv)
  obtain ⟨remSlice, hrem, hrem_val⟩ :=
    slice_range_from_ok data endPos (by rw [hend_val]; exact Nat.div_mul_le_self _ _)
  refine ⟨remSlice, self', ?_, by simpa [hend_val] using hrem_val, hinv'⟩
  rw [hloop]
  simp only [bind_ok, uncurry]
  rw [hrem]
  simp only [bind_ok]

/-- General `MerkleRootBuilder.complete` theorem from an aligned prefix `prefixBytes` plus
    trailing slice `data`. -/
theorem complete_from_aligned_prefix_gen_spec
    {H C : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {cInst : LeafCollector C} {maxLeaves : Nat} {R_C : C → List Hash → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hcol : ValidLeafCollector cInst maxLeaves R_C)
    (prefixBytes : List Std.U8) (hprefix_aligned : prefixBytes.length % 8192 = 0)
    (self : MerkleRootBuilder H C) (data : Slice Std.U8)
    (hinv : BuilderInv sha256 R_H R_C (fullBlockHashes sha256 prefixBytes) self)
    (hbound : (((prefixBytes.length + data.val.length) + 8191) / 8192) * 8192 ≤ U64.max)
    (hcap : (specLeafHashes sha256 (prefixBytes ++ data.val)).length ≤ maxLeaves) :
    ∃ c',
      MerkleRootBuilder.complete hInst cInst self data =
        ok (specMerkleRoot sha256 (prefixBytes ++ data.val), c') ∧
      R_C c' (specLeafHashes sha256 (prefixBytes ++ data.val)) := by
  set total := prefixBytes ++ data.val
  have ⟨hceil_le_sl, hsl_pos⟩ := ceil_blocks_le_specLeafHashes_length sha256 total
  have hceil_cap : ((prefixBytes.length + data.val.length) + 8191) / 8192 ≤ maxLeaves := by
    rw [← List.length_append]
    exact le_trans hceil_le_sl hcap
  have hmax_pos : 0 < maxLeaves := lt_of_lt_of_le hsl_pos hcap
  have hdiv_le_ceil :
      (prefixBytes.length + data.val.length) / 8192 ≤
      ((prefixBytes.length + data.val.length) + 8191) / 8192 :=
    Nat.div_le_div_right (by omega)
  obtain ⟨remSlice, self1, hab, hrem_val, hinv1⟩ :=
    add_blocks_gen_inv hsha hcol prefixBytes hprefix_aligned self data hinv
      (le_trans (Nat.mul_le_mul_right 8192 hdiv_le_ceil) hbound)
      (le_trans hdiv_le_ceil hceil_cap)
  unfold MerkleRootBuilder.complete
  rw [hab]
  simp only [bind_ok, uncurry]
  have hrem_eq_total_drop :
      remSlice.val = total.drop ((total.length / 8192) * 8192) := by
    rw [hrem_val]
    have htot_q : (total.length / 8192) * 8192 =
                  prefixBytes.length + (data.val.length / 8192) * 8192 := by
      simp [total]; omega
    rw [htot_q, List.drop_append]
    simp
  by_cases hrem_emp : remSlice.val.length = 0
  · have his_emp : core.slice.Slice.is_empty remSlice = ok true := by
      simp [core.slice.Slice.is_empty, Slice.length, hrem_emp]
    have hdvd_tot : total.length % 8192 = 0 := by
      have hlen_drop : remSlice.val.length = total.length - (total.length / 8192) * 8192 := by
        rw [hrem_eq_total_drop, List.length_drop]
      omega
    obtain ⟨c', hfin_emp, hc'_rel⟩ :=
      complete_finish_gen_spec hsha hcol (fullBlockHashes sha256 total) self1 hinv1
        (fun _ => hmax_pos)
    rw [his_emp]
    simp only [bind_ok, ↓reduceIte]
    by_cases htot_nil : total = []
    · rw [htot_nil, fullBlockHashes_nil] at hfin_emp hc'_rel
      refine ⟨c', ?_, ?_⟩
      · exact hfin_emp.trans (congrArg ok (Prod.ext
          (by simp [htot_nil, specMerkleRoot, specLeafHashes, specRootFromLeaves]) rfl))
      · simpa [htot_nil, specLeafHashes] using hc'_rel
    · have hsl : specLeafHashes sha256 total = fullBlockHashes sha256 total := by
        rw [specLeafHashes_of_full_and_rem sha256 total htot_nil]
        simp [hdvd_tot]
      have hfb_ne : (fullBlockHashes sha256 total).isEmpty = false := by
        apply List.isEmpty_eq_false_iff.mpr
        apply List.ne_nil_of_length_pos
        rw [fullBlockHashes_length]
        have hpos_tot : 0 < total.length := List.length_pos_iff.mpr htot_nil
        omega
      refine ⟨c', ?_, ?_⟩
      · exact hfin_emp.trans (congrArg ok (Prod.ext
          (by simp [specMerkleRoot, hsl]) rfl))
      · simpa [hsl, hfb_ne] using hc'_rel
  · -- Non-empty final trailing block (`1 ≤ remSlice.length < 8192`)
    have his_emp : core.slice.Slice.is_empty remSlice = ok false := by
      simp [core.slice.Slice.is_empty, Slice.length, hrem_emp]
    have hrem_lt : remSlice.val.length < 8192 := by
      rw [hrem_val, List.length_drop]; omega
    have hndvd_tot : total.length % 8192 ≠ 0 := by
      have hlen_drop : remSlice.val.length = total.length - (total.length / 8192) * 8192 := by
        rw [hrem_eq_total_drop, List.length_drop]
      omega
    have htot_ne : total ≠ [] := by
      intro hnil; simp [hnil] at hndvd_tot
    have hfb_len : (fullBlockHashes sha256 total).length = total.length / 8192 :=
      fullBlockHashes_length sha256 total
    have hceil_eq : (total.length + 8191) / 8192 = total.length / 8192 + 1 := by omega
    have hoff1_mod : self1.offset.val % BLOCK_BYTES = 0 := by
      rw [hinv1.hoffset, u64OfNat_val_of_le _ hinv1.hbound]
      simp [BLOCK_BYTES]
    have hhb := hash_block_gen_spec sha256 hInst R_H remSlice self1.offset hsha
      (by simp [BLOCK_BYTES]; omega) hoff1_mod
    obtain ⟨self2, hpush, hinv2⟩ :=
      push_hash_gen_inv hsha hcol (fullBlockHashes sha256 total) self1
        (specHashBlock sha256 remSlice.val (u64OfNat ((total.length / 8192) * 8192)))
        hinv1
        (by rw [hfb_len, ← hceil_eq]; simpa [total] using hbound)
        (by rw [hfb_len]; simp only [total, List.length_append] at hceil_eq; omega)
    have hsl :
        specLeafHashes sha256 total =
          fullBlockHashes sha256 total ++
            [specHashBlock sha256 remSlice.val (u64OfNat ((total.length / 8192) * 8192))] := by
      rw [specLeafHashes_of_full_and_rem sha256 total htot_ne]
      simp [hndvd_tot, hrem_eq_total_drop, BLOCK_BYTES]
    have hinv2' : BuilderInv sha256 R_H R_C (specLeafHashes sha256 total) self2 :=
      hsl ▸ hinv2
    obtain ⟨c', hfin_emp, hc'_rel⟩ :=
      complete_finish_gen_spec hsha hcol (specLeafHashes sha256 total) self2 hinv2'
        (fun _ => hmax_pos)
    rw [his_emp]
    simp only [bind_ok, Bool.false_eq_true, ↓reduceIte]
    rw [hhb]
    simp only [bind_ok, hinv1.hoffset, show prefixBytes ++ data.val = total from rfl, hfb_len]
    rw [hpush]
    simp only [bind_ok]
    have hsl_ne : (specLeafHashes sha256 total).isEmpty = false := by
      rw [hsl]; simp
    refine ⟨c', ?_, ?_⟩
    · exact hfin_emp.trans (congrArg ok (Prod.ext (by simp [specMerkleRoot]) rfl))
    · simpa [hsl_ne] using hc'_rel

/-- Single-pass `MerkleRootBuilder.new` + `MerkleRootBuilder.complete` functional specification
    for any `ValidSha256State` and `ValidLeafCollector`. -/
theorem merkle_root_builder_complete_gen_spec
    {H C : Type} (sha256 : List Std.U8 → Hash)
    (hInst : Sha256State H) (R_H : H → List Std.U8 → Prop)
    (cInst : LeafCollector C) (maxLeaves : Nat)
    (R_C : C → List Hash → Prop)
    (c₀ : C) (data : Slice Std.U8)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hcol : ValidLeafCollector cInst maxLeaves R_C)
    (hc₀ : R_C c₀ [])
    (hcap : (specLeafHashes sha256 data.val).length ≤ maxLeaves)
    (hbound : ((data.val.length + (BLOCK_BYTES - 1)) / BLOCK_BYTES) * BLOCK_BYTES ≤ U64.max) :
    ∃ c',
      (do
        let b ← MerkleRootBuilder.new hInst cInst c₀
        MerkleRootBuilder.complete hInst cInst b data) =
        ok (specMerkleRoot sha256 data.val, c') ∧
      R_C c' (specLeafHashes sha256 data.val) := by
  obtain ⟨b0, hb0, hinv0⟩ := merkle_new_inv sha256 hInst R_H cInst R_C c₀ hc₀
  have hbound8192 : ((data.val.length + 8191) / 8192) * 8192 ≤ U64.max := by
    simpa [BLOCK_BYTES] using hbound
  obtain ⟨c', hfin, hc'⟩ :=
    complete_from_aligned_prefix_gen_spec hsha hcol [] (by simp) b0 data
      (by simpa [fullBlockHashes_nil] using hinv0)
      (by simpa using hbound8192)
      (by simpa using hcap)
  refine ⟨c', ?_, by simpa using hc'⟩
  simp only [hb0, List.nil_append, hfin, bind_ok]

-- ============================================================================
-- 6. Polymorphic `BufferedMerkleRootBuilder` Streaming & Chunk-Size Invariance
-- ============================================================================

/-- Polymorphic streaming invariant of `BufferedMerkleRootBuilder H C` after receiving `bytes`:
    1. `buffer` holds the unaligned trailing `< 8,192` bytes `bytes.drop ((bytes.length / 8192) * 8192)`.
    2. `builder` satisfies `BuilderInv` for all full 8,192-byte blocks `fullBlockHashes sha256 bytes`. -/
structure BufferedInv {H C : Type}
    (sha256 : List Std.U8 → Hash)
    (R_H : H → List Std.U8 → Prop)
    (R_C : C → List Hash → Prop)
    (bytes : List Std.U8)
    (self : BufferedMerkleRootBuilder H C) : Prop where
  hbuf : self.buffer.val = bytes.drop ((bytes.length / 8192) * 8192)
  hbuilder : BuilderInv sha256 R_H R_C (fullBlockHashes sha256 bytes) self.builder

theorem buffered_new_gen_inv
    {H C : Type} (sha256 : List Std.U8 → Hash)
    (hInst : Sha256State H) (R_H : H → List Std.U8 → Prop)
    (cInst : LeafCollector C) (R_C : C → List Hash → Prop)
    (c₀ : C) (hc₀ : R_C c₀ []) :
    ∃ b, BufferedMerkleRootBuilder.new hInst cInst c₀ = ok b ∧
      BufferedInv sha256 R_H R_C [] b := by
  unfold BufferedMerkleRootBuilder.new
  obtain ⟨mrb, hmrb, hinv⟩ := merkle_new_inv sha256 hInst R_H cInst R_C c₀ hc₀
  refine ⟨{ buffer := alloc.vec.Vec.new Std.U8, builder := mrb }, by simp [hmrb], ?_⟩
  exact ⟨by simp, by simpa [fullBlockHashes_nil] using hinv⟩

theorem buffered_write_gen_inv
    {H C : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {cInst : LeafCollector C} {maxLeaves : Nat} {R_C : C → List Hash → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hcol : ValidLeafCollector cInst maxLeaves R_C)
    (bytes : List Std.U8) (self : BufferedMerkleRootBuilder H C) (data : Slice Std.U8)
    (hinv : BufferedInv sha256 R_H R_C bytes self)
    (hbound : (((bytes.length + data.val.length) / 8192) * 8192) ≤ U64.max)
    (hcap : (bytes.length + data.val.length) / 8192 ≤ maxLeaves) :
    ∃ self',
      BufferedMerkleRootBuilder.write hInst cInst self data = ok self' ∧
      BufferedInv sha256 R_H R_C (bytes ++ data.val) self' := by
  rcases hinv with ⟨hinv_buf, hinv_builder⟩
  unfold BufferedMerkleRootBuilder.write
  set q := bytes.length / 8192
  set alignedBytes := bytes.take (q * 8192)
  set remBytes := bytes.drop (q * 8192)
  have hbytes_split : alignedBytes ++ remBytes = bytes := List.take_append_drop (q * 8192) bytes
  have haligned_len : alignedBytes.length = q * 8192 :=
    List.length_take_of_le (Nat.div_mul_le_self _ _)
  have haligned_mod : alignedBytes.length % 8192 = 0 := by omega
  have hrem_len : remBytes.length = bytes.length % 8192 := by
    simp [remBytes, List.length_drop]; omega
  have hrem_lt : remBytes.length < 8192 := by rw [hrem_len]; exact Nat.mod_lt _ (by decide)
  have hbuf_len : self.buffer.val.length = remBytes.length := by rw [hinv_buf]
  by_cases hemp : data.val.length = 0
  · have his_emp : core.slice.Slice.is_empty data = ok true := by
      simp [core.slice.Slice.is_empty, Slice.length, hemp]
    have hnil : data.val = [] := List.eq_nil_of_length_eq_zero hemp
    rw [his_emp]
    simp only [bind_ok, ↓reduceIte]
    refine ⟨self, rfl, ?_⟩
    exact ⟨by simpa [hnil] using hinv_buf, by simpa [hnil] using hinv_builder⟩
  · have his_emp : core.slice.Slice.is_empty data = ok false := by
      simp [core.slice.Slice.is_empty, Slice.length, hemp]
    set sat := core.num.Usize.saturating_add (alloc.vec.Vec.len self.buffer) (Slice.len data)
    have hsat_val : sat.val = min Usize.max (remBytes.length + data.val.length) := by
      simpa [sat, hbuf_len] using
        usize_saturating_add_val (alloc.vec.Vec.len self.buffer) (Slice.len data)
    by_cases hlt_blk : sat < BLOCK_SIZE
    · -- Buffered bytes + `data` still `< 8,192` bytes
      have hsum_lt : remBytes.length + data.val.length < 8192 := by
        have hlt_v : sat.val < 8192 := by simpa [BLOCK_SIZE_val] using hlt_blk
        have humax := usize_max_val_ge
        omega
      have hext_cap : self.buffer.val.length + data.val.length ≤ Usize.max := by
        have humax := usize_max_val_ge; omega
      obtain ⟨v1, hext, hv1_val⟩ := vec_extend_u8_ok self.buffer data hext_cap
      rw [his_emp]
      simp only [bind_ok, Bool.false_eq_true, ↓reduceIte, lift, sat, hlt_blk, hext]
      refine ⟨{ self with buffer := v1 }, rfl, ?_, ?_⟩
      · rw [hv1_val, hinv_buf]
        have htot_div : ((bytes ++ data.val).length / 8192) * 8192 = q * 8192 := by
          have : (bytes ++ data.val).length = q * 8192 + (remBytes.length + data.val.length) := by
            simp; omega
          omega
        rw [htot_div, List.drop_append]
        have hle_b : q * 8192 ≤ bytes.length := Nat.div_mul_le_self _ _
        simp [show q * 8192 - bytes.length = 0 by omega, remBytes]
      · have hfb_same :
            fullBlockHashes sha256 (bytes ++ data.val) = fullBlockHashes sha256 bytes := by
          unfold fullBlockHashes
          have htot_div : ((bytes ++ data.val).length / 8192) * 8192 = q * 8192 := by
            have : (bytes ++ data.val).length = q * 8192 + (remBytes.length + data.val.length) := by
              simp; omega
            omega
          rw [htot_div, List.take_append_of_le_length (Nat.div_mul_le_self _ _)]
        simpa [hfb_same] using hinv_builder
    · -- Buffered bytes + `data` reaches at least one full 8,192-byte block (`≥ 8,192`)
      have hsum_ge : 8192 ≤ remBytes.length + data.val.length := by
        have hge_v : 8192 ≤ sat.val := by
          have : ¬ (sat.val < 8192) := by simpa [UScalar.lt_equiv, BLOCK_SIZE_val] using hlt_blk
          omega
        have humax := usize_max_val_ge
        omega
      have hmid_step :
          ∃ (v_empty : alloc.vec.Vec Std.U8) (mrb : MerkleRootBuilder H C)
            (data1 : Slice Std.U8) (midBytes : List Std.U8),
            (if alloc.vec.Vec.len self.buffer > 0#usize then do
              let i4 := alloc.vec.Vec.len self.buffer
              let needed ← BLOCK_SIZE - i4
              let (head, tail) ← core.slice.Slice.split_at data needed
              let v1 ← alloc.vec.Vec.extend_from_slice core.clone.CloneU8 self.buffer head
              let s := alloc.vec.Vec.deref v1
              let mrb1 ← MerkleRootBuilder.add_block hInst cInst self.builder s
              let v2 ← alloc.vec.Vec.resize core.clone.CloneU8 v1 0#usize 0#u8
              ok (v2, mrb1, tail)
             else ok (self.buffer, self.builder, data)) = ok (v_empty, mrb, data1) ∧
            v_empty.val = [] ∧
            midBytes.length % 8192 = 0 ∧
            midBytes ++ data1.val = bytes ++ data.val ∧
            BuilderInv sha256 R_H R_C (fullBlockHashes sha256 midBytes) mrb := by
        by_cases hgt0 : alloc.vec.Vec.len self.buffer > 0#usize
        · have hrem_pos : 0 < remBytes.length := by
            have : 0 < (alloc.vec.Vec.len self.buffer).val := hgt0
            simpa [alloc.vec.Vec.length, hbuf_len] using this
          obtain ⟨needed, hsub_need, hneed_val⟩ :=
            uscalar_sub_ok BLOCK_SIZE (alloc.vec.Vec.len self.buffer) (by
              simp [hbuf_len]; omega)
          have hneed_eq : needed.val = 8192 - remBytes.length := by
            simpa [BLOCK_SIZE_val, alloc.vec.Vec.length, hbuf_len] using hneed_val
          have hneed_le_data : needed.val ≤ data.val.length := by omega
          obtain ⟨head, tail, hsplit_at, hhead_val, htail_val⟩ :=
            slice_split_at_ok data needed hneed_le_data
          have hext_cap : self.buffer.val.length + head.val.length ≤ Usize.max := by
            rw [hbuf_len, hhead_val, List.length_take_of_le hneed_le_data]
            have humax := usize_max_val_ge; omega
          obtain ⟨v1, hext1, hv1_val⟩ := vec_extend_u8_ok self.buffer head hext_cap
          set fullSlice := alloc.vec.Vec.deref v1
          have hfull_val : fullSlice.val = remBytes ++ head.val := by
            simpa [fullSlice, alloc.vec.Vec.deref, hinv_buf] using hv1_val
          have hfull_len : fullSlice.val.length = 8192 := by
            rw [hfull_val, List.length_append, hhead_val, List.length_take_of_le hneed_le_data]
            omega
          have hbuilder_aligned :
              BuilderInv sha256 R_H R_C (fullBlockHashes sha256 alignedBytes) self.builder := by
            simpa [alignedBytes, q, fullBlockHashes_take_aligned] using hinv_builder
          have hfb_len : (fullBlockHashes sha256 alignedBytes).length = q := by
            rw [fullBlockHashes_length, haligned_len]; omega
          have hq1_le : q + 1 ≤ (bytes.length + data.val.length) / 8192 := by
            have hblen : bytes.length = q * 8192 + remBytes.length := by
              rw [← hbytes_split, List.length_append, haligned_len]
            have hsum8192 : (q + 1) * 8192 ≤ bytes.length + data.val.length :=
              calc (q + 1) * 8192
                  = q * 8192 + 8192 := Nat.succ_mul q 8192
                _ ≤ q * 8192 + (remBytes.length + data.val.length) := Nat.add_le_add_left hsum_ge _
                _ = (q * 8192 + remBytes.length) + data.val.length := (Nat.add_assoc _ _ _).symm
                _ = bytes.length + data.val.length := by rw [← hblen]
            exact (Nat.le_div_iff_mul_le (by decide)).mpr hsum8192
          have hq1_bound : (q + 1) * 8192 ≤ U64.max :=
            le_trans (Nat.mul_le_mul_right 8192 hq1_le) hbound
          obtain ⟨mrb1, hadd_blk, hmrb1_inv⟩ :=
            add_block_gen_inv hsha hcol (fullBlockHashes sha256 alignedBytes) self.builder
              fullSlice hfull_len hbuilder_aligned
              (by rw [hfb_len]; exact hq1_bound)
              (by rw [hfb_len]; omega)
          obtain ⟨v2, hresize, hv2_val⟩ := vec_resize_zero_u8_ok v1
          set midBytes := alignedBytes ++ fullSlice.val
          have hmid_mod : midBytes.length % 8192 = 0 := by
            rw [show midBytes = alignedBytes ++ fullSlice.val from rfl,
                List.length_append, haligned_len, hfull_len, ← Nat.succ_mul, Nat.mul_mod_left]
          have hmid_concat : midBytes ++ tail.val = bytes ++ data.val := by
            simp only [midBytes, hfull_val, hhead_val, htail_val]
            rw [← List.append_assoc alignedBytes remBytes, hbytes_split,
                List.append_assoc, List.take_append_drop]
          have hmrb1_mid :
              BuilderInv sha256 R_H R_C (fullBlockHashes sha256 midBytes) mrb1 := by
            rw [show midBytes = alignedBytes ++ fullSlice.val from rfl,
                fullBlockHashes_append_block sha256 alignedBytes fullSlice.val
                  haligned_mod hfull_len]
            have hdiv_eq :
                alignedBytes.length / 8192 = (fullBlockHashes sha256 alignedBytes).length := by
              rw [hfb_len, haligned_len]; omega
            simpa [hdiv_eq, BLOCK_BYTES] using hmrb1_inv
          refine ⟨v2, mrb1, tail, midBytes, ?_, hv2_val, hmid_mod, hmid_concat, hmrb1_mid⟩
          simp only [hgt0, ↓reduceIte, hsub_need, hsplit_at, hext1,
                     fullSlice, hadd_blk, hresize, bind_ok, uncurry]
        · have hrem0 : remBytes.length = 0 := by
            have : ¬ (0 < (alloc.vec.Vec.len self.buffer).val) := by
              simpa [UScalar.lt_equiv] using hgt0
            have h0 : ¬ (0 < remBytes.length) := by
              simpa [alloc.vec.Vec.length, hbuf_len] using this
            omega
          have hbuf_emp : self.buffer.val = [] :=
            List.eq_nil_of_length_eq_zero (by rw [hbuf_len, hrem0])
          have hbytes_mod : bytes.length % 8192 = 0 := by omega
          refine ⟨self.buffer, self.builder, data, bytes, ?_, hbuf_emp, hbytes_mod,
                  rfl, hinv_builder⟩
          simp [hgt0]
      obtain ⟨v_empty, mrb, data1, midBytes, hif_eq, hv_emp, hmid_mod, hmid_concat, hmrb_inv⟩ :=
        hmid_step
      have htot_len_eq : midBytes.length + data1.val.length = bytes.length + data.val.length := by
        have := congrArg List.length hmid_concat
        simpa using this
      obtain ⟨remSlice, mrb1, hab, hrem_val, hmrb1_inv⟩ :=
        add_blocks_gen_inv hsha hcol midBytes hmid_mod mrb data1 hmrb_inv
          (by rw [htot_len_eq]; exact hbound)
          (by rw [htot_len_eq]; exact hcap)
      rw [hmid_concat] at hmrb1_inv
      have hrem_drop_tot :
          remSlice.val = (bytes ++ data.val).drop (((bytes ++ data.val).length / 8192) * 8192) := by
        rw [hrem_val, ← hmid_concat]
        have htot_q : ((midBytes ++ data1.val).length / 8192) * 8192 =
                      midBytes.length + (data1.val.length / 8192) * 8192 := by
          simp; omega
        rw [htot_q, List.drop_append]
        simp
      rw [his_emp]
      simp only [Bool.false_eq_true, ↓reduceIte, lift, bind_ok, sat, hlt_blk]
      rw [hif_eq]
      simp only [bind_ok, uncurry]
      rw [hab]
      simp only [bind_ok, uncurry]
      by_cases hrem_emp : remSlice.val.length = 0
      · have his_emp1 : core.slice.Slice.is_empty remSlice = ok true := by
          simp [core.slice.Slice.is_empty, Slice.length, hrem_emp]
        have hrem_nil : remSlice.val = [] := List.eq_nil_of_length_eq_zero hrem_emp
        rw [his_emp1]
        simp only [bind_ok, ↓reduceIte]
        refine ⟨{ buffer := v_empty, builder := mrb1 }, rfl, ?_, hmrb1_inv⟩
        rw [hv_emp, ← hrem_drop_tot, hrem_nil]
      · have his_emp1 : core.slice.Slice.is_empty remSlice = ok false := by
          simp [core.slice.Slice.is_empty, Slice.length, hrem_emp]
        have hext_cap2 : v_empty.val.length + remSlice.val.length ≤ Usize.max := by
          rw [hv_emp]; simp
        obtain ⟨v_final, hext2, hv_final⟩ := vec_extend_u8_ok v_empty remSlice hext_cap2
        rw [his_emp1]
        simp only [bind_ok, Bool.false_eq_true, ↓reduceIte]
        rw [hext2]
        simp only [bind_ok]
        refine ⟨{ buffer := v_final, builder := mrb1 }, rfl, ?_, hmrb1_inv⟩
        rw [hv_final, hv_emp, List.nil_append, hrem_drop_tot]

theorem buffered_complete_gen_spec
    {H C : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {cInst : LeafCollector C} {maxLeaves : Nat} {R_C : C → List Hash → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hcol : ValidLeafCollector cInst maxLeaves R_C)
    (bytes : List Std.U8) (self : BufferedMerkleRootBuilder H C)
    (hinv : BufferedInv sha256 R_H R_C bytes self)
    (hbound : ((bytes.length + 8191) / 8192) * 8192 ≤ U64.max)
    (hcap : (specLeafHashes sha256 bytes).length ≤ maxLeaves) :
    ∃ c',
      BufferedMerkleRootBuilder.complete hInst cInst self =
        ok (specMerkleRoot sha256 bytes, c') ∧
      R_C c' (specLeafHashes sha256 bytes) := by
  rcases hinv with ⟨hinv_buf, hinv_builder⟩
  unfold BufferedMerkleRootBuilder.complete
  set alignedBytes := bytes.take ((bytes.length / 8192) * 8192)
  have haligned_len : alignedBytes.length = (bytes.length / 8192) * 8192 :=
    List.length_take_of_le (Nat.div_mul_le_self _ _)
  have haligned_mod : alignedBytes.length % 8192 = 0 := by omega
  set s := alloc.vec.Vec.deref self.buffer
  have hs_val : s.val = bytes.drop ((bytes.length / 8192) * 8192) := by
    simpa [s, alloc.vec.Vec.deref] using hinv_buf
  have hconcat : alignedBytes ++ s.val = bytes := by
    rw [hs_val]; exact List.take_append_drop _ _
  have hlen_sum : alignedBytes.length + s.val.length = bytes.length := by
    have := congrArg List.length hconcat
    simpa using this
  have hbuilder_aligned :
      BuilderInv sha256 R_H R_C (fullBlockHashes sha256 alignedBytes) self.builder := by
    simpa [alignedBytes, fullBlockHashes_take_aligned] using hinv_builder
  obtain ⟨c', hfin, hc'⟩ :=
    complete_from_aligned_prefix_gen_spec hsha hcol alignedBytes haligned_mod
      self.builder s hbuilder_aligned
      (by rw [hlen_sum]; exact hbound)
      (by rw [hconcat]; exact hcap)
  rw [hconcat] at hfin hc'
  exact ⟨c', hfin, hc'⟩

theorem bufferedWriteAll_gen_inv
    {H C : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {cInst : LeafCollector C} {maxLeaves : Nat} {R_C : C → List Hash → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hcol : ValidLeafCollector cInst maxLeaves R_C)
    (prefixBytes : List Std.U8) (self : BufferedMerkleRootBuilder H C) :
    ∀ (chunks : List (Slice Std.U8)),
      BufferedInv sha256 R_H R_C prefixBytes self →
      (((prefixBytes.length + (chunks.flatMap Slice.val).length) / 8192) * 8192) ≤ U64.max →
      (prefixBytes.length + (chunks.flatMap Slice.val).length) / 8192 ≤ maxLeaves →
      ∃ self',
        bufferedWriteAll hInst cInst self chunks = ok self' ∧
        BufferedInv sha256 R_H R_C (prefixBytes ++ chunks.flatMap Slice.val) self'
  | [], hinv, _, _ => ⟨self, rfl, by simpa using hinv⟩
  | c :: cs, hinv, hbound, hcap => by
    simp only [List.flatMap_cons, List.length_append] at hbound hcap ⊢
    have hstep_div :
        (prefixBytes.length + c.val.length) / 8192 ≤
        (prefixBytes.length + (c.val.length + (cs.flatMap Slice.val).length)) / 8192 :=
      Nat.div_le_div_right (by omega)
    obtain ⟨self1, hw, hinv1⟩ :=
      buffered_write_gen_inv hsha hcol prefixBytes self c hinv
        (le_trans (Nat.mul_le_mul_right 8192 hstep_div) hbound)
        (le_trans hstep_div hcap)
    obtain ⟨self', hrest, hinv'⟩ :=
      bufferedWriteAll_gen_inv hsha hcol (prefixBytes ++ c.val) self1 cs hinv1
        (by simpa [Nat.add_assoc] using hbound)
        (by simpa [Nat.add_assoc] using hcap)
    refine ⟨self', ?_, by simpa [List.append_assoc] using hinv'⟩
    simp only [bufferedWriteAll, hw, hrest, bind_ok]

/-- Streaming any sequence of slices `chunks` into `BufferedMerkleRootBuilder` and calling
    `complete` produces `(specMerkleRoot sha256 (chunks.flatMap Slice.val), c')` with
    `R_C c' (specLeafHashes sha256 (chunks.flatMap Slice.val))` for any `ValidSha256State`
    and `ValidLeafCollector`. -/
theorem buffered_streaming_gen_spec
    {H C : Type} (sha256 : List Std.U8 → Hash)
    (hInst : Sha256State H) (R_H : H → List Std.U8 → Prop)
    (cInst : LeafCollector C) (maxLeaves : Nat)
    (R_C : C → List Hash → Prop)
    (c₀ : C) (chunks : List (Slice Std.U8))
    (hsha : ValidSha256State sha256 hInst R_H)
    (hcol : ValidLeafCollector cInst maxLeaves R_C)
    (hc₀ : R_C c₀ [])
    (hcap : (specLeafHashes sha256 (chunks.flatMap Slice.val)).length ≤ maxLeaves)
    (hbound :
      (((chunks.flatMap Slice.val).length + (BLOCK_BYTES - 1)) / BLOCK_BYTES) *
        BLOCK_BYTES ≤ U64.max) :
    ∃ c',
      (do
        let b₀ ← BufferedMerkleRootBuilder.new hInst cInst c₀
        let b₁ ← bufferedWriteAll hInst cInst b₀ chunks
        BufferedMerkleRootBuilder.complete hInst cInst b₁) =
        ok (specMerkleRoot sha256 (chunks.flatMap Slice.val), c') ∧
      R_C c' (specLeafHashes sha256 (chunks.flatMap Slice.val)) := by
  set total := chunks.flatMap Slice.val
  have hbound8192 : ((total.length + 8191) / 8192) * 8192 ≤ U64.max := by
    simpa [total, BLOCK_BYTES] using hbound
  have ⟨hceil_le_sl, _⟩ := ceil_blocks_le_specLeafHashes_length sha256 total
  have hw_div_le : total.length / 8192 ≤ (total.length + 8191) / 8192 :=
    Nat.div_le_div_right (by omega)
  have hw_bound : (total.length / 8192) * 8192 ≤ U64.max :=
    le_trans (Nat.mul_le_mul_right 8192 hw_div_le) hbound8192
  have hw_cap : total.length / 8192 ≤ maxLeaves :=
    le_trans hw_div_le (le_trans hceil_le_sl hcap)
  obtain ⟨b₀, hb₀, hinv₀⟩ := buffered_new_gen_inv sha256 hInst R_H cInst R_C c₀ hc₀
  obtain ⟨b₁, hw, hinv₁⟩ :=
    bufferedWriteAll_gen_inv hsha hcol [] b₀ chunks hinv₀
      (by simpa [total] using hw_bound)
      (by simpa [total] using hw_cap)
  rw [List.nil_append] at hinv₁
  obtain ⟨c', hcomp, hc'⟩ :=
    buffered_complete_gen_spec hsha hcol total b₁ hinv₁ hbound8192 hcap
  refine ⟨c', ?_, hc'⟩
  simp only [hb₀, hw, hcomp, bind_ok]

/-- Chunk-partition invariance for `(StreamSha256 sha256, ListLeafCollector)`: any two chunk
    partitions `chunks₁` and `chunks₂` whose concatenated bytes agree yield the exact same
    `ok (root, leaves)` result. -/
theorem buffered_chunk_invariance_lem
    (sha256 : List Std.U8 → Hash)
    (chunks₁ chunks₂ : List (Slice Std.U8))
    (hsame : chunks₁.flatMap Slice.val = chunks₂.flatMap Slice.val)
    (hbound :
      (((chunks₁.flatMap Slice.val).length + (BLOCK_BYTES - 1)) / BLOCK_BYTES) *
        BLOCK_BYTES ≤ U64.max) :
    (do
      let b₀ ← BufferedMerkleRootBuilder.new
        (StreamSha256 sha256) ListLeafCollector []
      let b₁ ← bufferedWriteAll (StreamSha256 sha256) ListLeafCollector b₀ chunks₁
      BufferedMerkleRootBuilder.complete
        (StreamSha256 sha256) ListLeafCollector b₁) =
    (do
      let b₀ ← BufferedMerkleRootBuilder.new
        (StreamSha256 sha256) ListLeafCollector []
      let b₁ ← bufferedWriteAll (StreamSha256 sha256) ListLeafCollector b₀ chunks₂
      BufferedMerkleRootBuilder.complete
        (StreamSha256 sha256) ListLeafCollector b₁) := by
  obtain ⟨c₁, h₁, hc₁⟩ :=
    buffered_streaming_gen_spec sha256 (StreamSha256 sha256) Eq
      ListLeafCollector (specLeafHashes sha256 (chunks₁.flatMap Slice.val)).length Eq [] chunks₁
      (validSha256State_stream sha256)
      (validLeafCollector_list _)
      rfl (le_refl _) hbound
  obtain ⟨c₂, h₂, hc₂⟩ :=
    buffered_streaming_gen_spec sha256 (StreamSha256 sha256) Eq
      ListLeafCollector (specLeafHashes sha256 (chunks₂.flatMap Slice.val)).length Eq [] chunks₂
      (validSha256State_stream sha256)
      (validLeafCollector_list _)
      rfl (le_refl _) (by simpa [hsame] using hbound)
  subst hc₁
  subst hc₂
  exact h₁.trans (by rw [hsame]; exact h₂.symm)

end FuchsiaMerkle
