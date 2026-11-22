-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.VerifierCommon

set_option maxRecDepth 8192
set_option maxHeartbeats 800000

/-!
# Verification Proofs for `MerkleVerifier`

Proves panic/overflow freedom (P1), completeness (P3), collision-resistant
soundness and page-tail zero integrity (P4), and zero-hash leaf extension (P5)
for `MerkleVerifier.new`, `MerkleVerifier.verify`, and
`MerkleVerifier.verify_aligned`.
-/

namespace FuchsiaMerkle

open Aeneas Aeneas.Std Result
open fuchsia_merkle_core hiding Hash
open FuchsiaMerkle.Assumptions
open FuchsiaMerkle.Spec

-- ============================================================================
-- 1. `MerkleVerifier.new` -- Invariant, Totality (P1), Completeness (P3),
--    Soundness (P4), and Zero-Hash Leaf Extension (P5)
-- ============================================================================

/-- Polymorphic contract for `rebuild_root`, discharged by `BuilderProofs`. -/
def RebuildRootSpec {H : Type} (sha256 : List Std.U8 → Hash) (hInst : Sha256State H) : Prop :=
  ∀ (s : Slice Hash), s.val.length * BLOCK_BYTES ≤ U64.max →
    rebuild_root hInst s = ok (specRootFromLeaves sha256 s.val)

/-- Any `MerkleVerifier` constructed via `MerkleVerifier.new` satisfies
    `MerkleVerifierInv`, preserves `hashes`, and matches `specRootFromLeaves`. -/
theorem merkle_verifier_new_inv_of_rebuild
    {H : Type} {sha256 : List Std.U8 → Hash} {hInst : Sha256State H}
    {root : Hash} {hashes : Slice Hash} {mv : MerkleVerifier}
    (hreb : RebuildRootSpec sha256 hInst)
    (hmv : MerkleVerifier.new hInst root hashes = ok (.Ok mv)) :
    MerkleVerifierInv mv ∧
    mv.hashes = hashes ∧
    specRootFromLeaves sha256 hashes.val = root := by
  unfold MerkleVerifier.new at hmv
  simp only [core.slice.Slice.is_empty, Slice.length, bind_ok] at hmv
  by_cases hz : hashes.val.length = 0
  · simp [hz] at hmv
  · simp only [hz, decide_false, Bool.false_eq_true, ↓reduceIte, lift, bind_ok] at hmv
    rcases u64_checked_mul_spec_opt
        (UScalar.cast .U64 (Slice.len hashes))
        (UScalar.cast .U64 BLOCK_SIZE) with
      ⟨hnone, _⟩ | ⟨_, hsome, hmul_le, _⟩
    · simp [hnone] at hmv
    · simp only [hsome, option_is_none_some, Bool.false_eq_true, ↓reduceIte] at hmv
      rw [cast_u64_usize_eq, cast_u64_BLOCK_SIZE,
          u64OfNat_val_of_le _ (usize_val_le_u64_max _)] at hmul_le
      simp only [Slice.len_val, Slice.length, UScalar.ofNatCore_val_eq] at hmul_le
      simp only [hreb hashes hmul_le, hash_ne_spec, bind_ok] at hmv
      by_cases hne_root : specRootFromLeaves sha256 hashes.val ≠ root
      · simp [hne_root] at hmv
      · have hroot_eq : specRootFromLeaves sha256 hashes.val = root :=
          Classical.not_not.mp hne_root
        simp only [ne_eq, hroot_eq, not_true_eq_false, decide_false,
                   Bool.false_eq_true, ↓reduceIte] at hmv
        have hmv_eq : ({ hashes := hashes } : MerkleVerifier) = mv := by
          have hinj := Result.ok_injective hmv
          injection hinj
        subst hmv_eq
        exact ⟨⟨Nat.pos_of_ne_zero hz, hmul_le⟩, rfl, hroot_eq⟩

/-- **Property P1 (`MerkleVerifier.new`)**: Never panics or overflows on any inputs. -/
theorem merkle_verifier_new_no_panic_of_rebuild
    {H : Type} {sha256 : List Std.U8 → Hash} {hInst : Sha256State H}
    (hreb : RebuildRootSpec sha256 hInst)
    (root : Hash) (hashes : Slice Hash) :
    ∃ res, MerkleVerifier.new hInst root hashes = ok res := by
  unfold MerkleVerifier.new
  simp only [core.slice.Slice.is_empty, Slice.length, bind_ok]
  by_cases hz : hashes.val.length = 0
  · simp only [hz, decide_true, ↓reduceIte]; exact ⟨_, rfl⟩
  · simp only [hz, decide_false, Bool.false_eq_true, ↓reduceIte, lift, bind_ok]
    rcases u64_checked_mul_spec_opt
        (UScalar.cast .U64 (Slice.len hashes))
        (UScalar.cast .U64 BLOCK_SIZE) with
      ⟨hnone, _⟩ | ⟨_, hsome, hmul_le, _⟩
    · simp only [hnone, option_is_none_none, ↓reduceIte]; exact ⟨_, rfl⟩
    · simp only [hsome, option_is_none_some, Bool.false_eq_true, ↓reduceIte]
      rw [cast_u64_usize_eq, cast_u64_BLOCK_SIZE,
          u64OfNat_val_of_le _ (usize_val_le_u64_max _)] at hmul_le
      simp only [Slice.len_val, Slice.length, UScalar.ofNatCore_val_eq] at hmul_le
      simp only [hreb hashes hmul_le, hash_ne_spec, bind_ok]
      by_cases hne : decide (specRootFromLeaves sha256 hashes.val ≠ root) <;>
        simp only [hne, Bool.false_eq_true, ↓reduceIte] <;>
        exact ⟨_, rfl⟩

/-- **Property P3 (`MerkleVerifier.new` Completeness)**: Valid leaf hashes for any
    blob `B` are accepted. -/
theorem merkle_verifier_new_complete_of_rebuild
    {H : Type} {sha256 : List Std.U8 → Hash} {hInst : Sha256State H}
    {B : List Std.U8} {hashes : Slice Hash}
    (hreb : RebuildRootSpec sha256 hInst)
    (hleaves : hashes.val = specLeafHashes sha256 B)
    (hbound : hashes.val.length * BLOCK_BYTES ≤ U64.max) :
    ∃ mv, MerkleVerifier.new hInst (specMerkleRoot sha256 B) hashes = ok (.Ok mv) := by
  have hpos : 0 < hashes.val.length := by rw [hleaves]; exact specLeafHashes_pos sha256 B
  have hnz : hashes.val.length ≠ 0 := by omega
  refine ⟨{ hashes }, ?_⟩
  unfold MerkleVerifier.new
  simp only [core.slice.Slice.is_empty, Slice.length, hnz, decide_false, Bool.false_eq_true,
             ↓reduceIte, lift, bind_ok]
  rcases u64_checked_mul_spec_opt
      (UScalar.cast .U64 (Slice.len hashes))
      (UScalar.cast .U64 BLOCK_SIZE) with
    ⟨_, hovf⟩ | ⟨_, hsome, _, _⟩
  · rw [cast_u64_usize_eq, cast_u64_BLOCK_SIZE,
        u64OfNat_val_of_le _ (usize_val_le_u64_max _)] at hovf
    simp only [Slice.len_val, Slice.length, UScalar.ofNatCore_val_eq] at hovf
    unfold BLOCK_BYTES at hbound
    omega
  · simp only [hsome, option_is_none_some, Bool.false_eq_true, ↓reduceIte,
               hreb hashes hbound, hash_ne_spec, bind_ok]
    simp [hleaves, specMerkleRoot]

/-- **Property P4 (`MerkleVerifier.new` Soundness)**: Accepting `hashes` of the
    expected leaf count against `specMerkleRoot sha256 B` guarantees that
    `mv.hashes()` equals `specLeafHashes sha256 B` (or `HashCollision sha256`). -/
theorem merkle_verifier_new_sound_of_rebuild
    {H : Type} {sha256 : List Std.U8 → Hash} {hInst : Sha256State H}
    {B : List Std.U8} {hashes out_hashes : Slice Hash} {mv : MerkleVerifier}
    (hreb : RebuildRootSpec sha256 hInst)
    (hlen : hashes.val.length = (specLeafHashes sha256 B).length)
    (hmv : MerkleVerifier.new hInst (specMerkleRoot sha256 B) hashes = ok (.Ok mv))
    (hget : MerkleVerifier.impl.hashes mv = ok out_hashes) :
    out_hashes.val = specLeafHashes sha256 B ∨ HashCollision sha256 := by
  obtain ⟨hinv, hmv_h, hroot_eq⟩ := merkle_verifier_new_inv_of_rebuild hreb hmv
  unfold MerkleVerifier.impl.hashes at hget
  have hout : mv.hashes = out_hashes := Result.ok_injective hget
  rw [← hout, hmv_h]
  exact specRootFromLeaves_eq_imp_eq_or_collision_lem hlen (hmv_h ▸ hinv.hbound) hroot_eq

/-- **Property P5 (`MerkleVerifier.new` Zero-Hash Leaf Extension)**:
    Without an external leaf-count check, `MerkleVerifier.new` accepts
    `leaves ++ [zeroHash]` for every hash function `sha256` whenever
    `2 ≤ leaves.length` and `leaves.length % CHILDREN_PER_NODE ≠ 0`. -/
theorem merkle_verifier_zero_hash_extension_of_rebuild
    {H : Type} {sha256 : List Std.U8 → Hash} {hInst : Sha256State H}
    {leaves : List Hash} {extHashes : Slice Hash}
    (hreb : RebuildRootSpec sha256 hInst)
    (hge2 : 2 ≤ leaves.length)
    (hrem : leaves.length % CHILDREN_PER_NODE ≠ 0)
    (hu64 : (leaves.length + 1) * BLOCK_BYTES ≤ U64.max)
    (hext_val : extHashes.val = leaves ++ [zeroHash]) :
    ∃ mv, MerkleVerifier.new hInst (specRootFromLeaves sha256 leaves) extHashes = ok (.Ok mv) := by
  have hext_len : extHashes.val.length = leaves.length + 1 := by simp [hext_val]
  have hnz : extHashes.val.length ≠ 0 := by omega
  have hbound : extHashes.val.length * BLOCK_BYTES ≤ U64.max := by rw [hext_len]; exact hu64
  refine ⟨{ hashes := extHashes }, ?_⟩
  unfold MerkleVerifier.new
  simp only [core.slice.Slice.is_empty, Slice.length, hnz, decide_false, Bool.false_eq_true,
             ↓reduceIte, lift, bind_ok]
  rcases u64_checked_mul_spec_opt
      (UScalar.cast .U64 (Slice.len extHashes))
      (UScalar.cast .U64 BLOCK_SIZE) with
    ⟨_, hovf⟩ | ⟨_, hsome, _, _⟩
  · rw [cast_u64_usize_eq, cast_u64_BLOCK_SIZE,
        u64OfNat_val_of_le _ (usize_val_le_u64_max _)] at hovf
    simp only [Slice.len_val, Slice.length, hext_len, UScalar.ofNatCore_val_eq] at hovf
    unfold BLOCK_BYTES at hu64
    omega
  · simp only [hsome, option_is_none_some, Bool.false_eq_true, ↓reduceIte,
               hreb extHashes hbound, hash_ne_spec, bind_ok]
    rw [hext_val, specRootFromLeaves_append_zeroHash_lem sha256 leaves hge2 hrem]
    simp

-- ============================================================================
-- 2. `MerkleVerifier.verify` -- Totality (P1), Completeness (P3), & Soundness (P4)
-- ============================================================================

theorem verify_blocks_loop_gen_spec
    {H : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (mv : MerkleVerifier)
    (offset : Std.Usize) (data : Slice Std.U8) (base_block : Std.Usize)
    (hoff : offset.val % 8192 = 0)
    (hsum : data.val.length + offset.val ≤ Usize.max)
    (hbase : base_block.val = offset.val / 8192)
    (hend : base_block.val + (data.val.length + 8191) / 8192 ≤ mv.hashes.val.length) :
    ∀ (rem : Nat) (pos i : Std.Usize),
      pos.val = min data.val.length (i.val * 8192) →
      i.val ≤ (data.val.length + 8191) / 8192 →
      (data.val.length + 8191) / 8192 - i.val = rem →
      ∃ res,
        MerkleVerifier.verify_blocks_loop hInst mv offset data base_block pos i = ok res ∧
        (res = .Ok () ↔
          ∀ k, i.val ≤ k → k < (data.val.length + 8191) / 8192 →
            ∀ (hk : base_block.val + k < mv.hashes.val.length),
              mv.hashes.val[base_block.val + k]'hk =
                specHashBlock sha256 ((data.val.drop (k * 8192)).take 8192)
                  (u64OfNat ((base_block.val + k) * 8192)))
  | 0, pos, i, hpos, _, _ => by
    have hi_eq : i.val = (data.val.length + 8191) / 8192 := by omega
    have hge_len : data.val.length ≤ i.val * 8192 := by
      rw [hi_eq]
      by_cases h0 : data.val.length = 0
      · simp [h0]
      · have h1 := Nat.div_add_mod (data.val.length - 1) 8192
        have h2 : (data.val.length + 8191) / 8192 = (data.val.length - 1) / 8192 + 1 := by omega
        omega
    have hpos_eq : pos.val = data.val.length := by rw [hpos]; omega
    have hbody :
        MerkleVerifier.verify_blocks_loop.body hInst mv offset data base_block pos i =
          ok (.done (.Ok ())) := by
      unfold MerkleVerifier.verify_blocks_loop.body
      have hlt : ¬ (pos < Slice.len data) := by
        simp only [UScalar.lt_equiv, Slice.len_val, Slice.length]; omega
      simp [hlt]
    refine ⟨.Ok (), ?_, ?_⟩
    · unfold MerkleVerifier.verify_blocks_loop
      exact loop_done _ (pos, i) _ hbody
    · simp only [true_iff]
      intro k hk1 hk2
      omega
  | rem + 1, pos, i, hpos, _, _ => by
    have hi_lt : i.val < (data.val.length + 8191) / 8192 := by omega
    have hmul_lt : i.val * 8192 < data.val.length :=
      mul_lt_of_lt_div_ceil (k := 8192) (by decide) hi_lt
    have hpos_val : pos.val = i.val * 8192 := by rw [hpos]; omega
    have hlt_data : pos < Slice.len data := by
      simp only [UScalar.lt_equiv, Slice.len_val, Slice.length]; omega
    obtain ⟨rem_bytes, hsub, hrem_bytes⟩ := uscalar_sub_ok (Slice.len data) pos (by simp; omega)
    simp only [Slice.len_val, Slice.length] at hrem_bytes
    obtain ⟨chunk_len, hmin, hchunk_len⟩ := min_usize_spec BLOCK_SIZE rem_bytes
    rw [BLOCK_SIZE_val, hrem_bytes, hpos_val] at hchunk_len
    obtain ⟨pos1, hadd_pos, hpos1_val⟩ := uscalar_add_ok pos chunk_len (by
      rw [uscalar_max_usize, hpos_val, hchunk_len]; have := data.property; omega)
    obtain ⟨chunk, hslice, hchunk_val⟩ := slice_range_ok data pos pos1
      (by omega) (by rw [hpos1_val, hpos_val, hchunk_len]; omega)
    have hchunk_eq : chunk.val = (data.val.drop (i.val * 8192)).take 8192 := by
      rw [hchunk_val, hpos_val, show pos1.val - i.val * 8192 = chunk_len.val by omega, hchunk_len]
      rcases Nat.le_total 8192 (data.val.length - i.val * 8192) with hle | hle
      · rw [Nat.min_eq_left hle]
      · rw [Nat.min_eq_right hle, List.take_of_length_le (by simp),
            List.take_of_length_le (by simp; omega)]
    have hu64_sum : offset.val + pos.val ≤ U64.max := by
      have ⟨_, hu64⟩ := usize_bounds
      omega
    obtain ⟨block_offset, hadd64, hboff_val⟩ :=
      uscalar_add_ok (UScalar.cast .U64 offset) (UScalar.cast .U64 pos) (by
        rw [uscalar_max_u64, cast_u64_usize_eq, cast_u64_usize_eq,
            u64OfNat_val_of_le _ (usize_val_le_u64_max _),
            u64OfNat_val_of_le _ (usize_val_le_u64_max _)]
        exact hu64_sum)
    have hboff_eq : block_offset = u64OfNat ((base_block.val + i.val) * 8192) := by
      apply UScalar.eq_of_val_eq
      rw [hboff_val, cast_u64_usize_eq, cast_u64_usize_eq,
          u64OfNat_val_of_le _ (usize_val_le_u64_max _),
          u64OfNat_val_of_le _ (usize_val_le_u64_max _),
          u64OfNat_val_of_le _ (by rw [hpos_val] at hu64_sum; omega),
          hpos_val, hbase]
      omega
    have hboff_mod : block_offset.val % BLOCK_BYTES = 0 := by
      rw [hboff_eq, u64OfNat_val_of_le _ (by rw [hpos_val] at hu64_sum; omega)]
      unfold BLOCK_BYTES; omega
    have hchunk_le : chunk.val.length ≤ BLOCK_BYTES := by
      simp [hchunk_eq, BLOCK_BYTES]
    have hhb := hash_block_gen_spec sha256 hInst R_H chunk block_offset hsha hchunk_le hboff_mod
    rw [hchunk_eq, hboff_eq] at hhb
    have hidx_lt : base_block.val + i.val < mv.hashes.val.length := by omega
    have hmv_usize : mv.hashes.val.length ≤ Usize.max := mv.hashes.property
    obtain ⟨idx, hadd_idx, hidx_val⟩ := uscalar_add_ok base_block i (by
      rw [uscalar_max_usize]; omega)
    have hv_idx := slice_index_ok mv.hashes idx (by omega)
    set expected_i := specHashBlock sha256 ((data.val.drop (i.val * 8192)).take 8192)
                        (u64OfNat ((base_block.val + i.val) * 8192))
    set stored_i := mv.hashes.val[base_block.val + i.val]'hidx_lt
    have hv_idx' : Slice.index_usize mv.hashes idx = ok stored_i := by
      simpa [stored_i, hidx_val] using hv_idx
    by_cases hmatch : stored_i = expected_i
    · have hne_false :
          core.cmp.PartialEq.ne.trait_default Hash.Insts.CoreCmpPartialEqHash stored_i expected_i =
            ok false := by
        simp [hash_ne_spec, hmatch]
      obtain ⟨i1, hadd_i1, hi1_val⟩ := uscalar_add_ok i 1#usize (by
        rw [uscalar_max_usize]; simp only [UScalar.ofNatCore_val_eq]; omega)
      simp only [UScalar.ofNatCore_val_eq] at hi1_val
      have hbody :
          MerkleVerifier.verify_blocks_loop.body hInst mv offset data base_block pos i =
            ok (.cont (pos1, i1)) := by
        unfold MerkleVerifier.verify_blocks_loop.body
        simp only [hlt_data, ↓reduceIte, hsub, hmin, hadd_pos, bind_ok]
        refine bind_ok_step hslice ?_
        simp only [lift, hadd64, hboff_eq, hhb, hadd_idx, hv_idx', hne_false,
                   Bool.false_eq_true, ↓reduceIte, hadd_i1, bind_ok]
      have hpos1_inv : pos1.val = min data.val.length (i1.val * 8192) := by
        rw [hpos1_val, hpos_val, hchunk_len, hi1_val]
        omega
      obtain ⟨res, ih_exec, ih_iff⟩ :=
        verify_blocks_loop_gen_spec hsha mv offset data base_block hoff hsum hbase hend
          rem pos1 i1 hpos1_inv (by omega) (by omega)
      refine ⟨res, ?_, ?_⟩
      · unfold MerkleVerifier.verify_blocks_loop at ih_exec ⊢
        rw [loop_cont _ (pos, i) _ hbody]
        exact ih_exec
      · rw [ih_iff]
        constructor
        · intro hall k hk_ge hk_lt hk_in
          by_cases hki : k = i.val
          · subst hki; exact hmatch
          · exact hall k (by omega) hk_lt hk_in
        · intro hall k hk_ge hk_lt hk_in
          exact hall k (by omega) hk_lt hk_in
    · have hne_true :
          core.cmp.PartialEq.ne.trait_default Hash.Insts.CoreCmpPartialEqHash stored_i expected_i =
            ok true := by
        simp [hash_ne_spec, hmatch]
      have hbody :
          MerkleVerifier.verify_blocks_loop.body hInst mv offset data base_block pos i =
            ok (.done (.Err .IoDataIntegrity)) := by
        unfold MerkleVerifier.verify_blocks_loop.body
        simp only [hlt_data, ↓reduceIte, hsub, hmin, hadd_pos, bind_ok]
        refine bind_ok_step hslice ?_
        simp only [lift, hadd64, hboff_eq, hhb, hadd_idx, hv_idx', hne_true, ↓reduceIte, bind_ok]
      refine ⟨.Err .IoDataIntegrity, ?_, ?_⟩
      · unfold MerkleVerifier.verify_blocks_loop
        exact loop_done _ (pos, i) _ hbody
      · simp only [reduceCtorEq, false_iff]
        intro hall
        exact hmatch (hall i.val (le_refl _) hi_lt hidx_lt)

theorem verify_blocks_gen_spec
    {H : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (mv : MerkleVerifier)
    (offset : Std.Usize) (data : Slice Std.U8) (base_block : Std.Usize)
    (hoff : offset.val % 8192 = 0)
    (hsum : data.val.length + offset.val ≤ Usize.max)
    (hbase : base_block.val = offset.val / 8192)
    (hend : base_block.val + (data.val.length + 8191) / 8192 ≤ mv.hashes.val.length) :
    ∃ res,
      MerkleVerifier.verify_blocks hInst mv offset data base_block = ok res ∧
      (res = .Ok () ↔
        ∀ k, k < (data.val.length + 8191) / 8192 →
          ∀ (hk : base_block.val + k < mv.hashes.val.length),
            mv.hashes.val[base_block.val + k]'hk =
              specHashBlock sha256 ((data.val.drop (k * 8192)).take 8192)
                (u64OfNat ((base_block.val + k) * 8192))) := by
  unfold MerkleVerifier.verify_blocks
  obtain ⟨res, hexec, hiff⟩ :=
    verify_blocks_loop_gen_spec hsha mv offset data base_block hoff hsum hbase hend
      ((data.val.length + 8191) / 8192) 0#usize 0#usize (by simp) (Nat.zero_le _) (by simp)
  refine ⟨res, hexec, ?_⟩
  rw [hiff]
  simp

/-- **Property P1 (`MerkleVerifier.verify`)**: Total panic & overflow freedom. -/
theorem merkle_verifier_verify_no_panic_lem
    {H : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (mv : MerkleVerifier) (offset : Std.Usize) (data : Slice Std.U8) :
    ∃ res, MerkleVerifier.verify hInst mv offset data = ok res := by
  unfold MerkleVerifier.verify
  simp only [usize_is_multiple_of_eq, bind_ok]
  by_cases hoff : offset.val % BLOCK_SIZE.val = 0
  · simp only [hoff, decide_true, ↓reduceIte]
    rw [BLOCK_SIZE_val] at hoff
    by_cases hemp : data.val.length = 0
    · have his_emp : core.slice.Slice.is_empty data = ok true := by
        simp [core.slice.Slice.is_empty, Slice.length, hemp]
      simp only [his_emp, bind_ok, ↓reduceIte]
      by_cases hoff0 : offset != 0#usize
      · simp only [hoff0, ↓reduceIte]; exact ⟨_, rfl⟩
      · simp only [hoff0, Bool.false_eq_true, ↓reduceIte]
        by_cases hlen1 : Slice.len mv.hashes != 1#usize
        · simp only [hlen1, ↓reduceIte]; exact ⟨_, rfl⟩
        · simp only [hlen1, Bool.false_eq_true, ↓reduceIte, lift, bind_ok]
          have hmv1 : mv.hashes.val.length = 1 := by
            have hbeq : Slice.len mv.hashes = 1#usize := by
              by_contra hne; exact hlen1 (bne_iff_ne.mpr hne)
            simpa using congrArg UScalar.val hbeq
          have hhb0 := hash_block_gen_spec sha256 hInst R_H
            (Std.Array.empty Std.U8).to_slice 0#u64 hsha (by simp [BLOCK_BYTES]) (by simp)
          have hv0 := slice_index_ok mv.hashes 0#usize (by simp [hmv1])
          simp only [hhb0, hv0, hash_eq_spec, UScalar.ofNatCore_val_eq, bind_ok]
          by_cases heq_h : decide (mv.hashes.val[0]'(by omega) =
              specHashBlock sha256 (Std.Array.empty Std.U8).to_slice.val 0#u64) <;>
            simp only [heq_h, Bool.false_eq_true, ↓reduceIte] <;>
            exact ⟨_, rfl⟩
    · have his_emp : core.slice.Slice.is_empty data = ok false := by
        simp [core.slice.Slice.is_empty, Slice.length, hemp]
      rcases usize_checked_add_spec_opt (Slice.len data) offset with
        ⟨hnone, _⟩ | ⟨ending, hsome, hsum, hend_val⟩
      · refine ⟨.Err .InvalidArgs, ?_⟩
        refine bind_ok_step his_emp ?_
        simp only [Bool.false_eq_true, ↓reduceIte, lift, hnone, bind_ok]
      · simp only [Slice.len_val, Slice.length] at hsum hend_val
        obtain ⟨hend_idx, hceil, hceil_val⟩ := div_ceil_usize_spec ending BLOCK_SIZE (by simp)
        rw [BLOCK_SIZE_val, hend_val] at hceil_val
        by_cases hgt : hend_idx > Slice.len mv.hashes
        · refine ⟨.Err .InvalidArgs, ?_⟩
          refine bind_ok_step his_emp ?_
          simp only [Bool.false_eq_true, ↓reduceIte, lift, hsome, hceil, hgt, ↓reduceIte, bind_ok]
        · have hle_hashes : (data.val.length + offset.val + 8191) / 8192 ≤ mv.hashes.val.length := by
            have : hend_idx.val ≤ (Slice.len mv.hashes).val := by scalar_tac
            simpa [hceil_val] using this
          obtain ⟨base_block, hdiv, hbase_val⟩ := uscalar_div_ok offset BLOCK_SIZE (by simp)
          rw [BLOCK_SIZE_val] at hbase_val
          have hend_blocks :
              base_block.val + (data.val.length + 8191) / 8192 ≤ mv.hashes.val.length := by
            have hdecomp := div_ceil_add_aligned (k := 8192) (len := data.val.length) (by decide) hoff
            rw [Nat.add_comm offset.val data.val.length] at hdecomp
            omega
          obtain ⟨res, hvblk, _⟩ := verify_blocks_gen_spec hsha mv offset data base_block
            hoff hsum hbase_val hend_blocks
          by_cases hmult : ending.val % BLOCK_SIZE.val = 0
          · refine ⟨res, ?_⟩
            refine bind_ok_step his_emp ?_
            simp only [Bool.false_eq_true, ↓reduceIte, lift, hsome, hceil, hgt,
                       hmult, decide_true, hdiv, hvblk, bind_ok]
          · by_cases hne_end : hend_idx != Slice.len mv.hashes
            · refine ⟨.Err .InvalidArgs, ?_⟩
              refine bind_ok_step his_emp ?_
              simp only [Bool.false_eq_true, ↓reduceIte, lift, hsome, hceil, hgt,
                         hmult, decide_false, hne_end, bind_ok]
            · refine ⟨res, ?_⟩
              refine bind_ok_step his_emp ?_
              simp only [Bool.false_eq_true, ↓reduceIte, lift, hsome, hceil, hgt,
                         hmult, decide_false, hne_end, hdiv, hvblk, bind_ok]
  · simp only [hoff, decide_false, Bool.false_eq_true, ↓reduceIte]; exact ⟨_, rfl⟩

/-- **Property P3 (`MerkleVerifier.verify` Completeness on Empty Blob)**:
    `verify(0, &[])` succeeds with `ok (.Ok ())` on `B = []`. -/
theorem merkle_verifier_verify_complete_empty_of_inv
    {H : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {mv : MerkleVerifier} {data : Slice Std.U8}
    (hsha : ValidSha256State sha256 hInst R_H)
    (_hmv_inv : MerkleVerifierInv mv)
    (hmv_hashes : mv.hashes.val = specLeafHashes sha256 [])
    (hdata : data.val = []) :
    MerkleVerifier.verify hInst mv 0#usize data = ok (.Ok ()) := by
  unfold MerkleVerifier.verify
  have his_emp : core.slice.Slice.is_empty data = ok true := by
    simp [core.slice.Slice.is_empty, Slice.length, hdata]
  have hmv_len : mv.hashes.val.length = 1 := by simp [hmv_hashes, specLeafHashes]
  have hmv_len_u : Slice.len mv.hashes = 1#usize :=
    UScalar.eq_of_val_eq (by simp [hmv_len])
  have hhb0 := hash_block_gen_spec sha256 hInst R_H
    (Std.Array.empty Std.U8).to_slice 0#u64 hsha (by simp [BLOCK_BYTES]) (by simp)
  have hv0 := slice_index_ok mv.hashes 0#usize (by simp [hmv_len])
  have h0_eq : mv.hashes.val[0]'(by simp [hmv_len]) =
      specHashBlock sha256 (Std.Array.empty Std.U8).to_slice.val 0#u64 := by
    simp [hmv_hashes, specLeafHashes, Std.Array.empty]
  simp only [usize_is_multiple_of_eq, UScalar.ofNatCore_val_eq, Nat.zero_mod,
             decide_true, ↓reduceIte, bind_ok]
  refine bind_ok_step his_emp ?_
  simp only [↓reduceIte, bne_self_eq_false, Bool.false_eq_true, hmv_len_u, lift,
             hhb0, hv0, hash_eq_spec, UScalar.ofNatCore_val_eq, h0_eq, decide_true, bind_ok]

/-- **Property P3 (`MerkleVerifier.verify` Completeness on Non-Empty Slices)**:
    Every valid block-aligned non-empty slice `data` of `B` is accepted. -/
theorem merkle_verifier_verify_complete_of_inv
    {H : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {B : List Std.U8} {mv : MerkleVerifier}
    {offset : Std.Usize} {data : Slice Std.U8}
    (hsha : ValidSha256State sha256 hInst R_H)
    (_hmv_inv : MerkleVerifierInv mv)
    (hmv_hashes : mv.hashes.val = specLeafHashes sha256 B)
    (hoff : offset.val % BLOCK_BYTES = 0)
    (hdne : data.val ≠ [])
    (hsum_usize : offset.val + data.val.length ≤ Usize.max)
    (hin_blob : offset.val + data.val.length ≤ B.length)
    (halign_end : (offset.val + data.val.length) % BLOCK_BYTES = 0 ∨
                  offset.val + data.val.length = B.length)
    (hslice_eq : data.val = (B.drop offset.val).take data.val.length) :
    MerkleVerifier.verify hInst mv offset data = ok (.Ok ()) := by
  have hoff8 : offset.val % 8192 = 0 := hoff
  have hdpos : 0 < data.val.length := List.length_pos_iff.mpr hdne
  have hBne : B ≠ [] := List.ne_nil_of_length_pos (by omega)
  have hBemp : B.isEmpty = false := List.isEmpty_eq_false_iff.mpr hBne
  have hmv_len : mv.hashes.val.length = (B.length + 8191) / 8192 := by
    rw [hmv_hashes, specLeafHashes_length, hBemp]; simp
  unfold MerkleVerifier.verify
  have his_emp : core.slice.Slice.is_empty data = ok false := by
    simp [core.slice.Slice.is_empty, Slice.length, Nat.ne_of_gt hdpos]
  simp only [usize_is_multiple_of_eq, BLOCK_SIZE_val, hoff8, decide_true, ↓reduceIte, bind_ok]
  refine bind_ok_step his_emp ?_
  simp only [Bool.false_eq_true, ↓reduceIte, lift, bind_ok]
  rcases usize_checked_add_spec_opt (Slice.len data) offset with
    ⟨_, hovf⟩ | ⟨ending, hsome, hsum, hend_val⟩
  · simp only [Slice.len_val, Slice.length] at hovf; omega
  · simp only [hsome]
    simp only [Slice.len_val, Slice.length] at hsum hend_val
    obtain ⟨hend_idx, hceil, hceil_val⟩ := div_ceil_usize_spec ending BLOCK_SIZE (by simp)
    rw [BLOCK_SIZE_val, hend_val] at hceil_val
    have hend_le_mv : hend_idx.val ≤ mv.hashes.val.length := by
      rw [hceil_val, hmv_len]
      exact Nat.div_le_div_right (by omega)
    have hgt : ¬ (hend_idx > Slice.len mv.hashes) :=
      not_usize_gt_of_le (by simpa using hend_le_mv)
    simp only [hceil, hgt, ↓reduceIte, bind_ok]
    obtain ⟨base_block, hdiv, hbase_val⟩ := uscalar_div_ok offset BLOCK_SIZE (by simp)
    rw [BLOCK_SIZE_val] at hbase_val
    have hend_blocks :
        base_block.val + (data.val.length + 8191) / 8192 ≤ mv.hashes.val.length := by
      have hdecomp := div_ceil_add_aligned (k := 8192) (len := data.val.length) (by decide) hoff8
      rw [Nat.add_comm offset.val data.val.length] at hdecomp
      omega
    obtain ⟨res, hvblk, hiff⟩ := verify_blocks_gen_spec hsha mv offset data base_block
      hoff8 hsum hbase_val hend_blocks
    have hall_ok : res = .Ok () := by
      apply hiff.mpr
      intro k hk_lt hk_in
      have hk_leaf : base_block.val + k < (specLeafHashes sha256 B).length := by
        simpa [hmv_hashes] using hk_in
      obtain ⟨hleaf_eq, _, _⟩ := specLeafHashes_getElem_of_nonempty sha256 B hBne
        (base_block.val + k) hk_leaf
      have hget_mv : mv.hashes.val[base_block.val + k]'hk_in =
          (specLeafHashes sha256 B)[base_block.val + k]'hk_leaf := by
        simp [hmv_hashes]
      rw [hget_mv, hleaf_eq]
      have hmul_off : (base_block.val + k) * 8192 = offset.val + k * 8192 := by
        rw [Nat.add_mul, hbase_val, Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hoff8)]
      have hblk_eq :
          (B.drop ((base_block.val + k) * 8192)).take 8192 =
          (data.val.drop (k * 8192)).take 8192 := by
        rw [hslice_eq, List.drop_take, List.drop_drop, hmul_off]
        rcases halign_end with hmod0 | heq_B
        · unfold BLOCK_BYTES at hmod0
          have hrem_ge : 8192 ≤ data.val.length - k * 8192 := by
            have hk_mul :=
              mul_lt_of_lt_div_ceil (i := k) (n := data.val.length) (k := 8192) (by decide) hk_lt
            omega
          rw [List.take_take, Nat.min_eq_left hrem_ge]
        · have hrem_eq : data.val.length - k * 8192 = (B.drop (offset.val + k * 8192)).length := by
            simp [List.length_drop]; omega
          rw [hrem_eq, List.take_length]
      rw [hblk_eq]
    by_cases hmult : ending.val % 8192 = 0
    · simp only [hmult, decide_true, ↓reduceIte, hdiv, hvblk, hall_ok, bind_ok]
    · have heq_B : offset.val + data.val.length = B.length := by
        rcases halign_end with h0 | hB
        · exfalso; apply hmult
          rw [hend_val, Nat.add_comm]; exact h0
        · exact hB
      have hend_eq_len : hend_idx = Slice.len mv.hashes := by
        apply UScalar.eq_of_val_eq
        rw [hceil_val, Slice.len_val, Slice.length, hmv_len]
        omega
      simp only [hmult, decide_false, Bool.false_eq_true, ↓reduceIte,
                 hend_eq_len, bne_self_eq_false, hdiv, hvblk, hall_ok, bind_ok]

/-- **Property P4 (`MerkleVerifier.verify` Soundness)**:
    Accepting `(offset, data)` against `mv` (`mv.hashes.val = specLeafHashes sha256 B`)
    guarantees empty-blob authenticity when `data = []` and exact subslice authenticity
    when `data ≠ []`, or witnesses `HashCollision sha256`. -/
theorem merkle_verifier_verify_sound_of_inv
    {H : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {B : List Std.U8} {mv : MerkleVerifier}
    {offset : Std.Usize} {data : Slice Std.U8}
    (hsha : ValidSha256State sha256 hInst R_H)
    (_hmv_inv : MerkleVerifierInv mv)
    (hmv_hashes : mv.hashes.val = specLeafHashes sha256 B)
    (hok : MerkleVerifier.verify hInst mv offset data = ok (.Ok ())) :
    ((data.val = [] → B = [] ∧ offset.val = 0) ∧
     (data.val ≠ [] →
       offset.val + data.val.length ≤ B.length ∧
       data.val = (B.drop offset.val).take data.val.length)) ∨
    HashCollision sha256 := by
  unfold MerkleVerifier.verify at hok
  simp only [usize_is_multiple_of_eq, bind_ok] at hok
  by_cases hoff : offset.val % BLOCK_SIZE.val = 0
  · simp only [hoff, decide_true, ↓reduceIte] at hok
    rw [BLOCK_SIZE_val] at hoff
    by_cases hemp : data.val.length = 0
    · have hdata_nil : data.val = [] := List.eq_nil_of_length_eq_zero hemp
      have his_emp : core.slice.Slice.is_empty data = ok true := by
        simp [core.slice.Slice.is_empty, Slice.length, hemp]
      rw [bind_ok_step his_emp rfl] at hok
      simp only [↓reduceIte] at hok
      by_cases hoff0 : offset != 0#usize
      · simp [hoff0] at hok
      · have hoff_eq0 : offset = 0#usize := by
          by_contra hne; exact hoff0 (bne_iff_ne.mpr hne)
        simp only [hoff0, Bool.false_eq_true, ↓reduceIte] at hok
        by_cases hlen1 : Slice.len mv.hashes != 1#usize
        · simp [hlen1] at hok
        · have hmv1 : mv.hashes.val.length = 1 := by
            have hbeq : Slice.len mv.hashes = 1#usize := by
              by_contra hne; exact hlen1 (bne_iff_ne.mpr hne)
            simpa using congrArg UScalar.val hbeq
          simp only [hlen1, Bool.false_eq_true, ↓reduceIte, lift, bind_ok] at hok
          have hhb0 := hash_block_gen_spec sha256 hInst R_H
            (Std.Array.empty Std.U8).to_slice 0#u64 hsha (by simp [BLOCK_BYTES]) (by simp)
          have hemp_arr : (Std.Array.empty Std.U8).to_slice.val = [] := by simp [Std.Array.empty]
          rw [hemp_arr] at hhb0
          have hv0 := slice_index_ok mv.hashes 0#usize (by simp [hmv1])
          simp only [hhb0, hv0, hash_eq_spec, UScalar.ofNatCore_val_eq, bind_ok] at hok
          have h0_eq : mv.hashes.val[0]'(by omega) = specHashBlock sha256 [] 0#u64 := by
            by_contra hne
            simp [hne] at hok
          by_cases hBnil : B = []
          · exact Or.inl ⟨fun _ => ⟨hBnil, by simp [hoff_eq0]⟩, fun hc => absurd hdata_nil hc⟩
          · have hk0 : 0 < (specLeafHashes sha256 B).length := specLeafHashes_pos sha256 B
            obtain ⟨hB0, hB0_pos, hB0_le⟩ := specLeafHashes_getElem_of_nonempty sha256 B hBnil 0 hk0
            have hu0_eq : u64OfNat 0 = 0#u64 := UScalar.eq_of_val_eq (by simp)
            have heq_hash :
                specHashBlock sha256 ((B.drop 0).take 8192) 0#u64 =
                specHashBlock sha256 [] 0#u64 := by
              have : mv.hashes.val[0]'(by omega) = (specLeafHashes sha256 B)[0]'hk0 := by
                simp [hmv_hashes]
              rw [this, hB0, Nat.zero_mul, hu0_eq] at h0_eq
              exact h0_eq
            rcases specHashBlock_eq_imp_eq_or_collision hB0_le (by simp [BLOCK_BYTES])
                (by simp) (by simp) heq_hash with ⟨hblk_eq, _⟩ | hcol
            · rw [hblk_eq] at hB0_pos; simp at hB0_pos
            · exact Or.inr hcol
    · have hdata_ne : data.val ≠ [] := List.ne_nil_of_length_pos (Nat.pos_of_ne_zero hemp)
      have his_emp : core.slice.Slice.is_empty data = ok false := by
        simp [core.slice.Slice.is_empty, Slice.length, hemp]
      rw [bind_ok_step his_emp rfl] at hok
      simp only [Bool.false_eq_true, ↓reduceIte, lift, bind_ok] at hok
      rcases usize_checked_add_spec_opt (Slice.len data) offset with
        ⟨hnone, _⟩ | ⟨ending, hsome, hsum, hend_val⟩
      · simp [hnone] at hok
      · simp only [hsome] at hok
        simp only [Slice.len_val, Slice.length] at hsum hend_val
        obtain ⟨hend_idx, hceil, hceil_val⟩ := div_ceil_usize_spec ending BLOCK_SIZE (by simp)
        rw [BLOCK_SIZE_val, hend_val] at hceil_val
        rw [hceil, bind_ok] at hok
        by_cases hgt : hend_idx > Slice.len mv.hashes
        · simp [hgt] at hok
        · simp only [hgt, ↓reduceIte] at hok
          have hle_hashes : (data.val.length + offset.val + 8191) / 8192 ≤ mv.hashes.val.length := by
            have : hend_idx.val ≤ (Slice.len mv.hashes).val := by scalar_tac
            simpa [hceil_val] using this
          obtain ⟨base_block, hdiv, hbase_val⟩ := uscalar_div_ok offset BLOCK_SIZE (by simp)
          rw [BLOCK_SIZE_val] at hbase_val
          have hend_blocks :
              base_block.val + (data.val.length + 8191) / 8192 ≤ mv.hashes.val.length := by
            have hdecomp := div_ceil_add_aligned (k := 8192) (len := data.val.length) (by decide) hoff
            rw [Nat.add_comm offset.val data.val.length] at hdecomp
            omega
          obtain ⟨res, hvblk, hiff⟩ := verify_blocks_gen_spec hsha mv offset data base_block
            hoff hsum hbase_val hend_blocks
          have hres_ok : res = .Ok () := by
            by_cases hmult : ending.val % BLOCK_SIZE.val = 0
            · simp only [hmult, decide_true, ↓reduceIte, hdiv, hvblk, bind_ok] at hok
              exact Result.ok_injective hok
            · simp only [hmult, decide_false, Bool.false_eq_true, ↓reduceIte] at hok
              by_cases hne_end : hend_idx != Slice.len mv.hashes
              · simp [hne_end] at hok
              · simp only [hne_end, Bool.false_eq_true, ↓reduceIte, hdiv, hvblk, bind_ok] at hok
                exact Result.ok_injective hok
          have hall := hiff.mp hres_ok
          have hsum64 : offset.val + data.val.length ≤ U64.max := by
            have ⟨_, hu64⟩ := usize_bounds
            omega
          have hall_leaves :
              ∀ k < (data.val.length + 8191) / 8192,
                ∃ hk : offset.val / 8192 + k < (specLeafHashes sha256 B).length,
                  (specLeafHashes sha256 B)[offset.val / 8192 + k]'hk =
                    specHashBlock sha256 ((data.val.drop (k * 8192)).take 8192)
                      (u64OfNat ((offset.val / 8192 + k) * 8192)) := by
            intro k hk_lt
            have hk_mv : base_block.val + k < mv.hashes.val.length := by omega
            have hk_sl : offset.val / 8192 + k < (specLeafHashes sha256 B).length := by
              rw [← hbase_val, ← hmv_hashes]; exact hk_mv
            refine ⟨hk_sl, ?_⟩
            have hstep := hall k hk_lt hk_mv
            have hmv_eq : mv.hashes.val[base_block.val + k]'hk_mv =
                (specLeafHashes sha256 B)[offset.val / 8192 + k]'hk_sl := by
              simp [hmv_hashes, hbase_val]
            rw [← hmv_eq, hstep, hbase_val]
          rcases blocks_match_leaves_sound sha256 B data.val offset.val hoff hdata_ne
              hsum64 hall_leaves with ⟨hin_B, hslice⟩ | hcol
          · exact Or.inl ⟨fun hc => absurd hc hdata_ne, fun _ => ⟨hin_B, hslice⟩⟩
          · exact Or.inr hcol
  · simp only [hoff, decide_false, Bool.false_eq_true, ↓reduceIte] at hok
    cases Result.ok_injective hok

-- ============================================================================
-- 3. `MerkleVerifier.verify_aligned` -- Totality (P1), Completeness (P3),
--    and Soundness & Page-Tail Zero Integrity (P4)
-- ============================================================================

theorem verify_aligned_blocks_loop_spec
    {Src : Type} {sha256 : List Std.U8 → Hash}
    {srcInst : AlignedByteSource Src} {src : Src} {slice : Slice Std.U8}
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice)
    (mv : MerkleVerifier) (offset len unaligned_len base_block num_blocks : Std.Usize)
    (hlen_eq : len.val = slice.val.length)
    (hoff : offset.val % 8192 = 0)
    (hpage : slice.val.length % 4096 = 0)
    (hunz : 0 < unaligned_len.val)
    (hule : unaligned_len.val ≤ slice.val.length)
    (hsum : offset.val + slice.val.length ≤ Usize.max)
    (hbase : base_block.val = offset.val / 8192)
    (hnb : num_blocks.val = (unaligned_len.val + 8191) / 8192)
    (hend : base_block.val + num_blocks.val ≤ mv.hashes.val.length) :
    ∀ (rem : Nat) (i : Std.Usize),
      i.val ≤ num_blocks.val →
      num_blocks.val - i.val = rem →
      ∃ res,
        MerkleVerifier.verify_aligned_blocks_loop srcInst mv offset src len
          unaligned_len base_block num_blocks i = ok res ∧
        (res = .Ok () ↔
          ∀ k, i.val ≤ k → k < num_blocks.val →
            ∀ (hk : base_block.val + k < mv.hashes.val.length),
              mv.hashes.val[base_block.val + k]'hk =
                sha256 (specIdentity (u32OfNat (min 8192 (unaligned_len.val - k * 8192))) 0#u64
                          (u64OfNat ((base_block.val + k) * 8192)) ++
                        (slice.val.drop (k * 8192)).take (min 8192 (slice.val.length - k * 8192)) ++
                        specZeroPad (BLOCK_BYTES - min 8192 (slice.val.length - k * 8192))))
  | 0, i, _, _ => by
    have hlt : ¬ (i < num_blocks) := by scalar_tac
    have hbody :
        MerkleVerifier.verify_aligned_blocks_loop.body srcInst mv offset src len
          unaligned_len base_block num_blocks i = ok (.done (.Ok ())) := by
      unfold MerkleVerifier.verify_aligned_blocks_loop.body
      simp [hlt]
    refine ⟨.Ok (), ?_, ?_⟩
    · unfold MerkleVerifier.verify_aligned_blocks_loop
      exact loop_done _ i _ hbody
    · simp only [true_iff]
      intro k hk1 hk2
      omega
  | rem + 1, i, _, _ => by
    have hi_lt_nat : i.val < num_blocks.val := by omega
    have hi_lt : i < num_blocks := by scalar_tac
    have hmul_lt_u : i.val * 8192 < unaligned_len.val :=
      mul_lt_of_lt_div_ceil (k := 8192) (by decide) (by omega)
    have hmul_lt_len : i.val * 8192 < len.val := by omega
    obtain ⟨byte_pos, hmul_bp, hbp_val⟩ := uscalar_mul_ok i BLOCK_SIZE (by
      rw [uscalar_max_usize, BLOCK_SIZE_val]; have := slice.property; omega)
    rw [BLOCK_SIZE_val] at hbp_val
    obtain ⟨rem_len, hsub_len, hrem_len⟩ := uscalar_sub_ok len byte_pos (by omega)
    obtain ⟨bbuf_len, hmin_buf, hbbuf_val⟩ := min_usize_spec BLOCK_SIZE rem_len
    rw [BLOCK_SIZE_val, hrem_len, hbp_val, hlen_eq] at hbbuf_val
    obtain ⟨rem_ulen, hsub_ulen, hrem_ulen⟩ := uscalar_sub_ok unaligned_len byte_pos (by omega)
    obtain ⟨bulen, hmin_u, hbulen_val⟩ := min_usize_spec bbuf_len rem_ulen
    have hbulen_eq : bulen.val = min 8192 (unaligned_len.val - i.val * 8192) := by
      rw [hbulen_val, hbbuf_val, hrem_ulen, hbp_val]
      omega
    have hu64_sum : offset.val + byte_pos.val ≤ U64.max := by
      have ⟨_, hu64⟩ := usize_bounds
      omega
    obtain ⟨boff64, hadd64, hboff64_val⟩ :=
      uscalar_add_ok (UScalar.cast .U64 offset) (UScalar.cast .U64 byte_pos) (by
        rw [uscalar_max_u64, cast_u64_usize_eq, cast_u64_usize_eq,
            u64OfNat_val_of_le _ (usize_val_le_u64_max _),
            u64OfNat_val_of_le _ (usize_val_le_u64_max _)]
        exact hu64_sum)
    have hboff64_eq : boff64 = u64OfNat ((base_block.val + i.val) * 8192) := by
      apply UScalar.eq_of_val_eq
      rw [hboff64_val, cast_u64_usize_eq, cast_u64_usize_eq,
          u64OfNat_val_of_le _ (usize_val_le_u64_max _),
          u64OfNat_val_of_le _ (usize_val_le_u64_max _),
          u64OfNat_val_of_le _ (by rw [hbp_val] at hu64_sum; omega),
          hbp_val, hbase]
      omega
    have hboff64_mod : boff64.val % BLOCK_BYTES = 0 := by
      rw [hboff64_eq, u64OfNat_val_of_le _ (by rw [hbp_val] at hu64_sum; omega)]
      unfold BLOCK_BYTES; omega
    have hbuf_cases : bbuf_len.val = BLOCK_BYTES ∨ bbuf_len.val = MIN_PAGE_BYTES := by
      rw [hbbuf_val]
      exact aligned_buffer_len_cases hpage (by omega) (by omega)
    have hhba_src := hsrc.hash_aligned_block_eq boff64 byte_pos bbuf_len bulen
      (by unfold BLOCK_BYTES; omega) hbuf_cases (by omega) (by omega) (by omega) hboff64_mod
    rw [hbulen_eq, hboff64_eq, hbp_val, hbbuf_val] at hhba_src
    have hidx_lt : base_block.val + i.val < mv.hashes.val.length := by omega
    have hmv_usize : mv.hashes.val.length ≤ Usize.max := mv.hashes.property
    obtain ⟨idx, hadd_idx, hidx_val⟩ := uscalar_add_ok base_block i (by
      rw [uscalar_max_usize]; omega)
    have hv_idx := slice_index_ok mv.hashes idx (by omega)
    set computed_h :=
      sha256 (specIdentity (u32OfNat (min 8192 (unaligned_len.val - i.val * 8192))) 0#u64
                (u64OfNat ((base_block.val + i.val) * 8192)) ++
              (slice.val.drop (i.val * 8192)).take (min 8192 (slice.val.length - i.val * 8192)) ++
              specZeroPad (BLOCK_BYTES - min 8192 (slice.val.length - i.val * 8192)))
    set stored_i := mv.hashes.val[base_block.val + i.val]'hidx_lt
    have hv_idx' : Slice.index_usize mv.hashes idx = ok stored_i := by
      simpa [stored_i, hidx_val] using hv_idx
    by_cases hmatch : stored_i = computed_h
    · have hne_false :
          core.cmp.PartialEq.ne.trait_default Hash.Insts.CoreCmpPartialEqHash stored_i computed_h =
            ok false := by
        simp [hash_ne_spec, hmatch]
      obtain ⟨i1, hadd_i1, hi1_val⟩ := uscalar_add_ok i 1#usize (by
        rw [uscalar_max_usize]; simp only [UScalar.ofNatCore_val_eq]; omega)
      simp only [UScalar.ofNatCore_val_eq] at hi1_val
      have hbody :
          MerkleVerifier.verify_aligned_blocks_loop.body srcInst mv offset src len
            unaligned_len base_block num_blocks i = ok (.cont i1) := by
        unfold MerkleVerifier.verify_aligned_blocks_loop.body
        simp only [hi_lt, ↓reduceIte, hmul_bp, hsub_len, hmin_buf, hsub_ulen, hmin_u,
                   lift, hadd64, hboff64_eq, hhba_src, hadd_idx, hv_idx', hne_false,
                   Bool.false_eq_true, hadd_i1, bind_ok]
      obtain ⟨res, ih_exec, ih_iff⟩ :=
        verify_aligned_blocks_loop_spec hsrc mv offset len unaligned_len base_block num_blocks
          hlen_eq hoff hpage hunz hule hsum hbase hnb hend rem i1 (by omega) (by omega)
      refine ⟨res, ?_, ?_⟩
      · unfold MerkleVerifier.verify_aligned_blocks_loop at ih_exec ⊢
        rw [loop_cont _ i _ hbody]
        exact ih_exec
      · rw [ih_iff]
        constructor
        · intro hall k hk_ge hk_lt hk_in
          by_cases hki : k = i.val
          · subst hki; exact hmatch
          · exact hall k (by omega) hk_lt hk_in
        · intro hall k hk_ge hk_lt hk_in
          exact hall k (by omega) hk_lt hk_in
    · have hne_true :
          core.cmp.PartialEq.ne.trait_default Hash.Insts.CoreCmpPartialEqHash stored_i computed_h =
            ok true := by
        simp [hash_ne_spec, hmatch]
      have hbody :
          MerkleVerifier.verify_aligned_blocks_loop.body srcInst mv offset src len
            unaligned_len base_block num_blocks i = ok (.done (.Err .IoDataIntegrity)) := by
        unfold MerkleVerifier.verify_aligned_blocks_loop.body
        simp only [hi_lt, ↓reduceIte, hmul_bp, hsub_len, hmin_buf, hsub_ulen, hmin_u,
                   lift, hadd64, hboff64_eq, hhba_src, hadd_idx, hv_idx', hne_true,
                   ↓reduceIte, bind_ok]
      refine ⟨.Err .IoDataIntegrity, ?_, ?_⟩
      · unfold MerkleVerifier.verify_aligned_blocks_loop
        exact loop_done _ i _ hbody
      · simp only [reduceCtorEq, false_iff]
        intro hall
        exact hmatch (hall i.val (le_refl _) hi_lt_nat hidx_lt)

/-- **Property P1 (`MerkleVerifier.verify_aligned`)**:
    Total panic & arithmetic overflow freedom across all inputs and page sizes. -/
theorem merkle_verifier_verify_aligned_no_panic_lem
    {Src : Type} {sha256 : List Std.U8 → Hash}
    {srcInst : AlignedByteSource Src} {src : Src} {slice : Slice Std.U8}
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice)
    (mv : MerkleVerifier) (offset unaligned_len : Std.Usize) :
    ∃ res, MerkleVerifier.verify_aligned srcInst mv offset src unaligned_len = ok res := by
  unfold MerkleVerifier.verify_aligned
  simp only [hsrc.buffer_len_eq, lift, bind_ok]
  rcases usize_checked_add_spec_opt offset (Slice.len slice) with
    ⟨hnone, _⟩ | ⟨_, hsome, hsum, _⟩
  · simp only [hnone, option_is_none_none, ↓reduceIte]; exact ⟨_, rfl⟩
  · simp only [hsome, option_is_none_some, Bool.false_eq_true, ↓reduceIte]
    simp only [Slice.len_val, Slice.length] at hsum
    simp only [usize_is_multiple_of_eq, bind_ok]
    by_cases hoff : offset.val % BLOCK_SIZE.val = 0
    · simp only [hoff, decide_true, ↓reduceIte]
      rw [BLOCK_SIZE_val] at hoff
      by_cases hpage : (Slice.len slice).val % PAGE_SIZE_MIN.val = 0
      · simp only [hpage, decide_true, ↓reduceIte]
        simp only [Slice.len_val, Slice.length, PAGE_SIZE_MIN_val] at hpage
        by_cases hu0 : unaligned_len = 0#usize
        · simp only [hu0, ↓reduceIte]; exact ⟨_, rfl⟩
        · simp only [hu0, ↓reduceIte]
          have hunz : 0 < unaligned_len.val := by
            have : unaligned_len.val ≠ 0 :=
              fun h => hu0 (UScalar.eq_of_val_eq (by simpa using h))
            omega
          by_cases hugt : unaligned_len > Slice.len slice
          · simp only [hugt, ↓reduceIte]; exact ⟨_, rfl⟩
          · simp only [hugt, ↓reduceIte]
            have hule : unaligned_len.val ≤ slice.val.length := by scalar_tac
            obtain ⟨num_blocks, hceil_nb, hnb_val⟩ :=
              div_ceil_usize_spec unaligned_len BLOCK_SIZE (by simp)
            rw [BLOCK_SIZE_val] at hnb_val
            obtain ⟨len_blocks, hceil_lb, _⟩ :=
              div_ceil_usize_spec (Slice.len slice) BLOCK_SIZE (by simp)
            simp only [hceil_nb, hceil_lb, bind_ok]
            by_cases hgt_lb : len_blocks > num_blocks
            · simp only [hgt_lb, ↓reduceIte]; exact ⟨_, rfl⟩
            · simp only [hgt_lb, ↓reduceIte]
              obtain ⟨uend, hadd_uend, huend_val⟩ := uscalar_add_ok offset unaligned_len (by
                rw [uscalar_max_usize]; omega)
              obtain ⟨hend_idx, hceil_end, hceil_end_val⟩ :=
                div_ceil_usize_spec uend BLOCK_SIZE (by simp)
              rw [BLOCK_SIZE_val, huend_val] at hceil_end_val
              simp only [hadd_uend, hceil_end, bind_ok]
              by_cases hgt_h : hend_idx > Slice.len mv.hashes
              · simp only [hgt_h, ↓reduceIte]; exact ⟨_, rfl⟩
              · simp only [hgt_h, ↓reduceIte]
                have hle_hashes :
                    (offset.val + unaligned_len.val + 8191) / 8192 ≤ mv.hashes.val.length := by
                  have : hend_idx.val ≤ (Slice.len mv.hashes).val := by scalar_tac
                  simpa [hceil_end_val] using this
                obtain ⟨base_block, hdiv_bb, hbase_val⟩ :=
                  uscalar_div_ok offset BLOCK_SIZE (by simp)
                rw [BLOCK_SIZE_val] at hbase_val
                have hend_nb : base_block.val + num_blocks.val ≤ mv.hashes.val.length := by
                  have hdecomp :=
                    div_ceil_add_aligned (k := 8192) (len := unaligned_len.val) (by decide) hoff
                  omega
                obtain ⟨blk_res, hvab, _⟩ :=
                  verify_aligned_blocks_loop_spec hsrc mv offset (Slice.len slice)
                    unaligned_len base_block num_blocks (by simp) hoff hpage hunz hule hsum
                    hbase_val hnb_val hend_nb num_blocks.val 0#usize (Nat.zero_le _) (by simp)
                by_cases hmult_end : uend.val % BLOCK_SIZE.val = 0
                · simp only [hmult_end, decide_true, ↓reduceIte, hdiv_bb,
                             MerkleVerifier.verify_aligned_blocks, hvab, bind_ok]
                  exact ⟨_, rfl⟩
                · simp only [hmult_end, decide_false, Bool.false_eq_true, ↓reduceIte]
                  by_cases hne_i1 : hend_idx != Slice.len mv.hashes
                  · simp only [hne_i1, ↓reduceIte]; exact ⟨_, rfl⟩
                  · simp only [hne_i1, Bool.false_eq_true, ↓reduceIte, hdiv_bb,
                               MerkleVerifier.verify_aligned_blocks, hvab, bind_ok]
                    exact ⟨_, rfl⟩
      · simp only [hpage, decide_false, Bool.false_eq_true, ↓reduceIte]; exact ⟨_, rfl⟩
    · simp only [hoff, decide_false, Bool.false_eq_true, ↓reduceIte]; exact ⟨_, rfl⟩

/-- **Property P3 (`MerkleVerifier.verify_aligned` Completeness)**:
    Any valid page-aligned zero-padded `AlignedByteSource` whose prefix
    `slice[..unaligned_len]` equals `B[offset .. offset + unaligned_len]`, whose
    block count does not exceed `ceil(unaligned_len / 8192)`, and whose trailing
    bytes `slice[unaligned_len..]` are all `0#u8` is accepted with `ok (.Ok ())`. -/
theorem merkle_verifier_verify_aligned_complete_of_inv
    {Src : Type} {sha256 : List Std.U8 → Hash}
    {srcInst : AlignedByteSource Src}
    {B : List Std.U8} {mv : MerkleVerifier}
    {offset : Std.Usize} {src : Src} {slice : Slice Std.U8} {unaligned_len : Std.Usize}
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice)
    (_hmv_inv : MerkleVerifierInv mv)
    (hmv_hashes : mv.hashes.val = specLeafHashes sha256 B)
    (hoff : offset.val % BLOCK_BYTES = 0)
    (hpage : slice.val.length % MIN_PAGE_BYTES = 0)
    (hunz : 0 < unaligned_len.val)
    (hule : unaligned_len.val ≤ slice.val.length)
    (hblocks :
      (slice.val.length + (BLOCK_BYTES - 1)) / BLOCK_BYTES ≤
        (unaligned_len.val + (BLOCK_BYTES - 1)) / BLOCK_BYTES)
    (hsum_usize : offset.val + slice.val.length ≤ Usize.max)
    (hin_blob : offset.val + unaligned_len.val ≤ B.length)
    (halign_end :
      (offset.val + unaligned_len.val) % BLOCK_BYTES = 0 ∨
      offset.val + unaligned_len.val = B.length)
    (hprefix : slice.val.take unaligned_len.val = (B.drop offset.val).take unaligned_len.val)
    (htail_zero :
      slice.val.drop unaligned_len.val = specZeroPad (slice.val.length - unaligned_len.val)) :
    MerkleVerifier.verify_aligned srcInst mv offset src unaligned_len = ok (.Ok ()) := by
  have hoff8 : offset.val % 8192 = 0 := hoff
  have hpage4 : slice.val.length % 4096 = 0 := hpage
  have hblocks8 : (slice.val.length + 8191) / 8192 ≤ (unaligned_len.val + 8191) / 8192 := hblocks
  have hBne : B ≠ [] := List.ne_nil_of_length_pos (by omega)
  have hBemp : B.isEmpty = false := List.isEmpty_eq_false_iff.mpr hBne
  have hmv_len : mv.hashes.val.length = (B.length + 8191) / 8192 := by
    rw [hmv_hashes, specLeafHashes_length, hBemp]; simp
  unfold MerkleVerifier.verify_aligned
  simp only [hsrc.buffer_len_eq, lift, bind_ok]
  rcases usize_checked_add_spec_opt offset (Slice.len slice) with
    ⟨_, hovf⟩ | ⟨_, hsome, hsum, _⟩
  · simp only [Slice.len_val, Slice.length] at hovf; omega
  · simp only [hsome, option_is_none_some, Bool.false_eq_true, ↓reduceIte]
    simp only [Slice.len_val, Slice.length] at hsum
    simp only [usize_is_multiple_of_eq, BLOCK_SIZE_val, hoff8, decide_true, ↓reduceIte,
               Slice.len_val, Slice.length, PAGE_SIZE_MIN_val, hpage4, bind_ok]
    have hu0 : ¬ (unaligned_len = 0#usize) := fun h => by subst h; simp at hunz
    have hugt : ¬ (unaligned_len > Slice.len slice) := by scalar_tac
    simp only [hu0, hugt, ↓reduceIte]
    obtain ⟨num_blocks, hceil_nb, hnb_val⟩ :=
      div_ceil_usize_spec unaligned_len BLOCK_SIZE (by simp)
    rw [BLOCK_SIZE_val] at hnb_val
    obtain ⟨len_blocks, hceil_lb, hlb_val⟩ :=
      div_ceil_usize_spec (Slice.len slice) BLOCK_SIZE (by simp)
    simp only [Slice.len_val, Slice.length, BLOCK_SIZE_val] at hlb_val
    have hgt_lb : ¬ (len_blocks > num_blocks) :=
      not_usize_gt_of_le (by rw [hlb_val, hnb_val]; exact hblocks8)
    simp only [hceil_nb, hceil_lb, hgt_lb, ↓reduceIte, bind_ok]
    obtain ⟨uend, hadd_uend, huend_val⟩ := uscalar_add_ok offset unaligned_len (by
      rw [uscalar_max_usize]; omega)
    obtain ⟨hend_idx, hceil_end, hceil_end_val⟩ := div_ceil_usize_spec uend BLOCK_SIZE (by simp)
    rw [BLOCK_SIZE_val, huend_val] at hceil_end_val
    simp only [hadd_uend, hceil_end, bind_ok]
    have hend_le_mv : hend_idx.val ≤ mv.hashes.val.length := by
      rw [hceil_end_val, hmv_len]
      exact Nat.div_le_div_right (by omega)
    have hgt_h : ¬ (hend_idx > Slice.len mv.hashes) := by scalar_tac
    simp only [hgt_h, ↓reduceIte]
    obtain ⟨base_block, hdiv_bb, hbase_val⟩ := uscalar_div_ok offset BLOCK_SIZE (by simp)
    rw [BLOCK_SIZE_val] at hbase_val
    have hend_nb : base_block.val + num_blocks.val ≤ mv.hashes.val.length := by
      have hdecomp := div_ceil_add_aligned (k := 8192) (len := unaligned_len.val) (by decide) hoff8
      omega
    obtain ⟨blk_res, hvab, hiff⟩ :=
      verify_aligned_blocks_loop_spec hsrc mv offset (Slice.len slice)
        unaligned_len base_block num_blocks (by simp) hoff8 hpage4 hunz hule hsum
        hbase_val hnb_val hend_nb num_blocks.val 0#usize (Nat.zero_le _) (by simp)
    have hblk_ok : blk_res = .Ok () := by
      apply hiff.mpr
      intro k _ hk_lt hk_in
      have hk_sl : base_block.val + k < (specLeafHashes sha256 B).length := by
        simpa [hmv_hashes] using hk_in
      obtain ⟨hB_leaf, hB_pos, _⟩ :=
        specLeafHashes_getElem_of_nonempty sha256 B hBne (base_block.val + k) hk_sl
      have hmv_eq_sl :
          mv.hashes.val[base_block.val + k]'hk_in =
          (specLeafHashes sha256 B)[base_block.val + k]'hk_sl := by
        simp [hmv_hashes]
      rw [hmv_eq_sl, hB_leaf]
      set canon := (B.drop ((base_block.val + k) * 8192)).take 8192
      set buflen := min 8192 (slice.val.length - k * 8192)
      set ulen := min 8192 (unaligned_len.val - k * 8192)
      have hmul_off : (base_block.val + k) * 8192 = offset.val + k * 8192 := by
        rw [Nat.add_mul, hbase_val, Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hoff8)]
      have hcanon_eq_data : canon = (slice.val.drop (k * 8192)).take ulen := by
        have h1 : ((slice.val.take unaligned_len.val).drop (k * 8192)).take 8192 =
                  (((B.drop offset.val).take unaligned_len.val).drop (k * 8192)).take 8192 :=
          congrArg (fun l => (l.drop (k * 8192)).take 8192) hprefix
        rw [List.drop_take, List.take_take] at h1
        rw [List.drop_take, List.drop_drop, ← hmul_off] at h1
        rcases halign_end with hmod0 | heq_B
        · unfold BLOCK_BYTES at hmod0
          have hk_mul :=
            mul_lt_of_lt_div_ceil (i := k) (n := unaligned_len.val) (k := 8192)
              (by decide) (by rw [← hnb_val]; exact hk_lt)
          have hrem_ge : 8192 ≤ unaligned_len.val - k * 8192 := by omega
          have hulen_eq8 : ulen = 8192 := Nat.min_eq_left hrem_ge
          rw [List.take_take, Nat.min_eq_left hrem_ge] at h1
          rw [hulen_eq8]
          exact h1.symm
        · have hrem_eq :
              unaligned_len.val - k * 8192 = (B.drop ((base_block.val + k) * 8192)).length := by
            simp [List.length_drop, hmul_off]; omega
          have hrhs :
              ((B.drop ((base_block.val + k) * 8192)).take (unaligned_len.val - k * 8192)).take 8192 =
              canon := by
            rw [hrem_eq, List.take_length]
          rw [hrhs] at h1
          exact h1.symm
      have hulen_len : canon.length = ulen := by
        rw [hcanon_eq_data, List.length_take, List.length_drop]
        omega
      have hc_len_ne0 : canon.length ≠ 0 := by omega
      simp only [specHashBlock, hc_len_ne0, false_and, ↓reduceIte]
      simp only [hulen_len]
      congr 1
      simp only [List.append_assoc]
      congr 1
      have hule_buf : ulen ≤ buflen := by dsimp [ulen, buflen]; omega
      have hbuf_le : buflen ≤ 8192 := Nat.min_le_left _ _
      have hsplit_take :
          (slice.val.drop (k * 8192)).take buflen =
          (slice.val.drop (k * 8192)).take ulen ++
            ((slice.val.drop (k * 8192)).drop ulen).take (buflen - ulen) := by
        have hadd : buflen = ulen + (buflen - ulen) := by omega
        conv => lhs; rw [hadd]
        exact List.take_add
      have hmid_zero :
          ((slice.val.drop (k * 8192)).drop ulen).take (buflen - ulen) =
          specZeroPad (buflen - ulen) := by
        by_cases hsame : ulen = buflen
        · simp [hsame, specZeroPad]
        · have hulen_tail : ulen = unaligned_len.val - k * 8192 := by
            dsimp [ulen]
            have : unaligned_len.val - k * 8192 < 8192 := by
              by_contra hge
              have : ulen = 8192 := by dsimp [ulen]; omega
              omega
            exact Nat.min_eq_right (le_of_lt this)
          have hk_mul_lt :=
            mul_lt_of_lt_div_ceil (i := k) (n := unaligned_len.val) (k := 8192)
              (by decide) (by rw [← hnb_val]; exact hk_lt)
          have hsum_u : k * 8192 + ulen = unaligned_len.val := by
            rw [hulen_tail]
            exact Nat.add_sub_of_le (le_of_lt hk_mul_lt)
          rw [List.drop_drop, hsum_u, htail_zero]
          unfold specZeroPad
          have hdiff_le : buflen - ulen ≤ slice.val.length - unaligned_len.val := by
            rw [hulen_tail]
            exact min_sub_le_sub (le_of_lt hk_mul_lt)
          rw [List.take_replicate, Nat.min_eq_left hdiff_le]
      rw [hcanon_eq_data, hsplit_take, hmid_zero, List.append_assoc, ← specZeroPad_add]
      congr 2
      unfold BLOCK_BYTES
      exact (sub_add_sub_cancel_of_le hule_buf hbuf_le).symm
    by_cases hmult_end : uend.val % 8192 = 0
    · simp only [hmult_end, decide_true, ↓reduceIte, hdiv_bb,
                 MerkleVerifier.verify_aligned_blocks, hvab, hblk_ok, bind_ok]
    · simp only [hmult_end, decide_false, Bool.false_eq_true, ↓reduceIte]
      have heq_B : offset.val + unaligned_len.val = B.length := by
        rcases halign_end with h0 | hB
        · exfalso; apply hmult_end
          rw [huend_val]; exact h0
        · exact hB
      have hend_eq_len : hend_idx = Slice.len mv.hashes := by
        apply UScalar.eq_of_val_eq
        rw [hceil_end_val, Slice.len_val, Slice.length, hmv_len]
        omega
      simp only [hend_eq_len, bne_self_eq_false, Bool.false_eq_true, ↓reduceIte,
                 hdiv_bb, MerkleVerifier.verify_aligned_blocks, hvab, hblk_ok, bind_ok]

/-- **Property P4 (`MerkleVerifier.verify_aligned` Soundness)**:
    If `MerkleVerifier.verify_aligned` returns `ok (.Ok ())`, then either
    `HashCollision sha256`, or `0 < unaligned_len ≤ slice.len`, `slice.len` is
    page-aligned, `ceil(slice.len / 8192) ≤ ceil(unaligned_len / 8192)`,
    `slice[..unaligned_len]` equals `B[offset .. offset + unaligned_len]`, and
    every trailing byte `slice[unaligned_len..]` in the page buffer is `0#u8`. -/
theorem merkle_verifier_verify_aligned_sound_of_inv
    {Src : Type} {sha256 : List Std.U8 → Hash}
    {srcInst : AlignedByteSource Src}
    {B : List Std.U8} {mv : MerkleVerifier}
    {offset : Std.Usize} {src : Src} {slice : Slice Std.U8} {unaligned_len : Std.Usize}
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice)
    (_hmv_inv : MerkleVerifierInv mv)
    (hmv_hashes : mv.hashes.val = specLeafHashes sha256 B)
    (hok : MerkleVerifier.verify_aligned srcInst mv offset src unaligned_len = ok (.Ok ())) :
    (0 < unaligned_len.val ∧
     unaligned_len.val ≤ slice.val.length ∧
     slice.val.length % MIN_PAGE_BYTES = 0 ∧
     (slice.val.length + (BLOCK_BYTES - 1)) / BLOCK_BYTES ≤
       (unaligned_len.val + (BLOCK_BYTES - 1)) / BLOCK_BYTES ∧
     offset.val + unaligned_len.val ≤ B.length ∧
     slice.val.take unaligned_len.val = (B.drop offset.val).take unaligned_len.val ∧
     slice.val.drop unaligned_len.val = specZeroPad (slice.val.length - unaligned_len.val)) ∨
    HashCollision sha256 := by
  unfold MerkleVerifier.verify_aligned at hok
  simp only [hsrc.buffer_len_eq, lift, bind_ok] at hok
  rcases usize_checked_add_spec_opt offset (Slice.len slice) with
    ⟨hnone, _⟩ | ⟨_, hsome, hsum, _⟩
  · simp [hnone] at hok
  · simp only [hsome, option_is_none_some, Bool.false_eq_true, ↓reduceIte] at hok
    simp only [Slice.len_val, Slice.length] at hsum
    simp only [usize_is_multiple_of_eq, bind_ok] at hok
    by_cases hoff : offset.val % BLOCK_SIZE.val = 0
    · simp only [hoff, decide_true, ↓reduceIte] at hok
      rw [BLOCK_SIZE_val] at hoff
      by_cases hpage : (Slice.len slice).val % PAGE_SIZE_MIN.val = 0
      · simp only [hpage, decide_true, ↓reduceIte] at hok
        simp only [Slice.len_val, Slice.length, PAGE_SIZE_MIN_val] at hpage
        by_cases hu0 : unaligned_len = 0#usize
        · simp [hu0] at hok
        · simp only [hu0, ↓reduceIte] at hok
          have hunz : 0 < unaligned_len.val := by
            have : unaligned_len.val ≠ 0 :=
              fun h => hu0 (UScalar.eq_of_val_eq (by simpa using h))
            omega
          by_cases hugt : unaligned_len > Slice.len slice
          · simp [hugt] at hok
          · simp only [hugt, ↓reduceIte] at hok
            have hule : unaligned_len.val ≤ slice.val.length := by scalar_tac
            obtain ⟨num_blocks, hceil_nb, hnb_val⟩ :=
              div_ceil_usize_spec unaligned_len BLOCK_SIZE (by simp)
            rw [BLOCK_SIZE_val] at hnb_val
            obtain ⟨len_blocks, hceil_lb, hlb_val⟩ :=
              div_ceil_usize_spec (Slice.len slice) BLOCK_SIZE (by simp)
            simp only [Slice.len_val, Slice.length, BLOCK_SIZE_val] at hlb_val
            simp only [hceil_nb, hceil_lb, bind_ok] at hok
            by_cases hgt_lb : len_blocks > num_blocks
            · simp [hgt_lb] at hok
            · simp only [hgt_lb, ↓reduceIte] at hok
              have hblocks :
                  (slice.val.length + 8191) / 8192 ≤ (unaligned_len.val + 8191) / 8192 := by
                have : len_blocks.val ≤ num_blocks.val := by scalar_tac
                simpa [hlb_val, hnb_val] using this
              obtain ⟨uend, hadd_uend, huend_val⟩ := uscalar_add_ok offset unaligned_len (by
                rw [uscalar_max_usize]; omega)
              obtain ⟨hend_idx, hceil_end, hceil_end_val⟩ :=
                div_ceil_usize_spec uend BLOCK_SIZE (by simp)
              rw [BLOCK_SIZE_val, huend_val] at hceil_end_val
              simp only [hadd_uend, hceil_end, bind_ok] at hok
              by_cases hgt_h : hend_idx > Slice.len mv.hashes
              · simp [hgt_h] at hok
              · simp only [hgt_h, ↓reduceIte] at hok
                have hle_hashes :
                    (offset.val + unaligned_len.val + 8191) / 8192 ≤ mv.hashes.val.length := by
                  have : hend_idx.val ≤ (Slice.len mv.hashes).val := by scalar_tac
                  simpa [hceil_end_val] using this
                obtain ⟨base_block, hdiv_bb, hbase_val⟩ :=
                  uscalar_div_ok offset BLOCK_SIZE (by simp)
                rw [BLOCK_SIZE_val] at hbase_val
                have hend_nb : base_block.val + num_blocks.val ≤ mv.hashes.val.length := by
                  have hdecomp :=
                    div_ceil_add_aligned (k := 8192) (len := unaligned_len.val) (by decide) hoff
                  omega
                obtain ⟨blk_res, hvab, hiff⟩ :=
                  verify_aligned_blocks_loop_spec hsrc mv offset (Slice.len slice)
                    unaligned_len base_block num_blocks (by simp) hoff hpage hunz hule hsum
                    hbase_val hnb_val hend_nb num_blocks.val 0#usize (Nat.zero_le _) (by simp)
                have hblk_ok : blk_res = .Ok () := by
                  by_cases hmult_end : uend.val % BLOCK_SIZE.val = 0
                  · simp only [hmult_end, decide_true, ↓reduceIte, hdiv_bb,
                               MerkleVerifier.verify_aligned_blocks, hvab, bind_ok] at hok
                    exact Result.ok_injective hok
                  · simp only [hmult_end, decide_false, Bool.false_eq_true, ↓reduceIte] at hok
                    by_cases hne_i1 : hend_idx != Slice.len mv.hashes
                    · simp [hne_i1] at hok
                    · simp only [hne_i1, Bool.false_eq_true, ↓reduceIte, hdiv_bb,
                                 MerkleVerifier.verify_aligned_blocks, hvab, bind_ok] at hok
                      exact Result.ok_injective hok
                have hall := hiff.mp hblk_ok
                have hsum64 : offset.val + slice.val.length ≤ U64.max := by
                  have ⟨_, hu64⟩ := usize_bounds
                  omega
                have hzero_after_hb := drop_min_len_eq_zeroPad_of_div_ceil_le hblocks
                have hall_leaves :
                    ∀ k < (unaligned_len.val + 8191) / 8192,
                      ∃ hk : offset.val / 8192 + k < (specLeafHashes sha256 B).length,
                        (specLeafHashes sha256 B)[offset.val / 8192 + k]'hk =
                          sha256 (specIdentity (u32OfNat (min 8192 (unaligned_len.val - k * 8192))) 0#u64
                                    (u64OfNat ((offset.val / 8192 + k) * 8192)) ++
                                  (slice.val.drop (k * 8192)).take
                                    (min 8192 (slice.val.length - k * 8192)) ++
                                  specZeroPad (BLOCK_BYTES -
                                    min 8192 (slice.val.length - k * 8192))) := by
                  intro k hk_lt
                  have hk_nb : k < num_blocks.val := by rw [hnb_val]; exact hk_lt
                  have hk_mv : base_block.val + k < mv.hashes.val.length := by omega
                  have hk_sl : offset.val / 8192 + k < (specLeafHashes sha256 B).length := by
                    rw [← hbase_val, ← hmv_hashes]; exact hk_mv
                  refine ⟨hk_sl, ?_⟩
                  have hstep := hall k (Nat.zero_le _) hk_nb hk_mv
                  have hmv_eq : mv.hashes.val[base_block.val + k]'hk_mv =
                      (specLeafHashes sha256 B)[offset.val / 8192 + k]'hk_sl := by
                    simp [hmv_hashes, hbase_val]
                  rw [← hmv_eq, hstep, hbase_val]
                rcases aligned_blocks_match_leaves_sound sha256 B slice.val
                    offset.val unaligned_len.val hoff hpage hunz hule hsum64
                    hzero_after_hb hall_leaves with
                  ⟨_, _, hin_B, hprefix_exact, hfull_zero⟩ | hcol
                · exact Or.inl ⟨hunz, hule, hpage, hblocks, hin_B, hprefix_exact, hfull_zero⟩
                · exact Or.inr hcol
      · simp only [hpage, decide_false, Bool.false_eq_true, ↓reduceIte] at hok
        cases Result.ok_injective hok
    · simp only [hoff, decide_false, Bool.false_eq_true, ↓reduceIte] at hok
      cases Result.ok_injective hok

end FuchsiaMerkle
