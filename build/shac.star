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

# Bazel rules and macros that type check their Python sources via
# //build/bazel/aspects:pyrefly.bzl (unless `tags = ["no-pyrefly"]`).
_BAZEL_TYPE_CHECKED_RULES = [
    "host_py_test",
    "py_binary",
    "py_library",
    "py_test",
]

# Attributes of _BAZEL_TYPE_CHECKED_RULES that reference Python source files.
_BAZEL_SOURCE_ATTRS = ["srcs", "main"]

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

def _resolve_bazel_label(base_dir, label):
    """Resolves a Bazel file label or relative path to a path relative to //build.

    Returns None for paths outside //build or expressions that aren't file paths.
    """
    if label.startswith("@"):
        if label.startswith("@//"):
            label = label[1:]
        elif label.startswith("@@//"):
            label = label[2:]
        else:
            return None

    if label.startswith("//"):
        pkg, sep, target = label.partition(":")
        if not sep:
            return _resolve(base_dir, pkg)
        if pkg == "//":
            return None
        return _resolve(base_dir, pkg + "/" + target)

    if label.startswith(":"):
        label = label[1:]
    return _resolve(base_dir, label)

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

def _is_ident_char(ch):
    return ch.isalnum() or ch == "_"

def _tokenize_starlark(content):
    """Tokenizes a Starlark BUILD.bazel file into (kind, value) pairs."""
    chars = content.elems()
    n = len(chars)
    tokens = []
    i = 0
    for _ in range(n + 1):
        if i >= n:
            break
        ch = chars[i]
        if ch in (" ", "\t", "\r", "\n"):
            i += 1
            continue
        if ch == "#":
            for _ in range(n - i):
                if i >= n or chars[i] == "\n":
                    break
                i += 1
            continue

        # Handle raw string prefixes r"..." / r'...'
        raw = False
        if ch in ("r", "R") and i + 1 < n and chars[i + 1] in ('"', "'"):
            raw = True
            i += 1
            ch = chars[i]

        if ch in ('"', "'"):
            quote = ch
            triple = (i + 2 < n and chars[i + 1] == quote and chars[i + 2] == quote)
            i += 3 if triple else 1
            buf = []
            for _ in range(n - i + 1):
                if i >= n:
                    break
                if triple:
                    if i + 2 < n and chars[i] == quote and chars[i + 1] == quote and chars[i + 2] == quote:
                        i += 3
                        break
                elif chars[i] == quote:
                    i += 1
                    break
                if chars[i] == "\\" and i + 1 < n:
                    if raw:
                        buf.append("\\")
                    i += 1
                    buf.append(chars[i])
                    i += 1
                    continue
                buf.append(chars[i])
                i += 1
            tokens.append(("STR", "".join(buf)))
            continue

        if ch.isalpha() or ch == "_":
            start = i
            for _ in range(n - i):
                if i >= n or not _is_ident_char(chars[i]):
                    break
                i += 1
            tokens.append(("ID", content[start:i]))
            continue

        if ch in ("(", ")", "[", "]", "{", "}", ",", "=", "+"):
            tokens.append((ch, ch))
            i += 1
            continue

        i += 1
    return tokens

def _extract_string_literals(rhs_tokens):
    """Extracts plain string literals from a string, list, or `+` expression.

    Ignores strings nested inside function calls (such as `select(...)` or
    `glob(...)`) or dicts, and skips `%`-formatted strings.
    """
    strings = []
    paren_depth = 0
    brace_depth = 0
    bracket_depth = 0
    for kind, val in rhs_tokens:
        if kind == "(":
            paren_depth += 1
        elif kind == ")":
            paren_depth -= 1
        elif kind == "{":
            brace_depth += 1
        elif kind == "}":
            brace_depth -= 1
        elif kind == "[":
            bracket_depth += 1
        elif kind == "]":
            bracket_depth -= 1
        elif kind == "STR" and paren_depth == 0 and brace_depth == 0 and bracket_depth <= 1:
            if "%" not in val:
                strings.append(val)
    return strings

def _parse_call_kwargs(tokens, open_paren_idx):
    """Returns a dict mapping keyword arg names to their RHS token lists.

    Given `tokens` where `tokens[open_paren_idx]` is the opening `(` of a rule
    call, splits top-level comma-separated arguments (tracking nested `()`,
    `[]`, `{}`) and collects `attr_name = <expr>` assignments.
    """
    args = []
    current_arg = []
    depth = 1
    for token in tokens[open_paren_idx + 1:]:
        kind = token[0]
        if kind in ("(", "[", "{"):
            depth += 1
            current_arg.append(token)
        elif kind in (")", "]", "}"):
            depth -= 1
            if depth == 0:
                if current_arg:
                    args.append(current_arg)
                break
            current_arg.append(token)
        elif kind == "," and depth == 1:
            if current_arg:
                args.append(current_arg)
            current_arg = []
        else:
            current_arg.append(token)

    kwargs = {}
    for arg in args:
        if len(arg) >= 3 and arg[0][0] == "ID" and arg[1][0] == "=":
            kwargs[arg[0][1]] = arg[2:]
    return kwargs

def _find_bazel_type_checked_sources(content, build_dir):
    """Finds Python sources of type-checked targets in a BUILD.bazel file.

    Only string literals assigned directly to `_BAZEL_SOURCE_ATTRS` in a target
    call are matched, mirroring `_find_type_checked_sources` for GN.

    Args:
      content: Raw text content of the BUILD.bazel file.
      build_dir: Directory of the BUILD.bazel file, relative to //build.

    Returns:
      A list of paths relative to //build.
    """
    tokens = _tokenize_starlark(content)
    paths = []
    for idx, (kind, rule_name) in enumerate(tokens):
        if kind != "ID" or rule_name not in _BAZEL_TYPE_CHECKED_RULES:
            continue
        if idx + 1 >= len(tokens) or tokens[idx + 1][0] != "(":
            continue

        kwargs = _parse_call_kwargs(tokens, idx + 1)
        for attr in _BAZEL_SOURCE_ATTRS:
            for label in _extract_string_literals(kwargs.get(attr, [])):
                if not label.endswith(".py"):
                    continue
                resolved = _resolve_bazel_label(build_dir, label)
                if resolved != None:
                    paths.append(resolved)
    return paths

def _build_python_type_check_coverage(ctx):
    """Checks that every Python file under //build is in a GN or Bazel Python target.

    Type checking runs as a build validation in GN (`validate_python`) and in
    Bazel (`pyrefly_aspect`) on Python files listed in Python targets, so a
    script that's only referenced as an action's `script` never gets type
    checked.
    Targets with `enable_mypy = false` (GN) or `tags = ["no-pyrefly"]` (Bazel)
    also count so that opting out stays explicit and reviewable. This can't be a
    build-time test because discovering unlisted files requires reading the
    source tree, which isn't hermetic.

    Every Python file is checked, not just affected ones, because removing a
    script from a BUILD.gn or BUILD.bazel file uncovers it without touching the
    script.

    Args:
      ctx: A ctx instance.
    """
    if not ctx.scm.affected_files(glob = ["*.py", "BUILD.gn", "BUILD.bazel"]):
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

    # pyrefly_aspect skips build/bazel_sdk, whose packages are built as
    # external repositories, so only GN targets cover Python files there.
    for bazel_file in ctx.scm.all_files(glob = ["BUILD.bazel"]):
        if bazel_file.startswith("bazel_sdk/"):
            continue
        content = str(ctx.io.read_file(bazel_file))
        for path in _find_bazel_type_checked_sources(content, bazel_file.rpartition("/")[0]):
            covered[path] = True

    for f in py_files:
        if f in covered:
            continue
        ctx.emit.finding(
            level = "error",
            message = (
                "Python files under //build must be type checked. " +
                "Add this file to the `srcs`/`sources` of a Bazel or GN " +
                "Python target such as py_library(), py_binary(), " +
                "host_py_test(), python_build_time_tests(), python_library(), " +
                "python_binary(), or python_host_test(), or to " +
                "//build:type_checked_scripts if it isn't part of any other " +
                "target. Sources must be listed as string literals in the " +
                "target itself for this check to see them."
            ),
            filepath = f,
        )

shac.register_check(_build_python_type_check_coverage)
