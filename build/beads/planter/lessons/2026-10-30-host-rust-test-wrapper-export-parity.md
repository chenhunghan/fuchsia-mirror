# Host rust test wrapper export parity

- **Learned from:** inner loop friction on CL I4d77863e5462a881b402992ae5450caffe381a6a, patchset 2 (task `ffx-lib-pkg-pbms`)
- **Date:** 2026-10-30
- **Changed:** `checks/cq_reachability.sh`, `checks/manifest.json`

## Root cause

checks/cq_reachability.sh treated every rustc_test or rust_test target
containing HOST_OS_CONSTRAINTS as an independently exportable host test in
exportable, even when a wrap_host_rust_test or host_test in the same BUILD.bazel
already wrapped that binary_name and exported it via bazel_test_suite. Because
raw rustc_test targets do not provide FuchsiaHostTestInfo, requiring both
wrap_host_rust_test and the underlying rustc_test to be wired into
bazel_test_suite or an ancestor test_suite caused a false-positive
unexported_bazel_test error whenever a dual-build package paired rustc_test with
wrap_host_rust_test.

## Why not a one-off fix

Any dual-build Rust package that pairs an explicit rustc_test with
wrap_host_rust_test—such as packages with file-scope assert(is_host) that skip
HOST_OS_CONSTRAINTS on rustc_test to preserve shac bazel_test_parity polarity,
or packages with rustc_library_with_features host test wrappers—hits this
exact false positive unless cq_reachability.sh excludes local test binaries
already wrapped by wrap_host_rust_test or host_test.
