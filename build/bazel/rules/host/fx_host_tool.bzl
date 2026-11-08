# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule and macro for packaging `fx` host tools as relocatable Bazel build actions."""

load("@fuchsia_rules_common//:local_actions.bzl", "LOCAL_ONLY_ACTION_KWARGS")
load("@rules_python//python:defs.bzl", "py_binary")
load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("//build/bazel/rules:cipd_prebuilt.bzl", "CipdPrebuiltInfo")
load(
    "//build/bazel/rules/python:py_toolchain.bzl",
    "PY_TOOLCHAIN_ATTRS",
    "generate_python_build_action",
)

visibility(["//..."])

FxHostToolInfo = provider(
    doc = "Metadata for an `fx_host_tool` target.",
    fields = {
        "tool_name": "Command/output name installed into host-tools.",
        "launcher": "Launcher executable File.",
        "runtime_dir": "Relocatable runtime directory TreeArtifact.",
        "has_compiled_deps": "Boolean indicating whether this tool or its runtime_tools include compiled targets.",
    },
)

_FxHostToolSourcesInfo = provider(
    doc = "Transitive source files and package paths for an `fx_host_tool` target.",
    fields = {
        "source_files": "depset of source File objects in the main workspace.",
        "packages": "depset of main-workspace package path strings.",
    },
)

# Attributes to skip when walking the dependency graph (see //build/bazel/aspects:utils.bzl).
_IGNORED_ATTR_NAMES = ("disable_build_flags",)

def _is_main_repo_source_file(f):
    if not f.is_source:
        return False
    if f.owner and f.owner.workspace_name != "":
        return False
    short = f.short_path
    if short.startswith("../") or short.startswith("prebuilt/"):
        return False
    if short == "third_party" or short.startswith("third_party/"):
        return False
    return True

def _is_third_party_or_prebuilt(pkg):
    return (
        pkg == "third_party" or
        pkg.startswith("third_party/") or
        pkg == "prebuilt" or
        pkg.startswith("prebuilt/")
    )

def _fx_host_tool_sources_aspect_impl(target, ctx):
    # Short-circuit external, prebuilt, and third_party subgraphs.
    if target.label.workspace_name != "" or _is_third_party_or_prebuilt(target.label.package):
        return [
            _FxHostToolSourcesInfo(
                source_files = depset(),
                packages = depset(),
            ),
        ]

    direct_files = []
    direct_pkgs = [target.label.package]

    # Collect source files from default outputs if any are direct sources.
    if DefaultInfo in target:
        for f in target[DefaultInfo].files.to_list():
            if _is_main_repo_source_file(f):
                direct_files.append(f)

    # Collect direct files declared on the rule's public attributes.
    if hasattr(ctx, "rule") and hasattr(ctx.rule, "files"):
        for attr_name in dir(ctx.rule.files):
            if attr_name.startswith("_") or attr_name in _IGNORED_ATTR_NAMES:
                continue
            for f in getattr(ctx.rule.files, attr_name):
                if _is_main_repo_source_file(f):
                    direct_files.append(f)

    transitive_files = []
    transitive_pkgs = []

    if hasattr(ctx, "rule") and hasattr(ctx.rule, "attr"):
        for attr_name in dir(ctx.rule.attr):
            if attr_name.startswith("_") or attr_name in _IGNORED_ATTR_NAMES:
                continue
            val = getattr(ctx.rule.attr, attr_name)
            if type(val) == "list":
                deps = val
            elif type(val) == "Target":
                deps = [val]
            elif type(val) == "dict":
                # In Bazel, only keys in label_keyed_string_dict can be Targets.
                deps = val.keys()
            else:
                deps = []
            for dep in deps:
                if type(dep) == "Target" and _FxHostToolSourcesInfo in dep:
                    info = dep[_FxHostToolSourcesInfo]
                    transitive_files.append(info.source_files)
                    transitive_pkgs.append(info.packages)

    return [
        _FxHostToolSourcesInfo(
            source_files = depset(direct = direct_files, transitive = transitive_files),
            packages = depset(direct = direct_pkgs, transitive = transitive_pkgs),
        ),
    ]

_fx_host_tool_sources_aspect = aspect(
    implementation = _fx_host_tool_sources_aspect_impl,
    attr_aspects = ["*"],
    doc = "Walks transitive dependencies to collect all main-repo source files and package paths.",
)

def _fx_host_tool_bundle_impl(ctx):
    tool_name = ctx.attr.tool_name or ctx.attr.name
    out_prefix = (ctx.attr.name + "/") if ctx.attr.name != tool_name else ""
    launcher = ctx.actions.declare_file(out_prefix + tool_name)
    runtime_dir = ctx.actions.declare_directory(out_prefix + tool_name + ".runtime_dir")
    outputs = [launcher, runtime_dir]

    if FxHostToolInfo in ctx.attr.binary:
        fail("`binary` cannot be another `fx_host_tool`; list it in `runtime_tools` instead.")

    binary_info = ctx.attr.binary[DefaultInfo]
    entry_point = binary_info.files_to_run.executable
    if not entry_point:
        direct_files = binary_info.files.to_list()
        if len(direct_files) != 1:
            fail("`binary` must be an executable target or a single file, got %d files" % len(direct_files))
        entry_point = direct_files[0]

    has_compiled_deps = False
    if not ctx.attr.is_script_entry and CipdPrebuiltInfo not in ctx.attr.binary:
        if binary_info.files_to_run.executable != None or not entry_point.is_source:
            has_compiled_deps = True

    binary_runfiles_manifest = binary_info.files_to_run.runfiles_manifest
    binary_repo_mapping_manifest = binary_info.files_to_run.repo_mapping_manifest

    merged_runfiles = ctx.runfiles(
        files = ctx.files.data,
    ).merge_all(
        [binary_info.default_runfiles] +
        [t[DefaultInfo].default_runfiles for t in ctx.attr.data] +
        [
            t[DefaultInfo].default_runfiles
            for t in ctx.attr.runtime_tools
            if FxHostToolInfo not in t and not (CipdPrebuiltInfo in t and t[CipdPrebuiltInfo].source_rel_path)
        ],
    )

    runtime_tool_files = []
    runtime_tool_args = []
    seen_cipd_dirs = {}
    for target in ctx.attr.runtime_tools:
        if FxHostToolInfo in target:
            fx_info = target[FxHostToolInfo]
            runtime_tool_files.extend([fx_info.launcher, fx_info.runtime_dir])
            runtime_tool_args.append(
                "--runtime-fx-tool=%s=%s:%s" % (
                    fx_info.tool_name,
                    fx_info.launcher.path,
                    fx_info.runtime_dir.path,
                ),
            )
            if fx_info.has_compiled_deps:
                has_compiled_deps = True
        else:
            t_info = target[DefaultInfo]
            t_exec = t_info.files_to_run.executable
            t_files = t_info.files.to_list()
            if not t_exec:
                if len(t_files) != 1:
                    fail("Each target in `runtime_tools` must be an executable or single file, got: %s" % target.label)
                t_exec = t_files[0]
            runtime_tool_files.append(t_exec)
            if CipdPrebuiltInfo in target and t_exec.is_directory and target[CipdPrebuiltInfo].source_rel_path:
                source_rel_path = target[CipdPrebuiltInfo].source_rel_path
                if source_rel_path not in seen_cipd_dirs:
                    seen_cipd_dirs[source_rel_path] = True
                    runtime_tool_args.append(
                        "--runtime-cipd-dir=%s=%s:%s" % (
                            source_rel_path,
                            t_exec.path,
                            source_rel_path,
                        ),
                    )
            else:
                runtime_tool_args.append("--runtime-tool=%s=%s" % (t_exec.basename, t_exec.path))
            if CipdPrebuiltInfo not in target:
                is_raw_source = (
                    t_info.files_to_run.executable == None and
                    len(t_files) == 1 and
                    t_files[0].is_source
                )
                if not is_raw_source:
                    has_compiled_deps = True

    # Compiled targets (such as cc_binary and rustc_binary) do not expose their
    # compile-time source inputs (.cc, .h, .rs, .fidl) or transitive BUILD.bazel
    # packages in DefaultInfo.default_runfiles. Collect them via
    # _fx_host_tool_sources_aspect so generate_fx_host_tool.py can emit complete
    # sources.manifest and packages.manifest files for warm-run staleness checks.
    aspect_source_depsets = []
    aspect_pkg_depsets = []
    all_aspect_targets = [ctx.attr.binary] + ctx.attr.data + ctx.attr.runtime_tools
    for t in all_aspect_targets:
        if _FxHostToolSourcesInfo in t:
            aspect_source_depsets.append(t[_FxHostToolSourcesInfo].source_files)
            aspect_pkg_depsets.append(t[_FxHostToolSourcesInfo].packages)

    direct_inputs = list(runtime_tool_files) + list(ctx.files._python_modules) + [entry_point]
    if binary_runfiles_manifest:
        direct_inputs.append(binary_runfiles_manifest)
    if binary_repo_mapping_manifest:
        direct_inputs.append(binary_repo_mapping_manifest)
    inputs = depset(
        direct = direct_inputs,
        transitive = [binary_info.files, merged_runfiles.files] + aspect_source_depsets,
    ).to_list()

    args = [
        "--entry-point=%s" % entry_point.path,
        "--output-launcher=%s" % launcher.path,
        "--output-runtime-dir=%s" % runtime_dir.path,
        "--bazel-execroot=.",
    ]
    if has_compiled_deps:
        args.append("--always-build")
    if binary_runfiles_manifest:
        args.append("--entry-runfiles-manifest=%s" % binary_runfiles_manifest.path)

    all_packages = depset(
        direct = [ctx.label.package] if ctx.label.workspace_name == "" else [],
        transitive = aspect_pkg_depsets,
    ).to_list()
    for pkg in sorted(all_packages):
        if not _is_third_party_or_prebuilt(pkg):
            args.append("--source-package=%s" % pkg)

    extra_runfile_depsets = (
        [depset(ctx.files.data)] +
        [t[DefaultInfo].default_runfiles.files for t in ctx.attr.data] +
        [
            t[DefaultInfo].default_runfiles.files
            for t in ctx.attr.runtime_tools
            if FxHostToolInfo not in t and not (CipdPrebuiltInfo in t and t[CipdPrebuiltInfo].source_rel_path)
        ]
    )
    extra_runfile_paths = {}
    for f in depset(transitive = extra_runfile_depsets).to_list():
        extra_runfile_paths[f.short_path] = f.path
    for short_path, exec_path in extra_runfile_paths.items():
        args.append("--data-runfile=%s=%s" % (short_path, exec_path))
    args.extend(runtime_tool_args)

    python_module_files = {f: True for f in ctx.files._python_modules}
    for f in inputs:
        if f not in python_module_files and _is_main_repo_source_file(f):
            args.append("--source-file=%s=%s" % (f.short_path, f.path))

    generate_python_build_action(
        ctx = ctx,
        py_script = ctx.file._generator_script,
        arguments = args,
        inputs = inputs,
        outputs = outputs,
        **LOCAL_ONLY_ACTION_KWARGS
    )

    out_runfiles = ctx.runfiles(
        files = [launcher, runtime_dir] + runtime_tool_files,
    ).merge(merged_runfiles)

    return [
        DefaultInfo(
            files = depset(outputs),
            runfiles = out_runfiles,
            executable = launcher,
        ),
        FxHostToolInfo(
            tool_name = tool_name,
            launcher = launcher,
            runtime_dir = runtime_dir,
            has_compiled_deps = has_compiled_deps,
        ),
    ]

fx_host_tool_bundle = rule(
    implementation = _fx_host_tool_bundle_impl,
    executable = True,
    doc = "Packages a host executable, script, or prebuilt with its runfiles and runtime tools for installation into `host-tools`.",
    attrs = {
        "binary": attr.label(
            mandatory = True,
            allow_files = True,
            aspects = [_fx_host_tool_sources_aspect],
            doc = "Executable target or single-file script/binary to wrap.",
        ),
        "tool_name": attr.string(
            default = "",
            doc = "Optional output binary name; defaults to `name`.",
        ),
        "is_script_entry": attr.bool(
            default = False,
            doc = "True when `binary` was synthesized from `fx_script_host_tool`.",
        ),
        "data": attr.label_list(
            default = [],
            allow_files = True,
            aspects = [_fx_host_tool_sources_aspect],
            doc = "Additional runtime data files exposed in runfiles.",
        ),
        "runtime_tools": attr.label_list(
            default = [],
            allow_files = True,
            aspects = [_fx_host_tool_sources_aspect],
            doc = "Executable, `cipd_prebuilt_file`, or `cipd_prebuilt_directory` targets added to `PATH` at launch time.",
        ),
        "_generator_script": attr.label(
            default = "//build/bazel/scripts:generate_fx_host_tool.py",
            allow_single_file = True,
        ),
        "_python_modules": attr.label_list(
            allow_files = True,
            default = [
                "//build/bazel/scripts:runfiles_utils.py",
            ],
        ),
    } | PY_TOOLCHAIN_ATTRS,
)

def fx_host_tool(
        *,
        name,
        binary,
        data = [],
        runtime_tools = [],
        tool_name = "",
        visibility = None,
        testonly = False):
    """Packages a compiled binary or CIPD prebuilt into a self-contained fx host tool bundle.

    Produces:
      - `<tool_name>` (executable launcher script)
      - `<tool_name>.runtime_dir/` (TreeArtifact with staged runtime, runfiles,
        and `sources.manifest` for fast staleness checks)

    For Python (`.py`) or shell (`.sh`) scripts that require Bazel `deps`,
    `data`, or `runtime_tools`, use `fx_script_host_tool` instead.

    Args:
      name: Target name.
      binary: Label of an executable target (`cc_binary`, `rust_binary`,
        `go_binary`, `py_binary`, `sh_binary`, or `cipd_prebuilt_file`).
      data: Additional runtime data files/targets.
      runtime_tools: Other host tool executable targets required at runtime
        (staged into `<tool_name>.runtime_dir/.bin/` and prepended to `PATH`).
      tool_name: Override output launcher name (defaults to `name`).
      visibility: Standard Bazel visibility.
      testonly: Standard Bazel testonly flag.
    """
    fx_host_tool_bundle(
        name = name,
        binary = binary,
        tool_name = tool_name or name,
        is_script_entry = False,
        data = data,
        runtime_tools = runtime_tools,
        target_compatible_with = HOST_OS_CONSTRAINTS,
        visibility = visibility,
        testonly = testonly,
    )

def fx_script_host_tool(
        *,
        name,
        script,
        srcs = [],
        deps = [],
        data = [],
        runtime_tools = [],
        tool_name = "",
        visibility = None,
        testonly = False):
    """Packages a `.py` or `.sh` script with Bazel dependencies into an fx host tool bundle.

    Note on `fx_script_host_tool` vs `"script"` in `fx_host_tools.json`:
      - Use `"script"` in `fx_host_tools.json` for standalone scripts in the
        checkout that can be executed directly without Bazel build steps,
        library `deps`, `data` runfiles, or staged `runtime_tools`.
      - Use `fx_script_host_tool` when a Python or shell script needs Bazel
        `deps` (e.g., `py_library`), `data` runfiles, or `runtime_tools`
        (such as CIPD prebuilts or other `fx_host_tool` targets) staged into a
        hermetic `<tool_name>.runtime_dir/` bundle.

    Args:
      name: Target name.
      script: Direct `.py` or `.sh` source file for the entry script.
      srcs: Additional source files when `script` is a `.py` or `.sh` file.
      deps: Library dependencies when `script` is a `.py` or `.sh` file.
      data: Additional runtime data files/targets.
      runtime_tools: Other host tool executable targets required at runtime
        (staged into `<tool_name>.runtime_dir/.bin/` and prepended to `PATH`).
      tool_name: Override output launcher name (defaults to `name`).
      visibility: Standard Bazel visibility.
      testonly: Standard Bazel testonly flag.
    """
    if not (script.endswith(".py") or script.endswith(".sh")):
        fail("fx_script_host_tool `script` must be a .py or .sh file (got %r); use fx_host_tool with `binary` for compiled targets." % script)

    wrapped_binary = script
    extra_data = list(data) if type(data) == "list" else data

    if script.endswith(".py"):
        bin_target_name = name + "_fx_py_bin"
        py_binary(
            name = bin_target_name,
            main = script,
            srcs = [script] + [s for s in srcs if s != script],
            imports = ["."],
            deps = deps,
            data = data,
            target_compatible_with = HOST_OS_CONSTRAINTS,
            testonly = testonly,
            visibility = ["//visibility:private"],
        )
        wrapped_binary = ":" + bin_target_name
    elif script.endswith(".sh"):
        sh_extra = []
        for item in srcs + deps:
            if item != script and item not in sh_extra:
                if type(extra_data) != "list" or item not in extra_data:
                    sh_extra.append(item)
        if sh_extra:
            extra_data = extra_data + sh_extra

    fx_host_tool_bundle(
        name = name,
        binary = wrapped_binary,
        tool_name = tool_name or name,
        is_script_entry = True,
        data = extra_data,
        runtime_tools = runtime_tools,
        target_compatible_with = HOST_OS_CONSTRAINTS,
        visibility = visibility,
        testonly = testonly,
    )
