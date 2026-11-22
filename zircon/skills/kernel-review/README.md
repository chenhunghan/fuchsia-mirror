# Kernel Review

`/kernel-review` evaluates the current Fuchsia git commit (`HEAD`) and reports
whether it is ready for human review. It does not modify the commit or touch
Gerrit unless explicitly instructed.

See [SKILL.md](SKILL.md) for the full set of checks.

## What it runs

Step | Check                                | When
---- | ------------------------------------ | ----
0    | Pre-flight commit triage             | Always
1    | Commit message                       | Always
2    | Build and test                       | Default on; can be skipped
3    | C++ -> Rust conversion (`babeldiff`) | Auto: commit ports C++ to Rust
4    | Fuchsia code review                  | Always
5    | Performance and code generation      | Off by default; `brief`/`thorough`
6    | Concurrency                          | Auto: locks, atomics, shared state
7    | Provenance                           | Auto: raw/tagged ptrs, allocators
8    | Unsafe Rust                          | Auto: `unsafe`, `asm!`, pinning
9    | Code formatting                      | Always

Each step writes its own report artifact, followed by a summary with an overall
ready/not-ready recommendation.

## Knobs

Knobs are given in plain language when invoking the command.

* **Build and test** (Step 2): on by default. Say "skip build", "no build/test",
  or similar to skip it.
* **Architectures** (Step 2): x86 is always built and tested, plus any
  architecture whose arch-specific code is modified. Name extra architectures
  (e.g. "also arm64", "include riscv64") to add them.
* **Performance analysis** (Step 5):
    * `none` (default): skipped.
    * `brief`: disassembles only the modified functions using the existing build
      config, with at most one extra incremental `HEAD~1` build for a
      before/after comparison. Used when you ask for "performance analysis"
      without saying "thorough".
    * `thorough`: x86 and arm64 disassembly, parity checks for
      behavior-preserving changes, and may test candidate code changes.
* **Gerrit**: read-only by default. The skill only uploads or comments on Gerrit
  if you explicitly ask it to.

Steps 3 and 6-8 cannot be toggled; Step 0 enables them automatically based on
the contents of the diff.

## Examples

```
/kernel-review
```

Default review: build/test on x86 (plus touched arches), no performance
analysis.

```
/kernel-review skip build
```

Fast static review only (no compilation or tests).

```
/kernel-review also build and test arm64
```

Adds arm64 to the build/test matrix.

```
/kernel-review with a brief performance analysis
```

Default review plus a disassembly check of the modified functions.

```
/kernel-review skip build, thorough performance analysis
```

Skips Step 2's test run but performs a full x86 + arm64 codegen analysis (builds
required for disassembly still happen).
