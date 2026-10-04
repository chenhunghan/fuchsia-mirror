# Custom rust lint config default lint set resolution

- **Learned from:** inner loop friction on a local task (task `starnix-syscalls`)
- **Date:** 2026-09-26
- **Changed:** `checks/lint_config_parity.sh`, `checks/manifest.json`, `prompts/coder.md`, `prompts/reviewers/target_parity.md`

## Root cause

The lint_config_parity check unconditionally emitted a blocking
lint_config_semantics WARNING whenever a BUILD.bazel lint_config label did not
start with //build/config/rust/lints, without resolving the label or checking
whether the underlying rust_lint_config target already composed the default
clippy and rustc lint sets from //build/config/rust/lints.

## Why not a one-off fix

Exempting a single area lint_config label or target package would still falsely
block every other subsystem migration whose rustc_* targets reference a valid
area-specific rust_lint_config (or pre-existing alias) outside
//build/config/rust/lints, whereas resolving the target AST and verifying
default clippy/rustc inclusion catches only genuine lint-set omissions across
all packages.
