---
min_risk: high
---

# Risk Classification for `spec/` (Formal Specifications & Assumptions)

This directory defines the Trusted Computing Base (TCB) for the formal
verification of `fuchsia_merkle_core`:

- `Assumptions.lean` states the cryptographic hardness predicates
  (`HashCollision`, `HashZeroPreimage`) and the behavioral contracts assumed of
  external traits (`Sha256State`, `LeafCollector`, `AlignedByteSource`)
  implemented outside `fuchsia_merkle_core`.
- `Spec.lean` defines the pure mathematical Fuchsia Merkle Tree protocol
  (`//docs/concepts/security/merkleroot.md`), including 12-byte block identity
  headers, zero-padding rules, empty-blob hashing, bottom-up multi-level tree
  reduction, and read-sized level-1 digest computation.
- `Contract.lean` states the top-level theorems linking the extracted Rust
  functions in `fuchsia_merkle_core` to `Assumptions.lean` and `Spec.lean`.

## Why Changes to `spec/` Are High Risk

Unlike `../proof/`, the Lean 4 kernel cannot tell whether a definition in
`spec/` accurately matches Fuchsia's blob integrity requirements or makes an
unrealistic assumption about external callers and trait wrappers:

- An overly strong hypothesis in `Assumptions.lean` (for example, assuming
  `AlignedByteSource::hash_aligned_block` zeros unhashed tail bytes on its own
  when `PtrByteSliceSource` does not) makes verifier soundness theorems
  vacuously true or misses unzeroed page-tail bugs.
- A protocol discrepancy in `Spec.lean` (for example, incorrect empty-blob,
  identity-header, or partial-level zero-padding behavior) allows a broken
  Merkle builder or verifier to pass verification.
- Removing a theorem, adding an unintended precondition, weakening trailing-zero
  page integrity conclusions, or omitting `#print axioms` in `Contract.lean`
  weakens the end-to-end blob integrity guarantee.

All changes to files in `spec/` are therefore **High risk** (`min_risk: high`)
and require careful human review alongside the checks in
[`../REVIEW.md`](../REVIEW.md).
