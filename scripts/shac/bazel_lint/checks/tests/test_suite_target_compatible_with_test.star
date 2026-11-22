# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for bazel_test_suite_target_compatible_with."""

load(
    "//scripts/shac/bazel.star",
    "_bazel_test_suite_target_compatible_with",
)
load("./base_test.star", "run_lint_test")

_ERROR_MSG = (
    "Do not set `target_compatible_with` on `test_suite()`; set it on the " +
    "leaf tests instead. Bazel silently skips an incompatible `test_suite()` " +
    "and every test in it, which hides misplaced tests from tests.json. " +
    "See https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/tests#test-suite-target-compatible-with."
)

_VIOLATION = 'test_suite(name = "host_suite", target_compatible_with = HOST_OS_CONSTRAINTS)\n'

def _run_test_suite_target_compatible_with_test(files):
    return run_lint_test(
        _bazel_test_suite_target_compatible_with,
        "bazel_test_suite_target_compatible_with",
        files,
    )

def test_disallowed_test_suite_target_compatible_with():
    res = _run_test_suite_target_compatible_with_test({
        "pkg/BUILD.bazel": "\n" + _VIOLATION + """test_suite(name = "fuchsia_suite", target_compatible_with = ["@platforms//os:fuchsia"])
""",
        # Macros that call `native.test_suite()` define suites too.
        "pkg/defs.bzl": """
def fuchsia_tests(name):
    native.test_suite(name = name, target_compatible_with = ["@platforms//os:fuchsia"])
""",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "warning",
            message = _ERROR_MSG,
            filepath = "pkg/BUILD.bazel",
            line = 2,
            col = 33,
            end_col = 77,
        ),
        testing.finding(
            level = "warning",
            message = _ERROR_MSG,
            filepath = "pkg/BUILD.bazel",
            line = 3,
            col = 36,
            end_col = 87,
        ),
        testing.finding(
            level = "warning",
            message = _ERROR_MSG,
            filepath = "pkg/defs.bzl",
            line = 3,
            col = 36,
            end_col = 87,
        ),
    ))

def test_allowed_target_compatible_with_on_leaf_tests():
    res = _run_test_suite_target_compatible_with_test({
        "pkg/BUILD.bazel": """
test_suite(name = "tests", tests = [":host_only_test", ":not_on_coverage_test"])
host_test(name = "host_only_test", target_compatible_with = HOST_OS_CONSTRAINTS)
host_rustc_test(
    name = "not_on_coverage_test",
    target_compatible_with = select({
        "//build/bazel/config/select/is_coverage:true": ["@platforms//:incompatible"],
        "//conditions:default": [],
    }),
)
""",
    })
    asserts.eq(res.findings, ())

def test_ignored_file_types():
    res = _run_test_suite_target_compatible_with_test({
        "MODULE.bazel": _VIOLATION,
        "WORKSPACE.bazel": _VIOLATION,
    })
    asserts.eq(res.findings, ())

def run_test_suite_target_compatible_with_tests():
    """Runs all test_suite_target_compatible_with tests."""
    test_disallowed_test_suite_target_compatible_with()
    test_allowed_target_compatible_with_on_leaf_tests()
    test_ignored_file_types()
