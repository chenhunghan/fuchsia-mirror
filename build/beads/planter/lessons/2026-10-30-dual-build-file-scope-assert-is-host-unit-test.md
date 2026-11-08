# Dual build file scope assert is host unit test polarity parity

- **Learned from:** cq failure on CL I4d77863e5462a881b402992ae5450caffe381a6a, patchset 2 (task `ffx-lib-pkg-pbms`)
- **Date:** 2026-10-30
- **Changed:** `checks/build_verification.sh`, `checks/lint_config_parity.sh`, `checks/manifest.json`

## Root cause

When a pre-migration BUILD.gn restricts the entire file to the host toolchain
via a file-scope assert(is_host, ...) and declares a top-level rustc_library or
rustc_binary with with_unit_tests = true and group("tests") referencing
:<name>_test without ($host_toolchain), shac's check_bazel_test_parity.py
ignores file-scope assert(is_host) and classifies the pre-migration unit test as
a device leg. Authoring host_rustc_test (or rustc_test with unskipped
target_compatible_with = HOST_OS_CONSTRAINTS) in BUILD.bazel causes bazel2gn to
wrap the generated rustc_test("<name>_test") in if (is_host) below the BAZEL2GN
SENTINEL, changing its inferred shac polarity to host and triggering CLASS 1
SILENT ORPHANING on the device leg. Additionally, shac's bazel_test_parity.star
skips check_bazel_test_parity.py on uncommitted working trees when HEAD has a
Reviewed-on: footer, and build_verification.sh reused the pre-commit result
after committing because _fingerprint() only hashed the working tree.

## Why not a one-off fix

Fixing only a single package's BUILD.bazel and BUILD.gn files would leave every
other host-only dual-build package that relies on a file-scope assert(is_host,
...) with an unguarded pre-migration rustc_*(with_unit_tests = true) target
exposed to the same shac bazel_test_parity polarity mismatch and would leave
uncommitted static check runs blind to check_bazel_test_parity.py errors across
future migrations.
