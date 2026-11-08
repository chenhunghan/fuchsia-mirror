#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check auditing BUILD.bazel files for visibility scoping and rdeps anti-patterns.
# Uses shared rdep discovery and visibility recommendation logic in `rdeps_lib.py` and
# shared check helpers in `planter_check_lib.py`.

export PATH="${PATH:-}:${HOME:-}/.cargo/bin:/usr/local/bin:/usr/bin:/bin"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"

python3 - "$SCRIPT_DIR" "$WORKDIR" "$TARGET_DIR" "$TARGET_DIRS" <<'PYEOF'
import ast
import json
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.abspath(sys.argv[1]))
import planter_check_lib as pcl
import rdeps_lib as rl

workdir = os.path.abspath(sys.argv[2])
target_dir = rl.normalize_target_dir(sys.argv[3], workdir)
target_dirs = [
    rl.normalize_target_dir(d, workdir)
    for d in re.split(r"[\s,]+", f"{sys.argv[4]} {target_dir}")
    if rl.normalize_target_dir(d, workdir)
]
target_bazel_files = {os.path.join(d, "BUILD.bazel") for d in target_dirs}

candidate_files = {
    rel_bazel
    for rel_bazel in sorted(target_bazel_files)
    if not rel_bazel.startswith(rl.EXCLUDED_GLOBAL_VIS_PREFIXES)
    and os.path.isfile(os.path.join(workdir, rel_bazel))
}

# Check whether a changed BUILD.bazel outside target_dirs is a @bazel2gn:skip bridge stub.
def is_bridge_stub_bazel_file(workdir, rel_bazel, changed_set, target_bazel_files):
    if rel_bazel in target_bazel_files:
        return False
    pkg_dir = os.path.dirname(rel_bazel).strip("/")
    if not pkg_dir or f"{pkg_dir}/BUILD.gn" in changed_set:
        return False
    full_bz = os.path.join(workdir, rel_bazel)
    try:
        with open(full_bz, encoding="utf-8") as fh:
            bz_src = fh.read()
        tree = ast.parse(bz_src, filename=rel_bazel)
    except Exception:
        return False
    src_lines = bz_src.splitlines()
    saw_entry = False
    for stmt in tree.body:
        if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
            continue
        fn = (
            stmt.value.func.id
            if isinstance(stmt.value.func, ast.Name)
            else (stmt.value.func.attr if isinstance(stmt.value.func, ast.Attribute) else "")
        )
        if fn in ("load", "package", "licenses", "visibility"):
            continue
        saw_entry = True
        if fn == "exports_files":
            continue
        if not pcl.has_bazel2gn_skip_comment(src_lines, stmt.value.lineno):
            return False
    return saw_entry


change_base = pcl.resolve_change_base(workdir)
changed_set = set(pcl.changed_files(workdir, change_base))
for line in sorted(changed_set):
    if (
        line.endswith("BUILD.bazel")
        and not line.startswith(rl.EXCLUDED_GLOBAL_VIS_PREFIXES)
        and os.path.isfile(os.path.join(workdir, line))
        and not is_bridge_stub_bazel_file(
            workdir, line, changed_set, target_bazel_files
        )
    ):
        candidate_files.add(line)

findings = []
clean_audited_targets = set()
gni_importer_cache = {}
gni_grep_cache = {}
bzl_macro_cache = {}
ancestor_suite_cache = {}
cml_shard_caller_cache = {}


# Discover caller packages whose .cml manifests include a cml_shard file from pkg_path.
def discover_cml_shard_rdeps(node, target_name, pkg_path):
    shard_rel = ""
    for kw in node.keywords:
        if (
            kw.arg == "shard"
            and isinstance(kw.value, ast.Constant)
            and isinstance(kw.value.value, str)
        ):
            shard_rel = kw.value.value.strip().lstrip(":")
            break
    if not shard_rel:
        shard_rel = (
            target_name if target_name.endswith(".cml") else f"{target_name}.cml"
        )
    cache_key = (pkg_path, shard_rel)
    if cache_key in cml_shard_caller_cache:
        return cml_shard_caller_cache[cache_key]
    inc_label = (
        f"//{pkg_path}/{shard_rel.lstrip('/')}"
        if pkg_path
        else f"//{shard_rel.lstrip('/')}"
    )
    callers = set()
    try:
        proc = subprocess.run(
            [
                "git",
                "-C",
                workdir,
                "grep",
                "--untracked",
                "-l",
                "--fixed-strings",
                "-e",
                inc_label,
                "--",
                "*.cml",
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=15,
            check=False,
        )
        for raw_line in proc.stdout.splitlines():
            cml_rel = raw_line.strip().strip("/")
            if not cml_rel:
                continue
            cur_dir = os.path.dirname(cml_rel)
            while cur_dir:
                if os.path.isfile(
                    os.path.join(workdir, cur_dir, "BUILD.bazel")
                ) or os.path.isfile(
                    os.path.join(workdir, cur_dir, "BUILD.gn")
                ):
                    if cur_dir != pkg_path:
                        callers.add(cur_dir)
                    break
                cur_dir = os.path.dirname(cur_dir)
    except Exception:
        pass
    cml_shard_caller_cache[cache_key] = callers
    return callers


def emit_finding(category, severity, file_rel, line_no, message, remediation):
    pcl.emit(
        findings,
        category=category,
        severity=severity,
        file=file_rel,
        line=line_no,
        message=message,
        remediation=remediation,
        source="visibility_audit",
    )

# Check whether `<pkg_dir>/BUILD.gn` still depends on the crate guarded by `group_name`.
def gn_pkg_uses_restricted_rust_crate(pkg_dir, group_name):
    gn_file = os.path.join(workdir, pkg_dir, "BUILD.gn")
    if not os.path.isfile(gn_file):
        return False
    try:
        with open(gn_file, "r", encoding="utf-8") as f:
            gn_clean = pcl.strip_comments(f.read())
    except Exception:
        return False
    prefix = group_name[: -len("_visibility")].replace("_", "[-_]")
    if prefix == "md5":
        prefix = "md[-_]?5"
    crate_re = re.compile(
        r'"//third_party/rust_crates(?:/vendor)?:' + prefix + r'(?:"|\()'
    )
    return bool(crate_re.search(gn_clean))


# Parse restricted crate aliases in `third_party/rust_crates/vendor/BUILD.bazel` and
# their `*_visibility` package_groups in `third_party/rust_crates/BUILD.bazel`.
def load_restricted_rust_crate_allowlists():
    rc_bz_path = os.path.join(workdir, "third_party/rust_crates/BUILD.bazel")
    vendor_bz_path = os.path.join(
        workdir, "third_party/rust_crates/vendor/BUILD.bazel"
    )
    if not os.path.isfile(rc_bz_path) or not os.path.isfile(vendor_bz_path):
        return {}, {}
    try:
        with open(rc_bz_path, "r", encoding="utf-8") as f:
            rc_tree = ast.parse(
                f.read(), filename="third_party/rust_crates/BUILD.bazel"
            )
        with open(vendor_bz_path, "r", encoding="utf-8") as f:
            vendor_tree = ast.parse(
                f.read(), filename="third_party/rust_crates/vendor/BUILD.bazel"
            )
    except Exception:
        return {}, {}

    pkg_groups = {}
    for node in ast.walk(rc_tree):
        if not isinstance(node, ast.Call):
            continue
        fn = node.func.id if isinstance(node.func, ast.Name) else ""
        if fn != "package_group":
            continue
        gname = None
        exact = set()
        subtrees = set()
        for kw in node.keywords:
            if (
                kw.arg == "name"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                gname = kw.value.value
            elif kw.arg == "packages" and isinstance(kw.value, ast.List):
                for elt in kw.value.elts:
                    if not (
                        isinstance(elt, ast.Constant)
                        and isinstance(elt.value, str)
                        and elt.value.startswith("//")
                    ):
                        continue
                    raw_pkg = elt.value
                    if raw_pkg == "//...":
                        subtrees.add("")
                    elif raw_pkg.endswith("/..."):
                        subtrees.add(raw_pkg[2:-4].strip("/"))
                    else:
                        exact.add(raw_pkg[2:].strip("/"))
        if gname and gname.endswith("_visibility"):
            pkg_groups[gname] = (exact, subtrees)

    restricted_labels = {}
    vis_group_re = re.compile(
        r"^//third_party/rust_crates:([A-Za-z0-9_]+_visibility)$"
    )
    for node in ast.walk(vendor_tree):
        if not isinstance(node, ast.Call):
            continue
        fn = node.func.id if isinstance(node.func, ast.Name) else ""
        if fn != "alias":
            continue
        alias_name = None
        actual_lbl = None
        group_name = None
        for kw in node.keywords:
            if (
                kw.arg == "name"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                alias_name = kw.value.value
            elif (
                kw.arg == "actual"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                actual_lbl = kw.value.value
            elif kw.arg == "visibility" and isinstance(kw.value, ast.List):
                for elt in kw.value.elts:
                    if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                        m_vg = vis_group_re.match(elt.value)
                        if m_vg:
                            group_name = m_vg.group(1)
                            break
        if alias_name and group_name and group_name in pkg_groups:
            restricted_labels[
                f"//third_party/rust_crates/vendor:{alias_name}"
            ] = group_name
            if actual_lbl and actual_lbl.startswith("//"):
                restricted_labels[actual_lbl] = group_name
    return restricted_labels, pkg_groups


# Verify that candidate BUILD.bazel packages depending on restricted third-party Rust
# crates are listed in the corresponding `*_visibility` package_group in
# `third_party/rust_crates/BUILD.bazel`.
def check_missing_rust_crates_visibility(bazel_files):
    if not bazel_files:
        return
    restricted_labels, pkg_groups = load_restricted_rust_crate_allowlists()
    if not restricted_labels or not pkg_groups:
        return
    for rel_bz in sorted(bazel_files):
        pkg_dir = os.path.dirname(rel_bz).strip("/")
        if not pkg_dir or rel_bz.startswith("third_party/rust_crates/"):
            continue
        full_bz = os.path.join(workdir, rel_bz)
        try:
            with open(full_bz, "r", encoding="utf-8") as f:
                bz_src = f.read()
            bz_tree = ast.parse(bz_src, filename=rel_bz)
        except Exception:
            continue
        var_table = {}
        for stmt in bz_tree.body:
            if isinstance(stmt, ast.Assign):
                for tgt in stmt.targets:
                    if isinstance(tgt, ast.Name):
                        var_table[tgt.id] = stmt.value
        seen_groups = set()
        for node in ast.walk(bz_tree):
            if not isinstance(node, ast.Call):
                continue
            tname = None
            for kw in node.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    tname = kw.value.value
                    break
            if not tname:
                continue
            for kw in node.keywords:
                if not kw.arg or kw.arg in ("name", "visibility"):
                    continue
                for raw_lbl in pcl._extract_ast_strings(kw.value, var_table):
                    norm_lbl = re.sub(r"^@@?//", "//", raw_lbl.strip())
                    if norm_lbl.startswith("//") and ":" not in norm_lbl:
                        norm_lbl = f"{norm_lbl}:{norm_lbl.rsplit('/', 1)[-1]}"
                    group_name = restricted_labels.get(norm_lbl)
                    if not group_name or group_name in seen_groups:
                        continue
                    exact_pkgs, subtree_prefixes = pkg_groups[group_name]
                    is_allowed = pkg_dir in exact_pkgs or any(
                        pref == ""
                        or pkg_dir == pref
                        or pkg_dir.startswith(pref + "/")
                        for pref in subtree_prefixes
                    )
                    if is_allowed:
                        continue
                    seen_groups.add(group_name)
                    uses_in_gn = gn_pkg_uses_restricted_rust_crate(
                        pkg_dir, group_name
                    )
                    line_no = getattr(kw, "lineno", getattr(node, "lineno", 1))
                    msg = (
                        f"Target '{tname}' depends on restricted third-party Rust "
                        f"crate '{norm_lbl}', which requires visibility group "
                        f"'//third_party/rust_crates:{group_name}', but "
                        f"'//{pkg_dir}' is missing from `{group_name}` in "
                        "`third_party/rust_crates/BUILD.bazel`."
                    )
                    if uses_in_gn:
                        rem = (
                            f'Add `"//{pkg_dir}"` (sorted alphabetically) to '
                            f'`package_group(name = "{group_name}", ...)` in '
                            "`third_party/rust_crates/BUILD.bazel`. Keep "
                            f'`"//{pkg_dir}:*"` in `{group_name}` in '
                            "`third_party/rust_crates/visibility.gni` while GN "
                            f"targets in `{pkg_dir}/BUILD.gn` still depend on the "
                            "crate, never add a `NO_IFTTT` line to the commit "
                            "message or `CoderReport.summary`, and explain in the "
                            "commit body that the GN allowlist entry is still "
                            "required by `BUILD.gn`."
                        )
                    else:
                        rem = (
                            f'Add `"//{pkg_dir}"` (sorted alphabetically) to '
                            f'`package_group(name = "{group_name}", ...)` in '
                            "`third_party/rust_crates/BUILD.bazel`, and remove "
                            f'`"//{pkg_dir}:*"` from `{group_name}` in '
                            "`third_party/rust_crates/visibility.gni` if present "
                            "(paired via `LINT.ThenChange`)."
                        )
                    emit_finding(
                        "missing_rust_crates_visibility",
                        "error",
                        rel_bz,
                        line_no,
                        msg,
                        rem,
                    )


# Check third_party/rust_crates/BUILD.bazel and visibility.gni allowlist sync.
def check_rust_crates_visibility_sync(changed_paths):
    rc_bz_rel = "third_party/rust_crates/BUILD.bazel"
    rc_gni_rel = "third_party/rust_crates/visibility.gni"
    if rc_bz_rel not in changed_paths and rc_gni_rel not in changed_paths:
        return
    rc_bz_path = os.path.join(workdir, rc_bz_rel)
    rc_gni_path = os.path.join(workdir, rc_gni_rel)
    if not os.path.isfile(rc_bz_path) or not os.path.isfile(rc_gni_path):
        return
    try:
        with open(rc_bz_path, "r", encoding="utf-8") as f:
            bz_src = f.read()
        bz_tree = ast.parse(bz_src, filename=rc_bz_rel)
        with open(rc_gni_path, "r", encoding="utf-8") as f:
            gni_src = f.read()
    except Exception:
        return

    base_bz_src = pcl._git_out(workdir, ["show", f"{change_base}:{rc_bz_rel}"])
    try:
        base_bz_tree = ast.parse(base_bz_src) if base_bz_src else None
    except Exception:
        base_bz_tree = None

    def parse_pkg_groups(tree):
        groups = {}
        if not tree:
            return groups
        for node in ast.walk(tree):
            if not isinstance(node, ast.Call):
                continue
            fn = node.func.id if isinstance(node.func, ast.Name) else ""
            if fn != "package_group":
                continue
            gname = None
            pkgs = {}
            for kw in node.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    gname = kw.value.value
                elif kw.arg == "packages" and isinstance(kw.value, ast.List):
                    for elt in kw.value.elts:
                        if (
                            isinstance(elt, ast.Constant)
                            and isinstance(elt.value, str)
                            and elt.value.startswith("//")
                            and not elt.value.endswith("/...")
                        ):
                            pkgs[elt.value[2:].strip("/")] = elt.lineno
            if gname and gname.endswith("_visibility"):
                groups[gname] = pkgs
        return groups

    def parse_gni_vis_lists(text):
        lists = {}
        clean = pcl.blank_comments(text)
        pat = re.compile(r"(?m)^\s*([A-Za-z0-9_]+_visibility)\s*=\s*\[([^\]]*)\]")
        for m in pat.finditer(clean):
            vname = m.group(1)
            body = m.group(2)
            base_line = clean.count("\n", 0, m.start(2)) + 1
            entries = {}
            for sm in re.finditer(r'"//([^":*]+):?\*"', body):
                pkg = sm.group(1).strip("/")
                entries[pkg] = base_line + body.count("\n", 0, sm.start())
            lists[vname] = entries
        return lists

    cur_bz_groups = parse_pkg_groups(bz_tree)
    base_bz_groups = parse_pkg_groups(base_bz_tree)
    cur_gni_lists = parse_gni_vis_lists(gni_src)
    base_gni_src = pcl._git_out(workdir, ["show", f"{change_base}:{rc_gni_rel}"])
    base_gni_lists = parse_gni_vis_lists(base_gni_src) if base_gni_src else {}

    then_change_line = len(bz_src.splitlines())
    for idx, line_txt in enumerate(bz_src.splitlines(), start=1):
        if "LINT.ThenChange" in line_txt and "visibility.gni" in line_txt:
            then_change_line = idx
            break

    dual_build_added = []
    for gname, cur_pkgs in sorted(cur_bz_groups.items()):
        base_pkgs = base_bz_groups.get(gname, {})
        gni_pkgs = cur_gni_lists.get(gname, {})
        for pkg, _ in sorted(cur_pkgs.items()):
            if pkg in base_pkgs:
                continue
            uses_in_gn = gn_pkg_uses_restricted_rust_crate(pkg, gname)
            if not uses_in_gn and pkg in gni_pkgs:
                emit_finding(
                    "stale_rust_crates_gn_visibility",
                    "error",
                    rc_gni_rel,
                    gni_pkgs[pkg],
                    (
                        f"`{rc_bz_rel}` adds `//{pkg}` to `{gname}`, and no target in "
                        f"`{pkg}/BUILD.gn` depends on this restricted crate in GN, but "
                        f"`//{pkg}:*` still remains in `{gname}` in `{rc_gni_rel}` "
                        f"(violating `LINT.ThenChange(//{rc_gni_rel})`)."
                    ),
                    (
                        f"Remove `\"//{pkg}:*\"` from `{gname}` in `{rc_gni_rel}` now "
                        f"that no GN targets in `//{pkg}` use the crate."
                    ),
                )
            elif uses_in_gn and pkg in gni_pkgs:
                dual_build_added.append((gname, pkg))

    for gname, base_gni_pkgs in sorted(base_gni_lists.items()):
        cur_gni_pkgs = cur_gni_lists.get(gname, {})
        for pkg in sorted(base_gni_pkgs):
            if pkg not in cur_gni_pkgs and gn_pkg_uses_restricted_rust_crate(
                pkg, gname
            ):
                emit_finding(
                    "removed_active_rust_crates_gn_visibility",
                    "error",
                    rc_gni_rel,
                    1,
                    (
                        f"`//{pkg}:*` was removed from `{gname}` in `{rc_gni_rel}`, "
                        f"but `{pkg}/BUILD.gn` still has GN target(s) depending on "
                        "that restricted third-party crate."
                    ),
                    (
                        f"Restore `\"//{pkg}:*\"` in `{gname}` in `{rc_gni_rel}` "
                        f"while GN targets in `{pkg}/BUILD.gn` still depend on the "
                        "crate."
                    ),
                )

    if dual_build_added and rc_gni_rel not in changed_paths:
        desc = ", ".join(f"`//{p}` in `{g}`" for g, p in dual_build_added)
        emit_finding(
            "rust_crates_dual_build_ifttt",
            "info",
            rc_bz_rel,
            then_change_line,
            (
                f"`{rc_bz_rel}` adds {desc} while `{dual_build_added[0][1]}/BUILD.gn` "
                f"still depends on the crate in GN, so `{rc_gni_rel}` already lists "
                "the package and must remain unchanged despite "
                f"`LINT.ThenChange(//{rc_gni_rel})`."
            ),
            (
                f"Keep `{rc_gni_rel}` unchanged while GN targets in `BUILD.gn` still "
                "use the crate, and never add a `NO_IFTTT` line to the commit message "
                "or `CoderReport.summary` (only a human reviewer may add `NO_IFTTT` "
                "after reviewing the IFTTT warning). Explain in the commit body and "
                "`CoderReport.summary` that the GN allowlist entry in "
                f"`{rc_gni_rel}` is still required by `BUILD.gn`."
            ),
        )

check_rust_crates_visibility_sync(changed_set)
check_missing_rust_crates_visibility(candidate_files)

if not candidate_files:
    pcl.emit_json(findings)
    sys.exit(0)

origin_main_sources = {}
pkg_paths = sorted({os.path.dirname(p).strip("/") for p in candidate_files})
matched_files = rl.discover_referencing_build_files(
    workdir,
    pkg_paths,
    candidate_files=candidate_files,
    change_base=change_base,
    origin_main_sources=origin_main_sources,
    refresh_origin_main=len(pkg_paths) <= 10,
)
file_cache = rl.build_file_cache(matched_files, origin_main_sources)

def check_visibility_tightened_from_gn(
    target_name, rel_path, lineno, vis_vals, vis_scopes, preserved_gn_vis, pkg_path
):
    missing_preserved = []
    for pval in preserved_gn_vis:
        pm = re.match(r"^//([^:]+):__(pkg|subpackages)__$", pval)
        if not pm:
            continue
        psp, pkind = pm.group(1).strip("/"), pm.group(2)
        if pkind == "pkg":
            cov = any(
                psp == sp or (k == "subpackages" and (sp == "" or psp.startswith(sp + "/")))
                for sp, k in vis_scopes
            )
        else:
            cov = any(
                k == "subpackages" and (sp == "" or psp == sp or psp.startswith(sp + "/"))
                for sp, k in vis_scopes
            )
        if not cov:
            missing_preserved.append(pval)
    if missing_preserved:
        emit_finding(
            "visibility_tightened_from_gn",
            "error",
            rel_path,
            lineno,
            (
                f"Target '{target_name}' tightens visibility to {json.dumps(vis_vals)} "
                f"from the explicit narrow visibility in `{pkg_path}/BUILD.gn` "
                f"({json.dumps(preserved_gn_vis)}). Stick with the existing narrow "
                "visibility decision even if the target is not currently used outside "
                "a narrower scope."
            ),
            (
                f"Set visibility = {json.dumps(preserved_gn_vis)} on '{target_name}' "
                "to preserve the original narrow GN visibility scope, and re-run "
                "fx bazel2gn."
            ),
        )

def record_intentional_visibility_narrowing(
    target_name, rel_path, target_lineno, vis_scopes, orig_gn_vis_map, pkg_path
):
    gn_scopes = orig_gn_vis_map.get(target_name)
    if not gn_scopes:
        return
    uncovered_gn = []
    suggested_bz = []
    for gsp, gk in gn_scopes:
        if gsp == pkg_path and gk == "pkg":
            continue
        if gk == "pkg":
            cov = any(
                gsp == sp or (k == "subpackages" and (sp == "" or gsp.startswith(sp + "/")))
                for sp, k in vis_scopes
            )
            gn_pat, bz_pat = f"//{gsp}:*", f"//{gsp}:__pkg__"
        else:
            cov = any(
                k == "subpackages" and (sp == "" or gsp == sp or gsp.startswith(sp + "/"))
                for sp, k in vis_scopes
            )
            gn_pat = "//*" if not gsp else f"//{gsp}/*"
            bz_pat = "//:__subpackages__" if not gsp else f"//{gsp}:__subpackages__"
        if not cov and gn_pat not in uncovered_gn:
            uncovered_gn.append(gn_pat)
            suggested_bz.append(bz_pat)
    if not uncovered_gn:
        return
    emit_finding(
        "visibility_narrowed",
        "info",
        rel_path,
        target_lineno,
        (
            f'Target "{target_name}": GN visibility [{", ".join(uncovered_gn)}] is not '
            "covered by Bazel visibility (GN `/*` also grants subpackages, unlike `:*`; "
            "only the target's own package is implicit in Bazel); add "
            f'[{", ".join(suggested_bz)}] or confirm the narrowing is intended'
        ),
        (
            "Intentional visibility narrowing verified by visibility_audit "
            "(all actual reverse dependencies and preserved narrow GN scopes are covered)."
        ),
    )

def record_diff_visibility_narrowed_info(clean_targets):
    if not clean_targets or os.environ.get("PLANTER_SKIP_DIFF_COVERAGE") == "1":
        return
    diff_list = pcl.read_review_diff_findings()
    if not diff_list and "PLANTER_REVIEW_DIFF_JSON" not in os.environ:
        planter_bin = rl.find_planter_bin()
        if not planter_bin:
            return
        env = dict(os.environ)
        env["PLANTER_BIN"] = planter_bin
        env["PLANTER_SKIP_DIFF_COVERAGE"] = "1"
        try:
            out = subprocess.run(
                [planter_bin, "diff", "--local", "--check-findings", f"--workdir={workdir}"],
                cwd=workdir,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
                timeout=15,
            ).stdout
            start, end = out.find("["), out.rfind("]")
            if start >= 0 and end > start:
                parsed = json.loads(out[start : end + 1])
                if isinstance(parsed, list):
                    diff_list = parsed
        except Exception:
            return

    reason = (
        "Intentional visibility narrowing verified by visibility_audit "
        "(all actual reverse dependencies and preserved narrow GN scopes are covered)."
    )
    by_key = {
        (f.get("category"), f.get("file"), f.get("line"), f.get("message")): f
        for f in findings
    }
    for df in diff_list:
        if not isinstance(df, dict) or df.get("category") != "visibility_narrowed":
            continue
        f_rel = df.get("file", "")
        msg = df.get("message", "")
        ln = df.get("line", 0)
        tm = re.match(r'^Target "([^"]+)"', msg)
        tname = tm.group(1) if tm else None
        if not tname or (f_rel, tname) not in clean_targets:
            continue
        key = ("visibility_narrowed", f_rel, ln, msg)
        if key in by_key:
            # record_intentional_visibility_narrowing already reported this target; waive the
            # review_diff finding through that entry instead of emitting a duplicate.
            by_key[key]["waiver"] = reason
            continue
        entry = pcl.emit(
            findings,
            category="visibility_narrowed",
            severity="info",
            file=f_rel,
            line=ln,
            message=msg,
            remediation=reason,
            source="visibility_audit",
        )
        # An explicit waiver: planter's upload gate drops the review_diff visibility_narrowed
        # finding with this key (a same-key INFO finding alone no longer suppresses it).
        entry["waiver"] = reason
        by_key[key] = entry

# Collect (scope_pkg, kind) pairs declared in exports_files(..., visibility = [...]) calls.
def collect_exports_files_vis_scopes(tree, var_table, pkg_path):
    scopes = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        fn = (
            node.func.id
            if isinstance(node.func, ast.Name)
            else (node.func.attr if isinstance(node.func, ast.Attribute) else "")
        )
        if fn != "exports_files":
            continue
        vis_node = next((kw.value for kw in node.keywords if kw.arg == "visibility"), None)
        if vis_node is None and len(node.args) >= 2:
            vis_node = node.args[1]
        if vis_node is None:
            continue
        entries = rl.extract_list_entries(vis_node, var_table, node.lineno) or []
        for val, _ in entries:
            m_sc = re.match(r"^(?://([^:]*))?:__(pkg|subpackages)__$", val)
            if m_sc:
                sp = pkg_path if m_sc.group(1) is None else m_sc.group(1).strip("/")
                scopes.append((sp, m_sc.group(2)))
    return scopes


# Collect rule target names that already had an explicit narrow visibility list at change_base.
def collect_preexisting_narrow_vis_targets(workdir, rel_path, change_base):
    wt_diff = False
    try:
        wt_out = subprocess.check_output(
            [
                "git",
                "-C",
                workdir,
                "diff",
                "--name-only",
                "HEAD",
                "--",
                rel_path,
            ],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
        wt_diff = bool(wt_out)
    except Exception:
        pass
    refs = (
        ["HEAD~1"]
        if (change_base == "HEAD" and not wt_diff)
        else [change_base, "HEAD~1"]
    )
    for ref in dict.fromkeys(refs):
        try:
            prev_src = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{ref}:{rel_path}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
            prev_tree = ast.parse(prev_src, filename=rel_path)
        except Exception:
            continue
        prev_var_table = rl.extract_bazel_var_table(prev_tree)
        narrow_targets = set()
        for node in ast.walk(prev_tree):
            if not isinstance(node, ast.Call):
                continue
            tname = None
            vis_vals = []
            has_vis = False
            for kw in node.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    tname = kw.value.value
                elif kw.arg == "visibility":
                    vis_entries = rl.extract_list_entries(
                        kw.value, prev_var_table, getattr(kw, "lineno", 1)
                    )
                    if vis_entries is not None:
                        has_vis = True
                        vis_vals = [v for (v, _) in vis_entries]
            if (
                tname
                and has_vis
                and "//visibility:public" not in vis_vals
                and "//:__subpackages__" not in vis_vals
            ):
                narrow_targets.add(tname)
        return narrow_targets
    return set()


# Check cross-package dependency and exported file visibility
_NON_LABEL_ATTRS = frozenset({
    "name",
    "visibility",
    "default_visibility",
    "packages",
    "includes",
    "applicable_licenses",
    "default_applicable_licenses",
    "tags",
    "copts",
    "linkopts",
    "defines",
    "local_defines",
    "cxxopts",
    "conlyopts",
    "rustc_flags",
    "test_rustc_flags",
    "args",
    "env",
    "rustc_env",
    "package_name",
    "component_name",
    "binary_name",
    "crate_name",
    "library_name",
    "output_name",
    "gn_output_name",
    "install_root",
    "max_log_severity",
    "test_type",
})

dep_pkg_vis_cache = {}


# Check whether a concrete Bazel visibility entry list allows caller_pkg.
def _caller_covered_by_vis_entries(caller_pkg, dep_pkg, vis_entries):
    if not vis_entries:
        return False
    for entry in vis_entries:
        if entry == "//visibility:public":
            return True
        if entry == "//visibility:private":
            if caller_pkg == dep_pkg:
                return True
            continue
        m = re.match(r"^(?://([^:]*))?:__(pkg|subpackages)__$", entry)
        if not m:
            # Treat custom package_group labels conservatively as covered.
            return True
        scope = dep_pkg if m.group(1) is None else m.group(1).strip("/")
        kind = m.group(2)
        if kind == "pkg" and caller_pkg == scope:
            return True
        if kind == "subpackages" and (
            scope == "" or caller_pkg == scope or caller_pkg.startswith(scope + "/")
        ):
            return True
    return False


# Parse explicit target and exports_files visibility declarations in dep_pkg/BUILD.bazel.
def _load_dep_pkg_explicit_vis(dep_pkg):
    if dep_pkg in dep_pkg_vis_cache:
        return dep_pkg_vis_cache[dep_pkg]
    vis_map = {}
    dep_bazel = os.path.join(workdir, dep_pkg, "BUILD.bazel")
    if not os.path.isfile(dep_bazel):
        dep_pkg_vis_cache[dep_pkg] = vis_map
        return vis_map
    try:
        with open(dep_bazel, "r", encoding="utf-8") as fh:
            dep_src = fh.read()
        dep_tree = ast.parse(dep_src, filename=f"{dep_pkg}/BUILD.bazel")
    except Exception:
        dep_pkg_vis_cache[dep_pkg] = vis_map
        return vis_map
    dep_lines = dep_src.splitlines()
    dep_var_table = rl.parse_package_bazel_targets(dep_tree, dep_lines)["var_table"]
    for dnode in ast.walk(dep_tree):
        if not isinstance(dnode, ast.Call):
            continue
        dfn = (
            dnode.func.id
            if isinstance(dnode.func, ast.Name)
            else (dnode.func.attr if isinstance(dnode.func, ast.Attribute) else "")
        )
        if dfn == "exports_files":
            srcs_node = dnode.args[0] if dnode.args else None
            vis_node = dnode.args[1] if len(dnode.args) >= 2 else None
            vis_lineno = dnode.lineno
            for dkw in dnode.keywords:
                if dkw.arg == "srcs":
                    srcs_node = dkw.value
                elif dkw.arg == "visibility":
                    vis_node = dkw.value
                    vis_lineno = dkw.lineno
            if srcs_node is None:
                continue
            exported_names = [
                sub.value
                for sub in ast.walk(srcs_node)
                if isinstance(sub, ast.Constant) and isinstance(sub.value, str)
            ]
            if vis_node is None:
                vis_vals = ["//visibility:public"]
            else:
                parsed_entries = rl.extract_list_entries(vis_node, dep_var_table, vis_lineno)
                if parsed_entries is None:
                    continue
                vis_vals = [v for (v, _) in parsed_entries]
            for ename in exported_names:
                if ename in vis_map and vis_map[ename][2] == "exports_files":
                    merged = list(dict.fromkeys(vis_map[ename][1] + vis_vals))
                    vis_map[ename] = (vis_map[ename][0], merged, "exports_files")
                else:
                    vis_map[ename] = (vis_lineno, vis_vals, "exports_files")
            continue

        dname = None
        dvis_kw = None
        for dkw in dnode.keywords:
            if (
                dkw.arg == "name"
                and isinstance(dkw.value, ast.Constant)
                and isinstance(dkw.value.value, str)
            ):
                dname = dkw.value.value
            elif dkw.arg == "visibility":
                dvis_kw = dkw
        if dname and dvis_kw is not None:
            parsed_entries = rl.extract_list_entries(
                dvis_kw.value, dep_var_table, dvis_kw.lineno
            )
            if parsed_entries is not None:
                vis_map[dname] = (
                    dvis_kw.lineno,
                    [v for (v, _) in parsed_entries],
                    dfn or "rule",
                )
    dep_pkg_vis_cache[dep_pkg] = vis_map
    return vis_map


# Emit inaccessible_cross_package_dep when a cross-package label is not visible to pkg_path.
def check_cross_package_dep_visibility(node, target_name, rel_path, pkg_path):
    checked_pairs = set()
    for kw in node.keywords:
        if not kw.arg or kw.arg in _NON_LABEL_ATTRS:
            continue
        for sub in ast.walk(kw.value):
            if not (isinstance(sub, ast.Constant) and isinstance(sub.value, str)):
                continue
            lbl = sub.value.strip()
            if not lbl.startswith("//") or lbl.startswith(("//conditions:", "//visibility:")):
                continue
            body = lbl[2:]
            if ":" in body:
                dep_pkg, dep_target = body.split(":", 1)
            else:
                dep_pkg = body.strip("/")
                dep_target = os.path.basename(dep_pkg)
            dep_pkg = dep_pkg.strip("/")
            if not dep_pkg or dep_pkg == pkg_path or not dep_target:
                continue
            pair = (dep_pkg, dep_target)
            if pair in checked_pairs:
                continue
            checked_pairs.add(pair)
            dep_vis_info = _load_dep_pkg_explicit_vis(dep_pkg).get(dep_target)
            if dep_vis_info is None:
                continue
            vis_lineno, vis_vals, decl_kind = dep_vis_info
            if not _caller_covered_by_vis_entries(pkg_path, dep_pkg, vis_vals):
                ref_line = getattr(sub, "lineno", node.lineno)
                emit_finding(
                    "inaccessible_cross_package_dep",
                    "error",
                    rel_path,
                    ref_line,
                    (
                        f"Target '{target_name}' references '{lbl}', which is not visible "
                        f"from package '//{pkg_path}' ('{dep_pkg}/BUILD.bazel':{vis_lineno} "
                        f"restricts '{dep_target}' via {decl_kind} visibility)."
                    ),
                    (
                        f"Add '\"//{pkg_path}:__pkg__\"' (in sorted order) to the "
                        f"'visibility' list for '{dep_target}' in '{dep_pkg}/BUILD.bazel' "
                        f"(around line {vis_lineno}), leaving unrelated targets untouched."
                    ),
                )


for rel_path in sorted(candidate_files):
    full_path = os.path.join(workdir, rel_path)
    pkg_path = os.path.dirname(rel_path).strip("/")
    try:
        with open(full_path, "r", encoding="utf-8") as f:
            src = f.read()
        tree = ast.parse(src, filename=rel_path)
    except Exception:
        continue
    src_lines = src.splitlines()
    if re.search(r"^#\s*[@]generated\b", "\n".join(src_lines[:10]), flags=re.MULTILINE):
        continue

    (
        preexisting_vis,
        preexisting_no_vis_targets,
        preexisting_pkg_default_vis,
    ) = rl.get_preexisting_vis_info(workdir, rel_path, change_base)
    preexisting_narrow_vis_targets = collect_preexisting_narrow_vis_targets(
        workdir, rel_path, change_base
    )
    orig_gn_vis_map = rl.get_orig_gn_visibility_scopes(workdir, pkg_path, change_base)
    orig_gn_unrestricted_targets = rl.get_orig_gn_unrestricted_targets(
        workdir, pkg_path, change_base
    )
    parsed = rl.parse_package_bazel_targets(tree, src_lines)
    var_table = parsed["var_table"]
    exports_files_vis_scopes = collect_exports_files_vis_scopes(tree, var_table, pkg_path)
    bazel_targets = parsed["bazel_targets"]
    package_targets = parsed["package_targets"]
    non_gn_synced_targets = parsed["non_gn_synced_targets"]
    test_rule_targets = parsed["test_rule_targets"]
    idk_targets = parsed["idk_targets"]
    bazel2gn_skipped_targets = parsed["bazel2gn_skipped_targets"]
    alias_map = parsed["alias_map"]
    for _a_name, _a_actual in alias_map.items():
        if _a_actual in orig_gn_unrestricted_targets and _a_name not in orig_gn_vis_map:
            orig_gn_unrestricted_targets.add(_a_name)

    sym_macro_caller_pkgs = rl.compute_symbolic_macro_caller_pkgs(
        workdir, tree, var_table, pkg_path, bzl_cache=bzl_macro_cache
    )
    (
        true_rdeps,
        per_target_rdeps,
        per_target_direct_rdeps,
        vis_only_pkgs,
        gn_only_pkgs,
        has_macro_injected_rdeps,
    ) = rl.compute_package_rdeps(
        workdir,
        pkg_path,
        bazel_targets,
        package_targets,
        non_gn_synced_targets,
        alias_map,
        orig_gn_vis_map=orig_gn_vis_map,
        test_rule_targets=test_rule_targets,
        idk_targets=idk_targets,
        file_cache=file_cache,
        origin_main_sources=origin_main_sources,
        target_dir=target_dir,
        importer_cache=gni_importer_cache,
        grep_cache=gni_grep_cache,
    )

    def check_pkg_default_visibility(pkg_node):
        found_default_vis = False
        for kw in pkg_node.keywords:
            if kw.arg != "default_visibility":
                continue
            found_default_vis = True
            if preexisting_pkg_default_vis:
                continue
            ext_targets = [
                t for t in bazel_targets if len(per_target_rdeps.get(t, set())) > 0
            ]
            if ext_targets:
                per_t_desc = ", ".join(
                    f"'{t}': {json.dumps(rl.recommend_visibility(per_target_rdeps[t], pkg_path))}"
                    for t in ext_targets
                )
                target_guidance = (
                    "Set target-level visibility only on targets used outside this package "
                    f"({per_t_desc}), and omit visibility on package-private targets."
                )
            else:
                rec_pkg = rl.recommend_visibility(true_rdeps, pkg_path)
                target_guidance = (
                    f"Set target-level visibility = {json.dumps(rec_pkg)} only on targets "
                    "used outside this package, and omit visibility on package-private targets."
                )
            emit_finding(
                "visibility_scoping",
                "error",
                rel_path,
                kw.lineno,
                (
                    "Do not set package-level 'default_visibility' in package(...) "
                    "(see //docs/development/languages/bazel/style/build_files.md"
                    "#avoid-package-default-visibility). Declare 'visibility' only on individual "
                    "targets used outside this package, scoped to the packages and subpackages "
                    "that depend on them."
                ),
                f"Remove default_visibility from package(). {target_guidance}",
            )
        return found_default_vis

    has_pkg_default_vis = False
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        func_name = (
            node.func.id
            if isinstance(node.func, ast.Name)
            else (node.func.attr if isinstance(node.func, ast.Attribute) else "")
        )

        if func_name == "package":
            if check_pkg_default_visibility(node):
                has_pkg_default_vis = True
            continue
        if func_name == "package_group":
            continue

        target_name = None
        vis_kw = None
        is_testonly = False
        for kw in node.keywords:
            if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                target_name = kw.value.value
            elif kw.arg == "visibility":
                vis_kw = kw
            elif kw.arg == "testonly":
                if (isinstance(kw.value, ast.Constant) and kw.value.value is True) or (
                    isinstance(kw.value, ast.Name) and kw.value.id in ("True", "true")
                ):
                    is_testonly = True

        if not target_name:
            continue

        check_cross_package_dep_visibility(node, target_name, rel_path, pkg_path)

        is_test_suite_rule = func_name == "test_suite"
        suite_has_cross_pkg_member = False
        if is_test_suite_rule:
            for kw in node.keywords:
                if kw.arg == "tests":
                    suite_tests = (
                        rl.extract_list_entries(
                            kw.value, var_table, getattr(kw, "lineno", 1)
                        )
                        or []
                    )
                    for st_val, _ in suite_tests:
                        st_s = st_val.lstrip("@")
                        if st_s.startswith("//"):
                            st_pkg = st_s[2:].split(":", 1)[0].strip("/")
                            if st_pkg != pkg_path:
                                suite_has_cross_pkg_member = True
                                break
                        elif st_val.startswith("@"):
                            suite_has_cross_pkg_member = True
                            break
        is_exposable_test_suite = is_test_suite_rule and (
            target_name in ("tests", "host_tests")
            or suite_has_cross_pkg_member
        )
        is_individual_test_rule = (
            func_name in (
                "host_rustc_test", "host_test", "wrap_host_rust_test",
                "fx_test", "fx_unittest_package", "fx_test_package",
                "fx_test_component", "fx_unittest_component",
                "rustc_test", "rust_test", "cc_test", "fx_cc_test_binary", "go_test",
            )
            or func_name.startswith("wrap_host_")
            or func_name.endswith("_test")
            or (func_name == "fx_package" and any(k.arg == "test_components" for k in getattr(node, "keywords", [])))
            or (func_name == "fx_component_manifest" and is_testonly)
        )
        is_test_rule = is_individual_test_rule or is_test_suite_rule
        is_test_support_pkg = rl.is_test_support_package(rel_path)
        is_shard_rule = func_name in ("cml_shard", "expect_includes")
        target_rdeps = set(
            per_target_rdeps.get(target_name, set())
            if (is_test_rule or target_name in test_rule_targets)
            else per_target_rdeps.get(target_name, true_rdeps)
        )
        if func_name == "cml_shard":
            target_rdeps |= discover_cml_shard_rdeps(node, target_name, pkg_path)
        distinct_areas = rl.get_distinct_areas(target_rdeps, pkg_path)
        is_multi_area_test_bench_lib = (
            not is_test_rule
            and len(distinct_areas) >= 2
            and rl.has_only_test_or_bench_rdeps(target_rdeps, pkg_path)
        )
        allow_broad_test_vis = (
            (is_testonly or is_test_support_pkg or is_multi_area_test_bench_lib)
            and not is_test_rule
            and len(distinct_areas) >= 2
        ) or is_shard_rule

        is_public_lib_rule = (
            func_name.startswith("idk_")
            or func_name.endswith(("_library", "_library_zx", "_headers"))
            or func_name == "alias"
        )
        is_unrestricted_public_lib = (
            not is_test_rule
            and target_name not in test_rule_targets
            and target_name not in package_targets
            and target_name not in non_gn_synced_targets
            and target_name not in orig_gn_vis_map
            and target_name in orig_gn_unrestricted_targets
            and is_public_lib_rule
            and (func_name.startswith("idk_") or rl.is_public_sdk_or_ulib_pkg(pkg_path))
        )
        allow_unrestricted_gn_subpkgs = is_unrestricted_public_lib
        narrow_vis = rl.recommend_narrow_visibility(target_rdeps, pkg_path)
        area_rollup_vis = rl.recommend_area_rollup_visibility(target_rdeps, pkg_path)
        rec_target_vis = rl.recommend_visibility(
            target_rdeps,
            pkg_path,
            is_test_support=allow_broad_test_vis,
            target_name=target_name,
            orig_gn_vis_map=orig_gn_vis_map,
            vis_only_pkgs=vis_only_pkgs,
            gn_only_pkgs=gn_only_pkgs,
            non_gn_synced_targets=non_gn_synced_targets,
            workdir=workdir,
            orig_gn_unrestricted_targets=orig_gn_unrestricted_targets,
            idk_targets=idk_targets,
        )
        is_globally_used = rl.is_globally_used_target(target_rdeps, pkg_path)
        if pkg_path.startswith("src/lib/assembly/"):
            is_globally_used = False
        preserved_gn_vis = (
            None
            if is_test_rule or is_globally_used
            else rl.get_preserved_gn_visibility(
                workdir,
                target_name,
                target_rdeps,
                pkg_path,
                orig_gn_vis_map,
                vis_only_pkgs,
                gn_only_pkgs,
            )
        )
        if preserved_gn_vis:
            rec_target_vis = preserved_gn_vis

        requires_vis_for_callers = (
            not (is_test_rule or target_name in test_rule_targets)
            or has_macro_injected_rdeps
            or any(
                "bazel" in kinds
                for kinds in per_target_direct_rdeps.get(target_name, {}).values()
            )
        )
        target_start_findings = len(findings)
        if vis_kw is None:
            if (
                not preexisting_pkg_default_vis
                and len(target_rdeps) > 0
                and requires_vis_for_callers
                and target_name not in preexisting_no_vis_targets
            ):
                emit_finding(
                    "missing_target_visibility",
                    "error",
                    rel_path,
                    node.lineno,
                    (
                        f"Target '{target_name}' has {len(target_rdeps)} external "
                        "reverse-dependency package(s) but omits 'visibility' "
                        "(defaulting to private in Bazel and unrestricted in bazel2gn)."
                    ),
                    f"Add visibility = {json.dumps(rec_target_vis)} to '{target_name}'.",
                )
            elif (
                not preexisting_pkg_default_vis
                and is_unrestricted_public_lib
                and target_name not in preexisting_no_vis_targets
                and target_name not in preexisting_narrow_vis_targets
            ):
                emit_finding(
                    "visibility_tightened_from_gn",
                    "error",
                    rel_path,
                    node.lineno,
                    (
                        f"Target '{target_name}' omits 'visibility' (defaulting to package-private), "
                        f"but its pre-migration target in `{pkg_path}/BUILD.gn` had unrestricted "
                        "visibility. Do not restrict visibility on SDK or Zircon ulib libraries "
                        "during migration."
                    ),
                    (
                        f"Set visibility = {json.dumps(['//:__subpackages__'])} on '{target_name}' "
                        "and re-run fx bazel2gn."
                    ),
                )
            elif (
                not preexisting_pkg_default_vis
                and preserved_gn_vis
                and target_name not in preexisting_no_vis_targets
            ):
                check_visibility_tightened_from_gn(
                    target_name, rel_path, node.lineno, [], [], preserved_gn_vis, pkg_path
                )
            elif not has_pkg_default_vis and not preserved_gn_vis:
                clean_audited_targets.add((rel_path, target_name))
                record_intentional_visibility_narrowing(
                    target_name, rel_path, node.lineno, [], orig_gn_vis_map, pkg_path
                )
            continue

        entries = rl.extract_list_entries(vis_kw.value, var_table, vis_kw.lineno)
        if entries is None:
            continue

        vis_vals = [val for (val, _) in entries]
        vis_scopes = []
        only_scope_entries = True
        for val in vis_vals:
            if val == "//visibility:private":
                continue
            if val == "//visibility:public":
                only_scope_entries = False
                break
            m_scope = re.match(r"^(?://([^:]*))?:__(pkg|subpackages)__$", val)
            if not m_scope:
                only_scope_entries = False
                break
            scope_pkg = pkg_path if m_scope.group(1) is None else m_scope.group(1).strip("/")
            vis_scopes.append((scope_pkg, m_scope.group(2)))

        is_gn_synced_target = (
            target_name not in bazel2gn_skipped_targets
            and target_name not in non_gn_synced_targets
        )
        gn_scopes = vis_scopes if is_gn_synced_target else None
        end_line = getattr(vis_kw.value, "end_lineno", None)
        if is_gn_synced_target and end_line and end_line <= len(src_lines):
            m_raw = re.search(r"#\s*@bazel2gn:raw_overwrite:\s*(\[.*\])", src_lines[end_line - 1])
            if m_raw:
                try:
                    gn_scopes = []
                    for pat in json.loads(m_raw.group(1)):
                        m_gn = re.match(r"^//([^:*]*?)/?(\*|:.*)$", pat)
                        if not m_gn:
                            gn_scopes = None
                            break
                        gn_pkg = m_gn.group(1).strip("/")
                        gn_scopes.append((gn_pkg, "subpackages" if m_gn.group(2) == "*" else "pkg"))
                except Exception:
                    gn_scopes = None

        if only_scope_entries:
            uncovered = set()
            for caller, kinds in per_target_direct_rdeps.get(target_name, {}).items():
                for kind_bs in kinds:
                    scopes = (
                        gn_scopes
                        if kind_bs == "gn"
                        else (vis_scopes + exports_files_vis_scopes)
                    )
                    if scopes is None:
                        continue
                    covered = any(
                        caller == sp
                        or (
                            kind == "subpackages"
                            and (sp == "" or caller.startswith(sp + "/"))
                        )
                        for sp, kind in scopes
                    )
                    if not covered:
                        uncovered.add(caller)
            if uncovered:
                shown = sorted(
                    {
                        f"//{rl.VG_ROOT}" if (c == rl.VG_ROOT or c.startswith(rl.VG_ROOT + "/")) else f"//{c}"
                        for c in uncovered
                    }
                )
                add_entries = rl.recommend_narrow_visibility(uncovered, pkg_path)
                emit_finding(
                    "uncovered_rdep_visibility",
                    "error",
                    rel_path,
                    vis_kw.lineno,
                    (
                        f"Target '{target_name}' has visibility that does not cover {len(shown)} package(s) whose "
                        f"BUILD.gn/BUILD.bazel depend on it or on a sub-target it generates "
                        f"(e.g. '{target_name}_rust', '{target_name}_cpp'): {', '.join(shown)}. GN enforces the "
                        "converted visibility on every generated sub-target, and callers outside the locally "
                        "built product graph (other products, the internal tree) only fail in CQ with "
                        "'Dependency not allowed'."
                    ),
                    (
                        f"Add {json.dumps(add_entries)} to the visibility of '{target_name}' (or use the "
                        f"find_rdeps per_target_recommended_visibility {json.dumps(rec_target_vis)}) and re-run "
                        "fx bazel2gn."
                    ),
                )

        max_rollup_entries = max(15, len(area_rollup_vis))
        new_entries = [(val, lineno) for (val, lineno) in entries if val not in preexisting_vis]
        if (
            not is_test_rule
            and not is_shard_rule
            and allow_broad_test_vis
            and not preserved_gn_vis
            and target_name not in orig_gn_vis_map
            and len(distinct_areas) >= 3
            and new_entries
            and "//:__subpackages__" not in vis_vals
            and "//visibility:public" not in vis_vals
        ):
            emit_finding(
                "overly_narrow_global_visibility",
                "error",
                rel_path,
                vis_kw.lineno,
                (
                    f"Target '{target_name}' is a shared test/benchmark support library used "
                    f"across {len(distinct_areas)} distinct areas ({len(target_rdeps)} caller "
                    f"packages) whose pre-migration `{pkg_path}/BUILD.gn` had unrestricted "
                    f"visibility, but narrows visibility to {len(entries)} package/subtree "
                    "entries, breaking cross-tree and out-of-graph benchmark/test callers in "
                    "CQ with 'Dependency not allowed'."
                ),
                (
                    f"Replace visibility on '{target_name}' with {json.dumps(rec_target_vis)} "
                    "(shared test/benchmark support libraries used across multiple areas use "
                    "repository-wide subpackage visibility) and re-run fx bazel2gn."
                ),
            )
            continue

        # Avoid forcing repository-wide rollup on targets whose BUILD.bazel already
        # had an explicit narrow visibility allowlist before this change unless >10
        # new visibility entries are added at once.
        has_preexisting_narrow_vis = (
            target_name in preexisting_narrow_vis_targets and len(new_entries) <= 10
        )
        if (
            is_unrestricted_public_lib
            and not has_preexisting_narrow_vis
            and new_entries
            and "//:__subpackages__" not in vis_vals
            and "//visibility:public" not in vis_vals
        ):
            emit_finding(
                "visibility_tightened_from_gn",
                "error",
                rel_path,
                vis_kw.lineno,
                (
                    f"Target '{target_name}' narrows visibility to {json.dumps(vis_vals)}, "
                    f"but its pre-migration target in `{pkg_path}/BUILD.gn` had unrestricted "
                    "visibility. Do not restrict visibility on SDK or Zircon ulib libraries "
                    "during migration."
                ),
                (
                    f"Replace visibility on '{target_name}' with {json.dumps(['//:__subpackages__'])} "
                    "and re-run fx bazel2gn."
                ),
            )
            continue
        sprawling_vis = (
            bool(new_entries)
            and not has_preexisting_narrow_vis
            and (
                (
                    len(entries) > max_rollup_entries
                    and (is_globally_used or len(area_rollup_vis) < len(entries))
                )
                or (
                    len(entries) > 10
                    and len(distinct_areas) >= 3
                    and len(area_rollup_vis) < len(entries)
                )
            )
        )
        if sprawling_vis:
            if is_globally_used:
                emit_finding(
                    "overly_narrow_global_visibility",
                    "error",
                    rel_path,
                    vis_kw.lineno,
                    (
                        f"Target '{target_name}' is widely used across {len(distinct_areas)} distinct areas "
                        f"({len(target_rdeps)} reverse-dependency packages) but enumerates {len(entries)} narrow "
                        "visibility entries ('too narrow' for a widely-used platform library/test utility)."
                    ),
                    (
                        f"Replace the sprawling {len(entries)}-entry visibility list on '{target_name}' with "
                        f"subsystem/top-level rollup visibility {json.dumps(rec_target_vis)} (or "
                        'visibility = ["//:__subpackages__"] for foundational tree-wide libraries), per '
                        "determining_bazel_visibility/SKILL.md, and re-run fx bazel2gn."
                    ),
                )
            else:
                top_roots = {r.split("/")[0] for r in target_rdeps if r.strip("/")}
                top_rollup_hint = (
                    ' (or ["//src:__subpackages__"] when widely used across //src)'
                    if top_roots == {"src"} and len(distinct_areas) >= 3
                    else ""
                )
                emit_finding(
                    "overly_narrow_global_visibility",
                    "error",
                    rel_path,
                    vis_kw.lineno,
                    (
                        f"Target '{target_name}' enumerates {len(entries)} narrow visibility entries across "
                        f"{len(distinct_areas)} areas. Roll up visibility to subsystem or top-level subtrees "
                        "instead of appending individual leaf packages."
                    ),
                    (
                        f"Replace visibility on '{target_name}' with rollup entries "
                        f"{json.dumps(area_rollup_vis)}{top_rollup_hint} and re-run fx bazel2gn."
                    ),
                )
            continue

        findings_before_target = len(findings)
        allow_broad_rollup = (
            is_globally_used or len(narrow_vis) > 5 or len(distinct_areas) >= 3 or allow_broad_test_vis
        ) and len(entries) <= max_rollup_entries
        top_level_subpkgs = []
        for val, lineno in entries:
            if val in preexisting_vis:
                continue
            if val == "//visibility:private":
                if not has_pkg_default_vis and not (is_gn_synced_target and len(entries) == 1):
                    emit_finding(
                        "redundant_private_visibility",
                        "error",
                        rel_path,
                        lineno,
                        (
                            f"Target '{target_name}' specifies redundant "
                            '`"//visibility:private"` in `visibility` (which is already the '
                            "Bazel default when `default_visibility` is not set, and `bazel2gn` "
                            'automatically adds `":*"` to non-empty visibility lists).'
                        ),
                        f"Remove `\"//visibility:private\"` from `visibility` on '{target_name}' in '{rel_path}'.",
                    )
                continue
            if val == "//visibility:public":
                if rel_path.startswith("build/bazel_sdk/"):
                    continue
                if is_test_suite_rule:
                    if target_rdeps and narrow_vis:
                        rem = (
                            f"Replace '//visibility:public' on test_suite '{target_name}' with package-specific "
                            f"visibility for the aggregation package that includes it: {json.dumps(narrow_vis)}."
                        )
                    else:
                        rem = (
                            f"Replace '//visibility:public' on test_suite '{target_name}' with package-specific "
                            "visibility for the aggregation package that includes it (e.g. "
                            '["//<aggregation_dir>:__pkg__"] once an aggregation test_suite() exists in '
                            "//<aggregation_dir>, or omit 'visibility' until the aggregation package is known "
                            "or if it is a top-level aggregation test_suite() / package-private sub-suite)."
                        )
                elif is_individual_test_rule:
                    rem = (
                        f"Omit 'visibility' on individual test target '{target_name}' (default package-private) and "
                        "expose the package's tests via a package `test_suite(name = \"tests\" | \"host_tests\")` "
                        "(granting `visibility = [\"//<aggregation_dir>:__pkg__\"]` once wired into an existing "
                        "aggregation `test_suite()`) instead."
                    )
                elif allow_broad_test_vis:
                    rem = (
                        "Never use '//visibility:public' when migrating GN targets to Bazel (per "
                        "determining_bazel_visibility/SKILL.md). Replace '//visibility:public' with "
                        '["//:__subpackages__"] for test support libraries and re-run fx bazel2gn.'
                    )
                elif len(target_rdeps) == 0 and not has_macro_injected_rdeps:
                    rem = f"Omit 'visibility' on '{target_name}' (default private) as no external packages depend on it, and re-run fx bazel2gn."
                elif is_globally_used or has_macro_injected_rdeps:
                    rem = (
                        "Never use '//visibility:public' when migrating GN targets to Bazel (per "
                        "determining_bazel_visibility/SKILL.md). Replace '//visibility:public' with "
                        f"subsystem-scoped visibility {json.dumps(rec_target_vis)} (or "
                        '["//:__subpackages__"] for foundational tree-wide libraries) and re-run fx bazel2gn.'
                    )
                else:
                    rem = (
                        "Never use '//visibility:public' when migrating GN targets to Bazel (per "
                        "determining_bazel_visibility/SKILL.md). Replace '//visibility:public' with "
                        f"tightly-scoped caller visibility: {json.dumps(rec_target_vis)} and re-run fx bazel2gn."
                    )
                emit_finding(
                    "overbroad_public_visibility",
                    "error",
                    rel_path,
                    lineno,
                    (
                        f"Target '{target_name}' uses '//visibility:public', which is disallowed outside "
                        f"//build/bazel_sdk/... ({len(target_rdeps)} package(s) across {len(distinct_areas)} "
                        f"area(s) depend on '{target_name}')."
                    ),
                    rem,
                )
                continue
            if val in (":__pkg__", f"//{pkg_path}:__pkg__"):
                emit_finding(
                    "redundant_same_package_visibility",
                    "error",
                    rel_path,
                    lineno,
                    f"Target '{target_name}' includes redundant '{val}' in visibility. Same-package visibility is implicit in Bazel and added automatically by bazel2gn.",
                    f"Remove '{val}' from visibility.",
                )
            elif val == "//sdk:__pkg__" and func_name.startswith("idk_") and "sdk" not in target_rdeps:
                emit_finding(
                    "redundant_idk_sdk_visibility",
                    "error",
                    rel_path,
                    lineno,
                    (
                        f"`{func_name}(name = \"{target_name}\")` lists '//sdk:__pkg__' in visibility, but "
                        f"`{func_name}` already appends it to the target and its `_idk` atom, and nothing in "
                        "//sdk depends on the library target itself in GN (GN `sdk_*` templates only forward "
                        "`visibility` to the library, not to the `<name>_sdk` atom)."
                    ),
                    f"Remove '//sdk:__pkg__' from the visibility of '{target_name}' and re-run fx bazel2gn.",
                )
            elif val in ("//build/sdk:__pkg__", "//build/sdk:__subpackages__", "//sdk/fidl:__pkg__"):
                emit_finding(
                    "redundant_sdk_visibility",
                    "error",
                    rel_path,
                    lineno,
                    (
                        f"Target '{target_name}' includes redundant '{val}' in visibility. Bazel IDK atom rules "
                        "and GN SDK templates (sdk_source_set, sdk_prebuilt_library_impl, fidl_library) inject "
                        "required SDK visibility automatically."
                    ),
                    f"Remove '{val}' from visibility and re-run fx bazel2gn.",
                )
            elif val == "//:__subpackages__" and is_test_rule and not has_macro_injected_rdeps:
                emit_finding(
                    "overbroad_test_target_visibility",
                    "error",
                    rel_path,
                    lineno,
                    (
                        f"Test target '{target_name}' ({func_name}) uses '//:__subpackages__' in visibility; "
                        "individual test targets must omit 'visibility' (package-private), and package "
                        "test_suite() targets must use package-specific visibility (e.g. "
                        '["//<aggregation_dir>:__pkg__"]), never \':__subpackages__\'.'
                    ),
                    (
                        f"Replace '//:__subpackages__' on '{target_name}' with {json.dumps(narrow_vis)}."
                        if is_exposable_test_suite and narrow_vis
                        else (
                            f"Replace '//:__subpackages__' on '{target_name}' with package-specific "
                            'visibility for the aggregation package (or omit \'visibility\' until the '
                            "aggregation package is known)."
                            if is_exposable_test_suite
                            else f"Omit 'visibility' on '{target_name}' (default package-private) and expose a package `test_suite(name = \"tests\" | \"host_tests\")` instead."
                        )
                    ),
                )
            elif val == "//:__subpackages__":
                if (
                    not is_globally_used
                    and not has_macro_injected_rdeps
                    and not allow_broad_test_vis
                    and not allow_unrestricted_gn_subpkgs
                ):
                    emit_finding(
                        "disguised_public_visibility",
                        "error",
                        rel_path,
                        lineno,
                        (
                            f"Target '{target_name}' uses '//:__subpackages__' in visibility, but only "
                            f"{len(target_rdeps)} package(s) across {len(distinct_areas)} area(s) depend on '{target_name}'. "
                            "Reserve '//:__subpackages__' for foundational libraries with widespread tree-wide usage."
                        ),
                        f"Replace '//:__subpackages__' with {json.dumps(rec_target_vis)} and re-run fx bazel2gn.",
                    )
            else:
                if val.startswith("//" + rl.VG_ROOT + "/"):
                    emit_finding(
                        "internal_vendor_subpath_visibility",
                        "error",
                        rel_path,
                        lineno,
                        (
                            f"Target '{target_name}' exposes internal vendor subpath '{val}' in visibility. "
                            f"Never use internal vendor subpaths in visibility; expose to '{rl.VG_SUBPKGS}' instead."
                        ),
                        f"Replace '{val}' with '\"{rl.VG_SUBPKGS}\"' and re-run fx bazel2gn.",
                    )
                    continue

                if preserved_gn_vis and val in preserved_gn_vis:
                    continue

                m_sub = re.match(r"^//([^:]+):__subpackages__$", val)
                m_pkg = re.match(r"^//([^:]+):__pkg__$", val)

                if m_sub:
                    prefix = m_sub.group(1).strip("/")
                    if is_test_rule and not has_macro_injected_rdeps:
                        emit_finding(
                            "overbroad_subpackages_visibility",
                            "error",
                            rel_path,
                            lineno,
                            (
                                f"Test target '{target_name}' ({func_name}) uses subpackage wildcard '{val}'; "
                                "individual test targets must omit 'visibility' (package-private), and package "
                                "test_suite() targets must use package-specific visibility (e.g. "
                                f'["//{prefix}:__pkg__"]), never \':__subpackages__\'.'
                            ),
                            (
                                f"Replace '{val}' on '{target_name}' with {json.dumps(narrow_vis) if narrow_vis else f'[\"//{prefix}:__pkg__\"]'}."
                                if is_exposable_test_suite
                                else f"Omit 'visibility' on '{target_name}' (default package-private) and expose a package test_suite(name = \"tests\" | \"host_tests\", visibility = [\"//{prefix}:__pkg__\"]) instead."
                            ),
                        )
                        continue
                    if prefix == rl.VG_ROOT:
                        has_vg_rdep = any(
                            r == rl.VG_ROOT or r.startswith(rl.VG_ROOT + "/") for r in target_rdeps
                        )
                        if (
                            not has_vg_rdep
                            and not has_macro_injected_rdeps
                            and not rl.is_subrepo_origin_main_stale(workdir, rl.VG_ROOT)
                        ):
                            emit_finding(
                                "false_positive_rdep_visibility",
                                "error",
                                rel_path,
                                lineno,
                                (
                                    f"Target '{target_name}' grants visibility to '{val}', "
                                    "but no package in that vendor tree actually depends on "
                                    f"'{target_name}'."
                                ),
                                f"Remove '{val}' from visibility.",
                            )
                        continue
                    parts = prefix.split("/")
                    depth = len(parts)
                    covered_rdeps = sorted(
                        r for r in target_rdeps if r == prefix or r.startswith(prefix + "/")
                    )
                    replacement = rl.recommend_narrow_visibility(covered_rdeps, pkg_path)

                    if depth == 1:
                        top_level_subpkgs.append((val, lineno))
                        if (
                            prefix in rl.BANNED_TOP_LEVEL_SUBPKGS
                            and not is_globally_used
                            and not allow_broad_rollup
                            and not allow_broad_test_vis
                        ):
                            emit_finding(
                                "overbroad_top_level_subpackages_visibility",
                                "error",
                                rel_path,
                                lineno,
                                f"Target '{target_name}' grants top-level wildcard '{val}' in visibility ('All of //{prefix} seems too wide').",
                                (
                                    f"Replace '{val}' with tightly-scoped package/subsystem entries: {json.dumps(replacement)}."
                                    if replacement
                                    else f"Remove '{val}' from visibility (no true reverse dependencies exist under '//{prefix}')."
                                ),
                            )
                            continue

                    if prefix in rl.BANNED_UMBRELLA_SUBPKGS:
                        if not allow_broad_test_vis and not is_globally_used:
                            emit_finding(
                                "overbroad_subpackages_visibility",
                                "error",
                                rel_path,
                                lineno,
                                f"Target '{target_name}' grants multi-domain umbrella wildcard '{val}' in visibility ('//{prefix} seems too wide').",
                                (
                                    f"Replace '{val}' with specific library/package visibility entries: {json.dumps(replacement)}."
                                    if replacement
                                    else f"Remove '{val}' from visibility (no true reverse dependencies exist under '//{prefix}')."
                                ),
                            )
                            continue
                        if not allow_broad_test_vis and covered_rdeps:
                            emit_finding(
                                "overbroad_subpackages_visibility",
                                "info",
                                rel_path,
                                lineno,
                                f"Globally used target '{target_name}' grants multi-domain umbrella wildcard '{val}' in visibility.",
                                f"No change needed if '{target_name}' is meant for use across '//{prefix}' (a human reviewer should confirm).",
                            )
                            continue

                    if len(covered_rdeps) == 0:
                        if not has_macro_injected_rdeps:
                            emit_finding(
                                "false_positive_rdep_visibility",
                                "error",
                                rel_path,
                                lineno,
                                f"Target '{target_name}' grants visibility to '{val}', but no package under '//{prefix}' actually depends on '{target_name}'.",
                                f"Remove '{val}' from visibility.",
                            )
                        continue

                    if allow_broad_rollup:
                        continue

                    if depth == 2 and parts[0] in ("src", "sdk"):
                        same_area = pkg_path == prefix or pkg_path.startswith(prefix + "/")
                        if not same_area:
                            d3_subtrees = {
                                "/".join(r.split("/")[:3]) for r in covered_rdeps if len(r.split("/")) >= 3
                            }
                            emit_finding(
                                "overbroad_subpackages_visibility",
                                "error",
                                rel_path,
                                lineno,
                                f"Target '{target_name}' (in '//{pkg_path}') grants cross-area wildcard '{val}' ('//{prefix} seems too wide'; only {len(covered_rdeps)} caller package(s) across {len(d3_subtrees)} subcomponent(s) depend on '{target_name}').",
                                f"Replace '{val}' with narrower package/subcomponent visibility entries: {json.dumps(replacement)}.",
                            )
                            continue

                    same_subtree = pkg_path == prefix or pkg_path.startswith(prefix + "/")
                    if depth >= 3 and not same_subtree and not prefix.startswith(("build/bazel/", "build/images/", "bundles/")):
                        split_cov = [r.split("/") for r in covered_rdeps]
                        min_len = min(len(s) for s in split_cov)
                        lca_len = 0
                        for i in range(min_len):
                            if len({s[i] for s in split_cov}) == 1:
                                lca_len = i + 1
                            else:
                                break
                        lca = "/".join(split_cov[0][:lca_len])

                        if len(covered_rdeps) == 1:
                            only_pkg = covered_rdeps[0]
                            emit_finding(
                                "overbroad_subpackages_visibility",
                                "error",
                                rel_path,
                                lineno,
                                f"Target '{target_name}' uses wildcard '{val}', but only a single package ('//{only_pkg}') under '//{prefix}' depends on '{target_name}'.",
                                f"Replace '{val}' with '\"//{only_pkg}:__pkg__\"'.",
                            )
                        elif lca != prefix and lca.startswith(prefix + "/"):
                            emit_finding(
                                "overbroad_subpackages_visibility",
                                "error",
                                rel_path,
                                lineno,
                                f"Target '{target_name}' uses '{val}', which is wider than the Lowest Common Ancestor ('//{lca}') of its actual callers under '//{prefix}'.",
                                f"Replace '{val}' with {json.dumps(replacement)}.",
                            )

                elif m_pkg:
                    caller_pkg = m_pkg.group(1).strip("/")
                    if caller_pkg == "visibility":
                        continue
                    if caller_pkg in sym_macro_caller_pkgs.get(target_name, set()):
                        continue
                    if rl.is_pending_ancestor_test_suite_vis(
                        workdir,
                        caller_pkg,
                        pkg_path,
                        is_exposable_test_suite,
                        per_target_direct_rdeps.get(target_name, {}),
                        cache=ancestor_suite_cache,
                    ):
                        continue
                    if caller_pkg in target_rdeps:
                        continue
                    if is_individual_test_rule or (
                        is_test_suite_rule and not is_exposable_test_suite
                    ):
                        emit_finding(
                            "false_positive_rdep_visibility",
                            "error",
                            rel_path,
                            lineno,
                            (
                                f"Test target '{target_name}' ({func_name}) grants visibility to "
                                f"'{val}', but individual test targets and sub-suites within a package "
                                "must remain package-private (omit 'visibility') and be exposed via a "
                                "package test_suite(name = \"tests\" | \"host_tests\") instead."
                            ),
                            (
                                f"Omit 'visibility' on '{target_name}' in '{rel_path}', define a package "
                                f"`test_suite(name = \"tests\" | \"host_tests\", visibility = [\"{val}\"])` "
                                f"as the first target in '{rel_path}', and reference `//{pkg_path}:tests` "
                                f"(or `//{pkg_path}:host_tests`) in '//{caller_pkg}/BUILD.bazel'."
                            ),
                        )
                    elif caller_pkg in vis_only_pkgs:
                        emit_finding(
                            "visibility_allowlist_false_rdep",
                            "error",
                            rel_path,
                            lineno,
                            f"Target '{target_name}' grants visibility to '{val}', but '//{caller_pkg}' only references '//{pkg_path}' inside its own visibility allowlist.",
                            f"Remove '{val}' from visibility.",
                        )
                    elif caller_pkg in gn_only_pkgs or caller_pkg not in target_rdeps:
                        emit_finding(
                            "false_positive_rdep_visibility",
                            "error",
                            rel_path,
                            lineno,
                            f"Target '{target_name}' grants visibility to '{val}', but '//{caller_pkg}' does not depend on Bazel target '{target_name}' (e.g. only references :tests or :benchmarks).",
                            f"Remove '{val}' from visibility.",
                        )

        if len(top_level_subpkgs) >= 5 and not allow_broad_rollup:
            emit_finding(
                "disguised_public_visibility",
                "error",
                rel_path,
                vis_kw.lineno,
                f"Target '{target_name}' enumerates {len(top_level_subpkgs)} top-level '//<dir>:__subpackages__' roots in visibility.",
                f"Scope visibility to {json.dumps(rec_target_vis)}.",
            )

        if (
            preserved_gn_vis
            and new_entries
            and only_scope_entries
            and len(findings) == findings_before_target
        ):
            check_visibility_tightened_from_gn(
                target_name, rel_path, vis_kw.lineno, vis_vals, vis_scopes, preserved_gn_vis, pkg_path
            )

        if target_name in orig_gn_vis_map and len(findings) == findings_before_target:
            gn_allowed = orig_gn_vis_map[target_name]
            all_rdeps_in_gn = all(
                any(
                    r == gsp or (gk == "subpackages" and (gsp == "" or r.startswith(gsp + "/")))
                    for gsp, gk in gn_allowed
                )
                for r in target_rdeps
            )
            if all_rdeps_in_gn:
                for val, lineno in entries:
                    if val in preexisting_vis or val == "//visibility:private" or val in rec_target_vis:
                        continue
                    m_scope = re.match(r"^(?://([^:]*))?:__(pkg|subpackages)__$", val)
                    if not m_scope:
                        continue
                    sp = pkg_path if m_scope.group(1) is None else m_scope.group(1).strip("/")
                    kind = m_scope.group(2)
                    if kind == "pkg" and rl.is_pending_ancestor_test_suite_vis(
                        workdir,
                        sp,
                        pkg_path,
                        is_exposable_test_suite,
                        per_target_direct_rdeps.get(target_name, {}),
                        cache=ancestor_suite_cache,
                    ):
                        continue
                    if kind == "pkg":
                        covered_by_gn = any(
                            sp == gsp or (gk == "subpackages" and (gsp == "" or sp.startswith(gsp + "/")))
                            for gsp, gk in gn_allowed
                        )
                    else:
                        covered_by_gn = any(
                            gk == "subpackages" and (gsp == "" or sp == gsp or sp.startswith(gsp + "/"))
                            for gsp, gk in gn_allowed
                        )
                    if not covered_by_gn:
                        gn_bounds_desc = sorted(
                            {f"//{gsp}/*" if gk == "subpackages" else f"//{gsp}:*" for gsp, gk in gn_allowed}
                        )
                        emit_finding(
                            "visibility_widened_beyond_gn",
                            "error",
                            rel_path,
                            lineno,
                            (
                                f"Target '{target_name}' widens visibility with '{val}' beyond the explicit "
                                f"visibility bounds in `{pkg_path}/BUILD.gn` in base GN ({', '.join(gn_bounds_desc)}), "
                                "even though all actual reverse dependencies are within the original GN scope."
                            ),
                            (
                                f"Scope visibility on '{target_name}' to {json.dumps(rec_target_vis)} (or omit "
                                "visibility if private to the package) and re-run fx bazel2gn."
                            ),
                        )

        has_target_err = any(
            f.get("severity", "").lower() != "info" for f in findings[target_start_findings:]
        )
        if only_scope_entries and not has_target_err and (not preserved_gn_vis or new_entries):
            clean_audited_targets.add((rel_path, target_name))
            record_intentional_visibility_narrowing(
                target_name, rel_path, node.lineno, vis_scopes, orig_gn_vis_map, pkg_path
            )

record_diff_visibility_narrowed_info(clean_audited_targets)
pcl.emit_json(findings)
PYEOF
