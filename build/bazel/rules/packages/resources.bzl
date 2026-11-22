# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""In-tree macros for declaring Fuchsia package resources (synced by `bazel2gn`)."""

load(
    "@fuchsia_rules_common//packages:resources.bzl",
    "common_resource",
    "common_resource_group",
    _common_fuchsia_package_resource_collection = "fuchsia_package_resource_collection",
    _common_fuchsia_package_resource_group = "fuchsia_package_resource_group",
)

visibility(["//..."])

fuchsia_package_resource_collection = _common_fuchsia_package_resource_collection

def _resource_impl(
        name,
        src,
        dest,
        sources,
        srcs,
        outputs,
        deps,  # buildifier: disable=unused-variable - For GN conversion only.
        **kwargs):
    if sources != None and srcs != None:
        fail("resource() cannot specify both `sources` and `srcs`")
    actual_sources = sources if sources != None else srcs
    common_resource(
        name = name,
        src = src,
        dest = dest,
        sources = actual_sources,
        outputs = outputs,
        **kwargs
    )

resource = macro(
    doc = """Declares one or more resources to be included in a Fuchsia package.

Supports both single-file (`src` and `dest`) and GN-compatible multi-file
(`sources`/`srcs` and `outputs = [".../{{source_file_part}}"]`) forms, as well
as `deps` for `bazel2gn` forwarding.""",
    implementation = _resource_impl,
    inherit_attrs = common_resource,
    attrs = {
        "srcs": attr.label_list(
            doc = "Alias for `sources`.",
            allow_files = True,
        ),
        "deps": attr.label_list(
            doc = "Optional list of targets that produce `sources` (used by `bazel2gn`).",
            configurable = False,
        ),
    },
)

fuchsia_package_resource = resource

def _fuchsia_package_resource_group_impl(
        name,
        deps,  # buildifier: disable=unused-variable - For GN conversion only.
        **kwargs):
    _common_fuchsia_package_resource_group(
        name = name,
        **kwargs
    )

fuchsia_package_resource_group = macro(
    doc = """Declares a group of resources to be included in a Fuchsia package.""",
    implementation = _fuchsia_package_resource_group_impl,
    inherit_attrs = _common_fuchsia_package_resource_group,
    attrs = {
        "deps": attr.label_list(
            doc = "Optional list of targets that produce `srcs` (used by `bazel2gn`).",
            configurable = False,
        ),
    },
)

def resource_group(
        *,
        name,
        files = None,
        srcs = None,
        dest = None,
        strip_prefix = "",
        basename_only = False,
        allow_binary_output_dir = False,
        deps = None,  # buildifier: disable=unused-variable - For GN conversion only.
        data_deps = None,
        **kwargs):
    """Declares a group of resources to be included in a Fuchsia package.

    Supports both the GN-compatible `files = [{"source": "...", "dest": "..."}]`
    form and the `srcs` + `dest` form of `fuchsia_package_resource_group()`.

    Args:
        name: Target name.
        files: Optional list of dicts with `source` and `dest` keys.
        srcs: Source files when using the SDK-style form.
        dest: Destination directory when using `srcs`.
        strip_prefix: Path prefix stripped from `srcs`.
        basename_only: Whether to strip directories from `srcs` and use basename.
        allow_binary_output_dir: Whether installing under `bin/` is allowed.
        deps: Optional GN dependency list forwarded by `bazel2gn`.
        data_deps: Additional runtime resource dependencies.
        **kwargs: Additional common attributes forwarded to the underlying rule.
    """
    if files != None:
        if srcs != None or dest != None:
            fail("resource_group() cannot specify both `files` and `srcs`/`dest`")
        group_srcs = []
        group_dests = []
        for entry in files:
            if type(entry) != "dict" or "source" not in entry or "dest" not in entry:
                fail("each element of resource_group() `files` must be a dict with `source` and `dest` keys")
            group_srcs.append(entry["source"])
            group_dests.append(entry["dest"])
        common_resource_group(
            name = name,
            srcs = group_srcs,
            dests = group_dests,
            allow_binary_output_dir = allow_binary_output_dir,
            data_deps = data_deps,
            **kwargs
        )
    else:
        fuchsia_package_resource_group(
            name = name,
            srcs = srcs,
            dest = dest,
            strip_prefix = strip_prefix,
            basename_only = basename_only,
            allow_binary_output_dir = allow_binary_output_dir,
            data_deps = data_deps,
            **kwargs
        )

def _resource_tree_impl(
        name,
        sources,
        srcs,
        dest_dir,
        dest,
        sources_root,
        deps,  # buildifier: disable=unused-variable - For GN conversion only.
        **kwargs):
    if sources != None and srcs != None:
        fail("resource_tree() cannot specify both `sources` and `srcs`")
    if dest_dir != None and dest != None:
        fail("resource_tree() cannot specify both `dest_dir` and `dest`")
    actual_sources = sources if sources != None else srcs
    actual_dest = dest_dir if dest_dir != None else dest
    if actual_sources == None:
        fail("resource_tree() requires `sources`")
    if actual_dest == None:
        fail("resource_tree() requires `dest_dir`")
    if actual_dest.startswith("/"):
        fail("dest_dir cannot start with /: {}".format(actual_dest))
    prefix = ""
    if sources_root:
        prefix = sources_root.removesuffix("/") + "/"
    prefixed_srcs = [
        (prefix + s) if prefix and not s.startswith("/") and not s.startswith(":") else s
        for s in actual_sources
    ]
    _common_fuchsia_package_resource_group(
        name = name,
        srcs = prefixed_srcs,
        dest = actual_dest,
        strip_prefix = prefix,
        basename_only = False,
        **kwargs
    )

resource_tree = macro(
    doc = """Declares a directory tree of resources to be included in a Fuchsia package.""",
    implementation = _resource_tree_impl,
    inherit_attrs = _common_fuchsia_package_resource_group,
    attrs = {
        "sources": attr.label_list(
            doc = "The resource files to include in the package.",
            allow_files = True,
            configurable = False,
        ),
        "srcs": attr.label_list(
            doc = "Alias for `sources`.",
            allow_files = True,
            configurable = False,
        ),
        "dest_dir": attr.string(
            doc = "The destination directory inside the package.",
            configurable = False,
        ),
        "dest": attr.string(
            doc = "Alias for `dest_dir`.",
            configurable = False,
        ),
        "sources_root": attr.string(
            doc = "Prefix stripped from `sources` paths when installing into `dest_dir`.",
            default = "",
            configurable = False,
        ),
        "deps": attr.label_list(
            doc = "Optional list of targets that produce `sources` (used by `bazel2gn`).",
            configurable = False,
        ),
        "basename_only": None,
        "strip_prefix": None,
    },
)
