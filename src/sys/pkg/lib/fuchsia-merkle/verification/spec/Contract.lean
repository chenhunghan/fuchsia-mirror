-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Assumptions
import Spec
import Proof
import Extracted

/-!
# Top-Level Verified Contract for `fuchsia_merkle_core` (`spec/Contract.lean`)

This security-critical module (`spec/Contract.lean`, classified as High risk in
`RISK.md` and governed by `../REVIEW.md`) states the top-level theorems for
`//src/sys/pkg/lib/fuchsia-merkle/core`, delegates each proof body to
`FuchsiaMerkle.Proof`, and audits the transitive axiom footprint of every
theorem using `#print axioms`.

Every theorem is stated exclusively in terms of `FuchsiaMerkle.Assumptions`,
`FuchsiaMerkle.Spec`, and the public Rust API surface in `fuchsia_merkle_core`
(without referencing internal proof invariants or private struct fields), and is
paired with a mandatory `**Informal**:` English specification.
-/

namespace FuchsiaMerkle.Contract

open Aeneas.Std
open FuchsiaMerkle.Assumptions
open FuchsiaMerkle.Spec

-- ============================================================================
-- Property P0: Block Identity Header Injectivity & Domain Separation
-- ============================================================================

/--
**Informal**: Identity header injectivity (`make_identity_injective`): for any
8,192-byte-aligned byte offsets `off₁, off₂` and tree levels `lvl₁, lvl₂ < 8192`,
`make_identity` encodes `(length, level, offset)` injectively: equal 12-byte
output arrays imply `len₁ = len₂`, `lvl₁ = lvl₂`, and `off₁ = off₂`.
-/
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
  exact FuchsiaMerkle.Proof.make_identity_injective
    len₁ len₂ lvl₁ lvl₂ off₁ off₂ id₁ id₂
    hoff₁ hoff₂ hlvl₁ hlvl₂ h₁ h₂ heq

#print axioms make_identity_injective

/--
**Informal**: Leaf vs. internal node domain separation
(`leaf_internal_domain_separation`): for any 8,192-byte-aligned offsets
`off₁, off₂`, any internal level `0 < lvl < 8192`, any lengths `len₁, len₂`, and
any trailing payload bytes `rest₁, rest₂`, the level-0 preimage
`specIdentity len₁ 0 off₁ ++ rest₁` is never equal to the internal node preimage
`specIdentity len₂ lvl off₂ ++ rest₂`.
-/
theorem leaf_internal_domain_separation
    (len₁ len₂ : U32) (lvl off₁ off₂ : U64)
    (rest₁ rest₂ : List U8)
    (hoff₁ : off₁.val % BLOCK_BYTES = 0)
    (hoff₂ : off₂.val % BLOCK_BYTES = 0)
    (hlvl_pos : 0 < lvl.val)
    (hlvl_lt : lvl.val < BLOCK_BYTES) :
    specIdentity len₁ 0#u64 off₁ ++ rest₁ ≠
      specIdentity len₂ lvl off₂ ++ rest₂ := by
  exact FuchsiaMerkle.Proof.leaf_internal_domain_separation
    len₁ len₂ lvl off₁ off₂ rest₁ rest₂
    hoff₁ hoff₂ hlvl_pos hlvl_lt

#print axioms leaf_internal_domain_separation

/--
**Informal**: Tree root collision soundness
(`specRootFromLeaves_eq_imp_eq_or_collision`): two level-0 leaf digest lists
`leaves₁` and `leaves₂` of equal length whose byte span `leaves₁.length * 8192`
fits in `U64` produce equal Fuchsia Merkle roots under `sha256` only if
`leaves₁ = leaves₂` or `sha256` admits a collision (`HashCollision sha256`).
-/
theorem specRootFromLeaves_eq_imp_eq_or_collision
    (sha256 : List U8 → Hash)
    (leaves₁ leaves₂ : List Hash)
    (hlen : leaves₁.length = leaves₂.length)
    (hbound : leaves₁.length * BLOCK_BYTES ≤ U64.max)
    (heq : specRootFromLeaves sha256 leaves₁ = specRootFromLeaves sha256 leaves₂) :
    leaves₁ = leaves₂ ∨ HashCollision sha256 := by
  exact FuchsiaMerkle.Proof.specRootFromLeaves_eq_imp_eq_or_collision
    sha256 leaves₁ leaves₂ hlen hbound heq

#print axioms specRootFromLeaves_eq_imp_eq_or_collision

-- ============================================================================
-- Property P1: Total Panic & Arithmetic Overflow Freedom
-- ============================================================================

/--
**Informal**: Totality of `MerkleVerifier.new`: for any valid streaming
`Sha256State`, arbitrary `root`, and arbitrary untrusted `hashes` slice,
`MerkleVerifier.new` always returns cleanly without panicking or overflowing.
-/
theorem merkle_verifier_new_no_panic
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (root : Hash) (hashes : Slice Hash)
    (hsha : ValidSha256State sha256 hInst R_H) :
    ∃ res, fuchsia_merkle_core.MerkleVerifier.new hInst root hashes = .ok res := by
  exact FuchsiaMerkle.Proof.merkle_verifier_new_no_panic
    sha256 hInst R_H root hashes hsha

#print axioms merkle_verifier_new_no_panic

/--
**Informal**: Totality of `MerkleVerifier.verify`: for any valid streaming
`Sha256State`, any `MerkleVerifier`, any byte `offset`, and any `data` slice,
`MerkleVerifier.verify` always returns cleanly without panicking or overflowing.
-/
theorem merkle_verifier_verify_no_panic
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (mv : fuchsia_merkle_core.MerkleVerifier) (offset : Usize) (data : Slice U8)
    (hsha : ValidSha256State sha256 hInst R_H) :
    ∃ res, fuchsia_merkle_core.MerkleVerifier.verify hInst mv offset data = .ok res := by
  exact FuchsiaMerkle.Proof.merkle_verifier_verify_no_panic
    sha256 hInst R_H mv offset data hsha

#print axioms merkle_verifier_verify_no_panic

/--
**Informal**: Totality of `MerkleVerifier.verify_aligned`: for any
`AlignedByteSource` refining `slice`, any `MerkleVerifier`, any `offset`, and any
`unaligned_len`, `MerkleVerifier.verify_aligned` always returns cleanly without
panicking or overflowing.
-/
theorem merkle_verifier_verify_aligned_no_panic
    {Src : Type} (sha256 : List U8 → Hash)
    (srcInst : fuchsia_merkle_core.AlignedByteSource Src)
    (mv : fuchsia_merkle_core.MerkleVerifier) (offset : Usize)
    (src : Src) (slice : Slice U8) (unaligned_len : Usize)
    (hsrc : RefinesAlignedByteSource sha256 srcInst src slice) :
    ∃ res,
      fuchsia_merkle_core.MerkleVerifier.verify_aligned srcInst mv offset src unaligned_len =
        .ok res := by
  exact FuchsiaMerkle.Proof.merkle_verifier_verify_aligned_no_panic
    sha256 srcInst mv offset src slice unaligned_len hsrc

#print axioms merkle_verifier_verify_aligned_no_panic

/--
**Informal**: Totality of `ReadSizedMerkleVerifier.new`: for any valid streaming
`Sha256State`, any `MerkleVerifier` constructed via `MerkleVerifier.new`, and any
`read_size` (including `0`, unaligned sizes, or huge values),
`ReadSizedMerkleVerifier.new` always returns cleanly without panicking or
overflowing.
-/
theorem read_sized_verifier_new_no_panic
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (root : Hash) (hashes : Slice Hash)
    (mv : fuchsia_merkle_core.MerkleVerifier) (read_size : Usize)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hmv : fuchsia_merkle_core.MerkleVerifier.new hInst root hashes = .ok (.Ok mv)) :
    ∃ res,
      fuchsia_merkle_core.ReadSizedMerkleVerifier.new hInst mv read_size = .ok res := by
  exact FuchsiaMerkle.Proof.read_sized_verifier_new_no_panic
    sha256 hInst R_H root hashes mv read_size hsha hmv

#print axioms read_sized_verifier_new_no_panic

/--
**Informal**: Totality of `ReadSizedMerkleVerifier.verify`: for any valid
streaming `Sha256State`, any `ReadSizedMerkleVerifier` constructed via
`MerkleVerifier.new` and `ReadSizedMerkleVerifier.new`, any `offset`, and any
`data` slice, `ReadSizedMerkleVerifier.verify` always returns cleanly without
panicking or overflowing.
-/
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
  exact FuchsiaMerkle.Proof.read_sized_verifier_verify_no_panic
    sha256 hInst R_H root hashes mv read_size rsv offset data hsha hmv hrsv

#print axioms read_sized_verifier_verify_no_panic

/--
**Informal**: Totality of `ReadSizedMerkleVerifier.verify_aligned`: for any
valid streaming `Sha256State`, any `AlignedByteSource` refining `slice`, any
`ReadSizedMerkleVerifier` constructed via `MerkleVerifier.new` and
`ReadSizedMerkleVerifier.new`, and any `(offset, unaligned_len)`,
`ReadSizedMerkleVerifier.verify_aligned` always returns cleanly without
panicking or overflowing.
-/
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
  exact FuchsiaMerkle.Proof.read_sized_verifier_verify_aligned_no_panic
    sha256 hInst R_H srcInst root hashes mv read_size rsv offset src slice unaligned_len
    hsha hsrc hmv hrsv

#print axioms read_sized_verifier_verify_aligned_no_panic

-- ============================================================================
-- Property P2: Block Hashing, Streaming Builder Correctness, & Chunk Invariance
-- ============================================================================

/--
**Informal**: Level-0 block hashing correctness (`hash_block_spec`): for any
valid streaming `Sha256State`, any byte slice `block` of length at most `8,192`,
and any 8,192-byte-aligned `offset`, `hash_block` succeeds without panicking and
computes the exact protocol digest `specHashBlock sha256 block offset`.
-/
theorem hash_block_spec
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (block : Slice U8) (offset : U64)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hlen : block.val.length ≤ BLOCK_BYTES)
    (hoff : offset.val % BLOCK_BYTES = 0) :
    fuchsia_merkle_core.hash_block hInst block offset =
      .ok (specHashBlock sha256 block.val offset) := by
  exact FuchsiaMerkle.Proof.hash_block_spec
    sha256 hInst R_H block offset hsha hlen hoff

#print axioms hash_block_spec

/--
**Informal**: Page-aligned block hashing correctness (`hash_block_aligned_spec`):
for any valid streaming `Sha256State`, any 8,192-byte-aligned `offset`, any
aligned buffer length `buf_len ∈ {8192, 4096}`, any positive logical length
`0 < u_len ≤ buf_len`, and any slice `data` of length at least `buf_len`,
`hash_block_aligned` succeeds without panicking and hashes the exact level-0
preimage `specIdentity u_len 0 offset ++ data[0..buf_len] ++
specZeroPad (8192 - buf_len)` required by `RefinesAlignedByteSource`.
-/
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
  exact FuchsiaMerkle.Proof.hash_block_aligned_spec
    sha256 hInst R_H offset data buf_len u_len
    hsha hbuf hpos hule hoff hdlen

#print axioms hash_block_aligned_spec

/--
**Informal**: Root reconstruction correctness (`rebuild_root_spec`): for any
valid streaming `Sha256State` and any slice `leaf_hashes` whose total byte span
`leaf_hashes.len() * 8192` fits in `U64`, `rebuild_root` succeeds without
panicking and returns `specRootFromLeaves sha256 leaf_hashes`.
-/
theorem rebuild_root_spec
    {H : Type} (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H) (R_H : H → List U8 → Prop)
    (leaf_hashes : Slice Hash)
    (hsha : ValidSha256State sha256 hInst R_H)
    (hbound : leaf_hashes.val.length * BLOCK_BYTES ≤ U64.max) :
    fuchsia_merkle_core.rebuild_root hInst leaf_hashes =
      .ok (specRootFromLeaves sha256 leaf_hashes.val) := by
  exact FuchsiaMerkle.Proof.rebuild_root_spec
    sha256 hInst R_H leaf_hashes hsha hbound

#print axioms rebuild_root_spec

/--
**Informal**: Single-pass builder correctness (`merkle_root_builder_complete_spec`):
for any valid streaming `Sha256State`, any valid `LeafCollector` starting from an
empty accumulator `c₀` with sufficient capacity `maxLeaves` for `data`'s leaves,
and any blob `data` whose block-rounded byte length fits in `U64`, running
`MerkleRootBuilder::new(c₀)` followed by `complete(data)` succeeds without
panicking, returns `specMerkleRoot sha256 data`, and emits the exact ordered
level-0 leaf digests `specLeafHashes sha256 data` into the collector.
-/
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
  exact FuchsiaMerkle.Proof.merkle_root_builder_complete_spec
    sha256 hInst R_H cInst maxLeaves R_C c₀ data
    hsha hcol hc₀ hcap hbound

#print axioms merkle_root_builder_complete_spec

/--
**Informal**: Streaming buffered builder correctness (`buffered_streaming_spec`):
for any valid streaming `Sha256State`, any valid `LeafCollector` starting from an
empty accumulator `c₀` with sufficient capacity `maxLeaves`, and any list of
arbitrary-sized byte slices `chunks` whose concatenated block-rounded length fits
in `U64`, streaming every slice through `BufferedMerkleRootBuilder.write` and
calling `complete` succeeds without panicking, returns `specMerkleRoot` of the
concatenated bytes `chunks.flatMap Slice.val`, and streams the exact ordered
`specLeafHashes` into the collector.
-/
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
  exact FuchsiaMerkle.Proof.buffered_streaming_spec
    sha256 hInst R_H cInst maxLeaves R_C c₀ chunks
    hsha hcol hc₀ hcap hbound

#print axioms buffered_streaming_spec

/--
**Informal**: Chunk-partition invariance (`buffered_chunk_invariance`): for any
two chunk partitions `chunks₁` and `chunks₂` of the exact same logical byte
sequence (`chunks₁.flatMap Slice.val = chunks₂.flatMap Slice.val`) whose
block-rounded length fits in `U64`, streaming `chunks₁` vs. `chunks₂` through
`BufferedMerkleRootBuilder` (instantiated with `StreamSha256` and
`ListLeafCollector`) yields identical `(root, leaves)` outputs.
-/
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
  exact FuchsiaMerkle.Proof.buffered_chunk_invariance
    sha256 chunks₁ chunks₂ hsame hbound

#print axioms buffered_chunk_invariance

-- ============================================================================
-- Property P3: Verifier Completeness Across Slice & Page-Aligned Buffers
-- ============================================================================

/--
**Informal**: Completeness of `MerkleVerifier.new`: given the authentic level-0
leaf digests `hashes = specLeafHashes sha256 B` whose byte span fits in `U64`,
`MerkleVerifier.new` accepts `(specMerkleRoot sha256 B, hashes)` and returns
`Ok(mv)`.
-/
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
  exact FuchsiaMerkle.Proof.merkle_verifier_new_complete
    sha256 hInst R_H B hashes hsha hleaves hbound

#print axioms merkle_verifier_new_complete

/--
**Informal**: Empty-blob completeness of `MerkleVerifier.verify`: a
`MerkleVerifier` constructed from the authentic leaf slice of the empty blob `[]`
always accepts `offset = 0` and empty slice `data = []` with `Ok(())`.
-/
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
  exact FuchsiaMerkle.Proof.merkle_verifier_verify_complete_empty
    sha256 hInst R_H hashes mv data hsha hleaves hmv hdata

#print axioms merkle_verifier_verify_complete_empty

/--
**Informal**: Non-empty slice completeness of `MerkleVerifier.verify`: a
`MerkleVerifier` constructed from the authentic leaf digests of `B` accepts every
non-empty slice `data` matching `B[offset .. offset + data.len()]` at an
8,192-byte-aligned `offset` (without `Usize` overflow) whose end
`offset + data.len()` is either 8,192-byte-aligned or equal to `B.length`.
-/
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
  exact FuchsiaMerkle.Proof.merkle_verifier_verify_complete
    sha256 hInst R_H B hashes mv offset data
    hsha hleaves hmv hoff hdne hsum_usize hin_blob halign_end hslice_eq

#print axioms merkle_verifier_verify_complete

/--
**Informal**: Page-aligned completeness of `MerkleVerifier.verify_aligned`: for
any `AlignedByteSource` refining a 4,096-byte-multiple `slice` that holds
`0 < unaligned_len ≤ slice.len()` bytes of `B` starting at an 8,192-byte-aligned
`offset` (without `Usize` overflow), spans no extra 8,192-byte Merkle blocks
beyond `ceil(unaligned_len / 8192)`, ends on an 8,192-byte boundary or EOF
`B.length`, and is zero-padded across `unaligned_len .. slice.len()`, a
`MerkleVerifier` constructed from the authentic leaf digests of `B` accepts with
`Ok(())`.
-/
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
  exact FuchsiaMerkle.Proof.merkle_verifier_verify_aligned_complete
    sha256 hInst R_H srcInst B hashes mv offset src slice unaligned_len
    hsha hsrc hleaves hmv hoff hpage hunz hule hblocks hsum_usize
    hin_blob halign_end hprefix htail_zero

#print axioms merkle_verifier_verify_aligned_complete

/--
**Informal**: Completeness of `ReadSizedMerkleVerifier.new`: for any
`MerkleVerifier` constructed via `MerkleVerifier.new` and any positive
8,192-byte-multiple `read_size` whose level-1 header length
`(read_size / 8192) * 32` fits in `U32`, `ReadSizedMerkleVerifier.new` succeeds
with `Ok(rsv)`.
-/
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
  exact FuchsiaMerkle.Proof.read_sized_verifier_new_complete
    sha256 hInst R_H root hashes mv read_size hsha hmv hrs_pos hrs_align hbph_bound

#print axioms read_sized_verifier_new_complete

/--
**Informal**: Empty-blob completeness of `ReadSizedMerkleVerifier.verify`: a
`ReadSizedMerkleVerifier` constructed from the authentic leaf slice of the empty
blob `[]` always accepts `offset = 0` and empty slice `data = []` with `Ok(())`.
-/
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
  exact FuchsiaMerkle.Proof.read_sized_verifier_verify_complete_empty
    sha256 hInst R_H hashes mv read_size rsv data hsha hleaves hmv hrsv hdata

#print axioms read_sized_verifier_verify_complete_empty

/--
**Informal**: Non-empty slice completeness of `ReadSizedMerkleVerifier.verify`: a
`ReadSizedMerkleVerifier` constructed from the authentic leaf digests of `B`
accepts every non-empty slice `data` matching `B[offset .. offset + data.len()]`
at a `read_size`-aligned `offset` (without `Usize` overflow) whose end
`offset + data.len()` is either `read_size`-aligned or equal to `B.length`.
-/
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
  exact FuchsiaMerkle.Proof.read_sized_verifier_verify_complete
    sha256 hInst R_H B hashes mv read_size rsv offset data
    hsha hleaves hmv hrsv hdne hoff_rs hsum_usize hin_blob hdata halign_end

#print axioms read_sized_verifier_verify_complete

/--
**Informal**: Page-aligned completeness of `ReadSizedMerkleVerifier.verify_aligned`:
for any `AlignedByteSource` refining a 4,096-byte-multiple `slice` holding
`0 < unaligned_len ≤ slice.len()` bytes of `B` starting at a `read_size`-aligned
`offset` (without `Usize` overflow), spanning no extra 8,192-byte Merkle blocks
beyond `ceil(unaligned_len / 8192)`, ending on a `read_size` boundary or EOF
`B.length`, and zero-padded across `unaligned_len .. slice.len()`, a
`ReadSizedMerkleVerifier` constructed from the authentic leaf digests of `B`
accepts with `Ok(())`.
-/
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
  exact FuchsiaMerkle.Proof.read_sized_verifier_verify_aligned_complete
    sha256 hInst R_H srcInst B hashes mv read_size rsv offset src slice unaligned_len
    hsha hsrc hleaves hmv hrsv hoff_rs hpage hunz hule hblocks hsum_usize
    hin_blob hprefix htail_zero halign_end

#print axioms read_sized_verifier_verify_aligned_complete

-- ============================================================================
-- Property P4: End-to-End Collision-Resistant Soundness & Page-Tail Integrity
-- ============================================================================

/--
**Informal**: Soundness of `MerkleVerifier.new`: given untrusted `hashes` of the
canonical leaf count for `B`, if `MerkleVerifier.new` accepts against the trusted
root `specMerkleRoot sha256 B`, then `mv.hashes()` equals the exact canonical
leaf list `specLeafHashes sha256 B`, or `sha256` admits a collision
(`HashCollision sha256`).
-/
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
  exact FuchsiaMerkle.Proof.merkle_verifier_new_sound
    sha256 hInst R_H B hashes out_hashes mv hsha hlen hmv hget

#print axioms merkle_verifier_new_sound

/--
**Informal**: End-to-end soundness of `MerkleVerifier.verify`: even when
`MerkleVerifier.new` is constructed from untrusted `hashes` (subject only to the
canonical leaf-count bound for `B`), accepting `(offset, data)` guarantees that
either `data` is empty (and `B = []` with `offset = 0`) or `data` is in bounds
(`offset + data.len() ≤ B.length`) and byte-for-byte equal to
`B[offset .. offset + data.len()]`, unless `HashCollision sha256` holds.
-/
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
  exact FuchsiaMerkle.Proof.merkle_verifier_verify_sound
    sha256 hInst R_H B hashes mv offset data hsha hlen hmv hok

#print axioms merkle_verifier_verify_sound

/--
**Informal**: End-to-end soundness & trailing-zero page integrity of
`MerkleVerifier.verify_aligned`: even when `MerkleVerifier.new` is given
untrusted `hashes` (of canonical leaf count for `B`), accepting an
`AlignedByteSource` refining `slice` with `(offset, unaligned_len)` guarantees
all of the following (up to `HashCollision sha256`):
- `0 < unaligned_len ≤ slice.len()`, `slice.len()` is a multiple of `4,096`, and
  `slice` spans no extra 8,192-byte Merkle blocks beyond `ceil(unaligned_len / 8192)`;
- `offset + unaligned_len ≤ B.length` and the prefix `slice[0..unaligned_len]`
  equals the authentic blob bytes `B[offset .. offset + unaligned_len]`;
- every trailing byte in `slice[unaligned_len .. slice.len()]` is strictly `0u8`.
-/
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
  exact FuchsiaMerkle.Proof.merkle_verifier_verify_aligned_sound
    sha256 hInst R_H srcInst B hashes mv offset src slice unaligned_len
    hsha hsrc hlen hmv hok

#print axioms merkle_verifier_verify_aligned_sound

/--
**Informal**: Construction soundness of `ReadSizedMerkleVerifier.new`: whenever
`ReadSizedMerkleVerifier.new` accepts `(mv, read_size)` for a root-verified
`MerkleVerifier`, `read_size` is guaranteed to be positive, an exact multiple of
`8,192`, and small enough that `(read_size / 8192) * 32 ≤ U32.max`.
-/
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
  exact FuchsiaMerkle.Proof.read_sized_verifier_new_sound
    sha256 hInst R_H root hashes mv read_size rsv hsha hmv hrsv

#print axioms read_sized_verifier_new_sound

/--
**Informal**: Unconditional end-to-end soundness of
`ReadSizedMerkleVerifier.verify`: given untrusted `hashes` of canonical leaf
count for `B`, accepting `(offset, data)` guarantees that `data` is empty (with
`B = []` and `offset = 0`) or in-bounds (`offset + data.len() ≤ B.length`) and
equal to `B[offset .. offset + data.len()]`, up to `HashCollision sha256` or
`HashZeroPreimage sha256` (accounting for zero-padded partial level-1 slots when
the caller omits a blob-length check).
-/
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
  exact FuchsiaMerkle.Proof.read_sized_verifier_verify_sound
    sha256 hInst R_H B hashes mv read_size rsv offset data
    hsha hlen hmv hrsv hok

#print axioms read_sized_verifier_verify_sound

/--
**Informal**: Caller-size-bounded end-to-end soundness of
`ReadSizedMerkleVerifier.verify` (`fxblob` contract): when the caller enforces
`offset + data.len() ≤ B.length` and `read_size`-alignment (or EOF) of the read
end, accepting `(offset, data)` guarantees full byte authenticity against `B`
under standard collision resistance (`HashCollision sha256`) alone, without
relying on `HashZeroPreimage`.
-/
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
  exact FuchsiaMerkle.Proof.read_sized_verifier_verify_sound_bounded
    sha256 hInst R_H B hashes mv read_size rsv offset data
    hsha hlen hmv hrsv hin_blob halign_end hok

#print axioms read_sized_verifier_verify_sound_bounded

/--
**Informal**: Unconditional end-to-end soundness & trailing-zero page integrity
of `ReadSizedMerkleVerifier.verify_aligned`: accepting an `AlignedByteSource`
refining `slice` with `(offset, unaligned_len)` guarantees 4 KiB page alignment,
no extra 8 KiB blocks beyond `ceil(unaligned_len / 8192)`, prefix authenticity
`slice[0..unaligned_len] = B[offset .. offset + unaligned_len]`, and zeroed page
tail `slice[unaligned_len .. slice.len()] = 0`, up to `HashCollision sha256` or
`HashZeroPreimage sha256`.
-/
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
  exact FuchsiaMerkle.Proof.read_sized_verifier_verify_aligned_sound
    sha256 hInst R_H srcInst B hashes mv read_size rsv offset src slice unaligned_len
    hsha hsrc hlen hmv hrsv hok

#print axioms read_sized_verifier_verify_aligned_sound

/--
**Informal**: Caller-size-bounded end-to-end soundness & trailing-zero page
integrity of `ReadSizedMerkleVerifier.verify_aligned` (`fxblob` contract): when
the caller bounds `offset + unaligned_len ≤ B.length` and enforces
`read_size`-alignment (or EOF) of `offset + unaligned_len`, accepting an
`AlignedByteSource` refining `slice` guarantees 4 KiB page alignment, no extra
8 KiB blocks, prefix authenticity against `B`, and zeroed trailing page bytes
`slice[unaligned_len .. slice.len()] = 0` under `HashCollision sha256` alone.
-/
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
  exact FuchsiaMerkle.Proof.read_sized_verifier_verify_aligned_sound_bounded
    sha256 hInst R_H srcInst B hashes mv read_size rsv offset src slice unaligned_len
    hsha hsrc hlen hmv hrsv hin_blob halign_end hok

#print axioms read_sized_verifier_verify_aligned_sound_bounded

-- ============================================================================
-- Property P5: Zero-Hash Leaf Extension Characterization
-- ============================================================================

/--
**Informal**: Protocol zero-hash leaf extension
(`specRootFromLeaves_append_zeroHash`): because partial internal Merkle nodes pad
unused child digest slots with zero bytes, appending `zeroHash` (`[0u8; 32]`) to
any leaf list `leaves` with `2 ≤ leaves.length` and `leaves.length % 256 ≠ 0`
produces the exact same Fuchsia Merkle root for every `sha256` without any hash
collision.
-/
theorem specRootFromLeaves_append_zeroHash
    (sha256 : List U8 → Hash) (leaves : List Hash)
    (hge2 : 2 ≤ leaves.length)
    (hrem : leaves.length % CHILDREN_PER_NODE ≠ 0) :
    specRootFromLeaves sha256 (leaves ++ [zeroHash]) =
      specRootFromLeaves sha256 leaves := by
  exact FuchsiaMerkle.Proof.specRootFromLeaves_append_zeroHash
    sha256 leaves hge2 hrem

#print axioms specRootFromLeaves_append_zeroHash

/--
**Informal**: Verifier zero-hash extension (`merkle_verifier_zero_hash_extension`,
https://fxbug.dev/579509305): for any valid streaming `Sha256State`, any leaf
list `leaves` with `2 ≤ leaves.length` and `leaves.length % 256 ≠ 0`, and any
slice `extHashes = leaves ++ [zeroHash]` whose extended byte span fits in `U64`,
`MerkleVerifier.new` accepts `extHashes` against `specRootFromLeaves sha256
leaves`, proving that the caller-enforced leaf-count precondition `hlen` in
Property P4 is strictly required for soundness.
-/
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
  exact FuchsiaMerkle.Proof.merkle_verifier_zero_hash_extension
    sha256 hInst R_H leaves extHashes hsha hge2 hrem hu64 hext_val

#print axioms merkle_verifier_zero_hash_extension

end FuchsiaMerkle.Contract
