# Unreferenced gn unit test scoped fx gen and unconfigured fx test footer validation

- **Learned from:** inner loop friction on CL If1afbe49d3a4fbf4da55a10044879190b255500e, patchset 3 (task `src-lib-diagnostics-inspect-rust-bench-batch`)
- **Date:** 2026-10-30
- **Changed:** `checks/build_verification.sh`, `checks/commit_message_format.sh`, `checks/tests_json_parity.sh`, `checks/verify_tests_json_parity.py`, `prompts/coder.md`

## Root cause

Two systemic machinery defects caused the inner-loop friction: (1) In
checks/verify_tests_json_parity.py and checks/tests_json_parity.sh,
extract_gn_test_targets() unconditionally extracted `<name>_test` for any
non-host rustc_library/rustc_binary with `with_unit_tests = true` even when
`:<name>_test` was never referenced or packaged anywhere in BUILD.gn. Because an
unpackaged non-host `<name>_test` ELF binary emits no test_spec in tests.json
and has no enclosing group("tests"), resolve_highest_enclosing_groups() fell
back to injecting the bare `//<pkg>:<name>_test` label into
`developer_test_labels` (`scoped_dev_roots`). After migrating the package and
deleting the unreferenced GN target, the post-change scoped `fx gen` failed with
`ERROR Unresolved dependencies: //:developer_specified_tests needs
//<pkg>:<name>_test`. (2) When `<build dir>/tests.json` is non-empty (configured
in `args.gn` for another area's test group) and does not contain the migrated
package's tests, `prompts/coder.md` and `build_verification.sh`
(`unconfigured_fx_test_footer_label`) gave contradictory guidance:
`build_verification.sh` falsely reported valid leaf test targets absent from a
non-empty `tests.json` as 'non-test or aggregator target(s)' and told the coder
in its remediation to pass `Test: fx test //<dir>:<test_target>`, which then
failed Planter's `test_verifier` (`fx test --dry`) with
`missing_fx_test_target`.

## Why not a one-off fix

Hardcoding exclusions for the specific benchmark packages in this task would
leave every future migration that touches a package with an unreferenced
`with_unit_tests = true` target or runs in a worktree whose `args.gn` populates
`<build dir>/tests.json` with a different subsystem's tests vulnerable to the
exact same post-change `fx gen` failure and `fx test` footer loop. Instead, we
updated `verify_tests_json_parity.py` and `tests_json_parity.sh` to ignore
unreferenced non-host `<name>_test` targets and filter removed GN targets from
post-change scoped `args.gn` roots, and updated `build_verification.sh`,
`commit_message_format.sh`, and `prompts/coder.md` to distinguish valid test
targets absent from a non-empty `<build dir>/tests.json` and direct the coder to
use `fx bazel test|build --config=...` footers.
