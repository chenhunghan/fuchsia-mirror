#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Deterministic check mirroring the CQ static-checks `gn_format` step: every GN
# file (BUILD.gn, *.gni, *.gn) added or modified by the change, anywhere in the
# checkout (e.g. a hand-edited verification .gni list or a parent group), must
# be formatted per `gn format`. Prints a JSON array of findings on stdout.

WORKDIR="${PLANTER_WORKDIR:-.}"
CHANGE_BASE="${PLANTER_CHANGE_BASE:-HEAD}"

python3 - "$WORKDIR" "$CHANGE_BASE" <<'PYEOF'
import json
import os
import shutil
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
change_base = sys.argv[2].strip() or "HEAD"


def git_lines(args):
    try:
        out = subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return []
    return [l.strip() for l in out.splitlines() if l.strip()]


changed = set(git_lines(["diff", "--name-only", change_base]))
changed.update(git_lines(["ls-files", "--others", "--exclude-standard"]))
gn_files = sorted(
    p for p in changed
    if (p.endswith(".gn") or p.endswith(".gni")) and os.path.isfile(os.path.join(workdir, p))
)

gn_bin = None
for cand in (
    "prebuilt/third_party/gn/linux-x64/gn",
    "prebuilt/third_party/gn/linux-arm64/gn",
    "prebuilt/third_party/gn/mac-x64/gn",
    "prebuilt/third_party/gn/mac-arm64/gn",
):
    if os.path.isfile(os.path.join(workdir, cand)):
        gn_bin = os.path.join(workdir, cand)
        break
if not gn_bin:
    gn_bin = shutil.which("gn")

findings = []
if gn_files and gn_bin:
    for rel in gn_files:
        try:
            res = subprocess.run(
                [gn_bin, "format", "--dry-run", rel],
                cwd=workdir, capture_output=True, text=True, timeout=60,
            )
        except Exception:
            continue
        if res.returncode == 2:
            findings.append({
                "source": "gn_format",
                "category": "gn_format",
                "severity": "error",
                "file": rel,
                "line": 1,
                "message": f"GN file '{rel}' is not formatted according to `gn format` (CQ static-checks gn_format fails).",
                "remediation": (
                    f"Run `prebuilt/third_party/gn/linux-x64/gn format {rel}` (or `fx format-code` from the "
                    "repository root) after every hand edit or bazel2gn run; for sorted label lists such as "
                    "verification .gni files, insert new entries in the position `gn format` expects."
                ),
            })
        elif res.returncode == 1:
            findings.append({
                "source": "gn_format",
                "category": "gn_syntax_error",
                "severity": "error",
                "file": rel,
                "line": 1,
                "message": f"`gn format` could not parse '{rel}': {(res.stderr or res.stdout).strip()[:300]}",
                "remediation": f"Fix the GN syntax error in '{rel}', then run `gn format {rel}`.",
            })

print(json.dumps(findings, indent=2))
PYEOF
