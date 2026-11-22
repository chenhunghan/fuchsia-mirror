#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Flags edits outside a package cutover's logical group.
#
# A package cutover (see cutover_consistency.sh) must not be mixed with
# unrelated edits. The change may hold one logical group: the package being
# cut over plus its close dependencies. Static approximation of that group:
#   * the cut-over package's directory and its subdirectories;
#   * directories of the labels its BUILD.bazel references (its components,
#     binaries, manifests and their libraries), excluding //build, //sdk,
#     //third_party, //prebuilt and //zircon;
#   * BUILD.gn / BUILD.bazel files of ancestor directories (aggregation
#     groups and test suites);
#   * //bundles/assembly (the assembly label and the bazel_inputs bridge).
#
# Prints `[]` when no cutover is detected. Category:
# cutover_scope_outside_group (INFO at Stage 0).

WORKDIR="${PLANTER_WORKDIR:-.}"

python3 - "$WORKDIR" <<'PYEOF'
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
utils_dir = os.path.join(workdir, "build", "beads", "scripts")
if not os.path.isfile(os.path.join(utils_dir, "cutover_utils.py")):
    print("[]")
    sys.exit(0)
sys.path.insert(0, utils_dir)

EXTERNAL = ("build/", "sdk/", "third_party/", "prebuilt/", "zircon/")


def under(path, d):
    return path == d or path.startswith(d.rstrip("/") + "/")


def main():
    import cutover_utils as cu

    base = os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip() or os.environ.get(
        "PLANTER_CHANGE_BASE", "").strip() or "HEAD"
    changed = [l.strip() for l in os.environ.get("PLANTER_CHANGED_FILES", "").splitlines() if l.strip()]
    if not changed:
        changed = cu.git(workdir, ["diff", "--name-only", base]).splitlines()
        changed += cu.git(workdir, ["ls-files", "--others", "--exclude-standard"]).splitlines()
    changed = [p for p in changed if p and not os.path.basename(p).startswith("CoderReport")]

    # Packages that only stay dual-built (e.g. next to a bazel2gn mirror) are
    # not cutovers, unless Planter is running a cutover phase.
    phase = os.environ.get("PLANTER_CUTOVER_PHASE", "").strip()
    cutovers = [
        c for c in cu.detect_cutovers(workdir, base, changed)
        if phase in ("parallel", "final") or c.state != "dual_build"
    ]
    if not cutovers:
        return []

    group_dirs = {"bundles/assembly"}
    ancestors = set()
    for c in cutovers:
        pkg_dir = cu.label_dir(c.bazel_label)
        group_dirs.add(pkg_dir)
        gn_dir = cu.label_dir(c.gn_label)
        group_dirs.add(gn_dir)
        for d in (pkg_dir, gn_dir):
            parts = d.split("/")
            ancestors.update("/".join(parts[:i]) for i in range(0, len(parts)))
        text = cu.read_text(os.path.join(workdir, pkg_dir, "BUILD.bazel")) or ""
        for label in re.findall(r'"(//[^":]+)(?::[^"]*)?"', text):
            d = label[2:]
            if d and not (d + "/").startswith(EXTERNAL):
                group_dirs.add(d)

    findings = []
    for path in sorted(changed):
        if any(under(path, d) for d in group_dirs):
            continue
        if os.path.basename(path) in ("BUILD.gn", "BUILD.bazel") and os.path.dirname(path) in ancestors:
            continue
        names = ", ".join(sorted({c.package_name for c in cutovers}))
        findings.append({
            "source": "cutover_scope",
            "category": "cutover_scope_outside_group",
            "severity": "INFO",
            "file": path,
            "line": 0,
            "message": (
                f"[{path}] This change cuts over {names} from GN to Bazel, but also edits a file "
                "outside that package's logical group (the package directory, the directories of "
                "labels its BUILD.bazel references, ancestor BUILD files, and //bundles/assembly)."
            ),
            "remediation": (
                "Keep a package cutover in a change of its own: move unrelated edits to a separate "
                "change, or explain in the commit message why this file belongs with the cutover."
            ),
        })
    return findings


try:
    result = main()
except Exception as e:  # A static INFO check must never block.
    print(f"cutover_scope: {e!r}", file=sys.stderr)
    result = []
print(json.dumps(result, indent=2))
PYEOF
