-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.Sha256Sim

set_option maxRecDepth 8192
set_option maxHeartbeats 800000

/-!
# Shared Verifier Invariants, Arithmetic Lemmas, and Block-Matching Soundness

Shared helper definitions and pure protocol-reduction lemmas used across both
`MerkleVerifier` and `ReadSizedMerkleVerifier` proofs.
-/

namespace FuchsiaMerkle

open Aeneas Aeneas.Std Result
open fuchsia_merkle_core hiding Hash
open FuchsiaMerkle.Assumptions
open FuchsiaMerkle.Spec

/-- Structural invariant established by `MerkleVerifier.new`. -/
structure MerkleVerifierInv (mv : MerkleVerifier) : Prop where
  hne : 0 < mv.hashes.val.length
  hbound : mv.hashes.val.length * 8192 ≤ U64.max

/-- Structural invariant established by `ReadSizedMerkleVerifier.new`. -/
structure ReadSizedWf (rsv : ReadSizedMerkleVerifier) : Prop where
  hread_pos : 0 < rsv.read_size.val
  hread_aligned : rsv.read_size.val % 8192 = 0
  hbph : rsv.bytes_per_hash.val = (rsv.read_size.val / 8192) * 32
  hhashes_ne : 0 < rsv.hashes.val.length
  hoff_bound : rsv.hashes.val.length * rsv.bytes_per_hash.val ≤ U64.max

@[simp]
theorem option_is_none_none {α : Type _} :
    core.option.Option.is_none (none : Option α) = true := rfl

@[simp]
theorem option_is_none_some {α : Type _} (x : α) :
    core.option.Option.is_none (some x) = false := rfl

@[simp]
theorem try_branch_ok {T E : Type _} (x : T) :
    core.result.Result.Insts.CoreOpsTry.branch (core.result.Result.Ok x : core.result.Result T E) =
      ok (core.ops.control_flow.ControlFlow.Continue x) := by
  simp [core.result.Result.Insts.CoreOpsTry.branch]

@[simp]
theorem try_branch_err {T E : Type _} (e : E) :
    core.result.Result.Insts.CoreOpsTry.branch (core.result.Result.Err e : core.result.Result T E) =
      ok (core.ops.control_flow.ControlFlow.Break (core.result.Result.Err e)) := by
  simp [core.result.Result.Insts.CoreOpsTry.branch]

@[simp]
theorem try_from_residual_err {T E : Type _} (e : E) :
    core.result.Result.Insts.CoreOpsTry_traitFromResidualResult.from_residual
      T (core.convert.FromSame E) (core.result.Result.Err e) =
      ok (core.result.Result.Err e) := by
  simp [core.result.Result.Insts.CoreOpsTry_traitFromResidualResult.from_residual]

theorem not_usize_gt_of_le {a b : Std.Usize} (h : a.val ≤ b.val) : ¬ (a > b) := by
  scalar_tac

theorem usize_gt_irrefl (a : Std.Usize) : ¬ (a > a) := by
  scalar_tac

theorem not_usize_lt_of_ge {a b : Std.Usize} (h : b.val ≤ a.val) : ¬ (a < b) := by
  scalar_tac

theorem usize_lt_of_lt {a b : Std.Usize} (h : a.val < b.val) : a < b := by
  scalar_tac

theorem u32_val_le_max (x : Std.U32) : x.val ≤ U32.max := by
  scalar_tac

theorem usize_val_le_max (x : Std.Usize) : x.val ≤ Usize.max := by
  scalar_tac

theorem min_sub_le_sub {a d b c : Nat} (hc : c ≤ b) :
    min a (d - c) - (b - c) ≤ d - b := by
  omega

theorem sub_add_sub_cancel_of_le {u b m : Nat} (hub : u ≤ b) (hbm : b ≤ m) :
    b - u + (m - b) = m - u := by
  omega

theorem usize_is_multiple_of_eq (a b : Std.Usize) :
    core.num.Usize.is_multiple_of a b = ok (decide (a.val % b.val = 0)) := by
  simp [core.num.Usize.is_multiple_of, UScalar.is_multiple_of, beq_eq_decide]

theorem div_ceil_pos_of_pos {n k : Nat} (hn : 0 < n) (hk : 0 < k) :
    0 < (n + k - 1) / k := by
  have hge : k ≤ n + k - 1 := by omega
  have hdiv : 1 ≤ (n + k - 1) / k := (Nat.le_div_iff_mul_le hk).mpr (by omega)
  omega

theorem mul_lt_of_lt_div_ceil {i n k : Nat} (hk : 0 < k)
    (hi : i < (n + k - 1) / k) :
    i * k < n := by
  have hle : (i + 1) * k ≤ n + k - 1 := (Nat.le_div_iff_mul_le hk).mp (by omega)
  have hsucc : (i + 1) * k = i * k + k := Nat.succ_mul i k
  omega

theorem le_div_ceil_mul {n k : Nat} (hk : 0 < k) :
    n ≤ ((n + k - 1) / k) * k := by
  have h1 := Nat.div_add_mod (n + k - 1) k
  rw [Nat.mul_comm] at h1
  have h2 := Nat.mod_lt (n + k - 1) hk
  omega

theorem cast_usize_u32_max_val :
    (UScalar.cast .Usize core.num.U32.MAX).val = U32.max := by
  rw [UScalar.cast_val_eq]
  have p32 : 2 ^ 32 = 4294967296 := by decide
  have p64 : 2 ^ 64 = 18446744073709551616 := by decide
  rcases System.Platform.numBits_eq with hp | hp <;>
    simp [core.num.U32.MAX, U32.rMax, UScalarTy.numBits, U32.max, U32.numBits, hp, p32, p64]

theorem div_ceil_add_aligned {off len k : Nat} (hk : 0 < k) (hoff : off % k = 0) :
    (off + len + k - 1) / k = off / k + (len + k - 1) / k := by
  have hq : off = k * (off / k) := (Nat.mul_div_cancel' (Nat.dvd_of_mod_eq_zero hoff)).symm
  have hrew : off + len + k - 1 = k * (off / k) + (len + k - 1) := by omega
  rw [hrew, Nat.mul_add_div hk]

theorem aligned_buffer_len_cases {len bytePos : Nat}
    (hpage : len % 4096 = 0) (hbpos : bytePos % 8192 = 0) (hlt : bytePos < len) :
    min 8192 (len - bytePos) = 8192 ∨ min 8192 (len - bytePos) = 4096 := by
  omega

theorem last_aligned_end_eq {ulen len numBlocks : Nat}
    (hpos : 0 < ulen) (hule : ulen ≤ len)
    (hnb : numBlocks = (ulen + 8191) / 8192) :
    (numBlocks - 1) * 8192 + min 8192 (len - (numBlocks - 1) * 8192) =
      min len (numBlocks * 8192) := by
  omega

theorem last_unaligned_end_eq {ulen numBlocks : Nat}
    (hpos : 0 < ulen) (hnb : numBlocks = (ulen + 8191) / 8192) :
    (numBlocks - 1) * 8192 + min 8192 (ulen - (numBlocks - 1) * 8192) = ulen := by
  omega

theorem drop_min_len_eq_zeroPad_of_div_ceil_le {data : List Std.U8} {unaligned_len : Nat}
    (hblocks : (data.length + 8191) / 8192 ≤ (unaligned_len + 8191) / 8192) :
    data.drop (min data.length (((unaligned_len + 8191) / 8192) * 8192)) =
      specZeroPad (data.length - min data.length (((unaligned_len + 8191) / 8192) * 8192)) := by
  have hle : data.length ≤ ((unaligned_len + 8191) / 8192) * 8192 := by omega
  simp [Nat.min_eq_left hle, specZeroPad]

theorem sliceByteSource_refines (sha256 : List Std.U8 → Hash) (slice : Slice Std.U8) :
    RefinesAlignedByteSource sha256 (SliceByteSource sha256) slice slice where
  buffer_len_eq := rfl
  hash_aligned_block_eq _ _ _ _ _ _ _ _ _ _ := rfl

/-- Blockwise slice equality implies full prefix equality `xs = ys.take (M * K)`. -/
theorem list_eq_of_blocks_eq {α : Type _} (K : Nat) (hK : 0 < K) :
    ∀ (M : Nat) (xs ys : List α),
      (xs.length + K - 1) / K = M →
      (∀ i < M, (xs.drop (i * K)).take K = (ys.drop (i * K)).take K) →
      xs = ys.take (M * K)
  | 0, xs, _, hM, _ => by
    have hlen0 : xs.length = 0 := by
      by_contra hnz
      have hpos := div_ceil_pos_of_pos (Nat.pos_of_ne_zero hnz) hK
      omega
    simp [List.eq_nil_of_length_eq_zero hlen0]
  | M + 1, xs, ys, hM, hblk => by
    have h0 : xs.take K = ys.take K := by
      simpa using hblk 0 (Nat.zero_lt_succ M)
    cases M with
    | zero =>
      have hleK : xs.length ≤ K := by
        by_contra hgt
        have hge2 : 2 * K ≤ xs.length + K - 1 := by omega
        have hdiv2 : 2 ≤ (xs.length + K - 1) / K :=
          (Nat.le_div_iff_mul_le hK).mpr hge2
        omega
      rw [Nat.one_mul, ← h0, List.take_of_length_le hleK]
    | succ M' =>
      have hgtK : K < xs.length := by
        by_contra hle
        have hlt2 : xs.length + K - 1 < K * 2 := by omega
        have hdiv_lt : (xs.length + K - 1) / K < 2 := Nat.div_lt_of_lt_mul hlt2
        omega
      have hM_drop : ((xs.drop K).length + K - 1) / K = M' + 1 := by
        rw [List.length_drop]
        have hrew : xs.length - K + K - 1 + K = xs.length + K - 1 := by omega
        have hdiv := Nat.add_div_right (xs.length - K + K - 1) hK
        rw [hrew] at hdiv
        omega
      have hblk_drop :
          ∀ j < M' + 1,
            ((xs.drop K).drop (j * K)).take K = ((ys.drop K).drop (j * K)).take K := by
        intro j hj
        have hstep := hblk (j + 1) (Nat.succ_lt_succ hj)
        have hmul : K + j * K = (j + 1) * K := by ring
        simpa [List.drop_drop, hmul] using hstep
      have ih := list_eq_of_blocks_eq K hK (M' + 1) (xs.drop K) (ys.drop K) hM_drop hblk_drop
      have hxs_split : xs = xs.take K ++ xs.drop K := (List.take_append_drop K xs).symm
      rw [hxs_split, h0, ih]
      have hmul : (M' + 1 + 1) * K = K + (M' + 1) * K := by ring
      rw [hmul, List.take_add]

theorem specLeafHashes_length (sha256 : List Std.U8 → Hash) (B : List Std.U8) :
    (specLeafHashes sha256 B).length =
      if B.isEmpty then 1 else (B.length + 8191) / 8192 := by
  unfold specLeafHashes
  by_cases hemp : B.isEmpty
  · simp [hemp]
  · simp [hemp, chunksOf_length, BLOCK_BYTES]

theorem specLeafHashes_pos (sha256 : List Std.U8 → Hash) (B : List Std.U8) :
    0 < (specLeafHashes sha256 B).length := by
  rw [specLeafHashes_length]
  by_cases hemp : B.isEmpty
  · simp [hemp]
  · simp only [hemp, Bool.false_eq_true, ↓reduceIte]
    have hne : B ≠ [] := fun h => by subst h; simp at hemp
    exact div_ceil_pos_of_pos (k := 8192) (List.length_pos_iff.mpr hne) (by decide)

theorem specLeafHashes_getElem_of_nonempty
    (sha256 : List Std.U8 → Hash) (B : List Std.U8) (hne : B ≠ [])
    (k : Nat) (hk : k < (specLeafHashes sha256 B).length) :
    (specLeafHashes sha256 B)[k]'hk =
      specHashBlock sha256 ((B.drop (k * 8192)).take 8192) (u64OfNat (k * 8192)) ∧
    0 < ((B.drop (k * 8192)).take 8192).length ∧
    ((B.drop (k * 8192)).take 8192).length ≤ 8192 := by
  have hemp : B.isEmpty = false := List.isEmpty_eq_false_iff.mpr hne
  have hk_chunks : k < (chunksOf BLOCK_BYTES (by decide) B).length := by
    rw [specLeafHashes_length, hemp] at hk
    simp only [Bool.false_eq_true, ↓reduceIte] at hk
    simpa [chunksOf_length, BLOCK_BYTES] using hk
  refine ⟨?_, ?_, List.length_take_le 8192 _⟩
  · unfold specLeafHashes
    simp only [hemp, Bool.false_eq_true, ↓reduceIte, List.getElem_mapIdx]
    rw [chunksOf_getElem BLOCK_BYTES (by decide) B k hk_chunks]
  · rw [chunksOf_length] at hk_chunks
    have hlt_len : k * 8192 < B.length := mul_lt_of_lt_div_ceil (by decide) hk_chunks
    simp [List.length_take, List.length_drop]
    omega

/-- Preimage soundness for `hash_aligned_block`: matching a non-empty canonical leaf digest
    forces BOTH exact payload agreement on `0..ulen` AND all-zero padding on `ulen..buflen`
    (unless `HashCollision sha256`). -/
theorem aligned_block_eq_or_collision
    {sha256 : List Std.U8 → Hash}
    {slice canon : List Std.U8} {buflen ulen : Nat} {off : Std.U64}
    (hbuf : buflen = 8192 ∨ buflen = 4096)
    (_hunz : 0 < ulen)
    (hule : ulen ≤ buflen)
    (hslice_len : buflen ≤ slice.length)
    (hc_pos : 0 < canon.length)
    (hc_le : canon.length ≤ 8192)
    (hoff : off.val % 8192 = 0)
    (heq : sha256 (specIdentity (u32OfNat ulen) 0#u64 off ++
                   slice.take buflen ++
                   specZeroPad (BLOCK_BYTES - buflen)) =
           specHashBlock sha256 canon off) :
    (ulen = canon.length ∧
      slice.take ulen = canon ∧
      (slice.drop ulen).take (buflen - ulen) = specZeroPad (buflen - ulen)) ∨
    HashCollision sha256 := by
  have hc_ne0 : ¬ (canon.length = 0 ∧ off = 0#u64) := fun h => by omega
  unfold specHashBlock at heq
  dsimp only at heq
  simp only [hc_ne0, ↓reduceIte] at heq
  set p₁ := specIdentity (u32OfNat ulen) 0#u64 off ++
            slice.take buflen ++ specZeroPad (BLOCK_BYTES - buflen)
  set p₂ := specIdentity (u32OfNat canon.length) 0#u64 off ++
            canon ++ specZeroPad (BLOCK_BYTES - canon.length)
  by_cases hp : p₁ = p₂
  · left
    simp only [p₁, p₂, List.append_assoc] at hp
    obtain ⟨hid, hbody⟩ := List.append_inj hp (by simp)
    obtain ⟨hlen32_eq, _, _⟩ := specIdentity_injective hoff hoff (by simp) (by simp) hid
    have hbuflen_le : buflen ≤ 8192 := by rcases hbuf with rfl | rfl <;> omega
    have hulen_val : (u32OfNat ulen).val = ulen := u32OfNat_val_of_lt ulen (by omega)
    have hclen_val : (u32OfNat canon.length).val = canon.length :=
      u32OfNat_val_of_lt canon.length (by omega)
    have hulen_eq : ulen = canon.length := by
      rw [← hulen_val, ← hclen_val, hlen32_eq]
    have htake_len : (slice.take buflen).length = buflen := List.length_take_of_le hslice_len
    have hprefix := congrArg (List.take ulen) hbody
    rw [List.take_append_of_le_length (by omega), List.take_take, Nat.min_eq_left hule,
        List.take_append_of_le_length (by omega)] at hprefix
    have hrhs_take : canon.take ulen = canon := List.take_of_length_le (by omega)
    rw [hrhs_take] at hprefix
    have hmid := congrArg (fun l => (l.drop ulen).take (buflen - ulen)) hbody
    rw [List.drop_append_of_le_length (by omega),
        List.take_append_of_le_length (by simp [htake_len]),
        List.drop_take, List.take_take, Nat.min_eq_left (by omega)] at hmid
    have hrhs_drop :
        ((canon ++ specZeroPad (BLOCK_BYTES - canon.length)).drop ulen).take (buflen - ulen) =
        specZeroPad (buflen - ulen) := by
      rw [hulen_eq, List.drop_append_of_le_length (le_refl _), List.drop_length, List.nil_append]
      unfold specZeroPad BLOCK_BYTES
      rw [List.take_replicate, Nat.min_eq_left (by omega)]
    rw [hrhs_drop] at hmid
    exact ⟨hulen_eq, hprefix, hmid⟩
  · exact Or.inr ⟨p₁, p₂, hp, heq⟩

/-- Internal node preimage injectivity when the 12-byte identity `(bytesPerNode, lvl, off)` is
    shared (requiring no alignment side condition on `off`, covering `read_size = 8192`). -/
theorem specHashInternal_same_id_eq_imp_padded_eq_or_collision
    {sha256 : List Std.U8 → Hash}
    {hashesPerNode : Nat} {bytesPerNode : Std.U32}
    {lvl off : Std.U64} {c₁ c₂ : List Hash}
    (hc₁ : c₁.length ≤ hashesPerNode)
    (hc₂ : c₂.length ≤ hashesPerNode)
    (heq : specHashInternal sha256 hashesPerNode bytesPerNode lvl off c₁ =
           specHashInternal sha256 hashesPerNode bytesPerNode lvl off c₂) :
    (c₁ ++ List.replicate (hashesPerNode - c₁.length) zeroHash =
     c₂ ++ List.replicate (hashesPerNode - c₂.length) zeroHash) ∨
    HashCollision sha256 := by
  unfold specHashInternal specInternalPreimage at heq
  set p₁ := specIdentity bytesPerNode lvl off ++ concatHashes c₁ ++
            specZeroPad ((hashesPerNode - c₁.length) * 32)
  set p₂ := specIdentity bytesPerNode lvl off ++ concatHashes c₂ ++
            specZeroPad ((hashesPerNode - c₂.length) * 32)
  by_cases hp : p₁ = p₂
  · left
    simp only [p₁, p₂, List.append_assoc] at hp
    have hbody := (List.append_inj hp rfl).2
    rw [← concatHashes_replicate_zeroHash, ← concatHashes_replicate_zeroHash,
        ← concatHashes_append, ← concatHashes_append] at hbody
    have hpad_len :
        (c₁ ++ List.replicate (hashesPerNode - c₁.length) zeroHash).length =
        (c₂ ++ List.replicate (hashesPerNode - c₂.length) zeroHash).length := by
      simp; omega
    exact concatHashes_injective_of_length_eq hpad_len hbody
  · exact Or.inr ⟨p₁, p₂, hp, heq⟩

/-- Equality of zero-hash-padded child lists either yields `c₁ = c₂` (automatic when
    `c₁.length = c₂.length`), or witnesses `zeroHash ∈ c₁ ∨ zeroHash ∈ c₂`. -/
theorem padded_hashes_eq_imp_eq_or_zero_hash
    {K : Nat} {c₁ c₂ : List Hash}
    (hc₁ : c₁.length ≤ K)
    (hc₂ : c₂.length ≤ K)
    (hpad : c₁ ++ List.replicate (K - c₁.length) zeroHash =
            c₂ ++ List.replicate (K - c₂.length) zeroHash) :
    c₁ = c₂ ∨ zeroHash ∈ c₁ ∨ zeroHash ∈ c₂ := by
  by_cases hlen : c₁.length = c₂.length
  · left
    exact (List.append_inj hpad hlen).1
  · right
    by_cases hlt : c₁.length < c₂.length
    · right
      have hi₁ : c₁.length < (c₁ ++ List.replicate (K - c₁.length) zeroHash).length := by
        simp; omega
      have hi₂ : c₁.length < (c₂ ++ List.replicate (K - c₂.length) zeroHash).length := by
        simp; omega
      have hget :
          (c₁ ++ List.replicate (K - c₁.length) zeroHash)[c₁.length]'hi₁ =
          (c₂ ++ List.replicate (K - c₂.length) zeroHash)[c₁.length]'hi₂ := by
        simp [hpad]
      rw [List.getElem_append_right (le_refl _), List.getElem_replicate,
          List.getElem_append_left hlt] at hget
      exact hget ▸ List.getElem_mem hlt
    · left
      have hgt : c₂.length < c₁.length := by omega
      have hi₁ : c₂.length < (c₁ ++ List.replicate (K - c₁.length) zeroHash).length := by
        simp; omega
      have hi₂ : c₂.length < (c₂ ++ List.replicate (K - c₂.length) zeroHash).length := by
        simp; omega
      have hget :
          (c₁ ++ List.replicate (K - c₁.length) zeroHash)[c₂.length]'hi₁ =
          (c₂ ++ List.replicate (K - c₂.length) zeroHash)[c₂.length]'hi₂ := by
        simp [hpad]
      rw [List.getElem_append_left hgt,
          List.getElem_append_right (le_refl _), List.getElem_replicate] at hget
      exact hget.symm ▸ List.getElem_mem hgt

/-- Every element of `specLeafHashes sha256 B` is an explicit SHA-256 digest `sha256 m`,
    so `zeroHash ∈ specLeafHashes sha256 B` witnesses `HashZeroPreimage sha256`. -/
theorem mem_specLeafHashes_eq_zeroHash_imp_zeroPreimage
    {sha256 : List Std.U8 → Hash} {B : List Std.U8}
    (hmem : zeroHash ∈ specLeafHashes sha256 B) :
    HashZeroPreimage sha256 := by
  obtain ⟨k, hk, hget⟩ := List.getElem_of_mem hmem
  by_cases hB : B = []
  · subst hB
    simp only [specLeafHashes, List.isEmpty_nil, ↓reduceIte] at hk hget
    have hk0 : k = 0 := by simp at hk; omega
    subst hk0
    simp only [List.getElem_singleton, specHashBlock] at hget
    exact ⟨_, hget⟩
  · obtain ⟨heq, _, _⟩ := specLeafHashes_getElem_of_nonempty sha256 B hB k hk
    rw [heq] at hget
    unfold specHashBlock at hget
    dsimp only at hget
    exact ⟨_, hget⟩

theorem specReadSizedHashes_length
    (sha256 : List Std.U8 → Hash) (K : Nat) (hK : 0 < K)
    (bph : Std.U32) (leaves : List Hash) :
    (specReadSizedHashes sha256 K hK bph leaves).length = (leaves.length + K - 1) / K := by
  simp [specReadSizedHashes, chunksOf_length]

theorem specReadSizedHashes_getElem
    (sha256 : List Std.U8 → Hash) (K : Nat) (hK : 0 < K)
    (bph : Std.U32) (leaves : List Hash)
    (idx : Nat) (hidx : idx < (specReadSizedHashes sha256 K hK bph leaves).length) :
    (specReadSizedHashes sha256 K hK bph leaves)[idx]'hidx =
      specHashInternal sha256 K bph 1#u64 (u64OfNat (idx * bph.val))
        ((leaves.drop (idx * K)).take K) := by
  have hc : idx < (chunksOf K hK leaves).length := by
    simpa [specReadSizedHashes] using hidx
  simp only [specReadSizedHashes, List.getElem_mapIdx]
  rw [chunksOf_getElem K hK leaves idx hc]

/-- Shared per-block soundness assembly for `verify`: if every 8 KiB block `k < M` of non-empty
    `data` matches leaf `offset / 8192 + k` of `specLeafHashes sha256 B`, then either `data` is
    the exact slice `(B.drop offset).take data.length` with `offset + data.length ≤ B.length`,
    or `HashCollision sha256`. -/
theorem blocks_match_leaves_sound
    (sha256 : List Std.U8 → Hash) (B data : List Std.U8) (offset : Nat)
    (hoff : offset % 8192 = 0)
    (hdne : data ≠ [])
    (hsum64 : offset + data.length ≤ U64.max)
    (hall : ∀ k < (data.length + 8191) / 8192,
      ∃ hk : offset / 8192 + k < (specLeafHashes sha256 B).length,
        (specLeafHashes sha256 B)[offset / 8192 + k]'hk =
          specHashBlock sha256 ((data.drop (k * 8192)).take 8192)
            (u64OfNat ((offset / 8192 + k) * 8192))) :
    (offset + data.length ≤ B.length ∧ data = (B.drop offset).take data.length) ∨
    HashCollision sha256 := by
  have hdpos : 0 < data.length := List.length_pos_iff.mpr hdne
  set M := (data.length + 8191) / 8192
  have hM_pos : 0 < M := div_ceil_pos_of_pos (k := 8192) hdpos (by decide)
  by_cases hBnil : B = []
  · subst hBnil
    obtain ⟨hk0, h0⟩ := hall 0 hM_pos
    have hbase0 : offset / 8192 = 0 := by
      have : (specLeafHashes sha256 []).length = 1 := by simp [specLeafHashes]
      omega
    have hsl0 : (specLeafHashes sha256 [])[offset / 8192 + 0]'hk0 =
        specHashBlock sha256 [] 0#u64 := by
      simp [hbase0, specLeafHashes]
    have hu0_eq : u64OfNat 0 = 0#u64 := UScalar.eq_of_val_eq (by simp)
    rw [hsl0, hbase0] at h0
    simp only [Nat.zero_add, Nat.zero_mul, List.drop_zero, hu0_eq] at h0
    rcases specHashBlock_eq_imp_eq_or_collision (by simp [BLOCK_BYTES])
        (List.length_take_le 8192 _) (by simp) (by simp) h0 with ⟨hchunk0, _⟩ | hcol
    · exfalso
      have hlen0 := congrArg List.length hchunk0.symm
      simp [List.length_take] at hlen0
      exact hdne hlen0
    · exact Or.inr hcol
  · by_cases hex_diff :
        ∃ k < M,
          (data.drop (k * 8192)).take 8192 ≠
          ((B.drop offset).drop (k * 8192)).take 8192
    · right
      obtain ⟨k, hkM, hneq_blk⟩ := hex_diff
      obtain ⟨hk_sl, heq_k⟩ := hall k hkM
      obtain ⟨hB_leaf, _, hB_le⟩ := specLeafHashes_getElem_of_nonempty sha256 B hBnil
        (offset / 8192 + k) hk_sl
      rw [hB_leaf] at heq_k
      have hk_mul_lt : k * 8192 < data.length :=
        mul_lt_of_lt_div_ceil (i := k) (n := data.length) (k := 8192) (by decide) hkM
      have hmul_off : (offset / 8192 + k) * 8192 = offset + k * 8192 := by
        rw [Nat.add_mul, Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hoff)]
      have hoff64_le : (offset / 8192 + k) * 8192 ≤ U64.max := by omega
      have hoff_mod : (u64OfNat ((offset / 8192 + k) * 8192)).val % 8192 = 0 := by
        rw [u64OfNat_val_of_le _ hoff64_le]; omega
      rcases specHashBlock_eq_imp_eq_or_collision hB_le (List.length_take_le 8192 _)
          hoff_mod hoff_mod heq_k with ⟨hblk_eq, _⟩ | hcol
      · exfalso
        apply hneq_blk
        rw [List.drop_drop, ← hmul_off]
        exact hblk_eq.symm
      · exact hcol
    · left
      have hall_blk :
          ∀ k < M,
            (data.drop (k * 8192)).take 8192 =
            ((B.drop offset).drop (k * 8192)).take 8192 := by
        intro k hkM
        exact Classical.not_not.mp fun hneq => hex_diff ⟨k, hkM, hneq⟩
      have htake_M : data = (B.drop offset).take (M * 8192) :=
        list_eq_of_blocks_eq 8192 (by decide) M data (B.drop offset) rfl hall_blk
      have hlen_le : data.length ≤ B.length - offset := by
        have hlen_eq := congrArg List.length htake_M
        simp only [List.length_take, List.length_drop] at hlen_eq
        omega
      have hin_B : offset + data.length ≤ B.length := by omega
      have hd_le_M : data.length ≤ M * 8192 := by
        dsimp [M]
        have h1 := Nat.div_add_mod (data.length - 1) 8192
        have h2 : (data.length + 8191) / 8192 = (data.length - 1) / 8192 + 1 := by omega
        omega
      have hslice : data = (B.drop offset).take data.length := by
        calc data
            = data.take data.length := (List.take_length).symm
          _ = ((B.drop offset).take (M * 8192)).take data.length := by
                conv => lhs; arg 2; rw [htake_M]
          _ = (B.drop offset).take data.length := by
                rw [List.take_take, Nat.min_eq_left hd_le_M]
      exact ⟨hin_B, hslice⟩

/-- Shared per-block soundness assembly for `verify_aligned`: if `data.drop hb = specZeroPad (...)`
    and every 8 KiB block `k < num_blocks` matches leaf `offset / 8192 + k` of `specLeafHashes sha256 B`,
    then `data[..unaligned_len] = B[offset .. offset + unaligned_len]` AND `data[unaligned_len..]`
    is all zeros, or `HashCollision sha256`. -/
theorem aligned_blocks_match_leaves_sound
    (sha256 : List Std.U8 → Hash) (B data : List Std.U8) (offset unaligned_len : Nat)
    (hoff : offset % 8192 = 0)
    (hpage : data.length % 4096 = 0)
    (hunz : 0 < unaligned_len)
    (hule : unaligned_len ≤ data.length)
    (hsum64 : offset + data.length ≤ U64.max)
    (hzero_after_hb :
      data.drop (min data.length (((unaligned_len + 8191) / 8192) * 8192)) =
      specZeroPad (data.length - min data.length (((unaligned_len + 8191) / 8192) * 8192)))
    (hall : ∀ k < (unaligned_len + 8191) / 8192,
      ∃ hk : offset / 8192 + k < (specLeafHashes sha256 B).length,
        (specLeafHashes sha256 B)[offset / 8192 + k]'hk =
          sha256 (specIdentity (u32OfNat (min 8192 (unaligned_len - k * 8192))) 0#u64
                    (u64OfNat ((offset / 8192 + k) * 8192)) ++
                  (data.drop (k * 8192)).take (min 8192 (data.length - k * 8192)) ++
                  specZeroPad (BLOCK_BYTES - min 8192 (data.length - k * 8192)))) :
    (0 < unaligned_len ∧
     unaligned_len ≤ data.length ∧
     offset + unaligned_len ≤ B.length ∧
     data.take unaligned_len = (B.drop offset).take unaligned_len ∧
     data.drop unaligned_len = specZeroPad (data.length - unaligned_len)) ∨
    HashCollision sha256 := by
  set nb := (unaligned_len + 8191) / 8192
  set hb := min data.length (nb * 8192)
  have hnb_pos : 0 < nb := div_ceil_pos_of_pos (k := 8192) hunz (by decide)
  by_cases hBnil : B = []
  · right
    subst hBnil
    obtain ⟨hk0, h0⟩ := hall 0 hnb_pos
    have hbase0 : offset / 8192 = 0 := by
      have : (specLeafHashes sha256 []).length = 1 := by simp [specLeafHashes]
      omega
    have hsl0 : (specLeafHashes sha256 [])[offset / 8192 + 0]'hk0 =
        specHashBlock sha256 [] 0#u64 := by
      simp [hbase0, specLeafHashes]
    rw [hsl0, hbase0] at h0
    simp only [Nat.zero_add, Nat.zero_mul, List.drop_zero, Nat.sub_zero,
               specHashBlock, List.length_nil, true_and, ↓reduceIte] at h0
    set p_emp := specIdentity (u32OfNat 0) 0#u64 0#u64
    set p_aln := specIdentity (u32OfNat (min 8192 unaligned_len)) 0#u64 (u64OfNat 0) ++
                 data.take (min 8192 data.length) ++
                 specZeroPad (BLOCK_BYTES - min 8192 data.length)
    have hp_ne : p_emp ≠ p_aln := by
      intro hp
      have hl1 : p_emp.length = 12 := by simp [p_emp]
      have htake0 : (data.take (min 8192 data.length)).length = min 8192 data.length :=
        List.length_take_of_le (Nat.min_le_right _ _)
      have hl2 : p_aln.length = 12 + min 8192 data.length + (8192 - min 8192 data.length) := by
        simp [p_aln, BLOCK_BYTES, htake0]
      have hlen_eq := congrArg List.length hp
      omega
    exact ⟨p_emp, p_aln, hp_ne, h0⟩
  · have hper_block :
        (∀ k < nb,
          min 8192 (unaligned_len - k * 8192) =
            ((B.drop ((offset / 8192 + k) * 8192)).take 8192).length ∧
          ((data.drop (k * 8192)).take (min 8192 (unaligned_len - k * 8192))) =
            (B.drop ((offset / 8192 + k) * 8192)).take 8192 ∧
          (((data.drop (k * 8192)).drop (min 8192 (unaligned_len - k * 8192))).take
            (min 8192 (data.length - k * 8192) - min 8192 (unaligned_len - k * 8192))) =
            specZeroPad (min 8192 (data.length - k * 8192) -
                         min 8192 (unaligned_len - k * 8192))) ∨
        HashCollision sha256 := by
      by_cases hbad :
          ∃ k < nb,
            ¬ (min 8192 (unaligned_len - k * 8192) =
                 ((B.drop ((offset / 8192 + k) * 8192)).take 8192).length ∧
               ((data.drop (k * 8192)).take (min 8192 (unaligned_len - k * 8192))) =
                 (B.drop ((offset / 8192 + k) * 8192)).take 8192 ∧
               (((data.drop (k * 8192)).drop (min 8192 (unaligned_len - k * 8192))).take
                 (min 8192 (data.length - k * 8192) - min 8192 (unaligned_len - k * 8192))) =
                 specZeroPad (min 8192 (data.length - k * 8192) -
                              min 8192 (unaligned_len - k * 8192)))
      · right
        obtain ⟨k, hk_lt, hnot_k⟩ := hbad
        obtain ⟨hk_sl, hk_eq⟩ := hall k hk_lt
        obtain ⟨hB_leaf, hB_pos, hB_le⟩ :=
          specLeafHashes_getElem_of_nonempty sha256 B hBnil (offset / 8192 + k) hk_sl
        rw [hB_leaf] at hk_eq
        have hk_mul_u : k * 8192 < unaligned_len :=
          mul_lt_of_lt_div_ceil (i := k) (n := unaligned_len) (k := 8192) (by decide) hk_lt
        have hk_mul_d : k * 8192 < data.length := lt_of_lt_of_le hk_mul_u hule
        have hbuf_cases := aligned_buffer_len_cases hpage (by omega) hk_mul_d
        have hslice_ge : min 8192 (data.length - k * 8192) ≤ (data.drop (k * 8192)).length := by
          rw [List.length_drop]; exact Nat.min_le_right _ _
        have hoff64_le : (offset / 8192 + k) * 8192 ≤ U64.max := by
          have : (offset / 8192 + k) * 8192 = offset + k * 8192 := by
            rw [Nat.add_mul, Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hoff)]
          omega
        have hoff_mod : (u64OfNat ((offset / 8192 + k) * 8192)).val % 8192 = 0 := by
          rw [u64OfNat_val_of_le _ hoff64_le]; omega
        rcases aligned_block_eq_or_collision hbuf_cases (by omega) (by omega)
            hslice_ge hB_pos hB_le hoff_mod hk_eq.symm with hgood | hcol
        · exact absurd hgood hnot_k
        · exact hcol
      · left
        intro k hk_lt
        by_contra hnot
        exact hbad ⟨k, hk_lt, hnot⟩
    rcases hper_block with hall_blk | hcol
    · left
      set payload := data.take unaligned_len
      have hpayload_len : payload.length = unaligned_len := List.length_take_of_le hule
      have hpayload_blks :
          ∀ k < nb,
            (payload.drop (k * 8192)).take 8192 =
            ((B.drop offset).drop (k * 8192)).take 8192 := by
        intro k hk_lt
        obtain ⟨_, hdata_eq_B, _⟩ := hall_blk k hk_lt
        have hmul_off : (offset / 8192 + k) * 8192 = offset + k * 8192 := by
          rw [Nat.add_mul, Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hoff)]
        rw [List.drop_take, List.take_take, hdata_eq_B, List.drop_drop, hmul_off]
      have hpayload_take_M : payload = (B.drop offset).take (nb * 8192) :=
        list_eq_of_blocks_eq 8192 (by decide) nb payload (B.drop offset)
          (by rw [hpayload_len]; rfl) hpayload_blks
      have hulen_le_B : unaligned_len ≤ B.length - offset := by
        have hlen_eq := congrArg List.length hpayload_take_M
        simp only [hpayload_len, List.length_take, List.length_drop] at hlen_eq
        omega
      have hin_B : offset + unaligned_len ≤ B.length := by omega
      have hulen_le_nb : unaligned_len ≤ nb * 8192 := by
        dsimp [nb]
        have h1 := Nat.div_add_mod (unaligned_len - 1) 8192
        have h2 : (unaligned_len + 8191) / 8192 = (unaligned_len - 1) / 8192 + 1 := by omega
        omega
      have hprefix_exact : data.take unaligned_len = (B.drop offset).take unaligned_len := by
        calc data.take unaligned_len
            = payload.take unaligned_len := by simp [payload, List.take_take]
          _ = ((B.drop offset).take (nb * 8192)).take unaligned_len := by rw [← hpayload_take_M]
          _ = (B.drop offset).take unaligned_len := by
                rw [List.take_take, Nat.min_eq_left hulen_le_nb]
      obtain ⟨_, _, hlast_zero⟩ := hall_blk (nb - 1) (Nat.sub_lt hnb_pos (by decide))
      have hlast_u_end :
          (nb - 1) * 8192 + min 8192 (unaligned_len - (nb - 1) * 8192) = unaligned_len :=
        last_unaligned_end_eq hunz rfl
      have hlast_b_end :
          (nb - 1) * 8192 + min 8192 (data.length - (nb - 1) * 8192) = hb :=
        last_aligned_end_eq hunz hule rfl
      have hdiff_eq :
          min 8192 (data.length - (nb - 1) * 8192) -
            min 8192 (unaligned_len - (nb - 1) * 8192) =
          hb - unaligned_len := by
        omega
      rw [List.drop_drop, hlast_u_end, hdiff_eq] at hlast_zero
      have hdrop_split :
          data.drop unaligned_len =
          (data.drop unaligned_len).take (hb - unaligned_len) ++
            (data.drop unaligned_len).drop (hb - unaligned_len) :=
        (List.take_append_drop _ _).symm
      have hdrop_drop_hb :
          (data.drop unaligned_len).drop (hb - unaligned_len) = data.drop hb := by
        rw [List.drop_drop, show unaligned_len + (hb - unaligned_len) = hb by omega]
      have hfull_zero :
          data.drop unaligned_len = specZeroPad (data.length - unaligned_len) := by
        rw [hdrop_split, hlast_zero, hdrop_drop_hb, hzero_after_hb, ← specZeroPad_add]
        congr 1; omega
      exact ⟨hunz, hule, hin_B, hprefix_exact, hfull_zero⟩
    · exact Or.inr hcol

end FuchsiaMerkle
