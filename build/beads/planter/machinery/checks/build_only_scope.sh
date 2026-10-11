#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check enforcing that a GN-to-Bazel migration only touches
# build-definition files. Any modification to source files (.rs, .cc, .h, .py,
# .go, .fidl, .cml, .json5, etc.) in the migration change is a scope violation:
# migrations must be pure build-graph refactors with zero behavioral/source diff.
#
# Files inspected: `git diff $PLANTER_CHANGE_BASE` (the task's commit, if HEAD is
# the task's commit, plus uncommitted changes) plus untracked files.
#
# Allowed paths (build-definition surface):
#   - BUILD.gn, BUILD.bazel, BUILD, *.BUILD.bazel, *.BUILD (any directory, including
#     workspace/repo root build files such as build/bazel/toplevel.BUILD.bazel)
#   - *.gni, *.bzl (build registration lists / macros, e.g. verification lists)
#   - MODULE.bazel, *.MODULE.bazel, WORKSPACE*, *.bazelrc
#   - a newly added (not modified) test component manifest `.cml` that an
#     `fx_component_manifest` of the nearest BUILD.bazel uses as `manifest` next to an
#     `fx_test_component` (it replaces the manifest GN's fuchsia_unittest_package generated;
#     Bazel does not generate test manifests, for C++ or Rust tests)
#   - OWNERS / README.md are NOT allowed implicitly; they are not needed for migration.

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
ALLOWED_SUFFIXES = (".gni", ".bzl", ".bazelrc", ".BUILD.bazel", ".BUILD", ".MODULE.bazel")
ALLOWED_PREFIX_BASENAMES = ("WORKSPACE",)


def git_lines(args):
    try:
        out = subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return []
    return [l.strip() for l in out.splitlines() if l.strip()]


changed = set()
# The task's change: everything since PLANTER_CHANGE_BASE ("HEAD~1" when HEAD is the task's own
# commit, "HEAD" when HEAD is unrelated upstream history) plus untracked files.
change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"
changed.update(git_lines(["diff", "--name-only", change_base]))
changed.update(git_lines(["ls-files", "--others", "--exclude-standard"]))


def is_build_file(path: str) -> bool:
    base = os.path.basename(path)
    if base in ALLOWED_BASENAMES:
        return True
    if base.endswith(ALLOWED_SUFFIXES):
        return True
    if base.startswith(ALLOWED_PREFIX_BASENAMES):
        return True
    return False


# Newly added files (not modifications of existing ones).
added = set(git_lines(["diff", "--name-only", "--diff-filter=A", change_base]))
added.update(git_lines(["ls-files", "--others", "--exclude-standard"]))


def is_new_test_manifest(path: str) -> bool:
    """A new `.cml` that an `fx_component_manifest` in the nearest BUILD.bazel uses as `manifest`
    while that BUILD.bazel defines an `fx_test_component`: it replaces the manifest that GN's
    `fuchsia_unittest_package` generated when a (C++ or Rust) test package migrates to `fx_test`."""
    if path not in added or not path.endswith(".cml") or path.endswith(".shard.cml"):
        return False
    if not os.path.isfile(os.path.join(workdir, path)):
        return False
    pkg = os.path.dirname(path)
    for _ in range(4):
        bazel = os.path.join(workdir, pkg, "BUILD.bazel")
        if os.path.isfile(bazel):
            try:
                text = open(bazel, encoding="utf-8").read()
            except OSError:
                return False
            rel = os.path.relpath(path, pkg) if pkg else path
            return "fx_test_component(" in text and bool(
                re.search(r'\bmanifest\s*=\s*":?' + re.escape(rel) + r'"', text)
            )
        if not pkg:
            break
        pkg = os.path.dirname(pkg)
    return False


findings = []
for path in sorted(changed):
    if is_build_file(path) or is_new_test_manifest(path):
        continue
    findings.append({
        "source": "build_only_scope",
        "category": "non_build_file_modified",
        "severity": "error",
        "file": path,
        "line": 0,
        "message": (
            f"Migration modifies non-build file '{path}'"
            + ". GN-to-Bazel migrations must not change source code, tests, manifests, or data files."
        ),
        "remediation": (
            "Revert this file to its pre-migration contents (git checkout $PLANTER_CHANGE_BASE -- <file> or "
            "git checkout -- <file>). If the Bazel build surfaces new lint/compile errors, fix the "
            "BUILD.bazel attributes (lint_config, rustc_flags, configs, features, deps) to reproduce "
            "the GN behavior instead of editing sources; if parity is impossible, stop and report it."
        ),
    })

print(json.dumps(findings, indent=2))
PYEOF
