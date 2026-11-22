-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Assumptions
import Spec
import Extracted
import Proof.SpecLemmas
import Proof.Sha256Sim
import Proof.BuilderProofs
import Proof.VerifierCommon
import Proof.MerkleVerifierProofs
import Proof.ReadSizedSliceProofs
import Proof.ReadSizedAlignedProofs

/-!
# Correctness Proofs for `fuchsia_merkle_core` (`proof/Proof.lean`)

Provides proof entry points discharging every theorem in `spec/Contract.lean`.
-/

namespace FuchsiaMerkle.Proof

open Aeneas.Std
open FuchsiaMerkle.Assumptions
open FuchsiaMerkle.Spec

private theorem rebuildRootSpec_of_valid
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (hsha : ValidSha256State sha256 hInst R_H) :
    FuchsiaMerkle.RebuildRootSpec sha256 hInst :=
  fun s hs => FuchsiaMerkle.rebuild_root_gen_spec sha256 hInst R_H s hsha hs

-- ============================================================================
-- Property P0: Block Identity Header Injectivity & Domain Separation
-- ============================================================================

theorem make_identity_injective
    (len₁ len₂ : U32) (lvl₁ lvl₂ off₁ off₂ : U64)
    (id₁ id₂ : Aeneas.Std.Array U8 12#usize)
    (hoff₁ : off₁.val % BLOCK_BYTES = 0)
    (hoff₂ : off₂.val % BLOCK_BYTES = 0)
    (hlvl₁ : lvl₁.val < BLOCK_BYTES)
    (hlvl₂ : lvl₂.val < BLOCK_BYTES)
    (h₁ : fuchsia_merkle_core.make_identity len₁ lvl₁ off₁ = .ok id₁)
    (h₂ : fuchsia_merkle_core.make_identity len₂ lvl₂ off₂ = .ok id₂)
    (heq : id₁ = id₂) :
    len₁ = len₂ ∧ lvl₁ = lvl₂ ∧ off₁ = off₂ := by
  exact FuchsiaMerkle.make_identity_injective_lem hoff₁ hoff₂ hlvl₁ hlvl₂ h₁ h₂ heq

theorem leaf_internal_domain_separation
    (len₁ len₂ : U32) (lvl off₁ off₂ : U64)
    (rest₁ rest₂ : List U8)
    (hoff₁ : off₁.val % BLOCK_BYTES = 0)
    (hoff₂ : off₂.val % BLOCK_BYTES = 0)
    (hlvl_pos : 0 < lvl.val)
    (hlvl_lt : lvl.val < BLOCK_BYTES) :
    specIdentity len₁ 0#u64 off₁ ++ rest₁ ≠
      specIdentity len₂ lvl off₂ ++ rest₂ := by
  exact FuchsiaMerkle.leaf_internal_domain_separation_lem
    len₁ len₂ lvl off₁ off₂ rest₁ rest₂ hoff₁ hoff₂ hlvl_pos hlvl_lt

theorem specRootFromLeaves_eq_imp_eq_or_collision
    (sha256 : List U8 → Hash)
    (leaves₁ leaves₂ : List Hash)
    (hlen : leaves₁.length = leaves₂.length)
    (hbound : leaves₁.length * BLOCK_BYTES ≤ U64.max)
    (heq : specRootFromLeaves sha256 leaves₁ = specRootFromLeaves sha256 leaves₂) :
    leaves₁ = leaves₂ ∨ HashCollision sha256 := by
  exact FuchsiaMerkle.specRootFromLeaves_eq_imp_eq_or_collision_lem hlen hbound heq

-- ============================================================================
-- Property P1: Total Panic & Arithmetic Overflow Freedom
-- ============================================================================

theorem merkle_verifier_new_no_panic
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (root : Hash) (hashes : Slice Hash)
    (hsha : ValidSha256State sha256 hInst R_H) :
    ∃ res, fuchsia_merkle_core.MerkleVerifier.new hInst root hashes = .ok res := by
  exact FuchsiaMerkle.merkle_verifier_new_no_panic_of_rebuild
    (rebuildRootSpec_of_valid sha256 hInst R_H hsha) root hashes

theorem merkle_verifier_verify_no_panic
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (mv : fuchsia_merkle_core.MerkleVerifier) (offset : Usize) (data : Slice U8)
    (hsha : ValidSha256State sha256 hInst R_H) :
    ∃ res, fuchsia_merkle_core.MerkleVerifier.verify hInst mv offset data = .ok res := by
  exact FuchsiaMerkle.merkle_verifier_verify_no_panic_lem hsha mv offset data

theorem merkle_verifier_verify_aligned_no_panic
    {Src : Type} (sha256 : List U8 → Hash)
    (srcInst : fuchsia_merkle_core.AlignedByteSource Src)
    (mv : fuchsia_merkle_core.MerkleVerifier) (offset : Usize)
    (src : Src) (slice : Slice U8) (unaligned_len : Usize)
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice) :
    ∃ res,
      fuchsia_merkle_core.MerkleVerifier.verify_aligned srcInst mv offset src unaligned_len =
        .ok res := by
  exact FuchsiaMerkle.merkle_verifier_verify_aligned_no_panic_lem hsrc mv offset unaligned_len

theorem read_sized_verifier_new_no_panic
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (root : Hash) (hashes : Slice Hash)
    (mv : fuchsia_merkle_core.MerkleVerifier) (read_size : Usize)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hmv : fuchsia_merkle_core.MerkleVerifier.new hInst root hashes = .ok (.Ok mv)) :
    ∃ res,
      fuchsia_merkle_core.ReadSizedMerkleVerifier.new hInst mv read_size = .ok res := by
  obtain ⟨hmv_inv, _, _⟩ :=
    FuchsiaMerkle.merkle_verifier_new_inv_of_rebuild
      (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hmv
  exact FuchsiaMerkle.read_sized_verifier_new_no_panic_of_inv hsha hmv_inv read_size

theorem read_sized_verifier_verify_no_panic
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (root : Hash) (hashes : Slice Hash)
    (mv : fuchsia_merkle_core.MerkleVerifier) (read_size : Usize)
    (rsv : fuchsia_merkle_core.ReadSizedMerkleVerifier)
    (offset : Usize) (data : Slice U8)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hmv : fuchsia_merkle_core.MerkleVerifier.new hInst root hashes = .ok (.Ok mv))
    (hrsv : fuchsia_merkle_core.ReadSizedMerkleVerifier.new hInst mv read_size = .ok (.Ok rsv)) :
    ∃ res,
      fuchsia_merkle_core.ReadSizedMerkleVerifier.verify hInst rsv offset data = .ok res := by
  obtain ⟨hmv_inv, _, _⟩ :=
    FuchsiaMerkle.merkle_verifier_new_inv_of_rebuild
      (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hmv
  obtain ⟨hwf, _, _, _⟩ :=
    FuchsiaMerkle.read_sized_verifier_new_inv hsha hmv_inv hrsv
  exact FuchsiaMerkle.read_sized_verifier_verify_no_panic_of_wf hsha hwf offset data

theorem read_sized_verifier_verify_aligned_no_panic
    {H Src : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (srcInst : fuchsia_merkle_core.AlignedByteSource Src)
    (root : Hash) (hashes : Slice Hash)
    (mv : fuchsia_merkle_core.MerkleVerifier) (read_size : Usize)
    (rsv : fuchsia_merkle_core.ReadSizedMerkleVerifier)
    (offset : Usize) (src : Src) (slice : Slice U8) (unaligned_len : Usize)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice)
    (hmv : fuchsia_merkle_core.MerkleVerifier.new hInst root hashes = .ok (.Ok mv))
    (hrsv : fuchsia_merkle_core.ReadSizedMerkleVerifier.new hInst mv read_size = .ok (.Ok rsv)) :
    ∃ res,
      fuchsia_merkle_core.ReadSizedMerkleVerifier.verify_aligned
        hInst srcInst rsv offset src unaligned_len = .ok res := by
  obtain ⟨hmv_inv, _, _⟩ :=
    FuchsiaMerkle.merkle_verifier_new_inv_of_rebuild
      (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hmv
  obtain ⟨hwf, _, _, _⟩ :=
    FuchsiaMerkle.read_sized_verifier_new_inv hsha hmv_inv hrsv
  exact FuchsiaMerkle.read_sized_verifier_verify_aligned_no_panic_of_wf
    hsha hsrc hwf offset unaligned_len

-- ============================================================================
-- Property P2: Block Hashing, Streaming Builder Correctness, & Chunk Invariance
-- ============================================================================

theorem hash_block_spec
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (block : Slice U8) (offset : U64)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hlen : block.val.length ≤ BLOCK_BYTES)
    (hoff : offset.val % BLOCK_BYTES = 0) :
    fuchsia_merkle_core.hash_block hInst block offset =
      .ok (specHashBlock sha256 block.val offset) := by
  exact FuchsiaMerkle.hash_block_gen_spec sha256 hInst R_H block offset hsha hlen hoff

theorem hash_block_aligned_spec
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (offset : U64) (data : Slice U8) (buf_len u_len : Usize)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hbuf : buf_len.val = BLOCK_BYTES ∨ buf_len.val = MIN_PAGE_BYTES)
    (hpos : 0 < u_len.val)
    (hule : u_len.val ≤ buf_len.val)
    (hoff : offset.val % BLOCK_BYTES = 0)
    (hdlen : buf_len.val ≤ data.val.length) :
    fuchsia_merkle_core.hash_block_aligned hInst offset data buf_len u_len =
      .ok (sha256 (specIdentity (u32OfNat u_len.val) 0#u64 offset ++
        data.val.take buf_len.val ++
        specZeroPad (BLOCK_BYTES - buf_len.val))) := by
  exact FuchsiaMerkle.hash_block_aligned_gen_spec
    sha256 hInst R_H offset data buf_len u_len hsha hbuf hpos hule hoff hdlen

theorem rebuild_root_spec
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (leaf_hashes : Slice Hash)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hbound : leaf_hashes.val.length * BLOCK_BYTES ≤ U64.max) :
    fuchsia_merkle_core.rebuild_root hInst leaf_hashes =
      .ok (specRootFromLeaves sha256 leaf_hashes.val) := by
  exact FuchsiaMerkle.rebuild_root_gen_spec sha256 hInst R_H leaf_hashes hsha hbound

theorem merkle_root_builder_complete_spec
    {H C : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (cInst : fuchsia_merkle_core.LeafCollector C) (maxLeaves : Nat)
    (R_C : C → List Hash → Prop)
    (c₀ : C) (data : Slice U8)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hcol : ValidLeafCollector cInst maxLeaves R_C)
    (hc₀ : R_C c₀ [])
    (hcap : (specLeafHashes sha256 data.val).length ≤ maxLeaves)
    (hbound : ((data.val.length + (BLOCK_BYTES - 1)) / BLOCK_BYTES) * BLOCK_BYTES ≤ U64.max) :
    ∃ c',
      (do
        let b ← fuchsia_merkle_core.MerkleRootBuilder.new hInst cInst c₀
        fuchsia_merkle_core.MerkleRootBuilder.complete hInst cInst b data) =
        .ok (specMerkleRoot sha256 data.val, c') ∧
      R_C c' (specLeafHashes sha256 data.val) := by
  exact FuchsiaMerkle.merkle_root_builder_complete_gen_spec
    sha256 hInst R_H cInst maxLeaves R_C c₀ data hsha hcol hc₀ hcap hbound

theorem buffered_streaming_spec
    {H C : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (cInst : fuchsia_merkle_core.LeafCollector C) (maxLeaves : Nat)
    (R_C : C → List Hash → Prop)
    (c₀ : C) (chunks : List (Slice U8))
    (hsha : ValidSha256State sha256 hInst R_H)
    (hcol : ValidLeafCollector cInst maxLeaves R_C)
    (hc₀ : R_C c₀ [])
    (hcap : (specLeafHashes sha256 (chunks.flatMap Slice.val)).length ≤ maxLeaves)
    (hbound :
      (((chunks.flatMap Slice.val).length + (BLOCK_BYTES - 1)) / BLOCK_BYTES) *
        BLOCK_BYTES ≤ U64.max) :
    ∃ c',
      (do
        let b₀ ← fuchsia_merkle_core.BufferedMerkleRootBuilder.new hInst cInst c₀
        let b₁ ← bufferedWriteAll hInst cInst b₀ chunks
        fuchsia_merkle_core.BufferedMerkleRootBuilder.complete hInst cInst b₁) =
        .ok (specMerkleRoot sha256 (chunks.flatMap Slice.val), c') ∧
      R_C c' (specLeafHashes sha256 (chunks.flatMap Slice.val)) := by
  exact FuchsiaMerkle.buffered_streaming_gen_spec
    sha256 hInst R_H cInst maxLeaves R_C c₀ chunks hsha hcol hc₀ hcap hbound

theorem buffered_chunk_invariance
    (sha256 : List U8 → Hash)
    (chunks₁ chunks₂ : List (Slice U8))
    (hsame : chunks₁.flatMap Slice.val = chunks₂.flatMap Slice.val)
    (hbound :
      (((chunks₁.flatMap Slice.val).length + (BLOCK_BYTES - 1)) / BLOCK_BYTES) *
        BLOCK_BYTES ≤ U64.max) :
    (do
      let b₀ ← fuchsia_merkle_core.BufferedMerkleRootBuilder.new
        (StreamSha256 sha256) ListLeafCollector []
      let b₁ ← bufferedWriteAll (StreamSha256 sha256) ListLeafCollector b₀ chunks₁
      fuchsia_merkle_core.BufferedMerkleRootBuilder.complete
        (StreamSha256 sha256) ListLeafCollector b₁) =
    (do
      let b₀ ← fuchsia_merkle_core.BufferedMerkleRootBuilder.new
        (StreamSha256 sha256) ListLeafCollector []
      let b₁ ← bufferedWriteAll (StreamSha256 sha256) ListLeafCollector b₀ chunks₂
      fuchsia_merkle_core.BufferedMerkleRootBuilder.complete
        (StreamSha256 sha256) ListLeafCollector b₁) := by
  exact FuchsiaMerkle.buffered_chunk_invariance_lem sha256 chunks₁ chunks₂ hsame hbound

-- ============================================================================
-- Property P3: Verifier Completeness Across Slice & Page-Aligned Buffers
-- ============================================================================

theorem merkle_verifier_new_complete
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (B : List U8) (hashes : Slice Hash)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hleaves : hashes.val = specLeafHashes sha256 B)
    (hbound : hashes.val.length * BLOCK_BYTES ≤ U64.max) :
    ∃ mv,
      fuchsia_merkle_core.MerkleVerifier.new hInst (specMerkleRoot sha256 B) hashes =
        .ok (.Ok mv) := by
  exact FuchsiaMerkle.merkle_verifier_new_complete_of_rebuild
    (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hleaves hbound

theorem merkle_verifier_verify_complete_empty
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (hashes : Slice Hash) (mv : fuchsia_merkle_core.MerkleVerifier)
    (data : Slice U8)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hleaves : hashes.val = specLeafHashes sha256 [])
    (hmv :
      fuchsia_merkle_core.MerkleVerifier.new hInst (specMerkleRoot sha256 []) hashes =
        .ok (.Ok mv))
    (hdata : data.val = []) :
    fuchsia_merkle_core.MerkleVerifier.verify hInst mv 0#usize data =
      .ok (.Ok ()) := by
  obtain ⟨hmv_inv, hmv_h, _⟩ :=
    FuchsiaMerkle.merkle_verifier_new_inv_of_rebuild
      (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hmv
  exact FuchsiaMerkle.merkle_verifier_verify_complete_empty_of_inv
    hsha hmv_inv (by rw [hmv_h, hleaves]) hdata

theorem merkle_verifier_verify_complete
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (B : List U8) (hashes : Slice Hash)
    (mv : fuchsia_merkle_core.MerkleVerifier)
    (offset : Usize) (data : Slice U8)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hleaves : hashes.val = specLeafHashes sha256 B)
    (hmv :
      fuchsia_merkle_core.MerkleVerifier.new hInst (specMerkleRoot sha256 B) hashes =
        .ok (.Ok mv))
    (hoff : offset.val % BLOCK_BYTES = 0)
    (hdne : data.val ≠ [])
    (hsum_usize : offset.val + data.val.length ≤ Usize.max)
    (hin_blob : offset.val + data.val.length ≤ B.length)
    (halign_end :
      (offset.val + data.val.length) % BLOCK_BYTES = 0 ∨
      offset.val + data.val.length = B.length)
    (hslice_eq : data.val = (B.drop offset.val).take data.val.length) :
    fuchsia_merkle_core.MerkleVerifier.verify hInst mv offset data =
      .ok (.Ok ()) := by
  obtain ⟨hmv_inv, hmv_h, _⟩ :=
    FuchsiaMerkle.merkle_verifier_new_inv_of_rebuild
      (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hmv
  exact FuchsiaMerkle.merkle_verifier_verify_complete_of_inv
    hsha hmv_inv (by rw [hmv_h, hleaves])
    hoff hdne hsum_usize hin_blob halign_end hslice_eq

theorem merkle_verifier_verify_aligned_complete
    {H Src : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (srcInst : fuchsia_merkle_core.AlignedByteSource Src)
    (B : List U8) (hashes : Slice Hash)
    (mv : fuchsia_merkle_core.MerkleVerifier)
    (offset : Usize) (src : Src) (slice : Slice U8) (unaligned_len : Usize)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice)
    (hleaves : hashes.val = specLeafHashes sha256 B)
    (hmv :
      fuchsia_merkle_core.MerkleVerifier.new hInst (specMerkleRoot sha256 B) hashes =
        .ok (.Ok mv))
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
    fuchsia_merkle_core.MerkleVerifier.verify_aligned srcInst mv offset src unaligned_len =
      .ok (.Ok ()) := by
  obtain ⟨hmv_inv, hmv_h, _⟩ :=
    FuchsiaMerkle.merkle_verifier_new_inv_of_rebuild
      (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hmv
  exact FuchsiaMerkle.merkle_verifier_verify_aligned_complete_of_inv
    hsrc hmv_inv (by rw [hmv_h, hleaves])
    hoff hpage hunz hule hblocks hsum_usize hin_blob halign_end hprefix htail_zero

theorem read_sized_verifier_new_complete
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (root : Hash) (hashes : Slice Hash)
    (mv : fuchsia_merkle_core.MerkleVerifier) (read_size : Usize)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hmv : fuchsia_merkle_core.MerkleVerifier.new hInst root hashes = .ok (.Ok mv))
    (hrs_pos : 0 < read_size.val)
    (hrs_align : read_size.val % BLOCK_BYTES = 0)
    (hbph_bound : (read_size.val / BLOCK_BYTES) * HASH_BYTES ≤ U32.max) :
    ∃ rsv,
      fuchsia_merkle_core.ReadSizedMerkleVerifier.new hInst mv read_size =
        .ok (.Ok rsv) := by
  obtain ⟨hmv_inv, _, _⟩ :=
    FuchsiaMerkle.merkle_verifier_new_inv_of_rebuild
      (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hmv
  exact FuchsiaMerkle.read_sized_verifier_new_complete_of_inv
    hsha hmv_inv hrs_pos hrs_align hbph_bound

theorem read_sized_verifier_verify_complete_empty
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (hashes : Slice Hash) (mv : fuchsia_merkle_core.MerkleVerifier)
    (read_size : Usize) (rsv : fuchsia_merkle_core.ReadSizedMerkleVerifier)
    (data : Slice U8)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hleaves : hashes.val = specLeafHashes sha256 [])
    (hmv :
      fuchsia_merkle_core.MerkleVerifier.new hInst (specMerkleRoot sha256 []) hashes =
        .ok (.Ok mv))
    (hrsv :
      fuchsia_merkle_core.ReadSizedMerkleVerifier.new hInst mv read_size = .ok (.Ok rsv))
    (hdata : data.val = []) :
    fuchsia_merkle_core.ReadSizedMerkleVerifier.verify hInst rsv 0#usize data =
      .ok (.Ok ()) := by
  obtain ⟨hmv_inv, hmv_h, _⟩ :=
    FuchsiaMerkle.merkle_verifier_new_inv_of_rebuild
      (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hmv
  obtain ⟨hwf, hrs_eq, _, hK, hrsv_h⟩ :=
    FuchsiaMerkle.read_sized_verifier_new_inv hsha hmv_inv hrsv
  cases hrs_eq
  rw [hmv_h, hleaves] at hrsv_h
  exact FuchsiaMerkle.read_sized_verifier_verify_complete_empty_of_wf
    hsha hwf hK hrsv_h hdata

theorem read_sized_verifier_verify_complete
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (B : List U8) (hashes : Slice Hash)
    (mv : fuchsia_merkle_core.MerkleVerifier)
    (read_size : Usize) (rsv : fuchsia_merkle_core.ReadSizedMerkleVerifier)
    (offset : Usize) (data : Slice U8)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hleaves : hashes.val = specLeafHashes sha256 B)
    (hmv :
      fuchsia_merkle_core.MerkleVerifier.new hInst (specMerkleRoot sha256 B) hashes =
        .ok (.Ok mv))
    (hrsv :
      fuchsia_merkle_core.ReadSizedMerkleVerifier.new hInst mv read_size = .ok (.Ok rsv))
    (hdne : data.val ≠ [])
    (hoff_rs : offset.val % read_size.val = 0)
    (hsum_usize : offset.val + data.val.length ≤ Usize.max)
    (hin_blob : offset.val + data.val.length ≤ B.length)
    (hdata : data.val = (B.drop offset.val).take data.val.length)
    (halign_end :
      (offset.val + data.val.length) % read_size.val = 0 ∨
      offset.val + data.val.length = B.length) :
    fuchsia_merkle_core.ReadSizedMerkleVerifier.verify hInst rsv offset data =
      .ok (.Ok ()) := by
  obtain ⟨hmv_inv, hmv_h, _⟩ :=
    FuchsiaMerkle.merkle_verifier_new_inv_of_rebuild
      (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hmv
  obtain ⟨hwf, hrs_eq, _, hK, hrsv_h⟩ :=
    FuchsiaMerkle.read_sized_verifier_new_inv hsha hmv_inv hrsv
  cases hrs_eq
  rw [hmv_h, hleaves] at hrsv_h
  exact FuchsiaMerkle.read_sized_verifier_verify_complete_of_wf
    hsha hwf hK hrsv_h hdne hoff_rs hsum_usize hin_blob hdata halign_end

theorem read_sized_verifier_verify_aligned_complete
    {H Src : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (srcInst : fuchsia_merkle_core.AlignedByteSource Src)
    (B : List U8) (hashes : Slice Hash)
    (mv : fuchsia_merkle_core.MerkleVerifier)
    (read_size : Usize) (rsv : fuchsia_merkle_core.ReadSizedMerkleVerifier)
    (offset : Usize) (src : Src) (slice : Slice U8) (unaligned_len : Usize)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice)
    (hleaves : hashes.val = specLeafHashes sha256 B)
    (hmv :
      fuchsia_merkle_core.MerkleVerifier.new hInst (specMerkleRoot sha256 B) hashes =
        .ok (.Ok mv))
    (hrsv :
      fuchsia_merkle_core.ReadSizedMerkleVerifier.new hInst mv read_size = .ok (.Ok rsv))
    (hoff_rs : offset.val % read_size.val = 0)
    (hpage : slice.val.length % MIN_PAGE_BYTES = 0)
    (hunz : 0 < unaligned_len.val)
    (hule : unaligned_len.val ≤ slice.val.length)
    (hblocks :
      (slice.val.length + (BLOCK_BYTES - 1)) / BLOCK_BYTES ≤
        (unaligned_len.val + (BLOCK_BYTES - 1)) / BLOCK_BYTES)
    (hsum_usize : offset.val + slice.val.length ≤ Usize.max)
    (hin_blob : offset.val + unaligned_len.val ≤ B.length)
    (hprefix : slice.val.take unaligned_len.val = (B.drop offset.val).take unaligned_len.val)
    (htail_zero :
      slice.val.drop unaligned_len.val = specZeroPad (slice.val.length - unaligned_len.val))
    (halign_end :
      (offset.val + unaligned_len.val) % read_size.val = 0 ∨
      offset.val + unaligned_len.val = B.length) :
    fuchsia_merkle_core.ReadSizedMerkleVerifier.verify_aligned
      hInst srcInst rsv offset src unaligned_len = .ok (.Ok ()) := by
  obtain ⟨hmv_inv, hmv_h, _⟩ :=
    FuchsiaMerkle.merkle_verifier_new_inv_of_rebuild
      (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hmv
  obtain ⟨hwf, hrs_eq, _, hK, hrsv_h⟩ :=
    FuchsiaMerkle.read_sized_verifier_new_inv hsha hmv_inv hrsv
  cases hrs_eq
  rw [hmv_h, hleaves] at hrsv_h
  exact FuchsiaMerkle.read_sized_verifier_verify_aligned_complete_of_wf
    hsha hsrc hwf hK hrsv_h hoff_rs hpage hunz hule hblocks hsum_usize
    hin_blob hprefix htail_zero halign_end

-- ============================================================================
-- Property P4: End-to-End Collision-Resistant Soundness & Page-Tail Integrity
-- ============================================================================

theorem merkle_verifier_new_sound
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (B : List U8) (hashes out_hashes : Slice Hash)
    (mv : fuchsia_merkle_core.MerkleVerifier)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hlen : hashes.val.length = (specLeafHashes sha256 B).length)
    (hmv :
      fuchsia_merkle_core.MerkleVerifier.new hInst (specMerkleRoot sha256 B) hashes =
        .ok (.Ok mv))
    (hget : fuchsia_merkle_core.MerkleVerifier.impl.hashes mv = .ok out_hashes) :
    out_hashes.val = specLeafHashes sha256 B ∨ HashCollision sha256 := by
  exact FuchsiaMerkle.merkle_verifier_new_sound_of_rebuild
    (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hlen hmv hget

theorem merkle_verifier_verify_sound
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (B : List U8) (hashes : Slice Hash)
    (mv : fuchsia_merkle_core.MerkleVerifier)
    (offset : Usize) (data : Slice U8)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hlen : hashes.val.length = (specLeafHashes sha256 B).length)
    (hmv :
      fuchsia_merkle_core.MerkleVerifier.new hInst (specMerkleRoot sha256 B) hashes =
        .ok (.Ok mv))
    (hok : fuchsia_merkle_core.MerkleVerifier.verify hInst mv offset data = .ok (.Ok ())) :
    ((data.val = [] → B = [] ∧ offset.val = 0) ∧
     (data.val ≠ [] →
       offset.val + data.val.length ≤ B.length ∧
       data.val = (B.drop offset.val).take data.val.length)) ∨
    HashCollision sha256 := by
  obtain ⟨hmv_inv, hmv_h, hroot_eq⟩ :=
    FuchsiaMerkle.merkle_verifier_new_inv_of_rebuild
      (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hmv
  rcases FuchsiaMerkle.specRootFromLeaves_eq_imp_eq_or_collision_lem
      hlen (hmv_h ▸ hmv_inv.hbound) hroot_eq with hleaves | hcol
  · exact FuchsiaMerkle.merkle_verifier_verify_sound_of_inv
      hsha hmv_inv (by rw [hmv_h, hleaves]) hok
  · exact Or.inr hcol

theorem merkle_verifier_verify_aligned_sound
    {H Src : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (srcInst : fuchsia_merkle_core.AlignedByteSource Src)
    (B : List U8) (hashes : Slice Hash)
    (mv : fuchsia_merkle_core.MerkleVerifier)
    (offset : Usize) (src : Src) (slice : Slice U8) (unaligned_len : Usize)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice)
    (hlen : hashes.val.length = (specLeafHashes sha256 B).length)
    (hmv :
      fuchsia_merkle_core.MerkleVerifier.new hInst (specMerkleRoot sha256 B) hashes =
        .ok (.Ok mv))
    (hok :
      fuchsia_merkle_core.MerkleVerifier.verify_aligned srcInst mv offset src unaligned_len =
        .ok (.Ok ())) :
    (0 < unaligned_len.val ∧
     unaligned_len.val ≤ slice.val.length ∧
     slice.val.length % MIN_PAGE_BYTES = 0 ∧
     (slice.val.length + (BLOCK_BYTES - 1)) / BLOCK_BYTES ≤
       (unaligned_len.val + (BLOCK_BYTES - 1)) / BLOCK_BYTES ∧
     offset.val + unaligned_len.val ≤ B.length ∧
     slice.val.take unaligned_len.val = (B.drop offset.val).take unaligned_len.val ∧
     slice.val.drop unaligned_len.val = specZeroPad (slice.val.length - unaligned_len.val)) ∨
    HashCollision sha256 := by
  obtain ⟨hmv_inv, hmv_h, hroot_eq⟩ :=
    FuchsiaMerkle.merkle_verifier_new_inv_of_rebuild
      (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hmv
  rcases FuchsiaMerkle.specRootFromLeaves_eq_imp_eq_or_collision_lem
      hlen (hmv_h ▸ hmv_inv.hbound) hroot_eq with hleaves | hcol
  · exact FuchsiaMerkle.merkle_verifier_verify_aligned_sound_of_inv
      hsrc hmv_inv (by rw [hmv_h, hleaves]) hok
  · exact Or.inr hcol

theorem read_sized_verifier_new_sound
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (root : Hash) (hashes : Slice Hash)
    (mv : fuchsia_merkle_core.MerkleVerifier)
    (read_size : Usize) (rsv : fuchsia_merkle_core.ReadSizedMerkleVerifier)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hmv : fuchsia_merkle_core.MerkleVerifier.new hInst root hashes = .ok (.Ok mv))
    (hrsv :
      fuchsia_merkle_core.ReadSizedMerkleVerifier.new hInst mv read_size = .ok (.Ok rsv)) :
    0 < read_size.val ∧
    read_size.val % BLOCK_BYTES = 0 ∧
    (read_size.val / BLOCK_BYTES) * HASH_BYTES ≤ U32.max := by
  obtain ⟨hmv_inv, _, _⟩ :=
    FuchsiaMerkle.merkle_verifier_new_inv_of_rebuild
      (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hmv
  exact FuchsiaMerkle.read_sized_verifier_new_sound_of_inv hsha hmv_inv hrsv

theorem read_sized_verifier_verify_sound
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (B : List U8) (hashes : Slice Hash)
    (mv : fuchsia_merkle_core.MerkleVerifier)
    (read_size : Usize) (rsv : fuchsia_merkle_core.ReadSizedMerkleVerifier)
    (offset : Usize) (data : Slice U8)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hlen : hashes.val.length = (specLeafHashes sha256 B).length)
    (hmv :
      fuchsia_merkle_core.MerkleVerifier.new hInst (specMerkleRoot sha256 B) hashes =
        .ok (.Ok mv))
    (hrsv :
      fuchsia_merkle_core.ReadSizedMerkleVerifier.new hInst mv read_size = .ok (.Ok rsv))
    (hok :
      fuchsia_merkle_core.ReadSizedMerkleVerifier.verify hInst rsv offset data =
        .ok (.Ok ())) :
    ((data.val = [] → B = [] ∧ offset.val = 0) ∧
     (data.val ≠ [] →
       offset.val + data.val.length ≤ B.length ∧
       data.val = (B.drop offset.val).take data.val.length)) ∨
    HashCollision sha256 ∨ HashZeroPreimage sha256 := by
  obtain ⟨hmv_inv, hmv_h, hroot_eq⟩ :=
    FuchsiaMerkle.merkle_verifier_new_inv_of_rebuild
      (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hmv
  rcases FuchsiaMerkle.specRootFromLeaves_eq_imp_eq_or_collision_lem
      hlen (hmv_h ▸ hmv_inv.hbound) hroot_eq with hleaves | hcol
  · obtain ⟨hwf, hrs_eq, _, hK, hrsv_h⟩ :=
      FuchsiaMerkle.read_sized_verifier_new_inv hsha hmv_inv hrsv
    cases hrs_eq
    rw [hmv_h, hleaves] at hrsv_h
    exact FuchsiaMerkle.read_sized_verifier_verify_sound_of_wf hsha hwf hK hrsv_h hok
  · exact Or.inr (Or.inl hcol)

theorem read_sized_verifier_verify_sound_bounded
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (B : List U8) (hashes : Slice Hash)
    (mv : fuchsia_merkle_core.MerkleVerifier)
    (read_size : Usize) (rsv : fuchsia_merkle_core.ReadSizedMerkleVerifier)
    (offset : Usize) (data : Slice U8)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hlen : hashes.val.length = (specLeafHashes sha256 B).length)
    (hmv :
      fuchsia_merkle_core.MerkleVerifier.new hInst (specMerkleRoot sha256 B) hashes =
        .ok (.Ok mv))
    (hrsv :
      fuchsia_merkle_core.ReadSizedMerkleVerifier.new hInst mv read_size = .ok (.Ok rsv))
    (hin_blob : offset.val + data.val.length ≤ B.length)
    (halign_end :
      data.val ≠ [] →
        (offset.val + data.val.length) % read_size.val = 0 ∨
        offset.val + data.val.length = B.length)
    (hok :
      fuchsia_merkle_core.ReadSizedMerkleVerifier.verify hInst rsv offset data =
        .ok (.Ok ())) :
    ((data.val = [] → B = [] ∧ offset.val = 0) ∧
     (data.val ≠ [] → data.val = (B.drop offset.val).take data.val.length)) ∨
    HashCollision sha256 := by
  obtain ⟨hmv_inv, hmv_h, hroot_eq⟩ :=
    FuchsiaMerkle.merkle_verifier_new_inv_of_rebuild
      (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hmv
  rcases FuchsiaMerkle.specRootFromLeaves_eq_imp_eq_or_collision_lem
      hlen (hmv_h ▸ hmv_inv.hbound) hroot_eq with hleaves | hcol
  · obtain ⟨hwf, hrs_eq, _, hK, hrsv_h⟩ :=
      FuchsiaMerkle.read_sized_verifier_new_inv hsha hmv_inv hrsv
    cases hrs_eq
    rw [hmv_h, hleaves] at hrsv_h
    exact FuchsiaMerkle.read_sized_verifier_verify_sound_bounded_of_wf
      hsha hwf hK hrsv_h hin_blob halign_end hok
  · exact Or.inr hcol

theorem read_sized_verifier_verify_aligned_sound
    {H Src : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (srcInst : fuchsia_merkle_core.AlignedByteSource Src)
    (B : List U8) (hashes : Slice Hash)
    (mv : fuchsia_merkle_core.MerkleVerifier)
    (read_size : Usize) (rsv : fuchsia_merkle_core.ReadSizedMerkleVerifier)
    (offset : Usize) (src : Src) (slice : Slice U8) (unaligned_len : Usize)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice)
    (hlen : hashes.val.length = (specLeafHashes sha256 B).length)
    (hmv :
      fuchsia_merkle_core.MerkleVerifier.new hInst (specMerkleRoot sha256 B) hashes =
        .ok (.Ok mv))
    (hrsv :
      fuchsia_merkle_core.ReadSizedMerkleVerifier.new hInst mv read_size = .ok (.Ok rsv))
    (hok :
      fuchsia_merkle_core.ReadSizedMerkleVerifier.verify_aligned
        hInst srcInst rsv offset src unaligned_len = .ok (.Ok ())) :
    (0 < unaligned_len.val ∧
     unaligned_len.val ≤ slice.val.length ∧
     slice.val.length % MIN_PAGE_BYTES = 0 ∧
     (slice.val.length + (BLOCK_BYTES - 1)) / BLOCK_BYTES ≤
       (unaligned_len.val + (BLOCK_BYTES - 1)) / BLOCK_BYTES ∧
     offset.val + unaligned_len.val ≤ B.length ∧
     slice.val.take unaligned_len.val = (B.drop offset.val).take unaligned_len.val ∧
     slice.val.drop unaligned_len.val = specZeroPad (slice.val.length - unaligned_len.val)) ∨
    HashCollision sha256 ∨ HashZeroPreimage sha256 := by
  obtain ⟨hmv_inv, hmv_h, hroot_eq⟩ :=
    FuchsiaMerkle.merkle_verifier_new_inv_of_rebuild
      (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hmv
  rcases FuchsiaMerkle.specRootFromLeaves_eq_imp_eq_or_collision_lem
      hlen (hmv_h ▸ hmv_inv.hbound) hroot_eq with hleaves | hcol
  · obtain ⟨hwf, hrs_eq, _, hK, hrsv_h⟩ :=
      FuchsiaMerkle.read_sized_verifier_new_inv hsha hmv_inv hrsv
    cases hrs_eq
    rw [hmv_h, hleaves] at hrsv_h
    exact FuchsiaMerkle.read_sized_verifier_verify_aligned_sound_of_wf
      hsha hsrc hwf hK hrsv_h hok
  · exact Or.inr (Or.inl hcol)

theorem read_sized_verifier_verify_aligned_sound_bounded
    {H Src : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (srcInst : fuchsia_merkle_core.AlignedByteSource Src)
    (B : List U8) (hashes : Slice Hash)
    (mv : fuchsia_merkle_core.MerkleVerifier)
    (read_size : Usize) (rsv : fuchsia_merkle_core.ReadSizedMerkleVerifier)
    (offset : Usize) (src : Src) (slice : Slice U8) (unaligned_len : Usize)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice)
    (hlen : hashes.val.length = (specLeafHashes sha256 B).length)
    (hmv :
      fuchsia_merkle_core.MerkleVerifier.new hInst (specMerkleRoot sha256 B) hashes =
        .ok (.Ok mv))
    (hrsv :
      fuchsia_merkle_core.ReadSizedMerkleVerifier.new hInst mv read_size = .ok (.Ok rsv))
    (hin_blob : offset.val + unaligned_len.val ≤ B.length)
    (halign_end :
      (offset.val + unaligned_len.val) % read_size.val = 0 ∨
      offset.val + unaligned_len.val = B.length)
    (hok :
      fuchsia_merkle_core.ReadSizedMerkleVerifier.verify_aligned
        hInst srcInst rsv offset src unaligned_len = .ok (.Ok ())) :
    (0 < unaligned_len.val ∧
     unaligned_len.val ≤ slice.val.length ∧
     slice.val.length % MIN_PAGE_BYTES = 0 ∧
     (slice.val.length + (BLOCK_BYTES - 1)) / BLOCK_BYTES ≤
       (unaligned_len.val + (BLOCK_BYTES - 1)) / BLOCK_BYTES ∧
     slice.val.take unaligned_len.val = (B.drop offset.val).take unaligned_len.val ∧
     slice.val.drop unaligned_len.val = specZeroPad (slice.val.length - unaligned_len.val)) ∨
    HashCollision sha256 := by
  obtain ⟨hmv_inv, hmv_h, hroot_eq⟩ :=
    FuchsiaMerkle.merkle_verifier_new_inv_of_rebuild
      (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hmv
  rcases FuchsiaMerkle.specRootFromLeaves_eq_imp_eq_or_collision_lem
      hlen (hmv_h ▸ hmv_inv.hbound) hroot_eq with hleaves | hcol
  · obtain ⟨hwf, hrs_eq, _, hK, hrsv_h⟩ :=
      FuchsiaMerkle.read_sized_verifier_new_inv hsha hmv_inv hrsv
    cases hrs_eq
    rw [hmv_h, hleaves] at hrsv_h
    exact FuchsiaMerkle.read_sized_verifier_verify_aligned_sound_bounded_of_wf
      hsha hsrc hwf hK hrsv_h hin_blob halign_end hok
  · exact Or.inr hcol

-- ============================================================================
-- Property P5: Zero-Hash Leaf Extension Characterization
-- ============================================================================

theorem specRootFromLeaves_append_zeroHash
    (sha256 : List U8 → Hash) (leaves : List Hash)
    (hge2 : 2 ≤ leaves.length)
    (hrem : leaves.length % CHILDREN_PER_NODE ≠ 0) :
    specRootFromLeaves sha256 (leaves ++ [zeroHash]) =
      specRootFromLeaves sha256 leaves := by
  exact FuchsiaMerkle.specRootFromLeaves_append_zeroHash_lem sha256 leaves hge2 hrem

theorem merkle_verifier_zero_hash_extension
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (leaves : List Hash) (extHashes : Slice Hash)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hge2 : 2 ≤ leaves.length)
    (hrem : leaves.length % CHILDREN_PER_NODE ≠ 0)
    (hu64 : (leaves.length + 1) * BLOCK_BYTES ≤ U64.max)
    (hext_val : extHashes.val = leaves ++ [zeroHash]) :
    ∃ mv,
      fuchsia_merkle_core.MerkleVerifier.new
        hInst (specRootFromLeaves sha256 leaves) extHashes = .ok (.Ok mv) := by
  exact FuchsiaMerkle.merkle_verifier_zero_hash_extension_of_rebuild
    (rebuildRootSpec_of_valid sha256 hInst R_H hsha) hge2 hrem hu64 hext_val

end FuchsiaMerkle.Proof
