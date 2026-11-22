# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines `py_zipapp_binary()`, a Python binary packaged as a `.pyz` zipapp."""

load("@rules_python//python:py_info.bzl", "PyInfo")
load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load(
    ":py_data_package.bzl",
    "CollectedPyDataPackageInfo",
    "collect_py_data_package_aspect",
)
load(":py_toolchain.bzl", "PY_TOOLCHAIN_ATTRS", "generate_python_build_action")

visibility(["//..."])

# The runfiles directory name of the main repository. With Bzlmod, this is
# always "_main", which is also what `host_py_test()` assumes.
_MAIN_REPO_RUNFILES_DIR = "_main"

def _runfiles_path(file):
    """Returns the path of `file` relative to the runfiles root.

    This is the form used by `PyInfo.imports`, e.g. "_main/build/foo/bar.py"
    for a file in the main repository, or "<canonical_repo>/foo/bar.py" for a
    file in an external repository (whose `short_path` is "../<repo>/...").
    """
    if file.short_path.startswith("../"):
        return file.short_path[len("../"):]
    return _MAIN_REPO_RUNFILES_DIR + "/" + file.short_path

def _py_zipapp_binary_impl(ctx):
    output_name = ctx.attr.output_name or ctx.label.name + ".pyz"
    if not output_name.endswith(".pyz"):
        fail("output_name must have .pyz as extension, got: " + output_name)
    if ctx.files.data_sources and not ctx.attr.data_package_name:
        fail("data_package_name is required if data_sources are provided")

    main = ctx.file.main
    if not main.basename.endswith(".py"):
        fail("main must be a .py file, got: " + main.basename)

    binary_sources = [main] + [f for f in ctx.files.srcs if f != main]

    # Like `py_binary()`, the binary's own sources are also importable through
    # the import roots that cover them (e.g. the repository root), in addition
    # to being placed at the archive root. Data files staged by transitive
    # `py_data_package()` dependencies are placed under their import roots too.
    data_package_files = [
        dep[CollectedPyDataPackageInfo].files
        for dep in ctx.attr.deps
        if CollectedPyDataPackageInfo in dep
    ]
    all_sources = depset(
        direct = binary_sources,
        transitive = [dep[PyInfo].transitive_sources for dep in ctx.attr.deps] + data_package_files,
    )
    imports = depset(transitive = [dep[PyInfo].imports for dep in ctx.attr.deps])

    pyz = ctx.actions.declare_file(output_name)

    args = ctx.actions.args()
    args.use_param_file("@%s", use_always = True)
    args.set_param_file_format("multiline")
    args.add("--output", pyz)
    args.add("--main_module", main.basename[:-len(".py")])
    args.add("--main_callable", ctx.attr.main_callable)
    if ctx.attr.unbuffered_output:
        args.add("--unbuffered")
    args.add_all(binary_sources, before_each = "--binary_source")

    # Pass each source's runfiles path and its path as separate, parallel lists
    # (expanding the same depset gives the same order), since both can contain
    # any character other than a newline.
    args.add_all(all_sources, map_each = _runfiles_path, before_each = "--source_runfiles_path")
    args.add_all(all_sources, before_each = "--source")
    args.add_all(imports, before_each = "--import")
    if ctx.attr.data_package_name:
        args.add("--data_package_name", ctx.attr.data_package_name)
    args.add_all(ctx.files.data_sources, before_each = "--data_source")

    generate_python_build_action(
        ctx,
        py_script = ctx.file._builder,
        inputs = depset(ctx.files.data_sources, transitive = [all_sources]),
        outputs = [pyz],
        arguments = [args],
        mnemonic = "PyZipapp",
        progress_message = "Packaging Python zipapp %{output}",
    )

    return [DefaultInfo(
        files = depset([pyz]),
        executable = pyz,
    )]

_py_zipapp_binary = rule(
    implementation = _py_zipapp_binary_impl,
    doc = "Packages Python sources and their PyInfo dependencies into a `.pyz` zipapp.",
    executable = True,
    attrs = {
        "main": attr.label(
            doc = "Source file containing `main_callable`.",
            mandatory = True,
            allow_single_file = [".py"],
        ),
        "srcs": attr.label_list(
            doc = "Additional sources of the binary, placed at the archive root.",
            allow_files = [".py"],
        ),
        "deps": attr.label_list(
            doc = "Python libraries and `py_data_package()` targets the binary depends on.",
            providers = [PyInfo],
            aspects = [collect_py_data_package_aspect],
        ),
        "main_callable": attr.string(
            doc = "Name of the callable in `main` to run.",
            default = "main",
        ),
        "output_name": attr.string(
            doc = "Name of the output archive. Must end with `.pyz`.",
        ),
        "unbuffered_output": attr.bool(
            doc = "Run the interpreter with `-u` when the archive is executed directly.",
        ),
        "data_package_name": attr.string(
            doc = "Name of the Python package that holds `data_sources`.",
        ),
        "data_sources": attr.label_list(
            doc = "Data files accessible through `importlib.resources`.",
            allow_files = True,
        ),
        "_builder": attr.label(
            default = "//build/bazel/rules/python:build_py_zipapp.py",
            allow_single_file = True,
        ),
    } | PY_TOOLCHAIN_ATTRS,
)

def _py_zipapp_binary_macro_impl(name, visibility, target_compatible_with, **kwargs):
    _py_zipapp_binary(
        name = name,
        visibility = visibility,
        target_compatible_with = (
            HOST_OS_CONSTRAINTS if target_compatible_with == None else target_compatible_with
        ),
        **kwargs
    )

py_zipapp_binary = macro(
    doc = """Defines a Python binary packaged as a self-contained `.pyz` zipapp.

This is the Bazel equivalent of the GN `python_binary()` template
(//build/python/python_binary.gni). Unlike `py_binary()`, whose launcher needs
a sibling `<name>.runfiles/` tree, the output is a single file, so
`bazel_py_host_tool()` (//build/bazel/bazel_py_host_tool.gni) can copy it into
the Ninja build, for `python_action()` or to install it as a host tool.

Archive layout:

  * `main` and `srcs` at the archive root, by basename (like GN
    `python_binary()`).
  * Every source of the binary and of its transitive `deps`, under every
    import root that contains it, like `py_binary()` puts each root on
    `sys.path`: the root of the file's repository (so imports like
    `from build.foo import bar` work) and each `PyInfo.imports` entry.
  * `data_sources` in the `data_package_name` package, plus data files staged
    by transitive `py_data_package()` dependencies under their import roots.
  * A generated `__main__.py` that runs `main_callable` from `main` (with
    `asyncio.run()` if it is a coroutine function).

Only Python sources and `py_data_package()` resources are packaged: plain
`data` of dependencies and native extension modules (unless explicitly staged
and extracted with `importlib.resources.as_file()`) are not.

The archive starts with `#!/usr/bin/env fuchsia-vendored-python`, so it can be
executed directly in the Fuchsia build and developer environments.

`target_compatible_with` defaults to HOST_OS_CONSTRAINTS.
""",
    implementation = _py_zipapp_binary_macro_impl,
    # All the public attributes of the rule, including common ones such as
    # `testonly`, `tags` and `target_compatible_with`.
    inherit_attrs = _py_zipapp_binary,
)
