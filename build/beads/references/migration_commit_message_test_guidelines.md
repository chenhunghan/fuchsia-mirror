# Commit Message Test Footer Guidelines for Bazel Migration CLs

This document defines guidelines for formatting and consolidating `Test:` footers in Git commit messages for GN-to-Bazel migration changelists (CLs).

---

## 1. Overview

Every migration CL must include a `Test:` footer documenting the exact commands used to verify the build and tests. These commands serve as a reproducible record for reviewers and automated verification harnesses (such as Planter).

---

## 2. Core Guidelines

### A. Consolidate to At Most 3 Lines

During multi-directory migrations or iterative development, dozens of per-package incremental commands (e.g., individual `fx build` or `fx bazel build` invocations) are often executed.
- **Do not list per-package incremental commands.**
- Consolidate verification commands down to at most **3 `Test:` lines** total.
- Combining targets into single commands (or listing top-level verification targets) keeps the commit message concise and readable.

### B. Record Only Executed and Passing Commands

- Every command in the `Test:` footer must have actually been run and exited with status `0`.
- **Do not include speculative commands.** For example, do not list `fx test` commands if tests were only built but not executed (e.g. when an emulator or physical device was not available).

### C. Quote Toolchain Parentheses for Bash Safety

Automated verification tools (such as Planter's test verifier) execute `Test:` lines via `bash -c`.
- Any GN label containing toolchain specifications with parentheses must be properly shell-quoted:
  - **Correct:** `Test: fx build '//path/to:target(//build/toolchain:host_x64)'`
  - **Incorrect:** `Test: fx build //path/to:target(//build/toolchain:host_x64)` (causes bash syntax error near unexpected token `(`)

### D. Ensure Targets Are Configured in the Build Graph

- The paramount requirement is that **every listed `Test:` command was actually run and passed**.
- Targets do not necessarily need to be part of the default build graph, but if they are not, they **must be explicitly added to the active build graph** (for example, via `fx set ... --with //path/to:tests` or `args.gn`) so they can actually be built and tested.
- Never record commands against targets that were not configured into the build graph, as running `fx build` against unconfigured GN labels will fail (e.g. with `Unknown GN label`).
