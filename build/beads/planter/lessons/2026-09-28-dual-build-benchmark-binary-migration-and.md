# Dual build benchmark binary migration and configured gn test verification

- **Learned from:** inner loop friction on a local task (task `starnix-line-discipline-pstate`)
- **Date:** 2026-09-28
- **Changed:** `checks/bazel_minimality.sh`, `checks/build_verification.sh`, `checks/migration_sanity.sh`, `checks/visibility_audit.sh`, `prompts/coder.md`, `prompts/playbooks/case2_dual_build.md`, `tools/find_rdeps.sh`

## Root cause

Two systemic gaps in the migration machinery caused Round 0 inner-loop friction.
First, `prompts/coder.md` and `prompts/playbooks/case2_dual_build.md` emphasized
migrating reference/bitrot-prevention libraries depended upon by
`group("tests")`, but did not explicitly instruct the coder that benchmark,
component, and example binaries (`rustc_binary`, `executable`, etc. backing
`fuchsia_component`, `fuchsia_package`, `fuchsia_component_perf_test`, or
`group("benchmarks")`) must also be migrated to `BUILD.bazel` along with any
unmigrated first-party benchmark dependencies, nor did they or
`checks/migration_sanity.sh` explain `bazel2gn`'s `rustc_binary` `crate_name` ->
`output_name` mapping or how to translate GN `declare_args()` blocks in migrated
dependencies (`# @bazel2gn:skip` on `<arg> = False` plus `crate_features =
["..."] if <arg> else []`). Additionally, `tools/find_rdeps.sh` returned empty
`per_target_recommended_visibility` when invoked on an unmigrated dependency
package before its `BUILD.bazel` existed. Second, `prompts/coder.md` Step 2
instructed the coder to run `fx build //<dir>:tests` for every touched
directory, even though `fx build <label>` fails with `ERROR: Unknown GN label
(not in the configured graph)` whenever `//<dir>:tests` is absent from
`<build_dir>/ninja_outputs.json`; when the coder recorded that command in
`CoderReport.tests_run` (or a `Test:` commit footer), Planter's `test_verifier`
failed.

## Why not a one-off fix

Fixing only the current target packages by hand would leave every future
dual-build package containing a benchmark or component binary
(`rustc_binary("bin")`, `executable`, etc.) or an unmigrated dependency using
`declare_args()` vulnerable to leaving convertible binaries above `## BAZEL2GN
SENTINEL` or failing `verify_bazel2gn` on `output_name` and `declare_args()`
translation, and would leave the coder prompt instructing agents to run and
record unconfigured `fx build //<dir>:tests` commands that fail `test_verifier`
across all library migrations whose `:tests` groups are not in the configured
product graph.
