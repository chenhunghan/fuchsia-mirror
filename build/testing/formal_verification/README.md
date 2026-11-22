# Fuchsia Rust Formal Verification Infrastructure

This directory contains the shared Bazel/GN build rules, ownership configuration,
and Python verification runner for formally verifying pure-safe-Rust core crates
in Fuchsia using **Charon**, **Aeneas**, and **Lean 4**.

## Standard Directory Layout

A formally verified Rust component in Fuchsia is organized around a pure,
`#![forbid(unsafe_code)]` Rust `<crate>-core` crate paired with a sibling
`verification/` directory:

```text
<component>/
├── core/                         # Pure-safe-Rust core crate
│   ├── BUILD.bazel
│   └── src/
│       └── lib.rs
└── verification/
    ├── BUILD.bazel               # Invokes rust_lean_verification_test(...)
    ├── OWNERS                    # Includes /build/testing/formal_verification/VERIFICATION_OWNERS
    ├── REVIEW.md                 # Agentic & human code review rules for spec/ and proof/
    ├── README.md                 # Crate-specific overview (references this README)
    ├── lakefile.lean             # Lake package wiring
    ├── spec/                     # 🔒 SECURITY-CRITICAL (High risk)
    │   ├── RISK.md               # Sets min_risk: high and explains specification risks
    │   ├── Assumptions.lean      # Assumptions on external traits, callers & host ABI
    │   ├── Spec.lean             # Safety & functional specification of the crate
    │   └── Contract.lean         # Top-level theorem statements + `#print axioms`
    └── proof/                    # 🔧 ENGINEER-EDITABLE (Low risk)
        ├── RISK.md               # Explains why machine-checked proof edits are low risk
        └── Proof.lean            # Auxiliary invariants, lemmas, and proof bodies
```

## Key Design Principles

### 1. Governance via `VERIFICATION_OWNERS`, `RISK.md`, and `REVIEW.md`

- Every `verification/OWNERS` file includes
  [`VERIFICATION_OWNERS`](./VERIFICATION_OWNERS) via
  `include /build/testing/formal_verification/VERIFICATION_OWNERS` so
  maintainers of the formal verification infrastructure can update verification
  packages across the source tree alongside local subsystem `OWNERS`.
- `spec/RISK.md` sets `min_risk: high` and classifies all changes to `spec/` as
  **High risk** requiring expert human review.
- `proof/RISK.md` explains why proof-only changes under `proof/` are **Low risk**
  (because the Lean 4 kernel, `#print axioms`, and `lean4checker` mechanically
  verify the proof against the unchanged specification in `spec/`).
- `verification/REVIEW.md` instructs the code review agent to verify that every
  formal statement in `spec/` matches its paired `**Informal**:` English
  docstring and preserves strict layering between `Assumptions.lean`,
  `Spec.lean`, `Contract.lean`, and `Proof.lean`.

### 2. Separation of Trusted Computing Base (`spec/`) vs. Proofs (`proof/`)

| File | Risk Tier | Role & Import Rules |
| :--- | :--- | :--- |
| `spec/Assumptions.lean` | **High risk** | Formalizes assumptions about external dependencies (traits implemented outside `core/`, caller preconditions, host ABI layout). May import `Extracted` and `Aeneas`; **must never** import `Spec`, `Proof`, or `Contract`. |
| `spec/Spec.lean` | **High risk** | Formalizes the intrinsic safety and functional specification of the core crate. May import `Assumptions`, `Extracted`, and `Aeneas`; **must never** import `Proof` or `Contract`. |
| `spec/Contract.lean` | **High risk** | States the top-level theorems linking `Extracted` functions to `Assumptions` and `Spec`, delegates proof bodies (`:= by exact ...`) to `Proof.lean`, and emits `#print axioms` for every theorem. |
| `proof/Proof.lean` | **Low risk** | Contains engineer-editable inductive invariants, helper lemmas, and tactic scripts. Even an untrusted edit to `proof/Proof.lean` cannot weaken a theorem in `spec/Contract.lean`. |

### 3. Mandatory `**Informal**:` English Docstrings in `spec/`

Every top-level `structure`, `inductive`, `class`, `def`, `abbrev`, `theorem`,
and `lemma` in `spec/Assumptions.lean`, `spec/Spec.lean`, and
`spec/Contract.lean` must be immediately preceded by a `/-- ... -/` doc comment
containing an explicit `**Informal**:` section written in clear, precise English.

- **Human reviewers** read the `**Informal**:` English statements to judge
  whether external assumptions are realistic and whether the safety specification
  is complete.
- **Coding agents** check that each formal Lean statement faithfully matches the
  intent of its `**Informal**:` English statement.

## Running Verification Tests

When local `charon`, `aeneas`, and `lean` / `lake` binaries are available, set
the toolchain environment variables to extract `Extracted.lean`, build the Lake
package, and verify `#print axioms` and `lean4checker`:

```bash
LEAN_BIN_DIR=/path/to/lean/bin \
AENEAS_BIN_DIR=/path/to/aeneas \
AENEAS_LEAN_DIR=/path/to/aeneas/backends/lean \
CHARON_RUST_TOOLCHAIN_DIR=/path/to/rustup/toolchains/nightly-... \
fx test --host <verification_test_target>
```
