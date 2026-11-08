#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check verifying that when a GN allowlist group defined in a
# subdirectory governed by its own `OWNERS` file (`<dir>/<subpkg>/BUILD.gn` +
# `<dir>/<subpkg>/OWNERS`) is mirrored into Starlark (`.bzl`), the active
# (non-`None`) allowlist dictionary entry is placed in a `.bzl` file inside
# `<dir>/<subpkg>/` (with a package `BUILD.bazel`) rather than directly in
# `<dir>/*.bzl`, preserving Gerrit `OWNERS` enforcement over the allowlist.

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"
CHANGE_BASE="${PLANTER_RESOLVED_CHANGE_BASE:-${PLANTER_CHANGE_BASE:-HEAD~1}}"

python3 - "$WORKDIR" "$TARGET_DIRS" "$CHANGE_BASE" <<'PYEOF'
import ast
import json
import os
import re
import subprocess
import sys

GROUP_DEF_RE = re.compile(r'\bgroup\(\s*"([^"]+)"\s*\)\s*\{')


# Strip GN line comments outside string literals so commented examples are ignored.
def strip_gn_comments(text):
    return re.sub(
        r'("(?:\\.|[^"\\])*")|#[^\n]*',
        lambda m: m.group(1) or "",
        text,
    )


# Collect relative paths of `.bzl` files added or modified in the current change.
def collect_changed_bzl_files(workdir, change_base):
    candidates = set()
    git_cmds = [
        ["diff", "--name-only", "--diff-filter=ACMR", change_base],
        ["diff", "--name-only", "--diff-filter=ACMR", "HEAD"],
        ["diff", "--cached", "--name-only", "--diff-filter=ACMR"],
        ["show", "--name-only", "--pretty=format:", "HEAD"],
    ]
    for args in git_cmds:
        proc = subprocess.run(
            ["git", "-C", workdir] + args,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            check=False,
        )
        if proc.returncode == 0 and proc.stdout:
            for raw in proc.stdout.splitlines():
                rel = raw.strip()
                if rel.endswith(".bzl") and os.path.isfile(os.path.join(workdir, rel)):
                    candidates.add(rel)
    return sorted(candidates)


# Extract `group("<name>")` target names declared in a GN build file.
def extract_gn_group_names(gn_path):
    if not os.path.isfile(gn_path):
        return set()
    try:
        with open(gn_path, "r", encoding="utf-8") as fh:
            cleaned = strip_gn_comments(fh.read())
    except OSError:
        return set()
    return {m.group(1) for m in GROUP_DEF_RE.finditer(cleaned)}


# Map `<group_name> -> <subdir_name>` for groups defined in a child directory
# `<parent_abs>/<subdir>/BUILD.gn` that has its own `<parent_abs>/<subdir>/OWNERS`
# file and is not also defined in `<parent_abs>/BUILD.gn`.
def discover_owners_scoped_subgroups(parent_abs):
    if not os.path.isdir(parent_abs):
        return {}
    parent_groups = extract_gn_group_names(os.path.join(parent_abs, "BUILD.gn"))
    scoped_map = {}
    try:
        entries = sorted(os.listdir(parent_abs))
    except OSError:
        return {}
    for entry in entries:
        sub_abs = os.path.join(parent_abs, entry)
        if not os.path.isdir(sub_abs):
            continue
        if not os.path.isfile(os.path.join(sub_abs, "OWNERS")):
            continue
        sub_gn = os.path.join(sub_abs, "BUILD.gn")
        if not os.path.isfile(sub_gn):
            continue
        for grp in sorted(extract_gn_group_names(sub_gn) - parent_groups):
            scoped_map.setdefault(grp, entry)
    return scoped_map


# Check whether a Starlark AST expression represents `None`.
def is_none_constant(node):
    return isinstance(node, ast.Constant) and node.value is None


# Audit a single modified `.bzl` file for non-`None` allowlist dict entries that
# belong in an `OWNERS`-gated child subdirectory package.
def audit_bzl_file(workdir, bzl_rel):
    bzl_abs = os.path.join(workdir, bzl_rel)
    parent_rel = os.path.dirname(bzl_rel)
    parent_abs = os.path.join(workdir, parent_rel)
    scoped_groups = discover_owners_scoped_subgroups(parent_abs)
    if not scoped_groups:
        return []
    try:
        with open(bzl_abs, "r", encoding="utf-8") as fh:
            src = fh.read()
        tree = ast.parse(src, filename=bzl_rel)
    except (OSError, SyntaxError):
        return []

    file_findings = []
    bzl_basename = os.path.basename(bzl_rel)
    for node in ast.walk(tree):
        if not isinstance(node, ast.Dict):
            continue
        for key_node, val_node in zip(node.keys, node.values):
            if key_node is None or val_node is None or is_none_constant(val_node):
                continue
            if not (isinstance(key_node, ast.Constant) and isinstance(key_node.value, str)):
                continue
            key_name = key_node.value
            owner_subdir = scoped_groups.get(key_name)
            if not owner_subdir:
                continue
            sub_rel = f"{parent_rel}/{owner_subdir}" if parent_rel else owner_subdir
            sub_bzl_rel = f"{sub_rel}/{bzl_basename}"
            parent_label = f"//{parent_rel}" if parent_rel else "//:"
            msg = (
                f"Allowlist entry `\"{key_name}\"` in '{bzl_rel}' mirrors GN "
                f"`group(\"{key_name}\")` from '{sub_rel}/BUILD.gn', which is "
                f"governed by '{sub_rel}/OWNERS'; defining a non-`None` allowlist "
                f"in '{bzl_rel}' bypasses '{sub_rel}/OWNERS' review ownership."
            )
            rem = (
                f"Move `\"{key_name}\"` into '{sub_bzl_rel}' (with a package "
                f"'{sub_rel}/BUILD.bazel' and `visibility([\"{parent_label}\"])`), "
                f"wrap both '{sub_rel}/BUILD.gn' and '{sub_bzl_rel}' entries in "
                f"matching `# LINT.IfChange({key_name})` / `# LINT.ThenChange(...)` "
                f"comments, and `load(\"//{sub_rel}:{bzl_basename}\", ...)` and "
                f"merge that dict in '{bzl_rel}'."
            )
            file_findings.append(
                {
                    "source": "owners_allowlist_scope",
                    "category": "owners_scoped_allowlist_misplaced",
                    "severity": "error",
                    "file": bzl_rel,
                    "line": key_node.lineno,
                    "message": msg,
                    "remediation": rem,
                }
            )
    return file_findings


def main():
    workdir = os.path.abspath(sys.argv[1])
    change_base = sys.argv[3].strip() or "HEAD~1"
    findings = []
    for bzl_rel in collect_changed_bzl_files(workdir, change_base):
        findings.extend(audit_bzl_file(workdir, bzl_rel))
    print(json.dumps(findings, indent=2))


if __name__ == "__main__":
    main()
PYEOF
