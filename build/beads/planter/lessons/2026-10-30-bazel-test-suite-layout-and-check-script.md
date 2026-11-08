# Bazel test suite layout and check script registration

- **Learned from:** inner loop friction on CL I97179041bdc8f50598b4061dccb598cc1b87b170, patchset 7 (task `src-diagnostics-triage`)
- **Date:** 2026-10-30
- **Changed:** `checks/test_suite_layout.sh`

## Root cause

The check `test_suite_layout` was enabled in `checks/manifest.json`
(`script_file: "test_suite_layout.sh"`), but `checks/test_suite_layout.sh` was
missing on disk. Because `tools/run_checks.sh` downgraded missing check scripts
to an `INFO` finding (`check_script_missing_in_pin`) while Planter's post-report
check executor invoked `bash` on the missing script path and failed with exit
status 127 (`check_script_failure`), the coding agent saw `run_checks.sh` pass
and could not resolve the post-report failure.

## Why not a one-off fix

Every migration task runs all enabled checks in `checks/manifest.json` after the
coding agent reports; leaving `checks/test_suite_layout.sh` missing causes an
unrecoverable exit status 127 `check_script_failure` across all migration tasks
regardless of target directory.
