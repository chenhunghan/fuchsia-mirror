# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Bazel aspect running Pyrefly static type checking across Python targets."""

load("@fuchsia_build_config//:defs.bzl", "build_config")
load("@rules_python//python:py_info.bzl", "PyInfo")
load(
    "//build/bazel/rules/python:py_toolchain.bzl",
    "PY_TOOLCHAIN_ATTRS",
    "generate_python_build_action",
)

visibility(["//build/bazel/..."])

def runfiles_import_to_execroot_paths(imp, bin_dir_path):
    """Translates a PyInfo.imports entry (relative to runfiles root) to execroot paths.

    Args:
      imp: Runfiles-relative import directory string from `PyInfo.imports`.
      bin_dir_path: Execroot-relative output binary directory (`ctx.bin_dir.path`).

    Returns:
      List of execroot-relative source and generated search path strings.
    """
    if imp == "_main" or imp == ".":
        return [".", bin_dir_path]
    if imp.startswith("_main/"):
        rel = imp.removeprefix("_main/")
        return [rel, bin_dir_path + "/" + rel]
    return ["external/" + imp, bin_dir_path + "/external/" + imp]

def _dep_pyrefly_checks(ctx, attr_name):
    """Returns the `pyrefly_checks` depsets of the targets in `ctx.rule.attr.<attr_name>`."""
    value = getattr(ctx.rule.attr, attr_name, None)
    if value == None:
        return []
    deps = value if type(value) == "list" else [value]
    return [
        dep[OutputGroupInfo].pyrefly_checks
        for dep in deps
        if OutputGroupInfo in dep and hasattr(dep[OutputGroupInfo], "pyrefly_checks")
    ]

def _pyrefly_output_groups(checks):
    return [OutputGroupInfo(_validation = checks, pyrefly_checks = checks)] if checks else []

def _append_target_pyrefly_checks(checks, value):
    """Appends the `pyrefly_checks` depset of `value` to `checks` if it is a Target that has one."""
    if type(value) == "Target" and OutputGroupInfo in value and hasattr(value[OutputGroupInfo], "pyrefly_checks"):
        checks.append(value[OutputGroupInfo].pyrefly_checks)

def _all_public_attr_pyrefly_checks(ctx):
    """Returns the `pyrefly_checks` depsets of the targets in all public attributes.

    Attributes whose names start with `_` (implicit dependencies such as tools
    and toolchains) are skipped.
    """
    checks = []
    for name in dir(ctx.rule.attr):
        if name.startswith("_"):
            continue
        value = getattr(ctx.rule.attr, name, None)
        value_type = type(value)
        if value_type == "Target":
            _append_target_pyrefly_checks(checks, value)
        elif value_type == "list":
            for item in value:
                _append_target_pyrefly_checks(checks, item)
        elif value_type == "dict":
            # E.g. `attr.string_keyed_label_dict` (host_test_data_map's `files_map`)
            # or `attr.label_keyed_string_dict`.
            for key, item in value.items():
                _append_target_pyrefly_checks(checks, key)
                _append_target_pyrefly_checks(checks, item)
    return checks

def _pyrefly_aspect_impl(target, ctx):
    if build_config.host_os != "linux":
        fail("pyrefly_aspect is only supported on Linux hosts, got %s." % build_config.host_os)

    # Bazel only requests the `_validation` outputs of a command-line aspect
    # for top-level targets (https://github.com/bazelbuild/bazel/issues/19636),
    # so a check runs only when its target, or a target depending on it, is
    # built at the top level. Make the checks transitive over every public
    # attribute of targets that are not Python targets, so that Python targets
    # reached through other rules (e.g. a filegroup, or the `data` of a
    # host_test() such as the Mobly driver and test binaries of a
    # python_mobly_test()) are checked too.
    if PyInfo not in target:
        attr_checks = _all_public_attr_pyrefly_checks(ctx)
        return _pyrefly_output_groups(depset(transitive = attr_checks)) if attr_checks else []

    # Skip external repositories and vendored third_party/prebuilt packages.
    # build/bazel_sdk/ holds the sources of standalone repositories (e.g.
    # @rules_fuchsia), which the build consumes as external repositories.
    label = target.label
    if label.repo_name != "":
        return []
    if label.package.startswith(("third_party/", "prebuilt/", "build/bazel_sdk/")):
        return []

    # Make the checks transitive over `deps` and `pyi_deps` for the same reason.
    dep_checks = _dep_pyrefly_checks(ctx, "deps") + _dep_pyrefly_checks(ctx, "pyi_deps")

    tags = getattr(ctx.rule.attr, "tags", [])
    raw_srcs = getattr(ctx.rule.files, "srcs", []) + getattr(ctx.rule.files, "pyi_srcs", [])
    own_srcs = [f for f in raw_srcs if f.path.endswith((".py", ".py3", ".pyi"))]
    if "no-pyrefly" in tags or not own_srcs:
        return _pyrefly_output_groups(depset(transitive = dep_checks))

    py_info = target[PyInfo]
    transitive_srcs = py_info.transitive_sources
    transitive_pyi_files = getattr(py_info, "transitive_pyi_files", depset())
    raw_imports = py_info.imports.to_list()
    is_executable = ctx.rule.kind in ("py_binary", "py_test")

    bin_dir_path = ctx.bin_dir.path
    search_paths = []
    for imp in raw_imports:
        search_paths.extend(runfiles_import_to_execroot_paths(imp, bin_dir_path))

    # Python puts the main script's directory first in `sys.path`, but when it
    # is also a package under an `imports` directory, listing it first makes
    # Pyrefly miss the names that `pkg/__init__.py` re-exports with
    # `from .pkg import *` (e.g. //src/memory/tools/smaps). List it after them.
    if is_executable:
        main_file = getattr(ctx.rule.file, "main", None)
        if not main_file:
            default_main = label.name + ".py"
            matches = [
                f
                for f in getattr(ctx.rule.files, "srcs", [])
                if f.short_path == default_main or f.short_path.endswith("/" + default_main)
            ]
            main_file = matches[0] if len(matches) == 1 else None
        if main_file and not main_file.short_path.startswith("../"):
            main_dir = main_file.short_path.rpartition("/")[0]
        else:
            main_dir = label.package
        search_paths.append(main_dir if main_dir else ".")
        search_paths.append(bin_dir_path + ("/" + main_dir if main_dir else ""))

    # Include the Fuchsia root (execroot "." and bin_dir) last so in-tree
    # absolute imports (e.g. `from build.foo import bar`) resolve cleanly.
    search_paths.append(".")
    search_paths.append(bin_dir_path)

    stamp = ctx.actions.declare_file(label.name + ".pyrefly.ok")
    pyrefly_bin = ctx.file._pyrefly
    config_file = ctx.file._pyrefly_config
    runner_script = ctx.file._pyrefly_runner

    args = [
        "--pyrefly",
        pyrefly_bin.path,
        "--config",
        config_file.path,
        "--output",
        stamp.path,
    ]
    for sp in search_paths:
        args.extend(["--search-path", sp])
    args.extend([f.path for f in own_srcs])

    action_inputs = depset(
        direct = [pyrefly_bin, config_file] + own_srcs,
        transitive = [transitive_srcs, transitive_pyi_files],
    )

    generate_python_build_action(
        ctx,
        py_script = runner_script,
        inputs = action_inputs,
        outputs = [stamp],
        arguments = args,
        mnemonic = "Pyrefly",
        progress_message = "Pyrefly type-checking %s" % label,
    )

    return _pyrefly_output_groups(depset([stamp], transitive = dep_checks))

_PYREFLY_ASPECT_ATTRS = dict(PY_TOOLCHAIN_ATTRS) | {
    "_pyrefly": attr.label(
        default = "//build/prebuilt/third_party/pyrefly:pyrefly",
        allow_single_file = True,
        cfg = "exec",
    ),
    "_pyrefly_config": attr.label(
        default = "//:pyproject.toml",
        allow_single_file = True,
    ),
    "_pyrefly_runner": attr.label(
        default = "//build/bazel/aspects:pyrefly_runner.py",
        allow_single_file = True,
    ),
}

pyrefly_aspect = aspect(
    implementation = _pyrefly_aspect_impl,
    attr_aspects = ["*"],
    attrs = _PYREFLY_ASPECT_ATTRS,
)
