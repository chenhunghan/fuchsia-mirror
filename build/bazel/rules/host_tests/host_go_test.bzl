# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macros for defining `host_test()` targets for Go binaries."""

load("@io_bazel_rules_go//go:def.bzl", "go_test")
load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load(":host_test.bzl", "host_test")
load(":host_test_data.bzl", "host_test_data_files")

def legacy_host_go_test(
        name,
        binary_name = "",
        test_args = [],
        test_data = [],
        tags = [],
        timeout = "5m",
        target_compatible_with = None,
        visibility = None,
        **kwargs):
    """Define a host test wrapping a Go binary that can be used with Fuchsia test runners.

    This is a convenience macro to call go_test() and host_test() together.

    Unlike go_test(), these tests will be usable with `fx test` and `botanist`, and can
    still be run locally using `fx bazel test --config=host <label>`.

    The "manual" tag will be set on the go_test() target. In practice something like
    `fx bazel test --config=host //build/bazel/rules/host_tests/tests/go/...`
    will correctly only run one test target, instead of two for each host_go_test()
    definition.

    The inner `go_test()` target (`binary_name`, defaulting to `name + "_bin"`)
    inherits `visibility` so GN `bazel_tool_paths()` / `bazel_host_tool()` targets
    can export the standalone compiled Go test executable into `tool_paths.json`.

    Args:
      name: The name of the host test.
      binary_name: Optional. The name of the go_test target, defaults to 'name + "_bin"'.
      test_args: Arguments to pass to the test binary. Do not use `args`.
      test_data: Optional. The data dependencies for the test target itself.
      tags: Optional. Standard Bazel tags forwarded to `go_test` (with `"manual"` appended).
      timeout: Optional. Override default timeout. Values must be valid Go durations
         such as "300ms", "1.5h" or "2h45m". See
         https://golang.org/cmd/go/#hdr-Testing_flags for details on timeout. See
         https://golang.org/pkg/time/#ParseDuration for duration format.
      target_compatible_with: Optional. Platform constraints for the generated targets.
         Defaults to HOST_OS_CONSTRAINTS.
      visibility: Optional. Standard Bazel visibility applied to both `<name>` and `<binary_name>`.
      **kwargs: Arguments to pass to `go_test`.
    """
    if "args" in kwargs:
        fail("Use `test_args` to pass test arguments instead of `args`")
    binary_name = binary_name if binary_name else name + "_bin"
    if target_compatible_with == None:
        target_compatible_with = HOST_OS_CONSTRAINTS

    binary_as_test_data = binary_name + ".test_data"

    tags = (tags or [])
    if "manual" not in tags:
        tags = tags + ["manual"]

    go_test(
        name = binary_name,
        tags = tags,
        target_compatible_with = target_compatible_with,
        visibility = visibility,
        **kwargs
    )

    host_test_data_files(
        name = binary_as_test_data,
        srcs = [":" + binary_name],
        target_compatible_with = target_compatible_with,
        testonly = True,
    )

    # Wrap the binary invocation with //tools/go_test_parser.
    test_data = test_data + [":" + binary_as_test_data]

    # LINT.IfChange(go_test_wrapper)
    wrapper_script = "//tools/go_test_parser:go_test_parser"
    test_args = [
        "./" + binary_name,
        "-test.timeout",
        timeout,
        "-test.v",  # Emit detailed test case information.
    ] + test_args
    # LINT.ThenChange(//build/go/go_test.gni:go_test_wrapper)

    host_test(
        name = name,
        binary = wrapper_script,
        test_args = test_args,
        data = test_data,
        target_compatible_with = target_compatible_with,
        visibility = visibility,
    )

def _host_go_test_impl(
        name,
        visibility,
        binary_name = "",
        test_args = [],
        test_data = [],
        tags = [],
        timeout = "5m",
        target_compatible_with = None,
        **kwargs):
    legacy_host_go_test(
        name = name,
        binary_name = binary_name,
        test_args = test_args,
        test_data = test_data,
        tags = tags,
        timeout = timeout,
        target_compatible_with = target_compatible_with,
        visibility = visibility,
        **kwargs
    )

# TODO(https://fxbug.dev/349341932): Switch to symbolic macro once the inherit_attrs error is fixed.
#
# This is a broken attempt to create a symbolic macro that would inherit all attributes.
#
# Unfortunately, it requires an inherit_attrs line that fails with go_test.
# Hopefully this could be fixed with a future Bazel upgrade.
broken_symbolic_host_go_test = macro(
    implementation = _host_go_test_impl,
    doc = """
Define a host test wrapping a Go binary that can be used with Fuchsia test runners.

This is a convenience macro to call go_test() and host_test() together.

Unlike go_test(), these tests will be usable with `fx test` and `botanist`, and can
still be run locally using `fx bazel test --config=host <label>`.

The "manual" tag will be set on the go_test() target. In practice something like
`fx bazel test --config=host //build/bazel/rules/host_tests/tests/go/...`
will correctly only run one test target, instead of two for each host_go_test()
definition.

Accepts all go_test() attributes, plus `binary_name`, `timeout` and `test_xxx` ones.
""",
    inherit_attrs = go_test,
    attrs = {
        "binary_name": attr.string(default = "", doc = "The name of the go_test target, defaults to 'name + \"_bin\"'."),
        "test_args": attr.string_list(default = [], doc = "Arguments to pass to the test binary. Do not use `args`."),
        "test_data": attr.label_list(default = [], doc = "Data dependencies for the test target itself."),
        "timeout": attr.string(default = "5m", doc = """
             Override default timeout. Values must be valid Go durations.
             such as "300ms", "1.5h" or "2h45m". See
             https://golang.org/cmd/go/#hdr-Testing_flags for details on timeout. See
             https://golang.org/pkg/time/#ParseDuration for duration format."""),
    },
)

# Switch to symbolic macro once the inherit_attrs error is fixed.
host_go_test = legacy_host_go_test
