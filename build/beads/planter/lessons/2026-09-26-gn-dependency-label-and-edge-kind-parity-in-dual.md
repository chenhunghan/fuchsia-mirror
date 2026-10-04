# GN dependency label and edge-kind parity in dual-build migrations: a GN-only link-config wrapper (a * static target whose public configs link the static C++ runtime) swapped for the library it wraps, and a Rust public deps edge demoted to a private dep by bazel2gn, breaking GN links of tests and reverse dependencies that Bazel and the product-scoped fx build never exercise

- **Learned from:** cq failure on CL Ibc10722c545a28de240d3ace1e406d8ecf242f2c, patchset 1 (task `starnix-syscalls`)
- **Date:** 2026-09-26
- **Changed:** `checks/build_verification.sh`, `checks/gn_dep_parity.sh`, `checks/manifest.json`, `prompts/coder.md`, `prompts/panel.json`, `prompts/playbooks/case2_dual_build.md`, `prompts/reviewers/dual_build_sentinel.md`, `prompts/reviewers/target_parity.md`

## Root cause

No check compared migrated GN targets' dependency labels and edge kinds with the
base BUILD.gn. The coder prompt judged Bazel equivalence per package, so the
library behind a GN-only wrapper counted as an equivalent. Nothing warned that
bazel2gn emits every Rust dep as a private GN dep, which stops public_configs
forwarding. build_verification only ran the product-scoped fx build, which never
linked the migrated package's GN tests or its reverse dependencies' GN tests,
while Bazel links the C++ runtime through its toolchain and passed.

## Why not a one-off fix

Restoring the label in this one package only fixes this change. Every future
dual-build migration can still swap one of the 150+ GN-only targets above
sentinels (several carry link settings) for a sibling, or lose public config
forwarding through bazel2gn's private Rust deps, and CQ would again be the first
place those GN test links run. A deterministic label/edge parity check, a build
step for GN dependents, and matching coder, playbook and reviewer rules with one
sanctioned forwarding-group recipe catch the whole class before CQ.
