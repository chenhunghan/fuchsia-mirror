# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macros for defining `host_test()` targets for C/C++ binaries."""

load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")
load("@rules_cc//cc:cc_test.bzl", "cc_test")
load(":host_test.bzl", "host_test")

def legacy_host_cc_test(
        name,
        binary_name = "",
        test_label = None,
        test_args = [],
        test_data = [],
        tags = [],
        visibility = None,
        **kwargs):
    """Define a host test wrapping a C/C++ test that can be used with Fuchsia test runners.

    This is a convenience macro to call cc_test() and host_test() together.

    Unlike cc_test(), these tests will be usable with `fx test` and `botanist`, and can
    still be run locally using `fx bazel test --config=host <label>`.

    The "manual" tag will be set on the cc_test() target. In practice something like
    `fx bazel test --config=host //build/bazel/rules/host_tests/tests/cc/...`
    will correctly only run one test target, instead of two for each host_cc_test()
    definition.

    Args:
      name: The name of the host test.
      binary_name: Optional. The name of the cc_test target, defaults to 'name + "_bin"'.
      test_label: Optional test label to appear in tests.json.
      test_args: Arguments to pass to the test binary. Do not use `args`.
      test_data: Optional. The data dependencies for the test target itself.
      tags: Optional. List of test tags.
      visibility: Optional. Visibility of the top-level test target, the leaf
        cc_test() target will have private visibility instead.
      **kwargs: Arguments to pass to `cc_test`.
    """
    if kwargs.get("args"):
        fail("Use `test_args` to pass test arguments instead of `args`")

    binary_name = binary_name if binary_name else name + "_bin"

    tags = (tags or [])
    if "manual" not in tags:
        tags = tags + ["manual"]

    cc_test(
        name = binary_name,
        tags = tags,
        target_compatible_with = HOST_CONSTRAINTS,
        visibility = ["//visibility:private"],
        **kwargs
    )

    host_test(
        name = name,
        binary = ":" + binary_name,
        test_label = test_label,
        test_args = test_args,
        data = test_data,
        target_compatible_with = HOST_CONSTRAINTS,
        visibility = visibility,
    )

host_cc_test = legacy_host_cc_test
