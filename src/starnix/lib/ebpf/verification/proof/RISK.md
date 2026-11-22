# Risk Classification for `proof/` (Machine-Checked Proofs)

This directory contains the Lean 4 proof definitions, inductive invariants, and
helper lemmas (`Proof.lean`) that discharge the formal specifications declared
in [`../spec/Contract.lean`](../spec/Contract.lean).

## Why Changes to `proof/` Are Low Risk

- **One-way dependency**: Files in `../spec/` never import `proof/`. Editing
  `proof/Proof.lean` cannot alter the meaning of any assumption in
  `../spec/Assumptions.lean`, any safety property in `../spec/Spec.lean`, or any
  theorem statement in `../spec/Contract.lean`.
- **Mechanical verification by the Lean 4 kernel**: The verification test runner
  compiles `Contract.lean` against `Proof.lean`, audits `#print axioms` to
  confirm that no custom axioms were introduced, and replays the compiled proof
  terms in `lean4checker --fresh`. If a proof in `proof/` is flawed or does not
  match the theorem signature in `../spec/Contract.lean`, the Lean kernel
  rejects it.

Consequently, changes that only modify files under `proof/` (without modifying
`../spec/`, `../lakefile.lean`, or `../BUILD.bazel`) are **Low risk**.
