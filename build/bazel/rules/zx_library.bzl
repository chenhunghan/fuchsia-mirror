# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines a Zircon-specific libraries.

These rules provide functionality similar to the GN template `zx_library()`.

Because the Zircon/kernel toolchain is not yet supported in Bazel, the rules
are currently just thin wrappers around the built-in C++ rules.
"""

load("//build/bazel/rules/cc:fx_cc_library.bzl", "fx_cc_library")

visibility([
    "//build/bazel/rules/cc/tests",
    "//build/bazel/rules/idk/private/...",  # Uses helper functions defined here.
    "//src/devices/...",
    "//src/firmware/lib/...",
    "//src/lib/...",
    "//src/media/audio/...",
    "//zircon/...",
])

def _get_main_target_for_headers(target_label):
    if target_label.name == "headers":
        return Label("//" + target_label.package)
    if target_label.name.endswith(".headers"):
        return Label("//" + target_label.package + ":" + target_label.name[:-len(".headers")])
    return target_label

def _get_main_targets_for_headers(target_labels, exclude_labels = []):
    """Converts any header targets to their library targets.

For example, a target "//zircon/system/ulib/foo:headers" will be converted to
"//zircon/system/ulib/foo", and a target "//zircon/system/ulib/bar:bar.headers"
will be converted to "//zircon/system/ulib/bar:bar". Other targets will be
unmodified.

    Args:
        target_labels: A list of target labels, some of which may be header targets.
        exclude_labels: A list of targets to exclude from the result if they
            were converted from a header target. This can be used to avoid
            adding a conflicting duplicate target while still allowing other
            conflicts to be caught.

    Returns:
        A new list of target labels with header targets replaced by their
        library targets.
    """

    # The deps lists can sometimes be None rather than empty.
    if target_labels == None:
        return None
    if exclude_labels == None:
        exclude_labels = []

    # The labels for the main targets of labels in `target_labels`.
    main_target_labels = []

    for target_label in target_labels:
        main_target_label = _get_main_target_for_headers(target_label)
        if main_target_label != target_label and main_target_label in exclude_labels:
            # It was a header target and the new label is in the exclude list. Skip it.
            continue
        main_target_labels.append(main_target_label)

    return main_target_labels

def _get_main_target_for_as_needed(target_label):
    if target_label.name.endswith(".as-needed"):
        return Label("//" + target_label.package + ":" + target_label.name[:-len(".as-needed")])
    return target_label

def _get_main_targets_for_as_needed(target_labels):
    """Converts any as-needed targets to their library targets.

For example, a target "//zircon/system/ulib/bar:bar.as-needed"
will be converted to "//zircon/system/ulib/bar:bar". Other targets will be
unmodified.

    Args:
        target_labels: A list of target labels, some of which may be as-needed targets.

    Returns:
        A new list of target labels with as-needed targets replaced by their
        library targets.
    """

    # The deps lists can sometimes be None rather than empty.
    if target_labels == None:
        return None

    # The labels for the main targets of labels in `target_labels`.
    main_target_labels = []

    for target_label in target_labels:
        main_target_label = _get_main_target_for_as_needed(target_label)
        main_target_labels.append(main_target_label)

    return main_target_labels

# LINT.IfChange
def apply_common_zx_library_modifications(kwargs):
    """
    Apply common modifications for zx_libraries.

    Modifies the dependencies and defines as appropriate. The modifications are
    based on the implementation of the GN template `zx_library()`.

    When not using a Zircon-specific toolchain, the following modifications are made:
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
    * When not Fuchsia, add a public dependency on "//zircon/system/public".
    * Define "_ALL_SOURCE".

    Args:
        kwargs: The keyword arguments to modify.

    Returns:
        The modified keyword arguments.
    """

    # TODO(https://fxbug.dev/456186319): When adding support for building
    # Zircon, do apply these modifications when using a Zircon-specific toolchain.

    # Convert any ":<library>.as-needed" targets in the private deps to the
    # library target.
    kwargs["implementation_deps"] = _get_main_targets_for_as_needed(kwargs["implementation_deps"])
    if "fuchsia_implementation_deps" in kwargs and kwargs["fuchsia_implementation_deps"] != None:
        fail("The zx macros do not currently support `fuchsia_implementation_deps`.")

    # Convert any ":headers" or ":<library>.headers" targets in the public deps
    # to the library target.
    # Unlike other deps, "fuchsia_deps" is not supported by all of the macros.
    kwargs["deps"] = _get_main_targets_for_headers(kwargs["deps"], kwargs["implementation_deps"])
    if "fuchsia_deps" in kwargs:
        kwargs["fuchsia_deps"] = _get_main_targets_for_headers(kwargs["fuchsia_deps"], kwargs["implementation_deps"])

    # TODO(https://fxbug.dev/429377203): Add "//zircon/system/public" to
    # kwargs["deps"] when not Fuchsia.

    if kwargs["defines"] == None:
        kwargs["defines"] = []

    # The `defines` attribute is configurable, so `append()` cannot be used.
    # buildifier: disable=list-append
    kwargs["defines"] += ["_ALL_SOURCE"]

    return kwargs

# LINT.ThenChange(//build/zircon/zx_library.gni)

# `apply_common_zx_library_modifications()` rewrites the labels in these
# attributes, which requires plain lists. Inherited attributes of symbolic macros
# are configurable, which turns even plain lists into `select()`s.
_NON_CONFIGURABLE_DEPS_ATTRS = {
    "deps": attr.label_list(
        doc = "Public dependencies. See `fx_cc_library()`.",
        configurable = False,
    ),
    "implementation_deps": attr.label_list(
        doc = "Private dependencies. See `fx_cc_library()`.",
        configurable = False,
    ),
}

def _cc_shared_library_zx_impl(
        name,
        includes,
        **kwargs):
    """Implementation for the cc_shared_library_zx() macro."""

    # LINT.IfChange
    # `zx_library()` assumes headers files are under `include/`.
    if includes != ["include"]:
        fail('`includes` must be `["include"]`.')

    # LINT.ThenChange(//build/zircon/zx_library.gni)

    kwargs = apply_common_zx_library_modifications(kwargs)

    # TODO(https://fxbug.dev/421888626): Use `fx_cc_shared_library()`.
    fx_cc_library(
        name = name,
        includes = includes,
        **kwargs
    )

cc_shared_library_zx = macro(
    doc = """Defines a Zircon C++ shared library that will be a `zx_library()` in GN.

This macro is for libraries not in the IDK. For IDK libraries, use `idk_cc_shared_library_zx()`.

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
    implementation = _cc_shared_library_zx_impl,
    attrs = {
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
    } | _NON_CONFIGURABLE_DEPS_ATTRS,
)

def _cc_source_library_zx_impl(
        name,
        includes,
        **kwargs):
    """Implementation for the cc_source_library_zx() macro."""

    # LINT.IfChange
    # `zx_library()` assumes headers files are under `include/`.
    if includes != ["include"]:
        fail('`includes` must be `["include"]`.')

    # LINT.ThenChange(//build/zircon/zx_library.gni)

    kwargs = apply_common_zx_library_modifications(kwargs)

    fx_cc_library(
        name = name,
        includes = includes,
        **kwargs
    )

cc_source_library_zx = macro(
    doc = """Defines a Zircon C++ source library that will be a `zx_library()` in GN.

Bazel may create a static library as it does not have a concept of source libraries.

This macro is for libraries not in the IDK. For IDK libraries, use `idk_cc_source_library_zx()`.

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
    implementation = _cc_source_library_zx_impl,
    attrs = {
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
    } | _NON_CONFIGURABLE_DEPS_ATTRS,
)

def _cc_static_library_zx_impl(
        name,
        includes,
        **kwargs):
    """Implementation for the cc_static_library_zx() macro."""

    # LINT.IfChange
    # `zx_library()` assumes headers files are under `include/`.
    if includes != ["include"]:
        fail('`includes` must be `["include"]`.')

    # LINT.ThenChange(//build/zircon/zx_library.gni)

    kwargs = apply_common_zx_library_modifications(kwargs)

    # `cc_static_library_zx()` creates an in-tree static library, so it wraps
    # `fx_cc_library()` (which wraps `cc_library()`), NOT `cc_static_library()`,
    # which produces a self-contained prebuilt archive for external distribution
    # (such as in the IDK).
    fx_cc_library(
        name = name,
        includes = includes,
        **kwargs
    )

cc_static_library_zx = macro(
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
    implementation = _cc_static_library_zx_impl,
    attrs = {
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
    } | _NON_CONFIGURABLE_DEPS_ATTRS,
)
