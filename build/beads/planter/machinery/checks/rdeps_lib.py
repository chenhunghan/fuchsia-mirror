#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Shared reverse-dependency discovery and Bazel visibility recommendation library.

Used by both `machinery/checks/visibility_audit.sh` and `machinery/tools/find_rdeps.sh`
so coder guidance and gate enforcement use identical rdep and visibility logic.
"""

import ast
import hashlib
import os
import re
import shutil
import subprocess
import tempfile
import time

import planter_check_lib as pcl

normalize_target_dir = pcl.normalize_target_dir
normalize_target_pkg = pcl.normalize_target_dir
has_bazel2gn_skip_comment = pcl.has_bazel2gn_skip_comment

VG_ROOT = "vendor/" + "google"
VG_SUBPKGS = "//" + VG_ROOT + ":__subpackages__"
EXCLUDED_GLOBAL_VIS_PREFIXES = tuple(
    f"{d.strip('/')}/" for d in sorted(pcl.EXCLUDED_GLOBAL_DIRS)
)

BANNED_TOP_LEVEL_SUBPKGS = {"src", "sdk", "build", "zircon", "third_party"}
BANNED_UMBRELLA_SUBPKGS = {
    "src/lib",
    "sdk/lib",
    "src/testing",
    "src/tests",
    "third_party/rust_crates",
}
BANNED_UMBRELLA_AREAS = BANNED_UMBRELLA_SUBPKGS

TEST_OR_BENCH_CALLER_SEGMENTS = {
    "test",
    "tests",
    "testing",
    "bench",
    "benchmarks",
    "test_runners",
    "stress",
    "e2e",
}
TEST_OR_BENCH_CALLER_SUFFIXES = (
    "_test",
    "_tests",
    "_bench",
    "_benchmarks",
    "_stress",
)

CONVERTIBLE_GN_TEMPLATES = {
    "static_library",
    "source_set",
    "shared_library",
    "executable",
    "library_headers",
    "rustc_library",
    "rustc_library_with_features",
    "rustc_binary",
    "rustc_macro",
    "rustc_dylib",
    "rustc_test",
    "go_library",
    "go_binary",
    "go_test",
    "python_library",
    "python_binary",
    "fidl",
    "sdk_source_set",
    "sdk_static_library",
    "sdk_shared_library",
    "zx_library",
    "fuchsia_package",
    "fuchsia_package_with_single_component",
    "fuchsia_component",
    "fuchsia_component_manifest",
    "fuchsia_structured_config_rust_lib",
    "fuchsia_structured_config_cpp_elf_lib",
    "fuchsia_structured_config_values",
    "resource",
    "resource_tree",
    "resource_group",
    "config",
    "bind_library",
    "cml_shard",
}

GN_PACKAGE_TEMPLATES = {
    "fuchsia_package",
    "fuchsia_package_with_single_component",
}
BAZEL_GN_SYNCED_TEST_RULES = {
    "rustc_test",
    "host_rustc_test",
    "go_test",
    "host_go_test",
}

BAZEL_NON_GN_SYNCED_RULES = {
    "cc_test",
    "cml_shard",
    "expect_includes",
    "fx_cc_test_binary",
    "fx_packaged_binary",
    "fx_test_component",
    "fx_unittest_component",
    "fx_test",
    "fx_unittest_package",
    "test_suite",
    "rustc_test",
    "host_cc_test",
    "host_rustc_test",
    "host_go_test",
    "host_py_test",
    "host_shac_test",
    "host_test",
    "host_test_data_files",
    "host_test_data_map",
    "cc_binary_host_tool",
    "go_binary_host_tool",
    "py_binary_host_tool",
    "rustc_binary_host_tool",
    "idk_cc_binary_host_tool",
    "idk_go_binary_host_tool",
    "idk_rustc_binary_host_tool",
    "wrap_host_rust_test",
    "go_test",
}

GN_IMPORT_RE = re.compile(r'\bimport\(\s*"([^"]+\.gni)"\s*\)')
GN_TARGET_PARAM_VARS = {
    "args",
    "assert_no_deps",
    "cflags",
    "cflags_c",
    "cflags_cc",
    "configs",
    "data",
    "data_deps",
    "defines",
    "deps",
    "host_tests",
    "include_dirs",
    "inputs",
    "ldflags",
    "libs",
    "outputs",
    "packages",
    "public",
    "public_configs",
    "public_deps",
    "rustenv",
    "rustflags",
    "sources",
    "target_tests",
    "test_deps",
    "visibility",
}

# Keep origin/main fetch cache short so end-of-turn run_checks sees recent upstream commits.
FETCH_TTL_SECS = 15

_NON_DEP_LIST_RE = re.compile(
    r"\b(?:visibility|packages|assert_no_deps|_?excluded_[A-Za-z0-9_]*)\s*\+?=\s*\[[^\]]*\]",
    re.DOTALL,
)
_GN_NON_DEP_VAR_RE = re.compile(
    r"\b(?:visibility|packages|assert_no_deps|_?excluded_[A-Za-z0-9_]*)\s*\+?=\s*[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*"
)
_GN_MEMBERSHIP_CHECK_RE = re.compile(
    r"\b([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*)\s*"
    r"(?:\+\s*\[[^\]]+\]\s*-\s*\[[^\]]+\]\s*[!=]=\s*\1\b"
    r"|[!=]=\s*\1\s*\+\s*\[[^\]]+\]\s*-\s*\[[^\]]+\])",
    re.DOTALL,
)
_EXCLUDED_DEP_LIST_RE = re.compile(
    r"\b(?:assert_no_deps|_?excluded_[A-Za-z0-9_]*)\s*\+?=\s*\[([^\]]*)\]",
    re.DOTALL,
)
_SUITE_LIST_RE = re.compile(
    r"\b(?:target_tests|host_tests)\s*\+?=\s*\[[^\]]*\]", re.DOTALL
)
_BAZEL_TEST_SUITE_OPEN_RE = re.compile(
    r'\bbazel_test_suite\s*\(\s*"[^"]*"\s*\)\s*\{'
)


def get_pkg_area(p):
    parts = p.strip("/").split("/")
    if not parts or not parts[0]:
        return ""
    if parts[0] in ("src", "sdk") and len(parts) >= 2:
        return f"{parts[0]}/{parts[1]}"
    return parts[0]


def get_distinct_areas(rdep_pkgs, pkg_path="", with_slash_prefix=False):
    pkg_norm = (pkg_path or "").strip("/")
    pkgs = sorted(
        {
            p.strip("/")
            for p in rdep_pkgs
            if p.strip("/") and p.strip("/") != pkg_norm
        }
    )
    if with_slash_prefix:
        return sorted({f"//{get_pkg_area(p)}" for p in pkgs if get_pkg_area(p)})
    return sorted({get_pkg_area(p) for p in pkgs if get_pkg_area(p)})


def sanitize_rdep_pkg(p):
    p = p.strip("/")
    return VG_ROOT if (p == VG_ROOT or p.startswith(VG_ROOT + "/")) else p


def is_test_support_package(pkg_path):
    p = (pkg_path or "").strip("/")
    return (
        p.startswith(("src/testing/", "src/lib/testing/", "testing/"))
        or "/testing/" in p
    )


def recommend_narrow_visibility(rdep_pkgs, pkg_path=""):
    pkg_norm = (pkg_path or "").strip("/")
    raw_pkgs = sorted(
        {
            p.strip("/")
            for p in rdep_pkgs
            if p.strip("/") and p.strip("/") != pkg_norm
        }
    )
    if not raw_pkgs:
        return []
    has_vg = any(p == VG_ROOT or p.startswith(VG_ROOT + "/") for p in raw_pkgs)
    pkgs = [
        p for p in raw_pkgs if p != VG_ROOT and not p.startswith(VG_ROOT + "/")
    ]
    vendor_entries = [VG_SUBPKGS] if has_vg else []
    if not pkgs:
        return vendor_entries
    if len(pkgs) <= 5:
        result = list(vendor_entries)
        for p in pkgs:
            if p == "bundles/assembly":
                result.append("//bundles/assembly:__subpackages__")
                continue
            if any(
                p != o and p.startswith(o + "/")
                for o in pkgs
                if len(o.split("/")) >= 3
            ):
                continue
            has_child = any(o != p and o.startswith(p + "/") for o in pkgs)
            result.append(
                f"//{p}:__subpackages__"
                if (has_child and len(p.split("/")) >= 3)
                else f"//{p}:__pkg__"
            )
        return sorted(set(result))

    by_d3 = {}
    for p in pkgs:
        parts = p.split("/")
        by_d3.setdefault(
            p if len(parts) < 3 else "/".join(parts[:3]), []
        ).append(p)

    result = set(vendor_entries)
    for d3, group in sorted(by_d3.items()):
        if len(d3.split("/")) < 3:
            for p in group:
                result.add(
                    "//bundles/assembly:__subpackages__"
                    if p == "bundles/assembly"
                    else f"//{p}:__pkg__"
                )
            continue

        split_group = [g.split("/") for g in group]
        min_len = min(len(s) for s in split_group)
        lca_len = 0
        for i in range(min_len):
            if len({s[i] for s in split_group}) == 1:
                lca_len = i + 1
            else:
                break
        lca = "/".join(split_group[0][: max(3, lca_len)])
        lca_depth = len(lca.split("/"))

        if (
            (lca in group and len(group) >= 2)
            or (lca_depth >= 4 and len(group) >= 3)
            or (lca_depth == 3 and len(group) >= 4)
        ):
            result.add(f"//{lca}:__subpackages__")
        else:
            for p in group:
                if any(
                    p != o and p.startswith(o + "/")
                    for o in group
                    if len(o.split("/")) >= 3
                ):
                    continue
                has_child = any(o != p and o.startswith(p + "/") for o in group)
                result.add(
                    f"//{p}:__subpackages__"
                    if (has_child and len(p.split("/")) >= 3)
                    else f"//{p}:__pkg__"
                )

    return sorted(result)


def recommend_area_rollup_visibility(rdep_pkgs, pkg_path=""):
    pkg_norm = (pkg_path or "").strip("/")
    raw_pkgs = sorted(
        {
            p.strip("/")
            for p in rdep_pkgs
            if p.strip("/") and p.strip("/") != pkg_norm
        }
    )
    if not raw_pkgs:
        return []
    has_vg = any(p == VG_ROOT or p.startswith(VG_ROOT + "/") for p in raw_pkgs)
    pkgs = [
        p for p in raw_pkgs if p != VG_ROOT and not p.startswith(VG_ROOT + "/")
    ]
    by_area = {}
    for p in pkgs:
        by_area.setdefault(get_pkg_area(p), []).append(p)
    result = {VG_SUBPKGS} if has_vg else set()
    for area, group in sorted(by_area.items()):
        if area in BANNED_UMBRELLA_SUBPKGS:
            result.update(recommend_narrow_visibility(group, pkg_norm))
        elif "/" in area:
            if len(group) >= 2 or any(
                g != area and len(g.split("/")) > 2 for g in group
            ):
                result.add(f"//{area}:__subpackages__")
            else:
                result.add(f"//{group[0]}:__pkg__")
        else:
            by_d2 = {}
            for g in group:
                parts = g.split("/")
                by_d2.setdefault(
                    "/".join(parts[:2]) if len(parts) >= 2 else parts[0], []
                ).append(g)
            for d2, d2_group in sorted(by_d2.items()):
                if d2 in BANNED_UMBRELLA_SUBPKGS:
                    result.update(
                        recommend_narrow_visibility(d2_group, pkg_norm)
                    )
                elif len(d2_group) >= 2 or d2 == "bundles/assembly":
                    result.add(f"//{d2}:__subpackages__")
                else:
                    result.add(f"//{d2_group[0]}:__pkg__")
    return sorted(result)


def is_globally_used_target(rdep_pkgs, pkg_path=""):
    pkg_norm = (pkg_path or "").strip("/")
    pkgs = sorted(
        {
            p.strip("/")
            for p in rdep_pkgs
            if p.strip("/") and p.strip("/") != pkg_norm
        }
    )
    narrow = recommend_narrow_visibility(pkgs, pkg_norm)
    areas = get_distinct_areas(pkgs, pkg_norm)
    return len(areas) >= 6 and (len(narrow) > 15 or len(pkgs) >= 20)


def collapse_src_rollup_visibility(rdep_pkgs, pkg_path=""):
    """Collapses //src/* callers into //src:__subpackages__ while keeping narrow entries outside //src."""
    pkg_norm = (pkg_path or "").strip("/")
    raw_pkgs = sorted(
        {
            p.strip("/")
            for p in rdep_pkgs
            if p.strip("/") and p.strip("/") != pkg_norm
        }
    )
    if not raw_pkgs:
        return []
    has_src = any(p == "src" or p.startswith("src/") for p in raw_pkgs)
    non_src = [p for p in raw_pkgs if p != "src" and not p.startswith("src/")]
    res = set(recommend_narrow_visibility(non_src, pkg_norm))
    if has_src:
        res.add("//src:__subpackages__")
    return sorted(res)


def is_test_or_bench_caller_pkg(pkg):
    return any(
        seg in TEST_OR_BENCH_CALLER_SEGMENTS
        or seg.endswith(TEST_OR_BENCH_CALLER_SUFFIXES)
        for seg in pkg.strip("/").split("/")
        if seg
    )


def has_only_test_or_bench_rdeps(rdep_pkgs, pkg_path=""):
    pkg_norm = (pkg_path or "").strip("/")
    pkgs = [
        p.strip("/")
        for p in rdep_pkgs
        if p.strip("/")
        and p.strip("/") != pkg_norm
        and p.strip("/") != VG_ROOT
        and not p.strip("/").startswith(VG_ROOT + "/")
    ]
    return bool(pkgs) and all(is_test_or_bench_caller_pkg(p) for p in pkgs)


def get_orig_gn_visibility_scopes(workdir, pkg_path, change_base=None):
    """Returns {target_name: [(scope_pkg, 'pkg'|'subpackages'), ...]} for targets in pre-migration
    BUILD.gn (above BAZEL2GN SENTINEL) that had an explicit non-wildcard visibility = [...] restriction.
    """
    pkg_norm = (pkg_path or "").strip("/")
    gn_rel = f"{pkg_norm}/BUILD.gn" if pkg_norm else "BUILD.gn"
    eff_base = (
        change_base
        or os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip()
        or os.environ.get("PLANTER_CHANGE_BASE", "").strip()
        or "HEAD"
    )

    def parse_gn_vis_list(vis_body):
        if "$" in vis_body:
            return None
        pats = re.findall(r'"([^"]+)"', vis_body)
        if not pats:
            return None
        scopes = []
        for pat in pats:
            if pat in ("*", "//*"):
                return None
            if pat.startswith(":"):
                scopes.append((pkg_norm, "pkg"))
                continue
            if not pat.startswith("//"):
                rel_p = pat
                while rel_p.startswith("./"):
                    rel_p = rel_p[2:]
                pat = f"//{pkg_norm}/{rel_p}" if pkg_norm else f"//{rel_p}"
            m_gn = re.match(r"^//([^:*]*?)/?(\*|:.*)?$", pat)
            if not m_gn:
                return None
            sp = os.path.normpath(m_gn.group(1).strip("/"))
            if sp == ".":
                sp = ""
            if sp == ".." or sp.startswith("../"):
                return None
            suffix = m_gn.group(2) or ""
            scopes.append((sp, "subpackages" if suffix == "*" else "pkg"))
        return scopes

    out = {}
    target_re = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{')
    for ref in dict.fromkeys(["origin/main", "HEAD~1", eff_base, "HEAD"]):
        if not ref:
            continue
        try:
            gn_src = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{ref}:{gn_rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except Exception:
            continue
        raw_above, _ = pcl.split_sentinel(gn_src)
        above_sentinel = re.sub(r"#[^\n]*", "", raw_above)
        tmpl_vis_map = {}
        target_blocks = []
        for m in target_re.finditer(above_sentinel):
            tmpl_kind, tname = m.group(1), m.group(2)
            close_idx = pcl.find_closing_delim(
                above_sentinel, m.end() - 1, "{", "}"
            )
            tbody = above_sentinel[m.end() : close_idx]
            if tmpl_kind == "template":
                if not re.search(r"\bvisibility\s*\+=", tbody):
                    vm = re.search(r"\bvisibility\s*=\s*\[([^\]]*)\]", tbody)
                    if vm:
                        scopes = parse_gn_vis_list(vm.group(1))
                        if scopes:
                            tmpl_vis_map[tname] = scopes
                continue
            target_blocks.append((tmpl_kind, tname, tbody))
        for tmpl_kind, tname, tbody in target_blocks:
            if tname in out or re.search(r"\bvisibility\s*\+=", tbody):
                continue
            vm = re.search(r"\bvisibility\s*=\s*\[([^\]]*)\]", tbody)
            scopes = (
                parse_gn_vis_list(vm.group(1))
                if vm
                else tmpl_vis_map.get(tmpl_kind)
            )
            if scopes:
                unrestricted_fwd = False
                for gm in re.finditer(
                    r'\bgroup\(\s*"([^"$]+)"\s*\)\s*\{([^{}]*)\}',
                    above_sentinel,
                ):
                    gbody = gm.group(2)
                    if (
                        not re.search(r"\btestonly\s*=\s*true\b", gbody)
                        and not re.search(r"\bvisibility\s*=", gbody)
                        and re.search(
                            r'":' + re.escape(tname) + r'(?:\([^)]*\))?"', gbody
                        )
                    ):
                        unrestricted_fwd = True
                        break
                if not unrestricted_fwd:
                    out[tname] = list(scopes)
    return out


_GN_NON_LIBRARY_VIS_RULES = frozenset(
    {
        "test",
        "cc_test_executable",
        "rustc_test",
        "go_test",
        "python_host_test",
        "host_test",
        "host_test_data",
        "fuchsia_unittest_package",
        "fuchsia_test_package",
        "fuchsia_unittest_component",
        "fuchsia_test_component",
        "fuchsia_package",
        "fuchsia_component",
        "fuchsia_component_manifest",
        "fuchsia_package_with_single_component",
        "resource",
    }
)


def is_public_sdk_or_ulib_pkg(pkg_path):
    """Returns True for SDK and Zircon ulib library packages intended for tree-wide callers."""
    p = (pkg_path or "").strip("/")
    return p.startswith(("sdk/lib/", "zircon/system/ulib/"))


def get_orig_gn_unrestricted_targets(workdir, pkg_path, change_base=None):
    """Returns set of target names in pre-migration BUILD.gn (above BAZEL2GN SENTINEL)
    that had unrestricted GN visibility (omitted visibility, `["*"]`/`["//*"]`, or re-exported
    by an unrestricted group), excluding test/package/component leaf rules.
    """
    pkg_norm = (pkg_path or "").strip("/")
    gn_rel = f"{pkg_norm}/BUILD.gn" if pkg_norm else "BUILD.gn"
    eff_base = (
        change_base
        or os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip()
        or os.environ.get("PLANTER_CHANGE_BASE", "").strip()
        or "HEAD"
    )
    restricted_map = get_orig_gn_visibility_scopes(
        workdir, pkg_norm, change_base=change_base
    )
    unrestricted = set()
    target_re = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{')
    for ref in dict.fromkeys(["origin/main", "HEAD~1", eff_base, "HEAD"]):
        if not ref:
            continue
        try:
            gn_src = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{ref}:{gn_rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except Exception:
            continue
        raw_above, _ = pcl.split_sentinel(gn_src)
        above_sentinel = re.sub(r"#[^\n]*", "", raw_above)
        tmpl_restricted = set()
        target_blocks = []
        for m in target_re.finditer(above_sentinel):
            tmpl_kind, tname = m.group(1), m.group(2)
            close_idx = pcl.find_closing_delim(
                above_sentinel, m.end() - 1, "{", "}"
            )
            tbody = above_sentinel[m.end() : close_idx]
            if tmpl_kind == "template":
                if not re.search(r"\bvisibility\s*\+=", tbody):
                    vm = re.search(r"\bvisibility\s*=\s*\[([^\]]*)\]", tbody)
                    if vm:
                        pats = re.findall(r'"([^"]+)"', vm.group(1))
                        if (
                            pats
                            and "*" not in pats
                            and "//*" not in pats
                            and "$" not in vm.group(1)
                        ):
                            tmpl_restricted.add(tname)
                continue
            target_blocks.append((tmpl_kind, tname, tbody))
        for tmpl_kind, tname, tbody in target_blocks:
            if tmpl_kind in _GN_NON_LIBRARY_VIS_RULES:
                continue
            if tmpl_kind == "group" and tname in (
                "tests",
                "benchmarks",
                "hermetic_tests",
            ):
                continue
            if tname in restricted_map:
                continue
            has_plus_eq = bool(re.search(r"\bvisibility\s*\+=", tbody))
            vm = re.search(r"\bvisibility\s*=\s*\[([^\]]*)\]", tbody)
            is_unrestricted = False
            if has_plus_eq:
                is_unrestricted = True
            elif vm:
                pats = re.findall(r'"([^"]+)"', vm.group(1))
                if "*" in pats or "//*" in pats:
                    is_unrestricted = True
            elif tmpl_kind not in tmpl_restricted:
                is_unrestricted = True
            if is_unrestricted:
                unrestricted.add(tname)
                if tmpl_kind == "zx_library" and tname == "lib" and pkg_norm:
                    pkg_leaf = os.path.basename(pkg_norm)
                    if pkg_leaf not in restricted_map:
                        unrestricted.add(pkg_leaf)
    return unrestricted - set(restricted_map.keys())


def get_preserved_gn_visibility(
    workdir,
    tname,
    rdep_pkgs,
    pkg_path,
    orig_gn_vis_map,
    vis_only_pkgs=(),
    gn_only_pkgs=(),
    non_gn_synced_targets=(),
):
    """Converts explicit narrow GN visibility from pre-migration BUILD.gn into Bazel visibility labels."""
    if not tname or not orig_gn_vis_map or tname in non_gn_synced_targets:
        return None
    gn_scopes = orig_gn_vis_map.get(tname)
    if not gn_scopes:
        return None
    pkg_norm = (pkg_path or "").strip("/")
    pkgs = sorted(
        {
            p.strip("/")
            for p in rdep_pkgs
            if p.strip("/") and p.strip("/") != pkg_norm
        }
    )
    has_vg = any(p == VG_ROOT or p.startswith(VG_ROOT + "/") for p in pkgs)
    valid_scopes = []
    for sp, kind in gn_scopes:
        if sp == pkg_norm and kind == "pkg":
            continue
        if sp == VG_ROOT or sp.startswith(VG_ROOT + "/"):
            if has_vg:
                valid_scopes.append((VG_ROOT, "subpackages"))
            continue
        if kind == "subpackages":
            if (
                not sp
                or sp in BANNED_TOP_LEVEL_SUBPKGS
                or sp in BANNED_UMBRELLA_SUBPKGS
            ):
                return None
            parts = sp.split("/")
            if (
                len(parts) == 2
                and parts[0] in ("src", "sdk")
                and not (pkg_norm == sp or pkg_norm.startswith(sp + "/"))
            ):
                return None
            if not os.path.isdir(os.path.join(workdir, sp)):
                continue
            valid_scopes.append((sp, "subpackages"))
        else:
            if (
                not sp
                or not os.path.isdir(os.path.join(workdir, sp))
                or not (
                    os.path.isfile(os.path.join(workdir, sp, "BUILD.bazel"))
                    or os.path.isfile(os.path.join(workdir, sp, "BUILD"))
                )
                or sp in vis_only_pkgs
                or sp in gn_only_pkgs
            ):
                continue
            valid_scopes.append((sp, "pkg"))
    if not valid_scopes:
        return None
    sub_roots = {sp for sp, k in valid_scopes if k == "subpackages"}
    preserved = set()
    for sp, kind in valid_scopes:
        if kind == "subpackages":
            if not any(sp != o and sp.startswith(o + "/") for o in sub_roots):
                preserved.add(
                    VG_SUBPKGS if sp == VG_ROOT else f"//{sp}:__subpackages__"
                )
        elif not any(sp == o or sp.startswith(o + "/") for o in sub_roots):
            preserved.add(f"//{sp}:__pkg__")
    uncovered = [
        r
        for r in pkgs
        if not any(
            r == gsp
            or (gk == "subpackages" and (gsp == "" or r.startswith(gsp + "/")))
            for gsp, gk in valid_scopes
        )
    ]
    if uncovered:
        preserved.update(recommend_narrow_visibility(uncovered, pkg_norm))
    if not preserved or len(preserved) > 15:
        return None
    return sorted(preserved)


def recommend_visibility(
    rdep_pkgs,
    pkg_path="",
    is_test_support=False,
    target_name=None,
    orig_gn_vis_map=None,
    vis_only_pkgs=(),
    gn_only_pkgs=(),
    non_gn_synced_targets=(),
    workdir=None,
    orig_gn_unrestricted_targets=None,
    idk_targets=(),
):
    pkg_norm = (pkg_path or "").strip("/")
    pkgs = sorted(
        {
            p.strip("/")
            for p in rdep_pkgs
            if p.strip("/") and p.strip("/") != pkg_norm
        }
    )
    narrow = recommend_narrow_visibility(pkgs, pkg_norm)
    globally_used = is_globally_used_target(pkgs, pkg_norm)
    if (
        target_name
        and target_name not in non_gn_synced_targets
        and orig_gn_unrestricted_targets
        and target_name in orig_gn_unrestricted_targets
        and (target_name in idk_targets or is_public_sdk_or_ulib_pkg(pkg_norm))
    ):
        return ["//:__subpackages__"]
    if target_name and not globally_used and orig_gn_vis_map and workdir:
        preserved = get_preserved_gn_visibility(
            workdir,
            target_name,
            pkgs,
            pkg_norm,
            orig_gn_vis_map,
            vis_only_pkgs=vis_only_pkgs,
            gn_only_pkgs=gn_only_pkgs,
            non_gn_synced_targets=non_gn_synced_targets,
        )
        if preserved:
            return preserved
    is_shared_test_bench = is_test_support or (
        target_name is not None
        and target_name not in non_gn_synced_targets
        and has_only_test_or_bench_rdeps(pkgs, pkg_norm)
    )
    distinct_areas = get_distinct_areas(pkgs, pkg_norm)
    if is_shared_test_bench and len(distinct_areas) >= 2:
        return ["//:__subpackages__"]
    if globally_used or len(narrow) > 8:
        rollup = recommend_area_rollup_visibility(pkgs, pkg_norm)
        if (
            globally_used
            and len(rollup) > 15
            and (len(pkgs) >= 50 or len(distinct_areas) > 15)
        ):
            if len(pkgs) >= 50:
                return ["//:__subpackages__"]
            src_collapsed = collapse_src_rollup_visibility(pkgs, pkg_norm)
            return (
                src_collapsed
                if len(src_collapsed) <= 15
                else ["//:__subpackages__"]
            )
        if len(rollup) <= len(narrow):
            return rollup
    return narrow


def discover_git_repos(workdir):
    """Discovers all git repositories in the checkout (root and vendor sub-repositories)."""
    repos = [workdir]
    sub_root = os.path.join(workdir, VG_ROOT.split("/")[0])
    if os.path.isdir(sub_root):
        try:
            for entry in sorted(os.listdir(sub_root)):
                sub = os.path.join(sub_root, entry)
                if os.path.exists(os.path.join(sub, ".git")):
                    repos.append(sub)
        except OSError:
            pass
    return repos


def sso_https_git_config_args(repo):
    """Builds git config arguments rewriting sso:// remotes to https:// when needed."""
    try:
        url = subprocess.check_output(
            ["git", "-C", repo, "config", "--get", "remote.origin.url"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
    except Exception:
        return []
    if not url.startswith("sso://"):
        return []
    host = url[len("sso://") :].split("/", 1)[0].strip()
    if not host:
        return []
    return [
        "-c",
        f"url.https://{host}.googlesource.com/.insteadOf=sso://{host}/",
    ]


def maybe_refresh_repo_origin_main(repo, ttl_secs=FETCH_TTL_SECS):
    """Refreshes origin/main at most once per TTL unless PLANTER_SKIP_FETCH is set."""
    if os.environ.get("PLANTER_SKIP_FETCH", "").strip() not in ("", "0"):
        return
    if os.environ.get("PLANTER_AGENT_ROLE", "").strip() == "evolver":
        return
    stamp_key = hashlib.sha256(
        os.path.abspath(repo).encode("utf-8")
    ).hexdigest()[:16]
    stamp_path = os.path.join(
        tempfile.gettempdir(), f"planter-origin-main-fetch-{stamp_key}.stamp"
    )
    now = int(time.time())
    try:
        if os.path.isfile(stamp_path):
            if time.time() - os.path.getmtime(stamp_path) < ttl_secs:
                return
            with open(stamp_path, "r", encoding="utf-8") as sf:
                parts = sf.read().strip().split()
            if parts and (now - int(parts[0])) < ttl_secs:
                return
    except (OSError, ValueError):
        pass

    fetch_ok = False
    try:
        cfg_args = sso_https_git_config_args(repo)
        proc = subprocess.run(
            ["git", "-C", repo]
            + cfg_args
            + [
                "fetch",
                "--quiet",
                "--no-tags",
                "origin",
                "+refs/heads/main:refs/remotes/origin/main",
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=30,
            check=False,
        )
        fetch_ok = proc.returncode == 0
    except Exception:
        pass
    if fetch_ok:
        try:
            sha = subprocess.check_output(
                [
                    "git",
                    "-C",
                    repo,
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "origin/main",
                ],
                stderr=subprocess.DEVNULL,
                text=True,
            ).strip()
        except Exception:
            sha = ""
        try:
            with open(stamp_path, "w", encoding="utf-8") as sf:
                sf.write(f"{now} {sha}".strip() + "\n")
        except OSError:
            pass


def is_subrepo_origin_main_stale(workdir, sub_rel):
    sub_repo = os.path.join(workdir, sub_rel)
    if not os.path.exists(os.path.join(sub_repo, ".git")):
        return False
    try:
        root_ts = int(
            subprocess.check_output(
                ["git", "-C", workdir, "log", "-1", "--format=%ct", "HEAD"],
                stderr=subprocess.DEVNULL,
                text=True,
            ).strip()
        )
        sub_ts = int(
            subprocess.check_output(
                [
                    "git",
                    "-C",
                    sub_repo,
                    "log",
                    "-1",
                    "--format=%ct",
                    "origin/main",
                ],
                stderr=subprocess.DEVNULL,
                text=True,
            ).strip()
        )
        return (root_ts - sub_ts) > 86400
    except Exception:
        return False


_upstream_only_files_cache = {}


def get_upstream_only_file_filter(repo, change_base=None):
    """Returns a filter for files that only changed upstream in `repo`.

    The returned function `is_upstream_only_file(rel_path)` is True only when
    `rel_path` was modified upstream on `origin/main` since
    `merge-base(HEAD, origin/main)` and was not modified locally in the current
    branch or working tree. This prevents pre-migration dependency labels
    removed or updated by the current change from being resurrected from
    `origin/main`.
    """
    if repo in _upstream_only_files_cache:
        return _upstream_only_files_cache[repo]
    eff_base = (
        change_base
        or os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip()
        or os.environ.get("PLANTER_CHANGE_BASE", "").strip()
        or "HEAD"
    )
    local_changed = set()
    for cmd in (
        ["git", "-C", repo, "diff", "--name-only", "origin/main...HEAD"],
        ["git", "-C", repo, "diff", "--name-only", eff_base],
        ["git", "-C", repo, "diff", "--name-only", "HEAD"],
        ["git", "-C", repo, "diff", "--cached", "--name-only"],
    ):
        try:
            out = subprocess.check_output(
                cmd, stderr=subprocess.DEVNULL, text=True
            )
            for line in out.splitlines():
                if line.strip():
                    local_changed.add(line.strip())
        except Exception:
            pass
    try:
        up_out = subprocess.check_output(
            ["git", "-C", repo, "diff", "--name-only", "HEAD...origin/main"],
            stderr=subprocess.DEVNULL,
            text=True,
        )
        upstream_only = {
            line.strip() for line in up_out.splitlines() if line.strip()
        } - local_changed
        fn = lambda rel_p, u=upstream_only: rel_p in u
    except Exception:
        fn = lambda rel_p, lc=local_changed: rel_p not in lc
    _upstream_only_files_cache[repo] = fn
    return fn


# Collect descendant build files under `pkg_clean` (bounded by `max_depth`) that may
# reference targets in `pkg_clean` via local or parent-relative (`..`) labels.
def _collect_descendant_build_files(workdir, pkg_clean, max_depth=4):
    """Collects build files under `workdir/pkg_clean` that may reference `//pkg_clean`."""
    descendants = set()
    p_clean = (pkg_clean or "").strip("/")
    if not p_clean:
        return descendants
    pkg_dir = os.path.join(workdir, p_clean)
    if not os.path.isdir(pkg_dir):
        return descendants
    skip_dirs = {".git", ".hg", ".jiri_root", "out", "__pycache__"}
    norm_pkg_dir = os.path.normpath(pkg_dir)
    base_depth = p_clean.count("/")
    for dirpath, dirnames, filenames in os.walk(pkg_dir, followlinks=False):
        rel_dir = (
            os.path.relpath(dirpath, workdir).replace("\\", "/").strip("/")
        )
        cur_depth = rel_dir.count("/") - base_depth
        if cur_depth >= max_depth:
            dirnames[:] = []
        else:
            dirnames[:] = [
                d
                for d in dirnames
                if d not in skip_dirs and not d.startswith(".")
            ]
        is_pkg_root = os.path.normpath(dirpath) == norm_pkg_dir
        for fname in filenames:
            if fname not in (
                "BUILD.gn",
                "BUILD.bazel",
                "BUILD",
            ) and not fname.endswith((".gni", ".bzl")):
                continue
            fpath = os.path.join(dirpath, fname)
            if not os.path.isfile(fpath):
                continue
            if is_pkg_root:
                descendants.add(fpath)
                continue
            try:
                with open(fpath, "r", encoding="utf-8", errors="ignore") as f:
                    if ".." in f.read():
                        descendants.add(fpath)
            except OSError:
                continue
    return descendants


def discover_referencing_build_files(
    workdir,
    pkg_list,
    candidate_files=(),
    change_base=None,
    origin_main_sources=None,
    refresh_origin_main=False,
):
    """Batch-discovers all build files mentioning any package in `pkg_list` across all repos."""
    found = {
        os.path.join(workdir, rel)
        for rel in candidate_files
        if os.path.isfile(os.path.join(workdir, rel))
    }
    if not pkg_list:
        return found
    eff_base = (
        change_base
        or os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip()
        or os.environ.get("PLANTER_CHANGE_BASE", "").strip()
        or "HEAD"
    )
    search_terms = set()
    for p in pkg_list:
        p_clean = p.strip("/")
        if not p_clean:
            continue
        search_terms.add(f"//{p_clean}:")
        search_terms.add(f'//{p_clean}"')
        search_terms.add(f"//{p_clean}(")
        p_parts = [seg for seg in p_clean.split("/") if seg]
        for i in range(1, len(p_parts)):
            rel_p = "../" + "/".join(p_parts[i:])
            search_terms.add(f"{rel_p}:")
            search_terms.add(f'{rel_p}"')
            search_terms.add(f"{rel_p}(")
    if not search_terms:
        return found
    pattern_args = [
        arg for term in sorted(search_terms) for arg in ("-e", term)
    ]
    repos = discover_git_repos(workdir)
    used_git_grep = False
    for repo in repos:
        if not os.path.exists(os.path.join(repo, ".git")):
            continue
        if refresh_origin_main:
            maybe_refresh_repo_origin_main(repo)
        grep_cmd = (
            ["git", "-C", repo, "grep", "--untracked", "-l", "--fixed-strings"]
            + pattern_args
            + ["--", "*BUILD.gn", "*BUILD.bazel", "*.gni", "*.bzl"]
        )
        try:
            proc = subprocess.run(
                grep_cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
            )
            if proc.returncode in (0, 1):
                used_git_grep = True
            for line in proc.stdout.splitlines():
                if line.strip():
                    found.add(os.path.join(repo, line.strip()))
        except OSError:
            pass

        if origin_main_sources is not None:
            # Also search origin/main so callers added upstream in a
            # sub-repository whose working tree is at an older Jiri snapshot
            # are found, excluding files modified locally by the migration
            # change.
            is_upstream_only = get_upstream_only_file_filter(repo, eff_base)
            om_grep_cmd = (
                ["git", "-C", repo, "grep", "-l", "--fixed-strings"]
                + pattern_args
                + [
                    "origin/main",
                    "--",
                    "*BUILD.gn",
                    "*BUILD.bazel",
                    "*.gni",
                    "*.bzl",
                ]
            )
            try:
                om_proc = subprocess.run(
                    om_grep_cmd,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
                if om_proc.returncode == 0:
                    for line in om_proc.stdout.splitlines():
                        s = line.strip()
                        if s.startswith("origin/main:"):
                            rel_line = s[len("origin/main:") :]
                            if not is_upstream_only(rel_line):
                                continue
                            abs_cand = os.path.join(repo, rel_line)
                            origin_main_sources[abs_cand] = (repo, rel_line)
                            found.add(abs_cand)
            except OSError:
                pass

        for diff_cmd in (
            ["git", "-C", repo, "diff", "--name-only", eff_base],
            ["git", "-C", repo, "diff", "--cached", "--name-only"],
            [
                "git",
                "-C",
                repo,
                "ls-files",
                "--others",
                "--exclude-standard",
                "--",
                "*BUILD.bazel",
                "*BUILD.gn",
                "*.gni",
                "*.bzl",
            ],
        ):
            try:
                out = subprocess.check_output(
                    diff_cmd, stderr=subprocess.DEVNULL, text=True
                )
                for line in out.splitlines():
                    rel_line = line.strip()
                    if not rel_line:
                        continue
                    direct_cand = os.path.join(repo, rel_line)
                    if rel_line.endswith(
                        ("BUILD.bazel", "BUILD.gn", ".gni", ".bzl")
                    ) and os.path.isfile(direct_cand):
                        found.add(direct_cand)
                    d = os.path.dirname(rel_line)
                    for bname in ("BUILD.bazel", "BUILD.gn"):
                        cand = (
                            os.path.join(repo, d, bname)
                            if d
                            else os.path.join(repo, bname)
                        )
                        if os.path.isfile(cand):
                            found.add(cand)
            except Exception:
                pass

    if not used_git_grep:
        rg_bin = shutil.which(
            "rg",
            path=os.environ.get("PATH", "")
            + os.pathsep
            + os.path.expanduser("~/.cargo/bin"),
        )
        if rg_bin:
            rg_globs = [
                "-g",
                "*BUILD.gn",
                "-g",
                "*BUILD.bazel",
                "-g",
                "*.gni",
                "-g",
                "*.bzl",
                "-g",
                "!out/**",
            ]
            if ".jiri_root" not in workdir:
                rg_globs.extend(["-g", "!.jiri_root/**"])
            rg_cmd = (
                [
                    rg_bin,
                    "-l",
                    "--no-ignore-parent",
                    "--no-require-git",
                    "--fixed-strings",
                ]
                + rg_globs
                + pattern_args
                + repos
            )
            try:
                proc = subprocess.run(
                    rg_cmd,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
                if proc.returncode in (0, 1):
                    for line in proc.stdout.splitlines():
                        if line.strip():
                            found.add(line.strip())
            except OSError:
                pass

    for p in pkg_list:
        p_clean = p.strip("/")
        parts = [seg for seg in p_clean.split("/") if seg]
        for i in range(1, len(parts)):
            anc_gn = os.path.join(workdir, "/".join(parts[:i]), "BUILD.gn")
            if os.path.isfile(anc_gn):
                found.add(anc_gn)
        found.update(_collect_descendant_build_files(workdir, p_clean))
    return found


def _blank_match(m):
    return re.sub(r"[^\n]", " ", m.group(0))


def _blank_spans(text, spans):
    """Replaces `(start_offset, end_offset)` inclusive spans with spaces, preserving offsets."""
    if not spans:
        return text
    merged = []
    for s, e in sorted(spans):
        if merged and s <= merged[-1][1] + 1:
            merged[-1] = (merged[-1][0], max(merged[-1][1], e))
        else:
            merged.append((s, e))
    out = []
    pos = 0
    n = len(text)
    for s, e in merged:
        end = min(n, e + 1)
        if s > pos:
            out.append(text[pos:s])
        if end > s:
            out.append(" " * (end - s))
        pos = max(pos, end)
    if pos < n:
        out.append(text[pos:])
    return "".join(out)


def _blank_non_interp_string_contents(text):
    """Blanks non-interpolated string literal contents so prose/paths never match identifiers."""
    if not text:
        return ""
    out = []
    i = 0
    n = len(text)
    while i < n:
        c = text[i]
        if c in ('"', "'"):
            q = c
            out.append(q)
            i += 1
            while i < n:
                sc = text[i]
                if sc == "\\" and i + 1 < n:
                    out.append("  ")
                    i += 2
                    continue
                if sc == q:
                    out.append(q)
                    i += 1
                    break
                if q == '"' and sc == "$" and i + 1 < n:
                    nxt = text[i + 1]
                    if nxt == "{":
                        close_b = text.find("}", i + 2)
                        if close_b != -1:
                            out.append(text[i : close_b + 1])
                            i = close_b + 1
                            continue
                    elif nxt.isalpha() or nxt == "_":
                        m_id = re.match(r"[A-Za-z_]\w*", text[i + 1 :])
                        tok = m_id.group(0)
                        out.append("$" + tok)
                        i += 1 + len(tok)
                        continue
                out.append(" ")
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


def _uses_any_ident(code, idents):
    """Returns True if `code` references any top-level/interpolated identifier in `idents`."""
    return bool(idents) and any(
        re.search(r"(?<![.\w])" + re.escape(ident) + r"\b", code)
        for ident in idents
    )


def _expand_gn_label_variants(raw_lbl):
    """Returns normalized GN label variants (with and without `:default_target` or toolchain)."""
    s = raw_lbl.strip().strip('"').strip("'")
    if "(" in s:
        s = s.split("(", 1)[0].strip()
    if not s.startswith("//"):
        return {s} if s else set()
    variants = {s}
    pkg_part, sep, tgt_part = s[2:].partition(":")
    pkg_clean = pkg_part.strip("/")
    default_t = os.path.basename(pkg_clean)
    if sep:
        variants.add(f"//{pkg_clean}:{tgt_part}")
        if tgt_part == default_t:
            variants.add(f"//{pkg_clean}")
    else:
        variants.add(f"//{pkg_clean}")
        if default_t:
            variants.add(f"//{pkg_clean}:{default_t}")
    return variants


def _is_gn_label_excluded(raw_lbl, excluded_labels):
    """Returns True if `raw_lbl` matches any entry or package wildcard in `excluded_labels`."""
    if not excluded_labels:
        return False
    if _expand_gn_label_variants(raw_lbl) & excluded_labels:
        return True
    s = raw_lbl.strip().strip('"').strip("'")
    if "(" in s:
        s = s.split("(", 1)[0].strip()
    if s.startswith("//"):
        pkg_clean = s[2:].partition(":")[0].strip("/")
        if (
            f"//{pkg_clean}:*" in excluded_labels
            or f"//{pkg_clean}/*" in excluded_labels
        ):
            return True
    return False


def _extract_excluded_gn_labels(no_comments):
    """Returns frozenset of GN label variants listed in assert_no_deps / _?excluded_* lists."""
    if "excluded_" not in no_comments and "assert_no_deps" not in no_comments:
        return frozenset()
    excluded = set()
    for excl_m in _EXCLUDED_DEP_LIST_RE.finditer(no_comments):
        for raw_excl in re.findall(r'"([^"]+)"', excl_m.group(1)):
            excluded |= _expand_gn_label_variants(raw_excl)
    return frozenset(excluded)


def strip_non_dep_lists(no_comments, is_gn):
    """Replaces non-dependency attributes/constructs with spaces while preserving offsets."""
    stripped = _NON_DEP_LIST_RE.sub(_blank_match, no_comments)
    if not is_gn:
        return stripped
    stripped = _GN_NON_DEP_VAR_RE.sub(_blank_match, stripped)
    stripped = _GN_MEMBERSHIP_CHECK_RE.sub(_blank_match, stripped)
    for _ in range(3):
        stripped = re.sub(
            r"\bforeach\s*\([^)]*\)\s*\{\s*\}"
            r"|(?<!\belse\s)\bif\s*\([^)]*\)\s*\{\s*\}(?!\s*else\b)",
            _blank_match,
            stripped,
        )
    out = []
    pos = 0
    for m in _BAZEL_TEST_SUITE_OPEN_RE.finditer(stripped):
        start = m.end()
        if start < pos:
            continue
        depth, i = 1, start
        while i < len(stripped) and depth > 0:
            if stripped[i] == "{":
                depth += 1
            elif stripped[i] == "}":
                depth -= 1
            i += 1
        out.append(stripped[pos:start])
        out.append(_SUITE_LIST_RE.sub(_blank_match, stripped[start:i]))
        pos = i
    out.append(stripped[pos:])
    return "".join(out)


def build_file_cache(matched_files, origin_main_sources=None):
    """Reads and strips comments/non-dep lists for all `matched_files`."""
    file_cache = {}
    om_map = origin_main_sources or {}
    for fpath in sorted(set(matched_files)):
        try:
            parts = []
            if os.path.isfile(fpath):
                with open(fpath, "r", encoding="utf-8", errors="ignore") as f:
                    parts.append(f.read())
            om_info = om_map.get(fpath)
            if om_info:
                repo, rel_line = om_info
                try:
                    om_raw = subprocess.check_output(
                        ["git", "-C", repo, "show", f"origin/main:{rel_line}"],
                        stderr=subprocess.DEVNULL,
                        text=True,
                    )
                    if om_raw and (not parts or om_raw != parts[0]):
                        parts.append(om_raw)
                except Exception:
                    pass
            raw = "\n".join(parts)
            if not raw:
                continue
            no_comments = re.sub(r"#[^\n]*", "", raw)
            file_cache[fpath] = (
                no_comments,
                strip_non_dep_lists(
                    no_comments, fpath.endswith((".gn", ".gni"))
                ),
            )
        except Exception:
            pass
    return file_cache


_GN_CONTROL_AND_TARGET_FUNCS = frozenset(
    {
        "if",
        "else",
        "foreach",
        "template",
        "declare_args",
        "assert",
        "not_needed",
        "import",
        "set_defaults",
        "pool",
        "toolchain",
        "target",
        "action",
        "action_foreach",
        "copy",
        "executable",
        "group",
        "loadable_module",
        "shared_library",
        "source_set",
        "static_library",
        "rust_library",
        "rust_proc_macro",
        "rustc_binary",
        "rustc_library",
        "rustc_macro",
        "rustc_test",
        "fidl",
        "fidl_library",
    }
)


def parse_gni_template_blocks(no_comments):
    """Returns {template_name: (start_offset, end_offset, body_str)} for template() blocks."""
    tmpls = {}
    for m in re.finditer(r'\btemplate\(\s*"([^"$]+)"\s*\)\s*\{', no_comments):
        close_idx = pcl.find_closing_delim(no_comments, m.end() - 1, "{", "}")
        tmpls[m.group(1)] = (
            m.start(),
            close_idx,
            no_comments[m.end() : close_idx],
        )
    return tmpls


def _parse_gni_top_vars(text, tmpls=None):
    """Returns `(var_spans, rhs_deps)` for top-level variable assignments outside templates.

    `var_spans`: `{var_name: [(rhs_start, rhs_end), ...]}` in `text` character offsets.
    `rhs_deps`: `{var_name: set_of_referenced_identifiers}` on the RHS of `var_name`.
    """
    top_text = _blank_spans(
        text, [(s, e) for _, (s, e, _) in (tmpls or {}).items()]
    )
    search_top = _blank_non_interp_string_contents(top_text)
    target_block_spans = []
    pos_scan = 0
    n_top = len(search_top)
    for cm in re.finditer(r"(?<![.\w=+\-])\b([A-Za-z_]\w*)\s*\(", search_top):
        if cm.start() < pos_scan:
            continue
        line_start = search_top.rfind("\n", 0, cm.start()) + 1
        if search_top[line_start : cm.start()].rstrip().endswith("="):
            continue
        close_p = pcl.find_closing_delim(search_top, cm.end() - 1, "(", ")")
        j = close_p + 1
        while j < n_top and search_top[j].isspace():
            j += 1
        if j < n_top and search_top[j] == "{":
            close_b = pcl.find_closing_delim(search_top, j, "{", "}")
            if cm.group(1) not in ("if", "foreach", "declare_args"):
                target_block_spans.append((cm.start(), close_b))
                pos_scan = close_b + 1

    var_code = _blank_spans(search_top, target_block_spans)
    var_spans = {}
    rhs_deps = {}
    n = len(var_code)
    scan_pos = 0
    for vm in re.finditer(
        r"(?<![!<>=+\-*/.])\b([A-Za-z_]\w*)\s*(\+?=)(?!=)", var_code
    ):
        if vm.start() < scan_pos:
            continue
        vname = vm.group(1)
        idx = vm.end()
        depth = 0
        last_tok = ""
        rhs_tokens = set()
        while idx < n:
            ch = var_code[idx]
            if ch in " \t\r":
                idx += 1
                continue
            if ch == "\n":
                if depth > 0 or last_tok in (
                    "+",
                    "-",
                    "==",
                    "!=",
                    "<=",
                    ">=",
                    "&&",
                    "||",
                    "!",
                    "=",
                    "+=",
                ):
                    idx += 1
                    continue
                j = idx + 1
                while j < n and var_code[j] in " \t\r":
                    j += 1
                if j < n and var_code[j] in ("+", "-", "&", "|", "=", "!"):
                    idx = j
                    continue
                break
            if ch == "[":
                if depth == 0:
                    close_br = pcl.find_closing_delim(var_code, idx, "[", "]")
                    rhs_tokens.update(
                        re.findall(
                            r"\b([A-Za-z_]\w*)\b", var_code[idx:close_br]
                        )
                    )
                    rhs_tokens.discard(vname)
                    last_tok = "]"
                    idx = close_br + 1
                    continue
                depth += 1
                last_tok = "["
                idx += 1
                continue
            if ch in ("(", "{"):
                depth += 1
                last_tok = ch
                idx += 1
                continue
            if ch in (")", "}", "]"):
                if depth == 0:
                    break
                depth -= 1
                last_tok = ch
                idx += 1
                continue
            if ch.isalpha() or ch == "_":
                m_id = re.match(r"[A-Za-z_]\w*", var_code[idx:])
                ident = m_id.group(0)
                if ident != vname:
                    rhs_tokens.add(ident)
                last_tok = ident
                idx += len(ident)
                continue
            if var_code[idx : idx + 2] in ("==", "!=", "<=", ">=", "&&", "||"):
                last_tok = var_code[idx : idx + 2]
                idx += 2
                continue
            last_tok = ch
            idx += 1
        scan_pos = idx
        if (
            vname not in GN_TARGET_PARAM_VARS
            and vname not in _GN_CONTROL_AND_TARGET_FUNCS
        ):
            var_spans.setdefault(vname, []).append((vm.end(), idx))
            rhs_deps.setdefault(vname, set()).update(rhs_tokens)
    return var_spans, rhs_deps


def _resolve_gni_enclosing_scopes(pos, tmpls, var_spans, rhs_deps):
    """Returns `(frozenset(enc_tmpls), frozenset(enc_vars))` for match offset `pos` in a `.gni`."""
    direct_tmpls = {t for t, (s, e, _) in tmpls.items() if s <= pos <= e}
    direct_vars = {
        v
        for v, spans in var_spans.items()
        if any(s <= pos <= e for s, e in spans)
    }
    enc_vars = set(direct_vars)
    changed = True
    while changed:
        changed = False
        for v, deps in rhs_deps.items():
            if v not in enc_vars and (deps & enc_vars):
                enc_vars.add(v)
                changed = True

    enc_tmpls = set(direct_tmpls)
    if enc_vars:
        for t, (_, _, tbody) in tmpls.items():
            if t not in enc_tmpls:
                tbody_code = _blank_non_interp_string_contents(
                    strip_non_dep_lists(tbody, True)
                )
                if _uses_any_ident(tbody_code, enc_vars):
                    enc_tmpls.add(t)

    changed = True
    while changed:
        changed = False
        for t, (_, _, tbody) in tmpls.items():
            if t not in enc_tmpls and any(
                re.search(r"\b" + re.escape(c) + r"\s*\(", tbody)
                for c in enc_tmpls
            ):
                enc_tmpls.add(t)
                changed = True

    return frozenset(enc_tmpls), frozenset(enc_vars)


def _propagate_gni_downstream_scopes(
    imp_text, imp_dir, target_gni_rel, active_tmpls, active_vars
):
    """Computes `(top_direct_use, next_active_tmpls, next_active_vars)` when `imp_text` imports `target_gni_rel`."""
    cand_tmpls = parse_gni_template_blocks(imp_text)
    imp_stripped = strip_non_dep_lists(imp_text, True)
    imp_code = _blank_non_interp_string_contents(imp_stripped)
    cand_vars, cand_rhs = _parse_gni_top_vars(imp_stripped, cand_tmpls)

    top_level_import = False
    importing_tmpls = set()
    for m_imp in GN_IMPORT_RE.finditer(imp_text):
        imp_p = m_imp.group(1)
        resolved_p = (
            imp_p[2:].strip("/")
            if imp_p.startswith("//")
            else os.path.normpath(os.path.join(imp_dir, imp_p)).strip("/")
        )
        if resolved_p != target_gni_rel:
            continue
        enc_imp_t = {
            t for t, (s, e, _) in cand_tmpls.items() if s <= m_imp.start() <= e
        }
        if enc_imp_t:
            importing_tmpls |= enc_imp_t
        else:
            top_level_import = True

    cand_top_code = _blank_spans(
        imp_code, [(s, e) for _, (s, e, _) in cand_tmpls.items()]
    )
    cand_non_var_code = _blank_spans(
        cand_top_code,
        [sp for spans in cand_vars.values() for sp in spans],
    )
    top_direct_use = top_level_import and (
        (
            bool(active_tmpls)
            and any(
                re.search(r"\b" + re.escape(t) + r"\s*\(", cand_non_var_code)
                for t in active_tmpls
            )
        )
        or (
            bool(active_vars)
            and _uses_any_ident(cand_non_var_code, active_vars)
        )
    )
    if top_direct_use:
        return True, None, None

    wanted = set(active_tmpls or ()) | set(active_vars or ())
    next_vars = set()
    if top_level_import:
        next_vars = {
            vn
            for vn, spans in cand_vars.items()
            if any(_uses_any_ident(imp_code[vs:ve], wanted) for vs, ve in spans)
        }
        changed_v = True
        while changed_v:
            changed_v = False
            for vn, deps in cand_rhs.items():
                if vn not in next_vars and (deps & next_vars):
                    next_vars.add(vn)
                    changed_v = True

    wanted_in_tmpl = wanted | next_vars
    cand_tmpl_pool = cand_tmpls.keys() if top_level_import else importing_tmpls
    wrappers = set()
    for t in cand_tmpl_pool:
        if t in cand_tmpls:
            tbody = cand_tmpls[t][2]
            tbody_code = _blank_non_interp_string_contents(
                strip_non_dep_lists(tbody, True)
            )
            if _uses_any_ident(tbody_code, wanted_in_tmpl) or (
                active_tmpls
                and any(
                    re.search(r"\b" + re.escape(c) + r"\s*\(", tbody_code)
                    for c in active_tmpls
                )
            ):
                wrappers.add(t)

    changed_t = True
    while changed_t:
        changed_t = False
        for t, (_, _, tbody) in cand_tmpls.items():
            if t not in wrappers and any(
                re.search(r"\b" + re.escape(c) + r"\s*\(", tbody)
                for c in wrappers
            ):
                wrappers.add(t)
                changed_t = True

    if top_level_import:
        next_active_tmpls = (
            frozenset(set(active_tmpls or ()) | wrappers)
            if (active_tmpls or wrappers)
            else None
        )
        next_active_vars = (
            frozenset(set(active_vars or ()) | next_vars)
            if (active_vars or next_vars)
            else None
        )
    else:
        next_active_tmpls = frozenset(wrappers) if wrappers else None
        next_active_vars = None

    return False, next_active_tmpls, next_active_vars


def _gni_basename_importers(workdir, basename, grep_cache):
    if basename in grep_cache:
        return grep_cache[basename]
    hits = []
    for repo in discover_git_repos(workdir):
        if not os.path.exists(os.path.join(repo, ".git")):
            continue
        try:
            proc = subprocess.run(
                [
                    "git",
                    "-C",
                    repo,
                    "grep",
                    "--untracked",
                    "-l",
                    "--fixed-strings",
                    "-e",
                    basename,
                    "--",
                    "*BUILD.gn",
                    "*.gni",
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except OSError:
            continue
        for line in proc.stdout.splitlines():
            if line.strip():
                hits.append(os.path.join(repo, line.strip()))
    grep_cache[basename] = hits
    return hits


def _raw_gni_importers(
    workdir,
    gni_rel,
    active_tmpls=None,
    active_vars=None,
    depth=0,
    importer_cache=None,
    grep_cache=None,
):
    if importer_cache is None:
        importer_cache = {}
    if grep_cache is None:
        grep_cache = {}
    cache_key = (gni_rel, active_tmpls, active_vars)
    if cache_key in importer_cache:
        return importer_cache[cache_key]
    importer_cache[cache_key] = (set(), {})
    if depth > 3:
        return set(), {}
    importers = set()
    excl_by_pkg = {}
    for imp_abs in _gni_basename_importers(
        workdir, os.path.basename(gni_rel), grep_cache
    ):
        imp_rel = os.path.relpath(imp_abs, workdir).strip("/")
        if imp_rel == gni_rel or os.path.basename(imp_rel) == "visibility.gni":
            continue
        try:
            with open(imp_abs, "r", encoding="utf-8", errors="ignore") as f:
                imp_text = re.sub(r"#[^\n]*", "", f.read())
        except OSError:
            continue
        imp_dir = os.path.dirname(imp_rel).strip("/")
        imports_target = any(
            (
                path[2:].strip("/")
                if path.startswith("//")
                else os.path.normpath(os.path.join(imp_dir, path)).strip("/")
            )
            == gni_rel
            for path in GN_IMPORT_RE.findall(imp_text)
        )
        if not imports_target:
            continue
        file_excl = _extract_excluded_gn_labels(imp_text)
        if imp_rel.endswith(".gni"):
            if active_tmpls is None and active_vars is None:
                sub_imps, sub_excl = _raw_gni_importers(
                    workdir,
                    imp_rel,
                    active_tmpls=None,
                    active_vars=None,
                    depth=depth + 1,
                    importer_cache=importer_cache,
                    grep_cache=grep_cache,
                )
            else:
                (
                    top_direct_use,
                    next_active_tmpls,
                    next_active_vars,
                ) = _propagate_gni_downstream_scopes(
                    imp_text, imp_dir, gni_rel, active_tmpls, active_vars
                )
                if top_direct_use:
                    sub_imps, sub_excl = _raw_gni_importers(
                        workdir,
                        imp_rel,
                        active_tmpls=None,
                        active_vars=None,
                        depth=depth + 1,
                        importer_cache=importer_cache,
                        grep_cache=grep_cache,
                    )
                elif (
                    next_active_tmpls is not None
                    or next_active_vars is not None
                ):
                    sub_imps, sub_excl = _raw_gni_importers(
                        workdir,
                        imp_rel,
                        active_tmpls=next_active_tmpls,
                        active_vars=next_active_vars,
                        depth=depth + 1,
                        importer_cache=importer_cache,
                        grep_cache=grep_cache,
                    )
                else:
                    sub_imps, sub_excl = set(), {}
            importers |= sub_imps
            for p in sub_imps:
                merged_e = sub_excl.get(p, frozenset()) | file_excl
                if merged_e:
                    excl_by_pkg[p] = excl_by_pkg.get(p, frozenset()) | merged_e
        else:
            matched_pkg = False
            if active_tmpls is None and active_vars is None:
                matched_pkg = True
            else:
                imp_code = _blank_non_interp_string_contents(
                    strip_non_dep_lists(imp_text, True)
                )
                tmpl_match = bool(active_tmpls) and any(
                    re.search(r"\b" + re.escape(t) + r"\s*\(", imp_code)
                    for t in active_tmpls
                )
                var_match = bool(active_vars) and _uses_any_ident(
                    imp_code, active_vars
                )
                if tmpl_match or var_match:
                    matched_pkg = True
            if matched_pkg:
                importers.add(imp_dir)
                if file_excl:
                    excl_by_pkg[imp_dir] = (
                        excl_by_pkg.get(imp_dir, frozenset()) | file_excl
                    )
    res = (importers, excl_by_pkg)
    importer_cache[cache_key] = res
    return res


def gni_importer_pkgs(
    workdir,
    gni_rel,
    match_pos=None,
    gni_no_comments=None,
    active_tmpls=None,
    active_vars=None,
    depth=0,
    importer_cache=None,
    grep_cache=None,
    target_label=None,
):
    """Returns packages whose BUILD.gn imports `gni_rel` and uses any enclosing template/variable."""
    if importer_cache is None:
        importer_cache = {}
    if grep_cache is None:
        grep_cache = {}
    if (
        active_tmpls is None
        and active_vars is None
        and match_pos is not None
        and gni_no_comments
    ):
        tmpls = parse_gni_template_blocks(gni_no_comments)
        var_spans, rhs_deps = _parse_gni_top_vars(gni_no_comments, tmpls)
        enc_tmpls, enc_vars = _resolve_gni_enclosing_scopes(
            match_pos, tmpls, var_spans, rhs_deps
        )
        if enc_tmpls:
            active_tmpls = enc_tmpls
        if enc_vars:
            active_vars = enc_vars
    importers, excl_by_pkg = _raw_gni_importers(
        workdir,
        gni_rel,
        active_tmpls=active_tmpls,
        active_vars=active_vars,
        depth=depth,
        importer_cache=importer_cache,
        grep_cache=grep_cache,
    )
    if not target_label or not excl_by_pkg:
        return set(importers)
    return {
        p
        for p in importers
        if not _is_gn_label_excluded(target_label, excl_by_pkg.get(p))
    }


def resolve_in_tree_bzl_path(workdir, raw_label, cur_pkg):
    s = (raw_label or "").strip()
    if s.startswith("@@//"):
        s = s[2:]
    elif s.startswith("@//"):
        s = s[1:]
    if s.startswith("@"):
        return None
    if s.startswith("//"):
        lpkg, sep, lfile = s[2:].partition(":")
        bzl_rel = (
            f"{lpkg.strip('/')}/{lfile.lstrip('/')}".strip("/")
            if sep
            else lpkg.strip("/")
        )
    elif s.startswith(":"):
        bzl_rel = (
            f"{cur_pkg}/{s[1:].lstrip('/')}".strip("/")
            if cur_pkg
            else s[1:].lstrip("/")
        )
    else:
        return None
    if not bzl_rel.endswith(".bzl") or not os.path.isfile(
        os.path.join(workdir, bzl_rel)
    ):
        return None
    return bzl_rel


def get_bzl_symbolic_macro_pkgs(workdir, bzl_rel, visiting=None, cache=None):
    if cache is not None and bzl_rel in cache:
        return cache[bzl_rel]
    active = set() if visiting is None else set(visiting)
    if bzl_rel in active:
        return frozenset()
    active.add(bzl_rel)
    try:
        with open(os.path.join(workdir, bzl_rel), "r", encoding="utf-8") as f:
            bzl_tree = ast.parse(f.read(), filename=bzl_rel)
    except Exception:
        if cache is not None:
            cache[bzl_rel] = frozenset()
        return frozenset()
    cur_pkg = os.path.dirname(bzl_rel).strip("/")
    pkgs = set()
    for node in ast.walk(bzl_tree):
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
            if node.func.id == "macro" and cur_pkg:
                pkgs.add(cur_pkg)
            elif (
                node.func.id == "load"
                and node.args
                and isinstance(node.args[0], ast.Constant)
                and isinstance(node.args[0].value, str)
            ):
                sub_bzl = resolve_in_tree_bzl_path(
                    workdir, node.args[0].value, cur_pkg
                )
                if sub_bzl:
                    pkgs.update(
                        get_bzl_symbolic_macro_pkgs(
                            workdir, sub_bzl, active, cache=cache
                        )
                    )
    frozen = frozenset(pkgs)
    if visiting is None and cache is not None:
        cache[bzl_rel] = frozen
    return frozen


def _collect_ast_strings(node, var_table, depth=0):
    if node is None or depth > 5:
        return []
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return [node.value]
    if isinstance(node, ast.Name) and node.id in var_table:
        return _collect_ast_strings(var_table[node.id], var_table, depth + 1)
    if isinstance(node, (ast.List, ast.Tuple, ast.Set)):
        res = []
        for elt in node.elts:
            res.extend(_collect_ast_strings(elt, var_table, depth + 1))
        return res
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        return _collect_ast_strings(
            node.left, var_table, depth + 1
        ) + _collect_ast_strings(node.right, var_table, depth + 1)
    return []


def compute_symbolic_macro_caller_pkgs(
    workdir, tree, var_table, pkg_path, bzl_cache=None
):
    """Maps same-package targets passed into a loaded Bazel 8 symbolic macro to the macro definition package(s)."""
    loaded_sym_macros = {}
    for st in tree.body:
        if not (isinstance(st, ast.Expr) and isinstance(st.value, ast.Call)):
            continue
        call = st.value
        if not (
            isinstance(call.func, ast.Name)
            and call.func.id == "load"
            and call.args
        ):
            continue
        if not (
            isinstance(call.args[0], ast.Constant)
            and isinstance(call.args[0].value, str)
        ):
            continue
        bzl_rel = resolve_in_tree_bzl_path(
            workdir, call.args[0].value, pkg_path
        )
        if not bzl_rel:
            continue
        macro_pkgs = get_bzl_symbolic_macro_pkgs(
            workdir, bzl_rel, cache=bzl_cache
        )
        if not macro_pkgs:
            continue
        for arg in call.args[1:]:
            if isinstance(arg, ast.Constant) and isinstance(arg.value, str):
                loaded_sym_macros[arg.value] = macro_pkgs
        for kw in call.keywords:
            if kw.arg:
                loaded_sym_macros[kw.arg] = macro_pkgs
    if not loaded_sym_macros:
        return {}
    label_to_target = {}
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        tname = next(
            (
                kw.value.value
                for kw in node.keywords
                if kw.arg == "name"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ),
            None,
        )
        if not tname:
            continue
        label_to_target[tname] = tname
        for kw in node.keywords:
            if kw.arg in ("outs", "out"):
                for out_str in _collect_ast_strings(kw.value, var_table):
                    clean_out = out_str.strip().lstrip(":")
                    if clean_out:
                        label_to_target[clean_out] = tname
    sym_rdeps = {}
    pkg_prefix = f"//{pkg_path}:" if pkg_path else "//:"
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        fn = node.func.id if isinstance(node.func, ast.Name) else ""
        macro_pkgs = loaded_sym_macros.get(fn)
        if not macro_pkgs:
            continue
        caller_tname = next(
            (
                kw.value.value
                for kw in node.keywords
                if kw.arg == "name"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ),
            None,
        )
        for kw in node.keywords:
            if kw.arg in ("name", "visibility"):
                continue
            for raw_s in _collect_ast_strings(kw.value, var_table):
                s = raw_s.strip()
                ref_name = (
                    s[1:]
                    if s.startswith(":")
                    else s[len(pkg_prefix) :]
                    if s.startswith(pkg_prefix)
                    else s
                    if ("/" not in s and ":" not in s and s in label_to_target)
                    else None
                )
                owner = label_to_target.get(ref_name) if ref_name else None
                if owner and owner != caller_tname:
                    sym_rdeps.setdefault(owner, set()).update(macro_pkgs)
    return sym_rdeps


def is_idk_generated_ref(sub, owner, idk_targets):
    """Whether `sub` names an IDK atom target generated for the idk_* Bazel target `owner`."""
    return (
        bool(owner)
        and owner in idk_targets
        and sub != owner
        and sub.startswith(owner + "_")
        and (sub.endswith(("_sdk", "_idk")) or "_sdk_" in sub or "_idk_" in sub)
    )


def owning_bazel_target(
    sub,
    bazel_targets,
    default_target="",
    non_gn_synced_targets=(),
    test_rule_targets=(),
):
    """Maps a referenced label name to the Bazel target that defines or generates it."""
    if sub in bazel_targets:
        return sub
    # Match component manifest shard labels when either side omits or includes '.cml'.
    if sub.endswith(".cml") and sub[: -len(".cml")] in bazel_targets:
        return sub[: -len(".cml")]
    if f"{sub}.cml" in bazel_targets:
        return f"{sub}.cml"
    norm_sub = sub.replace("-", "_")
    for t in bazel_targets:
        if t.replace("-", "_") == norm_sub:
            return t
    best = None
    for t in bazel_targets:
        norm_t = t.replace("-", "_")
        if (sub.startswith(t + "_") or norm_sub.startswith(norm_t + "_")) and (
            best is None or len(t) > len(best)
        ):
            best = t
    if best is not None:
        return best
    if default_target and sub == default_target:
        primary = [
            t
            for t in bazel_targets
            if t not in non_gn_synced_targets
            and t not in test_rule_targets
            and t != "headers"
        ]
        if len(primary) == 1:
            return primary[0]
    return None


def is_gn_only_target_ref(
    rel,
    caller_pkg,
    sub,
    owner,
    bazel_targets,
    is_pkg_target,
    non_gn_synced_targets,
    gn_nonconvertible_targets,
):
    if not rel.endswith((".gn", ".gni")):
        return False
    if sub in non_gn_synced_targets or sub in gn_nonconvertible_targets:
        return True
    if owner in non_gn_synced_targets or owner in gn_nonconvertible_targets:
        return True
    if is_pkg_target and not caller_pkg.startswith(
        ("bundles/assembly", "build/images")
    ):
        return True
    if owner is None and bazel_targets and not is_pkg_target:
        return True
    return False


def extract_gn_forwarding_groups(
    gn_text, bazel_targets, non_gn_synced_targets, package_targets
):
    """Maps non-testonly GN forwarding group('<g>') targets above the sentinel to the Bazel targets they forward."""
    eligible = [
        t
        for t in bazel_targets
        if t not in non_gn_synced_targets and t not in package_targets
    ]
    if not eligible:
        return {}
    no_comments = re.sub(r"#[^\n]*", "", gn_text)
    forwarding = {}
    for m in re.finditer(r'\bgroup\(\s*"([^"$]+)"\s*\)\s*\{', no_comments):
        gname = m.group(1)
        if gname in ("tests", "benchmarks") or gname in bazel_targets:
            continue
        close_idx = pcl.find_closing_delim(no_comments, m.end() - 1, "{", "}")
        gbody = no_comments[m.end() : close_idx]
        if re.search(r"\btestonly\s*=\s*true\b", gbody):
            continue
        forwarded = [
            bt
            for bt in eligible
            if re.search(r'":' + re.escape(bt) + r'(?:\([^)]*\))?"', gbody)
        ]
        if forwarded:
            forwarding[gname] = forwarded
    return forwarding


def extract_bazel_var_table(tree):
    """Extracts top-level variable name -> AST expression bindings from a BUILD.bazel AST."""
    var_table = {}
    for node in tree.body:
        if isinstance(node, ast.Assign):
            for t in node.targets:
                if isinstance(t, ast.Name):
                    var_table[t.id] = node.value
        elif (
            isinstance(node, ast.AugAssign)
            and isinstance(node.op, ast.Add)
            and isinstance(node.target, ast.Name)
        ):
            prev = var_table.get(node.target.id)
            var_table[node.target.id] = (
                ast.BinOp(left=prev, op=ast.Add(), right=node.value)
                if prev is not None
                else node.value
            )
    return var_table


def parse_package_bazel_targets(tree, src_lines):
    """Extracts target sets, variable table, and alias map from a parsed BUILD.bazel AST."""
    var_table = extract_bazel_var_table(tree)
    bazel_targets = []
    package_targets = set()
    non_gn_synced_targets = set()
    test_rule_targets = set()
    testonly_targets = set()
    idk_targets = set()
    bazel2gn_skipped_targets = set()
    alias_map = {}

    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        func = (
            node.func.id
            if isinstance(node.func, ast.Name)
            else (
                node.func.attr if isinstance(node.func, ast.Attribute) else ""
            )
        )
        tname = None
        actual = None
        is_test_package = False
        is_testonly = False
        has_sdk_category = False
        for kw in node.keywords:
            if (
                kw.arg == "name"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                tname = kw.value.value
            elif (
                kw.arg == "actual"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                actual = kw.value.value
            elif kw.arg == "test_components":
                is_test_package = True
            elif kw.arg == "testonly":
                if (
                    isinstance(kw.value, ast.Constant)
                    and kw.value.value is True
                ) or (
                    isinstance(kw.value, ast.Name)
                    and kw.value.id in ("True", "true")
                ):
                    is_testonly = True
            elif kw.arg == "sdk_category" and not (
                isinstance(kw.value, ast.Constant)
                and kw.value.value == "excluded"
            ):
                has_sdk_category = True
        if tname:
            bazel_targets.append(tname)
            if is_testonly:
                testonly_targets.add(tname)
            if func in ("fx_package", "fuchsia_package", "fx_unittest_package"):
                package_targets.add(tname)
            if func in BAZEL_GN_SYNCED_TEST_RULES:
                test_rule_targets.add(tname)
            if func in BAZEL_NON_GN_SYNCED_RULES:
                non_gn_synced_targets.add(tname)
                test_rule_targets.add(tname)
            if func == "fx_package" and is_test_package:
                test_rule_targets.add(tname)
            if func == "fx_component_manifest" and is_testonly:
                test_rule_targets.add(tname)
            if func.startswith("idk_") or (
                func == "fidl_library" and has_sdk_category
            ):
                idk_targets.add(tname)
            if has_bazel2gn_skip_comment(src_lines, node.lineno):
                bazel2gn_skipped_targets.add(tname)
                if func not in ("fx_package", "fuchsia_package"):
                    non_gn_synced_targets.add(tname)
            if func == "alias" and actual and actual.startswith(":"):
                alias_map[tname] = actual[1:]

    return {
        "var_table": var_table,
        "bazel_targets": bazel_targets,
        "package_targets": package_targets,
        "non_gn_synced_targets": non_gn_synced_targets,
        "test_rule_targets": test_rule_targets,
        "testonly_targets": testonly_targets,
        "idk_targets": idk_targets,
        "bazel2gn_skipped_targets": bazel2gn_skipped_targets,
        "alias_map": alias_map,
    }


def compute_package_rdeps(
    workdir,
    pkg_path,
    bazel_targets,
    package_targets,
    non_gn_synced_targets,
    alias_map,
    orig_gn_vis_map=None,
    test_rule_targets=(),
    idk_targets=(),
    file_cache=None,
    origin_main_sources=None,
    target_dir="",
    excluded_gn_only_out=None,
    importer_cache=None,
    grep_cache=None,
):
    """Computes package-level and per-target reverse dependencies across the checkout."""
    pkg_norm = (pkg_path or "").strip("/")
    default_target = os.path.basename(pkg_norm)
    gn_file = os.path.join(workdir, pkg_norm, "BUILD.gn")
    gn_only_targets = {"tests", "benchmarks"}
    gn_nonconvertible_targets = {"tests", "benchmarks"}
    gn_forwarding_groups = {}

    if os.path.isfile(gn_file):
        try:
            with open(gn_file, "r", encoding="utf-8", errors="ignore") as f:
                gn_src = f.read()
            local_convertible_tmpls = set()
            for tm in re.finditer(
                r'\btemplate\(\s*"([^"$]+)"\s*\)\s*\{', gn_src
            ):
                close_idx = pcl.find_closing_delim(
                    gn_src, tm.end() - 1, "{", "}"
                )
                tbody_loc = gn_src[tm.end() : close_idx]
                if any(
                    re.search(r"\b" + re.escape(ct) + r"\s*\(", tbody_loc)
                    for ct in CONVERTIBLE_GN_TEMPLATES
                ):
                    local_convertible_tmpls.add(tm.group(1))
            all_convertible_tmpls = (
                CONVERTIBLE_GN_TEMPLATES | local_convertible_tmpls
            )
            for m in re.finditer(
                r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)',
                gn_src,
                flags=re.MULTILINE,
            ):
                if m.group(1) in GN_PACKAGE_TEMPLATES:
                    package_targets.add(m.group(2))
            if bazel_targets:
                has_sent = pcl.has_sentinel(gn_src)
                above_sentinel, below_sentinel = pcl.split_sentinel(gn_src)
                gn_forwarding_groups = extract_gn_forwarding_groups(
                    above_sentinel,
                    bazel_targets,
                    non_gn_synced_targets,
                    package_targets,
                )
                above_clean = re.sub(r"#[^\n]*", "", above_sentinel)
                below_clean = re.sub(r"#[^\n]*", "", below_sentinel)
                below_synced_targets = {
                    m.group(2)
                    for m in re.finditer(
                        r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)',
                        below_clean,
                        flags=re.MULTILINE,
                    )
                    if m.group(2) in bazel_targets
                }
                for m in re.finditer(
                    r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)',
                    above_clean,
                    flags=re.MULTILINE,
                ):
                    tmpl, t = m.group(1), m.group(2)
                    if tmpl == "template" or t in gn_forwarding_groups:
                        continue
                    if t not in bazel_targets:
                        gn_only_targets.add(t)
                    is_unmigrated_dep_target = (
                        not has_sent
                        and target_dir
                        and pkg_norm != target_dir
                        and t not in bazel_targets
                    )
                    if (
                        tmpl not in all_convertible_tmpls
                        and (
                            t not in bazel_targets
                            or t in ("tests", "benchmarks")
                            or tmpl == "bazel_test_suite"
                        )
                    ) or is_unmigrated_dep_target:
                        gn_nonconvertible_targets.add(t)
                non_gn_synced_targets.difference_update(below_synced_targets)
                package_targets.difference_update(below_synced_targets)
            else:
                for m in re.finditer(
                    r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)',
                    gn_src,
                    flags=re.MULTILINE,
                ):
                    tmpl, t = m.group(1), m.group(2)
                    if tmpl in all_convertible_tmpls and t not in bazel_targets:
                        bazel_targets.append(t)
                gn_forwarding_groups = extract_gn_forwarding_groups(
                    gn_src,
                    bazel_targets,
                    non_gn_synced_targets,
                    package_targets,
                )
                for m in re.finditer(
                    r'^\s*([a-zA-Z0-9_]+)\(\s*"([^"]+)"\s*\)',
                    gn_src,
                    flags=re.MULTILINE,
                ):
                    tmpl, t = m.group(1), m.group(2)
                    if (
                        tmpl != "template"
                        and t not in gn_forwarding_groups
                        and tmpl not in all_convertible_tmpls
                        and t not in bazel_targets
                    ):
                        gn_only_targets.add(t)
                        gn_nonconvertible_targets.add(t)
        except Exception:
            pass

    if file_cache is None:
        if origin_main_sources is None:
            origin_main_sources = {}
        matched = discover_referencing_build_files(
            workdir, [pkg_norm], origin_main_sources=origin_main_sources
        )
        file_cache = build_file_cache(matched, origin_main_sources)
    elif origin_main_sources is None:
        origin_main_sources = {}

    if importer_cache is None:
        importer_cache = {}
    if grep_cache is None:
        grep_cache = {}

    # Bazel target names may contain `/` and `+` (e.g. cml_shard(name = "meta/foo.shard")).
    abs_label_re = re.compile(
        r'"//'
        + re.escape(pkg_norm)
        + r'(?::([A-Za-z0-9_.+/-]+))?(?:\([^)]*\))?"'
    )
    true_rdep_packages = set()
    visibility_only_packages = set()
    gn_only_ref_packages = set()
    per_target_rdeps = {t: set() for t in bazel_targets}
    per_target_direct_rdeps = {t: {} for t in bazel_targets}
    has_macro_injected_rdeps = False

    def collect_gni_template_matches(label_re):
        out = {}
        pkg_prefix = f"//{pkg_norm}"
        for fp, (_, strp) in list(file_cache.items()):
            if pkg_prefix not in strp:
                continue
            g_rel = os.path.relpath(fp, workdir).strip("/")
            if (
                not g_rel.endswith(".gni")
                or os.path.basename(g_rel) == "visibility.gni"
            ):
                continue
            m_list = list(label_re.finditer(strp))
            if not m_list:
                continue
            tmpls = parse_gni_template_blocks(strp)
            var_spans, rhs_deps = _parse_gni_top_vars(strp, tmpls)
            v_names = set(var_spans.keys())
            scoped_matches = [
                (
                    *_resolve_gni_enclosing_scopes(
                        m_obj.start(), tmpls, var_spans, rhs_deps
                    ),
                    m_obj,
                )
                for m_obj in m_list
            ]
            if tmpls or v_names or scoped_matches:
                out[f"//{g_rel}"] = (set(tmpls.keys()), v_names, scoped_matches)
        if out:
            gni_pat_args = [arg for lbl in sorted(out) for arg in ("-e", lbl)]
            for repo in discover_git_repos(workdir):
                if not os.path.exists(os.path.join(repo, ".git")):
                    continue
                is_upstream_only = get_upstream_only_file_filter(repo)
                for g_cmd, is_om in (
                    (
                        [
                            "git",
                            "-C",
                            repo,
                            "grep",
                            "--untracked",
                            "-l",
                            "--fixed-strings",
                        ]
                        + gni_pat_args
                        + ["--", "*BUILD.gn", "*.gni"],
                        False,
                    ),
                    (
                        ["git", "-C", repo, "grep", "-l", "--fixed-strings"]
                        + gni_pat_args
                        + ["origin/main", "--", "*BUILD.gn", "*.gni"],
                        True,
                    ),
                ):
                    try:
                        proc = subprocess.run(
                            g_cmd,
                            stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL,
                            text=True,
                        )
                        for line in proc.stdout.splitlines():
                            s = line.strip()
                            if not s:
                                continue
                            if is_om and s.startswith("origin/main:"):
                                rel_l = s[len("origin/main:") :]
                                if not is_upstream_only(rel_l):
                                    continue
                                abs_c = os.path.join(repo, rel_l)
                                origin_main_sources[abs_c] = (repo, rel_l)
                            else:
                                abs_c = os.path.join(repo, s)
                            if abs_c not in file_cache:
                                parts_c = []
                                if os.path.isfile(abs_c):
                                    with open(
                                        abs_c,
                                        "r",
                                        encoding="utf-8",
                                        errors="ignore",
                                    ) as cf:
                                        parts_c.append(cf.read())
                                om_i = origin_main_sources.get(abs_c)
                                if om_i:
                                    try:
                                        om_r = subprocess.check_output(
                                            [
                                                "git",
                                                "-C",
                                                om_i[0],
                                                "show",
                                                f"origin/main:{om_i[1]}",
                                            ],
                                            stderr=subprocess.DEVNULL,
                                            text=True,
                                        )
                                        if om_r and (
                                            not parts_c or om_r != parts_c[0]
                                        ):
                                            parts_c.append(om_r)
                                    except Exception:
                                        pass
                                raw_c = "\n".join(parts_c)
                                if raw_c:
                                    nc = re.sub(r"#[^\n]*", "", raw_c)
                                    file_cache[abs_c] = (
                                        nc,
                                        strip_non_dep_lists(nc, True),
                                    )
                    except Exception:
                        pass
        return out

    gni_template_map = collect_gni_template_matches(abs_label_re)

    # Iterate through all build files (BUILD.gn, BUILD.bazel, *.gni, *.bzl) across the
    # repository that were pre-matched in `file_cache` as referencing `//{pkg_norm}`.
    # For each candidate referencing file `fpath`:
    #   - `rel`: Path of the referencing file relative to the checkout root.
    #   - `caller_pkg`: Directory/package path containing the referencing file (i.e. the
    #     referencing consumer or dependent package).
    # We inspect whether `fpath` contains actual target dependencies (in `stripped`, where
    # visibility and package list declarations have been removed) to distinguish true
    # reverse dependencies from comment-only or visibility-only references.
    for fpath, (no_comments, stripped) in file_cache.items():
        rel = os.path.relpath(fpath, workdir)
        caller_pkg = os.path.dirname(rel).strip("/")
        if caller_pkg == pkg_norm:
            continue

        rel_sub = None
        if rel.endswith((".gn", ".gni")) and caller_pkg and pkg_norm:
            try:
                if os.path.commonpath([pkg_norm, caller_pkg]):
                    cand_rel = os.path.relpath(pkg_norm, caller_pkg)
                    if not cand_rel.startswith("../../.."):
                        rel_sub = cand_rel
            except ValueError:
                pass
        elif pkg_norm.startswith(caller_pkg + "/"):
            rel_sub = pkg_norm[len(caller_pkg) + 1 :]
        elif rel.endswith((".gn", ".gni")) and caller_pkg:
            cand_rel = os.path.relpath(pkg_norm, caller_pkg)
            if cand_rel and cand_rel != ".":
                rel_sub = cand_rel
        if (
            f"//{pkg_norm}" not in no_comments
            and (not rel_sub or rel_sub not in no_comments)
            and not any(g_lbl in no_comments for g_lbl in gni_template_map)
        ):
            continue

        rel_label_re = (
            re.compile(
                r'"(?:\./)?'
                + re.escape(rel_sub)
                + r'(?::([A-Za-z0-9_.+/-]+))?(?:\([^)]*\))?"'
            )
            if rel_sub
            else None
        )

        if os.path.basename(rel) == "visibility.gni":
            if abs_label_re.search(no_comments):
                visibility_only_packages.add(caller_pkg)
            continue

        excluded_labels = set()
        if rel.endswith((".gn", ".gni")):
            for excl_m in _EXCLUDED_DEP_LIST_RE.finditer(no_comments):
                for raw_excl in re.findall(r'"([^"]+)"', excl_m.group(1)):
                    excluded_labels |= _expand_gn_label_variants(raw_excl)

        gni_call_matches = []
        gni_call_trigger = {}
        if rel.endswith((".gn", ".gni")) and gni_template_map:
            rel_dir = os.path.dirname(rel).strip("/")
            imported_gni_lbls = {
                "//" + imp_p[2:].strip("/")
                if imp_p.startswith("//")
                else "//"
                + os.path.normpath(os.path.join(rel_dir, imp_p)).strip("/")
                for imp_p in GN_IMPORT_RE.findall(stripped)
            }
            stripped_code = _blank_non_interp_string_contents(stripped)
            for gni_lbl, (
                t_names,
                v_names,
                scoped_matches,
            ) in gni_template_map.items():
                if gni_lbl not in imported_gni_lbls:
                    continue
                used_top_vars = {
                    vn for vn in v_names if _uses_any_ident(stripped_code, {vn})
                }
                called_tmpls = {
                    tn
                    for tn in t_names
                    if re.search(
                        r"\b" + re.escape(tn) + r"\s*\(", stripped_code
                    )
                }
                for enc_tmpls, enc_vars, m_obj in scoped_matches:
                    matched_tmpls = enc_tmpls & called_tmpls
                    matched_vars = enc_vars & used_top_vars
                    if not (matched_tmpls or matched_vars):
                        continue
                    if _is_gn_label_excluded(m_obj.group(0), excluded_labels):
                        continue
                    gni_call_matches.append(m_obj)
                    gni_call_trigger[id(m_obj)] = (
                        gni_lbl[2:].strip("/"),
                        frozenset(matched_tmpls),
                        frozenset(matched_vars),
                    )

        all_matches = (
            list(abs_label_re.finditer(no_comments))
            + (list(rel_label_re.finditer(no_comments)) if rel_label_re else [])
            + gni_call_matches
        )
        if not all_matches:
            continue

        dep_matches = (
            list(abs_label_re.finditer(stripped))
            + (list(rel_label_re.finditer(stripped)) if rel_label_re else [])
            + gni_call_matches
        )
        if not dep_matches:
            visibility_only_packages.add(caller_pkg)
            continue

        interp_gn_subs = (
            re.findall(
                r'"\$\{?[A-Za-z_]\w*\}?:([A-Za-z0-9_.+/-]+)(?:\([^)]*\))?"',
                stripped,
            )
            if rel.endswith((".gn", ".gni"))
            else []
        )
        expanded_subs = []
        for m in dep_matches:
            if m.group(1):
                expanded_subs.append((m.group(1), m))
            elif interp_gn_subs:
                for isub in interp_gn_subs:
                    expanded_subs.append((isub, m))
            else:
                expanded_subs.append((default_target, m))

        for sub, m in expanded_subs:
            norm_caller = (
                "bundles/assembly"
                if (
                    caller_pkg == "bundles/assembly"
                    or caller_pkg.startswith("bundles/assembly/")
                )
                else caller_pkg
            )
            if sub in gn_forwarding_groups:
                true_rdep_packages.add(norm_caller)
                for bt in gn_forwarding_groups[sub]:
                    per_target_rdeps[bt].add(norm_caller)
                continue
            if sub in gn_only_targets and sub not in bazel_targets:
                if excluded_gn_only_out is not None:
                    excluded_gn_only_out.add(f"//{caller_pkg} (:{sub})")
                gn_only_ref_packages.add(caller_pkg)
                continue
            is_pkg_target = sub in package_targets or (
                sub == default_target
                and len(bazel_targets) == 1
                and bazel_targets[0] in package_targets
            )
            owner = owning_bazel_target(
                sub,
                bazel_targets,
                default_target,
                non_gn_synced_targets,
                test_rule_targets,
            )
            if (
                owner is None
                and sub == default_target
                and len(bazel_targets) == 1
            ):
                owner = bazel_targets[0]
            if is_idk_generated_ref(sub, owner, idk_targets):
                continue
            if is_gn_only_target_ref(
                rel,
                caller_pkg,
                sub,
                owner,
                bazel_targets,
                is_pkg_target,
                non_gn_synced_targets,
                gn_nonconvertible_targets,
            ):
                if excluded_gn_only_out is not None:
                    excluded_gn_only_out.add(f"//{caller_pkg} (:{sub})")
                gn_only_ref_packages.add(caller_pkg)
                continue
            # `caller_pkg` is the package directory of the referencing build file.
            # If the referencing file lives directly in a top-level SDK allowlist package
            # ("build/sdk" or "sdk/fidl"), it references this target as part of an IDK
            # collection allowlist or group (e.g. partner_idk_versioned_shared_libraries).
            # Bazel IDK atom rules and GN templates automatically inject visibility to these
            # top-level collection targets, so the migrated target does not need to explicitly
            # grant them visibility in BUILD.bazel. Classifying them as `gn_only_ref_packages`
            # prevents the audit and rdep discovery from expecting redundant visibility entries.
            # Note: This only checks exact top-level package roots; legitimate library consumers
            # in subdirectories (such as "sdk/lib/..." or "sdk/fidl/...") are NOT excluded and
            # are tracked as true reverse dependencies.
            if caller_pkg in ("build/sdk", "sdk/fidl"):
                if excluded_gn_only_out is not None:
                    excluded_gn_only_out.add(f"//{caller_pkg} (:{sub})")
                gn_only_ref_packages.add(caller_pkg)
                continue
            if rel.endswith(".gni") and id(m) in gni_call_trigger:
                src_gni_rel, trig_tmpls, trig_vars = gni_call_trigger[id(m)]
                (
                    top_direct_use,
                    next_active_tmpls,
                    next_active_vars,
                ) = _propagate_gni_downstream_scopes(
                    no_comments,
                    caller_pkg,
                    src_gni_rel,
                    trig_tmpls or None,
                    trig_vars or None,
                )
                propagates_downstream = (
                    top_direct_use
                    or next_active_tmpls is not None
                    or next_active_vars is not None
                )
            else:
                top_direct_use = False
                next_active_tmpls = None
                next_active_vars = None
                propagates_downstream = True
            if (
                caller_pkg.startswith("build/")
                and (rel.endswith(".bzl") or rel.endswith(".gni"))
                and "verification" not in os.path.basename(rel)
                and propagates_downstream
            ):
                has_macro_injected_rdeps = True

            if (
                rel.endswith(".gni")
                and not caller_pkg.startswith("build/")
                and propagates_downstream
            ):
                if id(m) in gni_call_trigger:
                    gni_importers = gni_importer_pkgs(
                        workdir,
                        rel,
                        active_tmpls=(
                            None if top_direct_use else next_active_tmpls
                        ),
                        active_vars=(
                            None if top_direct_use else next_active_vars
                        ),
                        importer_cache=importer_cache,
                        grep_cache=grep_cache,
                        target_label=m.group(0),
                    )
                else:
                    gni_importers = gni_importer_pkgs(
                        workdir,
                        rel,
                        match_pos=m.start(),
                        gni_no_comments=no_comments,
                        importer_cache=importer_cache,
                        grep_cache=grep_cache,
                        target_label=m.group(0),
                    )
            else:
                gni_importers = set()

            has_non_test_bazel_targets = any(
                t not in test_rule_targets for t in bazel_targets
            )
            is_test_only_owner = (
                owner is not None
                and owner in test_rule_targets
                and has_non_test_bazel_targets
            )
            if rel.endswith(".gni") and not caller_pkg.startswith("build/"):
                if gni_importers and norm_caller and norm_caller != pkg_norm:
                    if not is_test_only_owner:
                        true_rdep_packages.add(norm_caller)
                    if owner is not None:
                        per_target_rdeps[owner].add(norm_caller)
                callers = sorted(gni_importers - {pkg_norm})
            elif not propagates_downstream:
                callers = []
            else:
                callers = (
                    sorted(gni_importers - {pkg_norm})
                    if gni_importers
                    else [norm_caller]
                )
            for caller in callers:
                if not is_test_only_owner:
                    true_rdep_packages.add(caller)
                if owner is None:
                    continue
                per_target_rdeps[owner].add(caller)
                if gni_importers:
                    per_target_direct_rdeps[owner].setdefault(
                        caller, set()
                    ).add("gn")
                elif os.path.basename(rel) in ("BUILD.gn", "BUILD.bazel"):
                    kind_bs = "gn" if rel.endswith(".gn") else "bazel"
                    per_target_direct_rdeps[owner].setdefault(
                        caller_pkg, set()
                    ).add(kind_bs)

    for alias_name, actual_name in alias_map.items():
        if alias_name in per_target_rdeps and actual_name in per_target_rdeps:
            combined = (
                per_target_rdeps[alias_name] | per_target_rdeps[actual_name]
            )
            per_target_rdeps[alias_name] = set(combined)
            if not orig_gn_vis_map or actual_name not in orig_gn_vis_map:
                per_target_rdeps[actual_name] = set(combined)

    all_target_rdeps = set(true_rdep_packages)
    for t_set in per_target_rdeps.values():
        all_target_rdeps.update(t_set)
    visibility_only_packages -= all_target_rdeps
    gn_only_ref_packages -= true_rdep_packages
    return (
        true_rdep_packages,
        per_target_rdeps,
        per_target_direct_rdeps,
        visibility_only_packages,
        gn_only_ref_packages,
        has_macro_injected_rdeps,
    )


def extract_list_entries(expr, var_table, default_lineno):
    """Extracts [(string_value, lineno), ...] from a Bazel AST list expression."""
    if isinstance(expr, ast.List):
        return [
            (elt.value, getattr(elt, "lineno", default_lineno))
            for elt in expr.elts
            if isinstance(elt, ast.Constant) and isinstance(elt.value, str)
        ]
    if isinstance(expr, ast.Name) and expr.id in var_table:
        return extract_list_entries(
            var_table[expr.id], var_table, default_lineno
        )
    if isinstance(expr, ast.BinOp) and isinstance(expr.op, ast.Add):
        left = extract_list_entries(expr.left, var_table, default_lineno)
        right = extract_list_entries(expr.right, var_table, default_lineno)
        if left is not None and right is not None:
            return left + right
    return None


def get_preexisting_vis_info(workdir, rel_path, change_base="HEAD"):
    """Returns (vis_strings, no_vis_targets, has_pkg_default_vis) from change_base."""
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
            prev_var_table = extract_bazel_var_table(prev_tree)
            vis_vals, no_vis_targets, has_pkg_default_vis = set(), set(), False
            for node in ast.walk(prev_tree):
                if not isinstance(node, ast.Call):
                    continue
                func_name = (
                    node.func.id
                    if isinstance(node.func, ast.Name)
                    else (
                        node.func.attr
                        if isinstance(node.func, ast.Attribute)
                        else ""
                    )
                )
                if func_name == "package":
                    if any(
                        kw.arg == "default_visibility" for kw in node.keywords
                    ):
                        has_pkg_default_vis = True
                    continue
                if func_name == "exports_files":
                    continue
                tname, has_vis = None, False
                for kw in node.keywords:
                    if (
                        kw.arg == "name"
                        and isinstance(kw.value, ast.Constant)
                        and isinstance(kw.value.value, str)
                    ):
                        tname = kw.value.value
                    elif kw.arg == "visibility":
                        has_vis = True
                        vis_entries = extract_list_entries(
                            kw.value, prev_var_table, getattr(kw, "lineno", 1)
                        )
                        if vis_entries is not None:
                            for val, _ in vis_entries:
                                vis_vals.add(val)
                if tname and not has_vis:
                    no_vis_targets.add(tname)
            return vis_vals, no_vis_targets, has_pkg_default_vis
        except Exception:
            continue
    return set(), set(), False


def is_ancestor_test_suite_package(workdir, anc_pkg, child_pkg, cache=None):
    if (
        not anc_pkg
        or anc_pkg == child_pkg
        or not child_pkg.startswith(anc_pkg + "/")
    ):
        return False
    if cache is None:
        cache = {}
    cached = cache.get(anc_pkg)
    if cached is None:
        has_suite = False
        anc_bazel = os.path.join(workdir, anc_pkg, "BUILD.bazel")
        if os.path.isfile(anc_bazel):
            try:
                with open(anc_bazel, "r", encoding="utf-8") as bf:
                    anc_tree = ast.parse(bf.read(), filename=anc_bazel)
                has_suite = any(
                    isinstance(n, ast.Call)
                    and (
                        (
                            isinstance(n.func, ast.Name)
                            and n.func.id == "test_suite"
                        )
                        or (
                            isinstance(n.func, ast.Attribute)
                            and n.func.attr == "test_suite"
                        )
                    )
                    for n in ast.walk(anc_tree)
                )
            except Exception:
                has_suite = False
        if not has_suite:
            anc_gn = os.path.join(workdir, anc_pkg, "BUILD.gn")
            if os.path.isfile(anc_gn):
                try:
                    with open(anc_gn, "r", encoding="utf-8") as gf:
                        gn_clean = re.sub(r"#[^\n]*", "", gf.read())
                    has_suite = bool(
                        re.search(r"\bbazel_test_suite\s*\(", gn_clean)
                    )
                except Exception:
                    has_suite = False
        cache[anc_pkg] = has_suite
        cached = has_suite
    if cached:
        return True
    return anc_pkg == get_pkg_area(child_pkg) and os.path.isfile(
        os.path.join(workdir, anc_pkg, "BUILD.gn")
    )


def is_pending_ancestor_test_suite_vis(
    workdir,
    caller_pkg,
    child_pkg,
    target_is_test_rule,
    direct_rdeps_for_target,
    cache=None,
):
    if not target_is_test_rule:
        return False
    if any(
        "bazel" in kinds
        and child_pkg != r_pkg
        and child_pkg.startswith(r_pkg + "/")
        for r_pkg, kinds in direct_rdeps_for_target.items()
    ):
        return False
    return is_ancestor_test_suite_package(
        workdir, caller_pkg, child_pkg, cache=cache
    )


def find_planter_bin():
    """Locates the active planter executable on disk without scanning unrelated /proc processes."""

    def valid_exe(path):
        return bool(path) and os.path.isfile(path) and os.access(path, os.X_OK)

    env_bin = os.environ.get("PLANTER_BIN", "").strip()
    if env_bin:
        resolved = env_bin if "/" in env_bin else (shutil.which(env_bin) or "")
        if valid_exe(resolved):
            return os.path.abspath(resolved)

    pid = os.getppid()
    visited = set()
    while pid > 1 and pid not in visited:
        visited.add(pid)
        next_pid = 0
        try:
            with open(f"/proc/{pid}/comm", encoding="utf-8") as cf:
                is_planter = cf.read().strip() == "planter"
            if is_planter:
                try:
                    with open(f"/proc/{pid}/cmdline", "rb") as cmdf:
                        parts = [p for p in cmdf.read().split(b"\x00") if p]
                    if b"diff" not in parts[1:]:
                        target = os.readlink(f"/proc/{pid}/exe")
                        if target.endswith(" (deleted)"):
                            target = target[: -len(" (deleted)")]
                        if valid_exe(target):
                            return target
                except OSError:
                    pass
            with open(f"/proc/{pid}/status", encoding="utf-8") as sf:
                for line in sf:
                    if line.startswith("PPid:"):
                        next_pid = int(line.split()[1])
                        break
        except (OSError, ValueError):
            break
        pid = next_pid

    which_bin = shutil.which("planter")
    if valid_exe(which_bin):
        return os.path.abspath(which_bin)

    for cand in (
        os.path.expanduser("~/.local/bin/planter"),
        os.path.expanduser("~/go/bin/planter"),
    ):
        if valid_exe(cand):
            return cand
    return None
