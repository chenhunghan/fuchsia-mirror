# Code Review Guidance for `//src/sys/pkg/lib/fuchsia-merkle/verification`

This directory contains the Lean 4 formal specification (`spec/`) and
correctness proofs (`proof/`) for `//src/sys/pkg/lib/fuchsia-merkle/core`.

## 1. Reviewing Changes to `spec/` (High Risk - Security-Critical TCB)

Files under `spec/` (`Assumptions.lean`, `Spec.lean`, `Contract.lean`) define
the Fuchsia Merkle Tree protocol, end-to-end blob and page-tail integrity
properties, and the behavioral contracts assumed of callers and external trait
implementations (`Sha256State`, `LeafCollector`, `AlignedByteSource`). See
[`spec/RISK.md`](spec/RISK.md).

When reviewing any change touching `spec/*.lean`, verify all of the following:

### 1.1 Informal-to-Formal Equivalence Check (Primary Agent Responsibility)
Every `structure`, `inductive`, `class`, `def`, `abbrev`, `theorem`, and `lemma`
in `spec/*.lean` must have a preceding `/-- ... -/` docstring with a
`**Informal**:` section in plain, precise English.
- Compare the `**Informal**:` English statement line-by-line against the formal
  Lean 4 definition or theorem signature.
- Flag a blocking comment if the formal Lean statement is weaker than the
  `**Informal**:` prose (e.g., if a soundness theorem adds an unmentioned
  caller precondition, omits the trailing-zero page check
  `slice.val.drop unaligned_len.val = specZeroPad ...`, or introduces a
  non-standard cryptographic escape disjunct).
- Flag a blocking comment if a formal definition or theorem is modified without
  updating its `**Informal**:` English docstring to match.

### 1.2 No Unsound Axioms or Admits in `spec/`
- Reject any `sorry`, `admit`, `axiom`, `opaque`, `partial`, `unsafe`,
  `implemented_by`, `native_decide`, or `extern` in `spec/Assumptions.lean`,
  `spec/Spec.lean`, or `spec/Contract.lean`.
- All cryptographic and external trait hypotheses must be expressed as explicit
  `Prop`-valued predicates or structures (`HashCollision`, `HashZeroPreimage`,
  `ValidSha256State`, `ValidLeafCollector`, `RefinesAlignedByteSource`) in
  `spec/Contract.lean` theorem statements, never as global `axiom` declarations.

### 1.3 Strict Import and Dependency Layering
- `spec/Assumptions.lean` may only import `Extracted` and `Aeneas`. It must
  never import `Spec`, `Proof`, or `Contract`.
- `spec/Spec.lean` may only import `Assumptions`, `Extracted`, and `Aeneas`.
  It must never import `Proof` or `Contract`.
- `spec/Contract.lean` may import `Assumptions`, `Spec`, `Proof`, `Extracted`,
  and `Aeneas`, must state theorems strictly over public `fuchsia_merkle_core`
  APIs (without accessing private struct fields), must delegate proof bodies to
  `Proof`, and must include a `#print axioms <theorem_name>` line for every
  theorem.

### 1.4 Protocol Fidelity & External Assumption Realism
- Verify that `specIdentity`, `specHashBlock`, `specHashInternal`,
  `specLeafHashes`, `specRootFromLeaves`, and `specMerkleRoot` match
  `//docs/concepts/security/merkleroot.md` byte-for-byte (including the 0-byte
  blob offset-0 special case and partial-block/partial-level zero padding).
- Verify that `ValidSha256State`, `ValidLeafCollector`, and
  `RefinesAlignedByteSource` hold for the production wrappers in
  `//src/sys/pkg/lib/fuchsia-merkle/src/` (`BoringSslSha256`,
  `CollectorBridge`, and `PtrByteSliceSource`) and that every soundness theorem
  remains paired with matching completeness theorems showing the preconditions
  are satisfiable on all valid blobs and reads.

## 2. Reviewing Changes to `proof/` (Low Risk - Machine-Checked Proofs)

Files under `proof/` (`Proof.lean`) contain auxiliary invariants, helper
lemmas, and tactic scripts that prove the theorems in `spec/Contract.lean`. See
[`proof/RISK.md`](proof/RISK.md).
- Because `spec/` does not import `proof/` and the Lean 4 kernel checks that
  `Proof.lean` discharges the exact theorem signatures in `spec/Contract.lean`,
  refactoring lemmas, invariants, or tactics in `proof/Proof.lean` cannot
  compromise the specification.
- Verify that `proof/*.lean` does not introduce `axiom`, `opaque`, `unsafe`,
  `implemented_by`, `native_decide`, or `extern`.
