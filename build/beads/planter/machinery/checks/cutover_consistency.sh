#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -uo pipefail

# Compares a Bazel package with the GN package it replaces (package cutover).
#
# A thin wrapper over //build/beads/verifications/verify_package_cutover.py in
# the target checkout. It prints `[]` unless the change cuts a package over
# from GN to Bazel: an added Bazel package with the package name of a GN
# package at the change base (paired by target name, a
# bridge such as `bazel_fuchsia_package`, or a unique package name), or a
# `bazel_inputs/...` label in //bundles/assembly/BUILD.bazel replaced with a
# direct label. A package that can't be paired gets cutover_info_ambiguous.
# A GN package at the change base that the change deletes or bridges without
# such a pair gets cutover_info_not_compared.
#
# Phases (PLANTER_CUTOVER_PHASE=parallel|final, else detected):
#   parallel  GN and Bazel packages both exist: build both, compare them
#             (package content comparison on the build dir's CPU; build
#             command comparison on x64 and arm64), and write a phase-1
#             record with both packages' facts and commands.
#   final     The GN package is gone: rebuild the Bazel package and compare it
#             with the recorded GN package again, noting whether it changed
#             since phase 1. Records from before version 2 have no GN facts:
#             re-emit their findings and check that the Bazel meta.far merkle
#             still matches phase 1 (guard).
#
# Records live in PLANTER_CUTOVER_RECORD_DIR (an absolute path) when it is
# set (Planter keeps that directory per task and restores it before each
# run), otherwise in $(git rev-parse --git-common-dir)/planter-cutover/, as
# `<package_name>--<Bazel target>.json`. A record found only in the git
# common dir for this checkout, or under the older `<package_name>.json`
# name, is copied there.
#
# PLANTER_CUTOVER_PACKAGE_ARCH=arm64 builds and compares the packages for
# arm64 instead, in the reusable out dir `<build dir>.cutover-arm64`.
#
# Categories (severity from cutover_categories.json: stage 2 is WARNING,
# otherwise INFO): cutover_diff_<kind> (a difference the uploader
# approves in Planter's cutover review, separately from disputes; matched by
# its key and `fingerprint`), cutover_summary (one per cutover),
# cutover_info_<kind> (coverage and status notes). Every cutover that was not
# compared gets cutover_info_phase1_missing or cutover_info_not_compared.
#
# This check builds, so it is in SERIAL_BUILD_CHECKS in tools/run_checks.sh
# and takes the workdir build lock. PLANTER_SKIP_BUILD=1 reuses existing
# outputs. With several target directories it only runs for the first one.

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"

norm_target_dir="${TARGET_DIR#//}"
norm_target_dir="${norm_target_dir#./}"
norm_target_dir="${norm_target_dir%/}"
first_dir="${TARGET_DIRS%%[ ,]*}"
first_dir="${first_dir#//}"
first_dir="${first_dir#./}"
first_dir="${first_dir%/}"
if [[ -n "$norm_target_dir" && -n "$first_dir" && "$norm_target_dir" != "$first_dir" ]]; then
  echo "[]"
  exit 0
fi

WORKDIR="$(cd "$WORKDIR" && pwd)"
CLI="$WORKDIR/build/beads/verifications/verify_package_cutover.py"

if [[ ! -f "$CLI" ]]; then
  # Without the CLI the cutover cannot be detected precisely. Stay silent
  # unless the change touches a BUILD.bazel file at all.
  python3 - "$WORKDIR" <<'PYEOF'
import json, os, subprocess, sys
workdir = sys.argv[1]
base = os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip() or "HEAD"
changed = os.environ.get("PLANTER_CHANGED_FILES", "")
if not changed.strip():
    try:
        changed = subprocess.check_output(
            ["git", "-C", workdir, "diff", "--name-only", base],
            stderr=subprocess.DEVNULL, text=True)
    except Exception:
        changed = ""
if not any(os.path.basename(p.strip()) == "BUILD.bazel" for p in changed.splitlines()):
    print("[]")
    sys.exit(0)
print(json.dumps([{
    "source": "cutover_consistency",
    "category": "cutover_info_tool_unavailable",
    "severity": "INFO",
    "file": "build/beads/verifications/verify_package_cutover.py",
    "line": 0,
    "message": "[verify_package_cutover.py] The target checkout has no package "
               "cutover CLI, so package cutovers were not checked. Rebase onto a "
               "revision that has //build/beads/verifications/verify_package_cutover.py.",
    "remediation": "",
}], indent=2))
PYEOF
  exit 0
fi

# The CLI imports in-tree build modules that need the vendored Python. It
# runs in a hermetic environment, so pass the Planter variables as flags.
PYTHON="$WORKDIR/scripts/fuchsia-vendored-python"
[[ -x "$PYTHON" ]] || PYTHON=python3
args=(--workdir "$WORKDIR" --json)
base="${PLANTER_RESOLVED_CHANGE_BASE:-${PLANTER_CHANGE_BASE:-}}"
[[ -n "$base" ]] && args+=(--base "$base")
case "${PLANTER_CUTOVER_PHASE:-}" in
  parallel | final) args+=(--phase "$PLANTER_CUTOVER_PHASE") ;;
esac
[[ -n "${PLANTER_SKIP_BUILD:-}" && "${PLANTER_SKIP_BUILD}" != "0" ]] && args+=(--no-build)
[[ "${PLANTER_CUTOVER_PACKAGE_ARCH:-}" == "arm64" ]] && args+=(--package-arch arm64)
record_dir="${PLANTER_CUTOVER_RECORD_DIR:-}"
if [[ -n "${record_dir//[[:space:]]/}" ]] && grep -q -- '--record-dir' "$CLI"; then
  [[ "$record_dir" == /* ]] || record_dir="$PWD/$record_dir"
  args+=(--record-dir "$record_dir")
fi
changed_file=""
if [[ -n "${PLANTER_CHANGED_FILES:-}" ]]; then
  changed_file="$(mktemp)"
  trap 'rm -f "$changed_file"' EXIT
  printf '%s\n' "$PLANTER_CHANGED_FILES" > "$changed_file"
  args+=(--changed-files "$changed_file")
fi
cd "$WORKDIR" && "$PYTHON" "$CLI" "${args[@]}"
