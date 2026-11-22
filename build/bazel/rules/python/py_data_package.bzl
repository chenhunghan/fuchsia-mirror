# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Defines `py_data_package()`, which stages data files as an importable Python package."""

load("@rules_python//python:py_info.bzl", "PyInfo")
load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")

visibility(["//..."])

# The runfiles directory name of the main repository under Bzlmod.
_MAIN_REPO_RUNFILES_DIR = "_main"

PyDataPackageInfo = provider(
    doc = "Data files staged into a Python package by `py_data_package()`.",
    fields = {
        "files": "A `depset[File]` of staged data artifacts (excluding generated `__init__.py` files).",
    },
)

CollectedPyDataPackageInfo = provider(
    doc = "Transitive `PyDataPackageInfo` files collected across Python `deps`.",
    fields = {
        "files": "A `depset[File]` of staged data artifacts from transitive `py_data_package()` dependencies.",
    },
)

def _collect_py_data_package_aspect_impl(target, ctx):
    direct = [target[PyDataPackageInfo].files] if PyDataPackageInfo in target else []
    transitive = [
        dep[CollectedPyDataPackageInfo].files
        for dep in getattr(ctx.rule.attr, "deps", [])
        if CollectedPyDataPackageInfo in dep
    ]
    return [CollectedPyDataPackageInfo(
        files = depset(transitive = direct + transitive),
    )]

collect_py_data_package_aspect = aspect(
    doc = "Collects `PyDataPackageInfo` files transitively across `deps`.",
    implementation = _collect_py_data_package_aspect_impl,
    attr_aspects = ["deps"],
    provides = [CollectedPyDataPackageInfo],
)

def _is_python_identifier(segment):
    if not segment or segment[0].isdigit():
        return False
    for ch in segment.elems():
        if not (ch.isalnum() or ch == "_"):
            return False
    return True

def _validate_package_dir(package_dir):
    if package_dir.startswith("/") or package_dir.endswith("/"):
        fail("`package_dir` must be a relative path without leading or trailing '/', got: {!r}".format(package_dir))
    segments = package_dir.split("/")
    for seg in segments:
        if seg in ("", ".", ".."):
            fail("`package_dir` must not contain empty, '.', or '..' segments, got: {!r}".format(package_dir))
    if not _is_python_identifier(segments[-1]):
        fail("Leaf directory of `package_dir` must be a valid Python identifier, got: {!r}".format(segments[-1]))

def _resolve_imports(ctx):
    if not ctx.attr.imports:
        return depset()
    repo_dir = ctx.label.repo_name if ctx.label.repo_name else _MAIN_REPO_RUNFILES_DIR
    pkg_parts = ctx.label.package.split("/") if ctx.label.package else []
    resolved = []
    for imp in ctx.attr.imports:
        if imp.startswith("/"):
            fail("`imports` entry must be a relative path without leading '/', got: {!r}".format(imp))
        parts = list(pkg_parts)
        for seg in imp.split("/"):
            if seg in ("", "."):
                continue
            if seg == "..":
                if not parts:
                    fail("`imports` entry {!r} escapes the repository root.".format(imp))
                parts.pop()
            else:
                parts.append(seg)
        resolved.append("/".join([repo_dir] + parts))
    return depset(resolved)

def _validate_dest_filename(dest_name):
    if not dest_name or "/" in dest_name or dest_name in (".", ".."):
        fail("Data resource filename must be a plain filename without '/', got: {!r}".format(dest_name))
    if dest_name == "__init__.py":
        fail("Data resource filename cannot be '__init__.py' (generated automatically by `py_data_package`).")

def _executable_rule_output(target):
    """Returns `files_to_run.executable` for executable rule targets, or `None` for plain files.

    Single-file `InputFile` and predeclared `OutputFile` targets also populate
    `files_to_run.executable`, so require `runfiles_manifest != None` and
    `not executable.is_source` before marking the staged file executable.
    """
    files_to_run = target[DefaultInfo].files_to_run
    if files_to_run == None:
        return None
    executable = files_to_run.executable
    if executable != None and not executable.is_source and files_to_run.runfiles_manifest != None:
        return executable
    return None

def _py_data_package_impl(ctx):
    if not ctx.attr.srcs and not ctx.attr.srcs_map:
        fail("At least one of `srcs` or `srcs_map` must be non-empty.")

    package_dir = ctx.attr.package_dir or ctx.label.name
    _validate_package_dir(package_dir)
    imports = _resolve_imports(ctx)

    dest_to_src = {}
    all_targets = []

    for src_target in ctx.attr.srcs:
        all_targets.append(src_target)
        executable = _executable_rule_output(src_target)
        if executable != None:
            entries = [(executable, True)]
        else:
            files_list = src_target[DefaultInfo].files.to_list()
            if not files_list:
                fail("Target {} in `srcs` produced no files.".format(src_target.label))
            entries = [(f, False) for f in files_list]

        for src_file, is_exec in entries:
            dest_name = src_file.basename
            _validate_dest_filename(dest_name)
            existing = dest_to_src.get(dest_name)
            if existing != None and existing[0] != src_file:
                fail("Duplicate data resource filename {!r} from {} and {}.".format(
                    dest_name,
                    existing[2],
                    src_target.label,
                ))
            dest_to_src[dest_name] = (src_file, is_exec, src_target.label)

    for dest_name, src_target in ctx.attr.srcs_map.items():
        _validate_dest_filename(dest_name)
        all_targets.append(src_target)
        executable = _executable_rule_output(src_target)
        if executable != None:
            src_file = executable
            is_exec = True
        else:
            files_list = src_target[DefaultInfo].files.to_list()
            if len(files_list) != 1:
                fail("Target {} for {!r} in `srcs_map` must produce a single file (or an executable), got {} files.".format(
                    src_target.label,
                    dest_name,
                    len(files_list),
                ))
            src_file = files_list[0]
            is_exec = False

        existing = dest_to_src.get(dest_name)
        if existing != None and existing[0] != src_file:
            fail("Duplicate data resource filename {!r} from {} and {}.".format(
                dest_name,
                existing[2],
                src_target.label,
            ))
        dest_to_src[dest_name] = (src_file, is_exec, src_target.label)

    init_py = ctx.actions.declare_file(package_dir + "/__init__.py")
    ctx.actions.write(output = init_py, content = "")

    staged_files = []
    for dest_name in sorted(dest_to_src.keys()):
        src_file, is_exec, _ = dest_to_src[dest_name]
        staged = ctx.actions.declare_file("{}/{}".format(package_dir, dest_name))

        # Copy rather than `ctx.actions.symlink`: Bazel symlinks source files
        # via absolute paths through `gen/build/bazel/workspace/<top_dir>/...`,
        # which collides with the `gen/build/bazel/workspace/<top_dir>`
        # directory symlink when LUCI CAS downloads `host_test` runtime trees
        # on Swarming bots. Keep `+x` on inputs that are already executable
        # (such as `cipd_prebuilt_file()` binaries or checked-in scripts), since
        # callers can't `chmod` read-only runfiles inside `linux-sandbox`.
        ctx.actions.run_shell(
            inputs = [src_file],
            outputs = [staged],
            command = "cp -f \"$1\" \"$2\" && " + (
                "chmod 0755 \"$2\"" if is_exec else "if [ -x \"$1\" ]; then chmod 0755 \"$2\"; else chmod 0644 \"$2\"; fi"
            ),
            arguments = [src_file.path, staged.path],
            mnemonic = "PyDataPackageCopy",
            progress_message = "Staging Python data resource %{output}",
            execution_requirements = {"no-remote": "1"},
        )
        staged_files.append(staged)

    dep_runfiles = [
        t[DefaultInfo].default_runfiles
        for t in all_targets
        if t[DefaultInfo].default_runfiles != None
    ]
    runfiles = ctx.runfiles(files = [init_py] + staged_files).merge_all(dep_runfiles)

    return [
        DefaultInfo(
            files = depset([init_py] + staged_files),
            runfiles = runfiles,
        ),
        PyInfo(
            transitive_sources = depset([init_py]),
            imports = imports,
        ),
        PyDataPackageInfo(
            files = depset(staged_files),
        ),
    ]

_py_data_package = rule(
    implementation = _py_data_package_impl,
    doc = "Stages data files into an importable Python package accessible via `importlib.resources`.",
    provides = [DefaultInfo, PyInfo, PyDataPackageInfo],
    attrs = {
        "srcs": attr.label_list(
            doc = "Source files or targets staged directly into `<package_dir>/` by their basename (or executable output basename), without preserving source subdirectories.",
            allow_files = True,
        ),
        "srcs_map": attr.string_keyed_label_dict(
            doc = "Mapping from plain destination filename (without `'/'`; nested subdirectories are not supported) to a single-file or executable source label.",
            allow_files = True,
        ),
        "package_dir": attr.string(
            doc = "Package-relative directory in which to stage `__init__.py` and data files (for example, `\"data\"` or `\"lib/my_pkg/data\"`). Defaults to `name`.",
        ),
        "imports": attr.string_list(
            doc = "List of import directories (relative to the current Bazel package, such as `[\".\"]` or `[\"..\"]`) to add to `PYTHONPATH`, matching `py_library.imports`.",
        ),
    },
)

def _py_data_package_macro_impl(name, visibility, tags, target_compatible_with, **kwargs):
    resolved_tags = list(tags) if tags != None else []
    if "no-pyrefly" not in resolved_tags:
        resolved_tags.append("no-pyrefly")
    _py_data_package(
        name = name,
        visibility = visibility,
        tags = resolved_tags,
        target_compatible_with = (
            HOST_OS_CONSTRAINTS if target_compatible_with == None else target_compatible_with
        ),
        **kwargs
    )

py_data_package = macro(
    doc = """Stages data files into an importable Python package for `importlib.resources`.

Generates `<package_dir>/__init__.py` and copies `srcs` (by basename) and
`srcs_map` (by plain filename key; nested subdirectories are not supported)
directly into `<package_dir>/` (`package_dir` defaults to `name`). Pass the
target in `deps` of `py_library()`, `py_binary()`, `host_py_test()`, or
`py_zipapp_binary()`.

Examples (in `//foo/bar/BUILD.bazel`):

  1. Subpackage of an existing library (`py_library` with `imports = [".."]`):
     ```python
     py_data_package(
         name = "data",
         srcs = ["config.json"],
         srcs_map = {"tool": "//tools:bin"},
     )
     ```
     Staged layout (imported via `from bar import data`):
     ```
     foo/bar/
       __init__.py          # from py_library
       data/
         __init__.py        # generated
         config.json        # from srcs
         tool               # from srcs_map
     ```

  2. Standalone package (`imports` adds a package-relative root to `PYTHONPATH`):
     ```python
     py_data_package(
         name = "my_data",
         package_dir = "my_data/test_data",
         srcs = ["fixture.txt"],
         imports = ["my_data"],
     )
     ```
     Staged layout (imported via `import test_data`; prefixing with `my_data/`
     avoids output collisions if multiple targets in `BUILD.bazel` define
     `test_data`):
     ```
     foo/bar/my_data/       # <- added to PYTHONPATH via imports
       test_data/
         __init__.py        # generated
         fixture.txt        # from srcs
     ```

Only `<package_dir>/__init__.py` is placed in `PyInfo.transitive_sources` (with
`"no-pyrefly"` in `tags`), while data files propagate via `DefaultInfo.runfiles`
and `PyDataPackageInfo` (for `py_zipapp_binary()`). `target_compatible_with`
defaults to `HOST_OS_CONSTRAINTS`.

When executing a staged binary via `importlib.resources.as_file(res)`, guard
`chmod` with `if not os.access(path, os.X_OK):`: `.pyz` extraction creates a
non-executable temporary file, while `py_binary()` and `host_py_test()` yield
the read-only runfiles path directly (where unconditional `chmod` fails with
`EROFS` inside `linux-sandbox`).
""",
    implementation = _py_data_package_macro_impl,
    inherit_attrs = _py_data_package,
)
