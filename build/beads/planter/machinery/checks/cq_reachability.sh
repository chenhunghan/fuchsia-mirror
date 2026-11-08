#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check verifying that migrated targets and tests in BUILD.bazel /
# BUILD.gn remain reachable in the build & CQ graph:
# 1. Every test target (`*_test`, `fuchsia_unittest_package`, `fuchsia_test_package`)
#    in the migrated package must be wired into the package's `group("tests")`
#    (or referenced by a test package in the same BUILD file that is wired into
#    `tests`).
# 2. The package must be registered in a parent BUILD file so CQ builds it.
# 3. Every Bazel test that Fuchsia test runners can run (`fx_test`, `host_rustc_test`,
#    `host_go_test`, `host_py_test`, `host_cc_test`, `host_test`, `wrap_host_rust_test`, and the
#    `<name>_test` host test of `rustc_*(with_unit_tests = "host"|"both")` or
#    `with_host_unit_tests = True`) must be listed,
#    directly or through a Bazel `test_suite`, in a GN `bazel_test_suite` (`fx_test` in
#    `target_tests`, host tests in `host_tests`) that the GN `group("tests")` of its
#    directory (or the parent BUILD.gn) references. Otherwise it is missing from
#    tests.json and neither `fx test` nor infra runs it. The GN copy bazel2gn generates for a
#    dual-build library's test runs the GN build of the test, not the Bazel one, so the Bazel
#    test needs the export too.
# 4. Every Rust device test binary (`rustc_test`, or the `<name>_test` of
#    `rustc_*(with_unit_tests = "fuchsia"|"both")`) must be packaged by an
#    `fx_unittest_package` or an `fx_packaged_binary` (for an `fx_test`) in the same
#    BUILD.bazel, otherwise nothing runs it (`unpackaged_rust_device_test`).
#    Only runs when the checkout has `build/bazel/rules/testing/fx_test.bzl` (older
#    checkouts keep tests in GN).
# 5. The change must merge cleanly onto `origin/main` without rebase conflicts
#    (`git merge-tree --write-tree`) or leftover conflict markers so Gerrit CQ
#    (`checkout|jiri patch`) does not fail with `Failed to rebase`.
# 6. Every workspace Bazel package referenced by `BUILD.bazel` in the change must
#    exist in `origin/main` or in the top commit `HEAD` (never only in an unmerged
#    local ancestor commit between `merge-base(origin/main, HEAD)` and `HEAD~1`),
#    otherwise Gerrit CQ fails with `no such package '<pkg>': BUILD file not found`.

# 6. Every main-repository `//<pkg>:...` dependency referenced in a migrated
#    `BUILD.bazel` must resolve to an existing `<pkg>/BUILD.bazel` or `<pkg>/BUILD`
#    package in the checkout (and not be missing from the commit ancestry when a
#    stacked change drops its parent dependency commit).

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"
CHANGE_BASE="${PLANTER_RESOLVED_CHANGE_BASE:-${PLANTER_CHANGE_BASE:-HEAD}}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$WORKDIR" "$TARGET_DIRS" "$CHANGE_BASE" "$SCRIPT_DIR" <<'PYEOF'
import ast
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

workdir = os.path.abspath(sys.argv[1])
raw_dirs = sys.argv[2]
change_base = sys.argv[3].strip() or "HEAD"
# Shared Fuchsia-incompatible external repository detection and remediation
# (planter_check_lib.py), also used by migration_sanity and build_verification.
if len(sys.argv) > 4 and sys.argv[4]:
    sys.path.insert(0, os.path.abspath(sys.argv[4]))
import planter_check_lib as pcl
target_dirs = [d.strip().strip("/") for d in re.split(r"[\s,]+", raw_dirs) if d.strip().strip("/")]

TEST_RULES = {
    "rustc_test",
    "cc_test",
    "fx_cc_test_binary",
    "go_test",
    "host_go_test",
    "host_rustc_test",
    "python_host_test",
    "fx_unittest_package",
    "fuchsia_unittest_package",
    "fuchsia_test_package",
}

FETCH_TTL_SECS = (
    60
    if os.environ.get("PLANTER_SKIP_BUILD", "").strip() in ("1", "true")
    else 15
)

findings = []

DEVICE_TEST_RULES = {"fx_test", "fx_unittest_package"}
HOST_TEST_RULES = {
    "host_rustc_test",
    "host_go_test",
    "host_py_test",
    "host_cc_test",
    "host_test",
    "wrap_host_rust_test",
}
# The GN copy bazel2gn generates for a test runs the GN build of the test, so it never stands
# in for exporting the Bazel test: both run while the library is in both build systems.
GN_CONVERTED_HOST_TEST_RULES = set()
RUST_TEST_OWNERS = {"rustc_library", "rustc_binary", "rustc_proc_macro"}
# Checkouts without fx_test keep tests in GN (the old path): skip the export check there.
FX_TEST_AVAILABLE = os.path.isfile(os.path.join(workdir, "build/bazel/rules/testing/fx_test.bzl"))
SENTINEL_RE = re.compile(r"^##\s*BAZEL2GN SENTINEL|#LOCAL_BAZEL_BUILD_SENTINEL", re.M)
SUITE_RE = re.compile(r'\bbazel_test_suite\(\s*"([^"]+)"\s*\)\s*\{')
SUITE_LIST_RE = re.compile(r"\b(host_tests|target_tests)\s*\+?=\s*\[([^\]]*)\]")


def bazel_strings(node):
    if isinstance(node, (ast.List, ast.Tuple)):
        return [e.value for e in node.elts if isinstance(e, ast.Constant) and isinstance(e.value, str)]
    return []


def is_true(node):
    return isinstance(node, ast.Constant) and node.value is True


def unit_test_envs(kws):
    """Environments (subset of {"fuchsia", "host"}) of the unit tests of a rustc_* target."""
    if is_true(kws.get("with_host_unit_tests")):
        return {"host"}
    v = kws.get("with_unit_tests")
    if isinstance(v, ast.Constant):
        return {"fuchsia": {"fuchsia"}, "host": {"host"}, "both": {"fuchsia", "host"}}.get(v.value, set())
    return set()


def split_label(label, pkg):
    """(package, name) of a Bazel label written in package pkg; None for other repositories."""
    label = re.sub(r"^@@?//", "//", label.strip())
    if label.startswith("@"):
        return None
    if label.startswith("//"):
        path, _, name = label[2:].partition(":")
        path = path.strip("/")
        return path, name or os.path.basename(path)
    return pkg, label.lstrip(":")


def local_name(label, pkg):
    parts = split_label(label, pkg)
    return parts[1] if parts and parts[0] == pkg else None


def label_target(label, pkg):
    """Name of the target in pkg that an (absolute) bazel_test_suite label designates."""
    if not re.match(r"^@{0,2}//", label.strip()):
        return None  # Relative labels do not resolve: bazel_test_suite labels must be absolute.
    return local_name(label, pkg)


def gn_code(text):
    """Blanks out GN comments (outside strings) without moving offsets."""
    return re.sub(r'("(?:\\.|[^"\\])*")|#[^\n]*', lambda m: m.group(1) or " " * len(m.group(0)), text)


# Check whether a GN test package/component packages a test binary from pkg_dir.
def _pre_sentinel_packages_test_bin(
    raw_gn, pkg_dir, rel_from_anc, bin_name, allow_post_sentinel=False
):
    if not raw_gn:
        return False
    m_sent = SENTINEL_RE.search(raw_gn)
    scan_txt = raw_gn if allow_post_sentinel else (raw_gn[: m_sent.start()] if m_sent else raw_gn)
    pre_clean = gn_code(scan_txt)
    if not re.search(
        r"\b(?:fuchsia_test_package|fuchsia_unittest_package|"
        r"fuchsia_test_component|fuchsia_unittest_component)\s*\(",
        pre_clean,
    ):
        return False
    prefixes = [f"//{pkg_dir}:"]
    if not rel_from_anc:
        prefixes.append(":")
        if os.path.basename(pkg_dir) == bin_name:
            prefixes.append(f"//{pkg_dir}")
    else:
        prefixes.append(f"{rel_from_anc}:")
        prefixes.append(f"./{rel_from_anc}:")
        if os.path.basename(pkg_dir) == bin_name:
            prefixes.append(rel_from_anc)
            prefixes.append(f"./{rel_from_anc}")
            prefixes.append(f"//{pkg_dir}")
    for pref in prefixes:
        if pref.endswith(":"):
            pat = r'"' + re.escape(pref) + re.escape(bin_name) + r'(?:\([^"]*\))?"'
        else:
            pat = r'"' + re.escape(pref) + r'(?:\([^"]*\))?"'
        if re.search(pat, pre_clean):
            return True
    return False


# Check whether a cross-package BUILD.bazel packages //pkg_dir:bin_name.
def _cross_bazel_packages_test_bin(cross_dir, pkg_dir, bin_name):
    cross_bz = os.path.join(workdir, cross_dir, "BUILD.bazel")
    if not os.path.isfile(cross_bz):
        return False
    try:
        with open(cross_bz, "r", encoding="utf-8") as fh:
            bz_txt = fh.read()
    except OSError:
        return False
    if not re.search(
        r"\b(?:fx_unittest_package|fx_packaged_binary|"
        r"fx_test_component|fx_unittest_component|fx_component)\s*\(",
        bz_txt,
    ):
        return False
    clean_pkg = pkg_dir.strip("/")
    lbl_re = r'"//' + re.escape(clean_pkg) + r":" + re.escape(bin_name) + r'"'
    if bin_name == os.path.basename(clean_pkg):
        lbl_re += r'|"//' + re.escape(clean_pkg) + r'"'
    return bool(re.search(lbl_re, bz_txt))


# Collect cross-package directories (explicit visibility targets, ancestors, and siblings)
# whose BUILD.gn or BUILD.bazel may reference or package a target from pkg_dir.
def _collect_cross_pkg_gn_dirs(pkg_dir):
    clean_pkg = pkg_dir.strip("/")
    cands = []
    bz_path = os.path.join(workdir, clean_pkg, "BUILD.bazel")
    if os.path.isfile(bz_path):
        try:
            with open(bz_path, "r", encoding="utf-8") as bf:
                bz_txt = bf.read()
            for m in re.finditer(r'"//([A-Za-z0-9_./-]+)(?::[^"]*)?"', bz_txt):
                vp = m.group(1).strip("/")
                if vp and vp != clean_pkg and not vp.startswith("visibility"):
                    cands.append(vp)
        except OSError:
            pass
    parent_dir = os.path.dirname(clean_pkg)
    parent_abs = os.path.join(workdir, parent_dir) if parent_dir else ""
    if parent_abs and os.path.isdir(parent_abs):
        try:
            for entry in sorted(os.listdir(parent_abs)):
                if entry.startswith("."):
                    continue
                sub = f"{parent_dir}/{entry}" if parent_dir else entry
                if sub != clean_pkg and (
                    os.path.isfile(os.path.join(workdir, sub, "BUILD.gn"))
                    or os.path.isfile(os.path.join(workdir, sub, "BUILD.bazel"))
                ):
                    cands.append(sub)
        except OSError:
            pass
    return list(dict.fromkeys(cands))


# Check whether a cross-package BUILD.gn or BUILD.bazel references //pkg_dir:target_name.
def _is_referenced_by_cross_pkg_gn(pkg_dir, target_name):
    clean_pkg = pkg_dir.strip("/")
    lbl_re = (
        r'"//' + re.escape(clean_pkg) + r":" + re.escape(target_name) + r'(?:\(|")'
    )
    if target_name == os.path.basename(clean_pkg):
        lbl_re += r'|"//' + re.escape(clean_pkg) + r'(?:\(|")'
    for cross_dir in _collect_cross_pkg_gn_dirs(clean_pkg):
        cross_code = strip_bazel_test_suites(
            read_gn(os.path.join(cross_dir, "BUILD.gn"))
        )
        if cross_code and re.search(lbl_re, cross_code):
            return True
        if _cross_bazel_packages_test_bin(cross_dir, clean_pkg, target_name):
            return True
    return False


# Check whether a device test binary is packaged in local GN, ancestor GN/Bazel, or cross-pkg GN/Bazel.
def is_packaged_by_kept_gn_test_package(gn_text, pkg_dir, bin_name):
    clean_pkg = pkg_dir.strip("/")
    if _pre_sentinel_packages_test_bin(gn_text, clean_pkg, "", bin_name):
        return True
    anc = os.path.dirname(clean_pkg)
    while anc:
        anc_gn = os.path.join(workdir, anc, "BUILD.gn")
        if os.path.isfile(anc_gn):
            try:
                with open(anc_gn, "r", encoding="utf-8") as fh:
                    anc_raw = fh.read()
            except OSError:
                anc_raw = ""
            rel_sub = os.path.relpath(clean_pkg, anc)
            if _pre_sentinel_packages_test_bin(
                anc_raw, clean_pkg, rel_sub, bin_name, allow_post_sentinel=True
            ):
                return True
        if _cross_bazel_packages_test_bin(anc, clean_pkg, bin_name):
            return True
        next_anc = os.path.dirname(anc)
        if next_anc == anc:
            break
        anc = next_anc
    for cross_dir in _collect_cross_pkg_gn_dirs(clean_pkg):
        cross_gn = os.path.join(workdir, cross_dir, "BUILD.gn")
        if os.path.isfile(cross_gn):
            try:
                with open(cross_gn, "r", encoding="utf-8") as fh:
                    cross_raw = fh.read()
            except OSError:
                cross_raw = ""
            if _pre_sentinel_packages_test_bin(
                cross_raw, clean_pkg, "", bin_name, allow_post_sentinel=True
            ):
                return True
        if _cross_bazel_packages_test_bin(cross_dir, clean_pkg, bin_name):
            return True
    return False


def closing(code, i):
    depth, in_str = 0, False
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


_gn_cache = {}


def read_gn(rel):
    if rel not in _gn_cache:
        try:
            with open(os.path.join(workdir, rel), "r", encoding="utf-8") as f:
                _gn_cache[rel] = gn_code(f.read())
        except Exception:
            _gn_cache[rel] = ""
    return _gn_cache[rel]


def gn_block(code, template, name):
    m = re.search(r"\b" + re.escape(template) + r'\(\s*"' + re.escape(name) + r'"\s*\)\s*\{', code)
    return code[m.end() : closing(code, m.end() - 1)] if m else ""


def bazel_test_suites_for(pkg):
    """[(gn_rel, suite, line, param, labels)] for GN bazel_test_suite targets that may list tests
    of pkg: in pkg's BUILD.gn, its ancestors' BUILD.gn, any BUILD.gn mentioning //pkg, and the
    BUILD.gn next to any BUILD.bazel mentioning //pkg (whose test_suite may aggregate pkg's tests)."""
    rels = []
    d = pkg
    while True:
        rels.append(os.path.join(d, "BUILD.gn") if d else "BUILD.gn")
        if not d:
            break
        d = os.path.dirname(d)
    try:
        out = subprocess.run(
            ["git", "-C", workdir, "grep", "-l", "--fixed-strings", "-e", f"//{pkg}:", "-e", f'//{pkg}"',
             "--", "BUILD.gn", "*/BUILD.gn", "BUILD.bazel", "*/BUILD.bazel"],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, timeout=300,
        ).stdout
        rels += [re.sub(r"BUILD\.bazel$", "BUILD.gn", l.strip()) for l in out.splitlines() if l.strip()]
    except (OSError, subprocess.SubprocessError):
        pass
    result = []
    for rel in dict.fromkeys(rels):
        code = read_gn(rel)
        for m in SUITE_RE.finditer(code):
            body = code[m.end() : closing(code, m.end() - 1)]
            line = code.count("\n", 0, m.start()) + 1
            for lm in SUITE_LIST_RE.finditer(body):
                result.append((rel, m.group(1), line, lm.group(1), re.findall(r'"([^"]+)"', lm.group(2))))
    return result


def refs_local(code, d, name):
    prefix = f"(?://{re.escape(d)})?:" if d else ":"
    return bool(re.search(r'"' + prefix + re.escape(name) + r'(?:\([^"]*\))?"', code))


def refs_from_parent(parent_code, d, name):
    if not parent_code or not d:
        return False
    base = os.path.basename(d)
    pat = r'"(?://' + re.escape(d) + r"|" + re.escape(base) + r"):" + re.escape(name) + r'(?:\([^"]*\))?"'
    return bool(re.search(pat, parent_code))


# Check whether a bazel2gn-generated GN twin test is already wired in BUILD.gn or parent BUILD.gn.
def is_dual_build_gn_test_wired(gn_pre_text, parent_gn_text, pkg_dir, twin_name):
    if re.search(r'":' + re.escape(twin_name) + r'"', gn_pre_text):
        return True
    if f"//{pkg_dir}:{twin_name}\"" in parent_gn_text:
        return True
    return refs_from_parent(parent_gn_text, pkg_dir, twin_name)


ROOT_TEST_GROUPS = {
    "tests",
    "host_tests",
    "target_tests",
    "hermetic_tests",
    "non_hermetic_tests",
    "e2e_tests",
    "tests_no_e2e",
}
GN_TESTS_ROOT_RE = re.compile(
    r'\b(?:group|standardized_test_groups)\(\s*"tests"\s*\)'
)
GN_GROUP_OR_STD_TEST_GROUPS_RE = re.compile(
    r'\b(group|standardized_test_groups)\(\s*"([^"]+)"\s*\)\s*\{'
)


# Check whether a GN `bazel_test_suite` is wired into a root test group (`group("tests")`,
# `standardized_test_groups(...)`, or a standard root group such as `host_tests`/`hermetic_tests`),
# either directly, via intermediate local groups, or from the parent `BUILD.gn`.
def suite_is_wired(gn_rel, suite):
    if suite in ROOT_TEST_GROUPS:
        return True
    d = os.path.dirname(gn_rel)
    code = read_gn(gn_rel)
    parent_code = read_gn(os.path.join(os.path.dirname(d), "BUILD.gn")) if d else ""
    if refs_from_parent(parent_code, d, suite):
        return True
    groups = {}
    std_roots = set()
    for m in GN_GROUP_OR_STD_TEST_GROUPS_RE.finditer(code):
        tmpl, g_name = m.group(1), m.group(2)
        body = code[m.end() : closing(code, m.end() - 1)]
        groups[g_name] = groups.get(g_name, "") + "\n" + body
        if tmpl == "standardized_test_groups":
            std_roots.add(g_name)
    queue = [
        g
        for g in groups
        if g in ROOT_TEST_GROUPS
        or g in std_roots
        or refs_from_parent(parent_code, d, g)
    ]
    seen = set(queue)
    while queue:
        body = groups[queue.pop(0)]
        if refs_local(body, d, suite):
            return True
        for g in groups:
            if g not in seen and refs_local(body, d, g):
                seen.add(g)
                queue.append(g)
    return False


def list_strings(node):
    """String elements of a Bazel list expression, including `a + b` concatenations."""
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        return list_strings(node.left) + list_strings(node.right)
    return bazel_strings(node)


_bazel_suites_cache = {}


# Parse `test_suite` targets in `pkg/BUILD.bazel`, stripping git conflict marker lines
# on syntax error so an unmerged ancestor `BUILD.bazel` does not hide its `test_suite`
# and trigger a misleading leaf `unexported_bazel_test` error during rebase resolution.
def bazel_suites_of(pkg):
    """{test_suite name: [test labels], or None when `tests` is omitted} of pkg's BUILD.bazel."""
    if pkg not in _bazel_suites_cache:
        found = {}
        try:
            with open(os.path.join(workdir, pkg, "BUILD.bazel"), "r", encoding="utf-8") as f:
                raw = f.read()
            try:
                t = ast.parse(raw)
            except SyntaxError:
                cleaned = re.sub(r"(?m)^(?:<{7}|={7}|\|{7}|>{7})[^\n]*$", "", raw)
                t = ast.parse(cleaned)
            for node in ast.walk(t):
                if isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id == "test_suite":
                    kws = {kw.arg: kw.value for kw in node.keywords if kw.arg}
                    n = kws.get("name")
                    if isinstance(n, ast.Constant) and isinstance(n.value, str):
                        tests_kw = kws.get("tests")
                        tests_list = None if tests_kw is None else list_strings(tests_kw)
                        found[n.value] = tests_list if tests_list else None
        except Exception:
            pass
        _bazel_suites_cache[pkg] = found
    return _bazel_suites_cache[pkg]


# Find closing `]` matching `code[i] == "["` outside GN string literals.
def closing_bracket(code, i):
    depth, in_str = 0, False
    while i < len(code):
        c = code[i]
        if in_str:
            if c == "\\":
                i += 2
                continue
            in_str = c != '"'
        elif c == '"':
            in_str = True
        elif c == "[":
            depth += 1
        elif c == "]":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return len(code)


# Detect duplicate `<attr> = [ <non-empty> ]` assignments in the same GN `{ ... }` scope,
# which cause `gn gen` to fail with `Replacing nonempty list`.
def find_gn_duplicate_list_assignments(gn_text):
    code = gn_code(gn_text)
    if not code.strip():
        return []
    scopes = [(0, len(code))]
    in_str = False
    i = 0
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
            end_i = closing(code, i)
            scopes.append((i + 1, end_i))
        i += 1

    dups = []
    assign_re = re.compile(r"(?m)^\s*([A-Za-z_]\w*)\s*(=|\+=|-=)\s*\[")
    for s_start, s_end in scopes:
        buf = list(code[s_start:s_end])
        b_in_str = False
        j = 0
        while j < len(buf):
            ch = buf[j]
            if b_in_str:
                if ch == "\\":
                    j += 2
                    continue
                b_in_str = ch != '"'
            elif ch == '"':
                b_in_str = True
            elif ch == "{":
                # Blank out the interior of every nested `{ ... }` scope (including scope
                # literals inside `[ ... ]` lists) while keeping the outer `{` and `}` tokens
                # so enclosing list assignments still register as non-empty.
                sub_end = closing(code, s_start + j) - s_start
                for k in range(j + 1, min(sub_end, len(buf))):
                    if buf[k] != "\n":
                        buf[k] = " "
                j = sub_end + 1
                continue
            j += 1
        direct = "".join(buf)
        seen_nonempty = {}
        for m in assign_re.finditer(direct):
            var_name, op = m.group(1), m.group(2)
            open_b = m.end() - 1
            close_b = closing_bracket(direct, open_b)
            inner = direct[open_b + 1 : close_b].strip()
            line_no = code.count("\n", 0, s_start + m.start(1)) + 1
            if op == "=":
                if not inner:
                    seen_nonempty.pop(var_name, None)
                elif var_name in seen_nonempty:
                    prev_ln, prev_end = seen_nonempty[var_name]
                    between = code[s_start + prev_end : s_start + m.start(1)]
                    clear_re = rf"(?m)^\s*{re.escape(var_name)}\s*=\s*\[\s*\]"
                    if not re.search(clear_re, between):
                        dups.append((var_name, prev_ln, line_no))
                    seen_nonempty[var_name] = (line_no, close_b)
                else:
                    seen_nonempty[var_name] = (line_no, close_b)
    return dups


# Detect duplicate top-level rule target `name = "..."` definitions in a BUILD.bazel file.
def find_bazel_duplicate_targets(bazel_text, bazel_rel=""):
    if not bazel_text.strip():
        return []
    try:
        tree = ast.parse(bazel_text, filename=bazel_rel or "<BUILD.bazel>")
    except Exception:
        return []
    seen = {}
    dups = []
    for stmt in tree.body:
        if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
            continue
        call = stmt.value
        rule_name = (
            call.func.id
            if isinstance(call.func, ast.Name)
            else (call.func.attr if isinstance(call.func, ast.Attribute) else "target")
        )
        t_name = None
        for kw in call.keywords:
            if (
                kw.arg == "name"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                t_name = kw.value.value
                break
        if not t_name:
            continue
        line_no = getattr(stmt, "lineno", 1)
        if t_name in seen:
            dups.append((t_name, rule_name, seen[t_name], line_no))
        else:
            seen[t_name] = line_no
    return dups


def foreign_suite_tests(label, pkg, td, seen):
    """Names of td's targets that label (written in pkg) runs, following Bazel test_suite targets
    of other packages (e.g. an ancestor's aggregate suite that a GN bazel_test_suite exports)."""
    parts = split_label(label, pkg)
    if not parts or parts in seen:
        return set()
    seen.add(parts)
    l_pkg, l_name = parts
    if l_pkg == td:
        return {l_name}
    out = set()
    for m in bazel_suites_of(l_pkg).get(l_name) or []:  # Without `tests`: only l_pkg's own tests.
        out |= foreign_suite_tests(m, l_pkg, td, seen)
    return out


# Read the base version of a package's BUILD.bazel to check pre-existing test_suite entries.
_base_bazel_src_cache = {}


def base_bazel_src_of(pkg):
    if pkg not in _base_bazel_src_cache:
        rel = os.path.join(pkg, "BUILD.bazel") if pkg else "BUILD.bazel"
        eff_base = globals().get("effective_diff_base") or change_base
        if eff_base == "HEAD" and globals().get("head_is_task_commit"):
            refs = ["HEAD~1"]
        elif eff_base == "HEAD":
            refs = ["HEAD~1", "HEAD"]
        else:
            refs = [eff_base]
        content = ""
        for r in refs:
            try:
                content = subprocess.check_output(
                    ["git", "-C", workdir, "show", f"{r}:{rel}"],
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
                break
            except Exception:
                pass
        _base_bazel_src_cache[pkg] = content
    return _base_bazel_src_cache[pkg]


_BROAD_UMBRELLA_ANCESTORS = {"", "src", "sdk", "build", "third_party"}
_base_bazel_suites_cache = {}


# Parse test_suite target names and member lists present in pkg's BUILD.bazel at the change base.
def base_bazel_suite_members(pkg):
    if pkg not in _base_bazel_suites_cache:
        suites = {}
        src = base_bazel_src_of(pkg)
        if src:
            try:
                t = ast.parse(src)
                for node in ast.walk(t):
                    if (
                        isinstance(node, ast.Call)
                        and isinstance(node.func, ast.Name)
                        and node.func.id == "test_suite"
                    ):
                        kws = {kw.arg: kw.value for kw in node.keywords if kw.arg}
                        n = kws.get("name")
                        if isinstance(n, ast.Constant) and isinstance(n.value, str):
                            t_kw = kws.get("tests")
                            suites[n.value] = (
                                None if t_kw is None else list_strings(t_kw)
                            )
            except Exception:
                pass
        _base_bazel_suites_cache[pkg] = suites
    return _base_bazel_suites_cache[pkg]


def base_bazel_suites_of(pkg):
    return set(base_bazel_suite_members(pkg).keys())


# Check whether any directory strictly between anc_pkg and td defines its own aggregation test suite.
def has_intermediate_test_suite_package(td, anc_pkg):
    cur = os.path.dirname(td)
    while cur and cur != anc_pkg and (not anc_pkg or cur.startswith(anc_pkg + "/")):
        if any(
            is_cross_pkg_bazel_suite(cur, s) for s in bazel_suites_of(cur)
        ) or SUITE_RE.search(read_gn(os.path.join(cur, "BUILD.gn"))):
            return True
        cur = os.path.dirname(cur)
    return False


# Check whether a Bazel test_suite aggregates tests from other packages rather than
# only local targets within `pkg`.
def is_cross_pkg_bazel_suite(pkg, suite_name):
    members = bazel_suites_of(pkg).get(suite_name)
    if not members:
        return False
    for lbl in members:
        sp = split_label(lbl, pkg)
        if sp and sp[0] != pkg:
            return True
    return False


# Check whether an aggregation BUILD.bazel test_suite already aggregates tests from td's
# child subtree under anc_pkg (or td is a direct child of anc_pkg).
def ancestor_suite_covers_subtree(anc_pkg, anc_suite, td):
    in_scope_pkgs = set(target_dirs) | {td}
    base_suites = base_bazel_suites_of(anc_pkg)
    suite_is_new_at_base = anc_suite not in base_suites
    visited = set()
    queue = [(anc_pkg, anc_suite)]
    member_pkgs = set()
    has_direct_leaf_member = False
    while queue:
        cur_pkg, cur_suite = queue.pop(0)
        if (cur_pkg, cur_suite) in visited:
            continue
        visited.add((cur_pkg, cur_suite))
        cur_base_src = base_bazel_src_of(cur_pkg)
        for lbl in bazel_suites_of(cur_pkg).get(cur_suite) or []:
            sp = split_label(lbl, cur_pkg)
            if not sp:
                continue
            m_pkg, m_name = sp
            # Ignore newly added references to target_dirs packages when deciding whether
            # an aggregation suite already covered td's subtree at the change base.
            if m_pkg in in_scope_pkgs and f"//{m_pkg}:" not in cur_base_src:
                continue
            member_pkgs.add(m_pkg)
            if m_name in bazel_suites_of(m_pkg):
                queue.append((m_pkg, m_name))
                if cur_pkg == anc_pkg and not is_cross_pkg_bazel_suite(
                    m_pkg, m_name
                ):
                    has_direct_leaf_member = True
            elif cur_pkg == anc_pkg:
                has_direct_leaf_member = True
    if td in member_pkgs:
        return True
    if os.path.dirname(td) == anc_pkg:
        if anc_pkg not in _BROAD_UMBRELLA_ANCESTORS:
            return True
        return bool(member_pkgs) or suite_is_new_at_base
    rel_td = os.path.relpath(td, anc_pkg)
    first_seg = rel_td.split("/")[0]
    sub_root = os.path.join(anc_pkg, first_seg) if anc_pkg else first_seg
    if any(p == sub_root or p.startswith(sub_root + "/") for p in member_pkgs):
        return True
    if suite_is_new_at_base or not member_pkgs:
        if any(
            ancestor_suite_covers_subtree(anc_pkg, other_s, td)
            for other_s in base_suites
            if other_s != anc_suite
        ):
            return True
        if (
            not base_suites
            and anc_pkg not in _BROAD_UMBRELLA_ANCESTORS
            and not has_intermediate_test_suite_package(td, anc_pkg)
        ):
            return True
    if (
        has_direct_leaf_member
        and anc_pkg not in _BROAD_UMBRELLA_ANCESTORS
        and not has_intermediate_test_suite_package(td, anc_pkg)
    ):
        return True
    # For flat area test suites (outside multi-subsystem top-level roots like src/sdk/zircon)
    # that directly list leaf test targets or package test_suites, cover all subdirectories unless
    # an intermediate ancestor between anc_pkg and td defines its own aggregation test_suite or bazel_test_suite.
    if anc_pkg not in ("src", "sdk", "zircon") and has_direct_leaf_member:
        if not has_intermediate_test_suite_package(td, anc_pkg):
            return True
    return False


# Find all wired aggregation BUILD.bazel test_suite targets covering td's subtree.
def wired_ancestor_test_suites(td, param):
    parts = [p for p in td.split("/") if p]
    ancestors = ["/".join(parts[:i]) for i in range(1, len(parts))]
    if not ancestors:
        return []
    anc_set = set(ancestors)
    queue = []
    discovered = []
    seen = set()
    for anc in ancestors:
        anc_gn_rel = os.path.join(anc, "BUILD.gn")
        code = read_gn(anc_gn_rel)
        if not code:
            continue
        for m in SUITE_RE.finditer(code):
            gn_suite = m.group(1)
            if not suite_is_wired(anc_gn_rel, gn_suite):
                continue
            body = code[m.end() : closing(code, m.end() - 1)]
            for lm in SUITE_LIST_RE.finditer(body):
                if lm.group(1) != param:
                    continue
                for lbl in re.findall(r'"([^"]+)"', lm.group(2)):
                    sp = split_label(lbl, anc)
                    if (
                        sp
                        and sp[0] in anc_set
                        and sp[1] in bazel_suites_of(sp[0])
                        and sp not in seen
                    ):
                        seen.add(sp)
                        queue.append(sp)
                        discovered.append(sp)
    while queue:
        cur_pkg, cur_suite = queue.pop(0)
        for m_lbl in bazel_suites_of(cur_pkg).get(cur_suite) or []:
            sp = split_label(m_lbl, cur_pkg)
            if (
                sp
                and sp[0] in anc_set
                and sp[1] in bazel_suites_of(sp[0])
                and sp not in seen
            ):
                seen.add(sp)
                queue.append(sp)
                discovered.append(sp)
    discovered = [
        item
        for item in discovered
        if ancestor_suite_covers_subtree(item[0], item[1], td)
    ]
    if not discovered:
        return []
    discovered.sort(
        key=lambda item: (item[0].count("/"), len(item[0]), item[1] == param)
    )
    return discovered


# Find the nearest ancestor BUILD.bazel test_suite exported by a wired ancestor bazel_test_suite.
def nearest_ancestor_test_suite(td, param):
    suites = wired_ancestor_test_suites(td, param)
    return suites[-1] if suites else None


# Check whether a pre-migration GN test package existed in base BUILD.gn alongside an
# explicit test group (`group("tests")`), yet was never wired into any test group or
# parent BUILD.gn (so leaving it unexported preserves 1-to-1 tests.json parity).
def was_unwired_gn_test_package_at_base(base_gn_text, parent_gn_text, target_name):
    if not base_gn_text:
        return False
    pkg_decl_re = re.compile(
        r'\b(?:fuchsia_unittest_package|fuchsia_test_package)\(\s*"'
        + re.escape(target_name)
        + r'"\s*\)'
    )
    if not pkg_decl_re.search(base_gn_text):
        return False
    if not re.search(
        r'\bgroup\(\s*"(?:tests|hermetic_tests|non_hermetic_tests)"\s*\)',
        base_gn_text,
    ):
        return False
    if re.search(r'":' + re.escape(target_name) + r'(?:\(|")', base_gn_text):
        return False
    if parent_gn_text and re.search(
        r":" + re.escape(target_name) + r'(?:\(|")', parent_gn_text
    ):
        return False
    return True


# Return the first wired bazel_test_suite target name in anc_pkg/BUILD.gn, if any.
def existing_wired_gn_suite_name(anc_pkg):
    anc_gn_rel = os.path.join(anc_pkg, "BUILD.gn")
    code = read_gn(anc_gn_rel)
    if not code:
        return None
    for m in SUITE_RE.finditer(code):
        if suite_is_wired(anc_gn_rel, m.group(1)):
            return m.group(1)
    return None


def ancestor_references_package(anc, td):
    for members in bazel_suites_of(anc).values():
        for m_lbl in members or []:
            sp = split_label(m_lbl, anc)
            if sp and sp[0] == td:
                return True
    return False


def _matching_area_suite_name(anc, param):
    default_name = "tests" if param == "target_tests" else param
    anc_suites = bazel_suites_of(anc)
    if default_name in anc_suites:
        return default_name
    if param == "host_tests":
        cands = [
            s
            for s in anc_suites
            if s.endswith("_host_tests") and is_cross_pkg_bazel_suite(anc, s)
        ]
    else:
        cands = [
            s
            for s in anc_suites
            if (s == "tests" or s.endswith("_tests"))
            and not s.endswith("host_tests")
            and is_cross_pkg_bazel_suite(anc, s)
        ]
    return cands[0] if len(cands) == 1 else default_name


_UNTRACKED_BAZEL_PKGS = None


def untracked_bazel_packages():
    global _UNTRACKED_BAZEL_PKGS
    if _UNTRACKED_BAZEL_PKGS is None:
        pkgs = set()
        for rel in git_lines(
            [
                "ls-files",
                "--others",
                "--exclude-standard",
                "--",
                "BUILD.bazel",
                "*/BUILD.bazel",
            ]
        ):
            if rel.endswith("BUILD.bazel"):
                pkgs.add(os.path.dirname(rel))
        _UNTRACKED_BAZEL_PKGS = pkgs
    return _UNTRACKED_BAZEL_PKGS


def external_package_suite_refs(td):
    """Return (target_refs, host_refs) listing (agg_pkg, suite_name) pairs in other packages
    that directly reference `//{td}:tests` or `//{td}:host_tests`."""
    parts = [p for p in td.split("/") if p]
    cands = ["/".join(parts[:i]) for i in range(len(parts))]
    cands.extend(target_dirs)
    for rel in git_lines(
        [
            "grep",
            "-l",
            "--fixed-strings",
            "-e",
            f"//{td}:",
            "--",
            "BUILD.bazel",
            "*/BUILD.bazel",
        ]
    ):
        if rel.endswith("BUILD.bazel"):
            cands.append(os.path.dirname(rel))
    cands.extend(sorted(untracked_bazel_packages()))
    target_refs = []
    host_refs = []
    for agg_pkg in dict.fromkeys(cands):
        if agg_pkg == td:
            continue
        for s_name, members in bazel_suites_of(agg_pkg).items():
            for lbl in members or []:
                sp = split_label(lbl, agg_pkg)
                if sp == (td, "tests"):
                    target_refs.append((agg_pkg, s_name))
                elif sp == (td, "host_tests"):
                    host_refs.append((agg_pkg, s_name))
    return (target_refs, host_refs)


# Find the canonical area directory where an aggregation BUILD.bazel test_suite and BUILD.gn
# bazel_test_suite should be created when no aggregation suite for `param` exists yet.
# Never invents a brand-new area BUILD.bazel when no aggregation suite exists yet.
def preferred_ancestor_area_suite(td, param, has_both_kinds=False):
    parts = [p for p in td.split("/") if p]
    ancestors = ["/".join(parts[:i]) for i in range(1, len(parts))]
    if not ancestors:
        return None
    if not has_both_kinds:
        td_suites = bazel_suites_of(td)
        has_both_kinds = "tests" in td_suites and "host_tests" in td_suites
    other_param = "host_tests" if param == "target_tests" else "target_tests"
    other_anc = nearest_ancestor_test_suite(td, other_param) if has_both_kinds else None
    # Prefer an ancestor directory that already directly references td first so both
    # "tests" and "host_tests" stay in the same aggregation BUILD.bazel; otherwise
    # prefer the nearest canonical area directory covering td's subtree.
    ordered_ancestors = [
        a for a in reversed(ancestors) if ancestor_references_package(a, td)
    ] + [
        a for a in reversed(ancestors) if not ancestor_references_package(a, td)
    ]
    for anc in ordered_ancestors:
        refs_td = ancestor_references_package(anc, td) or (
            other_anc is not None and other_anc[0] == anc
        )
        if anc in _BROAD_UMBRELLA_ANCESTORS and not refs_td:
            continue
        if has_intermediate_test_suite_package(td, anc):
            continue
        if not refs_td and os.path.dirname(td) != anc and not any(
            ancestor_suite_covers_subtree(anc, s_name, td)
            for s_name in bazel_suites_of(anc)
        ):
            continue
        if existing_wired_gn_suite_name(anc) is not None:
            return (anc, _matching_area_suite_name(anc, param))
        anc_gn_rel = os.path.join(anc, "BUILD.gn")
        code = read_gn(anc_gn_rel)
        if bazel_suites_of(anc) and code and re.search(
            r'\bgroup\(\s*"tests(?:_no_e2e)?"\s*\)\s*\{', code
        ):
            return (anc, _matching_area_suite_name(anc, param))
    ext_t_refs, ext_h_refs = external_package_suite_refs(td)
    ext_pkgs = list(dict.fromkeys([p for p, _ in ext_t_refs + ext_h_refs]))
    if len(ext_pkgs) == 1:
        ext_pkg = ext_pkgs[0]
        if existing_wired_gn_suite_name(ext_pkg) is not None:
            return (ext_pkg, _matching_area_suite_name(ext_pkg, param))
        ext_code = read_gn(os.path.join(ext_pkg, "BUILD.gn"))
        if bazel_suites_of(ext_pkg) and ext_code and re.search(
            r'\bgroup\(\s*"tests(?:_no_e2e)?"\s*\)\s*\{', ext_code
        ):
            return (ext_pkg, _matching_area_suite_name(ext_pkg, param))
    return None


_GIT_OUT_CACHE = {}
_IS_ANCESTOR_CACHE = {}


def git_out(args):
    key = tuple(args)
    if key in _GIT_OUT_CACHE:
        return _GIT_OUT_CACHE[key]
    try:
        res = subprocess.check_output(
            ["git", "-C", workdir] + list(key), stderr=subprocess.DEVNULL, text=True
        ).strip()
    except Exception:
        res = ""
    _GIT_OUT_CACHE[key] = res
    return res


def git_lines(args):
    out = git_out(args)
    return [l.strip() for l in out.splitlines() if l.strip()]


def is_ancestor(anc, desc):
    if not anc or not desc:
        return False
    key = (anc, desc)
    if key in _IS_ANCESTOR_CACHE:
        return _IS_ANCESTOR_CACHE[key]
    res = (
        subprocess.run(
            ["git", "-C", workdir, "merge-base", "--is-ancestor", anc, desc],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        ).returncode
        == 0
    )
    _IS_ANCESTOR_CACHE[key] = res
    return res


_GIT_SUCCESS_CACHE = {}


def git_success(args):
    key = tuple(args)
    if key in _GIT_SUCCESS_CACHE:
        return _GIT_SUCCESS_CACHE[key]
    try:
        res = (
            subprocess.run(
                ["git", "-C", workdir] + list(key),
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            ).returncode
            == 0
        )
    except Exception:
        res = False
    _GIT_SUCCESS_CACHE[key] = res
    return res


# Refresh local `refs/remotes/origin/main` when stale so `git merge-tree` checks
# against current upstream `origin/main` and only caches a stamp on success.
def maybe_refresh_origin_main(parent_sha):
    if (
        os.environ.get("PLANTER_SKIP_FETCH", "").strip() not in ("", "0", "false")
        or os.environ.get("PLANTER_SKIP_BUILD", "").strip() not in ("", "0", "false")
    ):
        return
    force_fetch = os.environ.get("PLANTER_FORCE_FETCH", "").strip() in ("1", "true")
    origin_sha = git_out(["rev-parse", "--verify", "--quiet", "origin/main^{commit}"])
    stamp_name = (
        "planter-origin-main-fetch-"
        + hashlib.sha256(workdir.encode("utf-8")).hexdigest()[:16]
        + ".stamp"
    )
    stamp_path = os.path.join(tempfile.gettempdir(), stamp_name)
    now = time.time()
    fresh_stamp = False
    try:
        with open(stamp_path, "r", encoding="utf-8") as sf:
            parts = sf.read().strip().split()
        if (
            len(parts) == 2
            and now - int(parts[0]) < FETCH_TTL_SECS
            and origin_sha
            and parts[1] == origin_sha
        ):
            fresh_stamp = True
    except (OSError, ValueError):
        pass
    fresh_ref = False
    if origin_sha and origin_sha != parent_sha:
        for ref_rel in ("logs/refs/remotes/origin/main", "refs/remotes/origin/main"):
            gp = git_out(["rev-parse", "--git-path", ref_rel])
            if gp:
                abs_gp = gp if os.path.isabs(gp) else os.path.join(workdir, gp)
                try:
                    if now - os.path.getmtime(abs_gp) < FETCH_TTL_SECS:
                        fresh_ref = True
                        break
                except OSError:
                    pass
    behind_parent = bool(
        origin_sha
        and parent_sha
        and origin_sha != parent_sha
        and is_ancestor(origin_sha, parent_sha)
    )
    if origin_sha and not force_fetch and (fresh_stamp or fresh_ref) and not behind_parent:
        return
    # Fast-path: check remote refs/heads/main SHA first to avoid a full fetch when unchanged.
    remote_sha = ""
    try:
        ls_proc = subprocess.run(
            ["git", "-C", workdir, "ls-remote", "--exit-code", "origin", "refs/heads/main"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=15,
            check=False,
        )
        if ls_proc.returncode == 0 and ls_proc.stdout:
            remote_sha = ls_proc.stdout.strip().split()[0]
    except Exception:
        remote_sha = ""
    if origin_sha and remote_sha and remote_sha == origin_sha:
        try:
            with open(stamp_path, "w", encoding="utf-8") as sf:
                sf.write(f"{int(now)} {origin_sha}\n")
        except OSError:
            pass
        return
    fetch_ok = False
    try:
        fetch_proc = subprocess.run(
            [
                "git",
                "-C",
                workdir,
                "fetch",
                "--quiet",
                "--no-tags",
                "origin",
                "+refs/heads/main:refs/remotes/origin/main",
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=90,
            check=False,
        )
        fetch_ok = fetch_proc.returncode == 0
    except Exception:
        fetch_ok = False
    new_origin_sha = git_out(["rev-parse", "--verify", "--quiet", "origin/main^{commit}"])
    if fetch_ok and new_origin_sha and (not remote_sha or new_origin_sha == remote_sha):
        try:
            with open(stamp_path, "w", encoding="utf-8") as sf:
                sf.write(f"{int(time.time())} {new_origin_sha}\n")
        except OSError:
            pass


# 1. Check for unfinished git rebase/merge state or unmerged index entries.
# Note: Do not check standalone `REBASE_HEAD`, which git can leave behind after `git rebase --continue`
# finishes even when `rebase-merge` and `rebase-apply` are gone.
active_rebase_or_merge = None
for state_name in ("rebase-merge", "rebase-apply", "MERGE_HEAD"):
    git_path = git_out(["rev-parse", "--git-path", state_name])
    if git_path:
        abs_git_path = git_path if os.path.isabs(git_path) else os.path.join(workdir, git_path)
        if os.path.exists(abs_git_path):
            active_rebase_or_merge = state_name
            break
unmerged = git_lines(["diff", "--name-only", "--diff-filter=U"])
if active_rebase_or_merge or unmerged:
    state_desc = active_rebase_or_merge or "unmerged index entries"
    findings.append({
        "source": "cq_reachability",
        "category": "unresolved_rebase_or_conflict_markers",
        "severity": "error",
        "file": unmerged[0] if unmerged else "",
        "line": 1,
        "message": (
            f"An unfinished git rebase/merge (`{state_desc}`) is in progress in the checkout"
            + (f" with unmerged file(s): {', '.join(unmerged)}" if unmerged else "")
            + "."
        ),
        "remediation": (
            "Resolve all conflicted files (remove `<<<<<<<`/`=======`/`>>>>>>>` markers and keep "
            "entries sorted; if a conflicted dependency `BUILD.bazel`/`BUILD.gn` outside the "
            "target directory was only edited for `visibility` and `origin/main` already grants "
            "sufficient visibility such as `[\"//:__subpackages__\"]`, take `origin/main`'s "
            "version cleanly with `git checkout origin/main -- <file>`), stage resolved files "
            "with `git add <file>`, and finish with `git -c core.editor=true rebase --continue` "
            "(or abort with `git rebase --abort` and re-run "
            "`git fetch origin main && git rebase origin/main`)."
        ),
    })

head_sha = git_out(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
parent_sha = git_out(["rev-parse", "--verify", "--quiet", "HEAD~1^{commit}"])
maybe_refresh_origin_main(parent_sha)
origin_main = git_out(["rev-parse", "--verify", "--quiet", "origin/main^{commit}"])
head_is_on_origin_main = bool(
    origin_main and head_sha and is_ancestor(head_sha, origin_main)
)
base_sha = git_out(["rev-parse", "--verify", "--quiet", f"{change_base}^{{commit}}"])
merge_base_with_origin = (
    git_out(["merge-base", origin_main, head_sha]) if (origin_main and head_sha) else ""
)
effective_diff_base = change_base
# Filter untracked paths so generated Bazel vendor trees and Python bytecode caches are ignored.
def list_untracked_worktree_files(extra_args=None):
    cmd = ["ls-files", "--others", "--exclude-standard"]
    if extra_args:
        cmd.extend(extra_args)
    return [
        f
        for f in git_lines(cmd)
        if not f.startswith("third_party/bazel_vendor/")
        and "/__pycache__/" not in f
        and not f.startswith("__pycache__/")
        and not f.endswith(".pyc")
    ]


wt_changed_files = (
    git_lines(["diff", "--name-only", "HEAD"])
    + list_untracked_worktree_files()
)
wt_dirty = bool(wt_changed_files)
wt_touches_target = bool(target_dirs) and any(
    any(p.startswith(td + "/") for td in target_dirs) for p in wt_changed_files
)
head_commit_files = (
    git_lines(["diff", "--name-only", "HEAD~1", "HEAD"]) if parent_sha else []
)
head_touches_target = bool(target_dirs) and any(
    any(p.startswith(td + "/") for td in target_dirs) for p in head_commit_files
)
if (
    base_sha
    and merge_base_with_origin
    and base_sha != merge_base_with_origin
    and is_ancestor(base_sha, merge_base_with_origin)
):
    effective_diff_base = merge_base_with_origin
elif parent_sha and (
    not base_sha
    or (
        change_base == "HEAD"
        and base_sha == head_sha
        and not head_is_on_origin_main
        and not wt_dirty
        and head_touches_target
    )
    or not is_ancestor(base_sha, head_sha)
):
    effective_diff_base = merge_base_with_origin or "HEAD~1"

# Check whether HEAD is the task's own commit rather than an upstream origin/main commit,
# PLANTER_CHANGE_BASE, or a shared base commit during a parallel uncommitted working-tree migration.
head_is_task_commit = bool(
    head_sha
    and not head_is_on_origin_main
    and (not base_sha or base_sha != head_sha or effective_diff_base == "HEAD~1")
    and not (wt_touches_target and not head_touches_target)
)

# 2. Check changed and target files for leftover conflict markers.
env_changed = os.environ.get("PLANTER_CHANGED_FILES", "").strip()
if env_changed:
    changed_files = {l.strip() for l in env_changed.splitlines() if l.strip()}
else:
    changed_files = set(git_lines(["diff", "--name-only", effective_diff_base]))
    changed_files.update(git_lines(["diff", "--name-only", "HEAD"]))
    changed_files.update(list_untracked_worktree_files())
for td in target_dirs:
    for bname in ("BUILD.bazel", "BUILD.gn"):
        rel_p = os.path.join(td, bname)
        if os.path.isfile(os.path.join(workdir, rel_p)):
            changed_files.add(rel_p)

# Collect changed and ancestor BUILD.gn / BUILD.bazel files to audit for duplicate definitions.
audit_build_files = set(changed_files)
for td in target_dirs:
    cur_anc = os.path.dirname(td)
    while cur_anc:
        for bname in ("BUILD.bazel", "BUILD.gn"):
            anc_rel = os.path.join(cur_anc, bname)
            if os.path.isfile(os.path.join(workdir, anc_rel)):
                audit_build_files.add(anc_rel)
        cur_anc = os.path.dirname(cur_anc)

worktree_gn_dups = {}
worktree_bz_dups = {}
pre_task_base_ref = "HEAD~1" if (head_is_task_commit and parent_sha) else "HEAD"


# Collect pre-existing duplicate-list variable names from upstream/parent revisions of a GN file.
def collect_preexisting_gn_dup_vars(rel_path):
    dup_vars = set()
    for base_ref in (origin_main, effective_diff_base, pre_task_base_ref):
        if not base_ref:
            continue
        base_txt = git_out(["show", f"{base_ref}:{rel_path}"])
        if base_txt:
            dup_vars.update(
                v for v, _, _ in find_gn_duplicate_list_assignments(base_txt)
            )
    return dup_vars


for rel_p in sorted(audit_build_files):
    abs_p = os.path.join(workdir, rel_p)
    if not os.path.isfile(abs_p):
        continue
    try:
        with open(abs_p, "r", encoding="utf-8", errors="replace") as f:
            src_txt = f.read()
    except OSError:
        continue
    if rel_p.endswith((".gn", ".gni")):
        gn_dups = find_gn_duplicate_list_assignments(src_txt)
        worktree_gn_dups[rel_p] = {(v, p_ln, c_ln) for v, p_ln, c_ln in gn_dups}
        base_dup_vars = collect_preexisting_gn_dup_vars(rel_p) if gn_dups else set()
        for var_name, prev_ln, cur_ln in gn_dups:
            if var_name in base_dup_vars:
                continue
            findings.append({
                "source": "cq_reachability",
                "category": "duplicate_gn_list_assignment",
                "severity": "error",
                "file": rel_p,
                "line": cur_ln,
                "message": (
                    f"'{rel_p}' assigns non-empty list `{var_name} = [...]` at line {cur_ln} "
                    f"after already assigning `{var_name} = [...]` at line {prev_ln} in the "
                    "same GN scope, which causes `gn gen` to fail across CQ builders with "
                    "`ERROR: Replacing nonempty list`."
                ),
                "remediation": (
                    f"Remove the duplicate `{var_name} = [...]` assignment at line {cur_ln} in "
                    f"'{rel_p}' (or merge its entries into the `{var_name} = [...]` list at "
                    f"line {prev_ln} in sorted order)."
                ),
            })
    elif os.path.basename(rel_p) in ("BUILD.bazel", "BUILD"):
        bz_dups = find_bazel_duplicate_targets(src_txt, rel_p)
        worktree_bz_dups[rel_p] = {(t, r) for t, r, _, _ in bz_dups}
        for t_name, rule_name, prev_ln, cur_ln in bz_dups:
            findings.append({
                "source": "cq_reachability",
                "category": "duplicate_bazel_target",
                "severity": "error",
                "file": rel_p,
                "line": cur_ln,
                "message": (
                    f"'{rel_p}' defines `{rule_name}(name = \"{t_name}\")` at line {cur_ln}, "
                    f"duplicating the `{t_name}` target already defined at line {prev_ln} in "
                    "the same package."
                ),
                "remediation": (
                    f"Merge the duplicate `{rule_name}(name = \"{t_name}\")` blocks at lines "
                    f"{prev_ln} and {cur_ln} in '{rel_p}' into a single `{rule_name}(name = "
                    f"\"{t_name}\")` definition combining their target lists in sorted order."
                ),
            })

CONFLICT_MARKER_RE = re.compile(r"^(?:<{7}\s|={7}\s*$|>{7}\s)")
for rel_p in sorted(changed_files):
    abs_p = os.path.join(workdir, rel_p)
    if not os.path.isfile(abs_p):
        continue
    try:
        with open(abs_p, "r", encoding="utf-8", errors="replace") as f:
            for idx, line in enumerate(f, 1):
                if CONFLICT_MARKER_RE.match(line):
                    findings.append({
                        "source": "cq_reachability",
                        "category": "unresolved_rebase_or_conflict_markers",
                        "severity": "error",
                        "file": rel_p,
                        "line": idx,
                        "message": f"Leftover git merge conflict marker `{line.strip()}` in '{rel_p}' at line {idx}.",
                        "remediation": (
                            f"Edit '{rel_p}' to remove all `<<<<<<<`, `=======`, and `>>>>>>>` "
                            "conflict markers, keep both upstream `origin/main` entries and this "
                            "change's additions in sorted order (or run "
                            f"`git checkout origin/main -- {rel_p}` if '{rel_p}' is an external "
                            "dependency package where `origin/main` already widened visibility), "
                            "and stage the resolved file with `git add`."
                        ),
                    })
                    break
    except OSError:
        pass

# Parse `git merge-tree --write-tree --name-only` stdout into (conflicted_files, conflict_msgs).
def parse_merge_tree_conflicts(stdout_text):
    mt_lines = [l.strip() for l in (stdout_text or "").splitlines()]
    conflicted_files = []
    conflict_msgs = []
    in_files = True
    for idx, line in enumerate(mt_lines):
        if idx == 0 and re.fullmatch(r"[0-9a-f]{40}", line):
            continue
        if not line:
            in_files = False
            continue
        if in_files and not line.startswith(("Auto-merging ", "CONFLICT ")):
            if line not in conflicted_files:
                conflicted_files.append(line)
        elif line.startswith("CONFLICT "):
            conflict_msgs.append(line)
    return conflicted_files, conflict_msgs


# Extract top-level named rule target names defined in a BUILD.bazel source string.
def extract_bazel_target_names(bazel_text, filename_hint="<BUILD.bazel>"):
    if not (bazel_text or "").strip():
        return set()
    try:
        tree = ast.parse(bazel_text, filename=filename_hint)
    except Exception:
        return set()
    names = set()
    for stmt in tree.body:
        if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
            continue
        for kw in stmt.value.keywords:
            if (
                kw.arg == "name"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                names.add(kw.value.value)
                break
    return names


# Check whether a composite ref is a stale pre-rebase snapshot or a downstream task's
# stack rather than an unapplied upstream dependency stack for the current commit.
def _is_superseded_or_downstream_composite(
    short_name, ref_sha, norm_targets, own_cids, dep_cids
):
    if not origin_main or not head_sha:
        return False
    ref_is_active_ancestor = bool(
        ref_sha
        and is_ancestor(ref_sha, head_sha)
        and not is_ancestor(ref_sha, origin_main)
    )
    comp_log = git_out(["log", "--format=%B", f"{origin_main}..{ref_sha}"])
    comp_cids = set(re.findall(r"^Change-Id:\s*(I[0-9a-f]{40})\b", comp_log, re.M))
    if short_name not in norm_targets:
        if not (dep_cids & comp_cids) and not ref_is_active_ancestor:
            return True
        if own_cids and (own_cids & comp_cids) and ref_sha != head_sha:
            return True
    head_mb = git_out(["merge-base", origin_main, head_sha])
    ref_mb = git_out(["merge-base", origin_main, ref_sha])
    if not head_mb or not ref_mb or not is_ancestor(ref_mb, head_mb):
        return False
    if head_mb == ref_mb:
        return False
    head_log = git_out(["log", "--format=%B", f"{origin_main}..{head_sha}"])
    head_cids = set(re.findall(r"^Change-Id:\s*(I[0-9a-f]{40})\b", head_log, re.M))
    needed_cids = (
        (comp_cids - own_cids)
        if (short_name in norm_targets or ref_is_active_ancestor)
        else (dep_cids & comp_cids)
    )
    if not (bool(needed_cids) and needed_cids <= head_cids):
        return False
    # If any needed prerequisite commit in ref_sha has an updated patch not in head_sha,
    # ref_sha is an updated stack rather than a superseded pre-rebase snapshot.
    cherry_out = git_lines(["cherry", head_sha, ref_sha, ref_mb])
    unapplied_shas = [l[2:].strip() for l in cherry_out if l.startswith("+ ")]
    for u_sha in unapplied_shas:
        u_msg = git_out(["log", "-1", "--format=%B", u_sha])
        u_cids = set(re.findall(r"^Change-Id:\s*(I[0-9a-f]{40})\b", u_msg, re.M))
        if u_cids & needed_cids:
            return False
    return True


# Resolve the active `refs/planter/composite/<task>` ref and SHA for the current task.
def resolve_task_composite_ref(head_msg_text):
    ref_lines = git_lines(
        ["for-each-ref", "--format=%(refname) %(objectname)", "refs/planter/composite"]
    )
    if not ref_lines:
        return "", ""
    norm_targets = set()
    for td in target_dirs:
        clean_td = td.strip("/")
        full_slug = clean_td.replace("/", "-").replace("_", "-")
        short_slug = (
            re.sub(
                r"^(?:src|sdk|build|tools|zircon|third_party)/",
                "",
                clean_td,
            )
            .replace("/", "-")
            .replace("_", "-")
        )
        if full_slug:
            norm_targets.add(full_slug)
        if short_slug:
            norm_targets.add(short_slug)
    dep_cids = set(re.findall(r"\b(I[0-9a-f]{40})\b", head_msg_text or ""))
    own_cids = set(
        re.findall(r"^Change-Id:\s*(I[0-9a-f]{40})\b", head_msg_text or "", re.M)
    )
    dep_cids -= own_cids
    parsed_refs = []
    for entry in ref_lines:
        parts = entry.split()
        if len(parts) == 2:
            ref_name, ref_sha = parts
            short_name = ref_name.rsplit("/", 1)[-1].replace("_", "-")
            parsed_refs.append((ref_name, ref_sha, short_name))
    for ref_name, ref_sha, short_name in parsed_refs:
        if short_name in norm_targets:
            if not _is_superseded_or_downstream_composite(
                short_name, ref_sha, norm_targets, own_cids, dep_cids
            ):
                return ref_name, ref_sha
    if origin_main:
        cands = []
        for ref_name, ref_sha, short_name in parsed_refs:
            ref_is_anc = bool(
                head_sha
                and is_ancestor(ref_sha, head_sha)
                and not is_ancestor(ref_sha, origin_main)
            )
            if not dep_cids and not ref_is_anc:
                continue
            if _is_superseded_or_downstream_composite(
                short_name, ref_sha, norm_targets, own_cids, dep_cids
            ):
                continue
            comp_log = git_out(["log", "--format=%B", f"{origin_main}..{ref_sha}"])
            comp_cids = set(
                re.findall(r"^Change-Id:\s*(I[0-9a-f]{40})\b", comp_log, re.M)
            )
            cands.append(
                (
                    ref_is_anc,
                    len(dep_cids & comp_cids),
                    len(comp_cids),
                    ref_name,
                    ref_sha,
                )
            )
        if cands:
            cands.sort(key=lambda item: (item[0], item[1], item[2]), reverse=True)
            return cands[0][3], cands[0][4]
    return "", ""


# Check whether a package and target label already exist in origin/main's BUILD.bazel,
# including macro-generated sub-targets (such as fidl_library's <name>_rust / <name>_cpp
# or bind_library's <name>_rust / <name>_cpp across any package directory).
def _bazel_dep_target_exists_on_ref(ref_name, dep_pkg, dep_tgt=""):
    if not ref_name or not dep_pkg:
        return False
    bz_txt = ""
    for bfile in ("BUILD.bazel", "BUILD"):
        txt = git_out(["show", f"{ref_name}:{dep_pkg}/{bfile}"])
        if txt:
            bz_txt += "\n" + txt
    if not bz_txt:
        return False
    if not dep_tgt:
        return True
    if dep_tgt.endswith(".bzl"):
        return git_success(["cat-file", "-e", f"{ref_name}:{dep_pkg}/{dep_tgt}"])
    defined_tgts = extract_bazel_target_names(bz_txt, f"{dep_pkg}/BUILD.bazel")
    if dep_tgt in defined_tgts or any(
        dep_tgt.startswith((t + "_", t + "-")) for t in defined_tgts
    ):
        return True
    if defined_tgts and (
        dep_pkg.startswith("sdk/fidl/")
        or re.search(
            r"\b(?:fidl_library|fuchsia_fidl_library|bind_library|fuchsia_bind_library)\s*\(",
            bz_txt,
        )
    ):
        return True
    if f'"{dep_tgt}"' in bz_txt and git_success(
        ["cat-file", "-e", f"{ref_name}:{dep_pkg}/{dep_tgt}"]
    ):
        return True
    return False


# Check whether all in-tree Bazel dependency packages and targets referenced by
# target_dirs already exist on origin/main (so HEAD needs no unmerged parent commits).
def all_target_bazel_deps_exist_on_origin_main():
    if not origin_main or not target_dirs:
        return False
    target_set = set(target_dirs)
    for td in target_dirs:
        bz_full = os.path.join(workdir, td, "BUILD.bazel")
        if not os.path.isfile(bz_full):
            continue
        try:
            with open(bz_full, "r", encoding="utf-8") as bf:
                bz_src = bf.read()
        except OSError:
            continue
        for m in re.finditer(r'"@?//([A-Za-z0-9_./-]+)(?::([^"]*))?"', bz_src):
            dep_pkg = m.group(1).strip("/")
            dep_tgt = (m.group(2) or os.path.basename(dep_pkg)).strip()
            if (
                not dep_pkg
                or dep_pkg in target_set
                or dep_pkg in ("conditions", "visibility", "command_line_option")
                or dep_pkg.startswith(("build/", "tools/", "third_party/"))
            ):
                continue
            # Ignore visibility-only entries (`:__pkg__` / `:__subpackages__`).
            full_tok = m.group(0)
            if full_tok.endswith((":__pkg__\"", ":__subpackages__\"")):
                continue
            if not _bazel_dep_target_exists_on_ref(origin_main, dep_pkg, dep_tgt):
                return False
    return True


# Build rebase command guidance preserving stacked prerequisite commits only when still needed.
def build_rebase_command_hint(
    composite_ref, has_stacked_parents, drop_obsolete_ancestors=False
):
    if composite_ref and not (
        composite_sha and head_sha and is_ancestor(composite_sha, head_sha)
    ):
        return (
            f"Rebase onto `{composite_ref}` (`git rebase {composite_ref}` after "
            "committing or stashing any uncommitted edits with `git stash push -u`; "
            "never drop prerequisite commits with `git rebase --onto origin/main "
            "HEAD~1`), drop any duplicate prerequisite `BUILD.bazel` target "
            "definitions now provided by the dependency commit (keeping only minimal "
            "caller `visibility` or `exports_files(...)` additions), keep both "
            "upstream/dependency and task entries in sorted order without duplicates, "
            "stage resolved files with `git add <file>`, finish with "
            "`git -c core.editor=true rebase --continue` (preserving `Change-Id`), "
            "and restore stashed working-tree edits with `git stash pop`."
        )
    if drop_obsolete_ancestors:
        return (
            "All in-tree Bazel dependencies of this package already exist on "
            "`origin/main`, while local ancestor commits between `merge-base` and "
            "`HEAD~1` have already landed on `origin/main` or are unrelated stale "
            "stack commits that cause Gerrit `jiri patch` to fail with `Failed to "
            "rebase`. Detach `HEAD` onto the latest `origin/main` by running "
            "`git fetch origin main && git rebase --onto origin/main HEAD~1` (commit "
            "or `git stash push -u` any uncommitted edits first, then `git stash pop` "
            "after rebasing), revert any redundant edit to shared `.bzl` rules under "
            "`build/bazel/rules/` already satisfied on `origin/main` "
            "(`git checkout origin/main -- <file>`), resolve any remaining conflict "
            "keeping entries in sorted order without duplicates, and preserve the "
            "existing `Change-Id` footer."
        )
    if has_stacked_parents:
        return (
            "Rebase the full prerequisite commit stack onto the latest `origin/main` "
            "so Gerrit CQ and downstream stacked tasks apply cleanly: (1) if the "
            "working tree has uncommitted or untracked task files (such as new "
            "`BUILD.bazel` files), stash them with `git stash push -u -m planter-wip` "
            "(plain `git stash` skips untracked files); (2) run `git fetch origin "
            "main && git rebase origin/main` (never rebase onto the old merge-base or "
            "drop stacked prerequisite commits with `git rebase --onto origin/main "
            "HEAD~1`); (3) for each conflicted ancestor commit in the stack (even in "
            "packages outside `PLANTER_TARGET_DIR`), drop duplicate prerequisite "
            "`BUILD.bazel` definitions already on `origin/main`, keep both upstream "
            "and stack entries in sorted order without duplicates, stage with "
            "`git add <file>`, and run `git -c core.editor=true rebase --continue` "
            "(preserving `Change-Id`); (4) restore stashed task edits with "
            "`git stash pop`."
        )
    return (
        "Rebase the change onto the latest `origin/main` so Gerrit CQ can patch it "
        "cleanly: (1) if the working tree has uncommitted or untracked files (such "
        "as a new `BUILD.bazel`), stash them first with `git stash push -u -m "
        "planter-wip`; (2) run `git fetch origin main && git rebase origin/main`, "
        "resolve each conflicted file keeping both `origin/main`'s entries and this "
        "change's additions in sorted order without duplicate entries, stage the "
        "resolved file(s) with `git add <file>`, and finish with "
        "`git -c core.editor=true rebase --continue` (preserving `Change-Id`); "
        "(3) if edits were stashed, restore them with `git stash pop`."
    )


# Snapshot the current working tree (or HEAD tree when clean) as a git tree SHA.
def compute_worktree_tree_sha():
    dirty_tracked = git_lines(
        ["diff", "--name-only", "--ignore-submodules=all", "HEAD"]
    )
    untracked_build = [
        f
        for f in list_untracked_worktree_files()
        if os.path.basename(f) in ("BUILD.bazel", "BUILD.gn", "BUILD")
        or f.endswith((".bzl", ".gni", ".cml"))
    ]
    for td in target_dirs:
        if os.path.isdir(os.path.join(workdir, td)):
            for f in list_untracked_worktree_files(["--", td]):
                if f not in untracked_build:
                    untracked_build.append(f)
    if not dirty_tracked and not untracked_build:
        return git_out(["rev-parse", "--verify", "--quiet", "HEAD^{tree}"])
    git_dir = git_out(["rev-parse", "--absolute-git-dir"])
    if not (git_dir and os.path.isdir(git_dir)):
        return ""
    # Allocate temporary index in system temp dir so worktrees with read-only
    # parent .git directories still run merge-tree cleanly.
    fd, tmp_idx = tempfile.mkstemp(prefix=".planter-cq-merge-index.")
    os.close(fd)
    try:
        real_idx = os.path.join(git_dir, "index")
        if os.path.isfile(real_idx):
            shutil.copyfile(real_idx, tmp_idx)
        else:
            os.unlink(tmp_idx)
        env = dict(os.environ, GIT_INDEX_FILE=tmp_idx)
        to_stage = list(dict.fromkeys(dirty_tracked + untracked_build))
        if to_stage:
            subprocess.run(
                ["git", "-C", workdir, "add", "-A", "--"] + to_stage,
                env=env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
        wt_proc = subprocess.run(
            ["git", "-C", workdir, "write-tree"],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            check=False,
        )
        return wt_proc.stdout.strip() if wt_proc.returncode == 0 else ""
    finally:
        try:
            os.unlink(tmp_idx)
        except OSError:
            pass


# Verify HEAD is rebased onto its composite dependency stack ref without conflicts.
def check_composite_stack_alignment(composite_ref, comp_sha, wt_tree):
    if not composite_ref or not comp_sha or not head_sha or not head_is_task_commit:
        return
    if is_ancestor(comp_sha, head_sha):
        return
    comp_base = git_out(["merge-base", comp_sha, head_sha])
    target_tree = wt_tree or git_out(
        ["rev-parse", "--verify", "--quiet", "HEAD^{tree}"]
    )
    conflicted_files = []
    conflict_msgs = []
    if target_tree:
        mt_cmd = [
            "git",
            "-C",
            workdir,
            "merge-tree",
            "--write-tree",
            "--name-only",
        ]
        if comp_base:
            mt_cmd.append(f"--merge-base={comp_base}")
        mt_cmd.extend([comp_sha, target_tree])
        mt_res = subprocess.run(
            mt_cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
        )
        if mt_res.returncode != 0:
            conflicted_files, conflict_msgs = parse_merge_tree_conflicts(
                mt_res.stdout
            )
    first_file = conflicted_files[0] if conflicted_files else "/COMMIT_MSG"
    conflict_detail = ""
    if conflicted_files:
        files_str = ", ".join(f"'{f}'" for f in conflicted_files)
        notes_str = f" ({'; '.join(conflict_msgs)})" if conflict_msgs else ""
        conflict_detail = (
            f" with merge/cherry-pick conflict(s) in {files_str}{notes_str}"
        )
    msg_text = (
        f"HEAD ({head_sha[:11]}) is not rebased onto its composite dependency stack "
        f"`{composite_ref}` ({comp_sha[:11]}){conflict_detail}, causing downstream "
        "stacked tasks and Gerrit cherry-picks to fail."
    )
    rem_text = (
        f"Rebase onto `{composite_ref}` (`git rebase {composite_ref}` after "
        "committing or stashing any uncommitted edits; never drop prerequisite "
        "commits with `git rebase --onto origin/main HEAD~1`). For any `add/add` "
        "conflict in a prerequisite package's `BUILD.bazel`, drop the duplicate "
        "target definitions now provided by the dependency commit and keep only "
        "minimal caller additions (such as `visibility` or `exports_files(...)`) "
        "required by this task. For shared `BUILD.bazel`/`BUILD.gn` conflicts, keep "
        "both upstream/dependency and task entries in sorted order without "
        "duplicates, stage resolved files with `git add`, and complete "
        "`git -c core.editor=true rebase --continue` preserving `Change-Id`."
    )
    findings.append({
        "source": "cq_reachability",
        "category": "unrebased_composite_stack",
        "severity": "error",
        "file": first_file,
        "line": 1,
        "message": msg_text,
        "remediation": rem_text,
    })


early_head_msg = git_out(["log", "-1", "--format=%B", head_sha]) if head_sha else ""
composite_ref_name, composite_sha = resolve_task_composite_ref(early_head_msg)


# Rebase target with the same precedence as build_rebase_command_hint and prompts/coder.md:
# the task's `refs/planter/composite/<task>` ref when it exists (and HEAD is not already on
# it), else the latest `origin/main`. Returns (description, command).
def rebase_target():
    if composite_ref_name and not (
        composite_sha and head_sha and is_ancestor(composite_sha, head_sha)
    ):
        return f"`{composite_ref_name}`", f"git rebase {composite_ref_name}"
    return "the latest `origin/main`", "git fetch origin main && git rebase origin/main"
has_stacked_ancestors = bool(
    origin_main
    and parent_sha
    and merge_base_with_origin
    and parent_sha != merge_base_with_origin
    and not is_ancestor(parent_sha, origin_main)
)
drop_obsolete_ancestors = bool(
    has_stacked_ancestors
    and not composite_ref_name
    and not re.search(r"(?im)^Depends[- ]on:", early_head_msg)
    and all_target_bazel_deps_exist_on_origin_main()
)
shared_worktree_tree = ""
if head_sha and (
    (composite_ref_name and composite_sha and not is_ancestor(composite_sha, head_sha))
    or (
        origin_main
        and head_sha != origin_main
        and merge_base_with_origin
        and merge_base_with_origin != origin_main
    )
):
    shared_worktree_tree = compute_worktree_tree_sha()

check_composite_stack_alignment(composite_ref_name, composite_sha, shared_worktree_tree)


# Parse `bazelRuleToGNTemplate` from `build/tools/bazel2gn/constants.go` into `{rule: gn_tmpl}`.
def _parse_bazel2gn_rule_map(go_src):
    if not go_src:
        return {}
    m = re.search(
        r"\bvar\s+bazelRuleToGNTemplate\s*=\s*map\[string\]string\s*\{([^}]*)\}",
        go_src,
        re.S,
    )
    body = m.group(1) if m else go_src
    out = {}
    for rm in re.finditer(
        r'"([A-Za-z0-9_]+)"\s*:\s*("([A-Za-z0-9_]+)"|noGNEquivalent|[A-Za-z0-9_]+)',
        body,
    ):
        out[rm.group(1)] = rm.group(3) or rm.group(2)
    return out


# Check whether a 1-based line in BUILD.bazel is preceded by a `# @bazel2gn:skip` comment.
def has_skip_above(lines, line_1based):
    idx = line_1based - 2
    while idx >= 0:
        s = lines[idx].strip() if idx < len(lines) else ""
        if not s:
            idx -= 1
            continue
        if s.startswith("#"):
            if "@bazel2gn:skip" in s:
                return True
            idx -= 1
            continue
        break
    return False


# Extract top-level rule calls `(rule_name, target_name, lineno, is_skipped)` from BUILD.bazel.
def _extract_bazel_rule_calls(bazel_text, filename_hint="<BUILD.bazel>"):
    if not (bazel_text or "").strip():
        return []
    try:
        tree_b = ast.parse(bazel_text, filename=filename_hint)
    except Exception:
        return []
    lines = bazel_text.splitlines()
    non_rule_calls = {
        "load",
        "package",
        "licenses",
        "exports_files",
        "rustc_check_cfg_build_flags",
    }
    calls = []
    for stmt in getattr(tree_b, "body", []):
        if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
            continue
        call = stmt.value
        if not isinstance(call.func, ast.Name):
            continue
        r_name = call.func.id
        if r_name in non_rule_calls:
            continue
        t_name = ""
        for kw in call.keywords:
            if (
                kw.arg == "name"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                t_name = kw.value.value
                break
        ln = getattr(stmt, "lineno", 1)
        calls.append((r_name, t_name, ln, has_skip_above(lines, ln)))
    return calls


# Load the active worktree's `bazelRuleToGNTemplate` map from `build/tools/bazel2gn`
# so unskipped rule calls in sentinel-synced BUILD.bazel files can be validated.
def _load_worktree_bazel2gn_rule_map():
    for rel_go in (
        "build/tools/bazel2gn/constants.go",
        "build/tools/bazel2gn/known_rules.go",
    ):
        go_path = os.path.join(workdir, rel_go)
        if os.path.isfile(go_path):
            try:
                with open(go_path, "r", encoding="utf-8") as gf:
                    parsed = _parse_bazel2gn_rule_map(gf.read())
                if parsed:
                    return parsed
            except OSError:
                pass
    return {}


worktree_b2gn_rule_map = _load_worktree_bazel2gn_rule_map()
reported_b2gn_rule_lines = set()

# 3. Check whether the change merges cleanly onto `origin/main` (`git merge-tree --write-tree`).
if origin_main and head_sha and head_sha != origin_main:
    merge_base = merge_base_with_origin or git_out(["merge-base", origin_main, head_sha])
    if merge_base and merge_base != origin_main:
        worktree_tree = shared_worktree_tree or compute_worktree_tree_sha()
        # Check whether upstream origin/main updated `build/tools/bazel2gn` rule mappings
        # relative to merge_base (adding support for skipped rules, or removing/renaming
        # or changing the GN template of an unskipped rule used in BUILD.bazel).
        b2gn_upstream_diff = git_lines(
            ["diff", "--name-only", merge_base, origin_main, "--", "build/tools/bazel2gn"]
        )
        if b2gn_upstream_diff:
            base_known = git_out(
                ["show", f"{merge_base}:build/tools/bazel2gn/constants.go"]
            ) or git_out(["show", f"{merge_base}:build/tools/bazel2gn/known_rules.go"])
            main_known = git_out(
                ["show", f"{origin_main}:build/tools/bazel2gn/constants.go"]
            ) or git_out(["show", f"{origin_main}:build/tools/bazel2gn/known_rules.go"])
            base_rule_map = _parse_bazel2gn_rule_map(base_known)
            main_rule_map = _parse_bazel2gn_rule_map(main_known)
            newly_supported_rules = {
                r
                for r, tmpl in main_rule_map.items()
                if tmpl != "noGNEquivalent"
                and base_rule_map.get(r, "noGNEquivalent") == "noGNEquivalent"
            }
            removed_or_changed_rules = {
                r
                for r, tmpl in base_rule_map.items()
                if r not in main_rule_map or main_rule_map[r] != tmpl
            }
            first_td_gn = ""
            matched_new_rules = []
            removed_hit = None
            for td in target_dirs:
                cand_gn = os.path.join(td, "BUILD.gn")
                cand_bz = os.path.join(td, "BUILD.bazel")
                if not (
                    os.path.isfile(os.path.join(workdir, cand_gn))
                    and os.path.isfile(os.path.join(workdir, cand_bz))
                ):
                    continue
                try:
                    with open(os.path.join(workdir, cand_bz), "r", encoding="utf-8") as bf:
                        bz_txt = bf.read()
                    with open(os.path.join(workdir, cand_gn), "r", encoding="utf-8") as gf:
                        gn_txt = gf.read()
                except OSError:
                    continue
                if newly_supported_rules and not first_td_gn:
                    used_new = sorted(
                        r
                        for r in newly_supported_rules
                        if re.search(r"\b" + re.escape(r) + r"\s*\(", bz_txt)
                    )
                    if used_new and ("@bazel2gn:skip" in bz_txt or not SENTINEL_RE.search(gn_txt)):
                        first_td_gn = cand_gn
                        matched_new_rules = used_new
                if main_rule_map and SENTINEL_RE.search(gn_txt) and removed_hit is None:
                    for r_name, _, r_ln, is_sk in _extract_bazel_rule_calls(bz_txt, cand_bz):
                        if not is_sk and (
                            r_name in removed_or_changed_rules or r_name not in main_rule_map
                        ):
                            removed_hit = (cand_bz, r_ln, r_name, td)
                            break
            if first_td_gn and matched_new_rules:
                rules_desc = ", ".join(f"`{r}`" for r in matched_new_rules)
                findings.append({
                    "source": "cq_reachability",
                    "category": "upstream_rebase_conflict",
                    "severity": "error",
                    "file": first_td_gn,
                    "line": 1,
                    "message": (
                        f"Checkout base {merge_base[:11]} is behind upstream `origin/main` "
                        f"({origin_main[:11]}), which adds `build/tools/bazel2gn` rule "
                        f"conversion support for {rules_desc} used in this package. "
                        "Migrating on a stale base leaves parallel hand-written target "
                        "definitions in `BUILD.gn`."
                    ),
                    "remediation": (
                        f"Rebase the checkout onto {rebase_target()[0]} before running "
                        "`fx bazel2gn`: commit or stash any worktree edits, run "
                        f"`{rebase_target()[1]}`, resolve any "
                        "conflicts keeping both upstream and migration changes, remove "
                        "obsolete `# @bazel2gn:skip` annotations and parallel `BUILD.gn` "
                        "definitions above `## BAZEL2GN SENTINEL`, and re-run `fx bazel2gn`."
                    ),
                })
            if removed_hit is not None:
                hit_bz, hit_ln, hit_rule, hit_td = removed_hit
                reported_b2gn_rule_lines.add((hit_bz, hit_ln))
                findings.append({
                    "source": "cq_reachability",
                    "category": "upstream_rebase_conflict",
                    "severity": "error",
                    "file": hit_bz,
                    "line": hit_ln,
                    "message": (
                        f"Checkout base {merge_base[:11]} is behind upstream `origin/main` "
                        f"({origin_main[:11]}), where `build/tools/bazel2gn/constants.go` "
                        f"removed, renamed, or changed the GN conversion of `{hit_rule}` "
                        f"used in '{hit_bz}', causing `verify_bazel2gn` in CQ to fail."
                    ),
                    "remediation": (
                        f"Rebase the checkout onto {rebase_target()[0]} (`{rebase_target()[1]}`), "
                        f"update '{hit_bz}' to load and call "
                        "the current rule in `build/tools/bazel2gn/constants.go`, and re-run "
                        f"`fx bazel2gn -d {hit_td}`."
                    ),
                })
        if worktree_tree:
            mt_proc = subprocess.run(
                [
                    "git",
                    "-C",
                    workdir,
                    "merge-tree",
                    "--write-tree",
                    "--name-only",
                    f"--merge-base={merge_base}",
                    origin_main,
                    worktree_tree,
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                check=False,
            )
            if mt_proc.returncode != 0:
                conflicted_files, conflict_msgs = parse_merge_tree_conflicts(
                    mt_proc.stdout
                )
                first_file = conflicted_files[0] if conflicted_files else ""
                files_desc = (
                    ", ".join(f"'{f}'" for f in conflicted_files)
                    if conflicted_files
                    else "modified files"
                )
                detail_desc = (
                    f" ({'; '.join(conflict_msgs)})" if conflict_msgs else ""
                )
                # Identify when merge conflicts come from stacked parent commits in merge_base..HEAD.
                stack_note = ""
                if has_stacked_ancestors and conflicted_files:
                    anc_files = set(
                        git_lines(["diff", "--name-only", f"{merge_base}..HEAD"])
                    )
                    anc_conf = [f for f in conflicted_files if f in anc_files]
                    if anc_conf:
                        anc_desc = ", ".join(f"'{f}'" for f in anc_conf)
                        stack_note = (
                            f" (originating in stacked prerequisite commit(s) "
                            f"`{merge_base[:11]}..HEAD` in {anc_desc})"
                        )
                msg_text = (
                    f"Change based on {merge_base[:11]} has a merge conflict "
                    f"with upstream `origin/main` ({origin_main[:11]}) in "
                    f"{files_desc}{stack_note}{detail_desc}, which causes Gerrit CQ "
                    "(`checkout|jiri patch`) to fail across all builders "
                    "with `Failed to rebase`."
                )
                rem_text = build_rebase_command_hint(
                    composite_ref_name,
                    has_stacked_ancestors,
                    drop_obsolete_ancestors,
                )
                findings.append({
                    "source": "cq_reachability",
                    "category": "upstream_rebase_conflict",
                    "severity": "error",
                    "file": first_file,
                    "line": 1,
                    "message": msg_text,
                    "remediation": rem_text,
                })
            else:
                mt_lines = [l.strip() for l in (mt_proc.stdout or "").splitlines() if l.strip()]
                merged_tree = (
                    mt_lines[0]
                    if mt_lines and re.fullmatch(r"[0-9a-f]{40}", mt_lines[0])
                    else ""
                )
                if merged_tree:
                    upstream_touched = set(
                        git_lines(["diff", "--name-only", merge_base, origin_main])
                    )
                    for rel_p in sorted(changed_files & upstream_touched):
                        if not (
                            rel_p.endswith((".gn", ".gni"))
                            or os.path.basename(rel_p) in ("BUILD.bazel", "BUILD")
                        ):
                            continue
                        merged_txt = git_out(["show", f"{merged_tree}:{rel_p}"])
                        if not merged_txt:
                            continue
                        if rel_p.endswith((".gn", ".gni")):
                            m_gn_dups = find_gn_duplicate_list_assignments(merged_txt)
                            if m_gn_dups and not worktree_gn_dups.get(rel_p):
                                var_name, prev_ln, cur_ln = m_gn_dups[0]
                                findings.append({
                                    "source": "cq_reachability",
                                    "category": "upstream_rebase_conflict",
                                    "severity": "error",
                                    "file": rel_p,
                                    "line": cur_ln,
                                    "message": (
                                        f"3-way merge of '{rel_p}' with upstream `origin/main` "
                                        f"({origin_main[:11]}) produces duplicate non-empty "
                                        f"`{var_name} = [...]` assignments (lines {prev_ln} and "
                                        f"{cur_ln}), which causes Gerrit CQ (`gn gen`) to fail "
                                        "with `Replacing nonempty list`."
                                    ),
                                    "remediation": (
                                        f"Rebase onto {rebase_target()[0]} (`{rebase_target()[1]}`) "
                                        "and deduplicate the "
                                        f"`{var_name} = [...]` assignment in '{rel_p}' so "
                                        f"`{var_name}` is assigned only once."
                                    ),
                                })
                        else:
                            m_bz_dups = find_bazel_duplicate_targets(merged_txt, rel_p)
                            if m_bz_dups and not worktree_bz_dups.get(rel_p):
                                t_name, rule_name, prev_ln, cur_ln = m_bz_dups[0]
                                findings.append({
                                    "source": "cq_reachability",
                                    "category": "upstream_rebase_conflict",
                                    "severity": "error",
                                    "file": rel_p,
                                    "line": cur_ln,
                                    "message": (
                                        f"3-way merge of '{rel_p}' with upstream `origin/main` "
                                        f"({origin_main[:11]}) produces duplicate "
                                        f"`{rule_name}(name = \"{t_name}\")` definitions "
                                        f"(lines {prev_ln} and {cur_ln})."
                                    ),
                                    "remediation": (
                                        f"Rebase onto {rebase_target()[0]} (`{rebase_target()[1]}`) "
                                        "and merge the duplicate "
                                        f"`{rule_name}(name = \"{t_name}\")` blocks in '{rel_p}' "
                                        "into a single target definition."
                                    ),
                                })
                    # Flag target directories where upstream origin/main modified BUILD or
                    # source files since merge_base, which leaves BUILD.bazel and bazel2gn
                    # output out of sync after a clean 3-way git merge.
                    for td in target_dirs:
                        td_upstream = sorted(
                            u for u in upstream_touched if u.startswith(td + "/")
                        )
                        if not td_upstream:
                            continue
                        td_bz = f"{td}/BUILD.bazel"
                        rep_file = (
                            td_bz
                            if os.path.isfile(os.path.join(workdir, td_bz))
                            else td_upstream[0]
                        )
                        touched_list = ", ".join(f"'{u}'" for u in td_upstream[:4])
                        findings.append({
                            "source": "cq_reachability",
                            "category": "upstream_rebase_conflict",
                            "severity": "error",
                            "file": rep_file,
                            "line": 1,
                            "message": (
                                f"Upstream `origin/main` ({origin_main[:11]}) modified "
                                f"{touched_list} in target package `//{td}` since checkout "
                                f"base {merge_base[:11]}. A 3-way git merge updates those "
                                f"files without updating '{td_bz}' or re-running "
                                "`fx bazel2gn`, leaving stale dependencies (such as unused "
                                "Rust crate dependencies) or `verify_bazel2gn` drift in CQ."
                            ),
                            "remediation": (
                                f"Rebase onto {rebase_target()[0]} (`{rebase_target()[1]}`), "
                                "update "
                                f"'{td_bz}' to match upstream's changes in `//{td}`, and "
                                f"re-run `fx bazel2gn -d {td}`."
                            ),
                        })

# Check whether all Bazel-Migration-Target entries and modified BUILD.bazel targets of a
# local ancestor commit have actually landed on origin/main since merge_base (and not just
# pre-existing exports_files / expect_includes stub files).
def _ancestor_targets_landed_on_main(anc, anc_msg, merge_base, origin_main, up_files):
    if composite_sha and is_ancestor(anc, composite_sha):
        return False
    mig_pairs = [
        (
            m.group(1).strip("/"),
            (m.group(2) or os.path.basename(m.group(1).strip("/"))).strip(),
        )
        for m in re.finditer(
            r"^Bazel-Migration-Target:\s*//([^:\s]+)(?::([^\s]+))?",
            anc_msg,
            re.M,
        )
        if m.group(1).strip("/")
    ]
    anc_bz_files = [
        f
        for f in git_lines(["diff-tree", "--no-commit-id", "--name-only", "-r", anc])
        if os.path.basename(f) in ("BUILD.bazel", "BUILD")
    ]
    if not mig_pairs and not anc_bz_files:
        return False
    added_any_target = False
    for rel_bz in anc_bz_files:
        if rel_bz not in up_files:
            return False
        anc_bz = git_out(["show", f"{anc}:{rel_bz}"])
        parent_anc_bz = git_out(["show", f"{anc}~1:{rel_bz}"])
        main_bz = git_out(["show", f"{origin_main}:{rel_bz}"])
        anc_tgts = extract_bazel_target_names(anc_bz, rel_bz)
        parent_anc_tgts = extract_bazel_target_names(parent_anc_bz, rel_bz)
        main_tgts = extract_bazel_target_names(main_bz, rel_bz)
        if not anc_tgts or not (anc_tgts <= main_tgts):
            return False
        if anc_tgts - parent_anc_tgts:
            added_any_target = True
    for pkg, tgt in mig_pairs:
        if f"{pkg}/BUILD.bazel" not in up_files and f"{pkg}/BUILD" not in up_files:
            return False
        if not _bazel_dep_target_exists_on_ref(origin_main, pkg, tgt):
            return False
        if not _bazel_dep_target_exists_on_ref(merge_base, pkg, tgt):
            added_any_target = True
        anc_bz = git_out(["show", f"{anc}:{pkg}/BUILD.bazel"]) or git_out(
            ["show", f"{anc}:{pkg}/BUILD"]
        )
        main_bz = git_out(["show", f"{origin_main}:{pkg}/BUILD.bazel"]) or git_out(
            ["show", f"{origin_main}:{pkg}/BUILD"]
        )
        anc_tgts = extract_bazel_target_names(anc_bz, f"{pkg}/BUILD.bazel")
        main_tgts = extract_bazel_target_names(main_bz, f"{pkg}/BUILD.bazel")
        if anc_tgts and not (anc_tgts <= main_tgts):
            return False
    return added_any_target


# Check whether HEAD is stacked on a local ancestor commit that has already landed on
# origin/main, or still carries a 'Depends on:' reference to a CL now in origin/main.
def check_landed_ancestor_commits(merge_base, origin_main, parent_sha, head_sha):
    # When HEAD is an ancestor of origin/main (merge_base == head_sha), there are no
    # local commits ahead of origin/main (e.g. uncommitted working-tree edits on top
    # of a base commit after origin/main advanced).
    if not merge_base or merge_base == head_sha:
        return
    landed_ancestors = []
    upstream_log = ""
    if merge_base != origin_main:
        upstream_log = git_out(
            ["log", "--format=%H%n%B%n---END---", f"{merge_base}..{origin_main}"]
        )
    if (
        parent_sha
        and merge_base != origin_main
        and merge_base != parent_sha
        and is_ancestor(merge_base, parent_sha)
    ):
        upstream_cids = set(
            re.findall(r"^Change-Id:\s*(I[0-9a-f]{40})\b", upstream_log, re.M)
        )
        upstream_files = set(git_lines(["diff", "--name-only", merge_base, origin_main]))
        cherry_lines = git_lines(["cherry", origin_main, parent_sha, merge_base])
        cherry_landed = {
            line[2:].strip() for line in cherry_lines if line.startswith("- ")
        }
        for anc in git_lines(["rev-list", f"{merge_base}..{parent_sha}"]):
            anc_msg = git_out(["log", "-1", "--format=%B", anc])
            anc_cids = set(
                re.findall(r"^Change-Id:\s*(I[0-9a-f]{40})\b", anc_msg, re.M)
            )
            anc_pkgs_on_main = _ancestor_targets_landed_on_main(
                anc, anc_msg, merge_base, origin_main, upstream_files
            )
            if anc in cherry_landed or (anc_cids & upstream_cids) or anc_pkgs_on_main:
                anc_subj = (
                    anc_msg.splitlines()[0] if anc_msg.splitlines() else anc[:11]
                )
                landed_ancestors.append((anc[:11], anc_subj))

    # Only inspect HEAD's commit message for 'Depends on:' when HEAD is a task commit.
    head_msg = (
        git_out(["log", "-1", "--format=%B", head_sha])
        if head_sha and head_is_task_commit
        else ""
    )
    if re.search(r"(?m)^Reviewed-on:\s+\S+", head_msg) and not head_touches_target:
        return
    head_cids = set(re.findall(r"^Change-Id:\s*(I[0-9a-f]{40})\b", head_msg, re.M))
    stale_depends_on = []
    unlanded_depends_on = []
    # Extract only explicit 'Depends on:' / 'Depends-on:' lines and bulleted/indented blocks
    # so Gerrit 'Reviewed-on:' footers or prose URLs in the commit body are not misread.
    dep_cont_re = (
        r"(?:[ \t]*[-*].*|[ \t]+\S.*"
        r"|(?:https?://|fxrev\.dev/|fxr/|I[0-9a-f]{40}\b).*)"
    )
    dep_section_re = re.compile(
        r"(?im)^Depends[- ]on:[ \t]*([^\n]*)"
        r"(?:\n((?:" + dep_cont_re + r"(?:\n|$))+))?"
    )
    dep_text = "\n".join(
        f"{m.group(1) or ''}\n{m.group(2) or ''}"
        for m in dep_section_re.finditer(head_msg)
    )
    dep_cl_re = (
        r"(?:fxrev\.dev|fxr|fuchsia-review\.googlesource\.com/c/fuchsia/\+)/(\d+)"
    )
    # Planter's uploader (`dependsOnBlock`) regenerates `- task <slug> (CL ...)` bullets from
    # `task.depends_on` on every upload even after the prerequisite task lands; ignore those
    # Planter-managed bullets (including 72-char wrapped continuation lines) for `stale_depends_on`
    # so post-ack uploads do not bounce.
    manual_dep_text = re.sub(
        r"(?im)^[ \t]*[-*][ \t]*task[ \t]+[A-Za-z0-9_.-]+[^\n]*(?:\n[ \t]+(?![-*]\s)[^\n]*)*",
        "",
        dep_text,
    )
    manual_cl_nums = set(re.findall(dep_cl_re, manual_dep_text))
    manual_cids = set(re.findall(r"\b(I[0-9a-f]{40})\b", manual_dep_text))
    for cl_num in re.findall(dep_cl_re, dep_text):
        rev_pat = r"Reviewed-on:[^\n]*/\+/" + re.escape(cl_num) + r"\b"
        ref = f"fxrev.dev/{cl_num}"
        if re.search(rev_pat, upstream_log) or git_out(
            [
                "log",
                "-1",
                "--since=90.days.ago",
                "--format=%H",
                f"--grep=Reviewed-on: .*/+/{cl_num}\\b",
                origin_main,
            ]
        ):
            if cl_num in manual_cl_nums and ref not in stale_depends_on:
                stale_depends_on.append(ref)
        elif ref not in unlanded_depends_on:
            unlanded_depends_on.append(ref)

    # Check Change-Id references in 'Depends on:' sections against origin/main.
    for cid in re.findall(r"\b(I[0-9a-f]{40})\b", dep_text):
        if cid in head_cids:
            continue
        landed_sha = git_out(
            [
                "log",
                "-1",
                "--since=90.days.ago",
                "--format=%H",
                f"--grep=^Change-Id: {cid}$",
                origin_main,
            ]
        )
        if landed_sha:
            if cid in manual_cids and cid not in stale_depends_on:
                stale_depends_on.append(cid)
        elif cid not in unlanded_depends_on:
            unlanded_depends_on.append(cid)

    # Check that unlanded 'Depends on:' Change-Ids are actually stacked in git ancestry.
    # Gerrit CQ/Tricium builds HEAD's git tree and ignores prose 'Depends on:' lines.
    stack_cids = set()
    if parent_sha and merge_base != parent_sha and is_ancestor(merge_base, parent_sha):
        stack_log = git_out(["log", "--format=%B", f"{merge_base}..{parent_sha}"])
        stack_cids = set(re.findall(r"^Change-Id:\s*(I[0-9a-f]{40})\b", stack_log, re.M))
    has_task_dep_slug = bool(re.search(r"(?i)\btask\s+[a-z0-9_-]+", dep_text))
    unstacked_prereqs = []
    for cid in unlanded_depends_on:
        if cid in stack_cids:
            continue
        local_prereq = git_out(
            ["log", "--all", "--reflog", "-1", "--format=%H", f"--grep=^Change-Id: {cid}$"]
        )
        if local_prereq or has_task_dep_slug:
            unstacked_prereqs.append((cid, local_prereq[:12] if local_prereq else ""))
    if unstacked_prereqs:
        prereq_desc = ", ".join(
            f"{cid} (local commit {sha})" if sha else cid for cid, sha in unstacked_prereqs
        )
        first_sha = next((sha for _, sha in unstacked_prereqs if sha), "<prereq_sha>")
        findings.append({
            "source": "cq_reachability",
            "category": "unstacked_depends_on_commit",
            "severity": "error",
            "file": "/COMMIT_MSG",
            "line": 1,
            "message": (
                f"Commit message lists unlanded `Depends on:` prerequisite(s) {prereq_desc}, "
                "but those commit(s) are missing from `HEAD`'s git ancestry above "
                "`origin/main`. Gerrit CQ/Tricium ignores prose `Depends on:` lines and "
                "tests `HEAD`'s git tree directly, failing with `no such package`."
            ),
            "remediation": (
                f"Restack `HEAD` on top of the prerequisite commit (`git rebase --onto "
                f"{first_sha} HEAD~1 HEAD`) so its `BUILD.bazel` is in `origin/main..HEAD`, "
                "or if the prerequisite CL has since landed, rebase onto "
                f"{rebase_target()[0]} (`{rebase_target()[1]}`) and remove `Depends on:`."
            ),
        })

    if landed_ancestors or stale_depends_on:
        anc_desc = ", ".join(f"{sha} ({subj!r})" for sha, subj in landed_ancestors)
        dep_desc = ", ".join(stale_depends_on)
        msg_parts = []
        if landed_ancestors:
            msg_parts.append(
                f"HEAD is stacked on local ancestor commit(s) {anc_desc} that have "
                f"already landed on `origin/main` ({origin_main[:11]}), leaving the "
                "branch diverged from `origin/main` on a closed prerequisite change."
            )
        if stale_depends_on:
            msg_parts.append(
                f"Commit message still lists `Depends on:` {dep_desc}, which has "
                "already landed on `origin/main`."
            )
        rem_parts = []
        if landed_ancestors:
            if drop_obsolete_ancestors:
                rem_parts.append(
                    "All in-tree Bazel dependencies of this package already exist on "
                    "`origin/main`: run `git fetch origin main && git rebase --onto "
                    "origin/main HEAD~1` (preserving the existing `Change-Id`) to "
                    "detach `HEAD` from the landed/superseded ancestor stack onto "
                    "`origin/main`, and revert any redundant edit to shared `.bzl` "
                    "rules under `build/bazel/rules/` (`git checkout origin/main -- "
                    "<file>`)."
                )
            else:
                rem_parts.append(
                    "Run `git fetch origin main && git rebase origin/main` so git drops the "
                    "already-landed prerequisite commit(s) and rebases onto `origin/main`."
                )
        if stale_depends_on:
            if unlanded_depends_on:
                keep_desc = ", ".join(unlanded_depends_on)
                rem_parts.append(
                    f"Remove the landed `Depends on:` entry/entries ({dep_desc}) from "
                    f"the commit message with `git commit --amend` while keeping still-"
                    f"unmerged `Depends on:` entry/entries ({keep_desc}), any local "
                    "unmerged prerequisite ancestor commit(s), and the existing "
                    "`Change-Id`."
                )
            else:
                rem_parts.append(
                    f"Remove the obsolete `Depends on:` block ({dep_desc}) from the "
                    "commit message with `git commit --amend` (preserving the existing "
                    "`Change-Id`)."
                )
        findings.append({
            "source": "cq_reachability",
            "category": "stale_landed_parent_commit",
            "severity": "error",
            "file": "/COMMIT_MSG",
            "line": 1,
            "message": " ".join(msg_parts),
            "remediation": " ".join(rem_parts),
        })


# Check that in-tree Bazel dependency packages exist on disk and on origin/main when standalone.
def is_non_dependency_bazel_label(val):
    if (
        not val.startswith("//")
        or val == "//..."
        or val.startswith("-//")
        or val.startswith(
            (
                "//build",
                "//tools",
                "//conditions:",
                "//visibility:",
                "//command_line_option:",
            )
        )
        or val.endswith((":__pkg__", ":__subpackages__", "/..."))
        or bool(re.search(r"\s", val))
    ):
        return True
    return False


# Collect AST node IDs belonging to non-dependency attributes or package_group specifications.
def collect_non_dep_ast_node_ids(tree):
    non_dep_attrs = (
        "visibility",
        "default_visibility",
        "packages",
        "includes",
        "copts",
        "cxxopts",
        "conlyopts",
        "linkopts",
        "rustc_flags",
        "args",
        "cmd",
        "cmd_bash",
        # bazel2gn copies public_configs to GN; its labels need no Bazel package.
        "public_configs",
    )
    node_ids = set()
    for call_node in ast.walk(tree):
        if isinstance(call_node, ast.Call):
            if (
                isinstance(call_node.func, ast.Name)
                and call_node.func.id == "package_group"
            ):
                for sub in ast.walk(call_node):
                    node_ids.add(id(sub))
                continue
            for kw in call_node.keywords:
                if kw.arg in non_dep_attrs:
                    for sub in ast.walk(kw.value):
                        node_ids.add(id(sub))
    return node_ids


_REAL_WORKDIR = os.path.realpath(workdir)
_NESTED_REPO_PKG_CACHE = {}


# Check whether a package directory resides in a nested jiri git repository or submodule
# outside the root checkout repository (so root-repo `git cat-file -e origin/main:<pkg>/...`
# does not resolve files inside that sub-repository).
def is_nested_git_repo_pkg(pkg):
    clean_pkg = (pkg or "").strip("/")
    if not clean_pkg:
        return False
    if clean_pkg in _NESTED_REPO_PKG_CACHE:
        return _NESTED_REPO_PKG_CACHE[clean_pkg]
    abs_dir = os.path.join(workdir, clean_pkg)
    if not os.path.isdir(abs_dir):
        _NESTED_REPO_PKG_CACHE[clean_pkg] = False
        return False
    try:
        top = subprocess.check_output(
            ["git", "-C", abs_dir, "rev-parse", "--show-toplevel"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
        is_nested = bool(top and os.path.realpath(top) != _REAL_WORKDIR)
    except Exception:
        is_nested = False
    _NESTED_REPO_PKG_CACHE[clean_pkg] = is_nested
    return is_nested


def check_unlanded_bazel_dependencies(origin_main, parent_sha):
    if not origin_main or not head_is_task_commit:
        return
    is_standalone_on_origin = parent_sha == origin_main or is_ancestor(
        parent_sha, origin_main
    )
    missing_packages = set()
    checked_targets = set()
    for td in target_dirs:
        bazel_rel = os.path.join(td, "BUILD.bazel")
        bazel_full = os.path.join(workdir, bazel_rel)
        if not os.path.isfile(bazel_full):
            continue
        try:
            with open(bazel_full, "r", encoding="utf-8") as f:
                bazel_src = f.read()
            tree = ast.parse(bazel_src, filename=bazel_rel)
        except Exception:
            continue

        non_dep_nodes = collect_non_dep_ast_node_ids(tree)

        for node in ast.walk(tree):
            if id(node) in non_dep_nodes:
                continue
            if not isinstance(node, ast.Constant) or not isinstance(node.value, str):
                continue
            val = node.value.strip()
            if is_non_dependency_bazel_label(val):
                continue
            parts = split_label(val, td)
            if not parts:
                continue
            dep_pkg, dep_tgt_name = parts[0], (parts[1] if len(parts) > 1 else "")
            if (
                not dep_pkg
                or dep_pkg in ("conditions", "visibility", "command_line_option")
                or dep_pkg == td
                or dep_pkg in target_dirs
                or dep_pkg in missing_packages
                or (dep_pkg, dep_tgt_name) in checked_targets
            ):
                continue
            checked_targets.add((dep_pkg, dep_tgt_name))

            # Check if this in-tree package exists on disk
            dep_bazel_rel = os.path.join(dep_pkg, "BUILD.bazel")
            dep_build_rel = os.path.join(dep_pkg, "BUILD")
            on_disk = os.path.isfile(
                os.path.join(workdir, dep_bazel_rel)
            ) or os.path.isfile(os.path.join(workdir, dep_build_rel))
            if not on_disk:
                missing_packages.add(dep_pkg)
                findings.append({
                    "source": "cq_reachability",
                    "category": "missing_bazel_dependency_package",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": getattr(node, "lineno", 1),
                    "message": (
                        f"Target in '{bazel_rel}' depends on '{val}', but package "
                        f"directory '{dep_pkg}' does not contain a BUILD.bazel or "
                        "BUILD file."
                    ),
                    "remediation": (
                        f"Fix the dependency label or create the missing "
                        f"'{dep_bazel_rel}' file."
                    ),
                })
                continue

            # Skip dependency packages added/modified in HEAD (co-migrated in this CL)
            # or committed in a stacked ancestor when not standalone on origin/main.
            co_migrated_files = (
                set(head_commit_files)
                if (head_touches_target and not wt_touches_target)
                else changed_files
            )
            dep_in_head = (
                dep_bazel_rel in co_migrated_files
                or dep_build_rel in co_migrated_files
                or (
                    not is_standalone_on_origin
                    and (
                        git_success(["cat-file", "-e", f"{head_sha}:{dep_bazel_rel}"])
                        or git_success(["cat-file", "-e", f"{head_sha}:{dep_build_rel}"])
                    )
                )
            )
            if dep_in_head:
                continue

            # Skip dependency packages residing in a nested jiri git repository /
            # submodule outside the root repository.
            if is_nested_git_repo_pkg(dep_pkg):
                continue

            # Check if this in-tree package and target exist on origin/main
            dep_on_origin = _bazel_dep_target_exists_on_ref(
                origin_main, dep_pkg, dep_tgt_name
            )
            if dep_on_origin:
                continue

            # If it does not exist on origin/main and HEAD is parented on origin/main:
            # Pushing standalone to refs/for/main will fail Tricium.
            if is_standalone_on_origin:
                findings.append({
                    "source": "cq_reachability",
                    "category": "unlanded_bazel_dependency_package",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": getattr(node, "lineno", 1),
                    "message": (
                        f"Target in '{bazel_rel}' depends on in-tree package "
                        f"'//{dep_pkg}', but '{dep_bazel_rel}' does not exist on "
                        "'origin/main', and HEAD is parented directly on origin/main. "
                        "Uploading or dry-running this CL standalone will fail Tricium "
                        f"with 'ERROR: no such package \"{dep_pkg}\"'. Wait for the "
                        f"prerequisite CL migrating '//{dep_pkg}' to land on "
                        "origin/main, or stack this change on top of it."
                    ),
                    "remediation": (
                        f"Ensure the prerequisite migration CL for '//{dep_pkg}' lands "
                        "on 'origin/main' before pushing or running CQ dry runs "
                        "standalone. If developing stacked changes, ensure this commit "
                        "is stacked on top of the prerequisite commit and includes "
                        "'Depends on:' in the commit message."
                    ),
                })


if origin_main and head_is_task_commit:
    check_landed_ancestor_commits(
        git_out(["merge-base", origin_main, head_sha]),
        origin_main,
        parent_sha,
        head_sha,
    )
    check_unlanded_bazel_dependencies(origin_main, parent_sha)


# Check that every main-repo Bazel package referenced (directly or transitively)
# in BUILD.bazel dependency attributes has a BUILD.bazel or BUILD file in the
# checkout, and note when a dependency package is added or updated by an unmerged
# stacked parent commit above origin/main (using 3-dot diff against the task's
# parent commit so stacked ancestor commits are distinguished from HEAD even when
# PLANTER_CHANGE_BASE points to the base of the stack).
task_parent_ref = (
    "HEAD~1"
    if (
        parent_sha
        and head_touches_target
        and not (wt_touches_target and not head_touches_target)
    )
    else (
        "HEAD"
        if (wt_touches_target and not head_touches_target and head_sha)
        else effective_diff_base
    )
)
has_stacked_ancestors_for_task = bool(
    origin_main and task_parent_ref and not is_ancestor(task_parent_ref, origin_main)
)
task_changed_files = (
    (set(head_commit_files) | set(wt_changed_files))
    if task_parent_ref == "HEAD~1"
    else set(changed_files)
)
stacked_parent_files = (
    set(git_lines(["diff", "--name-only", f"{origin_main}...{task_parent_ref}"]))
    if has_stacked_ancestors_for_task
    else set()
)
head_commit_msg = git_out(["log", "-1", "--format=%B", "HEAD"]) if head_sha else ""


# Flag prerequisite BUILD.bazel files outside target_dirs whose targets were dropped during rebase.
def check_prerequisite_bazel_target_loss(parent_ref):
    if not parent_ref or not stacked_parent_files:
        return
    target_set = set(target_dirs)
    for rel_p in sorted(task_changed_files & stacked_parent_files):
        if os.path.basename(rel_p) not in ("BUILD.bazel", "BUILD"):
            continue
        pkg_dir = os.path.dirname(rel_p).strip("/")
        if pkg_dir in target_set:
            continue
        parent_txt = git_out(["show", f"{parent_ref}:{rel_p}"])
        if not parent_txt:
            continue
        abs_p = os.path.join(workdir, rel_p)
        if not os.path.isfile(abs_p):
            continue
        try:
            with open(abs_p, "r", encoding="utf-8", errors="replace") as fh:
                cur_txt = fh.read()
        except OSError:
            continue
        parent_targets = extract_bazel_target_names(parent_txt, rel_p)
        cur_targets = extract_bazel_target_names(cur_txt, rel_p)
        dropped = sorted(parent_targets - cur_targets)
        if not dropped:
            continue
        dropped_desc = ", ".join(f"`:{t}`" for t in dropped[:5])
        msg_text = (
            f"Prerequisite package '{rel_p}' (provided by a stacked parent commit) "
            f"lost rule target(s) {dropped_desc} when resolving an `add/add` or "
            "rebase conflict."
        )
        rem_text = (
            f"Restore the prerequisite commit's version of '{rel_p}' with "
            f"`git checkout {parent_ref} -- {rel_p}`, drop duplicate target "
            "definitions from this commit, and re-apply only the minimal caller "
            "additions (such as `visibility` or `exports_files(...)`) needed by "
            "this task."
        )
        findings.append({
            "source": "cq_reachability",
            "category": "prerequisite_bazel_targets_overwritten",
            "severity": "error",
            "file": rel_p,
            "line": 1,
            "message": msg_text,
            "remediation": rem_text,
        })


check_prerequisite_bazel_target_loss(task_parent_ref)

# Dependency-bearing Starlark attributes whose cross-package main-repo labels (`//<pkg>:...`)
# must resolve to an existing `<pkg>/BUILD.bazel` or `<pkg>/BUILD` file in the checkout.
DEP_ATTRS = {
    "actual",
    "args_deps",
    "deps",
    "implementation_deps",
    "link_deps",
    "non_test_deps",
    "plugin_deps",
    "test_deps",
    "proc_macro_deps",
    "compile_data",
    "config_data",
    "data",
    "test_data",
    "tools",
    "unit_tests",
    "binary",
    "crate",
    "package",
    "compiled_manifest",
    "manifest",
    "includes",
    "tests",
    "lint_config",
    "components",
    "test_components",
    "subpackages",
    "resources",
    "target_compatible_with",
    "exec_compatible_with",
}


def has_bazel_pkg_in_worktree(pkg):
    if not pkg:
        return True
    pkg_dir = os.path.join(workdir, pkg)
    for bfile in ("BUILD.bazel", "BUILD"):
        if os.path.isfile(os.path.join(pkg_dir, bfile)):
            return True
    return False


_PKG_AT_REF_CACHE = {}


def has_bazel_pkg_at_ref(pkg, ref):
    if not pkg or not ref:
        return True
    key = (pkg, ref)
    if key in _PKG_AT_REF_CACHE:
        return _PKG_AT_REF_CACHE[key]
    res = False
    for bfile in ("BUILD.bazel", "BUILD"):
        if git_success(["cat-file", "-e", f"{ref}:{pkg}/{bfile}"]):
            res = True
            break
    _PKG_AT_REF_CACHE[key] = res
    return res


def iter_dep_label_strings(val_node, var_defs=None, seen_vars=None):
    if isinstance(val_node, ast.Constant) and isinstance(val_node.value, str):
        yield val_node.value, getattr(val_node, "lineno", 1)
    elif isinstance(val_node, (ast.List, ast.Tuple)):
        for elt in val_node.elts:
            yield from iter_dep_label_strings(elt, var_defs, seen_vars)
    elif isinstance(val_node, ast.BinOp) and isinstance(val_node.op, ast.Add):
        yield from iter_dep_label_strings(val_node.left, var_defs, seen_vars)
        yield from iter_dep_label_strings(val_node.right, var_defs, seen_vars)
    elif isinstance(val_node, ast.Call):
        for arg in val_node.args:
            if isinstance(arg, ast.Dict):
                for k in arg.keys:
                    if (
                        isinstance(k, ast.Constant)
                        and isinstance(k.value, str)
                        and not k.value.startswith("//conditions:")
                    ):
                        yield k.value, getattr(k, "lineno", 1)
                for v in arg.values:
                    yield from iter_dep_label_strings(v, var_defs, seen_vars)
    elif isinstance(val_node, ast.Name) and var_defs and val_node.id in var_defs:
        seen = seen_vars or set()
        if val_node.id not in seen:
            for item, _ in iter_dep_label_strings(
                var_defs[val_node.id], var_defs, seen | {val_node.id}
            ):
                yield item, getattr(val_node, "lineno", 1)


def iter_pkg_dep_labels(cur_tree):
    var_defs = {}
    for stmt in getattr(cur_tree, "body", []):
        if isinstance(stmt, ast.Assign):
            for t in stmt.targets:
                if isinstance(t, ast.Name):
                    var_defs[t.id] = stmt.value
    for node in ast.walk(cur_tree):
        if not isinstance(node, ast.Call):
            continue
        if isinstance(node.func, ast.Name) and node.func.id == "package_group":
            continue
        if (
            isinstance(node.func, ast.Name)
            and node.func.id == "load"
            and node.args
            and isinstance(node.args[0], ast.Constant)
            and isinstance(node.args[0].value, str)
        ):
            yield node.args[0].value, getattr(node.args[0], "lineno", 1)
            continue
        for kw in node.keywords:
            if kw.arg in DEP_ATTRS:
                yield from iter_dep_label_strings(kw.value, var_defs)


# Check whether a cross-package reference to `dep_pkg` in a non-target package `pkg_d`
# was already present on `origin/main` (or the change base when `origin/main` is absent),
# such as pre-existing `test_deps` in an already-migrated upstream library touched only
# to update `visibility` or `HOST_OS_CONSTRAINTS`.
def is_preexisting_non_target_dep(pkg_d, dep_pkg):
    if not pkg_d or pkg_d in target_dirs:
        return False
    ref_src = (
        git_out(["show", f"{origin_main}:{pkg_d}/BUILD.bazel"])
        if origin_main
        else base_bazel_src_of(pkg_d)
    )
    if not ref_src:
        return False
    return bool(
        re.search(r"//" + re.escape(dep_pkg) + r"(?![A-Za-z0-9_/-])", ref_src)
    )


# Check whether a missing .bzl load label in a non-target package was already present
# in a stacked prerequisite commit above origin/main at change_base or task_parent_ref.
def is_preexisting_stacked_parent_bzl_load(cur_rel, raw_label):
    cur_pkg = os.path.dirname(cur_rel).strip("/")
    if not cur_pkg or cur_pkg in target_dirs or is_nested_git_repo_pkg(cur_pkg):
        return False
    in_parent = cur_rel in stacked_parent_files or (
        has_stacked_ancestors_for_task
        and not git_success(["cat-file", "-e", f"{origin_main}:{cur_rel}"])
    )
    if not in_parent:
        return False
    base_src = base_bazel_src_of(cur_pkg)
    if base_src and raw_label in base_src:
        return True
    parent_src = (
        git_out(["show", f"{task_parent_ref}:{cur_rel}"]) if task_parent_ref else ""
    )
    return bool(parent_src and raw_label in parent_src)


# Verify that a loaded .bzl file exists in the checkout and note when it comes
# from a stacked parent commit above origin/main.
def check_bzl_load_target(
    cur_rel, report_file, report_ln, raw_label, dep_pkg, dep_name, reported_bzl
):
    if not dep_name.endswith(".bzl"):
        return
    bzl_rel = f"{dep_pkg}/{dep_name}" if dep_pkg else dep_name
    if bzl_rel in reported_bzl:
        return
    if not os.path.isfile(os.path.join(workdir, bzl_rel)):
        reported_bzl.add(bzl_rel)
        if is_preexisting_stacked_parent_bzl_load(cur_rel, raw_label):
            findings.append({
                "source": "cq_reachability",
                "category": "stacked_bazel_dependency_package",
                "severity": "INFO",
                "file": report_file,
                "line": report_ln,
                "message": (
                    f"Load statement `{raw_label}` in '{cur_rel}' comes from an "
                    "unlanded stacked parent commit above `origin/main` whose "
                    f"Starlark file '{bzl_rel}' does not exist in the current "
                    "checkout."
                ),
                "remediation": (
                    f"Do not rewrite unrelated rules in '{cur_rel}' inside this leaf "
                    "CL (update the prerequisite CL separately so downstream "
                    "cherry-picks apply cleanly), keep any collateral edits in "
                    f"'{cur_rel}' minimal, and set `disputed_findings` to `[]`."
                ),
            })
            return
        findings.append({
            "source": "cq_reachability",
            "category": "missing_bazel_bzl_load_file",
            "severity": "error",
            "file": report_file,
            "line": report_ln,
            "message": (
                f"Load statement `{raw_label}` in '{cur_rel}' references Starlark "
                f"file '{bzl_rel}', which does not exist in the checkout, causing "
                "Bazel package loading in CQ to fail with "
                f"`cannot load '{raw_label}': no such file`."
            ),
            "remediation": (
                f"Load an existing Starlark `.bzl` file under `//{dep_pkg}` or "
                f"restore the stacked prerequisite commit that provides '{bzl_rel}' "
                "(and any corresponding `build/tools/bazel2gn` rule conversion) in "
                "the git parent ancestry before `HEAD`."
            ),
        })
        return
    if is_nested_git_repo_pkg(dep_pkg):
        return
    # Flag loaded Starlark `.bzl` files from a stale merge-base that were deleted or
    # renamed on upstream `origin/main`.
    if (
        origin_main
        and merge_base_with_origin
        and merge_base_with_origin != origin_main
        and bzl_rel not in task_changed_files
        and bzl_rel not in stacked_parent_files
        and git_success(["cat-file", "-e", f"{merge_base_with_origin}:{bzl_rel}"])
        and not git_success(["cat-file", "-e", f"{origin_main}:{bzl_rel}"])
    ):
        reported_bzl.add(bzl_rel)
        findings.append({
            "source": "cq_reachability",
            "category": "upstream_rebase_conflict",
            "severity": "error",
            "file": report_file,
            "line": report_ln,
            "message": (
                f"Load statement `{raw_label}` in '{cur_rel}' references Starlark "
                f"file '{bzl_rel}' from stale base {merge_base_with_origin[:11]}, "
                f"which was deleted or renamed on upstream `origin/main` "
                f"({origin_main[:11]})."
            ),
            "remediation": (
                f"Rebase onto {rebase_target()[0]} (`{rebase_target()[1]}`), "
                f"update `load(\"{raw_label}\", ...)` and "
                f"its rule calls in '{cur_rel}' to use the current `.bzl` rule under "
                f"`//{dep_pkg}`, and re-run `fx bazel2gn`."
            ),
        })
        return
    in_stacked_bzl = bzl_rel in stacked_parent_files or (
        has_stacked_ancestors_for_task
        and not git_success(["cat-file", "-e", f"{origin_main}:{bzl_rel}"])
    )
    if origin_main and bzl_rel not in task_changed_files and in_stacked_bzl:
        reported_bzl.add(bzl_rel)
        findings.append({
            "source": "cq_reachability",
            "category": "stacked_bazel_dependency_package",
            "severity": "INFO",
            "file": report_file,
            "line": report_ln,
            "message": (
                f"Load statement `{raw_label}` in '{cur_rel}' references Starlark "
                f"file '{bzl_rel}', which is provided by a stacked parent commit "
                "above `origin/main` rather than `origin/main` itself."
            ),
            "remediation": (
                f"Keep the stacked parent commit that provides '{bzl_rel}' (and "
                "any accompanying `build/tools/bazel2gn` rule support) in the git "
                "ancestry between `origin/main` and `HEAD` when rebasing (never "
                "drop parent commits with `git rebase --onto origin/main HEAD~1` "
                "or fold shared rule/bazel2gn changes into the package migration "
                "commit)."
            ),
        })


def check_missing_bazel_dep_packages(bazel_rel, td, tree):
    reported_pkgs = set()
    reported_bzl = set()
    visited_pkgs = {td}
    queue = [(bazel_rel, td, tree, None)]
    while queue:
        cur_rel, cur_pkg, cur_tree, root_line = queue.pop(0)
        for raw_label, ln in iter_pkg_dep_labels(cur_tree):
            parsed = split_label(raw_label, cur_pkg)
            if not parsed:
                continue
            dep_pkg, dep_name = parsed
            if not dep_pkg or dep_pkg == cur_pkg:
                continue
            report_ln = root_line if root_line is not None else ln
            check_bzl_load_target(
                cur_rel, bazel_rel, report_ln, raw_label, dep_pkg, dep_name, reported_bzl
            )
            if not has_bazel_pkg_in_worktree(dep_pkg):
                if is_preexisting_non_target_dep(cur_pkg, dep_pkg):
                    continue
                if dep_pkg not in reported_pkgs:
                    reported_pkgs.add(dep_pkg)
                    msg = (
                        f"Dependency `{raw_label}` in '{cur_rel}' references Bazel "
                        f"package `//{dep_pkg}`, but neither '{dep_pkg}/BUILD.bazel' "
                        f"nor '{dep_pkg}/BUILD' exists in the checkout, causing CQ "
                        "`tests.json` generation (`bazel query`) to fail with "
                        f"`no such package '{dep_pkg}': BUILD file not found`."
                    )
                    if dep_pkg in target_dirs:
                        rem = (
                            f"`//{dep_pkg}` is one of this task's assigned target "
                            "directories (`$PLANTER_TARGET_DIRS`), but "
                            f"'{dep_pkg}/BUILD.bazel' is missing from the worktree "
                            "(for example because a parallel worker did not write or "
                            "merge its edits before the integration round). Author "
                            f"'{dep_pkg}/BUILD.bazel' and update '{dep_pkg}/BUILD.gn' "
                            f"(running `fx bazel2gn -d {dep_pkg}` if dual-built) "
                            "directly in this round; never report "
                            "`missing_bazel_dependency_package` in `disputed_findings`."
                        )
                    else:
                        prereq_sha = ""
                        for cid in re.findall(r"\b(I[0-9a-f]{40})\b", head_commit_msg):
                            cand_sha = git_out(
                                [
                                    "log",
                                    "--all",
                                    "--reflog",
                                    "-1",
                                    "--format=%H",
                                    f"--grep=^Change-Id: {cid}$",
                                ]
                            )
                            if cand_sha and has_bazel_pkg_at_ref(dep_pkg, cand_sha):
                                prereq_sha = cand_sha[:12]
                                break
                        stack_hint = (
                            f"restack `HEAD` onto local prerequisite commit `{prereq_sha}` "
                            f"(`git rebase --onto {prereq_sha} HEAD~1 HEAD`) or "
                            if prereq_sha
                            else ""
                        )
                        rem = (
                            f"Ensure `//{dep_pkg}` is present before uploading: {stack_hint}"
                            "rebase onto `origin/main` if its CL landed, co-migrate "
                            f"`//{dep_pkg}` in the integration/main pass if it is a small "
                            "same-area helper within CL scoping limits (`<= 5` dirs, "
                            "`<= ~500` lines), otherwise create a minimal header-only / "
                            f"file-export bridge stub '{dep_pkg}/BUILD.bazel' (no compiled "
                            f"`srcs`; leaving '{dep_pkg}/BUILD.gn' "
                            "completely untouched) whose rule target(s) each carry an "
                            "explanatory comment above `# @bazel2gn:skip` (e.g. `# Bridge "
                            "stub for Bazel callers; bazel2gn skips it and BUILD.gn is "
                            "unchanged.`, not \"kept/remains in GN\" wording, which "
                            "stale_gn_retention_comment_in_bazel rejects), export any "
                            "required public `hdrs`/`includes` (and `exports_files(...)` "
                            "if headers in subdirs are read across packages), and grant "
                            "`visibility` to the caller package; if no bridge stub is "
                            "viable, revert the blocked `BUILD.bazel` / `BUILD.gn` edits "
                            "and report the blocker with `disputed_findings: []` (never "
                            "dispute `missing_bazel_dependency_package`). Never leave a "
                            "dangling label to a directory without `BUILD.bazel`."
                        )
                    findings.append({
                        "source": "cq_reachability",
                        "category": "missing_bazel_dependency_package",
                        "severity": "error",
                        "file": bazel_rel,
                        "line": report_ln,
                        "message": msg,
                        "remediation": rem,
                    })
                continue
            # Skip nested git repositories (e.g. jiri subprojects under third_party/)
            # that are tracked outside the main repository's HEAD commit tree.
            if is_nested_git_repo_pkg(dep_pkg):
                continue
            # Verify that committed task changes do not rely on untracked/uncommitted
            # dependency BUILD.bazel files (direct or transitive) missing from HEAD's tree.
            if (
                head_is_task_commit
                and not wt_touches_target
                and head_sha
                and not has_bazel_pkg_at_ref(dep_pkg, head_sha)
            ):
                if dep_pkg not in reported_pkgs:
                    reported_pkgs.add(dep_pkg)
                    msg = (
                        f"Dependency `{raw_label}` in '{cur_rel}' references Bazel "
                        f"package `//{dep_pkg}`, which exists in the working tree but "
                        "is missing from `HEAD`'s committed git tree, causing Gerrit "
                        "CQ/Tricium (`bazel query` for `tests.json`) to fail with "
                        f"`no such package '{dep_pkg}': BUILD file not found`."
                    )
                    rem = (
                        f"Commit '{dep_pkg}/BUILD.bazel' if co-migrating `//{dep_pkg}` "
                        "in scope, restack `HEAD` onto the prerequisite commit that "
                        f"adds '{dep_pkg}/BUILD.bazel', or rebase onto `origin/main` "
                        "once the prerequisite CL lands."
                    )
                    findings.append({
                        "source": "cq_reachability",
                        "category": "unlanded_bazel_dependency_package",
                        "severity": "error",
                        "file": bazel_rel,
                        "line": report_ln,
                        "message": msg,
                        "remediation": rem,
                    })
                continue
            in_stacked_parent = (
                f"{dep_pkg}/BUILD.bazel" in stacked_parent_files
                or f"{dep_pkg}/BUILD" in stacked_parent_files
                or (
                    has_stacked_ancestors_for_task
                    and (not head_sha or has_bazel_pkg_at_ref(dep_pkg, head_sha))
                    and not has_bazel_pkg_at_ref(dep_pkg, origin_main)
                )
            )
            if (
                origin_main
                and f"{dep_pkg}/BUILD.bazel" not in task_changed_files
                and f"{dep_pkg}/BUILD" not in task_changed_files
                and in_stacked_parent
                and dep_pkg not in reported_pkgs
            ):
                reported_pkgs.add(dep_pkg)
                msg = (
                    f"Dependency `{raw_label}` in '{cur_rel}' references Bazel "
                    f"package `//{dep_pkg}`, whose `BUILD.bazel` is provided by a "
                    "stacked parent commit above `origin/main` rather than "
                    "`origin/main` itself."
                )
                rem = (
                    f"Keep the stacked parent commit that provides '{dep_pkg}/BUILD.bazel' "
                    "in the git ancestry between `origin/main` and `HEAD` (when rebasing, "
                    "rebase the full stack rather than dropping parent commits with "
                    "`git rebase --onto origin/main HEAD~1`; if a prior CQ run failed "
                    f"with `no such package '{dep_pkg}'` on a pre-rebase or unstacked "
                    f"patchset before the parent commit was re-cherry-picked into "
                    "`origin/main..HEAD`, do not duplicate or fold "
                    f"'{dep_pkg}/BUILD.bazel' into `HEAD` or dispute `cq_reachability` "
                    "in `disputed_findings`—instead, fix any remaining check findings on "
                    "`HEAD`, run `run_checks.sh`, and report (with `disputed_findings: []` "
                    "unless `run_checks.sh` identifies an active verified `review_diff` "
                    "false positive) so the properly stacked patchset is uploaded)."
                )
                findings.append({
                    "source": "cq_reachability",
                    "category": "stacked_bazel_dependency_package",
                    "severity": "INFO",
                    "file": bazel_rel,
                    "line": report_ln,
                    "message": msg,
                    "remediation": rem,
                })
            elif (
                origin_main
                and f"{dep_pkg}/BUILD.bazel" not in task_changed_files
                and f"{dep_pkg}/BUILD" not in task_changed_files
                and not in_stacked_parent
                and dep_pkg not in reported_pkgs
                and head_commit_msg
                and (
                    re.search(
                        r"(?<![A-Za-z0-9_-])"
                        + re.escape(dep_pkg.replace("/", "-").replace("_", "-"))
                        + r"(?![A-Za-z0-9_/-])",
                        head_commit_msg.replace("_", "-"),
                    )
                    or re.search(
                        r"(?<![A-Za-z0-9_])"
                        + re.escape(
                            re.sub(
                                r"^(?:src|sdk|build|tools|zircon|third_party)/",
                                "",
                                dep_pkg,
                            )
                            .replace("/", "-")
                            .replace("_", "-")
                        )
                        + r"(?![A-Za-z0-9_/-])",
                        head_commit_msg.replace("_", "-"),
                    )
                    or re.search(
                        r"//" + re.escape(dep_pkg) + r"(?![A-Za-z0-9_/-])",
                        head_commit_msg,
                    )
                )
            ):
                reported_pkgs.add(dep_pkg)
                msg = (
                    f"Dependency `{raw_label}` in '{cur_rel}' references Bazel "
                    f"package `//{dep_pkg}`, which is listed in `Depends on:` and "
                    "has now landed in `origin/main` and `HEAD~1`."
                )
                rem = (
                    f"'{dep_pkg}/BUILD.bazel' is now present in `HEAD~1` after rebasing onto "
                    "`origin/main` (resolving any earlier CQ `no such package` failure from "
                    "a pre-rebase patchset); remove any obsolete manually written landed "
                    "`Depends on:` entry with `git commit --amend` (Planter-managed "
                    "`- task <slug>` bullets are regenerated on upload and may remain), "
                    "run `run_checks.sh` once, and report (with `disputed_findings: []` "
                    "unless `run_checks.sh` identifies an active verified `review_diff` "
                    "false positive) so the rebased patchset is uploaded."
                )
                findings.append({
                    "source": "cq_reachability",
                    "category": "rebased_bazel_dependency_package",
                    "severity": "INFO",
                    "file": bazel_rel,
                    "line": report_ln,
                    "message": msg,
                    "remediation": rem,
                })
            if dep_pkg not in visited_pkgs and (
                f"{dep_pkg}/BUILD.bazel" in task_changed_files
                or f"{dep_pkg}/BUILD" in task_changed_files
                or in_stacked_parent
            ):
                visited_pkgs.add(dep_pkg)
                for bfile in ("BUILD.bazel", "BUILD"):
                    dep_rel = os.path.join(dep_pkg, bfile)
                    dep_full = os.path.join(workdir, dep_rel)
                    if os.path.isfile(dep_full):
                        try:
                            with open(dep_full, "r", encoding="utf-8") as df:
                                dep_tree = ast.parse(df.read(), filename=dep_rel)
                            queue.append((dep_rel, dep_pkg, dep_tree, report_ln))
                        except Exception:
                            pass
                        break


# Verify that any modified non-target BUILD.bazel (such as an ancestor test_suite
# BUILD.bazel) does not reference a non-existent Bazel package in the worktree.
def check_modified_ancestor_bazel_dep_packages():
    target_set = set(target_dirs)
    for rel_f in sorted(task_changed_files):
        if os.path.basename(rel_f) not in ("BUILD.bazel", "BUILD"):
            continue
        pkg_d = os.path.dirname(rel_f).strip("/")
        if pkg_d in target_set:
            continue
        full_f = os.path.join(workdir, rel_f)
        if not os.path.isfile(full_f):
            continue
        try:
            with open(full_f, "r", encoding="utf-8") as f:
                anc_tree = ast.parse(f.read(), filename=rel_f)
        except Exception:
            continue
        seen_missing = set()
        seen_bzl = set()
        for raw_label, ln in iter_pkg_dep_labels(anc_tree):
            parsed = split_label(raw_label, pkg_d)
            if not parsed:
                continue
            dep_pkg, dep_name = parsed
            if not dep_pkg or dep_pkg == pkg_d:
                continue
            check_bzl_load_target(
                rel_f, rel_f, ln, raw_label, dep_pkg, dep_name, seen_bzl
            )
            if (
                dep_pkg in seen_missing
                or has_bazel_pkg_in_worktree(dep_pkg)
                or is_preexisting_non_target_dep(pkg_d, dep_pkg)
            ):
                continue
            seen_missing.add(dep_pkg)
            findings.append({
                "source": "cq_reachability",
                "category": "missing_bazel_dependency_package",
                "severity": "error",
                "file": rel_f,
                "line": ln,
                "message": (
                    f"Dependency `{raw_label}` in '{rel_f}' references Bazel "
                    f"package `//{dep_pkg}`, but neither '{dep_pkg}/BUILD.bazel' "
                    f"nor '{dep_pkg}/BUILD' exists in the checkout, causing CQ "
                    "`tests.json` generation (`bazel query`) to fail with "
                    f"`no such package '{dep_pkg}': BUILD file not found`."
                ),
                "remediation": (
                    f"Remove the stale `{raw_label}` entry from '{rel_f}', or "
                    f"ensure '{dep_pkg}/BUILD.bazel' is present in the checkout "
                    "(or in a stacked parent commit before `HEAD`)."
                ),
            })


check_modified_ancestor_bazel_dep_packages()


# Check that component manifest includes (and other `*.shard` labels) name a target the
# owning package declares. A package that exists but lacks the cml_shard passes the package
# check above, yet `fx bazel build` fails analysis with `no such target '<pkg>:<x>.shard'`.
def iter_shard_labels(tree):
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        for kw in node.keywords:
            if kw.arg not in DEP_ATTRS:
                continue
            for raw_label, ln in iter_dep_label_strings(kw.value):
                name_part = raw_label.rpartition(":")[2]
                if kw.arg == "includes" or name_part.endswith(".shard"):
                    yield raw_label, ln


def bazel_pkg_declares(pkg, name):
    """True when <pkg>/BUILD(.bazel) quotes "<name>" (target or exports_files) or it is a file."""
    if os.path.isfile(os.path.join(workdir, pkg, name)):
        return True
    for bfile in ("BUILD.bazel", "BUILD"):
        full = os.path.join(workdir, pkg, bfile)
        if os.path.isfile(full):
            try:
                with open(full, "r", encoding="utf-8") as bf:
                    return f'"{name}"' in bf.read()
            except Exception:
                return True
    return True  # A missing package is reported as missing_bazel_dependency_package.


def check_missing_shard_targets(bazel_rel, td, tree):
    reported = set()
    for raw_label, ln in iter_shard_labels(tree):
        parsed = split_label(raw_label, td)
        if not parsed or not parsed[0] or not parsed[1]:
            continue
        dep_pkg, name = parsed
        if (dep_pkg, name) in reported or bazel_pkg_declares(dep_pkg, name):
            continue
        reported.add((dep_pkg, name))
        shard_src = f"{name}.cml" if name.endswith(".shard") else "<stem>.shard.cml"
        msg = (
            f"`{raw_label}` in '{bazel_rel}' names target `{name}`, which "
            f"'{dep_pkg}/BUILD.bazel' does not declare, so `fx bazel build` fails analysis "
            f"with `no such target '//{dep_pkg}:{name}'`."
        )
        rem = (
            f"Add `cml_shard(name = \"{name}\", shard = \"{shard_src}\", "
            "includes = [<cml_shard targets of the shards it includes>], "
            "visibility = [...])` (load from "
            "`//build/bazel/rules/components:cml_shard.bzl`) to "
            f"'{dep_pkg}/BUILD.bazel' as part of this change, the same way a dependency "
            "without a Bazel target is migrated. If another agent owns that file in this "
            "task, ask for that exact edit and keep the label; do not drop the include."
        )
        findings.append({
            "source": "cq_reachability",
            "category": "missing_bazel_dependency_target",
            "severity": "error",
            "file": bazel_rel,
            "line": ln,
            "message": msg,
            "remediation": rem,
        })


for _td in target_dirs:
    _bz_rel = os.path.join(_td, "BUILD.bazel")
    try:
        with open(os.path.join(workdir, _bz_rel), "r", encoding="utf-8") as _f:
            check_missing_shard_targets(_bz_rel, _td, ast.parse(_f.read(), filename=_bz_rel))
    except Exception:
        pass

# Read active build directory `args.gn` (if present) so GN targets referenced in
# build arguments (e.g. `target_labels`, `host_labels`, `developer_test_labels`)
# are recognized as reachable roots in `gn gen`.
def read_active_args_gn_code():
    bdirs = []
    env_bdir = os.environ.get("FUCHSIA_BUILD_DIR", "").strip()
    if env_bdir:
        bdirs.append(env_bdir)
    fx_bdir_file = os.path.join(workdir, ".fx-build-dir")
    if os.path.isfile(fx_bdir_file):
        try:
            with open(fx_bdir_file, "r", encoding="utf-8") as f:
                val = f.read().strip()
            if val:
                bdirs.append(val)
        except OSError:
            pass
    chunks = []
    seen_paths = set()
    for bdir in bdirs:
        full_bdir = bdir if os.path.isabs(bdir) else os.path.join(workdir, bdir)
        args_path = os.path.abspath(os.path.join(full_bdir, "args.gn"))
        if args_path in seen_paths or not os.path.isfile(args_path):
            continue
        seen_paths.add(args_path)
        try:
            with open(args_path, "r", encoding="utf-8") as f:
                chunks.append(gn_code(f.read()))
        except OSError:
            pass
    return "\n".join(chunks)


# Helpers for auditing ancestor GN umbrella `ffx_plugin(plugin_deps = [...])` `group("tests")`
# blocks when child plugins with unit tests migrate to Bazel (`fx bazel2gn` emits `ffx_plugin`
# without `with_unit_tests = true`, so `:<umbrella>_tests` stops aggregating migrated child tests).
STALE_ALL_PLUGINS_COMMENT_RE = re.compile(
    r"(?i)\b(?:aggregates?|includes?|collects?|covers?|runs?)\b[^.;]*"
    r"\b(?:for\s+|of\s+|from\s+|across\s+)?all\s+(?:the\s+)?(?:ffx\s+)?"
    r"(?:sub-?)?(?:plugins?|subcommands?|commands?|`?plugin_deps`?)\b"
)


def _resolve_gn_label(raw_lbl, cur_dir):
    lbl = raw_lbl.split("(", 1)[0].strip()
    if not lbl:
        return None, None
    if lbl.startswith("//"):
        path_part, sep, name_part = lbl[2:].partition(":")
        pkg = path_part.strip("/")
        return pkg, (name_part if sep else os.path.basename(pkg))
    if lbl.startswith(":"):
        return cur_dir, lbl[1:]
    path_part, sep, name_part = lbl.partition(":")
    pkg = os.path.normpath(os.path.join(cur_dir, path_part)).strip("/")
    if pkg == ".":
        pkg = ""
    return pkg, (name_part if sep else os.path.basename(pkg))


def _bazel_ffx_plugin_names(bz_tree):
    names = []
    for node in ast.walk(bz_tree):
        if not isinstance(node, ast.Call):
            continue
        r_nm = (
            node.func.id
            if isinstance(node.func, ast.Name)
            else (node.func.attr if isinstance(node.func, ast.Attribute) else "")
        )
        if r_nm != "ffx_plugin":
            continue
        for kw in node.keywords:
            if (
                kw.arg == "name"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                names.append(kw.value.value)
    return sorted(set(names))


def _gn_plugin_deps_reach(start_dir, start_plugin, target_pkg, target_plugins):
    queue = [(start_dir, start_plugin)]
    visited = set(queue)
    while queue:
        cur_dir, cur_plug = queue.pop(0)
        cur_rel = os.path.join(cur_dir, "BUILD.gn") if cur_dir else "BUILD.gn"
        cur_code = read_gn(cur_rel)
        if not cur_code:
            continue
        plug_body = gn_block(cur_code, "ffx_plugin", cur_plug)
        if not plug_body:
            continue
        for m_pd in re.finditer(r"\bplugin_deps\s*\+?=\s*\[", plug_body):
            open_b = m_pd.end() - 1
            close_b = closing_bracket(plug_body, open_b)
            for raw_dep in re.findall(r'"([^"]+)"', plug_body[open_b + 1 : close_b]):
                dep_pkg, dep_name = _resolve_gn_label(raw_dep, cur_dir)
                if not dep_pkg or not dep_name:
                    continue
                if dep_pkg == target_pkg and dep_name in target_plugins:
                    return True
                nxt = (dep_pkg, dep_name)
                if nxt in visited:
                    continue
                dep_code = read_gn(os.path.join(dep_pkg, "BUILD.gn"))
                if (
                    dep_code
                    and gn_block(dep_code, "ffx_plugin", dep_name)
                    and not gn_block(dep_code, "group", "tests")
                ):
                    visited.add(nxt)
                    queue.append(nxt)
    return False


def _find_umbrella_ffx_tests_owner(td_pkg, plugin_names):
    anc = os.path.dirname(td_pkg)
    while anc:
        anc_gn_rel = os.path.join(anc, "BUILD.gn")
        anc_full = os.path.join(workdir, anc_gn_rel)
        if os.path.isfile(anc_full):
            try:
                with open(anc_full, "r", encoding="utf-8") as af:
                    anc_raw = af.read()
            except OSError:
                anc_raw = ""
            anc_clean = gn_code(anc_raw)
            if re.search(r'\bgroup\(\s*"tests"\s*\)\s*\{', anc_clean):
                for m_fp in re.finditer(
                    r'\bffx_plugin\(\s*"([^"]+)"\s*\)\s*\{', anc_clean
                ):
                    umb_name = m_fp.group(1)
                    if _gn_plugin_deps_reach(anc, umb_name, td_pkg, set(plugin_names)):
                        return (anc, anc_gn_rel, anc_raw, anc_clean, umb_name)
        next_anc = os.path.dirname(anc)
        if next_anc == anc:
            break
        anc = next_anc
    return None


def _parse_gn_list_sections(list_raw, first_line_no):
    sections = []
    cur = {"first_line": None, "comments": [], "items": []}
    for offset, raw_line in enumerate(list_raw.splitlines()):
        ln = first_line_no + offset
        stripped = raw_line.strip()
        if not stripped:
            if cur["comments"] or cur["items"]:
                sections.append(cur)
                cur = {"first_line": None, "comments": [], "items": []}
            continue
        if cur["first_line"] is None:
            cur["first_line"] = ln
        if stripped.startswith("#"):
            cur["comments"].append(stripped.lstrip("#").strip())
        else:
            for s_val in re.findall(r'"([^"]+)"', gn_code(raw_line)):
                cur["items"].append((s_val, ln))
    if cur["comments"] or cur["items"]:
        sections.append(cur)
    return sections


def _check_umbrella_tests_stale_comment(
    anc_gn_rel, raw_grp, body_start_line, umb_name, seen_keys
):
    blocks = []
    cur_start = None
    cur_lines = []
    for idx, r_line in enumerate(raw_grp.splitlines()):
        s_line = r_line.strip()
        if s_line.startswith("#"):
            if cur_start is None:
                cur_start = body_start_line + idx
            cur_lines.append(s_line.lstrip("#").strip())
        elif cur_lines:
            blocks.append((cur_start, " ".join(cur_lines)))
            cur_start = None
            cur_lines = []
    if cur_lines:
        blocks.append((cur_start, " ".join(cur_lines)))
    for c_line, joined in blocks:
        if not STALE_ALL_PLUGINS_COMMENT_RE.search(joined):
            continue
        if re.search(r"(?i)\b(?:unmigrated|non-migrated|except)\b", joined):
            continue
        dedup_key = ("stale_comment", anc_gn_rel, c_line)
        if dedup_key in seen_keys:
            continue
        seen_keys.add(dedup_key)
        msg_text = (
            f"Comment in `group(\"tests\")` of '{anc_gn_rel}' at line {c_line} still "
            f"claims `:{umb_name}_tests` aggregates tests for all plugins in "
            "`plugin_deps`, which ceases to be true once child plugins migrate to "
            "Bazel (`fx bazel2gn` emits `ffx_plugin` without `with_unit_tests = true`)."
        )
        rem_text = (
            f"Update the comment above `\":{umb_name}_tests\"` in '{anc_gn_rel}' to "
            "clarify that it aggregates `<subdir>:<plugin_name>_tests` only for "
            "unmigrated plugins in `plugin_deps`, and list Bazel-migrated plugin "
            "`:tests` targets in a separate commented section below."
        )
        findings.append({
            "source": "cq_reachability",
            "category": "stale_plugin_tests_aggregation_comment",
            "severity": "error",
            "file": anc_gn_rel,
            "line": c_line,
            "message": msg_text,
            "remediation": rem_text,
        })


def _check_umbrella_tests_migrated_section(
    anc, anc_gn_rel, anc_raw, clean_grp, raw_grp, body_start, umb_name, td_pkg, valid_names, seen_keys
):
    rel_td = os.path.relpath(td_pkg, anc)
    for m_list in re.finditer(r"\b(?:public_)?deps\s*\+?=\s*\[", clean_grp):
        open_b = m_list.end() - 1
        close_b = closing_bracket(clean_grp, open_b)
        first_line_no = anc_raw.count("\n", 0, body_start + open_b + 1) + 1
        sections = _parse_gn_list_sections(raw_grp[open_b + 1 : close_b], first_line_no)
        list_has_umbrella = any(
            _resolve_gn_label(lbl, anc) == (anc, f"{umb_name}_tests")
            for sec in sections
            for lbl, _ in sec["items"]
        )
        if not list_has_umbrella:
            continue
        for sec in sections:
            td_matches = [
                (lbl, ln)
                for lbl, ln in sec["items"]
                if _resolve_gn_label(lbl, anc)[0] == td_pkg
                and _resolve_gn_label(lbl, anc)[1] in valid_names
            ]
            if not td_matches:
                continue
            mixed_with_umbrella = any(
                _resolve_gn_label(lbl, anc) == (anc, f"{umb_name}_tests")
                for lbl, _ in sec["items"]
            )
            sec_comment = " ".join(sec["comments"])
            has_migrated_comment = bool(
                re.search(r"(?i)\b(?:bazel|migrated)\b", sec_comment)
            )
            unmigrated_in_sec = []
            if not mixed_with_umbrella:
                for lbl, _ in sec["items"]:
                    i_pkg, _ = _resolve_gn_label(lbl, anc)
                    if i_pkg and (
                        (i_pkg == anc and not has_bazel_pkg_in_worktree(anc))
                        or not has_bazel_pkg_in_worktree(i_pkg)
                    ):
                        unmigrated_in_sec.append(lbl)
            if not (mixed_with_umbrella or not has_migrated_comment or unmigrated_in_sec):
                continue
            dedup_key = ("ungrouped_section", anc_gn_rel, sec["first_line"])
            if dedup_key in seen_keys:
                continue
            seen_keys.add(dedup_key)
            report_ln = td_matches[0][1]
            detail = (
                f"mixes unmigrated target(s) ({', '.join(f'`{u}`' for u in unmigrated_in_sec)}) "
                "into the Bazel-migrated plugins section"
                if unmigrated_in_sec
                else f"lists `{td_matches[0][0]}` without a separate commented section "
                f"from `\":{umb_name}_tests\"`"
            )
            msg_text = (
                f"`group(\"tests\")` in '{anc_gn_rel}' {detail}; migrated `ffx_plugin` "
                f"targets define standalone unit tests no longer aggregated by "
                f"`:{umb_name}_tests` and belong in their own commented section."
            )
            rem_text = (
                f"In `group(\"tests\")` of '{anc_gn_rel}', keep `\":{umb_name}_tests\"` "
                "(and any unmigrated test deps) in the first `deps` section, then add a "
                "blank line and a `# Plugins migrated to Bazel define standalone unit "
                f"test targets that are not aggregated by `:{umb_name}_tests`.` comment "
                f"header before sorted Bazel-migrated plugin `:tests` entries such as "
                f"`\"{rel_td}:tests\"`."
            )
            findings.append({
                "source": "cq_reachability",
                "category": "ungrouped_migrated_plugin_tests",
                "severity": "error",
                "file": anc_gn_rel,
                "line": report_ln,
                "message": msg_text,
                "remediation": rem_text,
            })


# Return the external repositories that keep a package's Bazel tests out of Bazel test
# suites: third_party_target_map.json modules whose upstream third_party/<repo>/src
# BUILD.bazel omits `includes` for angled `#include <prefix/...>` headers used by the
# package's C/C++ sources, and (for device tests) Bzlmod modules that cannot build for
# Fuchsia (exporting such a test into a test_suite fails `fx build` inside the upstream repo
# while the bazel2gn-generated GN test runs in CQ). Shared with migration_sanity.
def _external_repo_test_blockers(pkg_dir, bz_src, bz_tree, device=True):
    return pcl.external_repo_test_blockers(
        workdir,
        pkg_dir,
        bz_src,
        [raw_lbl for raw_lbl, _ in iter_pkg_dep_labels(bz_tree)],
        device=device,
    )


active_args_gn_code = read_active_args_gn_code()
unwired_reported = set()  # (gn_rel, suite) already reported as unwired
umbrella_plugin_findings_seen = set()
for td in target_dirs:
    bazel_rel = os.path.join(td, "BUILD.bazel")
    bazel_full = os.path.join(workdir, bazel_rel)
    gn_rel = os.path.join(td, "BUILD.gn")
    gn_full = os.path.join(workdir, gn_rel)

    if not os.path.isfile(bazel_full):
        continue

    try:
        with open(bazel_full, "r", encoding="utf-8") as f:
            bazel_src = f.read()
        tree = ast.parse(bazel_src, filename=bazel_rel)
    except Exception:
        continue

    check_missing_bazel_dep_packages(bazel_rel, td, tree)

    gn_src = ""
    if os.path.isfile(gn_full):
        try:
            with open(gn_full, "r", encoding="utf-8") as f:
                gn_src = f.read()
        except Exception:
            gn_src = ""

    # Dual-built packages blocked by an upstream external repository keep running their
    # tests via the generated GN copy: skip fx_test packaging/preservation requirements and
    # handle Bazel test_suite export specially below. A blocked package that is not
    # dual-built is reported by migration_sanity unbuildable_external_dep_device_test.
    ext_blockers_by_kind = {
        "target": _external_repo_test_blockers(td, bazel_src, tree, device=True),
        "host": _external_repo_test_blockers(td, bazel_src, tree, device=False),
    }
    ext_suite_blocked = bool(SENTINEL_RE.search(gn_src) and ext_blockers_by_kind["target"])

    # Verify that every unskipped top-level rule call in a sentinel-synced BUILD.bazel
    # is known to `build/tools/bazel2gn/constants.go` (`bazelRuleToGNTemplate`).
    if SENTINEL_RE.search(gn_src) and worktree_b2gn_rule_map:
        for r_name, r_tgt, r_ln, is_sk in _extract_bazel_rule_calls(bazel_src, bazel_rel):
            if (
                not is_sk
                and r_name not in worktree_b2gn_rule_map
                and (bazel_rel, r_ln) not in reported_b2gn_rule_lines
            ):
                tgt_desc = (
                    f"`{r_name}(name = \"{r_tgt}\")`" if r_tgt else f"`{r_name}(...)`"
                )
                msg_unk = (
                    f"{tgt_desc} in '{bazel_rel}' is not preceded by `# @bazel2gn:skip`, "
                    f"and `{r_name}` is not a known Bazel rule in "
                    "`build/tools/bazel2gn/constants.go` (`bazelRuleToGNTemplate`), so "
                    f"`verify_bazel2gn` fails with `{r_name} is not a known Bazel rule "
                    "to convert to GN`."
                )
                rem_unk = (
                    f"Replace `{r_name}` in '{bazel_rel}' with the supported Bazel rule "
                    "mapped in `build/tools/bazel2gn/constants.go` (for example, "
                    "`fx_cc_test_binary` from `//build/bazel/rules/cc:fx_cc_test_binary.bzl` "
                    f"for `cc_test_executable`) and re-run `fx bazel2gn -d {td}`, or add a "
                    "justified `# @bazel2gn:skip` annotation if the target is Bazel-only."
                )
                findings.append({
                    "source": "cq_reachability",
                    "category": "unknown_bazel2gn_rule",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": r_ln,
                    "message": msg_unk,
                    "remediation": rem_unk,
                })

    has_explicit_rustc_test = any(
        isinstance(n, ast.Call)
        and (
            (isinstance(n.func, ast.Name) and n.func.id in ("rustc_test", "host_rustc_test"))
            or (isinstance(n.func, ast.Attribute) and n.func.attr in ("rustc_test", "host_rustc_test"))
        )
        for n in ast.walk(tree)
    )
    base_gn_ref = change_base if change_base != "HEAD" else "HEAD~1"
    base_gn_src = git_out(["show", f"{base_gn_ref}:{gn_rel}"]) if gn_rel else ""

    test_targets = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        rule_name = ""
        if isinstance(node.func, ast.Name):
            rule_name = node.func.id
        elif isinstance(node.func, ast.Attribute):
            rule_name = node.func.attr
        kws = {kw.arg: kw.value for kw in node.keywords if kw.arg}
        t_name = ""
        call_lineno = getattr(node, "lineno", 1)
        lineno = call_lineno
        for kw in node.keywords:
            if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                t_name = kw.value.value
                lineno = kw.lineno
        if not t_name:
            continue
        if rule_name in TEST_RULES and "manual" not in bazel_strings(kws.get("tags")):
            test_targets.append((t_name, rule_name, lineno, [call_lineno]))
        elif rule_name in RUST_TEST_OWNERS and unit_test_envs(kws) and not has_explicit_rustc_test:
            unit_t_name = t_name + "_test"
            has_gn_tests_group = bool(
                re.search(r'\bgroup\(\s*"tests"\s*\)|\bbazel_test_suite\(', gn_src)
                or re.search(r'":' + re.escape(unit_t_name) + r'(?:\(|")', base_gn_src)
            )
            if has_gn_tests_group:
                for kw in node.keywords:
                    if kw.arg in ("with_unit_tests", "with_host_unit_tests"):
                        test_targets.append(
                            (unit_t_name, f"{rule_name}({kw.arg})", kw.lineno, [call_lineno, kw.lineno])
                        )
                        break

    parent_gn = os.path.join(workdir, os.path.dirname(td), "BUILD.gn")
    parent_gn_src = ""
    if os.path.isfile(parent_gn):
        try:
            with open(parent_gn, "r", encoding="utf-8") as f:
                parent_gn_src = f.read()
        except Exception:
            parent_gn_src = ""

    def strip_bazel_test_suites(code):
        out = list(code)
        for m in SUITE_RE.finditer(code):
            end = closing(code, m.end() - 1)
            for idx in range(m.start(), min(end + 1, len(out))):
                if out[idx] != "\n":
                    out[idx] = " "
        return "".join(out)

    # Return sorted ancestor GN package directories whose BUILD.gn references pkg_dir:target_name
    # directly outside bazel_test_suite(...).
    # Visibility a test wired into an ancestor test_suite needs: only the package of that
    # test_suite. GN files that list the test (group("tests") in an ancestor BUILD.gn) refer to
    # the GN target, and bazel_test_suite builds the Bazel test as a top-level target, so neither
    # is a Bazel rdep; naming their packages would conflict with visibility_audit
    # (false_positive_rdep_visibility).
    def ancestor_suite_visibility(anc_pkg):
        return [f"//{anc_pkg}:__pkg__"]

    def direct_gn_test_callers(pkg_dir, target_name):
        callers = []
        anc = os.path.dirname(pkg_dir)
        while True:
            anc_rel = os.path.join(anc, "BUILD.gn") if anc else "BUILD.gn"
            anc_code = strip_bazel_test_suites(read_gn(anc_rel))
            if anc_code:
                rel_from_anc = os.path.relpath(pkg_dir, anc) if anc else pkg_dir
                label_re = (
                    r'"(?://'
                    + re.escape(pkg_dir)
                    + r"|"
                    + re.escape(rel_from_anc)
                    + r"):"
                    + re.escape(target_name)
                    + r'(?:\(|")'
                )
                if target_name == os.path.basename(pkg_dir):
                    label_re += (
                        r'|"(?://'
                        + re.escape(pkg_dir)
                        + r"|"
                        + re.escape(rel_from_anc)
                        + r')(?:\(|")'
                    )
                if re.search(label_re, anc_code):
                    callers.append(anc)
            if not anc:
                break
            anc = os.path.dirname(anc)
        return sorted(set(callers))

    # Check whether group("tests") in pkg_dir/BUILD.gn is referenced by any ancestor BUILD.gn
    # or any other .gn/.gni file in the checkout.
    def gn_tests_group_is_referenced(pkg_dir):
        if direct_gn_test_callers(pkg_dir, "tests"):
            return True
        abs_tests_re = re.compile(
            r'"//'
            + re.escape(pkg_dir)
            + (
                r'(?::tests)?(?:\([^"]*\))?"'
                if os.path.basename(pkg_dir) == "tests"
                else r':tests(?:\([^"]*\))?"'
            )
        )
        # Check whether group("tests") in pkg_dir/BUILD.gn is referenced by any ancestor BUILD.gn,
        # active build-directory args.gn, or any other .gn/.gni file in the checkout.
        if active_args_gn_code and abs_tests_re.search(active_args_gn_code):
            return True
        for cand_rel in git_lines(
            [
                "grep",
                "-l",
                "--fixed-strings",
                f"//{pkg_dir}",
                "--",
                "*.gn",
                "*.gni",
            ]
        ):
            if os.path.dirname(cand_rel).strip("/") == pkg_dir:
                continue
            if abs_tests_re.search(read_gn(cand_rel)):
                return True
        return False

    # Check whether test target t_name of pkg_dir is run by a GN bazel_test_suite that is itself
    # wired into a root test group (directly, or via Bazel test_suite targets such as an
    # ancestor's aggregate `host_tests` suite exported by `bazel_test_suite("bazel_tests")`).
    # This covers Bazel-only host tests (e.g. host tools bridged to GN with bazel_host_tool()
    # instead of bazel2gn), which intentionally have no GN test target or group("tests").
    def wired_via_bazel_test_suite(pkg_dir, t_name):
        local_suites = bazel_suites_of(pkg_dir)

        def expand(name, seen):
            if name in seen:
                return set()
            seen.add(name)
            if name not in local_suites:
                return {name}
            if not local_suites[name]:
                return {t_name}
            out = set()
            for m in local_suites[name]:
                ln = local_name(m, pkg_dir)
                if ln is not None:
                    out |= expand(ln, seen)
            return out

        for s_rel, s_name, _, _, labels in bazel_test_suites_for(pkg_dir):
            if not suite_is_wired(s_rel, s_name):
                continue
            for label in labels:
                m = label_target(label, pkg_dir)
                if m is not None:
                    found = {m}
                elif re.match(r"^@{0,2}//", label.strip()):
                    found = foreign_suite_tests(label, pkg_dir, pkg_dir, set())
                else:
                    continue
                names = set()
                for f_name in found:
                    names |= expand(f_name, set())
                if t_name in names:
                    return True
        return False

    bazel_lines = bazel_src.splitlines()
    base_gn_clean = gn_code(base_gn_src) if base_gn_src else ""

    # Check whether a test target ran on host in base BUILD.gn (`:<name>($host_toolchain)`).
    def ran_on_host_in_base_gn(t_name):
        if not base_gn_clean:
            return False
        return bool(
            re.search(
                r'":' + re.escape(t_name) + r'\(\$host_toolchain\)"',
                base_gn_clean,
            )
        )

    # Check whether any ancestor BUILD.gn references pkg_dir:tests($host_toolchain).
    def ancestor_gn_calls_host_tests_group(pkg_dir):
        if not pkg_dir:
            return False
        anc = os.path.dirname(pkg_dir)
        while True:
            anc_rel = os.path.join(anc, "BUILD.gn") if anc else "BUILD.gn"
            anc_code = strip_bazel_test_suites(read_gn(anc_rel))
            if anc_code:
                rel_from_anc = os.path.relpath(pkg_dir, anc) if anc else pkg_dir
                pat = (
                    r'"(?://'
                    + re.escape(pkg_dir)
                    + r"|"
                    + re.escape(rel_from_anc)
                    + r'):tests\(\$host_toolchain\)"'
                )
                if os.path.basename(pkg_dir) == "tests":
                    pat += (
                        r'|"(?://'
                        + re.escape(pkg_dir)
                        + r"|"
                        + re.escape(rel_from_anc)
                        + r')\(\$host_toolchain\)"'
                    )
                if re.search(pat, anc_code):
                    return True
            if not anc:
                break
            anc = os.path.dirname(anc)
        return False

    # Check whether character offset `pos` in `code` lies inside an `if (is_host)` block.
    def _pos_in_is_host_block(code, pos):
        for mh in re.finditer(r"\bif\s*\(\s*is_host\b[^{]*\{", code):
            brace_i = mh.end() - 1
            end_i = closing(code, brace_i)
            if brace_i < pos < end_i:
                return True
        return False

    # Check whether a base GN `with_unit_tests = true` target (`:<ut_name>`) was
    # decomposed into a standalone `rustc_test("<ut_name>")` kept above
    # `## BAZEL2GN SENTINEL` in BUILD.gn and remains wired into GN tests.
    def decomposed_unit_test_kept_above_sentinel(ut_name):
        if not gn_src:
            return False
        m_sent_ut = SENTINEL_RE.search(gn_src)
        if not m_sent_ut:
            return False
        gn_pre_clean = strip_bazel_test_suites(gn_code(gn_src[: m_sent_ut.start()]))
        if not re.search(
            r'\brustc_test\(\s*"' + re.escape(ut_name) + r'"\s*\)\s*\{',
            gn_pre_clean,
        ):
            return False
        host_ref_pat = (
            r'"(?://' + re.escape(td) + r')?:' + re.escape(ut_name) + r'\(\$host_toolchain\)"'
            if td
            else r'":' + re.escape(ut_name) + r'\(\$host_toolchain\)"'
        )
        if (
            ran_on_host_in_base_gn(ut_name)
            and not re.search(host_ref_pat, gn_pre_clean)
            and not direct_gn_test_callers(td, ut_name)
        ):
            return False
        if direct_gn_test_callers(td, ut_name):
            return True
        blocks = {}
        for mb in re.finditer(r'\b[A-Za-z_]\w*\(\s*"([^"]+)"\s*\)\s*\{', gn_pre_clean):
            b_name = mb.group(1)
            b_body = gn_pre_clean[mb.end() : closing(gn_pre_clean, mb.end() - 1)]
            blocks[b_name] = blocks.get(b_name, "") + "\n" + b_body
        queue = ["tests"] if "tests" in blocks else []
        visited = set(queue)
        while queue:
            cur_body = blocks[queue.pop(0)]
            if refs_local(cur_body, td, ut_name):
                return True
            for cand_name in blocks:
                if cand_name not in visited and refs_local(cur_body, td, cand_name):
                    visited.add(cand_name)
                    queue.append(cand_name)
        return False

    # Verify that any host-only GN unit test (`rustc_test` or `go_test`) kept above
    # `## BAZEL2GN SENTINEL` in BUILD.gn is explicitly qualified with `($host_toolchain)`
    # in `group("tests")` so static parity checks (`review_diff/unit_test_parity`) do not
    # treat the group("tests") dependency as build-only.
    def check_kept_pre_sentinel_host_tests():
        if not gn_src:
            return
        m_sent_ht = SENTINEL_RE.search(gn_src)
        if not m_sent_ht:
            return
        gn_pre_clean = strip_bazel_test_suites(gn_code(gn_src[: m_sent_ht.start()]))
        file_is_host = bool(
            re.search(r"\bassert\s*\(\s*is_host\b", gn_pre_clean)
            or ancestor_gn_calls_host_tests_group(td)
        )
        if not file_is_host and not re.search(r"\bif\s*\(\s*is_host\b", gn_pre_clean):
            return
        bazel_test_names = {tn for tn, _, _, _ in test_targets}
        pkg_wrapper_bodies = [
            gn_pre_clean[mp.end() : closing(gn_pre_clean, mp.end() - 1)]
            for mp in re.finditer(
                r'\b(?:fuchsia_(?:unit)?test_(?:package|component)|fuchsia_component|bootfs_test)'
                r'\(\s*"[^"]+"\s*\)\s*\{',
                gn_pre_clean,
            )
        ]
        for mt in re.finditer(
            r'\b(rustc_test|go_test)\(\s*"([^"]+)"\s*\)\s*\{',
            gn_pre_clean,
        ):
            t_tmpl, t_name = mt.group(1), mt.group(2)
            t_body = gn_pre_clean[mt.end() : closing(gn_pre_clean, mt.end() - 1)]
            out_m = re.search(r'\boutput_name\s*=\s*"([^"]+)"', t_body)
            if t_name in bazel_test_names or (out_m and out_m.group(1) in bazel_test_names):
                continue
            if any(refs_local(wb, td, t_name) for wb in pkg_wrapper_bodies):
                continue
            if is_packaged_by_kept_gn_test_package(gn_src, td, t_name):
                continue
            test_is_host = (
                file_is_host
                or _pos_in_is_host_block(gn_pre_clean, mt.start())
                or ran_on_host_in_base_gn(t_name)
            )
            unqual_pat = (
                r'"(?://' + re.escape(td) + r')?:' + re.escape(t_name) + r'"'
                if td
                else r'":' + re.escape(t_name) + r'"'
            )
            host_pat = (
                r'"(?://' + re.escape(td) + r')?:' + re.escape(t_name) + r'\(\$host_toolchain\)"'
                if td
                else r'":' + re.escape(t_name) + r'\(\$host_toolchain\)"'
            )
            for mg in re.finditer(r'\bgroup\(\s*"tests"\s*\)\s*\{', gn_pre_clean):
                g_body = gn_pre_clean[mg.end() : closing(gn_pre_clean, mg.end() - 1)]
                m_unqual = re.search(unqual_pat, g_body)
                if not m_unqual or re.search(host_pat, g_body):
                    continue
                ref_abs_pos = mg.end() + m_unqual.start()
                if not (test_is_host or _pos_in_is_host_block(gn_pre_clean, ref_abs_pos)):
                    continue
                g_line = gn_pre_clean.count("\n", 0, mg.start()) + 1
                msg_text = (
                    f"`group(\"tests\")` above `## BAZEL2GN SENTINEL` in '{gn_rel}' "
                    f"lists kept host `{t_tmpl}(\"{t_name}\")` as '\":{t_name}\"' "
                    "without `($host_toolchain)`, which static parity analysis "
                    "treats as build-only (`review_diff/unit_test_parity`)."
                )
                rem_text = (
                    f"Replace '\":{t_name}\"' with '\":{t_name}($host_toolchain)\"' "
                    f"in `group(\"tests\")` above `## BAZEL2GN SENTINEL` in '{gn_rel}' "
                    "so both GN and static parity checks recognize that the kept "
                    "host unit test still runs."
                )
                findings.append({
                    "source": "cq_reachability",
                    "category": "unqualified_kept_host_test",
                    "severity": "error",
                    "file": gn_rel,
                    "line": g_line,
                    "message": msg_text,
                    "remediation": rem_text,
                })

    # Verify that unit tests and group("tests") present in base BUILD.gn are not dropped.
    def check_base_gn_test_preservation():
        if not base_gn_clean or not FX_TEST_AVAILABLE or ext_suite_blocked:
            return
        had_base_tests_group = bool(re.search(r'\bgroup\(\s*"tests"\s*\)\s*\{', base_gn_clean))
        has_now_tests_group = bool(
            gn_src and re.search(r'\bgroup\(\s*"tests"\s*\)\s*\{', gn_code(gn_src))
        )
        all_tests_wired_in_anc_gn = bool(
            test_targets
            and not gn_tests_group_is_referenced(td)
            and all(direct_gn_test_callers(td, tn) for tn, _, _, _ in test_targets)
        )
        # Bazel-only tests (no GN test target left in BUILD.gn) reached through a wired
        # bazel_test_suite keep CI coverage, provided nothing still depends on `//td:tests`.
        all_tests_wired_via_bazel_suite = bool(
            test_targets
            and not gn_tests_group_is_referenced(td)
            and all(wired_via_bazel_test_suite(td, tn) for tn, _, _, _ in test_targets)
        )
        has_todo_omitted_test = bool(re.search(r"#\s*TODO\b", bazel_src))
        if (
            had_base_tests_group
            and (gn_src or gn_tests_group_is_referenced(td))
            and not has_now_tests_group
            and (
                gn_tests_group_is_referenced(td)
                or (
                    not all_tests_wired_in_anc_gn
                    and not all_tests_wired_via_bazel_suite
                )
            )
            and (gn_tests_group_is_referenced(td) or test_targets or not has_todo_omitted_test)
        ):
            anc_cands = [
                c
                for c in (
                    nearest_ancestor_test_suite(td, "target_tests"),
                    nearest_ancestor_test_suite(td, "host_tests"),
                    preferred_ancestor_area_suite(td, "target_tests"),
                    preferred_ancestor_area_suite(td, "host_tests"),
                )
                if c is not None
            ]
            anc_info = (
                max(anc_cands, key=lambda c: c[0].count("/")) if anc_cands else None
            )
            suite_guidance = (
                f"wire the package's `test_suite()` (`//{td}:tests` or `//{td}:host_tests`) into "
                f"the aggregation `test_suite()` `//{anc_info[0]}:{anc_info[1]}` in '{anc_info[0]}/BUILD.bazel'"
                if anc_info
                else (
                    f"define a package `test_suite()` (`tests` or `host_tests`) in '{bazel_rel}' "
                    "(omitting `visibility` until the aggregation package is known; do not guess an "
                    "aggregation directory) and report any resulting unexported-test finding in "
                    "`CoderReport.disputed_findings` and `summary` asking the reviewer which aggregation "
                    "`test_suite()` (or where a new one, with a matching `bazel_test_suite` in "
                    "`BUILD.gn`, should be added if none exists) the package's `test_suite()`(s) should "
                    "be added to, suggesting asking the Build team if they do not know"
                )
            )
            msg_text = (
                f"'{gn_rel}' defined `group(\"tests\")` in base GN, "
                "but `group(\"tests\")` was removed during migration while callers or "
                "package tests still depend on it, dropping CI build and test verification."
            )
            rem_text = (
                f"Restore `group(\"tests\") {{ testonly = true deps = [ ... ] }}` "
                f"in '{gn_rel}' (or wire the generated GN test target directly into the "
                f"parent `BUILD.gn` `group(\"tests\")` when nothing references `//{td}:tests`) "
                f"and {suite_guidance}."
            )
            findings.append({
                "source": "cq_reachability",
                "category": "dropped_tests_group",
                "severity": "error",
                "file": gn_rel,
                "line": 1,
                "message": msg_text,
                "remediation": rem_text,
            })
        for m_ut in re.finditer(
            r'\b(rustc_library|rustc_binary|rustc_proc_macro)\(\s*"([^"]+)"\s*\)\s*\{',
            base_gn_clean,
        ):
            lib_name = m_ut.group(2)
            lib_body = base_gn_clean[m_ut.end() : closing(base_gn_clean, m_ut.end() - 1)]
            if not re.search(r"\bwith_unit_tests\s*=\s*true\b", lib_body):
                continue
            ut_name = f"{lib_name}_test"
            if not re.search(r'":' + re.escape(ut_name) + r'(?:\(|")', base_gn_clean):
                continue
            if decomposed_unit_test_kept_above_sentinel(ut_name):
                continue
            ut_reachable_in_gn = bool(
                gn_tests_group_is_referenced(td)
                or direct_gn_test_callers(td, ut_name)
                or re.search(r"\bfuchsia_(?:unit)?test_package\s*\(", base_gn_clean)
            )
            if not test_targets and (ut_reachable_in_gn or not has_todo_omitted_test):
                msg_text = (
                    f"`{m_ut.group(1)}(\"{lib_name}\")` in '{gn_rel}' had "
                    f"`with_unit_tests = true` (`:{ut_name}` wired into tests in base GN), "
                    f"but '{bazel_rel}' defines no unit test target for it."
                )
                rem_text = (
                    f"Migrate `:{ut_name}` to '{bazel_rel}' (`rustc_test(name = "
                    f"\"{ut_name}\", crate = \":{lib_name}\")`, plus "
                    "`fx_unittest_package` for target tests and `wrap_host_rust_test` "
                    "when GN ran it on `$host_toolchain`), expose a package "
                    f"`test_suite(name = \"tests\" | \"host_tests\", tests = [\":<test_target>\", ...])` as the "
                    f"first target in '{bazel_rel}', and wire `//{td}:tests` (or "
                    f"`//{td}:host_tests`) into the aggregation `test_suite()`."
                )
                findings.append({
                    "source": "cq_reachability",
                    "category": "missing_migrated_tests",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": 1,
                    "message": msg_text,
                    "remediation": rem_text,
                })
            elif ran_on_host_in_base_gn(ut_name) and not any(
                r in HOST_TEST_RULES for _, r, _, _ in test_targets
            ) and not re.search(r"\b(?:wrap_host_rust_test|host_rustc_test)\s*\(", bazel_src):
                msg_text = (
                    f"'{gn_rel}' ran `:{ut_name}($host_toolchain)` on host in CI "
                    f"in base GN, but '{bazel_rel}' does not declare "
                    f"`wrap_host_rust_test(name = \"{lib_name}_host_test\", "
                    f"binary_name = \"{ut_name}\")` (or `host_rustc_test`)."
                )
                rem_text = (
                    f"Add `wrap_host_rust_test(name = \"{lib_name}_host_test\", "
                    f"binary_name = \"{ut_name}\")` in '{bazel_rel}', expose "
                    f"`test_suite(name = \"host_tests\", tests = [\":"
                    f"{lib_name}_host_test\"], target_compatible_with = HOST_OS_CONSTRAINTS)` "
                    f"in '{bazel_rel}' wired into the aggregation `host_tests` `test_suite()`, "
                    f"and keep '\":{ut_name}($host_toolchain)\"' in `group(\"tests\")`."
                )
                findings.append({
                    "source": "cq_reachability",
                    "category": "missing_bazel_host_test_wrapper",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": 1,
                    "message": msg_text,
                    "remediation": rem_text,
                })

        # Verify that GN `ffx_plugin` inline unit tests (`with_unit_tests = true` and
        # `args_with_unit_tests = true`) are decomposed into standalone Rust test targets.
        def check_base_ffx_plugin_unit_tests():
            try:
                bz_tree = ast.parse(bazel_src)
            except SyntaxError:
                return
            tested_crates = set()
            bazel_test_names_set = set()
            for c_node in ast.walk(bz_tree):
                if not isinstance(c_node, ast.Call):
                    continue
                r_nm = (
                    c_node.func.id
                    if isinstance(c_node.func, ast.Name)
                    else (
                        c_node.func.attr
                        if isinstance(c_node.func, ast.Attribute)
                        else ""
                    )
                )
                if r_nm not in ("host_rustc_test", "rustc_test", "rust_test"):
                    continue
                c_kws = {kw.arg: kw.value for kw in c_node.keywords if kw.arg}
                nm_v = c_kws.get("name")
                if isinstance(nm_v, ast.Constant) and isinstance(nm_v.value, str):
                    bazel_test_names_set.add(nm_v.value)
                cr_v = c_kws.get("crate")
                if isinstance(cr_v, ast.Constant) and isinstance(cr_v.value, str):
                    ln = local_name(cr_v.value, td)
                    if ln:
                        tested_crates.add(ln)

            for m_fp in re.finditer(
                r'\bffx_plugin\(\s*"([^"]+)"\s*\)\s*\{', base_gn_clean
            ):
                p_name = m_fp.group(1)
                fp_body = base_gn_clean[
                    m_fp.end() : closing(base_gn_clean, m_fp.end() - 1)
                ]
                specs = (
                    (
                        "with_unit_tests",
                        "test_deps",
                        p_name,
                        f"{p_name}_test",
                        "sources",
                        "src/lib.rs",
                    ),
                    (
                        "args_with_unit_tests",
                        "args_test_deps",
                        f"{p_name}_args",
                        f"{p_name}_args_test",
                        "args_sources",
                        "src/args.rs",
                    ),
                )
                for attr_nm, deps_attr, crate_nm, ut_nm, src_attr, def_src in specs:
                    if not re.search(rf"\b{attr_nm}\s*=\s*true\b", fp_body):
                        continue
                    if (
                        crate_nm in tested_crates
                        or ut_nm in bazel_test_names_set
                        or decomposed_unit_test_kept_above_sentinel(ut_nm)
                        or has_todo_omitted_test
                    ):
                        continue
                    src_m = re.search(
                        rf"\b{src_attr}\s*=\s*\[([^\]]*)\]", fp_body
                    )
                    rel_srcs = (
                        re.findall(r'"([^"]+)"', src_m.group(1))
                        if src_m
                        else [def_src]
                    ) or [def_src]
                    has_rust_tests = False
                    for rs_rel in rel_srcs:
                        rs_abs = os.path.join(workdir, td, rs_rel.lstrip("/"))
                        if not os.path.isfile(rs_abs):
                            has_rust_tests = True
                            break
                        try:
                            with open(rs_abs, encoding="utf-8", errors="ignore") as rf:
                                rs_txt = rf.read()
                        except OSError:
                            has_rust_tests = True
                            break
                        if re.search(
                            r"#\s*\[\s*(?:cfg\s*\(\s*test\b|(?:\w+::)*test\b)",
                            rs_txt,
                        ) or re.search(r"(?m)^\s*(?:pub\s+)?mod\s+\w+\s*;", rs_txt):
                            has_rust_tests = True
                            break
                    if not has_rust_tests:
                        continue
                    anc_s_wired = nearest_ancestor_test_suite(td, "host_tests")
                    anc_s_pref = preferred_ancestor_area_suite(td, "host_tests")
                    anc_s = (
                        anc_s_pref
                        if anc_s_wired is not None
                        and anc_s_pref is not None
                        and anc_s_pref[0].count("/") > anc_s_wired[0].count("/")
                        else (anc_s_wired or anc_s_pref)
                    )
                    suite_hint = (
                        f"expose `test_suite(name = \"host_tests\", "
                        f"tests = [\":{ut_nm}\"], target_compatible_with = HOST_OS_CONSTRAINTS, "
                        f"visibility = [\"//{anc_s[0]}:__pkg__\"])` as the first "
                        f"target in '{bazel_rel}' and add `\"//{td}:host_tests\"` to "
                        f"the aggregation `test_suite()` `//{anc_s[0]}:{anc_s[1]}` in '{anc_s[0]}/BUILD.bazel'"
                        if anc_s
                        else (
                            f"expose `test_suite(name = \"host_tests\", "
                            f"tests = [\":{ut_nm}\"], target_compatible_with = HOST_OS_CONSTRAINTS)` "
                            f"as the first target in '{bazel_rel}' (omitting `visibility` until "
                            f"the aggregation package is known; do not guess an aggregation "
                            f"directory) and report the resulting unexported-test finding in "
                            f"`CoderReport.disputed_findings` and `summary` asking the "
                            f"reviewer which aggregation `test_suite()` (or where a new one, "
                            f"with a matching `bazel_test_suite` in `BUILD.gn`, should be added "
                            f"if none exists) the package's `test_suite()`(s) should be added to, "
                            f"suggesting asking the Build team if they do not know"
                        )
                    )
                    msg_text = (
                        f"`ffx_plugin(\"{p_name}\")` in '{gn_rel}' had "
                        f"`{attr_nm} = true`, but '{bazel_rel}' defines no "
                        f"`host_rustc_test` for `:{crate_nm}` (`:{ut_nm}`), "
                        "dropping its unit tests."
                    )
                    rem_text = (
                        f"Add `host_rustc_test(name = \"{ut_nm}\", "
                        f"crate = \":{crate_nm}\", deps = [<GN {deps_attr}>])` "
                        f"in '{bazel_rel}' (the Bazel `ffx_plugin` macro does not "
                        f"accept inline `{attr_nm}` or `{deps_attr}` attributes), "
                        f"re-run `fx bazel2gn -d {td}`, wire "
                        f"'\":{ut_nm}($host_toolchain)\"' into `group(\"tests\")` "
                        f"in '{gn_rel}', and {suite_hint}."
                    )
                    findings.append({
                        "source": "cq_reachability",
                        "category": "missing_migrated_tests",
                        "severity": "error",
                        "file": bazel_rel,
                        "line": 1,
                        "message": msg_text,
                        "remediation": rem_text,
                    })

        check_base_ffx_plugin_unit_tests()

    # Verify that ancestor GN umbrella `ffx_plugin(plugin_deps = [...])` `group("tests")`
    # targets wire migrated child plugin `:tests` directly in a dedicated commented section
    # and update stale "for all plugins" aggregation comments.
    def check_umbrella_ffx_plugin_tests_group():
        plugin_names = _bazel_ffx_plugin_names(tree)
        if not plugin_names or not test_targets or not SENTINEL_RE.search(gn_src):
            return
        has_gn_tests = any(
            not any(has_skip_above(bazel_lines, ln) for ln in c_lns)
            for _, _, _, c_lns in test_targets
        ) or bool(re.search(r'\bgroup\(\s*"tests"\s*\)\s*\{', gn_code(gn_src)))
        if not has_gn_tests:
            return
        owner = _find_umbrella_ffx_tests_owner(td, plugin_names)
        if not owner:
            return
        anc, anc_gn_rel, anc_raw, anc_clean, umb_name = owner
        rel_td = os.path.relpath(td, anc)
        valid_names = {"tests"} | {tn for tn, _, _, _ in test_targets}
        stale_subtargets = {f"{pn}_tests" for pn in plugin_names}
        wired_in_anc = False
        first_grp_line = 1
        for m_grp in re.finditer(r'\bgroup\(\s*"tests"\s*\)\s*\{', anc_clean):
            end_pos = closing(anc_clean, m_grp.end() - 1)
            body_start = m_grp.end()
            clean_grp = anc_clean[body_start:end_pos]
            raw_grp = anc_raw[body_start:end_pos]
            grp_line = anc_raw.count("\n", 0, m_grp.start()) + 1
            body_start_line = anc_raw.count("\n", 0, body_start) + 1
            if (
                re.search(r'":tests\(\$host_toolchain\)"', clean_grp)
                and not refs_local(clean_grp, anc, f"{umb_name}_tests")
            ):
                continue
            first_grp_line = grp_line
            for raw_lbl in re.findall(r'"([^"]+)"', clean_grp):
                l_pkg, l_nm = _resolve_gn_label(raw_lbl, anc)
                if l_pkg == td and l_nm in valid_names:
                    wired_in_anc = True
                elif l_pkg == td and l_nm in stale_subtargets:
                    s_key = ("stale_subtarget", anc_gn_rel, raw_lbl)
                    if s_key not in umbrella_plugin_findings_seen:
                        umbrella_plugin_findings_seen.add(s_key)
                        findings.append({
                            "source": "cq_reachability",
                            "category": "stale_ffx_plugin_tests_subtarget",
                            "severity": "error",
                            "file": anc_gn_rel,
                            "line": grp_line,
                            "message": (
                                f"`group(\"tests\")` in '{anc_gn_rel}' still references "
                                f"`\"{raw_lbl}\"`, which is empty after migrating "
                                f"`//{td}` to Bazel (`fx bazel2gn` emits `ffx_plugin` "
                                "without `with_unit_tests = true`)."
                            ),
                            "remediation": (
                                f"Replace `\"{raw_lbl}\"` with `\"{rel_td}:tests\"` in "
                                f"`group(\"tests\")` of '{anc_gn_rel}' so the standalone "
                                "generated unit test target runs in GN."
                            ),
                        })
            if refs_local(clean_grp, anc, f"{umb_name}_tests"):
                _check_umbrella_tests_stale_comment(
                    anc_gn_rel,
                    raw_grp,
                    body_start_line,
                    umb_name,
                    umbrella_plugin_findings_seen,
                )
                _check_umbrella_tests_migrated_section(
                    anc,
                    anc_gn_rel,
                    anc_raw,
                    clean_grp,
                    raw_grp,
                    body_start,
                    umb_name,
                    td,
                    valid_names,
                    umbrella_plugin_findings_seen,
                )
        if (
            not wired_in_anc
            and not gn_tests_group_is_referenced(td)
            and not any(direct_gn_test_callers(td, tn) for tn, _, _, _ in test_targets)
        ):
            u_key = ("unwired_plugin", anc_gn_rel, td)
            if u_key not in umbrella_plugin_findings_seen:
                umbrella_plugin_findings_seen.add(u_key)
                findings.append({
                    "source": "cq_reachability",
                    "category": "unwired_migrated_plugin_tests",
                    "severity": "error",
                    "file": anc_gn_rel,
                    "line": first_grp_line,
                    "message": (
                        f"Migrated `ffx_plugin` package `//{td}` defines unit tests in "
                        f"'{gn_rel}', but '{anc_gn_rel}' only references "
                        f"`:{umb_name}_tests`, which no longer aggregates `//{td}`'s "
                        "standalone unit tests once `with_unit_tests` moves out of "
                        "`ffx_plugin`."
                    ),
                    "remediation": (
                        f"Add `\"{rel_td}:tests\"` to `group(\"tests\")` in "
                        f"'{anc_gn_rel}' under a dedicated commented section for "
                        f"Bazel-migrated plugins (separated by a blank line from "
                        f"`\":{umb_name}_tests\"`), and update any comment claiming "
                        f"`:{umb_name}_tests` covers all `plugin_deps`."
                    ),
                })

    check_base_gn_test_preservation()
    check_kept_pre_sentinel_host_tests()
    check_umbrella_ffx_plugin_tests_group()

    for t_name, rule_name, lineno, check_linenos in test_targets:
        # Check if this test target (or test attribute) has `# @bazel2gn:skip` in BUILD.bazel
        # (in which case bazel2gn does not generate a GN copy; migration_sanity.sh validates the skip).
        is_skipped = any(has_skip_above(bazel_lines, ln) for ln in check_linenos)
        m_sent = SENTINEL_RE.search(gn_src)
        if m_sent and not is_skipped:
            gn_pre_no_suites = strip_bazel_test_suites(gn_code(gn_src[: m_sent.start()]))
            gn_post_code = gn_code(gn_src[m_sent.end() :])
            ref_in_gn_pre = bool(re.search(r'":' + re.escape(t_name) + r'(?:\(|")', gn_pre_no_suites))
            needs_host_gn_ref = ran_on_host_in_base_gn(t_name) and not bool(
                re.search(r'":' + re.escape(t_name) + r'\(\$host_toolchain\)"', gn_pre_no_suites)
            )
            ref_in_gn_post_pkg = (
                not needs_host_gn_ref
                and (rule_name in ("rustc_test", "cc_test", "fx_cc_test_binary") or rule_name.startswith("rustc_"))
                and bool(re.search(r'":' + re.escape(t_name) + r'"', gn_post_code))
            )
            ref_in_ancestor_gn = False
            anc = os.path.dirname(td)
            while True:
                anc_rel = os.path.join(anc, "BUILD.gn") if anc else "BUILD.gn"
                anc_code = strip_bazel_test_suites(read_gn(anc_rel))
                if anc_code:
                    rel_from_anc = os.path.relpath(td, anc) if anc else td
                    label_re = r'"(?://' + re.escape(td) + r"|" + re.escape(rel_from_anc) + r"):" + re.escape(t_name) + r'(?:\(|")'
                    if t_name == os.path.basename(td):
                        # `gn format` shortens "dir/name:name" to "dir/name".
                        label_re += r'|"(?://' + re.escape(td) + r"|" + re.escape(rel_from_anc) + r')(?:\(|")'
                    if re.search(label_re, anc_code):
                        ref_in_ancestor_gn = True
                        break
                if not anc:
                    break
                anc = os.path.dirname(anc)
            if not (
                ref_in_gn_pre
                or ref_in_gn_post_pkg
                or ref_in_ancestor_gn
                or _is_referenced_by_cross_pkg_gn(td, t_name)
            ):
                findings.append({
                    "source": "cq_reachability",
                    "category": "orphaned_test_target",
                    "severity": "error",
                    "file": gn_rel,
                    "line": 1,
                    "message": (
                        f"Generated GN test copy ':{t_name}' (from `{rule_name}` in '{bazel_rel}') below "
                        f"`## BAZEL2GN SENTINEL` in '{gn_rel}' is not wired into `group(\"tests\")` above the "
                        "sentinel (or an ancestor `BUILD.gn` `tests` group outside `bazel_test_suite`), so the "
                        "GN build of this test is dead code and never runs in CQ."
                    ),
                    "remediation": (
                        f"Add '\":{t_name}\"' (or '\":{t_name}($host_toolchain)\"' for a host test) to "
                        f"`group(\"tests\")` above `## BAZEL2GN SENTINEL` in '{gn_rel}' alongside `:bazel_tests`."
                    ),
                })
                continue
        if m_sent or rule_name in ("fx_unittest_package", "host_go_test", "host_rustc_test"):
            continue
        ref_patterns = [
            f'":{t_name}"',
            f'"{t_name}"',
            f":{t_name}\"",
            f"{os.path.basename(td)}:{t_name}",
        ]
        referenced_in_gn = any(p in gn_src for p in ref_patterns)
        referenced_in_parent = any(p in parent_gn_src for p in ref_patterns) or (
            f'"{os.path.basename(td)}:tests"' in parent_gn_src
        )
        referenced_in_bazel = bazel_src.count(f'":{t_name}"') >= 1 or bazel_src.count(f'"{t_name}"') >= 2
        if not (
            referenced_in_gn
            or referenced_in_parent
            or referenced_in_bazel
            or _is_referenced_by_cross_pkg_gn(td, t_name)
        ):
            findings.append({
                "source": "cq_reachability",
                "category": "orphaned_test_target",
                "severity": "error",
                "file": bazel_rel,
                "line": lineno,
                "message": (
                    f"Test target ':{t_name}' ({rule_name}) in '{bazel_rel}' is not referenced by "
                    "`group(\"tests\")`, a test package, or the parent directory's `tests` group, "
                    "so it would be silently dropped from CQ."
                ),
                "remediation": (
                    f"Wire ':{t_name}' into `group(\"tests\")` (or its enclosing `fuchsia_unittest_package` / "
                    "parent `BUILD.gn` `tests` group) so CQ continues to build and run this test."
                ),
            })

    # 3. Bazel tests visible to Fuchsia test runners only via a GN bazel_test_suite.
    exportable = []  # (name, kind, rule, lineno, GN twin: the bazel2gn-generated GN test it runs, or None)
    suites = {}  # test_suite name -> list of member names, or None for "every test"
    suite_lines = {}  # test_suite name -> start line number in BUILD.bazel
    all_tests = []  # names of non-manual test targets of the package (for empty test_suite)

    # Collect local test binary target names wrapped by `wrap_host_rust_test` or
    # `host_test` in the same BUILD.bazel so the underlying `rustc_test`/`rust_test`
    # binary is not flagged as a separate unexported host test alongside its wrapper.
    def collect_host_wrapped_binaries(tree_node, pkg_dir):
        wrapped = set()
        for call_node in ast.walk(tree_node):
            if not isinstance(call_node, ast.Call):
                continue
            r_name = (
                call_node.func.id
                if isinstance(call_node.func, ast.Name)
                else (
                    call_node.func.attr
                    if isinstance(call_node.func, ast.Attribute)
                    else ""
                )
            )
            if r_name not in ("wrap_host_rust_test", "host_test"):
                continue
            for kw in call_node.keywords:
                if (
                    kw.arg in ("binary_name", "binary", "unstripped_binary")
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    bn = local_name(kw.value.value, pkg_dir)
                    if bn:
                        wrapped.add(bn)
        return wrapped

    host_wrapped_bins = collect_host_wrapped_binaries(tree, td)

    # Check whether a Rust device test binary is packaged in BUILD.bazel or a GN-kept test pkg.
    def is_rust_device_test_packaged(device_bin, call_node):
        call_seg = ast.get_source_segment(bazel_src, call_node) or ""
        if "HOST_OS_CONSTRAINTS" in call_seg or "HOST_CONSTRAINTS" in call_seg:
            return True
        if device_bin in host_wrapped_bins and not re.search(
            r"\b(?:fuchsia_unittest_package|fuchsia_test_package|"
            r"fuchsia_test_component|fuchsia_unittest_component)\s*\(",
            base_gn_clean,
        ):
            return True
        bin_pat = (
            r'\bbinary\s*=\s*"(?://'
            + re.escape(td)
            + r')?:'
            + re.escape(device_bin)
            + r'"'
        )
        ut_pat = (
            r'\bunit_tests\s*=\s*\[[^\]]*"(?://'
            + re.escape(td)
            + r')?:'
            + re.escape(device_bin)
            + r'"'
        )
        if re.search(bin_pat, bazel_src) or re.search(ut_pat, bazel_src):
            return True
        comp_dep_pat = (
            r'\b(?:fx_component|fx_test_component)\s*\([^)]*"(?://'
            + re.escape(td)
            + r')?:'
            + re.escape(device_bin)
            + r'"'
        )
        if re.search(comp_dep_pat, bazel_src, re.S) and not re.search(
            r"\b(?:fuchsia_unittest_package|fuchsia_test_package)\s*\(",
            base_gn_clean,
        ):
            return True
        m_s = SENTINEL_RE.search(gn_src)
        gn_pre_clean = gn_code(gn_src[: m_s.start()]) if m_s else gn_code(gn_src)
        gn_pkg_pat = (
            r"\b(?:fuchsia_unittest_package|fuchsia_test_package|"
            r"fuchsia_test_component|fuchsia_unittest_component|"
            r"fuchsia_component)\s*\("
        )
        gn_ref_pat = (
            r'"(?://' + re.escape(td) + r')?:' + re.escape(device_bin) + r'"'
        )
        return bool(
            re.search(gn_pkg_pat, gn_pre_clean)
            and re.search(gn_ref_pat, gn_pre_clean)
        )

    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        rule_name = node.func.id if isinstance(node.func, ast.Name) else (
            node.func.attr if isinstance(node.func, ast.Attribute) else ""
        )
        kws = {kw.arg: kw.value for kw in node.keywords if kw.arg}
        name_v = kws.get("name")
        if not (isinstance(name_v, ast.Constant) and isinstance(name_v.value, str)):
            continue
        t_name, lineno = name_v.value, getattr(node, "lineno", 1)
        manual = "manual" in bazel_strings(kws.get("tags"))
        if not manual:
            if rule_name in DEVICE_TEST_RULES:
                exportable.append((t_name, "target", rule_name, lineno, None))
            elif rule_name in HOST_TEST_RULES or (
                rule_name in ("rustc_test", "rust_test")
                and t_name not in host_wrapped_bins
                and "HOST_OS_CONSTRAINTS" in (ast.get_source_segment(bazel_src, node) or "")
            ):
                twin = t_name if rule_name in GN_CONVERTED_HOST_TEST_RULES else None
                exportable.append((t_name, "host", rule_name, lineno, twin))
            elif rule_name in RUST_TEST_OWNERS and "host" in unit_test_envs(kws):
                exportable.append((t_name + "_test", "host", f"{rule_name}(with_unit_tests)", lineno, None))
        if rule_name == "test_suite":
            suite_lines[t_name] = lineno
            tests_v = kws.get("tests")
            tests_list = None if tests_v is None else [
                local_name(s, td) for s in bazel_strings(tests_v)
            ]
            suites[t_name] = tests_list if tests_list else None
        if (
            not manual
            and t_name not in host_wrapped_bins
            and (
                rule_name in DEVICE_TEST_RULES | HOST_TEST_RULES
                or rule_name.endswith("_test")
            )
        ):
            all_tests.append(t_name)
        if rule_name in RUST_TEST_OWNERS and "host" in unit_test_envs(kws):
            all_tests.append(t_name + "_test")
        if rule_name == "rustc_test" or (rule_name in RUST_TEST_OWNERS and "fuchsia" in unit_test_envs(kws)):
            device_bin = t_name if rule_name == "rustc_test" else t_name + "_test"
            packaged_in_bin = re.search(
                r'\bbinary\s*=\s*"(?://' + re.escape(td) + r')?:' + re.escape(device_bin) + r'"', bazel_src
            )
            packaged_in_unit_tests = re.search(
                r'\bunit_tests\s*=\s*\[[^\]]*"(?://' + re.escape(td) + r')?:' + re.escape(device_bin) + r'"', bazel_src
            )
            packaged_in_gn_pkg = is_packaged_by_kept_gn_test_package(
                gn_src, td, device_bin
            )
            if FX_TEST_AVAILABLE and not ext_suite_blocked and not (
                packaged_in_bin
                or packaged_in_unit_tests
                or packaged_in_gn_pkg
                or is_rust_device_test_packaged(device_bin, node)
            ):
                findings.append({
                    "source": "cq_reachability",
                    "category": "unpackaged_rust_device_test",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": lineno,
                    "message": (
                        f"Rust device test binary ':{device_bin}' ({rule_name}) in '{bazel_rel}' is not packaged by any "
                        "`fx_packaged_binary` or `fx_unittest_package`, so no `fx_test` runs it and it is missing from tests.json."
                    ),
                    "remediation": (
                        f"Add `fx_unittest_package(name = ..., unit_tests = [\":{device_bin}\"])`. Only when "
                        "the test needs a custom `.cml` beyond the standard Rust test runner/syslog shards "
                        "(otherwise migration_sanity reports `unrolled_unittest_package`), unroll it into "
                        f"`fx_packaged_binary(testonly = True, binary = \":{device_bin}\", binary_name = ...)` "
                        "(GN's output name, e.g. `<crate>_lib_test`), a custom `meta/<component>.cml` with "
                        "`program.binary` = `bin/<binary_name>`, `fx_component_manifest`, `fx_test_component`, "
                        "`fx_package(test_components = ...)` and `fx_test`, exported by a GN `bazel_test_suite` "
                        "(see \"Migrating Tests\" in the coder instructions). If the tests only ran on host in GN, "
                        "use `host_rustc_test` instead."
                    ),
                })
    if not exportable or not FX_TEST_AVAILABLE:
        continue

    def members(suite, seen):
        """Local test names a Bazel test_suite of this package runs (recursively)."""
        if suite in seen:
            return set()
        seen.add(suite)
        listed = suites[suite] if suites[suite] else all_tests
        out = set()
        for m in listed:
            if m is None:
                continue
            if m in suites:
                out |= members(m, seen)
            else:
                out.add(m)
        return out

    listed_in = {}  # local test name -> [(gn_rel, suite, param, line)]
    for s_rel, s_name, s_line, param, labels in bazel_test_suites_for(td):
        for label in labels:
            m = label_target(label, td)
            if m is not None:
                found = {m}
            elif re.match(r"^@{0,2}//", label.strip()):
                found = foreign_suite_tests(label, td, td, set())
            else:
                continue
            names = set()
            for f_name in found:
                names |= members(f_name, set()) if f_name in suites else {f_name}
            for n in names:
                listed_in.setdefault(n, []).append((s_rel, s_name, param, s_line))

    base_ref = (
        effective_diff_base
        if effective_diff_base != "HEAD"
        else ("HEAD~1" if head_is_task_commit else "HEAD")
    )
    base_gn_src = ""
    for ref in ([base_ref, "HEAD~1"] if base_ref == "HEAD" else [base_ref]):
        try:
            out = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{ref}:{gn_rel}"], stderr=subprocess.DEVNULL, text=True
            )
            if "## BAZEL2GN SENTINEL" not in out:
                base_gn_src = gn_code(out)
                break
            if not base_gn_src:
                base_gn_src = gn_code(out)
        except Exception:
            pass

    had_base_local_suite = bool(base_gn_src and SUITE_RE.search(base_gn_src))
    gn_pre = gn_src[: SENTINEL_RE.search(gn_src).start()] if SENTINEL_RE.search(gn_src) else gn_src
    has_mixed_exportable = len({k for _, k, _, _, _ in exportable}) > 1
    for t_name, kind, rule_name, lineno, twin in exportable:
        param = "target_tests" if kind == "target" else "host_tests"
        entries = listed_in.get(t_name, [])
        anc_suite_info = nearest_ancestor_test_suite(td, param)
        pref_area = (
            preferred_ancestor_area_suite(
                td, param, has_both_kinds=has_mixed_exportable
            )
            if not had_base_local_suite
            else None
        )
        if (
            anc_suite_info is not None
            and pref_area is not None
            and (
                pref_area[0].count("/") > anc_suite_info[0].count("/")
                or (
                    pref_area[0] != anc_suite_info[0]
                    and ancestor_references_package(pref_area[0], td)
                )
            )
        ):
            # Prefer a deeper canonical area aggregation directory or an ancestor
            # that already references td over another ancestor suite so both
            # "tests" and "host_tests" live in the same aggregation BUILD.bazel file.
            anc_suite_info = None
        kind_ext_blockers = ext_blockers_by_kind.get(kind, [])
        fuchsia_incompat_blockers = [b for b in kind_ext_blockers if b.startswith("@")]
        if fuchsia_incompat_blockers and not SENTINEL_RE.search(gn_src) and not entries:
            # Not dual-built: migration_sanity unbuildable_external_dep_device_test asks to keep
            # the package dual-built with this test unexported, so do not demand an export.
            findings.append({
                "source": "cq_reachability",
                "category": "external_repo_blocked_bazel_test_export",
                "severity": "INFO",
                "file": bazel_rel,
                "line": lineno,
                "message": (
                    f"Bazel test ':{t_name}' ({rule_name}) in '{bazel_rel}' is not required to be "
                    "exported via `bazel_test_suite` / ancestor `test_suite` because the package "
                    "depends on "
                    + ", ".join(f"`{b}`" for b in fuchsia_incompat_blockers)
                    + ", which cannot build for `@platforms//os:fuchsia`. "
                    + pcl.external_repo_blocked_test_remediation(
                        td, f"`:{t_name}`", fuchsia_incompat_blockers
                    )
                ),
            })
            continue
        if kind_ext_blockers and SENTINEL_RE.search(gn_src):
            if entries and anc_suite_info is not None:
                anc_pkg, anc_suite = anc_suite_info
                in_anc = t_name in foreign_suite_tests(
                    f"//{anc_pkg}:{anc_suite}", anc_pkg, td, set()
                )
                if in_anc:
                    anc_bz_rel = f"{anc_pkg}/BUILD.bazel"
                    msg_text = (
                        f"Dual-built Bazel test ':{t_name}' ({rule_name}) in '{bazel_rel}' "
                        f"is wired into ancestor `test_suite` `//{anc_pkg}:{anc_suite}` in "
                        f"'{anc_bz_rel}', which causes `fx build` on CQ to fail in an "
                        "upstream external repository under `third_party/` whose `BUILD.bazel` "
                        "lacks `includes` / Fuchsia SDK dependencies, while the generated GN "
                        f"test copy below `## BAZEL2GN SENTINEL` in '{gn_rel}' already runs in CQ."
                    )
                    rem_text = (
                        f"Remove `\"//{td}:{t_name}\"` from `test_suite(name = \"{anc_suite}\")` "
                        f"in '{anc_bz_rel}'. "
                        + pcl.external_repo_blocked_test_remediation(
                            td, f"`:{t_name}`", kind_ext_blockers
                        )
                    )
                    findings.append({
                        "source": "cq_reachability",
                        "category": "external_repo_incompatible_bazel_test_export",
                        "severity": "error",
                        "file": anc_bz_rel,
                        "line": 1,
                        "message": msg_text,
                        "remediation": rem_text,
                    })
                    continue
            elif not entries:
                findings.append({
                    "source": "cq_reachability",
                    "category": "external_repo_blocked_bazel_test_export",
                    "severity": "INFO",
                    "file": bazel_rel,
                    "line": lineno,
                    "message": (
                        f"Bazel test ':{t_name}' ({rule_name}) in '{bazel_rel}' is intentionally "
                        "not exported via `bazel_test_suite` / ancestor `test_suite` because an "
                        "upstream external repository dependency under `third_party/` lacks "
                        "Fuchsia `BUILD.bazel` support; the generated GN test below "
                        f"`## BAZEL2GN SENTINEL` in '{gn_rel}' continues to run in CQ."
                    ),
                })
                continue
        missing_anc_area = pref_area if anc_suite_info is None else None
        compat_expr = (
            '["@platforms//os:fuchsia"]'
            if param == "target_tests"
            else "HOST_OS_CONSTRAINTS"
        )
        if twin and SENTINEL_RE.search(gn_src):
            # GN already builds this test (bazel2gn output): it is reached through GN as today.
            # Exporting it too would run the same test twice when GN runs it on host.
            gn_host = re.search(r'":' + re.escape(twin) + r'\(', gn_pre) or f"//{td}:{twin}(" in parent_gn_src
            if entries and gn_host:
                s_rel, s_name, _, s_line = entries[0]
                findings.append({
                    "source": "cq_reachability",
                    "category": "duplicate_host_test_export",
                    "severity": "error",
                    "file": s_rel,
                    "line": s_line,
                    "message": (
                        f"`bazel_test_suite(\"{s_name}\")` in '{s_rel}' exports {rule_name} ':{t_name}', but GN "
                        f"already runs the bazel2gn-generated `:{twin}` on host (referenced with a toolchain in "
                        f"'{gn_rel}'), so the same test would run twice."
                    ),
                    "remediation": (
                        f"Remove `//{td}:{t_name}` from the suite (and a `wrap_host_rust_test` added only for it); "
                        f"keep `\":{twin}($host_toolchain)\"` in `group(\"tests\")`."
                    ),
                })
                continue
            was_wired_in_base_gn = bool(
                base_gn_src and re.search(r'":' + re.escape(twin) + r'(?:\(|")', base_gn_src)
            )
            had_with_unit_tests_in_base_gn = bool(
                base_gn_src and re.search(r"\bwith_unit_tests\s*=\s*true\b", base_gn_src)
            )
            if (
                gn_host
                or (
                    twin == t_name
                    and is_dual_build_gn_test_wired(gn_pre, parent_gn_src, td, twin)
                )
                or (
                    twin == t_name
                    and not entries
                    and had_with_unit_tests_in_base_gn
                    and not was_wired_in_base_gn
                    and f"//{td}:{twin}" not in parent_gn_src
                    and f'"{os.path.basename(td)}:tests"' not in parent_gn_src
                )
            ):
                continue
        gn_test_callers = direct_gn_test_callers(td, t_name)
        local_gn_tests_referenced = gn_tests_group_is_referenced(td)
        pkg_suite_name = "tests" if kind == "target" else "host_tests"
        same_kind_tests = sorted(
            {n for n, k, _, _, _ in exportable if k == kind}
        )
        pkg_suite_tests_attr = (
            "tests = [" + ", ".join(f'":{n}"' for n in same_kind_tests) + "]"
        )
        pkg_suite_attrs = [f'name = "{pkg_suite_name}"', pkg_suite_tests_attr]
        if kind == "host":
            pkg_suite_attrs.append(
                "target_compatible_with = HOST_OS_CONSTRAINTS"
            )
        pkg_suite_decl_no_vis = ", ".join(pkg_suite_attrs)

        def _pkg_suite_decl(vis_pkgs):
            return ", ".join(
                pkg_suite_attrs + [f"visibility = {json.dumps(vis_pkgs)}"]
            )

        if not entries:
            if (
                not gn_test_callers
                and was_unwired_gn_test_package_at_base(
                    base_gn_src, parent_gn_src, t_name
                )
            ):
                continue
            wrap_hint = ""
            if rule_name in ("rustc_test", "rust_test"):
                stem = t_name[:-5] if t_name.endswith("_test") else t_name
                wrap_hint = (
                    f" Because `{rule_name}` does not provide `FuchsiaHostTestInfo`, "
                    f"wrap `:{t_name}` with `wrap_host_rust_test(name = "
                    f"\"{stem}_host_test\", binary_name = \"{t_name}\")` (or use "
                    f"`host_rustc_test` when `BUILD.gn` has no unguarded "
                    "`assert(is_host)` unit test) and include the wrapper in "
                    f"`test_suite(name = \"{pkg_suite_name}\")`."
                )
            if anc_suite_info is not None:
                anc_pkg, anc_suite = anc_suite_info
                req_vis = ancestor_suite_visibility(anc_pkg)
                msg_text = (
                    f"Bazel test ':{t_name}' ({rule_name}) in '{bazel_rel}' is not wired "
                    f"into the existing aggregation `test_suite()` `//{anc_pkg}:{anc_suite}` in "
                    f"'{anc_pkg}/BUILD.bazel' (exported via `bazel_test_suite` in "
                    "`BUILD.gn`), so it is missing from tests.json and neither `fx test` "
                    "nor infra runs it."
                )
                rem_text = (
                    f"Define `test_suite({_pkg_suite_decl(req_vis)})` "
                    f"as the first target in '{bazel_rel}', keep `:{t_name}` "
                    f"package-private (omit `visibility`), and add "
                    f"`\"//{td}:{pkg_suite_name}\"` (in sorted order) to "
                    f"`test_suite(name = \"{anc_suite}\", tests = [...])` in "
                    f"'{anc_pkg}/BUILD.bazel' (do not add a per-package "
                    f"`bazel_test_suite` in '{gn_rel}').{wrap_hint}"
                )
            elif missing_anc_area is not None:
                anc_pkg, anc_suite = missing_anc_area
                anc_gn_suite = existing_wired_gn_suite_name(anc_pkg) or "bazel_tests"
                req_vis = ancestor_suite_visibility(anc_pkg)
                compat_load_note = (
                    ' (loading `HOST_OS_CONSTRAINTS` from '
                    '`//build/bazel/platforms:constraints.bzl`)'
                    if param == "host_tests"
                    else ""
                )
                msg_text = (
                    f"Bazel test ':{t_name}' ({rule_name}) in '{bazel_rel}' is not exported "
                    f"through an aggregation `test_suite()` in '{anc_pkg}/BUILD.bazel' and "
                    f"`bazel_test_suite` in '{anc_pkg}/BUILD.gn', so it is missing from "
                    "tests.json and neither `fx test` nor infra runs it."
                )
                rem_text = (
                    f"Define `test_suite({_pkg_suite_decl(req_vis)})` "
                    f"as the first target in '{bazel_rel}' (keeping `:{t_name}` "
                    f"package-private), create or update "
                    f"`test_suite(name = \"{anc_suite}\", "
                    f"target_compatible_with = {compat_expr}, "
                    f"tests = [\"//{td}:{pkg_suite_name}\"])` in "
                    f"'{anc_pkg}/BUILD.bazel'{compat_load_note}, and wire "
                    f"`\"//{anc_pkg}:{anc_suite}\"` into "
                    f"`bazel_test_suite(\"{anc_gn_suite}\") {{ {param} = "
                    f"[ \"//{anc_pkg}:{anc_suite}\" ] }}` (and `\":{anc_gn_suite}\"` into "
                    f"`group(\"tests\")`) in '{anc_pkg}/BUILD.gn' "
                    f"(do not add a per-package `bazel_test_suite` in '{gn_rel}').{wrap_hint}"
                )
            else:
                msg_text = (
                    f"Bazel test ':{t_name}' ({rule_name}) in '{bazel_rel}' is not wired "
                    f"into any aggregation `test_suite()` (exported via `bazel_test_suite({param} = [...])`), "
                    "so it is missing from tests.json: neither `fx test` nor infra runs it, and defining it "
                    "(or running it with `fx bazel test`) does not change that."
                )
                rem_text = (
                    f"Define `test_suite({pkg_suite_decl_no_vis})` "
                    f"as the first target in '{bazel_rel}' (keeping `:{t_name}` "
                    f"package-private, omitting `visibility` until the aggregation "
                    f"package is known, and never adding a per-package `bazel_test_suite` "
                    f"in '{gn_rel}'). Because no existing aggregation `test_suite()` "
                    f"was found automatically for `//{td}`, do not guess or invent an "
                    f"aggregation directory: report this finding in "
                    f"`CoderReport.disputed_findings` (`source: \"cq_reachability\"`, "
                    f"`category: \"unexported_bazel_test\"`, `file: \"{bazel_rel}\"`) and "
                    f"`summary` asking the reviewer which aggregation `test_suite()` (or "
                    f"where a new one, with a matching `bazel_test_suite` in that "
                    f"directory's `BUILD.gn`, should be added if none exists) the "
                    f"package's `test_suite()`(s) should be added to, and suggesting "
                    f"asking the Build team if they do not know.{wrap_hint}"
                )
            findings.append({
                "source": "cq_reachability",
                "category": "unexported_bazel_test",
                "severity": "error",
                "file": bazel_rel,
                "line": lineno,
                "message": msg_text,
                "remediation": rem_text,
            })
            continue
        # Enforce wiring into an existing or newly created aggregation BUILD.bazel test_suite()
        # instead of a new per-package bazel_test_suite in BUILD.gn.
        handled_local_ancestor_suite = False
        if anc_suite_info is not None and not had_base_local_suite:
            anc_pkg, anc_suite = anc_suite_info
            req_vis = ancestor_suite_visibility(anc_pkg)
            other_param = (
                "host_tests" if param == "target_tests" else "target_tests"
            )
            other_has_local_only = bool(
                re.search(
                    r"\b" + other_param + r'\s*\+?=\s*\[[^\]]*"//',
                    gn_code(gn_pre),
                )
                and nearest_ancestor_test_suite(td, other_param) is None
                and preferred_ancestor_area_suite(
                    td, other_param, has_both_kinds=has_mixed_exportable
                )
                is None
            )
            rm_local_desc = (
                f"remove `{param}` from the per-package `bazel_test_suite`"
                if other_has_local_only
                else (
                    "remove the per-package `bazel_test_suite`, unreachable `group(\"tests\")`, "
                    "and unused `//build/bazel/bazel_test_suite.gni` import"
                    if (gn_test_callers and not local_gn_tests_referenced)
                    else "remove the per-package `bazel_test_suite` "
                    "(and unused `//build/bazel/bazel_test_suite.gni` import)"
                )
            )
            def _expanded_foreign_tests(ap, as_name):
                raw_found = foreign_suite_tests(
                    f"//{ap}:{as_name}", ap, td, set()
                )
                expanded = set()
                for f_name in raw_found:
                    expanded |= (
                        members(f_name, set()) if f_name in suites else {f_name}
                    )
                return raw_found, expanded

            matched_anc = None
            for ap, as_name in reversed(wired_ancestor_test_suites(td, param)):
                raw_f, exp_f = _expanded_foreign_tests(ap, as_name)
                if t_name in exp_f:
                    matched_anc = (ap, as_name, raw_f, exp_f)
                    break
            if matched_anc is not None:
                anc_pkg, anc_suite, anc_raw_found, anc_expanded = matched_anc
                in_anc_suite = True
            else:
                anc_raw_found, anc_expanded = _expanded_foreign_tests(
                    anc_pkg, anc_suite
                )
                in_anc_suite = t_name in anc_expanded
            bad_anc_ref = next(
                (
                    f_name
                    for f_name in anc_raw_found
                    if (
                        (f_name == t_name and t_name not in suites)
                        or (
                            f_name in suites
                            and f_name not in ("tests", "host_tests")
                            and t_name in members(f_name, set())
                        )
                    )
                    and f"//{td}:{f_name}" not in base_bazel_src_of(anc_pkg)
                ),
                None,
            )
            local_entries = [e for e in entries if e[0] == gn_rel]
            if not in_anc_suite:
                handled_local_ancestor_suite = True
                msg_text = (
                    f"Bazel test ':{t_name}' ({rule_name}) in '{bazel_rel}' is exported "
                    f"via a per-package `bazel_test_suite` in '{gn_rel}' instead of "
                    f"being wired into the existing aggregation `test_suite()` "
                    f"`//{anc_pkg}:{anc_suite}` in '{anc_pkg}/BUILD.bazel'."
                )
                rem_text = (
                    f"Define `test_suite({_pkg_suite_decl(req_vis)})` "
                    f"as the first target in '{bazel_rel}', omit `visibility` on "
                    f"`:{t_name}`, wire `\"//{td}:{pkg_suite_name}\"` "
                    f"(in sorted order) into `test_suite(name = \"{anc_suite}\", "
                    f"tests = [...])` in '{anc_pkg}/BUILD.bazel', and "
                    f"{rm_local_desc} in '{gn_rel}'."
                )
                findings.append({
                    "source": "cq_reachability",
                    "category": "bypassed_ancestor_test_suite",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": lineno,
                    "message": msg_text,
                    "remediation": rem_text,
                })
            elif bad_anc_ref is not None:
                handled_local_ancestor_suite = True
                anc_bz_rel = os.path.join(anc_pkg, "BUILD.bazel")
                bad_ref_kind = (
                    "private sub-suite"
                    if bad_anc_ref in suites
                    else "individual test target"
                )
                msg_text = (
                    f"Aggregation `test_suite()` `//{anc_pkg}:{anc_suite}` in '{anc_bz_rel}' "
                    f"references {bad_ref_kind} `\"//{td}:{bad_anc_ref}\"` directly "
                    f"instead of a package `test_suite()` (`\"//{td}:{pkg_suite_name}\"`)."
                )
                rem_text = (
                    f"Define `test_suite({_pkg_suite_decl(req_vis)})` "
                    f"as the first target in '{bazel_rel}', omit `visibility` on "
                    f"`:{bad_anc_ref}`, and replace `\"//{td}:{bad_anc_ref}\"` with "
                    f"`\"//{td}:{pkg_suite_name}\"` in `test_suite(name = "
                    f"\"{anc_suite}\")` in '{anc_bz_rel}'."
                )
                findings.append({
                    "source": "cq_reachability",
                    "category": "direct_leaf_test_in_ancestor_suite",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": lineno,
                    "message": msg_text,
                    "remediation": rem_text,
                })
            elif local_entries:
                handled_local_ancestor_suite = True
                local_rel, local_name_s, _, local_line = local_entries[0]
                msg_text = (
                    f"`bazel_test_suite(\"{local_name_s}\")` in '{local_rel}' "
                    f"redundantly exports ':{t_name}', which is already wired into the "
                    f"aggregation `test_suite()` `//{anc_pkg}:{anc_suite}` in "
                    f"'{anc_pkg}/BUILD.bazel'."
                )
                rem_text = (
                    f"Remove `\"//{td}:{t_name}\"` (and empty `{param}`) from "
                    f"`bazel_test_suite(\"{local_name_s}\")` in '{local_rel}'."
                    if other_has_local_only
                    else (
                        f"Remove `bazel_test_suite(\"{local_name_s}\")` (and "
                        f"`\":{local_name_s}\"` from `group(\"tests\")`, plus any unused "
                        f"`//build/bazel/bazel_test_suite.gni` import) from '{local_rel}'."
                    )
                )
                findings.append({
                    "source": "cq_reachability",
                    "category": "redundant_local_bazel_test_suite",
                    "severity": "error",
                    "file": local_rel,
                    "line": local_line,
                    "message": msg_text,
                    "remediation": rem_text,
                })
        elif missing_anc_area is not None and any(e[0] == gn_rel for e in entries):
            handled_local_ancestor_suite = True
            anc_pkg, anc_suite = missing_anc_area
            anc_gn_suite = existing_wired_gn_suite_name(anc_pkg) or "bazel_tests"
            local_suite_name = next(e[1] for e in entries if e[0] == gn_rel)
            req_vis = ancestor_suite_visibility(anc_pkg)
            compat_load_note = (
                ' (loading `HOST_OS_CONSTRAINTS` from '
                '`//build/bazel/platforms:constraints.bzl`)'
                if param == "host_tests"
                else ""
            )
            msg_text = (
                f"Bazel test ':{t_name}' ({rule_name}) in '{bazel_rel}' is exported "
                f"via a per-package `bazel_test_suite` in '{gn_rel}' instead of "
                f"an aggregation `test_suite(name = \"{anc_suite}\")` in "
                f"'{anc_pkg}/BUILD.bazel' wired through "
                f"`bazel_test_suite(\"{anc_gn_suite}\")` in '{anc_pkg}/BUILD.gn'."
            )
            rem_text = (
                f"Define `test_suite({_pkg_suite_decl(req_vis)})` "
                f"as the first target in '{bazel_rel}' (omitting `visibility` on "
                f"`:{t_name}`), create or update "
                f"`test_suite(name = \"{anc_suite}\", "
                f"target_compatible_with = {compat_expr}, "
                f"tests = [\"//{td}:{pkg_suite_name}\"])` in "
                f"'{anc_pkg}/BUILD.bazel'{compat_load_note}, wire "
                f"`\"//{anc_pkg}:{anc_suite}\"` into "
                f"`bazel_test_suite(\"{anc_gn_suite}\") {{ {param} = "
                f"[ \"//{anc_pkg}:{anc_suite}\" ] }}` (and `\":{anc_gn_suite}\"` into "
                f"`group(\"tests\")`) in '{anc_pkg}/BUILD.gn', and remove "
                f"`bazel_test_suite(\"{local_suite_name}\")` (plus "
                f"`\":{local_suite_name}\"` from `group(\"tests\")` and any unused "
                f"`//build/bazel/bazel_test_suite.gni` import) from '{gn_rel}'."
            )
            findings.append({
                "source": "cq_reachability",
                "category": "bypassed_ancestor_test_suite",
                "severity": "error",
                "file": bazel_rel,
                "line": lineno,
                "message": msg_text,
                "remediation": rem_text,
            })
        elif (
            not had_base_local_suite
            and any(e[0] == gn_rel for e in entries)
            and not any(v and any(m is None for m in v) for v in suites.values())
        ):
            handled_local_ancestor_suite = True
            local_suite_name = next(e[1] for e in entries if e[0] == gn_rel)
            msg_text = (
                f"Bazel test ':{t_name}' ({rule_name}) in '{bazel_rel}' is exported "
                f"via a per-package `bazel_test_suite(\"{local_suite_name}\")` in "
                f"'{gn_rel}'; individual packages must never define a per-package "
                f"`bazel_test_suite()` in `BUILD.gn`."
            )
            rem_text = (
                f"Remove `bazel_test_suite(\"{local_suite_name}\")` (plus "
                f"`\":{local_suite_name}\"` from `group(\"tests\")` and any unused "
                f"`//build/bazel/bazel_test_suite.gni` import) from '{gn_rel}' and "
                f"define `test_suite({pkg_suite_decl_no_vis})` as the "
                f"first target in '{bazel_rel}' (omitting `visibility` on `:{t_name}` "
                f"and on the package `test_suite()` until the aggregation package is "
                f"known). Because no existing aggregation `test_suite()` was found "
                f"automatically for `//{td}`, do not guess or invent an aggregation "
                f"directory: report the resulting unexported test finding in "
                f"`CoderReport.disputed_findings` (`source: \"cq_reachability\"`, "
                f"`category: \"unexported_bazel_test\"`, `file: \"{bazel_rel}\"`) and "
                f"`summary` asking the reviewer which aggregation `test_suite()` (or "
                f"where a new one, with a matching `bazel_test_suite` in that "
                f"directory's `BUILD.gn`, should be added if none exists) the package's "
                f"`test_suite()`(s) should be added to, suggesting asking the Build "
                f"team if they do not know."
            )
            findings.append({
                "source": "cq_reachability",
                "category": "bypassed_ancestor_test_suite",
                "severity": "error",
                "file": bazel_rel,
                "line": lineno,
                "message": msg_text,
                "remediation": rem_text,
            })
        for s_rel, s_name, s_param, s_line in entries:
            if s_param != param:
                findings.append({
                    "source": "cq_reachability",
                    "category": "bazel_test_suite_kind_mismatch",
                    "severity": "error",
                    "file": s_rel,
                    "line": s_line,
                    "message": (
                        f"`bazel_test_suite(\"{s_name}\")` in '{s_rel}' lists {rule_name} ':{t_name}' in "
                        f"`{s_param}`, but {'device tests (`fx_test`)' if kind == 'target' else 'host tests'} "
                        f"belong in `{param}` (they are built in a different Bazel configuration)."
                    ),
                    "remediation": f"Move `//{td}:{t_name}` (or its `test_suite`) from `{s_param}` to `{param}`.",
                })
        local_suite_unwired_via_tests = bool(
            not handled_local_ancestor_suite
            and gn_test_callers
            and not local_gn_tests_referenced
            and all(
                s_rel == gn_rel and not refs_from_parent(parent_gn_src, td, s_name)
                for s_rel, s_name, _, _ in entries
            )
        )
        if local_suite_unwired_via_tests or not any(
            suite_is_wired(s_rel, s_name) for s_rel, s_name, _, _ in entries
        ):
            s_rel, s_name, _, s_line = entries[0]
            if (s_rel, s_name) in unwired_reported:
                continue
            unwired_reported.add((s_rel, s_name))
            findings.append({
                "source": "cq_reachability",
                "category": "unwired_bazel_test_suite",
                "severity": "error",
                "file": s_rel,
                "line": s_line,
                "message": (
                    f"Bazel test ':{t_name}' ({rule_name}) is listed in `bazel_test_suite(\"{s_name}\")` in "
                    f"'{s_rel}', but no reachable `group(\"tests\")` in the GN build graph references that "
                    "suite, so it is not reachable from the test roots and stays out of tests.json."
                ),
                "remediation": (
                    f"Wire `//{td}:{pkg_suite_name}` into the area's aggregation `test_suite()` exported to GN "
                    f"(or reference `//{os.path.dirname(s_rel)}:tests` / `//{os.path.dirname(s_rel)}:{s_name}` "
                    "from the parent directory's `tests` group)."
                    if local_suite_unwired_via_tests
                    else (
                        f"Add `\":{s_name}\"` to `group(\"tests\")` in '{s_rel}' (or reference "
                        f"`//{os.path.dirname(s_rel)}:{s_name}` from the parent directory's `tests` group)."
                    )
                ),
            })

    # Verify that when a package defines both "tests" and "host_tests", both are
    # referenced in the same aggregation package (same BUILD.bazel file).
    if "tests" in suites and "host_tests" in suites:
        ext_t_refs, ext_h_refs = external_package_suite_refs(td)
        ext_t_pkgs = {p for p, _ in ext_t_refs}
        ext_h_pkgs = {p for p, _ in ext_h_refs}
        if ext_t_pkgs and ext_h_pkgs and not (ext_t_pkgs & ext_h_pkgs):
            base_t_pkgs = {
                p
                for p, s in ext_t_refs
                if any(
                    split_label(lbl, p) == (td, "tests")
                    for lbl in (base_bazel_suite_members(p).get(s) or [])
                )
            }
            base_h_pkgs = {
                p
                for p, s in ext_h_refs
                if any(
                    split_label(lbl, p) == (td, "host_tests")
                    for lbl in (base_bazel_suite_members(p).get(s) or [])
                )
            }
            if not (base_t_pkgs and base_h_pkgs and not (base_t_pkgs & base_h_pkgs)):
                best_t = max(ext_t_pkgs, key=lambda p: (p.count("/"), len(p)))
                best_h = max(ext_h_pkgs, key=lambda p: (p.count("/"), len(p)))
                t_pkg_label = f"//{best_t}" if best_t else "//"
                h_pkg_label = f"//{best_h}" if best_h else "//"
                t_bz_rel = f"{best_t}/BUILD.bazel" if best_t else "BUILD.bazel"
                h_bz_rel = f"{best_h}/BUILD.bazel" if best_h else "BUILD.bazel"
                if best_t.count("/") != best_h.count("/"):
                    if best_t.count("/") > best_h.count("/"):
                        canon_pkg, other_pkg = best_t, best_h
                        other_suite = next(
                            s for p, s in ext_h_refs if p == best_h
                        )
                        moved_suite = "host_tests"
                        canon_suite_default = _matching_area_suite_name(
                            canon_pkg, "host_tests"
                        )
                        gn_param = "host_tests"
                        compat_expr = "HOST_OS_CONSTRAINTS"
                    else:
                        canon_pkg, other_pkg = best_h, best_t
                        other_suite = next(
                            s for p, s in ext_t_refs if p == best_t
                        )
                        moved_suite = "tests"
                        canon_suite_default = _matching_area_suite_name(
                            canon_pkg, "target_tests"
                        )
                        gn_param = "target_tests"
                        compat_expr = '["@platforms//os:fuchsia"]'
                    canon_bz = (
                        f"{canon_pkg}/BUILD.bazel" if canon_pkg else "BUILD.bazel"
                    )
                    canon_gn = (
                        f"{canon_pkg}/BUILD.gn" if canon_pkg else "BUILD.gn"
                    )
                    other_bz = (
                        f"{other_pkg}/BUILD.bazel" if other_pkg else "BUILD.bazel"
                    )
                    canon_label = f"//{canon_pkg}" if canon_pkg else "//"
                    canon_gn_suite = (
                        existing_wired_gn_suite_name(canon_pkg) or "bazel_tests"
                    )
                    rem_text = (
                        f"Reference both `\"//{td}:tests\"` and `\"//{td}:host_tests\"` in the "
                        f"same aggregation package '{canon_bz}': remove "
                        f"`\"//{td}:{moved_suite}\"` from `test_suite(name = \"{other_suite}\")` "
                        f"in '{other_bz}', create or update "
                        f"`test_suite(name = \"{canon_suite_default}\", "
                        f"target_compatible_with = {compat_expr}, "
                        f"tests = [\"//{td}:{moved_suite}\"])` in '{canon_bz}', wire "
                        f"`\"{canon_label}:{canon_suite_default}\"` into "
                        f"`bazel_test_suite(\"{canon_gn_suite}\") {{ {gn_param} = "
                        f"[ \"{canon_label}:{canon_suite_default}\" ] }}` in '{canon_gn}', "
                        f"and set `visibility = [\"{canon_label}:__pkg__\"]` on both "
                        f"`test_suite` targets in '{bazel_rel}'."
                    )
                else:
                    rem_text = (
                        f"Reference both `\"//{td}:tests\"` and `\"//{td}:host_tests\"` in the "
                        f"same aggregation package (either '{t_bz_rel}' or '{h_bz_rel}'): "
                        "consolidate both into one aggregation `BUILD.bazel` file (creating "
                        "or updating the companion aggregation `test_suite()` there and in "
                        "its `BUILD.gn` `bazel_test_suite()`) and set "
                        "`visibility = [\"//<aggregation_pkg>:__pkg__\"]` on both "
                        f"`test_suite` targets in '{bazel_rel}'."
                    )
                findings.append({
                    "source": "cq_reachability",
                    "category": "split_package_test_suite_aggregation",
                    "severity": "error",
                    "file": bazel_rel,
                    "line": min(
                        suite_lines.get("tests", 1),
                        suite_lines.get("host_tests", 1),
                    ),
                    "message": (
                        f"Package '//{td}' exposes both '//{td}:tests' (referenced in "
                        f"'{t_bz_rel}', {t_pkg_label}) and '//{td}:host_tests' (referenced in "
                        f"'{h_bz_rel}', {h_pkg_label}), splitting its test suites across "
                        "different aggregation packages."
                    ),
                    "remediation": rem_text,
                })

    # Flag an unreachable hand-written group("tests") above BAZEL2GN SENTINEL when an ancestor
    # BUILD.gn depends directly on the package's generated test target(s) instead of //<td >:tests.
    m_tests_grp = re.search(r'\bgroup\(\s*"tests"\s*\)\s*\{', gn_code(gn_pre))
    if (
        m_tests_grp
        and SENTINEL_RE.search(gn_src)
        and not SUITE_RE.search(gn_code(gn_pre))
        and not gn_tests_group_is_referenced(td)
        and any(direct_gn_test_callers(td, tn) for tn, _, _, _ in test_targets)
    ):
        grp_line = gn_pre.count("\n", 0, m_tests_grp.start()) + 1
        direct_callers = sorted(
            {c for tn, _, _, _ in test_targets for c in direct_gn_test_callers(td, tn)}
        )
        caller_desc = ", ".join(f"'{c}/BUILD.gn'" for c in direct_callers)
        findings.append({
            "source": "cq_reachability",
            "category": "unreachable_local_gn_tests_group",
            "severity": "error",
            "file": gn_rel,
            "line": grp_line,
            "message": (
                f"`group(\"tests\")` above `## BAZEL2GN SENTINEL` in '{gn_rel}' is unreachable in "
                f"the GN build graph because nothing depends on `//{td}:tests` ({caller_desc} "
                "depends directly on the generated test target below the sentinel)."
            ),
            "remediation": (
                f"Remove the unreachable `group(\"tests\")` target above `## BAZEL2GN SENTINEL` "
                f"in '{gn_rel}'."
            ),
        })

    # Check ancestor migrated-test exclusion lists (e.g. `<all_var> - <migrated_var>` in an
    # ancestor BUILD.gn or its imported .gni files feeding `foreach(..., ...) { deps += [ "...:tests" ] }`)
    # and flag leftover empty `group("tests")` stubs or `:tests` deps in `group("<name>")` when all
    # package tests are migrated to Bazel and skipped in bazel2gn.
    def check_ancestor_migrated_tests_list():
        if not SENTINEL_RE.search(gn_src) or not test_targets:
            return
        gn_post_clean = gn_code(gn_src[SENTINEL_RE.search(gn_src).end() :])
        if re.search(
            r"\b(?:rustc_test|go_test|test|fuchsia_unittest_package|fuchsia_test_package)\s*\("
            r"|\bwith_unit_tests\s*=\s*true\b",
            gn_post_clean,
        ):
            return
        gn_pre_clean = gn_code(gn_pre)
        if SUITE_RE.search(gn_pre_clean):
            return
        anc = os.path.dirname(td)
        while anc:
            anc_gn_rel = os.path.join(anc, "BUILD.gn")
            anc_code = read_gn(anc_gn_rel)
            if anc_code and re.search(r'\bforeach\s*\([^)]*\)\s*\{[^}]*:tests"', anc_code, re.S):
                gni_rels = [anc_gn_rel]
                for im in re.finditer(r'\bimport\(\s*"([^"]+\.gni)"\s*\)', anc_code):
                    imp = im.group(1)
                    if not imp.startswith("//"):
                        cand_gni = os.path.normpath(os.path.join(anc, imp))
                        if os.path.isfile(os.path.join(workdir, cand_gni)):
                            gni_rels.append(cand_gni)
                combined = "\n".join(read_gn(r) for r in gni_rels)
                sub_m = re.search(r"\b([A-Za-z_]\w*)\s*-\s*([A-Za-z_]\w*)\b", combined)
                if sub_m:
                    all_var, mig_var = sub_m.group(1), sub_m.group(2)
                    rel_td = os.path.relpath(td, anc)
                    for def_rel in gni_rels:
                        def_code = read_gn(def_rel)
                        all_m = re.search(
                            r"\b" + re.escape(all_var) + r"\s*=\s*\[", def_code
                        )
                        mig_m = re.search(
                            r"\b" + re.escape(mig_var) + r"\s*=\s*\[", def_code
                        )
                        if not (all_m and mig_m):
                            continue
                        all_items = re.findall(
                            r'"([^"]+)"',
                            def_code[all_m.end() : closing_bracket(def_code, all_m.end() - 1)],
                        )
                        mig_items = re.findall(
                            r'"([^"]+)"',
                            def_code[mig_m.end() : closing_bracket(def_code, mig_m.end() - 1)],
                        )
                        entry_key = (
                            rel_td
                            if rel_td in all_items
                            else (f"//{td}" if f"//{td}" in all_items else None)
                        )
                        if not entry_key:
                            continue
                        if entry_key not in mig_items:
                            mig_ln = def_code.count("\n", 0, mig_m.start()) + 1
                            findings.append({
                                "source": "cq_reachability",
                                "category": "unregistered_migrated_test_library",
                                "severity": "error",
                                "file": def_rel,
                                "line": mig_ln,
                                "message": (
                                    f"Tests for `//{td}` are migrated to Bazel and skipped in "
                                    f"`bazel2gn`, but `\"{entry_key}\"` is not listed in "
                                    f"`{mig_var}` in '{def_rel}'."
                                ),
                                "remediation": (
                                    f"Add `\"{entry_key}\"` (in sorted order) to `{mig_var}` "
                                    f"in '{def_rel}', remove the empty `group(\"tests\")` stub "
                                    f"from '{gn_rel}', and drop `\":tests\"` from "
                                    f"`group(\"{os.path.basename(td)}\")` in '{gn_rel}'."
                                ),
                            })
                        if m_tests_grp:
                            grp_ln = gn_pre.count("\n", 0, m_tests_grp.start()) + 1
                            findings.append({
                                "source": "cq_reachability",
                                "category": "empty_gn_tests_group_stub",
                                "severity": "error",
                                "file": gn_rel,
                                "line": grp_ln,
                                "message": (
                                    f"`group(\"tests\")` in '{gn_rel}' is an obsolete stub now "
                                    f"that `//{td}`'s tests are migrated to Bazel and excluded "
                                    f"via `{mig_var}` in '{def_rel}'."
                                ),
                                "remediation": (
                                    f"Ensure `\"{entry_key}\"` is in `{mig_var}` in '{def_rel}', "
                                    f"remove `group(\"tests\")` from '{gn_rel}', and drop "
                                    f"`\":tests\"` from `group(\"{os.path.basename(td)}\")` in "
                                    f"'{gn_rel}'."
                                ),
                            })
                        pkg_grp_body = gn_block(gn_pre_clean, "group", os.path.basename(td))
                        if pkg_grp_body and re.search(r'":tests"', pkg_grp_body):
                            findings.append({
                                "source": "cq_reachability",
                                "category": "dangling_local_tests_group_dep",
                                "severity": "error",
                                "file": gn_rel,
                                "line": 1,
                                "message": (
                                    f"`group(\"{os.path.basename(td)}\")` in '{gn_rel}' still "
                                    f"lists `\":tests\"` in `deps` even though `//{td}`'s tests "
                                    "are migrated to Bazel and skipped in `bazel2gn`."
                                ),
                                "remediation": (
                                    f"Remove `\":tests\"` from `group(\"{os.path.basename(td)}\")` "
                                    f"in '{gn_rel}'."
                                ),
                            })
                        return
            next_anc = os.path.dirname(anc)
            if next_anc == anc:
                break
            anc = next_anc

    check_ancestor_migrated_tests_list()

print(json.dumps(findings, indent=2))
PYEOF
