#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Helper tool to discover true reverse dependencies of a target package directory
# and compute deterministic, tiered Bazel visibility lists per target using the
# shared `machinery/checks/rdeps_lib.py` implementation.

export PATH="${PATH:-}:${HOME:-}/.cargo/bin:/usr/local/bin:/usr/bin:/bin"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKS_DIR="$(cd "${SCRIPT_DIR}/../checks" && pwd)"

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${1:-${PLANTER_TARGET_DIR:-}}"

if [[ -z "$TARGET_DIR" ]]; then
  echo "Usage: find_rdeps.sh <target_package_dir>" >&2
  exit 1
fi

python3 - "$CHECKS_DIR" "$WORKDIR" "$TARGET_DIR" <<'PYEOF'
import ast
import json
import os
import sys

sys.path.insert(0, os.path.abspath(sys.argv[1]))
import planter_check_lib as pcl
import rdeps_lib as rl

workdir = os.path.abspath(sys.argv[2])
target_pkg = rl.normalize_target_pkg(sys.argv[3], workdir)
default_target = os.path.basename(target_pkg)
bazel_file = os.path.join(workdir, target_pkg, "BUILD.bazel")

bazel_targets = []
package_targets = set()
non_gn_synced_targets = set()
test_rule_targets = set()
testonly_targets = set()
idk_targets = set()
alias_map = {}

if os.path.isfile(bazel_file):
    try:
        with open(bazel_file, "r", encoding="utf-8") as f:
            bazel_src = f.read()
        tree = ast.parse(bazel_src, filename=bazel_file)
        parsed = rl.parse_package_bazel_targets(tree, bazel_src.splitlines())
        bazel_targets = parsed["bazel_targets"]
        package_targets = parsed["package_targets"]
        non_gn_synced_targets = parsed["non_gn_synced_targets"]
        test_rule_targets = parsed["test_rule_targets"]
        testonly_targets = parsed["testonly_targets"]
        idk_targets = parsed["idk_targets"]
        alias_map = parsed["alias_map"]
    except Exception:
        pass

orig_gn_vis_map = rl.get_orig_gn_visibility_scopes(workdir, target_pkg)
orig_gn_unrestricted_targets = rl.get_orig_gn_unrestricted_targets(workdir, target_pkg)
for _a_name, _a_actual in alias_map.items():
    if _a_actual in orig_gn_unrestricted_targets and _a_name not in orig_gn_vis_map:
        orig_gn_unrestricted_targets.add(_a_name)
excluded_gn_only_references = set()
env_target_dir = pcl.normalize_target_dir(os.environ.get("PLANTER_TARGET_DIR", ""), workdir)

(
    true_rdep_packages,
    per_target_rdeps,
    _per_target_direct_rdeps,
    visibility_only_packages,
    gn_only_ref_packages,
    _has_macro_injected_rdeps,
) = rl.compute_package_rdeps(
    workdir,
    target_pkg,
    bazel_targets,
    package_targets,
    non_gn_synced_targets,
    alias_map,
    orig_gn_vis_map=orig_gn_vis_map,
    test_rule_targets=test_rule_targets,
    idk_targets=idk_targets,
    target_dir=env_target_dir,
    excluded_gn_only_out=excluded_gn_only_references,
)

is_test_pkg = rl.is_test_support_package(target_pkg)

def rec_vis(rset, is_test_support=False, target_name=None):
    return rl.recommend_visibility(
        rset,
        pkg_path=target_pkg,
        is_test_support=is_test_support,
        target_name=target_name,
        orig_gn_vis_map=orig_gn_vis_map,
        vis_only_pkgs=visibility_only_packages,
        gn_only_pkgs=gn_only_ref_packages,
        non_gn_synced_targets=non_gn_synced_targets,
        workdir=workdir,
        orig_gn_unrestricted_targets=orig_gn_unrestricted_targets,
        idk_targets=idk_targets,
    )

print(json.dumps({
    "target_package": f"//{target_pkg}",
    "true_rdep_packages": sorted({f"//{rl.sanitize_rdep_pkg(p)}" for p in true_rdep_packages}),
    "distinct_rdep_areas": rl.get_distinct_areas(true_rdep_packages, target_pkg, with_slash_prefix=True),
    "is_globally_used": rl.is_globally_used_target(true_rdep_packages, target_pkg),
    "recommended_visibility": rec_vis(
        true_rdep_packages,
        is_test_support=is_test_pkg or any(t in testonly_targets for t in bazel_targets),
        target_name=default_target
        if (default_target in orig_gn_vis_map or default_target in orig_gn_unrestricted_targets)
        else None,
    ),
    "per_target_rdeps": {
        t: sorted({f"//{rl.sanitize_rdep_pkg(p)}" for p in rset})
        for t, rset in per_target_rdeps.items()
    },
    "per_target_is_globally_used": {
        t: rl.is_globally_used_target(rset, target_pkg)
        for t, rset in per_target_rdeps.items()
    },
    "per_target_recommended_visibility": {
        t: rec_vis(rset, is_test_support=is_test_pkg or t in testonly_targets, target_name=t)
        for t, rset in per_target_rdeps.items()
    },
    "per_target_area_rollup_visibility": {
        t: rl.recommend_area_rollup_visibility(rset, target_pkg)
        for t, rset in per_target_rdeps.items()
    },
    "per_target_narrow_visibility": {
        t: rl.recommend_narrow_visibility(rset, target_pkg)
        for t, rset in per_target_rdeps.items()
    },
    "excluded_visibility_allowlist_packages": sorted(
        {f"//{rl.sanitize_rdep_pkg(p)}" for p in visibility_only_packages}
    ),
    "excluded_gn_only_or_verification_references": sorted(excluded_gn_only_references),
}, indent=2))
PYEOF
