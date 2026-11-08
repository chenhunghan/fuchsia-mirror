#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Rejects Bazel build files added or modified under build/secondary/, Bazel
# dependencies on //build/secondary/... labels, bazel2gn target-map entries
# pointing into build/secondary/, and new MODULE.bazel repositories over
# current-layout third_party/<name>/src code.
#
# build/secondary/ is GN's secondary source tree: GN-only overlay BUILD.gn files
# for third-party code that ships without one. Bazel has no equivalent, so a
# BUILD.bazel there is not transparent to callers and requires extra label
# mappings. The fix is the current third-party layout: move the code to
# //third_party/<name>/src and put BUILD.gn + BUILD.bazel (plus README.fuchsia
# and LICENSE as required) in //third_party/<name>/.

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


changed = {l.strip() for l in git(["diff", "--name-only", "--diff-filter=AM", change_base]).splitlines() if l.strip()}
changed.update(l.strip() for l in git(["ls-files", "--others", "--exclude-standard"]).splitlines() if l.strip())

REMEDIATION = (
    "build/secondary/ is a GN-only overlay tree; do not add Bazel builds there or depend on "
    "//build/secondary/... labels from Bazel. Migrate the third-party code to the current layout "
    "instead, as a separate CL below the migration CL (see docs/development/source_code/"
    "third-party-management.md, 'Migrating legacy third-party code to current layout'): "
    "(1) in the manifest, move the project path to third_party/<name>/src and, if the fork only "
    "added build files, pin the pristine upstream revision; update .gitignore, and move the gitlink "
    "and .gitmodules entry in the same CL (git cannot hold files under a gitlink path); "
    "(2) add BUILD.gn, BUILD.bazel, OWNERS and README.fuchsia (URL, Revision, License, License File, "
    "Security Critical) under //third_party/<name>/, delete the check-licenses asset README and the "
    "policy exceptions that named the old paths; "
    "(3) if upstream ships its own BUILD/WORKSPACE in src/, src/ becomes a separate Bazel package and "
    "cross-package file labels fail, so expose it with new_local_repository(build_file = "
    "//third_party/<name>:<name>.BUILD.bazel) in build/bazel/toplevel.MODULE.bazel and alias it from "
    "//third_party/<name> so GN and Bazel share one label and no bazel2gn target-map entry is needed; "
    "(4) update includes to the new src/ path. If that move is out of scope, stop and report the "
    "blocker in the summary."
)

findings = []
for path in sorted(changed):
    base = os.path.basename(path)
    if path.startswith("build/secondary/") and (
        base in ("BUILD.bazel", "BUILD", "MODULE.bazel")
        or base.endswith((".bzl", ".BUILD.bazel"))
    ):
        findings.append({
            "source": "gn_secondary_tree",
            "category": "bazel_build_in_gn_secondary_tree",
            "severity": "error",
            "file": path,
            "line": 0,
            "message": f"Change adds or edits Bazel build file '{path}' inside GN's secondary source tree.",
            "remediation": REMEDIATION,
        })

for path in sorted(changed):
    if not path.endswith("third_party_target_map.json"):
        continue
    diff = git(["diff", "-U0", change_base, "--", path])
    if not diff and path in changed:
        try:
            diff = "".join("+" + l for l in open(os.path.join(workdir, path)))
        except OSError:
            diff = ""
    for line in diff.splitlines():
        if line.startswith("+") and not line.startswith("+++") and "build/secondary" in line:
            findings.append({
                "source": "gn_secondary_tree",
                "category": "target_map_points_into_gn_secondary_tree",
                "severity": "error",
                "file": path,
                "line": 0,
                "message": f"Added bazel2gn target-map entry references build/secondary: {line[1:].strip()}",
                "remediation": REMEDIATION,
            })

for path in sorted(changed):
    base = os.path.basename(path)
    if path.startswith("build/secondary/") or not (
        base in ("BUILD.bazel", "BUILD") or base.endswith((".bzl", ".BUILD.bazel"))
    ):
        continue
    diff = git(["diff", "-U0", change_base, "--", path])
    if not diff:
        try:
            diff = "".join("+" + l for l in open(os.path.join(workdir, path)))
        except OSError:
            diff = ""
    for line in diff.splitlines():
        if line.startswith("+") and not line.startswith("+++") and "//build/secondary/" in line:
            findings.append({
                "source": "gn_secondary_tree",
                "category": "bazel_dep_on_gn_secondary_tree",
                "severity": "error",
                "file": path,
                "line": 0,
                "message": f"Bazel file depends on a GN secondary-tree label: {line[1:].strip()}",
                "remediation": REMEDIATION,
            })

# Check new repositories over current-layout third-party code: a third_party/<name>/ directory
# whose upstream code lives in third_party/<name>/src is tracked in fuchsia.git, so its
# BUILD.bazel belongs next to its BUILD.gn rather than in a new MODULE.bazel repository.
# A repository is only justified when src/ ships its own Bazel package markers.
LAYOUT_REMEDIATION = (
    "This third-party directory already uses the current layout (upstream code in "
    "third_party/<name>/src, Fuchsia build files in third_party/<name>/, tracked in "
    "fuchsia.git). Do not add a (new_)local_repository for it in a MODULE.bazel file. "
    "Instead add third_party/<name>/BUILD.bazel (next to its BUILD.gn, mirroring it) with "
    "cc_library/filegroup targets over the files it needs, give them the needed visibility, and "
    "depend on //third_party/<name>:<target> from the migrated package. Remove the "
    "repository and any @<repo>// labels."
)
BAZEL_PKG_MARKERS = ("BUILD", "BUILD.bazel", "WORKSPACE", "WORKSPACE.bazel", "MODULE.bazel")
REPO_PATH_RE = re.compile(r'^\+\s*path\s*=\s*"(third_party/[^/"]+)(?:/src)?/?"')


def current_layout_third_party(rel):
    src = os.path.join(workdir, rel, "src")
    if not os.path.isdir(src):
        return False
    return not any(os.path.exists(os.path.join(src, m)) for m in BAZEL_PKG_MARKERS)


for path in sorted(changed):
    if not path.endswith("MODULE.bazel"):
        continue
    diff = git(["diff", "-U0", change_base, "--", path])
    for line in diff.splitlines():
        m = REPO_PATH_RE.match(line)
        if m and current_layout_third_party(m.group(1)):
            findings.append({
                "source": "gn_secondary_tree",
                "category": "repository_for_current_layout_third_party",
                "severity": "error",
                "file": path,
                "line": 0,
                "message": f"Added repository path '{m.group(1)}' points at current-layout "
                           "third-party code that can host its own BUILD.bazel.",
                "remediation": LAYOUT_REMEDIATION,
            })

print(json.dumps(findings, indent=2))
PYEOF
