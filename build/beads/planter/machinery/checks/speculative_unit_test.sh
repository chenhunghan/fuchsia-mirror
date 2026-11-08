#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Flags three classes of speculative or no-op Rust unit test defects:
# 1. speculative_empty_unit_test: a Bazel rustc_test / host_rustc_test with
#    crate = ":<lib>" (or with_unit_tests on the library) whose crate sources
#    contain no test functions, when the library's GN target at
#    $PLANTER_CHANGE_BASE had no with_unit_tests (or set with_unit_tests = true
#    without ever wiring :<lib>_test into the GN build graph).
# 2. speculative_host_unit_test: a wrap_host_rust_test wrapper for a unit test
#    that only ran on Fuchsia in base BUILD.gn.
# 3. redundant_empty_unit_test_todo: a newly added # TODO comment in BUILD.bazel
#    about omitted unit tests or with_unit_tests on a Rust crate whose .rs
#    sources contain no tests (where GN with_unit_tests = true was a no-op).

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-}"
BASE="${PLANTER_CHANGE_BASE:-HEAD~1}"

python3 - "$WORKDIR" "$TARGET_DIR $TARGET_DIRS" "$BASE" <<'PYEOF'
import ast
import json
import os
import re
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
target_dirs = list(
    dict.fromkeys(
        d.strip().strip("/") for d in re.split(r"[,\s]+", sys.argv[2]) if d.strip().strip("/")
    )
)
base = sys.argv[3]

RUST_CRATE_RULES = {
    "rustc_library",
    "rustc_binary",
    "rustc_proc_macro",
    "rustc_staticlib",
    "rustc_dylib",
    "rust_library",
    "rust_binary",
    "rust_proc_macro",
    "ffx_plugin",
    "ffx_tool",
    "idk_ffx_tool",
    "rustc_binary_host_tool",
    "idk_rustc_binary_host_tool",
}
SPECULATIVE_TEST_LIB_RULES = {"rustc_library", "rustc_binary"}
RUST_UNIT_TEST_RULES = {"rustc_test", "host_rustc_test"}

HAS_TEST = re.compile(
    r"#\[\s*(?:[\w:]+::)?test(?:_case)?\b"
    r"|#\[\s*cfg\b[^\]]*\btest\b"
    r"|\(\s*test\s*\)\s*\]"
    r"|\bproptest!\s*[\(\{]"
)
TODO_MARKER_RE = re.compile(
    r"\b(?:TODO|FIXME)\b|Needs reviewer attention|confirm with owners",
    re.I,
)
UNIT_TEST_TOPIC_RE = re.compile(
    r"\bwith_(?:host_)?unit_tests\b"
    r"|(?:unit[- ]tests?|\b[A-Za-z0-9_]+_tests?\b).*?\b(?:omit(?:ted|ting)?|unreferenced|"
    r"not\s+(?:wired|referenced|migrated|built)|no\s+(?:unit\s+)?tests?)"
    r"|\b(?:no\s+(?:unit\s+)?tests?|defines?\s+no\s+tests?|contains?\s+no\s+tests?|"
    r"has\s+no\s+tests?)\b.*?\b(?:unit[- ]tests?|with_(?:host_)?unit_tests|"
    r"\b[A-Za-z0-9_]+_tests?\b)",
    re.I,
)

REMEDIATION = """\
The crate has no unit tests in its '.rs' sources and no wired unit test existed in GN,
so this adds an empty test binary (and package) that tests nothing. Remove the unit test
target / with_unit_tests, any fx_unittest_package or test_suite entry that wraps it, and
unused loads (without adding a '# TODO' comment in BUILD.bazel), then rerun
`fx bazel2gn -d <dir>`. Migrate only tests that actually existed and ran in GN."""

findings = []


def _emit(findings, category, severity, file, line, message, remediation=""):
    findings.append(
        {
            "source": "speculative_unit_test",
            "category": category,
            "severity": severity,
            "file": file,
            "line": line,
            "message": message,
            "remediation": remediation,
        }
    )


def block_at(text, start, open_c="(", close_c=")"):
    depth = 0
    for i in range(start, len(text)):
        if text[i] == open_c:
            depth += 1
        elif text[i] == close_c:
            depth -= 1
            if depth == 0:
                return text[start : i + 1]
    return text[start:]


# Resolve the pre-migration git revision and read files at that base commit.
def _normalize_git_rev(rev):
    rev = (rev or "").strip()
    if rev.startswith("@"):
        minus_cnt = len(rev) - 1
        if all(c == "-" for c in rev[1:]):
            return f"HEAD~{minus_cnt}" if minus_cnt > 0 else "HEAD"
    return rev


def resolve_base_ref():
    resolved_env = os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip()
    eff_base = _normalize_git_rev(resolved_env) if resolved_env else (_normalize_git_rev(base) or "HEAD")
    try:
        head_sha = subprocess.check_output(
            ["git", "-C", workdir, "rev-parse", "--verify", "HEAD"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
        base_sha = subprocess.check_output(
            ["git", "-C", workdir, "rev-parse", "--verify", f"{eff_base}^{{commit}}"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
    except Exception:
        head_sha, base_sha = "", ""
    if head_sha and head_sha == base_sha:
        try:
            head_subj = subprocess.check_output(
                ["git", "-C", workdir, "log", "-1", "--format=%s", "HEAD"],
                stderr=subprocess.DEVNULL,
                text=True,
            ).strip()
            head_files = subprocess.check_output(
                ["git", "-C", workdir, "diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
                stderr=subprocess.DEVNULL,
                text=True,
            ).splitlines()
            if (
                head_subj.startswith("[bazel_migration]")
                and target_dirs
                and any(
                    any(f == td or f.startswith(td + "/") for td in target_dirs)
                    for f in head_files
                )
            ):
                return "HEAD~1"
        except Exception:
            pass
    return eff_base


BASE_REF = resolve_base_ref()


def git_show_at_base(path, fallback_refs=()):
    for ref in dict.fromkeys([BASE_REF, *fallback_refs]):
        if not ref:
            continue
        r = subprocess.run(
            ["git", "-C", workdir, "show", f"{ref}:{path}"],
            capture_output=True,
            text=True,
        )
        if r.returncode == 0:
            return r.stdout
    return None


def git_show(path):
    return git_show_at_base(path, fallback_refs=("HEAD~1", "origin/main"))


# Parse Starlark string values and Rust crate / test targets from BUILD.bazel.
def _eval_starlark_strs(node, top_vars):
    if node is None:
        return []
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return [node.value]
    if isinstance(node, ast.Name):
        return list(top_vars.get(node.id, ()))
    if isinstance(node, (ast.List, ast.Tuple)):
        out = []
        for el in node.elts:
            out.extend(_eval_starlark_strs(el, top_vars))
        return out
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        return _eval_starlark_strs(node.left, top_vars) + _eval_starlark_strs(node.right, top_vars)
    if isinstance(node, ast.Call):
        out = []
        for arg in node.args:
            out.extend(_eval_starlark_strs(arg, top_vars))
        for kw in node.keywords:
            out.extend(_eval_starlark_strs(kw.value, top_vars))
        return out
    if isinstance(node, ast.Dict):
        out = []
        for val in node.values:
            out.extend(_eval_starlark_strs(val, top_vars))
        return out
    return []


def _ast_has_glob(node):
    if node is None:
        return False
    return any(
        isinstance(ch, ast.Call) and isinstance(ch.func, ast.Name) and ch.func.id == "glob"
        for ch in ast.walk(node)
    )


def _walk_package_rs_files(pkg):
    out = []
    for root, dirs, names in os.walk(pkg):
        dirs[:] = [
            x
            for x in dirs
            if not os.path.isfile(os.path.join(root, x, "BUILD.bazel"))
            and not os.path.isfile(os.path.join(root, x, "BUILD.gn"))
        ]
        out.extend(os.path.join(root, n) for n in sorted(names) if n.endswith(".rs"))
    return out


def _resolve_crate_sources(d, kws, top_vars):
    pkg = os.path.join(workdir, d)
    src_attrs = ("srcs", "args_srcs", "crate_root", "compile_data")
    if any(_ast_has_glob(kws.get(a)) for a in src_attrs):
        return _walk_package_rs_files(pkg)
    raw = []
    for attr in src_attrs:
        raw.extend(_eval_starlark_strs(kws.get(attr), top_vars))
    paths = []
    for entry in raw:
        s = entry.strip()
        if not s.endswith(".rs") or "$" in s or s.startswith("@"):
            continue
        if s.startswith("//") and ":" in s:
            l_pkg, l_rel = s[2:].split(":", 1)
            paths.append(os.path.join(workdir, l_pkg.strip("/"), l_rel.lstrip("/")))
        else:
            paths.append(os.path.join(pkg, s.lstrip(":")))
    return paths if paths else _walk_package_rs_files(pkg)


def sources_have_tests(paths):
    for p in paths:
        try:
            if HAS_TEST.search(open(p, encoding="utf-8", errors="ignore").read()):
                return True
        except OSError:
            pass
    return False


def _const_str(node):
    return node.value if isinstance(node, ast.Constant) and isinstance(node.value, str) else None


def parse_bazel_rust_targets(d, text):
    try:
        tree = ast.parse(text, filename=os.path.join(d, "BUILD.bazel"))
    except SyntaxError:
        return None
    top_vars = {}
    calls_by_line = {}
    libs = {}
    unit_tests = []
    host_wrappers = []
    for stmt in tree.body:
        if isinstance(stmt, ast.Assign):
            for tgt in stmt.targets:
                if isinstance(tgt, ast.Name):
                    top_vars[tgt.id] = _eval_starlark_strs(stmt.value, top_vars)
        elif isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call):
            call = stmt.value
            if not isinstance(call.func, ast.Name):
                continue
            rule = call.func.id
            kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
            name = _const_str(kws.get("name"))
            calls_by_line[stmt.lineno] = (rule, name)
            if rule in RUST_CRATE_RULES and name:
                srcs = _resolve_crate_sources(d, kws, top_vars)
                has_wut = any(
                    k in kws
                    and not (isinstance(kws[k], ast.Constant) and kws[k].value is False)
                    for k in ("with_unit_tests", "with_host_unit_tests")
                )
                libs[name] = {
                    "rule": rule,
                    "line": stmt.lineno,
                    "sources": srcs,
                    "has_tests": sources_have_tests(srcs),
                    "has_with_unit_tests": has_wut,
                }
            elif rule in RUST_UNIT_TEST_RULES:
                unit_tests.append(
                    {
                        "rule": rule,
                        "name": name or "?",
                        "line": stmt.lineno,
                        "crate": _const_str(kws.get("crate")),
                        "has_srcs": "srcs" in kws,
                    }
                )
            elif rule == "wrap_host_rust_test":
                host_wrappers.append(
                    {
                        "name": name or "wrap_host_rust_test",
                        "line": stmt.lineno,
                        "binary": _const_str(kws.get("binary_name"))
                        or _const_str(kws.get("binary"))
                        or "",
                    }
                )
    return {
        "calls_by_line": calls_by_line,
        "libs": libs,
        "unit_tests": unit_tests,
        "host_wrappers": host_wrappers,
    }


# Check base BUILD.gn unit test and host unit test wiring.
def is_gn_unit_test_wired(d, no_comments, name):
    for cand in (f"{name}_test", f"{name}_tests"):
        if (
            f'"{cand}"' in no_comments
            or f'":{cand}"' in no_comments
            or f'":{cand}(' in no_comments
        ):
            return True
    test_target = f"{name}_test"
    gn_rel = f"{d}/BUILD.gn"
    try:
        res = subprocess.run(
            [
                "git",
                "-C",
                workdir,
                "grep",
                "-l",
                "-F",
                test_target,
                "--",
                ":(glob)**/BUILD.gn",
                ":(glob)**/*.gni",
            ],
            capture_output=True,
            text=True,
            timeout=10,
        )
        cand_files = [
            p.strip() for p in res.stdout.splitlines() if p.strip() and p.strip() != gn_rel
        ]
    except Exception:
        cand_files = []
    base_name = os.path.basename(d)
    ref_re = re.compile(
        r'(?://' + re.escape(d) + r"|" + re.escape(base_name) + r"):" + re.escape(test_target) + r'\b'
    )
    for cf in cand_files:
        try:
            cf_txt = open(os.path.join(workdir, cf), encoding="utf-8", errors="ignore").read()
        except OSError:
            continue
        cf_clean = re.sub(r"#[^\n]*", "", cf_txt)
        if ref_re.search(cf_clean):
            return True
    return False


def base_gn_test_status(d, base_gn, name, crate_has_tests):
    if base_gn is None:
        return True, False, False
    no_comments = re.sub(r"#[^\n]*", "", base_gn)
    wired = is_gn_unit_test_wired(d, no_comments, name)
    m = re.search(r'\b(\w+)\(\s*"' + re.escape(name) + r'"\s*\)\s*\{', no_comments)
    if m:
        tmpl_name = m.group(1)
        body = block_at(no_comments, m.end() - 1, "{", "}")
        if re.search(r"\bwith_unit_tests\s*=\s*false\b", body):
            return False, True, False
        gn_set_wut = bool(re.search(r"\bwith_(?:host_)?unit_tests\s*=\s*true\b", body))
        if not gn_set_wut:
            tm = re.search(r'\btemplate\(\s*"' + re.escape(tmpl_name) + r'"\s*\)\s*\{', no_comments)
            if tm:
                t_body = block_at(no_comments, tm.end() - 1, "{", "}")
                if re.search(
                    r"\bwith_(?:host_)?unit_tests\s*=\s*(?:true|invoker\.with_unit_tests)\b",
                    t_body,
                ):
                    gn_set_wut = True
        if wired:
            return True, False, False
        if gn_set_wut:
            if crate_has_tests:
                return True, False, False
            return False, False, True
        return False, False, False
    if wired:
        return True, False, False
    return True, False, False


def base_gn_had_host_unit_tests(d, base_gn, lib_name, test_name):
    if base_gn is None:
        return True
    no_comments = re.sub(r"#[^\n]*", "", base_gn)
    if (
        "host_toolchain" in no_comments
        or "with_host_unit_tests" in no_comments
        or re.search(r"\bis_host\b", no_comments)
        or re.search(r"\b(?:ffx_plugin|ffx_tool|sdk_host_tool)\s*\(", no_comments)
    ):
        return True
    if re.search(
        r"\b(?:with_unit_tests|args_with_unit_tests)\s*=\s*true\b", no_comments
    ) and not re.search(
        r"\b(?:fuchsia_unittest_package|fuchsia_test_package|"
        r"fuchsia_unittest_component|fuchsia_test_component|bootfs_test)\s*\(",
        no_comments,
    ):
        return True
    for gm in re.finditer(r'\bgroup\(\s*"tests"\s*\)\s*\{', no_comments):
        gbody = block_at(no_comments, gm.end() - 1, "{", "}")
        for cand in (test_name, f"{lib_name}_test"):
            if cand and re.search(r'":' + re.escape(cand) + r'"', gbody):
                return True
    cur = os.path.dirname(d)
    while cur:
        anc_gn = git_show(f"{cur}/BUILD.gn")
        if anc_gn:
            anc_clean = re.sub(r"#[^\n]*", "", anc_gn)
            if re.search(r"\bassert\(\s*is_host\b", anc_clean):
                return True
            rel_sub = os.path.relpath(d, cur).replace("\\", "/")
            if (
                rel_sub in anc_clean
                and ("host_toolchain" in anc_clean or re.search(r"\bis_host\b", anc_clean))
            ):
                return True
        cur = os.path.dirname(cur)
    return False


# Flag speculative empty Rust unit tests and unrequested host test wrappers.
def report_empty_unit_test(d, line, desc, lib, explicit_false=False, unwired_noop=False):
    if explicit_false:
        reason = (
            f"the base GN target :{lib} explicitly set `with_unit_tests = false` "
            "(no existing unit tests in GN)."
        )
    elif unwired_noop:
        reason = (
            f"its '.rs' sources contain no tests, and although base GN set "
            f"`with_unit_tests = true`, `:{lib}_test` was never wired into the GN build "
            "graph (so `with_unit_tests` was a no-op)."
        )
    else:
        reason = (
            f"its sources contain no tests, and the base GN target :{lib} had no "
            "`with_unit_tests = true`."
        )
    _emit(
        findings,
        "speculative_empty_unit_test",
        "ERROR",
        os.path.join(d, "BUILD.bazel"),
        line,
        f"{desc} tests crate :{lib}, but {reason}",
        REMEDIATION,
    )


def check_speculative_unit_tests(d, parsed, base_gn):
    libs = parsed["libs"]
    tested_libs = set()
    test_to_lib = {}
    for name, info in libs.items():
        if info["has_with_unit_tests"]:
            tested_libs.add(name)
        if info["rule"] not in SPECULATIVE_TEST_LIB_RULES or not info["has_with_unit_tests"]:
            continue
        had_tests, explicit_false, unwired_noop = base_gn_test_status(
            d, base_gn, name, info["has_tests"]
        )
        if explicit_false or (not had_tests and not info["has_tests"]):
            report_empty_unit_test(
                d,
                info["line"],
                f"with_unit_tests on :{name}",
                name,
                explicit_false=explicit_false,
                unwired_noop=unwired_noop,
            )

    for ut in parsed["unit_tests"]:
        crate = ut["crate"]
        tname = ut["name"]
        if crate and crate.startswith(":"):
            lib = crate[1:]
            test_to_lib[tname] = lib
            tested_libs.add(lib)
        if not crate or not crate.startswith(":") or ut["has_srcs"]:
            continue
        lib = crate[1:]
        if lib not in libs or libs[lib]["rule"] not in SPECULATIVE_TEST_LIB_RULES:
            continue
        had_tests, explicit_false, unwired_noop = base_gn_test_status(
            d, base_gn, lib, libs[lib]["has_tests"]
        )
        if explicit_false or (not had_tests and not libs[lib]["has_tests"]):
            report_empty_unit_test(
                d,
                ut["line"],
                f"{ut['rule']} :{tname}",
                lib,
                explicit_false=explicit_false,
                unwired_noop=unwired_noop,
            )
    return tested_libs, test_to_lib


def check_speculative_host_wrappers(d, parsed, base_gn, test_to_lib):
    for hw in parsed["host_wrappers"]:
        wname = hw["name"]
        bin_attr = hw["binary"]
        tname = bin_attr[1:] if bin_attr.startswith(":") else bin_attr
        lib = test_to_lib.get(tname, tname[:-5] if tname.endswith("_test") else tname)
        if not base_gn_had_host_unit_tests(d, base_gn, lib, tname):
            _emit(
                findings,
                "speculative_host_unit_test",
                "ERROR",
                os.path.join(d, "BUILD.bazel"),
                hw["line"],
                (
                    f"`{wname}` wraps `{tname}` as a host test, but in `{d}/BUILD.gn` the unit "
                    "tests were only built/run on Fuchsia (inside a Fuchsia test package) and "
                    "were not run on host."
                ),
                (
                    f"Remove `{wname}` from `{d}/BUILD.bazel` and remove any "
                    f"`:{tname}($host_toolchain)` entry added to `group(\"tests\")` unless host "
                    f"unit tests existed in `{d}/BUILD.gn`."
                ),
            )


# Flag redundant # TODO comments in BUILD.bazel for no-op with_unit_tests on testless crates.
def _has_unit_test_todo_in_text(src):
    if not src:
        return False
    cur = []
    for raw_ln in src.splitlines():
        s = raw_ln.strip()
        if s.startswith("#") and not s.startswith("# @bazel2gn:"):
            cur.append(s.lstrip("#").strip())
        else:
            if cur:
                joined = " ".join(cur)
                if TODO_MARKER_RE.search(joined) and UNIT_TEST_TOPIC_RE.search(joined):
                    return True
                cur = []
    if cur:
        joined = " ".join(cur)
        if TODO_MARKER_RE.search(joined) and UNIT_TEST_TOPIC_RE.search(joined):
            return True
    return False


def check_redundant_empty_unit_test_todos(d, text, parsed, tested_libs, base_gn, base_bazel):
    libs = parsed["libs"]
    if not libs:
        return
    if _has_unit_test_todo_in_text(base_gn) or _has_unit_test_todo_in_text(base_bazel):
        return
    lines = text.splitlines()
    calls_by_line = parsed["calls_by_line"]
    n_lines = len(lines)
    idx = 0
    # Skip top-of-file license header block.
    while idx < n_lines and lines[idx].strip().startswith("#"):
        idx += 1

    while idx < n_lines:
        s = lines[idx].strip()
        if not s.startswith("#") or s.startswith("# @bazel2gn:"):
            idx += 1
            continue
        start_line = idx + 1
        comment_parts = []
        while idx < n_lines:
            cs = lines[idx].strip()
            if cs.startswith("#"):
                if not cs.startswith("# @bazel2gn:"):
                    comment_parts.append(cs.lstrip("#").strip())
                idx += 1
            else:
                break
        if not comment_parts:
            continue
        joined = " ".join(comment_parts)
        if not (TODO_MARKER_RE.search(joined) and UNIT_TEST_TOPIC_RE.search(joined)):
            continue

        # Identify the rule call immediately below the comment block (if attached).
        look = idx
        while look < n_lines and lines[look].strip().startswith("# @bazel2gn:"):
            look += 1
        attached = calls_by_line.get(look + 1)

        named_libs = [
            lname
            for lname in libs
            if re.search(r"\b" + re.escape(lname) + r"(?:_tests?)?\b", joined)
        ]
        if attached is not None:
            _, att_name = attached
            if att_name in libs:
                relevant = list(dict.fromkeys([att_name, *named_libs]))
            elif named_libs:
                relevant = named_libs
            else:
                continue
        else:
            relevant = named_libs if named_libs else [n for n in libs if n not in tested_libs]

        if not relevant or any(n in tested_libs or libs[n]["has_tests"] for n in relevant):
            continue

        shown = ", ".join(f"`:{n}`" for n in relevant)
        bz_rel = os.path.join(d, "BUILD.bazel")
        _emit(
            findings,
            "redundant_empty_unit_test_todo",
            "ERROR",
            bz_rel,
            start_line,
            (
                f"Comment/TODO in `{bz_rel}` notes omitted unit tests or `with_unit_tests` for "
                f"Rust target(s) {shown}, whose `.rs` sources contain no tests. In GN, "
                "`with_unit_tests = true` on a crate with no tests is a no-op, so omitting the "
                "empty unit test target requires no `# TODO` comment."
            ),
            (
                f"Keep the empty unit test target omitted and remove this `# TODO` comment "
                f"block from `{bz_rel}`. Only add a `# TODO` comment for an omitted unit test "
                "when the crate's `.rs` sources actually contain `#[test]` / `#[cfg(test)]` "
                "code that was unbuilt in GN and fails to compile."
            ),
        )


for d in target_dirs:
    path = os.path.join(workdir, d, "BUILD.bazel")
    if not os.path.isfile(path):
        continue
    text = open(path, encoding="utf-8").read()
    parsed = parse_bazel_rust_targets(d, text)
    if parsed is None:
        continue
    base_gn = git_show(f"{d}/BUILD.gn")
    base_bazel = git_show_at_base(f"{d}/BUILD.bazel")
    tested_libs, test_to_lib = check_speculative_unit_tests(d, parsed, base_gn)
    check_speculative_host_wrappers(d, parsed, base_gn, test_to_lib)
    check_redundant_empty_unit_test_todos(d, text, parsed, tested_libs, base_gn, base_bazel)

print(json.dumps(findings, indent=2))
PYEOF
