#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -uo pipefail

# Deterministic check that actually builds the migration. Static checks and the
# review panel only read BUILD files; this is what proves the change compiles.
#
# Steps (each failing step becomes one ERROR finding with the relevant log lines):
#   1. gn_build:               `fx build` (incremental, the whole configured
#                              product), so the migrated GN targets and every GN
#                              dependent of them are rebuilt.
#   2. bazel2gn_verifications: `fx build --host //build:bazel2gn_verifications`,
#                              which fails if a BUILD.gn has drifted from what
#                              `fx bazel2gn` generates from BUILD.bazel.
#   3. bazel_build_fuchsia:    `fx bazel build --config=fuchsia_platform //<dir>:all`
#                              for every changed directory with a BUILD.bazel
#                              (host-only targets are skipped as incompatible).
#   4. bazel_build_host:       `fx bazel build --config=host //<dir>:all` for the
#                              directories whose BUILD.bazel declares host targets.
#
# Directories: $PLANTER_TARGET_DIRS plus every directory whose BUILD.bazel or
# BUILD.gn changed since $PLANTER_CHANGE_BASE (e.g. migrated dependencies).
# With several target directories planter runs checks once per directory; this
# check only builds on the run for the first one.
#
# Environment:
#   PLANTER_SKIP_BUILD=1          skip the builds (reports an INFO finding).
#   PLANTER_BUILD_TIMEOUT=<secs>  per-step timeout (default 5400).

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"

first_dir="${TARGET_DIRS%% *}"
if [[ -n "$TARGET_DIR" && -n "$first_dir" && "$TARGET_DIR" != "$first_dir" ]]; then
  echo "[]"
  exit 0
fi

python3 - "$WORKDIR" "$TARGET_DIRS" <<'PYEOF'
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
target_dirs = [d.strip().strip("/") for d in sys.argv[2].split() if d.strip().strip("/")]
change_base = os.environ.get("PLANTER_CHANGE_BASE", "").strip() or "HEAD"
timeout = int(os.environ.get("PLANTER_BUILD_TIMEOUT", "5400"))

if os.environ.get("PLANTER_SKIP_BUILD", "").strip() not in ("", "0"):
    print(json.dumps([{
        "source": "build_verification",
        "category": "build_skipped",
        "severity": "INFO",
        "message": "PLANTER_SKIP_BUILD is set; the migration was NOT built.",
    }], indent=2))
    sys.exit(0)


def git_lines(args):
    try:
        out = subprocess.check_output(["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True)
    except Exception:
        return []
    return [l.strip() for l in out.splitlines() if l.strip()]


dirs = list(target_dirs)
changed = git_lines(["diff", "--name-only", change_base]) + git_lines(["ls-files", "--others", "--exclude-standard"])
for p in changed:
    if os.path.basename(p) in ("BUILD.bazel", "BUILD.gn"):
        d = os.path.dirname(p).strip("/")
        if d and d not in dirs:
            dirs.append(d)

fx = None
for rel in (".jiri_root/bin/fx", "scripts/fx"):
    cand = os.path.join(workdir, rel)
    if os.path.isfile(cand):
        fx = cand
        break
if fx is None:
    print(json.dumps([{
        "source": "build_verification",
        "category": "build_setup",
        "severity": "ERROR",
        "message": f"Cannot find fx in {workdir} (.jiri_root/bin/fx or scripts/fx); the migration could not be built.",
    }], indent=2))
    sys.exit(0)

bazel_dirs = [d for d in dirs if os.path.isfile(os.path.join(workdir, d, "BUILD.bazel"))]
host_markers = re.compile(r"HOST_CONSTRAINTS|HOST_OS_CONSTRAINTS|_host_tool\b|host_go_test|with_host_unit_tests")
host_dirs = []
for d in bazel_dirs:
    try:
        with open(os.path.join(workdir, d, "BUILD.bazel")) as f:
            if host_markers.search(f.read()):
                host_dirs.append(d)
    except OSError:
        pass

steps = [
    ("gn_build", [fx, "build"]),
    ("bazel2gn_verifications", [fx, "build", "--host", "//build:bazel2gn_verifications"]),
]
if bazel_dirs:
    steps.append(("bazel_build_fuchsia", [fx, "bazel", "build", "--config=fuchsia_platform"] + [f"//{d}:all" for d in bazel_dirs]))
if host_dirs:
    steps.append(("bazel_build_host", [fx, "bazel", "build", "--config=host"] + [f"//{d}:all" for d in host_dirs]))

ERROR_LINE = re.compile(
    r"(^ERROR:|^FAILED:|\berror(\[E\d+\])?:|^error\b|: error\b|\bundefined reference\b|"
    r"no such (target|package)|is not visible|Unable to load|not declared|drift|differs|--- a/|\+\+\+ b/)",
    re.I,
)
LOCATION = re.compile(r"(?:-->\s*)?((?:\.\./\.\./)?[\w./+-]+\.(?:rs|cc|h|c|gn|gni|bazel|bzl)):(\d+)")


def summarize(output):
    lines = output.splitlines()
    picked = []
    for i, line in enumerate(lines):
        if ERROR_LINE.search(line):
            for j in range(max(0, i - 1), min(len(lines), i + 4)):
                if not picked or picked[-1] < j:
                    picked.append(j)
    text = "\n".join(lines[j] for j in picked) if picked else "\n".join(lines[-40:])
    if len(text) > 6000:
        text = text[:6000] + "\n... (truncated)"
    file, line = "", 0
    m = LOCATION.search(text)
    if m:
        file = m.group(1).replace("../../", "", 1)
        if file.startswith(workdir + "/"):
            file = file[len(workdir) + 1:]
        line = int(m.group(2))
    return text, file, line


REMEDIATION = {
    "gn_build": "Fix the BUILD.bazel/BUILD.gn so the GN build passes (renamed/removed labels that dependents still use, "
                "missing deps, attribute drift), then re-run `fx bazel2gn -d <dir>` and `fx build`. Never edit sources to make it pass.",
    "bazel2gn_verifications": "BUILD.gn no longer matches BUILD.bazel. Run `fx bazel2gn -d <dir>` for every dual-build directory "
                              "you touched (never hand-edit below the BAZEL2GN SENTINEL) and re-run `fx build --host //build:bazel2gn_verifications`.",
    "bazel_build_fuchsia": "Fix BUILD.bazel so `fx bazel build --config=fuchsia_platform //<dir>:all` passes. If a dependency has no "
                           "Bazel target yet, migrate that dependency's directory in this change (see the coder instructions).",
    "bazel_build_host": "Fix BUILD.bazel so `fx bazel build --config=host //<dir>:all` passes. If a dependency has no Bazel target "
                        "yet, migrate that dependency's directory in this change (see the coder instructions).",
}

findings = []
for name, cmd in steps:
    try:
        proc = subprocess.run(cmd, cwd=workdir, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=timeout)
        code, output = proc.returncode, proc.stdout
    except subprocess.TimeoutExpired as e:
        code = "timeout"
        output = (e.stdout or b"").decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
        output += f"\n(timed out after {timeout}s)"
    if code == 0:
        continue
    text, file, line = summarize(output)
    findings.append({
        "source": "build_verification",
        "category": f"build_failure_{name}",
        "severity": "ERROR",
        "file": file,
        "line": line,
        "message": f"`{' '.join(['fx'] + cmd[1:])}` failed ({code}):\n{text}",
        "remediation": REMEDIATION[name],
    })
    if name == "gn_build" and re.search(r"fx set|no build directory|Unable to find build directory", output, re.I):
        break  # The tree is not configured; the remaining steps would fail the same way.

print(json.dumps(findings, indent=2))
PYEOF
