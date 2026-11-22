# Formal Verification of `fuchsia_merkle_core` (`//src/sys/pkg/lib/fuchsia-merkle`)

This package formally specifies and verifies the Fuchsia Merkle Tree core crate
([`//src/sys/pkg/lib/fuchsia-merkle/core`](../core)) using Charon, Aeneas, and
Lean 4.

See [`//build/testing/formal_verification/README.md`](../../../../../build/testing/formal_verification/README.md)
for the general verification architecture, `spec/` vs. `proof/` separation,
[`spec/RISK.md`](spec/RISK.md) / [`proof/RISK.md`](proof/RISK.md) /
[`REVIEW.md`](REVIEW.md) governance, and local toolchain setup.

## `fuchsia-merkle` Specification Overview

- **[`spec/Assumptions.lean`](spec/Assumptions.lean) - Cryptographic & External Trait Assumptions**:
  - Protocol block/digest/page sizing constants, 12-byte little-endian block
    identity encoding (`specIdentity`), and zero-padding (`specZeroPad`).
  - Cryptographic hardness predicates `HashCollision` (collision witness on a
    pure compression function `sha256 : List U8 → Hash`) and `HashZeroPreimage`
    (preimage witness mapping to `[0u8; 32]`).
  - Refinement contracts for external traits implemented in `fuchsia-merkle`:
    `ValidSha256State` (and pure witness `StreamSha256`) for `Sha256State`,
    `ValidLeafCollector` (and pure witness `ListLeafCollector`) for
    `LeafCollector`, and `RefinesAlignedByteSource` for `AlignedByteSource`
    (`PtrByteSliceSource` wrapping `PtrByteSlice` and BoringSSL/NEON SHA-256).
- **[`spec/Spec.lean`](spec/Spec.lean) - Fuchsia Merkle Tree Protocol Specification**:
  - Pure mathematical definitions of `//docs/concepts/security/merkleroot.md`:
    level-0 block hashing (`specHashBlock`), internal node hashing
    (`specHashInternal`), blob leaf digests (`specLeafHashes`), bottom-up tree
    reduction (`specRootFromLeaves`, `specMerkleRoot`), read-sized level-1
    digests (`specReadSizedHashes`), and multi-chunk streaming writes
    (`bufferedWriteAll`).
- **[`spec/Contract.lean`](spec/Contract.lean) - Top-Level Verified Theorems**:
  - **P0 (Domain Separation & Root Soundness)**: injectivity of `make_identity`,
    disjointness of leaf and internal node preimages, and collision-resistant
    uniqueness of `specRootFromLeaves` for equal-length leaf vectors.
  - **P1 (Panic & Overflow Freedom)**: totality of `hash_block_aligned`,
    `MerkleVerifier.{new, verify, verify_aligned}`, and
    `ReadSizedMerkleVerifier.{new, verify, verify_aligned}`.
  - **P2 (Streaming Builder Correctness)**: functional equivalence of
    `hash_block`, `hash_block_aligned`, `rebuild_root`, `MerkleRootBuilder`, and
    `BufferedMerkleRootBuilder` (including arbitrary chunk-partition invariance
    and ordered streaming leaf collection).
  - **P3 (Completeness)**: acceptance of all valid empty, slice, and zero-padded
    4 KiB page-aligned reads across `MerkleVerifier` and
    `ReadSizedMerkleVerifier`.
  - **P4 (Collision-Resistant Soundness & Page-Tail Integrity)**: end-to-end
    data authenticity and trailing-zero page enforcement (`unaligned_len..len`)
    across `MerkleVerifier` and `ReadSizedMerkleVerifier` (both unbounded and
    caller-size-bounded `fxblob` contracts).
  - **P5 (Zero-Hash Extension Characterization)**: exact characterization of why
    callers must enforce the canonical leaf-count bound when constructing
    `MerkleVerifier` (https://fxbug.dev/579509305).
- **[`proof/Proof.lean`](proof/Proof.lean) & [`proof/Proof/`](proof/Proof/) - Machine-Checked Proofs**:
  - Complete Lean 4 proofs discharging all 33 theorems in `spec/Contract.lean`
    with zero `sorry`s and zero user-defined axioms (`SpecLemmas.lean`,
    `Sha256Sim.lean`, `BuilderProofs.lean`, `VerifierCommon.lean`,
    `MerkleVerifierProofs.lean`, `ReadSizedSliceProofs.lean`, and
    `ReadSizedAlignedProofs.lean`).

## Running `:fuchsia_merkle_verification_test`

```bash
fx bazel test --config=host //src/sys/pkg/lib/fuchsia-merkle/verification:fuchsia_merkle_verification_test
```
