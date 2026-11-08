#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Checks where the change hooks Bazel tests into the two-tier test_suite()
# and GN test graph, per docs/development/build/bazel_concepts/tests.md and
# docs/development/languages/bazel/style/tests.md; only additions since
# $PLANTER_CHANGE_BASE count:
# 1. per_package_bazel_test_suite: a non-aggregation leaf package BUILD.gn
#    defines a new bazel_test_suite() instead of wiring through an area
#    aggregation test_suite().
# 2. bazel_test_suite_only_build_gn: a non-aggregation leaf BUILD.gn ends up
#    holding only imports, bazel_test_suite()s, and a group("tests") of them.
# 3. nested_test_suite: an aggregation test_suite() references a cross-package
#    individual *_test target or private sub-suite directly instead of an
#    exposed package test_suite() (//<pkg>:tests or //<pkg>:host_tests), a
#    package with local tests mixes cross-package references into its
#    test_suite(), or a GN bazel_test_suite() references a cross-package target.
# 4. implicit_test_suite / expanded_empty_test_suite: a Bazel test_suite()
#    omits `tests`, sets `tests = []`, or (when modifying a test_suite() that
#    previously had an empty/omitted `tests` attribute) omits existing test
#    targets in the package; when an existing empty/omitted test_suite() is
#    converted to an explicit `tests = [...]` list, emits
#    `expanded_empty_test_suite` requiring a human-review dispute so a human
#    verifies all relevant tests were included (agents must not resolve that
#    comment).

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"
CHANGE_BASE="${PLANTER_CHANGE_BASE:-HEAD}"

python3 - "$WORKDIR" "$TARGET_DIRS" "$CHANGE_BASE" <<'PYEOF'
import ast
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
raw_dirs = sys.argv[2]
change_base = sys.argv[3].strip() or "HEAD"
target_dirs = [
    d.strip().strip("/")
    for d in re.split(r"[\s,]+", raw_dirs)
    if d.strip().strip("/")
]

UMBRELLA_ANCESTORS = {"", "src", "sdk", "build", "third_party", "zircon"}
SENTINEL_RE = re.compile(
    r"^##\s*BAZEL2GN SENTINEL|#LOCAL_BAZEL_BUILD_SENTINEL", re.M
)
SUITE_RE = re.compile(r'\bbazel_test_suite\(\s*"([^"]+)"\s*\)\s*\{')
SUITE_LIST_RE = re.compile(r"\b(host_tests|target_tests)\s*\+?=\s*\[([^\]]*)\]")

findings = []


# --- Git and diff-base helpers ---


def git_out(args):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir] + args,
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
    except Exception:
        return ""


def git_lines(args):
    out = git_out(args)
    return [line.strip() for line in out.splitlines() if line.strip()]


def is_ancestor(anc, desc):
    if not anc or not desc:
        return False
    return (
        subprocess.run(
            ["git", "-C", workdir, "merge-base", "--is-ancestor", anc, desc],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        ).returncode
        == 0
    )


def resolve_effective_diff_base():
    head_sha = git_out(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
    parent_sha = git_out(["rev-parse", "--verify", "--quiet", "HEAD~1^{commit}"])
    origin_main = git_out(
        ["rev-parse", "--verify", "--quiet", "origin/main^{commit}"]
    )
    base_sha = git_out(
        ["rev-parse", "--verify", "--quiet", f"{change_base}^{{commit}}"]
    )
    merge_base = (
        git_out(["merge-base", origin_main, head_sha])
        if (origin_main and head_sha)
        else ""
    )
    head_on_origin = bool(
        origin_main and head_sha and is_ancestor(head_sha, origin_main)
    )
    wt_files = git_lines(["diff", "--name-only", "HEAD"]) + git_lines(
        ["ls-files", "--others", "--exclude-standard"]
    )
    head_files = (
        git_lines(["diff", "--name-only", "HEAD~1", "HEAD"])
        if parent_sha
        else []
    )
    head_touches_target = bool(target_dirs) and any(
        any(p.startswith(td + "/") for td in target_dirs) for p in head_files
    )
    if (
        base_sha
        and merge_base
        and base_sha != merge_base
        and is_ancestor(base_sha, merge_base)
    ):
        return merge_base
    if parent_sha and (
        not base_sha
        or (
            change_base == "HEAD"
            and base_sha == head_sha
            and not head_on_origin
            and not wt_files
            and head_touches_target
        )
        or not is_ancestor(base_sha, head_sha)
    ):
        return merge_base or "HEAD~1"
    return change_base


effective_base = resolve_effective_diff_base()


def read_worktree_file(rel_path):
    try:
        with open(os.path.join(workdir, rel_path), "r", encoding="utf-8") as f:
            return f.read()
    except OSError:
        return ""


def read_base_file(rel_path):
    refs = [effective_base]
    if effective_base == "HEAD":
        refs = ["HEAD~1", "HEAD"]
    for ref in refs:
        if not ref:
            continue
        try:
            return subprocess.check_output(
                ["git", "-C", workdir, "show", f"{ref}:{rel_path}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except Exception:
            continue
    return ""


# --- GN and Bazel parsing helpers ---


def gn_code(text):
    """Blank out GN comments outside string literals while preserving line offsets."""
    return re.sub(
        r'("(?:\\.|[^"\\])*")|#[^\n]*',
        lambda m: m.group(1) or " " * len(m.group(0)),
        text,
    )


def closing_brace(code, start_idx):
    depth, in_str = 0, False
    i = start_idx
    while i < len(code):
        c = code[i]
        if in_str:
            if c == "\\":
                i += 2
                continue
            in_str = c != '"'
        elif c == '"':
            in_str = True
        elif c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return len(code)


def split_label(label, current_pkg):
    label = re.sub(r"^@@?//", "//", label.strip())
    if label.startswith("@"):
        return None
    if label.startswith("//"):
        path, _, name = label[2:].partition(":")
        path = path.strip("/")
        return path, name or os.path.basename(path)
    return current_pkg, label.lstrip(":")


def list_strings(node):
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        return list_strings(node.left) + list_strings(node.right)
    if isinstance(node, (ast.List, ast.Tuple)):
        return [
            e.value
            for e in node.elts
            if isinstance(e, ast.Constant) and isinstance(e.value, str)
        ]
    return []


HOST_TEST_RULES = {
    "host_rustc_test",
    "host_test",
    "wrap_host_rust_test",
    "host_go_test",
    "host_cc_test",
    "host_py_test",
    "host_shac_test",
    "py_test",
    "sh_test",
}
TARGET_TEST_RULES = {
    "fx_test",
    "fx_unittest_package",
    "fuchsia_unittest_package",
    "fuchsia_test_package",
    "fx_test_package",
    "rustc_test",
    "rust_test",
    "cc_test",
    "go_test",
}
PACKAGING_HELPER_RULES = {
    "fx_unittest_package",
    "fuchsia_unittest_package",
    "fuchsia_test_package",
    "fuchsia_package",
    "fx_test_package",
    "fx_test",
    "fx_package",
    "fx_test_component",
    "fuchsia_test_component",
    "fuchsia_component",
    "fx_unittest_component",
    "fx_packaged_binary",
    "wrap_host_rust_test",
    "host_test",
}
PACKAGING_DEP_ATTRS = (
    "unit_tests",
    "binary",
    "binary_name",
    "unstripped_binary",
    "test_components",
    "components",
    "package",
    "compiled_manifest",
    "deps",
)


def _has_host_constraints(kws):
    tcw = kws.get("target_compatible_with")
    if tcw is None:
        return False
    try:
        src = ast.unparse(tcw)
    except Exception:
        src = ""
    return (
        "HOST_OS_CONSTRAINTS" in src
        or "HOST_CONSTRAINTS" in src
        or "@platforms//os:linux" in src
    )


def _extract_local_refs(node):
    if node is None:
        return []
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        val = node.value.strip()
        if not val.startswith("//") and not val.startswith("@"):
            return [val.lstrip(":")]
        return []
    return [
        s.lstrip(":")
        for s in list_strings(node)
        if not s.startswith("//") and not s.startswith("@")
    ]


def parse_bazel_file_info(src_text, rel_path=""):
    """Return dict with suites, suite_vis, suite_host, suite_src, target_tests, host_tests, all_targets."""
    info = {
        "suites": {},
        "suite_vis": {},
        "suite_host": {},
        "suite_src": {},
        "target_tests": set(),
        "host_tests": set(),
        "all_targets": set(),
    }
    if not src_text.strip():
        return info
    try:
        tree = ast.parse(src_text, filename=rel_path or "<BUILD.bazel>")
    except Exception:
        return info
    wrapped_local_targets = set()
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        fn = (
            node.func.id
            if isinstance(node.func, ast.Name)
            else (
                node.func.attr
                if isinstance(node.func, ast.Attribute)
                else ""
            )
        )
        kws = {kw.arg: kw.value for kw in node.keywords if kw.arg}
        name_node = kws.get("name")
        if not (
            isinstance(name_node, ast.Constant)
            and isinstance(name_node.value, str)
        ):
            continue
        t_name = name_node.value
        info["all_targets"].add(t_name)
        lineno = getattr(node, "lineno", 1)
        if fn in PACKAGING_HELPER_RULES:
            for attr_nm in PACKAGING_DEP_ATTRS:
                for ref_nm in _extract_local_refs(kws.get(attr_nm)):
                    if ref_nm != t_name:
                        wrapped_local_targets.add(ref_nm)
        if fn == "test_suite":
            tests_node = kws.get("tests")
            tests_val = None if tests_node is None else list_strings(tests_node)
            vis_node = kws.get("visibility")
            vis_val = None if vis_node is None else list_strings(vis_node)
            info["suites"][t_name] = (lineno, tests_val)
            info["suite_vis"][t_name] = vis_val
            info["suite_host"][t_name] = (
                "host" in t_name or _has_host_constraints(kws)
            )
            info["suite_src"][t_name] = (
                ast.get_source_segment(src_text, node) or ""
            ).strip()
        elif fn in HOST_TEST_RULES or (
            fn.startswith("host_") and fn.endswith("_test")
        ):
            info["host_tests"].add(t_name)
        elif fn in TARGET_TEST_RULES or fn.endswith("_test") or fn.endswith("_test_suite"):
            if _has_host_constraints(kws):
                info["host_tests"].add(t_name)
            else:
                info["target_tests"].add(t_name)
        else:
            wut = kws.get("with_unit_tests")
            if isinstance(wut, ast.Constant) and wut.value is True:
                ut_target = f"{t_name}_test"
                info["all_targets"].add(ut_target)
                if (
                    fn.startswith("host_")
                    or fn.endswith("_host_tool")
                    or fn == "ffx_plugin"
                    or _has_host_constraints(kws)
                ):
                    info["host_tests"].add(ut_target)
                else:
                    info["target_tests"].add(ut_target)
            whut = kws.get("with_host_unit_tests")
            if isinstance(whut, ast.Constant) and whut.value is True:
                ut_target = f"{t_name}_test"
                info["all_targets"].add(ut_target)
                info["host_tests"].add(ut_target)
    info["target_tests"] -= wrapped_local_targets
    info["host_tests"] -= wrapped_local_targets
    return info


def parse_bazel_suites_from_src(src_text, rel_path=""):
    """Return {suite_name: (lineno, tests_list_or_None)} for test_suite() calls."""
    return parse_bazel_file_info(src_text, rel_path)["suites"]


_bazel_info_cache = {}
_base_bazel_info_cache = {}
_bazel_suites_cache = {}
_base_bazel_suites_cache = {}


def bazel_info_of(pkg):
    if pkg not in _bazel_info_cache:
        rel = os.path.join(pkg, "BUILD.bazel") if pkg else "BUILD.bazel"
        _bazel_info_cache[pkg] = parse_bazel_file_info(
            read_worktree_file(rel), rel
        )
    return _bazel_info_cache[pkg]


def base_bazel_info_of(pkg):
    if pkg not in _base_bazel_info_cache:
        rel = os.path.join(pkg, "BUILD.bazel") if pkg else "BUILD.bazel"
        _base_bazel_info_cache[pkg] = parse_bazel_file_info(
            read_base_file(rel), rel
        )
    return _base_bazel_info_cache[pkg]


def bazel_suites_of(pkg):
    if pkg not in _bazel_suites_cache:
        _bazel_suites_cache[pkg] = bazel_info_of(pkg)["suites"]
    return _bazel_suites_cache[pkg]


def base_bazel_suites_of(pkg):
    if pkg not in _base_bazel_suites_cache:
        _base_bazel_suites_cache[pkg] = base_bazel_info_of(pkg)["suites"]
    return _base_bazel_suites_cache[pkg]


def parse_gn_bazel_test_suites(gn_text):
    """Return {suite_name: (lineno, {param: [labels]})} for bazel_test_suite() blocks."""
    code = gn_code(gn_text)
    out = {}
    for m in SUITE_RE.finditer(code):
        s_name = m.group(1)
        lineno = code.count("\n", 0, m.start()) + 1
        body = code[m.end() : closing_brace(code, m.end() - 1)]
        params = {}
        for lm in SUITE_LIST_RE.finditer(body):
            param = lm.group(1)
            lbls = re.findall(r'"([^"]+)"', lm.group(2))
            params.setdefault(param, []).extend(lbls)
        out[s_name] = (lineno, params)
    return out


# --- Aggregation test_suite() and group("tests") discovery ---


def has_aggregation_bazel_suite(pkg):
    for _, members in bazel_suites_of(pkg).values():
        for lbl in members or []:
            sp = split_label(lbl, pkg)
            if sp and sp[0] != pkg:
                return True
    return False


def ancestor_suite_covers_pkg(anc_pkg, anc_suite, td):
    if anc_pkg in UMBRELLA_ANCESTORS:
        return False
    cur = os.path.dirname(td)
    while cur and cur != anc_pkg and cur.startswith(anc_pkg + "/"):
        if has_aggregation_bazel_suite(cur) or SUITE_RE.search(
            gn_code(read_worktree_file(os.path.join(cur, "BUILD.gn")))
        ):
            return False
        cur = os.path.dirname(cur)
    suite_info = bazel_suites_of(anc_pkg).get(anc_suite)
    if not suite_info:
        return False
    _, raw_members = suite_info
    in_scope = set(target_dirs) | {td}
    base_src = read_base_file(os.path.join(anc_pkg, "BUILD.bazel"))
    member_pkgs = set()
    for lbl in raw_members or []:
        sp = split_label(lbl, anc_pkg)
        if not sp:
            continue
        m_pkg, _ = sp
        if m_pkg in in_scope and f"//{m_pkg}:" not in base_src:
            continue
        member_pkgs.add(m_pkg)
    if td in member_pkgs:
        return True
    if os.path.dirname(td) == anc_pkg:
        return True
    rel_td = os.path.relpath(td, anc_pkg)
    first_seg = rel_td.split("/")[0]
    sub_root = os.path.join(anc_pkg, first_seg)
    if any(p == sub_root or p.startswith(sub_root + "/") for p in member_pkgs):
        return True
    return bool(member_pkgs)


def find_covering_ancestor_suite(td, param):
    parts = [p for p in td.split("/") if p]
    ancestors = ["/".join(parts[:i]) for i in range(1, len(parts))]
    for anc in reversed(ancestors):
        if anc in UMBRELLA_ANCESTORS:
            continue
        anc_gn = gn_code(read_worktree_file(os.path.join(anc, "BUILD.gn")))
        if not anc_gn:
            continue
        for _, (_, params) in parse_gn_bazel_test_suites(anc_gn).items():
            for lbl in params.get(param, []):
                sp = split_label(lbl, anc)
                if (
                    sp
                    and sp[0] == anc
                    and sp[1] in bazel_suites_of(anc)
                    and ancestor_suite_covers_pkg(anc, sp[1], td)
                ):
                    return (anc, sp[1])
    return None


def find_non_umbrella_ancestor_tests_boundary(td):
    parts = [p for p in td.split("/") if p]
    ancestors = ["/".join(parts[:i]) for i in range(1, len(parts))]
    for anc in reversed(ancestors):
        if anc in UMBRELLA_ANCESTORS:
            continue
        anc_gn_rel = os.path.join(anc, "BUILD.gn")
        anc_gn = gn_code(read_worktree_file(anc_gn_rel))
        if not anc_gn:
            continue
        rel_from_anc = os.path.relpath(td, anc)
        for m in re.finditer(r'\bgroup\(\s*"tests"\s*\)\s*\{', anc_gn):
            body = anc_gn[m.end() : closing_brace(anc_gn, m.end() - 1)]
            pat = (
                r'"(?://'
                + re.escape(td)
                + r"|"
                + re.escape(rel_from_anc)
                + r')(?::tests)?(?:\([^"]*\))?"'
            )
            if re.search(pat, body):
                return anc
    return None


# --- Check 1: per_package_bazel_test_suite ---


def check_per_package_bazel_test_suite(gn_rel):
    pkg_dir = os.path.dirname(gn_rel).strip("/")
    if not pkg_dir or pkg_dir in UMBRELLA_ANCESTORS:
        return
    if has_aggregation_bazel_suite(pkg_dir):
        return
    gn_text = read_worktree_file(gn_rel)
    cur_suites = parse_gn_bazel_test_suites(gn_text)
    if not cur_suites:
        return
    base_suites = parse_gn_bazel_test_suites(read_base_file(gn_rel))

    for s_name, (lineno, params) in cur_suites.items():
        base_params = base_suites.get(s_name, (0, {}))[1]
        for param, labels in params.items():
            old_labels = set(base_params.get(param, []))
            new_labels = [lbl for lbl in labels if lbl not in old_labels]
            if not new_labels:
                continue
            anc_info = find_covering_ancestor_suite(pkg_dir, param)
            pkg_suite_name = "host_tests" if param == "host_tests" else "tests"
            for lbl in new_labels:
                sp = split_label(lbl, pkg_dir)
                if not sp or sp[0] != pkg_dir:
                    continue
                t_name = sp[1]
                if anc_info:
                    anc_pkg, anc_suite = anc_info
                    msg = (
                        f"`bazel_test_suite(\"{s_name}\")` in '{gn_rel}' exports "
                        f"`\"//{pkg_dir}:{t_name}\"` while aggregation `test_suite()` "
                        f"`//{anc_pkg}:{anc_suite}` in '{anc_pkg}/BUILD.bazel' already "
                        f"covers `{pkg_dir}`."
                    )
                    rem = (
                        f"Define a package `test_suite(name = \"{pkg_suite_name}\", "
                        f"tests = [\":{t_name}\"], "
                        f"visibility = [\"//{anc_pkg}:__pkg__\"])` as the "
                        f"first target in '{pkg_dir}/BUILD.bazel', keep individual "
                        f"test targets private (omit `visibility`), add "
                        f"`\"//{pkg_dir}:{pkg_suite_name}\"` to the aggregation "
                        f"`test_suite(name = \"{anc_suite}\", tests = [...])` in "
                        f"'{anc_pkg}/BUILD.bazel', and remove "
                        f"`bazel_test_suite(\"{s_name}\")` from '{gn_rel}'."
                    )
                else:
                    msg = (
                        f"`bazel_test_suite(\"{s_name}\")` in '{gn_rel}' exports "
                        f"`\"//{pkg_dir}:{t_name}\"` via a per-package "
                        f"`bazel_test_suite()` in an individual package's `BUILD.gn`, "
                        f"which is disallowed (tests must be wired through an area "
                        f"aggregation `test_suite()`)."
                    )
                    rem = (
                        f"Remove the per-package `bazel_test_suite(\"{s_name}\")` "
                        f"from '{gn_rel}' and define a package `test_suite(name = "
                        f"\"{pkg_suite_name}\", tests = [\":{t_name}\"])` "
                        f"as the first target in "
                        f"'{pkg_dir}/BUILD.bazel' (keeping individual test targets "
                        f"private and omitting `visibility` until the aggregation "
                        f"package is known). Because no existing aggregation "
                        f"`test_suite()` was found automatically for `{pkg_dir}`, do "
                        f"not guess or invent an aggregation directory; report the "
                        f"resulting unexported test finding in "
                        f"`CoderReport.disputed_findings` and `summary` asking the "
                        f"reviewer which aggregation `test_suite()` (or where a new "
                        f"one, with a `bazel_test_suite()` in that directory's "
                        f"`BUILD.gn`, should be added if none exists) the package's "
                        f"`test_suite()`(s) should be added to, suggesting asking the "
                        f"Build team if they do not know."
                    )
                findings.append({
                    "source": "test_suite_layout",
                    "category": "per_package_bazel_test_suite",
                    "severity": "error",
                    "file": gn_rel,
                    "line": lineno,
                    "message": msg,
                    "remediation": rem,
                })


# --- Check 2: bazel_test_suite_only_build_gn ---


def is_suite_only_build_gn(gn_text):
    if not gn_text.strip() or SENTINEL_RE.search(gn_text):
        return False
    code = gn_code(gn_text)
    suite_names = set()
    pos = 0
    n = len(code)
    while pos < n:
        while pos < n and code[pos].isspace():
            pos += 1
        if pos >= n:
            break
        m_imp = re.match(r'import\(\s*"[^"]+"\s*\)', code[pos:])
        if m_imp:
            pos += m_imp.end()
            continue
        m_call = re.match(
            r'([A-Za-z_]\w*)\(\s*"([^"]+)"\s*\)\s*\{', code[pos:]
        )
        if not m_call:
            return False
        tmpl, t_name = m_call.group(1), m_call.group(2)
        brace_open = pos + m_call.end() - 1
        brace_close = closing_brace(code, brace_open)
        body = code[brace_open + 1 : brace_close]
        if tmpl == "bazel_test_suite":
            suite_names.add(t_name)
        elif tmpl == "group" and t_name == "tests":
            deps = re.findall(r'"([^"]+)"', body)
            if not deps or not all(
                d.startswith(":") and d[1:] in suite_names for d in deps
            ):
                return False
        else:
            return False
        pos = brace_close + 1
    return bool(suite_names)


def check_bazel_test_suite_only_build_gn(gn_rel):
    pkg_dir = os.path.dirname(gn_rel).strip("/")
    if not pkg_dir or pkg_dir in UMBRELLA_ANCESTORS:
        return
    cur_text = read_worktree_file(gn_rel)
    if not is_suite_only_build_gn(cur_text):
        return
    base_text = read_base_file(gn_rel)
    if base_text and is_suite_only_build_gn(base_text):
        return
    # Keeping BUILD.gn is permitted when this package aggregates other packages
    # (descendant or sibling packages).
    if has_aggregation_bazel_suite(pkg_dir):
        return
    cur_suites = parse_gn_bazel_test_suites(cur_text)
    anc_suite = None
    first_param = "target_tests"
    for _, (_, params) in cur_suites.items():
        for param in params:
            first_param = param
            anc_suite = find_covering_ancestor_suite(pkg_dir, param)
            if anc_suite:
                break
        if anc_suite:
            break
    lineno = next(iter(cur_suites.values()))[0]
    pkg_suite_name = "host_tests" if first_param == "host_tests" else "tests"
    if anc_suite:
        anc_pkg, anc_s_name = anc_suite
        msg = (
            f"Leaf '{gn_rel}' holds only `import(...)`, `bazel_test_suite()`, and "
            f"`group(\"tests\")` while aggregation `test_suite()` "
            f"`//{anc_pkg}:{anc_s_name}` in '{anc_pkg}/BUILD.bazel' covers "
            f"`{pkg_dir}`."
        )
        rem = (
            f"Expose the package's tests via `test_suite(name = "
            f"\"{pkg_suite_name}\", tests = [...], visibility = "
            f"[\"//{anc_pkg}:__pkg__\"])` "
            f"as the first target in '{pkg_dir}/BUILD.bazel' (keeping "
            f"individual test targets private), wire "
            f"`\"//{pkg_dir}:{pkg_suite_name}\"` into the aggregation `test_suite()` "
            f"`//{anc_pkg}:{anc_s_name}` in '{anc_pkg}/BUILD.bazel', remove "
            f"`{pkg_dir}:tests` from '{anc_pkg}/BUILD.gn', and delete '{gn_rel}' "
            f"(`git rm {gn_rel}`)."
        )
    else:
        msg = (
            f"Leaf '{gn_rel}' holds only `import(...)`, `bazel_test_suite()`, and "
            f"`group(\"tests\")`; individual packages must not keep a `BUILD.gn` "
            f"just to define a per-package `bazel_test_suite()`."
        )
        rem = (
            f"Expose the package's tests via `test_suite(name = "
            f"\"{pkg_suite_name}\", tests = [...])` as the first target in "
            f"'{pkg_dir}/BUILD.bazel' (keeping individual test targets private "
            f"and omitting `visibility` until the aggregation package is known) "
            f"and delete '{gn_rel}' (`git rm {gn_rel}`). Because no existing "
            f"aggregation `test_suite()` was found automatically for `{pkg_dir}`, "
            f"do not guess or invent an aggregation directory; report the "
            f"resulting unexported test finding in `CoderReport.disputed_findings` "
            f"and `summary` asking the reviewer which aggregation `test_suite()` "
            f"(or where a new one, with a `bazel_test_suite()` in that "
            f"directory's `BUILD.gn`, should be added if none exists) the "
            f"package's `test_suite()`(s) should be added to, suggesting "
            f"asking the Build team if they do not know."
        )
    findings.append({
        "source": "test_suite_layout",
        "category": "bazel_test_suite_only_build_gn",
        "severity": "error",
        "file": gn_rel,
        "line": lineno,
        "message": msg,
        "remediation": rem,
    })


# --- Check 3: nested_test_suite ---


def check_nested_test_suite_in_bazel(bz_rel):
    pkg_dir = os.path.dirname(bz_rel).strip("/")
    pkg_info = bazel_info_of(pkg_dir)
    cur_suites = pkg_info["suites"]
    if not cur_suites:
        return
    has_local_tests = bool(pkg_info["target_tests"] or pkg_info["host_tests"])
    base_suites = base_bazel_suites_of(pkg_dir)
    for s_name, (lineno, members) in cur_suites.items():
        if not members:
            continue
        old_members = set((base_suites.get(s_name) or (0, None))[1] or [])
        for lbl in members:
            if lbl in old_members:
                continue
            sp = split_label(lbl, pkg_dir)
            if not sp:
                continue
            m_pkg, m_name = sp
            if m_pkg == pkg_dir:
                continue
            if has_local_tests:
                msg = (
                    f"`test_suite(name = \"{s_name}\")` in '{bz_rel}' mixes "
                    f"local package tests with cross-package reference "
                    f"`\"//{m_pkg}:{m_name}\"`; a package `test_suite()` must "
                    f"only cover tests in '{pkg_dir}'."
                )
                rem = (
                    f"Remove `\"//{m_pkg}:{m_name}\"` from `test_suite(name = "
                    f"\"{s_name}\")` in '{bz_rel}' and aggregate "
                    f"`\"//{m_pkg}:tests\"` (or `\"//{m_pkg}:host_tests\"`) in "
                    f"the area's aggregation `test_suite()` instead."
                )
                findings.append({
                    "source": "test_suite_layout",
                    "category": "nested_test_suite",
                    "severity": "error",
                    "file": bz_rel,
                    "line": lineno,
                    "message": msg,
                    "remediation": rem,
                })
            else:
                m_info = bazel_info_of(m_pkg)
                m_suites = m_info["suites"]
                m_vis = m_info["suite_vis"].get(m_name)
                m_has_local_tests = bool(
                    m_info["target_tests"] or m_info["host_tests"]
                )
                m_is_private_sub_suite = (
                    m_name in m_suites
                    and m_name not in ("tests", "host_tests")
                    and (
                        m_vis is None
                        or m_vis == ["//visibility:private"]
                        or m_has_local_tests
                    )
                )
                if m_name not in m_suites or m_is_private_sub_suite:
                    m_suite_name = (
                        "host_tests" if "host" in s_name else "tests"
                    )
                    target_kind_desc = (
                        "package-private sub-suite"
                        if m_is_private_sub_suite
                        else "individual cross-package test target"
                    )
                    msg = (
                        f"Aggregation `test_suite(name = \"{s_name}\")` in "
                        f"'{bz_rel}' references {target_kind_desc} "
                        f"`\"//{m_pkg}:{m_name}\"` directly instead of an "
                        f"exposed package `test_suite()` (`\"//{m_pkg}:tests\"` "
                        f"or `\"//{m_pkg}:host_tests\"`) in '{m_pkg}/BUILD.bazel'."
                    )
                    rem = (
                        f"Define a package `test_suite(name = \"{m_suite_name}\", "
                        f"tests = [\":{m_name}\"], "
                        f"visibility = [\"//{pkg_dir}:__pkg__\"])` as "
                        f"the first target in '{m_pkg}/BUILD.bazel', omit "
                        f"`visibility` on `:{m_name}`, and replace "
                        f"`\"//{m_pkg}:{m_name}\"` with "
                        f"`\"//{m_pkg}:{m_suite_name}\"` in `test_suite(name = "
                        f"\"{s_name}\")` in '{bz_rel}'."
                    )
                    findings.append({
                        "source": "test_suite_layout",
                        "category": "nested_test_suite",
                        "severity": "error",
                        "file": bz_rel,
                        "line": lineno,
                        "message": msg,
                        "remediation": rem,
                    })


def check_nested_test_suite_in_gn(gn_rel):
    pkg_dir = os.path.dirname(gn_rel).strip("/")
    cur_suites = parse_gn_bazel_test_suites(read_worktree_file(gn_rel))
    if not cur_suites:
        return
    base_suites = parse_gn_bazel_test_suites(read_base_file(gn_rel))
    for s_name, (lineno, params) in cur_suites.items():
        base_params = base_suites.get(s_name, (0, {}))[1]
        for param, labels in params.items():
            old_labels = set(base_params.get(param, []))
            for lbl in labels:
                if lbl in old_labels:
                    continue
                sp = split_label(lbl, pkg_dir)
                if not sp:
                    continue
                m_pkg, m_name = sp
                if m_pkg != pkg_dir:
                    agg_suite_name = (
                        "host_tests" if param == "host_tests" else "tests"
                    )
                    m_suite_name = (
                        m_name
                        if m_name in bazel_suites_of(m_pkg)
                        else agg_suite_name
                    )
                    msg = (
                        f"`bazel_test_suite(\"{s_name}\")` in '{gn_rel}' "
                        f"references cross-package target `\"//{m_pkg}:{m_name}\"` "
                        f"in `{param}`; `bazel_test_suite()` must only reference "
                        f"an aggregation `test_suite()` in '{pkg_dir}/BUILD.bazel'."
                    )
                    rem = (
                        f"Define an aggregation `test_suite(name = "
                        f"\"{agg_suite_name}\", tests = "
                        f"[\"//{m_pkg}:{m_suite_name}\", ...])` in "
                        f"'{pkg_dir}/BUILD.bazel' and reference "
                        f"`\"//{pkg_dir}:{agg_suite_name}\"` from "
                        f"`bazel_test_suite(\"{s_name}\")` in '{gn_rel}'."
                    )
                    findings.append({
                        "source": "test_suite_layout",
                        "category": "nested_test_suite",
                        "severity": "error",
                        "file": gn_rel,
                        "line": lineno,
                        "message": msg,
                        "remediation": rem,
                    })


# --- Check 4: implicit_test_suite & expanded_empty_test_suite ---


def check_implicit_test_suite(bz_rel):
    pkg_dir = os.path.dirname(bz_rel).strip("/")
    pkg_info = bazel_info_of(pkg_dir)
    cur_suites = pkg_info["suites"]
    if not cur_suites:
        return
    base_info = base_bazel_info_of(pkg_dir)
    base_suites = base_info["suites"]
    cur_target_tests = pkg_info["target_tests"]
    cur_host_tests = pkg_info["host_tests"]
    base_target_tests = base_info["target_tests"]
    base_host_tests = base_info["host_tests"]
    has_target_tests = bool(cur_target_tests)
    has_host_tests = bool(cur_host_tests)
    has_mixed_platforms = has_target_tests and has_host_tests
    has_multiple_suites = len(cur_suites) > 1
    tests_changed_in_pkg = (
        cur_target_tests != base_target_tests
        or cur_host_tests != base_host_tests
    )

    for s_name, (lineno, members) in cur_suites.items():
        in_base = s_name in base_suites
        old_members = base_suites[s_name][1] if in_base else None
        was_empty_in_base = in_base and (
            old_members is None or len(old_members) == 0
        )
        suite_call_modified = (not in_base) or (
            pkg_info["suite_src"].get(s_name)
            != base_info["suite_src"].get(s_name)
        )

        covered_by_other_suites = set()
        for other_s, (_, other_m) in cur_suites.items():
            if other_s == s_name or not other_m:
                continue
            for lbl in other_m:
                sp = split_label(lbl, pkg_dir)
                if sp and sp[0] == pkg_dir:
                    covered_by_other_suites.add(sp[1])

        is_host_suite = pkg_info["suite_host"].get(s_name, False)
        has_other_host_suite = any(
            pkg_info["suite_host"].get(other_s, False)
            for other_s in cur_suites
            if other_s != s_name
        )
        has_other_target_suite = any(
            not pkg_info["suite_host"].get(other_s, False)
            for other_s in cur_suites
            if other_s != s_name
        )
        if is_host_suite and (has_other_target_suite or has_host_tests):
            expected_local_for_suite = cur_host_tests - covered_by_other_suites
        elif not is_host_suite and (
            has_other_host_suite or (has_target_tests and not has_host_tests)
        ):
            expected_local_for_suite = (
                cur_target_tests - covered_by_other_suites
            )
        else:
            expected_local_for_suite = (
                cur_target_tests | cur_host_tests
            ) - covered_by_other_suites

        if expected_local_for_suite:
            suggested_tests_str = (
                "tests = ["
                + ", ".join(f'":{t}"' for t in sorted(expected_local_for_suite))
                + "]"
            )
        else:
            suggested_tests_str = 'tests = [":<test_target>", ...]'

        if members is None or len(members) == 0:
            if was_empty_in_base and not (
                suite_call_modified
                or tests_changed_in_pkg
                or has_multiple_suites
                or has_mixed_platforms
            ):
                continue
            detail = (
                "omits the `tests` attribute"
                if members is None
                else "sets an empty `tests = []` list"
            )
            if was_empty_in_base:
                msg = (
                    f"`test_suite(name = \"{s_name}\")` in '{bz_rel}' {detail}, "
                    f"which implicitly expands to every test target in the "
                    f"package; `test_suite()` targets must always explicitly "
                    f"list their test targets in `tests = [...]` (and when "
                    f"modifying a `test_suite()` with an empty `tests` attribute "
                    f"or adding/removing tests in its package, all existing test "
                    f"targets in the package must be added to `tests = [...]` in "
                    f"addition to any added tests, or started from the existing "
                    f"list before removing entries)."
                )
                rem = (
                    f"Update `test_suite(name = \"{s_name}\")` in '{bz_rel}' to "
                    f"specify explicit targets (`{suggested_tests_str}`), "
                    f"ensuring all existing test targets in '{pkg_dir}' are "
                    f"included in addition to any newly added tests (or starting "
                    f"from the existing list and removing only deleted entries). "
                    f"Then report the resulting "
                    f"`test_suite_layout/expanded_empty_test_suite` finding in "
                    f"`CoderReport.disputed_findings` and `summary` requesting "
                    f"that a human reviewer ensure that all relevant tests have "
                    f"been included (do not allow agents to resolve that comment)."
                )
            else:
                msg = (
                    f"`test_suite(name = \"{s_name}\")` in '{bz_rel}' {detail}, "
                    f"which implicitly expands to every test target in the "
                    f"package; `test_suite()` targets must always explicitly "
                    f"list their test targets in `tests = [...]`."
                )
                rem = (
                    f"Pass an explicit non-empty `{suggested_tests_str}` list to "
                    f"`test_suite(name = \"{s_name}\")` in '{bz_rel}' (never "
                    f"omit `tests` or use `tests = []`)."
                )
            findings.append({
                "source": "test_suite_layout",
                "category": "implicit_test_suite",
                "severity": "error",
                "file": bz_rel,
                "line": lineno,
                "message": msg,
                "remediation": rem,
            })
        elif was_empty_in_base:
            listed_local = set()
            for lbl in members:
                sp = split_label(lbl, pkg_dir)
                if sp and sp[0] == pkg_dir:
                    listed_local.add(sp[1])
            expanded_listed_local = set(listed_local)
            for m_nm in list(listed_local):
                if m_nm in cur_suites and cur_suites[m_nm][1]:
                    for sub_lbl in cur_suites[m_nm][1]:
                        sp = split_label(sub_lbl, pkg_dir)
                        if sp and sp[0] == pkg_dir:
                            expanded_listed_local.add(sp[1])

            missing_local = sorted(
                expected_local_for_suite - expanded_listed_local
            )
            removed_still_listed = sorted(
                (expanded_listed_local & (base_target_tests | base_host_tests))
                - pkg_info["all_targets"]
            )
            if missing_local:
                missing_fmt = ", ".join(f'":{t}"' for t in missing_local)
                full_list_fmt = ", ".join(
                    f'":{t}"'
                    for t in sorted(
                        (expanded_listed_local | expected_local_for_suite)
                        - set(removed_still_listed)
                    )
                )
                msg = (
                    f"`test_suite(name = \"{s_name}\")` in '{bz_rel}' was "
                    f"modified from an empty (or omitted) `tests` attribute to "
                    f"an explicit `tests` list, but omits package test "
                    f"target(s) {missing_fmt}. When modifying a `test_suite()` "
                    f"with an empty `tests` attribute, all existing test targets "
                    f"in the package must be added to `tests` in addition to any "
                    f"newly added tests (or started from the existing list "
                    f"before removing entries)."
                )
                rem = (
                    f"Add {missing_fmt} to `tests` on `test_suite(name = "
                    f"\"{s_name}\")` in '{bz_rel}' (`tests = [{full_list_fmt}]`), "
                    f"and then report the resulting "
                    f"`test_suite_layout/expanded_empty_test_suite` finding in "
                    f"`CoderReport.disputed_findings` and `summary` requesting "
                    f"that a human reviewer ensure that all relevant tests have "
                    f"been included (do not allow agents to resolve that comment)."
                )
                findings.append({
                    "source": "test_suite_layout",
                    "category": "implicit_test_suite",
                    "severity": "error",
                    "file": bz_rel,
                    "line": lineno,
                    "message": msg,
                    "remediation": rem,
                })
            elif removed_still_listed:
                removed_fmt = ", ".join(f'":{t}"' for t in removed_still_listed)
                full_list_fmt = ", ".join(
                    f'":{t}"'
                    for t in sorted(
                        expanded_listed_local - set(removed_still_listed)
                    )
                )
                msg = (
                    f"`test_suite(name = \"{s_name}\")` in '{bz_rel}' was "
                    f"modified from an empty (or omitted) `tests` attribute to "
                    f"an explicit `tests` list, but still lists removed test "
                    f"target(s) {removed_fmt} that no longer exist in '{bz_rel}'."
                )
                rem = (
                    f"Start from the existing test targets in '{bz_rel}' and "
                    f"remove {removed_fmt} from `tests` on `test_suite(name = "
                    f"\"{s_name}\")` (`tests = [{full_list_fmt}]`), then report "
                    f"the resulting `test_suite_layout/expanded_empty_test_suite` "
                    f"finding in `CoderReport.disputed_findings` and `summary` "
                    f"requesting that a human reviewer ensure that all relevant "
                    f"tests have been included (do not allow agents to resolve "
                    f"that comment)."
                )
                findings.append({
                    "source": "test_suite_layout",
                    "category": "implicit_test_suite",
                    "severity": "error",
                    "file": bz_rel,
                    "line": lineno,
                    "message": msg,
                    "remediation": rem,
                })
            else:
                findings.append({
                    "source": "test_suite_layout",
                    "category": "expanded_empty_test_suite",
                    "severity": "error",
                    "file": bz_rel,
                    "line": lineno,
                    "message": (
                        f"`test_suite(name = \"{s_name}\")` in '{bz_rel}' was "
                        f"modified from an empty (or omitted) `tests` attribute "
                        f"to an explicit `tests = {json.dumps(members)}` list. "
                        f"A human reviewer must ensure that all relevant test "
                        f"targets in '{pkg_dir}' (including any macro-generated "
                        f"tests) have been included in `tests`, and agents must "
                        f"not resolve that review comment."
                    ),
                    "remediation": (
                        f"Verify that `tests = [...]` on `test_suite(name = "
                        f"\"{s_name}\")` in '{bz_rel}' includes all existing "
                        f"test targets in '{pkg_dir}' in addition to any added "
                        f"tests (or starts from the existing list of all test "
                        f"targets in the package and removes only deleted "
                        f"entries). Once verified, report this finding in "
                        f"`CoderReport.disputed_findings` (`source: "
                        f"\"test_suite_layout\"`, `category: "
                        f"\"expanded_empty_test_suite\"`, `file: \"{bz_rel}\"`) "
                        f"and `summary` with a `reason` requesting that a human "
                        f"reviewer ensure that all relevant tests in the package "
                        f"have been included in `test_suite(name = \"{s_name}\")` "
                        f"and stating: \"Do not resolve this comment "
                        f"automatically; a human reviewer must verify and "
                        f"resolve it.\" Agents must NEVER resolve this comment "
                        f"or drop this dispute."
                    ),
                })


# --- Main execution over changed BUILD.gn and BUILD.bazel files ---


changed = set(git_lines(["diff", "--name-only", effective_base]))
changed.update(git_lines(["diff", "--name-only", "HEAD"]))
changed.update(git_lines(["ls-files", "--others", "--exclude-standard"]))
for td in target_dirs:
    for bname in ("BUILD.bazel", "BUILD.gn"):
        rel = os.path.join(td, bname)
        if os.path.isfile(os.path.join(workdir, rel)):
            changed.add(rel)

changed_gn = sorted(
    f
    for f in changed
    if os.path.basename(f) == "BUILD.gn"
    and os.path.isfile(os.path.join(workdir, f))
)
changed_bz = sorted(
    f
    for f in changed
    if os.path.basename(f) in ("BUILD.bazel", "BUILD")
    and os.path.isfile(os.path.join(workdir, f))
)

for gn_rel in changed_gn:
    check_per_package_bazel_test_suite(gn_rel)
    check_bazel_test_suite_only_build_gn(gn_rel)
    check_nested_test_suite_in_gn(gn_rel)

for bz_rel in changed_bz:
    check_nested_test_suite_in_bazel(bz_rel)
    check_implicit_test_suite(bz_rel)

print(json.dumps(findings, indent=2))
PYEOF
