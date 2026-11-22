#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Detects ancestor Bazel packages broken by a newly added BUILD.bazel.
#
# Adding BUILD.bazel to a directory makes it a new Bazel package. Any existing
# ancestor BUILD.bazel that refers to files under that directory with a
# package-relative path (e.g. "sub/dir/data/config.json" in `srcs`) becomes
# invalid ("Label ... is invalid because ... is a subpackage") and breaks
# analysis of every target that loads the ancestor package.
#
# Category: crossed_package_boundary (error).

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKDIR="${PLANTER_WORKDIR:-.}"

python3 - "$WORKDIR" "$SCRIPT_DIR" <<'PYEOF'
import ast
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
script_dir = os.path.abspath(sys.argv[2]) if len(sys.argv) > 2 else ""
if script_dir and os.path.isdir(script_dir) and script_dir not in sys.path:
    sys.path.insert(0, script_dir)
try:
    import planter_check_lib as pcl

    change_base = pcl.resolve_change_base(workdir)
except Exception:
    change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"


def git(args):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return ""


# Collect BUILD.bazel files that are new in this change (added or untracked).
def new_bazel_packages():
    added = git(["diff", "--name-only", "--diff-filter=A", change_base]).splitlines()
    added += git(["ls-files", "--others", "--exclude-standard"]).splitlines()
    return sorted(
        {os.path.dirname(p.strip()) for p in added if os.path.basename(p.strip()) == "BUILD.bazel"}
    )


# Yield (ancestor_dir, relative_subdir) for each ancestor that has a BUILD.bazel.
def ancestor_packages(pkg):
    parts = pkg.split("/")
    for i in range(len(parts) - 1, 0, -1):
        anc = "/".join(parts[:i])
        if os.path.isfile(os.path.join(workdir, anc, "BUILD.bazel")):
            yield anc, "/".join(parts[i:])
            return  # The nearest ancestor package owns the files.


STRING_RE = re.compile(r'"([^"\n]*)"')

# Non-label string attributes in Bazel rules and Fuchsia Starlark macros (such
# as in-package destination directories, target/artifact names, and tool flags).
NON_LABEL_ATTRS = frozenset({
    "add_prefix",
    "api_area",
    "args",
    "binary_name",
    "category",
    "cmd",
    "cmd_bash",
    "cmd_bat",
    "cmd_ps",
    "component_name",
    "conlyopts",
    "copts",
    "cpu",
    "crate_features",
    "crate_name",
    "cxxopts",
    "defines",
    "deprecation",
    "dest",
    "dest_dir",
    "destination",
    "driver_name",
    "edition",
    "env",
    "exec_properties",
    "expected_basenames",
    "expected_environments",
    "expected_message",
    "expected_realms",
    "features",
    "formatter_args",
    "formatter_extensions",
    "gc_linkopts",
    "gn_output_name",
    "go_tags",
    "golden_dir",
    "host_test_wrapper_generator_args",
    "id",
    "idk_name",
    "idk_root_path",
    "importpath",
    "include_base",
    "include_dest",
    "include_prefix",
    "install_root",
    "javacopts",
    "kernel_cmdline",
    "ldflags",
    "library_name",
    "linkopts",
    "list_cases_argument",
    "local_defines",
    "max_log_severity",
    "message",
    "meta_dest",
    "min_log_severity",
    "module_name",
    "name",
    "os",
    "output_name",
    "package_dir",
    "package_name",
    "prebuilt_library_type",
    "prefix",
    "restricted_features",
    "rustc_env",
    "rustc_flags",
    "shared_lib_dest",
    "shell_commands",
    "soname",
    "strip_include_prefix",
    "strip_prefix",
    "stub_name",
    "subdir",
    "tags",
    "test_args",
    "test_type",
    "timeout",
    "type",
    "version",
})

REMEDIATION = """\
The new BUILD.bazel turns this directory into its own Bazel package, so the ancestor package
(and external callers) can no longer name files inside it via `//<ancestor>:<subdir>/<file>`:
  1. In the new `<new pkg>/BUILD.bazel`, add `exports_files(["<file>"], visibility = [...])`
     preserving the visibility needed by the ancestor and any external callers.
  2. In the ancestor `BUILD.bazel`, if `"<subdir>/<file>"` is inside `exports_files([...])`,
     remove `"<subdir>/<file>"` from `exports_files` (or delete the `exports_files` call if no
     other files remain); if it is in a rule attribute (`srcs`, `data`, etc.), replace
     `"<subdir>/<file>"` with `"//<new pkg>:<file>"`.
  3. Update any consumer `BUILD.bazel` or `.bzl` files that reference
     `"//<ancestor>:<subdir>/<file>"` to `"//<new pkg>:<file>"`."""


# Check whether a candidate string crosses into subpackage `rel` as a relative file label.
def is_crossed_subpackage_label(value, rel, in_concat=False):
    if not value or ":" in value or value.startswith(("//", "@")):
        return False
    if value.endswith("/") and not in_concat:
        return False
    return value == rel or value.startswith(rel + "/")


# Resolve the simple function/macro name of an AST Call node.
def call_func_name(call_node):
    if isinstance(call_node.func, ast.Name):
        return call_node.func.id
    if isinstance(call_node.func, ast.Attribute):
        return call_node.func.attr
    return ""


# Extract `(lineno, value, in_concat)` candidates from label-bearing AST positions.
def iter_candidate_label_strings(source, anc_file):
    try:
        tree = ast.parse(source, filename=anc_file)
    except SyntaxError:
        for lineno, line in enumerate(source.splitlines(), 1):
            if line.lstrip().startswith("#"):
                continue
            for val in STRING_RE.findall(line):
                yield lineno, val, False
        return

    top_vars = {}
    for stmt in tree.body:
        if isinstance(stmt, ast.Assign):
            for tgt in stmt.targets:
                if isinstance(tgt, ast.Name):
                    top_vars[tgt.id] = stmt.value

    def walk_call(call_node, seen_vars):
        fname = call_func_name(call_node)
        if fname == "load":
            return
        if fname == "select":
            for arg in call_node.args:
                yield from walk_expr(arg, seen_vars, in_select=True)
            return
        for arg in call_node.args:
            yield from walk_expr(arg, seen_vars)
        for kw in call_node.keywords:
            if kw.arg is None:
                yield from walk_expr(kw.value, seen_vars)
            elif kw.arg in NON_LABEL_ATTRS:
                continue
            elif fname == "glob" and kw.arg == "exclude":
                continue
            else:
                yield from walk_expr(kw.value, seen_vars)

    def walk_expr(node, seen_vars, in_select=False, in_concat=False):
        if node is None:
            return
        if isinstance(node, ast.Constant) and isinstance(node.value, str):
            yield node.lineno, node.value, in_concat
            return
        if isinstance(node, ast.Name):
            if node.id in top_vars and node.id not in seen_vars:
                yield from walk_expr(
                    top_vars[node.id],
                    seen_vars | {node.id},
                    in_select=in_select,
                    in_concat=in_concat,
                )
            return
        if isinstance(node, ast.Call):
            yield from walk_call(node, seen_vars)
            return
        if isinstance(node, ast.Dict):
            for key_node, val_node in zip(node.keys, node.values):
                if in_select:
                    if key_node is not None:
                        yield from walk_expr(key_node, seen_vars)
                    yield from walk_expr(val_node, seen_vars)
                else:
                    if (
                        isinstance(key_node, ast.Constant)
                        and isinstance(key_node.value, str)
                        and key_node.value in NON_LABEL_ATTRS
                    ):
                        continue
                    yield from walk_expr(val_node, seen_vars)
            return
        if isinstance(node, ast.BinOp):
            is_str_op = isinstance(node.op, (ast.Add, ast.Mod)) and (
                (isinstance(node.left, ast.Constant) and isinstance(node.left.value, str))
                or (isinstance(node.right, ast.Constant) and isinstance(node.right.value, str))
            )
            yield from walk_expr(node.left, seen_vars, in_concat=is_str_op)
            yield from walk_expr(node.right, seen_vars, in_concat=is_str_op)
            return
        for child in ast.iter_child_nodes(node):
            yield from walk_expr(child, seen_vars)

    for stmt in tree.body:
        if isinstance(stmt, ast.Assign):
            for sub in ast.walk(stmt.value):
                if isinstance(sub, ast.Call) and call_func_name(sub) in ("glob", "exports_files"):
                    yield from walk_call(sub, frozenset())
            continue
        if (
            isinstance(stmt, ast.Expr)
            and isinstance(stmt.value, ast.Constant)
            and isinstance(stmt.value.value, str)
        ):
            continue
        for sub in ast.iter_child_nodes(stmt):
            yield from walk_expr(sub, frozenset())


findings = []
for pkg in new_bazel_packages():
    for anc, rel in ancestor_packages(pkg):
        anc_file = os.path.join(anc, "BUILD.bazel")
        try:
            with open(os.path.join(workdir, anc_file), encoding="utf-8") as f:
                source = f.read()
        except OSError:
            continue
        seen_hits = set()
        for lineno, value, in_concat in iter_candidate_label_strings(source, anc_file):
            if not is_crossed_subpackage_label(value, rel, in_concat=in_concat):
                continue
            hit_key = (lineno, value)
            if hit_key in seen_hits:
                continue
            seen_hits.add(hit_key)
            sub_file = value[len(rel) + 1 :] if value.startswith(rel + "/") else ""
            caller_files = []
            if sub_file:
                old_lbl = f"//{anc}:{value}"
                grep_out = git(
                    [
                        "grep",
                        "-l",
                        "--fixed-strings",
                        old_lbl,
                        "--",
                        "BUILD.bazel",
                        "*/BUILD.bazel",
                        "*.bzl",
                    ]
                )
                caller_files = [
                    c.strip() for c in grep_out.splitlines() if c.strip()
                ]
            caller_note = (
                f" Also update external reference(s) to `//{anc}:{value}` in "
                + ", ".join(f"'{c}'" for c in caller_files)
                + f" to `//{pkg}:{sub_file}`."
                if caller_files and sub_file
                else ""
            )
            findings.append({
                "source": "package_boundary_labels",
                "category": "crossed_package_boundary",
                "severity": "error",
                "file": anc_file,
                "line": lineno,
                "message": (
                    f"'{anc_file}' references \"{value}\" by relative path, but the new "
                    f"'{pkg}/BUILD.bazel' makes '{pkg}' a subpackage, so label "
                    f"'//{anc}:{value}' is invalid and the ancestor package fails analysis."
                    + caller_note
                ),
                "remediation": REMEDIATION + caller_note,
            })

print(json.dumps(findings, indent=2))
PYEOF
