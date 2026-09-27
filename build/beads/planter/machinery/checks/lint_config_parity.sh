#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check for lint-driven source edits during GN-to-Bazel migrations.
#
# 1. lint_driven_source_edit (error): any non-build file changed in the migration
#    change (git diff $PLANTER_CHANGE_BASE) whose diff hunks look like a lint/warning
#    "fix" (removed .clone(), added #[allow]/#[expect]/NOLINT, unused-import
#    removal, `let _ =` insertion, etc.). Reports exact file:line evidence.
# 2. lint_config_semantics (warning): BUILD.bazel `lint_config = "<label>"` whose
#    label is not under //build/config/rust/lints. In Bazel, lint_config REPLACES
#    the macro default lint set, while GN `configs += [...]` / GN lint_config
#    APPENDS to the defaults. Non-standard lint configs can change the effective
#    lint set; any resulting new lint must be solved in build attributes, never
#    by editing sources.

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"

python3 - "$WORKDIR" "$TARGET_DIR" <<'PYEOF'
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
target_dir = sys.argv[2].strip().strip("/")

ALLOWED_BASENAMES = {"BUILD.gn", "BUILD.bazel", "BUILD", "MODULE.bazel"}
ALLOWED_SUFFIXES = (".gni", ".bzl", ".bazelrc")


def git(args):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return ""


def is_build_file(path):
    base = os.path.basename(path)
    return (
        base in ALLOWED_BASENAMES
        or base.endswith(ALLOWED_SUFFIXES)
        or base.startswith("WORKSPACE")
    )


LINT_PATTERNS = [
    ("removed", re.compile(r"\.clone\(\)"), "removed a .clone() call (clippy redundant_clone style fix)"),
    ("removed", re.compile(r"^\s*use\s+[\w:{}, *]+;\s*$"), "removed a `use` import (unused-import style fix)"),
    ("added", re.compile(r"#!?\[\s*(allow|expect)\s*\("), "added a lint suppression attribute"),
    ("added", re.compile(r"NOLINT|clippy::"), "added a lint suppression/annotation"),
    ("added", re.compile(r"^\s*let\s+_\s*="), "added `let _ =` (unused-result style fix)"),
]

findings = []

# Collect the unified diff of the task's change since PLANTER_CHANGE_BASE ("HEAD~1" when HEAD is
# the task's own commit, "HEAD" when HEAD is unrelated upstream history).
change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"
diff_texts = [
    git(["diff", "--unified=0", change_base]),
]

seen = set()
for text in diff_texts:
    cur_file = None
    new_line = 0
    for raw in text.splitlines():
        if raw.startswith("+++ "):
            p = raw[4:].strip()
            cur_file = p[2:] if p.startswith("b/") else (None if p == "/dev/null" else p)
            continue
        if raw.startswith("--- "):
            continue
        m = re.match(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@", raw)
        if m:
            new_line = int(m.group(1))
            continue
        if not cur_file or is_build_file(cur_file):
            continue
        if raw.startswith("+"):
            kind, body, line = "added", raw[1:], new_line
            new_line += 1
        elif raw.startswith("-"):
            kind, body, line = "removed", raw[1:], new_line
        else:
            continue
        for pkind, rx, why in LINT_PATTERNS:
            if pkind == kind and rx.search(body):
                key = (cur_file, line, why)
                if key in seen:
                    continue
                seen.add(key)
                findings.append({
                    "source": "lint_config_parity",
                    "category": "lint_driven_source_edit",
                    "severity": "error",
                    "file": cur_file,
                    "line": line,
                    "message": (
                        f"Migration {why} in non-build file '{cur_file}': `{body.strip()[:120]}`. "
                        "GN-to-Bazel migrations must not edit sources to silence lints/warnings."
                    ),
                    "remediation": (
                        "Revert the source edit (git checkout $PLANTER_CHANGE_BASE -- <file>). "
                        "Pre-existing lint findings are out of scope. If the Bazel build newly reports this "
                        "lint, restore lint parity in BUILD.bazel (lint_config, rustc_flags, testonly, "
                        "features) instead; if impossible, stop and report the blocker."
                    ),
                })

# lint_config semantics warning for BUILD.bazel files in the target package tree.
if target_dir:
    root = os.path.join(workdir, target_dir)
    for dirpath, _dirs, files in os.walk(root):
        if "BUILD.bazel" not in files:
            continue
        path = os.path.join(dirpath, "BUILD.bazel")
        rel = os.path.relpath(path, workdir)
        try:
            lines = open(path, encoding="utf-8").read().splitlines()
        except Exception:
            continue
        for i, l in enumerate(lines, 1):
            m = re.search(r'\blint_config\s*=\s*"([^"]+)"', l)
            if m and not m.group(1).startswith("//build/config/rust/lints"):
                findings.append({
                    "source": "lint_config_parity",
                    "category": "lint_config_semantics",
                    "severity": "warning",
                    "file": rel,
                    "line": i,
                    "message": (
                        f"lint_config = \"{m.group(1)}\" is not under //build/config/rust/lints. "
                        "Bazel lint_config REPLACES the macro default lint set while GN appends to it, "
                        "so the effective lints may differ between GN and Bazel."
                    ),
                    "remediation": (
                        "Verify the Bazel lint set matches the original GN configs. Any lint/compile "
                        "difference must be fixed in build attributes, never by editing source files."
                    ),
                })

print(json.dumps(findings, indent=2))
PYEOF
