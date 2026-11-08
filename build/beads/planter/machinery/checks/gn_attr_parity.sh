#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# gn_attr_parity: GN-only target attributes that bazel2gn does not translate from a plain
# BUILD.bazel attribute must not be dropped.
#
# Some GN target attributes change how (or in which toolchain variants) GN builds a target and
# have no plain BUILD.bazel attribute that bazel2gn translates:
#   - exclude_toolchain_tags = [ ... ]  (e.g. "instrumented", "asan", "hwasan", "heapdump": the
#     target, and a rustc_* target's with_unit_tests test, are not built/run in those variants)
#   - configs -= [ ... ]                (removes default configs, e.g. implicit host libs)
#   - disable_syslog_backend = ...      (no implicit syslog backend dependency)
#   - toolchain-qualified assert_no_deps entries (e.g.
#     "//third_party/rust_crates:serde_json5($current_toolchain)": only the target toolchain is
#     restricted, so build tools may still use the crate; the unqualified `assert_no_deps=` tag
#     form would restrict every toolchain)
# Converting such a target to bazel2gn without naming the attribute in BUILD.bazel silently
# drops it: the generated GN target then fails to link in instrumented builders or its tests run
# (and fail) in sanitizer builders. bazel2gn emits them when BUILD.bazel names them with
# `# @bazel2gn:raw_attribute:` (or dedicated forms such as `disable_instrumentation`,
# `disable_build_flags`, and the tags newer bazel2gn versions document), and migration_sanity
# accepts such generated lines below the sentinel.
#
# For every changed package, each GN target at $PLANTER_CHANGE_BASE carrying one of these
# attributes (directly, or through `forward_variables_from(<scope>, ...)` of a scope defined in the
# same BUILD.gn) must still exist in the package's BUILD.gn with the same values
# (gn_only_attr_dropped, error).
#
# Exception (template semantics, see build/rust/rustc_library.gni): rustc_library and
# rustc_staticlib forward exclude_toolchain_tags only to their with_unit_tests test
# (`<name>_test`), never to the library artifact, and rustc_macro does not use it at all. For
# those templates a dropped exclude_toolchain_tags is accepted when the base target had no
# with_unit_tests, or when the current BUILD.gn keeps a hand-written `<name>_test` target with
# the same exclude_toolchain_tags values.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"

python3 - "$SCRIPT_DIR" "$WORKDIR" "$TARGET_DIR" <<'PYEOF'
import ast
import json
import os
import re
import subprocess
import sys

script_dir = os.path.abspath(sys.argv[1])
sys.path.insert(0, script_dir)
try:
    import planter_check_lib as pcl
except ImportError:
    pcl = None

workdir = os.path.abspath(sys.argv[2])
target_dir = sys.argv[3].strip().strip("/")
change_base = (
    os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip()
    or (pcl.resolve_change_base(workdir) if pcl is not None else "")
    or os.environ.get("PLANTER_CHANGE_BASE", "").strip()
    or "HEAD"
)


def _emit(findings, category, severity, file, line, message, remediation=""):
    d = {
        "source": "gn_attr_parity",
        "category": category,
        "severity": severity,
        "file": file,
        "line": line,
        "message": message,
    }
    if remediation:
        d["remediation"] = remediation
    findings.append(d)


GN_TARGET = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{')
NOT_TARGETS = {"template", "declare_args", "foreach", "forward_variables_from", "if"}
LIST_ATTRS = {
    "exclude_toolchain_tags": r"exclude_toolchain_tags\s*\+?=",
    "configs -=": r"configs\s*-=",
    "assert_no_deps": r"assert_no_deps\s*\+?=",
}
# GN variable each tracked attribute is assigned to (for forward_variables_from filtering).
ATTR_GN_VARS = {
    "exclude_toolchain_tags": "exclude_toolchain_tags",
    "configs -=": "configs",
    "assert_no_deps": "assert_no_deps",
    "disable_syslog_backend": "disable_syslog_backend",
}
SCALAR_ATTRS = {"disable_syslog_backend": r"disable_syslog_backend\s*="}
# Templates whose library artifact ignores exclude_toolchain_tags (only their unit test uses it).
TEST_ONLY_EXCLUDE_TEMPLATES = {"rustc_library", "rustc_staticlib"}
NO_EXCLUDE_TEMPLATES = {"rustc_macro"}
WITH_UNIT_TESTS = re.compile(r"(?<![\w.])with_unit_tests\s*=\s*true\b")


def git(args):
    try:
        return subprocess.run(["git", "-C", workdir] + args, capture_output=True, text=True, check=True).stdout
    except Exception:
        return None


# Extract same-file GN `template("<name>")` inner target bodies so invocations of local
# wrapper templates inherit their attributes (`test_deps`, `deps`, `sources`, `rustenv`, etc.).
def _extract_local_gn_templates(text):
    templates = {}
    if not text:
        return templates
    for m in GN_TARGET.finditer(text):
        if m.group(1) != "template":
            continue
        depth, i = 1, m.end()
        while i < len(text) and depth:
            depth += {"{": 1, "}": -1}.get(text[i], 0)
            i += 1
        tmpl_body = text[m.end() : i - 1]
        inner_m = re.search(r"\b([A-Za-z_]\w*)\(\s*target_name\s*\)\s*\{", tmpl_body)
        if inner_m:
            idepth, ii = 1, inner_m.end()
            while ii < len(tmpl_body) and idepth:
                idepth += {"{": 1, "}": -1}.get(tmpl_body[ii], 0)
                ii += 1
            templates[m.group(2)] = tmpl_body[inner_m.end() : ii - 1]
        else:
            templates[m.group(2)] = tmpl_body
    return templates


def targets(text):
    out = {}
    if not text:
        return out
    local_templates = _extract_local_gn_templates(text)
    for m in GN_TARGET.finditer(text):
        if m.group(1) in NOT_TARGETS:
            continue
        depth, i = 1, m.end()
        while i < len(text) and depth:
            depth += {"{": 1, "}": -1}.get(text[i], 0)
            i += 1
        body = text[m.end():i - 1]
        if m.group(1) in local_templates:
            body = local_templates[m.group(1)] + "\n" + body
        line = text.count("\n", 0, m.start()) + 1
        out.setdefault(m.group(2), (m.group(1), line, body))
    return out


# Tokenize GN `if (...)`, target/template headers, `assert(...)`, string literals, and braces.
GN_TOKEN_RE = re.compile(
    r'\b(if)\s*\(((?:[^(){}"]|"(?:\\.|[^"\\])*"|\([^()"{}]*\))+)\)\s*\{'
    r'|\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{'
    r'|\b(assert)\s*\(((?:[^,(){}"]|"(?:\\.|[^"\\])*"|\([^()"{}]*\))+)\s*(?:\)|,)'
    r'|"(?:\\.|[^"\\])*"'
    r"|\{|\}"
)


# Normalize equivalent GN host/Fuchsia platform guards (`!is_fuchsia`,
# `current_toolchain == host_toolchain`, `is_linux || is_mac`, etc.) to canonical
# `"is_host"` or `"is_fuchsia"` tokens for parity checks against bazel2gn output.
def _strip_outer_parens(expr):
    c = expr
    while c.startswith("(") and c.endswith(")"):
        inner = c[1:-1]
        depth = 0
        balanced = True
        for ch in inner:
            depth += {"(": 1, ")": -1}.get(ch, 0)
            if depth < 0:
                balanced = False
                break
        if balanced and depth == 0:
            c = inner
        else:
            break
    return c


def _normalize_platform_cond(raw_cond):
    c = _strip_outer_parens(re.sub(r"\s+", "", raw_cond or ""))
    if c.startswith("!(") and c.endswith(")"):
        neg_inner = _strip_outer_parens(c[1:])
        if neg_inner in ("is_fuchsia", "is_host"):
            c = f"!{neg_inner}"
    if c in (
        "is_host",
        "!is_fuchsia",
        "current_toolchain==host_toolchain",
        'current_os!="fuchsia"',
        "is_linux||is_mac",
        "is_mac||is_linux",
        "is_host&&is_linux",
        "is_linux&&is_host",
        "is_host&&is_mac",
        "is_mac&&is_host",
        "is_host&&(is_linux||is_mac)",
        "(is_linux||is_mac)&&is_host",
        "is_host||is_linux",
        "is_linux||is_host",
        "is_host||is_fidl_toolchain",
        "is_fidl_toolchain||is_host",
    ):
        return "is_host"
    if c in (
        "is_fuchsia",
        "!is_host",
        "current_toolchain!=host_toolchain",
        "current_toolchain==default_toolchain",
        'current_os=="fuchsia"',
    ):
        return "is_fuchsia"
    return (raw_cond or "").strip()


# Expand raw `(cond, line)` pairs into a set containing both raw and normalized platform tokens.
def _expand_platform_conds(cond_pairs):
    expanded = set()
    for raw_c, _ in cond_pairs:
        expanded.add(raw_c)
        expanded.add(_normalize_platform_cond(raw_c))
    return expanded


# Parse enclosing and inner `if (...)` / `assert(...)` conditions for each GN target in `text`,
# propagating conditions from file-level `assert(...)` and file-local `template(...)` definitions.
def parse_gn_target_conditions(text):
    out = {}
    if not text:
        return out
    cleaned = "\n".join(
        re.sub(r'("(?:\\.|[^"\\])*")|#.*$', lambda m: m.group(1) or "", ln)
        for ln in text.splitlines()
    )
    stack = []
    if_starts = {}
    tmpl_inner = {}
    tmpl_enclosing = {}
    file_asserts = []
    for m in GN_TOKEN_RE.finditer(cleaned):
        tok = m.group(0)
        line = cleaned.count("\n", 0, m.start()) + 1
        if m.group(1) == "if":
            cond = re.sub(r"\s+", " ", m.group(2).strip())
            tnames = []
            for kind, tname, _ in stack:
                if kind == "target" and tname in out:
                    out[tname]["inner"].append((cond, line))
                    tnames.append(tname)
                elif kind == "template" and tname:
                    tmpl_inner.setdefault(tname, []).append((cond, line))
            stack.append(("if", cond, line))
            if tnames:
                if_starts[len(stack)] = (tnames, cond, line, m.end())
        elif m.group(3):
            tmpl, tname = m.group(3), m.group(4)
            if tmpl == "template":
                stack.append(("template", tname, line))
                continue
            if tmpl in NOT_TARGETS:
                stack.append(("other", "", line))
                continue
            enclosing = [(c, l) for k, c, l in stack if k == "if"]
            info = out.setdefault(
                tname,
                {
                    "tmpl": tmpl,
                    "line": line,
                    "enclosing": [],
                    "direct_if_enclosing": [],
                    "inner": [],
                    "inner_bodies": {},
                },
            )
            info["enclosing"].extend(enclosing)
            info["direct_if_enclosing"].extend(enclosing)
            stack.append(("target", tname, line))
        elif m.group(5) == "assert":
            acond = _normalize_platform_cond(m.group(6))
            if acond not in ("is_host", "is_fuchsia"):
                continue
            in_target_or_tmpl = False
            for kind, tname, _ in stack:
                if kind == "target" and tname in out:
                    out[tname]["enclosing"].append((acond, line))
                    in_target_or_tmpl = True
                elif kind == "template" and tname:
                    tmpl_enclosing.setdefault(tname, []).append((acond, line))
                    in_target_or_tmpl = True
            if not in_target_or_tmpl and not any(k == "if" for k, _, _ in stack):
                file_asserts.append((acond, line))
        elif tok == "{":
            stack.append(("other", "", line))
        elif tok == "}" and stack:
            popped_depth = len(stack)
            stack.pop()
            if popped_depth in if_starts:
                tnames, icond, iline, istart = if_starts.pop(popped_depth)
                ibody = cleaned[istart : m.start()]
                for tn in tnames:
                    out[tn].setdefault("inner_bodies", {})[(icond, iline)] = ibody
    for tname, info in out.items():
        if file_asserts:
            info["enclosing"] = list(file_asserts) + info["enclosing"]
        if info["tmpl"] in tmpl_enclosing:
            info["enclosing"].extend(tmpl_enclosing[info["tmpl"]])
        if info["tmpl"] in tmpl_inner:
            info["inner"].extend(tmpl_inner[info["tmpl"]])
    return out


# Check whether an inner `if (...)` inside a `group(...)` only gates newly added
# `bazel_test_suite` dependencies (e.g. `if (!is_asan) { deps += [ ":bazel_tests" ] }`
# matching `exclude_toolchain_tags` on the GN test binary, which Bazel ignores).
def is_bazel_test_suite_inner_guard(pkg, new_info, cond, cond_line, above_after):
    if new_info.get("tmpl") != "group":
        return False
    ibody = new_info.get("inner_bodies", {}).get((cond, cond_line), "")
    if not ibody:
        return False
    above_code = re.sub(r"#[^\n]*", "", above_after or "")
    bts_names = set(
        re.findall(r'\bbazel_test_suite\(\s*"([^"$]+)"\s*\)', above_code)
    )
    if not bts_names:
        return False
    refs = re.findall(r'"([^"]+)"', ibody)
    if not refs:
        return False
    for ref in refs:
        base_ref = ref.split("(", 1)[0].strip()
        if base_ref.startswith(":"):
            tname = base_ref[1:]
        elif pkg and base_ref.startswith(f"//{pkg}:"):
            tname = base_ref[len(f"//{pkg}:") :]
        else:
            return False
        if tname not in bts_names:
            return False
    return True


# Collect unconditional dependency label strings from a Starlark AST node,
# ignoring conditional branches inside `select({...})`.
def _collect_unconditional_bazel_labels(node, top_vars):
    if node is None:
        return []
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return [node.value]
    if isinstance(node, (ast.List, ast.Tuple)):
        out = []
        for elem in node.elts:
            out.extend(_collect_unconditional_bazel_labels(elem, top_vars))
        return out
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        return _collect_unconditional_bazel_labels(
            node.left, top_vars
        ) + _collect_unconditional_bazel_labels(node.right, top_vars)
    if isinstance(node, ast.Name):
        return list(top_vars.get(node.id, []))
    return []


# Check whether `dep_pkg:dep_name` is already restricted to `cond` (`is_host` or
# `is_fuchsia`) in its own BUILD.gn or BUILD.bazel, meaning any target with an
# unconditional dependency on it could never build on other platforms in GN.
def _is_dep_restricted_to_platform(dep_pkg, dep_name, cond):
    gn_path = (
        os.path.join(workdir, dep_pkg, "BUILD.gn")
        if dep_pkg
        else os.path.join(workdir, "BUILD.gn")
    )
    if os.path.isfile(gn_path):
        try:
            with open(gn_path, encoding="utf-8") as gf:
                dep_gn = gf.read()
        except OSError:
            dep_gn = ""
        if dep_gn:
            dep_clean = "\n".join(
                re.sub(r'("(?:\\.|[^"\\])*")|#.*$', lambda m: m.group(1) or "", ln)
                for ln in dep_gn.splitlines()
            )
            if cond == "is_host" and re.search(
                r"(?m)^\s*assert\(\s*(?:is_host|!\s*is_fuchsia)\b", dep_clean
            ):
                return True
            if cond == "is_fuchsia" and re.search(
                r"(?m)^\s*assert\(\s*(?:is_fuchsia|!\s*is_host)\b", dep_clean
            ):
                return True
            dep_conds = parse_gn_target_conditions(dep_gn)
            if dep_name in dep_conds:
                if cond in _expand_platform_conds(dep_conds[dep_name]["enclosing"]):
                    return True
                if cond == "is_host" and dep_conds[dep_name].get("tmpl") in (
                    "bazel_ffx_tool",
                    "bazel_host_tool",
                    "ffx_plugin",
                    "ffx_tool",
                    "go_test",
                    "host_test",
                    "host_test_data",
                ):
                    return True
    bz_path = (
        os.path.join(workdir, dep_pkg, "BUILD.bazel")
        if dep_pkg
        else os.path.join(workdir, "BUILD.bazel")
    )
    if os.path.isfile(bz_path):
        try:
            with open(bz_path, encoding="utf-8") as bf:
                dep_bz = bf.read()
            dep_tree = ast.parse(dep_bz, filename=bz_path)
        except (OSError, SyntaxError):
            dep_bz, dep_tree = "", None
        if dep_tree is not None:
            host_only_rules = {
                "cc_binary_host_tool",
                "ffx_plugin",
                "ffx_tool",
                "go_binary_host_tool",
                "host_cc_test",
                "host_go_test",
                "host_py_test",
                "host_rustc_test",
                "host_shac_test",
                "host_test",
                "host_test_data_files",
                "host_test_data_map",
                "idk_cc_binary_host_tool",
                "idk_ffx_tool",
                "idk_go_binary_host_tool",
                "idk_rustc_binary_host_tool",
                "py_binary_host_tool",
                "rustc_binary_host_tool",
                "wrap_host_rust_test",
            }
            for stmt in dep_tree.body:
                if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
                    continue
                call = stmt.value
                rname = call.func.id if isinstance(call.func, ast.Name) else ""
                kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
                nm = kws.get("name")
                if not (isinstance(nm, ast.Constant) and nm.value == dep_name):
                    continue
                if cond == "is_host" and rname in host_only_rules:
                    return True
                tcw = kws.get("target_compatible_with")
                if tcw is not None:
                    tcw_seg = ast.get_source_segment(dep_bz, tcw) or ""
                    if "select(" not in tcw_seg:
                        if cond == "is_host" and (
                            "HOST_OS_CONSTRAINTS" in tcw_seg
                            or "HOST_CONSTRAINTS" in tcw_seg
                        ):
                            return True
                        if cond == "is_fuchsia" and "@platforms//os:fuchsia" in tcw_seg:
                            return True
    return False


# Check whether `target_name` in `bz_txt` has an unconditional dependency that is
# itself restricted to `cond` (`is_host` or `is_fuchsia`).
def _target_has_platform_restricted_dep(bz_txt, cur_pkg, target_name, cond):
    if not bz_txt or cond not in ("is_host", "is_fuchsia"):
        return False
    try:
        tree = ast.parse(bz_txt)
    except SyntaxError:
        return False
    top_vars = {}
    for stmt in tree.body:
        if (
            isinstance(stmt, ast.Assign)
            and len(stmt.targets) == 1
            and isinstance(stmt.targets[0], ast.Name)
        ):
            top_vars[stmt.targets[0].id] = _collect_unconditional_bazel_labels(
                stmt.value, top_vars
            )
    dep_attrs = (
        "deps",
        "public_deps",
        "implementation_deps",
        "link_deps",
        "proc_macro_deps",
    )
    for stmt in tree.body:
        if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
            continue
        call = stmt.value
        kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
        nm = kws.get("name")
        if not (isinstance(nm, ast.Constant) and nm.value == target_name):
            continue
        for attr in dep_attrs:
            for raw_lbl in _collect_unconditional_bazel_labels(kws.get(attr), top_vars):
                lbl = raw_lbl[1:] if raw_lbl.startswith("@//") else raw_lbl
                if lbl.startswith("//"):
                    body = lbl[2:]
                    if ":" in body:
                        d_pkg, d_name = body.split(":", 1)
                    else:
                        d_pkg = body.strip("/")
                        d_name = d_pkg.rsplit("/", 1)[-1]
                    d_pkg = d_pkg.strip("/")
                elif lbl.startswith(":"):
                    d_pkg, d_name = cur_pkg, lbl[1:]
                else:
                    continue
                if d_pkg.startswith("third_party/rust_crates"):
                    continue
                if _is_dep_restricted_to_platform(d_pkg, d_name, cond):
                    return True
    return False


# Check that retained hand-written GN targets above BAZEL2GN SENTINEL and bazel2gn-synced
# library/binary targets below BAZEL2GN SENTINEL do not silently gain new `if (...)`
# platform/toolchain constraints that were absent at $PLANTER_CHANGE_BASE.
def check_unrequested_gn_conditions(gn_rel, pkg, before, after):
    if not before or not after:
        return []
    parts = after.split("## BAZEL2GN SENTINEL", 1)
    above_after = parts[0]
    old_cond_map = parse_gn_target_conditions(before)
    new_cond_map = parse_gn_target_conditions(above_after)
    grouped = {}
    for tname, new_info in sorted(new_cond_map.items()):
        old_info = old_cond_map.get(tname)
        if not old_info:
            continue
        old_conds = _expand_platform_conds(
            old_info["enclosing"] + old_info["inner"]
        )
        for cond, cond_line in new_info["enclosing"] + new_info["inner"]:
            if cond in old_conds or _normalize_platform_cond(cond) in old_conds:
                continue
            if is_bazel_test_suite_inner_guard(
                pkg, new_info, cond, cond_line, above_after
            ):
                continue
            grouped.setdefault((cond, cond_line), []).append(
                f'`{new_info["tmpl"]}("{tname}")`'
            )
    out_findings = []
    for (cond, cond_line), target_descs in sorted(
        grouped.items(), key=lambda x: x[0][1]
    ):
        joined = ", ".join(target_descs)
        _emit(
            out_findings,
            "unrequested_gn_condition",
            "error",
            gn_rel,
            cond_line,
            (
                f"Retained GN target(s) {joined} in //{pkg} are newly gated by "
                f"`if ({cond})` on line {cond_line} of {gn_rel}, which was not "
                "present at the change base. Adding unrequested platform or "
                "toolchain conditions to retained GN targets or `group(\"tests\")` "
                "alters GN target availability across toolchains."
            ),
            (
                f"Remove the newly added `if ({cond})` wrapper on line {cond_line} "
                f"of {gn_rel} so {joined} keeps its original unconditional "
                "definition from the change base."
            ),
        )

    # Also check synced library/binary targets below BAZEL2GN SENTINEL for newly added
    # enclosing `if (is_fuchsia)` / `if (is_host)` platform restrictions on multi-platform
    # targets, and reject redundant `TODO: Verify whether ... should use target_compatible_with`
    # comments when `if (is_fuchsia)` / `if (is_host)` wrapping is expected.
    if len(parts) > 1:
        full_cond_map = parse_gn_target_conditions(after)
        bz_rel = f"{pkg}/BUILD.bazel" if pkg else "BUILD.bazel"
        bz_txt = ""
        try:
            with open(os.path.join(workdir, bz_rel), encoding="utf-8") as bf:
                bz_txt = bf.read()
        except OSError:
            pass
        head_sha = (git(["rev-parse", "HEAD"]) or "").strip()
        base_sha = (git(["rev-parse", "--verify", f"{change_base}^{{commit}}"]) or "").strip()
        head_touched = set(
            (
                git(["diff", "--name-only", "HEAD~1", "HEAD", "--", bz_rel, gn_rel])
                or ""
            ).splitlines()
        )
        bz_untracked = bool(
            (
                git(["ls-files", "--others", "--exclude-standard", "--", bz_rel])
                or ""
            ).strip()
        )
        has_task_commit = bool(
            head_sha
            and base_sha
            and head_sha != base_sha
            and head_touched
            and not bz_untracked
        )
        commit_body = (
            (git(["log", "-1", "--format=%b", "HEAD"]) or "") if has_task_commit else ""
        )
        note_re = re.compile(
            r"#\s*(?:TODO|Note|Needs reviewer attention)\b[^\n]*(?:target_compatible_with|Fuchsia-only|host-only|platform)",
            re.I,
        )
        false_not_gated_note_re = re.compile(
            r"#\s*(?:TODO|Note|Needs reviewer attention)\b[^\n]*target_compatible_with"
            r"[^\n]*(?:\bnot\s+(?:gated|wrapped|guarded)\b|\b(?:unguarded|no\s+explicit)\b)",
            re.I,
        )

        # Locate the target call line and check for explanatory or false-unguarded comments.
        def _bz_target_line_and_note(text, target_name):
            if not text:
                return 0, 0, False, False
            m_nm = re.search(r'(?m)^\s*name\s*=\s*"' + re.escape(target_name) + r'"', text)
            if not m_nm:
                return 0, 0, False, False
            call_start = text.rfind("\n(", 0, m_nm.start())
            for cm in re.finditer(r"(?m)^[A-Za-z_]\w*\s*\(", text[: m_nm.start()]):
                call_start = cm.start()
            if call_start < 0:
                call_start = m_nm.start()
            call_end = text.find("\n)", m_nm.end())
            call_end = len(text) if call_end < 0 else call_end + 2
            block = text[call_start:call_end]
            tcw_m = re.search(r"(?m)^\s*target_compatible_with\s*=", block)
            line_no = (
                text.count("\n", 0, call_start + tcw_m.start()) + 1
                if tcw_m
                else text.count("\n", 0, call_start) + 1
            )
            prefix_lines = text[:call_start].splitlines()
            above_comments = []
            for idx in range(len(prefix_lines) - 1, -1, -1):
                s = prefix_lines[idx].strip()
                if s.startswith("#"):
                    above_comments.append((idx + 1, s))
                elif s == "":
                    if above_comments:
                        break
                else:
                    break
            ordered_above = list(reversed(above_comments))
            unwrapped_above = (
                "# " + " ".join(s.lstrip("#").strip() for _, s in ordered_above)
                if ordered_above
                else ""
            )
            above_ctx = "\n".join(s for _, s in ordered_above) + "\n" + unwrapped_above
            comment_ctx = above_ctx + "\n" + block
            note_ln = line_no
            if ordered_above and false_not_gated_note_re.search(above_ctx):
                note_ln = next(
                    (
                        ln
                        for ln, s in ordered_above
                        if re.search(
                            r"(?:TODO|Note|Needs reviewer attention|target_compatible_with)",
                            s,
                            re.I,
                        )
                    ),
                    ordered_above[0][0],
                )
            return (
                line_no,
                note_ln,
                bool(note_re.search(comment_ctx)),
                bool(false_not_gated_note_re.search(comment_ctx)),
            )

        for tname, new_info in sorted(full_cond_map.items()):
            if tname in new_cond_map:
                continue
            if new_info["tmpl"] in ("rustc_test", "test", "go_test", "host_test"):
                continue
            old_info = old_cond_map.get(tname)
            if not old_info:
                continue
            old_enc = _expand_platform_conds(old_info["enclosing"])
            old_direct_if_enc = _expand_platform_conds(
                old_info.get("direct_if_enclosing", [])
            )
            old_inner = [c for c, _ in old_info["inner"]]
            had_multiplatform_inner = any(
                "is_fuchsia" in c or "is_host" in c for c in old_inner
            )
            bz_line, bz_note_line, has_bz_note, has_false_not_gated_note = (
                _bz_target_line_and_note(bz_txt, tname)
            )
            rep_ln = bz_line or 1
            has_msg_note = (not has_task_commit) or bool(
                re.search(
                    r"\b(?:target_compatible_with|Fuchsia-only|host-only)\b",
                    commit_body,
                    re.I,
                )
            )
            for cond, cond_line in new_info["enclosing"]:
                if cond not in ("is_fuchsia", "is_host"):
                    continue
                # Targets already restricted to `cond` in BUILD.gn via `if (...)`,
                # equivalent toolchain/OS guards, or `assert(is_host)`/`assert(is_fuchsia)`
                # preserve 1:1 platform parity without needing extra `# Note:` or `# TODO:`
                # boilerplate claiming the target was unguarded in GN.
                if cond in old_enc:
                    if has_false_not_gated_note:
                        guard_desc = (
                            "an enclosing `if (...)` platform guard"
                            if cond in old_direct_if_enc
                            else f"`assert({cond})` (or an equivalent platform guard)"
                        )
                        _emit(
                            out_findings,
                            "redundant_target_compatible_with_todo",
                            "error",
                            bz_rel,
                            bz_note_line or rep_ln,
                            (
                                f"Target `{tname}` in `{bz_rel}` has a comment/TODO claiming "
                                f"`{tname}` was not gated by `if ({cond})` in GN, but "
                                f"`{gn_rel}` at the change base already restricted `{tname}` "
                                f"to `{cond}` via {guard_desc}."
                            ),
                            (
                                f"Remove the `# TODO: Verify whether {tname} should use "
                                f"target_compatible_with ...` comment on `{tname}` in "
                                f"`{bz_rel}` while keeping `target_compatible_with` synced "
                                "(do not add `# @bazel2gn:skip`)."
                            ),
                        )
                        break
                    continue
                if had_multiplatform_inner:
                    _emit(
                        out_findings,
                        "unrequested_gn_condition",
                        "error",
                        bz_rel,
                        rep_ln,
                        (
                            f"Target `{tname}` in `{bz_rel}` adds `target_compatible_with` "
                            f"(generating `if ({cond})` in `{gn_rel}`), narrowing `{tname}` "
                            "to a single platform even though the base GN target supported "
                            "multiple platforms via conditional dependencies "
                            f"(`if ({old_inner[0]})`)."
                        ),
                        (
                            f"Remove `target_compatible_with` from `{tname}` in `{bz_rel}`, "
                            'use `select({"@platforms//os:fuchsia": [...], '
                            '"//conditions:default": []})` on platform-specific '
                            "`deps`/`srcs`, and re-run `fx bazel2gn`."
                        ),
                    )
                    continue
                has_explicit_doc = has_bz_note or (has_task_commit and has_msg_note)
                if _target_has_platform_restricted_dep(bz_txt, pkg, tname, cond):
                    if has_false_not_gated_note:
                        _emit(
                            out_findings,
                            "redundant_target_compatible_with_todo",
                            "error",
                            bz_rel,
                            bz_note_line or rep_ln,
                            (
                                f"Target `{tname}` in `{bz_rel}` has a redundant `# TODO` "
                                f"questioning `target_compatible_with`, even though `{tname}` "
                                f"depends unconditionally on `{cond}`-restricted targets and "
                                "therefore requires `target_compatible_with`."
                            ),
                            (
                                f"Remove the `# TODO: Verify whether {tname} should use "
                                f"target_compatible_with ...` comment on `{tname}` in "
                                f"`{bz_rel}` while keeping `target_compatible_with` synced "
                                "(do not add `# @bazel2gn:skip`)."
                            ),
                        )
                        break
                    continue
                if not has_explicit_doc:
                    _emit(
                        out_findings,
                        "unrequested_gn_condition",
                        "error",
                        bz_rel,
                        rep_ln,
                        (
                            f"Target `{tname}` in `{bz_rel}` adds `target_compatible_with` "
                            f"(generating `if ({cond})` in `{gn_rel}`), narrowing `{tname}` to "
                            f"`{cond}` when `{gn_rel}` at the change base did not wrap `{tname}` "
                            f"in `if ({cond})`."
                        ),
                        (
                            f"If `{tname}` itself can build on both host and Fuchsia (and only "
                            "its unit tests were platform-specific in GN), remove "
                            f"`target_compatible_with` from `{tname}` in `{bz_rel}` (keeping "
                            "`target_compatible_with` only on the platform-specific test "
                            f"target) and re-run `fx bazel2gn`. If `{tname}` genuinely cannot "
                            "compile on other platforms, add `# TODO: Verify whether "
                            f"{tname} should use target_compatible_with (was not gated by "
                            f"if ({cond}) in GN).` directly above `{tname}` in `{bz_rel}`, "
                            f"re-run `fx bazel2gn -d {pkg}`, and note the added "
                            "`target_compatible_with` restriction in the commit message body."
                        ),
                    )
    return out_findings


def gn_only_attrs(body, text=None):
    """GN-only attribute values of a target body. With the BUILD.gn `text`, also the values the
    body pulls in through `forward_variables_from(<scope>, ...)` of a scope literal in `text`."""
    body_code = re.sub(r"#[^\n]*", "", body)
    found = {}
    for attr, pat in LIST_ATTRS.items():
        vals = set()
        for m in re.finditer(r"(?<![\w.])" + pat + r"\s*\[([^\]]*)\]", body_code):
            vals |= set(re.findall(r'"([^"]+)"', m.group(1)))
        if attr == "assert_no_deps":
            # Only toolchain-qualified entries: the unqualified ones map to `assert_no_deps=` tags.
            vals = {v for v in vals if "($" in v}
        if vals:
            found[attr] = vals
    for attr, pat in SCALAR_ATTRS.items():
        m = re.search(r"(?<![\w.])" + pat + r"\s*(\w+)", body_code)
        if m:
            found[attr] = {m.group(1)}
    if text:
        scopes = gn_scope_literals(text)
        for fm in re.finditer(
            r"(?<![\w.])forward_variables_from\(\s*([A-Za-z_]\w*)\s*,\s*(\"\*\"|\[[^\]]*\])\s*(?:,\s*(\[[^\]]*\]))?\s*\)",
            body_code,
        ):
            scope_body = scopes.get(fm.group(1))
            if scope_body is None:
                continue
            wanted = None if fm.group(2) == '"*"' else set(re.findall(r'"([^"]+)"', fm.group(2)))
            excluded = set(re.findall(r'"([^"]+)"', fm.group(3) or ""))
            for attr, vals in gn_only_attrs(scope_body).items():
                gn_var = ATTR_GN_VARS[attr]
                if gn_var in excluded or (wanted is not None and gn_var not in wanted):
                    continue
                found.setdefault(attr, set()).update(vals)
    return found


_scope_cache = {}


def gn_scope_literals(text):
    """Bodies of top-level `name = { ... }` scope literals in a BUILD.gn."""
    key = hash(text)
    if key in _scope_cache:
        return _scope_cache[key]
    scopes = {}
    for m in re.finditer(r"(?m)^([A-Za-z_]\w*)\s*=\s*\{", text):
        depth, i = 1, m.end()
        while i < len(text) and depth:
            depth += {"{": 1, "}": -1}.get(text[i], 0)
            i += 1
        scopes[m.group(1)] = text[m.end():i - 1]
    _scope_cache[key] = scopes
    return scopes


def eval_starlark(node, top_vars):
    if node is None:
        return []
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return [node.value]
    if isinstance(node, ast.Name):
        return list(top_vars.get(node.id, [node.id]))
    if isinstance(node, (ast.List, ast.Tuple)):
        out = []
        for elem in node.elts:
            out.extend(eval_starlark(elem, top_vars))
        return out
    if isinstance(node, ast.BinOp):
        return eval_starlark(node.left, top_vars) + eval_starlark(node.right, top_vars)
    if isinstance(node, ast.Call):
        out = []
        for arg in node.args:
            out.extend(eval_starlark(arg, top_vars))
        return out
    if isinstance(node, ast.Dict):
        out = []
        for val in node.values:
            out.extend(eval_starlark(val, top_vars))
        return out
    return []


def parse_gn_top_attrs(body, top_list_vars=None):
    attrs = {}
    lines = body.splitlines()
    i = 0
    assign_re = re.compile(r"^([A-Za-z_]\w*)\s*(\+?=|-=)\s*(.*)$")
    while i < len(lines):
        code = re.sub(r"#.*$", "", lines[i]).strip()
        if not code or code == "}":
            i += 1
            continue
        m = assign_re.match(code)
        if not m or (m.group(2) == "=" and m.group(3).lstrip().startswith("=")):
            i += 1
            continue
        attr_name, op, rhs = m.group(1), m.group(2), m.group(3).strip()
        balance = rhs.count("[") - rhs.count("]")
        while (balance > 0 or rhs.rstrip().endswith("+")) and i + 1 < len(lines):
            i += 1
            nxt = re.sub(r"#.*$", "", lines[i]).strip()
            rhs += " " + nxt
            balance += nxt.count("[") - nxt.count("]")
        vals = []
        if top_list_vars:
            unquoted_rhs = re.sub(r'"[^"]*"', " ", rhs)
            for ident in re.findall(r"\b([A-Za-z_]\w*)\b", unquoted_rhs):
                if ident in top_list_vars:
                    vals.extend(top_list_vars[ident])
        vals.extend(re.findall(r'"([^"]+)"', rhs))
        # Preserve unquoted GN config variable references such as `_concurrent_jobs.configs`.
        if not vals and attr_name == "configs":
            tok = rhs.strip()
            if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_.]*", tok):
                vals = [tok]
        if op == "+=":
            cur_vals = attrs.setdefault(attr_name, [])
            cur_vals.extend(v for v in vals if v not in cur_vals)
        elif op == "=":
            if vals:
                cur_vals = attrs.setdefault(attr_name, [])
                cur_vals.extend(v for v in vals if v not in cur_vals)
            else:
                attrs.setdefault(attr_name, [])
        i += 1
    return attrs


# Parse top-level GN list variables (outside target blocks) so target attributes
# referencing shared lists like `deps = lib_deps + [...]` expand cleanly.
def parse_gn_top_list_vars(text):
    if not text:
        return {}
    spans = []
    for m in GN_TARGET.finditer(text):
        if m.group(1) in NOT_TARGETS and m.group(1) != "template":
            continue
        depth, i = 1, m.end()
        while i < len(text) and depth:
            depth += {"{": 1, "}": -1}.get(text[i], 0)
            i += 1
        spans.append((m.start(), i))
    if spans:
        chunks = []
        last = 0
        for s, e in spans:
            if s > last:
                chunks.append(text[last:s])
            last = max(last, e)
        if last < len(text):
            chunks.append(text[last:])
        outer_text = "\n".join(chunks)
    else:
        outer_text = text
    raw_vars = parse_gn_top_attrs(outer_text)
    return {k: v for k, v in raw_vars.items() if v}


# Check whether a Starlark AST node is the boolean literal `value` (True/False).
def _is_starlark_bool(node, value):
    if isinstance(node, ast.Constant):
        return node.value is value
    return isinstance(node, ast.Name) and node.id in (str(value), str(value).lower())


# Check whether a BUILD.bazel `package(default_testonly = True)` call is present.
def bazel_has_default_testonly(bazel_tree):
    if bazel_tree is None:
        return False
    for stmt in bazel_tree.body:
        if isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call):
            call = stmt.value
            if isinstance(call.func, ast.Name) and call.func.id == "package":
                for kw in call.keywords:
                    if kw.arg == "default_testonly":
                        return _is_starlark_bool(kw.value, True)
    return False


# Check whether a Bazel target is testonly, explicitly or via package default_testonly.
def _bazel_target_has_testonly(bt_kws, pkg_default_testonly=False):
    to_node = bt_kws.get("testonly")
    if to_node is None:
        return pkg_default_testonly
    if _is_starlark_bool(to_node, True):
        return True
    return pkg_default_testonly and not _is_starlark_bool(to_node, False)


# GN bridge templates whose `testonly = true` attribute wraps cross-graph Bazel/GN
# inputs or actions rather than a direct same-package Bazel target equivalent.
GN_BAZEL_BRIDGE_TEMPLATES = (
    "bazel_action",
    "bazel_build_action",
    "bazel_build_group",
    "bazel_input_directory",
    "bazel_input_file",
    "export_fuchsia_package_to_bazel",
    "icu_aib_bazel_inputs",
)


# Verify that GN `testonly = true` on a newly migrated target is preserved on its Bazel target.
def check_testonly_dropped_from_gn(
    pkg,
    gn_rel,
    bazel_rel,
    body,
    bt_name,
    bt_rule,
    bt_line,
    bt_kws,
    pkg_default_testonly=False,
    cur=None,
    before_bt=None,
    before_pkg_default_testonly=False,
    tmpl="",
):
    if (
        tmpl in GN_BAZEL_BRIDGE_TEMPLATES
        or bt_rule in BAZEL_TEST_RULES
        or bt_rule
        in (
            "assembly_input_bundle",
            "cc_test_executable",
            "fuchsia_test_component",
            "fuchsia_unittest_component",
            "fuchsia_unittest_package",
            "fuchsia_test_package",
            "fx_bootfs_files_package",
            "host_shac_test",
            "icu_assembly_input_bundle",
            "platform_artifacts",
            "prebuilt_package",
            "test_suite",
        )
    ):
        return None
    body_code = re.sub(r"#[^\n]*", "", body)
    if not re.search(r"(?<![\w.])testonly\s*=\s*true\b", body_code):
        return None
    if _bazel_target_has_testonly(bt_kws, pkg_default_testonly):
        return None
    # Skip pre-existing BUILD.bazel targets that already had no `testonly = True` at the
    # change base (or whose retained BUILD.gn definition still carries `testonly = true`).
    if before_bt is not None and not _bazel_target_has_testonly(
        before_bt[-1], before_pkg_default_testonly
    ):
        return None
    if cur is not None and before_bt is not None:
        cur_code = re.sub(r"#[^\n]*", "", cur[2])
        if re.search(r"(?<![\w.])testonly\s*=\s*true\b", cur_code):
            return None
    return {
        "source": "gn_attr_parity",
        "category": "attribute_alignment",
        "severity": "error",
        "file": bazel_rel,
        "line": bt_line,
        "message": (
            f'Target "{bt_name}" (Target Attributes): '
            "testonly in GN not carried over to Bazel"
        ),
        "remediation": (
            f'Restore `testonly = True` on `{bt_rule}(name = "{bt_name}")` in '
            f"'{bazel_rel}' and re-run `fx bazel2gn -d {pkg}` so `{gn_rel}` "
            "preserves `testonly = true` from the change base. If `testonly` was "
            "dropped for an `fx_package` assembly cutover that cannot proceed "
            "(for example, because a packaged binary links a dual-built "
            "`rustc_dylib` that would duplicate the shared library in BlobFS and "
            "exceed release size creep budgets), keep the `fuchsia_package` in "
            "`BUILD.gn` bridged via `//bundles/assembly/bazel_inputs/...`, keep "
            f"`testonly = True` on `{bt_name}`, and set `disputed_findings: []`."
        ),
    }


# Verify that a custom GN `name`, `output_name`, or `crate_name` on a migrated target at
# $PLANTER_CHANGE_BASE is preserved on the generated GN target (via `crate_name` or
# `binary_name` in BUILD.bazel for `rustc_test` / `rustc_binary`).
def check_custom_output_or_crate_name_dropped(
    pkg, gn_rel, bazel_rel, name, tmpl, bt_rule, bt_line, gn_top, cur, cur_top
):
    if not cur or cur_top is None:
        return None
    rust_bin_tmpls = ("rustc_test", "rustc_binary")
    rust_lib_tmpls = ("rustc_library", "rustc_macro", "rustc_dylib", "rustc_staticlib")
    cc_bin_tmpls = ("executable", "test", "cc_test_executable")
    if tmpl in rust_bin_tmpls:
        base_vals = gn_top.get("output_name") or gn_top.get("name") or gn_top.get("crate_name")
        if not base_vals:
            return None
        expected = base_vals[0]
        if expected.replace("-", "_") == name.replace("-", "_"):
            return None
        cur_vals = (
            cur_top.get("output_name", [])
            + cur_top.get("name", [])
            + cur_top.get("crate_name", [])
        )
        if any(v.replace("-", "_") == expected.replace("-", "_") for v in cur_vals):
            return None
        exp_crate = expected.replace("-", "_")
        # rules_rust names a binary's executable from binary_name, not crate_name, so crate_name
        # would only rename the GN output and leave the Bazel binary misnamed. binary_name is the
        # literal file name, so keep any hyphens GN's output_name had.
        if tmpl == "rustc_binary":
            name_attr, name_val = "binary_name", expected
        else:
            name_attr, name_val = "crate_name", exp_crate
        return {
            "source": "gn_attr_parity",
            "category": "gn_custom_output_name_dropped",
            "severity": "error",
            "file": bazel_rel,
            "line": bt_line,
            "message": (
                f'`{tmpl}("{name}")` in //{pkg} set custom binary/crate name `"{expected}"` '
                f"in BUILD.gn at the change base, but the migrated target in '{gn_rel}' no "
                f'longer sets `output_name = "{exp_crate}"` (changing the GN binary output '
                f"name to `{name.replace('-', '_')}` and risking duplicate output collisions)."
            ),
            "remediation": (
                f'Set `{name_attr} = "{name_val}"` on `{bt_rule}(name = "{name}")` in '
                f"'{bazel_rel}' and re-run `fx bazel2gn -d {pkg}` so the generated GN target "
                f'keeps `output_name = "{name_val}"`.'
            ),
        }
    if tmpl in rust_lib_tmpls:
        base_vals = gn_top.get("crate_name") or gn_top.get("name")
        if not base_vals:
            return None
        expected = base_vals[0].replace("-", "_")
        if expected == name.replace("-", "_"):
            return None
        cur_vals = cur_top.get("crate_name", []) + cur_top.get("name", [])
        if any(v.replace("-", "_") == expected for v in cur_vals):
            return None
        return {
            "source": "gn_attr_parity",
            "category": "gn_custom_output_name_dropped",
            "severity": "error",
            "file": bazel_rel,
            "line": bt_line,
            "message": (
                f'`{tmpl}("{name}")` in //{pkg} set custom crate name `"{expected}"` in '
                f"BUILD.gn at the change base, but the migrated target in '{gn_rel}' lost it."
            ),
            "remediation": (
                f'Set `crate_name = "{expected}"` on `{bt_rule}(name = "{name}")` in '
                f"'{bazel_rel}' and re-run `fx bazel2gn -d {pkg}`."
            ),
        }
    if tmpl in cc_bin_tmpls and "output_name" in gn_top:
        base_vals = gn_top.get("output_name", [])
        if not base_vals or base_vals[0] == name:
            return None
        expected = base_vals[0]
        if expected in cur_top.get("output_name", []):
            return None
        return {
            "source": "gn_attr_parity",
            "category": "gn_custom_output_name_dropped",
            "severity": "error",
            "file": bazel_rel,
            "line": bt_line,
            "message": (
                f'`{tmpl}("{name}")` in //{pkg} set `output_name = "{expected}"` in BUILD.gn '
                f"at the change base, but the migrated target in '{gn_rel}' dropped it."
            ),
            "remediation": (
                f'Set `gn_output_name = "{expected}"` on `{bt_rule}(name = "{name}")` in '
                f"'{bazel_rel}' (and `binary_name = \"{expected}\"` on `fx_packaged_binary` "
                f"when packaged; never `# @bazel2gn:raw_attribute:output_name`) and re-run "
                f"`fx bazel2gn -d {pkg}`."
            ),
        }
    return None


CC_RULES = {
    "fx_cc_library",
    "cc_library",
    "fx_cc_binary",
    "cc_binary",
    "fx_cc_test_binary",
    "cc_test",
}
CC_SRC_EXTS = (".c", ".cc", ".cpp", ".cxx", ".h", ".hh", ".hpp", ".inl", ".inc", ".S")
QUOTED_INCLUDE_RE = re.compile(r'^\s*#\s*include\s*"([^"]+)"', re.M)


# Resolve a BUILD.bazel source/header entry to (workspace_rel_path, source_pkg_prefix).
def resolve_bazel_src_path(b_pkg, entry):
    s = entry.strip()
    if not s or "$" in s or s.startswith("@"):
        return None, None
    if s.startswith("//"):
        if ":" not in s:
            return None, None
        l_pkg, l_rel = s[2:].split(":", 1)
        l_pkg = l_pkg.strip("/")
        norm = os.path.normpath(os.path.join(l_pkg, l_rel)).strip("/")
        return norm, l_pkg
    if s.startswith(":"):
        s = s[1:]
    norm = os.path.normpath(os.path.join(b_pkg, s)).strip("/")
    return norm, b_pkg


# Check whether a Starlark AST node contains a glob() call.
def ast_has_glob(node):
    if node is None:
        return False
    for child in ast.walk(node):
        if isinstance(child, ast.Call) and isinstance(child.func, ast.Name):
            if child.func.id == "glob":
                return True
    return False


# Verify that every quoted relative #include "..." in C/C++ sources and headers
# across all preprocessor branches (including non-default target CPUs like
# riscv64/arm64) is declared in the BUILD.bazel target's srcs/hdrs/textual_hdrs
# or exported via hdrs/textual_hdrs/hdrs_for_internal_use of its deps.
_bazel_pkg_ast_cache = {}


def load_bazel_pkg_targets(b_pkg):
    if b_pkg in _bazel_pkg_ast_cache:
        return _bazel_pkg_ast_cache[b_pkg]
    b_rel = f"{b_pkg}/BUILD.bazel"
    b_abs = os.path.join(workdir, b_rel)
    if not os.path.isfile(b_abs):
        _bazel_pkg_ast_cache[b_pkg] = ({}, {})
        return {}, {}
    try:
        b_tree = ast.parse(open(b_abs, encoding="utf-8").read(), filename=b_rel)
    except Exception:
        _bazel_pkg_ast_cache[b_pkg] = ({}, {})
        return {}, {}
    top_vars = {}
    pkg_targets = {}
    for stmt in b_tree.body:
        if isinstance(stmt, ast.Assign):
            for tgt in stmt.targets:
                if isinstance(tgt, ast.Name):
                    top_vars[tgt.id] = eval_starlark(stmt.value, top_vars)
        elif isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call):
            call = stmt.value
            rule = call.func.id if isinstance(call.func, ast.Name) else None
            if not rule:
                continue
            kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
            tname_node = kws.get("name")
            if isinstance(tname_node, ast.Constant) and isinstance(tname_node.value, str):
                pkg_targets[tname_node.value] = (rule, call.lineno, kws)
    _bazel_pkg_ast_cache[b_pkg] = (top_vars, pkg_targets)
    return top_vars, pkg_targets


def resolve_bazel_target_ref(cur_pkg, label):
    s = label.strip()
    if not s or "$" in s or s.startswith("@"):
        return None, None
    if s.startswith(":"):
        return cur_pkg, s[1:]
    if s.startswith("//"):
        body = s[2:]
        if ":" in body:
            d_pkg, d_name = body.split(":", 1)
            return d_pkg.strip("/"), d_name.strip()
        d_pkg = body.strip("/")
        return d_pkg, os.path.basename(d_pkg)
    if "/" not in s:
        return cur_pkg, s
    return None, None


def collect_target_declared_files(t_pkg, kws, top_vars, attrs):
    raw_entries = []
    for attr in attrs:
        raw_entries.extend(eval_starlark(kws.get(attr), top_vars))
    declared = {}
    for rf in raw_entries:
        ref_pkg, ref_name = resolve_bazel_target_ref(t_pkg, rf)
        if ref_pkg is not None and ref_name:
            fg_vars, fg_targets = load_bazel_pkg_targets(ref_pkg)
            fg = fg_targets.get(ref_name)
            if fg and fg[0] == "filegroup":
                for fg_src in eval_starlark(fg[2].get("srcs"), fg_vars):
                    norm_p, src_prefix = resolve_bazel_src_path(ref_pkg, fg_src)
                    if norm_p:
                        declared[norm_p] = src_prefix
                continue
        norm_p, src_prefix = resolve_bazel_src_path(t_pkg, rf)
        if norm_p:
            declared[norm_p] = src_prefix
    return declared


def collect_dep_exported_headers(b_pkg, kws, top_vars, src_prefixes):
    direct_attrs = (
        "deps",
        "implementation_deps",
        "fuchsia_deps",
        "fuchsia_implementation_deps",
        "non_fuchsia_deps",
    )
    transitive_attrs = ("deps", "fuchsia_deps", "non_fuchsia_deps", "actual")
    hdr_attrs = ("hdrs", "textual_hdrs", "hdrs_for_internal_use")
    queue = []
    for attr in direct_attrs:
        for lbl in eval_starlark(kws.get(attr), top_vars):
            d_pkg, d_name = resolve_bazel_target_ref(b_pkg, lbl)
            if d_pkg is not None and d_name:
                queue.append((d_pkg, d_name))
    visited = set()
    exported_hdrs = set()
    has_dep_glob = False
    while queue:
        d_pkg, d_name = queue.pop(0)
        if (d_pkg, d_name) in visited:
            continue
        visited.add((d_pkg, d_name))
        d_vars, d_targets = load_bazel_pkg_targets(d_pkg)
        d_tgt = d_targets.get(d_name)
        if not d_tgt:
            continue
        _, _, d_kws = d_tgt
        if d_pkg in src_prefixes and any(ast_has_glob(d_kws.get(a)) for a in hdr_attrs):
            has_dep_glob = True
        exported_hdrs.update(
            collect_target_declared_files(d_pkg, d_kws, d_vars, hdr_attrs).keys()
        )
        for t_attr in transitive_attrs:
            for sub_lbl in eval_starlark(d_kws.get(t_attr), d_vars):
                sub_pkg, sub_name = resolve_bazel_target_ref(d_pkg, sub_lbl)
                if sub_pkg is not None and sub_name and (
                    sub_pkg == d_pkg or sub_pkg in src_prefixes
                ):
                    queue.append((sub_pkg, sub_name))
    return exported_hdrs, has_dep_glob


def check_undeclared_cc_local_headers(pkg):
    out = []
    candidate_pkgs = [pkg]
    bazel_path = os.path.join(workdir, pkg, "BUILD.bazel")
    if os.path.isfile(bazel_path):
        try:
            raw_text = open(bazel_path, encoding="utf-8").read()
        except OSError:
            raw_text = ""
        compat_root = os.path.join(workdir, "third_party/rust_crates/compat")
        if os.path.isdir(compat_root):
            try:
                compat_entries = sorted(os.listdir(compat_root))
            except OSError:
                compat_entries = []
            for m in re.finditer(
                r"//third_party/rust_crates/(?:vendor:([A-Za-z0-9_-]+)|compat/([^:\"\s]+))",
                raw_text,
            ):
                if m.group(2):
                    c_pkg = f"third_party/rust_crates/compat/{m.group(2).strip('/')}"
                    if c_pkg not in candidate_pkgs:
                        candidate_pkgs.append(c_pkg)
                elif m.group(1):
                    crate_norm = m.group(1).replace("_", "-")
                    for ce in compat_entries:
                        if ce == crate_norm or ce.startswith(crate_norm + "-"):
                            c_pkg = f"third_party/rust_crates/compat/{ce}"
                            if c_pkg not in candidate_pkgs:
                                candidate_pkgs.append(c_pkg)

    for b_pkg in candidate_pkgs:
        b_rel = f"{b_pkg}/BUILD.bazel"
        top_vars, pkg_targets = load_bazel_pkg_targets(b_pkg)
        if not pkg_targets:
            continue
        cc_targets = [
            (tname, rule, lineno, kws)
            for tname, (rule, lineno, kws) in pkg_targets.items()
            if rule in CC_RULES
        ]

        for tname, rule, lineno, kws in cc_targets:
            if any(ast_has_glob(kws.get(a)) for a in ("srcs", "hdrs", "textual_hdrs")):
                continue
            declared = collect_target_declared_files(
                b_pkg, kws, top_vars, ("srcs", "hdrs", "textual_hdrs", "hdrs_for_internal_use")
            )
            if not declared:
                continue
            src_prefixes = set(declared.values()) | {b_pkg}
            dep_hdrs, has_dep_glob = collect_dep_exported_headers(
                b_pkg, kws, top_vars, src_prefixes
            )
            if has_dep_glob:
                continue
            reported = set()
            for f_rel, src_prefix in sorted(declared.items()):
                if not f_rel.endswith(CC_SRC_EXTS):
                    continue
                f_abs = os.path.join(workdir, f_rel)
                if not os.path.isfile(f_abs):
                    continue
                try:
                    content = open(f_abs, encoding="utf-8", errors="replace").read()
                except OSError:
                    continue
                f_dir = os.path.dirname(f_rel)
                for inc_m in QUOTED_INCLUDE_RE.finditer(content):
                    inc = inc_m.group(1).strip()
                    cand_rel = os.path.normpath(os.path.join(f_dir, inc)).strip("/")
                    if os.path.dirname(cand_rel) != f_dir:
                        continue
                    if not (cand_rel == src_prefix or cand_rel.startswith(src_prefix + "/")):
                        continue
                    if cand_rel in declared or cand_rel in dep_hdrs or cand_rel in reported:
                        continue
                    if not os.path.isfile(os.path.join(workdir, cand_rel)):
                        continue
                    reported.add(cand_rel)
                    _emit(
                        out,
                        "undeclared_cc_local_header",
                        "error",
                        b_rel,
                        lineno,
                        (
                            f'`{rule}("{tname}")` in {b_rel} compiles `{f_rel}`, which has '
                            f'`#include "{inc}"` resolving to `{cand_rel}` on disk, but '
                            f"`{cand_rel}` is not listed in `srcs`, `hdrs`, or `textual_hdrs` "
                            "(nor exported via `hdrs` of its `deps`). "
                            "GN compiles unsandboxed against the source tree, whereas Bazel "
                            "enforces sandboxed header inclusion checking (including on "
                            "non-default target CPUs such as riscv64 or arm64 when `#include` "
                            "is guarded by an `#if` branch)."
                        ),
                        (
                            f"Add `{cand_rel}` (or its `//{src_prefix}:...` label) to `srcs` "
                            f'or `hdrs` (or to `hdrs` of a library in `deps`) of `{rule}("{tname}")` '
                            f"in `{b_rel}`, and if `{b_pkg}/BUILD.gn` is dual-built, regenerate "
                            f"it with `fx bazel2gn -d {b_pkg}`."
                        ),
                    )
    return out


env_changed = os.environ.get("PLANTER_CHANGED_FILES", "").strip()
if env_changed:
    changed = [p.strip() for p in env_changed.splitlines() if p.strip()]
else:
    changed = (git(["diff", "--name-only", change_base]) or "").split() + (
        git(["ls-files", "--others", "--exclude-standard"]) or ""
    ).split()
head_subj = (git(["log", "-1", "--format=%s", "HEAD"]) or "").strip()
# Commit subject tags identifying the task's own migration commit at HEAD.
MIGRATION_SUBJECT_PREFIXES = ("[bazel_migration]", "[bazel2gn]", "[bazel]")
head_sha = (git(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]) or "").strip()
base_sha = (git(["rev-parse", "--verify", "--quiet", f"{change_base}^{{commit}}"]) or "").strip()
parent_sha = (git(["rev-parse", "--verify", "--quiet", "HEAD~1^{commit}"]) or "").strip()


# Check whether `anc` is a git ancestor of `desc` in the checkout.
def _is_git_ancestor(anc, desc):
    if not anc or not desc:
        return False
    return (
        subprocess.run(
            ["git", "-C", workdir, "merge-base", "--is-ancestor", anc, desc],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        ).returncode
        == 0
    )


if head_subj.startswith(MIGRATION_SUBJECT_PREFIXES) and (
    change_base == "HEAD"
    or (head_sha and base_sha == head_sha and not changed)
    or (head_sha and base_sha and not _is_git_ancestor(base_sha, head_sha))
    or (
        head_sha
        and base_sha
        and parent_sha
        and base_sha != parent_sha
        and _is_git_ancestor(base_sha, parent_sha)
    )
):
    head_files = (git(["diff", "--name-only", "HEAD~1", "HEAD"]) or "").split()
    if not target_dir or any(p.startswith(target_dir + "/") for p in head_files):
        change_base = "HEAD~1"
        changed = head_files + (
            git(["ls-files", "--others", "--exclude-standard"]) or ""
        ).split()
dirs = {target_dir} if target_dir else set()
for p in changed:
    if os.path.basename(p) in ("BUILD.gn", "BUILD.bazel") and os.path.dirname(p):
        dirs.add(os.path.dirname(p).strip("/"))


def ast_str_list(node):
    if isinstance(node, (ast.List, ast.Tuple)):
        return [
            el.value for el in node.elts
            if isinstance(el, ast.Constant) and isinstance(el.value, str)
        ]
    return []


BAZEL_TEST_RULES = (
    "rustc_test",
    "rust_test",
    "host_rustc_test",
    "wrap_host_rust_test",
    "fx_test",
    "fx_unittest_package",
    "fx_unittest_component",
    "fx_test_component",
    "cc_test",
    "fx_cc_test_binary",
    "host_cc_test",
    "host_py_test",
    "host_go_test",
    "host_test",
    "go_test",
)


# Match a GN target to its BUILD.bazel target by name, GN output_name, or identical source list.
def match_bazel_target_for_gn(name, gn_top, before_targets, bazel_targets, top_vars=None):
    bt = bazel_targets.get(name)
    if bt:
        return name, bt
    for out_n in gn_top.get("output_name", []):
        if out_n in bazel_targets:
            return out_n, bazel_targets[out_n]
    gn_srcs = set(gn_top.get("sources", []))
    if gn_srcs:
        for cand_n, cand_bt in sorted(bazel_targets.items()):
            if cand_n in before_targets and before_targets[cand_n][0] != "group":
                continue
            cand_srcs = set(
                eval_starlark(cand_bt[-1].get("srcs"), top_vars or {})
                if top_vars is not None
                else ast_str_list(cand_bt[-1].get("srcs"))
            )
            if cand_srcs and cand_srcs == gn_srcs:
                return cand_n, cand_bt
    return None, None


# Verify that Rust host test `test_args`/`args` do not use `"env RUST_BACKTRACE=0"` (which
# GN's `rustc_test.gni` overrides by appending `"env RUST_BACKTRACE=1"` after `args`), and
# that `args = [...]` on base GN targets (or `with_unit_tests` targets) are preserved on
# the migrated test target.
def check_test_args_parity(
    pkg, gn_rel, bazel_rel, before_targets, new_targets, bazel_targets, top_vars
):
    out = []
    rust_test_rules = ("host_rustc_test", "wrap_host_rust_test", "rustc_test", "rust_test")
    all_bazel_test_args = set()
    for bname, bt_val in sorted(bazel_targets.items()):
        brule, bline, bkws = bt_val[0], bt_val[1], bt_val[-1]
        if brule not in BAZEL_TEST_RULES:
            continue
        b_args = (
            eval_starlark(bkws.get("test_args"), top_vars)
            + eval_starlark(bkws.get("args"), top_vars)
        )
        all_bazel_test_args.update(b_args)
        if brule in rust_test_rules and "env RUST_BACKTRACE=0" in b_args:
            _emit(
                out,
                "invalid_rust_backtrace_test_arg",
                "error",
                bazel_rel,
                bline,
                (
                    f'`{brule}("{bname}")` in //{pkg} sets `"env RUST_BACKTRACE=0"` in '
                    "`test_args`/`args`. When synced to GN `rustc_test`, `rustc_test.gni` "
                    'appends `"env RUST_BACKTRACE=1"` after `args`, overriding '
                    '`RUST_BACKTRACE=0` and causing tests that assert on exact '
                    "`anyhow::Error` output to fail on CQ."
                ),
                (
                    f'Replace `"env RUST_BACKTRACE=0"` with `"env RUST_LIB_BACKTRACE=0"` '
                    f'in `test_args` on `{brule}("{bname}")` in {bazel_rel} and re-run '
                    f"`fx bazel2gn -d {pkg}`."
                ),
            )

    all_new_gn_test_args = set()
    for nname, (ntmpl, _, nbody) in new_targets.items():
        ntop = parse_gn_top_attrs(nbody)
        all_new_gn_test_args.update(ntop.get("args", []))

    for name, (tmpl, line, body) in sorted(before_targets.items()):
        if tmpl in ("action", "action_foreach", "compiled_action"):
            continue
        if tmpl not in ("rustc_test", "host_test", "test") and not WITH_UNIT_TESTS.search(body):
            continue
        gn_top = parse_gn_top_attrs(body)
        old_args = gn_top.get("args", [])
        if not old_args:
            continue
        missing_bz = [a for a in old_args if a not in all_bazel_test_args]
        missing_gn = (
            [a for a in old_args if a not in all_new_gn_test_args]
            if new_targets
            else []
        )
        if missing_bz or missing_gn:
            shown = ", ".join(f'"{a}"' for a in old_args)
            _emit(
                out,
                "test_args_dropped_or_modified",
                "error",
                bazel_rel,
                1,
                (
                    f'`{tmpl}("{name}")` in //{pkg} had `args = [{shown}]` at the '
                    "change base, but the migrated test target in BUILD.bazel/BUILD.gn "
                    "dropped or modified those test arguments."
                ),
                (
                    f"Preserve `test_args = [{shown}]` on the migrated test target in "
                    f"{bazel_rel} (and `args = [{shown}]` on the GN test target in "
                    f"{gn_rel} via `fx bazel2gn -d {pkg}`)."
                ),
            )
    return out


HOST_BIN_GN_TEMPLATES = {
    "rustc_binary",
    "go_binary",
    "executable",
    "cc_binary",
    "ffx_tool",
    "install_host_tools",
    "sdk_executable_host_tool",
    "sdk_go_binary_host_tool",
    "sdk_rustc_binary_host_tool",
}
BAZEL_HOST_TOOL_BINARY_RULES = {
    "rustc_binary_host_tool",
    "cc_binary_host_tool",
    "go_binary_host_tool",
    "py_binary_host_tool",
    "idk_rustc_binary_host_tool",
    "idk_cc_binary_host_tool",
    "idk_go_binary_host_tool",
    "ffx_tool",
    "idk_ffx_tool",
}
BAZEL_PLAIN_BINARY_RULES = {
    "rustc_binary",
    "rust_binary",
    "go_binary",
    "cc_binary",
    "fx_cc_binary",
    "py_binary",
}
HOST_TOOLCHAIN_GN_CONDS = {
    "is_host",
    "!is_host",
    "is_fuchsia",
    "!is_fuchsia",
    "current_toolchain == host_toolchain",
    "current_toolchain != host_toolchain",
}


# Parse top-level variables and rule target calls from a package's BUILD.bazel file.
def parse_bazel_package_targets(bazel_path, bazel_rel):
    if not os.path.isfile(bazel_path):
        return None, [], {}, {}, False
    try:
        bazel_src = open(bazel_path, encoding="utf-8").read()
        bazel_lines = bazel_src.splitlines()
        bazel_tree = ast.parse(bazel_src, filename=bazel_rel)
    except Exception:
        return None, [], {}, {}, False
    top_vars = {}
    bazel_targets = {}
    has_bazel_test_rule = False
    for stmt in bazel_tree.body:
        if isinstance(stmt, ast.Assign):
            for tgt in stmt.targets:
                if isinstance(tgt, ast.Name):
                    top_vars[tgt.id] = eval_starlark(stmt.value, top_vars)
        elif isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call):
            call = stmt.value
            rule = call.func.id if isinstance(call.func, ast.Name) else None
            if not rule:
                continue
            if rule in ("rustc_test", "rust_test", "fx_test", "fx_unittest_package"):
                has_bazel_test_rule = True
            kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
            tname_node = kws.get("name")
            if isinstance(tname_node, ast.Constant) and isinstance(tname_node.value, str):
                bazel_targets[tname_node.value] = (
                    rule,
                    call.lineno,
                    getattr(call, "end_lineno", call.lineno),
                    kws,
                )
    return bazel_tree, bazel_lines, top_vars, bazel_targets, has_bazel_test_rule


# Check retained GN targets above BAZEL2GN SENTINEL for newly added `if (...)` conditions,
# allowing `if (is_host)` / `else` toolchain guards required by `bazel_host_tool()` or
# `bazel_ffx_tool()` (whose underlying `bazel_host_tool` template in
# //build/bazel/bazel_host_tool.gni asserts `is_host`) and companion forwarding groups.
def check_gn_conditions_with_host_tools(gn_rel, pkg, before, after):
    if not before or not after:
        return []
    above_after = after.split("## BAZEL2GN SENTINEL", 1)[0]
    above_code = re.sub(r"#[^\n]*", "", above_after)
    before_code = re.sub(r"#[^\n]*", "", before)
    bht_pat = r'\b(?:bazel_host_tool|bazel_ffx_tool)\(\s*"([^"$]+)"\s*\)'
    bht_names_after = set(re.findall(bht_pat, above_code))
    if not bht_names_after:
        return check_unrequested_gn_conditions(gn_rel, pkg, before, after)
    bht_names_before = set(re.findall(bht_pat, before_code))
    new_bht_names = bht_names_after - bht_names_before
    after_above_targets = targets(above_after)
    old_cond_map = parse_gn_target_conditions(before)
    new_cond_map = parse_gn_target_conditions(above_after)
    grouped = {}
    for tname, new_info in sorted(new_cond_map.items()):
        old_info = old_cond_map.get(tname)
        if not old_info:
            continue
        t_body = after_above_targets.get(tname, ("", 0, ""))[2]
        refs_new_bht = new_info["tmpl"] in (
            "group",
            "sdk_atom",
            "host_test_data",
        ) and any(
            re.search(
                r'":' + re.escape(bn) + r'(?:_[A-Za-z0-9_-]+)?(?:"|\()',
                t_body,
            )
            for bn in new_bht_names
        )
        is_migrated_host_tool = (
            tname in new_bht_names
            or refs_new_bht
            or (
                tname not in bht_names_before
                and new_info["tmpl"] == "group"
                and old_info["tmpl"] in HOST_BIN_GN_TEMPLATES
            )
        )
        old_conds = _expand_platform_conds(
            old_info["enclosing"] + old_info["inner"]
        )
        for cond, cond_line in new_info["enclosing"] + new_info["inner"]:
            if cond in old_conds or _normalize_platform_cond(cond) in old_conds:
                continue
            if is_bazel_test_suite_inner_guard(
                pkg, new_info, cond, cond_line, above_after
            ):
                continue
            if is_migrated_host_tool and cond in HOST_TOOLCHAIN_GN_CONDS:
                continue
            grouped.setdefault((cond, cond_line), []).append(
                f'`{new_info["tmpl"]}("{tname}")`'
            )
    out_findings = []
    for (cond, cond_line), target_descs in sorted(
        grouped.items(), key=lambda x: x[0][1]
    ):
        joined = ", ".join(target_descs)
        _emit(
            out_findings,
            "unrequested_gn_condition",
            "error",
            gn_rel,
            cond_line,
            (
                f"Retained GN target(s) {joined} in //{pkg} are newly gated by "
                f"`if ({cond})` on line {cond_line} of {gn_rel}, which was not "
                "present at the change base. Adding unrequested platform or "
                "toolchain conditions to retained GN targets or `group(\"tests\")` "
                "alters GN target availability across toolchains."
            ),
            (
                f"Remove the newly added `if ({cond})` wrapper on line {cond_line} "
                f"of {gn_rel} so {joined} keeps its original unconditional "
                "definition from the change base."
            ),
        )
    return out_findings


# Check that migrated `bazel_ffx_tool` / `bazel_host_tool` targets in BUILD.gn do not carry over
# redundant `testonly = false` from `ffx_tool()` (which only needed `testonly = false` in GN to
# override the `testonly = true` default on its internal `host_test_data` sub-target).
def check_redundant_gn_host_tool_testonly_false(gn_rel, before, after):
    if not after:
        return []
    above_after = after.split("## BAZEL2GN SENTINEL", 1)[0]
    old_targets = targets(before) if before else {}
    out_findings = []
    for m in GN_TARGET.finditer(above_after):
        tmpl, tname = m.group(1), m.group(2)
        if tmpl not in ("bazel_ffx_tool", "bazel_host_tool"):
            continue
        depth, i = 1, m.end()
        while i < len(above_after) and depth:
            depth += {"{": 1, "}": -1}.get(above_after[i], 0)
            i += 1
        body = above_after[m.end() : i - 1]
        old_t = old_targets.get(tname)
        if old_t and old_t[0] == tmpl:
            old_clean = re.sub(r"#[^\n]*", "", old_t[2])
            if re.search(r"(?<![\w.])testonly\s*=\s*false\b", old_clean):
                continue
        body_start_line = above_after.count("\n", 0, m.end()) + 1
        for offset_idx, raw_ln in enumerate(body.splitlines()):
            code_ln = re.sub(r"#.*$", "", raw_ln)
            if re.search(r"(?<![\w.])testonly\s*=\s*false\b", code_ln):
                ln_no = body_start_line + offset_idx
                _emit(
                    out_findings,
                    "redundant_gn_testonly_false",
                    "error",
                    gn_rel,
                    ln_no,
                    (
                        f'`{tmpl}("{tname}")` in {gn_rel} sets `testonly = false` on line '
                        f"{ln_no}. Unlike `ffx_tool()` (which only needed `testonly = false` "
                        "to override the `testonly = true` default on its generated "
                        f"`host_test_data` sub-target), `{tmpl}()` does not produce a "
                        "`host_test_data` target and already defaults to `testonly = false` "
                        "in GN."
                    ),
                    (
                        f"Remove `testonly = false` (and any comment explaining it) from "
                        f'`{tmpl}("{tname}")` in {gn_rel}. Keep `testonly = false` only on '
                        "an explicit `host_test_data(...)` target if one is defined in "
                        f"{gn_rel}."
                    ),
                )
    return out_findings


# Extract `exclude_toolchain_tags` values carried on a BUILD.bazel target via
# `tags = ["exclude_toolchain_tags=<tag>", ...]`, `disable_instrumentation = True`,
# or `# @bazel2gn:raw_attribute:exclude_toolchain_tags` comments, plus whether the
# target has `# @bazel2gn:skip`.
def bazel_target_exclude_toolchain_tags(bt, top_vars, bazel_lines=None):
    if not bt:
        return set(), False
    _, bt_line, bt_end_line, bt_kws = bt
    found = set()
    for t in eval_starlark(bt_kws.get("tags"), top_vars):
        if t.startswith("exclude_toolchain_tags="):
            val = t.split("=", 1)[1].strip()
            if val:
                found.add(val)
    dis_inst = bt_kws.get("disable_instrumentation")
    if isinstance(dis_inst, ast.Constant) and dis_inst.value is True:
        found.add("instrumented")
    has_skip = False
    if bazel_lines and bt_line > 0:
        start_idx = max(0, bt_line - 1)
        while start_idx > 0 and bazel_lines[start_idx - 1].strip().startswith("#"):
            start_idx -= 1
        span_txt = "\n".join(bazel_lines[start_idx : max(bt_line, bt_end_line)])
        if re.search(r"#\s*@bazel2gn:skip\b", span_txt):
            has_skip = True
        for rm in re.finditer(
            r"#\s*@bazel2gn:raw_attribute:\s*exclude_toolchain_tags\s*\+?=\s*\[([^\]]*)\]",
            span_txt,
        ):
            found.update(re.findall(r'"([^"]+)"', rm.group(1)))
    return found, has_skip


# Check whether a base GN target's `exclude_toolchain_tags` is satisfied by migrating
# a host binary or ffx subtool to a Bazel `*_binary_host_tool` / `ffx_tool` / `idk_ffx_tool`
# or `bazel_host_tool` / `bazel_ffx_tool`-wrapped binary rule (which builds only in
# `toolchain_variant.base` with `bazel_config = "host"` and copies the uninstrumented
# binary into variant toolchains), or by carrying all missing tags on the BUILD.bazel
# target via `exclude_toolchain_tags=<tag>` / `disable_instrumentation` /
# `@bazel2gn:raw_attribute` when the target is not generated as a convertible rule below
# the BAZEL2GN SENTINEL (e.g. `# @bazel2gn:skip`, `bazel_host_tool`, or full removal).
def is_host_tool_exclude_tags_exempt(
    name,
    tmpl,
    body,
    cur,
    after,
    new_targets,
    bt_rule,
    missing_tags,
    matched_bt=None,
    top_vars=None,
    bazel_lines=None,
):
    bz_tags, has_bz_skip = bazel_target_exclude_toolchain_tags(
        matched_bt, top_vars or {}, bazel_lines
    )
    non_generated_gn = (
        after is None
        or cur is None
        or cur[0]
        in (
            "bazel_host_tool",
            "bazel_ffx_tool",
            "group",
            "bazel_test_suite",
            "bazel_fuchsia_package",
        )
        or has_bz_skip
    )
    carried_in_bazel = bool(matched_bt) and not (missing_tags - bz_tags) and non_generated_gn
    after_code = re.sub(r"#[^\n]*", "", after) if after is not None else ""
    has_gn_bht = bool(
        after_code
        and re.search(r'\b(?:bazel_host_tool|bazel_ffx_tool)\(\s*"[^"$]+"\s*\)', after_code)
    )
    is_host_bin_migration = tmpl in HOST_BIN_GN_TEMPLATES and (
        bt_rule in BAZEL_HOST_TOOL_BINARY_RULES
        or (bt_rule in BAZEL_PLAIN_BINARY_RULES and has_gn_bht)
    )
    if not carried_in_bazel and not is_host_bin_migration:
        return False
    if after is not None:
        if not carried_in_bazel:
            if cur is not None and cur[0] not in ("bazel_host_tool", "bazel_ffx_tool", "group"):
                return False
            if cur is not None and not has_gn_bht:
                return False
        if WITH_UNIT_TESTS.search(body):
            gn_tests = [
                (t_name, t_body)
                for t_name, (t_tmpl, _, t_body) in new_targets.items()
                if t_name in (f"{name}_test", f"{name}_bin_test")
                or t_tmpl == "rustc_test"
            ]
            if gn_tests and not any(
                not (
                    missing_tags
                    - gn_only_attrs(t_body, after).get("exclude_toolchain_tags", set())
                )
                for _, t_body in gn_tests
            ):
                return False
    return True


findings = []
for pkg in sorted(d for d in dirs if d):
    findings.extend(check_undeclared_cc_local_headers(pkg))
    gn_rel = f"{pkg}/BUILD.gn"
    before = git(["show", f"{change_base}:{gn_rel}"])
    now_path = os.path.join(workdir, gn_rel)
    after = open(now_path, encoding="utf-8").read() if os.path.isfile(now_path) else None
    if head_subj.startswith(MIGRATION_SUBJECT_PREFIXES):
        prev_gn = git(["show", f"HEAD~1:{gn_rel}"])
        if prev_gn and prev_gn != after and (
            not before
            or before == after
            or (
                "## BAZEL2GN SENTINEL" in before
                and "## BAZEL2GN SENTINEL" not in prev_gn
            )
        ):
            before = prev_gn
    if not before:
        continue
    new_targets = targets(after)
    before_targets = targets(before)
    before_top_lists = parse_gn_top_list_vars(before)
    bazel_rel = f"{pkg}/BUILD.bazel"
    bazel_path = os.path.join(workdir, bazel_rel)
    bazel_tree, bazel_lines, top_vars, bazel_targets, has_bazel_test_rule = (
        parse_bazel_package_targets(bazel_path, bazel_rel)
    )
    findings.extend(check_gn_conditions_with_host_tools(gn_rel, pkg, before, after))
    findings.extend(check_redundant_gn_host_tool_testonly_false(gn_rel, before, after))
    for name, (tmpl, line, body) in sorted(before_targets.items()):
        if tmpl == "config":
            continue
        old_attrs = gn_only_attrs(body, before)
        if not old_attrs:
            continue
        cur = new_targets.get(name)
        new_attrs = gn_only_attrs(cur[2], after) if cur else {}
        gn_top_for_match = (
            parse_gn_top_attrs(body, before_top_lists)
            if before_top_lists
            else parse_gn_top_attrs(body)
        )
        matched_bt_name, matched_bt = match_bazel_target_for_gn(
            name, gn_top_for_match, before_targets, bazel_targets, top_vars
        )
        matched_bt_rule = matched_bt[0] if matched_bt else None
        if (
            cur is None
            and matched_bt_name
            and matched_bt_name in new_targets
            and matched_bt_name in before_targets
            and before_targets[matched_bt_name][0] == "group"
            and re.search(
                r'"(?::|//' + re.escape(pkg) + r":)" + re.escape(name) + r'(?:\([^"]*\))?"',
                before_targets[matched_bt_name][2],
            )
        ):
            cur = new_targets[matched_bt_name]
            new_attrs = gn_only_attrs(cur[2], after)
        lost = []
        for attr, vals in sorted(old_attrs.items()):
            missing = vals - new_attrs.get(attr, set())
            if missing and attr == "exclude_toolchain_tags" and tmpl in NO_EXCLUDE_TEMPLATES:
                continue
            if missing and attr == "exclude_toolchain_tags" and tmpl in TEST_ONLY_EXCLUDE_TEMPLATES:
                if not WITH_UNIT_TESTS.search(body):
                    continue
                if any(
                    not (missing - gn_only_attrs(t_body, after).get(attr, set()))
                    for t_name, (t_tmpl, _, t_body) in new_targets.items()
                    if t_name == f"{name}_test" or t_tmpl == "rustc_test"
                ):
                    continue
            if (
                missing
                and attr == "exclude_toolchain_tags"
                and is_host_tool_exclude_tags_exempt(
                    name,
                    tmpl,
                    body,
                    cur,
                    after,
                    new_targets,
                    matched_bt_rule,
                    missing,
                    matched_bt,
                    top_vars,
                    bazel_lines,
                )
            ):
                continue
            if missing:
                shown = ", ".join(f'"{v}"' for v in sorted(missing))
                lost.append(
                    f"`{attr}` ({shown})" if attr in LIST_ATTRS else f"`{attr} = {sorted(missing)[0]}`"
                )
        if not lost:
            continue
        where = (
            f"is now `{cur[0]}(\"{name}\")` without them"
            if cur
            else ("was removed from BUILD.gn" if after is not None else "lost them when BUILD.gn was deleted")
        )
        _emit(
            findings,
            "gn_only_attr_dropped",
            "error",
            gn_rel if after is not None else f"{pkg}/BUILD.bazel",
            cur[1] if cur else 1,
            (
                f'`{tmpl}("{name}")` in //{pkg} had GN-only attribute(s) {", ".join(lost)} at the change base '
                f"and {where}. No plain BUILD.bazel attribute carries them, so unless BUILD.bazel names them "
                "explicitly the generated GN target is built differently (e.g. linked with coverage/profile "
                "runtimes it cannot satisfy, its unit tests run in sanitizer builders they are excluded from, or "
                "its assert_no_deps restricts a different set of toolchains)."
            ),
            (
                f'Preferred: keep `{name}` generated by bazel2gn and name the attributes on its BUILD.bazel target, '
                "then re-run `fx bazel2gn`. Use the dedicated forms the checkout's build/tools/bazel2gn/README.md "
                'documents: `tags = ["exclude_toolchain_tags=heapdump"]` (never '
                "`# @bazel2gn:raw_attribute:exclude_toolchain_tags`, which lint_config_parity rejects), "
                "`assert_no_deps=<label>` tags (with `# @bazel2gn:path_overwrite:<gn_label>($current_toolchain)` "
                "for toolchain-qualified labels), and on `fx_cc_*` targets `disable_instrumentation = True` "
                "(`exclude_toolchain_tags = [ \"instrumented\" ]`) / `disable_build_flags` (`configs -=`). "
                "Only for an attribute with no dedicated form, stack `# @bazel2gn:raw_attribute:<GN line>` "
                "comments right above the target (e.g. `# @bazel2gn:raw_attribute:configs -= [ ... ]`). For host tools "
                "and ffx subtools migrated to `*_binary_host_tool` / `ffx_tool` / `idk_ffx_tool` in BUILD.bazel and "
                "`bazel_host_tool` / `bazel_ffx_tool` (inside `if (is_host)`) in BUILD.gn, `exclude_toolchain_tags` "
                "on the host binary is satisfied automatically by building in `toolchain_variant.base`. See "
                "\"GN-only attributes on generated "
                "targets\" in the coder prompt. Alternative when even that cannot express "
                f'it: keep `{tmpl}("{name}")` hand-written above the BAZEL2GN SENTINEL of {gn_rel} with the original '
                "attributes and a comment saying why; mark the BUILD.bazel target `# @bazel2gn:skip` (it stays in "
                "Bazel, migration_sanity accepts the skip for such targets) with a comment naming why bazel2gn cannot "
                "generate it (e.g. `# bazel2gn cannot express <attribute>; hand-written above the GN sentinel.`, not "
                "\"kept/remains in GN\", which stale_gn_retention_comment_in_bazel rejects). If the target has `with_unit_tests`/"
                "`test_deps` or is a test binary, keep its GN test package in GN too (the Bazel fx_test would lose "
                "the toolchain-variant exclusion), and name the kept attributes in the CoderReport summary. Do not "
                "approximate them with copts/linkopts alone. For rustc_library/rustc_staticlib, exclude_toolchain_tags "
                "only reaches the with_unit_tests test: the library may be generated by bazel2gn if a hand-written GN "
                f"`{name}_test` above the sentinel keeps the same exclude_toolchain_tags."
            ),
        )

    if os.path.isfile(bazel_path):
        if bazel_tree is not None:
            after_top_lists = parse_gn_top_list_vars(after) if after else {}
            pkg_def_testonly = bazel_has_default_testonly(bazel_tree)
            before_bazel_src = git(["show", f"{change_base}:{bazel_rel}"])
            if (
                before_bazel_src is not None
                and os.path.isfile(bazel_path)
                and head_subj.startswith(MIGRATION_SUBJECT_PREFIXES)
            ):
                try:
                    cur_bz_src = open(bazel_path, encoding="utf-8").read()
                except OSError:
                    cur_bz_src = None
                if before_bazel_src == cur_bz_src:
                    prev_bz = git(["show", f"HEAD~1:{bazel_rel}"])
                    if prev_bz is not None and prev_bz != cur_bz_src:
                        before_bazel_src = prev_bz
            before_bazel_targets = {}
            before_pkg_def_testonly = False
            if before_bazel_src:
                try:
                    before_bz_tree = ast.parse(before_bazel_src, filename=bazel_rel)
                    before_pkg_def_testonly = bazel_has_default_testonly(before_bz_tree)
                    for stmt in before_bz_tree.body:
                        if isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call):
                            call = stmt.value
                            b_rule = call.func.id if isinstance(call.func, ast.Name) else None
                            if not b_rule:
                                continue
                            b_kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
                            tn_node = b_kws.get("name")
                            if isinstance(tn_node, ast.Constant) and isinstance(tn_node.value, str):
                                before_bazel_targets[tn_node.value] = (
                                    b_rule,
                                    call.lineno,
                                    getattr(call, "end_lineno", call.lineno),
                                    b_kws,
                                )
                except Exception:
                    pass
            findings.extend(
                check_test_args_parity(
                    pkg, gn_rel, bazel_rel, before_targets, new_targets, bazel_targets, top_vars
                )
            )
            for name, (tmpl, gn_line, body) in sorted(before_targets.items()):
                gn_top = parse_gn_top_attrs(body)
                gn_top_expanded = (
                    parse_gn_top_attrs(body, before_top_lists)
                    if before_top_lists
                    else gn_top
                )
                bt_name, bt = match_bazel_target_for_gn(
                    name, gn_top_expanded, before_targets, bazel_targets, top_vars
                )
                if not bt:
                    continue
                bt_rule, bt_line, bt_end_line, bt_kws = bt
                cur = new_targets.get(name) or new_targets.get(bt_name)
                cur_top = parse_gn_top_attrs(cur[2], after_top_lists) if cur else None
                dropped_name = check_custom_output_or_crate_name_dropped(
                    pkg, gn_rel, bazel_rel, name, tmpl, bt_rule, bt_line, gn_top, cur, cur_top
                )
                if dropped_name:
                    findings.append(dropped_name)
                dropped_testonly = check_testonly_dropped_from_gn(
                    pkg,
                    gn_rel,
                    bazel_rel,
                    body,
                    bt_name,
                    bt_rule,
                    bt_line,
                    bt_kws,
                    pkg_def_testonly,
                    cur=cur,
                    before_bt=before_bazel_targets.get(bt_name),
                    before_pkg_default_testonly=before_pkg_def_testonly,
                    tmpl=tmpl,
                )
                if dropped_testonly:
                    findings.append(dropped_testonly)

# Check for GN root_out_dir binary output_name collisions on migrated rustc_test targets
def check_rust_test_output_name_collision():
    for t_dir in sorted(d for d in dirs if d):
        gn_rel = f"{t_dir}/BUILD.gn"
        bz_rel = f"{t_dir}/BUILD.bazel"
        gn_abs = os.path.join(workdir, gn_rel)
        bz_abs = os.path.join(workdir, bz_rel)
        if not os.path.isfile(gn_abs) or not os.path.isfile(bz_abs):
            continue
        try:
            with open(gn_abs, "r", encoding="utf-8") as f:
                gn_content = f.read()
            with open(bz_abs, "r", encoding="utf-8") as f:
                bz_content = f.read()
        except OSError:
            continue

        sent_split = re.split(r"^##\s*BAZEL2GN SENTINEL[^\n]*\n", gn_content, maxsplit=1, flags=re.M)
        if len(sent_split) < 2:
            continue
        generated_gn = sent_split[1]
        gen_targets = targets(generated_gn)

        for t_name, (tmpl, _, body) in sorted(gen_targets.items()):
            if tmpl != "rustc_test":
                continue
            out_m = re.search(r'\b(?:output_name|name)\s*=\s*"([^"]+)"', body)
            out_name = out_m.group(1) if out_m else t_name.replace("-", "_")

            # Search other BUILD.gn files for targets producing the same root_out_dir binary
            try:
                res = subprocess.run(
                    [
                        "git",
                        "-C",
                        workdir,
                        "grep",
                        "-l",
                        f'"{out_name}"',
                        "--",
                        ":(glob)**/BUILD.gn",
                    ],
                    capture_output=True,
                    text=True,
                    timeout=15,
                )
                candidate_files = [
                    p.strip()
                    for p in res.stdout.splitlines()
                    if p.strip() and p.strip() != gn_rel
                ]
            except Exception:
                candidate_files = []

            colliding_file = None
            target_pat = re.compile(
                r'\b(?:test|executable|cc_test_executable|rustc_test|rustc_binary)\("'
                + re.escape(out_name)
                + r'"\)\s*\{([^}]*)\}'
            )
            for c_file in candidate_files:
                try:
                    with open(os.path.join(workdir, c_file), "r", encoding="utf-8") as cf:
                        c_text = cf.read()
                except OSError:
                    continue
                for cm in target_pat.finditer(c_text):
                    c_body = cm.group(1)
                    c_out = re.search(r'\b(?:output_name|name)\s*=\s*"([^"]+)"', c_body)
                    if not c_out or c_out.group(1) == out_name:
                        colliding_file = c_file
                        break
                if colliding_file:
                    break

            stem = t_name[:-5] if t_name.endswith("_test") else t_name
            lib_crate = None
            if not out_m and stem in gen_targets and gen_targets[stem][0] == "rustc_library":
                cm = re.search(r'\b(?:crate_name|name)\s*=\s*"([^"]+)"', gen_targets[stem][2])
                if cm and cm.group(1).replace("-", "_") != stem.replace("-", "_"):
                    lib_crate = cm.group(1).replace("-", "_")
            if not colliding_file and lib_crate:
                try:
                    lres = subprocess.run(
                        [
                            "git",
                            "-C",
                            workdir,
                            "grep",
                            "-l",
                            f'rustc_library("{stem}")',
                            "--",
                            ":(glob)**/BUILD.gn",
                        ],
                        capture_output=True,
                        text=True,
                        timeout=15,
                    )
                    for p in lres.stdout.splitlines():
                        if p.strip() and p.strip() != gn_rel:
                            colliding_file = p.strip()
                            break
                except Exception:
                    pass

            if colliding_file:
                suggested_out = (
                    f"{lib_crate}_test" if lib_crate else f"{stem.replace('-', '_')}_lib_test"
                )
                line_no = 1
                bz_m = re.search(
                    r'name\s*=\s*"' + re.escape(t_name) + r'"', bz_content
                )
                if bz_m:
                    line_no = bz_content[: bz_m.start()].count("\n") + 1
                msg = (
                    f"Migrated rustc_test(\"{t_name}\") emits GN binary '{out_name}' "
                    f"in root_out_dir, which collides with '{colliding_file}' "
                    f"(GN ERROR: Duplicate output file: {out_name})."
                )
                rem = (
                    f"Set `crate_name = \"{suggested_out}\"` on `rustc_test(name = \"{t_name}\")` "
                    f"in '{bz_rel}' and re-run `fx bazel2gn -d {t_dir}` so the generated GN "
                    f"target emits `output_name = \"{suggested_out}\"`."
                )
                _emit(
                    findings,
                    "duplicate_gn_output_name",
                    "ERROR",
                    bz_rel,
                    line_no,
                    msg,
                    rem,
                )


check_rust_test_output_name_collision()


# Load review_diff findings from PLANTER_REVIEW_DIFF_JSON or the shared `planter diff`
# fallback, pinning the resolved change base for direct (non-run_checks.sh) runs.
def _load_review_diff_findings_with_fallback():
    if pcl is None or os.environ.get("PLANTER_SKIP_DIFF_COVERAGE") == "1":
        return []
    os.environ["PLANTER_CHANGE_BASE"] = change_base
    os.environ["PLANTER_RESOLVED_CHANGE_BASE"] = change_base
    return pcl.read_review_diff_findings(workdir)


# Emit INFO waiver entries for false-positive review_diff alignment warnings that no
# other verifier covers: GN `ifs_shared_library` mapped to `cc_link_stub_from_ifs_file`
# and default-named cutover `:<name>-lib` library deps. Pre-existing unchanged targets,
# companion-manifest attrs, @bazel2gn:raw_attribute/raw_overwrite attrs and deps,
# fx_packaged_binary wrapper deps, cross-language Rust metadata deps and obsolete
# "Retained in GN" comments are verified (more strictly) by migration_sanity
# waive_verified_review_diff_findings and tools/run_checks.sh, so they are not
# re-waived here.
def waive_false_positive_review_diff_warnings():
    rd_findings = _load_review_diff_findings_with_fallback()
    for rf in rd_findings:
        cat = rf.get("category", "")
        rfile = rf.get("file", "")
        rline = rf.get("line", 1)
        rmsg = rf.get("message", "")
        rrem = rf.get("remediation", "")
        pkg_dir = os.path.dirname(rfile).strip("/") if rfile else ""
        bz_abs = os.path.join(workdir, pkg_dir, "BUILD.bazel") if pkg_dir else ""
        bz_txt = ""
        if bz_abs and os.path.isfile(bz_abs):
            try:
                bz_txt = open(bz_abs, encoding="utf-8").read()
            except OSError:
                bz_txt = ""
        waiver_reason = None
        m_tgt = re.match(r'^Target "([^"]+)"', rmsg)
        tname = m_tgt.group(1) if m_tgt else ""
        if cat == "attribute_alignment" and tname:
            if "abi in GN not carried over to Bazel" in rmsg and (
                "cc_link_stub_from_ifs_file" in bz_txt and "ifs_file" in bz_txt
            ):
                waiver_reason = (
                    "In Bazel, `cc_link_stub_from_ifs_file` declares the `.ifs` ABI file "
                    "via `ifs_file` rather than GN `ifs_shared_library`'s `abi` attribute."
                )
            elif (
                "Unusual rule mapping from ifs_shared_library to cc_link_stub_from_ifs_file"
                in rmsg
            ):
                waiver_reason = (
                    "`cc_link_stub_from_ifs_file` is the canonical Bazel equivalent of "
                    "GN `ifs_shared_library`."
                )
        elif cat == "dep_alignment" and tname and "unmatched in GN:" in rmsg:
            m_unm = re.search(r"unmatched in GN:\s*\[([^\]]+)\]", rmsg)
            unm_items = (
                [x.strip() for x in m_unm.group(1).split(",") if x.strip()]
                if m_unm
                else []
            )
            if unm_items and all(
                u.startswith("//")
                and ":" in u
                and u.split(":", 1)[1].endswith(("-lib", "_lib"))
                and f'"{u.split(":", 1)[0]}"' in bz_txt
                for u in unm_items
            ):
                waiver_reason = (
                    "The GN `:<name>-lib` target corresponds to the default package "
                    "target `//<pkg>` in `BUILD.bazel` after `BUILD.gn` deletion."
                )
        if waiver_reason:
            findings.append({
                "source": "gn_attr_parity",
                "category": cat,
                "severity": "info",
                "file": rfile,
                "line": rline,
                "message": rmsg,
                "remediation": rrem,
                "waiver": waiver_reason,
            })


waive_false_positive_review_diff_warnings()

deduped_findings = []
seen_keys = set()
for f in findings:
    key = (f["category"], f["file"], f["line"], f["message"])
    if key not in seen_keys:
        seen_keys.add(key)
        deduped_findings.append(f)

print(json.dumps(deduped_findings, indent=2))
PYEOF
