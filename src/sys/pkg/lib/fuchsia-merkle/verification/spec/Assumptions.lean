-- Copyright 2026 The Fuchsia Authors. All rights reserved.
-- Use of this source code is governed by a BSD-style license that can be
-- found in the LICENSE file.

import Extracted

/-!
# Formal Assumptions on External Traits & Cryptographic Primitives

This security-critical module (`spec/Assumptions.lean`, classified as High risk
in `RISK.md` and governed by `../REVIEW.md`) isolates all constants,
cryptographic hardness predicates, and external trait contracts assumed about
code *outside* `//src/sys/pkg/lib/fuchsia-merkle/core`:

1. Protocol block, digest, fanout, and page-alignment constants together with
   12-byte little-endian identity encoding (`specIdentity`) and zero-padding
   (`specZeroPad`).
2. Constructive cryptographic failure predicates (`HashCollision`,
   `HashZeroPreimage`) over an arbitrary pure compression function
   `sha256 : List U8 → Hash`.
3. Refinement contracts (`ValidSha256State`, `ValidLeafCollector`,
   `RefinesAlignedByteSource`) for the three external traits bridging
   `fuchsia_merkle_core` to BoringSSL/NEON SHA-256, streaming leaf sinks (`fxfs`),
   and `PtrByteSliceSource` in `fuchsia-merkle`, along with pure model instances
   (`StreamSha256`, `ListLeafCollector`, `SliceByteSource`) witnessing
   non-vacuity.

Every top-level formal declaration is paired with a mandatory `**Informal**:`
English specification.
-/

namespace FuchsiaMerkle.Assumptions

open Aeneas.Std

/--
**Informal**: Size of a single Fuchsia Merkle data block or internal hash block
in bytes (`8,192`).
-/
abbrev BLOCK_BYTES : Nat := 8192

/--
**Informal**: Size of a single SHA-256 / Fuchsia Merkle digest in bytes (`32`).
-/
abbrev HASH_BYTES : Nat := 32

/--
**Informal**: Number of 32-byte child digests packed into a full 8,192-byte
internal Merkle tree node (`8192 / 32 = 256`).
-/
abbrev CHILDREN_PER_NODE : Nat := 256

/--
**Informal**: Minimum supported VM page size in bytes (`4,096`) enforced on
page-aligned verification buffers in `verify_aligned`.
-/
abbrev MIN_PAGE_BYTES : Nat := 4096

/--
**Informal**: Maximum message byte length permitted by FIPS 180-4 SHA-256
($\lfloor (2^{64} - 1) / 8 \rfloor = 2^{61} - 1$ bytes, matching BoringSSL's
64-bit bit-length counter). Every `Sha256State` instance in `fuchsia_merkle_core`
hashes at most $12 + (2^{32} - 1) < 2^{33} \ll 2^{61} - 1$ bytes.
-/
abbrev SHA256_MAX_BYTES : Nat := (1 <<< 61) - 1

/--
**Informal**: Type alias for the extracted 32-byte Fuchsia Merkle digest array
(`fuchsia_merkle_core::Hash`, represented by Aeneas as `Array U8 32#usize`).
-/
abbrev Hash := fuchsia_merkle_core.Hash

/--
**Informal**: Converts a natural number `n` modulo $2^{32}$ into an unsigned
32-bit Rust scalar `U32`.
-/
def u32OfNat (n : Nat) : U32 := ⟨BitVec.ofNat 32 n⟩

/--
**Informal**: Converts a natural number `n` modulo $2^{64}$ into an unsigned
64-bit Rust scalar `U64`.
-/
def u64OfNat (n : Nat) : U64 := ⟨BitVec.ofNat 64 n⟩

/--
**Informal**: The all-zero 32-byte digest (`[0u8; 32]`) corresponding to unused
digest slots in zero-padded internal Merkle nodes.
-/
def zeroHash : Hash := Aeneas.Std.Array.repeat 32#usize 0#u8

/--
**Informal**: Pure 12-byte little-endian Fuchsia Merkle block identity header:
the 8-byte little-endian encoding of `offset | level` followed by the 4-byte
little-endian encoding of `length` (`//docs/concepts/security/merkleroot.md`).
-/
def specIdentity (length : U32) (level : U64) (offset : U64) : List U8 :=
  ((offset ||| level).bv.toLEBytes.map (@UScalar.mk UScalarTy.U8)) ++
    (length.bv.toLEBytes.map (@UScalar.mk UScalarTy.U8))

/--
**Informal**: Sequence of `n` zero bytes (`0u8`), used to zero-pad partial data
blocks and partial internal hash blocks up to block capacity.
-/
def specZeroPad (n : Nat) : List U8 := List.replicate n 0#u8

/--
**Informal**: Constructive SHA-256 collision witness (`HashCollision`): holds
when there exist two distinct byte sequences `m₁ ≠ m₂` that `sha256` maps to the
exact same 32-byte digest.
-/
def HashCollision (sha256 : List U8 → Hash) : Prop :=
  ∃ m₁ m₂ : List U8, m₁ ≠ m₂ ∧ sha256 m₁ = sha256 m₂

/--
**Informal**: Constructive SHA-256 zero-digest preimage witness
(`HashZeroPreimage`): holds when there exists a byte sequence `m` whose `sha256`
digest equals the all-zero 32-byte `zeroHash`.
-/
def HashZeroPreimage (sha256 : List U8 → Hash) : Prop :=
  ∃ m : List U8, sha256 m = zeroHash

/--
**Informal**: External streaming SHA-256 assumption (`ValidSha256State`): relates
concrete states of a `Sha256State` trait implementation `hInst` (such as
`BoringSslSha256` in `fuchsia-merkle`) to accumulated byte streams via relation
`R` within SHA-256's FIPS 180-4 input length bound (`SHA256_MAX_BYTES` =
$2^{61} - 1$ bytes):
- `new_ok`: `Sha256State::new()` succeeds without panicking and produces an
  initial state related by `R` to the empty byte list `[]`.
- `update_ok`: from any state `s` related to `acc` with
  `acc.length + bytes.len() ≤ SHA256_MAX_BYTES`, `s.update(bytes)` succeeds
  without panicking and produces a state related by `R` to `acc ++ bytes`.
- `finalize_ok`: from any state `s` related to `acc` with
  `acc.length ≤ SHA256_MAX_BYTES`, `s.finalize()` succeeds without panicking and
  returns `sha256 acc`.
-/
structure ValidSha256State {H : Type}
    (sha256 : List U8 → Hash)
    (hInst : fuchsia_merkle_core.Sha256State H)
    (R : H → List U8 → Prop) : Prop where
  new_ok :
    ∃ s₀, hInst.new = .ok s₀ ∧ R s₀ []
  update_ok :
    ∀ (s : H) (acc : List U8) (bytes : Slice U8),
      R s acc →
      acc.length + bytes.val.length ≤ SHA256_MAX_BYTES →
      ∃ s', hInst.update s bytes = .ok s' ∧ R s' (acc ++ bytes.val)
  finalize_ok :
    ∀ (s : H) (acc : List U8),
      R s acc →
      acc.length ≤ SHA256_MAX_BYTES →
      hInst.finalize s = .ok (sha256 acc)

/--
**Informal**: Canonical pure `Sha256State` instance (`StreamSha256`) whose state
is the exact list of accumulated message bytes `List U8`, starting at `[]`,
appending each slice in `update`, and applying `sha256` in `finalize`. Witnesses
satisfiability of `ValidSha256State` under `Eq`.
-/
def StreamSha256 (sha256 : List U8 → Hash) :
    fuchsia_merkle_core.Sha256State (List U8) where
  new := .ok []
  update acc bytes := .ok (acc ++ bytes.val)
  finalize acc := .ok (sha256 acc)

/--
**Informal**: External streaming leaf-sink assumption (`ValidLeafCollector`):
relates concrete states of a `LeafCollector` trait implementation `cInst` (such
as `NoopLeafHashCollector`, `Vec<Hash>`, or `fxfs`'s
`BlobMetadataLeafHashCollector`) up to capacity `maxLeaves` to the ordered list
of emitted level-0 leaf digests via relation `R`: from any collector state `c`
related to `acc` with `acc.length < maxLeaves`, calling `c.add_leaf_hash(h)`
succeeds without panicking and yields a state `c'` related to `acc ++ [h]`.
-/
structure ValidLeafCollector {C : Type}
    (cInst : fuchsia_merkle_core.LeafCollector C)
    (maxLeaves : Nat)
    (R : C → List Hash → Prop) : Prop where
  add_leaf_hash_ok :
    ∀ (c : C) (acc : List Hash) (h : Hash),
      acc.length < maxLeaves →
      R c acc →
      ∃ c', cInst.add_leaf_hash c h = .ok c' ∧ R c' (acc ++ [h])

/--
**Informal**: Canonical pure `LeafCollector` instance (`ListLeafCollector`) that
accumulates emitted level-0 leaf digests in exact emission order inside a
`List Hash`, witnessing satisfiability of `ValidLeafCollector` under `Eq`.
-/
def ListLeafCollector : fuchsia_merkle_core.LeafCollector (List Hash) where
  add_leaf_hash acc h := .ok (acc ++ [h])

/--
**Informal**: External page-aligned buffer assumption
(`RefinesAlignedByteSource`): states that an `AlignedByteSource` implementation
`srcInst` (such as `PtrByteSliceSource` wrapping `PtrByteSlice` and
BoringSSL/ARM64-NEON `hash_block_aligned` in `fuchsia-merkle`) faithfully models
the concrete byte slice `slice`:
- `buffer_len_eq`: `src.buffer_len()` succeeds and returns `slice.len()`.
- `hash_aligned_block_eq`: whenever called on an 8,192-byte-aligned sub-buffer
  offset `byte_pos` with aligned buffer size `buf_len ∈ {8192, 4096}`, positive
  logical length `0 < u_len ≤ buf_len`, in-bounds range
  `byte_pos + buf_len ≤ slice.len()`, and 8,192-byte-aligned blob offset
  `block_offset`, `src.hash_aligned_block(block_offset, byte_pos, buf_len, u_len)`
  succeeds and returns `sha256` of the 12-byte level-0 identity header for
  `(u_len, block_offset)` concatenated with `slice[byte_pos .. byte_pos + buf_len]`
  and `8192 - buf_len` zero-padding bytes.
-/
structure RefinesAlignedByteSource {Src : Type}
    (sha256 : List U8 → Hash)
    (srcInst : fuchsia_merkle_core.AlignedByteSource Src)
    (src : Src)
    (slice : Slice U8) : Prop where
  buffer_len_eq :
    srcInst.buffer_len src = .ok (Slice.len slice)
  hash_aligned_block_eq :
    ∀ (block_offset : U64) (byte_pos buf_len u_len : Usize),
      byte_pos.val % BLOCK_BYTES = 0 →
      (buf_len.val = BLOCK_BYTES ∨ buf_len.val = MIN_PAGE_BYTES) →
      0 < u_len.val →
      u_len.val ≤ buf_len.val →
      byte_pos.val + buf_len.val ≤ slice.val.length →
      block_offset.val % BLOCK_BYTES = 0 →
      srcInst.hash_aligned_block src block_offset byte_pos buf_len u_len =
        .ok (sha256 (specIdentity (u32OfNat u_len.val) 0#u64 block_offset ++
          (slice.val.drop byte_pos.val).take buf_len.val ++
          specZeroPad (BLOCK_BYTES - buf_len.val)))

/--
**Informal**: Canonical pure `AlignedByteSource` instance (`SliceByteSource`)
directly over `Slice U8`, returning `Slice.len` for `buffer_len` and hashing
`specIdentity u_len 0 block_offset ++ slice[byte_pos .. byte_pos + buf_len] ++
specZeroPad (8192 - buf_len)` for `hash_aligned_block`, witnessing non-vacuity
of `RefinesAlignedByteSource`.
-/
def SliceByteSource (sha256 : List U8 → Hash) :
    fuchsia_merkle_core.AlignedByteSource (Slice U8) where
  buffer_len slice := .ok (Slice.len slice)
  hash_aligned_block slice block_offset byte_pos buf_len u_len :=
    .ok (sha256 (specIdentity (u32OfNat u_len.val) 0#u64 block_offset ++
      (slice.val.drop byte_pos.val).take buf_len.val ++
      specZeroPad (BLOCK_BYTES - buf_len.val)))

end FuchsiaMerkle.Assumptions
