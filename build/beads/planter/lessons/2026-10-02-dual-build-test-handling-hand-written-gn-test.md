# Dual-build test handling: hand-written GN test templates (rustc test/go test) above the BAZEL2GN SENTINEL treated as bazel2gn-convertible targets

- **Learned from:** inner loop friction on [CL 1841581](https://fuchsia-review.googlesource.com/c/fuchsia/+/1841581), patchset 6 (task `cl-1841581`)
- **Date:** 2026-10-02
- **Changed:** `checks/migration_sanity.sh`

## Root cause

migration_sanity still listed rustc_test and go_test among the
bazel2gn-convertible GN templates, a leftover from before tests moved to
Bazel-only. Any GN test above the sentinel therefore raised
duplicate_target_above_sentinel with the fix 'define it in BUILD.bazel without #
@bazel2gn:skip and run fx bazel2gn'. Following that fix trips the same script's
test_generated_by_bazel2gn rule and contradicts the coder prompt, which allows a
test Bazel cannot run yet to stay hand-written in GN. When the only fix is a
source change (test code reading data at a path relative to the GN build
directory), which build_only_scope forbids, the coder cannot clear the ERROR at
all.

## Why not a one-off fix

Waiving the finding for one package leaves the contradiction in place: every
future dual-build migration that keeps a GN rustc_test or go_test above the
sentinel would get the same impossible ERROR. The coder would again have to
choose between a forbidden source edit, a Bazel test that fails, or a
bazel2gn-generated test. The check has to tell a real duplicate (the test
defined in both builds) from a test that legitimately stays in GN.
