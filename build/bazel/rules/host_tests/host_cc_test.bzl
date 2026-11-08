# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macros for defining `host_test()` targets for C/C++ binaries."""

load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("//build/bazel/rules/cc:fx_cc_test_binary.bzl", "fx_cc_test_binary")
load(":host_test.bzl", "host_test")

def legacy_host_cc_test(
        name,
        binary_name = "",
        test_label = None,
        test_args = [],
        test_data = [],
        target_compatible_with = None,
        visibility = None,
        **kwargs):
    """Define a host test for a C/C++ test binary that can be used with Fuchsia test runners.

    This is a convenience macro to call fx_cc_test_binary() and host_test()
    together, for a C/C++ test that only runs on host.

    Unlike cc_test(), these tests will be usable with `fx test` and `botanist`, and can
    still be run locally using `fx bazel test --config=host <label>`.

    To also run an existing fx_cc_test_binary() on host, for example one that
    is already packaged as a device test, do not use this macro, which would
    build a second binary from the same sources. Instead, wrap the existing
    binary directly with:

    ```
    host_test(
        name = "<name>_host_test",
        binary = ":<name>",
        target_compatible_with = HOST_OS_CONSTRAINTS,
    )
    ```

    Args:
      name: The name of the host test.
      binary_name: Optional. The name of the fx_cc_test_binary() target, defaults to
        'name + "_bin"'.
      test_label: Optional test label to appear in tests.json.
      test_args: Arguments to pass to the test binary. Do not use `args`.
      test_data: Optional. The data dependencies for the test target itself.
      target_compatible_with: Optional. Platform constraints for the generated targets.
        Defaults to HOST_OS_CONSTRAINTS.
      visibility: Optional. Visibility of the top-level test target, the leaf
        fx_cc_test_binary() target will have private visibility instead.
      **kwargs: Arguments to pass to `fx_cc_test_binary()`, e.g. `srcs`, `deps`,
        `build_flags`, `disable_lto` or `transitive_features`.
    """
    if kwargs.get("args"):
        fail("Use `test_args` to pass test arguments instead of `args`")

    binary_name = binary_name if binary_name else name + "_bin"
    if target_compatible_with == None:
        target_compatible_with = HOST_OS_CONSTRAINTS

    fx_cc_test_binary(
        name = binary_name,
        target_compatible_with = target_compatible_with,
        visibility = ["//visibility:private"],
        **kwargs
    )

    host_test(
        name = name,
        binary = ":" + binary_name,
        test_label = test_label,
        test_args = test_args,
        data = test_data,
        target_compatible_with = target_compatible_with,
        visibility = visibility,
    )

host_cc_test = legacy_host_cc_test
