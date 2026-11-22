# Code Review Guidance for `//src/starnix/lib/ebpf/verification`

This directory contains the Lean 4 formal specification (`spec/`) and
correctness proofs (`proof/`) for `//src/starnix/lib/ebpf/core`.

## 1. Reviewing Changes to `spec/` (High Risk - Security-Critical TCB)

Files under `spec/` (`Assumptions.lean`, `Spec.lean`, `Contract.lean`) define
what it means for `ebpf_core` to be safe and what assumptions are made about
its external callers and trait implementations (`EbpfMachineBackend`,
`CoreVerifierLogger`). See [`spec/RISK.md`](spec/RISK.md).

When reviewing any change touching `spec/*.lean`, verify all of the following:

### 1.1 Informal-to-Formal Equivalence Check (Primary Agent Responsibility)
Every `structure`, `inductive`, `class`, `def`, `abbrev`, `theorem`, and `lemma`
in `spec/*.lean` must have a preceding `/-- ... -/` docstring with a
`**Informal**:` section in plain, precise English.
- Compare the `**Informal**:` English statement line-by-line against the formal
  Lean 4 definition or theorem signature.
- Flag a blocking comment if the formal Lean statement is weaker than the
  `**Informal**:` prose (e.g., if the formal theorem adds an unmentioned
  precondition, omits a safety clause promised in English, or restricts a
  quantifier to a subset of states).
- Flag a blocking comment if a formal definition or theorem is modified without
  updating its `**Informal**:` English docstring to match.

### 1.2 No Unsound Axioms or Admits in `spec/`
- Reject any `sorry`, `admit`, `axiom`, `opaque`, `partial`, `unsafe`,
  `implemented_by`, or `extern` in `spec/Assumptions.lean`, `spec/Spec.lean`,
  or `spec/Contract.lean`.
- All external assumptions must be expressed as explicit `Prop`-valued
  structures or predicates (e.g., `ValidHostLayout`, `ValidVerifierLogger`,
  `RefinesMachineBackend`) passed as hypotheses to the theorems in
  `spec/Contract.lean`, never as global `axiom` declarations.

### 1.3 Strict Import and Dependency Layering
- `spec/Assumptions.lean` may only import `Extracted` and `Aeneas`. It must
  never import `Spec`, `Proof`, or `Contract`.
- `spec/Spec.lean` may only import `Assumptions`, `Extracted`, and `Aeneas`.
  It must never import `Proof` or `Contract`.
- `spec/Contract.lean` may import `Assumptions`, `Spec`, `Proof`, `Extracted`,
  and `Aeneas`, must delegate proof bodies to `Proof`, and must include a
  `#print axioms <theorem_name>` line for every theorem.

### 1.4 External Assumption Realism (`spec/Assumptions.lean`)
- Check that any hypothesis added to `ValidHostLayout`, `EntryStatesConform`,
  `ValidVerifierLogger`, or `RefinesMachineBackend` actually holds for the
  production Starnix eBPF runtime in `//src/starnix/lib/ebpf/src/`. Adding an
  assumption that is false at runtime makes the theorems in `Contract.lean`
  vacuously true on paths where that assumption is violated.

## 2. Reviewing Changes to `proof/` (Low Risk - Machine-Checked Proofs)

Files under `proof/` (`Proof.lean`) contain auxiliary invariants, helper
lemmas, and tactic scripts that prove the theorems in `spec/Contract.lean`. See
[`proof/RISK.md`](proof/RISK.md).
- Because `spec/` does not import `proof/` and the Lean 4 kernel checks that
  `Proof.lean` discharges the exact theorem signatures in `spec/Contract.lean`,
  refactoring lemmas, tactics, or proof structure in `proof/Proof.lean` cannot
  compromise the specification.
- Verify that `proof/*.lean` does not introduce `axiom`, `opaque`, `unsafe`,
  `implemented_by`, or `extern`.
