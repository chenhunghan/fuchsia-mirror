#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Flags test-only data files placed in the runtime `data` attribute of a
# rustc_library/rustc_binary/rustc_proc_macro that defines unit tests
# (with_unit_tests / with_host_unit_tests). Files needed only by the generated
# <name>_test belong in `test_data` (or a host_test_data() target from
# //build/bazel/rules/host_tests/host_test_data.bzl for host tests), not in
# `data`, which ships them with the library and forces rule macros (e.g. host
# test wrappers) to be edited to forward `data`.
#
# Also flags host_test_data_files / host_test_data_map targets listed in `data`
# of a host_rustc_test (host_test_data_in_data): runtime test data belongs in
# `test_data`, which host_rustc_test forwards to its host_test() wrapper.
#
# Also checks GN/Bazel host test data staging parity:
# - Rejects `BUILD.gn` `host_test_data` (and unguarded `copy`/`generated_file`/
#   `action`) targets whose `outputs` write into `$root_build_dir`, which
#   collides across `host_x64` and host variant toolchains in CQ (`gn gen`
#   `Duplicate output file`).
# - Rejects test source edits in dual-built packages that drop GN's
#   `../../<pkg>/...` runtime test-data fallback path while `BUILD.gn` still
#   builds a host test without `$root_out_dir` staging, as well as runtime
#   `.exists()` fallback probing or attribute-level `# @bazel2gn:skip` on
#   `test_data` of an unskipped standalone host test.
# - Flags host_test_data_map / host_test_data_files targets that stage a subset
#   of entries into `test_data/<subdir>/` while omitting tools or files staged
#   into `$root_out_dir/test_data/<subdir>/` by GN `host_test_data()` targets in
#   the migrated packages without documenting the omitted entries in
#   `BUILD.bazel` as pending migration prerequisites
#   (undocumented_omitted_host_test_data).

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"

python3 - "$WORKDIR" "$TARGET_DIR" "$TARGET_DIRS" <<'PYEOF'
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
target_dirs = list(
    dict.fromkeys(
        d.strip().strip("/")
        for d in re.split(r"[,\s]+", f"{sys.argv[2]} {sys.argv[3]}")
        if d.strip().strip("/")
    )
)

RULE_RE = re.compile(r"^(rustc_library|rustc_binary|rustc_proc_macro)\(\s*$", re.M)
TESTY = re.compile(r"(^|[/_.-])(test_?data|testdata|tests?|fixtures?|golden)([/_.-]|$)", re.I)

findings = []


# Emit a standardized check finding entry.
def _emit(findings, category, severity, file, line, message, remediation=""):
    findings.append(
        {
            "source": "test_data_attribute",
            "category": category,
            "severity": severity,
            "file": file,
            "line": line,
            "message": message,
            "remediation": remediation,
        }
    )


# Extract a balanced parenthesized rule call block starting at `start`.
def block_at(text, start):
    depth = 0
    for i in range(start, len(text)):
        c = text[i]
        if c == "(":
            depth += 1
        elif c == ")":
            depth -= 1
            if depth == 0:
                return text[start:i + 1]
    return text[start:]


def list_attr(block, attr):
    m = re.search(r"^\s+" + attr + r"\s*=\s*\[(.*?)\]", block, re.M | re.S)
    if not m:
        return None
    return re.findall(r'"([^"]+)"', m.group(1))


HOST_RUST_TEST_RE = re.compile(r"^host_rustc_test\(\s*$", re.M)
HOST_TEST_DATA_RULE_RE = re.compile(r"^(host_test_data_files|host_test_data_map)\(\s*$", re.M)
_htd_cache = {}


# Names and staged path tokens of host_test_data_files / host_test_data_map targets in `pkg`.
_GENERIC_SEGMENTS = {
    "test_data",
    "testdata",
    "data",
    "host_test_data",
    "keys",
    "repository",
    "blobs",
    "meta",
    "bin",
    "lib",
}


def host_test_data_info(pkg):
    if pkg not in _htd_cache:
        info = {}
        p = os.path.join(workdir, pkg, "BUILD.bazel")
        if os.path.isfile(p):
            t = open(p, encoding="utf-8").read()
            for hm in HOST_TEST_DATA_RULE_RE.finditer(t):
                blk = block_at(t, hm.end() - 1)
                nm = re.search(r'^\s+name\s*=\s*"([^"]+)"', blk, re.M)
                if not nm:
                    continue
                tokens = set()
                dm = re.search(r'^\s+dest_dir\s*=\s*"([^"]+)"', blk, re.M)
                if dm:
                    dd = dm.group(1).strip().strip("/")
                    if dd:
                        tokens.add(dd)
                        parent = os.path.dirname(dd)
                        if parent and parent not in _GENERIC_SEGMENTS:
                            tokens.add(parent)
                        for seg in dd.split("/"):
                            if seg and seg not in _GENERIC_SEGMENTS:
                                tokens.add(seg)
                for src in list_attr(blk, "srcs") or []:
                    base = os.path.basename(src.split(":", 1)[-1]).strip()
                    if base and not tokens:
                        tokens.add(base)
                info[nm.group(1)] = tokens
        _htd_cache[pkg] = info
    return _htd_cache[pkg]


def host_test_data_names(pkg):
    return set(host_test_data_info(pkg).keys())


def parse_host_test_data_label(label, pkg):
    if label.startswith(":"):
        lpkg, name = pkg, label[1:]
    elif label.startswith("//") and ":" in label:
        lpkg, name = label[2:].split(":", 1)
    else:
        return None
    if name in host_test_data_names(lpkg):
        return lpkg, name
    return None


def is_host_test_data_label(label, pkg):
    return parse_host_test_data_label(label, pkg) is not None


# Read all non-BUILD source files in package `pkg` to verify test_data consumption.
def package_source_blob(pkg):
    chunks = []
    root = os.path.join(workdir, pkg)
    if not os.path.isdir(root):
        return ""
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [
            dn
            for dn in dirnames
            if not dn.startswith(".")
            and not os.path.isfile(os.path.join(dirpath, dn, "BUILD.bazel"))
        ]
        for fn in filenames:
            if fn in ("BUILD.bazel", "BUILD.gn", "BUILD"):
                continue
            fpath = os.path.join(dirpath, fn)
            try:
                chunks.append(open(fpath, encoding="utf-8", errors="ignore").read())
            except OSError:
                pass
    return "\n".join(chunks)


# Check that host_test_data targets listed in `test_data` of a host_rustc_test are actually
# referenced by at least one source or test file in the package.
def check_unused_host_test_data(text, d):
    src_blob = None
    for m in HOST_RUST_TEST_RE.finditer(text):
        block = block_at(text, m.end() - 1)
        td_entries = list_attr(block, "test_data") or []
        if not td_entries:
            continue
        name_m = re.search(r'^\s+name\s*=\s*"([^"]+)"', block, re.M)
        name = name_m.group(1) if name_m else "?"
        unused = []
        for entry in td_entries:
            parsed = parse_host_test_data_label(entry, d)
            if not parsed:
                continue
            lpkg, htd_name = parsed
            tokens = host_test_data_info(lpkg).get(htd_name) or set()
            if not tokens:
                continue
            if src_blob is None:
                src_blob = package_source_blob(d)
            if not any(tok in src_blob for tok in tokens):
                unused.append((entry, lpkg, htd_name))
        if not unused:
            continue
        labels_desc = ", ".join(u[0] for u in unused)
        cross_pkgs = sorted({u[1] for u in unused if u[1] != d})
        vis_note = (
            f" Also remove `\"//{d}:__pkg__\"` from the `visibility` list of those "
            f"`host_test_data_*` target(s) in {', '.join(f'{p}/BUILD.bazel' for p in cross_pkgs)}."
            if cross_pkgs
            else ""
        )
        _emit(
            findings,
            "unused_test_data",
            "ERROR",
            os.path.join(d, "BUILD.bazel"),
            text[: m.start()].count("\n") + 1,
            (
                f'host_rustc_test(name = "{name}") specifies `test_data` entries '
                f"({labels_desc}), but no source or test file in //{d} references or "
                "consumes their staged test data paths."
            ),
            (
                f"Remove the unused `test_data` entry/attribute ({labels_desc}) from "
                f'`host_rustc_test(name = "{name}")` in {d}/BUILD.bazel (and update '
                "any skip comment above it that mentions `test_data`)."
                + vis_note
                + " If those targets were in GN `test_deps`, note their removal in "
                "`CoderReport.summary` and dispute any resulting "
                "`review_diff/unit_test_parity` finding in "
                "`CoderReport.disputed_findings` once all deterministic check "
                "scripts pass."
            ),
        )


# host_rustc_test forwards `test_data` to its host_test() wrapper; `data` only
# reaches the inner rustc_test binary. Runtime test data belongs in `test_data`.
def check_host_rustc_test_data(text, d):
    for m in HOST_RUST_TEST_RE.finditer(text):
        block = block_at(text, m.end() - 1)
        bad = [e for e in (list_attr(block, "data") or []) if is_host_test_data_label(e, d)]
        if not bad:
            continue
        name_m = re.search(r'^\s+name\s*=\s*"([^"]+)"', block, re.M)
        name = name_m.group(1) if name_m else "?"
        _emit(
            findings,
            "host_test_data_in_data",
            "ERROR",
            os.path.join(d, "BUILD.bazel"),
            text[:m.start()].count("\n") + 1,
            (
                f"host_rustc_test(name = \"{name}\") lists host test data targets in `data`: "
                f"{', '.join(bad)}. `data` only reaches the inner rustc_test binary; runtime "
                "test data belongs in `test_data`, which is forwarded to the host_test() wrapper."
            ),
            (
                "Move these labels from `data` to `test_data`. bazel2gn cannot translate "
                "`test_data` on host_rustc_test, so mark the test `# @bazel2gn:skip` with an "
                "explanatory comment (migration_sanity T3), export it via the area's aggregation "
                "`test_suite()` (`host_tests`), and drop GN host_test_data targets above the "
                "sentinel that no longer have GN consumers."
            ),
        )


STANDALONE_HOST_TEST_RE = re.compile(r"^(host_rustc_test|host_go_test)\(\s*$", re.M)


# Reject attribute-level `# @bazel2gn:skip` on `test_data` (or `raw_attribute` injecting
# `non_rust_deps` / `data_deps`) on an unskipped standalone host test target.
def check_skipped_test_data_attr_on_host_test(text, d):
    lines = text.splitlines()
    for m in STANDALONE_HOST_TEST_RE.finditer(text):
        rule = m.group(1)
        start_line = text[: m.start()].count("\n") + 1
        idx = start_line - 2
        target_skipped = False
        raw_non_rust_dep = False
        while idx >= 0:
            s = lines[idx].strip()
            if not s:
                idx -= 1
                continue
            if s.startswith("#"):
                if "@bazel2gn:skip" in s:
                    target_skipped = True
                if "@bazel2gn:raw_attribute:" in s and (
                    "non_rust_deps" in s or "data_deps" in s
                ):
                    raw_non_rust_dep = True
                idx -= 1
                continue
            break
        if target_skipped:
            continue
        block = block_at(text, m.end() - 1)
        td_m = re.search(r"^\s+test_data\s*=", block, re.M)
        if not td_m:
            continue
        td_line = text[: m.end() - 1 + td_m.start()].count("\n") + 1
        attr_skipped = "@bazel2gn:skip" in lines[td_line - 1]
        kidx = td_line - 2
        while kidx >= start_line - 1 and not attr_skipped:
            ks = lines[kidx].strip()
            if not ks:
                kidx -= 1
                continue
            if ks.startswith("#"):
                if "@bazel2gn:skip" in ks:
                    attr_skipped = True
                    break
                kidx -= 1
                continue
            break
        if not attr_skipped and not raw_non_rust_dep:
            continue
        name_m = re.search(r'^\s+name\s*=\s*"([^"]+)"', block, re.M)
        name = name_m.group(1) if name_m else "?"
        msg = (
            f'{rule}(name = "{name}") uses `test_data` without target-level '
            "`# @bazel2gn:skip` (using attribute-level `# @bazel2gn:skip` or "
            "`# @bazel2gn:raw_attribute:non_rust_deps`). `bazel2gn` cannot "
            "translate `test_data`, and generating a GN copy produces a host "
            "test whose `$root_build_dir` CWD lacks staged `<dest_dir>` files."
        )
        rem = (
            f'Annotate `{rule}(name = "{name}")` itself with `# @bazel2gn:skip` '
            "and a brief explanatory comment, remove attribute-level "
            "`# @bazel2gn:skip` on `test_data` and any "
            "`# @bazel2gn:raw_attribute:non_rust_deps`, export the test via "
            f"the area's aggregation `test_suite()`, rerun `fx bazel2gn -d {d}`, "
            "and remove hand-written `group(\"tests\")` or parent `:tests` "
            "wiring left without GN test targets."
        )
        _emit(
            findings,
            "skipped_test_data_attr_on_host_test",
            "ERROR",
            os.path.join(d, "BUILD.bazel"),
            td_line if attr_skipped else start_line,
            msg,
            rem,
        )


COMPILE_DATA_RULE_RE = re.compile(
    r"^(rustc_library|rustc_binary|rustc_proc_macro|ffx_plugin|ffx_tool|"
    r"idk_ffx_tool|rustc_binary_host_tool|idk_rustc_binary_host_tool|"
    r"rustc_test|host_rustc_test)\(\s*$",
    re.M,
)


# Flag runtime test fixtures listed in `compile_data` when they are not consumed at
# compile time (e.g. via `include_str!`, `include_bytes!`, `include!`, or `rustc_env`)
# and are instead loaded dynamically at runtime or staged via `host_test_data_*`.
def check_runtime_test_data_in_compile_data(text, d):
    htd_srcs = set()
    for hm in HOST_TEST_DATA_RULE_RE.finditer(text):
        hblk = block_at(text, hm.end() - 1)
        for src in list_attr(hblk, "srcs") or []:
            htd_srcs.add(src.strip())
    src_blob = None
    for m in COMPILE_DATA_RULE_RE.finditer(text):
        rule = m.group(1)
        if rule == "rustc_proc_macro":
            continue
        block = block_at(text, m.end() - 1)
        cd_entries = list_attr(block, "compile_data") or []
        if not cd_entries:
            continue
        if re.search(r"^\s+rustc_env\s*=", block, re.M):
            continue
        bad = []
        for entry in cd_entries:
            in_htd = entry in htd_srcs
            if not (in_htd or TESTY.search(entry)):
                continue
            base = os.path.basename(entry.split(":", 1)[-1]).strip()
            if not base:
                continue
            if src_blob is None:
                src_blob = package_source_blob(d)
            inc_pat = re.compile(
                r"include(?:_(?:str|bytes))?!\s*\([^)]*" + re.escape(base), re.S
            )
            if inc_pat.search(src_blob) or (
                "concat!" in src_blob
                and re.search(r"include(?:_(?:str|bytes))?!", src_blob)
                and base in src_blob
            ):
                continue
            if in_htd or base in src_blob:
                bad.append(entry)
        if not bad:
            continue
        name_m = re.search(r'^\s+name\s*=\s*"([^"]+)"', block, re.M)
        name = name_m.group(1) if name_m else "?"
        cd_m = re.search(r"^\s+compile_data\s*=", block, re.M)
        line = (
            text[: m.end() - 1 + cd_m.start()].count("\n") + 1
            if cd_m
            else text[: m.start()].count("\n") + 1
        )
        labels_desc = ", ".join(bad)
        _emit(
            findings,
            "runtime_test_data_in_compile_data",
            "ERROR",
            os.path.join(d, "BUILD.bazel"),
            line,
            (
                f'{rule}(name = "{name}") lists runtime test fixture(s) in '
                f"`compile_data` ({labels_desc}). `compile_data` is for compile-time "
                "inputs consumed by rustc (such as `include_str!` or `include_bytes!`), "
                "not runtime test fixtures loaded dynamically by tests; specifying "
                "runtime fixtures in `compile_data` exposes test data to the production "
                "build and causes `bazel2gn` to propagate `inputs` into `BUILD.gn`."
            ),
            (
                f"Remove {labels_desc} from `compile_data` (and remove the `compile_data` "
                f'attribute if empty) on `{rule}(name = "{name}")` in {d}/BUILD.bazel, '
                "stage runtime test fixtures on the test target via `test_data` / "
                f"`host_test_data_files`, and re-run `fx bazel2gn -d {d}` if "
                "dual-building. If the file was listed in GN `inputs` at the change "
                "base, note its removal in `CoderReport.summary` and dispute any "
                "resulting `review_diff` finding in `CoderReport.disputed_findings` "
                "once all deterministic check scripts pass."
            ),
        )


# Run a read-only git command in workdir and return stripped output lines.
def git_lines(args):
    try:
        out = subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return []
    return [line.strip() for line in out.splitlines() if line.strip()]


# Resolve the effective base revision for diffing task changes against pre-migration state.
def resolve_change_base():
    raw = (
        os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip()
        or os.environ.get("PLANTER_CHANGE_BASE", "").strip()
        or "HEAD"
    )
    m_jj = re.fullmatch(r"@(-*)", raw)
    if m_jj:
        raw = f"HEAD~{len(m_jj.group(1))}" if m_jj.group(1) else "HEAD"
    subj = (git_lines(["log", "-1", "--format=%s", "HEAD"]) or [""])[0]
    if subj.startswith(("[bazel_migration]", "[bazel2gn]", "[bazel]")):
        parent_ok = bool(git_lines(["rev-parse", "--verify", "--quiet", "HEAD~1^{commit}"]))
        if parent_ok:
            head_files = git_lines(["diff", "--name-only", "HEAD~1", "HEAD"])
            if not target_dirs or any(
                f == td or f.startswith(td + "/") for td in target_dirs for f in head_files
            ):
                head_sha = (git_lines(["rev-parse", "HEAD"]) or [""])[0]
                base_sha = (git_lines(["rev-parse", "--verify", "--quiet", raw]) or [""])[0]
                origin_sha = (
                    git_lines(["rev-parse", "--verify", "--quiet", "origin/main^{commit}"])
                    or [""]
                )[0]
                # Like planter_check_lib.resolve_change_base, never step back from a HEAD
                # already merged on origin/main: its edits are pre-existing upstream code.
                head_on_origin = bool(
                    head_sha
                    and origin_sha
                    and (
                        head_sha == origin_sha
                        or (git_lines(["merge-base", origin_sha, head_sha]) or [""])[0]
                        == head_sha
                    )
                )
                if (raw == "HEAD" or base_sha == head_sha) and not head_on_origin:
                    return "HEAD~1"
    return raw


# Blank out GN comments outside string literals while preserving line numbers and offsets.
def strip_gn_comments(text):
    return re.sub(
        r'("(?:\\.|[^"\\])*")|#[^\n]*',
        lambda m: m.group(1) or " " * len(m.group(0)),
        text,
    )


# Find the closing '}' matching code[start] == '{' outside GN string literals.
def gn_closing_brace(code, start):
    depth = 0
    in_str = False
    i = start
    while i < len(code):
        ch = code[i]
        if in_str:
            if ch == "\\":
                i += 2
                continue
            in_str = ch != '"'
        elif ch == '"':
            in_str = True
        elif ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return len(code) - 1


# Collect `(open_idx, close_idx, condition)` spans for `if (...) { ... }` blocks in GN code.
def gn_if_condition_spans(code):
    spans = []
    if_re = re.compile(r"\bif\s*\(([^()\n]*(?:\([^()\n]*\)[^()\n]*)*)\)\s*\{")
    for m in if_re.finditer(code):
        open_b = m.end() - 1
        close_b = gn_closing_brace(code, open_b)
        spans.append((open_b, close_b, m.group(1).strip()))
    return spans


# Check whether a GN target body assigns `outputs` referencing `$root_build_dir`.
def block_uses_root_build_dir_outputs(body):
    out_Assigns = list(re.finditer(r"\boutputs\s*\+?=\s*\[([^\]]*)\]", body, re.S))
    if not out_Assigns:
        return False
    out_expr = "\n".join(m.group(1) for m in out_Assigns)
    if "$root_build_dir" in out_expr or "${root_build_dir}" in out_expr:
        return True
    for vm in re.finditer(r"\b([A-Za-z_]\w*)\s*=\s*([^\n]+)", body):
        rhs = vm.group(2)
        if "$root_build_dir" in rhs or "${root_build_dir}" in rhs:
            if re.search(r"\b" + re.escape(vm.group(1)) + r"\b", out_expr):
                return True
    return False


# Read the pre-migration contents of `<pkg>/BUILD.gn` at `change_base`, if available.
_base_gn_cache = {}


def read_base_gn(pkg, change_base):
    if pkg not in _base_gn_cache:
        rel = f"{pkg}/BUILD.gn" if pkg else "BUILD.gn"
        try:
            raw = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{change_base}:{rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except Exception:
            raw = ""
        _base_gn_cache[pkg] = strip_gn_comments(raw) if raw else ""
    return _base_gn_cache[pkg]


GN_OUTPUT_RULE_RE = re.compile(
    r'\b(host_test_data|copy|generated_file|action|action_foreach)\s*\(\s*"([^"]+)"\s*\)\s*\{'
)
GN_COMPILED_TARGET_RE = re.compile(
    r"\b(?:rustc_test|rustc_library|rustc_binary|ffx_plugin|ffx_tool|test|"
    r"cc_test_executable|executable|source_set|static_library|shared_library|"
    r'go_test|go_library|go_binary)\s*\(\s*"([^"]+)"\s*\)\s*\{'
)


# Check whether any compiled target in `code` depends on `:<tname>` without a toolchain suffix.
def has_unqualified_compiled_gn_dep(code, pkg, tname):
    dep_pat = re.compile(
        r'"(?::|//' + re.escape(pkg) + r":)" + re.escape(tname) + r'"'
    )
    for cm in GN_COMPILED_TARGET_RE.finditer(code):
        cbody = code[cm.end() - 1 : gn_closing_brace(code, cm.end() - 1) + 1]
        if dep_pat.search(cbody):
            return True
    return False


# Check whether `base_code` already defined `<tmpl>("<tname>")` with `$root_build_dir` outputs.
def existed_with_root_build_dir_in_base(base_code, tmpl, tname):
    if not base_code:
        return False
    pat = re.compile(r"\b" + re.escape(tmpl) + r'\s*\(\s*"' + re.escape(tname) + r'"\s*\)\s*\{')
    for bm in pat.finditer(base_code):
        bbody = base_code[bm.end() - 1 : gn_closing_brace(base_code, bm.end() - 1) + 1]
        if block_uses_root_build_dir_outputs(bbody):
            return True
    return False


# Flag BUILD.gn `host_test_data` (and unguarded copy/action/generated_file) targets whose
# `outputs` write into `$root_build_dir`, which collides across host variant toolchains in CQ.
def check_gn_root_build_dir_outputs(d, change_base):
    gn_rel = os.path.join(d, "BUILD.gn")
    gn_abs = os.path.join(workdir, gn_rel)
    if not os.path.isfile(gn_abs):
        return
    raw_gn = open(gn_abs, encoding="utf-8").read()
    if "root_build_dir" not in raw_gn:
        return
    code = strip_gn_comments(raw_gn)
    if_spans = gn_if_condition_spans(code)
    base_code = read_base_gn(d, change_base)
    for m in GN_OUTPUT_RULE_RE.finditer(code):
        tmpl, tname = m.group(1), m.group(2)
        open_b = m.end() - 1
        body = code[open_b : gn_closing_brace(code, open_b) + 1]
        if not block_uses_root_build_dir_outputs(body):
            continue
        conds = [c for s, e, c in if_spans if s < m.start() < e]
        single_tc = any(
            re.search(r"\bcurrent_toolchain\s*==|==\s*current_toolchain\b", c)
            for c in conds
        )
        was_in_base = existed_with_root_build_dir_in_base(base_code, tmpl, tname)
        line = code.count("\n", 0, m.start()) + 1
        if tmpl == "host_test_data":
            unqual_dep = has_unqualified_compiled_gn_dep(code, d, tname)
            if was_in_base and not unqual_dep:
                continue
            msg = (
                f'host_test_data("{tname}") in {gn_rel} stages `outputs` under '
                "`$root_build_dir`. `$root_build_dir` is shared across all toolchains, "
                "and `//build/testing/host_test_data.gni` adds a dependency on "
                f'`:{tname}(${{toolchain_variant.base}})` when evaluated in a host '
                "variant toolchain (`host_x64-asan-ubsan`, `host_x64-novariant`, "
                "`host_x64-coverage`), causing `gn gen` to fail on CQ variant builders "
                "with `Duplicate output file`."
            )
            rem = (
                "Do not stage `host_test_data` `outputs` under `$root_build_dir` in "
                f"{gn_rel}. Instead, annotate the Bazel host test target itself with "
                "`# @bazel2gn:skip` and an explanatory comment (exporting it via the "
                "area's aggregation `test_suite()` and removing the GN host test from `BUILD.gn`) "
                "so the test uses a single consistent `<dest_dir>/...` path without "
                "runtime `.exists()` probing. If GN genuinely must still build a "
                "separate host test, stage outputs under toolchain-scoped "
                "`$root_out_dir/...` instead of `$root_build_dir/...`."
            )
            _emit(
                findings,
                "host_test_data_root_build_dir_outputs",
                "ERROR",
                gn_rel,
                line,
                msg,
                rem,
            )
        elif not single_tc and not was_in_base:
            msg = (
                f'{tmpl}("{tname}") in {gn_rel} writes `outputs` to `$root_build_dir` '
                "without a single-toolchain `if (current_toolchain == ...)` guard "
                "(`if (is_host)` matches both the base host toolchain and every host "
                "variant toolchain), causing `gn gen` to fail with `Duplicate output "
                "file` on variant CQ builders."
            )
            rem = (
                "Stage toolchain-specific outputs under `$root_out_dir`, "
                "`$target_out_dir`, or `$target_gen_dir` instead of `$root_build_dir`, "
                "guard the target with `if (current_toolchain == default_toolchain)` or "
                "`if (current_toolchain == host_toolchain)` (pinning that toolchain on "
                "all dependents), or remove the redundant GN copy target."
            )
            _emit(
                findings,
                "unguarded_root_build_dir_outputs",
                "ERROR",
                gn_rel,
                line,
                msg,
                rem,
            )


TEST_SRC_EXTS = (".rs", ".cc", ".cpp", ".c", ".h", ".hpp", ".go", ".py")


def _uses_host_test_data(bazel_text):
    """True if BUILD.bazel stages host test data or sets `test_data` on a host test."""
    return bool(
        HOST_TEST_DATA_RULE_RE.search(bazel_text)
        or (
            re.search(r"\b(?:host_rustc_test|host_go_test|host_py_test)\s*\(", bazel_text)
            and re.search(r"^\s+test_data\s*=", bazel_text, re.M)
        )
    )


def _per_file_source_diff(d, change_base):
    """Maps each changed test source file under `d` to its removed/added lines and the
    first changed line number in the post-image."""
    try:
        diff_out = subprocess.check_output(
            ["git", "-C", workdir, "diff", "-U0", change_base, "--", d],
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except Exception:
        return {}
    per_file = {}
    cur_file = None
    for raw in diff_out.splitlines():
        if raw.startswith("+++ b/"):
            cur_file = raw[6:].strip()
            per_file.setdefault(cur_file, {"rem": [], "add": [], "line": 1})
            continue
        if not cur_file:
            continue
        hm = re.match(r"^@@ -\d+(?:,\d+)? \+(\d+)", raw)
        if hm and per_file[cur_file]["line"] == 1:
            per_file[cur_file]["line"] = max(1, int(hm.group(1)))
            continue
        if raw.startswith("-") and not raw.startswith("--- "):
            per_file[cur_file]["rem"].append(raw[1:])
        elif raw.startswith("+") and not raw.startswith("+++ "):
            per_file[cur_file]["add"].append(raw[1:])
    return {f: info for f, info in per_file.items() if f.endswith(TEST_SRC_EXTS)}


def _in_nested_subpackage(rel_file, d):
    """True if `rel_file` is owned by a nested subpackage under `d`."""
    sub_parent = os.path.dirname(rel_file)
    while sub_parent and sub_parent != d and sub_parent.startswith(d + "/"):
        if os.path.isfile(os.path.join(workdir, sub_parent, "BUILD.bazel")) or os.path.isfile(
            os.path.join(workdir, sub_parent, "BUILD.gn")
        ):
            return True
        sub_parent = os.path.dirname(sub_parent)
    return False


# Check that test source edits in a dual-built package do not drop GN's `../../<pkg>/...`
# runtime test-data fallback path while `BUILD.gn` still builds a host test without
# `$root_out_dir` staging.
def check_dual_build_test_data_fallback(bazel_text, d, change_base):
    if not _uses_host_test_data(bazel_text):
        return
    gn_abs = os.path.join(workdir, d, "BUILD.gn")
    if not os.path.isfile(gn_abs):
        return
    gn_clean = strip_gn_comments(open(gn_abs, encoding="utf-8").read())
    has_gn_test = bool(
        re.search(
            r"\b(?:rustc_test|go_test|test|cc_test_executable|python_host_test)\s*\("
            r"|\bwith_unit_tests\s*=\s*true\b",
            gn_clean,
        )
    )
    if not has_gn_test or "root_out_dir" in gn_clean:
        return
    for rel_file, info in sorted(_per_file_source_diff(d, change_base).items()):
        full_file = os.path.join(workdir, rel_file)
        if not os.path.isfile(full_file):
            continue
        if _in_nested_subpackage(rel_file, d):
            continue
        rem_paths = re.findall(r'"(\.\./\.\./[^"\n]+)"', "\n".join(info["rem"]))
        if not rem_paths:
            continue
        cur_src = open(full_file, encoding="utf-8", errors="ignore").read()
        add_blob = "\n".join(info["add"])
        if "../../" in cur_src or re.search(
            r"current_exe|os\.Executable|executable_path", add_blob
        ):
            continue
        msg = (
            f'Test code in {rel_file} removed the GN checkout-relative test data path '
            f'`"{rem_paths[0]}"` while {d}/BUILD.gn still builds a host test without '
            "`$root_out_dir` staging. Bazel host tests run in `<test>.runtime_dir` "
            "(where `host_test_data_*` stages files under `dest_dir`), whereas GN host "
            "tests run in `$root_build_dir` where source-relative `host_test_data` "
            "files remain at `../../<pkg>/...`."
        )
        rem = (
            "Annotate the Bazel host test target itself with `# @bazel2gn:skip` and an "
            f"explanatory comment in {d}/BUILD.bazel (exporting it via the area's aggregation "
            f"`test_suite()`), rerun `fx bazel2gn -d {d}`, and remove the GN host test "
            f"from {d}/BUILD.gn so {rel_file} uses a single consistent `<dest_dir>/...` "
            "path. Do NOT add runtime `.exists()` fallback probing or copy "
            "`host_test_data` into `$root_build_dir` (which fails variant CQ `gn gen`)."
        )
        _emit(
            findings,
            "dual_build_test_data_fallback_dropped",
            "ERROR",
            rel_file,
            info["line"],
            msg,
            rem,
        )


RUNTIME_PATH_PROBE_RE = re.compile(
    r"\.(?:exists|is_file|try_exists)\s*\(|fs::metadata\s*\(|os\.Stat\s*\(|os\.path\.exists\s*\("
)


# Reject runtime `.exists()` / `os.Stat` fallback probing between a Bazel `<dest_dir>` path
# and a GN checkout-relative `"../../..."` path in modified test source files.
def check_runtime_test_data_path_probing(bazel_text, d, change_base):
    if not _uses_host_test_data(bazel_text):
        return
    for rel_file, info in sorted(_per_file_source_diff(d, change_base).items()):
        if not info["add"] or _in_nested_subpackage(rel_file, d):
            continue
        add_blob = "\n".join(info["add"])
        if not RUNTIME_PATH_PROBE_RE.search(add_blob):
            continue
        if not re.search(r'"\.\./\.\./[^"\n]+"', add_blob):
            continue
        msg = (
            f"Test code in {rel_file} introduces runtime path-existence fallback probing "
            "between a Bazel staged `test_data` path and a GN `../../` checkout-relative "
            "path. Host tests must use a single consistent, build-predictable path rather "
            "than probing ambient working-directory paths at runtime."
        )
        rem = (
            f"Replace runtime `.exists()` fallback probing in {rel_file} with a single "
            "consistent `<dest_dir>/...` path matching `host_test_data_files` / "
            f"`host_test_data_map`, annotate the Bazel host test in {d}/BUILD.bazel "
            "with `# @bazel2gn:skip` and an explanatory comment (exporting it via the "
            f"area's aggregation `test_suite()`), and remove the GN host test from {d}/BUILD.gn "
            "(do NOT copy `host_test_data` into `$root_build_dir`)."
        )
        _emit(
            findings,
            "runtime_test_data_path_probing",
            "ERROR",
            rel_file,
            info["line"],
            msg,
            rem,
        )


# Map each `test_data/<subdir>` destination of a host_test_data_map / host_test_data_files
# rule block to the basenames it stages there.
def host_test_data_dest_entries(rule_kind, blk):
    dests = {}
    if rule_kind == "host_test_data_map":
        fm_m = re.search(r"^\s*files_map\s*=\s*\{(.*?)\}", blk, re.M | re.S)
        if fm_m:
            for dest_key, _ in re.findall(r'"([^"]+)"\s*:\s*"([^"]+)"', fm_m.group(1)):
                parts = dest_key.strip("/").split("/")
                if len(parts) >= 3 and parts[0] == "test_data":
                    dests.setdefault(f"{parts[0]}/{parts[1]}", set()).add(parts[-1])
    else:
        dm = re.search(r'^\s*dest_dir\s*=\s*"([^"]+)"', blk, re.M)
        if dm:
            parts = dm.group(1).strip("/").split("/")
            if len(parts) >= 2 and parts[0] == "test_data":
                names = dests.setdefault(f"{parts[0]}/{parts[1]}", set())
                for src in list_attr(blk, "srcs") or []:
                    bname = os.path.basename(src.split(":", 1)[-1]).strip()
                    if bname:
                        names.add(bname)
    return dests


# Collect GN host_test_data staged basenames per destination directory across migrated
# packages (current and change-base `BUILD.gn`).
def _collect_gn_host_test_data_by_dest(pkgs, change_base):
    by_dest = {}
    out_re = re.compile(
        r"\$root_out_dir/(test_data/[^/\"{}]+)/(?:\{\{source_file_part\}\}|([^\"{}]+))"
    )
    for pkg in pkgs:
        gn_path = os.path.join(workdir, pkg, "BUILD.gn")
        codes = []
        if os.path.isfile(gn_path):
            try:
                codes.append(
                    strip_gn_comments(open(gn_path, encoding="utf-8", errors="ignore").read())
                )
            except OSError:
                pass
        base_code = read_base_gn(pkg, change_base)
        if base_code:
            codes.append(base_code)
        for code in codes:
            for m in GN_OUTPUT_RULE_RE.finditer(code):
                if m.group(1) != "host_test_data":
                    continue
                htd_name = m.group(2)
                open_b = m.end() - 1
                blk = code[open_b : gn_closing_brace(code, open_b) + 1]
                outs = list_attr(blk, "outputs") or []
                for out_str in outs:
                    om = out_re.search(out_str)
                    if not om:
                        continue
                    dest_dir = om.group(1).strip("/")
                    explicit_file = om.group(2)
                    dest_map = by_dest.setdefault(dest_dir, {})
                    origin = f"//{pkg}:{htd_name}"
                    if explicit_file:
                        bname = os.path.basename(explicit_file.strip("/"))
                        if bname:
                            dest_map.setdefault(bname, origin)
                        continue
                    src_m = re.search(r"^\s*sources\s*=\s*\[(.*?)\]", blk, re.M | re.S)
                    if not src_m:
                        continue
                    src_body = re.sub(r"get_label_info\([^)]*\)", "", src_m.group(1))
                    for s_lit in re.findall(r'"([^"]+)"', src_body):
                        bname = os.path.basename(s_lit.strip().rstrip("/"))
                        if bname and not bname.startswith(("$", "{")):
                            dest_map.setdefault(bname, origin)
    return by_dest


# Collect Bazel host_test_data_map and host_test_data_files staged basenames per destination dir.
def _collect_bazel_host_test_data_by_dest(pkgs):
    by_dest = {}
    for pkg in pkgs:
        bz_path = os.path.join(workdir, pkg, "BUILD.bazel")
        if not os.path.isfile(bz_path):
            continue
        try:
            text = open(bz_path, encoding="utf-8", errors="ignore").read()
        except OSError:
            continue
        for hm in HOST_TEST_DATA_RULE_RE.finditer(text):
            blk = block_at(text, hm.end() - 1)
            for dest_dir, names in host_test_data_dest_entries(hm.group(1), blk).items():
                by_dest.setdefault(dest_dir, set()).update(names)
    return by_dest


# Check that host_test_data_map / host_test_data_files documents any omitted GN host_test_data
# tools or files that were staged into the same test_data/<subdir>/ directory.
def check_omitted_host_test_data_entries(text, d, gn_by_dest, bazel_by_dest):
    comments_blob = "\n".join(
        ln for ln in text.splitlines() if ln.lstrip().startswith("#")
    )
    for hm in HOST_TEST_DATA_RULE_RE.finditer(text):
        rule_kind = hm.group(1)
        blk = block_at(text, hm.end() - 1)
        nm = re.search(r'^\s+name\s*=\s*"([^"]+)"', blk, re.M)
        tname = nm.group(1) if nm else "?"
        target_dests = set(host_test_data_dest_entries(rule_kind, blk))
        for dest_dir in sorted(target_dests):
            gn_entries = gn_by_dest.get(dest_dir, {})
            staged = bazel_by_dest.get(dest_dir, set())
            missing = [
                b
                for b in sorted(gn_entries)
                if b not in staged
                and not re.search(r"\b" + re.escape(b) + r"\b", comments_blob)
            ]
            if not missing:
                continue
            omitted_desc = ", ".join(f"`{b}` (from `{gn_entries[b]}`)" for b in missing)
            names_list = ", ".join(f"`{b}`" for b in missing)
            line_no = text[: hm.start()].count("\n") + 1
            bz_rel = os.path.join(d, "BUILD.bazel")
            _emit(
                findings,
                "undocumented_omitted_host_test_data",
                "ERROR",
                bz_rel,
                line_no,
                (
                    f'{rule_kind}(name = "{tname}") stages files into `{dest_dir}/`, '
                    f"but omits GN `host_test_data` entries ({omitted_desc}) without "
                    "documenting them as pending migration prerequisites."
                ),
                (
                    f"Either stage {names_list} in `{rule_kind}(name = \"{tname}\")` in "
                    f"`{bz_rel}` if their producing targets/prebuilts are exported in "
                    f"Bazel, or document each omitted tool/file ({omitted_desc}) in a "
                    f"comment above `{tname}` (or above the `non_go_deps` / test data "
                    f"target in `{bz_rel}`) as a pending migration prerequisite (e.g. "
                    "`# Not staged yet: <file> needs <producer> exported to Bazel (pending "
                    "migration prerequisite).`; avoid \"kept/remains/stays in GN\" wording, "
                    "which stale_gn_retention_comment_in_bazel rejects)."
                ),
            )


# Repo-relative paths changed since `change_base` (`$PLANTER_CHANGED_FILES` when set).
def changed_files(change_base):
    env_changed = os.environ.get("PLANTER_CHANGED_FILES")
    if env_changed is not None:
        return [l.strip() for l in env_changed.splitlines() if l.strip()]
    return git_lines(["diff", "--name-only", change_base])


change_base = resolve_change_base()
changed = changed_files(change_base)
# Packages whose BUILD.bazel or BUILD.gn changed; they stand in for target_dirs when none
# are given.
changed_pkg_dirs = list(
    dict.fromkeys(
        os.path.dirname(p).strip("/")
        for p in changed
        if os.path.basename(p) in ("BUILD.bazel", "BUILD.gn") and os.path.dirname(p).strip("/")
    )
)
if not target_dirs:
    target_dirs = list(changed_pkg_dirs)
all_migrated_dirs = list(dict.fromkeys(target_dirs + changed_pkg_dirs))
gn_check_dirs = list(target_dirs)
for p in changed:
    if os.path.basename(p) == "BUILD.gn":
        gd = os.path.dirname(p).strip("/")
        if gd and gd not in gn_check_dirs:
            gn_check_dirs.append(gd)

for gd in gn_check_dirs:
    check_gn_root_build_dir_outputs(gd, change_base)

gn_htd_by_dest = _collect_gn_host_test_data_by_dest(all_migrated_dirs, change_base)
bazel_htd_by_dest = _collect_bazel_host_test_data_by_dest(all_migrated_dirs)

for d in target_dirs:
    path = os.path.join(workdir, d, "BUILD.bazel")
    if not os.path.isfile(path):
        continue
    text = open(path, encoding="utf-8").read()
    check_host_rustc_test_data(text, d)
    check_skipped_test_data_attr_on_host_test(text, d)
    check_unused_host_test_data(text, d)
    check_runtime_test_data_in_compile_data(text, d)
    check_dual_build_test_data_fallback(text, d, change_base)
    check_runtime_test_data_path_probing(text, d, change_base)
    check_omitted_host_test_data_entries(text, d, gn_htd_by_dest, bazel_htd_by_dest)
    for m in RULE_RE.finditer(text):
        block = block_at(text, m.end() - 1)
        if not re.search(r"^\s+with_(host_)?unit_tests\s*=", block, re.M):
            continue
        name_m = re.search(r'^\s+name\s*=\s*"([^"]+)"', block, re.M)
        name = name_m.group(1) if name_m else "?"
        data = list_attr(block, "data") or []
        testy = [e for e in data if TESTY.search(e)]
        if not testy:
            continue
        line = text[:m.start()].count("\n") + 1
        _emit(
            findings,
            "test_only_data_in_data",
            "ERROR",
            os.path.join(d, "BUILD.bazel"),
            line,
            (
                f"{m.group(1)}(name = \"{name}\") defines unit tests but lists test-only "
                f"files in `data`: {', '.join(testy)}. `data` is runtime data of the "
                "library itself and ships to every dependent."
            ),
            (
                "Move files used only by the generated <name>_test to `test_data` (for host "
                "tests, a host_test_data() target from "
                "//build/bazel/rules/host_tests/host_test_data.bzl when the test needs them at "
                "a stable runtime path). Do not edit shared rule macros (.bzl) to forward "
                "`data` to work around this, and do not change sources to locate the files. "
                "Example: data = [\"test_data/main.json\"] -> test_data = [\"test_data/main.json\"]."
            ),
        )

print(json.dumps(findings, indent=2))
PYEOF
