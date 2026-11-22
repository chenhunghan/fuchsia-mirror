---
name: code-simplification
description: >
  Fuchsia code simplification and structural clarity review rubric covering
  unnecessary scope and machinery, in-tree and standard-library reuse, data-
  structure and layer altitude, control-flow and algorithmic clarity, and
  focused unit tests (avoiding testing theatre). Intended for use by review
  agents and orchestrators (`/review simplify` and `/review deep`). Do NOT use
  as a standalone review orchestrator (use `/review` instead).
---

# Fuchsia Code Simplification Guidelines

Review newly added or modified code for opportunities to delete unnecessary
machinery, reuse existing Fuchsia utilities, simplify data structures or layer
boundaries, flatten convoluted control flow, and keep unit tests focused on
behavior rather than implementation details.

## Core Principles & Hard Guardrails

- **Preserve exact behavior and contracts**: Every suggested simplification must
  preserve observable functionality, error handling, locking/async cancellation
  safety, and published SDK FIDL, Zircon syscall, DFv2 driver, or C ABI
  contracts.
- **Suggestions only (`severity: "suggestion"`)**: Simplification findings
  critique working code that could be cleaner, smaller, or better-factored;
  always emit `"severity": "suggestion"` (never `"blocking"`).
- **Focus on the diff's blast radius**: Evaluate newly introduced or actively
  modified functions, types, and tests. Do not propose drive-by rewrites of
  untouched legacy code or verbatim-moved files, and do not propose refactors
  that require sweeping edits across unrelated callers outside the change.
- **Clarity beats brevity (no "code golf")**: Prefer explicit, readable control
  flow over dense one-liners, deeply chained combinators, or clever macros. If a
  helper or intermediate variable makes invariants clearer, keep it.
- **Applicability**: If your assigned files contain only prose, documentation,
  or static data/configuration (`.md`, `.yaml`, `.json`, `.json5`, `.textproto`,
  `OWNERS`) with no executable logic, build rules, schemas, or tests to
  simplify, return `decision: "not_applicable"` with empty `findings`.
- **Rule references**: Cite the matching rule ID below
  (`code-simplification/scope`, `code-simplification/reuse`,
  `code-simplification/altitude`, `code-simplification/clarity`, or
  `code-simplification/tests`) and link the corresponding lines of this
  `SKILL.md` in `rule_link`.

## 1. Scope & Unnecessary Machinery (`code-simplification/scope`)

Look for newly added code that does not need to exist:

- **Single-use indirection**: A new trait, abstract base class, factory,
  builder, generic type parameter, callback hook, or pass-through wrapper that
  has only one concrete implementation or caller and is not required by a
  published SDK FIDL, driver, or test-seam boundary. Recommend deleting the
  indirection and using the concrete type or function directly.
- **Speculative configurability**: New flags, options structs, fallback ladders,
  or extension points added for hypothetical future callers that do not exist in
  the tree.
- **Unreachable defensive code**: Branches, null/empty guards, retries, or
  fallback paths handling a state that caller types or established module
  invariants already rule out. Before recommending deletion, search callers
  (`git grep` or `rg`, excluding `//out`) to confirm no path can produce that
  state. Preserve validation at trust, IPC/FIDL, syscall, hardware/MMIO, and
  deserialization boundaries.
- **Dead weight & Change-narrating comments**: Newly orphaned helpers, unused
  struct fields or parameters, and comments that merely restate *what* the next
  line does or narrate the CL/prompt history instead of explaining non-obvious
  *why* constraints or invariants.

## 2. In-Tree & Standard-Library Reuse (`code-simplification/reuse`)

Look for newly written code that reinvents functionality already available:

- **Existing Fuchsia utilities & sibling helpers**: Hand-rolled string/path
  parsing, bit manipulation, async/timer plumbing, Zircon handle/status
  wrappers, or test helpers that duplicate utilities in sibling modules,
  `//src/lib`, `//sdk/lib`, `//zircon/system/ulib`, or local `testing/` helpers.
- **Standard library & approved crates**: Custom loops or data structures
  replaceable by Rust `std`/`core` combinators, C++ standard/`fbl`/`fit`/`zx`
  utilities (`zx::result`, `fit::defer`), or crates already present in
  `third_party/rust_crates/Cargo.toml` (per `AGENTS.md`, never introduce new
  external dependencies).
- **Verification requirement**: Search the repository (`git grep` or `rg`,
  excluding `//out`) before reporting. Only emit a reuse finding when you have
  verified that the existing symbol exists, has compatible visibility and build
  layering (host vs. target, kernel vs. userspace, SDK category), and can cite
  its exact path and symbol name in `why` or `trace`.

## 3. Altitude & Data-Structure Shape (`code-simplification/altitude`)

Evaluate whether the change solves the problem at the right architectural layer
and with the right data representation:

- **Tighter types over coupled flags (`code-simplification/altitude`)**: Two or
  more coupled `bool` flags, parallel `Option`/`std::optional` fields, or
  stringly-typed tags on a struct/class that force callers to check invalid
  combinations, when a single Rust `enum` / C++ `std::variant` or state struct
  makes invalid states unrepresentable and eliminates repeated branching.
- **Root vs. symptom & layer placement (`code-simplification/altitude`)**:
  Special-case branches, parameter sprawl, or ad-hoc conversions bolted onto
  individual call sites (or leaked into shared infrastructure for a single
  caller) when placing the invariant in one shared boundary/type removes
  repeated plumbing. Provide a concrete `suggested_diff` or compact sketch
  showing the simpler shape when localized.

## 4. Control-Flow & Algorithmic Clarity (`code-simplification/clarity`)

Look for localized complexity that obscures intent without adding capability:

- **Deep nesting & convoluted branching**: Conditionals, `match`, or `switch`
  blocks nested 3+ levels deep, or chained boolean/ternary expressions, that
  flatten cleanly using guard clauses, early returns, `let ... else`, or `?` /
  `zx::result` propagation.
- **Copy-paste with slight variation**: Near-identical code blocks introduced in
  the diff that unify cleanly into a small local helper or closure without
  introducing premature abstraction.
- **Avoidable multi-pass or $O(N^2)$ work**: Repeated linear scans
  (`.iter().find()` inside a loop), redundant clones/allocations, or re-parsing
  data already in memory when a direct lookup (`HashMap`/`BTreeSet`) or single
  pass is both simpler and faster.

## 5. Focused Tests & Anti-"Testing Theatre" (`code-simplification/tests`)

On new or modified tests, keep suites high-signal and resilient to benign
refactoring:

- **Assert only the behavior under test**: Flag tests that `ASSERT`, `EXPECT`,
  or `assert_eq!` tangential fields, internal helper call counts, or unrelated
  side effects outside the specific contract named by the test.
- **No mock-echoing or vacuous smoke tests**: Flag tests that solely verify a
  mock/fake returns the canned value configured two lines above, re-check
  default struct initialization without invoking production logic, or duplicate
  an identical path already covered in the same test module.
- **Consolidate repetitive test boilerplate**: When multiple new multi-line test
  functions differ only in input parameters and expected output, suggest
  collapsing them into a table-driven loop, `#[test_case]`, or GTest `TEST_P`
  fixture-provided failure messages or line attribution remain clear.
