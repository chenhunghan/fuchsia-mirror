# Dual build test data helpers ancestor test suite and serial build checks

- **Learned from:** inner loop friction on CL I97179041bdc8f50598b4061dccb598cc1b87b170, patchset 4 (task `src-diagnostics-triage`)
- **Date:** 2026-10-29
- **Changed:** `checks/commit_message_format.sh`, `checks/cq_reachability.sh`, `tools/run_checks.sh`

## Root cause

Three distinct machinery bugs blocked dual-build host-test migrations: (1)
commit_message_format matched bare parenthesized calls without a trailing brace
block as GN target definitions (misclassifying GN built-in calls like
get_target_outputs as unmigrated targets), treated testonly pre-sentinel GN
copy/resource/host_test_data helpers kept for bazel2gn test_runtime_deps as
unmigrated targets when BUILD.bazel already defined host_test_data_* and test
targets, and passed an undefined change_base list to git() in
_is_ifttt_only_source_diff; (2) cq_reachability resolved base BUILD.bazel files
from raw PLANTER_CHANGE_BASE (trying HEAD before HEAD~1) instead of
effective_diff_base and rejected direct-child or newly populated subsystem area
test_suite targets when in-scope test labels left member_pkgs empty across
multi-patchset review rounds; and (3) run_checks.sh executed build_verification
and tests_json_parity concurrently in ThreadPoolExecutor against the shared
build directory and working tree, causing concurrent fx gen / regenerator.py
runs to race.

## Why not a one-off fix

Any dual-build package whose bazel2gn-generated GN tests retain pre-sentinel
copy/host_test_data helpers for GN test_runtime_deps, any migration wiring host
or device tests into a parent subsystem area test_suite across review rounds,
and any full run_checks.sh invocation running both build_verification and
tests_json_parity on the same build directory will hit the exact same false
positives, script crash, and regenerator race unless fixed generically in the
check and runner scripts.
