-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Assumptions
import Extracted

set_option maxRecDepth 8192

/-!
# Formal Specification of the Fuchsia Merkle Tree Protocol

This security-critical module (`spec/Spec.lean`, classified as High risk in
`RISK.md` and governed by `../REVIEW.md`) formalizes the pure mathematical
Fuchsia Merkle Tree specification (`//docs/concepts/security/merkleroot.md`) for
`//src/sys/pkg/lib/fuchsia-merkle/core`:

1. Level-0 leaf block hashing (`specHashBlock`), including the 0-byte empty-blob
   special case at offset 0 and zero-padding of partial blocks to `8,192` bytes.
2. Internal tree node hashing (`concatHashes`, `specInternalPreimage`,
   `specHashInternal`) at tree levels `level ≥ 1`.
3. Canonical blob chunking (`chunksOf`), level-0 leaf digest generation
   (`specLeafHashes`), bottom-up multi-level tree reduction (`specLevelStep`,
   `specReduceLevels`, `specRootFromLeaves`), and end-to-end Merkle root
   computation (`specMerkleRoot`).
4. Read-sized level-1 digest folding (`specReadSizedHashes`) used by
   `ReadSizedMerkleVerifier` and multi-chunk streaming driver
   (`bufferedWriteAll`) for `BufferedMerkleRootBuilder`.

Every top-level formal declaration is paired with a mandatory `**Informal**:`
English specification.
-/

namespace FuchsiaMerkle.Spec

open Aeneas.Std
open FuchsiaMerkle.Assumptions

/--
**Informal**: Pure Fuchsia Merkle level-0 data block digest (`specHashBlock`) at
byte `offset` (`//docs/concepts/security/merkleroot.md`):
- When `block` is empty (`block.length = 0`) and `offset = 0`, hashes only the
  12-byte all-zero identity header `specIdentity 0 0 0`.
- For every non-empty block (or non-zero offset), hashes the 12-byte header
  `specIdentity block.length 0 offset` followed by `block` and
  `8192 - block.length` zero-padding bytes (`8,204` bytes total when
  `block.length ≤ 8192`).
-/
def specHashBlock (sha256 : List U8 → Hash) (block : List U8) (offset : U64) : Hash :=
  let len32 := u32OfNat block.length
  let preimage :=
    if block.length = 0 ∧ offset = 0#u64 then
      specIdentity len32 0#u64 offset
    else
      specIdentity len32 0#u64 offset ++ block ++ specZeroPad (BLOCK_BYTES - block.length)
  sha256 preimage

/--
**Informal**: Flattens an ordered list of 32-byte `Hash` digests (`hs`) into a
contiguous byte list of length `32 * hs.length`.
-/
def concatHashes (hs : List Hash) : List U8 :=
  hs.flatMap Aeneas.Std.Array.val

/--
**Informal**: Pure preimage bytes (`specInternalPreimage`) of an internal Merkle
tree node at `level ≥ 1` and byte offset `levelOffset` holding `children` out of
`hashesPerNode` digest slots (`bytesPerNode = 32 * hashesPerNode`): concatenates
`specIdentity bytesPerNode level levelOffset`, the raw child digest bytes
`concatHashes children`, and `(hashesPerNode - children.length) * 32` zero bytes.
-/
def specInternalPreimage
    (hashesPerNode : Nat) (bytesPerNode : U32) (level : U64) (levelOffset : U64)
    (children : List Hash) : List U8 :=
  specIdentity bytesPerNode level levelOffset ++
    concatHashes children ++
    specZeroPad ((hashesPerNode - children.length) * HASH_BYTES)

/--
**Informal**: Pure Fuchsia Merkle internal node digest (`specHashInternal`):
applies `sha256` to `specInternalPreimage hashesPerNode bytesPerNode level
levelOffset children`.
-/
def specHashInternal
    (sha256 : List U8 → Hash)
    (hashesPerNode : Nat) (bytesPerNode : U32) (level : U64) (levelOffset : U64)
    (children : List Hash) : Hash :=
  sha256 (specInternalPreimage hashesPerNode bytesPerNode level levelOffset children)

/--
**Informal**: Splits a list into contiguous, non-empty chunks of length `k`
(`0 < k`), with a possibly shorter final chunk when the input length is not a
multiple of `k` (returns `[]` on `[]`).
-/
def chunksOf {α : Type _} (k : Nat) (hk : 0 < k) : List α → List (List α)
  | [] => []
  | x :: xs => (x :: xs).take k :: chunksOf k hk ((x :: xs).drop k)
termination_by xs => xs.length
decreasing_by
  simp only [List.length_drop, List.length_cons]
  omega

/--
**Informal**: Structural termination bound showing that chunking a list `xs` into
non-empty groups of size `k > 0` produces at most `xs.length` chunks (used purely
to discharge the well-founded termination obligation of `specReduceLevels`).
-/
theorem chunksOf_length_le {α : Type _} (k : Nat) (hk : 0 < k) (xs : List α) :
    (chunksOf k hk xs).length ≤ xs.length := by
  match xs with
  | [] => simp [chunksOf]
  | x :: xs =>
    rw [chunksOf]
    have ih := chunksOf_length_le k hk ((x :: xs).drop k)
    simp only [List.length_cons, List.length_drop] at ih ⊢
    omega
termination_by xs.length
decreasing_by
  simp only [List.length_drop, List.length_cons]
  omega

/--
**Informal**: Canonical ordered list of level-0 leaf digests (`specLeafHashes`)
for a byte blob `blob`:
- If `blob` is empty (`[]`), produces the single-element list
  `[specHashBlock sha256 [] 0]`.
- Otherwise, splits `blob` into `8,192`-byte blocks and hashes the `idx`-th
  block with `specHashBlock` at byte offset `idx * 8192` (modulo $2^{64}$).
-/
def specLeafHashes (sha256 : List U8 → Hash) (blob : List U8) : List Hash :=
  if blob.isEmpty then
    [specHashBlock sha256 [] 0#u64]
  else
    (chunksOf BLOCK_BYTES (by decide) blob).mapIdx fun idx blk =>
      specHashBlock sha256 blk (u64OfNat (idx * BLOCK_BYTES))

/--
**Informal**: Single bottom-up tree reduction step (`specLevelStep`) at internal
tree `level`: partitions `nodes` into groups of up to `256` child digests
(`8,192` bytes per full node) and hashes group `idx` with `specHashInternal` at
byte offset `idx * 8192`.
-/
def specLevelStep (sha256 : List U8 → Hash) (level : Nat) (nodes : List Hash) : List Hash :=
  (chunksOf CHILDREN_PER_NODE (by decide) nodes).mapIdx fun idx grp =>
    specHashInternal sha256 CHILDREN_PER_NODE 8192#u32
      (u64OfNat level) (u64OfNat (idx * BLOCK_BYTES)) grp

/--
**Informal**: Recursive bottom-up Merkle tree reduction (`specReduceLevels`)
starting at internal `level`:
- Returns the canonical empty-blob digest `specHashBlock sha256 [] 0` on `[]`.
- Returns `root` directly on a single-digest list `[root]`.
- When two or more digests remain, applies `specLevelStep` at `level` and
  recurses at `level + 1` until a single root digest remains.
-/
def specReduceLevels (sha256 : List U8 → Hash) (level : Nat) : List Hash → Hash
  | [] => specHashBlock sha256 [] 0#u64
  | [root] => root
  | a :: b :: rest =>
    specReduceLevels sha256 (level + 1) (specLevelStep sha256 level (a :: b :: rest))
termination_by nodes => nodes.length
decreasing_by
  simp only [specLevelStep, List.length_mapIdx]
  rw [chunksOf]
  have hle := chunksOf_length_le CHILDREN_PER_NODE (by decide) ((a :: b :: rest).drop CHILDREN_PER_NODE)
  simp only [List.length_cons, List.length_drop, CHILDREN_PER_NODE] at hle ⊢
  omega

/--
**Informal**: Canonical Fuchsia Merkle root reconstructed from an ordered list of
level-0 leaf digests `leaves` (`specReduceLevels sha256 1 leaves`).
-/
def specRootFromLeaves (sha256 : List U8 → Hash) (leaves : List Hash) : Hash :=
  specReduceLevels sha256 1 leaves

/--
**Informal**: Canonical end-to-end Fuchsia Merkle root (`specMerkleRoot`) of a
byte blob `blob`, obtained by reducing `specLeafHashes sha256 blob` via
`specRootFromLeaves`.
-/
def specMerkleRoot (sha256 : List U8 → Hash) (blob : List U8) : Hash :=
  specRootFromLeaves sha256 (specLeafHashes sha256 blob)

/--
**Informal**: Canonical level-1 read-sized digests (`specReadSizedHashes`) stored
by `ReadSizedMerkleVerifier` when grouping every `hashesPerHash = read_size / 8192`
(`bytesPerHash = 32 * hashesPerHash`) contiguous level-0 leaf digests into a
level-1 internal hash at offset `idx * bytesPerHash`.
-/
def specReadSizedHashes
    (sha256 : List U8 → Hash) (hashesPerHash : Nat) (hk : 0 < hashesPerHash)
    (bytesPerHash : U32) (leaves : List Hash) : List Hash :=
  (chunksOf hashesPerHash hk leaves).mapIdx fun idx grp =>
    specHashInternal sha256 hashesPerHash bytesPerHash 1#u64
      (u64OfNat (idx * bytesPerHash.val)) grp

/--
**Informal**: Feeds an ordered list of arbitrary-length byte slices sequentially
into `BufferedMerkleRootBuilder.write`, propagating any runtime failure in
`Result`.
-/
def bufferedWriteAll
    {H C : Type}
    (hInst : fuchsia_merkle_core.Sha256State H)
    (cInst : fuchsia_merkle_core.LeafCollector C)
    (self : fuchsia_merkle_core.BufferedMerkleRootBuilder H C) :
    List (Slice U8) → Result (fuchsia_merkle_core.BufferedMerkleRootBuilder H C)
  | [] => .ok self
  | chunk :: rest => do
    let self' ← fuchsia_merkle_core.BufferedMerkleRootBuilder.write hInst cInst self chunk
    bufferedWriteAll hInst cInst self' rest

end FuchsiaMerkle.Spec
