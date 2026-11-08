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

WORKDIR="${PLANTER_WORKDIR:-.}"

python3 - "$WORKDIR" <<'PYEOF'
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
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

REMEDIATION = """\
The new BUILD.bazel turns this directory into its own Bazel package, so the ancestor package
can no longer name files inside it by relative path. Fix it in the same change (this edit
outside the target directory is required, not optional):
  1. In the new package, add `exports_files(["<file>"], visibility = [<ancestor>:__pkg__])`
     (or a filegroup) for each file the ancestor uses.
  2. In the ancestor BUILD.bazel, replace "<subdir>/<file>" with "//<new pkg>:<file>".
Then re-run the Bazel build of the ancestor's consumers."""

findings = []
for pkg in new_bazel_packages():
    for anc, rel in ancestor_packages(pkg):
        anc_file = os.path.join(anc, "BUILD.bazel")
        try:
            with open(os.path.join(workdir, anc_file), encoding="utf-8") as f:
                lines = f.read().splitlines()
        except OSError:
            continue
        for lineno, line in enumerate(lines, 1):
            if line.lstrip().startswith("#"):
                continue
            for value in STRING_RE.findall(line):
                if ":" in value or value.startswith("//"):
                    continue
                if value == rel or value.startswith(rel + "/"):
                    findings.append({
                        "source": "package_boundary_labels",
                        "category": "crossed_package_boundary",
                        "severity": "error",
                        "file": anc_file,
                        "line": lineno,
                        "message": (
                            f"'{anc_file}' references \"{value}\" by relative path, but the new "
                            f"'{pkg}/BUILD.bazel' makes '{pkg}' a subpackage, so this label "
                            "is invalid and the ancestor package fails analysis."
                        ),
                        "remediation": REMEDIATION,
                    })

print(json.dumps(findings, indent=2))
PYEOF
