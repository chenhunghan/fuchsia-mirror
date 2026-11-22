---
min_risk: high
---

# Risk Classification for `spec/` (Formal Specifications & Assumptions)

This directory defines the Trusted Computing Base (TCB) for the formal
verification of `ebpf_core`:

- `Assumptions.lean` states the hypotheses assumed about external callers,
  host memory layout, and callback traits (`EbpfMachineBackend`,
  `CoreVerifierLogger`) implemented outside `ebpf_core`.
- `Spec.lean` defines the reference eBPF virtual machine operational semantics
  and safety predicates (`SafeAndTerminating`, `NonInterfering`).
- `Contract.lean` states the top-level theorems linking the extracted Rust
  functions in `ebpf_core` to `Assumptions.lean` and `Spec.lean`.

## Why Changes to `spec/` Are High Risk

Unlike `../proof/`, the Lean 4 kernel cannot tell whether a definition in
`spec/` accurately captures the real-world security requirements of Starnix or
makes an unrealistic assumption about the host environment:

- An overly strong hypothesis in `Assumptions.lean` (for example, assuming a
  callback always succeeds or never aliases memory when it can in production)
  makes theorems vacuously true on paths where that hypothesis fails.
- An incomplete safety predicate in `Spec.lean` (for example, omitting a class
  of out-of-bounds memory access or pointer leak from `SafetyViolation`) allows
  an unsound verifier change to pass verification.
- Removing a theorem, adding a precondition, or omitting `#print axioms` in
  `Contract.lean` weakens the end-to-end security guarantee.

All changes to files in `spec/` are therefore **High risk** (`min_risk: high`)
and require careful human review alongside the checks in [`../REVIEW.md`](../REVIEW.md).
