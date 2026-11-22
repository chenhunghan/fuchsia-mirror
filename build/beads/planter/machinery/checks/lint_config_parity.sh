#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check for lint-driven source edits during GN-to-Bazel migrations.
#
# 1. lint_driven_source_edit (error): any non-build file changed in the migration
#    change (git diff $PLANTER_CHANGE_BASE) whose diff hunks look like a lint/warning
#    "fix" (removed .clone(), added #[allow]/#[expect]/NOLINT, unused-import
#    removal, `let _ =` insertion, etc.). Reports exact file:line evidence.
# 2. rust_edition_downgrade (error): a migrated Rust target in BUILD.bazel that
#    downgrades `edition` relative to BUILD.gn at $PLANTER_CHANGE_BASE, injects
#    unrequested `allow_legacy_edition = true` via `@bazel2gn:raw_attribute:`, or
#    adds unrequested Rust safety-lint suppressions (`unsafe_attr_outside_unsafe`,
#    `missing_unsafe_on_extern`, `unsafe_op_in_unsafe_fn`) to mask pre-existing
#    compile bitrot on unwired targets.
# 3. lint_config_semantics (warning): BUILD.bazel `lint_config = "<label>"` whose
#    label is not under //build/config/rust/lints and whose resolved
#    rust_lint_config target either cannot be resolved or omits the default
#    clippy/rustc lints from //build/config/rust/lints. In Bazel, lint_config
#    REPLACES the macro default lint set, while GN `configs += [...]` / GN
#    lint_config APPENDS to the defaults; an area rust_lint_config must therefore
#    include the default clippy and rustc lint sets.
# 4. unrequested_disable_clippy (error): `disable_clippy = True` or
#    `tags = ["noclippy"]` added to a Rust target in BUILD.bazel when the base
#    BUILD.gn did not set `disable_clippy = true`.
# 5. ineffective_gn_rustflags_lint_allow (error): `-A<lint>` in `rustc_flags` /
#    `rustflags` on a bazel2gn-synced Rust target for a lint in GN's default Rust
#    lint configs (`warn_default` / `deny_default` / `werror`), which GN places
#    before `configs` on the rustc command line and overrides with `-W<lint>` and
#    `-Dwarnings`.
# 6. gn_non_test_deps_leaked_to_test (error): a `rustc_test` / `host_rustc_test`
#    in BUILD.bazel that sets `crate = ":<lib>"` when `:<lib>` had `non_test_deps`
#    in the base BUILD.gn, leaking test-excluded dependencies into the unit test.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"

python3 - "$WORKDIR" "$TARGET_DIR" "$SCRIPT_DIR" <<'PYEOF'
import ast
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
target_dir = sys.argv[2].strip().strip("/")
sys.path.insert(0, sys.argv[3])
import planter_check_lib as pcl

ALLOWED_BASENAMES = {"BUILD.gn", "BUILD.bazel", "BUILD", "MODULE.bazel"}
ALLOWED_SUFFIXES = (".gni", ".bzl", ".bazelrc")


def git(args):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return ""


def is_build_file(path):
    base = os.path.basename(path)
    return (
        base in ALLOWED_BASENAMES
        or base.endswith(ALLOWED_SUFFIXES)
        or base.startswith("WORKSPACE")
    )


LINT_PATTERNS = [
    ("removed", re.compile(r"\.clone\(\)"), "removed a .clone() call (clippy redundant_clone style fix)"),
    ("removed", re.compile(r"^\s*use\s+[\w:{}, *]+;\s*$"), "removed a `use` import (unused-import style fix)"),
    ("added", re.compile(r"#!?\[\s*(allow|expect)\s*\("), "added a lint suppression attribute"),
    ("added", re.compile(r"NOLINT|clippy::"), "added a lint suppression/annotation"),
    ("added", re.compile(r"^\s*let\s+_\s*="), "added `let _ =` (unused-result style fix)"),
]

findings = []


def _emit(findings, category, severity, file, line, message, remediation=""):
    findings.append({"source": "lint_config_parity", "category": category, "severity": severity, "file": file, "line": line, "message": message, "remediation": remediation})


# Resolve effective change base and changed files via planter_check_lib so
# uncommitted working-tree edits, stacked prerequisite commits, upstream-relocated
# target directories, and build/secondary/<dir> overlays are handled consistently.
raw_tdirs = f"{os.environ.get('PLANTER_TARGET_DIRS', '')} {target_dir}".strip()
target_dirs = pcl.with_overlaid_target_dirs(
    pcl.parse_target_dirs(raw_tdirs, workdir)
)
change_base = pcl.resolve_change_base(workdir, target_dirs=target_dirs)
cfiles = pcl.changed_files(workdir, change_base, target_dirs=target_dirs)
non_build = [p for p in cfiles if p and not is_build_file(p)]
diff_texts = (
    [git(["diff", "--unified=0", change_base, "--"] + non_build)]
    if non_build
    else []
)

# Check whether a Rust source file belongs to a migrated package whose BUILD.bazel
# applies `//build/config/rust/lints:clippy_warn_all` to a test target (in Bazel,
# `clippy_warn_all` bakes `_CLIPPY_WARN_PRODUCTION` in, whereas GN `rustc_test`
# omits `clippy_warn_production`).
def has_clippy_warn_all_on_rust_test(rs_path):
    if not rs_path.endswith(".rs"):
        return False
    pkg = os.path.dirname(rs_path).strip("/")
    while pkg:
        bzl_path = os.path.join(workdir, pkg, "BUILD.bazel")
        if os.path.isfile(bzl_path):
            try:
                text = open(bzl_path, encoding="utf-8").read()
            except OSError:
                return False
            has_warn_all = "//build/config/rust/lints:clippy_warn_all" in text
            has_test = any(
                tok in text
                for tok in (
                    "rustc_test(",
                    "host_rustc_test(",
                    "with_unit_tests",
                    "with_host_unit_tests",
                )
            )
            return has_warn_all and has_test
        pkg = os.path.dirname(pkg).strip("/")
    return False


seen = set()
seen_allowed_test_files = set()
for text in diff_texts:
    cur_file = None
    new_line = 0
    for raw in text.splitlines():
        if raw.startswith("+++ "):
            p = raw[4:].strip()
            cur_file = p[2:] if p.startswith("b/") else (None if p == "/dev/null" else p)
            continue
        if raw.startswith("--- "):
            continue
        m = re.match(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@", raw)
        if m:
            new_line = int(m.group(1))
            continue
        if not cur_file or is_build_file(cur_file):
            continue
        if raw.startswith("+"):
            kind, body, line = "added", raw[1:], new_line
            new_line += 1
        elif raw.startswith("-"):
            kind, body, line = "removed", raw[1:], new_line
        else:
            continue
        for pkind, rx, why in LINT_PATTERNS:
            if pkind == kind and rx.search(body):
                if has_clippy_warn_all_on_rust_test(cur_file):
                    if cur_file not in seen_allowed_test_files:
                        seen_allowed_test_files.add(cur_file)
                        _emit(
                            findings,
                            "allowed_test_clippy_warn_all_edit",
                            "INFO",
                            cur_file,
                            line,
                            (
                                f"Allowed clippy_warn_all test lint fix ({why}) in '{cur_file}' "
                                "to preserve clippy coverage without disable_clippy = True."
                            ),
                        )
                    continue
                key = (cur_file, line, why)
                if key in seen:
                    continue
                seen.add(key)
                _emit(
                    findings,
                    "lint_driven_source_edit",
                    "error",
                    cur_file,
                    line,
                    f"Migration {why} in non-build file '{cur_file}': `{body.strip()[:120]}`. "
                    "GN-to-Bazel migrations must not edit sources to silence lints/warnings.",
                    "Revert the source edit (git checkout $PLANTER_CHANGE_BASE -- <file>). "
                    "Pre-existing lint findings are out of scope. If the Bazel build newly reports this "
                    "lint, restore lint parity in BUILD.bazel (lint_config, rustc_flags, testonly, "
                    "features) instead; if impossible, stop and report the blocker.",
                )

# lint_config semantics check for BUILD.bazel files in the target package tree:
# resolve custom lint_config labels (including through alias() chains and loaded .bzl
# constants) and warn only when the target cannot be resolved to a rust_lint_config or
# when its clippy/rustc dicts omit the default lints from //build/config/rust/lints.
LINTS_PKG = "build/config/rust/lints"
BAZEL_BUILD_FILES = ("BUILD.bazel", "BUILD")


def read_rel(rel):
    try:
        with open(os.path.join(workdir, rel), encoding="utf-8") as f:
            return f.read()
    except Exception:
        return None


def parse_rel(rel):
    text = read_rel(rel)
    if text is None:
        return None
    try:
        return ast.parse(text, filename=rel)
    except Exception:
        return None


def resolve_label(label, cur_pkg):
    if not label or label.startswith("@"):
        return None
    if label.startswith(":"):
        return cur_pkg, label[1:]
    if label.startswith("//"):
        body = label[2:]
        if ":" in body:
            p, n = body.split(":", 1)
        else:
            p, n = body, body.rsplit("/", 1)[-1]
        return p.strip("/"), n
    return None


def eval_dict_branches(node, env):
    """Evaluates a Starlark dict / variable / BitOr / select() into a list of branch dicts."""
    if node is None:
        return None
    if isinstance(node, ast.Dict):
        d = {}
        for k, v in zip(node.keys, node.values):
            if (
                isinstance(k, ast.Constant)
                and isinstance(k.value, str)
                and isinstance(v, ast.Constant)
                and isinstance(v.value, str)
            ):
                d[k.value] = v.value
        return [d]
    if isinstance(node, ast.Name):
        return env.get(node.id)
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.BitOr):
        left = eval_dict_branches(node.left, env)
        right = eval_dict_branches(node.right, env)
        if not left or not right:
            return left or right
        return [{**dl, **dr} for dl in left for dr in right]
    if (
        isinstance(node, ast.Call)
        and isinstance(node.func, ast.Name)
        and node.func.id == "select"
        and node.args
        and isinstance(node.args[0], ast.Dict)
    ):
        branches = []
        for val_node in node.args[0].values:
            sub = eval_dict_branches(val_node, env)
            if sub:
                branches.extend(sub)
        return branches or None
    return None


_bzl_cache = {}
_pkg_cache = {}


def eval_bzl_file(rel, depth=0):
    if rel in _bzl_cache:
        return _bzl_cache[rel]
    if depth > 5:
        return {}
    tree = parse_rel(rel)
    if tree is None:
        _bzl_cache[rel] = {}
        return {}
    cur_pkg = os.path.dirname(rel)
    env = {}
    for node in tree.body:
        if (
            isinstance(node, ast.Expr)
            and isinstance(node.value, ast.Call)
            and isinstance(node.value.func, ast.Name)
            and node.value.func.id == "load"
            and node.value.args
            and isinstance(node.value.args[0], ast.Constant)
            and isinstance(node.value.args[0].value, str)
        ):
            resolved = resolve_label(node.value.args[0].value, cur_pkg)
            if resolved:
                bpkg, bfile = resolved
                brel = os.path.join(bpkg, bfile) if bpkg else bfile
                benv = eval_bzl_file(brel, depth + 1)
                for arg in node.value.args[1:]:
                    if isinstance(arg, ast.Constant) and isinstance(arg.value, str) and arg.value in benv:
                        env[arg.value] = benv[arg.value]
                for kw in node.value.keywords:
                    if (
                        kw.arg
                        and isinstance(kw.value, ast.Constant)
                        and isinstance(kw.value.value, str)
                        and kw.value.value in benv
                    ):
                        env[kw.arg] = benv[kw.value.value]
        elif isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            branches = eval_dict_branches(node.value, env)
            if branches is not None:
                env[node.targets[0].id] = branches
    _bzl_cache[rel] = env
    return env


def eval_pkg_targets(pkg):
    if pkg in _pkg_cache:
        return _pkg_cache[pkg]
    tree = None
    for base in BAZEL_BUILD_FILES:
        rel = os.path.join(pkg, base) if pkg else base
        tree = parse_rel(rel)
        if tree is not None:
            break
    if tree is None:
        _pkg_cache[pkg] = {}
        return {}
    env = {}
    out = {}
    for node in tree.body:
        if (
            isinstance(node, ast.Expr)
            and isinstance(node.value, ast.Call)
            and isinstance(node.value.func, ast.Name)
            and node.value.func.id == "load"
            and node.value.args
            and isinstance(node.value.args[0], ast.Constant)
            and isinstance(node.value.args[0].value, str)
        ):
            resolved = resolve_label(node.value.args[0].value, pkg)
            if resolved:
                bpkg, bfile = resolved
                brel = os.path.join(bpkg, bfile) if bpkg else bfile
                benv = eval_bzl_file(brel)
                for arg in node.value.args[1:]:
                    if isinstance(arg, ast.Constant) and isinstance(arg.value, str) and arg.value in benv:
                        env[arg.value] = benv[arg.value]
                for kw in node.value.keywords:
                    if (
                        kw.arg
                        and isinstance(kw.value, ast.Constant)
                        and isinstance(kw.value.value, str)
                        and kw.value.value in benv
                    ):
                        env[kw.arg] = benv[kw.value.value]
        elif isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            branches = eval_dict_branches(node.value, env)
            if branches is not None:
                env[node.targets[0].id] = branches
        elif isinstance(node, ast.Expr) and isinstance(node.value, ast.Call) and isinstance(node.value.func, ast.Name):
            call = node.value
            rule = call.func.id
            tname = None
            actual = None
            clippy_branches = None
            rustc_branches = None
            for kw in call.keywords:
                if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                    tname = kw.value.value
                elif kw.arg == "actual" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                    actual = kw.value.value
                elif kw.arg == "clippy":
                    clippy_branches = eval_dict_branches(kw.value, env)
                elif kw.arg == "rustc":
                    rustc_branches = eval_dict_branches(kw.value, env)
            if tname:
                out[tname] = {
                    "rule": rule,
                    "actual": actual,
                    "clippy": clippy_branches,
                    "rustc": rustc_branches,
                }
    _pkg_cache[pkg] = out
    return out


def resolve_lint_config_target(pkg, name, depth=0):
    if depth > 5:
        return None
    t = eval_pkg_targets(pkg).get(name)
    if not t:
        return None
    if t["rule"] == "alias" and t["actual"]:
        r = resolve_label(t["actual"], pkg)
        if r:
            return resolve_lint_config_target(r[0], r[1], depth + 1)
    return pkg, name, t


def extract_label_strings(node):
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        yield node.value, getattr(node, "lineno", 1)
    elif isinstance(node, ast.Dict):
        for v in node.values:
            yield from extract_label_strings(v)
    elif isinstance(node, ast.Call):
        for a in node.args:
            yield from extract_label_strings(a)
    elif isinstance(node, ast.BinOp):
        yield from extract_label_strings(node.left)
        yield from extract_label_strings(node.right)


default_lints_targets = eval_pkg_targets(LINTS_PKG)
default_clippy_dict = (
    (default_lints_targets.get("clippy_warn_default", {}).get("clippy") or [{}])[0]
)
production_clippy_dict = (
    (default_lints_targets.get("clippy_warn_production", {}).get("clippy") or [{}])[0]
)
default_rustc_dict = (
    (default_lints_targets.get("clippy_warn_default", {}).get("rustc") or [{}])[0]
)


# Strip `#` comments from GN text while preserving string literals.
def strip_gn_comments(text):
    out = []
    in_str = False
    i, n = 0, len(text or "")
    while i < n:
        ch = text[i]
        if in_str:
            if ch == "\\" and i + 1 < n:
                out.append(text[i : i + 2])
                i += 2
                continue
            if ch == '"':
                in_str = False
        elif ch == '"':
            in_str = True
        elif ch == "#":
            nl = text.find("\n", i)
            i = n if nl < 0 else nl
            continue
        out.append(ch)
        i += 1
    return "".join(out)


GN_BLOCK_HEADER_RE = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{')
GN_NON_TARGET_CALLS = {
    "template",
    "declare_args",
    "foreach",
    "forward_variables_from",
    "if",
}
RUST_SAFETY_LINTS = (
    "unsafe_attr_outside_unsafe",
    "missing_unsafe_on_extern",
    "unsafe_op_in_unsafe_fn",
)
RUST_SAFETY_FLAG_RE = re.compile(
    r"^(?:-A|--allow[= ])(" + "|".join(RUST_SAFETY_LINTS) + r")$"
)
EDITION_REMEDIATION = (
    "Keep the same `edition` and Rust safety lint rigor as `BUILD.gn` at "
    "`$PLANTER_CHANGE_BASE` (remove `allow_legacy_edition = true` and any newly "
    "added safety-lint allowances). If this target was unwired in the default GN "
    "build graph and already fails to compile at `$PLANTER_CHANGE_BASE` due to "
    "pre-existing source bitrot, do not edit `.rs` sources or weaken build settings: "
    "revert all migration changes (including any collateral "
    "`//build/config/rust/lints` edits), leave the directory unmigrated with "
    '`modified_files: []` and `test_footers: ["fx lint"]`, and report the '
    "pre-existing compile failure in `CoderReport.summary`."
)


# Extract explicit integer `edition` values per target name from base BUILD.gn.
def parse_base_gn_target_editions(clean_gn):
    editions = {}
    n = len(clean_gn)
    for m in GN_BLOCK_HEADER_RE.finditer(clean_gn):
        tmpl, tname = m.group(1), m.group(2)
        if tmpl in GN_NON_TARGET_CALLS:
            continue
        depth, i, in_str = 1, m.end(), False
        while i < n and depth > 0:
            ch = clean_gn[i]
            if in_str:
                if ch == "\\" and i + 1 < n:
                    i += 2
                    continue
                if ch == '"':
                    in_str = False
            elif ch == '"':
                in_str = True
            elif ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
            i += 1
        body = clean_gn[m.end() : max(m.end(), i - 1)]
        for em in re.finditer(r'\bedition\s*=\s*"(\d{4})"', body):
            editions.setdefault(tname, set()).add(int(em.group(1)))
    return editions


# Check for Rust edition downgrades and unrequested legacy-edition or safety-lint overrides.
def check_rust_edition_and_safety_lints(
    rel, cur_pkg, raw_bzl, tree, clean_base_gn, base_editions
):
    if not clean_base_gn.strip():
        return
    for idx, bzl_line in enumerate(raw_bzl.splitlines(), start=1):
        if re.search(
            r"#\s*@bazel2gn:raw_attribute:.*\ballow_legacy_edition\s*=\s*true\b",
            bzl_line,
        ) and not re.search(r"\ballow_legacy_edition\s*=\s*true\b", clean_base_gn):
            _emit(
                findings,
                "rust_edition_downgrade",
                "error",
                rel,
                idx,
                (
                    f"Line {idx} in {rel} injects `allow_legacy_edition = true` via "
                    f"`# @bazel2gn:raw_attribute:`, which was absent from "
                    f"`{cur_pkg}/BUILD.gn` at the change base."
                ),
                EDITION_REMEDIATION,
            )
    pkg_tmap = eval_pkg_targets(cur_pkg) if cur_pkg != LINTS_PKG else {}
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        rule_name = node.func.id if isinstance(node.func, ast.Name) else ""
        tname = "<target>"
        edition_kw = None
        rustc_flags_kw = None
        rustc_dict_kw = None
        for kw in node.keywords:
            if (
                kw.arg == "name"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
            ):
                tname = kw.value.value
            elif (
                kw.arg == "edition"
                and isinstance(kw.value, ast.Constant)
                and isinstance(kw.value.value, str)
                and re.fullmatch(r"\d{4}", kw.value.value)
            ):
                edition_kw = kw
            elif kw.arg == "rustc_flags":
                rustc_flags_kw = kw
            elif kw.arg == "rustc" and rule_name == "rust_lint_config":
                rustc_dict_kw = kw
        if edition_kw is not None and tname in base_editions:
            bz_ed = int(edition_kw.value.value)
            min_gn_ed = min(base_editions[tname])
            if bz_ed < min_gn_ed:
                _emit(
                    findings,
                    "rust_edition_downgrade",
                    "error",
                    rel,
                    getattr(edition_kw, "lineno", getattr(node, "lineno", 1)),
                    (
                        f"Target '{tname}' in {rel} downgrades Rust `edition` from "
                        f'"{min_gn_ed}" (in `{cur_pkg}/BUILD.gn` at the change base) '
                        f'to "{bz_ed}". Migrations must preserve the target\'s Rust '
                        "edition."
                    ),
                    EDITION_REMEDIATION,
                )
        if rustc_flags_kw is not None:
            for sub in ast.walk(rustc_flags_kw.value):
                if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                    fm = RUST_SAFETY_FLAG_RE.match(sub.value.strip())
                    if fm and fm.group(1) not in clean_base_gn:
                        _emit(
                            findings,
                            "rust_edition_downgrade",
                            "error",
                            rel,
                            getattr(
                                sub, "lineno", getattr(rustc_flags_kw, "lineno", 1)
                            ),
                            (
                                f"Target '{tname}' in {rel} adds `{sub.value.strip()}` "
                                f"in `rustc_flags`, which was not enabled in "
                                f"`{cur_pkg}/BUILD.gn` at the change base."
                            ),
                            EDITION_REMEDIATION,
                        )
        if rustc_dict_kw is not None and tname not in clean_base_gn:
            t_rustc = (pkg_tmap.get(tname) or {}).get("rustc") or []
            allowed_safety = sorted(
                {
                    sl
                    for sl in RUST_SAFETY_LINTS
                    if sl not in clean_base_gn
                    and any(b.get(sl) == "allow" for b in t_rustc)
                }
            )
            if allowed_safety:
                _emit(
                    findings,
                    "rust_edition_downgrade",
                    "error",
                    rel,
                    getattr(rustc_dict_kw, "lineno", getattr(node, "lineno", 1)),
                    (
                        f"`rust_lint_config` target '{tname}' in {rel} newly "
                        f"suppresses Rust safety lint(s) "
                        f"({', '.join(allowed_safety)}) that were not allowed in "
                        f"`{cur_pkg}/BUILD.gn` at the change base."
                    ),
                    EDITION_REMEDIATION,
                )


def read_base_gn(pkg_dir):
    gn_rel = f"{pkg_dir}/BUILD.gn" if pkg_dir else "BUILD.gn"
    gn_txt = git(["show", f"{change_base}:{gn_rel}"])
    if gn_txt or not pkg_dir or pkg_dir.startswith("build/secondary/"):
        return gn_txt, gn_rel
    sec_rel = f"build/secondary/{pkg_dir}/BUILD.gn"
    sec_txt = git(["show", f"{change_base}:{sec_rel}"])
    if sec_txt:
        return sec_txt, sec_rel
    return "", gn_rel


GN_RUST_TEMPLATES = {
    "rustc_library",
    "rustc_staticlib",
    "rustc_binary",
}

BAZEL_RUST_RULES = {
    "rustc_library",
    "rustc_binary",
    "rustc_staticlib",
    "rustc_proc_macro",
    "rustc_cdylib",
    "rustc_dylib",
    "rustc_test",
    "host_rustc_test",
    "rust_library",
    "rust_binary",
    "rust_test",
    "rust_proc_macro",
    "rust_static_library",
    "fuchsia_rust_dso",
    "ffx_plugin",
    "ffx_tool",
    "idk_ffx_tool",
    "rustc_binary_host_tool",
    "idk_rustc_binary_host_tool",
}

# Lints configured in GN's default_common_rust_configs (warn_default / deny_default /
# 2018_idioms) and //build/config:werror. In GN, target `rustflags` are placed on the
# rustc command line BEFORE `configs`, so `-A<lint>` in `rustflags` is overridden by
# the later `-W<lint>` / `-D<lint>` / `-Dwarnings` flags from `configs`.
DEFAULT_GN_RUST_LINTS = {
    "unused_crate_dependencies",
    "missing_unsafe_on_extern",
    "unsafe_attr_outside_unsafe",
    "deref_nullptr",
    "invalid_value",
    "unused_must_use",
    "rust_2018_idioms",
    "warnings",
}

ALLOW_LINT_FLAG_RE = re.compile(
    r"^(?:-A|--allow(?:=|\s+))(?:rustc::)?([A-Za-z0-9_-]+)$"
)


# Extract non_test_deps per Rust target from a base BUILD.gn file.
def parse_base_gn_non_test_deps(clean_gn):
    if not clean_gn:
        return {}
    out = {}
    for m in GN_BLOCK_HEADER_RE.finditer(clean_gn):
        tmpl, tname = m.group(1), m.group(2)
        if tmpl not in GN_RUST_TEMPLATES:
            continue
        depth, idx = 1, m.end()
        while idx < len(clean_gn) and depth > 0:
            if clean_gn[idx] == "{":
                depth += 1
            elif clean_gn[idx] == "}":
                depth -= 1
            idx += 1
        body = clean_gn[m.end() : idx - 1]
        nt_m = re.search(r"\bnon_test_deps\s*\+?=\s*\[([^\]]*)\]", body, re.S)
        if nt_m:
            deps = re.findall(r'"([^"]+)"', nt_m.group(1))
            if deps:
                out[tname] = deps
    return out


# Check whether a target or attribute line in BUILD.bazel is preceded by # @bazel2gn:skip.
def has_bazel2gn_skip_above(bzl_lines, lineno):
    if lineno <= 0 or lineno > len(bzl_lines):
        return False
    if re.search(r"#\s*@bazel2gn:skip\b", bzl_lines[lineno - 1]):
        return True
    idx = lineno - 2
    while idx >= 0:
        s = bzl_lines[idx].strip()
        if not s.startswith("#"):
            break
        if re.search(r"#\s*@bazel2gn:skip\b", s):
            return True
        idx -= 1
    return False


# Collect string constants from a Starlark list/expression AST node.
def extract_list_strings(node):
    if node is None:
        return []
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return [node.value]
    if isinstance(node, (ast.List, ast.Tuple)):
        out = []
        for el in node.elts:
            out.extend(extract_list_strings(el))
        return out
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        return extract_list_strings(node.left) + extract_list_strings(node.right)
    return []


# Check a Rust target call for ineffective GN rustflags -A<lint> and leaked non_test_deps.
def check_rust_target_lint_and_test_deps(
    rel,
    cur_pkg,
    base_gn_rel,
    node,
    rule_name,
    target_name,
    bzl_lines,
    has_gn_sentinel,
    base_non_test_deps,
):
    call_line = getattr(node, "lineno", 1)
    target_skipped = has_bazel2gn_skip_above(bzl_lines, call_line)
    crate_lib = None
    for kw in node.keywords:
        if (
            kw.arg == "crate"
            and isinstance(kw.value, ast.Constant)
            and isinstance(kw.value.value, str)
            and kw.value.value.startswith(":")
        ):
            crate_lib = kw.value.value[1:]

    if (
        rule_name in ("rustc_test", "host_rustc_test")
        and crate_lib
        and crate_lib in base_non_test_deps
        and not target_skipped
    ):
        nt_list = base_non_test_deps[crate_lib]
        shown_nt = ", ".join(f'`"{d}"`' for d in nt_list)
        _emit(
            findings,
            "gn_non_test_deps_leaked_to_test",
            "error",
            rel,
            call_line,
            (
                f"Test target '{target_name}' in {rel} sets `crate = \":{crate_lib}\"`, "
                "which causes both Bazel and `bazel2gn` (`inlineRustTestCrates`) to copy "
                f"all `deps` of `:{crate_lib}` into `{target_name}`, including {shown_nt} "
                f"from `non_test_deps` in `{base_gn_rel}` at the change base "
                "(excluded from unit tests because it is unused under `#[cfg(test)]` and "
                "triggers `-Dunused-crate-dependencies`)."
            ),
            (
                f"Keep {shown_nt} in `deps` of `:{crate_lib}`, but on `{target_name}` in "
                f"{rel} remove `crate = \":{crate_lib}\"` (and any "
                '`rustc_flags = ["-Aunused-crate-dependencies"]` or `tags = ["noclippy"]`), '
                f"explicitly set `srcs` (plus `crate_root` if non-default and `edition` "
                f"matching `:{crate_lib}`), and set `deps` to the common library `deps` "
                f"plus `test_deps` without {shown_nt}, then re-run `fx bazel2gn -d {cur_pkg}`."
            ),
        )

    if not has_gn_sentinel or target_skipped or rule_name not in BAZEL_RUST_RULES:
        return

    for kw in node.keywords:
        if kw.arg not in ("rustc_flags", "rustflags"):
            continue
        kw_line = getattr(kw, "lineno", call_line)
        if has_bazel2gn_skip_above(bzl_lines, kw_line):
            continue
        for flag_str in extract_list_strings(kw.value):
            m = ALLOW_LINT_FLAG_RE.match(flag_str.strip())
            if not m:
                continue
            lint_norm = m.group(1).replace("-", "_")
            if lint_norm not in DEFAULT_GN_RUST_LINTS:
                continue
            _emit(
                findings,
                "ineffective_gn_rustflags_lint_allow",
                "error",
                rel,
                kw_line,
                (
                    f"Target '{target_name}' in {rel} passes `{flag_str}` in `{kw.arg}`, "
                    f"which `bazel2gn` converts to `rustflags = [\"{flag_str}\"]` in "
                    f"`{cur_pkg}/BUILD.gn`. In GN, target `rustflags` are placed on the "
                    "rustc command line BEFORE `configs` (`//build/config/rust/lints:"
                    "warn_default`, `deny_default`, and `//build/config:werror`), so GN's "
                    f"default `-W{lint_norm}` and `-Dwarnings` flags override `{flag_str}` "
                    "and fail the GN build on CQ."
                ),
                (
                    f"Remove `{flag_str}` from `{kw.arg}` on `{target_name}` in {rel}. If "
                    f"`{target_name}` is a `rustc_test` or `host_rustc_test` using "
                    "`crate = \":<lib>\"` where `:<lib>` has dependencies only used under "
                    "`#[cfg(not(test))]` (such as `non_test_deps` in the base BUILD.gn), "
                    "remove `crate = \":<lib>\"` (and any `tags = [\"noclippy\"]`) and "
                    "instead set `srcs` (plus `crate_root` if non-default and `edition` "
                    "matching `:<lib>`) and `deps` directly on the test target with only "
                    "the dependencies used by the test, then re-run "
                    f"`fx bazel2gn -d {cur_pkg}`. Otherwise, if the base BUILD.gn target "
                    "suppressed this lint via `configs += [\"//build/config/rust/lints:"
                    "allow_...\"]`, use `build_flags` or `# @bazel2gn:raw_attribute:"
                    "configs += [...]` instead of `rustc_flags`."
                ),
            )


candidate_bzl_rels = []
seen_bzl_rels = set()
for scan_td in target_dirs:
    root = os.path.join(workdir, scan_td)
    if not os.path.isdir(root):
        continue
    for dirpath, _dirs, files in os.walk(root):
        if "BUILD.bazel" not in files:
            continue
        path = os.path.join(dirpath, "BUILD.bazel")
        rel = os.path.relpath(path, workdir)
        if rel not in seen_bzl_rels and not rel.startswith("build/config/rust/lints"):
            seen_bzl_rels.add(rel)
            candidate_bzl_rels.append(rel)
for cp in cfiles:
    if (
        cp
        and os.path.basename(cp) == "BUILD.bazel"
        and cp not in seen_bzl_rels
        and not cp.startswith("build/config/rust/lints")
        and os.path.isfile(os.path.join(workdir, cp))
    ):
        seen_bzl_rels.add(cp)
        candidate_bzl_rels.append(cp)

for rel in candidate_bzl_rels:
    cur_pkg = os.path.dirname(rel).strip("/")
    tree = parse_rel(rel)
    if tree is None:
        continue
    raw_bzl = read_rel(rel) or ""
    bzl_lines = raw_bzl.splitlines()
    cur_gn = read_rel(f"{cur_pkg}/BUILD.gn") or ""
    has_gn_sentinel = "BAZEL2GN SENTINEL" in cur_gn
    base_gn, base_gn_rel = read_base_gn(cur_pkg)
    base_bzl = git(["show", f"{change_base}:{rel}"])
    clean_base_gn = strip_gn_comments(base_gn)
    base_editions = parse_base_gn_target_editions(clean_base_gn)
    base_non_test_deps = parse_base_gn_non_test_deps(clean_base_gn)
    check_rust_edition_and_safety_lints(
        rel, cur_pkg, raw_bzl, tree, clean_base_gn, base_editions
    )
    for idx, bzl_line in enumerate(bzl_lines, start=1):
        if re.search(
            r"#\s*@bazel2gn:raw_attribute:.*\bexclude_toolchain_tags\s*\+?=",
            bzl_line,
        ):
            _emit(
                findings,
                "raw_attribute_for_supported_bazel2gn_tag",
                "error",
                rel,
                idx,
                (
                    f"Line {idx} in {rel} uses `# @bazel2gn:raw_attribute:` to emit "
                    "`exclude_toolchain_tags`, which `bazel2gn` supports natively."
                ),
                (
                    'Use `tags = ["exclude_toolchain_tags=<tag>", ...]` (plus '
                    "`target_compatible_with = HOST_OS_CONSTRAINTS` when host-only, "
                    "or `disable_instrumentation = True` on `fx_cc_*` for "
                    '`"instrumented"`) instead of `# @bazel2gn:raw_attribute:`. '
                    "If the binary is wrapped by `bazel_host_tool` in BUILD.gn "
                    "and marked `# @bazel2gn:skip`, remove the `raw_attribute`."
                ),
            )
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        rule_name = node.func.id if isinstance(node.func, ast.Name) else ""
        is_testonly = False
        lint_kw = None
        disable_clippy_kw = None
        noclippy_tag_kw = None
        target_name = "<target>"
        for kw in node.keywords:
            if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                target_name = kw.value.value
            elif kw.arg == "testonly" and isinstance(kw.value, ast.Constant) and kw.value.value is True:
                is_testonly = True
            elif kw.arg == "lint_config":
                lint_kw = kw
            elif (
                kw.arg == "disable_clippy"
                and isinstance(kw.value, ast.Constant)
                and kw.value.value is True
            ):
                disable_clippy_kw = kw
            elif (
                kw.arg == "tags"
                and rule_name in BAZEL_RUST_RULES
                and "noclippy" in extract_list_strings(kw.value)
            ):
                noclippy_tag_kw = kw
        check_rust_target_lint_and_test_deps(
            rel,
            cur_pkg,
            base_gn_rel,
            node,
            rule_name,
            target_name,
            bzl_lines,
            has_gn_sentinel,
            base_non_test_deps,
        )
        clippy_suppression_kw = disable_clippy_kw or noclippy_tag_kw
        if clippy_suppression_kw is not None:
            if not re.search(r"\bdisable_clippy\s*=\s*true\b", base_gn) and not (
                noclippy_tag_kw is not None
                and disable_clippy_kw is None
                and '"noclippy"' in base_bzl
            ):
                attr_desc = (
                    "`disable_clippy = True`"
                    if disable_clippy_kw is not None
                    else '`tags = ["noclippy"]`'
                )
                _emit(
                    findings,
                    "unrequested_disable_clippy",
                    "error",
                    rel,
                    getattr(clippy_suppression_kw, "lineno", getattr(node, "lineno", 1)),
                    (
                        f"Target '{target_name}' in {rel} sets {attr_desc}, "
                        f"but `{base_gn_rel}` did not set `disable_clippy = true` at "
                        "the change base. Disabling clippy loses lint coverage during migration."
                    ),
                    (
                        f"Remove {attr_desc} from BUILD.bazel. If '{target_name}' is a "
                        "`rustc_test` / `host_rustc_test` using `crate = \":<lib>\"` where "
                        "`:<lib>` has `non_test_deps` (`#[cfg(not(test))]`-only dependencies "
                        "that trigger `unused_crate_dependencies`), remove `crate = \":<lib>\"`, "
                        '`tags = ["noclippy"]`, and `rustc_flags = ["-Aunused-crate-dependencies"]` '
                        "and instead declare `srcs`, `edition`, and test `deps` (excluding "
                        "`non_test_deps`) directly on the test target. If `clippy_warn_all` "
                        "on a Rust test target in Bazel surfaces a `_CLIPPY_WARN_PRODUCTION` "
                        "lint (such as `clippy::redundant_clone`, which GN `rustc_test` "
                        "omitted), fix the lint or add a narrow `#[allow(clippy::<lint>)]` "
                        "in the test source file instead of disabling clippy on the target."
                    ),
                )
        if lint_kw is None:
            continue
        is_test_target = is_testonly or rule_name in ("rustc_test", "rust_test")
        req_clippy = default_clippy_dict if is_test_target else production_clippy_dict
        for label, lineno in extract_label_strings(lint_kw.value):
            if label == "//conditions:default":
                continue
            resolved = resolve_label(label, cur_pkg)
            if not resolved:
                continue
            lpkg, lname = resolved
            if lpkg == LINTS_PKG:
                continue
            target_res = resolve_lint_config_target(lpkg, lname)
            if not target_res or target_res[2]["rule"] != "rust_lint_config":
                _emit(
                    findings,
                    "lint_config_semantics",
                    "warning",
                    rel,
                    lineno,
                    f'lint_config = "{label}" does not resolve to a `rust_lint_config` target. '
                    "Bazel lint_config REPLACES the macro default lint set while GN appends to it.",
                    "Point `lint_config` at a valid `rust_lint_config` target (or pre-existing alias) "
                    "that includes the default clippy and rustc lint sets from //build/config/rust/lints.",
                )
                continue
            def_pkg, def_name, tinfo = target_res
            clippy_branches = tinfo.get("clippy") or []
            rustc_branches = tinfo.get("rustc") or []
            missing_clippy = sorted({
                k for k in req_clippy
                if k != "all" and (not clippy_branches or any(k not in b for b in clippy_branches))
            })
            missing_rustc = sorted({
                k for k in default_rustc_dict
                if not rustc_branches or any(k not in b for b in rustc_branches)
            })
            if missing_clippy or missing_rustc:
                missing_desc = []
                if missing_clippy:
                    missing_desc.append(f"clippy lints ({', '.join(missing_clippy[:5])})")
                if missing_rustc:
                    missing_desc.append(f"rustc lints ({', '.join(missing_rustc[:5])})")
                _emit(
                    findings,
                    "lint_config_semantics",
                    "warning",
                    rel,
                    lineno,
                    f'lint_config = "{label}" (resolving to //{def_pkg}:{def_name}) omits default '
                    f"{' and '.join(missing_desc)} from //build/config/rust/lints. "
                    "Bazel lint_config REPLACES the macro default lint set while GN appends to it, "
                    "so omitting the defaults changes the effective lint set.",
                    f"Compose `//{def_pkg}:{def_name}` with the default clippy/rustc lint constants from "
                    "//build/config/rust/lints so the Bazel lint set matches GN's append semantics.",
                )

print(json.dumps(findings, indent=2))
PYEOF
