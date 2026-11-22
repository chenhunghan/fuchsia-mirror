-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Proof.VerifierCommon

set_option maxRecDepth 8192
set_option maxHeartbeats 800000

/-!
# `ReadSizedMerkleVerifier.verify_aligned` Proofs

Machine-checked proofs of totality / panic-freedom (P1), completeness (P3),
unconditional collision/zero-preimage soundness (P4), and size-bounded
collision-only soundness (P4) for `ReadSizedMerkleVerifier.verify_aligned`
over any `ValidSha256State` and `RefinesAlignedByteSource`.
-/

namespace FuchsiaMerkle

open Aeneas Aeneas.Std Result
open fuchsia_merkle_core hiding Hash
open FuchsiaMerkle.Assumptions
open FuchsiaMerkle.Spec

-- ============================================================================
-- 1. Arithmetic & Ceiling-Division Helpers
-- ============================================================================

private theorem cast_u64_u32_val (x : Std.U32) :
    (UScalar.cast .U64 x).val = x.val := by
  rw [UScalar.cast_val_eq]
  have hx : x.val < 4294967296 := by
    have h1 := u32_val_le_max x
    have h2 := u32_max_val_eq
    omega
  have p64 : 2 ^ 64 = 18446744073709551616 := by decide
  simp only [UScalarTy.numBits, p64]
  omega

private theorem cast_u64_usize_val (x : Std.Usize) :
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

-- ============================================================================
-- 2. Aligned Chunk Leaf Hashes & Loop Simulation Specs
-- ============================================================================

/-- Pure specification of the leaf hashes hashed within one `read_size` chunk of `verify_aligned`. -/
def alignedChunkLeafHashes
    (sha256 : List Std.U8 → Hash) (offset : Nat) (data : List Std.U8)
    (len ulen chunkPos numBlocks : Nat) : List Hash :=
  (List.range numBlocks).map fun j =>
    let bytePos := chunkPos + j * 8192
    let bbuf := min 8192 (len - bytePos)
    let bulen := min 8192 (ulen - bytePos)
    sha256 (specIdentity (u32OfNat bulen) 0#u64 (u64OfNat (offset + bytePos)) ++
            (data.drop bytePos).take bbuf ++
            specZeroPad (BLOCK_BYTES - bbuf))

theorem alignedChunkLeafHashes_length
    (sha256 : List Std.U8 → Hash) (offset : Nat) (data : List Std.U8)
    (len ulen chunkPos numBlocks : Nat) :
    (alignedChunkLeafHashes sha256 offset data len ulen chunkPos numBlocks).length = numBlocks := by
  simp [alignedChunkLeafHashes]

theorem alignedChunkLeafHashes_getElem
    (sha256 : List Std.U8 → Hash) (offset : Nat) (data : List Std.U8)
    (len ulen chunkPos numBlocks j : Nat)
    (hj : j < (alignedChunkLeafHashes sha256 offset data len ulen chunkPos numBlocks).length) :
    (alignedChunkLeafHashes sha256 offset data len ulen chunkPos numBlocks)[j]'hj =
      sha256 (specIdentity (u32OfNat (min 8192 (ulen - (chunkPos + j * 8192)))) 0#u64
                (u64OfNat (offset + (chunkPos + j * 8192))) ++
              (data.drop (chunkPos + j * 8192)).take (min 8192 (len - (chunkPos + j * 8192))) ++
              specZeroPad (BLOCK_BYTES - min 8192 (len - (chunkPos + j * 8192)))) := by
  simp [alignedChunkLeafHashes]

theorem mem_alignedChunkLeafHashes_eq_zeroHash_imp_zeroPreimage
    {sha256 : List Std.U8 → Hash} {offset : Nat} {data : List Std.U8}
    {len ulen chunkPos numBlocks : Nat}
    (hmem : zeroHash ∈ alignedChunkLeafHashes sha256 offset data len ulen chunkPos numBlocks) :
    HashZeroPreimage sha256 := by
  obtain ⟨j, hj, hget⟩ := List.getElem_of_mem hmem
  rw [alignedChunkLeafHashes_getElem] at hget
  exact ⟨_, hget⟩

theorem verify_aligned_chunks_inner_loop_spec
    {H Src : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {srcInst : AlignedByteSource Src} {src : Src} {slice : Slice Std.U8}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice)
    (offset : Std.Usize) (len unaligned_len chunk_pos num_blocks : Std.Usize)
    (hlen_eq : len.val = slice.val.length)
    (hpage : len.val % 4096 = 0)
    (hoff_align8 : offset.val % 8192 = 0)
    (hcpos_align8 : chunk_pos.val % 8192 = 0)
    (hule : unaligned_len.val ≤ len.val)
    (hsum_usize : offset.val + len.val ≤ Usize.max)
    (hnb_lt : ∀ k < num_blocks.val, chunk_pos.val + k * 8192 < unaligned_len.val) :
    ∀ (rem : Nat) (hasher : H) (acc : List Std.U8) (j : Std.Usize),
      j.val ≤ num_blocks.val →
      num_blocks.val - j.val = rem →
      R_H hasher acc →
      acc.length + (num_blocks.val - j.val) * 32 ≤ SHA256_MAX_BYTES →
      ∃ hasher',
        ReadSizedMerkleVerifier.verify_aligned_chunks_loop0_loop0 hInst srcInst
          offset src len unaligned_len chunk_pos num_blocks hasher j = ok hasher' ∧
        R_H hasher' (acc ++ concatHashes
          ((alignedChunkLeafHashes sha256 offset.val slice.val len.val unaligned_len.val
            chunk_pos.val num_blocks.val).drop j.val))
  | 0, hasher, acc, j, _, _, hrel, _ => by
    have hj_eq : j.val = num_blocks.val := by omega
    have hnlt : ¬ (j < num_blocks) := not_usize_lt_of_ge (by omega)
    have hbody :
        ReadSizedMerkleVerifier.verify_aligned_chunks_loop0_loop0.body hInst srcInst
          offset src len unaligned_len chunk_pos num_blocks hasher j = ok (.done hasher) := by
      unfold ReadSizedMerkleVerifier.verify_aligned_chunks_loop0_loop0.body
      simp only [hnlt, ↓reduceIte]
    have hdrop_nil :
        (alignedChunkLeafHashes sha256 offset.val slice.val len.val unaligned_len.val
          chunk_pos.val num_blocks.val).drop j.val = [] := by
      apply List.drop_eq_nil_of_le
      rw [alignedChunkLeafHashes_length]; omega
    refine ⟨hasher, ?_, ?_⟩
    · unfold ReadSizedMerkleVerifier.verify_aligned_chunks_loop0_loop0
      exact loop_done _ (hasher, j) _ hbody
    · simpa [hdrop_nil, concatHashes] using hrel
  | rem + 1, hasher, acc, j, _, _, hrel, hmax => by
    have hj_lt_nat : j.val < num_blocks.val := by omega
    have hj_lt : j < num_blocks := usize_lt_of_lt hj_lt_nat
    have hbp_lt_u : chunk_pos.val + j.val * 8192 < unaligned_len.val := hnb_lt j.val hj_lt_nat
    have hbp_lt_l : chunk_pos.val + j.val * 8192 < len.val := lt_of_lt_of_le hbp_lt_u hule
    obtain ⟨jmul, hmul_j, hjmul_val⟩ := uscalar_mul_ok j BLOCK_SIZE (by
      rw [uscalar_max_usize, BLOCK_SIZE_val]; omega)
    rw [BLOCK_SIZE_val] at hjmul_val
    obtain ⟨byte_pos, hadd_bp, hbp_val⟩ := uscalar_add_ok chunk_pos jmul (by
      rw [uscalar_max_usize, hjmul_val]; omega)
    rw [hjmul_val] at hbp_val
    obtain ⟨rem_len, hsub_len, hrem_len⟩ := uscalar_sub_ok len byte_pos (by omega)
    obtain ⟨bbuf, hmin_buf, hbbuf_val⟩ := min_usize_spec BLOCK_SIZE rem_len
    rw [BLOCK_SIZE_val, hrem_len, hbp_val] at hbbuf_val
    obtain ⟨rem_ulen, hsub_ulen, hrem_ulen⟩ := uscalar_sub_ok unaligned_len byte_pos (by omega)
    obtain ⟨bulen, hmin_u, hbulen_val_raw⟩ := min_usize_spec bbuf rem_ulen
    have hbulen_val :
        bulen.val = min 8192 (unaligned_len.val - (chunk_pos.val + j.val * 8192)) := by
      rw [hbulen_val_raw, hbbuf_val, hrem_ulen, hbp_val]; omega
    have hu64_sum : offset.val + byte_pos.val ≤ U64.max := by
      have ⟨_, hu64⟩ := usize_bounds; omega
    obtain ⟨boff64, hadd64, hboff64_val⟩ :=
      uscalar_add_ok (UScalar.cast .U64 offset) (UScalar.cast .U64 byte_pos) (by
        rw [uscalar_max_u64, cast_u64_usize_val, cast_u64_usize_val]; exact hu64_sum)
    have hboff_nat_le : offset.val + (chunk_pos.val + j.val * 8192) ≤ U64.max := by omega
    have hboff64_eq : boff64 = u64OfNat (offset.val + (chunk_pos.val + j.val * 8192)) := by
      apply UScalar.eq_of_val_eq
      rw [hboff64_val, cast_u64_usize_val, cast_u64_usize_val, hbp_val,
          u64OfNat_val_of_le _ hboff_nat_le]
    have hboff64_mod : boff64.val % BLOCK_BYTES = 0 := by
      rw [hboff64_eq, u64OfNat_val_of_le _ hboff_nat_le]; unfold BLOCK_BYTES; omega
    have hbuf_cases : bbuf.val = BLOCK_BYTES ∨ bbuf.val = MIN_PAGE_BYTES := by
      rw [hbbuf_val]
      exact aligned_buffer_len_cases hpage (by omega) hbp_lt_l
    have hhba := hsrc.hash_aligned_block_eq boff64 byte_pos bbuf bulen
      (by unfold BLOCK_BYTES; omega) hbuf_cases (by omega) (by omega) (by omega) hboff64_mod
    rw [hboff64_eq, hbp_val, hbbuf_val, hbulen_val] at hhba
    set leaf_h :=
      sha256 (specIdentity (u32OfNat (min 8192 (unaligned_len.val - (chunk_pos.val + j.val * 8192)))) 0#u64
                (u64OfNat (offset.val + (chunk_pos.val + j.val * 8192))) ++
              (slice.val.drop (chunk_pos.val + j.val * 8192)).take
                (min 8192 (len.val - (chunk_pos.val + j.val * 8192))) ++
              specZeroPad (BLOCK_BYTES - min 8192 (len.val - (chunk_pos.val + j.val * 8192))))
    have hslice_len : leaf_h.to_slice.val.length = 32 := by simp [Array.val_to_slice]
    obtain ⟨hasher_mid, hupd, hrel_mid⟩ := hsha.update_ok hasher acc leaf_h.to_slice hrel
      (by rw [hslice_len]; omega)
    rw [Array.val_to_slice] at hrel_mid
    have hj_le_mul : j.val ≤ j.val * 8192 := Nat.le_mul_of_pos_right _ (by decide)
    obtain ⟨j1, hadd_j1, hj1_val⟩ := uscalar_add_ok j 1#usize (by
      rw [uscalar_max_usize]; simp only [UScalar.ofNatCore_val_eq]; omega)
    simp only [UScalar.ofNatCore_val_eq] at hj1_val
    have hbody :
        ReadSizedMerkleVerifier.verify_aligned_chunks_loop0_loop0.body hInst srcInst
          offset src len unaligned_len chunk_pos num_blocks hasher j =
          ok (.cont (hasher_mid, j1)) := by
      unfold ReadSizedMerkleVerifier.verify_aligned_chunks_loop0_loop0.body
      simp only [hj_lt, ↓reduceIte, hmul_j, hadd_bp, hsub_len, hmin_buf, hsub_ulen, hmin_u,
                 lift, hadd64, hboff64_eq, hhba, hupd, hadd_j1, bind_ok]
    obtain ⟨hasher', ih, hrel'⟩ :=
      verify_aligned_chunks_inner_loop_spec hsha hsrc offset len unaligned_len chunk_pos num_blocks
        hlen_eq hpage hoff_align8 hcpos_align8 hule hsum_usize hnb_lt
        rem hasher_mid (acc ++ leaf_h.val) j1 (by omega) (by omega) hrel_mid
        (by simp [leaf_h.property]; omega)
    have hacl_lt :
        j.val < (alignedChunkLeafHashes sha256 offset.val slice.val len.val unaligned_len.val
                  chunk_pos.val num_blocks.val).length := by
      rw [alignedChunkLeafHashes_length]; exact hj_lt_nat
    have hacl_get := alignedChunkLeafHashes_getElem sha256 offset.val slice.val len.val
      unaligned_len.val chunk_pos.val num_blocks.val j.val hacl_lt
    refine ⟨hasher', ?_, ?_⟩
    · unfold ReadSizedMerkleVerifier.verify_aligned_chunks_loop0_loop0 at ih ⊢
      rw [loop_cont _ (hasher, j) _ hbody, ih]
    · rw [List.drop_eq_getElem_cons hacl_lt, hacl_get, concatHashes_cons, ← hj1_val]
      simpa [List.append_assoc, leaf_h] using hrel'

theorem verify_aligned_chunks_outer_loop_spec
    {H Src : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {srcInst : AlignedByteSource Src} {src : Src} {slice : Slice Std.U8}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice)
    (rsv : ReadSizedMerkleVerifier)
    (offset len unaligned_len hash_start_index hashes_per_hash num_chunks : Std.Usize)
    (hwf : ReadSizedWf rsv)
    (hlen_eq : len.val = slice.val.length)
    (hpage : len.val % 4096 = 0)
    (hoff_rs : offset.val % rsv.read_size.val = 0)
    (_hunz : 0 < unaligned_len.val)
    (hule : unaligned_len.val ≤ len.val)
    (hsum_usize : offset.val + len.val ≤ Usize.max)
    (_hhstart : hash_start_index.val = offset.val / rsv.read_size.val)
    (hhph : hashes_per_hash.val = rsv.read_size.val / 8192)
    (hnchunks : num_chunks.val = (unaligned_len.val + rsv.read_size.val - 1) / rsv.read_size.val)
    (hend : hash_start_index.val + num_chunks.val ≤ rsv.hashes.val.length) :
    ∀ (rem : Nat) (i : Std.Usize),
      i.val ≤ num_chunks.val →
      num_chunks.val - i.val = rem →
      ∃ res : core.result.Result Unit VerifyError,
        ReadSizedMerkleVerifier.verify_aligned_chunks_loop0 hInst srcInst
          rsv offset src len unaligned_len hash_start_index hashes_per_hash num_chunks i = ok res ∧
        (res = .Ok () ↔
          ∀ k, i.val ≤ k →
            k < num_chunks.val →
            ∀ hk : hash_start_index.val + k < rsv.hashes.val.length,
              rsv.hashes.val[hash_start_index.val + k]'hk =
                specHashInternal sha256 hashes_per_hash.val rsv.bytes_per_hash 1#u64
                  (u64OfNat ((hash_start_index.val + k) * rsv.bytes_per_hash.val))
                  (alignedChunkLeafHashes sha256 offset.val slice.val len.val unaligned_len.val
                    (k * rsv.read_size.val)
                    ((min rsv.read_size.val (unaligned_len.val - k * rsv.read_size.val) + 8191) / 8192)))
  | 0, i, _, _ => by
    have hnlt : ¬ (i < num_chunks) := not_usize_lt_of_ge (by omega)
    have hbody :
        ReadSizedMerkleVerifier.verify_aligned_chunks_loop0.body hInst srcInst
          rsv offset src len unaligned_len hash_start_index hashes_per_hash num_chunks i =
          ok (.done (.Ok ())) := by
      unfold ReadSizedMerkleVerifier.verify_aligned_chunks_loop0.body
      simp only [hnlt, ↓reduceIte]
    refine ⟨.Ok (), ?_, ?_⟩
    · unfold ReadSizedMerkleVerifier.verify_aligned_chunks_loop0
      exact loop_done _ i _ hbody
    · simp only [true_iff]
      intro k hk_ge hk_lt
      omega
  | rem + 1, i, _, _ => by
    have hi_lt_nat : i.val < num_chunks.val := by omega
    have hi_lt : i < num_chunks := usize_lt_of_lt hi_lt_nat
    have hmul_lt_u : i.val * rsv.read_size.val < unaligned_len.val :=
      mul_lt_of_lt_div_ceil hwf.hread_pos (by rw [← hnchunks]; exact hi_lt_nat)
    obtain ⟨chunk_pos, hmul_cp, hcpos_val⟩ := uscalar_mul_ok i rsv.read_size (by
      rw [uscalar_max_usize]; omega)
    obtain ⟨rem_u, hsub_u, hrem_u_val⟩ := uscalar_sub_ok unaligned_len chunk_pos (by omega)
    obtain ⟨chunk_ulen, hmin_cu, hculen_val⟩ := min_usize_spec rsv.read_size rem_u
    rw [hrem_u_val, hcpos_val] at hculen_val
    obtain ⟨num_blocks, hceil_nb, hnb_val_raw⟩ := div_ceil_usize_spec chunk_ulen BLOCK_SIZE (by simp)
    rw [BLOCK_SIZE_val, hculen_val] at hnb_val_raw
    have hnb_val :
        num_blocks.val =
          (min rsv.read_size.val (unaligned_len.val - i.val * rsv.read_size.val) + 8191) / 8192 :=
      hnb_val_raw
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
    obtain ⟨h0, hmhh, hrel0⟩ :=
      make_hash_hasher_gen_spec hsha rsv.bytes_per_hash 1#u64 lvl_off (by decide)
    have hrs_eq_K : rsv.read_size.val = hashes_per_hash.val * 8192 := by
      rw [hhph]; exact (Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hwf.hread_aligned)).symm
    have hoff_align8 : offset.val % 8192 = 0 :=
      Nat.mod_eq_zero_of_dvd
        (Nat.dvd_trans (Nat.dvd_of_mod_eq_zero hwf.hread_aligned)
          (Nat.dvd_of_mod_eq_zero hoff_rs))
    have hcpos_align8 : chunk_pos.val % 8192 = 0 := by
      rw [hcpos_val, hrs_eq_K, ← Nat.mul_assoc]; simp
    have hnb_lt : ∀ k < num_blocks.val, chunk_pos.val + k * 8192 < unaligned_len.val := by
      intro k hk_lt
      have hk_mul : k * 8192 < min rsv.read_size.val (unaligned_len.val - i.val * rsv.read_size.val) :=
        mul_lt_of_lt_div_ceil (k := 8192) (by decide) (by rw [← hnb_val_raw]; exact hk_lt)
      rw [hcpos_val]; omega
    have hnb_le_K : num_blocks.val ≤ hashes_per_hash.val := by
      rw [hnb_val]
      exact div_ceil_8192_le_of_le_mul (by rw [← hrs_eq_K]; exact Nat.min_le_left _ _)
    have hsha_max := rsv_preimage_le_sha256_max hwf
    obtain ⟨h1, hinner, hrel1⟩ :=
      verify_aligned_chunks_inner_loop_spec hsha hsrc offset len unaligned_len chunk_pos num_blocks
        hlen_eq hpage hoff_align8 hcpos_align8 hule hsum_usize hnb_lt
        num_blocks.val h0 (specIdentity rsv.bytes_per_hash 1#u64 lvl_off) 0#usize
        (Nat.zero_le _) (by simp) hrel0
        (by simp; omega)
    simp only [UScalar.ofNatCore_val_eq, List.drop_zero] at hrel1
    set comp_i :=
      alignedChunkLeafHashes sha256 offset.val slice.val len.val unaligned_len.val
        (i.val * rsv.read_size.val)
        ((min rsv.read_size.val (unaligned_len.val - i.val * rsv.read_size.val) + 8191) / 8192)
    have hcomp_eq :
        alignedChunkLeafHashes sha256 offset.val slice.val len.val unaligned_len.val
          chunk_pos.val num_blocks.val = comp_i := by
      rw [hcpos_val, hnb_val]
    have hcomp_len : comp_i.length = num_blocks.val := by
      rw [← hcomp_eq, alignedChunkLeafHashes_length]
    rw [hcomp_eq] at hrel1
    obtain ⟨h2, hpad_branch, hrel2⟩ :
        ∃ h2 : H,
          (if num_blocks != hashes_per_hash
           then do
             let i5 ← hashes_per_hash - num_blocks
             let i6 ← i5 * HASH_SIZE
             update_with_zeros hInst h1 i6
           else ok h1) = ok h2 ∧
          R_H h2 (specInternalPreimage hashes_per_hash.val rsv.bytes_per_hash 1#u64 lvl_off comp_i) := by
      by_cases hne : num_blocks != hashes_per_hash
      · simp only [hne, ↓reduceIte]
        obtain ⟨diff, hsub_k, hdiff_val⟩ := uscalar_sub_ok hashes_per_hash num_blocks hnb_le_K
        have hmul_pad_le : diff.val * 32 ≤ Usize.max :=
          rsv_pad_bytes_le_usize_max hwf (by omega)
        obtain ⟨zbytes, hmul_z, hz_val⟩ := uscalar_mul_ok diff HASH_SIZE (by
          rw [uscalar_max_usize, HASH_SIZE_val]; exact hmul_pad_le)
        rw [HASH_SIZE_val, hdiff_val, ← hcomp_len] at hz_val
        obtain ⟨h2, hzupd, hrel2⟩ := update_with_zeros_gen_spec hsha h1 _ zbytes hrel1
          (by simp [hz_val, hcomp_len]; omega)
        rw [hz_val] at hrel2
        refine ⟨h2, ?_, ?_⟩
        · simp only [hsub_k, hmul_z, hzupd, bind_ok]
        · simpa [specInternalPreimage, HASH_BYTES, List.append_assoc] using hrel2
      · simp only [hne, Bool.false_eq_true, ↓reduceIte]
        have heq_nb : num_blocks.val = hashes_per_hash.val := by
          have : num_blocks = hashes_per_hash := by
            by_contra hc; exact hne (bne_iff_ne.mpr hc)
          exact congrArg UScalar.val this
        refine ⟨h1, rfl, ?_⟩
        simpa [specInternalPreimage, hcomp_len, heq_nb, specZeroPad] using hrel1
    have hfin := hsha.finalize_ok h2 _ hrel2
      (by simp [specInternalPreimage, HASH_BYTES, hcomp_len]; omega)
    have hv_idx := slice_index_ok rsv.hashes hidx (by omega)
    set expected_i :=
      specHashInternal sha256 hashes_per_hash.val rsv.bytes_per_hash 1#u64
        (u64OfNat ((hash_start_index.val + i.val) * rsv.bytes_per_hash.val)) comp_i
    set stored_i := rsv.hashes.val[hash_start_index.val + i.val]'hidx_lt
    have hv_idx' : Slice.index_usize rsv.hashes hidx = ok stored_i := by
      simpa [stored_i, hhidx_val] using hv_idx
    have hfin' : hInst.finalize h2 = ok expected_i := by
      simpa [expected_i, specHashInternal, hloff_eq] using hfin
    by_cases hmatch : expected_i = stored_i
    · have hne_false :
          core.cmp.PartialEq.ne.trait_default Hash.Insts.CoreCmpPartialEqHash expected_i stored_i =
            ok false := by
        simp [hash_ne_spec, hmatch]
      have hnc_usize : num_chunks.val ≤ Usize.max := usize_val_le_max num_chunks
      obtain ⟨i1, hadd_i1, hi1_val⟩ := uscalar_add_ok i 1#usize (by
        rw [uscalar_max_usize]; simp only [UScalar.ofNatCore_val_eq]; omega)
      simp only [UScalar.ofNatCore_val_eq] at hi1_val
      have hbody :
          ReadSizedMerkleVerifier.verify_aligned_chunks_loop0.body hInst srcInst
            rsv offset src len unaligned_len hash_start_index hashes_per_hash num_chunks i =
            ok (.cont i1) := by
        unfold ReadSizedMerkleVerifier.verify_aligned_chunks_loop0.body
        simp only [hi_lt, ↓reduceIte, hmul_cp, hsub_u, hmin_cu, hceil_nb, hadd_hidx, lift,
                   hmul_loff, hmhh, hinner, bind_ok]
        refine bind_ok_step hpad_branch ?_
        simp only [hfin', bind_ok, hv_idx', hne_false, Bool.false_eq_true, ↓reduceIte, hadd_i1]
      obtain ⟨res, ih_exec, ih_iff⟩ :=
        verify_aligned_chunks_outer_loop_spec hsha hsrc rsv offset len unaligned_len
          hash_start_index hashes_per_hash num_chunks hwf hlen_eq hpage hoff_rs _hunz hule
          hsum_usize _hhstart hhph hnchunks hend rem i1 (by omega) (by omega)
      refine ⟨res, ?_, ?_⟩
      · unfold ReadSizedMerkleVerifier.verify_aligned_chunks_loop0 at ih_exec ⊢
        rw [loop_cont _ i _ hbody]
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
          ReadSizedMerkleVerifier.verify_aligned_chunks_loop0.body hInst srcInst
            rsv offset src len unaligned_len hash_start_index hashes_per_hash num_chunks i =
            ok (.done (.Err .IoDataIntegrity)) := by
        unfold ReadSizedMerkleVerifier.verify_aligned_chunks_loop0.body
        simp only [hi_lt, ↓reduceIte, hmul_cp, hsub_u, hmin_cu, hceil_nb, hadd_hidx, lift,
                   hmul_loff, hmhh, hinner, bind_ok]
        refine bind_ok_step hpad_branch ?_
        simp only [hfin', bind_ok, hv_idx', hne_true, ↓reduceIte]
      refine ⟨.Err .IoDataIntegrity, ?_, ?_⟩
      · unfold ReadSizedMerkleVerifier.verify_aligned_chunks_loop0
        exact loop_done _ i _ hbody
      · simp only [reduceCtorEq, false_iff]
        intro hall
        exact hmatch (hall i.val (le_refl _) hi_lt_nat hidx_lt).symm

-- ============================================================================
-- 3. Totality / Panic-Freedom (P1) & Completeness (P3)
-- ============================================================================

/-- **Property P1 (`ReadSizedMerkleVerifier.verify_aligned`)**:
    Total panic & arithmetic overflow freedom across ALL `(offset, src, unaligned_len)` inputs
    and ALL page sizes (4 KiB, 16 KiB, 64 KiB, etc.). -/
theorem read_sized_verifier_verify_aligned_no_panic_of_wf
    {H Src : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {srcInst : AlignedByteSource Src}
    {rsv : ReadSizedMerkleVerifier} {src : Src} {slice : Slice Std.U8}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice)
    (hwf : ReadSizedWf rsv)
    (offset unaligned_len : Std.Usize) :
    ∃ res,
      ReadSizedMerkleVerifier.verify_aligned hInst srcInst rsv offset src unaligned_len = ok res := by
  unfold ReadSizedMerkleVerifier.verify_aligned
  simp only [hsrc.buffer_len_eq, lift, bind_ok]
  rcases usize_checked_add_spec_opt offset (Slice.len slice) with
    ⟨hnone, _⟩ | ⟨_, hsome, hsum, _⟩
  · simp only [hnone, option_is_none_none, ↓reduceIte]; exact ⟨_, rfl⟩
  · simp only [hsome, option_is_none_some, Bool.false_eq_true, ↓reduceIte]
    simp only [Slice.len_val, Slice.length] at hsum
    simp only [usize_is_multiple_of_eq, bind_ok]
    by_cases hoff : offset.val % rsv.read_size.val = 0
    · simp only [hoff, decide_true, ↓reduceIte]
      by_cases hpage : (Slice.len slice).val % PAGE_SIZE_MIN.val = 0
      · simp only [hpage, decide_true, ↓reduceIte]
        simp only [Slice.len_val, Slice.length, PAGE_SIZE_MIN_val] at hpage
        by_cases hu0 : unaligned_len = 0#usize
        · simp only [hu0, ↓reduceIte]; exact ⟨_, rfl⟩
        · simp only [hu0, ↓reduceIte]
          have hunz : 0 < unaligned_len.val := by
            have : unaligned_len.val ≠ 0 := fun h => hu0 (UScalar.eq_of_val_eq (by simpa using h))
            omega
          by_cases hugt : unaligned_len > Slice.len slice
          · simp only [hugt, ↓reduceIte]; exact ⟨_, rfl⟩
          · simp only [hugt, ↓reduceIte]
            have hule : unaligned_len.val ≤ slice.val.length := by
              by_contra hc
              exact hugt (usize_lt_of_lt (by simpa using Nat.lt_of_not_ge hc))
            obtain ⟨len_blks, hceil_lb, _⟩ :=
              div_ceil_usize_spec (Slice.len slice) BLOCK_SIZE (by simp)
            obtain ⟨u_blks, hceil_ub, _⟩ :=
              div_ceil_usize_spec unaligned_len BLOCK_SIZE (by simp)
            simp only [hceil_lb, hceil_ub, bind_ok]
            by_cases hblk_gt : len_blks > u_blks
            · simp only [hblk_gt, ↓reduceIte]; exact ⟨_, rfl⟩
            · simp only [hblk_gt, ↓reduceIte]
              obtain ⟨uend, hadd_uend, huend_val⟩ := uscalar_add_ok offset unaligned_len (by
                rw [uscalar_max_usize]; omega)
              obtain ⟨hstart, hdiv_start, hstart_val⟩ :=
                uscalar_div_ok offset rsv.read_size (by have := hwf.hread_pos; omega)
              obtain ⟨hend_idx, hceil_end, hend_val⟩ :=
                div_ceil_usize_spec uend rsv.read_size hwf.hread_pos
              rw [huend_val] at hend_val
              obtain ⟨hph, hdiv_hph, hhph_val⟩ := uscalar_div_ok rsv.read_size BLOCK_SIZE (by simp)
              rw [BLOCK_SIZE_val] at hhph_val
              obtain ⟨nchunks, hceil_nc, hnc_val⟩ :=
                div_ceil_usize_spec unaligned_len rsv.read_size hwf.hread_pos
              simp only [hadd_uend, hdiv_start, hceil_end, bind_ok]
              by_cases hmult_end : uend.val % rsv.read_size.val = 0
              · simp only [hmult_end, decide_true, ↓reduceIte]
                by_cases hgt : hend_idx > Slice.len rsv.hashes
                · simp only [hgt, ↓reduceIte]; exact ⟨_, rfl⟩
                · simp only [hgt, ↓reduceIte, hdiv_hph, hceil_nc, bind_ok]
                  have hend_nc : hstart.val + nchunks.val ≤ rsv.hashes.val.length := by
                    have hle : hend_idx.val ≤ rsv.hashes.val.length := by
                      by_contra hc
                      exact hgt (usize_lt_of_lt (by simpa using Nat.lt_of_not_ge hc))
                    have hdecomp := div_ceil_add_aligned (len := unaligned_len.val) hwf.hread_pos hoff
                    omega
                  obtain ⟨res, hvac, _⟩ :=
                    verify_aligned_chunks_outer_loop_spec hsha hsrc rsv offset (Slice.len slice)
                      unaligned_len hstart hph nchunks hwf (by simp) hpage hoff hunz hule hsum
                      hstart_val hhph_val hnc_val hend_nc nchunks.val 0#usize (Nat.zero_le _) (by simp)
                  exact ⟨res, hvac⟩
              · simp only [hmult_end, decide_false, Bool.false_eq_true, ↓reduceIte]
                by_cases hne_end : hend_idx != Slice.len rsv.hashes
                · simp only [hne_end, ↓reduceIte]; exact ⟨_, rfl⟩
                · have hend_eq : hend_idx = Slice.len rsv.hashes := by
                    by_contra hc; exact hne_end (bne_iff_ne.mpr hc)
                  simp only [hend_eq, bne_self_eq_false, usize_gt_irrefl, Bool.false_eq_true, ↓reduceIte,
                             hdiv_hph, hceil_nc, bind_ok]
                  have hend_nc : hstart.val + nchunks.val ≤ rsv.hashes.val.length := by
                    have hv_eq := congrArg UScalar.val hend_eq
                    simp only [Slice.len_val, Slice.length] at hv_eq
                    have hdecomp := div_ceil_add_aligned (len := unaligned_len.val) hwf.hread_pos hoff
                    omega
                  obtain ⟨res, hvac, _⟩ :=
                    verify_aligned_chunks_outer_loop_spec hsha hsrc rsv offset (Slice.len slice)
                      unaligned_len hstart hph nchunks hwf (by simp) hpage hoff hunz hule hsum
                      hstart_val hhph_val hnc_val hend_nc nchunks.val 0#usize (Nat.zero_le _) (by simp)
                  exact ⟨res, hvac⟩
      · simp only [hpage, decide_false, Bool.false_eq_true, ↓reduceIte]; exact ⟨_, rfl⟩
    · simp only [hoff, decide_false, Bool.false_eq_true, ↓reduceIte]; exact ⟨_, rfl⟩

/-- Exact equality between computed aligned-chunk leaf hashes and `specLeafHashes`
    whenever `data[..ulen] = B[offset .. offset + ulen]` and `data[ulen..]` is all zeros. -/
theorem alignedChunkLeafHashes_eq_specLeafHashes_group
    (sha256 : List Std.U8 → Hash) (B data : List Std.U8) (offset ulen readSize : Nat)
    (hrs_pos : 0 < readSize)
    (hrs_align : readSize % 8192 = 0)
    (hoff_rs : offset % readSize = 0)
    (_hunz : 0 < ulen)
    (_hule : ulen ≤ data.length)
    (hin_B : offset + ulen ≤ B.length)
    (hprefix : data.take ulen = (B.drop offset).take ulen)
    (htail_zero : data.drop ulen = specZeroPad (data.length - ulen))
    (halign_end : (offset + ulen) % readSize = 0 ∨ offset + ulen = B.length)
    (i : Nat) (hi : i < (ulen + readSize - 1) / readSize) :
    alignedChunkLeafHashes sha256 offset data data.length ulen (i * readSize)
      ((min readSize (ulen - i * readSize) + 8191) / 8192) =
    ((specLeafHashes sha256 B).drop ((offset / readSize + i) * (readSize / 8192))).take
      (readSize / 8192) := by
  set K := readSize / 8192
  have hrs_eq : readSize = K * 8192 :=
    (Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hrs_align)).symm
  have hK : 0 < K := by
    by_contra h0; simp [Nat.eq_zero_of_not_pos h0] at hrs_eq; omega
  have hBne : B ≠ [] := fun h => by subst h; simp only [List.length_nil] at hin_B; omega
  have hi_mul_lt : i * readSize < ulen := mul_lt_of_lt_div_ceil hrs_pos hi
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
      (alignedChunkLeafHashes sha256 offset data data.length ulen (i * readSize)
        ((min readSize (ulen - i * readSize) + 8191) / 8192)).length =
      (((specLeafHashes sha256 B).drop base).take K).length := by
    rw [alignedChunkLeafHashes_length, List.length_take, List.length_drop, hsl_len]
    rcases halign_end with hmod0 | hendB
    · have hu_mod0 : ulen % readSize = 0 := by
        have hq : offset = readSize * (offset / readSize) :=
          (Nat.mul_div_cancel' (Nat.dvd_of_mod_eq_zero hoff_rs)).symm
        have hrew : offset + ulen = ulen + readSize * (offset / readSize) := by omega
        rw [hrew, Nat.add_mul_mod_self_left] at hmod0
        exact hmod0
      have hi1_le : (i + 1) * readSize ≤ ulen := by
        have hu_div : (ulen + readSize - 1) / readSize = ulen / readSize := by
          have hq : ulen = readSize * (ulen / readSize) :=
            (Nat.mul_div_cancel' (Nat.dvd_of_mod_eq_zero hu_mod0)).symm
          have h1 : (readSize - 1) / readSize = 0 := Nat.div_eq_of_lt (by omega)
          calc (ulen + readSize - 1) / readSize
              = (readSize * (ulen / readSize) + (readSize - 1)) / readSize := by congr 1; omega
            _ = ulen / readSize := by rw [Nat.mul_add_div hrs_pos, h1, Nat.add_zero]
        rw [hu_div] at hi
        exact (Nat.le_div_iff_mul_le hrs_pos).mp hi
      have hmin_u : min readSize (ulen - i * readSize) = K * 8192 := by
        rw [Nat.succ_mul] at hi1_le
        rw [Nat.min_eq_left (by omega), hrs_eq]
      have h1 : (K * 8192 + 8191) / 8192 = K := by
        have hrew : K * 8192 + 8191 = 8192 * K + 8191 := by ring
        rw [hrew, Nat.mul_add_div (by decide : 0 < 8192)]; simp
      have h2 : K ≤ (B.length + 8191) / 8192 - base := by
        rw [Nat.succ_mul] at hi1_le
        have hmul : (base + K) * 8192 ≤ B.length + 8191 := by
          rw [Nat.add_mul, hbase_mul, ← hrs_eq]; omega
        have hdiv : base + K ≤ (B.length + 8191) / 8192 :=
          (Nat.le_div_iff_mul_le (by decide : 0 < 8192)).mpr hmul
        omega
      rw [hmin_u, h1, Nat.min_eq_left h2]
    · have hu_diff : ulen - i * readSize = B.length - base * 8192 := by omega
      rw [hu_diff, hrs_eq]
      exact min_div_ceil_eq hK hbase_lt_B
  refine List.ext_getElem hlen_eq fun j hj₁ hj₂ => ?_
  rw [alignedChunkLeafHashes_getElem, List.getElem_take, List.getElem_drop]
  have hj_lt_K : j < K := by
    rw [List.length_take] at hj₂; exact lt_of_lt_of_le hj₂ (Nat.min_le_left _ _)
  have hidx_sl : base + j < (specLeafHashes sha256 B).length := by
    rw [List.length_take, List.length_drop] at hj₂; omega
  obtain ⟨hB_leaf, _, _⟩ := specLeafHashes_getElem_of_nonempty sha256 B hBne (base + j) hidx_sl
  rw [hB_leaf]
  set bytePos := i * readSize + j * 8192
  have hbp_lt_u : bytePos < ulen := by
    rw [alignedChunkLeafHashes_length] at hj₁
    have hj_mul := mul_lt_of_lt_div_ceil (k := 8192) (by decide) hj₁
    dsimp [bytePos]; omega
  have hoff_bp_eq : offset + bytePos = (base + j) * 8192 := by
    dsimp [bytePos]; rw [Nat.add_mul, hbase_mul, Nat.add_assoc]
  set canon := (B.drop ((base + j) * 8192)).take 8192
  set bulen := min 8192 (ulen - bytePos)
  set bbuf := min 8192 (data.length - bytePos)
  have hcanon_eq_data : canon = (data.drop bytePos).take bulen := by
    have h1 : (data.drop bytePos).take bulen = ((data.take ulen).drop bytePos).take 8192 := by
      rw [List.drop_take, List.take_take]
    rw [hprefix, List.drop_take, List.drop_drop, hoff_bp_eq] at h1
    rcases halign_end with hmod0 | hendB
    · have hu_mod0 : ulen % readSize = 0 := by
        have hq : offset = readSize * (offset / readSize) :=
          (Nat.mul_div_cancel' (Nat.dvd_of_mod_eq_zero hoff_rs)).symm
        have hrew : offset + ulen = ulen + readSize * (offset / readSize) := by omega
        rw [hrew, Nat.add_mul_mod_self_left] at hmod0
        exact hmod0
      have hi1_le : (i + 1) * readSize ≤ ulen := by
        have hu_div : (ulen + readSize - 1) / readSize = ulen / readSize := by
          have hq : ulen = readSize * (ulen / readSize) :=
            (Nat.mul_div_cancel' (Nat.dvd_of_mod_eq_zero hu_mod0)).symm
          have h1' : (readSize - 1) / readSize = 0 := Nat.div_eq_of_lt (by omega)
          calc (ulen + readSize - 1) / readSize
              = (readSize * (ulen / readSize) + (readSize - 1)) / readSize := by congr 1; omega
            _ = ulen / readSize := by rw [Nat.mul_add_div hrs_pos, h1', Nat.add_zero]
        rw [hu_div] at hi
        exact (Nat.le_div_iff_mul_le hrs_pos).mp hi
      have hrem_ge : 8192 ≤ ulen - bytePos := by
        rw [Nat.succ_mul] at hi1_le
        have hj1 : (j + 1) * 8192 ≤ readSize := by
          rw [hrs_eq]; exact Nat.mul_le_mul_right 8192 hj_lt_K
        rw [Nat.succ_mul] at hj1
        dsimp [bytePos]; omega
      rw [List.take_take, Nat.min_eq_left hrem_ge] at h1
      exact h1.symm
    · have hrem_eq : ulen - bytePos = (B.drop ((base + j) * 8192)).length := by
        rw [List.length_drop, ← hoff_bp_eq]; omega
      rw [hrem_eq, List.take_length] at h1
      exact h1.symm
  have hbulen_len : canon.length = bulen := by
    rw [hcanon_eq_data, List.length_take, List.length_drop]; omega
  have hc_ne0 : canon.length ≠ 0 := by dsimp [bulen] at hbulen_len; omega
  simp only [specHashBlock, hc_ne0, false_and, ↓reduceIte]
  rw [hbulen_len, hoff_bp_eq]
  congr 1
  simp only [List.append_assoc]
  congr 1
  have hule_buf : bulen ≤ bbuf := by dsimp [bulen, bbuf]; omega
  have hbuf_le : bbuf ≤ 8192 := Nat.min_le_left _ _
  have hsplit_take :
      (data.drop bytePos).take bbuf =
      (data.drop bytePos).take bulen ++
        ((data.drop bytePos).drop bulen).take (bbuf - bulen) := by
    have hadd : bbuf = bulen + (bbuf - bulen) := by omega
    conv => lhs; rw [hadd]
    exact List.take_add
  have hmid_zero :
      ((data.drop bytePos).drop bulen).take (bbuf - bulen) =
      specZeroPad (bbuf - bulen) := by
    by_cases hsame : bulen = bbuf
    · simp [hsame, specZeroPad]
    · have hbulen_tail : bulen = ulen - bytePos := by
        dsimp [bulen]
        have : ulen - bytePos < 8192 := by
          by_contra hge
          have : bulen = 8192 := by dsimp [bulen]; omega
          omega
        exact Nat.min_eq_right (le_of_lt this)
      have hsum_u : bytePos + bulen = ulen := by
        rw [hbulen_tail]; exact Nat.add_sub_of_le (le_of_lt hbp_lt_u)
      rw [List.drop_drop, hsum_u, htail_zero]
      unfold specZeroPad
      have hdiff_le : bbuf - bulen ≤ data.length - ulen := by
        rw [hbulen_tail]; exact min_sub_le_sub (le_of_lt hbp_lt_u)
      rw [List.take_replicate, Nat.min_eq_left hdiff_le]
  rw [hcanon_eq_data, hsplit_take, hmid_zero, List.append_assoc, ← specZeroPad_add]
  congr 2
  unfold BLOCK_BYTES
  exact sub_add_sub_cancel_of_le hule_buf hbuf_le

/-- **Property P3 (`ReadSizedMerkleVerifier.verify_aligned` Completeness)**:
    Accepts every valid `read_size`-aligned, page-aligned, zero-padded buffer
    across all page sizes (4 KiB, 16 KiB, 64 KiB, etc.). -/
theorem read_sized_verifier_verify_aligned_complete_of_wf
    {H Src : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {srcInst : AlignedByteSource Src}
    {B : List Std.U8} {rsv : ReadSizedMerkleVerifier}
    {offset : Std.Usize} {src : Src} {slice : Slice Std.U8} {unaligned_len : Std.Usize}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice)
    (hwf : ReadSizedWf rsv)
    (hK : 0 < rsv.read_size.val / 8192)
    (hrsv_h :
      rsv.hashes.val =
        specReadSizedHashes sha256 (rsv.read_size.val / 8192) hK rsv.bytes_per_hash
          (specLeafHashes sha256 B))
    (hoff_rs : offset.val % rsv.read_size.val = 0)
    (hpage : slice.val.length % MIN_PAGE_BYTES = 0)
    (hunz : 0 < unaligned_len.val)
    (hule : unaligned_len.val ≤ slice.val.length)
    (hblocks :
      (slice.val.length + (BLOCK_BYTES - 1)) / BLOCK_BYTES ≤
        (unaligned_len.val + (BLOCK_BYTES - 1)) / BLOCK_BYTES)
    (hsum_usize : offset.val + slice.val.length ≤ Usize.max)
    (hin_blob : offset.val + unaligned_len.val ≤ B.length)
    (hprefix : slice.val.take unaligned_len.val = (B.drop offset.val).take unaligned_len.val)
    (htail_zero : slice.val.drop unaligned_len.val = specZeroPad (slice.val.length - unaligned_len.val))
    (halign_end :
      (offset.val + unaligned_len.val) % rsv.read_size.val = 0 ∨
      offset.val + unaligned_len.val = B.length) :
    ReadSizedMerkleVerifier.verify_aligned hInst srcInst rsv offset src unaligned_len =
      ok (.Ok ()) := by
  have hBne : B ≠ [] := fun h => by subst h; simp only [List.length_nil] at hin_blob; omega
  have hpage4096 : slice.val.length % 4096 = 0 := hpage
  unfold ReadSizedMerkleVerifier.verify_aligned
  simp only [hsrc.buffer_len_eq, lift, bind_ok]
  rcases usize_checked_add_spec_opt offset (Slice.len slice) with
    ⟨_, hovf⟩ | ⟨_, hsome, _, _⟩
  · simp only [Slice.len_val, Slice.length] at hovf; omega
  · simp only [hsome, option_is_none_some, Bool.false_eq_true, ↓reduceIte,
               usize_is_multiple_of_eq, hoff_rs, Slice.len_val, Slice.length,
               PAGE_SIZE_MIN_val, hpage4096, decide_true, bind_ok]
    have hu0 : ¬ (unaligned_len = 0#usize) := fun h => by subst h; simp at hunz
    have hugt : ¬ (unaligned_len > Slice.len slice) :=
      not_usize_gt_of_le (by simpa using hule)
    simp only [hu0, hugt, ↓reduceIte]
    obtain ⟨len_blks, hceil_lb, hlb_val⟩ :=
      div_ceil_usize_spec (Slice.len slice) BLOCK_SIZE (by simp)
    obtain ⟨u_blks, hceil_ub, hub_val⟩ :=
      div_ceil_usize_spec unaligned_len BLOCK_SIZE (by simp)
    simp only [Slice.len_val, Slice.length, BLOCK_SIZE_val] at hlb_val hub_val
    have hblk_gt : ¬ (len_blks > u_blks) :=
      not_usize_gt_of_le (by unfold BLOCK_BYTES at hblocks; omega)
    simp only [hceil_lb, hceil_ub, hblk_gt, ↓reduceIte, bind_ok]
    obtain ⟨uend, hadd_uend, huend_val⟩ := uscalar_add_ok offset unaligned_len (by
      rw [uscalar_max_usize]; omega)
    obtain ⟨hstart, hdiv_start, hstart_val⟩ :=
      uscalar_div_ok offset rsv.read_size (by have := hwf.hread_pos; omega)
    obtain ⟨hend_idx, hceil_end, hend_val⟩ :=
      div_ceil_usize_spec uend rsv.read_size hwf.hread_pos
    rw [huend_val] at hend_val
    obtain ⟨hph, hdiv_hph, hhph_val⟩ := uscalar_div_ok rsv.read_size BLOCK_SIZE (by simp)
    rw [BLOCK_SIZE_val] at hhph_val
    obtain ⟨nchunks, hceil_nc, hnc_val⟩ :=
      div_ceil_usize_spec unaligned_len rsv.read_size hwf.hread_pos
    have hrs_eq : rsv.read_size.val = (rsv.read_size.val / 8192) * 8192 :=
      (Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hwf.hread_aligned)).symm
    have hrsv_len :
        rsv.hashes.val.length = (B.length + rsv.read_size.val - 1) / rsv.read_size.val := by
      rw [hrsv_h, specReadSizedHashes_length, specLeafHashes_length,
          List.isEmpty_eq_false_iff.mpr hBne]
      simp only [Bool.false_eq_true, ↓reduceIte]
      rw [div_ceil_div_ceil_eq hK, ← hrs_eq]
    have hend_le_rsv : hend_idx.val ≤ rsv.hashes.val.length := by
      rw [hend_val, hrsv_len]; exact Nat.div_le_div_right (by omega)
    have hend_nc : hstart.val + nchunks.val ≤ rsv.hashes.val.length := by
      have hdecomp := div_ceil_add_aligned (len := unaligned_len.val) hwf.hread_pos hoff_rs
      omega
    obtain ⟨res, hvac, hiff⟩ :=
      verify_aligned_chunks_outer_loop_spec hsha hsrc rsv offset (Slice.len slice)
        unaligned_len hstart hph nchunks hwf (by simp) hpage4096 hoff_rs hunz hule hsum_usize
        hstart_val hhph_val hnc_val hend_nc nchunks.val 0#usize (Nat.zero_le _) (by simp)
    have hres_ok : res = .Ok () := by
      apply hiff.mpr
      intro i _ hi_lt hk_in
      simp only [Slice.len_val, Slice.length]
      have hgrp_eq := alignedChunkLeafHashes_eq_specLeafHashes_group sha256 B slice.val
        offset.val unaligned_len.val rsv.read_size.val hwf.hread_pos hwf.hread_aligned hoff_rs
        hunz hule hin_blob hprefix htail_zero halign_end i (by rw [← hnc_val]; exact hi_lt)
      have hrs_get := specReadSizedHashes_getElem sha256 (rsv.read_size.val / 8192) hK
        rsv.bytes_per_hash (specLeafHashes sha256 B) (hstart.val + i)
        (by rw [← hrsv_h]; exact hk_in)
      rw [List.getElem_of_eq hrsv_h _, hrs_get, hgrp_eq, hstart_val, hhph_val]
    simp only [hadd_uend, hdiv_start, hceil_end, bind_ok]
    by_cases hmult_end : uend.val % rsv.read_size.val = 0
    · have hgt_false : ¬ (hend_idx > Slice.len rsv.hashes) :=
        not_usize_gt_of_le (by simpa using hend_le_rsv)
      simp only [hmult_end, decide_true, ↓reduceIte, hgt_false, hdiv_hph, hceil_nc,
                 ReadSizedMerkleVerifier.verify_aligned_chunks, hvac, hres_ok, bind_ok]
    · have hend_eq_B : offset.val + unaligned_len.val = B.length := by
        rcases halign_end with h0 | hB
        · exfalso; apply hmult_end; rw [huend_val]; exact h0
        · exact hB
      have hend_eq_len : hend_idx = Slice.len rsv.hashes := by
        apply UScalar.eq_of_val_eq
        rw [hend_val, Slice.len_val, Slice.length, hrsv_len, hend_eq_B]
      simp only [hmult_end, decide_false, Bool.false_eq_true, ↓reduceIte,
                 hend_eq_len, bne_self_eq_false, usize_gt_irrefl, hdiv_hph, hceil_nc,
                 ReadSizedMerkleVerifier.verify_aligned_chunks, hvac, hres_ok, bind_ok]

-- ============================================================================
-- 4. Unconditional & Size-Bounded Soundness (P4)
-- ============================================================================

private theorem verify_aligned_extract_ok
    {H Src : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {srcInst : AlignedByteSource Src}
    {rsv : ReadSizedMerkleVerifier} {offset : Std.Usize}
    {src : Src} {slice : Slice Std.U8} {unaligned_len : Std.Usize}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice)
    (hwf : ReadSizedWf rsv)
    (hok : ReadSizedMerkleVerifier.verify_aligned hInst srcInst rsv offset src unaligned_len =
             ok (.Ok ())) :
    offset.val + slice.val.length ≤ Usize.max ∧
    offset.val % rsv.read_size.val = 0 ∧
    slice.val.length % MIN_PAGE_BYTES = 0 ∧
    0 < unaligned_len.val ∧
    unaligned_len.val ≤ slice.val.length ∧
    (slice.val.length + (BLOCK_BYTES - 1)) / BLOCK_BYTES ≤
      (unaligned_len.val + (BLOCK_BYTES - 1)) / BLOCK_BYTES ∧
    offset.val / rsv.read_size.val +
      (unaligned_len.val + rsv.read_size.val - 1) / rsv.read_size.val ≤
        rsv.hashes.val.length ∧
    (∀ i,
      i < (unaligned_len.val + rsv.read_size.val - 1) / rsv.read_size.val →
      ∀ hk : offset.val / rsv.read_size.val + i < rsv.hashes.val.length,
        rsv.hashes.val[offset.val / rsv.read_size.val + i]'hk =
          specHashInternal sha256 (rsv.read_size.val / 8192) rsv.bytes_per_hash 1#u64
            (u64OfNat ((offset.val / rsv.read_size.val + i) * rsv.bytes_per_hash.val))
            (alignedChunkLeafHashes sha256 offset.val slice.val slice.val.length unaligned_len.val
              (i * rsv.read_size.val)
              ((min rsv.read_size.val (unaligned_len.val - i * rsv.read_size.val) + 8191) / 8192))) := by
  unfold ReadSizedMerkleVerifier.verify_aligned at hok
  simp only [hsrc.buffer_len_eq, lift, bind_ok] at hok
  rcases usize_checked_add_spec_opt offset (Slice.len slice) with
    ⟨hnone, _⟩ | ⟨_, hsome, hsum, _⟩
  · simp [hnone] at hok
  · simp only [hsome, option_is_none_some, Bool.false_eq_true, ↓reduceIte,
               usize_is_multiple_of_eq, bind_ok] at hok
    simp only [Slice.len_val, Slice.length] at hsum
    by_cases hoff : offset.val % rsv.read_size.val = 0
    · simp only [hoff, decide_true, ↓reduceIte] at hok
      by_cases hpage : (Slice.len slice).val % PAGE_SIZE_MIN.val = 0
      · simp only [hpage, decide_true, ↓reduceIte] at hok
        simp only [Slice.len_val, Slice.length, PAGE_SIZE_MIN_val] at hpage
        by_cases hu0 : unaligned_len = 0#usize
        · simp [hu0] at hok
        · simp only [hu0, ↓reduceIte] at hok
          have hunz : 0 < unaligned_len.val := by
            have : unaligned_len.val ≠ 0 := fun h => hu0 (UScalar.eq_of_val_eq (by simpa using h))
            omega
          by_cases hugt : unaligned_len > Slice.len slice
          · simp [hugt] at hok
          · simp only [hugt, ↓reduceIte] at hok
            have hule : unaligned_len.val ≤ slice.val.length := by
              by_contra hc
              exact hugt (usize_lt_of_lt (by simpa using Nat.lt_of_not_ge hc))
            obtain ⟨len_blks, hceil_lb, hlb_val⟩ :=
              div_ceil_usize_spec (Slice.len slice) BLOCK_SIZE (by simp)
            obtain ⟨u_blks, hceil_ub, hub_val⟩ :=
              div_ceil_usize_spec unaligned_len BLOCK_SIZE (by simp)
            simp only [Slice.len_val, Slice.length, BLOCK_SIZE_val] at hlb_val hub_val
            simp only [hceil_lb, hceil_ub, bind_ok] at hok
            by_cases hblk_gt : len_blks > u_blks
            · simp [hblk_gt] at hok
            · simp only [hblk_gt, ↓reduceIte] at hok
              have hblocks :
                  (slice.val.length + (BLOCK_BYTES - 1)) / BLOCK_BYTES ≤
                    (unaligned_len.val + (BLOCK_BYTES - 1)) / BLOCK_BYTES := by
                have hle_blk : len_blks.val ≤ u_blks.val := by
                  by_contra hc
                  exact hblk_gt (usize_lt_of_lt (Nat.lt_of_not_ge hc))
                unfold BLOCK_BYTES; omega
              obtain ⟨uend, hadd_uend, huend_val⟩ := uscalar_add_ok offset unaligned_len (by
                rw [uscalar_max_usize]; omega)
              obtain ⟨hstart, hdiv_start, hstart_val⟩ :=
                uscalar_div_ok offset rsv.read_size (by have := hwf.hread_pos; omega)
              obtain ⟨hend_idx, hceil_end, hend_val⟩ :=
                div_ceil_usize_spec uend rsv.read_size hwf.hread_pos
              rw [huend_val] at hend_val
              obtain ⟨hph, hdiv_hph, hhph_val⟩ := uscalar_div_ok rsv.read_size BLOCK_SIZE (by simp)
              rw [BLOCK_SIZE_val] at hhph_val
              obtain ⟨nchunks, hceil_nc, hnc_val⟩ :=
                div_ceil_usize_spec unaligned_len rsv.read_size hwf.hread_pos
              simp only [hadd_uend, hdiv_start, hceil_end, bind_ok] at hok
              have hextract :
                  hend_idx.val ≤ rsv.hashes.val.length ∧
                  ReadSizedMerkleVerifier.verify_aligned_chunks hInst srcInst
                    rsv offset src (Slice.len slice) unaligned_len hstart hph nchunks =
                      ok (.Ok ()) := by
                by_cases hmult_end : uend.val % rsv.read_size.val = 0
                · simp only [hmult_end, decide_true, ↓reduceIte] at hok
                  by_cases hgt : hend_idx > Slice.len rsv.hashes
                  · simp [hgt] at hok
                  · simp only [hgt, ↓reduceIte, hdiv_hph, hceil_nc, bind_ok] at hok
                    have hle : hend_idx.val ≤ rsv.hashes.val.length := by
                      by_contra hc
                      exact hgt (usize_lt_of_lt (by simpa using Nat.lt_of_not_ge hc))
                    exact ⟨hle, hok⟩
                · simp only [hmult_end, decide_false, Bool.false_eq_true, ↓reduceIte] at hok
                  by_cases hne_end : hend_idx != Slice.len rsv.hashes
                  · simp [hne_end] at hok
                  · have hend_eq : hend_idx = Slice.len rsv.hashes := by
                      by_contra hc; exact hne_end (bne_iff_ne.mpr hc)
                    simp only [hend_eq, bne_self_eq_false, usize_gt_irrefl, Bool.false_eq_true,
                               ↓reduceIte, hdiv_hph, hceil_nc, bind_ok] at hok
                    have hle : hend_idx.val ≤ rsv.hashes.val.length := by
                      have hv_eq := congrArg UScalar.val hend_eq
                      simpa using le_of_eq hv_eq
                    exact ⟨hle, hok⟩
              rcases hextract with ⟨hle_h, hvac_ok⟩
              have hend_nc : hstart.val + nchunks.val ≤ rsv.hashes.val.length := by
                have hdecomp := div_ceil_add_aligned (len := unaligned_len.val) hwf.hread_pos hoff
                omega
              obtain ⟨res, hvac, hiff⟩ :=
                verify_aligned_chunks_outer_loop_spec hsha hsrc rsv offset (Slice.len slice)
                  unaligned_len hstart hph nchunks hwf (by simp) hpage hoff hunz hule hsum
                  hstart_val hhph_val hnc_val hend_nc nchunks.val 0#usize (Nat.zero_le _) (by simp)
              have hres_ok : res = .Ok () := Result.ok_injective (hvac.symm.trans hvac_ok)
              have hall_chunks := hiff.mp hres_ok
              simp only [Slice.len_val, Slice.length] at hall_chunks
              refine ⟨hsum, hoff, hpage, hunz, hule, hblocks, ?_, ?_⟩
              · rw [← hstart_val, ← hnc_val]; exact hend_nc
              · intro i hi_lt hk_in
                have hi_nc : i < nchunks.val := by rw [hnc_val]; exact hi_lt
                have hk_in' : hstart.val + i < rsv.hashes.val.length := by
                  rw [hstart_val]; exact hk_in
                have heq := hall_chunks i (Nat.zero_le _) hi_nc hk_in'
                rw [hhph_val] at heq
                simpa [hstart_val] using heq
      · simp only [hpage, decide_false, Bool.false_eq_true, ↓reduceIte] at hok
        cases Result.ok_injective hok
    · simp only [hoff, decide_false, Bool.false_eq_true, ↓reduceIte] at hok
      cases Result.ok_injective hok

private theorem verify_aligned_groups_eq_imp_sound
    (sha256 : List Std.U8 → Hash) (B : List Std.U8) (rsv : ReadSizedMerkleVerifier)
    (offset : Std.Usize) (slice : Slice Std.U8) (unaligned_len : Std.Usize)
    (hwf : ReadSizedWf rsv)
    (hK : 0 < rsv.read_size.val / 8192)
    (hoff : offset.val % rsv.read_size.val = 0)
    (hpage : slice.val.length % MIN_PAGE_BYTES = 0)
    (hunz : 0 < unaligned_len.val)
    (hule : unaligned_len.val ≤ slice.val.length)
    (hblocks :
      (slice.val.length + (BLOCK_BYTES - 1)) / BLOCK_BYTES ≤
        (unaligned_len.val + (BLOCK_BYTES - 1)) / BLOCK_BYTES)
    (hsum : offset.val + slice.val.length ≤ Usize.max)
    (hall_grp :
      ∀ i < (unaligned_len.val + rsv.read_size.val - 1) / rsv.read_size.val,
        ((specLeafHashes sha256 B).drop
          ((offset.val / rsv.read_size.val + i) * (rsv.read_size.val / 8192))).take
            (rsv.read_size.val / 8192) =
        alignedChunkLeafHashes sha256 offset.val slice.val slice.val.length unaligned_len.val
          (i * rsv.read_size.val)
          ((min rsv.read_size.val (unaligned_len.val - i * rsv.read_size.val) + 8191) / 8192)) :
    (0 < unaligned_len.val ∧
     unaligned_len.val ≤ slice.val.length ∧
     slice.val.length % MIN_PAGE_BYTES = 0 ∧
     (slice.val.length + (BLOCK_BYTES - 1)) / BLOCK_BYTES ≤
       (unaligned_len.val + (BLOCK_BYTES - 1)) / BLOCK_BYTES ∧
     offset.val + unaligned_len.val ≤ B.length ∧
     slice.val.take unaligned_len.val = (B.drop offset.val).take unaligned_len.val ∧
     slice.val.drop unaligned_len.val = specZeroPad (slice.val.length - unaligned_len.val)) ∨
    HashCollision sha256 := by
  set K := rsv.read_size.val / 8192
  have hrs_eq_K : rsv.read_size.val = K * 8192 :=
    (Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hwf.hread_aligned)).symm
  have hoff_align8 : offset.val % 8192 = 0 :=
    Nat.mod_eq_zero_of_dvd
      (Nat.dvd_trans (Nat.dvd_of_mod_eq_zero hwf.hread_aligned)
        (Nat.dvd_of_mod_eq_zero hoff))
  have hsum64 : offset.val + slice.val.length ≤ U64.max := by
    have ⟨_, hu64⟩ := usize_bounds; omega
  have hoff_div8 : offset.val / 8192 = (offset.val / rsv.read_size.val) * K := by
    have hoff_eq : offset.val = (offset.val / rsv.read_size.val) * (K * 8192) := by
      rw [← hrs_eq_K, Nat.mul_comm]
      exact (Nat.mul_div_cancel' (Nat.dvd_of_mod_eq_zero hoff)).symm
    conv => lhs; rw [hoff_eq]
    rw [← Nat.mul_assoc, Nat.mul_div_cancel _ (by decide : 0 < 8192)]
  have hall_leaves :
      ∀ k < (unaligned_len.val + 8191) / 8192,
        ∃ hk : offset.val / 8192 + k < (specLeafHashes sha256 B).length,
          (specLeafHashes sha256 B)[offset.val / 8192 + k]'hk =
            sha256 (specIdentity (u32OfNat (min 8192 (unaligned_len.val - k * 8192))) 0#u64
                      (u64OfNat ((offset.val / 8192 + k) * 8192)) ++
                    (slice.val.drop (k * 8192)).take (min 8192 (slice.val.length - k * 8192)) ++
                    specZeroPad (BLOCK_BYTES - min 8192 (slice.val.length - k * 8192))) := by
    intro k hk_lt
    have hk_mul_lt : k * 8192 < unaligned_len.val :=
      mul_lt_of_lt_div_ceil (k := 8192) (by decide) hk_lt
    set i := k / K
    set j := k % K
    have hj_lt_K : j < K := Nat.mod_lt k hK
    have hk_decomp : i * K + j = k := by
      have h1 := Nat.div_add_mod k K
      rw [Nat.mul_comm] at h1; exact h1
    have hi_mul_lt : i * rsv.read_size.val < unaligned_len.val := by
      have hle : i * (K * 8192) ≤ k * 8192 := by
        rw [← hk_decomp]; calc i * (K * 8192)
            = (i * K) * 8192 := (Nat.mul_assoc _ _ _).symm
          _ ≤ (i * K + j) * 8192 := Nat.mul_le_mul_right 8192 (Nat.le_add_right _ _)
      rw [hrs_eq_K]; omega
    have hi_lt : i < (unaligned_len.val + rsv.read_size.val - 1) / rsv.read_size.val :=
      lt_div_ceil_of_mul_lt hwf.hread_pos hi_mul_lt
    have hgrp_i := hall_grp i hi_lt
    have hj_mul_lt :
        j * 8192 < min rsv.read_size.val (unaligned_len.val - i * rsv.read_size.val) := by
      rw [hrs_eq_K]
      have h1 : j * 8192 < K * 8192 := Nat.mul_lt_mul_of_pos_right hj_lt_K (by decide)
      have h2 : i * (K * 8192) + j * 8192 = k * 8192 := by rw [← hk_decomp]; ring
      exact Nat.lt_min.mpr ⟨h1, Nat.lt_sub_of_add_lt (by rw [Nat.add_comm, h2]; exact hk_mul_lt)⟩
    have hj_lt_comp :
        j < (alignedChunkLeafHashes sha256 offset.val slice.val slice.val.length unaligned_len.val
              (i * rsv.read_size.val)
              ((min rsv.read_size.val (unaligned_len.val - i * rsv.read_size.val) + 8191) / 8192)).length := by
      rw [alignedChunkLeafHashes_length]
      exact lt_div_ceil_8192_of_mul_lt hj_mul_lt
    have hj_lt_grp :
        j < (((specLeafHashes sha256 B).drop ((offset.val / rsv.read_size.val + i) * K)).take K).length := by
      rw [hgrp_i]; exact hj_lt_comp
    have hidx_eq : (offset.val / rsv.read_size.val + i) * K + j = offset.val / 8192 + k := by
      rw [hoff_div8, ← hk_decomp]; ring
    have hk_sl : offset.val / 8192 + k < (specLeafHashes sha256 B).length := by
      rw [List.length_take, List.length_drop] at hj_lt_grp; omega
    refine ⟨hk_sl, ?_⟩
    have hget_eq :
        (((specLeafHashes sha256 B).drop ((offset.val / rsv.read_size.val + i) * K)).take K)[j]'hj_lt_grp =
        (alignedChunkLeafHashes sha256 offset.val slice.val slice.val.length unaligned_len.val
          (i * rsv.read_size.val)
          ((min rsv.read_size.val (unaligned_len.val - i * rsv.read_size.val) + 8191) / 8192))[j]'hj_lt_comp :=
      List.getElem_of_eq hgrp_i _
    rw [List.getElem_take, List.getElem_drop, alignedChunkLeafHashes_getElem] at hget_eq
    have hbp_eq : i * rsv.read_size.val + j * 8192 = k * 8192 := by
      rw [hrs_eq_K, ← hk_decomp]; ring
    have hoff_k_eq : offset.val + k * 8192 = (offset.val / 8192 + k) * 8192 := by
      rw [Nat.add_mul, Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hoff_align8)]
    rw [hbp_eq, hoff_k_eq] at hget_eq
    simpa [hidx_eq] using hget_eq
  have hzero_after_hb := drop_min_len_eq_zeroPad_of_div_ceil_le hblocks
  rcases aligned_blocks_match_leaves_sound sha256 B slice.val offset.val unaligned_len.val
      hoff_align8 hpage hunz hule hsum64 hzero_after_hb hall_leaves with
      ⟨_, _, hin_B, hpfx, htail0⟩ | hcol
  · exact Or.inl ⟨hunz, hule, hpage, hblocks, hin_B, hpfx, htail0⟩
  · exact Or.inr hcol

/-- **Property P4 (`ReadSizedMerkleVerifier.verify_aligned` Unconditional Soundness)**:
    Without any external `B.length` bound, accepting `(offset, src, unaligned_len)` implies either:
    - `slice[..unaligned_len] = B[offset .. offset + unaligned_len]` AND **every** trailing byte
      `slice[unaligned_len..]` in the page buffer is `0#u8`, or
    - `HashCollision sha256 ∨ HashZeroPreimage sha256`. -/
theorem read_sized_verifier_verify_aligned_sound_of_wf
    {H Src : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {srcInst : AlignedByteSource Src}
    {B : List Std.U8} {rsv : ReadSizedMerkleVerifier}
    {offset : Std.Usize} {src : Src} {slice : Slice Std.U8} {unaligned_len : Std.Usize}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice)
    (hwf : ReadSizedWf rsv)
    (hK : 0 < rsv.read_size.val / 8192)
    (hrsv_h :
      rsv.hashes.val =
        specReadSizedHashes sha256 (rsv.read_size.val / 8192) hK rsv.bytes_per_hash
          (specLeafHashes sha256 B))
    (hok : ReadSizedMerkleVerifier.verify_aligned hInst srcInst rsv offset src unaligned_len =
             ok (.Ok ())) :
    (0 < unaligned_len.val ∧
     unaligned_len.val ≤ slice.val.length ∧
     slice.val.length % MIN_PAGE_BYTES = 0 ∧
     (slice.val.length + (BLOCK_BYTES - 1)) / BLOCK_BYTES ≤
       (unaligned_len.val + (BLOCK_BYTES - 1)) / BLOCK_BYTES ∧
     offset.val + unaligned_len.val ≤ B.length ∧
     slice.val.take unaligned_len.val = (B.drop offset.val).take unaligned_len.val ∧
     slice.val.drop unaligned_len.val = specZeroPad (slice.val.length - unaligned_len.val)) ∨
    HashCollision sha256 ∨ HashZeroPreimage sha256 := by
  obtain ⟨hsum, hoff, hpage, hunz, hule, hblocks, hend_nc, hall_chunks⟩ :=
    verify_aligned_extract_ok hsha hsrc hwf hok
  set K := rsv.read_size.val / 8192
  set hstart := offset.val / rsv.read_size.val
  set nchunks := (unaligned_len.val + rsv.read_size.val - 1) / rsv.read_size.val
  have hrs_eq_K : rsv.read_size.val = K * 8192 :=
    (Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hwf.hread_aligned)).symm
  by_cases hbad_chunk :
      ∃ i < nchunks,
        ((specLeafHashes sha256 B).drop ((hstart + i) * K)).take K ≠
        alignedChunkLeafHashes sha256 offset.val slice.val slice.val.length unaligned_len.val
          (i * rsv.read_size.val)
          ((min rsv.read_size.val (unaligned_len.val - i * rsv.read_size.val) + 8191) / 8192)
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
          (alignedChunkLeafHashes sha256 offset.val slice.val slice.val.length unaligned_len.val
            (i * rsv.read_size.val)
            ((min rsv.read_size.val (unaligned_len.val - i * rsv.read_size.val) + 8191) / 8192)) := by
      have h0_eq : rsv.hashes.val[hstart + i]'hk_in =
          (specReadSizedHashes sha256 K hK rsv.bytes_per_hash
            (specLeafHashes sha256 B))[hstart + i]'(by rw [← hrsv_h]; exact hk_in) :=
        List.getElem_of_eq hrsv_h _
      rw [← hrs_get, ← h0_eq, heq_i]
    have hcomp_le_K :
        (alignedChunkLeafHashes sha256 offset.val slice.val slice.val.length unaligned_len.val
          (i * rsv.read_size.val)
          ((min rsv.read_size.val (unaligned_len.val - i * rsv.read_size.val) + 8191) / 8192)).length ≤ K := by
      rw [alignedChunkLeafHashes_length]
      exact div_ceil_8192_le_of_le_mul (by rw [← hrs_eq_K]; exact Nat.min_le_left _ _)
    rcases specHashInternal_same_id_eq_imp_padded_eq_or_collision
        (List.length_take_le _ _) hcomp_le_K hint_eq with hpad | hcol
    · rcases padded_hashes_eq_imp_eq_or_zero_hash
          (List.length_take_le _ _) hcomp_le_K hpad with heq_lists | hz_grp | hz_comp
      · exact absurd heq_lists hneq_i
      · exact Or.inr (mem_specLeafHashes_eq_zeroHash_imp_zeroPreimage
          (List.mem_of_mem_drop (List.mem_of_mem_take hz_grp)))
      · exact Or.inr (mem_alignedChunkLeafHashes_eq_zeroHash_imp_zeroPreimage hz_comp)
    · exact Or.inl hcol
  · have hall_grp :
        ∀ i < nchunks,
          ((specLeafHashes sha256 B).drop ((hstart + i) * K)).take K =
          alignedChunkLeafHashes sha256 offset.val slice.val slice.val.length unaligned_len.val
            (i * rsv.read_size.val)
            ((min rsv.read_size.val (unaligned_len.val - i * rsv.read_size.val) + 8191) / 8192) := by
      intro i hi_lt; by_contra hneq; exact hbad_chunk ⟨i, hi_lt, hneq⟩
    rcases verify_aligned_groups_eq_imp_sound sha256 B rsv offset slice unaligned_len
        hwf hK hoff hpage hunz hule hblocks hsum hall_grp with hgood | hcol
    · exact Or.inl hgood
    · exact Or.inr (Or.inl hcol)

/-- **Property P4 (`ReadSizedMerkleVerifier.verify_aligned` Size-Bounded Soundness -- `fxblob` Contract)**:
    When the caller enforces `offset + unaligned_len ≤ B.length` and the `read_size` end-alignment
    rule (as `fxblob` does via `uncompressed_size`), soundness holds up to `HashCollision sha256`
    **alone** (with no `HashZeroPreimage` disjunct). -/
theorem read_sized_verifier_verify_aligned_sound_bounded_of_wf
    {H Src : Type} {sha256 : List Std.U8 → Hash}
    {hInst : Sha256State H} {R_H : H → List Std.U8 → Prop}
    {srcInst : AlignedByteSource Src}
    {B : List Std.U8} {rsv : ReadSizedMerkleVerifier}
    {offset : Std.Usize} {src : Src} {slice : Slice Std.U8} {unaligned_len : Std.Usize}
    (hsha : ValidSha256State sha256 hInst R_H)
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice)
    (hwf : ReadSizedWf rsv)
    (hK : 0 < rsv.read_size.val / 8192)
    (hrsv_h :
      rsv.hashes.val =
        specReadSizedHashes sha256 (rsv.read_size.val / 8192) hK rsv.bytes_per_hash
          (specLeafHashes sha256 B))
    (hin_blob : offset.val + unaligned_len.val ≤ B.length)
    (halign_end :
      (offset.val + unaligned_len.val) % rsv.read_size.val = 0 ∨
      offset.val + unaligned_len.val = B.length)
    (hok : ReadSizedMerkleVerifier.verify_aligned hInst srcInst rsv offset src unaligned_len =
             ok (.Ok ())) :
    (0 < unaligned_len.val ∧
     unaligned_len.val ≤ slice.val.length ∧
     slice.val.length % MIN_PAGE_BYTES = 0 ∧
     (slice.val.length + (BLOCK_BYTES - 1)) / BLOCK_BYTES ≤
       (unaligned_len.val + (BLOCK_BYTES - 1)) / BLOCK_BYTES ∧
     slice.val.take unaligned_len.val = (B.drop offset.val).take unaligned_len.val ∧
     slice.val.drop unaligned_len.val = specZeroPad (slice.val.length - unaligned_len.val)) ∨
    HashCollision sha256 := by
  obtain ⟨hsum, hoff, hpage, hunz, hule, hblocks, hend_nc, hall_chunks⟩ :=
    verify_aligned_extract_ok hsha hsrc hwf hok
  have hBne : B ≠ [] := fun h => by subst h; simp only [List.length_nil] at hin_blob; omega
  set K := rsv.read_size.val / 8192
  set hstart := offset.val / rsv.read_size.val
  set nchunks := (unaligned_len.val + rsv.read_size.val - 1) / rsv.read_size.val
  have hrs_eq_K : rsv.read_size.val = K * 8192 :=
    (Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hwf.hread_aligned)).symm
  by_cases hbad_chunk :
      ∃ i < nchunks,
        ((specLeafHashes sha256 B).drop ((hstart + i) * K)).take K ≠
        alignedChunkLeafHashes sha256 offset.val slice.val slice.val.length unaligned_len.val
          (i * rsv.read_size.val)
          ((min rsv.read_size.val (unaligned_len.val - i * rsv.read_size.val) + 8191) / 8192)
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
          (alignedChunkLeafHashes sha256 offset.val slice.val slice.val.length unaligned_len.val
            (i * rsv.read_size.val)
            ((min rsv.read_size.val (unaligned_len.val - i * rsv.read_size.val) + 8191) / 8192)) := by
      have h0_eq : rsv.hashes.val[hstart + i]'hk_in =
          (specReadSizedHashes sha256 K hK rsv.bytes_per_hash
            (specLeafHashes sha256 B))[hstart + i]'(by rw [← hrsv_h]; exact hk_in) :=
        List.getElem_of_eq hrsv_h _
      rw [← hrs_get, ← h0_eq, heq_i]
    have hcomp_le_K :
        (alignedChunkLeafHashes sha256 offset.val slice.val slice.val.length unaligned_len.val
          (i * rsv.read_size.val)
          ((min rsv.read_size.val (unaligned_len.val - i * rsv.read_size.val) + 8191) / 8192)).length ≤ K := by
      rw [alignedChunkLeafHashes_length]
      exact div_ceil_8192_le_of_le_mul (by rw [← hrs_eq_K]; exact Nat.min_le_left _ _)
    have hlen_grp_comp :
        (((specLeafHashes sha256 B).drop ((hstart + i) * K)).take K).length =
        (alignedChunkLeafHashes sha256 offset.val slice.val slice.val.length unaligned_len.val
          (i * rsv.read_size.val)
          ((min rsv.read_size.val (unaligned_len.val - i * rsv.read_size.val) + 8191) / 8192)).length := by
      have hsl_len : (specLeafHashes sha256 B).length = (B.length + 8191) / 8192 := by
        rw [specLeafHashes_length, List.isEmpty_eq_false_iff.mpr hBne]; simp
      have hi_mul_lt : i * rsv.read_size.val < unaligned_len.val :=
        mul_lt_of_lt_div_ceil hwf.hread_pos hi_lt
      have hbase_mul : ((hstart + i) * K) * 8192 = offset.val + i * rsv.read_size.val := by
        have hoff_eq : hstart * rsv.read_size.val = offset.val :=
          Nat.div_mul_cancel (Nat.dvd_of_mod_eq_zero hoff)
        calc ((hstart + i) * K) * 8192
            = hstart * (K * 8192) + i * (K * 8192) := by ring
          _ = offset.val + i * rsv.read_size.val := by rw [← hrs_eq_K, hoff_eq]
      rw [alignedChunkLeafHashes_length, List.length_take, List.length_drop, hsl_len]
      rcases halign_end with hmod0 | hendB
      · have hu_mod0 : unaligned_len.val % rsv.read_size.val = 0 := by
          have hq : offset.val = rsv.read_size.val * (offset.val / rsv.read_size.val) :=
            (Nat.mul_div_cancel' (Nat.dvd_of_mod_eq_zero hoff)).symm
          have hrew : offset.val + unaligned_len.val =
              unaligned_len.val + rsv.read_size.val * (offset.val / rsv.read_size.val) := by omega
          rw [hrew, Nat.add_mul_mod_self_left] at hmod0
          exact hmod0
        have hi1_le : (i + 1) * rsv.read_size.val ≤ unaligned_len.val := by
          have hu_div : nchunks = unaligned_len.val / rsv.read_size.val := by
            dsimp [nchunks]
            have hq : unaligned_len.val =
                rsv.read_size.val * (unaligned_len.val / rsv.read_size.val) :=
              (Nat.mul_div_cancel' (Nat.dvd_of_mod_eq_zero hu_mod0)).symm
            have h1' : (rsv.read_size.val - 1) / rsv.read_size.val = 0 :=
              Nat.div_eq_of_lt (by have := hwf.hread_pos; omega)
            calc (unaligned_len.val + rsv.read_size.val - 1) / rsv.read_size.val
                = (rsv.read_size.val * (unaligned_len.val / rsv.read_size.val) +
                    (rsv.read_size.val - 1)) / rsv.read_size.val := by congr 1; omega
              _ = unaligned_len.val / rsv.read_size.val := by
                    rw [Nat.mul_add_div hwf.hread_pos, h1', Nat.add_zero]
          have hi_lt' : i < unaligned_len.val / rsv.read_size.val := by
            rw [← hu_div]; exact hi_lt
          exact (Nat.le_div_iff_mul_le hwf.hread_pos).mp hi_lt'
        rw [Nat.succ_mul] at hi1_le
        have hmin_u : min rsv.read_size.val (unaligned_len.val - i * rsv.read_size.val) = K * 8192 := by
          rw [Nat.min_eq_left (by omega), hrs_eq_K]
        have h1 : (K * 8192 + 8191) / 8192 = K := by
          have hrew : K * 8192 + 8191 = 8192 * K + 8191 := by ring
          rw [hrew, Nat.mul_add_div (by decide : 0 < 8192)]; simp
        have h2 : K ≤ (B.length + 8191) / 8192 - (hstart + i) * K := by
          have hmul : ((hstart + i) * K + K) * 8192 ≤ B.length + 8191 := by
            rw [Nat.add_mul, hbase_mul, ← hrs_eq_K]; omega
          have hdiv : (hstart + i) * K + K ≤ (B.length + 8191) / 8192 :=
            (Nat.le_div_iff_mul_le (by decide : 0 < 8192)).mpr hmul
          omega
        rw [hmin_u, h1, Nat.min_eq_left h2]
      · have hu_diff : unaligned_len.val - i * rsv.read_size.val =
            B.length - ((hstart + i) * K) * 8192 := by omega
        rw [hu_diff, hrs_eq_K]
        exact (min_div_ceil_eq hK (by omega)).symm
    rcases specHashInternal_same_id_eq_imp_padded_eq_or_collision
        (List.length_take_le _ _) hcomp_le_K hint_eq with hpad | hcol
    · exact absurd (List.append_inj hpad hlen_grp_comp).1 hneq_i
    · exact hcol
  · have hall_grp :
        ∀ i < nchunks,
          ((specLeafHashes sha256 B).drop ((hstart + i) * K)).take K =
          alignedChunkLeafHashes sha256 offset.val slice.val slice.val.length unaligned_len.val
            (i * rsv.read_size.val)
            ((min rsv.read_size.val (unaligned_len.val - i * rsv.read_size.val) + 8191) / 8192) := by
      intro i hi_lt; by_contra hneq; exact hbad_chunk ⟨i, hi_lt, hneq⟩
    rcases verify_aligned_groups_eq_imp_sound sha256 B rsv offset slice unaligned_len
        hwf hK hoff hpage hunz hule hblocks hsum hall_grp with
        ⟨_, _, _, _, _, hpfx, htail0⟩ | hcol
    · exact Or.inl ⟨hunz, hule, hpage, hblocks, hpfx, htail0⟩
    · exact Or.inr hcol

end FuchsiaMerkle
