# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines a Zircon C++ static library.

This macro provides functionality similar to the GN template `zx_library()`.

Because the Zircon/kernel toolchain is not yet supported in Bazel, this macro
is currently just a thin wrapper around the Fuchsia C++ rules.
"""

load("//build/bazel/rules/cc:fx_cc_library.bzl", "fx_cc_library")
load(
    "//build/bazel/rules/cc:zx_library_utils.bzl",
    "NON_CONFIGURABLE_DEPS_ATTRS",
    "apply_common_zx_library_modifications",
)

visibility(["//..."])

def _fx_cc_static_library_zx_impl(
        name,
        includes,
        **kwargs):
    """Implementation for the fx_cc_static_library_zx() macro."""

    # LINT.IfChange
    # `zx_library()` assumes headers files are under `include/`.
    if includes != ["include"]:
        fail('`includes` must be `["include"]`.')

    # LINT.ThenChange(//build/zircon/zx_library.gni)

    kwargs = apply_common_zx_library_modifications(kwargs)

    # `fx_cc_static_library_zx()` creates an in-tree static library, so it
    # wraps `fx_cc_library()` (which wraps `cc_library()`), NOT
    # `cc_static_library()`, which produces a self-contained prebuilt archive
    # for external distribution (such as in the IDK).
    fx_cc_library(
        name = name,
        includes = includes,
        **kwargs
    )

fx_cc_static_library_zx = macro(
    # TODO(https://fxbug.dev/429377203): Move the `deps` and
    # `implementation_deps` substitution details from `doc` to
    # `NON_CONFIGURABLE_DEPS_ATTRS` and state here that they are
    # non-configurable because they are rewritten as described there.
    doc = """Defines a Zircon C++ static library that will be a `zx_library()` in GN.

This wraps `fx_cc_library()`, not `cc_static_library()`, because it defines an
in-tree library rather than a self-contained prebuilt archive.

This macro is for libraries not in the IDK. For IDK libraries, use `idk_cc_static_library_zx()`.

When not using a Zircon-specific toolchain:
 * Any ":headers" or ":<library>.headers" targets that appear in public
   dependencies will be rewritten into a dependency on the library itself.
   For example:
        deps = [ "//zircon/system/ulib/foo:headers", "//zircon/system/ulib/bar:bar.headers" ]
    will be replaced by:
        deps = [ "//zircon/system/ulib/foo", "//zircon/system/ulib/bar" ]

 * Any ":<library>.as-needed" targets that appear in private dependencies
   will be rewritten into a dependency on the library itself.
   For example:
        implementation_deps = [ "//zircon/system/ulib/bar:bar.as-needed" ]
    will be replaced by:
        implementation_deps = [ "//zircon/system/ulib/bar" ]
""",
    inherit_attrs = fx_cc_library,
    implementation = _fx_cc_static_library_zx_impl,
    attrs = {
        # TODO(https://fxbug.dev/429377203): Teach bazel2gn to handle
        # `includes` automatically and remove this attribute.
        "includes": attr.string_list(
            doc = 'Path to the root directory for includes. Must always be `["include"]`.',
            mandatory = True,
            configurable = False,
        ),
        # Bazel's `inherit_attrs` skips undocumented built-in `BaseRuleClasses`
        # attributes such as `aspect_hints`, so it must be declared explicitly
        # on symbolic macros that accept `aspect_hints` from callers.
        "aspect_hints": attr.label_list(
            doc = "Hints for aspects.",
        ),
    } | NON_CONFIGURABLE_DEPS_ATTRS,
)
