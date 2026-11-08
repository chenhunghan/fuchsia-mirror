# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Macro for defining a C/C++ test binary for device and/or host tests."""

load("//build/bazel/rules/cc:fx_cc_binary.bzl", "fx_cc_binary")

visibility(["//..."])

def _fx_cc_test_binary_impl(
        name,
        visibility,
        testonly,
        disable_lto,
        build_flags,
        **kwargs):
    """Implementation for the fx_cc_test_binary() macro."""

    if testonly != None and not testonly:
        fail("`fx_cc_test_binary()` targets are always testonly.")

    # Link test executables with `--no-fat-lto-objects` by default, matching
    # GN's `cc_test_executable()`. This is appended
    # after the caller's `build_flags`, like GN's `configs += [...]`, so that it
    # takes precedence in `ld.lld`. Targets can opt out with
    # `disable_lto = False`.
    #
    # TODO(https://fxbug.dev/571444250): This does not opt out of LTO, it only
    # links the native code of fat LTO objects. Use a proper LTO opt-out once
    # it is available.
    if disable_lto:
        build_flags = (build_flags or []) + [
            Label("//build/config/lto:no-fat-lto-objects-linking"),
        ]

    # A plain binary, not a Bazel test rule; see the macro doc.
    fx_cc_binary(
        name = name,
        visibility = visibility,
        testonly = True,
        build_flags = build_flags,
        **kwargs
    )

fx_cc_test_binary = macro(
    doc = """Defines a C/C++ test binary, for device and/or host tests.

    This is not a Bazel test rule: it builds a plain `cc_binary` (through
    `fx_cc_binary()`), which only becomes a test once it is wrapped by a test
    runner rule:

    - For device tests, package it with `fx_unittest_package()`,
      `fx_unittest_component()`, or `fx_packaged_binary()` +
      `fx_test_component()`.
    - For host tests, wrap it with `host_test(binary = ...)` and
      `target_compatible_with = HOST_OS_CONSTRAINTS`, or use `host_cc_test()`
      for a host-only test.

    The same target can be used for both, like a GN `test()` that is built in
    both the Fuchsia and host toolchains.

    Targets defined with `fx_cc_test_binary()` are always `testonly = True`, and
    are linked with `//build/config/lto:no-fat-lto-objects-linking` by default
    (`disable_lto = True`).

    This is the Bazel counterpart of GN's `cc_test_executable()` template (from
    `//build/testing/cc_test_executable.gni`) and syncs to it via `bazel2gn`.
    """,
    implementation = _fx_cc_test_binary_impl,
    inherit_attrs = fx_cc_binary,
    attrs = {
        "disable_lto": attr.bool(
            doc = """Links with `//build/config/lto:no-fat-lto-objects-linking`.

            Mirrors `disable_lto` on GN's `cc_test_executable()`, which adds the
            same config. Set to `False` when the test specifically exercises
            LTO linking behavior.
            """,
            default = True,
            configurable = False,
        ),
        # GN `cc_test_executable()` always produces an executable, never a
        # shared library.
        "linkshared": None,
    },
)
