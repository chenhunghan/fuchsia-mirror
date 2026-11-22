-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.VerifierCommon

set_option maxRecDepth 8192
set_option maxHeartbeats 800000

/-!
# `ReadSizedMerkleVerifier` Construction & Slice Verification Proofs

Machine-checked totality (panic/overflow freedom), completeness, and collision-resistant
soundness proofs for `ReadSizedMerkleVerifier.new` and `ReadSizedMerkleVerifier.verify`
over any external streaming SHA-256 implementation satisfying `ValidSha256State`.
-/

namespace FuchsiaMerkle

open Aeneas Aeneas.Std Result
open fuchsia_merkle_core hiding Hash
open FuchsiaMerkle.Assumptions
open FuchsiaMerkle.Spec

-- ============================================================================
-- 1. `ReadSizedMerkleVerifier.new` -- Totality, Completeness, Invariant, & Soundness
-- ============================================================================

theorem read_sized_verifier_new_loop_spec
    {H : Type} {sha256 : List U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List U8 → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (s : Slice Hash)
    (hashes_per_hash : Usize) (bytes_per_hash : U32)
    (hK : 0 < hashes_per_hash.val)
    (hbph : bytes_per_hash.val = hashes_per_hash.val * 32)
    (hv_bound : s.val.length * 8192 ≤ U64.max) :
    ∀ (rem : Nat) (acc : alloc.vec.Vec Hash) (pos idx : Usize),
      pos.val = min s.val.length (idx.val * hashes_per_hash.val) →
      acc.val.length = idx.val →
      idx.val ≤ (s.val.length + hashes_per_hash.val - 1) / hashes_per_hash.val →
      (s.val.length + hashes_per_hash.val - 1) / hashes_per_hash.val - idx.val = rem →
      ∃ out : alloc.vec.Vec Hash,
        ReadSizedMerkleVerifier.new_loop hInst s hashes_per_hash
          bytes_per_hash acc pos idx = ok out ∧
        out.val = acc.val ++
          (specReadSizedHashes sha256 hashes_per_hash.val hK bytes_per_hash s.val).drop idx.val
  | 0, acc, pos, idx, hpos, _, hidx_le, hrem => by
    have hidx_eq : idx.val = (s.val.length + hashes_per_hash.val - 1) / hashes_per_hash.val := by
      omega
    have hmul_ge : s.val.length ≤ idx.val * hashes_per_hash.val := by
      rw [hidx_eq]
      exact le_div_ceil_mul hK
    have hpos_eq : pos.val = s.val.length := by rw [hpos]; exact Nat.min_eq_left hmul_ge
    have hnlt : ¬ (pos < Slice.len s) :=
      not_usize_lt_of_ge (by rw [Slice.len_val, Slice.length, hpos_eq])
    have hbody :
        ReadSizedMerkleVerifier.new_loop.body hInst s hashes_per_hash
          bytes_per_hash acc pos idx = ok (.done acc) := by
      unfold ReadSizedMerkleVerifier.new_loop.body
      simp only [hnlt, ↓reduceIte]
    refine ⟨acc, ?_, ?_⟩
    · unfold ReadSizedMerkleVerifier.new_loop
      exact loop_done _ (acc, pos, idx) _ hbody
    · have hdrop_nil :
          (specReadSizedHashes sha256 hashes_per_hash.val hK bytes_per_hash s.val).drop idx.val = [] := by
        apply List.drop_eq_nil_of_le
        rw [specReadSizedHashes_length]
        omega
      rw [hdrop_nil, List.append_nil]
  | rem + 1, acc, pos, idx, hpos, hacc_len, _, hrem => by
    have hidx_lt : idx.val < (s.val.length + hashes_per_hash.val - 1) / hashes_per_hash.val := by
      omega
    have hmul_lt : idx.val * hashes_per_hash.val < s.val.length :=
      mul_lt_of_lt_div_ceil (i := idx.val) (n := s.val.length) (k := hashes_per_hash.val) hK hidx_lt
    have hpos_val : pos.val = idx.val * hashes_per_hash.val := by
      rw [hpos]; exact Nat.min_eq_right (le_of_lt hmul_lt)
    have hlt_v : pos < Slice.len s :=
      usize_lt_of_lt (by rw [Slice.len_val, Slice.length, hpos_val]; exact hmul_lt)
    have hv_usize : s.val.length ≤ Usize.max := s.property
    obtain ⟨rem_v, hsub, hrem_v_val⟩ := uscalar_sub_ok (Slice.len s) pos (by
      rw [Slice.len_val, Slice.length, hpos_val]; omega)
    simp only [Slice.len_val, Slice.length] at hrem_v_val
    obtain ⟨chunk_len, hmin, hclen_val⟩ := min_usize_spec hashes_per_hash rem_v
    rw [hrem_v_val, hpos_val] at hclen_val
    obtain ⟨pos1, hadd_pos, hpos1_val⟩ := uscalar_add_ok pos chunk_len (by
      rw [uscalar_max_usize]; omega)
    obtain ⟨slice, hslice, hslice_val⟩ := slice_range_ok s pos pos1 (by omega) (by omega)
    have hslice_eq :
        slice.val = (s.val.drop (idx.val * hashes_per_hash.val)).take hashes_per_hash.val := by
      rw [hslice_val, hpos_val, show pos1.val - idx.val * hashes_per_hash.val = chunk_len.val by omega,
          hclen_val]
      by_cases hle : hashes_per_hash.val ≤ s.val.length - idx.val * hashes_per_hash.val
      · rw [Nat.min_eq_left hle]
      · rw [Nat.min_eq_right (by omega),
            List.take_of_length_le (by rw [List.length_drop]),
            List.take_of_length_le (by rw [List.length_drop]; omega)]
    have hslice_len : slice.length ≤ hashes_per_hash.val := by
      rw [Slice.length, hslice_eq]; exact List.length_take_le _ _
    have hbph_u32 : bytes_per_hash.val ≤ U32.max := u32_val_le_max bytes_per_hash
    have hpad_usize : (hashes_per_hash.val - slice.length) * 32 ≤ Usize.max := by
      have ⟨hu32_usize, _⟩ := usize_bounds
      omega
    have hoff_u64 : idx.val * bytes_per_hash.val ≤ U64.max := by
      calc idx.val * bytes_per_hash.val
          = (idx.val * hashes_per_hash.val) * 32 := by rw [hbph, Nat.mul_assoc]
        _ ≤ s.val.length * 32 := Nat.mul_le_mul_right 32 (le_of_lt hmul_lt)
        _ ≤ s.val.length * 8192 := Nat.mul_le_mul_left _ (by decide)
        _ ≤ U64.max := hv_bound
    have hmax_sha : 12 + hashes_per_hash.val * 32 ≤ SHA256_MAX_BYTES := by
      have hu32 := u32_max_val_eq
      have hsmax := sha256_max_bytes_eq
      omega
    have hhh := hash_hashes_slice_gen_spec hsha hashes_per_hash bytes_per_hash idx slice
      hslice_len hpad_usize hoff_u64 hmax_sha
    rw [hslice_eq] at hhh
    set expected_h :=
      specHashInternal sha256 hashes_per_hash.val bytes_per_hash 1#u64
        (u64OfNat (idx.val * bytes_per_hash.val))
        ((s.val.drop (idx.val * hashes_per_hash.val)).take hashes_per_hash.val)
    have hacc_lt_max : acc.val.length < Usize.max := by
      have hidx_le_mul : idx.val ≤ idx.val * hashes_per_hash.val :=
        Nat.le_mul_of_pos_right _ hK
      rw [hacc_len]; omega
    obtain ⟨acc1, hpush, hacc1_val⟩ := vec_push_ok acc expected_h hacc_lt_max
    obtain ⟨idx1, hadd_idx, hidx1_val⟩ := uscalar_add_ok idx 1#usize (by
      rw [uscalar_max_usize]; simp only [UScalar.ofNatCore_val_eq]; omega)
    simp only [UScalar.ofNatCore_val_eq] at hidx1_val
    have hbody :
        ReadSizedMerkleVerifier.new_loop.body hInst s hashes_per_hash
          bytes_per_hash acc pos idx = ok (.cont (acc1, pos1, idx1)) := by
      unfold ReadSizedMerkleVerifier.new_loop.body
      simp only [hlt_v, ↓reduceIte, hsub, hmin, hadd_pos, bind_ok]
      refine bind_ok_step hslice ?_
      simp only [hhh, hpush, hadd_idx, bind_ok]
    have hpos1_inv : pos1.val = min s.val.length (idx1.val * hashes_per_hash.val) := by
      rw [hpos1_val, hpos_val, hclen_val, hidx1_val, Nat.succ_mul]
      omega
    obtain ⟨out, ih_exec, ih_val⟩ :=
      read_sized_verifier_new_loop_spec hsha s hashes_per_hash bytes_per_hash
        hK hbph hv_bound rem acc1 pos1 idx1 hpos1_inv
        (by rw [hacc1_val, List.length_append, List.length_singleton, hacc_len, hidx1_val])
        (by omega) (by omega)
    refine ⟨out, ?_, ?_⟩
    · unfold ReadSizedMerkleVerifier.new_loop at ih_exec ⊢
      rw [loop_cont _ (acc, pos, idx) _ hbody]
      exact ih_exec
    · have hrs_lt : idx.val <
          (specReadSizedHashes sha256 hashes_per_hash.val hK bytes_per_hash s.val).length := by
        rw [specReadSizedHashes_length]; exact hidx_lt
      have hrs_get := specReadSizedHashes_getElem sha256 hashes_per_hash.val hK
        bytes_per_hash s.val idx.val hrs_lt
      rw [ih_val, hacc1_val, List.append_assoc, List.singleton_append,
          List.drop_eq_getElem_cons hrs_lt, hrs_get, hidx1_val]

/-- Totality of `ReadSizedMerkleVerifier.new` for any `MerkleVerifier` satisfying `MerkleVerifierInv`. -/
theorem read_sized_verifier_new_no_panic_of_inv
    {H : Type} {sha256 : List U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List U8 → Prop}
    {mv : MerkleVerifier}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hmv_inv : MerkleVerifierInv mv)
    (read_size : Usize) :
    ∃ res, ReadSizedMerkleVerifier.new hInst mv read_size = ok res := by
  unfold ReadSizedMerkleVerifier.new
  by_cases hrs0 : read_size = 0#usize
  · simp only [hrs0, ↓reduceIte]; exact ⟨_, rfl⟩
  · simp only [hrs0, ↓reduceIte, usize_is_multiple_of_eq, bind_ok]
    have hrs_pos : 0 < read_size.val := by
      have : read_size.val ≠ 0 := fun h => hrs0 (UScalar.eq_of_val_eq (by simpa using h))
      omega
    by_cases hmult : read_size.val % BLOCK_SIZE.val = 0
    · simp only [hmult, decide_true, ↓reduceIte]
      rw [BLOCK_SIZE_val] at hmult
      obtain ⟨hph, hdiv, hhph_val⟩ := uscalar_div_ok read_size BLOCK_SIZE (by simp)
      rw [BLOCK_SIZE_val] at hhph_val
      have hK : 0 < hph.val := by rw [hhph_val]; omega
      simp only [hdiv, lift, bind_ok]
      rcases usize_checked_mul_spec_opt hph HASH_SIZE with
        ⟨hnone, _⟩ | ⟨v, hsome, _, hv_val⟩
      · simp only [hnone]; exact ⟨_, rfl⟩
      · simp only [hsome]
        rw [HASH_SIZE_val] at hv_val
        by_cases hgt32 : v > UScalar.cast .Usize core.num.U32.MAX
        · simp only [hgt32, ↓reduceIte]; exact ⟨_, rfl⟩
        · simp only [hgt32, ↓reduceIte]
          have hv_le_u32 : v.val ≤ U32.max := by
            have hle : v.val ≤ (UScalar.cast .Usize core.num.U32.MAX).val := by scalar_tac
            rw [cast_usize_u32_max_val] at hle; exact hle
          have hv_lt : v.val < 4294967296 := by
            have := u32_max_val_eq; omega
          have hbph_val : (UScalar.cast .U32 v).val = hph.val * 32 := by
            rw [cast_u32_usize_eq v hv_lt, u32OfNat_val_of_lt v.val hv_lt, hv_val]
          obtain ⟨cap, hcap, _⟩ := div_ceil_usize_spec (Slice.len mv.hashes) hph hK
          obtain ⟨out, hloop, _⟩ :=
            read_sized_verifier_new_loop_spec hsha mv.hashes hph (UScalar.cast .U32 v)
              hK hbph_val hmv_inv.hbound
              ((mv.hashes.val.length + hph.val - 1) / hph.val)
              (alloc.vec.Vec.with_capacity Hash cap) 0#usize 0#usize
              (by simp)
              (by simp [alloc.vec.Vec.with_capacity, alloc.vec.Vec.new])
              (Nat.zero_le _)
              (by simp)
          simp only [hcap, hloop, alloc.vec.FromBoxSliceVec.from, bind_ok]
          exact ⟨_, rfl⟩
    · simp only [hmult, decide_false, Bool.false_eq_true, ↓reduceIte]; exact ⟨_, rfl⟩

/-- Full functional characterization of `ReadSizedMerkleVerifier.new` on valid `read_size`. -/
private theorem read_sized_verifier_new_complete_full
    {H : Type} {sha256 : List U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List U8 → Prop}
    {mv : MerkleVerifier} {read_size : Usize}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hmv_inv : MerkleVerifierInv mv)
    (hrs_pos : 0 < read_size.val)
    (hrs_align : read_size.val % BLOCK_BYTES = 0)
    (hbph_bound : (read_size.val / BLOCK_BYTES) * HASH_BYTES ≤ U32.max) :
    ∃ rsv : ReadSizedMerkleVerifier,
      ∃ hK : 0 < read_size.val / 8192,
        ReadSizedMerkleVerifier.new hInst mv read_size = ok (.Ok rsv) ∧
        ReadSizedWf rsv ∧
        rsv.read_size = read_size ∧
        rsv.hashes.val =
          specReadSizedHashes sha256 (read_size.val / 8192) hK rsv.bytes_per_hash mv.hashes.val := by
  have hrs_align' : read_size.val % 8192 = 0 := by simpa [BLOCK_BYTES] using hrs_align
  have hbph_bound' : (read_size.val / 8192) * 32 ≤ U32.max := by
    simpa [BLOCK_BYTES, HASH_BYTES] using hbph_bound
  have hrs0 : ¬ (read_size = 0#usize) := fun h => by subst h; simp at hrs_pos
  unfold ReadSizedMerkleVerifier.new
  simp only [hrs0, ↓reduceIte, usize_is_multiple_of_eq, BLOCK_SIZE_val, hrs_align',
             decide_true, bind_ok]
  obtain ⟨hph, hdiv, hhph_val⟩ := uscalar_div_ok read_size BLOCK_SIZE (by simp)
  rw [BLOCK_SIZE_val] at hhph_val
  have hK : 0 < hph.val := by rw [hhph_val]; omega
  have hK' : 0 < read_size.val / 8192 := by omega
  simp only [hdiv, lift, bind_ok]
  have ⟨hu32_usize, _⟩ := usize_bounds
  rcases usize_checked_mul_spec_opt hph HASH_SIZE with ⟨_, hovf⟩ | ⟨v, hsome, _, hv_val⟩
  · rw [HASH_SIZE_val, hhph_val] at hovf; omega
  · simp only [hsome]
    rw [HASH_SIZE_val] at hv_val
    have hv_le_u32 : v.val ≤ U32.max := by omega
    have hgt32 : ¬ (v > UScalar.cast .Usize core.num.U32.MAX) :=
      not_usize_gt_of_le (by rw [cast_usize_u32_max_val]; exact hv_le_u32)
    simp only [hgt32, ↓reduceIte]
    have hv_lt : v.val < 4294967296 := by have := u32_max_val_eq; omega
    have hbph_val : (UScalar.cast .U32 v).val = hph.val * 32 := by
      rw [cast_u32_usize_eq v hv_lt, u32OfNat_val_of_lt v.val hv_lt, hv_val]
    obtain ⟨cap, hcap, _⟩ := div_ceil_usize_spec (Slice.len mv.hashes) hph hK
    obtain ⟨out, hloop, hout_val⟩ :=
      read_sized_verifier_new_loop_spec hsha mv.hashes hph (UScalar.cast .U32 v)
        hK hbph_val hmv_inv.hbound
        ((mv.hashes.val.length + hph.val - 1) / hph.val)
        (alloc.vec.Vec.with_capacity Hash cap) 0#usize 0#usize
        (by simp)
        (by simp [alloc.vec.Vec.with_capacity, alloc.vec.Vec.new])
        (Nat.zero_le _)
        (by simp)
    simp only [alloc.vec.Vec.with_capacity, alloc.vec.Vec.new, alloc.vec.Vec.from_val,
               UScalar.ofNatCore_val_eq, List.nil_append, List.drop_zero] at hout_val
    simp only [hcap, hloop, alloc.vec.FromBoxSliceVec.from, bind_ok]
    have hout_slice_val : out.slice.val = out.val := rfl
    refine ⟨{ read_size, bytes_per_hash := UScalar.cast .U32 v, hashes := out.slice }, hK', rfl, ?_, rfl, ?_⟩
    · refine ⟨hrs_pos, hrs_align', ?_, ?_, ?_⟩
      · rw [hbph_val, hhph_val]
      · rw [hout_slice_val, hout_val, specReadSizedHashes_length]
        exact div_ceil_pos_of_pos hmv_inv.hne hK
      · rw [hout_slice_val, hout_val, specReadSizedHashes_length, hbph_val]
        set total := (mv.hashes.val.length + hph.val - 1) / hph.val
        have htot_mul : total * hph.val ≤ mv.hashes.val.length + hph.val - 1 :=
          Nat.div_mul_le_self _ _
        have hcalc : total * (hph.val * 32) = (total * hph.val) * 32 := (Nat.mul_assoc _ _ _).symm
        have hu64 := u64_max_val_eq
        have hu32 := u32_max_val_eq
        have hmv_b := hmv_inv.hbound
        have hm_pos := hmv_inv.hne
        omega
    · simpa [hout_slice_val, hhph_val] using hout_val

/-- Completeness of `ReadSizedMerkleVerifier.new` for valid `read_size`. -/
theorem read_sized_verifier_new_complete_of_inv
    {H : Type} {sha256 : List U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List U8 → Prop}
    {mv : MerkleVerifier} {read_size : Usize}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hmv_inv : MerkleVerifierInv mv)
    (hrs_pos : 0 < read_size.val)
    (hrs_align : read_size.val % BLOCK_BYTES = 0)
    (hbph_bound : (read_size.val / BLOCK_BYTES) * HASH_BYTES ≤ U32.max) :
    ∃ rsv, ReadSizedMerkleVerifier.new hInst mv read_size = ok (.Ok rsv) := by
  obtain ⟨rsv, _, hok, _, _, _⟩ :=
    read_sized_verifier_new_complete_full hsha hmv_inv hrs_pos hrs_align hbph_bound
  exact ⟨rsv, hok⟩

/-- Invariant and digest characterization established whenever `ReadSizedMerkleVerifier.new` succeeds. -/
theorem read_sized_verifier_new_inv
    {H : Type} {sha256 : List U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List U8 → Prop}
    {mv : MerkleVerifier} {read_size : Usize} {rsv : ReadSizedMerkleVerifier}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hmv_inv : MerkleVerifierInv mv)
    (hrsv : ReadSizedMerkleVerifier.new hInst mv read_size = ok (.Ok rsv)) :
    ReadSizedWf rsv ∧
    rsv.read_size = read_size ∧
    (read_size.val / BLOCK_BYTES) * HASH_BYTES ≤ U32.max ∧
    ∃ hK : 0 < read_size.val / 8192,
      rsv.hashes.val =
        specReadSizedHashes sha256 (read_size.val / 8192) hK rsv.bytes_per_hash mv.hashes.val := by
  have hfull := hrsv
  unfold ReadSizedMerkleVerifier.new at hrsv
  by_cases hrs0 : read_size = 0#usize
  · simp [hrs0] at hrsv
  · simp only [hrs0, ↓reduceIte, usize_is_multiple_of_eq, bind_ok] at hrsv
    have hrs_pos : 0 < read_size.val := by
      have : read_size.val ≠ 0 := fun h => hrs0 (UScalar.eq_of_val_eq (by simpa using h))
      omega
    by_cases hmult : read_size.val % BLOCK_SIZE.val = 0
    · simp only [hmult, decide_true, ↓reduceIte] at hrsv
      rw [BLOCK_SIZE_val] at hmult
      obtain ⟨hph, hdiv, hhph_val⟩ := uscalar_div_ok read_size BLOCK_SIZE (by simp)
      rw [BLOCK_SIZE_val] at hhph_val
      simp only [hdiv, lift, bind_ok] at hrsv
      rcases usize_checked_mul_spec_opt hph HASH_SIZE with ⟨hnone, _⟩ | ⟨v, hsome, _, hv_val⟩
      · simp [hnone] at hrsv
      · simp only [hsome] at hrsv
        rw [HASH_SIZE_val] at hv_val
        by_cases hgt32 : v > UScalar.cast .Usize core.num.U32.MAX
        · simp [hgt32] at hrsv
        · simp only [hgt32, ↓reduceIte] at hrsv
          have hv_le_u32 : v.val ≤ U32.max := by
            have hle : v.val ≤ (UScalar.cast .Usize core.num.U32.MAX).val := by scalar_tac
            rw [cast_usize_u32_max_val] at hle; exact hle
          have hbph_bound : (read_size.val / BLOCK_BYTES) * HASH_BYTES ≤ U32.max := by
            simp only [BLOCK_BYTES, HASH_BYTES]; omega
          have hrs_align : read_size.val % BLOCK_BYTES = 0 := by
            simpa [BLOCK_BYTES] using hmult
          obtain ⟨rsv', hK', hcomp, hwf', hrs_eq', hhashes'⟩ :=
            read_sized_verifier_new_complete_full hsha hmv_inv hrs_pos hrs_align hbph_bound
          have heq_rsv : rsv' = rsv := by
            have := Result.ok_injective (hcomp.symm.trans hfull)
            cases this; rfl
          subst heq_rsv
          exact ⟨hwf', hrs_eq', hbph_bound, hK', hhashes'⟩
    · simp only [hmult, decide_false, Bool.false_eq_true, ↓reduceIte] at hrsv
      cases Result.ok_injective hrsv

/-- Parameter validation soundness of `ReadSizedMerkleVerifier.new`. -/
theorem read_sized_verifier_new_sound_of_inv
    {H : Type} {sha256 : List U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List U8 → Prop}
    {mv : MerkleVerifier} {read_size : Usize} {rsv : ReadSizedMerkleVerifier}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hmv_inv : MerkleVerifierInv mv)
    (hrsv : ReadSizedMerkleVerifier.new hInst mv read_size = ok (.Ok rsv)) :
    0 < read_size.val ∧
    read_size.val % BLOCK_BYTES = 0 ∧
    (read_size.val / BLOCK_BYTES) * HASH_BYTES ≤ U32.max := by
  obtain ⟨hwf, hrs_eq, hbph_bound, _⟩ := read_sized_verifier_new_inv hsha hmv_inv hrsv
  refine ⟨?_, ?_, hbph_bound⟩
  · rw [← hrs_eq]; exact hwf.hread_pos
  · rw [← hrs_eq]; simpa [BLOCK_BYTES] using hwf.hread_aligned

-- ============================================================================
-- 2. Arithmetic Helpers & Polymorphic Loop Specs for `ReadSizedMerkleVerifier.verify`
-- ============================================================================

private theorem cast_u64_u32_val (x : U32) :
    (UScalar.cast .U64 x).val = x.val := by
  rw [UScalar.cast_val_eq]
  have hx : x.val < 4294967296 := by
    have h1 := u32_val_le_max x
    have h2 := u32_max_val_eq
    omega
  have p64 : 2 ^ 64 = 18446744073709551616 := by decide
  simp only [UScalarTy.numBits, p64]
  omega

private theorem cast_u64_usize_val (x : Usize) :
    (UScalar.cast .U64 x).val = x.val := by
  rw [cast_u64_usize_eq, u64OfNat_val_of_le _ (usize_val_le_u64_max _)]

private theorem div_ceil_8192_le_of_le_mul {n K : Nat} (h : n ≤ K * 8192) :
    (n + 8191) / 8192 ≤ K := by
  have hlt : n + 8191 < 8192 * (K + 1) := by
    have : 8192 * (K + 1) = K * 8192 + 8192 := by ring
    omega
  have hdiv := Nat.div_lt_of_lt_mul hlt
  omega

private theorem rsv_pad_bytes_le_usize_max {rsv : ReadSizedMerkleVerifier} (hwf : ReadSizedWf rsv)
    {d : Nat} (hd : d ≤ rsv.read_size.val / 8192) :
    d * 32 ≤ Usize.max := by
  have h1 : d * 32 ≤ (rsv.read_size.val / 8192) * 32 := Nat.mul_le_mul_right 32 hd
  have h2 := hwf.hbph
  have h3 := u32_val_le_max rsv.bytes_per_hash
  have ⟨h4, _⟩ := usize_bounds
  omega

private theorem rsv_preimage_le_sha256_max {rsv : ReadSizedMerkleVerifier} (hwf : ReadSizedWf rsv) :
    12 + (rsv.read_size.val / 8192) * 32 ≤ SHA256_MAX_BYTES := by
  have h1 := hwf.hbph
  have h2 := u32_val_le_max rsv.bytes_per_hash
  have h3 := u32_max_val_eq
  have h4 := sha256_max_bytes_eq
  omega

private theorem lt_div_ceil_of_mul_lt {i n b : Nat} (hb : 0 < b) (h : i * b < n) :
    i < (n + b - 1) / b := by
  have hsucc : (i + 1) * b ≤ n + b - 1 := by rw [Nat.succ_mul]; omega
  have hle : i + 1 ≤ (n + b - 1) / b := (Nat.le_div_iff_mul_le hb).mpr hsucc
  omega

private theorem lt_div_ceil_8192_of_mul_lt {i n : Nat} (h : i * 8192 < n) :
    i < (n + 8191) / 8192 := by
  have hsucc : (i + 1) * 8192 ≤ n + 8191 := by rw [Nat.succ_mul]; omega
  have hle : i + 1 ≤ (n + 8191) / 8192 := (Nat.le_div_iff_mul_le (by decide)).mpr hsucc
  omega

private theorem div_ceil_eq_pred_div_succ {n b : Nat} (hn : 0 < n) (hb : 0 < b) :
    (n + b - 1) / b = (n - 1) / b + 1 := by
  have hrew : n + b - 1 = (n - 1) + b := by omega
  rw [hrew, Nat.add_div_right _ hb]

private theorem div_ceil_div_ceil_eq {m K : Nat} (hK : 0 < K) :
    (((m + 8191) / 8192) + K - 1) / K = (m + K * 8192 - 1) / (K * 8192) := by
  by_cases hm : m = 0
  · subst hm
    have h1 : (K - 1) / K = 0 := Nat.div_eq_of_lt (by omega)
    have h2 : (K * 8192 - 1) / (K * 8192) = 0 := Nat.div_eq_of_lt (by omega)
    simpa using h1.trans h2.symm
  · have hm_pos : 0 < m := Nat.pos_of_ne_zero hm
    have hc1_pos : 0 < (m + 8191) / 8192 := by
      have : 1 * 8192 ≤ m + 8191 := by omega
      exact (Nat.le_div_iff_mul_le (by decide)).mpr this
    have hKB_pos : 0 < K * 8192 := Nat.mul_pos hK (by decide)
    have hm8 : (m + 8191) / 8192 = (m - 1) / 8192 + 1 := by
      have hrew : m + 8191 = (m - 1) + 8192 := by omega
      rw [hrew, Nat.add_div_right _ (by decide)]
    rw [div_ceil_eq_pred_div_succ hc1_pos hK,
        div_ceil_eq_pred_div_succ hm_pos hKB_pos,
        hm8]
    simp only [Nat.add_sub_cancel, Nat.div_div_eq_div_mul, Nat.mul_comm 8192 K]

private theorem min_div_ceil_eq {m base K : Nat} (_hK : 0 < K) (hlt : base * 8192 < m) :
    (min (K * 8192) (m - base * 8192) + 8191) / 8192 =
    min K ((m + 8191) / 8192 - base) := by
  by_cases hle : K * 8192 ≤ m - base * 8192
  · rw [Nat.min_eq_left hle]
    have h1 : (K * 8192 + 8191) / 8192 = K := by
      have hrew : K * 8192 + 8191 = 8192 * K + 8191 := by ring
      rw [hrew, Nat.mul_add_div (by decide : 0 < 8192)]
      simp
    have h2 : K ≤ (m + 8191) / 8192 - base := by
      have hmul : (base + K) * 8192 ≤ m + 8191 := by
        rw [Nat.add_mul]
        calc base * 8192 + K * 8192
            ≤ base * 8192 + (m - base * 8192) := Nat.add_le_add_left hle _
          _ = m := Nat.add_sub_of_le (le_of_lt hlt)
          _ ≤ m + 8191 := Nat.le_add_right m 8191
      have hdiv : base + K ≤ (m + 8191) / 8192 :=
        (Nat.le_div_iff_mul_le (by decide : 0 < 8192)).mpr hmul
      exact Nat.le_sub_of_add_le (by rw [Nat.add_comm]; exact hdiv)
    rw [h1, Nat.min_eq_left h2]
  · rw [Nat.min_eq_right (le_of_lt (Nat.not_le.mp hle))]
    have hdecomp : m + 8191 = 8192 * base + (m - base * 8192 + 8191) := by
      have h1 : 8192 * base = base * 8192 := Nat.mul_comm 8192 base
      rw [h1]
      omega
    have hdiv : (m + 8191) / 8192 = base + (m - base * 8192 + 8191) / 8192 := by
      rw [hdecomp, Nat.mul_add_div (by decide : 0 < 8192)]
    have hleK : (m - base * 8192 + 8191) / 8192 ≤ K :=
      div_ceil_8192_le_of_le_mul (le_of_lt (Nat.not_le.mp hle))
    rw [hdiv, Nat.add_sub_cancel_left, Nat.min_eq_right hleK]

private theorem drop_take_subblock {α : Type _} (xs : List α) (i j K : Nat) (hj : j < K) :
    (((xs.drop (i * (K * 8192))).take (K * 8192)).drop (j * 8192)).take 8192 =
    (xs.drop ((i * K + j) * 8192)).take 8192 := by
  rw [List.drop_take, List.take_take]
  have hle : 8192 ≤ K * 8192 - j * 8192 := by
    have h1 : (j + 1) * 8192 ≤ K * 8192 := Nat.mul_le_mul_right 8192 hj
    rw [Nat.succ_mul] at h1
    omega
  have hmul : i * (K * 8192) + j * 8192 = (i * K + j) * 8192 := by ring
  rw [Nat.min_eq_left hle, List.drop_drop, hmul]

/-- Pure specification of the leaf hashes hashed within one `read_size` chunk of `verify`. -/
def chunkLeafHashes (sha256 : List U8 → Hash) (baseOff : Nat) (chunk : List U8) : List Hash :=
  (List.range ((chunk.length + 8191) / 8192)).map fun j =>
    specHashBlock sha256 ((chunk.drop (j * 8192)).take 8192) (u64OfNat (baseOff + j * 8192))

theorem chunkLeafHashes_length (sha256 : List U8 → Hash) (baseOff : Nat) (chunk : List U8) :
    (chunkLeafHashes sha256 baseOff chunk).length = (chunk.length + 8191) / 8192 := by
  simp [chunkLeafHashes]

theorem chunkLeafHashes_getElem (sha256 : List U8 → Hash) (baseOff : Nat) (chunk : List U8)
    (j : Nat) (hj : j < (chunkLeafHashes sha256 baseOff chunk).length) :
    (chunkLeafHashes sha256 baseOff chunk)[j]'hj =
      specHashBlock sha256 ((chunk.drop (j * 8192)).take 8192) (u64OfNat (baseOff + j * 8192)) := by
  simp [chunkLeafHashes]

theorem mem_chunkLeafHashes_eq_zeroHash_imp_zeroPreimage
    {sha256 : List U8 → Hash} {baseOff : Nat} {chunk : List U8}
    (hmem : zeroHash ∈ chunkLeafHashes sha256 baseOff chunk) :
    HashZeroPreimage sha256 := by
  obtain ⟨j, hj, hget⟩ := List.getElem_of_mem hmem
  rw [chunkLeafHashes_getElem] at hget
  unfold specHashBlock at hget
  dsimp only at hget
  exact ⟨_, hget⟩

theorem verify_chunks_inner_loop_spec
    {H : Type} {sha256 : List U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List U8 → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (offset chunk_pos : Usize) (chunk : Slice U8)
    (hbase_align : (offset.val + chunk_pos.val) % 8192 = 0)
    (hsum64 : offset.val + chunk_pos.val + chunk.val.length ≤ U64.max) :
    ∀ (rem : Nat) (hasher : H) (acc : List U8) (sub_pos hash_count : Usize),
      sub_pos.val = min chunk.val.length (hash_count.val * 8192) →
      hash_count.val ≤ (chunk.val.length + 8191) / 8192 →
      (chunk.val.length + 8191) / 8192 - hash_count.val = rem →
      R_H hasher acc →
      acc.length + ((chunk.val.length + 8191) / 8192 - hash_count.val) * 32 ≤ SHA256_MAX_BYTES →
      ∃ (hasher_out : H) (hc_out : Usize),
        ReadSizedMerkleVerifier.verify_chunks_loop0_loop0 hInst
          offset chunk_pos chunk hasher sub_pos hash_count = ok (hasher_out, hc_out) ∧
        R_H hasher_out
          (acc ++ concatHashes ((chunkLeafHashes sha256 (offset.val + chunk_pos.val) chunk.val).drop hash_count.val)) ∧
        hc_out.val = (chunk.val.length + 8191) / 8192
  | 0, hasher, acc, sub_pos, hash_count, hsub, _, _, hrel, _ => by
    have hhc_eq : hash_count.val = (chunk.val.length + 8191) / 8192 := by omega
    have hmul_ge : chunk.val.length ≤ hash_count.val * 8192 := by
      rw [hhc_eq]
      exact le_div_ceil_mul (k := 8192) (by decide)
    have hspos_eq : sub_pos.val = chunk.val.length := by
      rw [hsub]; exact Nat.min_eq_left hmul_ge
    have hnlt : ¬ (sub_pos < Slice.len chunk) :=
      not_usize_lt_of_ge (by rw [Slice.len_val, Slice.length, hspos_eq])
    have hbody :
        ReadSizedMerkleVerifier.verify_chunks_loop0_loop0.body hInst
          offset chunk_pos chunk hasher sub_pos hash_count = ok (.done (hasher, hash_count)) := by
      unfold ReadSizedMerkleVerifier.verify_chunks_loop0_loop0.body
      simp only [hnlt, ↓reduceIte]
    have hdrop_nil :
        (chunkLeafHashes sha256 (offset.val + chunk_pos.val) chunk.val).drop hash_count.val = [] := by
      apply List.drop_eq_nil_of_le
      rw [chunkLeafHashes_length]; omega
    refine ⟨hasher, hash_count, ?_, ?_, hhc_eq⟩
    · unfold ReadSizedMerkleVerifier.verify_chunks_loop0_loop0
      exact loop_done _ (hasher, sub_pos, hash_count) _ hbody
    · simpa [hdrop_nil, concatHashes] using hrel
  | rem + 1, hasher, acc, sub_pos, hash_count, hsub, _, _, hrel, hmax => by
    have hhc_lt : hash_count.val < (chunk.val.length + 8191) / 8192 := by omega
    have hmul_lt : hash_count.val * 8192 < chunk.val.length :=
      mul_lt_of_lt_div_ceil (k := 8192) (by decide) hhc_lt
    have hspos_val : sub_pos.val = hash_count.val * 8192 := by
      rw [hsub]; exact Nat.min_eq_right (le_of_lt hmul_lt)
    have hlt_c : sub_pos < Slice.len chunk :=
      usize_lt_of_lt (by rw [Slice.len_val, Slice.length, hspos_val]; exact hmul_lt)
    have hc_usize : chunk.val.length ≤ Usize.max := chunk.property
    obtain ⟨rem_c, hsub_c, hrem_c_val⟩ := uscalar_sub_ok (Slice.len chunk) sub_pos (by
      rw [Slice.len_val, Slice.length, hspos_val]; omega)
    simp only [Slice.len_val, Slice.length] at hrem_c_val
    obtain ⟨block_len, hmin_b, hblen_val⟩ := min_usize_spec BLOCK_SIZE rem_c
    rw [BLOCK_SIZE_val, hrem_c_val, hspos_val] at hblen_val
    obtain ⟨spos1, hadd_spos, hspos1_val⟩ := uscalar_add_ok sub_pos block_len (by
      rw [uscalar_max_usize]; omega)
    obtain ⟨block, hslice_b, hblock_val⟩ := slice_range_ok chunk sub_pos spos1
      (by omega) (by omega)
    have hblock_eq :
        block.val = (chunk.val.drop (hash_count.val * 8192)).take 8192 := by
      rw [hblock_val, hspos_val, show spos1.val - hash_count.val * 8192 = block_len.val by omega,
          hblen_val]
      by_cases hle8 : 8192 ≤ chunk.val.length - hash_count.val * 8192
      · rw [Nat.min_eq_left hle8]
      · rw [Nat.min_eq_right (by omega),
            List.take_of_length_le (by rw [List.length_drop]),
            List.take_of_length_le (by rw [List.length_drop]; omega)]
    have hblock_len : block.val.length ≤ BLOCK_BYTES := by
      rw [hblock_eq, BLOCK_BYTES]; exact List.length_take_le _ _
    obtain ⟨off_cp64, hadd64_1, hoff_cp64_val⟩ :=
      uscalar_add_ok (UScalar.cast .U64 offset) (UScalar.cast .U64 chunk_pos) (by
        rw [uscalar_max_u64, cast_u64_usize_val, cast_u64_usize_val]; omega)
    obtain ⟨boff64, hadd64_2, hboff64_val⟩ :=
      uscalar_add_ok off_cp64 (UScalar.cast .U64 sub_pos) (by
        rw [uscalar_max_u64, hoff_cp64_val, cast_u64_usize_val, cast_u64_usize_val,
            cast_u64_usize_val, hspos_val]; omega)
    have hboff_nat_le : offset.val + chunk_pos.val + hash_count.val * 8192 ≤ U64.max := by omega
    have hboff64_eq :
        boff64 = u64OfNat (offset.val + chunk_pos.val + hash_count.val * 8192) := by
      apply UScalar.eq_of_val_eq
      rw [hboff64_val, hoff_cp64_val, cast_u64_usize_val, cast_u64_usize_val, cast_u64_usize_val,
          hspos_val, u64OfNat_val_of_le _ hboff_nat_le]
    have hboff64_mod : boff64.val % BLOCK_BYTES = 0 := by
      rw [BLOCK_BYTES, hboff64_eq, u64OfNat_val_of_le _ hboff_nat_le]; omega
    have hhb := hash_block_gen_spec sha256 hInst R_H block boff64 hsha hblock_len hboff64_mod
    rw [hblock_eq, hboff64_eq] at hhb
    set leaf_h :=
      specHashBlock sha256 ((chunk.val.drop (hash_count.val * 8192)).take 8192)
        (u64OfNat (offset.val + chunk_pos.val + hash_count.val * 8192))
    have hleaf_len : leaf_h.val.length = 32 := by simp
    obtain ⟨hasher1, hupd, hrel1⟩ := hsha.update_ok hasher acc leaf_h.to_slice hrel (by
      rw [Array.val_to_slice, hleaf_len]; omega)
    rw [Array.val_to_slice] at hrel1
    have hhc_le_mul : hash_count.val ≤ hash_count.val * 8192 :=
      Nat.le_mul_of_pos_right _ (by decide)
    obtain ⟨hc1, hadd_hc, hhc1_val⟩ := uscalar_add_ok hash_count 1#usize (by
      rw [uscalar_max_usize]; simp only [UScalar.ofNatCore_val_eq]; omega)
    simp only [UScalar.ofNatCore_val_eq] at hhc1_val
    have hbody :
        ReadSizedMerkleVerifier.verify_chunks_loop0_loop0.body hInst
          offset chunk_pos chunk hasher sub_pos hash_count =
          ok (.cont (hasher1, spos1, hc1)) := by
      unfold ReadSizedMerkleVerifier.verify_chunks_loop0_loop0.body
      simp only [hlt_c, ↓reduceIte, hsub_c, hmin_b, hadd_spos, bind_ok]
      refine bind_ok_step hslice_b ?_
      simp only [lift, hadd64_1, hadd64_2, hboff64_eq, hhb, hupd, hadd_hc, bind_ok]
    have hspos1_inv : spos1.val = min chunk.val.length (hc1.val * 8192) := by
      rw [hspos1_val, hspos_val, hblen_val, hhc1_val, Nat.succ_mul]
      omega
    obtain ⟨hasher_out, hc_out, ih_exec, ih_rel, ih_val⟩ :=
      verify_chunks_inner_loop_spec hsha offset chunk_pos chunk hbase_align hsum64
        rem hasher1 (acc ++ leaf_h.val) spos1 hc1 hspos1_inv (by omega) (by omega) hrel1
        (by simp only [List.length_append, hleaf_len, hhc1_val]; omega)
    refine ⟨hasher_out, hc_out, ?_, ?_, ih_val⟩
    · unfold ReadSizedMerkleVerifier.verify_chunks_loop0_loop0 at ih_exec ⊢
      rw [loop_cont _ (hasher, sub_pos, hash_count) _ hbody]
      exact ih_exec
    · have hcl_lt :
          hash_count.val < (chunkLeafHashes sha256 (offset.val + chunk_pos.val) chunk.val).length := by
        rw [chunkLeafHashes_length]; exact hhc_lt
      have hcl_get := chunkLeafHashes_getElem sha256 (offset.val + chunk_pos.val) chunk.val
        hash_count.val hcl_lt
      rw [List.drop_eq_getElem_cons hcl_lt, hcl_get, concatHashes_cons, ← hhc1_val]
      simpa [List.append_assoc, leaf_h] using ih_rel

theorem verify_chunks_outer_loop_spec
    {H : Type} {sha256 : List U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List U8 → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (rsv : ReadSizedMerkleVerifier)
    (offset : Usize) (data : Slice U8)
    (hash_start_index hashes_per_hash : Usize)
    (hwf : ReadSizedWf rsv)
    (hoff_rs : offset.val % rsv.read_size.val = 0)
    (hsum : offset.val + data.val.length ≤ Usize.max)
    (hhstart : hash_start_index.val = offset.val / rsv.read_size.val)
    (hhph : hashes_per_hash.val = rsv.read_size.val / 8192)
    (hend : hash_start_index.val + (data.val.length + rsv.read_size.val - 1) / rsv.read_size.val ≤
              rsv.hashes.val.length) :
    ∀ (rem : Nat) (chunk_pos i : Usize),
      chunk_pos.val = min data.val.length (i.val * rsv.read_size.val) →
      i.val ≤ (data.val.length + rsv.read_size.val - 1) / rsv.read_size.val →
      (data.val.length + rsv.read_size.val - 1) / rsv.read_size.val - i.val = rem →
      ∃ res : core.result.Result Unit VerifyError,
        ReadSizedMerkleVerifier.verify_chunks_loop0 hInst
          rsv offset data hash_start_index hashes_per_hash chunk_pos i = ok res ∧
        (res = .Ok () ↔
          ∀ k, i.val ≤ k →
            k < (data.val.length + rsv.read_size.val - 1) / rsv.read_size.val →
            ∀ hk : hash_start_index.val + k < rsv.hashes.val.length,
              rsv.hashes.val[hash_start_index.val + k]'hk =
                specHashInternal sha256 hashes_per_hash.val rsv.bytes_per_hash 1#u64
                  (u64OfNat ((hash_start_index.val + k) * rsv.bytes_per_hash.val))
                  (chunkLeafHashes sha256 (offset.val + k * rsv.read_size.val)
                    ((data.val.drop (k * rsv.read_size.val)).take rsv.read_size.val)))
  | 0, chunk_pos, i, hcpos, _, _ => by
    have hi_eq : i.val = (data.val.length + rsv.read_size.val - 1) / rsv.read_size.val := by omega
    have hmul_ge : data.val.length ≤ i.val * rsv.read_size.val := by
      rw [hi_eq]; exact le_div_ceil_mul hwf.hread_pos
    have hcpos_eq : chunk_pos.val = data.val.length := by
      rw [hcpos]; exact Nat.min_eq_left hmul_ge
    have hnlt : ¬ (chunk_pos < Slice.len data) :=
      not_usize_lt_of_ge (by rw [Slice.len_val, Slice.length, hcpos_eq])
    have hbody :
        ReadSizedMerkleVerifier.verify_chunks_loop0.body hInst
          rsv offset data hash_start_index hashes_per_hash chunk_pos i = ok (.done (.Ok ())) := by
      unfold ReadSizedMerkleVerifier.verify_chunks_loop0.body
      simp only [hnlt, ↓reduceIte]
    refine ⟨.Ok (), ?_, ?_⟩
    · unfold ReadSizedMerkleVerifier.verify_chunks_loop0
      exact loop_done _ (chunk_pos, i) _ hbody
    · simp only [true_iff]
      intro k hk_ge hk_lt
      omega
  | rem + 1, chunk_pos, i, hcpos, _, _ => by
    have hi_lt : i.val < (data.val.length + rsv.read_size.val - 1) / rsv.read_size.val := by omega
    have hmul_lt : i.val * rsv.read_size.val < data.val.length :=
      mul_lt_of_lt_div_ceil hwf.hread_pos hi_lt
    have hcpos_val : chunk_pos.val = i.val * rsv.read_size.val := by
      rw [hcpos]; exact Nat.min_eq_right (le_of_lt hmul_lt)
    have hlt_d : chunk_pos < Slice.len data :=
      usize_lt_of_lt (by rw [Slice.len_val, Slice.length, hcpos_val]; exact hmul_lt)
    obtain ⟨rem_d, hsub_d, hrem_d_val⟩ := uscalar_sub_ok (Slice.len data) chunk_pos (by
      rw [Slice.len_val, Slice.length, hcpos_val]; omega)
    simp only [Slice.len_val, Slice.length] at hrem_d_val
    obtain ⟨chunk_len, hmin_c, hclen_val⟩ := min_usize_spec rsv.read_size rem_d
    rw [hrem_d_val, hcpos_val] at hclen_val
    obtain ⟨cpos1, hadd_cpos, hcpos1_val⟩ := uscalar_add_ok chunk_pos chunk_len (by
      rw [uscalar_max_usize]; omega)
    obtain ⟨chunk, hslice_c, hchunk_val⟩ := slice_range_ok data chunk_pos cpos1
      (by omega) (by omega)
    have hchunk_eq :
        chunk.val = (data.val.drop (i.val * rsv.read_size.val)).take rsv.read_size.val := by
      rw [hchunk_val, hcpos_val, show cpos1.val - i.val * rsv.read_size.val = chunk_len.val by omega,
          hclen_val]
      by_cases hle_rs : rsv.read_size.val ≤ data.val.length - i.val * rsv.read_size.val
      · rw [Nat.min_eq_left hle_rs]
      · rw [Nat.min_eq_right (by omega),
            List.take_of_length_le (by rw [List.length_drop]),
            List.take_of_length_le (by rw [List.length_drop]; omega)]
    have hidx_lt : hash_start_index.val + i.val < rsv.hashes.val.length := by omega
    have hrsv_usize : rsv.hashes.val.length ≤ Usize.max := rsv.hashes.property
    obtain ⟨hidx, hadd_hidx, hhidx_val⟩ := uscalar_add_ok hash_start_index i (by
      rw [uscalar_max_usize]; omega)
    have hlvl_off_bound : (hash_start_index.val + i.val) * rsv.bytes_per_hash.val ≤ U64.max := by
      calc (hash_start_index.val + i.val) * rsv.bytes_per_hash.val
          ≤ rsv.hashes.val.length * rsv.bytes_per_hash.val :=
            Nat.mul_le_mul_right _ (le_of_lt hidx_lt)
        _ ≤ U64.max := hwf.hoff_bound
    obtain ⟨lvl_off, hmul_loff, hloff_val⟩ :=
      uscalar_mul_ok (UScalar.cast .U64 hidx) (UScalar.cast .U64 rsv.bytes_per_hash) (by
        rw [uscalar_max_u64, cast_u64_usize_val, cast_u64_u32_val, hhidx_val]
        exact hlvl_off_bound)
    have hloff_eq : lvl_off = u64OfNat ((hash_start_index.val + i.val) * rsv.bytes_per_hash.val) := by
      apply UScalar.eq_of_val_eq
      rw [hloff_val, cast_u64_usize_val, cast_u64_u32_val, hhidx_val,
          u64OfNat_val_of_le _ hlvl_off_bound]
    obtain ⟨hasher0, hmhh, hrel0⟩ :=
      make_hash_hasher_gen_spec hsha rsv.bytes_per_hash 1#u64 lvl_off (by decide)
    have hrs_eq_K : rsv.read_size.val = hashes_per_hash.val * 8192 := by
      rw [hhph]; exact (Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hwf.hread_aligned)).symm
    have hoff_align8 : offset.val % 8192 = 0 := by
      have hdvd_rs : rsv.read_size.val ∣ offset.val := Nat.dvd_of_mod_eq_zero hoff_rs
      have hdvd8 : 8192 ∣ rsv.read_size.val := Nat.dvd_of_mod_eq_zero hwf.hread_aligned
      exact Nat.mod_eq_zero_of_dvd (Nat.dvd_trans hdvd8 hdvd_rs)
    have hcpos_align8 : chunk_pos.val % 8192 = 0 := by
      rw [hcpos_val, hrs_eq_K, ← Nat.mul_assoc]; simp
    have hbase_align : (offset.val + chunk_pos.val) % 8192 = 0 := by omega
    have hchunk_len_le : chunk.val.length ≤ data.val.length - chunk_pos.val := by
      rw [hchunk_eq, hcpos_val, List.length_take, List.length_drop]; exact Nat.min_le_right _ _
    have hsum64 : offset.val + chunk_pos.val + chunk.val.length ≤ U64.max := by
      have ⟨_, hu64⟩ := usize_bounds; omega
    have hchunk_le_rs : chunk.val.length ≤ hashes_per_hash.val * 8192 := by
      rw [← hrs_eq_K, hchunk_eq]; exact List.length_take_le _ _
    have hceil_le_K : (chunk.val.length + 8191) / 8192 ≤ hashes_per_hash.val :=
      div_ceil_8192_le_of_le_mul hchunk_le_rs
    have hmax_rsv : 12 + hashes_per_hash.val * 32 ≤ SHA256_MAX_BYTES := by
      rw [hhph]; exact rsv_preimage_le_sha256_max hwf
    obtain ⟨hasher1, hc_out, hinner, hrel1, hhc_out_val⟩ :=
      verify_chunks_inner_loop_spec hsha offset chunk_pos chunk hbase_align hsum64
        ((chunk.val.length + 8191) / 8192)
        hasher0 (specIdentity rsv.bytes_per_hash 1#u64 lvl_off) 0#usize 0#usize
        (by simp) (Nat.zero_le _) (by simp) hrel0
        (by simp only [specIdentity_length, UScalar.ofNatCore_val_eq, Nat.sub_zero]; omega)
    simp only [UScalar.ofNatCore_val_eq, List.drop_zero] at hrel1
    have hhc_le_K : hc_out.val ≤ hashes_per_hash.val := by
      rw [hhc_out_val]; exact hceil_le_K
    set comp_i :=
      chunkLeafHashes sha256 (offset.val + i.val * rsv.read_size.val)
        ((data.val.drop (i.val * rsv.read_size.val)).take rsv.read_size.val)
    have hcomp_eq : chunkLeafHashes sha256 (offset.val + chunk_pos.val) chunk.val = comp_i := by
      rw [hcpos_val, hchunk_eq]
    rw [hcomp_eq] at hrel1
    have hcomp_len : comp_i.length = hc_out.val := by
      rw [← hcomp_eq, chunkLeafHashes_length, hhc_out_val]
    have hpad_branch :
        ∃ hasher2 : H,
          (if hc_out != hashes_per_hash
           then do
             let i8 ← hashes_per_hash - hc_out
             let i9 ← i8 * HASH_SIZE
             update_with_zeros hInst hasher1 i9
           else ok hasher1) = ok hasher2 ∧
          R_H hasher2 (specInternalPreimage hashes_per_hash.val rsv.bytes_per_hash 1#u64 lvl_off comp_i) := by
      by_cases hne : hc_out != hashes_per_hash
      · simp only [hne, ↓reduceIte]
        obtain ⟨diff, hsub_k, hdiff_val⟩ := uscalar_sub_ok hashes_per_hash hc_out hhc_le_K
        have hmul_pad_le : diff.val * 32 ≤ Usize.max :=
          rsv_pad_bytes_le_usize_max hwf (by omega)
        obtain ⟨zbytes, hmul_z, hz_val⟩ := uscalar_mul_ok diff HASH_SIZE (by
          rw [uscalar_max_usize, HASH_SIZE_val]; exact hmul_pad_le)
        rw [HASH_SIZE_val, hdiff_val, ← hcomp_len] at hz_val
        obtain ⟨hasher2, hzupd, hrel2⟩ := update_with_zeros_gen_spec hsha hasher1 _ zbytes hrel1
          (by simp only [List.length_append, specIdentity_length, concatHashes_length, hz_val,
                         hcomp_len]; omega)
        rw [hz_val] at hrel2
        refine ⟨hasher2, ?_, ?_⟩
        · simp only [hsub_k, hmul_z, hzupd, bind_ok]
        · simpa [specInternalPreimage, HASH_BYTES, List.append_assoc] using hrel2
      · simp only [hne, Bool.false_eq_true, ↓reduceIte]
        have heq_hc : hc_out.val = hashes_per_hash.val := by
          have : hc_out = hashes_per_hash := by
            by_contra hc; exact hne (bne_iff_ne.mpr hc)
          exact congrArg UScalar.val this
        exact ⟨hasher1, rfl, by simpa [specInternalPreimage, hcomp_len, heq_hc, specZeroPad] using hrel1⟩
    obtain ⟨hasher2, hpad_exec, hrel2⟩ := hpad_branch
    have hfin := hsha.finalize_ok hasher2 _ hrel2
      (by simp only [specInternalPreimage, List.length_append, specIdentity_length,
                     concatHashes_length, specZeroPad_length, HASH_BYTES, hcomp_len]; omega)
    have hv_idx := slice_index_ok rsv.hashes hidx (by omega)
    set expected_i :=
      specHashInternal sha256 hashes_per_hash.val rsv.bytes_per_hash 1#u64
        (u64OfNat ((hash_start_index.val + i.val) * rsv.bytes_per_hash.val)) comp_i
    have hfin_exp : hInst.finalize hasher2 = ok expected_i := by
      simpa [expected_i, specHashInternal, hloff_eq] using hfin
    set stored_i := rsv.hashes.val[hash_start_index.val + i.val]'hidx_lt
    have hv_idx' : Slice.index_usize rsv.hashes hidx = ok stored_i := by
      simpa [stored_i, hhidx_val] using hv_idx
    by_cases hmatch : expected_i = stored_i
    · have hne_false :
          core.cmp.PartialEq.ne.trait_default Hash.Insts.CoreCmpPartialEqHash expected_i stored_i =
            ok false := by
        simp [hash_ne_spec, hmatch]
      have hi_le_mul : i.val ≤ i.val * rsv.read_size.val :=
        Nat.le_mul_of_pos_right _ hwf.hread_pos
      have hd_usize : data.val.length ≤ Usize.max := data.property
      obtain ⟨i1, hadd_i1, hi1_val⟩ := uscalar_add_ok i 1#usize (by
        rw [uscalar_max_usize]; simp only [UScalar.ofNatCore_val_eq]; omega)
      simp only [UScalar.ofNatCore_val_eq] at hi1_val
      have hbody :
          ReadSizedMerkleVerifier.verify_chunks_loop0.body hInst
            rsv offset data hash_start_index hashes_per_hash chunk_pos i =
            ok (.cont (cpos1, i1)) := by
        unfold ReadSizedMerkleVerifier.verify_chunks_loop0.body
        simp only [hlt_d, ↓reduceIte, hsub_d, hmin_c, hadd_cpos, bind_ok]
        refine bind_ok_step hslice_c ?_
        simp only [hadd_hidx, lift, hmul_loff, hmhh, hinner, uncurry_apply_pair, bind_ok]
        refine bind_ok_step hpad_exec ?_
        simp only [hfin_exp, bind_ok, hv_idx', hne_false, Bool.false_eq_true, ↓reduceIte, hadd_i1]
      have hcpos1_inv : cpos1.val = min data.val.length (i1.val * rsv.read_size.val) := by
        rw [hcpos1_val, hcpos_val, hclen_val, hi1_val, Nat.succ_mul]
        omega
      obtain ⟨res, ih_exec, ih_iff⟩ :=
        verify_chunks_outer_loop_spec hsha rsv offset data hash_start_index hashes_per_hash
          hwf hoff_rs hsum hhstart hhph hend rem cpos1 i1 hcpos1_inv (by omega) (by omega)
      refine ⟨res, ?_, ?_⟩
      · unfold ReadSizedMerkleVerifier.verify_chunks_loop0 at ih_exec ⊢
        rw [loop_cont _ (chunk_pos, i) _ hbody]
        exact ih_exec
      · rw [ih_iff]
        constructor
        · intro hall k hk_ge hk_lt hk_in
          by_cases hki : k = i.val
          · subst hki; exact hmatch.symm
          · exact hall k (by omega) hk_lt hk_in
        · intro hall k hk_ge hk_lt hk_in
          exact hall k (by omega) hk_lt hk_in
    · have hne_true :
          core.cmp.PartialEq.ne.trait_default Hash.Insts.CoreCmpPartialEqHash expected_i stored_i =
            ok true := by
        simp [hash_ne_spec, hmatch]
      have hbody :
          ReadSizedMerkleVerifier.verify_chunks_loop0.body hInst
            rsv offset data hash_start_index hashes_per_hash chunk_pos i =
            ok (.done (.Err .IoDataIntegrity)) := by
        unfold ReadSizedMerkleVerifier.verify_chunks_loop0.body
        simp only [hlt_d, ↓reduceIte, hsub_d, hmin_c, hadd_cpos, bind_ok]
        refine bind_ok_step hslice_c ?_
        simp only [hadd_hidx, lift, hmul_loff, hmhh, hinner, uncurry_apply_pair, bind_ok]
        refine bind_ok_step hpad_exec ?_
        simp only [hfin_exp, bind_ok, hv_idx', hne_true, ↓reduceIte]
      refine ⟨.Err .IoDataIntegrity, ?_, ?_⟩
      · unfold ReadSizedMerkleVerifier.verify_chunks_loop0
        exact loop_done _ (chunk_pos, i) _ hbody
      · simp only [reduceCtorEq, false_iff]
        intro hall
        exact hmatch (hall i.val (le_refl _) hi_lt hidx_lt).symm

private theorem verify_empty_slice_branch_spec
    {H : Type} {sha256 : List U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List U8 → Prop}
    (hsha : ValidSha256State sha256 hInst R_H)
    (rsv : ReadSizedMerkleVerifier) (hph : Usize)
    (hwf : ReadSizedWf rsv)
    (hhph_val : hph.val = rsv.read_size.val / 8192)
    (hK : 0 < hph.val)
    (hrsv1 : rsv.hashes.val.length = 1) :
    (do
      let h ← hash_block hInst (Std.Array.empty U8).to_slice 0#u64
      let expected ← hash_hashes_slice hInst hph rsv.bytes_per_hash 0#usize
        (Array.make 1#usize [ h ]).to_slice
      let h1 ← Slice.index_usize rsv.hashes 0#usize
      let b2 ← Hash.Insts.CoreCmpPartialEqHash.eq h1 expected
      if b2
      then ok (core.result.Result.Ok ())
      else ok (core.result.Result.Err VerifyError.IoDataIntegrity)) =
    ok (if rsv.hashes.val[0]'(by omega) =
           specHashInternal sha256 (rsv.read_size.val / 8192) rsv.bytes_per_hash 1#u64
             (u64OfNat (0 * rsv.bytes_per_hash.val)) [specHashBlock sha256 [] 0#u64]
        then core.result.Result.Ok ()
        else core.result.Result.Err VerifyError.IoDataIntegrity) := by
  have hhb0 := hash_block_gen_spec sha256 hInst R_H (Std.Array.empty U8).to_slice 0#u64 hsha
    (by simp [BLOCK_BYTES]) (by simp [BLOCK_BYTES])
  have hemp_arr : (Std.Array.empty U8).to_slice.val = [] := by simp [Std.Array.empty]
  rw [hemp_arr] at hhb0
  set h0 := specHashBlock sha256 [] 0#u64
  have hs1_val : (Array.make 1#usize [h0]).to_slice.val = [h0] := by
    simp only [Array.val_to_slice]; rfl
  have hs1_len : (Array.make 1#usize [h0]).to_slice.length ≤ hph.val := by
    rw [Slice.length, hs1_val, List.length_singleton]; exact hK
  have hpad_u : (hph.val - (Array.make 1#usize [h0]).to_slice.length) * 32 ≤ Usize.max :=
    rsv_pad_bytes_le_usize_max hwf (by omega)
  have hmax_sha : 12 + hph.val * 32 ≤ SHA256_MAX_BYTES := by
    rw [hhph_val]; exact rsv_preimage_le_sha256_max hwf
  have hhh0 := hash_hashes_slice_gen_spec hsha hph rsv.bytes_per_hash 0#usize
    (Array.make 1#usize [h0]).to_slice hs1_len hpad_u (by simp) hmax_sha
  rw [hhph_val, show (0#usize).val = 0 from rfl, hs1_val] at hhh0
  have hv0 :
      Slice.index_usize rsv.hashes 0#usize =
        ok (rsv.hashes.val[0]'(by omega)) := by
    simpa using slice_index_ok rsv.hashes 0#usize (by simp [hrsv1])
  simp only [hhb0, hhh0, hv0, hash_eq_spec, bind_ok]
  by_cases hc :
      rsv.hashes.val[0]'(by omega) =
      specHashInternal sha256 (rsv.read_size.val / 8192) rsv.bytes_per_hash 1#u64
        (u64OfNat (0 * rsv.bytes_per_hash.val)) [h0]
  · simp only [hc, decide_true, ↓reduceIte]
  · simp only [hc, decide_false, Bool.false_eq_true, ↓reduceIte]

-- ============================================================================
-- 3. `ReadSizedMerkleVerifier.verify` -- Totality (P1) & Completeness (P3)
-- ============================================================================

/-- Totality (panic and arithmetic overflow freedom) of `ReadSizedMerkleVerifier.verify`. -/
theorem read_sized_verifier_verify_no_panic_of_wf
    {H : Type} {sha256 : List U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List U8 → Prop}
    {rsv : ReadSizedMerkleVerifier}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hwf : ReadSizedWf rsv)
    (offset : Usize) (data : Slice U8) :
    ∃ res, ReadSizedMerkleVerifier.verify hInst rsv offset data = ok res := by
  unfold ReadSizedMerkleVerifier.verify
  simp only [lift, bind_ok]
  rcases usize_checked_add_spec_opt offset (Slice.len data) with
    ⟨hnone, _⟩ | ⟨v, hsome, hsum, hv_val⟩
  · simp only [hnone]; exact ⟨_, rfl⟩
  · simp only [hsome, usize_is_multiple_of_eq, bind_ok]
    simp only [Slice.len_val, Slice.length] at hsum hv_val
    by_cases hoff : offset.val % rsv.read_size.val = 0
    · simp only [hoff, decide_true, ↓reduceIte]
      obtain ⟨hph, hdiv_hph, hhph_val⟩ := uscalar_div_ok rsv.read_size BLOCK_SIZE (by simp)
      rw [BLOCK_SIZE_val] at hhph_val
      have hK : 0 < hph.val := by
        have hge8 : 8192 ≤ rsv.read_size.val := by
          have := hwf.hread_pos; have := hwf.hread_aligned; omega
        rw [hhph_val]; exact (Nat.le_div_iff_mul_le (by decide)).mpr (by omega)
      simp only [hdiv_hph, bind_ok]
      by_cases hemp : data.val.length = 0
      · have his_emp : core.slice.Slice.is_empty data = ok true := by
          simp [core.slice.Slice.is_empty, Slice.length, hemp]
        rw [bind_ok_step his_emp rfl]
        simp only [↓reduceIte]
        by_cases hoff0 : offset != 0#usize
        · simp only [hoff0, ↓reduceIte]; exact ⟨_, rfl⟩
        · simp only [hoff0, Bool.false_eq_true, ↓reduceIte]
          by_cases hlen1 : Slice.len rsv.hashes != 1#usize
          · simp only [hlen1, ↓reduceIte]; exact ⟨_, rfl⟩
          · have hrsv1 : rsv.hashes.val.length = 1 := by
              have hbeq : Slice.len rsv.hashes = 1#usize := by
                by_contra hne; exact hlen1 (bne_iff_ne.mpr hne)
              simpa using congrArg UScalar.val hbeq
            simp only [hlen1, Bool.false_eq_true, ↓reduceIte]
            exact ⟨_, verify_empty_slice_branch_spec hsha rsv hph hwf hhph_val hK hrsv1⟩
      · have his_emp : core.slice.Slice.is_empty data = ok false := by
          simp [core.slice.Slice.is_empty, Slice.length, hemp]
        rw [bind_ok_step his_emp rfl]
        simp only [Bool.false_eq_true, ↓reduceIte]
        obtain ⟨hstart, hdiv_start, hstart_val⟩ :=
          uscalar_div_ok offset rsv.read_size (by have := hwf.hread_pos; omega)
        obtain ⟨hend_idx, hceil_end, hend_val⟩ :=
          div_ceil_usize_spec v rsv.read_size hwf.hread_pos
        rw [hv_val] at hend_val
        simp only [hdiv_start, hceil_end, bind_ok]
        by_cases hmult_v : v.val % rsv.read_size.val = 0
        · simp only [hmult_v, decide_true, ↓reduceIte]
          by_cases hgt : hend_idx > Slice.len rsv.hashes
          · simp only [hgt, ↓reduceIte]; exact ⟨_, rfl⟩
          · simp only [hgt, ↓reduceIte]
            have hle_h : hend_idx.val ≤ rsv.hashes.val.length := by
              have : hend_idx.val ≤ (Slice.len rsv.hashes).val := by
                by_contra hc; exact hgt (usize_lt_of_lt (by omega))
              simpa using this
            have hend_chunks :
                hstart.val + (data.val.length + rsv.read_size.val - 1) / rsv.read_size.val ≤
                  rsv.hashes.val.length := by
              have hdecomp := div_ceil_add_aligned (len := data.val.length) hwf.hread_pos hoff
              omega
            obtain ⟨res, hvc, _⟩ :=
              verify_chunks_outer_loop_spec hsha rsv offset data hstart hph
                hwf hoff hsum hstart_val hhph_val hend_chunks
                ((data.val.length + rsv.read_size.val - 1) / rsv.read_size.val)
                0#usize 0#usize (by simp) (Nat.zero_le _) (by simp)
            exact ⟨res, hvc⟩
        · simp only [hmult_v, decide_false, Bool.false_eq_true, ↓reduceIte]
          by_cases hne_end : hend_idx != Slice.len rsv.hashes
          · simp only [hne_end, ↓reduceIte]; exact ⟨_, rfl⟩
          · have hend_eq : hend_idx = Slice.len rsv.hashes := by
              by_contra hc; exact hne_end (bne_iff_ne.mpr hc)
            simp only [hend_eq, bne_self_eq_false, usize_gt_irrefl, Bool.false_eq_true, ↓reduceIte]
            have hend_chunks :
                hstart.val + (data.val.length + rsv.read_size.val - 1) / rsv.read_size.val ≤
                  rsv.hashes.val.length := by
              have hdecomp := div_ceil_add_aligned (len := data.val.length) hwf.hread_pos hoff
              have hv_eq := congrArg UScalar.val hend_eq
              simp only [Slice.len_val, Slice.length] at hv_eq
              omega
            obtain ⟨res, hvc, _⟩ :=
              verify_chunks_outer_loop_spec hsha rsv offset data hstart hph
                hwf hoff hsum hstart_val hhph_val hend_chunks
                ((data.val.length + rsv.read_size.val - 1) / rsv.read_size.val)
                0#usize 0#usize (by simp) (Nat.zero_le _) (by simp)
            exact ⟨res, hvc⟩
    · simp only [hoff, decide_false, Bool.false_eq_true, ↓reduceIte]; exact ⟨_, rfl⟩

/-- Completeness of `ReadSizedMerkleVerifier.verify` on the empty blob `[]` at offset `0`. -/
theorem read_sized_verifier_verify_complete_empty_of_wf
    {H : Type} {sha256 : List U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List U8 → Prop}
    {rsv : ReadSizedMerkleVerifier} {data : Slice U8}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hwf : ReadSizedWf rsv)
    (hK : 0 < rsv.read_size.val / 8192)
    (hrsv_h :
      rsv.hashes.val =
        specReadSizedHashes sha256 (rsv.read_size.val / 8192) hK rsv.bytes_per_hash
          (specLeafHashes sha256 []))
    (hdata : data.val = []) :
    ReadSizedMerkleVerifier.verify hInst rsv 0#usize data = ok (.Ok ()) := by
  unfold ReadSizedMerkleVerifier.verify
  simp only [lift, bind_ok]
  rcases usize_checked_add_spec_opt 0#usize (Slice.len data) with
    ⟨_, hovf⟩ | ⟨_, hsome, _, _⟩
  · simp only [UScalar.ofNatCore_val_eq, Slice.len_val, Slice.length, hdata, List.length_nil,
               Nat.add_zero] at hovf
    omega
  · simp only [hsome, usize_is_multiple_of_eq, UScalar.ofNatCore_val_eq, Nat.zero_mod,
               decide_true, ↓reduceIte, bind_ok]
    obtain ⟨hph, hdiv_hph, hhph_val⟩ := uscalar_div_ok rsv.read_size BLOCK_SIZE (by simp)
    rw [BLOCK_SIZE_val] at hhph_val
    have hK_hph : 0 < hph.val := by rw [hhph_val]; exact hK
    have his_emp : core.slice.Slice.is_empty data = ok true := by
      simp [core.slice.Slice.is_empty, Slice.length, hdata]
    have hrsv_len : rsv.hashes.val.length = 1 := by
      rw [hrsv_h, specReadSizedHashes_length, specLeafHashes]
      simp only [List.isEmpty_nil, ↓reduceIte, List.length_singleton]
      have hrew : 1 + rsv.read_size.val / 8192 - 1 = rsv.read_size.val / 8192 := by omega
      rw [hrew, Nat.div_self hK]
    have hlen1_eq : Slice.len rsv.hashes = 1#usize :=
      UScalar.eq_of_val_eq (by simp [hrsv_len])
    simp only [hdiv_hph, bind_ok]
    rw [bind_ok_step his_emp rfl]
    simp only [↓reduceIte, bne_self_eq_false, Bool.false_eq_true, hlen1_eq]
    rw [verify_empty_slice_branch_spec hsha rsv hph hwf hhph_val hK_hph hrsv_len]
    have hrsv0 :
        rsv.hashes.val[0]'(by omega) =
        specHashInternal sha256 (rsv.read_size.val / 8192) rsv.bytes_per_hash 1#u64
          (u64OfNat (0 * rsv.bytes_per_hash.val)) [specHashBlock sha256 [] 0#u64] := by
      have h0_eq :
          rsv.hashes.val[0]'(by omega) =
          (specReadSizedHashes sha256 (rsv.read_size.val / 8192) hK rsv.bytes_per_hash
            (specLeafHashes sha256 []))[0]'(by rw [← hrsv_h]; omega) :=
        List.getElem_of_eq hrsv_h _
      have hget := specReadSizedHashes_getElem sha256 (rsv.read_size.val / 8192) hK
        rsv.bytes_per_hash (specLeafHashes sha256 []) 0 (by rw [← hrsv_h]; omega)
      have htake1 :
          ((specLeafHashes sha256 []).drop (0 * (rsv.read_size.val / 8192))).take
            (rsv.read_size.val / 8192) = [specHashBlock sha256 [] 0#u64] := by
        simp only [Nat.zero_mul, List.drop_zero, specLeafHashes, List.isEmpty_nil, ↓reduceIte]
        exact List.take_of_length_le (by simp only [List.length_singleton]; omega)
      rw [h0_eq, hget, htake1]
    simp only [hrsv0, ↓reduceIte]

/-- Exact equality between computed chunk leaf hashes and the corresponding group of `specLeafHashes`
    whenever `data = (B.drop offset).take data.len` satisfies the `read_size` alignment contract. -/
theorem chunkLeafHashes_eq_specLeafHashes_group
    (sha256 : List U8 → Hash) (B data : List U8) (offset readSize : Nat)
    (hrs_pos : 0 < readSize)
    (hrs_align : readSize % 8192 = 0)
    (hoff_rs : offset % readSize = 0)
    (hdne : data ≠ [])
    (hin_B : offset + data.length ≤ B.length)
    (hdata : data = (B.drop offset).take data.length)
    (halign_end : (offset + data.length) % readSize = 0 ∨ offset + data.length = B.length)
    (i : Nat) (hi : i < (data.length + readSize - 1) / readSize) :
    chunkLeafHashes sha256 (offset + i * readSize) ((data.drop (i * readSize)).take readSize) =
      ((specLeafHashes sha256 B).drop ((offset / readSize + i) * (readSize / 8192))).take
        (readSize / 8192) := by
  set K := readSize / 8192
  have hrs_eq : readSize = K * 8192 :=
    (Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hrs_align)).symm
  have hK : 0 < K := by
    by_contra h0; simp [Nat.eq_zero_of_not_pos h0] at hrs_eq; omega
  have hBne : B ≠ [] := by
    have hdpos := List.length_pos_iff.mpr hdne
    intro hnil; subst hnil; simp only [List.length_nil] at hin_B; omega
  have hi_mul_lt : i * readSize < data.length := mul_lt_of_lt_div_ceil hrs_pos hi
  have hchunk_B :
      (data.drop (i * readSize)).take readSize =
      (B.drop (offset + i * readSize)).take readSize := by
    rcases halign_end with hmod0 | hendB
    · have hd_mod0 : data.length % readSize = 0 := by
        have hq : offset = readSize * (offset / readSize) :=
          (Nat.mul_div_cancel' (Nat.dvd_of_mod_eq_zero hoff_rs)).symm
        have hrew : offset + data.length = data.length + readSize * (offset / readSize) := by omega
        rw [hrew, Nat.add_mul_mod_self_left] at hmod0
        exact hmod0
      have hi1_le : (i + 1) * readSize ≤ data.length := by
        have hd_div : (data.length + readSize - 1) / readSize = data.length / readSize := by
          have hq : data.length = readSize * (data.length / readSize) :=
            (Nat.mul_div_cancel' (Nat.dvd_of_mod_eq_zero hd_mod0)).symm
          have h1 : (readSize - 1) / readSize = 0 := Nat.div_eq_of_lt (by omega)
          calc (data.length + readSize - 1) / readSize
              = (readSize * (data.length / readSize) + (readSize - 1)) / readSize := by
                  congr 1; omega
            _ = data.length / readSize := by rw [Nat.mul_add_div hrs_pos, h1, Nat.add_zero]
        rw [hd_div] at hi
        have h1 : i + 1 ≤ data.length / readSize := hi
        exact (Nat.le_div_iff_mul_le hrs_pos).mp h1
      rw [hdata, List.drop_take, List.take_take,
          Nat.min_eq_left (by rw [Nat.succ_mul] at hi1_le; omega), List.drop_drop]
    · have hd_eq : data = B.drop offset := by
        rw [hdata, show data.length = B.length - offset by omega,
            ← List.length_drop, List.take_length]
      rw [hd_eq, List.drop_drop]
  set base := (offset / readSize + i) * K
  have hbase_mul : base * 8192 = offset + i * readSize := by
    dsimp [base]
    have hoff_eq : (offset / readSize) * readSize = offset :=
      Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hoff_rs)
    calc ((offset / readSize + i) * K) * 8192
        = (offset / readSize) * (K * 8192) + i * (K * 8192) := by ring
      _ = offset + i * readSize := by rw [← hrs_eq, hoff_eq]
  have hbase_lt_B : base * 8192 < B.length := by omega
  have hsl_len : (specLeafHashes sha256 B).length = (B.length + 8191) / 8192 := by
    rw [specLeafHashes_length, List.isEmpty_eq_false_iff.mpr hBne]
    simp
  have hlen_eq :
      (chunkLeafHashes sha256 (offset + i * readSize)
        ((data.drop (i * readSize)).take readSize)).length =
      (((specLeafHashes sha256 B).drop base).take K).length := by
    rw [chunkLeafHashes_length, hchunk_B, List.length_take, List.length_drop,
        List.length_take, List.length_drop, hsl_len, ← hbase_mul, hrs_eq]
    exact min_div_ceil_eq hK hbase_lt_B
  refine List.ext_getElem hlen_eq fun j hj₁ hj₂ => ?_
  rw [chunkLeafHashes_getElem, List.getElem_take, List.getElem_drop]
  have hj_lt_K : j < K := by
    rw [List.length_take] at hj₂; exact lt_of_lt_of_le hj₂ (Nat.min_le_left _ _)
  have hidx_sl : base + j < (specLeafHashes sha256 B).length := by
    rw [List.length_take, List.length_drop] at hj₂; omega
  obtain ⟨hB_leaf, _, _⟩ := specLeafHashes_getElem_of_nonempty sha256 B hBne (base + j) hidx_sl
  rw [hB_leaf, hchunk_B]
  have hsub :
      (((B.drop (offset + i * readSize)).take readSize).drop (j * 8192)).take 8192 =
      (B.drop ((base + j) * 8192)).take 8192 := by
    rw [← hbase_mul, hrs_eq, List.drop_take, List.take_take]
    have hle : 8192 ≤ K * 8192 - j * 8192 := by
      have h1 : (j + 1) * 8192 ≤ K * 8192 := Nat.mul_le_mul_right 8192 hj_lt_K
      rw [Nat.succ_mul] at h1; omega
    rw [Nat.min_eq_left hle, List.drop_drop, Nat.add_mul]
  have hoff_eq_j : offset + i * readSize + j * 8192 = (base + j) * 8192 := by
    rw [Nat.add_mul, hbase_mul]
  rw [hsub, hoff_eq_j]

/-- Completeness of `ReadSizedMerkleVerifier.verify` on non-empty `read_size`-aligned slices. -/
theorem read_sized_verifier_verify_complete_of_wf
    {H : Type} {sha256 : List U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List U8 → Prop}
    {B : List U8} {rsv : ReadSizedMerkleVerifier}
    {offset : Usize} {data : Slice U8}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hwf : ReadSizedWf rsv)
    (hK : 0 < rsv.read_size.val / 8192)
    (hrsv_h :
      rsv.hashes.val =
        specReadSizedHashes sha256 (rsv.read_size.val / 8192) hK rsv.bytes_per_hash
          (specLeafHashes sha256 B))
    (hdne : data.val ≠ [])
    (hoff_rs : offset.val % rsv.read_size.val = 0)
    (hsum_usize : offset.val + data.val.length ≤ Usize.max)
    (hin_blob : offset.val + data.val.length ≤ B.length)
    (hdata : data.val = (B.drop offset.val).take data.val.length)
    (halign_end :
      (offset.val + data.val.length) % rsv.read_size.val = 0 ∨
      offset.val + data.val.length = B.length) :
    ReadSizedMerkleVerifier.verify hInst rsv offset data = ok (.Ok ()) := by
  have hdpos : 0 < data.val.length := List.length_pos_iff.mpr hdne
  have hBne : B ≠ [] := fun h => by subst h; simp only [List.length_nil] at hin_blob; omega
  unfold ReadSizedMerkleVerifier.verify
  simp only [lift, bind_ok]
  rcases usize_checked_add_spec_opt offset (Slice.len data) with
    ⟨_, hovf⟩ | ⟨v, hsome, _, hv_val⟩
  · simp only [Slice.len_val, Slice.length] at hovf; omega
  · simp only [hsome, usize_is_multiple_of_eq, hoff_rs, decide_true, ↓reduceIte, bind_ok]
    simp only [Slice.len_val, Slice.length] at hv_val
    obtain ⟨hph, hdiv_hph, hhph_val⟩ := uscalar_div_ok rsv.read_size BLOCK_SIZE (by simp)
    rw [BLOCK_SIZE_val] at hhph_val
    have his_emp : core.slice.Slice.is_empty data = ok false := by
      simp [core.slice.Slice.is_empty, Slice.length, Nat.ne_of_gt hdpos]
    simp only [hdiv_hph, bind_ok]
    rw [bind_ok_step his_emp rfl]
    simp only [Bool.false_eq_true, ↓reduceIte]
    obtain ⟨hstart, hdiv_start, hstart_val⟩ :=
      uscalar_div_ok offset rsv.read_size (by have := hwf.hread_pos; omega)
    obtain ⟨hend_idx, hceil_end, hend_val⟩ :=
      div_ceil_usize_spec v rsv.read_size hwf.hread_pos
    rw [hv_val] at hend_val
    have hrs_eq : rsv.read_size.val = (rsv.read_size.val / 8192) * 8192 :=
      (Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hwf.hread_aligned)).symm
    have hrsv_len :
        rsv.hashes.val.length = (B.length + rsv.read_size.val - 1) / rsv.read_size.val := by
      rw [hrsv_h, specReadSizedHashes_length, specLeafHashes_length,
          List.isEmpty_eq_false_iff.mpr hBne]
      simp only [Bool.false_eq_true, ↓reduceIte]
      rw [div_ceil_div_ceil_eq hK, ← hrs_eq]
    have hend_le_rsv : hend_idx.val ≤ rsv.hashes.val.length := by
      rw [hend_val, hrsv_len]
      exact Nat.div_le_div_right (by omega)
    have hend_chunks :
        hstart.val + (data.val.length + rsv.read_size.val - 1) / rsv.read_size.val ≤
          rsv.hashes.val.length := by
      have hdecomp := div_ceil_add_aligned (len := data.val.length) hwf.hread_pos hoff_rs
      omega
    obtain ⟨res, hvc, hiff⟩ :=
      verify_chunks_outer_loop_spec hsha rsv offset data hstart hph
        hwf hoff_rs hsum_usize hstart_val hhph_val hend_chunks
        ((data.val.length + rsv.read_size.val - 1) / rsv.read_size.val)
        0#usize 0#usize (by simp) (Nat.zero_le _) (by simp)
    have hres_ok : res = .Ok () := by
      apply hiff.mpr
      intro i _ hi_lt hk_in
      have hgrp_eq := chunkLeafHashes_eq_specLeafHashes_group sha256 B data.val
        offset.val rsv.read_size.val hwf.hread_pos hwf.hread_aligned hoff_rs
        hdne hin_blob hdata halign_end i hi_lt
      have hrs_get := specReadSizedHashes_getElem sha256 (rsv.read_size.val / 8192) hK
        rsv.bytes_per_hash (specLeafHashes sha256 B) (hstart.val + i)
        (by rw [← hrsv_h]; exact hk_in)
      rw [List.getElem_of_eq hrsv_h _, hrs_get, hgrp_eq, hstart_val, hhph_val]
    simp only [hdiv_start, hceil_end, bind_ok]
    by_cases hmult_v : v.val % rsv.read_size.val = 0
    · have hgt_false : ¬ (hend_idx > Slice.len rsv.hashes) :=
        not_usize_gt_of_le (by simpa using hend_le_rsv)
      simp only [hmult_v, decide_true, ↓reduceIte, hgt_false, ReadSizedMerkleVerifier.verify_chunks,
                 hvc, hres_ok]
    · have hend_eq_B : offset.val + data.val.length = B.length := by
        rcases halign_end with h0 | hB
        · exfalso; apply hmult_v; rw [hv_val]; exact h0
        · exact hB
      have hend_eq_len : hend_idx = Slice.len rsv.hashes := by
        apply UScalar.eq_of_val_eq
        rw [hend_val, Slice.len_val, Slice.length, hrsv_len, hend_eq_B]
      simp only [hmult_v, decide_false, Bool.false_eq_true, ↓reduceIte,
                 hend_eq_len, bne_self_eq_false, usize_gt_irrefl,
                 ReadSizedMerkleVerifier.verify_chunks, hvc, hres_ok]

-- ============================================================================
-- 4. `ReadSizedMerkleVerifier.verify` -- Collision-Resistant Soundness (P4)
-- ============================================================================

/-- Shared empty-slice soundness: if `verify` accepts `data = []`, then `(B = [] ∧ offset = 0)`
    or `HashCollision sha256` (using collision resistance alone, with no zero-preimage assumption). -/
private theorem verify_ok_empty_sound
    {H : Type} {sha256 : List U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List U8 → Prop}
    {B : List U8} {rsv : ReadSizedMerkleVerifier}
    {offset : Usize} {data : Slice U8}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hwf : ReadSizedWf rsv)
    (hK : 0 < rsv.read_size.val / 8192)
    (hrsv_h :
      rsv.hashes.val =
        specReadSizedHashes sha256 (rsv.read_size.val / 8192) hK rsv.bytes_per_hash
          (specLeafHashes sha256 B))
    (hdata_nil : data.val = [])
    (hok : ReadSizedMerkleVerifier.verify hInst rsv offset data = ok (.Ok ())) :
    (B = [] ∧ offset.val = 0) ∨ HashCollision sha256 := by
  unfold ReadSizedMerkleVerifier.verify at hok
  simp only [lift, bind_ok] at hok
  rcases usize_checked_add_spec_opt offset (Slice.len data) with ⟨hnone, _⟩ | ⟨_, hsome, _, _⟩
  · simp [hnone] at hok
  · simp only [hsome, usize_is_multiple_of_eq, bind_ok] at hok
    by_cases hoff : offset.val % rsv.read_size.val = 0
    · simp only [hoff, decide_true, ↓reduceIte] at hok
      obtain ⟨hph, hdiv_hph, hhph_val⟩ := uscalar_div_ok rsv.read_size BLOCK_SIZE (by simp)
      rw [BLOCK_SIZE_val] at hhph_val
      simp only [hdiv_hph, bind_ok] at hok
      have his_emp : core.slice.Slice.is_empty data = ok true := by
        simp [core.slice.Slice.is_empty, Slice.length, hdata_nil]
      rw [bind_ok_step his_emp rfl] at hok
      simp only [↓reduceIte] at hok
      by_cases hoff0 : offset != 0#usize
      · simp [hoff0] at hok
      · have hoff_eq0 : offset = 0#usize := by
          by_contra hne; exact hoff0 (bne_iff_ne.mpr hne)
        simp only [hoff0, Bool.false_eq_true, ↓reduceIte] at hok
        by_cases hlen1 : Slice.len rsv.hashes != 1#usize
        · simp [hlen1] at hok
        · have hrsv1 : rsv.hashes.val.length = 1 := by
            have hbeq : Slice.len rsv.hashes = 1#usize := by
              by_contra hne; exact hlen1 (bne_iff_ne.mpr hne)
            simpa using congrArg UScalar.val hbeq
          have hK_hph : 0 < hph.val := by rw [hhph_val]; exact hK
          simp only [hlen1, Bool.false_eq_true, ↓reduceIte] at hok
          rw [verify_empty_slice_branch_spec hsha rsv hph hwf hhph_val hK_hph hrsv1] at hok
          set h0 := specHashBlock sha256 [] 0#u64
          have hrsv0_eq :
              rsv.hashes.val[0]'(by omega) =
              specHashInternal sha256 (rsv.read_size.val / 8192) rsv.bytes_per_hash 1#u64
                (u64OfNat (0 * rsv.bytes_per_hash.val)) [h0] := by
            have hif := Result.ok_injective hok
            by_contra hne
            simp only [hne, ↓reduceIte, reduceCtorEq] at hif
          have hget0 := specReadSizedHashes_getElem sha256 (rsv.read_size.val / 8192) hK
            rsv.bytes_per_hash (specLeafHashes sha256 B) 0 (by rw [← hrsv_h]; omega)
          have hint_eq :
              specHashInternal sha256 (rsv.read_size.val / 8192) rsv.bytes_per_hash 1#u64
                (u64OfNat (0 * rsv.bytes_per_hash.val))
                (((specLeafHashes sha256 B).drop 0).take (rsv.read_size.val / 8192)) =
              specHashInternal sha256 (rsv.read_size.val / 8192) rsv.bytes_per_hash 1#u64
                (u64OfNat (0 * rsv.bytes_per_hash.val)) [h0] := by
            have h0_eq : rsv.hashes.val[0]'(by omega) =
                (specReadSizedHashes sha256 (rsv.read_size.val / 8192) hK rsv.bytes_per_hash
                  (specLeafHashes sha256 B))[0]'(by rw [← hrsv_h]; omega) :=
              List.getElem_of_eq hrsv_h _
            have hdrop_eq :
                (((specLeafHashes sha256 B).drop 0).take (rsv.read_size.val / 8192)) =
                (((specLeafHashes sha256 B).drop (0 * (rsv.read_size.val / 8192))).take
                  (rsv.read_size.val / 8192)) := by
              rw [Nat.zero_mul]
            exact (congrArg
              (specHashInternal sha256 (rsv.read_size.val / 8192) rsv.bytes_per_hash 1#u64
                (u64OfNat (0 * rsv.bytes_per_hash.val))) hdrop_eq).trans
              (hget0.symm.trans (h0_eq.symm.trans hrsv0_eq))
          rcases specHashInternal_same_id_eq_imp_padded_eq_or_collision
              (List.length_take_le _ _) (by simp only [List.length_singleton]; exact hK) hint_eq with hpad | hcol
          · by_cases hBnil : B = []
            · exact Or.inl ⟨hBnil, by simp [hoff_eq0]⟩
            · have hsl_pos := specLeafHashes_pos sha256 B
              have htake_pos :
                  0 < (((specLeafHashes sha256 B).drop 0).take (rsv.read_size.val / 8192)).length := by
                simp only [List.drop_zero, List.length_take]; omega
              have hidx0_l :
                  0 < ((((specLeafHashes sha256 B).drop 0).take (rsv.read_size.val / 8192)) ++
                    List.replicate (rsv.read_size.val / 8192 -
                      (((specLeafHashes sha256 B).drop 0).take (rsv.read_size.val / 8192)).length)
                      zeroHash).length := by
                rw [List.length_append]; omega
              have hget_pad :
                  ((((specLeafHashes sha256 B).drop 0).take (rsv.read_size.val / 8192)) ++
                    List.replicate (rsv.read_size.val / 8192 -
                      (((specLeafHashes sha256 B).drop 0).take (rsv.read_size.val / 8192)).length)
                      zeroHash)[0]'hidx0_l =
                  ([h0] ++ List.replicate (rsv.read_size.val / 8192 - [h0].length) zeroHash)[0]'
                    (by simp) :=
                List.getElem_of_eq hpad _
              rw [List.getElem_append_left htake_pos, List.getElem_take,
                  List.getElem_append_left (by simp : 0 < [h0].length)] at hget_pad
              simp only [List.drop_zero, List.getElem_singleton] at hget_pad
              obtain ⟨hB0, hB0_pos, hB0_le⟩ :=
                specLeafHashes_getElem_of_nonempty sha256 B hBnil 0 hsl_pos
              have hu0_eq : u64OfNat 0 = 0#u64 := UScalar.eq_of_val_eq (by simp)
              rw [hB0, Nat.zero_mul, hu0_eq] at hget_pad
              rcases specHashBlock_eq_imp_eq_or_collision hB0_le (by simp [BLOCK_BYTES])
                  (by simp) (by simp) hget_pad with ⟨hblk_eq, _⟩ | hcol
              · rw [hblk_eq] at hB0_pos; simp at hB0_pos
              · exact Or.inr hcol
          · exact Or.inr hcol
    · simp only [hoff, decide_false, Bool.false_eq_true, ↓reduceIte] at hok
      cases Result.ok_injective hok

/-- Shared non-empty control-flow extraction: if `verify` accepts non-empty `data`, then
    `offset % read_size = 0`, `offset + data.len ≤ Usize.max`, all accessed chunk indices lie
    in `rsv.hashes`, and every chunk digest matches its stored level-1 digest. -/
private theorem verify_ok_nonempty_extract
    {H : Type} {sha256 : List U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List U8 → Prop}
    {rsv : ReadSizedMerkleVerifier} {offset : Usize} {data : Slice U8}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hwf : ReadSizedWf rsv)
    (hdne : data.val ≠ [])
    (hok : ReadSizedMerkleVerifier.verify hInst rsv offset data = ok (.Ok ())) :
    offset.val % rsv.read_size.val = 0 ∧
    offset.val + data.val.length ≤ Usize.max ∧
    offset.val / rsv.read_size.val +
      (data.val.length + rsv.read_size.val - 1) / rsv.read_size.val ≤ rsv.hashes.val.length ∧
    ∀ i < (data.val.length + rsv.read_size.val - 1) / rsv.read_size.val,
      ∀ hk : offset.val / rsv.read_size.val + i < rsv.hashes.val.length,
        rsv.hashes.val[offset.val / rsv.read_size.val + i]'hk =
          specHashInternal sha256 (rsv.read_size.val / 8192) rsv.bytes_per_hash 1#u64
            (u64OfNat ((offset.val / rsv.read_size.val + i) * rsv.bytes_per_hash.val))
            (chunkLeafHashes sha256 (offset.val + i * rsv.read_size.val)
              ((data.val.drop (i * rsv.read_size.val)).take rsv.read_size.val)) := by
  have hdpos : 0 < data.val.length := List.length_pos_iff.mpr hdne
  unfold ReadSizedMerkleVerifier.verify at hok
  simp only [lift, bind_ok] at hok
  rcases usize_checked_add_spec_opt offset (Slice.len data) with
    ⟨hnone, _⟩ | ⟨v, hsome, hsum, hv_val⟩
  · simp [hnone] at hok
  · simp only [hsome, usize_is_multiple_of_eq, bind_ok] at hok
    simp only [Slice.len_val, Slice.length] at hsum hv_val
    by_cases hoff : offset.val % rsv.read_size.val = 0
    · simp only [hoff, decide_true, ↓reduceIte] at hok
      obtain ⟨hph, hdiv_hph, hhph_val⟩ := uscalar_div_ok rsv.read_size BLOCK_SIZE (by simp)
      rw [BLOCK_SIZE_val] at hhph_val
      have his_emp : core.slice.Slice.is_empty data = ok false := by
        simp [core.slice.Slice.is_empty, Slice.length, Nat.ne_of_gt hdpos]
      simp only [hdiv_hph, bind_ok] at hok
      rw [bind_ok_step his_emp rfl] at hok
      simp only [Bool.false_eq_true, ↓reduceIte] at hok
      obtain ⟨hstart, hdiv_start, hstart_val⟩ :=
        uscalar_div_ok offset rsv.read_size (by have := hwf.hread_pos; omega)
      obtain ⟨hend_idx, hceil_end, hend_val⟩ :=
        div_ceil_usize_spec v rsv.read_size hwf.hread_pos
      rw [hv_val] at hend_val
      simp only [hdiv_start, hceil_end, bind_ok] at hok
      have hextract :
          hend_idx.val ≤ rsv.hashes.val.length ∧
          ReadSizedMerkleVerifier.verify_chunks hInst rsv offset data hstart hph = ok (.Ok ()) := by
        by_cases hmult_v : v.val % rsv.read_size.val = 0
        · simp only [hmult_v, decide_true, ↓reduceIte] at hok
          by_cases hgt : hend_idx > Slice.len rsv.hashes
          · simp [hgt] at hok
          · simp only [hgt, ↓reduceIte] at hok
            have hle : hend_idx.val ≤ rsv.hashes.val.length := by
              by_contra hc
              exact hgt (usize_lt_of_lt (by simpa using Nat.lt_of_not_ge hc))
            exact ⟨hle, hok⟩
        · simp only [hmult_v, decide_false, Bool.false_eq_true, ↓reduceIte] at hok
          by_cases hne_end : hend_idx != Slice.len rsv.hashes
          · simp [hne_end] at hok
          · have hend_eq : hend_idx = Slice.len rsv.hashes := by
              by_contra hc; exact hne_end (bne_iff_ne.mpr hc)
            simp only [hend_eq, bne_self_eq_false, usize_gt_irrefl, Bool.false_eq_true, ↓reduceIte] at hok
            have hle : hend_idx.val ≤ rsv.hashes.val.length := by
              have hv_eq := congrArg UScalar.val hend_eq
              simpa using le_of_eq hv_eq
            exact ⟨hle, hok⟩
      rcases hextract with ⟨hle_h, hvc_ok⟩
      have hend_chunks :
          hstart.val + (data.val.length + rsv.read_size.val - 1) / rsv.read_size.val ≤
            rsv.hashes.val.length := by
        have hdecomp := div_ceil_add_aligned (len := data.val.length) hwf.hread_pos hoff
        omega
      obtain ⟨res, hvc, hiff⟩ :=
        verify_chunks_outer_loop_spec hsha rsv offset data hstart hph
          hwf hoff hsum hstart_val hhph_val hend_chunks
          ((data.val.length + rsv.read_size.val - 1) / rsv.read_size.val)
          0#usize 0#usize (by simp) (Nat.zero_le _) (by simp)
      have hres_ok : res = .Ok () := Result.ok_injective (hvc.symm.trans hvc_ok)
      have hall_chunks := hiff.mp hres_ok
      refine ⟨hoff, hsum, ?_, ?_⟩
      · rwa [← hstart_val]
      · intro i hi_lt hk
        have hk' : hstart.val + i < rsv.hashes.val.length := by rwa [hstart_val]
        have hstep := hall_chunks i (Nat.zero_le _) hi_lt hk'
        simpa [hstart_val, hhph_val] using hstep
    · simp only [hoff, decide_false, Bool.false_eq_true, ↓reduceIte] at hok
      cases Result.ok_injective hok

/-- Shared leaf-level assembly: once every chunk's computed leaf list equals the corresponding
    slice of `specLeafHashes sha256 B`, `blocks_match_leaves_sound` yields byte-exact slice
    equality or `HashCollision sha256`. -/
private theorem all_chunk_groups_eq_imp_slice_or_collision
    {sha256 : List U8 → Hash} {B : List U8} {rsv : ReadSizedMerkleVerifier}
    {offset : Usize} {data : Slice U8}
    (hwf : ReadSizedWf rsv)
    (hK : 0 < rsv.read_size.val / 8192)
    (hoff : offset.val % rsv.read_size.val = 0)
    (hdne : data.val ≠ [])
    (hsum : offset.val + data.val.length ≤ Usize.max)
    (hall_grp :
      ∀ i < (data.val.length + rsv.read_size.val - 1) / rsv.read_size.val,
        ((specLeafHashes sha256 B).drop
          ((offset.val / rsv.read_size.val + i) * (rsv.read_size.val / 8192))).take
          (rsv.read_size.val / 8192) =
        chunkLeafHashes sha256 (offset.val + i * rsv.read_size.val)
          ((data.val.drop (i * rsv.read_size.val)).take rsv.read_size.val)) :
    (offset.val + data.val.length ≤ B.length ∧
     data.val = (B.drop offset.val).take data.val.length) ∨
    HashCollision sha256 := by
  set K := rsv.read_size.val / 8192
  set hstart := offset.val / rsv.read_size.val
  set num_chunks := (data.val.length + rsv.read_size.val - 1) / rsv.read_size.val
  have hrs_eq_K : rsv.read_size.val = K * 8192 :=
    (Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hwf.hread_aligned)).symm
  have hoff_align8 : offset.val % 8192 = 0 :=
    Nat.mod_eq_zero_of_dvd
      (Nat.dvd_trans (Nat.dvd_of_mod_eq_zero hwf.hread_aligned)
        (Nat.dvd_of_mod_eq_zero hoff))
  have hsum64 : offset.val + data.val.length ≤ U64.max := by
    have ⟨_, hu64⟩ := usize_bounds; omega
  have hoff_div8 : offset.val / 8192 = hstart * K := by
    have hoff_eq : offset.val = hstart * rsv.read_size.val := by
      dsimp [hstart]; rw [Nat.mul_comm]
      exact (Nat.mul_div_cancel' (Nat.dvd_of_mod_eq_zero hoff)).symm
    rw [hoff_eq, hrs_eq_K, ← Nat.mul_assoc, Nat.mul_div_cancel _ (by decide : 0 < 8192)]
  have hall_leaves :
      ∀ k < (data.val.length + 8191) / 8192,
        ∃ hk : offset.val / 8192 + k < (specLeafHashes sha256 B).length,
          (specLeafHashes sha256 B)[offset.val / 8192 + k]'hk =
            specHashBlock sha256 ((data.val.drop (k * 8192)).take 8192)
              (u64OfNat ((offset.val / 8192 + k) * 8192)) := by
    intro k hk_lt
    have hk_mul_lt : k * 8192 < data.val.length :=
      mul_lt_of_lt_div_ceil (k := 8192) (by decide) hk_lt
    set i := k / K
    set j := k % K
    have hj_lt_K : j < K := Nat.mod_lt k hK
    have hk_decomp : i * K + j = k := by
      have h1 := Nat.div_add_mod k K
      rw [Nat.mul_comm] at h1; exact h1
    have hi_mul_lt : i * rsv.read_size.val < data.val.length := by
      have hle : i * (K * 8192) ≤ k * 8192 := by
        rw [← hk_decomp]; calc i * (K * 8192)
            = (i * K) * 8192 := (Nat.mul_assoc _ _ _).symm
          _ ≤ (i * K + j) * 8192 := Nat.mul_le_mul_right 8192 (Nat.le_add_right _ _)
      rw [hrs_eq_K]; omega
    have hi_lt : i < num_chunks := lt_div_ceil_of_mul_lt hwf.hread_pos hi_mul_lt
    have hgrp_i := hall_grp i hi_lt
    have hj_mul_lt :
        j * 8192 < ((data.val.drop (i * rsv.read_size.val)).take rsv.read_size.val).length := by
      rw [List.length_take, List.length_drop, hrs_eq_K]
      have h1 : j * 8192 < K * 8192 := Nat.mul_lt_mul_of_pos_right hj_lt_K (by decide)
      have h2 : i * (K * 8192) + j * 8192 = k * 8192 := by
        rw [← hk_decomp]; ring
      exact Nat.lt_min.mpr ⟨h1, Nat.lt_sub_of_add_lt (by rw [Nat.add_comm, h2]; exact hk_mul_lt)⟩
    have hj_lt_comp :
        j < (chunkLeafHashes sha256 (offset.val + i * rsv.read_size.val)
              ((data.val.drop (i * rsv.read_size.val)).take rsv.read_size.val)).length := by
      rw [chunkLeafHashes_length]
      exact lt_div_ceil_8192_of_mul_lt hj_mul_lt
    have hj_lt_grp :
        j < (((specLeafHashes sha256 B).drop ((hstart + i) * K)).take K).length := by
      rw [hgrp_i]; exact hj_lt_comp
    have hidx_eq : (hstart + i) * K + j = offset.val / 8192 + k := by
      rw [hoff_div8, ← hk_decomp]; ring
    have hk_sl : offset.val / 8192 + k < (specLeafHashes sha256 B).length := by
      rw [List.length_take, List.length_drop] at hj_lt_grp
      omega
    refine ⟨hk_sl, ?_⟩
    have hget_eq :
        (((specLeafHashes sha256 B).drop ((hstart + i) * K)).take K)[j]'hj_lt_grp =
        (chunkLeafHashes sha256 (offset.val + i * rsv.read_size.val)
          ((data.val.drop (i * rsv.read_size.val)).take rsv.read_size.val))[j]'hj_lt_comp :=
      List.getElem_of_eq hgrp_i _
    rw [List.getElem_take, List.getElem_drop, chunkLeafHashes_getElem] at hget_eq
    have hsub :
        (((data.val.drop (i * rsv.read_size.val)).take rsv.read_size.val).drop (j * 8192)).take 8192 =
        (data.val.drop (k * 8192)).take 8192 := by
      rw [hrs_eq_K, drop_take_subblock data.val i j K hj_lt_K, hk_decomp]
    have hoff_k_eq :
        offset.val + i * rsv.read_size.val + j * 8192 =
        (offset.val / 8192 + k) * 8192 := by
      have hoff_eq8 : (offset.val / 8192) * 8192 = offset.val :=
        Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hoff_align8)
      rw [hrs_eq_K, Nat.add_mul, hoff_div8, ← hk_decomp]
      have : (hstart * K) * 8192 = offset.val := by rw [← hoff_div8, hoff_eq8]
      calc offset.val + i * (K * 8192) + j * 8192
          = offset.val + (i * K + j) * 8192 := by ring
        _ = (hstart * K) * 8192 + (i * K + j) * 8192 := by rw [this]
    rw [hsub, hoff_k_eq] at hget_eq
    simpa [hidx_eq] using hget_eq
  exact blocks_match_leaves_sound sha256 B data.val offset.val hoff_align8 hdne hsum64 hall_leaves

/-- Unconditional soundness of `ReadSizedMerkleVerifier.verify` (without any external blob-size bound). -/
theorem read_sized_verifier_verify_sound_of_wf
    {H : Type} {sha256 : List U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List U8 → Prop}
    {B : List U8} {rsv : ReadSizedMerkleVerifier}
    {offset : Usize} {data : Slice U8}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hwf : ReadSizedWf rsv)
    (hK : 0 < rsv.read_size.val / 8192)
    (hrsv_h :
      rsv.hashes.val =
        specReadSizedHashes sha256 (rsv.read_size.val / 8192) hK rsv.bytes_per_hash
          (specLeafHashes sha256 B))
    (hok : ReadSizedMerkleVerifier.verify hInst rsv offset data = ok (.Ok ())) :
    ((data.val = [] → B = [] ∧ offset.val = 0) ∧
     (data.val ≠ [] →
        offset.val + data.val.length ≤ B.length ∧
        data.val = (B.drop offset.val).take data.val.length)) ∨
    HashCollision sha256 ∨ HashZeroPreimage sha256 := by
  by_cases hdata_nil : data.val = []
  · rcases verify_ok_empty_sound hsha hwf hK hrsv_h hdata_nil hok with hemp_ok | hcol
    · exact Or.inl ⟨fun _ => hemp_ok, fun hne => absurd hdata_nil hne⟩
    · exact Or.inr (Or.inl hcol)
  · obtain ⟨hoff, hsum, hend_chunks, hall_chunks⟩ :=
      verify_ok_nonempty_extract hsha hwf hdata_nil hok
    set K := rsv.read_size.val / 8192
    set hstart := offset.val / rsv.read_size.val
    set num_chunks := (data.val.length + rsv.read_size.val - 1) / rsv.read_size.val
    have hrs_eq_K : rsv.read_size.val = K * 8192 :=
      (Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hwf.hread_aligned)).symm
    by_cases hbad_chunk :
        ∃ i < num_chunks,
          ((specLeafHashes sha256 B).drop ((hstart + i) * K)).take K ≠
          chunkLeafHashes sha256 (offset.val + i * rsv.read_size.val)
            ((data.val.drop (i * rsv.read_size.val)).take rsv.read_size.val)
    · right
      obtain ⟨i, hi_lt, hneq_i⟩ := hbad_chunk
      have hk_in : hstart + i < rsv.hashes.val.length := by omega
      have heq_i := hall_chunks i hi_lt hk_in
      have hrs_get := specReadSizedHashes_getElem sha256 K hK rsv.bytes_per_hash
        (specLeafHashes sha256 B) (hstart + i) (by rw [← hrsv_h]; exact hk_in)
      have hint_eq :
          specHashInternal sha256 K rsv.bytes_per_hash 1#u64
            (u64OfNat ((hstart + i) * rsv.bytes_per_hash.val))
            (((specLeafHashes sha256 B).drop ((hstart + i) * K)).take K) =
          specHashInternal sha256 K rsv.bytes_per_hash 1#u64
            (u64OfNat ((hstart + i) * rsv.bytes_per_hash.val))
            (chunkLeafHashes sha256 (offset.val + i * rsv.read_size.val)
              ((data.val.drop (i * rsv.read_size.val)).take rsv.read_size.val)) := by
        have h0_eq : rsv.hashes.val[hstart + i]'hk_in =
            (specReadSizedHashes sha256 K hK rsv.bytes_per_hash
              (specLeafHashes sha256 B))[hstart + i]'(by rw [← hrsv_h]; exact hk_in) :=
          List.getElem_of_eq hrsv_h _
        rw [← hrs_get, ← h0_eq, heq_i]
      have hcomp_le_K :
          (chunkLeafHashes sha256 (offset.val + i * rsv.read_size.val)
            ((data.val.drop (i * rsv.read_size.val)).take rsv.read_size.val)).length ≤ K := by
        rw [chunkLeafHashes_length]
        exact div_ceil_8192_le_of_le_mul
          (by rw [← hrs_eq_K]; exact List.length_take_le _ _)
      rcases specHashInternal_same_id_eq_imp_padded_eq_or_collision
          (List.length_take_le _ _) hcomp_le_K hint_eq with hpad | hcol
      · rcases padded_hashes_eq_imp_eq_or_zero_hash
            (List.length_take_le _ _) hcomp_le_K hpad with heq_lists | hz_grp | hz_comp
        · exact absurd heq_lists hneq_i
        · right
          have hz_sl : zeroHash ∈ specLeafHashes sha256 B :=
            List.mem_of_mem_drop (List.mem_of_mem_take hz_grp)
          exact mem_specLeafHashes_eq_zeroHash_imp_zeroPreimage hz_sl
        · right
          exact mem_chunkLeafHashes_eq_zeroHash_imp_zeroPreimage hz_comp
      · exact Or.inl hcol
    · have hall_grp :
          ∀ i < num_chunks,
            ((specLeafHashes sha256 B).drop ((hstart + i) * K)).take K =
            chunkLeafHashes sha256 (offset.val + i * rsv.read_size.val)
              ((data.val.drop (i * rsv.read_size.val)).take rsv.read_size.val) := by
        intro i hi_lt
        by_contra hneq; exact hbad_chunk ⟨i, hi_lt, hneq⟩
      rcases all_chunk_groups_eq_imp_slice_or_collision hwf hK hoff hdata_nil hsum hall_grp with
        ⟨hin_B, hslice⟩ | hcol
      · exact Or.inl ⟨fun hc => absurd hc hdata_nil, fun _ => ⟨hin_B, hslice⟩⟩
      · exact Or.inr (Or.inl hcol)

/-- Size-bounded soundness of `ReadSizedMerkleVerifier.verify`: when `offset + data.len ≤ B.len`
    and the `read_size` end-alignment rule holds (`fxblob` contract), soundness holds up to
    `HashCollision sha256` alone (with zero `HashZeroPreimage` assumption). -/
theorem read_sized_verifier_verify_sound_bounded_of_wf
    {H : Type} {sha256 : List U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List U8 → Prop}
    {B : List U8} {rsv : ReadSizedMerkleVerifier}
    {offset : Usize} {data : Slice U8}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hwf : ReadSizedWf rsv)
    (hK : 0 < rsv.read_size.val / 8192)
    (hrsv_h :
      rsv.hashes.val =
        specReadSizedHashes sha256 (rsv.read_size.val / 8192) hK rsv.bytes_per_hash
          (specLeafHashes sha256 B))
    (hin_blob : offset.val + data.val.length ≤ B.length)
    (halign_end :
      data.val ≠ [] →
        (offset.val + data.val.length) % rsv.read_size.val = 0 ∨
        offset.val + data.val.length = B.length)
    (hok : ReadSizedMerkleVerifier.verify hInst rsv offset data = ok (.Ok ())) :
    ((data.val = [] → B = [] ∧ offset.val = 0) ∧
     (data.val ≠ [] → data.val = (B.drop offset.val).take data.val.length)) ∨
    HashCollision sha256 := by
  by_cases hdata_nil : data.val = []
  · rcases verify_ok_empty_sound hsha hwf hK hrsv_h hdata_nil hok with hemp_ok | hcol
    · exact Or.inl ⟨fun _ => hemp_ok, fun hne => absurd hdata_nil hne⟩
    · exact Or.inr hcol
  · obtain ⟨hoff, hsum, hend_chunks, hall_chunks⟩ :=
      verify_ok_nonempty_extract hsha hwf hdata_nil hok
    set K := rsv.read_size.val / 8192
    set hstart := offset.val / rsv.read_size.val
    set num_chunks := (data.val.length + rsv.read_size.val - 1) / rsv.read_size.val
    have hrs_eq_K : rsv.read_size.val = K * 8192 :=
      (Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hwf.hread_aligned)).symm
    set canonData := (B.drop offset.val).take data.val.length
    have hcanon_len : canonData.length = data.val.length :=
      List.length_take_of_le (by rw [List.length_drop]; omega)
    have hcanon_ne : canonData ≠ [] :=
      List.ne_nil_of_length_pos (by rw [hcanon_len]; exact List.length_pos_iff.mpr hdata_nil)
    by_cases hbad_chunk :
        ∃ i < num_chunks,
          ((specLeafHashes sha256 B).drop ((hstart + i) * K)).take K ≠
          chunkLeafHashes sha256 (offset.val + i * rsv.read_size.val)
            ((data.val.drop (i * rsv.read_size.val)).take rsv.read_size.val)
    · right
      obtain ⟨i, hi_lt, hneq_i⟩ := hbad_chunk
      have hk_in : hstart + i < rsv.hashes.val.length := by omega
      have heq_i := hall_chunks i hi_lt hk_in
      have hrs_get := specReadSizedHashes_getElem sha256 K hK rsv.bytes_per_hash
        (specLeafHashes sha256 B) (hstart + i) (by rw [← hrsv_h]; exact hk_in)
      have hint_eq :
          specHashInternal sha256 K rsv.bytes_per_hash 1#u64
            (u64OfNat ((hstart + i) * rsv.bytes_per_hash.val))
            (((specLeafHashes sha256 B).drop ((hstart + i) * K)).take K) =
          specHashInternal sha256 K rsv.bytes_per_hash 1#u64
            (u64OfNat ((hstart + i) * rsv.bytes_per_hash.val))
            (chunkLeafHashes sha256 (offset.val + i * rsv.read_size.val)
              ((data.val.drop (i * rsv.read_size.val)).take rsv.read_size.val)) := by
        have h0_eq : rsv.hashes.val[hstart + i]'hk_in =
            (specReadSizedHashes sha256 K hK rsv.bytes_per_hash
              (specLeafHashes sha256 B))[hstart + i]'(by rw [← hrsv_h]; exact hk_in) :=
          List.getElem_of_eq hrsv_h _
        rw [← hrs_get, ← h0_eq, heq_i]
      have hcomp_le_K :
          (chunkLeafHashes sha256 (offset.val + i * rsv.read_size.val)
            ((data.val.drop (i * rsv.read_size.val)).take rsv.read_size.val)).length ≤ K := by
        rw [chunkLeafHashes_length]
        exact div_ceil_8192_le_of_le_mul
          (by rw [← hrs_eq_K]; exact List.length_take_le _ _)
      have hgrp_eq_canon := chunkLeafHashes_eq_specLeafHashes_group sha256 B canonData
        offset.val rsv.read_size.val hwf.hread_pos hwf.hread_aligned hoff hcanon_ne
        (by rw [hcanon_len]; exact hin_blob)
        (by simp [canonData, hcanon_len])
        (by rw [hcanon_len]; exact halign_end hdata_nil)
        i (by rw [hcanon_len]; exact hi_lt)
      have hlen_grp_comp :
          (((specLeafHashes sha256 B).drop ((hstart + i) * K)).take K).length =
          (chunkLeafHashes sha256 (offset.val + i * rsv.read_size.val)
            ((data.val.drop (i * rsv.read_size.val)).take rsv.read_size.val)).length := by
        rw [← hgrp_eq_canon]
        simp only [chunkLeafHashes_length, List.length_take, List.length_drop]
        rw [hcanon_len]
      rcases specHashInternal_same_id_eq_imp_padded_eq_or_collision
          (List.length_take_le _ _) hcomp_le_K hint_eq with hpad | hcol
      · exact absurd (List.append_inj hpad hlen_grp_comp).1 hneq_i
      · exact hcol
    · have hall_grp :
          ∀ i < num_chunks,
            ((specLeafHashes sha256 B).drop ((hstart + i) * K)).take K =
            chunkLeafHashes sha256 (offset.val + i * rsv.read_size.val)
              ((data.val.drop (i * rsv.read_size.val)).take rsv.read_size.val) := by
        intro i hi_lt
        by_contra hneq; exact hbad_chunk ⟨i, hi_lt, hneq⟩
      rcases all_chunk_groups_eq_imp_slice_or_collision hwf hK hoff hdata_nil hsum hall_grp with
        ⟨_, hslice⟩ | hcol
      · exact Or.inl ⟨fun hc => absurd hc hdata_nil, fun _ => hslice⟩
      · exact Or.inr hcol

end FuchsiaMerkle
