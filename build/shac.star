# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""shac checks specific to //build.

shac discovers nested shac.star files automatically, and runs each one with its
file paths scoped to its own directory, so all paths here are relative to
//build.
"""

load("//scripts/shac/common.star", "cipd_platform_name", "os_exec")

# GN templates that type check their sources (unless `enable_mypy = false`).
_TYPE_CHECKED_TEMPLATES = [
    "python_binary",
    "python_build_time_tests",
    "python_host_test",
    "python_library",
    "python_mobly_test",
    "python_perf_test",
    "sdk_python_mobly_test",
]

# Attributes of _TYPE_CHECKED_TEMPLATES that pass Python source files to the
# type checker. Not every template accepts every attribute; ones that a target
# doesn't set are simply skipped.
_SOURCE_ATTRS = ["main_source", "sources", "tests", "inputs"]

def _resolve(base_dir, gn_path):
    """Resolves a GN path to a path relative to //build.

    Returns None for paths outside //build.
    """
    if gn_path.startswith("//"):
        path = gn_path[2:]
        if path == "build":
            path = ""
        elif path.startswith("build/"):
            path = path[len("build/"):]
        else:
            return None
    elif gn_path.startswith("/"):
        return None
    else:
        path = base_dir + "/" + gn_path

    parts = []
    for part in path.split("/"):
        if part in ("", "."):
            continue
        if part == "..":
            if not parts:
                return None
            parts.pop()
        else:
            parts.append(part)
    return "/".join(parts)

def _literals(node):
    """Returns the plain string literals in a GN string or list expression."""
    if node["type"] == "LITERAL":
        value = node["value"]
        if value.startswith('"') and "$" not in value:
            return [value[1:-1]]
    elif node["type"] == "LIST" or node.get("value") == "+":
        return [s for child in node.get("child", []) for s in _literals(child)]
    return []

def _find_type_checked_sources(node, build_dir):
    """Recursively finds the Python sources of type-checked targets in a GN AST.

    Only string literals assigned directly in a target's block are seen, so
    sources passed via variables, foreach() or wrapper templates are missed.

    Args:
      node: A node of the JSON AST from `gn format --dump-tree=json`.
      build_dir: Directory of the BUILD.gn file, relative to //build.

    Returns:
      A list of paths relative to //build.
    """
    children = node.get("child", [])
    if node["type"] != "FUNCTION" or node["value"] not in _TYPE_CHECKED_TEMPLATES:
        return [
            path
            for child in children
            for path in _find_type_checked_sources(child, build_dir)
        ]

    # A template call's children are its argument list, e.g. `("foo")`, and
    # then its `{ ... }` block, which holds the target's attribute assignments.
    block = children[-1]
    if block["type"] != "BLOCK":
        return []
    attrs = {}
    for stmt in block.get("child", []):
        lhs = stmt.get("child", [{}])[0]
        if stmt["type"] == "BINARY" and lhs.get("type") == "IDENTIFIER":
            attrs.setdefault(lhs["value"], []).extend(_literals(stmt["child"][1]))

    # python_library() resolves `sources` relative to `source_root`.
    base_dir = build_dir
    for source_root in attrs.get("source_root", []):
        base_dir = _resolve(build_dir, source_root)
    if base_dir == None:
        return []
    paths = []
    for attr in _SOURCE_ATTRS:
        for s in attrs.get(attr, []):
            path = _resolve(base_dir, s)
            if s.endswith(".py") and path != None:
                paths.append(path)
    return paths

def _build_python_type_check_coverage(ctx):
    """Checks that every Python file under //build is in a GN Python target.

    Type checking runs as a GN build validation (with access to
    `library_infos` and generated sources) on Python files listed in GN Python
    targets, so a script that's only referenced as an action's `script` never
    gets type checked.
    Targets with `enable_mypy = false` also count so that opting out stays
    explicit and reviewable in GN. This can't be a build-time test because
    discovering unlisted files requires reading the source tree, which isn't
    hermetic.

    Every Python file is checked, not just affected ones, because removing a
    script from a BUILD.gn file uncovers it without touching the script.

    Args:
      ctx: A ctx instance.
    """
    if not ctx.scm.affected_files(glob = ["*.py", "BUILD.gn"]):
        return

    py_files = [
        f
        for f in ctx.scm.all_files(glob = ["*.py", "!*_pb2.py"])
        if f.rpartition("/")[2] != "__init__.py"
    ]

    # Parse BUILD.gn files with GN itself rather than regexes, starting all
    # the processes before waiting on any so they run in parallel.
    # ctx.scm.root is //build, since this is a nested shac entrypoint.
    gn = "%s/../prebuilt/third_party/gn/%s/gn" % (ctx.scm.root, cipd_platform_name(ctx))
    procs = {
        f: os_exec(ctx, [gn, "format", "--dump-tree=json", f])
        for f in ctx.scm.all_files(glob = ["BUILD.gn"])
    }
    covered = {}
    for build_file, proc in procs.items():
        tree = json.decode(proc.wait().stdout)
        for path in _find_type_checked_sources(tree, build_file.rpartition("/")[0]):
            covered[path] = True

    for f in py_files:
        if f in covered:
            continue
        ctx.emit.finding(
            level = "error",
            message = (
                "Python files under //build must be type checked. " +
                "Add this file to the `sources` of a GN Python target such " +
                "as a python_library(), python_binary(), python_host_test() " +
                "or python_build_time_tests(), or to " +
                "//build:type_checked_scripts if it isn't part of any other " +
                "target. Sources must be listed as string literals in the " +
                "target itself for this check to see them."
            ),
            filepath = f,
        )

shac.register_check(_build_python_type_check_coverage)
