# Bazel2gn native tag and clippy warn all test parity

- **Learned from:** human review on CL I97179041bdc8f50598b4061dccb598cc1b87b170, patchset 3 (task `src-diagnostics-triage`)
- **Date:** 2026-10-29
- **Changed:** `checks/build_only_scope.sh`, `checks/lint_config_parity.sh`, `prompts/coder.md`

## Root cause

First, prompts/coder.md still showed # @bazel2gn:raw_attribute: for
exclude_toolchain_tags, assert_no_deps, and concurrent_jobs.rust_highmem instead
of bazel2gn's native tags and exec_properties = RBE_USE_LARGE_WORKER, and no
check rejected raw_attribute for exclude_toolchain_tags or unnecessary #
@bazel2gn:skip on target_compatible_with = HOST_OS_CONSTRAINTS. Second, in Bazel
//build/config/rust/lints:clippy_warn_all bakes _CLIPPY_WARN_PRODUCTION
(including clippy::redundant_clone) into the lint set, whereas GN's rustc_test
omits clippy_warn_production; because build_only_scope.sh and
lint_config_parity.sh unconditionally rejected fixing redundant_clone or adding
#[allow(clippy::...)] and LINT.IfChange/LINT.ThenChange comments in test .rs
files while not flagging unrequested disable_clippy = True in BUILD.bazel, the
agent disabled clippy on the test target instead of fixing the lint in the test
source.

## Why not a one-off fix

Fixing only the single package leaves every future Rust migration with host
binaries, exclude_toolchain_tags, path-sync comments, or clippy_warn_all on test
targets vulnerable to emitting raw_attribute workarounds, adding unnecessary #
@bazel2gn:skip annotations on HOST_OS_CONSTRAINTS, or setting disable_clippy =
True when build_only_scope and lint_config_parity block minimal test-source
clippy fixes and LINT.IfChange/LINT.ThenChange synchronization comments.
