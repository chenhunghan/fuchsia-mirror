#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Flags GN groups and BUILD.gn files that a migration leaves empty.
#
# When every target behind a GN group moves to Bazel, the group (and often the
# whole BUILD.gn) should be deleted along with the references to it. Leaving an
# empty `group("tests")` behind makes `fx set --with-test //<dir>:tests` succeed
# while configuring no tests, and leaves dead aggregation chains in GN.
#
# Only groups that this change made empty are reported, so pre-existing empty
# placeholders (e.g. zircon `boot_tests` stubs) are left alone. A group counts
# as empty when it has no `deps`, `public_deps`, `data_deps`, or any attribute
# other than `testonly` and `visibility`. A group whose deps all point at such
# groups is also empty, which catches forwarding groups in ancestor BUILD.gn
# files. A group with a `# Intentionally empty: <reason>` comment directly
# above it is never reported.
#
# Categories: empty_gn_group, empty_gn_build_file.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKDIR="${PLANTER_WORKDIR:-.}"
CHANGE_BASE="${PLANTER_RESOLVED_CHANGE_BASE:-${PLANTER_CHANGE_BASE:-HEAD}}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"

python3 - "$WORKDIR" "$CHANGE_BASE" "$TARGET_DIRS" "$SCRIPT_DIR" <<'PYEOF'
import json
import os
import subprocess
import sys
import tempfile

workdir = os.path.abspath(sys.argv[1])
script_dir = os.path.abspath(sys.argv[4])
sys.path.insert(0, script_dir)
import planter_check_lib as pcl

change_base = pcl.resolve_change_base(
    workdir, sys.argv[2].strip() or "HEAD", target_dirs=sys.argv[3]
)

DEP_ATTRS = ("deps", "public_deps", "data_deps")
# Attributes that don't give a group any effect on their own.
INERT_ATTRS = ("testonly", "visibility")
OPT_OUT_MARKER = "intentionally empty"
# Files whose empty groups are placeholders by design.
EXEMPT_FILES = (
    # Needs one group per AIB in //bundles/assembly/BUILD.gn, even when the AIB
    # has no GN-built inputs left.
    "bundles/assembly/bazel_inputs/BUILD.gn",
)


def git(args):
    proc = subprocess.run(
        ["git", "-C", workdir] + args,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        check=False,
    )
    return proc.stdout if proc.returncode == 0 else None


def changed_gn_files():
    raw = os.environ.get("PLANTER_CHANGED_FILES", "")
    paths = [l.strip() for l in raw.splitlines() if l.strip()]
    if not paths:
        paths = (git(["diff", "--name-only", "--diff-filter=AMR", change_base]) or "").split()
        paths += (git(["ls-files", "--others", "--exclude-standard"]) or "").split()
    return sorted(
        {
            p
            for p in paths
            if os.path.basename(p) == "BUILD.gn"
            and p not in EXEMPT_FILES
            and os.path.isfile(os.path.join(workdir, p))
        }
    )


# Run GN through fx rather than a hardcoded //prebuilt path, which
# check_no_new_direct_prebuilts forbids. Fall back to the checkout this script
# lives in when the workdir has no fx (e.g. in unit tests).
def find_fx():
    roots = (workdir, os.path.normpath(os.path.join(script_dir, *[os.pardir] * 5)))
    for root in roots:
        for rel in (".jiri_root/bin/fx", "scripts/fx"):
            path = os.path.join(root, rel)
            if os.access(path, os.X_OK):
                return path
    return None


FX = find_fx()


def dump_tree(content):
    # `gn format --stdin` prints the formatted file after the JSON, so parse a
    # temp file instead.
    with tempfile.NamedTemporaryFile("w", suffix=".gn", encoding="utf-8", delete=False) as f:
        f.write(content)
        tmp = f.name
    try:
        proc = subprocess.run(
            [FX, "gn", "format", "--dump-tree=json", tmp],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            check=False,
        )
    finally:
        os.unlink(tmp)
    if proc.returncode != 0:
        return None
    try:
        return json.loads(proc.stdout)
    except ValueError:
        return None


class Group:
    def __init__(self, name, line, deps, inert, opted_out):
        self.name = name
        self.line = line
        # (label, line) pairs from literal entries of dep lists.
        self.deps = deps
        # False if the body has anything besides inert attrs and literal deps.
        self.inert = inert
        self.opted_out = opted_out

    @property
    def empty(self):
        return self.inert and not self.deps


class ParsedFile:
    def __init__(self, groups, only_groups, has_targets):
        # name -> Group
        self.groups = groups
        # True if every non-import statement at the top level is a group.
        self.only_groups = only_groups
        # True if the file has any statement besides imports and comments.
        self.has_targets = has_targets


def parse_group(node):
    kids = node.get("child") or []
    if len(kids) < 2 or kids[0].get("type") != "LIST":
        return None
    name_nodes = kids[0].get("child") or []
    if not name_nodes or name_nodes[0].get("type") != "LITERAL":
        return None
    name = name_nodes[0]["value"].strip('"')
    deps = []
    inert = True
    for stmt in kids[1].get("child") or []:
        if stmt.get("type") == "BLOCK_COMMENT":
            continue
        # `-=` can empty a list, so don't try to evaluate it.
        if stmt.get("type") != "BINARY" or stmt.get("value") == "-=":
            inert = False
            continue
        lhs, rhs = stmt["child"][0], stmt["child"][1]
        attr = lhs.get("value")
        if attr in INERT_ATTRS:
            continue
        if attr not in DEP_ATTRS or rhs.get("type") != "LIST":
            inert = False
            continue
        for item in rhs.get("child") or []:
            if item.get("type") == "BLOCK_COMMENT":
                continue
            if item.get("type") != "LITERAL":
                inert = False
                continue
            deps.append((item["value"].strip('"'), item["location"]["begin_line"]))
    comments = " ".join(node.get("before_comment") or []).lower()
    return Group(
        name,
        node["location"]["begin_line"],
        deps,
        inert,
        OPT_OUT_MARKER in comments,
    )


def collect_groups(block, out):
    for node in block.get("child") or []:
        kind = node.get("type")
        if kind == "FUNCTION" and node.get("value") == "group":
            g = parse_group(node)
            if g:
                out[g.name] = g
        elif kind == "CONDITION":
            for sub in (node.get("child") or [])[1:]:
                if sub.get("type") == "BLOCK":
                    collect_groups(sub, out)
                elif sub.get("type") == "CONDITION":
                    collect_groups({"child": [sub]}, out)


def parse(content):
    if content is None:
        return None
    tree = dump_tree(content)
    if tree is None:
        return None
    groups = {}
    collect_groups(tree, groups)
    statements = [
        n
        for n in tree.get("child") or []
        if n.get("type") != "BLOCK_COMMENT"
        and not (n.get("type") == "FUNCTION" and n.get("value") == "import")
    ]
    only_groups = all(
        n.get("type") == "FUNCTION" and n.get("value") == "group" for n in statements
    )
    return ParsedFile(groups, only_groups, bool(statements))


_cache = {}


def parse_current(rel):
    if rel not in _cache:
        path = os.path.join(workdir, rel)
        content = None
        if os.path.isfile(path):
            with open(path, encoding="utf-8", errors="replace") as f:
                content = f.read()
        _cache[rel] = parse(content)
    return _cache[rel]


_renames = None


def base_path(rel):
    """Returns the path `rel` had at the change base, following renames."""
    global _renames
    if _renames is None:
        _renames = {}
        out = git(["diff", "--name-status", "-M", "--diff-filter=R", change_base]) or ""
        for line in out.splitlines():
            parts = line.split("\t")
            if len(parts) == 3:
                _renames[parts[2]] = parts[1]
    return _renames.get(rel, rel)


class UnparsableBase(Exception):
    pass


def parse_base(rel):
    """Returns the file at the change base, or None if it didn't exist."""
    content = git(["show", f"{change_base}:{base_path(rel)}"])
    if content is None:
        return None
    parsed = parse(content)
    if parsed is None:
        raise UnparsableBase(rel)
    return parsed


def resolve(dep, gn_dir):
    """Returns the absolute `//dir:name` label for a GN dep string."""
    dep = dep.split("(", 1)[0]
    if "$" in dep:
        return None
    if dep.startswith(":"):
        return f"//{gn_dir}{dep}"
    if dep.startswith("//"):
        path, _, name = dep[2:].partition(":")
    else:
        rel, _, name = dep.partition(":")
        path = os.path.normpath(os.path.join(gn_dir, rel))
        if path.startswith(".."):
            return None
        path = "" if path == "." else path
    path = path.rstrip("/")
    return f"//{path}:{name or os.path.basename(path)}"


def label_of(rel, name):
    return f"//{os.path.dirname(rel)}:{name}"


def sorted_locations(locations):
    def key(loc):
        path, _, line = loc.rpartition(":")
        return (path, int(line))

    return sorted(set(locations), key=key)


def ancestor_build_files(rel):
    d = os.path.dirname(rel)
    while d:
        d = os.path.dirname(d)
        candidate = os.path.join(d, "BUILD.gn") if d else "BUILD.gn"
        if os.path.isfile(os.path.join(workdir, candidate)):
            yield candidate


def main():
    if FX is None:
        return []
    changed = changed_gn_files()
    if not changed:
        return []

    # Groups this change made empty (newly added empty, or emptied).
    emptied = {}  # label -> (rel, Group)
    for rel in changed:
        cur = parse_current(rel)
        if cur is None:
            continue
        try:
            base = parse_base(rel)
        except UnparsableBase:
            # Without the old version, every pre-existing empty group would
            # look new.
            continue
        base_groups = base.groups if base else {}
        for name, g in cur.groups.items():
            if not g.empty or g.opted_out:
                continue
            before = base_groups.get(name)
            if before is not None and before.empty:
                continue
            emptied[label_of(rel, name)] = (rel, g)

    # Forwarding groups whose deps now only point at emptied groups, in changed
    # files and in the ancestor BUILD.gn files that usually aggregate them.
    scope = list(changed)
    for rel in changed:
        scope.extend(
            a for a in ancestor_build_files(rel) if a not in scope and a not in EXEMPT_FILES
        )
    progress = bool(emptied)
    while progress:
        progress = False
        for rel in scope:
            cur = parse_current(rel)
            if cur is None:
                continue
            gn_dir = os.path.dirname(rel)
            for name, g in cur.groups.items():
                label = label_of(rel, name)
                if label in emptied or g.opted_out or not g.inert or not g.deps:
                    continue
                if all(resolve(d, gn_dir) in emptied for d, _ in g.deps):
                    emptied[label] = (rel, g)
                    progress = True

    # References to emptied groups from groups that stay, so the remediation
    # can point at every line to delete.
    refs = {}  # label -> ["path:line", ...]
    for rel in scope:
        cur = parse_current(rel)
        if cur is None:
            continue
        gn_dir = os.path.dirname(rel)
        for name, g in cur.groups.items():
            if label_of(rel, name) in emptied:
                continue
            for dep, line in g.deps:
                target = resolve(dep, gn_dir)
                if target in emptied:
                    refs.setdefault(target, []).append(f"{rel}:{line}")

    findings = []
    by_file = {}
    for label, (rel, g) in emptied.items():
        by_file.setdefault(rel, []).append((label, g))

    for rel in sorted(by_file):
        cur = parse_current(rel)
        file_labels = {label_of(rel, n) for n in cur.groups}
        groups_here = sorted(by_file[rel], key=lambda x: x[1].line)
        if cur.only_groups and file_labels and file_labels <= set(emptied):
            ref_lines = sorted_locations(r for l, _ in groups_here for r in refs.get(l, []))
            findings.append(
                {
                    "source": "empty_gn_targets",
                    "category": "empty_gn_build_file",
                    "severity": "error",
                    "file": rel,
                    "line": 1,
                    "message": (
                        f"'{rel}' only contains GN groups that are empty after this change "
                        f"({', '.join(l for l, _ in groups_here)}). Nothing in GN builds "
                        "anything through this file anymore."
                    ),
                    "remediation": (
                        f"Delete '{rel}'"
                        + (
                            f" and remove the references to its groups at {', '.join(ref_lines)}"
                            if ref_lines
                            else " and remove any references to its groups"
                        )
                        + ". Run `fx gen` afterwards to confirm nothing else references "
                        "them. If a group must stay as a placeholder (e.g. a builder "
                        "references it), add a `# Intentionally empty: <reason>` comment "
                        "directly above it."
                    ),
                }
            )
            continue
        for label, g in groups_here:
            ref_lines = sorted_locations(refs.get(label, []))
            findings.append(
                {
                    "source": "empty_gn_targets",
                    "category": "empty_gn_group",
                    "severity": "error",
                    "file": rel,
                    "line": g.line,
                    "message": (
                        f"GN group `{label}` is empty after this change"
                        + (
                            " (all of its deps are empty groups)."
                            if g.deps
                            else "."
                        )
                    ),
                    "remediation": (
                        f"Delete `group(\"{g.name}\")`"
                        + (
                            f" and remove the references to it at {', '.join(ref_lines)}"
                            if ref_lines
                            else " and remove any references to it"
                        )
                        + ". If it must stay as a placeholder (e.g. a builder references "
                        "it), add a `# Intentionally empty: <reason>` comment directly "
                        "above it."
                    ),
                }
            )

    # A changed BUILD.gn left with no statements besides imports and comments.
    for rel in changed:
        cur = parse_current(rel)
        if cur is None or cur.has_targets or rel in by_file:
            continue
        try:
            base = parse_base(rel)
        except UnparsableBase:
            continue
        if base is None or not base.has_targets:
            continue
        findings.append(
            {
                "source": "empty_gn_targets",
                "category": "empty_gn_build_file",
                "severity": "error",
                "file": rel,
                "line": 1,
                "message": f"'{rel}' has no GN targets left after this change.",
                "remediation": f"Delete '{rel}' and remove any references to its directory.",
            }
        )
    return findings


print(json.dumps(main(), indent=2))
PYEOF
